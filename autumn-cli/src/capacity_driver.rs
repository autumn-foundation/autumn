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
    CONTRACT_VERSION, Calibration, CapacityContract, Envelope, HostProfile, Provenance,
    route_graph_digest,
};

use crate::capacity::{
    CheckOutcome, DumpedRoute, RungSamples, RungStats, SaturationOptions, Tolerances,
    calibratable_targets, check_contract, find_saturation, plan_requests, render_check_report,
    route_shapes,
};
use crate::cold_start_driver::{reserve_free_port, stop_child};

/// How many requests the seeded plan holds. Long enough that a mixed-shape app
/// sees every route many times, short enough to build once and replay.
const PLAN_LENGTH: usize = 4096;

/// Multiplier applied to the measured saturation concurrency to get the
/// admission limit the contract licenses.
///
/// The two numbers are deliberately not the same, because the concurrency a
/// loopback driver offers is not the concurrency the runtime counts. A
/// `LoadShedLayer` slot is held from the moment the layer sees a request until
/// the handler's future resolves — which in production also spans reading the
/// request body off a real network. By Little's law the same throughput needs
/// a strictly larger in-flight count over a WAN than over `127.0.0.1`, so
/// enforcing the raw knee would shed traffic the binary can actually serve.
/// Headroom errs toward admitting; the envelope itself is still recorded
/// unscaled as `saturation_concurrency`.
const ADMISSION_HEADROOM: usize = 2;

/// Floor for the licensed admission limit.
///
/// A ladder whose second rung fails to gain 5% — a CPU-quota'd container, a
/// noisy shared runner, one calibratable route that serialises on a lock —
/// puts the knee at concurrency 1, and a contract licensing a single in-flight
/// request would 503 essentially all production traffic. Never let a
/// degenerate calibration write a ceiling below what the host can obviously
/// carry.
fn admission_floor(host: &HostProfile) -> usize {
    host.logical_cpus.max(1)
}

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
    /// Seed for the request profile. `None` means the user did not name one.
    pub seed: Option<u64>,
    /// Concurrency ladder to walk. Empty means the user did not name one.
    pub concurrency: Vec<usize>,
    /// Milliseconds to hold each rung. `None` means unspecified.
    pub rung_ms: Option<u64>,
    /// Milliseconds of discarded warmup. `None` means unspecified.
    pub warmup_ms: Option<u64>,
    /// Regression tolerances used by `--check`.
    pub tolerances: Tolerances,
    /// Emit the contract as JSON on stdout instead of the human summary.
    pub json: bool,
}

/// Run `autumn calibrate`, returning the process exit code.
pub fn run(opts: &CalibrateOptions<'_>) -> i32 {
    eprintln!("\u{1F342} autumn calibrate\n");

    let binary = build_binary(opts);

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

    // `--check` must replay the workload the committed contract was measured
    // with, not this invocation's defaults: comparing an envelope taken with
    // `--concurrency 1,8,64 --rung-ms 5000` against one taken with the default
    // ladder compares two different experiments, and the verdict would be about
    // the flags rather than the build.
    let committed = if opts.check {
        match CapacityContract::load(opts.contract_path) {
            Ok(contract) => Some(contract),
            Err(error) => {
                eprintln!(
                    "\u{2717} {error}\n\n  Run `autumn calibrate` to record a contract before \
                     gating against one."
                );
                return 1;
            }
        }
    } else {
        None
    };

    let calibration = match resolve_calibration(opts, committed.as_ref()) {
        Ok(calibration) => calibration,
        Err(error) => {
            eprintln!("\u{2717} {error}");
            return 1;
        }
    };

    eprintln!(
        "  {} route(s) in the graph, {} calibratable target(s), seed {}, ladder {:?}",
        shapes.len(),
        targets.len(),
        calibration.seed,
        calibration.concurrency
    );

    let port = match reserve_free_port() {
        Ok(port) => port,
        Err(error) => {
            eprintln!("\u{2717} {error}");
            return 1;
        }
    };

    // `targets` is non-empty (checked above) and sorted, so the first entry is
    // a stable, unauthenticated, parameterless GET this app really serves.
    let ready_path = targets.first().map_or("/", String::as_str);
    let child = match boot(&binary, port, ready_path) {
        Ok(child) => child,
        Err(error) => {
            eprintln!("\u{2717} {error}");
            return 1;
        }
    };

    // A guard rather than a bare `stop_child` call: a worker panic propagating
    // out of `thread::scope` inside `measure` would otherwise unwind past the
    // cleanup and leave the calibrated app running, holding its port.
    let mut child = ChildGuard(child);
    let rungs = measure(&calibration, port, &targets);
    child.stop();

    let Some(knee) = find_saturation(&rungs, &SaturationOptions::default()) else {
        eprintln!(
            "\u{2717} no sustainable envelope found — every rung of the ladder failed beyond \
             the\n  acceptable error rate. Check that the app boots cleanly and that its \
             calibratable\n  routes return success responses."
        );
        return 1;
    };

    print_ladder(&rungs, knee.concurrency);

    let candidate = build_contract(&knee, shapes, calibration);

    committed.map_or_else(
        || write(opts, &candidate),
        |committed| check(opts, &committed, &candidate),
    )
}

