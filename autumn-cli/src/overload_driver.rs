//! Live overload / load-shedding measurement driver for `autumn dev-loop-bench
//! --overload` (issue #1006).
//!
//! This module is the **orchestration** half of the overload benchmark: it
//! scaffolds a throwaway app exposing a `/block` handler that sleeps
//! `block_ms`, boots it with `server.max_concurrent_requests` set to
//! `ceiling`, fires concurrent HTTP load against it, and samples the child
//! process's RSS. All of that is subprocess / TCP / filesystem I/O that
//! cannot be exercised in unit tests, so this file is excluded from coverage
//! (see `codecov.yml`), mirroring `cold_start_driver.rs` and
//! `scaling_driver.rs`.
//!
//! The **pure**, unit-tested half — the budget, stats, and report logic for
//! issue #1006's Success Metric — lives in [`crate::dev_loop_bench`]; this
//! driver measures the raw samples and calls into it.
//!
//! ## Methodology
//!
//! 1. Scaffold a minimal app (a single `/block` handler, no database) and
//!    compile it against the workspace's local `autumn-web` source.
//! 2. Boot it with `AUTUMN_SERVER__MAX_CONCURRENT_REQUESTS=<ceiling>` and wait
//!    for `/live` to report ready.
//! 3. **Baseline**: fire `ceiling` concurrent requests (offered load == the
//!    ceiling, no shedding expected) and record admitted-request latency.
//! 4. **Overload**: fire `ceiling * load_multiplier` concurrent requests
//!    simultaneously, classify each response as admitted (2xx) or shed (503),
//!    and sample the child's RSS every 30ms for the duration.
//! 5. Repeat steps 3-4 `--runs` times against the same running server,
//!    accumulating samples, then hand everything to
//!    [`crate::dev_loop_bench::build_overload_report`].

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier, Mutex};
use std::time::{Duration, Instant};

use crate::cold_start_driver::{
    cached_autumn_web, repoint_autumn_web, reserve_free_port, stop_child,
};
use crate::dev_loop_bench::{
    OverloadStats, build_overload_report, cargo_executable_path, emit_overload_report,
    format_overload_budget_table,
};
use crate::dev_loop_scaling::GeneratedApp;

/// Generate a minimal throwaway app with a single `/block` handler that
/// sleeps `block_ms` before responding. No database, no extra routes — the
/// framework's built-in `/live` probe is used for readiness polling.
fn generate_overload_app(block_ms: u64) -> GeneratedApp {
    // `autumn-web = "*"`: the driver always appends a `[patch.crates-io]`
    // pointing this dependency at the workspace's local source (see
    // `scaffold_app`), so the wildcard here just needs to satisfy Cargo's
    // resolver before the patch redirects it — same convention as
    // `dev_loop_scaling::generate_synthetic_app`.
    let cargo_toml = "[package]\n\
         name = \"overload_bench_app\"\n\
         version = \"0.1.0\"\n\
         edition = \"2024\"\n\
         publish = false\n\
         \n\
         [workspace]\n\
         \n\
         [dependencies]\n\
         autumn-web = \"*\"\n"
        .to_string();

    let main_rs = format!(
        "use autumn_web::prelude::*;\n\n\
         #[get(\"/block\")]\n\
         async fn block() -> &'static str {{\n\
         \tautumn_web::reexports::tokio::time::sleep(std::time::Duration::from_millis({block_ms})).await;\n\
         \t\"done\"\n\
         }}\n\n\
         #[autumn_web::main]\n\
         async fn main() {{\n\
         \tautumn_web::app()\n\
         \t\t.routes(routes![block])\n\
         \t\t.run()\n\
         \t\t.await;\n\
         }}\n"
    );

    GeneratedApp {
        cargo_toml,
        files: vec![("src/main.rs".to_owned(), main_rs)],
    }
}

