//! `autumn calibrate` — the impure half: build, boot, drive load, sample
//! (issue #1733).
//!
//! This module is the orchestration side of the capacity contract. It builds
//! the target binary in **release** mode (a debug-profile envelope is not a
//! number anyone should size infrastructure from), reads its route graph back
//! through the same `AUTUMN_DUMP_ROUTES` pipeline `autumn routes` uses, boots
//! it on a reserved port with admission control *off*, walks a seeded
//! concurrency ladder against it, and hands the samples to the pure,
//! unit-tested logic in [`crate::capacity`].
//!
//! All of it is subprocess / TCP / wall-clock I/O that cannot be exercised in
//! unit tests, so this file is excluded from coverage (see `codecov.yml`),
//! mirroring `cold_start_driver.rs`, `scaling_driver.rs`, and
//! `overload_driver.rs`.
//!
//! ## Why admission control is disabled during calibration
//!
//! Calibration measures what the binary *can* sustain. Leaving a previously
//! configured `server.max_concurrent_requests` — or an already-committed
//! contract — in force would measure that ceiling instead, and each
//! re-calibration would ratchet the recorded envelope down toward whatever
//! the last one happened to record. The driver clears both in the child's
//! environment so every run measures the app, not its own last answer.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use autumn_web::capacity::{
    CONTRACT_VERSION, CapacityContract, Envelope, HostProfile, Provenance, route_graph_digest,
};

use crate::capacity::{
    CheckOutcome, DumpedRoute, RungSamples, RungStats, SaturationOptions, Tolerances,
    calibratable_targets, check_contract, find_saturation, plan_requests, render_check_report,
    route_shapes,
};
use crate::cold_start_driver::{reserve_free_port, stop_child};

/// Default concurrency ladder. Doubling rungs span 1..64 in seven steps, which
/// is enough resolution to locate a knee for a typical single-host app without
/// turning calibration into a coffee break. Widen it with `--concurrency` for
/// a service that saturates higher.
pub const DEFAULT_LADDER: &str = "1,2,4,8,16,32,64";

/// How many requests the seeded plan holds. Long enough that a mixed-shape app
/// sees every route many times, short enough to build once and replay.
const PLAN_LENGTH: usize = 4096;

/// Options for `autumn calibrate`.
pub struct CalibrateOptions<'a> {
    /// Cargo package to calibrate (for a workspace with several).
    pub package: Option<&'a str>,
    /// Binary target name, for packages exposing several.
    pub bin: Option<&'a str>,
    /// Path of the contract to write (or to check against).
    pub contract_path: &'a str,
    /// Gate mode: compare against the committed contract instead of writing.
    pub check: bool,
    /// Seed for the request profile, so a run is replayable.
    pub seed: u64,
    /// Concurrency ladder to walk.
    pub concurrency: Vec<usize>,
    /// Milliseconds to hold each rung.
    pub rung_ms: u64,
    /// Milliseconds of discarded warmup before the ladder.
    pub warmup_ms: u64,
    /// Regression tolerances used by `--check`.
    pub tolerances: Tolerances,
    /// Emit the contract as JSON on stdout instead of the human summary.
    pub json: bool,
}