/// The workload this run should drive.
///
/// A flag the user did not name arrives as `None`/empty rather than as its
/// default, so "I did not say" stays distinguishable from "I said the
/// default". In `--check` mode the first replays the committed contract's own
/// workload, because an envelope only means something next to the workload
/// that produced it; the second is honoured, with a warning that the
/// comparison now spans two experiments.
fn resolve_calibration(
    opts: &CalibrateOptions<'_>,
    committed: Option<&CapacityContract>,
) -> Result<Calibration, String> {
    let baseline = committed.map_or_else(Calibration::default, |contract| {
        normalize_recorded(&contract.calibration)
    });

    let named_workload = opts.seed.is_some()
        || !opts.concurrency.is_empty()
        || opts.rung_ms.is_some()
        || opts.warmup_ms.is_some();

    let resolved = Calibration {
        seed: opts.seed.unwrap_or(baseline.seed),
        concurrency: if opts.concurrency.is_empty() {
            baseline.concurrency.clone()
        } else {
            normalized_ladder(&opts.concurrency)
        },
        rung_ms: opts.rung_ms.unwrap_or(baseline.rung_ms),
        warmup_ms: opts.warmup_ms.unwrap_or(baseline.warmup_ms),
    };

    if resolved.concurrency.is_empty() {
        return Err(
            "the concurrency ladder is empty; pass at least one positive value to --concurrency"
                .to_owned(),
        );
    }
    if resolved.rung_ms == 0 {
        return Err(
            "--rung-ms must be positive: a zero-length rung measures nothing, and a \
                    contract built from it would record an envelope nobody proved"
                .to_owned(),
        );
    }

    if committed.is_some() && named_workload && resolved != baseline {
        eprintln!(
            "  note: measuring with the workload you passed, which differs from the one the \
             committed contract records (seed {}, ladder {:?}, rung {}ms). The comparison below \
             therefore spans two different experiments.",
            baseline.seed, baseline.concurrency, baseline.rung_ms
        );
    }

    Ok(resolved)
}

