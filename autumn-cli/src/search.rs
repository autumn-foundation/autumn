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

use std::process::Command;

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

/// Options for `autumn search reindex`.
pub struct ReindexOptions<'a> {
    /// Package to run (for workspaces).
    pub package: Option<&'a str>,
    /// Binary target to run (for packages with multiple bin targets).
    pub bin: Option<&'a str>,
    /// Index to rebuild. `None` rebuilds every registered index.
    pub index: Option<&'a str>,
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

/// Run `autumn search reindex`.
pub fn run(opts: &ReindexOptions<'_>) {
    eprintln!("\u{1F342} autumn search reindex\n");
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

    let status = command
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()
        .unwrap_or_else(|e| {
            eprintln!("\u{2717} Failed to run {}: {e}", binary.display());
            std::process::exit(1);
        });

    if !status.success() {
        eprintln!(
            "\u{2717} Reindex failed (exit status {status}). \
             Is `SearchPlugin` installed and the database reachable?"
        );
        std::process::exit(status.code().unwrap_or(1));
    }
    eprintln!("\n\u{2714} Reindex complete");
}

#[cfg(test)]
mod tests {
    use super::*;

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
    }
}
