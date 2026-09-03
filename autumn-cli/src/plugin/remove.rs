//! Removal planning for `autumn plugin remove` — the mirror image of
//! [`super::install`] (issue #1631).
//!
//! Symmetry is the point: `plugin add` (issue #1606) made the install
//! machine-applied, so removal can be machine-reversed rather than the manual
//! scavenger hunt every comparable framework leaves behind. The same
//! discipline applies in both directions — every decision that can refuse the
//! removal is made *before* a single [`crate::generate::emit::Action`] is
//! queued, so a refusal leaves the app byte-identical, and a removal the CLI
//! cannot make confidently changes nothing at all.

use std::path::Path;

use crate::generate::emit::Plan;

use super::catalog::CatalogEntry;
use super::install::{PluginError, dependency_line, manifest_path};

/// One of the two wires `plugin add` writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wire {
    /// The `[dependencies]` entry in `Cargo.toml`.
    Dependency,
    /// The `.plugin(...)` / `.with_blob_store(...)` call in `src/main.rs`.
    Mount,
}

impl Wire {
    /// How the wire is named in the report.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Dependency => "the Cargo.toml dependency",
            Self::Mount => "the builder-chain mount",
        }
    }
}

/// Why a dependency this command found is still in `Cargo.toml` afterwards.
///
/// The distinction is the exit code: keeping a dependency the app still uses is
/// the *correct*, complete outcome, while a dependency written in a shape this
/// command will not rewrite leaves work for the user to do by hand — the same
/// situation [`RemoveOutcome::Manual`] reports, and it exits the same way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DependencyKept {
    /// The app's own code still names the crate; removing the dependency would
    /// stop that code compiling. Nothing left for the user to do.
    StillUsed(String),
    /// Declared in a form the manifest rewriter does not touch. The user has to
    /// delete the line themselves.
    NotEditable(String),
}

impl DependencyKept {
    /// The message to print.
    #[must_use]
    pub fn reason(&self) -> &str {
        match self {
            Self::StillUsed(reason) | Self::NotEditable(reason) => reason,
        }
    }

    /// Whether this leaves the user something to do by hand.
    #[must_use]
    pub const fn needs_a_hand_edit(&self) -> bool {
        matches!(self, Self::NotEditable(_))
    }
}

/// What the plugin leaves behind in the database when its code is unwired.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DataResidue {
    /// Migration versions the plugin declares.
    pub migrations: Vec<String>,
    /// Tables the plugin creates and owns, in safe drop order (dependents
    /// first) so the statements can be applied top to bottom.
    pub tables: Vec<String>,
}

impl DataResidue {
    /// Whether the plugin owns any database state at all.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.migrations.is_empty() && self.tables.is_empty()
    }
}

/// What `plugin remove` decided to do.
#[derive(Debug)]
pub enum RemoveOutcome {
    /// At least one wire was found and can be removed.
    Removed {
        /// Filesystem actions to execute.
        plan: Box<Plan>,
        /// Wires this run removes.
        removed: Vec<Wire>,
        /// Wires `plugin add` writes that were not found — a partially wired
        /// app (a manual install, or a half-finished one).
        missing: Vec<Wire>,
        /// Set when the dependency is still in `Cargo.toml` afterwards, with
        /// the reason to print and whether it needs the user's attention.
        dependency_retained: Option<DependencyKept>,
        /// Database state the removal deliberately leaves in place.
        residue: DataResidue,
    },
    /// Neither wire is present — an idempotent no-op.
    NotInstalled {
        /// Database state a previous install may still have left behind.
        residue: DataResidue,
    },
    /// Nothing was changed; the user deletes the printed lines by hand.
    Manual {
        /// Why the automatic edit was declined.
        reason: String,
        /// The `[dependencies]` line to delete, when there is one.
        dependency_line: Option<String>,
        /// The builder-chain lines to delete.
        mount_snippet: String,
        /// Database state removal would leave in place either way.
        residue: DataResidue,
    },
}

/// The database state `entry` declares it owns.
#[must_use]
pub fn residue_for(entry: &CatalogEntry) -> DataResidue {
    DataResidue {
        migrations: entry.migrations.iter().map(|m| (*m).to_owned()).collect(),
        tables: entry.tables.iter().map(|t| (*t).to_owned()).collect(),
    }
}

/// The SQL `--drop-data` would run for `entry`, in application order.
#[must_use]
pub fn drop_data_statements(entry: &CatalogEntry) -> Vec<String> {
    let mut out = Vec::new();
    for table in entry.tables {
        out.push(format!("DROP TABLE IF EXISTS {table};"));
    }
    if !entry.migrations.is_empty() {
        let versions = entry
            .migrations
            .iter()
            .map(|version| format!("'{}'", version.split('_').next().unwrap_or(version)))
            .collect::<Vec<_>>()
            .join(", ");
        out.push(format!(
            "DELETE FROM __diesel_schema_migrations WHERE version IN ({versions});"
        ));
    }
    out
}

/// The marker comment `plugin add` writes above every mount it splices.
const MOUNT_MARKER: &str = "// added by `autumn plugin add";

/// The byte range of `entry`'s mount in `main_rs`, including the whole line it
/// starts on, the line its closing paren ends on, and any immediately
/// preceding "added by `autumn plugin add`" marker comment.
///
/// The scan runs against [`crate::rust_source::mask_non_code`], whose output is
/// byte-for-byte the same length as its input, so an offset found in the mask
/// is an offset into `main_rs` — and a commented-out or string-literal mount
/// can never be excised as if it were real code.
///
/// `None` when there is no such span, which is the whole safety story of AC #4:
///
/// - The plugin is mounted, but not through a literal
///   `mount_call(… mount_arg …)` — a plugin built into a variable and mounted
///   as `.plugin(configured)` is a real mount this code cannot see the type in,
///   and deleting the `let` that built it is far past what a line-oriented edit
///   can justify.
/// - The mount shares its line with other builder calls (a one-line chain).
///   Removing it would mean rewriting the line rather than deleting lines, and
///   a wrong rewrite leaves the app not compiling — exactly what `plugin add`
///   promises never to do, in reverse.
///
/// Both cases become the documented manual fallback: nothing is changed, and
/// the lines to delete are printed.
fn mount_span(main_rs: &str, entry: &CatalogEntry) -> Option<std::ops::Range<usize>> {
    let masked = crate::rust_source::mask_non_code(main_rs);
    // `starts_with`, not `contains`: the argument must BE this plugin, not
    // merely mention it. `.plugin(MyWrapper::new(admin::AdminPlugin::new(), …))`
    // contains the type path but is the user's wrapper mount, and excising it
    // would delete their code — and still compile, so nothing would surface
    // the loss.
    if let Some((open, close)) = super::install::mount_call_span(&masked, entry, |argument| {
        argument.trim_start().starts_with(entry.mount_arg)
    }) {
        let call_start = open + 1 - entry.mount_call.len();
        let line_start = masked[..call_start].rfind('\n').map_or(0, |at| at + 1);
        // Past the closing paren, up to and including the newline that ends
        // its line — a mount is removed by deleting whole lines.
        let line_end = masked[close..]
            .find('\n')
            .map_or(masked.len(), |at| close + at + 1);
        // Nothing but whitespace may share those lines: a `.routes(…)` before
        // or after this call would be deleted along with the mount.
        if masked[line_start..call_start].trim().is_empty()
            && masked[close + 1..line_end].trim().is_empty()
        {
            return Some(marker_start(main_rs, line_start, entry.crate_name)..line_end);
        }
        return None;
    }
    // Not found as `mount_call(mount_arg…)`. The one remaining shape this can
    // excise is the mount `plugin add` writes VERBATIM — `autumn-storage-s3`'s
    // is a `.with_blob_store({ … })` block whose argument is a brace expression,
    // not the type path — and only while it is still byte-identical to what was
    // written. An edited block is the user's code now, and goes to the manual
    // fallback rather than being guessed at.
    verbatim_mount_span(main_rs, &masked, entry)
}

