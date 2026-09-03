//! `autumn plugin list` / `autumn plugin add` — one-command plugin discovery
//! and install (issue #1606).
//!
//! The CLI already knew how to *author* a plugin (`autumn generate plugin`) and
//! how to *audit* one (`autumn plugin-check`); this module is the consumer
//! half. `list` answers "what can I install into this app, at what version",
//! and `add` performs the four hand edits — find the crate, add the
//! dependency, mount it in the builder chain, read the config docs — as one
//! command.

pub mod catalog;
pub mod install;
pub mod registry;
pub mod remove;

use std::path::Path;

use catalog::CatalogEntry;
use install::{AddOutcome, Compat, PluginError};
use remove::{DataResidue, DependencyKept, RemoveOutcome, Wire};

/// Where a listed plugin came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// Shipped in this workspace, released in lockstep with `autumn-web`.
    FirstParty,
    /// Found on crates.io through the `autumn-plugin-` naming convention.
    Community,
}

/// One row of `autumn plugin list`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListRow {
    /// crates.io name.
    pub crate_name: String,
    /// The version that would be installed.
    pub version: String,
    /// One-line description.
    pub summary: String,
    /// Where the row came from.
    pub origin: Origin,
    /// Whether that version works with this app's `autumn-web`.
    pub compat: Compat,
}

/// Options for `autumn plugin list`.
#[derive(Debug, Clone, Copy)]
pub struct ListOptions<'a> {
    /// Project root to resolve the app's `autumn-web` version from.
    pub root: &'a Path,
    /// Emit JSON instead of a table.
    pub json: bool,
    /// Skip the crates.io lookup entirely.
    pub offline: bool,
}

/// Options for `autumn plugin add`.
#[derive(Debug, Clone, Copy)]
pub struct AddOptions<'a> {
    /// Project root to install into.
    pub root: &'a Path,
    /// Plugin crate name.
    pub name: &'a str,
    /// Print the plan without touching the filesystem.
    pub dry_run: bool,
    /// Skip the crates.io lookup (community crates only).
    pub offline: bool,
}

/// What [`resolve`] found for a plugin name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolved {
    /// A first-party plugin the CLI knows how to mount.
    FirstParty(&'static CatalogEntry),
    /// A community crate following the `autumn-plugin-` convention.
    Community(String),
}

/// Options for `autumn plugin remove`.
#[derive(Debug, Clone, Copy)]
pub struct RemoveOptions<'a> {
    /// Project root to remove from.
    pub root: &'a Path,
    /// Plugin crate name.
    pub name: &'a str,
    /// Print the plan without touching the filesystem.
    pub dry_run: bool,
    /// Also revert the plugin's declared migrations and drop its tables.
    pub drop_data: bool,
    /// Skip the interactive confirmation `--drop-data` otherwise requires.
    pub yes: bool,
}

/// Exit code for the manual-fallback outcome: nothing was written, and the
/// dependency line plus mount snippet were printed for the user to apply.
pub const MANUAL_FALLBACK_EXIT_CODE: i32 = 2;

/// Exit code for `plugin remove --dry-run` when there **is** something to
/// change (AC #3).
///
/// A dry run that found nothing to do exits `0`, so a script can tell the two
/// apart without parsing prose: `0` means the plugin is already gone, `3` means
/// a real run would edit files. Deliberately distinct from
/// [`MANUAL_FALLBACK_EXIT_CODE`], which still means "nothing can be changed
/// automatically — here are the lines", dry run or not.
///
/// `plugin add --dry-run` keeps its issue-#1606 contract of always exiting `0`;
/// this code is scoped to `remove`, whose AC asks for the distinction.
pub const DRY_RUN_PENDING_EXIT_CODE: i32 = 3;

/// The version of every first-party plugin: they are released in lockstep
/// with `autumn-web` and with this CLI, so the CLI's own version is the one to
/// install.
#[must_use]
pub const fn first_party_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Build the rows `autumn plugin list` renders.
///
/// `app_version` is the app's `autumn-web` requirement, or `None` when the
/// command is run outside a project — in which case nothing can be said about
/// compatibility, so every row is [`Compat::Unknown`] rather than optimistically
/// compatible.
#[must_use]
pub fn list_rows(
    app_version: Option<&str>,
    community: &[registry::CommunityPlugin],
) -> Vec<ListRow> {
    let version = first_party_version();
    let compat = app_version.map_or(Compat::Unknown, |app| install::check_compat(app, version));
    let mut rows: Vec<ListRow> = catalog::FIRST_PARTY
        .iter()
        .map(|entry| ListRow {
            crate_name: entry.crate_name.to_owned(),
            version: version.to_owned(),
            summary: entry.summary.to_owned(),
            origin: Origin::FirstParty,
            compat,
        })
        .collect();
    rows.extend(community.iter().map(|found| ListRow {
        crate_name: found.crate_name.clone(),
        version: found.version.clone(),
        summary: found.summary.clone(),
        origin: Origin::Community,
        // A community crate's supported `autumn-web` range is not in the
        // crates.io search response, and issue #1606 rules out a registry that
        // would carry it. Reported unknown rather than guessed.
        compat: Compat::Unknown,
    }));
    rows
}

/// Longest single-line summary rendered in the table before it is elided.
const SUMMARY_WIDTH: usize = 68;

/// Render `rows` as the human-readable table.
#[must_use]
pub fn render_list(rows: &[ListRow], app_version: Option<&str>, note: Option<&str>) -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    match app_version {
        Some(version) => {
            let _ = writeln!(
                out,
                "Installable plugins (this app uses autumn-web {version})\n"
            );
        }
        None => {
            out.push_str(
                "Installable plugins (run inside an Autumn project to check version compatibility)\n\n",
            );
        }
    }

    let name_width = rows
        .iter()
        .map(|row| crate::text_width::display_width(&row.crate_name))
        .max()
        .unwrap_or(0);
    let version_width = rows
        .iter()
        .map(|row| crate::text_width::display_width(&row.version))
        .max()
        .unwrap_or(0);

    for (origin, heading) in [
        (Origin::FirstParty, "First-party"),
        (Origin::Community, "Community (crates.io `autumn-plugin-*`)"),
    ] {
        let group: Vec<&ListRow> = rows.iter().filter(|row| row.origin == origin).collect();
        if group.is_empty() {
            continue;
        }
        let _ = writeln!(out, "{heading}:");
        for row in group {
            let _ = writeln!(
                out,
                "  {name:<name_width$}  {version:<version_width$}  {summary}{compat}",
                name = row.crate_name,
                version = row.version,
                summary = elide(&row.summary, SUMMARY_WIDTH),
                compat = match row.compat {
                    // Name the series that WOULD work, rather than only saying
                    // this one will not: on an older app the bare word
                    // "incompatible" leaves the reader with no next step.
                    Compat::Incompatible => format!(
                        "  [needs autumn-web {}]",
                        install::supported_range(&row.version)
                    ),
                    Compat::Unknown if row.origin == Origin::FirstParty =>
                        "  [compatibility unknown]".to_owned(),
                    Compat::Compatible | Compat::Unknown => String::new(),
                },
            );
        }
        out.push('\n');
    }

    if let Some(note) = note {
        let _ = writeln!(out, "Note: {note}");
    }
    let _ = write!(out, "Install one with `autumn plugin add <name>`.");
    out
}

