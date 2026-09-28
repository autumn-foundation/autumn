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
pub mod curate;
pub mod index;
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
    /// The index listing. `None` for an unlisted crate: not verified.
    pub listing: Option<index::Listing>,
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
/// Index listings come first, in index order (issue #1625). Then any
/// first-party crate the index does not list, then crates.io results the
/// index does not list. Rows without a listing are unlisted: not verified.
///
/// `app_version` is the app's `autumn-web` requirement, or `None` when the
/// command is run outside a project — in which case nothing can be said about
/// compatibility, so every row is [`Compat::Unknown`] rather than optimistically
/// compatible.
#[must_use]
pub fn list_rows(
    app_version: Option<&str>,
    plugin_index: &index::PluginIndex,
    community: &[registry::CommunityPlugin],
) -> Vec<ListRow> {
    let version = first_party_version();
    let lockstep = app_version.map_or(Compat::Unknown, |app| install::check_compat(app, version));
    let mut rows: Vec<ListRow> = plugin_index
        .visible()
        .map(|listing| {
            let (origin, version, compat) = match listing.origin {
                // Lockstep: the CLI version is the one to install. A failed
                // re-verification still wins over the series match.
                index::ListingOrigin::FirstParty => {
                    let flagged = listing.status == index::Status::Incompatible
                        && app_version
                            .is_some_and(|app| listing.compat(app) == Compat::Incompatible);
                    let compat = if flagged {
                        Compat::Incompatible
                    } else {
                        lockstep
                    };
                    (Origin::FirstParty, version.to_owned(), compat)
                }
                index::ListingOrigin::Community => (
                    Origin::Community,
                    listing.version.clone(),
                    app_version.map_or(Compat::Unknown, |app| listing.compat(app)),
                ),
            };
            ListRow {
                crate_name: listing.name.clone(),
                version,
                summary: listing.description.clone(),
                origin,
                compat,
                listing: Some(listing.clone()),
            }
        })
        .collect();

    // crates.io treats `-`/`_` and case as one name, so the index's spelling
    // and a search result's are the same crate.
    let listed = |rows: &[ListRow], name: &str| {
        rows.iter()
            .any(|row| index::canonical(&row.crate_name) == index::canonical(name))
    };
    for entry in catalog::FIRST_PARTY {
        if !listed(&rows, entry.crate_name) {
            rows.push(ListRow {
                crate_name: entry.crate_name.to_owned(),
                version: version.to_owned(),
                summary: entry.summary.to_owned(),
                origin: Origin::FirstParty,
                compat: lockstep,
                listing: None,
            });
        }
    }
    for found in community {
        if listed(&rows, &found.crate_name) {
            continue;
        }
        rows.push(ListRow {
            crate_name: found.crate_name.clone(),
            version: found.version.clone(),
            summary: found.summary.clone(),
            origin: Origin::Community,
            // The crates.io search response has no `autumn-web` range, so
            // an unlisted crate is reported unknown rather than guessed.
            compat: Compat::Unknown,
            listing: None,
        });
    }
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

    for (listed, heading) in [
        (true, "Listed in the Autumn plugin index"),
        (false, "Unlisted (not in the plugin index, not verified)"),
    ] {
        let group: Vec<&ListRow> = rows
            .iter()
            .filter(|row| row.listing.is_some() == listed)
            .collect();
        if group.is_empty() {
            continue;
        }
        let _ = writeln!(out, "{heading}:");
        for row in group {
            let _ = writeln!(
                out,
                "  {name:<name_width$}  {version:<version_width$}  {summary}{flags}",
                name = row.crate_name,
                version = row.version,
                summary = elide(&row.summary, SUMMARY_WIDTH),
                flags = row_flags(row, app_version),
            );
            if let Some(listing) = &row.listing {
                let _ = writeln!(out, "      {}", listing_facts(row.origin, listing));
                if !listing.note.is_empty() {
                    let _ = writeln!(out, "      note: {}", listing.note);
                }
            }
        }
        out.push('\n');
    }

    if let Some(note) = note {
        let _ = writeln!(out, "Note: {note}");
    }
    let _ = write!(out, "Install one with `autumn plugin add <name>`.");
    out
}

/// The bracketed flags after a row's summary.
fn row_flags(row: &ListRow, app_version: Option<&str>) -> String {
    use std::fmt::Write as _;

    let listing = row.listing.as_ref();
    // The flag shows unless the app is on an older series the listing
    // still supports.
    let flagged = listing.is_some_and(|l| {
        l.status == index::Status::Incompatible
            && !(app_version.is_some() && row.compat == Compat::Compatible)
            && app_version.is_none_or(|app| l.flag_applies(app) || l.compat(app) == Compat::Unknown)
    });
    let mut flags = String::new();
    match row.compat {
        _ if flagged => {
            flags.push_str("  [incompatible: failed re-verification]");
        }
        // Name the series that WOULD work, rather than only saying this one
        // will not: on an older app the bare word "incompatible" leaves the
        // reader with no next step.
        Compat::Incompatible => {
            let range = match listing {
                Some(l) if row.origin == Origin::Community => l.autumn_web.clone(),
                _ => install::supported_range(&row.version),
            };
            let _ = write!(flags, "  [needs autumn-web {range}]");
        }
        Compat::Unknown if row.origin == Origin::FirstParty => {
            flags.push_str("  [compatibility unknown]");
        }
        Compat::Compatible | Compat::Unknown => {}
    }
    match listing {
        None => flags.push_str("  [unlisted: not verified]"),
        Some(l) => {
            if l.tier == index::Tier::Experimental {
                flags.push_str("  [EXPERIMENTAL API]");
            }
            if let Some(app) = app_version
                && !flagged
                && !l.verified_for(app)
            {
                let _ = write!(flags, "  [not verified on autumn-web {app}]");
            }
        }
    }
    flags
}

/// The facts line under a listed row: origin, trust, tier, conformance.
fn listing_facts(origin: Origin, listing: &index::Listing) -> String {
    let origin = match origin {
        Origin::FirstParty => "first-party",
        Origin::Community => "community",
    };
    format!(
        "{origin} · {} · {} · {}",
        listing.trust_label(),
        tier_text(listing),
        conformance_text(listing)
    )
}

/// `stable API`, or `experimental API: <surfaces>`.
fn tier_text(listing: &index::Listing) -> String {
    match listing.tier {
        index::Tier::Stable => "stable API".to_owned(),
        index::Tier::Experimental => format!(
            "experimental API: {}",
            listing.experimental_surfaces.join(", ")
        ),
    }
}

/// The last conformance run, in words.
fn conformance_text(listing: &index::Listing) -> String {
    let run = &listing.conformance;
    match run.result {
        index::CheckOutcome::Pass => format!("plugin-check pass on autumn-web {}", run.autumn_web),
        index::CheckOutcome::Fail => format!("plugin-check FAIL on autumn-web {}", run.autumn_web),
        index::CheckOutcome::Exempt => format!("plugin-check exempt: {}", run.reason),
    }
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
                "listed": row.listing.is_some(),
                "verified": row
                    .listing
                    .as_ref()
                    .is_some_and(|l| l.status == index::Status::Listed),
                "status": row.listing.as_ref().map(|l| l.status),
                "autumn_web_range": row.listing.as_ref().map(|l| l.autumn_web.clone()),
                "tier": row.listing.as_ref().map(|l| l.tier),
                "experimental_surfaces": row
                    .listing
                    .as_ref()
                    .map(|l| l.experimental_surfaces.clone()),
                "trust": row.listing.as_ref().map(|l| serde_json::json!({
                    "kind": l.trust,
                    "label": l.trust_label(),
                    "capabilities": l.capabilities,
                    "routes": l.routes,
                    "artifact_sha256": (!l.artifact_sha256.is_empty()).then_some(&l.artifact_sha256),
                    "grants": l.grants,
                    "quotas": l.quotas,
                    "limits": l.limits,
                })),
                "conformance": row.listing.as_ref().map(|l| &l.conformance),
                "note": row.listing.as_ref().map(|l| l.note.clone()),
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

/// What the index says about a name `plugin add` was given.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Standing<'a> {
    /// Listed or flagged: the index vouches for (or warns about) it.
    Listed(&'a index::Listing),
    /// Once listed, now removed. Treated as unlisted.
    Delisted(&'a index::Listing),
    /// Not in the index. Nothing about it is verified.
    Unlisted,
}

/// Look `name` up in the index.
#[must_use]
pub fn standing<'a>(plugin_index: &'a index::PluginIndex, name: &str) -> Standing<'a> {
    match plugin_index.get(name) {
        Some(listing) if listing.status == index::Status::Delisted => Standing::Delisted(listing),
        Some(listing) => Standing::Listed(listing),
        None => Standing::Unlisted,
    }
}

