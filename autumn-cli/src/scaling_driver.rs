//! Live warm-incremental scaling measurement driver for
//! `autumn dev-loop-bench --scaling` (issue #983).
//!
//! This module scaffolds a synthetic Autumn app at each requested size N,
//! performs a warm build (untimed), then runs `--runs` timed incremental
//! rebuilds after a single-file edit (`src/handlers.rs` `BENCH_EDIT_SENTINEL`
//! bump), and feeds the timing samples to the pure report logic in
//! [`crate::dev_loop_scaling`].
//!
//! All subprocess / filesystem / timing I/O is here; the pure budget and
//! report logic is unit-tested in `dev_loop_scaling.rs`. This file is
//! excluded from coverage like `cold_start_driver.rs`.

use std::path::Path;
use std::process::Command;
use std::time::Instant;

use crate::cold_start_driver::{cached_autumn_web, repoint_autumn_web};
use crate::dev_loop_scaling::{
    GeneratedApp, apply_handler_edit, build_scaling_report, emit_scaling_report,
    generate_synthetic_app, load_baseline, parse_sizes, run_scaling_dry_run,
};

// ── App scaffold ─────────────────────────────────────────────────────────────

/// Write a `GeneratedApp` to `project_dir` and append a
/// `[patch.crates-io] autumn-web` section pointing at local source.
///
/// The `[patch]` append reuses `cold_start_driver::repoint_autumn_web` so both
/// benchmarks generate byte-identical patch sections.
fn scaffold_app(app: &GeneratedApp, project_dir: &Path, autumn_web: &Path) -> Result<(), String> {
    // Write the generated manifest, then repoint autumn-web at local source.
    std::fs::write(project_dir.join("Cargo.toml"), &app.cargo_toml)
        .map_err(|e| format!("write Cargo.toml: {e}"))?;
    repoint_autumn_web(project_dir, autumn_web)?;

    // Write source files
    for (relpath, content) in &app.files {
        let dest = project_dir.join(relpath);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
        }
        std::fs::write(&dest, content).map_err(|e| format!("write {}: {e}", dest.display()))?;
    }

    Ok(())
}

// ── Build helpers ────────────────────────────────────────────────────────────

