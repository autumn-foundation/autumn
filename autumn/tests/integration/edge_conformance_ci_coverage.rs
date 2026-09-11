//! The edge capsule must build only once in CI (issue #2244, follow-up to #2243).
//!
//! ci.yml's `edge-conformance` job builds the wasm32-wasip1 capsule in its own
//! "Build the edge capsule" step, then `examples/edge-greeting/tests/conformance.rs`
//! builds it again inside the conformance suite. The suite's own build points
//! `CARGO_TARGET_DIR` at `<workspace target dir>/edge-conformance` (see
//! `workspace_target_directory` and `build_capsule` there), a subtree
//! `Swatinem/rust-cache` does not manage. If the CI step's build does not land
//! in that same subtree, cargo treats the suite's build as a second, separate
//! compile: wasted CI time and an uncached, ever-growing target directory.
//!
//! This test pins the CI step's `CARGO_TARGET_DIR` so a future edit cannot
//! silently reintroduce the double build.

use std::path::{Path, PathBuf};

/// The target-dir subtree both the CI step and `build_capsule()` must use.
/// `target/edge-conformance` from the repo root (the CI job's working
/// directory) is the same absolute path `workspace_target_directory(...)
/// .join("edge-conformance")` resolves to at test-run time.
const EDGE_CONFORMANCE_TARGET_DIR: &str = "target/edge-conformance";

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
        .to_path_buf()
}

/// Slices out the `edge-conformance:` job block: from its own job key up to
/// the next top-level (two-space-indented) job key. Keeps the assertion below
/// scoped to this job instead of matching a `CARGO_TARGET_DIR` set elsewhere
/// in the file for an unrelated job.
fn edge_conformance_job_block(ci: &str) -> &str {
    let start = ci
        .find("\n  edge-conformance:\n")
        .expect("ci.yml no longer has an `edge-conformance:` job");
    // Skip past this job's own header line before looking for the next one.
    let body_start = start + "\n  edge-conformance:\n".len();
    let rest = &ci[body_start..];
    let next_job_offset = rest
        .match_indices("\n  ")
        .find(|(at, _)| {
            let line = &rest[at + 1..];
            let line = &line[..line.find('\n').unwrap_or(line.len())];
            // A top-level job key: two-space indent, then a bare `name:` line
            // (not four-space-or-deeper step content).
            !line.starts_with("   ") && line.trim_end().ends_with(':') && !line.trim().is_empty()
        })
        .map(|(at, _)| at + 1);
    match next_job_offset {
        Some(at) => &rest[..at],
        None => rest,
    }
}

/// The "Build the edge capsule" step's own text, up to the next `- name:`
/// step in the same job (or the end of the job block).
fn build_capsule_step_block(job: &str) -> &str {
    let start = job
        .find("- name: Build the edge capsule")
        .expect("edge-conformance job no longer has a \"Build the edge capsule\" step");
    let rest = &job[start..];
    match rest[1..].find("- name:") {
        Some(at) => &rest[..=at],
        None => rest,
    }
}

#[test]
fn build_edge_capsule_step_reuses_the_conformance_suites_target_dir() {
    let root = workspace_root();
    let ci = std::fs::read_to_string(root.join(".github/workflows/ci.yml"))
        .expect("read .github/workflows/ci.yml");

    let job = edge_conformance_job_block(&ci);
    let build_step = build_capsule_step_block(job);

    let expected = format!("CARGO_TARGET_DIR: {EDGE_CONFORMANCE_TARGET_DIR}");
    assert!(
        build_step.contains(&expected),
        "the edge-conformance job's \"Build the edge capsule\" step no longer sets \
         `{expected}` — without it, the CI step's build and the conformance suite's own \
         `build_capsule()` compile the wasm32-wasip1 capsule twice, into two different \
         target-dir subtrees, wasting CI time and leaving one uncached by rust-cache.\n\
         step text was:\n{build_step}"
    );
}

/// Ties the CI-side target-dir suffix to the suite's own, so the two sides
/// cannot drift apart independently. Not full YAML/Rust parsing — just the
/// shared literal both sides key off.
#[test]
fn conformance_suite_uses_the_same_target_dir_suffix() {
    let root = workspace_root();
    let conformance =
        std::fs::read_to_string(root.join("examples/edge-greeting/tests/conformance.rs"))
            .expect("read examples/edge-greeting/tests/conformance.rs");

    assert!(
        conformance.contains("\"edge-conformance\""),
        "examples/edge-greeting/tests/conformance.rs's build_capsule()/\
         workspace_target_directory() no longer join(\"edge-conformance\") — update this test's \
         `EDGE_CONFORMANCE_TARGET_DIR` and ci.yml's `CARGO_TARGET_DIR` to match wherever it \
         builds now"
    );
}
