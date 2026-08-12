//! `--belongs-to` parent/child nesting for `autumn generate scaffold` (issue #1323).
//!
//! A flat scaffold stops at single-table CRUD. Real apps are mostly parent →
//! child (posts → comments, lists → tasks), and the parent-side half of that —
//! "show me this post's comments and let me add one right here" — is the part
//! every app author re-writes by hand.
//!
//! `--belongs-to Post` composes shipped primitives into that missing half:
//!
//! * `GET  /posts/{post_id}/comments` — the child list, scoped to one parent and
//!   paginated through the existing `PageRequest`/`Page` extractors.
//! * `POST /posts/{post_id}/comments` — a create whose foreign key comes from the
//!   **path**, never the submitted body.
//! * a `children_section` helper the child module owns and the parent's generated
//!   `show` view renders, so the list + inline form have one source of truth.
//!
//! The parent-side edit is a marker-delimited, line-oriented injection into the
//! parent's already-generated `src/routes/<parents>.rs`. Every injected line
//! carries a `// autumn:nested:<child_plural>` trailer, which makes re-running the
//! generator idempotent (the marker is already there → skip) and lets `autumn
//! destroy` (#1048) reverse exactly what was added, even when the same parent has
//! several nested children.

use std::path::{Path, PathBuf};

use super::GenerateError;
use super::dsl::Field;
use super::naming::{pascal, pluralize, snake};
use super::scaffold::ScaffoldOptions;

/// Trailer stamped on every line injected into the parent's `show` handler for
/// one child resource. `autumn destroy` removes exactly the lines carrying it.
pub(super) fn child_marker(child_plural: &str) -> String {
    format!("// autumn:nested:{child_plural}")
}

/// The shared prefix of [`child_marker`], used to answer "does this parent still
/// have ANY nested child?" when deciding whether the extra `show` extractors are
/// still needed.
const CHILD_MARKER_PREFIX: &str = "// autumn:nested:";

/// Trailer stamped on the parent `show` signature line once the extra
/// CSRF/submit-token extractors have been spliced in. Shared by every child of
/// the same parent — added by the first, removed by the last.
const EXTRACTORS_MARKER: &str = "// autumn:nested-extractors";

/// The extractors appended to the parent's `show` signature so the injected
/// children section can owner-scope its query and render a CSRF token plus a
/// one-time submit token on the inline create form.
///
/// Deliberately `__nested_`-prefixed rather than reusing `state`/`session`/
/// `csrf`: a parent whose own `show` already declares those (an attachment
/// parent takes `state`, an authorized one takes `session`) must not collide.
/// Duplicating a `FromRequestParts` extractor within one handler is allowed, so
/// the prefixed set is always safe to add unconditionally.
const EXTRACTOR_PARAM_DECLS: &[&str] = &[
    "autumn_web::extract::State(__nested_state): autumn_web::extract::State<autumn_web::AppState>",
    "__nested_session: autumn_web::session::Session",
    "__nested_csrf: Option<CsrfToken>",
    "__nested_csrf_field: Option<CsrfFormField>",
    "__nested_submit_token: Option<SubmitToken>",
    "__nested_submit_field: Option<SubmitFormField>",
];

/// [`EXTRACTOR_PARAM_DECLS`] as the single-line suffix spliced into a
/// one-line `show` signature — the shape the flat scaffold emits.
fn extractor_params_inline() -> String {
    format!(", {}", EXTRACTOR_PARAM_DECLS.join(", "))
}

/// The signature fragment the parent `show` handler must end with for the
/// injection to be able to splice extractors in front of it.
const SHOW_RETURN: &str = ") -> AutumnResult<Markup> {";

/// The `show` handler shape this injection understands: the standard,
/// `id`-keyed, non-state-machine handler a flat HTML scaffold emits.
const SHOW_SIGNATURE_PREFIX: &str = "pub async fn show(id: Path<";

/// The connection type the injected `children_section(&mut db, …)` call
/// requires. A `--sharded` PARENT takes `mut db: ShardedDb`, which would not
/// coerce — so the parent's own db type is a precondition, not an assumption.
const SHOW_DB_PARAM: &str = "mut db: Db";

/// Line-ending discipline for the parent-file edits.
///
/// Both the injection and its reverse are line-oriented, and `str::lines()`
/// silently swallows `\r`. Rejoining with plain `\n` would rewrite every line of
/// a CRLF-checked-out parent file — a whole-file diff from a two-line edit. So
/// CRLF input is normalised once, edited as LF, and restored on the way out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Endings {
    Lf,
    Crlf,
}

impl Endings {
    /// Normalise `src` to LF, remembering what it used.
    fn split(src: &str) -> (String, Self) {
        if src.contains("\r\n") {
            (src.replace("\r\n", "\n"), Self::Crlf)
        } else {
            (src.to_owned(), Self::Lf)
        }
    }

    /// Rejoin edited `lines`, restoring the original endings and trailing
    /// newline (if `original` had one).
    fn rejoin(self, lines: &[String], original: &str) -> String {
        let sep = match self {
            Self::Lf => "\n",
            Self::Crlf => "\r\n",
        };
        let mut out = lines.join(sep);
        if original.ends_with('\n') {
            out.push_str(sep);
        }
        out
    }
}

/// A resolved `--belongs-to` binding: everything the emitters need to name the
/// nested routes, the foreign key, and the parent module they link back to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Nesting {
    /// `Post` — the parent type, used in titles and link labels.
    pub parent_pascal: String,
    /// `post` — the parent's `snake_case` name.
    pub parent_snake: String,
    /// `posts` — the parent's table/route/module name.
    pub parent_plural: String,
    /// `post_id` — the child's own `references` column pointing at the parent.
    pub fk: String,
    /// `true` when this binding was recovered from the markers already in the
    /// parent's routes file rather than from an explicit `--belongs-to`. The
    /// planner surfaces a warning in that case, so the inference is never
    /// silent.
    pub inferred: bool,
}