/// Shorten `text` to `width` display columns, marking the cut with `…`.
fn elide(text: &str, width: usize) -> String {
    if crate::text_width::display_width(text) <= width {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(width.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Render `rows` as JSON.
#[must_use]
pub fn render_list_json(rows: &[ListRow], app_version: Option<&str>) -> String {
    let plugins: Vec<serde_json::Value> = rows
        .iter()
        .map(|row| {
            serde_json::json!({
                "name": row.crate_name,
                "version": row.version,
                "description": row.summary,
                "origin": match row.origin {
                    Origin::FirstParty => "first-party",
                    Origin::Community => "community",
                },
                "compatible": match row.compat {
                    Compat::Compatible => serde_json::Value::Bool(true),
                    Compat::Incompatible => serde_json::Value::Bool(false),
                    Compat::Unknown => serde_json::Value::Null,
                },
            })
        })
        .collect();
    let document = serde_json::json!({
        "autumn_web": app_version,
        "plugins": plugins,
    });
    serde_json::to_string_pretty(&document).unwrap_or_else(|_| "{}".to_owned())
}

/// Render the report `plugin add` prints for `outcome`.
///
/// `dry_run` only changes the wording: a dry run has printed the edits it
/// *would* make, so claiming the plugin was installed would be a lie.
#[must_use]
pub fn render_add(entry_name: &str, outcome: &AddOutcome, dry_run: bool) -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    match outcome {
        AddOutcome::Installed { steps, .. } => {
            if dry_run {
                let _ = writeln!(
                    out,
                    "\nDry run: nothing was written. `autumn plugin add {entry_name}` would make the edits above."
                );
            } else {
                let _ = writeln!(out, "\nInstalled {entry_name}.");
            }
            append_steps(&mut out, steps);
        }
        AddOutcome::AlreadyInstalled => {
            let _ = write!(
                out,
                "\n{entry_name} is already installed — nothing to do (the dependency and the mount are both in place)."
            );
        }
        AddOutcome::DependencyOnly {
            dependency_added,
            dependency_line,
            mount_snippet,
            ..
        } => {
            if dry_run {
                let _ = writeln!(
                    out,
                    "\nDry run: nothing was written. `autumn plugin add {entry_name}` would add {dependency_line}."
                );
            } else if *dependency_added {
                let _ = writeln!(out, "\nAdded {dependency_line}.");
            } else {
                let _ = writeln!(
                    out,
                    "\n{dependency_line} is already declared — nothing to change."
                );
            }
            let _ = writeln!(
                out,
                "\n{entry_name} is a community crate, so the mount is not written for you — the\n\
                 `<Name>Plugin` below is derived from the naming convention in docs/plugins.md and\n\
                 cannot be verified from here. Check the crate's README, then add to your builder chain:\n"
            );
            let _ = writeln!(out, "{mount_snippet}");
        }
        AddOutcome::Manual {
            reason,
            dependency_line,
            mount_snippet,
            steps,
        } => {
            let _ = writeln!(out, "\nNo files were changed: {reason}.");
            let _ = writeln!(out, "\nAdd to `[dependencies]` in Cargo.toml:\n");
            let _ = writeln!(out, "  {dependency_line}");
            let _ = writeln!(out, "\nAdd to your `autumn_web::app()` builder chain:\n");
            let _ = writeln!(out, "{mount_snippet}");
            append_steps(&mut out, steps);
        }
    }
    out
}

/// Append a numbered "Next steps" block, if there is anything to say.
fn append_steps(out: &mut String, steps: &[String]) {
    use std::fmt::Write as _;

    if steps.is_empty() {
        return;
    }
    out.push_str("\nNext steps:\n");
    for (index, step) in steps.iter().enumerate() {
        let _ = writeln!(out, "  {}. {step}", index + 1);
    }
}

/// Resolve `name` to a first-party catalog entry, or report it as a community
/// crate that follows the documented convention.
///
/// # Errors
///
/// [`PluginError::UnknownPlugin`] when the name is neither.
pub fn resolve(name: &str) -> Result<Resolved, PluginError> {
    if let Some(entry) = catalog::lookup(name) {
        return Ok(Resolved::FirstParty(entry));
    }
    if catalog::is_community_name(name) {
        return Ok(Resolved::Community(name.to_owned()));
    }
    Err(PluginError::UnknownPlugin(name.to_owned()))
}

/// The app's `autumn-web` version, or `None` when it cannot be determined.
fn app_version(root: &Path) -> Option<String> {
    match install::app_autumn_web(root) {
        Ok(install::AppAutumnWeb::Version(version)) => Some(version),
        Ok(install::AppAutumnWeb::Unversioned) | Err(_) => None,
    }
}

/// Run `autumn plugin list`. Returns the process exit code.
#[must_use]
pub fn run_list(opts: &ListOptions<'_>) -> i32 {
    let app = app_version(opts.root);
    let (community, note) = if opts.offline {
        (
            Vec::new(),
            Some(
                "--offline: crates.io was not queried, so no community plugins are listed."
                    .to_owned(),
            ),
        )
    } else {
        registry::search().map_or_else(
            || {
                (
                    Vec::new(),
                    Some(
                        "could not reach crates.io, so no community plugins are listed.".to_owned(),
                    ),
                )
            },
            |found| (found, None),
        )
    };
    let rows = list_rows(app.as_deref(), &community);
    if opts.json {
        println!("{}", render_list_json(&rows, app.as_deref()));
    } else {
        println!("{}", render_list(&rows, app.as_deref(), note.as_deref()));
    }
    0
}

/// Run `autumn plugin add`. Returns the process exit code.
#[must_use]
pub fn run_add(opts: &AddOptions<'_>) -> i32 {
    let resolved = match resolve(opts.name) {
        Ok(resolved) => resolved,
        Err(err) => {
            eprintln!("autumn plugin add: {err}");
            return 1;
        }
    };

    let outcome = match &resolved {
        Resolved::FirstParty(entry) => install::plan_add(opts.root, entry, first_party_version()),
        Resolved::Community(crate_name) => {
            if opts.offline {
                eprintln!(
                    "autumn plugin add: --offline cannot resolve a version for the community crate `{crate_name}`; drop --offline or add the dependency by hand."
                );
                return 1;
            }
            let Some(version) = registry::latest_version(crate_name) else {
                eprintln!(
                    "autumn plugin add: could not find `{crate_name}` on crates.io (or crates.io is unreachable); no files were changed."
                );
                return 1;
            };
            install::plan_add_community(opts.root, crate_name, &version)
        }
    };

    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(err) => {
            eprintln!("autumn plugin add: {err}");
            return 1;
        }
    };

    let flags = crate::generate::Flags {
        dry_run: opts.dry_run,
        force: false,
    };
    match &outcome {
        AddOutcome::Installed { plan, .. } | AddOutcome::DependencyOnly { plan, .. } => {
            if let Err(err) = plan.execute(flags) {
                eprintln!("autumn plugin add: {err}");
                return 1;
            }
        }
        AddOutcome::AlreadyInstalled | AddOutcome::Manual { .. } => {}
    }

    let report = render_add(opts.name, &outcome, opts.dry_run);
    if matches!(outcome, AddOutcome::Manual { .. }) {
        // A refusal, not a result: it goes to stderr and exits non-zero so
        // `autumn plugin add … && cargo build` cannot read "I changed nothing,
        // do it yourself" as a successful install. `2` rather than `1` so a
        // script can tell "apply this by hand" apart from a hard error.
        eprintln!("{report}");
        return MANUAL_FALLBACK_EXIT_CODE;
    }
    println!("{report}");
    0
}

/// Render the report `plugin remove` prints for `outcome`.
///
/// `dry_run` only changes the wording: a dry run has printed the edits it
/// *would* make, so claiming the plugin was removed would be a lie.
#[must_use]
pub fn render_remove(entry_name: &str, outcome: &RemoveOutcome, dry_run: bool) -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    match outcome {
        RemoveOutcome::Removed {
            removed,
            missing,
            dependency_retained,
            residue,
            ..
        } => {
            if dry_run {
                let _ = writeln!(
                    out,
                    "\nDry run: nothing was written. `autumn plugin remove {entry_name}` would make the edits above."
                );
            } else if removed.is_empty() {
                let _ = writeln!(out, "\n{entry_name}: nothing was left to unwire.");
            } else {
                let _ = writeln!(
                    out,
                    "\nRemoved {entry_name} — {}.",
                    join_wires(removed, "and")
                );
            }
            if let Some(kept) = dependency_retained {
                let _ = writeln!(out, "\n{}.", kept.reason());
            }
            if !missing.is_empty() {
                let _ = writeln!(
                    out,
                    "\nCould not find {} — {} nothing to remove there.",
                    join_wires(missing, "or"),
                    if missing.len() == 1 {
                        "so"
                    } else {
                        "so there was"
                    }
                );
            }
            append_residue(&mut out, entry_name, residue);
        }
        RemoveOutcome::NotInstalled { residue } => {
            let _ = writeln!(
                out,
                "\n{entry_name} is not installed — nothing to do (neither the dependency nor the mount is present)."
            );
            append_residue(&mut out, entry_name, residue);
        }
        RemoveOutcome::Manual {
            reason,
            dependency_line,
            mount_snippet,
            residue,
        } => {
            let _ = writeln!(out, "\nNo files were changed: {reason}.");
            if let Some(line) = dependency_line {
                let _ = writeln!(out, "\nDelete from `[dependencies]` in Cargo.toml:\n");
                let _ = writeln!(out, "  {line}");
            }
            let _ = writeln!(
                out,
                "\nDelete from your `autumn_web::app()` builder chain (this is the shape\n\
                 `autumn plugin add` writes; yours may be configured differently):\n"
            );
            let _ = writeln!(out, "{mount_snippet}");
            append_residue(&mut out, entry_name, residue);
        }
    }
    out
}

