//! The Bolt and Ledger profile gates must stay wired into CI.
//!
//! `docs/perf/README.md` promises that a new workload or query fails CI until
//! it has a profile. The promise holds only while ci.yml runs both gates and
//! the Docker sweep builds with `ledger-capture`. A sweep without the
//! feature writes no capture, and the ledger gate then has nothing to check.

use super::pg_relative_delay_ci_coverage::workspace_root;

fn ci() -> String {
    std::fs::read_to_string(workspace_root().join(".github/workflows/ci.yml"))
        .expect("read ci.yml")
        .replace("\r\n", "\n")
}

#[test]
fn bolt_gate_runs_in_ci() {
    assert!(
        ci().contains("run: python3 scripts/check-perf-profiles.py bolt"),
        "ci.yml must run `scripts/check-perf-profiles.py bolt`"
    );
}

#[test]
fn docker_sweep_builds_with_ledger_capture_and_gates_it() {
    let ci = ci();
    let sweep = ci
        .lines()
        .find(|l| {
            l.contains("--test integration_tests -- \\") && l.contains("cargo test -p autumn-web")
        })
        .expect("ci.yml has the integration_tests Docker sweep");
    assert!(
        sweep.contains("ledger-capture"),
        "the Docker sweep must build with `ledger-capture`: {sweep}"
    );
    assert!(
        ci.contains("AUTUMN_LEDGER_CAPTURE_DIR: ${{ runner.temp }}/ledger-capture"),
        "the Docker step must set AUTUMN_LEDGER_CAPTURE_DIR"
    );
    // Most testcontainers run Postgres 11, which cannot `EXPLAIN (GENERIC_PLAN)`.
    // Without a plan server their statements get no plan.
    assert!(
        ci.contains("AUTUMN_LEDGER_PLAN_URL: postgres://postgres@127.0.0.1:55432/postgres")
            && ci.contains("docker run -d --name ledger-plan -p 55432:5432"),
        "the Docker job must start the Ledger plan server and point the capture at it"
    );
    assert!(
        ci.contains(
            "python3 scripts/check-perf-profiles.py ledger \"${RUNNER_TEMP}/ledger-capture\""
        ),
        "ci.yml must run the ledger gate on the sweep's capture"
    );
}

#[test]
fn every_bench_has_a_manifest_entry() {
    // The CI gate uses `cargo metadata`. This is the cheap local mirror: every
    // `autumn/benches/*.rs` file must appear in the manifest.
    let root = workspace_root();
    let manifest = std::fs::read_to_string(root.join("docs/perf/bolt/workloads.toml"))
        .expect("read docs/perf/bolt/workloads.toml");
    for entry in std::fs::read_dir(root.join("autumn/benches")).expect("read autumn/benches") {
        let path = entry.expect("dir entry").path();
        if path.extension().is_some_and(|e| e == "rs") {
            let bench = path
                .file_stem()
                .and_then(|s| s.to_str())
                .expect("utf-8 name");
            assert!(
                manifest.contains(&format!("bench = \"{bench}\"")),
                "autumn/benches/{bench}.rs has no entry in docs/perf/bolt/workloads.toml"
            );
        }
    }
}
