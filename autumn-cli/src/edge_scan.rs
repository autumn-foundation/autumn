//! Source scan for `#[edge]` route markers and `edge_routes![]` registrations
//! (issue #1790).
//!
//! `autumn build` and `autumn doctor` both need to know, *without compiling the
//! project*, whether it has any edge-eligible routes: the build needs it to
//! decide whether to emit an edge capsule at all, and doctor needs it to run its
//! `edge_target` / `edge_routes` preflights in a project whose dependencies may
//! not even be fetched yet. Compiling to find out would defeat the purpose of a
//! preflight, so — exactly like `autumn a11y verify`, `autumn i18n check`, and
//! `autumn lifecycle check` — this is a source scanner: it parses each `.rs`
//! file under `src/` with `syn` and inspects the attributes each function
//! carries, and it tokenizes the same file to find `edge_routes![...]`
//! invocations and the handler identifiers they register.
//!
//! It records, per marked function: its name, the file and line it is declared
//! at, and whether the *same* function also carries one of the auth/guard
//! attributes (`#[secured]`, `#[authorize]`, `#[step_up]`, `#[throttle]`) that
//! the `#[edge]` macro rejects — the edge capsule has no session or auth state,
//! so that combination is a compile error, and catching it here turns a failed
//! build into a doctor line.
//!
//! **Recognition limits.** Like its sibling scanners this is a best-effort
//! textual pass, not a name resolver:
//!
//! - `#[edge]` is recognized under a bare `#[edge]` / `#[edge(needs(kv))]` path
//!   and under any qualified path whose **last** segment is `edge` (so
//!   `#[autumn_web::edge]` and `#[macros::edge]` are recognized). An `use ...
//!   ::edge as e;` rename is **not** resolved, so `#[e]` is invisible to the
//!   scan.
//! - Guard attributes are matched the same way (last path segment), on the same
//!   function only. A guard applied by an enclosing module or a layer is not
//!   visible here — that is the macro's and the router's job, not this scan's.
//! - `edge_routes![...]` is recognized as the identifier `edge_routes` followed
//!   by `!` and a delimited group, anywhere in the file (item position, inside a
//!   function body, nested in another macro's group). A re-exported alias of the
//!   macro under a different name is not recognized. Each comma-separated entry
//!   keeps its full path text, so `edge_routes![handlers::greet]` registers
//!   `handlers::greet`.
//! - A registration is matched against a function by name, plus the chain of
//!   modules the scan saw the function declared under: *inline* (`mod users {
//!   #[edge] fn show() {} }` gives `show` the path `users`), or derived from
//!   the declaring file's own location for an *out-of-line* `mod users;`
//!   (`src/users.rs` also gives `users`, following Rust's directory-mirrors-
//!   modules convention). A bare entry (`greet`, no `::`) matches a function
//!   with that name in any module — the old, lenient rule. A qualified entry
//!   (`users::show`, or `crate::users::show` with the `crate::` prefix
//!   stripped first) matches only a function whose own module path is
//!   exactly `users`, unless the scan never learned the function's module
//!   path — the scan cannot know such a function's true, resolved module
//!   path from its declaring file alone, so any qualifier still matches it.
//! - Only free functions are scanned, including those declared in inline
//!   modules (`mod routes { ... }`) or out-of-line ones (`mod routes;`
//!   backed by `src/routes.rs` or `src/routes/mod.rs`). A function generated
//!   by another macro, or declared in a file the scan does not reach, is
//!   invisible. The file-derived module path is a location heuristic, not
//!   name resolution: it does not follow a `#[path = "..."]` attribute that
//!   points a module at a differently-named file, and nested out-of-line
//!   modules under an inline one are not stitched together.
//! - `#[cfg(...)]` is evaluated, but only a safe, narrow slice of it. The scan
//!   reads the crate's own `Cargo.toml`, follows `[features] default = [...]`
//!   plus any features the caller explicitly requested (`autumn build
//!   --features x` — see [`resolve_edge_scan_with_features`]) through the
//!   feature graph in `[features]`, and builds the set of feature names this
//!   build turns on. It then checks each `#[cfg(...)]` on the same function
//!   as `#[edge]` (real Rust requires every one of them to hold, so several
//!   such attributes combine the same way). A predicate built only from
//!   `feature = "x"` leaves combined with `not(...)`, `all(...)`, and
//!   `any(...)` is evaluated against that set, and the function is excluded
//!   only when such a predicate is definitely false. Any other predicate —
//!   `target_os`, `debug_assertions`, a feature implied by an optional
//!   dependency, or one this scan cannot parse — is treated as true, so the
//!   function stays in the scan. `autumn build` has no `--no-default-features`
//!   flag, so that case does not arise; workspace feature unification (a
//!   sibling crate elsewhere in the build turning on a feature this scan does
//!   not know about) is not considered.
//! - A file that does not parse is skipped silently (it will fail the build with
//!   a far better message than this scanner could produce).
//!
//! Every consequence of a miss is conservative: a missed marker means the edge
//! capsule step is skipped (the native build is unaffected), and a missed
//! registration means at most a warning that names the function.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::str::FromStr as _;

use proc_macro2::{Delimiter, TokenStream, TokenTree};

/// Attribute names that cannot coexist with `#[edge]`: the edge capsule has no
/// session, no auth state, and no rate-limit store, and `#[intercept]` layers
/// are origin-only tower middleware that never run in the capsule, so the
/// `#[edge]` macro rejects each of these on the same handler.
pub const EDGE_GUARD_ATTRS: &[&str] = &["secured", "authorize", "step_up", "throttle", "intercept"];

