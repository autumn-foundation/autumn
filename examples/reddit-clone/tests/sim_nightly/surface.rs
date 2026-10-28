//! The DST API surface gate.
//!
//! This module reads the public `fn` items and the `pub trait` methods of
//! `autumn_web::sim` from the source. Each item must have a call in `tests/sim_nightly/`, or an entry in
//! [`EXCLUDED`] with a reason. A new public sim API item that the nightly does
//! not call makes the gate fail.
//!
//! The check is textual: the call text (`name(`, or the needle in
//! [`NEEDLES`]) must occur in a scenario source file. Thus it shows that the
//! nightly calls each item. It does not show what the call asserts.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// The sim source files that make the public surface. The other sim files
/// are excluded as complete files:
/// - `gate.rs`, `shuffle.rs`: private plumbing.
/// - `substrate.rs`, `dblink.rs`, `fleet.rs`: they need the `sqlite`
///   feature. reddit-clone cannot enable it, because `backend_select!` then
///   compiles the whole app for `SQLite`. The `sim-sweep` CI job covers them.
/// - `scenario.rs`: the hidden toy scenario of `sim-sweep`.
const SURFACE_FILES: &[&str] = &[
    "sim.rs",
    "sim/assert.rs",
    "sim/chaos.rs",
    "sim/crash.rs",
    "sim/fault.rs",
    "sim/llm.rs",
    "sim/net.rs",
    "sim/op.rs",
    "sim/replica.rs",
    "sim/sweep.rs",
    "sim/trace.rs",
];

/// Public items outside the files above that a sim uses. Each must have a
/// call too.
const EXTRA_ITEMS: &[(&str, &str)] = &[
    ("sim::runtime", "runtime("),
    ("#[sim_test]", "#[sim_test]"),
    ("always!", "always!("),
    ("sometimes!", "sometimes!("),
    ("time::ambient_now", "ambient_now("),
    ("time::ambient_monotonic", "ambient_monotonic("),
    ("time::ambient_instant", "ambient_instant("),
    ("time::ambient_system_time", "ambient_system_time("),
    ("time::clock_unix_secs", "clock_unix_secs("),
    ("time::clock_unix_duration", "clock_unix_duration("),
    ("backoff::ceiling_ms", "ceiling_ms("),
    ("backoff::full_jitter_ms", "full_jitter_ms("),
    ("backoff::full_jitter", "full_jitter("),
    ("backoff::retry_after_wait", "retry_after_wait("),
    ("entropy::SeededEntropy::new", "SeededEntropy::new("),
    ("TestApp::with_entropy", ".with_entropy("),
    ("TestApp::with_fault_plan", ".with_fault_plan("),
    ("TestApp::record_broadcasts", ".record_broadcasts("),
    ("TestClient::fault_outcome", ".fault_outcome("),
    ("TestClient::fault_ledger", ".fault_ledger("),
];

/// Items with a common name get a stricter needle, so that an unrelated call
/// does not satisfy the gate. Key: `file:name`.
const NEEDLES: &[(&str, &str)] = &[
    ("sim/assert.rs:new", "SometimesRegistry::new("),
    ("sim/llm.rs:new", "LlmRequest::new("),
    ("sim/net.rs:new", "SimNet::new("),
    ("sim/llm.rs:build", ".latency_up_to("),
    ("sim/fault.rs:from_seed", "FaultPlan::from_seed("),
    ("sim/fault.rs:seed", "plan.seed()"),
    ("sim.rs:seed", "sim.seed()"),
    ("sim.rs:from_seed", "Sim::from_seed("),
    ("sim/fault.rs:outcome", ".outcome()"),
    ("sim/net.rs:events", "net.events()"),
    ("sim/replica.rs:name", ".name()"),
    ("sim/trace.rs:len", "trace.len()"),
    ("sim/trace.rs:is_empty", "trace.is_empty()"),
    ("sim/trace.rs:lines", ".lines()"),
    ("sim/trace.rs:diff", ".diff("),
];

/// Items the nightly does not call, with the reason. Key: `file:name`.
pub const EXCLUDED: &[(&str, &str)] = &[(
    "sim.rs:db_link",
    "needs the `sqlite` feature; the `sim-sweep` fleet scenarios cover it",
)];

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The public `fn` names of one source file, as `file:name`: each `pub fn`,
/// and each method of a `pub trait`. The scan stops at the first
/// `#[cfg(test)]` line. It skips `__` names and items after a
/// `#[doc(hidden)]` attribute.
fn public_fns(file: &str, source: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut hidden = false;
    // The brace depth inside a `pub trait` block, if the scan is in one.
    let mut trait_depth: Option<usize> = None;
    for line in source.lines() {
        let trimmed = line.trim_start();
        if line.starts_with("#[cfg(test)]") {
            break;
        }
        if let Some(depth) = trait_depth {
            if depth == 1
                && let Some(name) = trimmed.strip_prefix("fn ").map(ident)
                && !name.starts_with("__")
            {
                found.push(format!("{file}:{name}"));
            }
            let depth =
                (depth + trimmed.matches('{').count()).saturating_sub(trimmed.matches('}').count());
            trait_depth = (depth > 0).then_some(depth);
            continue;
        }
        if trimmed.starts_with("pub trait ") && !hidden {
            trait_depth = Some(trimmed.matches('{').count()).filter(|d| *d > 0);
            continue;
        }
        if trimmed.starts_with("#[doc(hidden)]") {
            hidden = true;
            continue;
        }
        if trimmed.starts_with("///") || trimmed.starts_with("#[") {
            continue;
        }
        if let Some(name) = fn_name(trimmed)
            && !name.starts_with("__")
            && !hidden
        {
            found.push(format!("{file}:{name}"));
        }
        hidden = false;
    }
    found
}