/// Write a `GeneratedApp` to `project_dir` and repoint `autumn-web` at local
/// source, mirroring `scaling_driver::scaffold_app` (byte-identical patch
/// section, since both reuse `cold_start_driver::repoint_autumn_web`).
fn scaffold_app(app: &GeneratedApp, project_dir: &Path, autumn_web: &Path) -> Result<(), String> {
    std::fs::write(project_dir.join("Cargo.toml"), &app.cargo_toml)
        .map_err(|e| format!("write Cargo.toml: {e}"))?;
    repoint_autumn_web(project_dir, autumn_web)?;

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

/// Warm `cargo build` (the compile itself is not timed — only the subsequent
/// HTTP load is measured) and return the built binary's path.
fn build_app(project_dir: &Path, bin_name: &str) -> Result<PathBuf, String> {
    let target_dir = project_dir.join("target");
    let output = Command::new("cargo")
        .args(["build", "--message-format=json", "--target-dir"])
        .arg(&target_dir)
        .current_dir(project_dir)
        .env_remove("CARGO_TARGET_DIR")
        .env_remove("CARGO_BUILD_TARGET_DIR")
        .output()
        .map_err(|e| format!("cargo build spawn failed: {e}"))?;

    if !output.status.success() {
        return Err(format!(
            "cargo build failed for the scaffolded overload app:\n{}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    cargo_executable_path(&output.stdout, bin_name).ok_or_else(|| {
        "could not determine the built binary path from cargo's JSON output".to_owned()
    })
}

/// Outcome of one HTTP request fired during load generation.
#[derive(Debug)]
struct RequestOutcome {
    /// `true` for any 2xx response, `false` for a `503` (or a transport
    /// error, treated conservatively as "not admitted").
    admitted: bool,
    elapsed_ms: u64,
}

/// Fire `count` requests to `url` as close to simultaneously as possible
/// (synchronized with a [`Barrier`]) and return each one's outcome.
///
/// Uses one OS thread per request rather than an async client: the CLI binary
/// has no long-lived tokio runtime of its own (matching `cold_start_driver`'s
/// use of `reqwest::blocking::Client` elsewhere), and at the request counts
/// this benchmark operates at (tens to low hundreds), thread-per-request
/// concurrency is simple and accurate enough to saturate the ceiling.
///
/// Takes a shared, already-built `client` (cloning a `reqwest::blocking::
/// Client` is cheap — it's an `Arc` handle to the connection pool) so
/// repeated calls across the baseline and overload phases, and across
/// `--runs` cycles, reuse warm keep-alive connections instead of each paying
/// fresh TCP-handshake/thread-scheduling overhead. Without this, that
/// overhead reads as inflated (client-observed) latency unrelated to the
/// server's actual admission/rejection speed — see the module-level caveat
/// in `docs/guide/dev-loop-latency.md`.
fn fire_concurrent(
    client: &reqwest::blocking::Client,
    url: &str,
    count: usize,
) -> Vec<RequestOutcome> {
    let barrier = Arc::new(Barrier::new(count));
    let results = Arc::new(Mutex::new(Vec::with_capacity(count)));

    let handles: Vec<_> = (0..count)
        .map(|_| {
            let client = client.clone();
            let url = url.to_owned();
            let barrier = Arc::clone(&barrier);
            let results = Arc::clone(&results);
            std::thread::spawn(move || {
                barrier.wait();
                let start = Instant::now();
                let resp = client.get(&url).send();
                let elapsed_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
                let admitted = matches!(&resp, Ok(r) if r.status().is_success());
                results.lock().unwrap().push(RequestOutcome {
                    admitted,
                    elapsed_ms,
                });
            })
        })
        .collect();

    for handle in handles {
        let _ = handle.join();
    }

    Arc::try_unwrap(results)
        .expect("all request threads have joined")
        .into_inner()
        .expect("results mutex should not be poisoned")
}

/// Sample a process's resident set size in KB, or `None` if unsupported on
/// this platform. Only Linux's `/proc/<pid>/status` is implemented; the
/// RSS-bounded check treats an empty sample set as "skipped", not "failed"
/// (see `check_overload_budget`), so this degrades gracefully elsewhere.
#[cfg(target_os = "linux")]
fn sample_rss_kb(pid: u32) -> Option<u64> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    status.lines().find_map(|line| {
        line.strip_prefix("VmRSS:")
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|kb| kb.parse::<u64>().ok())
    })
}

#[cfg(not(target_os = "linux"))]
fn sample_rss_kb(_pid: u32) -> Option<u64> {
    None
}