impl Nesting {
    /// `/posts/{post_id}/comments` — the nested route path both nested handlers
    /// mount at, as a route-macro path string.
    pub fn route_path(&self, child_plural: &str) -> String {
        format!("/{}/{{{}}}/{child_plural}", self.parent_plural, self.fk)
    }
}

/// Resolve `options.belongs_to` into a [`Nesting`], validating every
/// precondition the generated code depends on.
///
/// Returns `Ok(None)` for the overwhelming common case — no `--belongs-to` — in
/// which the scaffold's output stays byte-for-byte what it was before this
/// feature existed.
///
/// `for_revert` skips the two checks that read the project from disk (the parent
/// module must exist, and its `show` must be the shape this injection patches).
/// `autumn destroy scaffold` recomputes this same plan before reverting it, and
/// a generate-time guard must never strand the files it is meant to remove
/// (the precedent set by the shared-layout preflight in issue #1834).
///
/// # Errors
/// [`GenerateError::Config`] when the flag is combined with a scaffold variant
/// this slice does not support, when no `references` column targets the named
/// parent, when that column is nullable or self-referential, or when the parent
/// resource has not been scaffolded (or was scaffolded into a shape — slug-keyed
/// or state-machine — whose `show` this injection does not yet understand).
#[allow(
    clippy::too_many_lines,
    reason = "a linear sequence of independent preconditions, each carrying the \
              actionable message the user sees — splitting it would scatter those \
              messages across helpers without simplifying anything"
)]
pub(super) fn resolve(
    project_root: &Path,
    child_plural: &str,
    fields: &[Field],
    options: &ScaffoldOptions,
    for_revert: bool,
) -> Result<Option<Nesting>, GenerateError> {
    // `--belongs-to` is typed once, when the relationship is created. A later
    // `generate … --force` (the ordinary "I changed a field, re-scaffold it"
    // move) rarely repeats it — and dropping the nesting there would rewrite the
    // child module without its nested handlers while the parent kept calling
    // `children_section`, leaving the project uncompilable. The markers in the
    // parent's routes file are a durable record of the relationship, so recover
    // it. `destroy` already reads the same evidence, which keeps the two
    // directions symmetric.
    let inferred_parent = if options.belongs_to.is_none() {
        infer_parent_from_markers(project_root, child_plural, fields)
    } else {
        None
    };
    let inferred = options.belongs_to.is_none() && inferred_parent.is_some();
    let effective = options.belongs_to.as_deref().or(inferred_parent.as_deref());

    // A parent's injected section must never outlive the relationship it calls
    // into. That section passes `row.id` — the PARENT's own key — to
    // `children_section`, so if this run rebinds the child to a DIFFERENT parent
    // (or to none), the call keeps compiling while meaning something else
    // entirely: `posts.rs` would hand a post id to a helper now filtering on
    // `user_id`, and Post #3's page would quietly list User #3's comments and
    // create rows owned by them. Nothing on the child's side can fix that, and
    // silently rewriting the other parent's view is not this command's
    // business — so refuse before a single file is written, which leaves the
    // project exactly as it was.
    if !for_revert && !options.api {
        let effective_plural = effective.map(|parent| pluralize(&snake(parent)));
        let stale: Vec<String> = parents_carrying_child(project_root, child_plural)
            .iter()
            .filter_map(|path| path.file_stem()?.to_str().map(ToOwned::to_owned))
            .filter(|parent_plural| Some(parent_plural.as_str()) != effective_plural.as_deref())
            .collect();
        if let Some(stale_parent) = stale.first() {
            // Each shape fails differently, and saying which is the whole
            // value of the message: a dropped foreign key stops the project
            // COMPILING (loud), while a re-parent leaves it compiling and
            // reading the wrong rows (silent, and much worse).
            let because = effective_plural.as_deref().map_or_else(
                || {
                    format!(
                        "this field list has no `references` column pointing at {stale_parent}, so \
                     the nested routes that section calls would not be regenerated at all and \
                     that call would no longer compile"
                    )
                },
                |new_parent| {
                    format!(
                        "this run binds {child_plural} to {new_parent} instead, so the same helper \
                     would start reading THAT parent's id — the call keeps compiling and \
                     quietly serves the wrong rows"
                    )
                },
            );
            return Err(GenerateError::Config(format!(
                "{child_plural} is already nested under {stale_parent}: \
                 src/routes/{stale_parent}.rs renders their children section and calls \
                 `crate::routes::{child_plural}::children_section(…)` with its own row id. But \
                 {because}. \
                 Run `autumn destroy scaffold` for this resource first (it removes the \
                 parent-side section from src/routes/{stale_parent}.rs), then generate again. \
                 Nothing has been written."
            )));
        }
    }

    let Some(raw_parent) = effective else {
        return Ok(None);
    };
    let parent_pascal = pascal(raw_parent);
    let parent_snake = snake(raw_parent);
    let parent_plural = pluralize(&parent_snake);

    // Variant gates. Each of these owns a different list/DOM/persistence
    // contract that the nested list + inline form would silently break, so
    // refuse them up front with an actionable message rather than emit output
    // that half-works (the precedent set by the `slug` gates in issue #1260).
    let unsupported = if options.api {
        Some((
            "--api",
            "an API scaffold renders no HTML views, and the nested list + inline create \
             form are exactly those views",
        ))
    } else if options.live_validation {
        Some((
            "--live-validation",
            "the live-validation form owns an htmx contract the nested inline form does \
             not participate in",
        ))
    } else if options.live {
        Some((
            "--live",
            "the SSE live list owns an out-of-band swap contract that a second, \
             parent-scoped list on the same page would break",
        ))
    } else if options.model.sharded {
        Some((
            "--sharded",
            "a sharded repository has no cross-shard parent lookup to scope the child \
             list through",
        ))
    } else {
        None
    };
    if let Some((variant, why)) = unsupported {
        return Err(GenerateError::Config(format!(
            "`--belongs-to` is not supported together with `{variant}`: {why}. Scaffold the \
             child resource without `{variant}` to get the nested routes and the parent-side \
             list, or drop `--belongs-to` to keep a flat resource."
        )));
    }

    // The foreign key is the child's OWN `references` column pointing at the
    // parent's table — this generator consumes `references:` (#1026), it does
    // not re-derive it.
    let Some(fk_field) = fields
        .iter()
        .find(|f| f.reference_table().as_deref() == Some(parent_plural.as_str()))
    else {
        return Err(GenerateError::Config(format!(
            "`--belongs-to {parent_pascal}` needs a foreign key to {parent_plural}: add a \
             `{parent_snake}:references` column to the field list (it becomes the \
             `{parent_snake}_id` column the nested routes filter and insert on)."
        )));
    };
    if fk_field.nullable {
        return Err(GenerateError::Config(format!(
            "`--belongs-to {parent_pascal}` requires a non-nullable parent reference, but \
             `{}` is optional. A nested resource always has a parent — the nested create \
             route sets the foreign key from the URL — so declare it as \
             `{parent_snake}:references`.",
            fk_field.name
        )));
    }
    if let Some(attachment) = fields.iter().find(|f| f.kind.is_attachment()) {
        return Err(GenerateError::Config(format!(
            "`--belongs-to {parent_pascal}` is not supported alongside the `{}` attachment \
                 column: the flat create streams a `multipart/form-data` body, and the \
                 nested inline create form is url-encoded. Scaffold the child without the \
                 attachment column (or without `--belongs-to`) for now.",
            attachment.name
        )));
    }
    if parent_plural == child_plural {
        return Err(GenerateError::Config(format!(
            "`--belongs-to {parent_pascal}` would nest a resource under itself. \
             Self-referential parents (threaded replies) are out of scope for nested \
             scaffolding — scaffold the resource flat and wire the recursion by hand."
        )));
    }

    let nesting = Nesting {
        parent_pascal: parent_pascal.clone(),
        parent_snake: parent_snake.clone(),
        parent_plural: parent_plural.clone(),
        fk: fk_field.name.clone(),
        inferred,
    };

    if for_revert {
        return Ok(Some(nesting));
    }

    // The generated child module links back to `crate::routes::<parents>::paths`,
    // and the parent-side list is injected into that module's `show` — so the
    // parent has to be a scaffolded HTML resource already.
    let parent_routes = project_root
        .join("src")
        .join("routes")
        .join(format!("{parent_plural}.rs"));
    let Ok(parent_src) = std::fs::read_to_string(&parent_routes) else {
        return Err(GenerateError::Config(format!(
            "`--belongs-to {parent_pascal}` needs the parent resource to be scaffolded first: \
             src/routes/{parent_plural}.rs was not found. Run \
             `autumn generate scaffold {parent_pascal} <fields…>` first, then re-run this \
             command."
        )));
    };
    // The preflight must check EVERY anchor the body injection relies on, not
    // just the signature: a `show` whose signature still matches but whose body
    // has been rewritten (the `Ok(…)` tail moved, the markup furniture removed)
    // would otherwise be "patched" into code that does not parse — the
    // generator breaking a file it does not own, and reporting success.
    if injection_anchors(&parent_src).is_none() {
        return Err(GenerateError::Config(format!(
            "`--belongs-to {parent_pascal}` could not find a standard `show` handler in \
             src/routes/{parent_plural}.rs to render the children list in. Nested scaffolding \
             patches the `show` view the flat scaffold generated — not a `slug`-keyed or \
             `:states(…)` parent, and not a `show` whose signature or view body has been \
             rewritten by hand. Scaffold {child_plural} WITHOUT `--belongs-to` (the \
             `{parent_snake}:references` column and its belongs_to dropdown still work), then \
             hand-write the parent-scoped list in that `show` view."
        )));
    }
    // A `--sharded` PARENT holds `mut db: ShardedDb`, which will not coerce to
    // the `&mut Db` the injected `children_section` call takes. The child's own
    // `--sharded` is already refused above; this is the same refusal for the
    // OTHER side of the relationship, which the child's own flags cannot see.
    if !show_signature(&parent_src).is_some_and(|sig| sig.contains(SHOW_DB_PARAM)) {
        return Err(GenerateError::Config(format!(
            "`--belongs-to {parent_pascal}` needs a parent on the standard `Db` connection, \
             but src/routes/{parent_plural}.rs's `show` does not take one (a `--sharded` \
             parent holds a `ShardedDb`, which the children-section query cannot borrow). \
             Scaffold {child_plural} without `--belongs-to` and write the parent-scoped list \
             against the shard handle by hand."
        )));
    }

    Ok(Some(nesting))
}