/// The trust review `plugin add` prints before it changes any file
/// (issue #1625, AC 6).
#[must_use]
pub fn render_trust(name: &str, standing: &Standing<'_>) -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    let listing = match standing {
        Standing::Listed(listing) => listing,
        Standing::Delisted(listing) => {
            let _ = writeln!(out, "Trust review for {name}:");
            let _ = writeln!(
                out,
                "  UNLISTED: {name} was removed from the Autumn plugin index: {}",
                listing.note
            );
            append_unlisted_warning(&mut out);
            return out;
        }
        Standing::Unlisted => {
            let _ = writeln!(out, "Trust review for {name}:");
            let _ = writeln!(out, "  UNLISTED: {name} is not in the Autumn plugin index.");
            append_unlisted_warning(&mut out);
            return out;
        }
    };
    let _ = writeln!(out, "Trust review for {name} (Autumn plugin index):");
    let _ = writeln!(out, "  trust:       {}", listing.trust_label());
    if listing.trust == index::Trust::Sandboxed {
        let _ = writeln!(out, "  artifact:    sha256 {}", listing.artifact_sha256);
    }
    if listing.trust == index::Trust::Native {
        let _ = writeln!(
            out,
            "               it gets the whole AppBuilder: config, credentials, network, database"
        );
    }
    let tier = tier_text(listing);
    if listing.tier == index::Tier::Experimental {
        let _ = writeln!(
            out,
            "  API tier:    EXPERIMENTAL — {tier}; it can break in any autumn-web release"
        );
    } else {
        let _ = writeln!(out, "  API tier:    {tier}");
    }
    let _ = writeln!(
        out,
        "  conformance: {} ({})",
        conformance_text(listing),
        listing.conformance.checked
    );
    let _ = writeln!(out, "  supports:    autumn-web {}", listing.autumn_web);
    if !listing.note.is_empty() {
        let _ = writeln!(out, "  note:        {}", listing.note);
    }
    out
}

/// The warning for a crate the index does not vouch for.
fn append_unlisted_warning(out: &mut String) {
    out.push_str(
        "  It is not verified: no conformance run, no declared autumn-web range, no tier.\n  \
         Read its source before you install it.\n",
    );
    out.push_str("  trust:       ");
    out.push_str(index::FULL_TRUST_LABEL);
    out.push_str(" (a crates.io crate gets the whole AppBuilder)\n");
}

/// Refuse a listing that must not be installed into an app on `app`.
///
/// # Errors
///
/// A message that names the reason, when the listing failed
/// re-verification on this series or its range excludes the app.
pub fn gate_listing(listing: &index::Listing, app: Option<&str>) -> Result<(), String> {
    let flagged = listing.status == index::Status::Incompatible;
    let compat = app.map_or(Compat::Unknown, |app| listing.compat(app));
    // Fail closed: with no concrete app version, a flag cannot be ruled out.
    // A sandboxed artifact is not tied to an `autumn-web` series, so its flag
    // applies to every app.
    if flagged && (compat == Compat::Unknown || listing.trust == index::Trust::Sandboxed) {
        let why = if listing.trust == index::Trust::Sandboxed {
            "a sandboxed flag applies to every app"
        } else {
            "this app's autumn-web version is not a plain version"
        };
        return Err(format!(
            "`{}` failed re-verification on autumn-web {} ({}); {why}. No files were changed.",
            listing.name, listing.conformance.autumn_web, listing.note
        ));
    }
    let Some(app) = app else {
        return Ok(());
    };
    if compat != Compat::Incompatible {
        return Ok(());
    }
    if listing.flag_applies(app) {
        return Err(format!(
            "`{}` failed re-verification on autumn-web {} and this app uses autumn-web {app}: {}. \
             No files were changed.",
            listing.name, listing.conformance.autumn_web, listing.note
        ));
    }
    // A first-party listing is lockstep; `install::plan_add` gives the
    // series diagnostic for it.
    if listing.origin == index::ListingOrigin::FirstParty {
        return Ok(());
    }
    Err(format!(
        "`{}` {} supports autumn-web {}, but this app uses autumn-web {app}. No files were changed.",
        listing.name, listing.version, listing.autumn_web
    ))
}

/// The manual steps for a sandboxed listing. `plugin add` writes no file for
/// one: its artifact is not a crate dependency.
#[must_use]
pub fn render_sandboxed_steps(listing: &index::Listing) -> String {
    format!(
        "\nNo files were changed: `{name}` is a sandboxed plugin. It ships as a\n\
         `.autumn-plugin` artifact, not as a crate dependency.\n\n\
         1. Get the artifact from {repository}.\n\
         2. Review it: `autumn plugin inspect <file>.autumn-plugin`. Its artifact\n   \
            sha256 must be {digest}, and its capabilities {capabilities}.\n\
         3. Mount it: `SandboxedPlugin::from_file(Path::new(\"plugins/<file>.autumn-plugin\"))`.\n\n\
         See docs/guide/sandboxed-plugins.md.",
        name = listing.name,
        repository = listing.repository,
        capabilities = listing.capabilities.join(", "),
        digest = listing.artifact_sha256,
    )
}

/// The requirement `plugin add` writes for a listed community crate: the
/// verified version exactly. A caret would admit unverified patch releases.
#[must_use]
pub fn pinned_version(version: &str) -> String {
    format!("={version}")
}

/// Refuse when the app already declares `crate_name` at a requirement other
/// than the verified `pinned` one. The trust review vouches for that version
/// only.
///
/// # Errors
///
/// A message naming both requirements. No file is changed.
pub fn check_existing_pin(manifest: &str, crate_name: &str, pinned: &str) -> Result<(), String> {
    check_existing_source(manifest, crate_name, pinned)?;
    if !install::dependency_present(manifest, crate_name) {
        return Ok(());
    }
    match install::declared_dependency_version(manifest, crate_name) {
        Some(declared) if declared == pinned => Ok(()),
        declared => Err(format!(
            "Cargo.toml already declares `{crate_name}`{}, but the index verified `{pinned}`. \
             Set `{crate_name} = \"{pinned}\"` or remove the entry, then re-run. No files were changed.",
            declared.map_or_else(
                || " from a path or git source".to_owned(),
                |v| format!(" = \"{v}\"")
            )
        )),
    }
}

/// Refuse an existing declaration that is not the crates.io crate the index
/// reviewed: a variant key, or a `path`, `git`, `registry` or renamed
/// `package` entry. `release` names the reviewed version in the message.
///
/// # Errors
///
/// A message ready to print. No file is changed.
pub fn check_existing_source(
    manifest: &str,
    crate_name: &str,
    release: &str,
) -> Result<(), String> {
    // crates.io treats `-`/`_` and case as one name, so a variant key is the
    // same crate. A second key would be a duplicate dependency.
    if let Some(key) = install::declared_dependency_key(manifest, crate_name)
        && key != crate_name
    {
        return Err(format!(
            "Cargo.toml declares this crate as `{}`. Rename the key to `{crate_name}`, then \
             re-run. No files were changed.",
            index::sanitize(&key)
        ));
    }
    // A version from a path, git or other registry is not the reviewed
    // crates.io release, whatever it says.
    if install::dependency_present(manifest, crate_name)
        && install::dependency_has_alternate_source(manifest, crate_name)
    {
        return Err(format!(
            "Cargo.toml takes `{crate_name}` from a path, git or other registry, but the index \
             verified the crates.io release `{release}`. Set `{crate_name} = \"{release}\"` or \
             remove the entry, then re-run. No files were changed."
        ));
    }
    Ok(())
}

/// [`check_listed_declaration`] for `plugin add`: prints a notice, returns
/// a refusal. `version` is the listed pin (first-party: this release).
fn print_listed_declaration(root: &Path, resolved: &Resolved, version: &str) -> Result<(), String> {
    let version = match resolved {
        Resolved::FirstParty(_) => first_party_version(),
        Resolved::Community(_) => version,
    };
    if let Some(notice) = check_listed_declaration(root, resolved, version)? {
        println!("{notice}");
    }
    Ok(())
}