/// The span of a byte-identical copy of `entry.mount` that is real code.
///
/// The literal search runs over the ORIGINAL source, because the marker comment
/// is part of `entry.mount` and the mask has blanked it. That alone would let a
/// commented-out or quoted copy of the mount — Autumn's own docs ship these
/// snippets — be excised while the live mount below it survived, and the report
/// would still say the plugin was removed. So every candidate is confirmed
/// against the mask: the call and the type path must still be *there* in the
/// masked text, which they are not inside a comment or a string literal.
fn verbatim_mount_span(
    main_rs: &str,
    masked: &str,
    entry: &CatalogEntry,
) -> Option<std::ops::Range<usize>> {
    let mut from = 0usize;
    while let Some(found) = main_rs[from..].find(entry.mount) {
        let at = from + found;
        let end = at + entry.mount.len();
        let starts_a_line = at == 0 || main_rs.as_bytes()[at - 1] == b'\n';
        if starts_a_line
            && masked[at..end].contains(entry.mount_call)
            && masked[at..end].contains(entry.mount_arg)
        {
            return Some(at..end);
        }
        from = at + 1;
    }
    None
}

/// `line_start`, extended back over the "added by `autumn plugin add
/// <crate>`" marker comment line when the install wrote one.
///
/// Read from the ORIGINAL source, not the mask: the marker is a comment, and
/// the mask has blanked its text away. Matched on the crate name too, so a
/// marker belonging to a different plugin — two mounts stacked with no blank
/// line between them — is never swallowed with this one.
fn marker_start(main_rs: &str, line_start: usize, crate_name: &str) -> usize {
    if line_start == 0 {
        return line_start;
    }
    let before = &main_rs[..line_start - 1];
    let previous_start = before.rfind('\n').map_or(0, |at| at + 1);
    let previous = main_rs[previous_start..line_start].trim();
    // Byte-exact, not `starts_with`: a marker the user annotated
    // (`… autumn-admin-plugin` — KEEP, see ticket 42`) is their comment now,
    // and taking it with the mount would delete a note they left on purpose.
    if previous == format!("{MOUNT_MARKER} {crate_name}`") {
        previous_start
    } else {
        line_start
    }
}

/// `main_rs` with every excisable mount of `entry` removed, or `None` when
/// there is none to remove.
///
/// Loops rather than removing the first: a chain can carry the same plugin
/// twice (a copy-paste, or two `plugin add` runs against a chain that was
/// edited in between), and stopping at the first would leave the plugin mounted
/// while the report said it had been removed.
///
/// A user's trailing comment on a mount line goes with the line, by design —
/// a mount is removed by deleting whole lines, and half a line is not a
/// removal.
#[must_use]
pub fn remove_mount(main_rs: &str, entry: &CatalogEntry) -> Option<String> {
    let mut current = main_rs.to_owned();
    let mut removed_any = false;
    while let Some(span) = mount_span(&current, entry) {
        current.replace_range(span, "");
        removed_any = true;
    }
    removed_any.then_some(current)
}

/// The first file that still names `crate_name`, across every tree a Cargo
/// target can be built from.
///
/// Wider than [`crate::generate::emit::crate_reference_site`]'s
/// `src`/`tests`/`benches`, because this is the only gate standing between a
/// user-facing destructive edit and a build that stops working: a plugin used
/// solely from `build.rs` or an example is still a plugin the manifest entry is
/// holding up.
fn still_referenced(
    root: &Path,
    crate_name: &str,
    overrides: &std::collections::HashMap<std::path::PathBuf, String>,
) -> Option<std::path::PathBuf> {
    if let Some(site) =
        crate::generate::emit::crate_reference_site(root, crate_name, &[], overrides)
    {
        return Some(site);
    }
    let ident = crate_name.replace('-', "_");
    let markers = [format!("{ident}::"), format!("use {ident}")];
    explicit_target_roots(root)
        .into_iter()
        .find(|path| file_or_tree_contains(path, &markers, root))
}

/// Every source path the manifest names explicitly, outside the conventional
/// trees: the build script and any `path = "…"` on a `[lib]`, `[[bin]]`,
/// `[[example]]`, `[[test]]` or `[[bench]]` target.
///
/// Cargo lets a target live anywhere — `[[bin]] path = "cmd/server.rs"` is
/// valid and invisible to a `src`/`tests`/`benches`/`examples` sweep. That
/// target compiles against the same `[dependencies]`, so a plugin it still uses
/// must keep its dependency line just as surely as one `src/` uses.
fn explicit_target_roots(root: &Path) -> Vec<std::path::PathBuf> {
    let mut roots = vec![root.join("build.rs")];
    let Ok(manifest) = std::fs::read_to_string(root.join("Cargo.toml")) else {
        return roots;
    };
    let Ok(table) = toml::from_str::<toml::Table>(&manifest) else {
        return roots;
    };
    // A custom build-script path (`[package] build = "…"`) replaces `build.rs`.
    if let Some(build) = table
        .get("package")
        .and_then(|package| package.get("build"))
        .and_then(toml::Value::as_str)
    {
        roots.push(root.join(build));
    }
    let mut push_path = |value: &toml::Value| {
        if let Some(path) = value.get("path").and_then(toml::Value::as_str) {
            roots.push(root.join(path));
        }
    };
    if let Some(lib) = table.get("lib") {
        push_path(lib);
    }
    for kind in ["bin", "example", "test", "bench"] {
        if let Some(targets) = table.get(kind).and_then(toml::Value::as_array) {
            for target in targets {
                push_path(target);
            }
        }
    }
    roots
}