/// The child plurals currently nested INTO `parent_plural`'s routes file — the
/// mirror of [`parents_carrying_child`], read from the same markers.
///
/// `parents_carrying_child` answers "who nests ME?"; this answers "who do I
/// nest?". A resource that is a parent has no `--belongs-to` of its own and no
/// `Nesting`, so without this its own regeneration would overwrite the injected
/// section away (silently losing the children list AND the only durable record
/// of the relationship), and its own destroy would delete the module its
/// children still import.
pub(super) fn children_nested_under(project_root: &Path, parent_plural: &str) -> Vec<String> {
    let path = project_root
        .join("src")
        .join("routes")
        .join(format!("{parent_plural}.rs"));
    std::fs::read_to_string(&path)
        .map_or_else(|_| Vec::new(), |src| children_nested_under_src(&src))
}

/// Re-apply every child section that `previous` carried onto a freshly rendered
/// parent routes file.
///
/// A parent's own `generate … --force` re-renders `src/routes/<parents>.rs` from
/// the flat template, which knows nothing about children. Without this the
/// regeneration would quietly drop the children list and the markers with it —
/// and a later child regeneration, having no markers left to read, would emit no
/// nested handlers while `main.rs` still mounted them.
pub(super) fn reapply_children(previous: &str, fresh: &str) -> String {
    children_nested_under_src(previous)
        .iter()
        .fold(fresh.to_owned(), |acc, child| {
            inject_into_parent_show(&acc, child)
        })
}

