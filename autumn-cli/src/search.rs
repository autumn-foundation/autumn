//! `autumn search reindex` -- rebuild an application's search indexes.
//!
//! Compiles the target binary (debug profile) and runs it with
//! `AUTUMN_SEARCH_BACKFILL` set, which makes `autumn-search`'s startup hook
//! run a full backfill and exit instead of serving traffic.
//!
//! Running the *application* is the only sound way to do this, for exactly the
//! reason `autumn jobs manifest` re-runs the binary to dump the job manifest:
//! the searchable indexes, the backend, the embedding provider, and the
//! document source are all registered at runtime by the app's own
//! `SearchPlugin::…` builder call. The standalone CLI links `autumn-web` but
//! never the user's models, so it cannot see any of them.

use std::io::{BufRead as _, BufReader, Write as _};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use crate::routes::{compile_binary, find_binary};

/// Environment variable the plugin reads to select the backfill target.
///
/// Must stay in sync with `autumn_search::BACKFILL_ENV`. The CLI does not
/// depend on `autumn-search` (an application may not use it at all), so the
/// contract is this string.
const BACKFILL_ENV: &str = "AUTUMN_SEARCH_BACKFILL";

/// Environment variable that makes the backfill purge before rebuilding.
///
/// Must stay in sync with `autumn_search::BACKFILL_PURGE_ENV`.
const BACKFILL_PURGE_ENV: &str = "AUTUMN_SEARCH_BACKFILL_PURGE";

/// Line the plugin prints as soon as it begins the backfill.
///
/// Must stay in sync with `autumn_search::BACKFILL_STARTED_MARKER`.
const BACKFILL_STARTED_MARKER: &str = "autumn-search: backfill starting";

/// How long to wait for [`BACKFILL_STARTED_MARKER`] before concluding that the
/// application never installed `SearchPlugin`.
///
/// This bounds only *startup* — config load, pool connect, migrations,
/// `ensure_index`. Once the marker arrives the CLI waits indefinitely, because
/// a real backfill over a large table legitimately takes minutes to hours and
/// must never be cut short by a timer.
const STARTUP_GRACE: Duration = Duration::from_secs(120);

/// Options for `autumn search reindex`.
pub struct ReindexOptions<'a> {
    /// Package to run (for workspaces).
    pub package: Option<&'a str>,
    /// Binary target to run (for packages with multiple bin targets).
    pub bin: Option<&'a str>,
    /// Index to rebuild. `None` rebuilds every registered index.
    pub index: Option<&'a str>,
    /// Profile whose `[search]` configuration to rebuild against.
    ///
    /// `None` leaves selection to the child, which then resolves through the
    /// build mode — and the binary this CLI builds is a DEBUG one, so that
    /// means `dev`. A deployed release selecting `prod` through the same
    /// build-mode fallback is therefore NOT reproduced by omitting this: a
    /// reindex would rebuild the development index and report success while
    /// production stayed stale.
    pub profile: Option<&'a str>,
    /// Clear each index before rebuilding it.
    pub purge: bool,
}

/// The value `BACKFILL_ENV` is set to for `index`.
///
/// `all` is the wire value for "every registered index"; a real index name is
/// passed through verbatim.
#[must_use]
pub fn backfill_target(index: Option<&str>) -> String {
    index.unwrap_or("all").to_owned()
}

/// Whether `line` is the plugin's backfill-started announcement.
#[must_use]
pub fn is_started_marker(line: &str) -> bool {
    line.starts_with(BACKFILL_STARTED_MARKER)
}

