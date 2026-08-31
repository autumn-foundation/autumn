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

use std::path::Path;

use catalog::CatalogEntry;
use install::{AddOutcome, Compat, PluginError};

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

/// Exit code for the manual-fallback outcome: nothing was written, and the
/// dependency line plus mount snippet were printed for the user to apply.
pub const MANUAL_FALLBACK_EXIT_CODE: i32 = 2;

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
            dependency_line,
            mount_snippet,
            ..
        } => {
            if dry_run {
                let _ = writeln!(
                    out,
                    "\nDry run: nothing was written. `autumn plugin add {entry_name}` would add {dependency_line}."
                );
            } else {
                let _ = writeln!(out, "\nAdded {dependency_line}.");
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
}