/// [`children_nested_under`] against source text already in hand.
fn children_nested_under_src(src: &str) -> Vec<String> {
    let mut children: Vec<String> = src
        .lines()
        .filter_map(|line| {
            // `rfind` because the marker is a line TRAILER; the code ahead of it
            // may legitimately mention the same text. The prefix carries its
            // colon, so `// autumn:nested-extractors` cannot match.
            let trimmed = line.trim_end();
            let at = trimmed.rfind(CHILD_MARKER_PREFIX)?;
            let child = &trimmed[at + CHILD_MARKER_PREFIX.len()..];
            (!child.is_empty()).then(|| child.to_owned())
        })
        .collect();
    children.sort();
    children.dedup();
    children
}

/// Recover the parent this resource is already nested under from the markers in
/// `src/routes/*.rs`, as the `snake_case` base name `--belongs-to` would have
/// been given (`posts.rs` -> `post`).
///
/// Resolved through the child's OWN `references` columns rather than by
/// singularising the file name: the column that targets that table is the
/// foreign key, and its `_id`-stripped base is exactly the name the flag takes.
/// That also means a regeneration which DROPPED the foreign key infers nothing —
/// correctly, since there is no longer a column to nest on.
///
/// `None` unless exactly one parent matches: with several, guessing which one
/// the author meant would be worse than asking for the flag.
fn infer_parent_from_markers(
    project_root: &Path,
    child_plural: &str,
    fields: &[Field],
) -> Option<String> {
    let mut bases: Vec<String> = parents_carrying_child(project_root, child_plural)
        .iter()
        .filter_map(|path| path.file_stem()?.to_str().map(ToOwned::to_owned))
        .filter_map(|parent_plural| {
            let fk = fields
                .iter()
                .find(|f| f.reference_table().as_deref() == Some(parent_plural.as_str()))?;
            Some(fk.name.strip_suffix("_id").unwrap_or(&fk.name).to_owned())
        })
        .collect();
    bases.dedup();
    match bases.as_slice() {
        [only] => Some(only.clone()),
        _ => None,
    }
}

/// The parent `show` handler's signature text, joined onto one line when the
/// file has been through `rustfmt` (which splits a long signature across
/// lines). `None` when there is no `show` of the shape this injection patches.
fn show_signature(src: &str) -> Option<String> {
    let lines: Vec<&str> = src.lines().collect();
    let start = find_show_signature_line(src)?;
    let end = signature_end_line(&lines, start)?;
    Some(
        lines[start..=end]
            .iter()
            .map(|line| line.trim())
            .collect::<Vec<_>>()
            .join(" "),
    )
}

/// Whether `src` carries `child_plural`'s injection marker.
///
/// Matched as a COMPLETE line trailer, never as a bare substring: the markers
/// are a shared namespace, and `// autumn:nested:posts` is a prefix of
/// `// autumn:nested:postscripts`. A substring test would let a `postscripts`
/// section make a flat `Post` scaffold believe it was nested — mounting
/// `routes::posts::nested_index` against a module that emits no such handler.
/// Every injected line puts the marker last, before and after `rustfmt`, so
/// "ends the line" is the exact test.
fn carries_child_marker(src: &str, child_plural: &str) -> bool {
    let marker = child_marker(child_plural);
    src.lines().any(|line| line.trim_end().ends_with(&marker))
}

/// Every `src/routes/*.rs` in `project_root` still carrying `child_plural`'s
/// injection marker.
///
/// Used on the DESTROY path so `autumn destroy scaffold Comment …` cleans the
/// parent up whether or not the user remembers to repeat `--belongs-to`. The
/// markers are self-describing, so the parent can be found from the child's name
/// alone — and a destroy that removed `src/routes/comments.rs` while leaving
/// `crate::routes::comments::children_section(…)` behind in the parent would
/// leave the project uncompilable.
pub(super) fn parents_carrying_child(project_root: &Path, child_plural: &str) -> Vec<PathBuf> {
    let routes_dir = project_root.join("src").join("routes");
    let Ok(entries) = std::fs::read_dir(&routes_dir) else {
        return Vec::new();
    };
    let mut found: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "rs"))
        .filter(|path| {
            std::fs::read_to_string(path).is_ok_and(|src| carries_child_marker(&src, child_plural))
        })
        .collect();
    // Stable order so a recomputed plan is deterministic across filesystems.
    found.sort();
    found
}