/// Name a list of wires in prose: `the Cargo.toml dependency and the
/// builder-chain mount`.
fn join_wires(wires: &[Wire], conjunction: &str) -> String {
    let labels: Vec<&str> = wires.iter().map(|wire| wire.label()).collect();
    match labels.as_slice() {
        [] => String::new(),
        [only] => (*only).to_owned(),
        [first, second] => format!("{first} {conjunction} {second}"),
        [rest @ .., last] => format!("{}, {conjunction} {last}", rest.join(", ")),
    }
}

/// Append the data-safety paragraph: what stayed in the database, and the one
/// flag that would remove it (AC #2).
///
/// Silent when the plugin owns nothing — a data warning about a plugin with no
/// data teaches the user to skip the paragraph that matters.
fn append_residue(out: &mut String, entry_name: &str, residue: &DataResidue) {
    use std::fmt::Write as _;

    if residue.is_empty() {
        return;
    }
    let _ = writeln!(
        out,
        "\nThe database was not touched. {entry_name} owns the following, and it is\nall still there:"
    );
    for migration in &residue.migrations {
        let _ = writeln!(out, "  migration  {migration}");
    }
    for table in &residue.tables {
        let _ = writeln!(out, "  table      {table}");
    }
    let _ = writeln!(
        out,
        "\nThese are left in place on purpose: unwiring code is reversible, dropping\n\
         data is not. To revert those migrations and drop those tables, re-run with\n\
         `--drop-data` (it asks for confirmation first, or pass `--yes`)."
    );
}

/// Whether `outcome` would actually edit a file.
///
/// Reads the plan rather than the variant: a `Removed` outcome whose plan
/// turned out to hold no action (a dependency declared in a shape the manifest
/// rewriter leaves alone) has nothing to do, and must not report otherwise.
#[must_use]
pub fn removal_changes_files(outcome: &RemoveOutcome) -> bool {
    match outcome {
        RemoveOutcome::Removed { plan, .. } => !plan.actions.is_empty(),
        RemoveOutcome::NotInstalled { .. } | RemoveOutcome::Manual { .. } => false,
    }
}

/// The process exit code for a completed `plugin remove` (AC #3).
#[must_use]
pub fn remove_exit_code(outcome: &RemoveOutcome, dry_run: bool) -> i32 {
    if matches!(outcome, RemoveOutcome::Manual { .. }) || leaves_a_hand_edit(outcome) {
        return MANUAL_FALLBACK_EXIT_CODE;
    }
    if dry_run && removal_changes_files(outcome) {
        return DRY_RUN_PENDING_EXIT_CODE;
    }
    0
}

/// Whether `outcome` finished with something still to do by hand.
///
/// Two shapes: a dependency declared in a form this command will not rewrite,
/// and a run that removed nothing while a wire is still in place (a community
/// crate whose hand-pasted mount keeps its dependency alive). Both mean
/// `autumn plugin remove x && …` must NOT read as "x is gone" — the same
/// reason [`RemoveOutcome::Manual`] exits [`MANUAL_FALLBACK_EXIT_CODE`].
fn leaves_a_hand_edit(outcome: &RemoveOutcome) -> bool {
    let RemoveOutcome::Removed {
        removed,
        dependency_retained,
        ..
    } = outcome
    else {
        return false;
    };
    dependency_retained.as_ref().is_some_and(|kept| {
        kept.needs_a_hand_edit()
            || (removed.is_empty() && matches!(kept, DependencyKept::StillUsed(_)))
    })
}