/// Whether `path` — or, when it sits in a directory of its own, that whole
/// directory tree — contains any of `markers`.
///
/// The directory sweep is what catches a target's sibling modules
/// (`cmd/server.rs` plus `cmd/routes.rs`). It is deliberately skipped when the
/// file sits directly at the project root, where "the whole tree" would mean
/// the entire checkout — `target/` included.
fn file_or_tree_contains(path: &Path, markers: &[String], root: &Path) -> bool {
    let contains = |file: &Path| {
        std::fs::read_to_string(file)
            .is_ok_and(|src| markers.iter().any(|marker| src.contains(marker.as_str())))
    };
    if contains(path) {
        return true;
    }
    match path.parent() {
        Some(parent) if parent != root => rs_files_under(parent).iter().any(|file| contains(file)),
        _ => false,
    }
}

/// Every `.rs` file under `dir`, recursively.
fn rs_files_under(dir: &Path) -> Vec<std::path::PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.is_dir() {
            out.extend(rs_files_under(&path));
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
    out
}

/// Whether `remove_cargo_dependencies` actually took `crate_name` out of
/// `manifest_src` cleanly.
///
/// [`crate::generate::model::remove_cargo_dependencies`] is line-based, while
/// [`super::install::dependency_present`] parses the manifest — so a
/// dependency written as a MULTI-LINE inline table
/// (`foo = { version = "1", features = [\n  "bar",\n] }`) reads as present but
/// has only its first line deleted, leaving a `Cargo.toml` Cargo can no longer
/// parse. Nothing may be written unless the result still parses AND no longer
/// declares the crate; anything else is reported as a dependency the user has
/// to delete by hand.
fn dependency_cleanly_removed(updated: &str, crate_name: &str) -> bool {
    toml::from_str::<toml::Table>(updated).is_ok()
        && !super::install::dependency_present(updated, crate_name)
}

/// The reason text for a dependency this command declines to rewrite.
fn undeletable_dependency_reason(crate_name: &str) -> DependencyKept {
    DependencyKept::NotEditable(format!(
        "The {crate_name} dependency was left in Cargo.toml: it is not declared as a \
         plain `{crate_name} = \"…\"` line (a `[dependencies.{crate_name}]` subtable, or a \
         multi-line inline table), and rewriting that shape safely is beyond this \
         command. Delete it by hand"
    ))
}

/// Plan the removal of `entry` from the project at `root`.
///
/// Ordering is the contract, mirrored from [`super::install::plan_add`]. Every
/// refusal is decided before a single [`crate::generate::emit::Action`] exists,
/// so a refusal leaves the app byte-identical. The two writes are queued
/// **mount first**, exactly as the install queues them: `Plan::execute` writes
/// in order with no rollback, so a mid-execute I/O failure leaves a dependency
/// with no mount — an unused dependency, which still compiles — rather than a
/// mount with no dependency, which does not.
///
/// The database is never touched here, at all. `--drop-data` is a separate,
/// confirmed step on top of this plan (AC #2).
///
/// # Errors
///
/// [`PluginError::NotInProject`] when `root` has no `Cargo.toml`, or an I/O
/// error reading the manifest.
pub fn plan_remove(root: &Path, entry: &CatalogEntry) -> Result<RemoveOutcome, PluginError> {
    let manifest = manifest_path(root);
    if !manifest.is_file() {
        return Err(PluginError::NotInProject);
    }
    let manifest_src = std::fs::read_to_string(&manifest)?;
    let main_path = root.join("src").join("main.rs");
    let main_src = std::fs::read_to_string(&main_path).unwrap_or_default();

    let residue = residue_for(entry);
    let has_dependency = super::install::dependency_present(&manifest_src, entry.crate_name);
    let is_mounted = super::install::mount_present(&main_src, entry);

    // AC #5: removing something that was never installed is a no-op that says
    // so, mirroring `add`'s idempotency in the other direction.
    if !has_dependency && !is_mounted {
        return Ok(RemoveOutcome::NotInstalled { residue });
    }

    let unmounted_src = if is_mounted {
        match remove_mount(&main_src, entry) {
            Some(updated) => Some(updated),
            // AC #4: the builder chain cannot be edited confidently, so NOTHING
            // is edited — not even the dependency, whose removal would turn a
            // mount this command could not reach into a compile error.
            None => {
                return Ok(RemoveOutcome::Manual {
                    reason: format!(
                        "could not identify `{}`'s mount in {} as a single builder-chain call — nothing was changed",
                        entry.crate_name,
                        crate::generate::emit::relative_display(&main_path, root)
                    ),
                    dependency_line: has_dependency
                        .then(|| declared_dependency_line(&manifest_src, entry.crate_name)),
                    mount_snippet: entry.mount.trim_end_matches('\n').to_owned(),
                    residue,
                });
            }
        }
    } else {
        None
    };

    // Would stripping the dependency break code the user wrote by hand? The
    // check runs against the POST-removal `main.rs`, so this command's own
    // mount never counts as a reason to keep the dependency it is removing.
    let mut overrides = std::collections::HashMap::new();
    if let Some(updated) = &unmounted_src {
        overrides.insert(main_path.clone(), updated.clone());
    }
    let retained_by = if has_dependency {
        still_referenced(root, entry.crate_name, &overrides)
    } else {
        None
    };

    let mut plan = Plan::new(root);
    let mut removed = Vec::new();
    let mut missing = Vec::new();

    if let Some(updated) = unmounted_src {
        plan.modify(main_path, updated);
        removed.push(Wire::Mount);
    } else {
        missing.push(Wire::Mount);
    }

    let dependency_retained = match (has_dependency, &retained_by) {
        (true, None) => {
            let updated = crate::generate::model::remove_cargo_dependencies(
                &manifest_src,
                &[entry.crate_name],
            );
            if updated != manifest_src && dependency_cleanly_removed(&updated, entry.crate_name) {
                plan.modify(manifest, updated);
                removed.push(Wire::Dependency);
                None
            } else {
                Some(undeletable_dependency_reason(entry.crate_name))
            }
        }
        (true, Some(site)) => Some(DependencyKept::StillUsed(format!(
            "The {} dependency was kept: {} still names the crate, and removing it would \
             stop that code compiling",
            entry.crate_name,
            crate::generate::emit::relative_display(site, root)
        ))),
        (false, _) => {
            missing.push(Wire::Dependency);
            None
        }
    };

    Ok(RemoveOutcome::Removed {
        plan: Box::new(plan),
        removed,
        missing,
        dependency_retained,
        residue,
    })
}

/// The `[dependencies]` line `plugin remove` reports deleting, rendered from
/// what the manifest actually declares.
fn declared_dependency_line(manifest: &str, crate_name: &str) -> String {
    super::install::declared_dependency_version(manifest, crate_name).map_or_else(
        || format!("{crate_name} = …"),
        |version| dependency_line(crate_name, &version),
    )
}

/// What `--drop-data` decided to do, worked out before anything runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DropDataDecision {
    /// The plugin declares no database state — there is nothing to drop.
    NothingToDrop,
    /// Apply these statements to the database at the resolved URL.
    Run {
        /// The database to apply them to.
        url: String,
        /// The statements, in application order.
        statements: Vec<String>,
    },
    /// The statements are printed for the user to apply: either no database is
    /// configured, or it is a backend this command will not drive blind.
    PrintOnly {
        /// Why the command is not applying them itself.
        reason: String,
        /// The statements, in application order.
        statements: Vec<String>,
    },
}