/// The name of a `pub fn`, `pub const fn` or `pub async fn` declaration.
fn fn_name(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("pub ")?;
    let rest = rest.strip_prefix("const ").unwrap_or(rest);
    let rest = rest.strip_prefix("async ").unwrap_or(rest);
    Some(ident(rest.strip_prefix("fn ")?))
}

/// The identifier at the start of `text`.
fn ident(text: &str) -> &str {
    let end = text
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(text.len());
    &text[..end]
}

/// Every surface item, as `(key, needle)`.
fn surface() -> Vec<(String, String)> {
    let mut items = BTreeSet::new();
    for file in SURFACE_FILES {
        let path = workspace_root().join("autumn/src").join(file);
        let source = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
        items.extend(public_fns(file, &source));
    }
    let mut surface: Vec<(String, String)> = items
        .into_iter()
        .map(|key| {
            let needle = NEEDLES.iter().find(|(k, _)| *k == key).map_or_else(
                || format!("{}(", key.rsplit(':').next().unwrap_or_default()),
                |(_, needle)| (*needle).to_owned(),
            );
            (key, needle)
        })
        .collect();
    surface.extend(
        EXTRA_ITEMS
            .iter()
            .map(|(key, needle)| ((*key).to_owned(), (*needle).to_owned())),
    );
    surface
}

/// The scenario sources the needles must occur in. This file is not one of
/// them, because it names every needle.
fn scenario_sources() -> String {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let mut sources =
        std::fs::read_to_string(dir.join("sim_nightly.rs")).expect("read sim_nightly.rs");
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir.join("sim_nightly"))
        .expect("read tests/sim_nightly")
        .map(|entry| entry.expect("dir entry").path())
        .filter(|path| path.file_name().is_some_and(|name| name != "surface.rs"))
        .collect();
    files.sort();
    for file in files {
        sources.push_str(&std::fs::read_to_string(&file).expect("read scenario source"));
    }
    sources
}

/// The surface items with no call and no exclusion.
pub fn missing_items() -> Vec<String> {
    let sources = scenario_sources();
    surface()
        .into_iter()
        .filter(|(key, _)| !EXCLUDED.iter().any(|(excluded, _)| excluded == key))
        .filter(|(_, needle)| !sources.contains(needle.as_str()))
        .map(|(key, needle)| format!("{key} (needle `{needle}`)"))
        .collect()
}

/// The exclusions that name no current surface item.
pub fn stale_exclusions() -> Vec<&'static str> {
    let keys: BTreeSet<String> = surface().into_iter().map(|(key, _)| key).collect();
    EXCLUDED
        .iter()
        .map(|(key, _)| *key)
        .filter(|key| !keys.contains(*key))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fn_name_reads_each_declaration_form() {
        assert_eq!(fn_name("pub fn build(&mut self)"), Some("build"));
        assert_eq!(fn_name("pub const fn seed(&self)"), Some("seed"));
        assert_eq!(fn_name("pub async fn advance(&self)"), Some("advance"));
        assert_eq!(fn_name("pub(crate) fn hidden()"), None);
        assert_eq!(fn_name("fn private()"), None);
    }

    #[test]
    fn public_fns_reads_the_methods_of_a_public_trait() {
        let source = "pub trait Client: Send {\n    /// Doc.\n    fn complete<'a>(\n        &'a self,\n    ) -> u8;\n}\n\
                      trait Private {\n    fn hidden(&self);\n}\nfn private() {}\n";
        assert_eq!(public_fns("x.rs", source), vec!["x.rs:complete".to_owned()]);
    }

    #[test]
    fn the_surface_has_the_llm_client_trait_method() {
        assert!(
            surface()
                .iter()
                .any(|(key, _)| key == "sim/llm.rs:complete")
        );
    }

    #[test]
    fn public_fns_skips_hidden_and_test_items() {
        let source = "pub fn a() {}\n#[doc(hidden)]\npub fn b() {}\npub fn __c() {}\n\
                      #[cfg(test)]\nmod tests { pub fn d() {} }\n";
        assert_eq!(public_fns("x.rs", source), vec!["x.rs:a".to_owned()]);
    }
}