/// Run `autumn plugin remove`. Returns the process exit code.
#[must_use]
pub fn run_remove(opts: &RemoveOptions<'_>) -> i32 {
    let resolved = match resolve(opts.name) {
        Ok(resolved) => resolved,
        Err(err) => {
            eprintln!("autumn plugin remove: {err}");
            return 1;
        }
    };

    // Refused BEFORE any planning, so `--drop-data` on a crate whose data this
    // CLI cannot enumerate never gets as far as editing a file.
    if opts.drop_data
        && let Resolved::Community(crate_name) = &resolved
    {
        eprintln!(
            "autumn plugin remove: --drop-data works from a plugin's declared migration and table list, which only first-party plugins carry — `{crate_name}` is a community crate, so check its README for what it owns and revert that by hand. No files were changed."
        );
        return 1;
    }

    let outcome = match &resolved {
        Resolved::FirstParty(entry) => remove::plan_remove(opts.root, entry),
        Resolved::Community(crate_name) => remove::plan_remove_community(opts.root, crate_name),
    };
    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(err) => {
            eprintln!("autumn plugin remove: {err}");
            return 1;
        }
    };

    // `--drop-data` is decided and CONFIRMED before a single file is written.
    // Asking after the edits are on disk means a declined prompt leaves the app
    // already unwired while the message says "Aborted" — the one ordering a
    // destructive flag must not have.
    let drop_plan = match &resolved {
        Resolved::FirstParty(entry) if opts.drop_data => {
            let absent = matches!(outcome, RemoveOutcome::NotInstalled { .. });
            match prepare_drop_data(entry, opts, absent) {
                Ok(plan) => plan,
                Err(code) => return code,
            }
        }
        _ => None,
    };

    let flags = crate::generate::Flags {
        dry_run: opts.dry_run,
        force: false,
    };
    if let RemoveOutcome::Removed { plan, .. } = &outcome
        && let Err(err) = plan.execute(flags)
    {
        eprintln!("autumn plugin remove: {err}");
        return 1;
    }

    let report = render_remove(opts.name, &outcome, opts.dry_run);
    if matches!(outcome, RemoveOutcome::Manual { .. }) {
        // A refusal, not a result — same contract as `plugin add`'s manual
        // fallback: stderr, and an exit code a script can act on.
        eprintln!("{report}");
        return MANUAL_FALLBACK_EXIT_CODE;
    }
    println!("{report}");
    if matches!(resolved, Resolved::Community(_)) {
        println!(
            "\n{} is a community crate: this CLI has no list of the migrations or tables it\nowns, so nothing here can tell you what it left in the database. Check the\ncrate's README before assuming the removal was complete.",
            opts.name
        );
    }

    if let Some(plan) = drop_plan {
        let code = apply_drop_data(&plan);
        if code != 0 {
            return code;
        }
    }

    remove_exit_code(&outcome, opts.dry_run)
}

/// A confirmed `--drop-data` step, ready to apply once the code edits land.
#[derive(Debug)]
struct ConfirmedDrop {
    /// The database to apply the statements to.
    url: String,
    /// The statements, in application order.
    statements: Vec<String>,
    /// The plugin whose data they drop, for the closing message.
    crate_name: &'static str,
}

/// Decide, print and confirm the `--drop-data` step — before anything is
/// written.
///
/// `Ok(None)` means there is nothing left to do for `--drop-data` (nothing
/// declared, or the statements were printed for the user to run). `Err(code)`
/// is a refusal that aborts the whole command with **no** file changed.
fn prepare_drop_data(
    entry: &'static catalog::CatalogEntry,
    opts: &RemoveOptions<'_>,
    not_installed_here: bool,
) -> Result<Option<ConfirmedDrop>, i32> {
    use remove::DropDataDecision;

    // The `.env`/config resolution `autumn migrate` uses, but WITHOUT
    // `resolve_database_url`'s exit-on-missing: a missing URL is a reason to
    // print the statements, not to fail a removal that can still proceed.
    let url = crate::config::resolve_primary_database_url_with_env(&autumn_web::config::OsEnv);
    match remove::decide_drop_data(entry, url.as_deref()) {
        DropDataDecision::NothingToDrop => {
            println!(
                "\n--drop-data: {} declares no migrations and owns no tables, so there is\nnothing in the database to drop.",
                entry.crate_name
            );
            Ok(None)
        }
        DropDataDecision::PrintOnly { reason, statements } => {
            // Exit 2, not 0: the database was NOT changed, and a script reading
            // `remove --drop-data && echo dropped` must not print "dropped".
            // Same meaning `plugin add`/`remove` already give 2 — "nothing was
            // changed automatically; apply the printed lines by hand".
            eprintln!(
                "\n--drop-data was not applied — {reason}.\nThe database is unchanged. Run these yourself, in this order:\n"
            );
            for statement in &statements {
                eprintln!("  {statement}");
            }
            Err(MANUAL_FALLBACK_EXIT_CODE)
        }
        DropDataDecision::Run { url, statements } => {
            if opts.dry_run {
                println!(
                    "\nDry run: the database was not touched. `--drop-data` would run these\nagainst {}, in this order:\n",
                    redact_database_url(&url)
                );
                for statement in &statements {
                    println!("  {statement}");
                }
                return Ok(None);
            }
            println!(
                "\n--drop-data will run these against {}, in this order:\n",
                redact_database_url(&url)
            );
            for statement in &statements {
                println!("  {statement}");
            }
            if not_installed_here {
                // The database URL can come from an ambient `DATABASE_URL` that
                // outranks this project's own config, so "the plugin was never
                // wired here" is worth saying out loud before a DROP.
                eprintln!(
                    "\nNote: {} is not wired into this project at all, so these statements\nwould drop data belonging to a plugin this app does not use.",
                    entry.crate_name
                );
            }
            if !confirm_drop_data(entry.crate_name, opts.yes) {
                eprintln!("\nAborted: nothing was changed — not the code, not the database.");
                return Err(1);
            }
            Ok(Some(ConfirmedDrop {
                url,
                statements,
                crate_name: entry.crate_name,
            }))
        }
    }
}

/// Apply a confirmed drop. Returns `0` on success.
fn apply_drop_data(plan: &ConfirmedDrop) -> i32 {
    match remove::execute_drop_data(&plan.url, &plan.statements) {
        Ok(()) => {
            println!("\nDropped {}'s data.", plan.crate_name);
            0
        }
        Err(err) => {
            eprintln!("\nautumn plugin remove --drop-data: {err}");
            eprintln!(
                "The code changes above were already applied; only the database step failed."
            );
            1
        }
    }
}

/// `url` with every password removed, so a confirmation prompt does not print
/// credentials into a terminal scrollback or a CI log.
///
/// Two places carry one, and both are in shapes libpq accepts and this CLI's
/// own URL resolution passes straight through: the userinfo component
/// (`user:pass@host`) and the query string (`?password=`, `?sslpassword=`).
/// The userinfo split is from the RIGHT, because a password may itself contain
/// `@` — splitting from the left would print its tail verbatim.
fn redact_database_url(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_owned();
    };
    // The authority ends at the first `/`, `?` or `#`; anything after that is
    // path and query, where a `@` is not a userinfo separator.
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(authority_end);
    let authority = match authority.rsplit_once('@') {
        // Only mask what is actually there: turning `user@host` into
        // `user:***@host` invents a password the URL does not carry.
        Some((credentials, host)) => match credentials.split_once(':') {
            Some((user, _)) => format!("{user}:***@{host}"),
            None => authority.to_owned(),
        },
        None => authority.to_owned(),
    };
    format!("{scheme}://{authority}{}", redact_query_secrets(tail))
}