/// Decide what `--drop-data` does for `entry` against `database_url`.
///
/// Every reason not to run the statements is settled *here*, before the
/// confirmation prompt — asking "are you sure?" and then failing to connect is
/// the worst possible order for a destructive command.
#[must_use]
pub fn decide_drop_data(entry: &CatalogEntry, database_url: Option<&str>) -> DropDataDecision {
    let statements = drop_data_statements(entry);
    if statements.is_empty() {
        return DropDataDecision::NothingToDrop;
    }
    let Some(url) = database_url else {
        return DropDataDecision::PrintOnly {
            reason: "no database is configured here, so there is nothing to connect to".to_owned(),
            statements,
        };
    };
    if !is_postgres_url(url) {
        return DropDataDecision::PrintOnly {
            // Naming the backend, not just refusing: a SQLite app's owner needs
            // to know this is a "run it yourself" answer, not a broken URL.
            reason: format!(
                "`--drop-data` drives Postgres only, and this app's database is {}",
                backend_label(url)
            ),
            statements,
        };
    }
    DropDataDecision::Run {
        url: url.to_owned(),
        statements,
    }
}

/// Whether `url` names a Postgres database, by the schemes Diesel accepts.
fn is_postgres_url(url: &str) -> bool {
    let lowered = url.to_ascii_lowercase();
    lowered.starts_with("postgres://") || lowered.starts_with("postgresql://")
}

/// A human name for the backend `url` points at, for the refusal message.
fn backend_label(url: &str) -> String {
    let lowered = url.to_ascii_lowercase();
    lowered.split_once("://").map_or_else(
        || "not a recognisable database URL".to_owned(),
        |(scheme, _)| scheme.to_owned(),
    )
}

/// Apply `statements` to the Postgres database at `url`, in order.
///
/// # Errors
///
/// The connection or statement error, as a message ready to print.
pub fn execute_drop_data(url: &str, statements: &[String]) -> Result<(), String> {
    use diesel::connection::SimpleConnection as _;
    use diesel::{Connection as _, PgConnection};

    let mut conn = PgConnection::establish(url)
        .map_err(|err| format!("could not connect to the database: {err}"))?;
    // One transaction for the whole set. Postgres has transactional DDL, so
    // this costs nothing and removes the state no repair path covers: tables
    // dropped while `__diesel_schema_migrations` still records their migration
    // as applied, which makes `diesel migration run` refuse to recreate them.
    conn.transaction(|conn| {
        for statement in statements {
            conn.batch_execute(statement).map_err(|err| {
                diesel::result::Error::QueryBuilderError(
                    format!("`{statement}` failed: {err}").into(),
                )
            })?;
        }
        Ok(())
    })
    .map_err(|err: diesel::result::Error| err.to_string())
}

