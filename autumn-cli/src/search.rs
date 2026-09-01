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

/// Clear the internal one-shot mode env vars `AppBuilder::run` dispatches
/// *before* it falls through to normal server startup, where `SearchPlugin`
/// consumes [`BACKFILL_ENV`].
///
/// `Command` inherits the parent process's environment by default, so any of
/// these left over in the CLI's own environment (e.g. from a wrapping script,
/// or a previous `autumn migrate`/`autumn task run ...`/`autumn replay ...`/
/// `autumn db retention --purge` invocation in the same shell) would silently
/// hijack `autumn search
/// reindex` into a completely different — and potentially mutating — mode,
/// since every one of them is dispatched earlier than server startup in
/// `AppBuilder::run`.
fn clear_competing_one_shot_env(command: &mut Command) {
    for var in [
        "AUTUMN_BUILD_STATIC",
        "AUTUMN_DUMP_ROUTES",
        "AUTUMN_DUMP_DATA_FLOW",
        "AUTUMN_DUMP_JOBS",
        "AUTUMN_LIST_TASKS",
        "AUTUMN_RUN_TASK",
        "AUTUMN_MIGRATE",
        "AUTUMN_RETENTION_DRY_RUN",
        "AUTUMN_DB_RETENTION",
        "AUTUMN_REPLAY_CAPSULE",
    ] {
        command.env_remove(var);
    }
}

/// Set or clear [`BACKFILL_PURGE_ENV`] on `command` from `purge`.
///
/// `Command` inherits the parent process's environment by default, so
/// without an explicit removal when `purge` is `false`, an
/// `AUTUMN_SEARCH_BACKFILL_PURGE=1` the CLI's own environment already
/// happens to carry (e.g. exported by a wrapping script, or left over from a
/// previous `--purge` invocation) would silently leak into a purge-less
/// reindex, clearing every selected index before rebuilding it — the exact
/// search outage `--purge` being opt-in is meant to guard against.
///
/// A free function (rather than inlined into [`run`]) so this is
/// unit-testable via [`Command::get_envs`] without actually spawning a
/// process.
fn apply_purge_env(command: &mut Command, purge: bool) {
    if purge {
        command.env(BACKFILL_PURGE_ENV, "1");
    } else {
        command.env_remove(BACKFILL_PURGE_ENV);
    }
}