/// The path-and-query `tail` with the value of every secret-bearing query
/// parameter replaced.
fn redact_query_secrets(tail: &str) -> String {
    /// Query keys libpq reads a secret from.
    const SECRET_KEYS: &[&str] = &["password", "sslpassword"];

    let Some((path, query)) = tail.split_once('?') else {
        return tail.to_owned();
    };
    let redacted: Vec<String> = query
        .split('&')
        .map(|pair| {
            let key = pair.split('=').next().unwrap_or(pair);
            if SECRET_KEYS.contains(&key.to_ascii_lowercase().as_str()) {
                format!("{key}=***")
            } else {
                pair.to_owned()
            }
        })
        .collect();
    format!("{path}?{}", redacted.join("&"))
}

/// Ask before dropping. `--yes` answers for the user; a non-interactive stdin
/// with no `--yes` is a refusal, never an assumed yes.
fn confirm_drop_data(crate_name: &str, assume_yes: bool) -> bool {
    use std::io::{BufRead as _, IsTerminal as _, Write as _};

    let stdin = std::io::stdin();
    match crate::starters::confirm_mode(assume_yes, stdin.is_terminal()) {
        crate::starters::ConfirmMode::Proceed => return true,
        crate::starters::ConfirmMode::NeedsYesFlag => {
            eprintln!(
                "\n--drop-data needs a confirmation, and stdin is not a terminal. Re-run with\n`--yes` if you really mean to drop {crate_name}'s data."
            );
            return false;
        }
        crate::starters::ConfirmMode::Prompt => {}
    }
    // stderr, like every other refusal message here: under
    // `autumn plugin remove … > log.txt` a stdout prompt is invisible and the
    // command looks hung.
    eprint!("Drop {crate_name}'s data? This cannot be undone. [y/N] ");
    let _ = std::io::stderr().flush();
    let mut answer = String::new();
    if stdin.lock().read_line(&mut answer).is_err() {
        return false;
    }
    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

/// One plugin `autumn new --with` will wire into the app it scaffolds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScaffoldPlugin {
    /// The crate name as the user typed it.
    pub name: String,
    /// What that name resolved to.
    pub resolved: Resolved,
    /// The version to install.
    pub version: String,
}

/// Resolve and version-check every `--with` name.
///
/// # Errors
///
/// A message ready to print when a name is unknown, incompatible, or (for a
/// community crate) has no resolvable version.
pub fn preflight_scaffold_plugins(
    names: &[String],
    scaffold_autumn_web: &str,
    resolve_community_version: impl Fn(&str) -> Option<String>,
) -> Result<Vec<ScaffoldPlugin>, String> {
    let mut out: Vec<ScaffoldPlugin> = Vec::with_capacity(names.len());
    for name in names {
        // `--with X --with X` is a typo, not a conflict: the second one names
        // the same install, and `plugin add` is idempotent regardless.
        if out.iter().any(|already| &already.name == name) {
            continue;
        }
        let resolved = resolve(name).map_err(|err| err.to_string())?;
        let version = match &resolved {
            Resolved::FirstParty(_) => first_party_version().to_owned(),
            Resolved::Community(crate_name) => {
                let version = resolve_community_version(crate_name).ok_or_else(|| {
                    format!(
                        "could not find `{crate_name}` on crates.io (or crates.io is unreachable) — no files were written"
                    )
                })?;
                // The version is written verbatim into a manifest, so it is
                // vetted the same way `plugin add` vets it.
                if !install::is_plausible_version(&version) {
                    return Err(format!(
                        "crates.io reported version `{version}` for `{crate_name}`, which is not a usable version requirement — no files were written"
                    ));
                }
                version
            }
        };
        // A first-party plugin is released in lockstep with `autumn-web`, so
        // this can only fail if the scaffold ever stops pinning the CLI's own
        // series — which is exactly the regression worth catching before a
        // project exists on disk. A community crate's range is not knowable,
        // so it is not gated here (`Compat::Unknown` passes).
        if matches!(resolved, Resolved::FirstParty(_))
            && install::check_compat(scaffold_autumn_web, &version) == Compat::Incompatible
        {
            return Err(format!(
                "`{name} {version}` supports autumn-web {}, but this scaffold pins autumn-web {scaffold_autumn_web} — no files were written.\nInstall the matching CLI series and try again.",
                install::supported_range(&version)
            ));
        }
        out.push(ScaffoldPlugin {
            name: name.clone(),
            resolved,
            version,
        });
    }
    Ok(out)
}

/// Wire every preflighted plugin into the freshly scaffolded app at `root`.
///
/// Returns the process exit code: `0` when every plugin was wired, or the
/// worst code any single install produced. Runs only after
/// [`preflight_scaffold_plugins`] has passed, so nothing here can be the first
/// place a bad name is noticed.
#[must_use]
pub fn wire_scaffold_plugins(root: &Path, plugins: &[ScaffoldPlugin]) -> i32 {
    let mut worst = 0;
    for plugin in plugins {
        let outcome = match &plugin.resolved {
            Resolved::FirstParty(entry) => install::plan_add(root, entry, &plugin.version),
            Resolved::Community(crate_name) => {
                install::plan_add_community(root, crate_name, &plugin.version)
            }
        };
        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(err) => {
                eprintln!("autumn new --with {}: {err}", plugin.name);
                worst = worst.max(1);
                continue;
            }
        };
        match &outcome {
            AddOutcome::Installed { plan, .. } | AddOutcome::DependencyOnly { plan, .. } => {
                if let Err(err) = plan.execute(crate::generate::Flags::default()) {
                    eprintln!("autumn new --with {}: {err}", plugin.name);
                    worst = worst.max(1);
                    continue;
                }
            }
            AddOutcome::AlreadyInstalled | AddOutcome::Manual { .. } => {}
        }
        let report = render_add(&plugin.name, &outcome, false);
        if matches!(outcome, AddOutcome::Manual { .. }) {
            eprintln!("{report}");
            worst = worst.max(MANUAL_FALLBACK_EXIT_CODE);
        } else {
            println!("{report}");
        }
    }
    worst
}

#[cfg(test)]
mod tests {
    use super::*;
    use registry::CommunityPlugin;

    fn community() -> Vec<CommunityPlugin> {
        vec![CommunityPlugin {
            crate_name: "autumn-plugin-live-feed".to_owned(),
            version: "0.3.1".to_owned(),
            summary: "Live feeds for autumn-web".to_owned(),
        }]
    }

    #[test]
    fn resolve_finds_first_party_plugins() {
        assert!(matches!(
            resolve("autumn-admin-plugin").unwrap(),
            Resolved::FirstParty(_)
        ));
    }

    #[test]
    fn resolve_accepts_convention_named_community_crates() {
        assert_eq!(
            resolve("autumn-plugin-live-feed").unwrap(),
            Resolved::Community("autumn-plugin-live-feed".to_owned())
        );
    }