/// One `#[edge]`-marked handler function found in the project's sources.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdgeFn {
    /// The function's name — also the identifier `edge_routes![]` registers it by.
    pub name: String,
    /// Path of the declaring file, relative to the scan root (`src/routes/home.rs`).
    pub file: String,
    /// 1-based line of the function's declaration.
    pub line: usize,
    /// Guard attributes carried by the same function, in source order
    /// (a subset of [`EDGE_GUARD_ATTRS`]).
    pub guards: Vec<String>,
    /// Names of the modules the function is declared under, outermost first
    /// (`mod a { mod b { ... } }` gives `["a", "b"]`; an out-of-line `mod a;`
    /// backed by `src/a.rs` gives the same, derived from the file's own
    /// location). Empty for a genuinely top-level function.
    pub module_path: Vec<String>,
}

impl EdgeFn {
    /// `name @ file:line`, the form used in build warnings and doctor details.
    #[must_use]
    pub fn location(&self) -> String {
        format!("{} @ {}:{}", self.name, self.file, self.line)
    }
}

/// Result of scanning a project's sources for edge routes.
#[derive(Debug, Clone, Default)]
pub struct EdgeScan {
    /// Every `#[edge]`-marked function, in scan order (files sorted by path).
    pub functions: Vec<EdgeFn>,
    /// Full path text of each `edge_routes![...]` entry, as written
    /// (`"users::show"`, or a bare `"greet"`).
    pub registered: BTreeSet<String>,
    /// Number of `edge_routes![...]` invocations seen (0 means the app never
    /// registers its marked handlers, so the capsule would serve nothing).
    pub registrations: usize,
    /// Number of `.rs` files read.
    pub files_scanned: usize,
}

impl EdgeScan {
    /// `true` when the project has no `#[edge]`-marked function at all.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.functions.is_empty()
    }

    /// Names of every marked function, in scan order.
    #[must_use]
    pub fn names(&self) -> Vec<&str> {
        self.functions.iter().map(|f| f.name.as_str()).collect()
    }

    /// Marked functions that no `edge_routes![...]` invocation registers — they
    /// compile, but never reach the capsule's router.
    #[must_use]
    pub fn unregistered(&self) -> Vec<&EdgeFn> {
        self.functions
            .iter()
            .filter(|f| !is_registered(f, &self.registered))
            .collect()
    }

    /// Marked functions that an `edge_routes![...]` invocation registers — the
    /// routes the capsule actually serves.
    #[must_use]
    pub fn registered_fns(&self) -> Vec<&EdgeFn> {
        self.functions
            .iter()
            .filter(|f| is_registered(f, &self.registered))
            .collect()
    }

    /// Marked functions that also carry an auth/guard attribute — a combination
    /// the `#[edge]` macro rejects at compile time.
    #[must_use]
    pub fn guarded(&self) -> Vec<&EdgeFn> {
        self.functions
            .iter()
            .filter(|f| !f.guards.is_empty())
            .collect()
    }
}

/// `true` when some `edge_routes![...]` entry in `registered` names `f`.
///
/// An entry's last `::` segment must equal `f.name`. A bare entry (no `::`)
/// matches `f` regardless of module — the scanner's long-standing lenient
/// rule for unqualified names. A qualified entry matches only when its
/// qualifier equals `f.module_path` exactly, unless `f.module_path` is empty
/// (a top-level function, whose real, resolved module path the scan cannot
/// know from its declaring file alone) — then any qualifier still matches.
///
/// Exact equality, not a suffix match, is deliberate: a relative reference
/// written from inside an ancestor of `f`'s module (`edge_routes![v1::greet]`
/// for a `greet` nested two levels down) will not match here and reports as
/// unregistered — a false-positive warning, the scanner's documented safe
/// direction. A suffix match would fix that case but reopen the one this
/// function exists to close: with sibling modules `mod a { mod x { #[edge]
/// fn f() {} } }` and `mod b { mod x { #[edge] fn f() {} } }`, a suffix
/// match on `x::f` would satisfy both, silently muting the real warning for
/// whichever one was not actually registered.
///
/// A leading `crate` segment in the qualifier is stripped first:
/// `edge_routes![crate::users::show]` is the same route as `users::show`,
/// and `f.module_path` never carries that prefix, so leaving it in would
/// make every `crate::`-qualified registration miss its own function.
fn is_registered(f: &EdgeFn, registered: &BTreeSet<String>) -> bool {
    registered.iter().any(|entry| {
        let (qualifier, name) = entry.rsplit_once("::").unwrap_or(("", entry.as_str()));
        if name != f.name {
            return false;
        }
        if qualifier.is_empty() || f.module_path.is_empty() {
            return true;
        }
        let mut qualifier_segments = qualifier.split("::");
        if qualifier_segments.clone().next() == Some("crate") {
            qualifier_segments.next();
        }
        qualifier_segments.eq(f.module_path.iter().map(String::as_str))
    })
}