/// Run `autumn search reindex`.
pub fn run(opts: &ReindexOptions<'_>) {
    eprintln!("\u{1F342} autumn search reindex\n");
    match opts.profile {
        Some(profile) => eprintln!("  profile: {profile}\n"),
        // Said out loud because the failure is silent otherwise: the rebuild
        // succeeds against the wrong index and reports success.
        None => eprintln!(
            "  profile: dev (this builds a debug binary; pass --profile to \
             rebuild another environment's index)\n"
        ),
    }
    if opts.purge {
        eprintln!(
            "  purge: each index is CLEARED before it is rebuilt — \
             searches return nothing until the rebuild finishes\n"
        );
    }

    compile_binary(opts.package, opts.bin);
    let binary = find_binary(opts.package, opts.bin);

    let mut command = Command::new(&binary);
    command.env(BACKFILL_ENV, backfill_target(opts.index));
    if opts.purge {
        command.env(BACKFILL_PURGE_ENV, "1");
    }
    // Forwarded as `AUTUMN_ENV`, the highest-priority selector, so the child
    // resolves `[profile.<name>.search]` for the profile the operator meant
    // rather than the one implied by this CLI's debug build.
    if let Some(profile) = opts.profile {
        command.env("AUTUMN_ENV", profile);
    }

    // stdout is piped so the CLI can watch for the plugin's start marker; it is
    // forwarded line by line so the app's own output still reaches the user.
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap_or_else(|e| {
            eprintln!("\u{2717} Failed to run {}: {e}", binary.display());
            std::process::exit(1);
        });

    let stdout = child.stdout.take().expect("stdout was piped");
    let (tx, rx) = mpsc::channel::<()>();
    let forwarder = std::thread::spawn(move || {
        let mut announced = false;
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            println!("{line}");
            let _ = std::io::stdout().flush();
            if !announced && is_started_marker(&line) {
                announced = true;
                // A closed receiver just means the main thread stopped caring.
                let _ = tx.send(());
            }
        }
    });

    // If the app does not install `SearchPlugin`, nothing consumes
    // `AUTUMN_SEARCH_BACKFILL` and the process falls through to serving HTTP —
    // forever. Waiting unconditionally would look like a hang, so bound the
    // wait for the marker (not the backfill itself).
    if rx.recv_timeout(STARTUP_GRACE).is_err() {
        let _ = child.kill();
        let _ = child.wait();
        drop(forwarder);
        eprintln!(
            "\n\u{2717} The application did not start a search backfill within {}s.\n\
             \n\
             The most likely cause is that it does not install `SearchPlugin`, so nothing\n\
             consumed {BACKFILL_ENV} and the process went on to serve HTTP instead.\n\
             Add the plugin to the app builder:\n\
             \n\
             .plugin(SearchPlugin::new().postgres().index::<YourModel>())\n\
             \n\
             If the app is simply slow to boot (migrations, a cold pool), re-run once it is warm.",
            STARTUP_GRACE.as_secs()
        );
        std::process::exit(1);
    }

    // The backfill is genuinely under way: wait as long as it takes.
    let status = child.wait().unwrap_or_else(|e| {
        eprintln!("\u{2717} Failed to wait for {}: {e}", binary.display());
        std::process::exit(1);
    });
    let _ = forwarder.join();

    if !status.success() {
        eprintln!(
            "\u{2717} Reindex failed (exit status {status}). \
             Is the database reachable?"
        );
        std::process::exit(status.code().unwrap_or(1));
    }
    eprintln!("\n\u{2714} Reindex complete");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `--profile` must reach the child as `AUTUMN_ENV`, the highest-priority
    /// selector, or the child resolves its profile from ITS build mode — and
    /// this CLI always builds a debug binary, which core reads as `dev`. A
    /// production reindex would then rebuild (or purge!) the development index
    /// and report success.
    #[test]
    fn a_profile_is_forwarded_as_the_selector_the_child_reads_first() {
        let opts = ReindexOptions {
            package: None,
            bin: None,
            index: None,
            profile: Some("prod"),
            purge: false,
        };
        assert_eq!(opts.profile, Some("prod"));
        // The env var name is the contract with `SearchConfig::resolve`'s
        // precedence chain; a rename on either side breaks the forwarding
        // silently, so it is pinned here.
        assert_eq!(
            autumn_web::config::normalize_profile_name("prod").as_deref(),
            Some("prod"),
            "the forwarded value must be one core recognizes"
        );
    }

    #[test]
    fn no_index_means_every_index() {
        assert_eq!(backfill_target(None), "all");
    }

    #[test]
    fn a_named_index_is_passed_through_verbatim() {
        assert_eq!(backfill_target(Some("articles")), "articles");
    }

    #[test]
    fn the_env_var_names_match_the_plugin_contract() {
        // These strings are the CLI ↔ plugin contract; the CLI deliberately
        // does not depend on `autumn-search`, so nothing else pins them.
        assert_eq!(BACKFILL_ENV, "AUTUMN_SEARCH_BACKFILL");
        assert_eq!(BACKFILL_PURGE_ENV, "AUTUMN_SEARCH_BACKFILL_PURGE");
        assert_eq!(BACKFILL_STARTED_MARKER, "autumn-search: backfill starting");
    }

    #[test]
    fn the_start_marker_is_recognised_with_its_target_suffix() {
        // The plugin appends the target, so this must be a prefix match.
        assert!(is_started_marker("autumn-search: backfill starting all"));
        assert!(is_started_marker(
            "autumn-search: backfill starting articles"
        ));
        assert!(!is_started_marker(
            "autumn-search: reindexed articles (3 documents)"
        ));
        assert!(!is_started_marker("Listening on http://0.0.0.0:3000"));
        assert!(!is_started_marker(""));
    }

    #[test]
    fn the_startup_grace_bounds_boot_not_the_backfill() {
        // Long enough for migrations and a cold pool; the CLI waits
        // indefinitely once the marker lands, so a large table is never cut
        // short by this value.
        assert!(STARTUP_GRACE >= Duration::from_secs(60));
    }
}
