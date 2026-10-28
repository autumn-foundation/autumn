//! The web-standard JavaScript host, shipped as source (ADR-0024).
//!
//! [`HOST`] is `js/autumn-edge-host.mjs`: one dependency-free ES module that
//! runs a capsule on any runtime with the `WinterTC` Minimum Common API — a
//! Cloudflare Worker, Deno, Bun, a Vercel or Netlify edge function, Node 18+.
//! It is this crate's reference host ([`crate::host`]) and gateway
//! ([`crate::gateway`]) over `WebAssembly` and the Fetch API, and it names no
//! vendor SDK, so no Autumn release waits on a vendor's release.
//!
//! `autumn edge bundle` writes it next to the capsule. The edge-greeting
//! conformance suite runs it and the reference host over one corpus and
//! requires the same outcome for every request.

/// File name the bundle writes [`HOST`] under.
pub const HOST_FILE_NAME: &str = "autumn-edge-host.mjs";

/// The host module's source.
pub const HOST: &str = include_str!("../js/autumn-edge-host.mjs");

/// Every WASI import [`HOST`] provides, as `module::name` — the same set the
/// reference host links, so a capsule that runs under one runs under the
/// other. A capsule importing anything else fails to link.
pub const HOST_WASI_IMPORTS: &[&str] = &[
    "wasi_snapshot_preview1::fd_read",
    "wasi_snapshot_preview1::fd_write",
    "wasi_snapshot_preview1::random_get",
    "wasi_snapshot_preview1::clock_time_get",
    "wasi_snapshot_preview1::environ_get",
    "wasi_snapshot_preview1::environ_sizes_get",
    "wasi_snapshot_preview1::args_get",
    "wasi_snapshot_preview1::args_sizes_get",
    "wasi_snapshot_preview1::proc_exit",
    "wasi_snapshot_preview1::sched_yield",
    "wasi_snapshot_preview1::fd_close",
    "wasi_snapshot_preview1::fd_seek",
    "wasi_snapshot_preview1::fd_fdstat_get",
    "wasi_snapshot_preview1::fd_prestat_get",
    "wasi_snapshot_preview1::fd_prestat_dir_name",
];

/// The imports of a capsule that [`HOST`] does not provide, in order.
#[must_use]
pub fn unprovided_imports<'a>(imports: impl IntoIterator<Item = &'a str>) -> Vec<&'a str> {
    imports
        .into_iter()
        .filter(|import| !HOST_WASI_IMPORTS.contains(import))
        .collect()
}

/// The conformance driver: runs a capsule through [`HOST`] for each case of
/// a job file (see its header comment) and prints one outcome per line.
pub const CONFORMANCE_DRIVER: &str = include_str!("../js/conformance-driver.mjs");

#[cfg(test)]
mod tests {
    //! The JS host restates constants this crate owns. These tests read them
    //! back out of the source, so a change on one side fails here instead of
    //! as a divergence in production.

    use super::HOST;
    use crate::conformance::SECURITY_HEADERS;
    use crate::wire::{FALLTHROUGH_SENTINEL, SENSITIVE_HEADERS, WIRE_VERSION};

    /// The quoted strings of `export const NAME = Object.freeze([ ... ]);`.
    fn js_list(name: &str) -> Vec<String> {
        let start = format!("export const {name} = Object.freeze([");
        let body = HOST
            .split_once(&start)
            .and_then(|(_, rest)| rest.split_once("]);"))
            .map_or_else(
                || panic!("`{name}` is not an Object.freeze list in the JS host"),
                |(body, _)| body,
            );
        body.split(',')
            .map(str::trim)
            .filter(|item| !item.is_empty())
            .map(|item| {
                if item == "FALLTHROUGH_SENTINEL" {
                    FALLTHROUGH_SENTINEL.to_owned()
                } else {
                    item.trim_matches('"').to_owned()
                }
            })
            .collect()
    }