    #[test]
    fn resolve_rejects_anything_else() {
        let err = resolve("tokio").unwrap_err();
        assert!(matches!(err, PluginError::UnknownPlugin(_)));
        assert!(err.to_string().contains("autumn plugin list"), "{err}");
    }

    /// AC #1: name, one-line description, and the version compatible with the
    /// app's `autumn-web` — for first-party *and* community crates.
    #[test]
    fn rows_cover_first_party_and_community() {
        let rows = list_rows(Some("0.7.0"), &community());
        assert!(
            rows.iter()
                .filter(|r| r.origin == Origin::FirstParty)
                .count()
                >= 5
        );
        let feed = rows
            .iter()
            .find(|r| r.crate_name == "autumn-plugin-live-feed")
            .expect("community row");
        assert_eq!(feed.origin, Origin::Community);
        assert_eq!(feed.version, "0.3.1");
        assert_eq!(feed.summary, "Live feeds for autumn-web");
    }

    #[test]
    fn first_party_rows_carry_a_summary_and_a_version() {
        for row in list_rows(Some("0.7.0"), &[])
            .iter()
            .filter(|r| r.origin == Origin::FirstParty)
        {
            assert!(!row.summary.is_empty(), "{row:?}");
            assert!(!row.version.is_empty(), "{row:?}");
            assert_eq!(row.compat, Compat::Compatible, "{row:?}");
        }
    }

    /// A first-party plugin cannot be installed into an app on a different
    /// `autumn-web` minor series — the listing has to say so rather than
    /// offering a version that will be refused at `add` time.
    #[test]
    fn rows_mark_incompatibility_against_an_older_app() {
        let rows = list_rows(Some("0.5.0"), &[]);
        assert!(
            rows.iter()
                .filter(|r| r.origin == Origin::FirstParty)
                .all(|r| r.compat == Compat::Incompatible),
            "{rows:?}"
        );
    }

    /// Outside a project there is no app version to compare against; the
    /// listing still renders, marked unknown.
    #[test]
    fn rows_outside_a_project_are_unknown_not_incompatible() {
        let rows = list_rows(None, &[]);
        assert!(
            rows.iter()
                .filter(|r| r.origin == Origin::FirstParty)
                .all(|r| r.compat == Compat::Unknown),
            "{rows:?}"
        );
    }

    /// A community crate's `autumn-web` range is not knowable from the search
    /// API, so it is reported as unknown rather than guessed.
    #[test]
    fn community_rows_have_unknown_compatibility() {
        let rows = list_rows(Some("0.7.0"), &community());
        let feed = rows
            .iter()
            .find(|r| r.crate_name == "autumn-plugin-live-feed")
            .unwrap();
        assert_eq!(feed.compat, Compat::Unknown);
    }

    #[test]
    fn the_table_shows_name_version_and_description() {
        let out = render_list(&list_rows(Some("0.7.0"), &community()), Some("0.7.0"), None);
        assert!(out.contains("autumn-admin-plugin"), "{out}");
        assert!(out.contains("autumn-plugin-live-feed"), "{out}");
        assert!(out.contains("Live feeds for autumn-web"), "{out}");
        assert!(out.contains("0.3.1"), "{out}");
        assert!(out.contains("0.7.0"), "{out}");
    }

    #[test]
    fn the_table_flags_incompatible_rows() {
        let out = render_list(&list_rows(Some("0.5.0"), &[]), Some("0.5.0"), None);
        // Naming the series that would work is the actionable half.
        assert!(out.contains("needs autumn-web 0.7"), "{out}");
    }

    #[test]
    fn a_note_is_rendered_when_crates_io_could_not_be_reached() {
        let out = render_list(
            &list_rows(Some("0.7.0"), &[]),
            Some("0.7.0"),
            Some("offline"),
        );
        assert!(out.contains("offline"), "{out}");
    }

    #[test]
    fn json_output_is_machine_readable() {
        let json = render_list_json(&list_rows(Some("0.7.0"), &community()), Some("0.7.0"));
        let value: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        assert_eq!(value["autumn_web"], "0.7.0");
        let plugins = value["plugins"].as_array().expect("plugins array");
        let admin = plugins
            .iter()
            .find(|p| p["name"] == "autumn-admin-plugin")
            .expect("admin row");
        assert_eq!(admin["origin"], "first-party");
        assert_eq!(admin["compatible"], true);
        assert!(admin["description"].as_str().is_some_and(|d| !d.is_empty()));
    }

    #[test]
    fn the_add_report_lists_post_install_steps() {
        let entry = catalog::lookup("autumn-cache-redis").unwrap();
        let outcome = AddOutcome::Installed {
            plan: Box::new(crate::generate::emit::Plan::new(".")),
            steps: entry.post_install.iter().map(|s| (*s).to_owned()).collect(),
        };
        let out = render_add("autumn-cache-redis", &outcome, false);
        for step in entry.post_install {
            assert!(out.contains(step), "{out}");
        }
    }

    /// A dry run must not claim the plugin was installed.
    #[test]
    fn the_dry_run_report_does_not_claim_an_install() {
        let outcome = AddOutcome::Installed {
            plan: Box::new(crate::generate::emit::Plan::new(".")),
            steps: Vec::new(),
        };
        let out = render_add("autumn-admin-plugin", &outcome, true);
        assert!(out.contains("Dry run"), "{out}");
        assert!(!out.contains("Installed autumn-admin-plugin"), "{out}");
    }

    #[test]
    fn the_already_installed_report_says_nothing_changed() {
        let out = render_add("autumn-admin-plugin", &AddOutcome::AlreadyInstalled, false);
        assert!(out.contains("already installed"), "{out}");
        assert!(out.contains("autumn-admin-plugin"), "{out}");
    }

    /// A community crate gets its dependency written but never an automatic
    /// mount — the report has to say so, and show the derived snippet.
    #[test]
    fn the_dependency_only_report_shows_the_derived_mount() {
        let out = render_add(
            "autumn-plugin-live-feed",
            &AddOutcome::DependencyOnly {
                plan: Box::new(crate::generate::emit::Plan::new(".")),
                dependency_added: true,
                dependency_line: "autumn-plugin-live-feed = \"0.3.1\"".to_owned(),
                mount_snippet: "        .plugin(autumn_plugin_live_feed::LiveFeedPlugin::new())"
                    .to_owned(),
            },
            false,
        );
        assert!(out.contains("autumn-plugin-live-feed = \"0.3.1\""), "{out}");
        assert!(out.contains("LiveFeedPlugin::new()"), "{out}");
        assert!(out.contains("community crate"), "{out}");
    }