/// A recorded calibration put through the same normalisation a fresh one gets,
/// so a hand-edited contract cannot smuggle a descending or zero-bearing ladder
/// past `find_saturation`'s ascending-order assumption.
fn normalize_recorded(calibration: &Calibration) -> Calibration {
    Calibration {
        seed: calibration.seed,
        concurrency: normalized_ladder(&calibration.concurrency),
        rung_ms: calibration.rung_ms,
        warmup_ms: calibration.warmup_ms,
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
///
/// Infallible from this side: both helpers report and exit the process
/// themselves on a compilation or resolution failure, exactly as `autumn
/// routes` does.
fn build_binary(opts: &CalibrateOptions<'_>) -> PathBuf {
    eprintln!("  building (release) \u{2026}");
    crate::routes::compile_binary_with_profile(
        opts.package,
        opts.bin,
        &crate::routes::CargoFeatures::default(),
        true,
    );
    crate::routes::find_binary_in_profile(opts.package, opts.bin, true)
}

/// Owns the calibrated app process and reaps it on drop, including while
/// unwinding.
struct ChildGuard(std::process::Child);

impl ChildGuard {
    /// Stop the child now. Idempotent — `stop_child` on an already-exited
    /// process is a no-op, so the `Drop` impl running afterwards is harmless.
    fn stop(&mut self) {
        stop_child(&mut self.0);
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        self.stop();
    }
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

/// Boot the binary on `port` with admission control disabled, and wait until
/// it answers.
///
/// Readiness is probed against the first route the ladder will actually drive,
/// not a hard-coded `/live`: an app that moves `health.live_path` (to `/livez`,
/// say) or disables the built-in probes entirely would 404 that path forever,
/// and every calibration would burn the full timeout without measuring
/// anything. A 2xx from a route we are about to load-test is a stronger
/// readiness signal anyway.
fn boot(binary: &Path, port: u16, ready_path: &str) -> Result<std::process::Child, String> {
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
    let ready_url = format!("http://127.0.0.1:{port}{ready_path}");
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(status)) => {
                // Already exited: nothing to reap.
                return Err(format!(
                    "the app exited during startup with status {status}"
                ));
            }
            Ok(None) => {}
            Err(error) => {
                stop_child(&mut child);
                return Err(format!("failed to poll the app process: {error}"));
            }
        }
        if client
            .get(&ready_url)
            .send()
            .is_ok_and(|response| response.status().is_success())
        {
            return Ok(child);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    stop_child(&mut child);
    Err(format!(
        "the app did not answer {ready_path} with a success status within 60s"
    ))
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
fn measure(calibration: &Calibration, port: u16, targets: &[String]) -> Vec<RungStats> {
    let client = blocking_client();
    let base = format!("http://127.0.0.1:{port}");
    let plan = plan_requests(calibration.seed, targets, PLAN_LENGTH);

    if calibration.warmup_ms > 0 {
        eprintln!("  warming up for {}ms …", calibration.warmup_ms);
        drop(drive(
            &client,
            &base,
            &plan,
            1,
            Duration::from_millis(calibration.warmup_ms),
        ));
    }

    // `find_saturation` walks the ladder assuming ascending concurrency: a
    // descending one would make the widest rung the baseline and stop at the
    // second, recording that widest rung as the knee no matter where the real
    // one is. Normalising here makes the flag order-insensitive.
    let mut rungs = Vec::with_capacity(calibration.concurrency.len());
    for &concurrency in &calibration.concurrency {
        let samples = drive(
            &client,
            &base,
            &plan,
            concurrency,
            Duration::from_millis(calibration.rung_ms),
        );
        rungs.push(RungStats::from_samples(&samples));
    }
    rungs
}

/// Ascending, deduplicated, zero-free concurrency ladder.
///
/// A rung of `0` spawns no workers and measures nothing, which
/// `find_saturation` would have to reject anyway; dropping it here keeps the
/// ladder meaningful instead.
fn normalized_ladder(requested: &[usize]) -> Vec<usize> {
    let mut ladder: Vec<usize> = requested.iter().copied().filter(|&n| n > 0).collect();
    ladder.sort_unstable();
    ladder.dedup();
    ladder
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
                    let outcome = client.get(&url).send();
                    // Count only what finished inside the offered window. A
                    // request still in flight at the deadline was neither
                    // delivered during the window nor is its latency
                    // attributable to it, and counting it against a
                    // fixed-length denominator would overstate throughput for
                    // exactly the slow handlers where the envelope matters
                    // most. Requests issued before the deadline that land
                    // after it are simply dropped from both tallies.
                    if Instant::now() > deadline {
                        break;
                    }
                    match outcome {
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

    // Deliberately the *offered* window, not `started.elapsed()`. Workers stop
    // issuing at the deadline, but `thread::scope` joins on the slowest
    // in-flight request: a single straggler that runs into the client's 30s
    // timeout would stretch the denominator to 30s and report a rung at a few
    // percent of what it actually delivered — which `find_saturation` would
    // then read as "gained too little" and record a ceiling far below what the
    // binary sustains. Every counted response was issued inside this window,
    // so the window is the honest denominator.
    // The offered window, and now an honest denominator for it: every counted
    // response both started and finished inside it.
    let wall_secs = duration.as_secs_f64();
    debug_assert!(started.elapsed() >= duration || concurrency == 0);
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
    calibration: Calibration,
) -> CapacityContract {
    let (git_commit, git_dirty) = git_provenance();
    let host = HostProfile::detect();
    let admission_limit = knee
        .concurrency
        .saturating_mul(ADMISSION_HEADROOM)
        .max(admission_floor(&host));
    if admission_limit > knee.concurrency.saturating_mul(ADMISSION_HEADROOM) {
        eprintln!(
            "  note: saturation was measured at concurrency {}, which is below this host's \
             {} logical\n        CPUs; licensing {admission_limit} instead so a degenerate \
             ladder cannot write a\n        ceiling that brownouts the deploy.",
            knee.concurrency, host.logical_cpus
        );
    }
    let mut contract = CapacityContract {
        version: CONTRACT_VERSION,
        provenance: Provenance {
            autumn_version: env!("CARGO_PKG_VERSION").to_owned(),
            calibrated_at: chrono::Utc::now().to_rfc3339(),
            git_commit,
            git_dirty,
            route_graph_digest: route_graph_digest(&routes),
        },
        host,
        envelope: Envelope {
            sustained_rps: knee.rps,
            p99_latency_ms: knee.p99_ms,
            saturation_concurrency: knee.concurrency,
            admission_limit,
        },
        calibration,
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
        eprintln!("\u{2717} failed to write {}: {error}", opts.contract_path);
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
fn check(
    opts: &CalibrateOptions<'_>,
    committed: &CapacityContract,
    candidate: &CapacityContract,
) -> i32 {
    let report = check_contract(committed, candidate, &opts.tolerances);
    print!("{}", render_check_report(&report));

    match report.outcome {
        CheckOutcome::Pass => 0,
        CheckOutcome::Regressed => 1,
        // A distinct code so a red X can be told apart from a real regression
        // without reading the log: exit 2 means "this gate could not judge
        // this build", which is a re-calibration, not an optimisation.
        CheckOutcome::HostMismatch | CheckOutcome::Unusable => 2,
    }
}