/// Index of the parent `show` handler's signature line, or `None` when the file
/// has no `show` handler of the shape this injection patches.
///
/// Matches both the pristine signature and one this generator has already
/// spliced extractors into for an earlier nested child — otherwise nesting a
/// SECOND child under the same parent (or re-running the generator for the
/// first) would report the parent as unsupported.
fn find_show_signature_line(src: &str) -> Option<usize> {
    let lines: Vec<&str> = src.lines().collect();
    (0..lines.len()).find(|i| {
        let trimmed = lines[*i].trim_start();
        // One-line form, as the flat scaffold emits it.
        if trimmed.starts_with(SHOW_SIGNATURE_PREFIX) {
            return signature_end_line(&lines, *i).is_some();
        }
        // Multi-line form, as `rustfmt` rewrites it once the signature grows
        // past `max_width` — which is exactly what happens after this injection
        // adds six extractors and the project's CI runs `cargo fmt --check`.
        trimmed == "pub async fn show("
            && lines
                .get(i + 1)
                .is_some_and(|next| next.trim_start().starts_with("id: Path<"))
            && signature_end_line(&lines, *i).is_some()
    })
}

/// The line index closing the `show` signature — the one carrying
/// `) -> AutumnResult<Markup> {`. Equal to `start` for a one-line signature.
///
/// Bounded so a `pub async fn show(` that never closes (a truncated or
/// hand-mangled file) is reported as "not a shape we understand" instead of
/// scanning to EOF and matching some unrelated handler's return line.
fn signature_end_line(lines: &[&str], start: usize) -> Option<usize> {
    const MAX_SIGNATURE_LINES: usize = 40;
    let closes = |line: &str| {
        let trimmed = line.trim_start();
        trimmed.starts_with(SHOW_RETURN)
    };
    if lines[start].trim_end().ends_with(SHOW_RETURN)
        || lines[start].trim_end().ends_with(EXTRACTORS_MARKER)
    {
        return Some(start);
    }
    let limit = (start + MAX_SIGNATURE_LINES).min(lines.len());
    (start + 1..limit).find(|i| closes(lines[*i]))
}

/// The `[start, end)` line range of the `show` handler's body, from its
/// signature line to the closing `}` at column 0.
fn show_body_range(lines: &[&str], signature: usize) -> (usize, usize) {
    let start = signature_end_line(lines, signature).unwrap_or(signature) + 1;
    let end = lines[start..]
        .iter()
        .position(|line| *line == "}")
        .map_or(lines.len(), |offset| start + offset);
    (start, end)
}

/// The markup line the children section is rendered above: the standard `show`
/// view's "Back to list" furniture.
const RENDER_ANCHOR: &str = "(autumn_web::a11y::Link::new(paths::index(), \"Back to list\"))";

/// Every line index [`inject_into_parent_show`] needs, or `None` when the
/// parent's `show` is not the shape this injection understands.
///
/// Returns `(signature, ok_tail, render_anchor)`. Checking all three UP FRONT is
/// what keeps a partially-rewritten `show` from being edited into something that
/// does not compile: the injection either has all its anchors or makes no edit
/// at all.
fn injection_anchors(src: &str) -> Option<(usize, usize, usize)> {
    let lines: Vec<&str> = src.lines().collect();
    let signature = find_show_signature_line(src)?;
    let (body_start, body_end) = show_body_range(&lines, signature);
    let ok_tail = (body_start..body_end).find(|i| lines[*i].trim_start().starts_with("Ok("))?;
    let render_anchor = (body_start..body_end).find(|i| lines[*i].trim_start() == RENDER_ANCHOR)?;
    // The markup furniture must come after the `Ok(…)` the binding is inserted
    // before — otherwise the section would be rendered before it is bound.
    if render_anchor <= ok_tail {
        return None;
    }
    Some((signature, ok_tail, render_anchor))
}