/// Check an existing declaration of a listed crate before `plugin add` or
/// `--with` wires it. A community crate must be the verified `=` pin, and no
/// `[patch]` may redirect it. A first-party crate must come from crates.io;
/// a `[patch]` to a local checkout is the dev workflow, so it gets a notice.
///
/// # Errors
///
/// A message ready to print. No file is changed.
pub fn check_listed_declaration(
    root: &Path,
    resolved: &Resolved,
    version: &str,
) -> Result<Option<String>, String> {
    let crate_name = match resolved {
        Resolved::FirstParty(entry) => entry.crate_name,
        Resolved::Community(name) => name.as_str(),
    };
    let manifest = std::fs::read_to_string(install::manifest_path(root)).unwrap_or_default();
    let manifest = install::with_inherited_dependency(root, &manifest, crate_name);
    let patched = install::patched_by(root, crate_name, version);
    match resolved {
        Resolved::Community(_) => {
            check_existing_pin(&manifest, crate_name, version)?;
            if let Some(patch) = patched {
                return Err(format!(
                    "{patch} redirects `{crate_name}`, but the index verified its crates.io \
                     release `{version}`. Remove the patch entry, then re-run. No files were changed."
                ));
            }
            Ok(None)
        }
        Resolved::FirstParty(_) => {
            check_existing_source(&manifest, crate_name, version)?;
            // Lockstep: the declared requirement must admit this release, or
            // Cargo keeps an older crate than the one the review describes.
            if let Some(declared) = install::declared_dependency_version(&manifest, crate_name)
                && let (Ok(req), Ok(release)) = (
                    semver::VersionReq::parse(&declared),
                    semver::Version::parse(version),
                )
                && !req.matches(&release)
            {
                return Err(format!(
                    "Cargo.toml declares `{crate_name} = \"{}\"`, which excludes the verified \
                     release {version}. Set `{crate_name} = \"={version}\"`, then re-run. No \
                     files were changed.",
                    index::sanitize(&declared)
                ));
            }
            // A requirement that admits the release can still lock another
            // one: Cargo builds what `Cargo.lock` says.
            if let Some(locked) = install::locked_version(root, crate_name)
                && semver::Version::parse(&locked).ok() != semver::Version::parse(version).ok()
            {
                return Err(format!(
                    "Cargo.lock locks `{crate_name}` at {}, but the index verified {version}. Run \
                     `cargo update -p {crate_name} --precise {version}`, then re-run. No files \
                     were changed.",
                    index::sanitize(&locked)
                ));
            }
            Ok(patched.map(|patch| {
                format!(
                    "Note: {patch} redirects `{crate_name}`. The trust review covers the \
                     crates.io release, not the patched source."
                )
            }))
        }
    }
}

/// Load the index ([`index::OVERRIDE_ENV`] or the bundled copy) and refuse
/// one that breaks an admission rule: its trust labels cannot be shown.
///
/// Staleness is not checked here. A consumer on an older app is not stale.
pub fn load_index() -> Result<index::Loaded, String> {
    use std::fmt::Write as _;

    let loaded = index::load_from_env().map_err(|err| err.to_string())?;
    let findings = index::validate(&loaded.index);
    if findings.is_empty() {
        return Ok(loaded);
    }
    let mut message = format!(
        "the plugin index at {} breaks {} admission rule(s):",
        source_label(&loaded.source),
        findings.len()
    );
    for finding in findings {
        let _ = write!(
            message,
            "\n  {}: {}",
            index::sanitize(&finding.plugin),
            index::sanitize(&finding.message)
        );
    }
    Err(message)
}

/// Where the index came from, for messages.
fn source_label(source: &index::Source) -> String {
    match source {
        index::Source::Bundled => "the bundled copy".to_owned(),
        index::Source::Override(path) => path.display().to_string(),
    }
}