/// Plan the removal of a community crate from the project at `root`.
///
/// Dependency-only, because `plugin add` is dependency-only for a community
/// crate: nothing outside that crate can verify it exposes `<Name>Plugin`, so
/// the mount was printed for the user to paste, never spliced — and what was
/// never written cannot be confidently unwritten.
///
/// The reference check does the real work here. A hand-pasted mount *is* a
/// reference to the crate, so it keeps the dependency alive on its own: the
/// removal degrades to "delete your mount first", instead of stripping a
/// dependency the builder chain still needs.
///
/// # Errors
///
/// [`PluginError::NotInProject`] when `root` has no `Cargo.toml`, or an I/O
/// error reading the manifest.
pub fn plan_remove_community(root: &Path, crate_name: &str) -> Result<RemoveOutcome, PluginError> {
    let manifest = manifest_path(root);
    if !manifest.is_file() {
        return Err(PluginError::NotInProject);
    }
    let manifest_src = std::fs::read_to_string(&manifest)?;
    if !super::install::dependency_present(&manifest_src, crate_name) {
        return Ok(RemoveOutcome::NotInstalled {
            // A community crate declares nothing about its schema, so nothing
            // can be reported here. `run_remove` says as much in prose rather
            // than letting an empty list read as "owns no data".
            residue: DataResidue::default(),
        });
    }

    let overrides = std::collections::HashMap::new();
    if let Some(site) = still_referenced(root, crate_name, &overrides) {
        return Ok(RemoveOutcome::Removed {
            plan: Box::new(Plan::new(root)),
            removed: Vec::new(),
            missing: Vec::new(),
            dependency_retained: Some(DependencyKept::StillUsed(format!(
                "The {crate_name} dependency was kept: {} still names the crate. `plugin add` \
                 never wrote that mount, so `remove` will not delete it — delete it yourself, \
                 then re-run",
                crate::generate::emit::relative_display(&site, root)
            ))),
            residue: DataResidue::default(),
        });
    }

    let updated = crate::generate::model::remove_cargo_dependencies(&manifest_src, &[crate_name]);
    let mut plan = Plan::new(root);
    let mut removed = Vec::new();
    let mut dependency_retained = None;
    if updated != manifest_src && dependency_cleanly_removed(&updated, crate_name) {
        plan.modify(manifest, updated);
        removed.push(Wire::Dependency);
    } else {
        dependency_retained = Some(undeletable_dependency_reason(crate_name));
    }
    Ok(RemoveOutcome::Removed {
        plan: Box::new(plan),
        removed,
        missing: Vec::new(),
        dependency_retained,
        residue: DataResidue::default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::catalog;

    const SCAFFOLD_MAIN: &str = r"use autumn_web::prelude::*;

#[autumn_web::main]
async fn main() {
    let app = autumn_web::app()
        // added by `autumn plugin add autumn-admin-plugin`
        .plugin(autumn_admin_plugin::AdminPlugin::new())
        .routes(routes![index])
        .migrations(MIGRATIONS);

    app
        .run()
        .await;
}
";

    /// The scaffold `plugin add` splices into, with no plugin mounted yet.
    const SCAFFOLD_MAIN_BARE: &str = r"use autumn_web::prelude::*;

#[autumn_web::main]
async fn main() {
    let app = autumn_web::app()
        .routes(routes![index])
        .migrations(MIGRATIONS);

    app
        .run()
        .await;
}
";

    const SCAFFOLD_CARGO: &str = r#"[package]
name = "demo"
version = "0.1.0"
edition = "2024"

[dependencies]
autumn-web = "0.7.0"
autumn-admin-plugin = "0.7.0"
maud = { version = "0.27", features = ["axum"] }
"#;

    fn admin() -> &'static CatalogEntry {
        catalog::lookup("autumn-admin-plugin").expect("admin entry")
    }

    fn media() -> &'static CatalogEntry {
        catalog::lookup("autumn-media-plugin").expect("media entry")
    }

    fn fake_project(main_rs: &str, cargo: &str) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), cargo).unwrap();
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::write(tmp.path().join("src/main.rs"), main_rs).unwrap();
        tmp
    }

    // ── AC #1: remove reverses both wires ────────────────────────────────────

    #[test]
    fn the_mount_and_its_marker_comment_are_excised_together() {
        let out = remove_mount(SCAFFOLD_MAIN, admin()).expect("excisable");
        assert!(!out.contains("AdminPlugin"), "{out}");
        assert!(!out.contains("added by `autumn plugin add"), "{out}");
        // Everything else survives byte-for-byte.
        assert!(out.contains(".routes(routes![index])"), "{out}");
        assert!(out.contains("let app = autumn_web::app()"), "{out}");
        assert!(out.contains(".migrations(MIGRATIONS)"), "{out}");
    }

    /// A configured mount spans several lines; excising one line would leave a
    /// dangling paren. The whole balanced-paren span goes.
    #[test]
    fn a_configured_multi_line_mount_is_excised_whole() {
        let main = "#[autumn_web::main]\nasync fn main() {\n    let app = autumn_web::app()\n        .plugin(\n            autumn_admin_plugin::AdminPlugin::new()\n                .require_role(\"staff\"),\n        )\n        .routes(routes![index]);\n}\n";
        let out = remove_mount(main, admin()).expect("excisable");
        assert!(!out.contains("AdminPlugin"), "{out}");
        assert!(!out.contains("require_role"), "{out}");
        assert!(out.contains(".routes(routes![index]);"), "{out}");
        assert!(!out.contains(".plugin("), "{out}");
    }

    /// The `autumn-storage-s3` mount is a whole block with an `.await` inside;
    /// it must come out in one piece.
    #[test]
    fn the_storage_block_mount_is_excised_whole() {
        let entry = catalog::lookup("autumn-storage-s3").unwrap();
        let main = format!(
            "#[autumn_web::main]\nasync fn main() {{\n    let app = autumn_web::app()\n{}        .routes(routes![index]);\n}}\n",
            entry.mount
        );
        let out = remove_mount(&main, entry).expect("excisable");
        assert!(!out.contains("S3BlobStore"), "{out}");
        assert!(!out.contains("with_blob_store"), "{out}");
        assert!(out.contains(".routes(routes![index]);"), "{out}");
    }

    // ── AC #4: safe degradation ──────────────────────────────────────────────

    /// A mount built into a variable is a real mount that this code cannot
    /// excise: the `.plugin(configured)` call does not name the type. Nothing
    /// is changed.
    #[test]
    fn a_mount_through_a_variable_is_not_excised() {
        let main = "#[autumn_web::main]\nasync fn main() {\n    let configured = autumn_admin_plugin::AdminPlugin::new().require_role(\"staff\");\n    let app = autumn_web::app()\n        .plugin(configured)\n        .routes(routes![index]);\n}\n";
        assert!(remove_mount(main, admin()).is_none());
    }

    /// A mount sharing its line with other builder calls cannot be excised by
    /// deleting lines, and rewriting the line is exactly the kind of guess
    /// that leaves an app not compiling.
    #[test]
    fn a_mount_sharing_a_line_with_other_calls_is_not_excised() {
        let main = "#[autumn_web::main]\nasync fn main() {\n    autumn_web::app().plugin(autumn_admin_plugin::AdminPlugin::new()).routes(routes![index]).run().await;\n}\n";
        assert!(remove_mount(main, admin()).is_none());
    }

    /// A comment that merely *mentions* the mount is not a mount.
    #[test]
    fn a_commented_out_mount_is_not_excised() {
        let main = "#[autumn_web::main]\nasync fn main() {\n    let app = autumn_web::app()\n        // .plugin(autumn_admin_plugin::AdminPlugin::new())\n        .routes(routes![index]);\n}\n";
        assert!(remove_mount(main, admin()).is_none());
    }

    // ── AC #1 end-to-end planning ────────────────────────────────────────────

    #[test]
    fn a_fully_wired_plugin_has_both_wires_removed() {
        let tmp = fake_project(SCAFFOLD_MAIN, SCAFFOLD_CARGO);
        let outcome = plan_remove(tmp.path(), admin()).unwrap();
        let RemoveOutcome::Removed {
            removed, missing, ..
        } = outcome
        else {
            panic!("expected Removed, got {outcome:?}");
        };
        assert!(removed.contains(&Wire::Dependency), "{removed:?}");
        assert!(removed.contains(&Wire::Mount), "{removed:?}");
        assert!(missing.is_empty(), "{missing:?}");
    }

    // ── AC #5: idempotent no-op ──────────────────────────────────────────────

    #[test]
    fn removing_a_plugin_that_is_not_installed_is_a_no_op() {
        let tmp = fake_project(
            "#[autumn_web::main]\nasync fn main() {\n    let app = autumn_web::app();\n}\n",
            "[package]\nname = \"demo\"\n\n[dependencies]\nautumn-web = \"0.7.0\"\n",
        );
        assert!(matches!(
            plan_remove(tmp.path(), admin()).unwrap(),
            RemoveOutcome::NotInstalled { .. }
        ));
    }

    // ── AC #4: partial installs ──────────────────────────────────────────────

    #[test]
    fn a_dependency_without_a_mount_is_removed_and_the_mount_reported_missing() {
        let tmp = fake_project(
            "#[autumn_web::main]\nasync fn main() {\n    let app = autumn_web::app()\n        .routes(routes![index]);\n}\n",
            SCAFFOLD_CARGO,
        );
        let outcome = plan_remove(tmp.path(), admin()).unwrap();
        let RemoveOutcome::Removed {
            removed, missing, ..
        } = outcome
        else {
            panic!("expected Removed, got {outcome:?}");
        };
        assert_eq!(removed, vec![Wire::Dependency]);
        assert_eq!(missing, vec![Wire::Mount]);
    }

    #[test]
    fn a_mount_without_a_dependency_is_removed_and_the_dependency_reported_missing() {
        let tmp = fake_project(
            SCAFFOLD_MAIN,
            "[package]\nname = \"demo\"\n\n[dependencies]\nautumn-web = \"0.7.0\"\n",
        );
        let outcome = plan_remove(tmp.path(), admin()).unwrap();
        let RemoveOutcome::Removed {
            removed, missing, ..
        } = outcome
        else {
            panic!("expected Removed, got {outcome:?}");
        };
        assert_eq!(removed, vec![Wire::Mount]);
        assert_eq!(missing, vec![Wire::Dependency]);
    }

    /// The app is never left non-compiling: a mount the CLI cannot excise
    /// means the dependency stays too, and nothing at all is written.
    #[test]
    fn an_unexcisable_mount_leaves_the_dependency_alone() {
        let main = "#[autumn_web::main]\nasync fn main() {\n    let configured = autumn_admin_plugin::AdminPlugin::new();\n    let app = autumn_web::app()\n        .plugin(configured);\n}\n";
        let tmp = fake_project(main, SCAFFOLD_CARGO);
        let outcome = plan_remove(tmp.path(), admin()).unwrap();
        let RemoveOutcome::Manual {
            dependency_line,
            mount_snippet,
            ..
        } = &outcome
        else {
            panic!("expected Manual, got {outcome:?}");
        };
        assert!(
            dependency_line
                .as_ref()
                .is_some_and(|line| line.contains("autumn-admin-plugin")),
            "{dependency_line:?}"
        );
        assert!(mount_snippet.contains("AdminPlugin"), "{mount_snippet}");
    }

    /// A dependency the app still names elsewhere must survive: stripping it
    /// would break a build the user's own code depends on.
    #[test]
    fn a_dependency_still_used_elsewhere_is_retained() {
        let tmp = fake_project(SCAFFOLD_MAIN, SCAFFOLD_CARGO);
        std::fs::write(
            tmp.path().join("src/support.rs"),
            "pub fn role() -> autumn_admin_plugin::AdminPlugin { todo!() }\n",
        )
        .unwrap();
        let outcome = plan_remove(tmp.path(), admin()).unwrap();
        let RemoveOutcome::Removed {
            removed,
            dependency_retained,
            ..
        } = &outcome
        else {
            panic!("expected Removed, got {outcome:?}");
        };
        assert_eq!(removed, &vec![Wire::Mount]);
        assert!(
            dependency_retained
                .as_ref()
                .is_some_and(|kept| kept.reason().contains("support.rs")),
            "{dependency_retained:?}"
        );
    }

    // ── AC #2: data safety ───────────────────────────────────────────────────

    #[test]
    fn a_plugin_that_owns_tables_reports_them_as_residue() {
        let residue = residue_for(media());
        assert!(
            residue.tables.iter().any(|t| t == "media_rooms"),
            "{residue:?}"
        );
        assert!(!residue.migrations.is_empty(), "{residue:?}");
    }

    #[test]
    fn a_plugin_that_owns_no_tables_has_no_residue() {
        assert!(residue_for(admin()).is_empty());
        assert!(residue_for(catalog::lookup("autumn-cache-redis").unwrap()).is_empty());
    }

    /// The declared drop plan must drop dependents before their parents, so it
    /// can be applied top to bottom — the same order the plugin's own
    /// `down.sql` uses.
    #[test]
    fn the_drop_plan_drops_dependents_first_and_forgets_the_migrations() {
        let statements = drop_data_statements(media());
        let participants = statements
            .iter()
            .position(|s| s.contains("media_room_participants"))
            .expect("participants drop");
        let rooms = statements
            .iter()
            .position(|s| s.contains("DROP TABLE IF EXISTS media_rooms"))
            .expect("rooms drop");
        assert!(participants < rooms, "{statements:?}");
        assert!(
            statements
                .iter()
                .any(|s| s.contains("__diesel_schema_migrations")),
            "{statements:?}"
        );
    }

    #[test]
    fn a_plugin_with_no_data_has_an_empty_drop_plan() {
        assert!(drop_data_statements(admin()).is_empty());
    }

    // ── AC #2: what `--drop-data` decides, before anything runs ──────────────

    #[test]
    fn a_plugin_with_no_declared_data_has_nothing_to_drop() {
        assert_eq!(
            decide_drop_data(admin(), Some("postgres://localhost/app")),
            DropDataDecision::NothingToDrop
        );
    }

    #[test]
    fn a_postgres_url_gets_the_statements_applied() {
        let decision = decide_drop_data(media(), Some("postgres://localhost/app"));
        let DropDataDecision::Run { url, statements } = decision else {
            panic!("expected Run, got {decision:?}");
        };
        assert_eq!(url, "postgres://localhost/app");
        assert_eq!(statements, drop_data_statements(media()));
    }

    /// No database configured is not an error — it is a reason to print the
    /// statements instead of guessing at a connection.
    #[test]
    fn no_database_url_prints_the_statements_instead() {
        let decision = decide_drop_data(media(), None);
        let DropDataDecision::PrintOnly { statements, .. } = decision else {
            panic!("expected PrintOnly, got {decision:?}");
        };
        assert_eq!(statements, drop_data_statements(media()));
    }

    /// The dropper speaks Postgres. A `SQLite` app gets the statements to run
    /// itself rather than a connection error after the confirmation prompt.
    #[test]
    fn a_non_postgres_backend_prints_the_statements_instead() {
        let decision = decide_drop_data(media(), Some("sqlite://app.db"));
        let DropDataDecision::PrintOnly { reason, statements } = decision else {
            panic!("expected PrintOnly, got {decision:?}");
        };
        assert!(reason.to_lowercase().contains("sqlite"), "{reason}");
        assert_eq!(statements, drop_data_statements(media()));
    }

    // ── Community crates: dependency-only, in both directions ────────────────

    const COMMUNITY_CARGO: &str = r#"[package]