/// Scan a set of in-memory `(file, source)` pairs, given the set of feature
/// names enabled for this build — see [`enabled_features_from_manifest`] —
/// used to evaluate `#[cfg(...)]` on `#[edge]`-marked functions. The pure
/// core of the scan: [`resolve_edge_scan_with_features`] is the thin
/// filesystem wrapper around it.
fn scan_sources_with_features(
    sources: &[(&str, &str)],
    default_features: &BTreeSet<String>,
) -> EdgeScan {
    let mut scan = EdgeScan::default();
    for (file, src) in sources {
        scan_source(file, src, default_features, &mut scan);
        scan.files_scanned += 1;
    }
    scan
}

/// Scan a set of in-memory `(file, source)` pairs, treating every feature as
/// off (so a `#[cfg(feature = "...")]`-gated `#[edge]` function is excluded
/// unless a `not(...)` around it makes that resolve to true). Most unit tests
/// in this module — and in `build.rs`'s and `doctor.rs`'s own test modules —
/// drive this directly with inline source strings; a test that exercises
/// `#[cfg(...)]` evaluation against a specific feature set uses
/// [`scan_sources_with_features`] instead.
///
/// `#[cfg(test)]`: nothing outside a test build calls this — production code
/// goes through [`resolve_edge_scan`], which needs the real feature set.
#[cfg(test)]
#[must_use]
pub fn scan_sources(sources: &[(&str, &str)]) -> EdgeScan {
    scan_sources_with_features(sources, &BTreeSet::new())
}

/// [`resolve_edge_scan`] with no explicitly-requested features — the shape
/// every caller used before `autumn build --features x` needed to widen the
/// considered set beyond the manifest's own defaults.
#[must_use]
pub fn resolve_edge_scan(project_root: &Path) -> EdgeScan {
    resolve_edge_scan_with_features(project_root, &[])
}

/// Walk `project_root/src` and scan every `.rs` file below it. Paths are
/// recorded relative to `project_root` (`src/routes/home.rs`). A missing `src/`
/// yields an empty scan — a project without sources simply has no edge routes.
///
/// `requested_features` are feature names asked for on the command line
/// (`autumn build --features x`, forwarded here from `build.rs`), on top of
/// the manifest's own `[features] default = [...]`. Without them, a route
/// gated on a non-default feature the caller explicitly requested would look
/// cfg'd-out to this scan even though the real build the caller is about to
/// run turns it on — exactly the dangerous direction (a route that will
/// really be served, silently missing from the scan) this module's `#[cfg]`
/// evaluation is designed to never risk.
#[must_use]
pub fn resolve_edge_scan_with_features(
    project_root: &Path,
    requested_features: &[&str],
) -> EdgeScan {
    // A missing or unparseable Cargo.toml yields no default features, which is
    // the safe direction: every `#[cfg(feature = "...")]` then stays
    // unresolved, and the function it gates stays in the scan.
    let default_features = std::fs::read_to_string(project_root.join("Cargo.toml"))
        .ok()
        .map(|manifest| enabled_features_from_manifest(&manifest, requested_features))
        .unwrap_or_default();

    let mut files = Vec::new();
    collect_rs_files(&project_root.join("src"), &mut files);
    // Sorted so warnings, doctor details, and the build's route list are stable
    // across platforms and filesystem orderings.
    files.sort();

    // Read first, scan second, so the filesystem half and the pure half stay
    // separable: `scan_sources` is the same entry point the unit tests drive
    // with inline sources. An unreadable file is skipped, like the sibling
    // scanners do.
    let sources: Vec<(String, String)> = files
        .iter()
        .filter_map(|path| {
            let src = std::fs::read_to_string(path).ok()?;
            let rel = path
                .strip_prefix(project_root)
                .unwrap_or(path)
                .to_string_lossy()
                .replace('\\', "/");
            Some((rel, src))
        })
        .collect();
    let borrowed: Vec<(&str, &str)> = sources
        .iter()
        .map(|(file, src)| (file.as_str(), src.as_str()))
        .collect();
    scan_sources_with_features(&borrowed, &default_features)
}