/// Run `cargo build` in `project_dir`, capturing stderr for diagnostics.
///
/// Pass `timed = true` to wrap the call in an `Instant` and return elapsed ms;
/// `timed = false` returns `0` (warm-up build that is not measured).
fn run_cargo_build(project_dir: &Path, timed: bool) -> Result<u64, String> {
    let target_dir = project_dir.join("target");
    let mut cmd = Command::new("cargo");
    cmd.args(["build", "--target-dir"])
        .arg(&target_dir)
        .current_dir(project_dir)
        // Remove inherited target-dir overrides so the warm cache for THIS
        // project accumulates in its own dedicated directory.
        .env_remove("CARGO_TARGET_DIR")
        .env_remove("CARGO_BUILD_TARGET_DIR")
        .env_remove("CARGO_BUILD_TARGET");

    // Capture output rather than inheriting it: keeps the benchmark's own
    // stderr clean on success, but preserves the compiler's diagnostics so a
    // build failure mid-sweep is actually debuggable.
    let start = Instant::now();
    let output = cmd
        .output()
        .map_err(|e| format!("cargo build spawn failed: {e}"))?;
    let elapsed = if timed {
        u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
    } else {
        0
    };

    if !output.status.success() {
        return Err(format!(
            "cargo build failed (exit: {}):\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(elapsed)
}

// ── Per-size measurement ─────────────────────────────────────────────────────

/// Measure warm incremental rebuild latency for a synthetic app of size `n`.
///
/// 1. Scaffolds the app in a fresh tempdir.
/// 2. Runs one **warm build** (untimed) to populate the incremental cache.
/// 3. Runs `runs` **timed builds**, each preceded by bumping the
///    `BENCH_EDIT_SENTINEL` in `src/handlers.rs` to force recompilation of
///    exactly that one file.
///
/// Returns a `Vec<u64>` of `runs` timing samples in milliseconds.
fn measure_size(n: usize, runs: u32, autumn_web: &Path) -> Result<Vec<u64>, String> {
    let tmp = tempfile::tempdir().map_err(|e| format!("create temp dir: {e}"))?;
    let project_dir = tmp.path().to_path_buf();

    let app = generate_synthetic_app(n);
    scaffold_app(&app, &project_dir, autumn_web)?;

    eprintln!("  N={n}: warm build (untimed)…");
    run_cargo_build(&project_dir, false)?;

    let handlers_path = project_dir.join("src/handlers.rs");
    let mut samples = Vec::with_capacity(runs as usize);

    for run in 1..=runs {
        // Bump the sentinel to trigger a one-file incremental recompile.
        let handlers_src = std::fs::read_to_string(&handlers_path)
            .map_err(|e| format!("read handlers.rs: {e}"))?;
        let edited = apply_handler_edit(&handlers_src, run);
        std::fs::write(&handlers_path, &edited).map_err(|e| format!("write handlers.rs: {e}"))?;

        eprintln!("  N={n} run {run}/{runs}: timed incremental build…");
        let ms = run_cargo_build(&project_dir, true)?;
        eprintln!("    → {ms} ms");
        samples.push(ms);
    }

    Ok(samples)
}

// ── Sweep ────────────────────────────────────────────────────────────────────

/// Run the full scaling sweep: for each size in `sizes`, measure `runs`
/// incremental rebuild samples and return `(n, samples)` pairs.
///
/// A single size that fails to build (transient OOM, flaky proc-macro, disk
/// pressure) is logged and skipped rather than aborting the whole sweep, so the
/// other sizes' samples are still reported. Returns `Err` only if **every** size
/// failed, leaving no data to build a report from.
fn measure_scaling(
    sizes: &[usize],
    runs: u32,
    autumn_web: &Path,
) -> Result<Vec<(usize, Vec<u64>)>, String> {
    let mut results = Vec::with_capacity(sizes.len());
    let mut failures = Vec::new();
    for &n in sizes {
        eprintln!("Measuring N={n}…");
        match measure_size(n, runs, autumn_web) {
            Ok(samples) => results.push((n, samples)),
            Err(e) => {
                eprintln!("  N={n}: measurement failed, skipping this size: {e}");
                failures.push(n);
            }
        }
    }
    if results.is_empty() {
        return Err(format!(
            "every size failed to measure ({} attempted: {failures:?})",
            sizes.len()
        ));
    }
    if !failures.is_empty() {
        eprintln!(
            "Warning: {} size(s) failed and were skipped: {failures:?}",
            failures.len()
        );
    }
    Ok(results)
}

// ── CLI entry point ──────────────────────────────────────────────────────────

/// Run the `autumn dev-loop-bench --scaling` command.
///
/// In `--dry-run` mode prints the budget table without building anything.
/// Otherwise locates the workspace `autumn-web`, runs the sizing sweep, and
/// emits a scaling report.
#[allow(clippy::fn_params_excessive_bools)]
pub fn run_scaling(
    sizes_csv: &str,
    runs: u32,
    output: Option<&str>,
    json: bool,
    fail_on_regression: bool,
    dry_run: bool,
    baseline_path: Option<&str>,
) -> i32 {
    if dry_run {
        return run_scaling_dry_run();
    }

    if runs == 0 {
        eprintln!("Error: --runs must be at least 1 for a scaling measurement.");
        return 1;
    }

    let sizes = match parse_sizes(sizes_csv) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Error: invalid --sizes: {e}");
            return 1;
        }
    };

    let Some(autumn_web) = cached_autumn_web() else {
        eprintln!(
            "Error: could not locate the workspace autumn-web crate; \
             set AUTUMN_BENCH_AUTUMN_WEB_PATH to its directory."
        );
        return 1;
    };

    eprintln!(
        "autumn dev-loop-bench --scaling: warming and measuring {} size(s), {} run(s) each",
        sizes.len(),
        runs
    );
    eprintln!(
        "This scaffolds a synthetic Autumn app for each N and runs timed incremental builds — \
         expect several minutes total.\n"
    );

    let measurements = match measure_scaling(&sizes, runs, &autumn_web) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("Error: scaling measurement failed: {e}");
            return 1;
        }
    };

    let baseline = match load_baseline(baseline_path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("Error: {e}");
            return 1;
        }
    };
    let report = build_scaling_report(&measurements, baseline.as_ref());
    emit_scaling_report(&report, json, output, fail_on_regression)
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // `locate_autumn_web` / `cached_autumn_web` are owned and tested by
    // `cold_start_driver` (see its `locate_autumn_web_honours_env_override`);
    // this module reuses them rather than duplicating the logic or its test.

    #[test]
    fn scaffold_app_writes_cargo_toml_and_sources() {
        use crate::dev_loop_scaling::generate_synthetic_app;
        let tmp = tempfile::tempdir().expect("tempdir");
        let autumn_web = tmp.path().join("autumn-web");
        std::fs::create_dir_all(&autumn_web).expect("mkdir");

        let app = generate_synthetic_app(2);
        scaffold_app(&app, tmp.path(), &autumn_web).expect("scaffold must succeed");

        assert!(
            tmp.path().join("Cargo.toml").is_file(),
            "Cargo.toml must exist"
        );
        let manifest =
            std::fs::read_to_string(tmp.path().join("Cargo.toml")).expect("read manifest");
        assert!(
            manifest.contains("[patch.crates-io]"),
            "patch section must be appended"
        );

        for (relpath, _) in &app.files {
            assert!(
                tmp.path().join(relpath).is_file(),
                "{relpath} must be written"
            );
        }
    }

    #[test]
    fn run_scaling_dry_run_returns_zero() {
        assert_eq!(
            run_scaling("1,25,50,100", 3, None, false, false, true, None),
            0
        );
    }

    #[test]
    fn run_scaling_rejects_zero_runs() {
        let exit = run_scaling("1,100", 0, None, false, false, false, None);
        assert_eq!(exit, 1, "zero runs must be rejected");
    }

    #[test]
    fn run_scaling_rejects_invalid_sizes() {
        let exit = run_scaling("abc", 1, None, false, false, false, None);
        assert_eq!(exit, 1, "invalid sizes must be rejected");
    }

    // Live test — compiles a real n=1 synthetic app; gated behind --ignored.
    #[test]
    #[ignore = "compiles a synthetic app from scratch; run with --ignored"]
    fn scaling_live_n1_measures_positive_duration() {
        let autumn_web = cached_autumn_web().expect("must locate autumn-web");
        let samples = measure_size(1, 1, &autumn_web).expect("must succeed");
        assert_eq!(samples.len(), 1);
        assert!(samples[0] > 0, "measured duration must be positive");
    }
}