/// Inject one child's list + inline create form into the parent's generated
/// `show` view, returning the new file content.
///
/// Idempotent: a file already carrying this child's marker comes back unchanged,
/// so re-running the generator (or running it with `--force`) never
/// double-injects. A parent with several nested children accumulates one
/// binding + one render line per child and a single shared extractor edit.
pub(super) fn inject_into_parent_show(parent_src: &str, child_plural: &str) -> String {
    let marker = child_marker(child_plural);
    if carries_child_marker(parent_src, child_plural) {
        return parent_src.to_owned();
    }
    let (source, endings) = Endings::split(parent_src);
    let Some((signature, ok_tail, render_anchor)) = injection_anchors(&source) else {
        // Not a shape we understand. `resolve` already refuses this on the
        // generate path; returning the input untouched keeps the function
        // total — it never emits a half-edit.
        return parent_src.to_owned();
    };

    let mut lines: Vec<String> = source.lines().map(ToOwned::to_owned).collect();

    // 1. Extra extractors on the signature — added once per parent, however many
    //    children it ends up with. Every added declaration carries the marker so
    //    the undo is exact in either signature shape.
    let signature_close = signature_end_line(
        &lines.iter().map(String::as_str).collect::<Vec<_>>(),
        signature,
    )
    .unwrap_or(signature);
    // Detected by the DECLARATION text as well as the marker: `rustfmt` relocates
    // the marker comment into the body when it explodes the signature, so a
    // marker-only check would re-inject a duplicate set of parameters into an
    // already-injected, already-formatted parent.
    let already_injected = lines[signature..=signature_close].iter().any(|line| {
        line.contains(EXTRACTORS_MARKER) || EXTRACTOR_PARAM_DECLS.iter().any(|d| line.contains(d))
    });
    let mut shift = 0usize;
    if !already_injected {
        if signature == signature_close {
            // One-line signature (as generated): splice the declarations in
            // front of the return type and stamp the marker on the line.
            lines[signature] = format!(
                "{}{}{SHOW_RETURN} {EXTRACTORS_MARKER}",
                lines[signature]
                    .strip_suffix(SHOW_RETURN)
                    .expect("signature_end_line matched this suffix"),
                extractor_params_inline(),
            );
        } else {
            // `rustfmt`-split signature: one marked line per declaration, above
            // the closing `) -> …` line.
            for (offset, decl) in EXTRACTOR_PARAM_DECLS.iter().enumerate() {
                lines.insert(
                    signature_close + offset,
                    format!("    {decl}, {EXTRACTORS_MARKER}"),
                );
            }
            shift = EXTRACTOR_PARAM_DECLS.len();
        }
    }
    let (ok_tail, render_anchor) = (ok_tail + shift, render_anchor + shift);

    // 2. The render call inside the `html!` block, above the "Back to list"
    //    furniture so the children read as part of the record's own detail.
    //    Inserted BEFORE the binding so the earlier `ok_tail` index stays valid.
    lines.insert(
        render_anchor,
        format!("        (__autumn_children_{child_plural}) {marker}"),
    );

    // 3. The `let … = children_section(…).await?;` binding, immediately before the
    //    handler's `Ok(…)` tail. The section is `await`ed outside the `html!`
    //    macro because Maud's markup is a synchronous expression.
    //
    //    `row.id` is the parent's own primary key — the value the child's foreign
    //    key column holds. `&mut db` (rather than a move) so a parent with several
    //    nested children can render each section in turn off the same connection.
    let binding = format!(
        "    let __autumn_children_{child_plural} = crate::routes::{child_plural}::children_section(\
         &mut db, row.id, &autumn_web::pagination::PageRequest::default(), \
         &__nested_state, &__nested_session, \
         __nested_csrf.as_ref(), __nested_csrf_field.as_ref(), __nested_submit_token.as_ref(), \
         __nested_submit_field.as_ref()).await?; {marker}"
    );
    lines.insert(ok_tail, binding);

    endings.rejoin(&lines, parent_src)
}