/// Read a crate's `Cargo.toml`, seed the feature queue with `[features]
/// default = [...]` plus `requested` (features asked for on the command
/// line), and follow both through the feature graph in `[features]` to build
/// the full set of feature names actually enabled.
///
/// Only follows features the crate declares in its own `[features]` table. A
/// dependency-feature reference (`pkg/feat`, `pkg?/feat`, `dep:pkg`) is not a
/// feature name of this crate, so it stops there rather than being treated as
/// one — `cfg(feature = "...")` never names a dependency's feature anyway.
/// A requested name is still recorded as enabled even without a `[features]`
/// table to expand it through — an app can request a feature that exists
/// only to gate `#[cfg(feature = "...")]` code, with no `[features]` entry of
/// its own. Returns just `requested` (or nothing) on a parse failure — the
/// safe direction, since an unresolvable `feature = "..."` predicate stays
/// conservative regardless.
///
/// `requested` may use Cargo's package-qualified `--features` syntax
/// (`autumn build -p blog --features blog/extra-routes`, `pkg?/feat`) — the
/// qualifier is stripped so the plain feature name reaches the queue instead
/// of being dropped by the dependency-feature check below, which would
/// silently miss it and leave a real, enabled route out of the scan (the
/// dangerous direction this scan exists to avoid). A qualifier naming a
/// different crate does no harm: the stripped name will not match any
/// `#[cfg(feature = "...")]` in this crate's own sources either way.
#[must_use]
fn enabled_features_from_manifest(manifest: &str, requested: &[&str]) -> BTreeSet<String> {
    let mut enabled = BTreeSet::new();
    let features_table = toml::from_str::<toml::Table>(manifest)
        .ok()
        .and_then(|table| {
            table
                .get("features")
                .and_then(toml::Value::as_table)
                .cloned()
        });

    let mut queue: Vec<String> = features_table
        .as_ref()
        .and_then(|features| features.get("default"))
        .and_then(toml::Value::as_array)
        .map(|default| {
            default
                .iter()
                .filter_map(toml::Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    queue.extend(requested.iter().map(|name| {
        name.rsplit_once('/')
            .map_or_else(|| (*name).to_owned(), |(_, feat)| feat.to_owned())
    }));

    while let Some(name) = queue.pop() {
        if name.contains('/') || name.starts_with("dep:") {
            continue;
        }
        if !enabled.insert(name.clone()) {
            continue; // Already expanded — skip, so a feature cycle can't loop forever.
        }
        if let Some(sub) = features_table
            .as_ref()
            .and_then(|features| features.get(&name))
            .and_then(toml::Value::as_array)
        {
            queue.extend(
                sub.iter()
                    .filter_map(toml::Value::as_str)
                    .map(str::to_owned),
            );
        }
    }
    enabled
}

/// Recursively collect `.rs` files under `dir`, skipping build output, dot-dirs,
/// and `tests/` trees, and never following symlinks (a symlinked directory cycle
/// would otherwise hang this tool).
fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() {
            continue;
        }
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if file_type.is_dir() {
            if name == "target" || name == "tests" || name.starts_with('.') {
                continue;
            }
            collect_rs_files(&path, out);
        } else if file_type.is_file() && path.extension().and_then(|s| s.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

/// Scan one source: `syn` for the marked functions, tokens for the registrations.
///
/// The two halves are independent on purpose — a file whose *items* fail to
/// parse can still contribute registrations, and vice versa.
fn scan_source(file: &str, src: &str, default_features: &BTreeSet<String>, scan: &mut EdgeScan) {
    if let Ok(ast) = syn::parse_file(src) {
        let mut module_path = module_path_from_file(file);
        scan_items(&ast.items, file, &mut module_path, default_features, scan);
    }
    if let Ok(stream) = TokenStream::from_str(src) {
        collect_registrations(&stream, scan);
    }
}

/// Derive a function's module-path *prefix* from the file it is declared
/// in, following Rust's directory-mirrors-modules convention: `src/routes/
/// home.rs` is module `routes::home`; `src/routes/mod.rs`, `src/main.rs`,
/// and `src/lib.rs` contribute no segment of their own (each is the "index"
/// file for its directory, or the crate root). Inline `mod x { ... }`
/// nesting (tracked separately in [`scan_items`]) appends after this
/// prefix, giving the full accumulated path.
///
/// Without this, every function reached via `mod x;` (a separate file) —
/// the common case, not the inline-module one — got an empty module path,
/// which the lenient top-level fallback in [`is_registered`] treats as
/// "any qualifier matches." That let `edge_routes![users::show]` mark an
/// unrelated `admin::show` as registered too, for the exact `mod users;` /
/// `mod admin;` layout this whole check exists to handle (Codex review on
/// #2739, P2).
///
/// A non-standard layout (`#[path = "..."]`, e.g.) can make this wrong, like
/// every other heuristic in this best-effort scanner — see the module doc's
/// "Recognition limits". A wrong prefix can only ever produce an extra
/// false-positive "unregistered" warning, never a missed one: this scanner's
/// documented safe direction.
#[must_use]
fn module_path_from_file(file: &str) -> Vec<String> {
    let without_ext = file.strip_suffix(".rs").unwrap_or(file);
    let without_src = without_ext.strip_prefix("src/").unwrap_or(without_ext);
    if without_src.is_empty() {
        return Vec::new();
    }
    let mut segments: Vec<&str> = without_src.split('/').collect();
    if matches!(segments.last(), Some(&("mod" | "main" | "lib"))) {
        segments.pop();
    }
    segments.into_iter().map(str::to_owned).collect()
}

/// Walk items, collecting `#[edge]`-marked functions and descending into inline
/// modules (`mod routes { ... }`). `mod routes;` has no items here — its file is
/// visited separately by the directory walk.
///
/// `module_path` is the chain of inline module names enclosing the items
/// currently being walked. It is pushed before, and popped after, each
/// `mod routes { ... }` recursion, so it always reads as the current nesting.
fn scan_items(
    items: &[syn::Item],
    file: &str,
    module_path: &mut Vec<String>,
    default_features: &BTreeSet<String>,
    scan: &mut EdgeScan,
) {
    for item in items {
        match item {
            syn::Item::Fn(item_fn) => {
                if let Some(found) = edge_fn(
                    &item_fn.attrs,
                    &item_fn.sig,
                    file,
                    module_path,
                    default_features,
                ) {
                    scan.functions.push(found);
                }
            }
            syn::Item::Mod(item_mod) => {
                if let Some((_, inner)) = &item_mod.content {
                    module_path.push(item_mod.ident.to_string());
                    scan_items(inner, file, module_path, default_features, scan);
                    module_path.pop();
                }
            }
            _ => {}
        }
    }
}

/// The last `::` segment of an attribute path, e.g. `edge` for
/// `#[autumn_web::edge]`. Attribute paths with generic arguments are not
/// attribute macros, so a plain segment match is enough.
fn attr_name(attr: &syn::Attribute) -> Option<String> {
    attr.path().segments.last().map(|s| s.ident.to_string())
}

/// Build an [`EdgeFn`] when `attrs` contains an `#[edge]` marker and no
/// `#[cfg(...)]` on the same function definitely excludes it.
fn edge_fn(
    attrs: &[syn::Attribute],
    sig: &syn::Signature,
    file: &str,
    module_path: &[String],
    default_features: &BTreeSet<String>,
) -> Option<EdgeFn> {
    let names: Vec<String> = attrs.iter().filter_map(attr_name).collect();
    if !names.iter().any(|n| n == "edge") {
        return None;
    }
    // Several `#[cfg(...)]` attributes on one function are ANDed, like real
    // Rust: any one of them resolving to definitely false excludes it.
    let cfg_excludes = attrs
        .iter()
        .any(|attr| eval_cfg_attr(attr, default_features) == Some(false));
    if cfg_excludes {
        return None;
    }
    let guards: Vec<String> = names
        .into_iter()
        .filter(|n| EDGE_GUARD_ATTRS.contains(&n.as_str()))
        .collect();
    Some(EdgeFn {
        name: sig.ident.to_string(),
        file: file.to_owned(),
        line: sig.ident.span().start().line,
        guards,
        module_path: module_path.to_vec(),
    })
}

/// A `#[cfg(...)]` predicate this scan can fully resolve: built only from
/// `feature = "x"` leaves, combined with `not`, `all`, and `any`.
enum CfgPredicate {
    Feature(String),
    Not(Box<Self>),
    All(Vec<Self>),
    Any(Vec<Self>),
}

impl syn::parse::Parse for CfgPredicate {
    fn parse(input: syn::parse::ParseStream) -> syn::Result<Self> {
        let ident: syn::Ident = input.parse()?;
        if ident == "feature" {
            input.parse::<syn::Token![=]>()?;
            let lit: syn::LitStr = input.parse()?;
            return Ok(Self::Feature(lit.value()));
        }
        if ident == "not" {
            let content;
            syn::parenthesized!(content in input);
            let inner: Self = content.parse()?;
            return Ok(Self::Not(Box::new(inner)));
        }
        if ident == "all" || ident == "any" {
            let content;
            syn::parenthesized!(content in input);
            let list =
                syn::punctuated::Punctuated::<Self, syn::Token![,]>::parse_terminated(&content)?;
            let parts: Vec<Self> = list.into_iter().collect();
            return Ok(if ident == "all" {
                Self::All(parts)
            } else {
                Self::Any(parts)
            });
        }
        // Anything else — `target_os`, `debug_assertions`, a name this scan
        // does not know — is not part of the resolvable grammar.
        Err(input.error("cfg predicate not resolvable by this scan"))
    }
}

impl CfgPredicate {
    /// Evaluate against the crate's default-on feature set. Only called after
    /// a full, successful parse, so every leaf is a plain feature name.
    fn eval(&self, default_features: &BTreeSet<String>) -> bool {
        match self {
            Self::Feature(name) => default_features.contains(name),
            Self::Not(inner) => !inner.eval(default_features),
            Self::All(parts) => parts.iter().all(|p| p.eval(default_features)),
            Self::Any(parts) => parts.iter().any(|p| p.eval(default_features)),
        }
    }
}

/// Try to fully resolve one `#[cfg(...)]` attribute against `default_features`.
///
/// `None` means "stay conservative": a non-`cfg` attribute, a predicate this
/// scan does not parse (`target_os`, ...), or one with trailing tokens it does
/// not understand. A caller treats `None` the same as `Some(true)` — kept in
/// the scan — so only `Some(false)` ever excludes a function.
fn eval_cfg_attr(attr: &syn::Attribute, default_features: &BTreeSet<String>) -> Option<bool> {
    if !attr.path().is_ident("cfg") {
        return None;
    }
    let syn::Meta::List(list) = &attr.meta else {
        return None;
    };
    syn::parse2::<CfgPredicate>(list.tokens.clone())
        .ok()
        .map(|pred| pred.eval(default_features))
}

/// Find every `edge_routes![...]` invocation in a token stream and record the
/// handler identifiers it registers.
///
/// Token-level rather than AST-level because the invocation is equally valid in
/// item position, inside a function body, or nested in another macro's group —
/// and because the registration list is a plain path list, so no parsing beyond
/// "last identifier of each comma-separated entry" is needed.
fn collect_registrations(stream: &TokenStream, scan: &mut EdgeScan) {
    let trees: Vec<TokenTree> = stream.clone().into_iter().collect();
    for (index, tree) in trees.iter().enumerate() {
        match tree {
            TokenTree::Group(group) => collect_registrations(&group.stream(), scan),
            TokenTree::Ident(ident) if ident == "edge_routes" => {
                let bang =
                    matches!(trees.get(index + 1), Some(TokenTree::Punct(p)) if p.as_char() == '!');
                if !bang {
                    // `fn edge_routes()` / a path mention — not an invocation.
                    continue;
                }
                let Some(TokenTree::Group(group)) = trees.get(index + 2) else {
                    continue;
                };
                if group.delimiter() == Delimiter::None {
                    continue;
                }
                scan.registrations += 1;
                for name in registered_idents(&group.stream()) {
                    scan.registered.insert(name);
                }
            }
            _ => {}
        }
    }
}

/// Split a macro argument list on top-level commas and keep each entry's full
/// path text, so `handlers::greet, note,` yields `["handlers::greet",
/// "note"]`.
fn registered_idents(stream: &TokenStream) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    for tree in stream.clone() {
        match tree {
            TokenTree::Punct(p) if p.as_char() == ',' => {
                if !current.is_empty() {
                    out.push(std::mem::take(&mut current));
                }
            }
            TokenTree::Punct(p) if p.as_char() == ':' => current.push(':'),
            TokenTree::Ident(ident) => current.push_str(&ident.to_string()),
            _ => {}
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan_one(src: &str) -> EdgeScan {
        scan_sources(&[("src/main.rs", src)])
    }

    fn scan_one_with_features(src: &str, features: &[&str]) -> EdgeScan {
        let default_features: BTreeSet<String> = features.iter().map(|s| (*s).to_owned()).collect();
        scan_sources_with_features(&[("src/main.rs", src)], &default_features)
    }

    #[test]
    fn bare_edge_attribute_is_found() {
        let scan = scan_one(
            r#"
            #[get("/hello")]
            #[edge]
            pub async fn hello() -> &'static str { "hi" }
            "#,
        );
        assert_eq!(scan.names(), vec!["hello"]);
        assert_eq!(scan.functions[0].file, "src/main.rs");
        assert!(scan.functions[0].guards.is_empty());
        assert!(!scan.is_empty());
    }

    #[test]
    fn qualified_edge_paths_are_recognized() {
        let scan = scan_sources(&[
            ("src/a.rs", "#[autumn_web::edge]\nfn a() {}"),
            ("src/b.rs", "#[macros::edge]\nfn b() {}"),
            ("src/c.rs", "#[edge(needs(kv))]\nfn c() {}"),
        ]);
        assert_eq!(scan.names(), vec!["a", "b", "c"]);
        assert_eq!(scan.files_scanned, 3);
    }

    #[test]
    fn unmarked_functions_are_ignored() {
        let scan = scan_one("#[get(\"/x\")]\nfn plain() {}\n#[edgy]\nfn other() {}");
        assert!(scan.is_empty(), "{:?}", scan.functions);
    }

    #[test]
    fn guard_attributes_on_the_same_fn_are_recorded() {
        let scan = scan_one(
            r#"
            #[get("/dash")]
            #[edge]
            #[secured]
            fn dash() {}

            #[get("/t")]
            #[edge]
            #[autumn_web::throttle(per_minute = 5)]
            fn throttled() {}

            #[get("/ok")]
            #[edge]
            fn ok() {}
            "#,
        );
        let guarded: Vec<&str> = scan.guarded().iter().map(|f| f.name.as_str()).collect();
        assert_eq!(guarded, vec!["dash", "throttled"]);
        assert_eq!(scan.functions[0].guards, vec!["secured".to_owned()]);
        assert_eq!(scan.functions[1].guards, vec!["throttle".to_owned()]);
    }

    #[test]
    fn every_guard_attribute_is_detected() {
        for guard in EDGE_GUARD_ATTRS {
            let scan = scan_one(&format!("#[edge]\n#[{guard}]\nfn g() {{}}"));
            assert_eq!(
                scan.guarded().len(),
                1,
                "#[{guard}] must be detected next to #[edge]"
            );
        }
    }

    #[test]
    fn registrations_collect_handler_idents() {
        let scan = scan_one(
            r"
            #[edge]
            fn greet() {}
            #[edge]
            fn note() {}

            pub fn edge_routes() -> Vec<EdgeRoute> {
                autumn_edge::edge_routes![handlers::greet, note,]
            }
            ",
        );
        assert_eq!(scan.registrations, 1);
        // Full path text, not just the last segment.
        assert!(scan.registered.contains("handlers::greet"));
        assert!(scan.registered.contains("note"));
        assert!(scan.unregistered().is_empty());
        assert_eq!(
            scan.registered_fns()
                .iter()
                .map(|f| f.name.as_str())
                .collect::<Vec<_>>(),
            vec!["greet", "note"]
        );
    }

    #[test]
    fn a_function_named_edge_routes_is_not_a_registration() {
        let scan = scan_one(
            r"
            #[edge]
            fn greet() {}
            pub fn edge_routes() -> Vec<EdgeRoute> { Vec::new() }
            ",
        );
        assert_eq!(scan.registrations, 0);
        assert_eq!(
            scan.unregistered()
                .iter()
                .map(|f| f.name.as_str())
                .collect::<Vec<_>>(),
            vec!["greet"]
        );
    }

    #[test]
    fn unregistered_lists_only_the_missing_handlers() {
        let scan = scan_one(
            r"
            #[edge]
            fn greet() {}
            #[edge]
            fn stats() {}
            fn wire() { edge_routes![greet]; }
            ",
        );
        let missing: Vec<String> = scan.unregistered().iter().map(|f| f.location()).collect();
        assert_eq!(missing.len(), 1);
        assert!(
            missing[0].starts_with("stats @ src/main.rs:"),
            "{missing:?}"
        );
    }

    #[test]
    fn inline_modules_are_scanned() {
        let scan = scan_one(
            r"
            mod routes {
                #[edge]
                pub fn nested() {}
            }
            ",
        );
        assert_eq!(scan.names(), vec!["nested"]);
    }

    #[test]
    fn line_numbers_point_at_the_declaration() {
        let scan = scan_one("// header\n\n#[edge]\nfn marked() {}\n");
        assert_eq!(scan.functions[0].line, 4);
    }

    #[test]
    fn unparseable_source_is_skipped() {
        let scan = scan_one("fn broken( {");
        assert!(scan.is_empty());
        assert_eq!(scan.registrations, 0);
    }

    #[test]
    fn registrations_are_found_across_files() {
        let scan = scan_sources(&[
            ("src/handlers.rs", "#[edge]\nfn greet() {}"),
            (
                "src/bin/edge-capsule.rs",
                "fn main() { autumn_edge::serve(edge_routes![greet]); }",
            ),
        ]);
        assert_eq!(scan.registrations, 1);
        assert!(scan.unregistered().is_empty());
    }

    #[test]
    fn resolve_reads_src_tree_relative_to_project_root() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src/routes")).unwrap();
        std::fs::create_dir_all(dir.path().join("target")).unwrap();
        std::fs::write(
            dir.path().join("src/routes/home.rs"),
            "#[edge]\npub fn home() {}\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "fn main() {}\n").unwrap();
        // Build output must never be scanned.
        std::fs::write(dir.path().join("target/gen.rs"), "#[edge]\nfn gen() {}\n").unwrap();

        let scan = resolve_edge_scan(dir.path());
        assert_eq!(scan.names(), vec!["home"]);
        assert_eq!(scan.functions[0].file, "src/routes/home.rs");
        assert_eq!(scan.files_scanned, 2);
    }

    #[test]
    fn resolve_without_src_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let scan = resolve_edge_scan(dir.path());
        assert!(scan.is_empty());
        assert_eq!(scan.files_scanned, 0);
    }

    // --- Item 1: safe-direction #[cfg(...)] evaluation ---

    #[test]
    fn cfg_feature_gated_fn_excluded_when_feature_not_default() {
        let scan = scan_one_with_features(
            r#"
            #[cfg(feature = "unused")]
            #[edge]
            fn hidden() {}
            "#,
            &[],
        );
        assert!(scan.is_empty(), "{:?}", scan.functions);
    }

    #[test]
    fn cfg_feature_gated_fn_included_when_feature_is_default() {
        let scan = scan_one_with_features(
            r#"
            #[cfg(feature = "default-on")]
            #[edge]
            fn shown() {}
            "#,
            &["default-on"],
        );
        assert_eq!(scan.names(), vec!["shown"]);
    }

    #[test]
    fn cfg_not_feature_included_when_feature_absent() {
        let scan = scan_one_with_features(
            r#"
            #[cfg(not(feature = "unused"))]
            #[edge]
            fn shown() {}
            "#,
            &[],
        );
        assert_eq!(scan.names(), vec!["shown"]);
    }

    #[test]
    fn cfg_any_included_when_one_default_feature_present() {
        let scan = scan_one_with_features(
            r#"
            #[cfg(any(feature = "a", feature = "unused"))]
            #[edge]
            fn shown() {}
            "#,
            &["a"],
        );
        assert_eq!(scan.names(), vec!["shown"]);
    }

    #[test]
    fn cfg_all_excludes_when_one_branch_is_not_default() {
        let scan = scan_one_with_features(
            r#"
            #[cfg(all(feature = "a", feature = "unused"))]
            #[edge]
            fn hidden() {}
            "#,
            &["a"],
        );
        assert!(scan.is_empty(), "{:?}", scan.functions);
    }

    #[test]
    fn cfg_non_feature_predicate_stays_conservative() {
        let scan = scan_one_with_features(
            r#"
            #[cfg(target_os = "linux")]
            #[edge]
            fn maybe() {}
            "#,
            &[],
        );
        assert_eq!(
            scan.names(),
            vec!["maybe"],
            "a non-feature cfg predicate must stay conservative (included)"
        );
    }

    #[test]
    fn cfg_multiple_attributes_are_anded() {
        let scan = scan_one_with_features(
            r#"
            #[cfg(feature = "a")]
            #[cfg(feature = "unused")]
            #[edge]
            fn hidden() {}
            "#,
            &["a"],
        );
        assert!(scan.is_empty(), "{:?}", scan.functions);
    }

    #[test]
    fn compute_default_features_expands_transitive_defaults() {
        let manifest = r#"
            [features]
            default = ["a"]
            a = ["b"]
            b = []
        "#;
        let enabled = enabled_features_from_manifest(manifest, &[]);
        assert!(enabled.contains("a"));
        assert!(enabled.contains("b"));
    }

    #[test]
    fn enabled_features_from_manifest_includes_a_requested_feature() {
        let manifest = r"
            [features]
            default = []
            other = []
        ";
        let enabled = enabled_features_from_manifest(manifest, &["premium"]);
        assert!(enabled.contains("premium"));
        assert!(!enabled.contains("other"));
    }

    /// `autumn build -p blog --features blog/extra-routes` forwards
    /// `blog/extra-routes` here, Cargo's package-qualified `--features`
    /// syntax. It must resolve to `extra-routes`, not be dropped as a
    /// dependency-feature reference — dropping it would make the scan miss a
    /// route the real build turns on.
    #[test]
    fn a_package_qualified_requested_feature_resolves_to_its_bare_name() {
        let manifest = r"
            [features]
            default = []
        ";
        let enabled = enabled_features_from_manifest(manifest, &["blog/extra-routes"]);
        assert!(enabled.contains("extra-routes"));
        assert!(!enabled.iter().any(|f| f.contains('/')));
    }

    #[test]
    fn cfg_transitive_default_feature_is_included() {
        let mut features = BTreeSet::new();
        // Mirrors `default = ["a"]`, `a = ["b"]` already expanded by
        // `enabled_features_from_manifest`.
        features.insert("a".to_owned());
        features.insert("b".to_owned());
        let scan = scan_sources_with_features(
            &[(
                "src/main.rs",
                r#"
                #[cfg(feature = "b")]
                #[edge]
                fn shown() {}
                "#,
            )],
            &features,
        );
        assert_eq!(scan.names(), vec!["shown"]);
    }

    #[test]
    fn resolve_edge_scan_honors_cargo_toml_default_features() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            r#"
            [package]
            name = "demo"
            version = "0.1.0"

            [features]
            default = ["a"]
            a = ["b"]
            unused = []
            "#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("src/main.rs"),
            r#"
            #[cfg(feature = "b")]
            #[edge]
            fn shown() {}

            #[cfg(feature = "unused")]
            #[edge]
            fn hidden() {}
            "#,
        )
        .unwrap();

        let scan = resolve_edge_scan(dir.path());
        assert_eq!(scan.names(), vec!["shown"]);
    }

    /// `autumn build --features premium` must not lose a route gated on
    /// `premium` just because `premium` is not in the manifest's own
    /// `default = [...]` — the real build turns it on, so the scan must too
    /// (Codex review on #2739, P1).
    #[test]
    fn resolve_edge_scan_with_features_honors_explicitly_requested_features() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            r#"
            [package]
            name = "demo"
            version = "0.1.0"

            [features]
            default = []
            premium = []
            "#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("src/main.rs"),
            r#"
            #[cfg(feature = "premium")]
            #[edge]
            fn premium_only() {}
            "#,
        )
        .unwrap();

        // Without the requested feature, the route looks cfg'd-out.
        assert!(resolve_edge_scan(dir.path()).is_empty());
        // With it requested, exactly as `--features premium` would forward,
        // the route must be found.
        let scan = resolve_edge_scan_with_features(dir.path(), &["premium"]);
        assert_eq!(scan.names(), vec!["premium_only"]);
    }

    // --- Item 2: qualified registration matching ---

    #[test]
    fn qualified_registration_does_not_match_a_same_named_fn_in_another_module() {
        let scan = scan_one(
            r"
            mod users {
                #[edge]
                pub fn show() {}
            }
            mod admin {
                #[edge]
                pub fn show() {}
            }

            fn wire() { edge_routes![users::show]; }
            ",
        );
        let unregistered: Vec<&EdgeFn> = scan.unregistered();
        assert_eq!(unregistered.len(), 1, "{unregistered:?}");
        assert_eq!(unregistered[0].name, "show");
        assert_eq!(unregistered[0].module_path, vec!["admin".to_owned()]);

        let registered: Vec<&str> = scan
            .registered_fns()
            .iter()
            .map(|f| f.name.as_str())
            .collect();
        assert_eq!(registered, vec!["show"]);
        assert_eq!(
            scan.registered_fns()[0].module_path,
            vec!["users".to_owned()]
        );
    }

    #[test]
    fn qualified_registration_matches_a_genuinely_nested_fn() {
        let scan = scan_one(
            r"
            mod routes {
                #[edge]
                pub fn nested() {}
            }
            fn wire() { edge_routes![routes::nested]; }
            ",
        );
        assert!(scan.unregistered().is_empty(), "{:?}", scan.unregistered());
        assert_eq!(scan.registered_fns().len(), 1);
    }

    /// `edge_routes![crate::users::show]` is the same route as `users::show`
    /// — `crate::` is Cargo's own absolute-path prefix, not part of the
    /// module chain `f.module_path` records. Without stripping it, this
    /// registration would compare `["crate", "users"]` against `["users"]`,
    /// never match, and wrongly warn that `show` is unregistered.
    #[test]
    fn a_crate_qualified_registration_matches_its_module_path() {
        let scan = scan_one(
            r"
            mod users {
                #[edge]
                pub fn show() {}
            }
            fn wire() { edge_routes![crate::users::show]; }
            ",
        );
        assert!(scan.unregistered().is_empty(), "{:?}", scan.unregistered());
        assert_eq!(scan.registered_fns().len(), 1);
    }

    /// `mod users;` / `mod admin;` (separate files, not inline `mod { }`
    /// blocks) must derive the same module path from each file's location, so
    /// a same-named `admin::show` cannot pass as a match for a registration of
    /// `users::show`. This is the out-of-line counterpart to
    /// `qualified_registration_does_not_match_a_same_named_fn_in_another_module`.
    #[test]
    fn module_path_is_derived_from_an_out_of_line_modules_file() {
        let scan = scan_sources(&[
            ("src/users.rs", "#[edge]\npub fn show() {}\n"),
            ("src/admin.rs", "#[edge]\npub fn show() {}\n"),
            (
                "src/lib.rs",
                "mod users; mod admin; fn wire() { edge_routes![users::show]; }",
            ),
        ]);

        let unregistered: Vec<&EdgeFn> = scan.unregistered();
        assert_eq!(unregistered.len(), 1, "{unregistered:?}");
        assert_eq!(unregistered[0].name, "show");
        assert_eq!(unregistered[0].module_path, vec!["admin".to_owned()]);

        let registered = scan.registered_fns();
        assert_eq!(registered.len(), 1, "{registered:?}");
        assert_eq!(registered[0].name, "show");
        assert_eq!(registered[0].module_path, vec!["users".to_owned()]);
    }
}