/// Run `autumn calibrate`, returning the process exit code.
pub fn run(opts: &CalibrateOptions<'_>) -> i32 {
    eprintln!("\u{1F342} autumn calibrate\n");

    let binary = match build_binary(opts) {
        Ok(path) => path,
        Err(error) => {
            eprintln!("\u{2717} {error}");
            return 1;
        }
    };

    let dump = match dump_routes(&binary) {
        Ok(dump) => dump,
        Err(error) => {
            eprintln!("\u{2717} {error}");
            return 1;
        }
    };

    let shapes = route_shapes(&dump);
    let targets = calibratable_targets(&dump);
    if targets.is_empty() {
        eprintln!(
            "\u{2717} no calibratable routes found.\n\n  \
             A calibration run drives load against unauthenticated `GET` routes with no \
             path\n  parameters, so it measures handlers rather than auth rejections or \
             404s.\n  This app exposes none, so there is nothing to calibrate yet."
        );
        return 1;
    }

    eprintln!(
        "  {} route(s) in the graph, {} calibratable target(s), seed {}",
        shapes.len(),
        targets.len(),
        opts.seed
    );

    let port = match reserve_free_port() {
        Ok(port) => port,
        Err(error) => {
            eprintln!("\u{2717} {error}");
            return 1;
        }
    };

    let mut child = match boot(&binary, port) {
        Ok(child) => child,
        Err(error) => {
            eprintln!("\u{2717} {error}");
            return 1;
        }
    };

    let result = measure(opts, port, &targets);
    stop_child(&mut child);

    let rungs = match result {
        Ok(rungs) => rungs,
        Err(error) => {
            eprintln!("\u{2717} {error}");
            return 1;
        }
    };

    let Some(knee) = find_saturation(&rungs, &SaturationOptions::default()) else {
        eprintln!(
            "\u{2717} no sustainable envelope found — every rung of the ladder failed beyond \
             the\n  acceptable error rate. Check that the app boots cleanly and that its \
             calibratable\n  routes return success responses."
        );
        return 1;
    };

    print_ladder(&rungs, knee.concurrency);

    let candidate = build_contract(&knee, shapes);

    if opts.check {
        check(opts, &candidate)
    } else {
        write(opts, &candidate)
    }
}

// ── build / boot / dump ──────────────────────────────────────────────────

/// Build the target binary in release mode and return its path.
///
/// Reuses the same cargo-metadata-driven build + resolution `autumn routes`
/// uses, so `calibrate` and `routes` can never disagree about which binary
/// they mean in a multi-target workspace. `release: true` is not optional:
/// `cfg!(debug_assertions)` and profile-gated `#[cfg]` code make a debug
/// binary a genuinely different program, and its throughput is not a number
/// anyone should size infrastructure from.
fn build_binary(opts: &CalibrateOptions<'_>) -> Result<PathBuf, String> {
    eprintln!("  building (release) \u{2026}");
    crate::routes::compile_binary_with_profile(
        opts.package,
        opts.bin,
        &crate::routes::CargoFeatures::default(),
        true,
    );
    Ok(crate::routes::find_binary_in_profile(
        opts.package,
        opts.bin,
        true,
    ))
}