/// Poll `/live` until it reports ready or the deadline elapses.
fn wait_until_ready(
    client: &reqwest::blocking::Client,
    child: &mut std::process::Child,
    port: u16,
    deadline: Instant,
) -> Result<(), String> {
    let url = format!("http://127.0.0.1:{port}/live");
    while Instant::now() < deadline {
        if let Ok(Some(status)) = child.try_wait() {
            return Err(format!(
                "scaffolded server exited before serving (exit: {status}); \
                 port {port} may already be in use"
            ));
        }
        if let Ok(resp) = client.get(&url).send()
            && resp.status().is_success()
        {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Err("server did not become ready before the deadline".to_owned())
}

/// Run one baseline+overload measurement cycle against an already-running
/// server, appending samples into `stats`.
fn measure_one_cycle(
    client: &reqwest::blocking::Client,
    url_block: &str,
    pid: u32,
    ceiling: usize,
    load_multiplier: u32,
    stats: &mut OverloadStats,
) {
    // Baseline: offered load == ceiling, no shedding expected.
    let baseline = fire_concurrent(client, url_block, ceiling);
    stats
        .baseline_samples_ms
        .extend(baseline.iter().filter(|o| o.admitted).map(|o| o.elapsed_ms));

    // Sample RSS on a background thread for the duration of the overload burst.
    let rss_samples = Arc::new(Mutex::new(Vec::new()));
    let sampling = Arc::new(AtomicBool::new(true));
    let sampler = {
        let rss_samples = Arc::clone(&rss_samples);
        let sampling = Arc::clone(&sampling);
        std::thread::spawn(move || {
            while sampling.load(Ordering::Relaxed) {
                if let Some(kb) = sample_rss_kb(pid) {
                    rss_samples.lock().unwrap().push(kb);
                }
                std::thread::sleep(Duration::from_millis(30));
            }
        })
    };

    // Overload: offered load == ceiling * multiplier, fired simultaneously.
    let overload_count = ceiling * load_multiplier as usize;
    let overload = fire_concurrent(client, url_block, overload_count);

    sampling.store(false, Ordering::Relaxed);
    let _ = sampler.join();

    for outcome in &overload {
        if outcome.admitted {
            stats.admitted_samples_ms.push(outcome.elapsed_ms);
            stats.admitted_count += 1;
        } else {
            stats.shed_samples_ms.push(outcome.elapsed_ms);
            stats.shed_count += 1;
        }
    }
    stats
        .rss_samples_kb
        .extend(rss_samples.lock().unwrap().iter().copied());
}

/// Scaffold, build, boot, and measure the overload benchmark `runs` times
/// against the same running server. Returns the accumulated raw samples.
fn measure_overload(
    ceiling: usize,
    block_ms: u64,
    load_multiplier: u32,
    runs: u32,
) -> Result<OverloadStats, String> {
    let autumn_web = cached_autumn_web().ok_or_else(|| {
        "could not locate (or canonicalize) the workspace autumn-web crate; \
         set AUTUMN_BENCH_AUTUMN_WEB_PATH to its directory"
            .to_owned()
    })?;

    let tmp = tempfile::tempdir().map_err(|e| format!("create temp dir: {e}"))?;
    let bin_name = "overload_bench_app";
    let app = generate_overload_app(block_ms);
    scaffold_app(&app, tmp.path(), &autumn_web)?;
    let bin = build_app(tmp.path(), bin_name)?;

    let port = match std::env::var("AUTUMN_BENCH_PORT")
        .ok()
        .and_then(|p| p.parse::<u16>().ok())
    {
        Some(p) => p,
        None => reserve_free_port()?,
    };

    let mut cmd = Command::new(&bin);
    cmd.current_dir(tmp.path());
    // Isolate the child from the surrounding environment — see
    // `cold_start_driver::measure_cold_start_once` for the full rationale.
    for (key, _) in std::env::vars() {
        if key.starts_with("AUTUMN_") {
            cmd.env_remove(&key);
        }
    }
    cmd.env("AUTUMN_SERVER__HOST", "127.0.0.1")
        .env("AUTUMN_SERVER__PORT", port.to_string())
        .env(
            "AUTUMN_SERVER__MAX_CONCURRENT_REQUESTS",
            ceiling.to_string(),
        )
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to start the built server binary: {e}"))?;

    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| format!("build http client: {e}"))?;

    let ready_deadline = Instant::now() + Duration::from_secs(30);
    if let Err(e) = wait_until_ready(&client, &mut child, port, ready_deadline) {
        stop_child(&mut child);
        return Err(e);
    }

    let url_block = format!("http://127.0.0.1:{port}/block");
    let mut stats = OverloadStats::default();
    let pid = child.id();

    for i in 1..=runs {
        eprintln!("  overload run {i}/{runs}…");
        measure_one_cycle(
            &client,
            &url_block,
            pid,
            ceiling,
            load_multiplier,
            &mut stats,
        );
    }

    stop_child(&mut child);
    Ok(stats)
}

/// Run the `autumn dev-loop-bench --overload` command.
///
/// In `--dry-run` mode it prints the overload budget/methodology table with
/// no build or server. Otherwise it scaffolds and measures the live overload
/// benchmark and emits the report.
// Mirrors the flag set of `crate::dev_loop_bench::run` plus the
// overload-only `ceiling`/`block_ms`/`load_multiplier` params; grouping
// these into a struct would not improve the single CLI call site (same
// rationale as `run_cold_start`'s identical allow).
#[allow(clippy::fn_params_excessive_bools, clippy::too_many_arguments)]
pub fn run_overload(
    ceiling: usize,
    block_ms: u64,
    load_multiplier: u32,
    runs: u32,
    output: Option<&str>,
    json: bool,
    fail_on_regression: bool,
    dry_run: bool,
) -> i32 {
    if dry_run {
        print!("{}", format_overload_budget_table());
        return 0;
    }

    if runs == 0 {
        eprintln!("Error: --runs must be at least 1 for an overload measurement.");
        return 1;
    }
    if ceiling == 0 {
        eprintln!("Error: --ceiling must be at least 1.");
        return 1;
    }

    eprintln!(
        "autumn dev-loop-bench --overload: measuring overload/load-shedding \
         (ceiling={ceiling}, block_ms={block_ms}, load_multiplier={load_multiplier}x, {runs} run(s))"
    );
    eprintln!("This scaffolds a throwaway app and compiles it — expect tens of seconds.\n");

    let stats = match measure_overload(ceiling, block_ms, load_multiplier, runs) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Error: overload measurement failed: {e}");
            return 1;
        }
    };

    let report = build_overload_report(ceiling, block_ms, load_multiplier, &stats);
    emit_overload_report(&report, json, output, fail_on_regression)
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_overload_app_bakes_block_ms_into_source() {
        let app = generate_overload_app(200);
        assert!(app.cargo_toml.contains("autumn-web"));
        let (path, content) = &app.files[0];
        assert_eq!(path, "src/main.rs");
        assert!(content.contains("from_millis(200)"));
        assert!(content.contains("/block"));
    }

    #[test]
    fn scaffold_app_writes_manifest_and_patches_autumn_web() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let autumn_web = tmp.path().join("autumn");
        let app = generate_overload_app(150);
        scaffold_app(&app, tmp.path(), &autumn_web).expect("scaffold should succeed");

        let manifest =
            std::fs::read_to_string(tmp.path().join("Cargo.toml")).expect("read manifest");
        assert!(manifest.contains("[patch.crates-io]"));
        assert!(manifest.contains("autumn-web = { path ="));

        assert!(tmp.path().join("src/main.rs").is_file());
    }

    #[test]
    fn run_overload_dry_run_returns_zero() {
        assert_eq!(run_overload(64, 200, 2, 1, None, false, false, true), 0);
        assert_eq!(run_overload(64, 200, 2, 1, None, true, false, true), 0);
    }

    #[test]
    fn run_overload_rejects_zero_runs() {
        let exit = run_overload(64, 200, 2, 0, None, false, false, false);
        assert_eq!(exit, 1, "zero runs must be rejected");
    }

    #[test]
    fn run_overload_rejects_zero_ceiling() {
        let exit = run_overload(0, 200, 2, 1, None, false, false, false);
        assert_eq!(exit, 1, "zero ceiling must be rejected");
    }

    #[test]
    fn run_overload_dry_run_ignores_invalid_params() {
        // Dry-run never measures, so a zero runs/ceiling is harmless there.
        assert_eq!(run_overload(0, 200, 2, 0, None, false, false, true), 0);
    }

    #[test]
    fn sample_rss_kb_of_current_process_is_plausible_or_unsupported() {
        // On Linux this should return a positive number for our own PID; on
        // other platforms it must gracefully return None rather than panic.
        let pid = std::process::id();
        if let Some(kb) = sample_rss_kb(pid) {
            assert!(kb > 0, "RSS should be positive for a running process");
        }
        // `None` (unsupported platform) is acceptable — nothing further to assert.
    }

    #[test]
    fn fire_concurrent_reports_transport_errors_as_not_admitted() {
        // Nothing is listening on this reserved-but-unused port, so every
        // request should fail to connect and be classified as not admitted.
        let port = reserve_free_port().expect("reserve a free port");
        let url = format!("http://127.0.0.1:{port}/nope");
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .expect("build http client");
        let outcomes = fire_concurrent(&client, &url, 3);
        assert_eq!(outcomes.len(), 3);
        assert!(outcomes.iter().all(|o| !o.admitted));
    }

    // ── live driver (slow: compiles and runs a throwaway project) ──────────

    #[test]
    #[ignore = "compiles and runs a throwaway project; run with --ignored"]
    fn overload_live_measurement_sheds_excess_and_keeps_admitted_healthy() {
        let stats =
            measure_overload(4, 100, 3, 1).expect("live overload measurement should succeed");
        assert!(
            stats.shed_count > 0,
            "offered load at 3x a ceiling of 4 should shed some requests"
        );
        assert!(
            stats.admitted_count > 0,
            "some requests should still be admitted"
        );
    }
}