    fn js_number(name: &str) -> u64 {
        let start = format!("export const {name} = ");
        let expression = HOST
            .split_once(&start)
            .and_then(|(_, rest)| rest.split_once(';'))
            .map_or_else(
                || panic!("`{name}` is not a numeric constant in the JS host"),
                |(expression, _)| expression,
            );
        expression
            .split('*')
            .map(|factor| {
                factor
                    .trim()
                    .parse::<u64>()
                    .unwrap_or_else(|_| panic!("`{name}` = `{expression}` is not a product"))
            })
            .product()
    }

    /// The keys of the object `wasiImports` returns, in source order.
    ///
    /// Line endings are normalized first: a Windows checkout with
    /// `core.autocrlf` embeds the file with `\r\n`.
    fn js_wasi_imports(source: &str) -> Vec<String> {
        let source = source.replace('\r', "");
        let body = source
            .split_once("function wasiImports(")
            .and_then(|(_, rest)| rest.split_once("\n  return {\n"))
            .and_then(|(_, rest)| rest.split_once("\n  };\n"))
            .map(|(body, _)| body)
            .expect("wasiImports returns one object literal");
        body.lines()
            .filter(|line| line.starts_with("    ") && !line.starts_with("     "))
            .filter_map(|line| {
                let name: String = line
                    .trim_start()
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                    .collect();
                (!name.is_empty() && !line.trim_start().starts_with("//"))
                    .then(|| format!("wasi_snapshot_preview1::{name}"))
            })
            .collect()
    }

    #[test]
    fn the_js_host_provides_exactly_the_listed_imports() {
        let mut listed: Vec<String> = super::HOST_WASI_IMPORTS
            .iter()
            .map(|name| (*name).to_owned())
            .collect();
        listed.sort();
        // The source with LF and with CRLF endings, whichever the checkout
        // embedded.
        let lf = HOST.replace("\r\n", "\n");
        for source in [lf.clone(), lf.replace('\n', "\r\n")] {
            let mut provided = js_wasi_imports(&source);
            provided.sort();
            assert_eq!(provided, listed);
        }
    }

    #[test]
    fn an_unprovided_import_is_reported() {
        assert_eq!(
            super::unprovided_imports([
                "wasi_snapshot_preview1::fd_write",
                "wasi_snapshot_preview1::path_open",
                "env::host_fetch",
            ]),
            ["wasi_snapshot_preview1::path_open", "env::host_fetch"]
        );
    }

    #[test]
    fn the_wire_version_matches() {
        assert_eq!(js_number("WIRE_VERSION"), u64::from(WIRE_VERSION));
        assert!(HOST.contains(&format!(
            "export const FALLTHROUGH_SENTINEL = \"{FALLTHROUGH_SENTINEL}\";"
        )));
    }

    #[test]
    fn the_header_lists_match() {
        assert_eq!(js_list("SENSITIVE_HEADERS"), SENSITIVE_HEADERS);
        assert_eq!(js_list("SECURITY_HEADERS"), SECURITY_HEADERS);
        assert_eq!(
            js_list("FALLTHROUGH_REASONS"),
            [
                "unknown_route",
                "method_not_edge_eligible",
                "missing_capability",
                "capsule_error"
            ]
        );
    }

    #[cfg(feature = "host")]
    #[test]
    fn the_hop_by_hop_list_and_budgets_match() {
        use crate::host::{MEMORY_BUDGET_BYTES, STDERR_BUDGET_BYTES, STDOUT_LINE_BUDGET_BYTES};

        assert_eq!(js_list("HOP_BY_HOP"), crate::gateway::HOP_BY_HOP);
        let budget = |value: usize| u64::try_from(value).unwrap_or(u64::MAX);
        assert_eq!(
            js_number("MEMORY_BUDGET_BYTES"),
            budget(MEMORY_BUDGET_BYTES)
        );
        assert_eq!(
            js_number("STDOUT_LINE_BUDGET_BYTES"),
            budget(STDOUT_LINE_BUDGET_BYTES)
        );
        assert_eq!(
            js_number("STDERR_BUDGET_BYTES"),
            budget(STDERR_BUDGET_BYTES)
        );
    }
}