/// Read the app's route graph back through `AUTUMN_DUMP_ROUTES`.
fn dump_routes(binary: &Path) -> Result<Vec<DumpedRoute>, String> {
    let output = Command::new(binary)
        .env("AUTUMN_DUMP_ROUTES", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .output()
        .map_err(|e| format!("failed to run {} to dump routes: {e}", binary.display()))?;
    if !output.status.success() {
        return Err(format!(
            "{} exited with status {} while dumping routes",
            binary.display(),
            output.status
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str(&stdout).map_err(|e| format!("failed to parse route listing JSON: {e}"))
}

/// Boot the binary on `port` with admission control disabled, and wait for it
/// to report live.
fn boot(binary: &Path, port: u16) -> Result<std::process::Child, String> {
    let mut child = Command::new(binary)
        .env("AUTUMN_SERVER__PORT", port.to_string())
        .env("AUTUMN_SERVER__HOST", "127.0.0.1")
        // See the module docs: calibration must not measure a ceiling.
        .env("AUTUMN_SERVER__MAX_CONCURRENT_REQUESTS", "0")
        .env("AUTUMN_SERVER__CAPACITY_CONTRACT", "")
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| format!("failed to start {}: {e}", binary.display()))?;

    let client = blocking_client();
    let live = format!("http://127.0.0.1:{port}/live");
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        if let Some(status) = child
            .try_wait()
            .map_err(|e| format!("failed to poll the app process: {e}"))?
        {
            return Err(format!("the app exited during startup with status {status}"));
        }
        if client
            .get(&live)
            .send()
            .is_ok_and(|response| response.status().is_success())
        {
            return Ok(child);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    stop_child(&mut child);
    Err("the app did not become live within 60s".to_owned())
}

/// A keep-alive HTTP client sized for the ladder's widest rung.
fn blocking_client() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .pool_max_idle_per_host(1024)
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap_or_default()
}

// ── the ladder ───────────────────────────────────────────────────────────

/// Warm up, then walk the ladder, returning one [`RungStats`] per rung.
fn measure(
    opts: &CalibrateOptions<'_>,
    port: u16,
    targets: &[String],
) -> Result<Vec<RungStats>, String> {
    let client = blocking_client();
    let base = format!("http://127.0.0.1:{port}");
    let plan = plan_requests(opts.seed, targets, PLAN_LENGTH);

    if opts.warmup_ms > 0 {
        eprintln!("  warming up for {}ms …", opts.warmup_ms);
        drop(drive(
            &client,
            &base,
            &plan,
            1,
            Duration::from_millis(opts.warmup_ms),
        ));
    }

    let mut rungs = Vec::with_capacity(opts.concurrency.len());
    for &concurrency in &opts.concurrency {
        let samples = drive(
            &client,
            &base,
            &plan,
            concurrency,
            Duration::from_millis(opts.rung_ms),
        );
        rungs.push(RungStats::from_samples(&samples));
    }
    Ok(rungs)
}

/// Closed-loop load: `concurrency` threads each issue requests back to back
/// until `duration` elapses, drawing targets from the shared seeded plan.
///
/// The seed fixes *which* requests are offered and in what order they are
/// handed out; thread interleaving is not (and cannot be) deterministic, which
/// is exactly why `--check` compares an aggregate envelope with a tolerance
/// rather than exact numbers.
fn drive(
    client: &reqwest::blocking::Client,
    base: &str,
    plan: &[String],
    concurrency: usize,
    duration: Duration,
) -> RungSamples {
    if plan.is_empty() {
        return RungSamples {
            concurrency,
            errors: 0,
            wall_secs: 0.0,
            latencies_ms: Vec::new(),
        };
    }

    let cursor = Arc::new(AtomicUsize::new(0));
    let latencies = Arc::new(Mutex::new(Vec::<f64>::new()));
    let errors = Arc::new(AtomicUsize::new(0));
    let deadline = Instant::now() + duration;
    let started = Instant::now();

    std::thread::scope(|scope| {
        for _ in 0..concurrency {
            let client = client.clone();
            let cursor = Arc::clone(&cursor);
            let latencies = Arc::clone(&latencies);
            let errors = Arc::clone(&errors);
            scope.spawn(move || {
                let mut local = Vec::new();
                while Instant::now() < deadline {
                    let idx = cursor.fetch_add(1, Ordering::Relaxed) % plan.len();
                    let Some(path) = plan.get(idx) else { break };
                    let url = format!("{base}{path}");
                    let sent = Instant::now();
                    match client.get(&url).send() {
                        Ok(response) if response.status().is_success() => {
                            local.push(sent.elapsed().as_secs_f64() * 1000.0);
                        }
                        _ => {
                            errors.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
                if let Ok(mut shared) = latencies.lock() {
                    shared.extend(local);
                }
            });
        }
    });

    let wall_secs = started.elapsed().as_secs_f64();
    let latencies_ms = latencies.lock().map(|l| l.clone()).unwrap_or_default();
    // `usize` -> `u64` is lossless on every target autumn supports.
    #[allow(clippy::cast_possible_truncation)]
    let errors = errors.load(Ordering::Relaxed) as u64;
    RungSamples {
        concurrency,
        errors,
        wall_secs,
        latencies_ms,
    }
}

// ── reporting ────────────────────────────────────────────────────────────

fn print_ladder(rungs: &[RungStats], knee: usize) {
    eprintln!("\n  concurrency    req/s      P99 (ms)   errors");
    for rung in rungs {
        let marker = if rung.concurrency == knee {
            " \u{2190} saturation"
        } else {
            ""
        };
        eprintln!(
            "  {:>11}  {:>9.1}  {:>10.2}   {:>5.2}%{marker}",
            rung.concurrency,
            rung.rps,
            rung.p99_ms,
            rung.error_rate * 100.0
        );
    }
    eprintln!();
}

/// Assemble the contract from the measured knee and the static route graph.
fn build_contract(
    knee: &RungStats,
    routes: Vec<autumn_web::capacity::RouteShape>,
) -> CapacityContract {
    let (git_commit, git_dirty) = git_provenance();
    let mut contract = CapacityContract {
        version: CONTRACT_VERSION,
        provenance: Provenance {
            autumn_version: env!("CARGO_PKG_VERSION").to_owned(),
            calibrated_at: chrono::Utc::now().to_rfc3339(),
            git_commit,
            git_dirty,
            route_graph_digest: route_graph_digest(&routes),
        },
        host: HostProfile::detect(),
        envelope: Envelope {
            sustained_rps: knee.rps,
            p99_latency_ms: knee.p99_ms,
            saturation_concurrency: knee.concurrency,
            // The binary should admit exactly as far as it was still gaining
            // throughput, and shed past it.
            admission_limit: knee.concurrency,
        },
        routes,
    };
    contract.canonicalize();
    contract
}

/// Short commit and dirty flag, when this is a git working tree.
fn git_provenance() -> (Option<String>, bool) {
    let commit = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_owned())
        .filter(|s| !s.is_empty());

    let dirty = Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .is_some_and(|out| !out.stdout.is_empty());

    (commit, dirty)
}

/// Write the freshly measured contract to disk.
fn write(opts: &CalibrateOptions<'_>, contract: &CapacityContract) -> i32 {
    let rendered = match contract.to_toml() {
        Ok(rendered) => rendered,
        Err(error) => {
            eprintln!("\u{2717} failed to render the contract: {error}");
            return 1;
        }
    };

    if opts.json {
        match serde_json::to_string_pretty(contract) {
            Ok(json) => println!("{json}"),
            Err(error) => {
                eprintln!("\u{2717} failed to render the contract as JSON: {error}");
                return 1;
            }
        }
    }

    if let Err(error) = std::fs::write(opts.contract_path, rendered) {
        eprintln!(
            "\u{2717} failed to write {}: {error}",
            opts.contract_path
        );
        return 1;
    }

    eprintln!(
        "\u{2713} wrote {} \u{2014} sustains {:.1} req/s at P99 {:.2}ms on {}, \
         admitting {} concurrent requests",
        opts.contract_path,
        contract.envelope.sustained_rps,
        contract.envelope.p99_latency_ms,
        contract.host.summary(),
        contract.envelope.admission_limit
    );
    eprintln!(
        "  Commit it, gate rebuilds with `autumn calibrate --check`, and point\n  \
         `[server] capacity_contract` at it to admit against the proven envelope."
    );
    0
}

/// Compare the freshly measured contract against the committed one.
fn check(opts: &CalibrateOptions<'_>, candidate: &CapacityContract) -> i32 {
    let committed = match CapacityContract::load(opts.contract_path) {
        Ok(committed) => committed,
        Err(error) => {
            eprintln!(
                "\u{2717} {error}\n\n  Run `autumn calibrate` to record a contract before \
                 gating against one."
            );
            return 1;
        }
    };

    let report = check_contract(&committed, candidate, &opts.tolerances);
    print!("{}", render_check_report(&report));

    match report.outcome {
        CheckOutcome::Pass => 0,
        CheckOutcome::Regressed | CheckOutcome::HostMismatch => 1,
    }
}