/// Reverse [`inject_into_parent_show`] for one child: drop the whole injected
/// binding statement and its render call, then — only once no other nested child
/// is left — undo the shared extractor edit on the `show` signature.
///
/// Removal is deliberately **statement**-shaped rather than "delete lines
/// carrying the marker". `rustfmt` splits the injected binding across nine
/// lines, and only the last of them keeps the trailing marker comment; a
/// marker-suffix filter would delete that one line and leave eight orphans
/// behind — a destroy that reports success and leaves the parent uncompilable.
/// So the binding is matched by its `let __autumn_children_<child> =` head and
/// removed through the `.await?;` that terminates it, however it is laid out.
///
/// Idempotent, like every other [`super::emit::Revert`] transform: a file that
/// was never injected (or was already reverted) comes back unchanged.
pub(super) fn remove_nested_child_section(content: &str, child_plural: &str) -> String {
    let (source, endings) = Endings::split(content);
    let all: Vec<&str> = source.lines().collect();
    let binding_head = format!("let __autumn_children_{child_plural} =");
    let render_call = format!("(__autumn_children_{child_plural})");

    // Only lines inside the `show` handler are ever injected, so only those are
    // ever removed: a user who happens to write one of these tokens elsewhere in
    // the file keeps their line. A file whose `show` can no longer be located
    // (it was rewritten after injection) falls back to a whole-file sweep —
    // leaving dangling references to a just-deleted module would be worse.
    let (from, to) = find_show_signature_line(&source)
        .map_or((0, all.len()), |signature| show_body_range(&all, signature));

    let mut drop = vec![false; all.len()];
    let mut i = from;
    while i < to {
        let trimmed = all[i].trim_start();
        if trimmed.starts_with(&binding_head) {
            // The binding runs to its terminating `.await?;`, which is on this
            // same line when unformatted and several lines down once `rustfmt`
            // has split the call.
            let end = (i..to).find(|j| all[*j].contains(".await?;")).unwrap_or(i);
            for item in drop.iter_mut().take(end + 1).skip(i) {
                *item = true;
            }
            i = end + 1;
            continue;
        }
        if trimmed.starts_with(&render_call) {
            drop[i] = true;
        }
        i += 1;
    }
    let mut lines: Vec<String> = all
        .iter()
        .enumerate()
        .filter(|(i, _)| !drop[*i])
        .map(|(_, line)| (*line).to_owned())
        .collect();

    // The extractors are shared by every nested child of this parent, so they
    // only come off with the last one. Both layouts are undone: the inline
    // splice on a one-line signature, and the one-declaration-per-line form
    // `rustfmt` leaves behind.
    let any_child_left = lines
        .iter()
        .any(|line| line.contains(CHILD_MARKER_PREFIX) && !line.contains(EXTRACTORS_MARKER));
    // Only ever touch a signature this generator actually spliced — the marker
    // is the proof, wherever `rustfmt` has since moved it.
    let was_injected = lines.iter().any(|line| line.contains(EXTRACTORS_MARKER));
    if !any_child_left && was_injected {
        // Drop only lines that ARE one of the declarations this generator adds
        // — matched in full, not merely "carries the marker". A blanket
        // marker-suffix filter would delete the whole one-line `show` signature,
        // which also carries the marker.
        //
        // The marker is matched OPTIONALLY because `rustfmt` relocates it: when
        // it explodes the one-line signature it keeps the single trailing
        // comment and parks it as the first line of the body, leaving the
        // declarations themselves bare. Matching the declaration text is what
        // makes the undo survive a formatted project — and the project's own
        // generated CI runs `cargo fmt --check`, so formatted is the norm.
        let decl_lines: Vec<String> = EXTRACTOR_PARAM_DECLS
            .iter()
            .map(|decl| format!("{decl},"))
            .collect();
        lines.retain(|line| {
            let trimmed = line.trim();
            if trimmed == EXTRACTORS_MARKER {
                return false; // the orphaned marker `rustfmt` left in the body
            }
            let without_marker = trimmed
                .strip_suffix(EXTRACTORS_MARKER)
                .map_or(trimmed, str::trim_end);
            !decl_lines.iter().any(|decl| without_marker == decl)
        });
        // …and un-splice the inline form from a one-line signature. If the
        // declarations are not the ones THIS version emits (a project injected
        // by an older CLI), the line is left completely alone: leftover unused
        // parameters are a warning, deleting a signature we don't recognise is a
        // broken build.
        let inline = extractor_params_inline();
        for line in &mut lines {
            if !line.trim_end().ends_with(EXTRACTORS_MARKER) {
                continue;
            }
            let stripped = line.replace(&inline, "");
            if stripped == *line {
                continue;
            }
            if let Some(rest) = stripped.trim_end().strip_suffix(EXTRACTORS_MARKER) {
                rest.trim_end().clone_into(line);
            }
        }
    }

    endings.rejoin(&lines, content)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PARENT: &str = r#"pub async fn show(id: Path<i64>, mut db: Db, flash: Flash) -> AutumnResult<Markup> {
    let row: Post = posts::table
        .find(*id)
        .select(Post::as_select())
        .first(&mut *db)
        .await
        .map_err(AutumnError::not_found)?;
    let props: Vec<(&str, maud::Markup)> = vec![
        ("Id", maud::html! { (row.id) }),
    ];
    Ok(crate::layout(&format!("Post #{}", row.id), "/posts", flash_messages(&flash.consume().await), html! {
        h1 { "Post #" (row.id) }
        (autumn_web::widgets::property_list(&props))
        (autumn_web::a11y::Link::new(paths::index(), "Back to list"))
        " "
        (autumn_web::a11y::Link::new(paths::edit(row.id), "Edit"))
    }))
}
"#;

    #[test]
    fn injection_adds_extractors_binding_and_render_call() {
        let out = inject_into_parent_show(PARENT, "comments");
        assert!(out.contains("__nested_csrf: Option<CsrfToken>"), "{out}");
        assert!(
            out.contains("crate::routes::comments::children_section(&mut db, row.id,"),
            "{out}"
        );
        assert!(
            out.contains("        (__autumn_children_comments) // autumn:nested:comments"),
            "{out}"
        );
        let render = out.find("(__autumn_children_comments)").unwrap();
        let back = out.find("\"Back to list\"").unwrap();
        assert!(render < back, "{out}");
    }

    #[test]
    fn injection_is_idempotent() {
        let once = inject_into_parent_show(PARENT, "comments");
        let twice = inject_into_parent_show(&once, "comments");
        assert_eq!(once, twice);
    }

    #[test]
    fn round_trips_back_to_the_original_source() {
        let injected = inject_into_parent_show(PARENT, "comments");
        assert_ne!(injected, PARENT);
        assert_eq!(remove_nested_child_section(&injected, "comments"), PARENT);
    }

    #[test]
    fn two_children_share_the_extractors_and_removal_is_per_child() {
        let one = inject_into_parent_show(PARENT, "comments");
        let both = inject_into_parent_show(&one, "likes");
        assert_eq!(
            both.matches("__nested_csrf: Option<CsrfToken>").count(),
            1,
            "the extractor edit is shared, not repeated:\n{both}"
        );
        // Removing one child keeps the other — and keeps the extractors it needs.
        let after = remove_nested_child_section(&both, "comments");
        assert!(!after.contains("__autumn_children_comments"), "{after}");
        assert!(after.contains("__autumn_children_likes"), "{after}");
        assert!(
            after.contains("__nested_csrf: Option<CsrfToken>"),
            "{after}"
        );
        // Removing the last one restores the original signature.
        assert_eq!(remove_nested_child_section(&after, "likes"), PARENT);
    }

    /// The signature this injection produces is long enough that `rustfmt`
    /// splits it — and the generated project's own CI runs `cargo fmt --check`,
    /// so a formatted parent is the normal state, not an edge case.
    /// Both halves must survive it: nesting a SECOND child, and destroying.
    const FORMATTED_PARENT: &str = r#"pub async fn show(
    id: Path<i64>,
    mut db: Db,
    flash: Flash,
    autumn_web::extract::State(__nested_state): autumn_web::extract::State<autumn_web::AppState>,
    __nested_session: autumn_web::session::Session,
    __nested_csrf: Option<CsrfToken>,
    __nested_csrf_field: Option<CsrfFormField>,
    __nested_submit_token: Option<SubmitToken>,
    __nested_submit_field: Option<SubmitFormField>,
) -> AutumnResult<Markup> {
    // autumn:nested-extractors
    let row: Post = posts::table.find(*id).first(&mut *db).await?;
    let __autumn_children_comments = crate::routes::comments::children_section(
        &mut db,
        row.id,
        &autumn_web::pagination::PageRequest::default(),
        &__nested_state,
        &__nested_session,
    )
    .await?; // autumn:nested:comments
    Ok(crate::layout(&format!("Post #{}", row.id), "/posts", flash, html! {
        h1 { "Post #" (row.id) }
        (autumn_web::widgets::property_list(&props))
        (__autumn_children_comments) // autumn:nested:comments
        (autumn_web::a11y::Link::new(paths::index(), "Back to list"))
    }))
}
"#;
    #[test]
    fn removal_takes_a_rustfmt_split_binding_out_whole() {
        let out = remove_nested_child_section(FORMATTED_PARENT, "comments");
        assert!(
            !out.contains("__autumn_children_comments"),
            "the whole reflowed statement must go, not just its marked last line:\n{out}"
        );
        assert!(!out.contains("children_section("), "{out}");
        assert!(
            !out.contains("&mut db,"),
            "orphaned argument lines left behind:\n{out}"
        );
        // The extractors were the last nested child's, so they come off too.
        assert!(!out.contains("__nested_csrf"), "{out}");
        assert!(!out.contains(EXTRACTORS_MARKER), "{out}");
        // Everything that was not injected survives.
        assert!(out.contains("let row: Post = posts::table"), "{out}");
        assert!(out.contains("\"Back to list\""), "{out}");
    }

    #[test]
    fn a_second_child_injects_into_a_rustfmt_split_signature() {
        let out = inject_into_parent_show(FORMATTED_PARENT, "likes");
        assert!(out.contains("__autumn_children_likes"), "{out}");
        // The first child's section is untouched.
        assert!(out.contains("__autumn_children_comments"), "{out}");
        // No second copy of the shared extractors.
        assert_eq!(out.matches("__nested_csrf:").count(), 1, "{out}");
    }

    #[test]
    fn crlf_parent_files_keep_their_line_endings() {
        let crlf = PARENT.replace('\n', "\r\n");
        let injected = inject_into_parent_show(&crlf, "comments");
        assert!(injected.contains("\r\n"), "CRLF must survive the injection");
        assert!(
            !injected.contains("\n\n"),
            "no bare LF lines should be introduced"
        );
        assert_eq!(remove_nested_child_section(&injected, "comments"), crlf);
    }

    #[test]
    fn injection_refuses_a_show_whose_view_body_was_rewritten() {
        // Signature intact, markup furniture gone: the anchors the render call
        // needs are missing, so the file must be left EXACTLY alone rather than
        // patched into something that does not parse.
        let mangled = PARENT.replace(
            "        (autumn_web::a11y::Link::new(paths::index(), \"Back to list\"))\n",
            "",
        );
        assert_ne!(mangled, PARENT, "the fixture must actually change");
        assert_eq!(inject_into_parent_show(&mangled, "comments"), mangled);
    }

    #[test]
    fn the_injected_call_only_uses_extractors_the_signature_declares() {
        let out = inject_into_parent_show(PARENT, "comments");
        let signature = out
            .lines()
            .find(|line| line.contains("pub async fn show("))
            .expect("show signature");
        for binding in [
            "__nested_state",
            "__nested_session",
            "__nested_csrf",
            "__nested_csrf_field",
            "__nested_submit_token",
            "__nested_submit_field",
        ] {
            assert!(
                signature.contains(binding),
                "the injected call passes `{binding}` but the signature does not declare it:\n{signature}"
            );
        }
    }

    #[test]
    fn removal_never_deletes_a_signature_carrying_an_unrecognised_extractor_splice() {
        // A parent injected by an OLDER CLI carries the marker but a different
        // parameter list. Removal must not guess: leaving unused parameters is a
        // warning, deleting the `show` signature is a broken build.
        let stale = PARENT.replace(
            ") -> AutumnResult<Markup> {",
            ", __nested_csrf: Option<CsrfToken>) -> AutumnResult<Markup> { // autumn:nested-extractors",
        );
        let out = remove_nested_child_section(&stale, "comments");
        assert!(
            out.contains("pub async fn show("),
            "the signature must survive an unrecognised splice:\n{out}"
        );
        assert!(out.contains("-> AutumnResult<Markup> {"), "{out}");
    }

    #[test]
    fn a_marker_is_never_matched_as_a_prefix_of_a_longer_plural() {
        // `// autumn:nested:posts` is a prefix of `// autumn:nested:postscripts`.
        // A substring test would make a flat `Post` scaffold believe it was
        // nested — and mount `routes::posts::nested_index` against a module
        // that emits no such handler.
        let with_longer = inject_into_parent_show(PARENT, "postscripts");
        assert!(with_longer.contains("// autumn:nested:postscripts"));
        assert!(
            !carries_child_marker(&with_longer, "posts"),
            "a `postscripts` section must not read as a `posts` one:\n{with_longer}"
        );
        assert!(carries_child_marker(&with_longer, "postscripts"));
        // …and the idempotency check keys off the same test, so nesting
        // `posts` under that parent still injects rather than silently
        // skipping.
        let both = inject_into_parent_show(&with_longer, "posts");
        assert!(both.contains("__autumn_children_posts ="), "{both}");
        assert!(both.contains("__autumn_children_postscripts ="), "{both}");
        // Removing one leaves the other intact.
        let after = remove_nested_child_section(&both, "posts");
        assert!(!after.contains("__autumn_children_posts ="), "{after}");
        assert!(after.contains("__autumn_children_postscripts ="), "{after}");
    }

    #[test]
    fn removal_is_a_no_op_on_a_file_that_was_never_injected() {
        assert_eq!(remove_nested_child_section(PARENT, "comments"), PARENT);
    }

    #[test]
    fn a_non_standard_parent_show_is_left_untouched() {
        let hand_rolled =
            "pub async fn show(Path(slug): Path<String>) -> AutumnResult<Markup> {\n}\n";
        assert_eq!(
            inject_into_parent_show(hand_rolled, "comments"),
            hand_rolled
        );
    }
}