/// The app's `autumn-web` version, or `None` when it cannot be determined.
fn app_version(root: &Path) -> Option<String> {
    // What Cargo resolved, when it has: a `"0.7"` requirement may be 0.7.1.
    if let Some(locked) = install::locked_version(root, "autumn-web") {
        return Some(locked);
    }
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
                "--offline: crates.io was not queried, so unlisted crates are not shown."
                    .to_owned(),
            ),
        )
    } else {
        registry::search().map_or_else(
            || {
                (
                    Vec::new(),
                    Some("could not reach crates.io, so unlisted crates are not shown.".to_owned()),
                )
            },
            |found| (found, None),
        )
    };
    let loaded = match load_index() {
        Ok(loaded) => loaded,
        Err(err) => {
            eprintln!("autumn plugin list: {err}");
            return 1;
        }
    };
    if let index::Source::Override(path) = &loaded.source {
        eprintln!("Using the plugin index at {}.", path.display());
    }
    let rows = list_rows(app.as_deref(), &loaded.index, &community);
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
    let loaded = match load_index() {
        Ok(loaded) => loaded,
        Err(err) => {
            eprintln!("autumn plugin add: {err}");
            return 1;
        }
    };
    let standing = standing(&loaded.index, opts.name);
    // A listed name wins over a case or `-`/`_` variant of it: crates.io
    // treats them as one crate, so the flag and the pin must apply.
    let name = match standing {
        Standing::Listed(listing) => listing.name.as_str(),
        Standing::Delisted(_) | Standing::Unlisted => opts.name,
    };
    let resolved = match resolve(name) {
        Ok(resolved) => resolved,
        Err(err) => {
            eprintln!("autumn plugin add: {err}");
            return 1;
        }
    };
    if let index::Source::Override(path) = &loaded.source {
        println!("Using the plugin index at {}.", path.display());
    }

    // The trust review comes first: before any gate, plan or write (AC 6).
    println!("{}", render_trust(name, &standing));
    if let Standing::Listed(listing) = standing {
        if let Err(err) = gate_listing(listing, app_version(opts.root).as_deref()) {
            eprintln!("autumn plugin add: {err}");
            return 1;
        }
        if listing.trust == index::Trust::Sandboxed {
            eprintln!("{}", render_sandboxed_steps(listing));
            return MANUAL_FALLBACK_EXIT_CODE;
        }
    }

    let listed_version = match standing {
        Standing::Listed(listing) => Some(pinned_version(&listing.version)),
        Standing::Delisted(_) | Standing::Unlisted => None,
    };
    // A listed crate's existing declaration must be the reviewed one.
    if let Some(version) = &listed_version
        && let Err(err) = print_listed_declaration(opts.root, &resolved, version)
    {
        eprintln!("autumn plugin add: {err}");
        return 1;
    }
    let outcome = match (&resolved, listed_version) {
        (Resolved::FirstParty(entry), _) => {
            install::plan_add(opts.root, entry, first_party_version())
        }
        // A listed crate installs the version the index verified. No
        // crates.io lookup, so this works with `--offline`.
        (Resolved::Community(crate_name), Some(version)) => {
            install::plan_add_community(opts.root, crate_name, &version)
        }
        (Resolved::Community(crate_name), None) => {
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

    let report = render_add(name, &outcome, opts.dry_run);
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
///
/// `drop_would_run` is the `--drop-data` half: a plugin already unwired whose
/// tables are still there has no file to change, but a real run WOULD change
/// the database — and "`--dry-run` exits 3 whenever a real run would change
/// something" has to mean the database too, or a script reads "nothing left to
/// clean up" off a plan that still drops tables.
#[must_use]
pub fn remove_exit_code(outcome: &RemoveOutcome, dry_run: bool, drop_would_run: bool) -> i32 {
    if matches!(outcome, RemoveOutcome::Manual { .. }) || leaves_a_hand_edit(outcome) {
        return MANUAL_FALLBACK_EXIT_CODE;
    }
    if dry_run && (removal_changes_files(outcome) || drop_would_run) {
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

    // The manual fallback is settled FIRST, before `--drop-data` prints a
    // statement or asks for a confirmation. The plugin is still wired into the
    // app on this path, so dropping the tables it is about to read would break
    // a running app — and confirming a destructive step that then silently does
    // nothing is worse still. Neither the code nor the database is touched.
    if let RemoveOutcome::Manual { residue, .. } = &outcome {
        let needs_data_note = opts.drop_data && !residue.is_empty();
        eprintln!("{}", render_remove(opts.name, &outcome, opts.dry_run));
        if needs_data_note {
            eprintln!(
                "\n--drop-data was not applied, and nothing was asked: {} is still wired into\nthis app, and dropping what it owns while it is still mounted would break it.\nUnwire it by hand as above, then re-run with `--drop-data`.",
                opts.name
            );
        }
        return MANUAL_FALLBACK_EXIT_CODE;
    }

    // `--drop-data` is decided and CONFIRMED before a single file is written.
    // Asking after the edits are on disk means a declined prompt leaves the app
    // already unwired while the message says "Aborted" — the one ordering a
    // destructive flag must not have.
    let drop_step = match &resolved {
        Resolved::FirstParty(entry) if opts.drop_data => {
            let absent = matches!(outcome, RemoveOutcome::NotInstalled { .. });
            match prepare_drop_data(entry, opts, absent) {
                Ok(step) => step,
                Err(code) => return code,
            }
        }
        _ => DropStep::Nothing,
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

    println!("{}", render_remove(opts.name, &outcome, opts.dry_run));
    if matches!(resolved, Resolved::Community(_)) {
        println!(
            "\n{} is a community crate: this CLI has no list of the migrations or tables it\nowns, so nothing here can tell you what it left in the database. Check the\ncrate's README before assuming the removal was complete.",
            opts.name
        );
    }

    if let DropStep::Confirmed(plan) = &drop_step {
        let code = apply_drop_data(plan);
        if code != 0 {
            return code;
        }
    }

    remove_exit_code(
        &outcome,
        opts.dry_run,
        matches!(drop_step, DropStep::WouldRun),
    )
}

/// What the `--drop-data` step resolved to, before anything is applied.
#[derive(Debug)]
enum DropStep {
    /// Not asked for, nothing declared, or the statements were handed to the
    /// user to run themselves.
    Nothing,
    /// A dry run: these statements WOULD run. Carried separately from
    /// [`Self::Confirmed`] because a dry run must still say, in its exit code,
    /// that a real run would change something.
    WouldRun,
    /// Confirmed and ready to apply once the code edits land.
    Confirmed(Box<ConfirmedDrop>),
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
/// `Err(code)` is a refusal that aborts the whole command with **no** file
/// changed — not the manifest, not `main.rs`, not the database.
fn prepare_drop_data(
    entry: &'static catalog::CatalogEntry,
    opts: &RemoveOptions<'_>,
    not_installed_here: bool,
) -> Result<DropStep, i32> {
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
            Ok(DropStep::Nothing)
        }
        DropDataDecision::PrintOnly { reason, statements } => {
            // Exit 2, not 0: the database was NOT changed, and a script reading
            // `remove --drop-data && echo dropped` must not print "dropped".
            // Same meaning `plugin add`/`remove` already give 2 — "nothing was
            // changed automatically; apply the printed lines by hand".
            eprintln!(
                "\n--drop-data was not applied — {reason}.\nNothing was changed at all: the database is untouched, and the plugin is still\nwired (re-run without `--drop-data` to unwire just the code). Run these\nyourself, in this order:\n"
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
                // A real run WOULD change the database, so a dry run has to say
                // so in its exit code even when no file would move.
                return Ok(DropStep::WouldRun);
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
            Ok(DropStep::Confirmed(Box::new(ConfirmedDrop {
                url,
                statements,
                crate_name: entry.crate_name,
            })))
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
    /// The index listing, when the name is listed. Gated again against the
    /// scaffolded manifest: a `--starter` pin is not known at preflight.
    pub listing: Option<index::Listing>,
}

/// Resolve and version-check every `--with` name.
///
/// # Errors
///
/// A message ready to print when a name is unknown, incompatible, or (for a
/// community crate) has no resolvable version.
pub fn preflight_scaffold_plugins(
    names: &[String],
    plugin_index: &index::PluginIndex,
    scaffold_autumn_web: Option<&str>,
    resolve_community_version: impl Fn(&str) -> Option<String>,
) -> Result<Vec<ScaffoldPlugin>, String> {
    let mut out: Vec<ScaffoldPlugin> = Vec::with_capacity(names.len());
    for name in names {
        // The index decides first, as for `plugin add` (#1625): a listed name
        // resolves to its listing, a sandboxed or flagged one is refused, and a
        // listed community crate uses its verified pin.
        let listed = match standing(plugin_index, name) {
            Standing::Listed(listing) => Some(listing),
            Standing::Delisted(_) | Standing::Unlisted => None,
        };
        let name = listed.map_or(name.as_str(), |l| l.name.as_str());
        if let Some(listing) = listed {
            if listing.trust == index::Trust::Sandboxed {
                return Err(format!(
                    "`{name}` is a sandboxed plugin; `--with` cannot wire it. Run \
                     `autumn plugin add {name}` after the app exists for the manual steps \
                     — no files were written"
                ));
            }
            gate_listing(listing, scaffold_autumn_web)
                .map_err(|err| err.replace("No files were changed.", "No files were written."))?;
        }
        // `--with X --with X` is a typo, not a conflict: the second one names
        // the same install, and `plugin add` is idempotent regardless.
        if out
            .iter()
            .any(|already| index::canonical(&already.name) == index::canonical(name))
        {
            continue;
        }
        let resolved = resolve(name).map_err(|err| err.to_string())?;
        let version = match (&resolved, listed) {
            (Resolved::FirstParty(_), _) => first_party_version().to_owned(),
            (Resolved::Community(_), Some(listing)) => pinned_version(&listing.version),
            (Resolved::Community(crate_name), None) => {
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
        let name = &name.to_owned();
        // A first-party plugin is released in lockstep with `autumn-web`, so
        // this can only fail if the scaffold ever stops pinning the CLI's own
        // series — which is exactly the regression worth catching before a
        // project exists on disk. A community crate's range is not knowable,
        // so it is not gated here (`Compat::Unknown` passes).
        // `None` when the pin is not knowable yet — a `--starter` brings its own
        // manifest, which does not exist until the starter is fetched. The
        // version answer then comes from `plan_add` reading the real manifest,
        // and `wire_scaffold_plugins` reports it as "the app was created, the
        // plugin was not wired" rather than as a bare failure.
        if let Some(pinned) = scaffold_autumn_web
            && matches!(resolved, Resolved::FirstParty(_))
            && install::check_compat(pinned, &version) == Compat::Incompatible
        {
            return Err(format!(
                "`{name} {version}` supports autumn-web {}, but this scaffold pins autumn-web {pinned} — no files were written.\nInstall the matching CLI series and try again.",
                install::supported_range(&version)
            ));
        }
        out.push(ScaffoldPlugin {
            name: name.clone(),
            resolved,
            version,
            listing: listed.cloned(),
        });
    }
    Ok(out)
}

/// Wire every preflighted plugin into the freshly scaffolded app at `root`.
///
/// Returns the process exit code: `0` when every plugin was wired, or the
/// worst code any single install produced. Runs only after
/// [`preflight_scaffold_plugins`] has passed, so nothing here can be the first
/// place a bad name is noticed — but it IS the first place a `--starter`'s own
/// `autumn-web` pin can be read, so an incompatibility there surfaces here
/// rather than in the preflight. The app has already been scaffolded by then,
/// so that is reported as an app that exists without the plugin (exit
/// [`MANUAL_FALLBACK_EXIT_CODE`], "install it yourself later") rather than as
/// a failed `autumn new`.
#[must_use]
pub fn wire_scaffold_plugins(root: &Path, plugins: &[ScaffoldPlugin]) -> i32 {
    let mut worst = 0;
    for plugin in plugins {
        // Gate a listing again, now that the manifest exists: a `--starter`
        // pins its own `autumn-web`, and may already declare the crate.
        if let Some(listing) = &plugin.listing {
            let refused = if matches!(plugin.resolved, Resolved::Community(_)) {
                gate_listing(listing, app_version(root).as_deref())
            } else {
                Ok(())
            }
            .and_then(|()| check_listed_declaration(root, &plugin.resolved, &plugin.version));
            if let Ok(Some(notice)) = &refused {
                println!("{notice}");
            }
            if let Err(err) = refused {
                eprintln!(
                    "\nautumn new: the app was created, but {} was not wired — {err}",
                    plugin.name
                );
                worst = worst.max(MANUAL_FALLBACK_EXIT_CODE);
                continue;
            }
        }
        let outcome = match &plugin.resolved {
            Resolved::FirstParty(entry) => install::plan_add(root, entry, &plugin.version),
            Resolved::Community(crate_name) => {
                install::plan_add_community(root, crate_name, &plugin.version)
            }
        };
        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(err) => {
                eprintln!(
                    "\nautumn new: the app was created, but {} was not wired — {err}\nInstall it later with `autumn plugin add {}`.",
                    plugin.name, plugin.name
                );
                worst = worst.max(MANUAL_FALLBACK_EXIT_CODE);
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

    const RELEASE: &str = env!("CARGO_PKG_VERSION");

    /// This release's series, e.g. `0.7`: the bundled first-party range.
    fn series() -> String {
        autumn_web::plugin_contract::lockstep_range(RELEASE)
    }

    fn bundled() -> index::PluginIndex {
        index::parse(index::BUNDLED).expect("bundled index")
    }

    /// The bundled index plus one listed community plugin.
    fn index_with(listing: index::Listing) -> index::PluginIndex {
        let mut plugin_index = bundled();
        plugin_index.plugins.push(listing);
        plugin_index
    }

    fn listed_community() -> index::Listing {
        let mut listing = bundled()
            .get("autumn-admin-plugin")
            .expect("admin listing")
            .clone();
        listing.name = "autumn-plugin-live-feed".to_owned();
        listing.description = "Live feeds (listed)".to_owned();
        listing.origin = index::ListingOrigin::Community;
        listing.version = "0.3.0".to_owned();
        listing
    }

    fn row<'a>(rows: &'a [ListRow], name: &str) -> &'a ListRow {
        rows.iter()
            .find(|r| r.crate_name == name)
            .unwrap_or_else(|| panic!("{name} missing from {rows:?}"))
    }

    // ── `plugin list` against the index (issue #1625) ───────────────────

    /// AC 1 + AC 6: every first-party row carries its listing, so trust,
    /// tier and conformance are known at discovery time.
    #[test]
    fn first_party_rows_carry_their_index_listing() {
        let rows = list_rows(Some(RELEASE), &bundled(), &[]);
        for entry in catalog::FIRST_PARTY {
            let listing = row(&rows, entry.crate_name)
                .listing
                .as_ref()
                .unwrap_or_else(|| panic!("{} has no listing", entry.crate_name));
            assert_eq!(listing.trust_label(), index::FULL_TRUST_LABEL);
        }
    }

    /// AC 2: a listed community crate resolves from the index: its version
    /// and range come from the listing, not from a crates.io guess.
    #[test]
    fn a_listed_community_crate_resolves_from_the_index() {
        let rows = list_rows(Some(RELEASE), &index_with(listed_community()), &community());
        let feed = row(&rows, "autumn-plugin-live-feed");
        assert!(feed.listing.is_some());
        assert_eq!(feed.version, "0.3.0");
        assert_eq!(feed.summary, "Live feeds (listed)");
        assert_eq!(feed.compat, Compat::Compatible);
        let count = rows
            .iter()
            .filter(|r| r.crate_name == "autumn-plugin-live-feed")
            .count();
        assert_eq!(count, 1, "listed and searched rows must merge");
    }

    /// AC 2: a crates.io result with no listing is kept, but unlisted.
    #[test]
    fn a_crates_io_result_without_a_listing_is_unlisted() {
        let rows = list_rows(Some(RELEASE), &bundled(), &community());
        assert!(row(&rows, "autumn-plugin-live-feed").listing.is_none());
    }

    /// AC 2: the fallback is visibly marked in the table.
    #[test]
    fn the_table_marks_unlisted_rows_as_unverified() {
        let out = render_list(
            &list_rows(Some(RELEASE), &bundled(), &community()),
            Some(RELEASE),
            None,
        );
        assert!(out.contains("Unlisted"), "{out}");
        assert!(out.contains("[unlisted: not verified]"), "{out}");
        let feed = out
            .lines()
            .find(|l| l.contains("autumn-plugin-live-feed"))
            .expect("feed line");
        assert!(feed.contains("[unlisted: not verified]"), "{feed}");
    }

    /// AC 1 + AC 6: the table shows trust, tier and conformance per listing.
    #[test]
    fn the_table_shows_trust_tier_and_conformance_for_listings() {
        let out = render_list(
            &list_rows(Some(RELEASE), &bundled(), &[]),
            Some(RELEASE),
            None,
        );
        assert!(out.contains(index::FULL_TRUST_LABEL), "{out}");
        assert!(out.contains("stable API"), "{out}");
        assert!(
            out.contains("plugin-check pass on autumn-web 0.7.0"),
            "{out}"
        );
        assert!(out.contains("plugin-check exempt"), "{out}");
    }

    /// AC 5: a listing on experimental surface is marked on its own line.
    #[test]
    fn the_table_marks_experimental_listings() {
        let mut listing = listed_community();
        let surface = autumn_web::plugin_contract::experimental_surface_names()
            .next()
            .expect("an experimental surface")
            .to_owned();
        listing.tier = index::Tier::Experimental;
        listing.experimental_surfaces = vec![surface.clone()];
        let out = render_list(
            &list_rows(Some(RELEASE), &index_with(listing), &[]),
            Some(RELEASE),
            None,
        );
        let feed = out
            .lines()
            .find(|l| l.contains("autumn-plugin-live-feed"))
            .expect("feed line");
        assert!(feed.contains("[EXPERIMENTAL API]"), "{feed}");
        assert!(
            out.contains(&format!("experimental API: {surface}")),
            "{out}"
        );
    }

    /// AC 4: a flagged listing is shown as incompatible, with the note.
    #[test]
    fn the_table_flags_a_listing_that_failed_re_verification() {
        let mut listing = listed_community();
        listing.status = index::Status::Incompatible;
        listing.conformance.result = index::CheckOutcome::Fail;
        listing.note = "route-collision on re-verification".to_owned();
        let rows = list_rows(Some(RELEASE), &index_with(listing), &[]);
        assert_eq!(
            row(&rows, "autumn-plugin-live-feed").compat,
            Compat::Incompatible
        );
        let out = render_list(&rows, Some(RELEASE), None);
        assert!(
            out.contains("[incompatible: failed re-verification]"),
            "{out}"
        );
        assert!(out.contains("route-collision on re-verification"), "{out}");
    }

    /// `--with x --with x` with crates.io-equivalent spellings is one plugin.
    #[test]
    fn scaffold_plugins_dedupe_by_canonical_name() {
        let names = [
            "autumn-admin-plugin".to_owned(),
            "autumn_admin_plugin".to_owned(),
        ];
        let plugins = preflight_scaffold_plugins(
            &names,
            &index::load(None).unwrap().index,
            Some(RELEASE),
            |_| None,
        )
        .expect("preflight");
        assert_eq!(plugins.len(), 1);
    }

    /// A crates.io result spelled with `_` where the index has `-` is the
    /// same crate: one listed row, no unlisted duplicate.
    #[test]
    fn a_separator_variant_search_result_merges_with_its_listing() {
        let found = registry::CommunityPlugin {
            crate_name: "autumn_plugin_live_feed".to_owned(),
            version: "0.3.0".to_owned(),
            summary: "Live feed".to_owned(),
        };
        let rows = list_rows(Some(RELEASE), &index_with(listed_community()), &[found]);
        let feed: Vec<&ListRow> = rows
            .iter()
            .filter(|r| index::canonical(&r.crate_name) == "autumn-plugin-live-feed")
            .collect();
        assert_eq!(
            feed.len(),
            1,
            "{:?}",
            rows.iter().map(|r| &r.crate_name).collect::<Vec<_>>()
        );
        assert!(feed[0].listing.is_some());
    }

    /// AC 4: a delisted plugin is not shown as listed. If crates.io still
    /// has it, it is shown unlisted.
    #[test]
    fn a_delisted_plugin_falls_back_to_unlisted() {
        let mut listing = listed_community();
        listing.status = index::Status::Delisted;
        listing.conformance.result = index::CheckOutcome::Fail;
        listing.note = "failed on two releases".to_owned();
        let rows = list_rows(Some(RELEASE), &index_with(listing.clone()), &[]);
        assert!(
            rows.iter()
                .all(|r| r.crate_name != "autumn-plugin-live-feed")
        );
        let rows = list_rows(Some(RELEASE), &index_with(listing), &community());
        assert!(row(&rows, "autumn-plugin-live-feed").listing.is_none());
    }

    /// AC 4: a listing not verified on the app's series says so.
    #[test]
    fn the_table_says_when_a_listing_is_not_verified_on_this_app() {
        let mut listing = listed_community();
        listing.autumn_web = ">=0.1".to_owned();
        let out = render_list(
            &list_rows(Some("99.0.0"), &index_with(listing), &[]),
            Some("99.0.0"),
            None,
        );
        let feed = out
            .lines()
            .find(|l| l.contains("autumn-plugin-live-feed"))
            .expect("feed line");
        assert!(
            feed.contains("[not verified on autumn-web 99.0.0]"),
            "{feed}"
        );
    }

    /// AC 1 + AC 2: the JSON carries the same trust facts.
    #[test]
    fn json_carries_the_listing_facts() {
        let json = render_list_json(
            &list_rows(Some(RELEASE), &bundled(), &community()),
            Some(RELEASE),
        );
        let value: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        let plugins = value["plugins"].as_array().expect("plugins");
        let admin = plugins
            .iter()
            .find(|p| p["name"] == "autumn-admin-plugin")
            .expect("admin");
        assert_eq!(admin["listed"], true);
        assert_eq!(admin["trust"]["kind"], "native");
        assert_eq!(admin["trust"]["label"], index::FULL_TRUST_LABEL);
        assert_eq!(admin["tier"], "stable");
        assert_eq!(admin["status"], "listed");
        assert_eq!(admin["conformance"]["result"], "pass");
        assert_eq!(admin["autumn_web_range"], series());
        let feed = plugins
            .iter()
            .find(|p| p["name"] == "autumn-plugin-live-feed")
            .expect("feed");
        assert_eq!(feed["listed"], false);
        assert_eq!(feed["verified"], false);
        assert!(feed["trust"].is_null());
    }

    /// AC 5 + AC 6 in JSON: experimental surfaces, capabilities, and a
    /// flagged row that is not verified.
    #[test]
    fn json_carries_experimental_sandboxed_and_flagged_facts() {
        let mut feed = listed_community();
        feed.tier = index::Tier::Experimental;
        feed.experimental_surfaces = vec!["x".to_owned()];
        let mut hello = listed_community();
        hello.name = "autumn-plugin-hello".to_owned();
        hello.trust = index::Trust::Sandboxed;
        hello.capabilities = vec!["http-request".to_owned()];
        hello.artifact_sha256 = "ef".repeat(32);
        hello.grants.hosts = vec!["api.example.com".to_owned()];
        hello.quotas.insert("kv_reads".to_owned(), 7);
        hello.limits.insert("fuel".to_owned(), 9);
        hello.routes = vec!["GET /hello".to_owned()];
        let mut broken = flagged_on(RELEASE);
        broken.name = "autumn-plugin-broken".to_owned();
        let mut plugin_index = bundled();
        plugin_index.plugins.extend([feed, hello, broken]);
        let json = render_list_json(&list_rows(Some(RELEASE), &plugin_index, &[]), Some(RELEASE));
        let value: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        let find = |name: &str| {
            value["plugins"]
                .as_array()
                .unwrap()
                .iter()
                .find(|p| p["name"] == name)
                .cloned()
                .unwrap_or_else(|| panic!("{name} missing"))
        };
        let feed = find("autumn-plugin-live-feed");
        assert_eq!(feed["tier"], "experimental");
        assert_eq!(feed["experimental_surfaces"][0], "x");
        let hello = find("autumn-plugin-hello");
        assert_eq!(hello["trust"]["artifact_sha256"], "ef".repeat(32));
        assert_eq!(hello["trust"]["grants"]["hosts"][0], "api.example.com");
        assert_eq!(hello["trust"]["quotas"]["kv_reads"], 7);
        assert_eq!(hello["trust"]["limits"]["fuel"], 9);
        assert_eq!(hello["trust"]["kind"], "sandboxed");
        assert_eq!(hello["trust"]["capabilities"][0], "http-request");
        assert_eq!(hello["trust"]["routes"][0], "GET /hello");
        let broken = find("autumn-plugin-broken");
        assert_eq!(broken["listed"], true);
        assert_eq!(broken["verified"], false);
        assert_eq!(broken["status"], "incompatible");
        assert_eq!(broken["compatible"], false);
    }

    // ── `plugin add` trust surface (issue #1625) ────────────────────────

    #[test]
    fn standing_distinguishes_listed_delisted_and_unlisted() {
        let plugin_index = bundled();
        assert!(matches!(
            standing(&plugin_index, "autumn-admin-plugin"),
            Standing::Listed(_)
        ));
        assert_eq!(
            standing(&plugin_index, "autumn-plugin-x"),
            Standing::Unlisted
        );
        let mut gone = listed_community();
        gone.status = index::Status::Delisted;
        let plugin_index = index_with(gone);
        assert!(matches!(
            standing(&plugin_index, "autumn-plugin-live-feed"),
            Standing::Delisted(_)
        ));
    }

    /// AC 6: the full-trust label, tier and conformance are shown.
    #[test]
    fn the_trust_review_for_a_listing_names_every_fact() {
        let plugin_index = bundled();
        let out = render_trust(
            "autumn-admin-plugin",
            &standing(&plugin_index, "autumn-admin-plugin"),
        );
        assert!(out.contains(index::FULL_TRUST_LABEL), "{out}");
        assert!(out.contains("stable API"), "{out}");
        assert!(out.contains("plugin-check pass on autumn-web"), "{out}");
        assert!(out.contains(&format!("autumn-web {}", series())), "{out}");
    }

    /// AC 2 + AC 6: an unlisted crate is marked unverified before install.
    #[test]
    fn the_trust_review_for_an_unlisted_crate_says_unverified() {
        let out = render_trust("autumn-plugin-x", &Standing::Unlisted);
        assert!(out.contains("UNLISTED"), "{out}");
        assert!(out.contains("not verified"), "{out}");
        assert!(out.contains(index::FULL_TRUST_LABEL), "{out}");
    }

    #[test]
    fn the_trust_review_for_a_delisted_crate_gives_the_reason() {
        let mut gone = listed_community();
        gone.status = index::Status::Delisted;
        gone.note = "failed plugin-check on two releases".to_owned();
        let out = render_trust("autumn-plugin-live-feed", &Standing::Delisted(&gone));
        assert!(out.contains("UNLISTED"), "{out}");
        assert!(out.contains("failed plugin-check on two releases"), "{out}");
    }

    /// AC 6: a sandboxed listing shows its capability manifest.
    #[test]
    fn the_trust_review_for_a_sandboxed_listing_shows_capabilities() {
        let mut listing = listed_community();
        listing.trust = index::Trust::Sandboxed;
        listing.capabilities = vec!["http-request".to_owned(), "kv".to_owned()];
        listing.artifact_sha256 = "ab".repeat(32);
        let out = render_trust("autumn-plugin-live-feed", &Standing::Listed(&listing));
        assert!(out.contains("http-request, kv"), "{out}");
        assert!(out.contains(&"ab".repeat(32)), "{out}");
        assert!(!out.contains(index::FULL_TRUST_LABEL), "{out}");
    }

    #[test]
    fn the_trust_review_marks_experimental_api() {
        let mut listing = listed_community();
        listing.tier = index::Tier::Experimental;
        listing.experimental_surfaces = vec!["x".to_owned()];
        let out = render_trust("autumn-plugin-live-feed", &Standing::Listed(&listing));
        assert!(out.contains("EXPERIMENTAL"), "{out}");
    }

    /// AC 4: a flagged listing is refused on the failed series.
    #[test]
    fn the_gate_refuses_a_flagged_listing() {
        let mut listing = listed_community();
        listing.status = index::Status::Incompatible;
        listing.conformance.result = index::CheckOutcome::Fail;
        listing.note = "route-collision".to_owned();
        let err = gate_listing(&listing, Some(RELEASE)).unwrap_err();
        assert!(err.contains("re-verification"), "{err}");
        assert!(err.contains("route-collision"), "{err}");
    }

    #[test]
    fn the_gate_refuses_a_range_that_excludes_the_app() {
        let err = gate_listing(&listed_community(), Some("0.0.1")).unwrap_err();
        assert!(err.contains("0.0.1"), "{err}");
        assert!(err.contains(&series()), "{err}");
    }

    fn flagged_on(release: &str) -> index::Listing {
        let mut listing = listed_community();
        listing.status = index::Status::Incompatible;
        listing.conformance.result = index::CheckOutcome::Fail;
        listing.conformance.autumn_web = release.to_owned();
        listing.note = "route-collision".to_owned();
        listing
    }

    /// Fail closed: a flagged listing is refused when the app's version is
    /// unknown (a path dependency, or a range).
    #[test]
    fn the_gate_refuses_a_flagged_listing_on_an_unknown_app() {
        let listing = flagged_on(RELEASE);
        assert!(gate_listing(&listing, None).is_err());
        assert!(gate_listing(&listing, Some(">=0.1, <99")).is_err());
    }

    /// A sandboxed artifact is not tied to an `autumn-web` series. A flag on
    /// it (a failed load, or an unconsented upgrade) applies to every app.
    #[test]
    fn the_gate_refuses_a_flagged_sandboxed_listing_on_every_app() {
        let mut listing = flagged_on("99.0.0");
        listing.trust = index::Trust::Sandboxed;
        listing.capabilities = vec!["http-request".to_owned()];
        listing.autumn_web = ">=0.0.1, <100".to_owned();
        let err = gate_listing(&listing, Some("0.0.1")).unwrap_err();
        assert!(err.contains("re-verification"), "{err}");
    }

    /// On an app older than the failed series, the reason is the range.
    #[test]
    fn the_gate_names_the_range_for_an_older_app() {
        let mut listing = flagged_on("0.9.0");
        listing.autumn_web = ">=0.8, <0.10".to_owned();
        let err = gate_listing(&listing, Some(RELEASE)).unwrap_err();
        assert!(err.contains(">=0.8, <0.10"), "{err}");
        assert!(!err.contains("re-verification"), "{err}");
    }

    fn project_with(cargo: &str) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), cargo).unwrap();
        tmp
    }

    /// A `[patch]` that redirects a listed community crate is refused; the
    /// trust review would describe code Cargo does not build.
    #[test]
    fn a_patched_community_crate_is_refused() {
        let x = Resolved::Community("autumn-plugin-x".to_owned());
        let patched = project_with(
            "[package]\nname = \"a\"\n\n[dependencies]\nautumn-plugin-x = \"=0.3.0\"\n\n\
             [patch.crates-io]\nautumn_plugin_x = { path = \"../x\" }\n",
        );
        let err = check_listed_declaration(patched.path(), &x, "=0.3.0").unwrap_err();
        assert!(err.contains("[patch.crates-io]"), "{err}");
        // A renamed patch entry patches its `package`, whatever the key.
        let renamed = project_with(
            "[package]\nname = \"a\"\n\n[dependencies]\nautumn-plugin-x = \"=0.3.0\"\n\n\
             [patch.crates-io]\nlocal-x = { package = \"autumn-plugin-x\", path = \"../x\" }\n",
        );
        assert!(check_listed_declaration(renamed.path(), &x, "=0.3.0").is_err());
        let clean = project_with(
            "[package]\nname = \"a\"\n\n[dependencies]\nautumn-plugin-x = \"=0.3.0\"\n",
        );
        assert_eq!(
            check_listed_declaration(clean.path(), &x, "=0.3.0"),
            Ok(None)
        );
    }

    /// First-party: the entry must come from crates.io, and a `[patch]` to a
    /// local checkout (the dev workflow) gets a notice, not a refusal.
    #[test]
    fn a_first_party_declaration_is_source_checked() {
        let admin = resolve("autumn-admin-plugin").unwrap();
        let path = project_with(
            "[package]\nname = \"a\"\n\n[dependencies]\nautumn-admin-plugin = { path = \"../admin\" }\n",
        );
        let err = check_listed_declaration(path.path(), &admin, RELEASE).unwrap_err();
        assert!(err.contains("path, git or other registry"), "{err}");
        let patched = project_with(
            "[package]\nname = \"a\"\n\n[dependencies]\n\n[patch.crates-io]\n\
             autumn-admin-plugin = { path = \"../admin\" }\n",
        );
        let notice = check_listed_declaration(patched.path(), &admin, RELEASE).unwrap();
        assert!(notice.is_some_and(|n| n.contains("[patch.crates-io]")));
        // A requirement that excludes this release keeps an older crate.
        let old = project_with(
            "[package]\nname = \"a\"\n\n[dependencies]\nautumn-admin-plugin = \"=0.0.1\"\n",
        );
        let err = check_listed_declaration(old.path(), &admin, RELEASE).unwrap_err();
        assert!(err.contains("excludes the verified release"), "{err}");
        let caret = project_with(&format!(
            "[package]\nname = \"a\"\n\n[dependencies]\nautumn-admin-plugin = \"{}\"\n",
            series()
        ));
        assert!(check_listed_declaration(caret.path(), &admin, RELEASE).is_ok());
        // The requirement admits the release, but the lockfile holds another.
        let lock = |v: &str| {
            format!(
                "version = 4\n\n[[package]]\nname = \"autumn-admin-plugin\"\nversion = \"{v}\"\n"
            )
        };
        std::fs::write(caret.path().join("Cargo.lock"), lock("0.0.1")).unwrap();
        let err = check_listed_declaration(caret.path(), &admin, RELEASE).unwrap_err();
        assert!(err.contains("Cargo.lock locks"), "{err}");
        std::fs::write(caret.path().join("Cargo.lock"), lock(RELEASE)).unwrap();
        assert!(check_listed_declaration(caret.path(), &admin, RELEASE).is_ok());
    }

    /// A `{ workspace = true }` entry is checked against the workspace
    /// root's `[workspace.dependencies]`, as Cargo resolves it.
    #[test]
    fn a_workspace_inherited_pin_is_resolved() {
        let check = |root_dep: Option<&str>| {
            let tmp = tempfile::tempdir().unwrap();
            let mut root = "[workspace]\nmembers = [\"app\"]\n".to_owned();
            if let Some(dep) = root_dep {
                root = root + "\n[workspace.dependencies]\nautumn-plugin-x = " + dep + "\n";
            }
            std::fs::write(tmp.path().join("Cargo.toml"), root).unwrap();
            let app = tmp.path().join("app");
            std::fs::create_dir_all(&app).unwrap();
            let member = "[package]\nname = \"app\"\n\n[dependencies]\n\
                          autumn-plugin-x = { workspace = true, features = [\"a\"] }\n";
            std::fs::write(app.join("Cargo.toml"), member).unwrap();
            let manifest = install::with_inherited_dependency(&app, member, "autumn-plugin-x");
            check_existing_pin(&manifest, "autumn-plugin-x", "=0.3.0")
        };
        assert!(check(Some("\"=0.3.0\"")).is_ok());
        assert!(check(Some("{ version = \"=0.3.0\" }")).is_ok());
        assert!(check(Some("\"0.4\"")).is_err());
        assert!(check(Some("{ path = \"../x\", version = \"=0.3.0\" }")).is_err());
        // Not defined in the workspace: unresolvable, so refused.
        assert!(check(None).is_err());
    }

    /// An existing requirement other than the verified pin is refused.
    #[test]
    fn an_existing_unpinned_requirement_is_refused() {
        let manifest = "[package]\nname = \"x\"\n\n[dependencies]\nautumn-plugin-x = \"0.4\"\n";
        let err = check_existing_pin(manifest, "autumn-plugin-x", "=0.3.0").unwrap_err();
        assert!(err.contains("0.4") && err.contains("=0.3.0"), "{err}");
        let path = "[dependencies]\nautumn-plugin-x = { path = \"../x\" }\n";
        assert!(check_existing_pin(path, "autumn-plugin-x", "=0.3.0").is_err());
        // A matching version from another source is not the reviewed crate.
        for source in [
            "path = \"../x\"",
            "git = \"https://example.com/x\"",
            "registry = \"other\"",
        ] {
            let alt =
                format!("[dependencies]\nautumn-plugin-x = {{ {source}, version = \"=0.3.0\" }}\n");
            assert!(
                check_existing_pin(&alt, "autumn-plugin-x", "=0.3.0").is_err(),
                "{alt}"
            );
        }
        // A crates.io-equivalent spelling is the same crate: a second key
        // would be a duplicate dependency.
        let variant = "[dependencies]\nautumn_plugin_X = \"=0.3.0\"\n";
        let err = check_existing_pin(variant, "autumn-plugin-x", "=0.3.0").unwrap_err();
        assert!(err.contains("autumn_plugin_X"), "{err}");
        // A key renamed to another package compiles that package.
        let renamed = "[dependencies]\nautumn-plugin-x = { package = \"other-crate\", version = \"=0.3.0\" }\n";
        assert!(check_existing_pin(renamed, "autumn-plugin-x", "=0.3.0").is_err());
        let same = "[dependencies]\nautumn-plugin-x = { package = \"autumn_plugin_x\", version = \"=0.3.0\" }\n";
        assert!(check_existing_pin(same, "autumn-plugin-x", "=0.3.0").is_ok());
        let table =
            "[dependencies]\nautumn-plugin-x = { version = \"=0.3.0\", features = [\"a\"] }\n";
        assert!(check_existing_pin(table, "autumn-plugin-x", "=0.3.0").is_ok());
        let pinned = "[dependencies]\nautumn-plugin-x = \"=0.3.0\"\n";
        assert!(check_existing_pin(pinned, "autumn-plugin-x", "=0.3.0").is_ok());
        assert!(check_existing_pin("[dependencies]\n", "autumn-plugin-x", "=0.3.0").is_ok());
    }

    /// A listed community crate is pinned to its verified version.
    #[test]
    fn a_listed_community_crate_is_pinned_exactly() {
        assert_eq!(pinned_version("0.3.0"), "=0.3.0");
    }

    #[test]
    fn the_gate_passes_a_compatible_or_unknown_app() {
        assert!(gate_listing(&listed_community(), Some(RELEASE)).is_ok());
        assert!(gate_listing(&listed_community(), None).is_ok());
    }

    #[test]
    fn the_sandboxed_steps_point_at_inspect_and_from_file() {
        let mut listing = listed_community();
        listing.trust = index::Trust::Sandboxed;
        listing.capabilities = vec!["http-request".to_owned()];
        listing.artifact_sha256 = "cd".repeat(32);
        let out = render_sandboxed_steps(&listing);
        assert!(out.contains("autumn plugin inspect"), "{out}");
        assert!(out.contains(&"cd".repeat(32)), "{out}");
        assert!(out.contains("SandboxedPlugin::from_file"), "{out}");
        assert!(out.contains("No files were changed"), "{out}");
    }

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
        let rows = list_rows(Some("0.7.0"), &bundled(), &community());
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
        for row in list_rows(Some("0.7.0"), &bundled(), &[])
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
        let rows = list_rows(Some("0.5.0"), &bundled(), &[]);
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
        let rows = list_rows(None, &bundled(), &[]);
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
        let rows = list_rows(Some("0.7.0"), &bundled(), &community());
        let feed = rows
            .iter()
            .find(|r| r.crate_name == "autumn-plugin-live-feed")
            .unwrap();
        assert_eq!(feed.compat, Compat::Unknown);
    }

    #[test]
    fn the_table_shows_name_version_and_description() {
        let out = render_list(
            &list_rows(Some("0.7.0"), &bundled(), &community()),
            Some("0.7.0"),
            None,
        );
        assert!(out.contains("autumn-admin-plugin"), "{out}");
        assert!(out.contains("autumn-plugin-live-feed"), "{out}");
        assert!(out.contains("Live feeds for autumn-web"), "{out}");
        assert!(out.contains("0.3.1"), "{out}");
        assert!(out.contains("0.7.0"), "{out}");
    }

    #[test]
    fn the_table_flags_incompatible_rows() {
        let out = render_list(
            &list_rows(Some("0.5.0"), &bundled(), &[]),
            Some("0.5.0"),
            None,
        );
        // Naming the series that would work is the actionable half.
        assert!(out.contains("needs autumn-web 0.7"), "{out}");
    }

    #[test]
    fn a_note_is_rendered_when_crates_io_could_not_be_reached() {
        let out = render_list(
            &list_rows(Some("0.7.0"), &bundled(), &[]),
            Some("0.7.0"),
            Some("offline"),
        );
        assert!(out.contains("offline"), "{out}");
    }

    #[test]
    fn json_output_is_machine_readable() {
        let json = render_list_json(
            &list_rows(Some("0.7.0"), &bundled(), &community()),
            Some("0.7.0"),
        );
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
        assert_eq!(
            remove_exit_code(&outcome, true, false),
            DRY_RUN_PENDING_EXIT_CODE
        );
        // A real run of the same outcome is an ordinary success.
        assert_eq!(remove_exit_code(&outcome, false, false), 0);
    }

    #[test]
    fn a_dry_run_with_nothing_to_do_exits_zero() {
        let outcome = RemoveOutcome::NotInstalled {
            residue: DataResidue::default(),
        };
        assert!(!removal_changes_files(&outcome));
        assert_eq!(remove_exit_code(&outcome, true, false), 0);
        assert_eq!(remove_exit_code(&outcome, false, false), 0);
    }

    /// An outcome whose plan turns out to hold no action is "nothing to do"
    /// too — the exit code follows the plan, not the variant.
    #[test]
    fn a_dry_run_whose_plan_is_empty_exits_zero() {
        let outcome = removed(empty_plan(), Vec::new(), vec![Wire::Mount]);
        assert!(!removal_changes_files(&outcome));
        assert_eq!(remove_exit_code(&outcome, true, false), 0);
    }

    /// The plugin is already unwired, but `--drop-data` would still drop its
    /// tables. No file would move, so the file-level check says "nothing to
    /// do" — and a dry run that answered `0` there would tell a script the
    /// cleanup is finished while a real run still drops data.
    #[test]
    fn a_dry_run_with_only_pending_database_work_still_exits_three() {
        let outcome = RemoveOutcome::NotInstalled {
            residue: DataResidue {
                migrations: vec!["20260720000000_media_rooms".to_owned()],
                tables: vec!["media_rooms".to_owned()],
            },
        };
        assert!(!removal_changes_files(&outcome));
        assert_eq!(
            remove_exit_code(&outcome, true, true),
            DRY_RUN_PENDING_EXIT_CODE
        );
        // A real run applies it, so it is an ordinary success.
        assert_eq!(remove_exit_code(&outcome, false, true), 0);
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
        assert_eq!(
            remove_exit_code(&outcome, true, false),
            MANUAL_FALLBACK_EXIT_CODE
        );
        assert_eq!(
            remove_exit_code(&outcome, false, false),
            MANUAL_FALLBACK_EXIT_CODE
        );
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

    /// The pin `autumn new`'s own template writes, as the preflight sees it.
    const PINNED: Option<&str> = Some(env!("CARGO_PKG_VERSION"));

    fn panics_on_lookup(name: &str) -> Option<String> {
        panic!("a listed crate must not be looked up on crates.io: {name}")
    }

    /// `autumn new --with` takes the same index decisions as `plugin add`: a
    /// listed community crate uses its verified pin, with no crates.io lookup.
    #[test]
    fn scaffold_preflight_pins_a_listed_community_crate() {
        let names = vec!["autumn_plugin_LIVE_feed".to_owned()];
        let resolved = preflight_scaffold_plugins(
            &names,
            &index_with(listed_community()),
            PINNED,
            panics_on_lookup,
        )
        .unwrap();
        assert_eq!(resolved[0].name, "autumn-plugin-live-feed");
        assert_eq!(resolved[0].version, "=0.3.0");
    }

    #[test]
    fn scaffold_preflight_refuses_a_flagged_listing() {
        let names = vec!["autumn-plugin-live-feed".to_owned()];
        let err = preflight_scaffold_plugins(
            &names,
            &index_with(flagged_on(RELEASE)),
            PINNED,
            panics_on_lookup,
        )
        .unwrap_err();
        assert!(err.contains("re-verification"), "{err}");
    }

    #[test]
    fn scaffold_preflight_refuses_a_sandboxed_listing() {
        let mut listing = listed_community();
        listing.trust = index::Trust::Sandboxed;
        listing.capabilities = vec!["http-request".to_owned()];
        let names = vec!["autumn-plugin-live-feed".to_owned()];
        let err =
            preflight_scaffold_plugins(&names, &index_with(listing), PINNED, panics_on_lookup)
                .unwrap_err();
        assert!(err.contains("sandboxed"), "{err}");
    }

    fn starter_project(cargo: &str) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), cargo).unwrap();
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::write(
            tmp.path().join("src/main.rs"),
            "#[autumn_web::main]\nasync fn main() { autumn_web::app().run().await; }\n",
        )
        .unwrap();
        tmp
    }

    fn listed_scaffold_plugin() -> ScaffoldPlugin {
        let names = vec!["autumn-plugin-live-feed".to_owned()];
        preflight_scaffold_plugins(
            &names,
            &index_with(listed_community()),
            None,
            panics_on_lookup,
        )
        .unwrap()
        .remove(0)
    }

    /// A `--starter` pin is known only after the starter exists. The listing
    /// is gated again against it before the dependency is added.
    #[test]
    fn wiring_regates_a_listing_against_the_starter_manifest() {
        let cargo = "[package]\nname = \"s\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n\
                     [dependencies]\nautumn-web = \"0.0.1\"\n";
        let tmp = starter_project(cargo);
        let code = wire_scaffold_plugins(tmp.path(), &[listed_scaffold_plugin()]);
        assert_ne!(code, 0);
        let after = std::fs::read_to_string(tmp.path().join("Cargo.toml")).unwrap();
        assert_eq!(after, cargo, "a refused listing writes nothing");
    }

    /// A starter that already declares the crate another way keeps
    /// unreviewed code; the wiring refuses it.
    #[test]
    fn wiring_enforces_the_pin_against_starter_dependencies() {
        let cargo = format!(
            "[package]\nname = \"s\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n\
             [dependencies]\nautumn-web = \"{RELEASE}\"\nautumn-plugin-live-feed = \"0.4\"\n"
        );
        let tmp = starter_project(&cargo);
        let code = wire_scaffold_plugins(tmp.path(), &[listed_scaffold_plugin()]);
        assert_ne!(code, 0);
        let after = std::fs::read_to_string(tmp.path().join("Cargo.toml")).unwrap();
        assert_eq!(after, cargo);
    }

    #[test]
    fn scaffold_preflight_resolves_first_party_plugins_in_order() {
        let names = vec!["autumn-search".to_owned(), "autumn-admin-plugin".to_owned()];
        let resolved =
            preflight_scaffold_plugins(&names, &bundled(), PINNED, no_community).unwrap();
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
            preflight_scaffold_plugins(&names, &bundled(), PINNED, no_community).unwrap();
        assert_eq!(resolved.len(), 1);
    }

    /// An unknown name must fail here — before `autumn new` writes anything.
    #[test]
    fn scaffold_preflight_rejects_an_unknown_plugin() {
        let names = vec!["tokio".to_owned()];
        let err = preflight_scaffold_plugins(&names, &bundled(), PINNED, no_community).unwrap_err();
        assert!(err.contains("tokio"), "{err}");
        assert!(err.contains("autumn plugin list"), "{err}");
    }

    /// AC #6: version compatibility is checked before any file is written.
    #[test]
    fn scaffold_preflight_refuses_an_incompatible_series() {
        let names = vec!["autumn-admin-plugin".to_owned()];
        let err = preflight_scaffold_plugins(&names, &bundled(), Some("0.1.0"), no_community)
            .unwrap_err();
        assert!(err.contains("0.1.0"), "{err}");
        assert!(err.contains(first_party_version()), "{err}");
    }

    /// A community crate goes through the same gate; its version comes from the
    /// registry lookup, and an unresolvable one is refused rather than guessed.
    #[test]
    fn scaffold_preflight_resolves_a_community_version() {
        let names = vec!["autumn-plugin-live-feed".to_owned()];
        let resolved = preflight_scaffold_plugins(&names, &bundled(), PINNED, |name| {
            (name == "autumn-plugin-live-feed").then(|| "0.3.1".to_owned())
        })
        .unwrap();
        assert_eq!(resolved[0].version, "0.3.1");
        assert!(matches!(resolved[0].resolved, Resolved::Community(_)));
    }

    #[test]
    fn scaffold_preflight_refuses_an_unresolvable_community_version() {
        let names = vec!["autumn-plugin-live-feed".to_owned()];
        let err = preflight_scaffold_plugins(&names, &bundled(), PINNED, no_community).unwrap_err();
        assert!(err.contains("autumn-plugin-live-feed"), "{err}");
    }

    /// crates.io is not trusted to return something writable into a manifest.
    #[test]
    fn scaffold_preflight_refuses_an_implausible_community_version() {
        let names = vec!["autumn-plugin-live-feed".to_owned()];
        let err = preflight_scaffold_plugins(&names, &bundled(), PINNED, |_| {
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

    /// Codex review: a `--starter` brings its own manifest, which does not
    /// exist until it is fetched — so there is no pin to gate against yet. The
    /// name still has to resolve before anything is written; the version answer
    /// comes later, from the starter's real manifest.
    #[test]
    fn scaffold_preflight_without_a_known_pin_still_resolves_but_does_not_gate() {
        let names = vec!["autumn-admin-plugin".to_owned()];
        let resolved = preflight_scaffold_plugins(&names, &bundled(), None, no_community).unwrap();
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].version, first_party_version());

        // An unknown name is still refused, because that IS knowable up front.
        let err = preflight_scaffold_plugins(&["tokio".to_owned()], &bundled(), None, no_community)
            .unwrap_err();
        assert!(err.contains("autumn plugin list"), "{err}");
    }
}