    /// A community crate's mount is never written, so a re-run is still
    /// dependency-only: it must not claim the mount is in place, and it must
    /// keep showing the snippet the user has yet to paste.
    #[test]
    fn a_repeated_community_add_still_shows_the_mount() {
        let out = render_add(
            "autumn-plugin-live-feed",
            &AddOutcome::DependencyOnly {
                plan: Box::new(crate::generate::emit::Plan::new(".")),
                dependency_added: false,
                dependency_line: "autumn-plugin-live-feed = \"0.3.1\"".to_owned(),
                mount_snippet: "        .plugin(autumn_plugin_live_feed::LiveFeedPlugin::new())"
                    .to_owned(),
            },
            false,
        );
        assert!(out.contains("already declared"), "{out}");
        assert!(out.contains("LiveFeedPlugin::new()"), "{out}");
        assert!(!out.contains("already installed"), "{out}");
    }

    #[test]
    fn the_manual_report_prints_both_the_dependency_and_the_mount() {
        let out = render_add(
            "autumn-admin-plugin",
            &AddOutcome::Manual {
                reason: "could not find the builder chain".to_owned(),
                dependency_line: "autumn-admin-plugin = \"0.7.0\"".to_owned(),
                mount_snippet: ".plugin(autumn_admin_plugin::AdminPlugin::new())".to_owned(),
                steps: vec!["run `autumn generate admin Post`".to_owned()],
            },
            false,
        );
        assert!(out.contains("autumn-admin-plugin = \"0.7.0\""), "{out}");
        assert!(out.contains("AdminPlugin::new()"), "{out}");
        assert!(out.contains("could not find the builder chain"), "{out}");
    }

    // ── `autumn plugin remove` (issue #1631) ─────────────────────────────────

    fn empty_plan() -> crate::generate::emit::Plan {
        crate::generate::emit::Plan::new(".")
    }

    fn plan_with_one_edit() -> crate::generate::emit::Plan {
        let mut plan = crate::generate::emit::Plan::new(".");
        plan.modify("src/main.rs", "fn main() {}\n");
        plan
    }

    fn removed(
        plan: crate::generate::emit::Plan,
        removed: Vec<Wire>,
        missing: Vec<Wire>,
    ) -> RemoveOutcome {
        RemoveOutcome::Removed {
            plan: Box::new(plan),
            removed,
            missing,
            dependency_retained: None,
            residue: DataResidue::default(),
        }
    }

    /// AC #1: a full removal says both wires came out.
    #[test]
    fn the_remove_report_names_both_wires() {
        let out = render_remove(
            "autumn-admin-plugin",
            &removed(
                plan_with_one_edit(),
                vec![Wire::Mount, Wire::Dependency],
                Vec::new(),
            ),
            false,
        );
        assert!(out.contains("autumn-admin-plugin"), "{out}");
        assert!(out.contains("dependency"), "{out}");
        assert!(out.contains("mount"), "{out}");
    }

    /// AC #2: the default never touches the database, and says exactly what it
    /// left behind and what would remove it.
    #[test]
    fn the_remove_report_lists_data_left_in_place_and_names_the_destructive_flag() {
        let out = render_remove(
            "autumn-media-plugin",
            &RemoveOutcome::Removed {
                plan: Box::new(plan_with_one_edit()),
                removed: vec![Wire::Mount, Wire::Dependency],
                missing: Vec::new(),
                dependency_retained: None,
                residue: DataResidue {
                    migrations: vec!["20260720000000_media_rooms".to_owned()],
                    tables: vec![
                        "media_room_participants".to_owned(),
                        "media_rooms".to_owned(),
                    ],
                },
            },
            false,
        );
        assert!(out.contains("20260720000000_media_rooms"), "{out}");
        assert!(out.contains("media_rooms"), "{out}");
        assert!(out.contains("--drop-data"), "{out}");
        // The reassurance is the point: nothing in the database moved.
        assert!(
            out.contains("left in place") || out.contains("still there"),
            "{out}"
        );
    }

    /// A plugin that owns no database state must not invent a data warning.
    #[test]
    fn the_remove_report_stays_quiet_about_data_when_there_is_none() {
        let out = render_remove(
            "autumn-cache-redis",
            &removed(plan_with_one_edit(), vec![Wire::Mount], Vec::new()),
            false,
        );
        assert!(!out.contains("--drop-data"), "{out}");
    }

    /// AC #4: a partial install is unwired as far as it goes, and the report
    /// names what it could not find.
    #[test]
    fn the_remove_report_names_the_wire_it_could_not_find() {
        let out = render_remove(
            "autumn-admin-plugin",
            &removed(
                plan_with_one_edit(),
                vec![Wire::Dependency],
                vec![Wire::Mount],
            ),
            false,
        );
        assert!(out.to_lowercase().contains("could not find"), "{out}");
        assert!(out.contains("mount"), "{out}");
    }

    /// AC #4: a dependency kept because the app still uses the crate has to say
    /// so, or the user reads a half-finished removal as a bug.
    #[test]
    fn the_remove_report_explains_a_retained_dependency() {
        let out = render_remove(
            "autumn-admin-plugin",
            &RemoveOutcome::Removed {
                plan: Box::new(plan_with_one_edit()),
                removed: vec![Wire::Mount],
                missing: Vec::new(),
                dependency_retained: Some(DependencyKept::StillUsed(
                    "The autumn-admin-plugin dependency was kept: src/support.rs still names it"
                        .to_owned(),
                )),
                residue: DataResidue::default(),
            },
            false,
        );
        assert!(out.contains("src/support.rs"), "{out}");
    }

    /// AC #5: removing something that is not installed says so.
    #[test]
    fn the_not_installed_report_says_nothing_to_do() {
        let out = render_remove(
            "autumn-admin-plugin",
            &RemoveOutcome::NotInstalled {
                residue: DataResidue::default(),
            },
            false,
        );
        assert!(out.contains("not installed"), "{out}");
        assert!(out.contains("nothing to do"), "{out}");
    }

    /// AC #4: the manual fallback prints the exact lines to delete.
    #[test]
    fn the_manual_remove_report_prints_the_lines_to_delete() {
        let out = render_remove(
            "autumn-admin-plugin",
            &RemoveOutcome::Manual {
                reason: "could not identify the mount".to_owned(),
                dependency_line: Some("autumn-admin-plugin = \"0.7.0\"".to_owned()),
                mount_snippet: "        .plugin(autumn_admin_plugin::AdminPlugin::new())"
                    .to_owned(),
                residue: DataResidue::default(),
            },
            false,
        );
        assert!(out.contains("No files were changed"), "{out}");
        assert!(out.contains("autumn-admin-plugin = \"0.7.0\""), "{out}");
        assert!(out.contains("AdminPlugin::new()"), "{out}");
    }

    /// A dry run must not claim the plugin was removed.
    #[test]
    fn the_dry_run_remove_report_does_not_claim_a_removal() {
        let out = render_remove(
            "autumn-admin-plugin",
            &removed(plan_with_one_edit(), vec![Wire::Mount], Vec::new()),
            true,
        );
        assert!(out.contains("Dry run"), "{out}");
        assert!(!out.contains("Removed autumn-admin-plugin"), "{out}");
    }

    // ── AC #3: the dry-run exit-code contract ────────────────────────────────