/// Set or clear the profile selector on `command` from `profile`.
///
/// `Command` inherits the parent process's environment by default, so
/// without an explicit removal when `profile` is `None`, an inherited
/// `AUTUMN_ENV`/`AUTUMN_PROFILE` would outrank the debug-build inference
/// this CLI just announced (`AUTUMN_ENV` is the configuration resolver's
/// highest-priority selector), silently reindexing — and, with `--purge`,
/// temporarily emptying — that other profile's index while claiming to
/// operate on `dev`.
///
/// A free function (rather than inlined into [`run`]) so this is
/// unit-testable via [`Command::get_envs`] without actually spawning a
/// process.
fn apply_profile_env(command: &mut Command, profile: Option<&str>) {
    if let Some(profile) = profile {
        command.env("AUTUMN_ENV", profile);
    } else {
        command.env_remove("AUTUMN_ENV");
        command.env_remove("AUTUMN_PROFILE");
    }
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
    clear_competing_one_shot_env(&mut command);
    command.env(BACKFILL_ENV, backfill_target(opts.index));
    apply_purge_env(&mut command, opts.purge);
    apply_profile_env(&mut command, opts.profile);

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

    fn set_envs(command: &mut Command, keys: impl IntoIterator<Item = &'static str>) {
        for key in keys {
            command.env(key, "1");
        }
    }

    /// Vars still explicitly set (present with a value, i.e. not
    /// `env_remove`d) on `command`, restricted to `keys`.
    fn still_set(command: &Command, keys: &[&str]) -> Vec<String> {
        command
            .get_envs()
            .filter_map(|(key, value)| value.map(|_| key))
            .filter_map(|key| key.to_str().map(str::to_owned))
            .filter(|key| keys.contains(&key.as_str()))
            .collect()
    }

    #[test]
    fn clear_competing_one_shot_env_removes_every_var_checked_before_server_startup() {
        // AppBuilder::run dispatches build, route/job dumps, tasks, migration,
        // retention dry-run, and capsule replay *before* falling through to
        // normal server startup, where SearchPlugin consumes
        // AUTUMN_SEARCH_BACKFILL — any of them left over in the CLI's own
        // environment would hijack `autumn search reindex` into a completely
        // different (potentially mutating) mode via Command's default
        // environment inheritance.
        let competing_vars = [
            "AUTUMN_BUILD_STATIC",
            "AUTUMN_DUMP_ROUTES",
            "AUTUMN_DUMP_DATA_FLOW",
            "AUTUMN_DUMP_JOBS",
            "AUTUMN_LIST_TASKS",
            "AUTUMN_RUN_TASK",
            "AUTUMN_MIGRATE",
            "AUTUMN_RETENTION_DRY_RUN",
            "AUTUMN_DB_RETENTION",
            "AUTUMN_REPLAY_CAPSULE",
        ];
        let mut command = Command::new("true");
        set_envs(&mut command, competing_vars);
        clear_competing_one_shot_env(&mut command);
        let remaining = still_set(&command, &competing_vars);
        assert!(
            remaining.is_empty(),
            "these competing one-shot vars survived: {remaining:?}"
        );
    }

    #[test]
    fn an_inherited_framework_retention_purge_cannot_hijack_a_reindex() {
        // AUTUMN_DB_RETENTION=purge is dispatched long before SearchPlugin
        // consumes AUTUMN_SEARCH_BACKFILL at server startup, so an inherited
        // value would turn `autumn search reindex` into a destructive sweep of
        // every framework-owned dataset (#1605).
        let mut command = Command::new("true");
        command.env("AUTUMN_DB_RETENTION", "purge");
        clear_competing_one_shot_env(&mut command);
        assert!(
            still_set(&command, &["AUTUMN_DB_RETENTION"]).is_empty(),
            "an inherited AUTUMN_DB_RETENTION=purge must never survive into a reindex"
        );
    }

    #[test]
    fn apply_purge_env_clears_an_inherited_purge_flag_when_purge_is_not_requested() {
        let mut command = Command::new("true");
        command.env(BACKFILL_PURGE_ENV, "1");
        apply_purge_env(&mut command, false);
        assert!(
            still_set(&command, &[BACKFILL_PURGE_ENV]).is_empty(),
            "an inherited purge flag must not survive a purge-less reindex"
        );
    }

    #[test]
    fn apply_purge_env_sets_the_flag_when_purge_is_requested() {
        let mut command = Command::new("true");
        apply_purge_env(&mut command, true);
        assert_eq!(
            still_set(&command, &[BACKFILL_PURGE_ENV]),
            [BACKFILL_PURGE_ENV]
        );
    }

    #[test]
    fn apply_profile_env_clears_inherited_profile_selectors_for_default_reindex() {
        let mut command = Command::new("true");
        command.env("AUTUMN_ENV", "prod");
        command.env("AUTUMN_PROFILE", "prod");
        apply_profile_env(&mut command, None);
        assert!(
            still_set(&command, &["AUTUMN_ENV", "AUTUMN_PROFILE"]).is_empty(),
            "an inherited profile selector must not survive an unset --profile"
        );
    }

    #[test]
    fn apply_profile_env_forwards_an_explicit_profile_as_autumn_env() {
        let mut command = Command::new("true");
        apply_profile_env(&mut command, Some("prod"));
        assert_eq!(still_set(&command, &["AUTUMN_ENV"]), ["AUTUMN_ENV"]);
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