name = "demo"
version = "0.1.0"

[dependencies]
autumn-web = "0.7.0"
autumn-plugin-live-feed = "0.3.1"
"#;

    /// `plugin add` never writes a community mount, so `remove` only has the
    /// dependency to take back — and only when nothing still uses it.
    #[test]
    fn a_community_dependency_is_removed_when_nothing_uses_it() {
        let tmp = fake_project(
            "#[autumn_web::main]\nasync fn main() {\n    let app = autumn_web::app();\n}\n",
            COMMUNITY_CARGO,
        );
        let outcome = plan_remove_community(tmp.path(), "autumn-plugin-live-feed").unwrap();
        let RemoveOutcome::Removed { removed, .. } = &outcome else {
            panic!("expected Removed, got {outcome:?}");
        };
        assert_eq!(removed, &vec![Wire::Dependency]);
    }

    /// The user pasted the derived mount by hand. Stripping the dependency now
    /// would leave an app that does not compile, so the dependency stays and
    /// the report says why.
    #[test]
    fn a_community_dependency_with_a_hand_written_mount_is_retained() {
        let tmp = fake_project(
            "#[autumn_web::main]\nasync fn main() {\n    let app = autumn_web::app()\n        .plugin(autumn_plugin_live_feed::LiveFeedPlugin::new());\n}\n",
            COMMUNITY_CARGO,
        );
        let outcome = plan_remove_community(tmp.path(), "autumn-plugin-live-feed").unwrap();
        let RemoveOutcome::Removed {
            removed,
            dependency_retained,
            ..
        } = &outcome
        else {
            panic!("expected Removed, got {outcome:?}");
        };
        assert!(removed.is_empty(), "{removed:?}");
        assert!(dependency_retained.is_some(), "{outcome:?}");
    }

    #[test]
    fn removing_an_absent_community_dependency_is_a_no_op() {
        let tmp = fake_project(
            "#[autumn_web::main]\nasync fn main() {}\n",
            "[package]\nname = \"demo\"\n\n[dependencies]\nautumn-web = \"0.7.0\"\n",
        );
        assert!(matches!(
            plan_remove_community(tmp.path(), "autumn-plugin-live-feed").unwrap(),
            RemoveOutcome::NotInstalled { .. }
        ));
    }

    /// The strongest statement of AC #1, for every plugin in the catalog: what
    /// `plugin add` splices in, `plugin remove` takes back out — byte for
    /// byte, marker comment included. A mount whose shape drifts from its
    /// removal is caught here rather than in a user's `git diff`.
    #[test]
    fn every_first_party_mount_round_trips_through_insert_and_remove() {
        for entry in catalog::FIRST_PARTY {
            let installed = crate::plugin::install::insert_mount(SCAFFOLD_MAIN_BARE, entry.mount)
                .unwrap_or_else(|| panic!("{} could not be mounted", entry.crate_name));
            assert_ne!(installed, SCAFFOLD_MAIN_BARE, "{}", entry.crate_name);
            let removed = remove_mount(&installed, entry)
                .unwrap_or_else(|| panic!("{} could not be unmounted", entry.crate_name));
            assert_eq!(
                removed, SCAFFOLD_MAIN_BARE,
                "{} did not round-trip",
                entry.crate_name
            );
        }
    }

    /// Two plugins stacked in one chain: removing either leaves the other —
    /// and the other's marker comment — completely intact.
    #[test]
    fn removing_one_of_two_stacked_mounts_leaves_the_other_whole() {
        let admin = admin();
        let search = catalog::lookup("autumn-search").unwrap();
        let with_search =
            crate::plugin::install::insert_mount(SCAFFOLD_MAIN_BARE, search.mount).unwrap();
        let both = crate::plugin::install::insert_mount(&with_search, admin.mount).unwrap();

        let without_admin = remove_mount(&both, admin).expect("excisable");
        assert_eq!(without_admin, with_search);

        let without_search = remove_mount(&both, search).expect("excisable");
        assert!(
            without_search.contains("AdminPlugin::new()"),
            "{without_search}"
        );
        assert!(
            without_search.contains("added by `autumn plugin add autumn-admin-plugin`"),
            "{without_search}"
        );
        assert!(!without_search.contains("SearchPlugin"), "{without_search}");
        assert!(
            !without_search.contains("added by `autumn plugin add autumn-search`"),
            "{without_search}"
        );
    }

    // ── Review follow-ups: code this command must NOT delete ─────────────────

    /// A plugin wrapped inside ANOTHER plugin's constructor. The type path is
    /// inside the `.plugin(...)` argument, but the argument is not that
    /// plugin — excising it would delete the user's wrapper mount and still
    /// compile, so nothing would ever surface the loss.
    #[test]
    fn a_mount_nested_inside_another_plugins_argument_is_not_excised() {
        let main = "#[autumn_web::main]\nasync fn main() {\n    let app = autumn_web::app()\n        .plugin(MyWrapperPlugin::new(autumn_admin_plugin::AdminPlugin::new(), my_setting()))\n        .routes(routes![index]);\n}\n";
        assert!(remove_mount(main, admin()).is_none());
    }

    /// A blob store chosen at runtime: the `else` branch has nothing to do with
    /// the plugin, and deleting it with the mount also still compiles.
    #[test]
    fn a_conditional_blob_store_is_not_excised() {
        let entry = catalog::lookup("autumn-storage-s3").unwrap();
        let main = "#[autumn_web::main]\nasync fn main() {\n    let app = autumn_web::app()\n        .with_blob_store(if use_s3 {\n            Box::new(autumn_storage_s3::S3BlobStore::from_config(&c).await.unwrap())\n        } else {\n            Box::new(LocalBlobStore::new(\"./uploads\"))\n        })\n        .routes(routes![index]);\n}\n";
        assert!(remove_mount(main, entry).is_none());
    }

    /// An annotated marker comment is the user's note, not this command's
    /// bookkeeping — the mount goes, the note stays.
    #[test]
    fn an_annotated_marker_comment_is_left_alone() {
        let main = "#[autumn_web::main]\nasync fn main() {\n    let app = autumn_web::app()\n        // added by `autumn plugin add autumn-admin-plugin` -- KEEP: see ticket 42\n        .plugin(autumn_admin_plugin::AdminPlugin::new())\n        .routes(routes![index]);\n}\n";
        let out = remove_mount(main, admin()).expect("excisable");
        assert!(!out.contains("AdminPlugin"), "{out}");
        assert!(out.contains("KEEP: see ticket 42"), "{out}");
    }

    /// The verbatim fallback (the shape `autumn-storage-s3`'s block mount takes)
    /// must respect the mask too. A commented-out copy of the mount — Autumn's
    /// own docs ship these snippets — must never be gutted while the live mount
    /// below it survives and the report claims a removal.
    #[test]
    fn a_commented_out_copy_of_a_block_mount_is_never_excised_instead_of_the_real_one() {
        let entry = catalog::lookup("autumn-storage-s3").unwrap();
        let main = format!(
            "#[autumn_web::main]\nasync fn main() {{\n    let app = autumn_web::app()\n/*\n{}*/\n{}        .routes(routes![index]);\n}}\n",
            entry.mount, entry.mount
        );
        let out = remove_mount(&main, entry).expect("the live mount is excisable");
        // The live mount is gone; the commented-out copy is untouched.
        assert!(out.contains("/*\n"), "{out}");
        assert_eq!(
            out.matches("S3BlobStore::from_config(").count(),
            1,
            "only the commented-out copy should remain:\n{out}"
        );
        assert!(!crate::plugin::install::mount_present(&out, entry), "{out}");
    }

    /// Two copies of the same mount: removing one and reporting success would
    /// leave the plugin mounted and running.
    #[test]
    fn every_copy_of_a_repeated_mount_is_removed() {
        let main = "#[autumn_web::main]\nasync fn main() {\n    let app = autumn_web::app()\n        .plugin(autumn_admin_plugin::AdminPlugin::new())\n        .plugin(autumn_admin_plugin::AdminPlugin::new())\n        .routes(routes![index]);\n}\n";
        let out = remove_mount(main, admin()).expect("excisable");
        assert!(!out.contains("AdminPlugin"), "{out}");
        assert!(out.contains(".routes(routes![index])"), "{out}");
    }

    /// A dependency written as a multi-line inline table parses as present but
    /// is not a plain `name = "…"` line. Deleting only its first line leaves a
    /// `Cargo.toml` Cargo cannot parse, so nothing is written and the report
    /// says to delete it by hand.
    #[test]
    fn a_multi_line_inline_table_dependency_is_never_half_deleted() {
        let cargo = "[package]\nname = \"demo\"\n\n[dependencies]\nautumn-web = \"0.7.0\"\nautumn-admin-plugin = { version = \"0.7.0\", features = [\n    \"extra\",\n] }\nmaud = \"0.27\"\n";
        let tmp = fake_project(SCAFFOLD_MAIN, cargo);
        let outcome = plan_remove(tmp.path(), admin()).unwrap();
        let RemoveOutcome::Removed {
            plan,
            removed,
            dependency_retained,
            ..
        } = &outcome
        else {
            panic!("expected Removed, got {outcome:?}");
        };
        assert_eq!(removed, &vec![Wire::Mount]);
        assert!(
            dependency_retained
                .as_ref()
                .is_some_and(DependencyKept::needs_a_hand_edit),
            "{dependency_retained:?}"
        );
        for action in &plan.actions {
            assert!(
                !action.path().ends_with("Cargo.toml"),
                "Cargo.toml must not be rewritten: {action:?}"
            );
        }
    }

    /// A `[dependencies.<crate>]` subtable is the same story.
    #[test]
    fn a_dependency_subtable_is_never_half_deleted() {
        let cargo = "[package]\nname = \"demo\"\n\n[dependencies]\nautumn-web = \"0.7.0\"\n\n[dependencies.autumn-admin-plugin]\nversion = \"0.7.0\"\n";
        let tmp = fake_project(SCAFFOLD_MAIN, cargo);
        let outcome = plan_remove(tmp.path(), admin()).unwrap();
        let RemoveOutcome::Removed {
            dependency_retained,
            ..
        } = &outcome
        else {
            panic!("expected Removed, got {outcome:?}");
        };
        assert!(
            dependency_retained
                .as_ref()
                .is_some_and(DependencyKept::needs_a_hand_edit),
            "{outcome:?}"
        );
    }

    /// A plugin used only from `build.rs` still needs its dependency: the
    /// build script is a Cargo target, and stripping the line stops it
    /// compiling just as surely as stripping one `src/` uses.
    #[test]
    fn a_dependency_used_only_from_a_build_script_is_retained() {
        let tmp = fake_project(SCAFFOLD_MAIN, SCAFFOLD_CARGO);
        std::fs::write(
            tmp.path().join("build.rs"),
            "fn main() { let _ = autumn_admin_plugin::VERSION; }\n",
        )
        .unwrap();
        let outcome = plan_remove(tmp.path(), admin()).unwrap();
        let RemoveOutcome::Removed {
            dependency_retained,
            ..
        } = &outcome
        else {
            panic!("expected Removed, got {outcome:?}");
        };
        assert!(
            dependency_retained
                .as_ref()
                .is_some_and(|kept| kept.reason().contains("build.rs")),
            "{dependency_retained:?}"
        );
    }

    /// Codex review: Cargo lets a target live anywhere. A `[[bin]] path =
    /// "cmd/server.rs"` that still uses the plugin is invisible to a
    /// `src`/`tests`/`benches`/`examples` sweep, and stripping the dependency
    /// stops that target compiling.
    #[test]
    fn a_dependency_used_only_from_a_custom_target_path_is_retained() {
        let cargo =
            format!("{SCAFFOLD_CARGO}\n[[bin]]\nname = \"server\"\npath = \"cmd/server.rs\"\n");
        let tmp = fake_project(SCAFFOLD_MAIN, &cargo);
        std::fs::create_dir_all(tmp.path().join("cmd")).unwrap();
        std::fs::write(
            tmp.path().join("cmd/server.rs"),
            "fn main() { let _ = autumn_admin_plugin::AdminPlugin::new(); }\n",
        )
        .unwrap();

        let outcome = plan_remove(tmp.path(), admin()).unwrap();
        let RemoveOutcome::Removed {
            dependency_retained,
            ..
        } = &outcome
        else {
            panic!("expected Removed, got {outcome:?}");
        };
        assert!(
            dependency_retained
                .as_ref()
                .is_some_and(|kept| kept.reason().contains("server.rs")),
            "{dependency_retained:?}"
        );
    }

    /// A sibling module of such a target counts too — the target's own tree is
    /// swept, not just the file the manifest names.
    #[test]
    fn a_dependency_used_from_a_custom_targets_sibling_module_is_retained() {
        let cargo =
            format!("{SCAFFOLD_CARGO}\n[[bin]]\nname = \"server\"\npath = \"cmd/server.rs\"\n");
        let tmp = fake_project(SCAFFOLD_MAIN, &cargo);
        std::fs::create_dir_all(tmp.path().join("cmd")).unwrap();
        std::fs::write(
            tmp.path().join("cmd/server.rs"),
            "mod panel;\nfn main() {}\n",
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("cmd/panel.rs"),
            "pub use autumn_admin_plugin::AdminPlugin;\n",
        )
        .unwrap();

        let outcome = plan_remove(tmp.path(), admin()).unwrap();
        let RemoveOutcome::Removed {
            dependency_retained,
            ..
        } = &outcome
        else {
            panic!("expected Removed, got {outcome:?}");
        };
        assert!(dependency_retained.is_some(), "{outcome:?}");
    }

    /// A custom build-script path (`[package] build = "…"`) replaces
    /// `build.rs`, and is scanned in its place.
    #[test]
    fn a_dependency_used_only_from_a_custom_build_script_is_retained() {
        let cargo = SCAFFOLD_CARGO.replace(
            "edition = \"2024\"",
            "edition = \"2024\"\nbuild = \"tools/build.rs\"",
        );
        let tmp = fake_project(SCAFFOLD_MAIN, &cargo);
        std::fs::create_dir_all(tmp.path().join("tools")).unwrap();
        std::fs::write(
            tmp.path().join("tools/build.rs"),
            "fn main() { let _ = autumn_admin_plugin::VERSION; }\n",
        )
        .unwrap();

        let outcome = plan_remove(tmp.path(), admin()).unwrap();
        let RemoveOutcome::Removed {
            dependency_retained,
            ..
        } = &outcome
        else {
            panic!("expected Removed, got {outcome:?}");
        };
        assert!(dependency_retained.is_some(), "{outcome:?}");
    }

    /// The widened scan must not become "retain everything": an ordinary
    /// project with a custom target that does NOT use the plugin still gets its
    /// dependency removed.
    #[test]
    fn a_custom_target_that_does_not_use_the_plugin_does_not_retain_it() {
        let cargo =
            format!("{SCAFFOLD_CARGO}\n[[bin]]\nname = \"server\"\npath = \"cmd/server.rs\"\n");
        let tmp = fake_project(SCAFFOLD_MAIN, &cargo);
        std::fs::create_dir_all(tmp.path().join("cmd")).unwrap();
        std::fs::write(tmp.path().join("cmd/server.rs"), "fn main() {}\n").unwrap();

        let outcome = plan_remove(tmp.path(), admin()).unwrap();
        let RemoveOutcome::Removed {
            removed,
            dependency_retained,
            ..
        } = &outcome
        else {
            panic!("expected Removed, got {outcome:?}");
        };
        assert!(dependency_retained.is_none(), "{dependency_retained:?}");
        assert!(removed.contains(&Wire::Dependency), "{removed:?}");
    }
}