    #[test]
    fn a_dry_run_with_pending_edits_exits_distinctly() {
        let outcome = removed(plan_with_one_edit(), vec![Wire::Mount], Vec::new());
        assert!(removal_changes_files(&outcome));
        assert_eq!(remove_exit_code(&outcome, true), DRY_RUN_PENDING_EXIT_CODE);
        // A real run of the same outcome is an ordinary success.
        assert_eq!(remove_exit_code(&outcome, false), 0);
    }

    #[test]
    fn a_dry_run_with_nothing_to_do_exits_zero() {
        let outcome = RemoveOutcome::NotInstalled {
            residue: DataResidue::default(),
        };
        assert!(!removal_changes_files(&outcome));
        assert_eq!(remove_exit_code(&outcome, true), 0);
        assert_eq!(remove_exit_code(&outcome, false), 0);
    }

    /// An outcome whose plan turns out to hold no action is "nothing to do"
    /// too — the exit code follows the plan, not the variant.
    #[test]
    fn a_dry_run_whose_plan_is_empty_exits_zero() {
        let outcome = removed(empty_plan(), Vec::new(), vec![Wire::Mount]);
        assert!(!removal_changes_files(&outcome));
        assert_eq!(remove_exit_code(&outcome, true), 0);
    }

    /// The manual fallback is a refusal in both modes: nothing can be changed
    /// automatically, so a dry run of it is not "would change something".
    #[test]
    fn the_manual_fallback_keeps_its_exit_code_under_dry_run() {
        let outcome = RemoveOutcome::Manual {
            reason: "nope".to_owned(),
            dependency_line: None,
            mount_snippet: String::new(),
            residue: DataResidue::default(),
        };
        assert_eq!(remove_exit_code(&outcome, true), MANUAL_FALLBACK_EXIT_CODE);
        assert_eq!(remove_exit_code(&outcome, false), MANUAL_FALLBACK_EXIT_CODE);
    }

    /// A confirmation prompt must not print the database password into a
    /// terminal scrollback or a CI log.
    #[test]
    fn the_confirmation_prompt_redacts_the_database_password() {
        assert_eq!(
            redact_database_url("postgres://app:s3cret@db.internal:5432/app"),
            "postgres://app:***@db.internal:5432/app"
        );
        // Nothing to redact, nothing to mangle.
        assert_eq!(
            redact_database_url("postgres://localhost/app"),
            "postgres://localhost/app"
        );
        assert_eq!(redact_database_url("not a url"), "not a url");
        // A URL with a user but no password has nothing to mask, and must not
        // grow a fake one.
        assert_eq!(
            redact_database_url("postgres://app@db.internal/app"),
            "postgres://app@db.internal/app"
        );
    }

    // ── AC #6: `autumn new --with <plugin>` ──────────────────────────────────

    fn no_community(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn scaffold_preflight_resolves_first_party_plugins_in_order() {
        let names = vec!["autumn-search".to_owned(), "autumn-admin-plugin".to_owned()];
        let resolved =
            preflight_scaffold_plugins(&names, first_party_version(), no_community).unwrap();
        assert_eq!(resolved.len(), 2);
        assert_eq!(resolved[0].name, "autumn-search");
        assert_eq!(resolved[1].name, "autumn-admin-plugin");
        assert_eq!(resolved[0].version, first_party_version());
    }

    /// `--with X --with X` is a typo, not an error: the second one is the same
    /// install, and `plugin add` is idempotent anyway.
    #[test]
    fn scaffold_preflight_deduplicates_repeated_names() {
        let names = vec!["autumn-search".to_owned(), "autumn-search".to_owned()];
        let resolved =
            preflight_scaffold_plugins(&names, first_party_version(), no_community).unwrap();
        assert_eq!(resolved.len(), 1);
    }

    /// An unknown name must fail here — before `autumn new` writes anything.
    #[test]
    fn scaffold_preflight_rejects_an_unknown_plugin() {
        let names = vec!["tokio".to_owned()];
        let err =
            preflight_scaffold_plugins(&names, first_party_version(), no_community).unwrap_err();
        assert!(err.contains("tokio"), "{err}");
        assert!(err.contains("autumn plugin list"), "{err}");
    }

    /// AC #6: version compatibility is checked before any file is written.
    #[test]
    fn scaffold_preflight_refuses_an_incompatible_series() {
        let names = vec!["autumn-admin-plugin".to_owned()];
        let err = preflight_scaffold_plugins(&names, "0.1.0", no_community).unwrap_err();
        assert!(err.contains("0.1.0"), "{err}");
        assert!(err.contains(first_party_version()), "{err}");
    }

    /// A community crate goes through the same gate; its version comes from the
    /// registry lookup, and an unresolvable one is refused rather than guessed.
    #[test]
    fn scaffold_preflight_resolves_a_community_version() {
        let names = vec!["autumn-plugin-live-feed".to_owned()];
        let resolved = preflight_scaffold_plugins(&names, first_party_version(), |name| {
            (name == "autumn-plugin-live-feed").then(|| "0.3.1".to_owned())
        })
        .unwrap();
        assert_eq!(resolved[0].version, "0.3.1");
        assert!(matches!(resolved[0].resolved, Resolved::Community(_)));
    }

    #[test]
    fn scaffold_preflight_refuses_an_unresolvable_community_version() {
        let names = vec!["autumn-plugin-live-feed".to_owned()];
        let err =
            preflight_scaffold_plugins(&names, first_party_version(), no_community).unwrap_err();
        assert!(err.contains("autumn-plugin-live-feed"), "{err}");
    }

    /// crates.io is not trusted to return something writable into a manifest.
    #[test]
    fn scaffold_preflight_refuses_an_implausible_community_version() {
        let names = vec!["autumn-plugin-live-feed".to_owned()];
        let err = preflight_scaffold_plugins(&names, first_party_version(), |_| {
            Some("\"; rm -rf /".to_owned())
        })
        .unwrap_err();
        assert!(err.to_lowercase().contains("version"), "{err}");
    }

    /// Review follow-up: a password in the query string (libpq's
    /// `?password=`) and a `@` inside the password itself both used to be
    /// printed verbatim into stdout — i.e. into a CI log.
    #[test]
    fn the_confirmation_prompt_redacts_every_password_shape() {
        // `@` inside the password: the userinfo split must come from the right.
        assert_eq!(
            redact_database_url("postgres://app:p@ssw0rd@db.internal/app"),
            "postgres://app:***@db.internal/app"
        );
        // libpq's query-parameter form, with no userinfo at all.
        assert_eq!(
            redact_database_url("postgres://db.internal/app?user=app&password=hunter2"),
            "postgres://db.internal/app?user=app&password=***"
        );
        assert_eq!(
            redact_database_url("postgres://app:s3cret@db/app?sslpassword=keysecret"),
            "postgres://app:***@db/app?sslpassword=***"
        );
        // A `@` in the path or query is not a userinfo separator.
        assert_eq!(
            redact_database_url("postgres://db.internal/app?options=-c%20a@b"),
            "postgres://db.internal/app?options=-c%20a@b"
        );
    }
}
