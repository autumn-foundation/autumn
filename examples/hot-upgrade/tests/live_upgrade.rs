//! Live in-place upgrade under sustained load (issue #1674).
//!
//! Boots the real `hot-upgrade-v1` binary, drives it with concurrent HTTP
//! clients, upgrades it in place to the real `hot-upgrade-v2` binary with
//! `SIGUSR2`, and proves the properties the issue asks for:
//!
//! * zero refused connections, and no request fails twice in a row, across
//!   the cutover — an isolated mid-flight reset (`ECONNRESET`/`ECONNABORTED`)
//!   is retried once, but only up to `MAX_TOLERATED_RESETS` of them, so a
//!   systemic source of resets still fails the run rather than being masked;
//! * a `503` from the successor's startup barrier is retried for up to
//!   `STARTUP_BARRIER_BUDGET`. A successor that never becomes ready fails the run;
//! * a value written before the upgrade is readable after it (from the *new*
//!   binary, whose state shape is different);
//! * the state migration ran (`upgrades=1`) and the counter did not reset;
//! * the cutover latency spike stays inside the graceful-restart drain window.
//!
//! Linux/Unix only: in-place upgrade is a `SIGUSR2` + listening-fd handoff.

#![cfg(unix)]

use std::io::{BufRead as _, BufReader};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::net::TcpStream;

/// One observed HTTP exchange.
#[derive(Debug)]
struct Observation {
    status: u16,
    body: String,
    latency: Duration,
}

/// What a response line (`v2 hits=7 note=carried upgrades=1 pid=123`) says.
#[derive(Debug, PartialEq, Eq)]
struct Reported {
    version: String,
    hits: u64,
    note: String,
    upgrades: u64,
    pid: u32,
}

fn parse_line(body: &str) -> Option<Reported> {
    let mut parts = body.split_whitespace();
    let version = parts.next()?.to_owned();
    let mut hits = None;
    let mut note = None;
    let mut upgrades = None;
    let mut pid = None;
    for part in parts {
        let (key, value) = part.split_once('=')?;
        match key {
            "hits" => hits = value.parse().ok(),
            "note" => note = Some(value.to_owned()),
            "upgrades" => upgrades = value.parse().ok(),
            "pid" => pid = value.parse().ok(),
            _ => {}
        }
    }
    Some(Reported {
        version,
        hits: hits?,
        note: note?,
        upgrades: upgrades?,
        pid: pid?,
    })
}

/// Kills the successor when the test ends, however it ends.
///
/// The successor is a *grand*child — the test spawns v1, v1 execs v2 — so
/// nothing here reaps it automatically. A leaked one would hold both the port
/// and the stdout pipe it inherited from the test open indefinitely, which
/// hangs the whole `cargo test` run rather than just failing this test.
struct ReapSuccessor(Arc<Mutex<Option<u32>>>);

impl Drop for ReapSuccessor {
    fn drop(&mut self) {
        let pid = *self.0.lock().expect("successor pid");
        if let Some(pid) = pid {
            let _ = std::process::Command::new("sh")
                .arg("-c")
                .arg(format!("kill -9 {pid}"))
                .status();
        }
    }
}

/// One request per connection, so every single request exercises `accept()` —
/// which is what "zero refused connections" is actually about.
async fn get(addr: &str, path: &str) -> std::io::Result<Observation> {
    let started = Instant::now();
    let mut stream = TcpStream::connect(addr).await?;
    let request =
        format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nAccept: */*\r\n\r\n");
    {
        use tokio::io::AsyncWriteExt as _;
        stream.write_all(request.as_bytes()).await?;
    }
    let mut raw = Vec::new();
    {
        use tokio::io::AsyncReadExt as _;
        stream.read_to_end(&mut raw).await?;
    }
    let latency = started.elapsed();
    let text = String::from_utf8_lossy(&raw).into_owned();
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    let body = text
        .split("\r\n\r\n")
        .nth(1)
        .unwrap_or("")
        .trim()
        .to_owned();
    Ok(Observation {
        status,
        body,
        latency,
    })
}

/// Poll `path` until it stops returning the startup-barrier's `503 Service
/// is still starting up` (`StartupBarrierLayer`/`router.rs`), or `budget`
/// elapses.
///
/// `capture_bound_addr` only waits for v1 to log that it bound the socket —
/// which happens before `on_startup` hooks finish and the app marks itself
/// ready. The barrier is real, intentional behavior (Autumn refuses traffic
/// until startup completes), not a bug to route around; a freshly spawned
/// process satisfies it almost immediately in the common case, so this loop
/// costs nothing there. It only matters when the async runtime itself is
/// starved (heavy CPU contention on a busy CI runner, or an instrumented
/// coverage binary), where the gap between "socket bound" and "startup
/// complete" can stretch well past the time a single unretried request
/// takes — hitting that gap is not a zero-downtime violation, it is the
/// documented startup barrier doing its job on a process that just started.
async fn wait_until_ready(
    addr: &str,
    path: &str,
    budget: Duration,
) -> std::io::Result<Observation> {
    let deadline = Instant::now() + budget;
    loop {
        let observation = get(addr, path).await?;
        if !is_startup_barrier_response(&observation) || Instant::now() >= deadline {
            return Ok(observation);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// A connection reset (`ECONNRESET`) or abort (`ECONNABORTED`) mid-flight.
///
/// Autumn's in-place upgrade hands the successor the *same* listening socket
/// by duplicating its fd across the `exec`
/// (`HandoffSocket::from_listener`/`upgrade.rs`), not by binding a second
/// `SO_REUSEPORT` socket — so there is no accept-queue race between two
/// listeners to blame a reset on; predecessor and successor share one kernel
/// socket throughout. The one observed instance of this (issue #2462, macOS
/// only, did not reproduce on a re-run) has no confirmed root cause. Treating
/// it as retryable-up-to-a-small-bound (`with_reset_retry`, and the count
/// asserted against `MAX_TOLERATED_RESETS` below) means an isolated anomaly
/// doesn't fail the run, while a systemic source of resets — which would
/// point at a genuine defect in the handoff or drain path — still does.
///
/// This is distinct from `ECONNREFUSED`, which means nothing was listening —
/// the actual zero-downtime violation.
fn is_mid_flight_reset(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
    )
}

/// True once a write answered `200` from the `v2` build.
fn v2_accepted_a_write(writes: &Mutex<Vec<Observation>>) -> bool {
    writes
        .lock()
        .expect("writes")
        .iter()
        .any(|w| w.status == 200 && parse_line(&w.body).is_some_and(|r| r.version == "v2"))
}

fn is_connection_refused(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::ConnectionRefused
}

/// How many mid-flight resets a single run may retry past before the test
/// fails anyway. One or two is the isolated anomaly issue #2462 observed;
/// anywhere near double digits stops being noise and starts being evidence of
/// a real defect in the handoff or drain path, which `with_reset_retry`
/// retrying past a reset must not be allowed to hide.
const MAX_TOLERATED_RESETS: u64 = 5;

/// Runs `attempt` once; if it fails with a mid-flight reset, waits out a
/// short backoff and runs it exactly one more time, returning that outcome.
/// A refused connection is never retried — there is nothing to wait out, the
/// guarantee is already broken.
///
/// The backoff (rather than retrying immediately) gives whatever caused the
/// reset a moment to resolve, so the retry doesn't land in the same narrow
/// window that reset the first attempt. The caller is responsible for
/// bounding how many resets a run may retry past — see
/// `MAX_TOLERATED_RESETS`.
///
/// A successful retry's `latency` is overwritten to cover the *whole* span
/// from the first attempt's start through the retry's completion (failed
/// attempt + backoff + retry), not just the retry's own timing — otherwise a
/// first attempt that stalled for seconds before resetting would report only
/// the few milliseconds the cheap retry took, hiding exactly the cutover
/// latency spike this test exists to catch.
async fn with_reset_retry<F, Fut>(
    reset_retries: &AtomicU64,
    mut attempt: F,
) -> std::io::Result<Observation>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = std::io::Result<Observation>>,
{
    let started = Instant::now();
    match attempt().await {
        Err(error) if is_mid_flight_reset(&error) => {
            reset_retries.fetch_add(1, Ordering::Relaxed);
            tokio::time::sleep(Duration::from_millis(20)).await;
            let mut retried = attempt().await;
            if let Ok(observation) = &mut retried {
                observation.latency = started.elapsed();
            }
            retried
        }
        other => other,
    }
}

#[cfg(test)]
mod reset_retry_tests {
    use super::*;

    #[tokio::test]
    async fn retries_exactly_once_after_a_mid_flight_reset() {
        let attempts = AtomicU64::new(0);
        let reset_retries = AtomicU64::new(0);
        let result = with_reset_retry(&reset_retries, || {
            let attempts = &attempts;
            async move {
                if attempts.fetch_add(1, Ordering::Relaxed) == 0 {
                    Err(std::io::Error::from(std::io::ErrorKind::ConnectionReset))
                } else {
                    Ok(Observation {
                        status: 200,
                        body: String::new(),
                        latency: Duration::ZERO,
                    })
                }
            }
        })
        .await;

        assert!(
            result.is_ok(),
            "the retried attempt should succeed: {result:?}"
        );
        assert_eq!(attempts.load(Ordering::Relaxed), 2, "exactly one retry");
        assert_eq!(reset_retries.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn a_second_reset_after_retry_is_reported_as_a_failure() {
        let reset_retries = AtomicU64::new(0);
        let result = with_reset_retry(&reset_retries, || async {
            Err::<Observation, _>(std::io::Error::from(std::io::ErrorKind::ConnectionReset))
        })
        .await;

        assert!(result.is_err(), "two resets in a row must not be swallowed");
        assert!(is_mid_flight_reset(&result.unwrap_err()));
        assert_eq!(
            reset_retries.load(Ordering::Relaxed),
            1,
            "retried exactly once, not looped"
        );
    }

    #[tokio::test]
    async fn a_retried_success_reports_latency_spanning_the_whole_attempt() {
        let reset_retries = AtomicU64::new(0);
        let attempts = AtomicU64::new(0);
        let result = with_reset_retry(&reset_retries, || {
            let attempts = &attempts;
            async move {
                if attempts.fetch_add(1, Ordering::Relaxed) == 0 {
                    Err(std::io::Error::from(std::io::ErrorKind::ConnectionReset))
                } else {
                    Ok(Observation {
                        status: 200,
                        body: String::new(),
                        // Deliberately tiny: if this leaked through
                        // unmodified, the assertion below would fail.
                        latency: Duration::from_millis(1),
                    })
                }
            }
        })
        .await
        .expect("the retried attempt should succeed");

        assert!(
            result.latency >= Duration::from_millis(15),
            "latency must cover the failed first attempt plus the backoff, \
             not just the retry's own reported {:?}",
            result.latency
        );
    }

    #[tokio::test]
    async fn a_refused_connection_is_never_retried() {
        let attempts = AtomicU64::new(0);
        let reset_retries = AtomicU64::new(0);
        let result = with_reset_retry(&reset_retries, || {
            let attempts = &attempts;
            async move {
                attempts.fetch_add(1, Ordering::Relaxed);
                Err(std::io::Error::from(std::io::ErrorKind::ConnectionRefused))
            }
        })
        .await;

        assert!(result.is_err());
        assert!(is_connection_refused(&result.unwrap_err()));
        assert_eq!(
            attempts.load(Ordering::Relaxed),
            1,
            "refused is not retried"
        );
        assert_eq!(reset_retries.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn an_unrelated_error_is_never_retried() {
        let attempts = AtomicU64::new(0);
        let reset_retries = AtomicU64::new(0);
        let result = with_reset_retry(&reset_retries, || {
            let attempts = &attempts;
            async move {
                attempts.fetch_add(1, Ordering::Relaxed);
                Err(std::io::Error::from(std::io::ErrorKind::TimedOut))
            }
        })
        .await;

        assert!(result.is_err());
        assert_eq!(attempts.load(Ordering::Relaxed), 1);
        assert_eq!(reset_retries.load(Ordering::Relaxed), 0);
    }
}

/// How long one request may keep hitting the successor's startup barrier
/// (`503 Service is still starting up`). After this time, the test keeps the
/// 503 as the result.
///
/// The successor starts to accept connections (#2368) a little before
/// `mark_startup_complete`. A request can arrive in that short gap. A busy CPU
/// can make the gap long. This 503 is not a refused connection, so the test
/// retries it.
///
/// The limit is time, not a count. It equals the 30 s limit in
/// `wait_until_ready` and `wait_for_exit`. If the successor never becomes
/// ready, the `failed.is_empty()` check fails the test.
const STARTUP_BARRIER_BUDGET: Duration = Duration::from_secs(30);

/// Whether `observation` is the startup barrier's own response, not
/// application data that happens to also be a 503 for a different reason.
fn is_startup_barrier_response(observation: &Observation) -> bool {
    observation.status == 503 && observation.body == "Service is still starting up"
}

/// Runs `attempt` again, after a short pause, while the result is the startup
/// barrier's 503. Stops when `budget` ends and returns the last result.
/// Returns any other result at once, including a connection error. See
/// [`is_startup_barrier_response`].
///
/// After a retry, `latency` covers the whole wait, as in `with_reset_retry`.
/// The spike check must see time spent behind the barrier.
async fn with_startup_barrier_retry<F, Fut>(
    startup_barrier_hits: &AtomicU64,
    budget: Duration,
    mut attempt: F,
) -> std::io::Result<Observation>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = std::io::Result<Observation>>,
{
    let started = tokio::time::Instant::now();
    let deadline = started + budget;
    let mut retried = false;
    loop {
        let mut observation = attempt().await?;
        if retried {
            observation.latency = started.elapsed();
        }
        if !is_startup_barrier_response(&observation) || tokio::time::Instant::now() >= deadline {
            return Ok(observation);
        }
        retried = true;
        startup_barrier_hits.fetch_add(1, Ordering::Relaxed);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[cfg(test)]
mod barrier_retry_tests {
    use super::*;

    fn barrier() -> Observation {
        Observation {
            status: 503,
            body: "Service is still starting up".to_owned(),
            latency: Duration::ZERO,
        }
    }

    fn ok() -> Observation {
        Observation {
            status: 200,
            body: String::new(),
            latency: Duration::ZERO,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn waits_out_a_barrier_longer_than_twenty_attempts() {
        let attempts = AtomicU64::new(0);
        let hits = AtomicU64::new(0);
        let result = with_startup_barrier_retry(&hits, STARTUP_BARRIER_BUDGET, || {
            let attempts = &attempts;
            async move {
                if attempts.fetch_add(1, Ordering::Relaxed) < 30 {
                    Ok(barrier())
                } else {
                    Ok(ok())
                }
            }
        })
        .await
        .expect("no connection error");

        assert_eq!(result.status, 200, "a slow startup is not a failure");
        assert_eq!(hits.load(Ordering::Relaxed), 30);
    }

    // Paused time: the 20 ms pauses run at once and the budget is exact.
    #[tokio::test(start_paused = true)]
    async fn gives_up_when_the_barrier_never_clears() {
        let hits = AtomicU64::new(0);
        let result = with_startup_barrier_retry(&hits, Duration::from_millis(100), || async {
            Ok(barrier())
        })
        .await
        .expect("no connection error");

        assert!(is_startup_barrier_response(&result), "last result is kept");
        assert_eq!(hits.load(Ordering::Relaxed), 5);
        assert!(result.latency >= Duration::from_millis(100));
    }

    #[tokio::test(start_paused = true)]
    async fn a_retried_success_reports_latency_spanning_the_whole_wait() {
        let attempts = AtomicU64::new(0);
        let hits = AtomicU64::new(0);
        let result = with_startup_barrier_retry(&hits, STARTUP_BARRIER_BUDGET, || {
            let attempts = &attempts;
            async move {
                if attempts.fetch_add(1, Ordering::Relaxed) < 10 {
                    Ok(barrier())
                } else {
                    Ok(ok())
                }
            }
        })
        .await
        .expect("no connection error");

        assert_eq!(result.status, 200);
        assert!(result.latency >= Duration::from_millis(200));
    }

    #[tokio::test]
    async fn a_non_barrier_result_is_never_retried() {
        let attempts = AtomicU64::new(0);
        let hits = AtomicU64::new(0);
        let result = with_startup_barrier_retry(&hits, STARTUP_BARRIER_BUDGET, || {
            let attempts = &attempts;
            async move {
                attempts.fetch_add(1, Ordering::Relaxed);
                Ok(ok())
            }
        })
        .await
        .expect("no connection error");

        assert_eq!(result.status, 200);
        assert_eq!(attempts.load(Ordering::Relaxed), 1);
        assert_eq!(hits.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn a_connection_error_is_returned_at_once() {
        let attempts = AtomicU64::new(0);
        let hits = AtomicU64::new(0);
        let result = with_startup_barrier_retry(&hits, STARTUP_BARRIER_BUDGET, || {
            let attempts = &attempts;
            async move {
                attempts.fetch_add(1, Ordering::Relaxed);
                Err::<Observation, _>(std::io::Error::from(std::io::ErrorKind::ConnectionRefused))
            }
        })
        .await;

        assert!(result.is_err());
        assert_eq!(attempts.load(Ordering::Relaxed), 1);
    }
}

/// Drain the child's stdout — where Autumn's log lines go — into a shared
/// buffer (so it can never block on a full pipe) and hand back the address the
/// app reported binding.
fn capture_bound_addr(
    stdout: std::process::ChildStdout,
    log: Arc<Mutex<Vec<String>>>,
) -> Option<String> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut tx = Some(tx);
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if line.contains("Listening")
                && let Some(addr) = line.split("127.0.0.1:").nth(1)
            {
                let port: String = addr.chars().take_while(char::is_ascii_digit).collect();
                if !port.is_empty()
                    && let Some(tx) = tx.take()
                {
                    let _ = tx.send(format!("127.0.0.1:{port}"));
                }
            }
            log.lock().expect("log mutex").push(line);
        }
    });
    rx.recv_timeout(Duration::from_secs(30)).ok()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn upgrades_in_place_under_load_without_dropping_a_connection_or_the_state() {
    let handoff_dir =
        std::env::temp_dir().join(format!("autumn-live-upgrade-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&handoff_dir);
    std::fs::create_dir_all(&handoff_dir).expect("handoff dir");

    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_hot-upgrade-v1"))
        .env("AUTUMN_SERVER__HOST", "127.0.0.1")
        .env("AUTUMN_SERVER__PORT", "0")
        .env(
            "AUTUMN_UPGRADE_BINARY",
            env!("CARGO_BIN_EXE_hot-upgrade-v2"),
        )
        .env("AUTUMN_UPGRADE_DIR", &handoff_dir)
        .env("AUTUMN_LOG__LEVEL", "info")
        // Nothing here is behind a load balancer, so the deregistration window
        // only slows the test's own teardown down.
        .env("AUTUMN_SERVER__PRESTOP_GRACE_SECS", "0")
        // Pin the production drain budget, for the same reason the prestop
        // grace above is pinned: the `dev` profile shortens
        // `shutdown_timeout_secs` from the 30s default to 1s so Ctrl-C is snappy
        // while developing, and this test asserts a *production* guarantee, so
        // it should not inherit a dev-convenience timing knob.
        //
        // Without this the test is asking for something it has made impossible:
        // the predecessor starts draining at the cutover, but the load loop
        // below keeps driving traffic for another 3.5 seconds. A 1-second drain
        // budget therefore expires while requests are still arriving, and
        // whatever is in flight at that moment is aborted — `exit_code: 1`,
        // failing the "drained and exited 0" assertion below. Nothing about the
        // upgrade path is wrong when that happens; the budget is just shorter
        // than the load window.
        //
        // Measured, because the intuitive story (a slow cutover) is not the
        // real one: reproduced locally under CPU oversubscription with the
        // cutover completing in 135 ms and the drain still aborting two
        // requests. Contention only changes the odds that a request is in
        // flight at the one-second mark. See issue #2372.
        //
        // This weakens nothing. `wait_for_exit` still caps the wait at 30s, so
        // a predecessor that genuinely fails to drain still fails the
        // assertion, and the connection guarantees below — `refused == 0`,
        // `hard == 0`, and the `MAX_TOLERATED_RESETS` bound (#2510) — are
        // untouched by it. Verified rather than assumed: under CPU
        // oversubscription with this pin in place, the refusal/reset assertions
        // still fired on their own, so the budget is not what was holding them.
        .env("AUTUMN_SERVER__SHUTDOWN_TIMEOUT_SECS", "30")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .expect("the v1 binary starts");
    let pid = child.id();
    let log = Arc::new(Mutex::new(Vec::new()));
    let addr = capture_bound_addr(child.stdout.take().expect("piped stdout"), Arc::clone(&log))
        .expect("v1 logs the address it bound");

    // The socket is bound the moment the log line above appears, but v1's
    // `on_startup` hooks may still be running — see `wait_until_ready`.
    let ready = wait_until_ready(&addr, "/", Duration::from_secs(30))
        .await
        .expect("polling for startup readiness");
    assert!(
        !is_startup_barrier_response(&ready),
        "v1 never left the startup barrier within 30s: {ready:?}"
    );

    // A value written to the live state *before* the upgrade. It must be
    // readable, from the new binary, after it.
    let nonce = format!("carried-{pid}");
    let seeded = get(&addr, &format!("/note/{nonce}"))
        .await
        .expect("seeding the live state");
    assert_eq!(seeded.status, 200, "seed response: {seeded:?}");
    assert_eq!(
        parse_line(&seeded.body).expect("parseable").version,
        "v1",
        "the seed must land on the old build"
    );

    // ---- sustained load across the cutover -------------------------------
    let successor_pid = Arc::new(Mutex::new(None));
    let _reaper = ReapSuccessor(Arc::clone(&successor_pid));

    let stop = Arc::new(AtomicBool::new(false));
    let reads = Arc::new(Mutex::new(Vec::<Observation>::new()));
    let writes = Arc::new(Mutex::new(Vec::<Observation>::new()));
    // A refused connection means nothing was listening — the actual
    // zero-downtime violation. A mid-flight reset gets one free retry (see
    // `with_reset_retry`); only a *second* failure in a row is a hard one.
    let refused_errors = Arc::new(AtomicU64::new(0));
    let hard_failures = Arc::new(AtomicU64::new(0));
    let reset_retries = Arc::new(AtomicU64::new(0));
    let startup_barrier_hits = Arc::new(AtomicU64::new(0));

    let mut load = Vec::new();
    for i in 0..8 {
        let addr = addr.clone();
        let stop = Arc::clone(&stop);
        let reads = Arc::clone(&reads);
        let writes = Arc::clone(&writes);
        let refused_errors = Arc::clone(&refused_errors);
        let hard_failures = Arc::clone(&hard_failures);
        let reset_retries = Arc::clone(&reset_retries);
        let startup_barrier_hits = Arc::clone(&startup_barrier_hits);
        let successor_pid = Arc::clone(&successor_pid);
        // Six readers and two writers: reads must never fail, writes may be
        // refused with `503` while the old process's state is frozen.
        let (path, sink) = if i < 6 {
            ("/", Arc::clone(&reads))
        } else {
            ("/bump", Arc::clone(&writes))
        };
        let _ = writes;
        load.push(tokio::spawn(async move {
            while !stop.load(Ordering::Relaxed) {
                // `/bump` is not idempotent, so a reset that lands after the
                // server already applied it — but before the client read the
                // response — means the retry can double-apply the write. That
                // is a real, at-least-once-delivery trade-off, not a bug: the
                // guarantees this test checks (`hits` never goes backwards,
                // 200/503-only outcomes) hold regardless, and it mirrors how
                // any client that retries past a reset behaves in practice.
                match with_startup_barrier_retry(
                    &startup_barrier_hits,
                    STARTUP_BARRIER_BUDGET,
                    || with_reset_retry(&reset_retries, || get(&addr, path)),
                )
                .await
                {
                    Ok(observation) => {
                        if let Some(reported) = parse_line(&observation.body)
                            && reported.version == "v2"
                        {
                            *successor_pid.lock().expect("successor pid") = Some(reported.pid);
                        }
                        sink.lock().expect("sink").push(observation);
                    }
                    Err(error) => {
                        if is_connection_refused(&error) {
                            refused_errors.fetch_add(1, Ordering::Relaxed);
                        } else {
                            hard_failures.fetch_add(1, Ordering::Relaxed);
                        }
                        eprintln!("connection error against {addr}{path}: {error}");
                    }
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }));
    }

    // Let the load settle, then upgrade in place.
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    let signalled = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("kill -USR2 {pid}"))
        .status()
        .expect("signalling the running process");
    assert!(signalled.success(), "kill -USR2 failed");

    // Keep the load running past the cutover — but "past the cutover" has to
    // mean past when the cutover actually finishes, not a fixed guess at how
    // long that takes. The old/new binary swap includes v2's full startup
    // (route table, extensions, adopting the handed-over state and socket),
    // and that startup is NOT part of what this test is trying to bound: the
    // latency-spike assertion below already covers the cutover itself via
    // real request latencies, and `AUTUMN_SERVER__SHUTDOWN_TIMEOUT_SECS`
    // above covers v1's drain. A fixed sleep here has no such budget backing
    // it — it was just long enough on whatever machine last tuned it. Under
    // an instrumented coverage binary (slower to start than a plain release
    // build) or a contended CI runner, v2 can still be starting up when a
    // flat window like the 3.5s this replaced expires, so `successor_pid`
    // never gets set and the "the new build should have served part of the
    // load" assertion fails on a real request that just hadn't arrived yet
    // — not a defect in the upgrade path.
    //
    // So: wait for a v2 response to actually show up (bounded by the same
    // 30s ceiling `wait_for_exit` below uses for "drained and exited", since
    // that is this codebase's existing budget for "an in-place upgrade
    // finished"), then keep the load running for a further fixed window of
    // *sustained* post-cutover traffic — preserving the original intent
    // (checking guarantees against real sustained load after the cutover,
    // not a single v2 sighting) without also being the thing that decides
    // whether the cutover count as having happened in time.
    //
    // Wait for a *write* served by v2, not only a read. Reads reach v2 first,
    // and writes get `503` until v2 owns the state. On a slow instrumented
    // run the fixed tail below can then hold no accepted write.
    let cutover_seen_by = Instant::now() + Duration::from_secs(30);
    while !v2_accepted_a_write(&writes) && Instant::now() < cutover_seen_by {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    tokio::time::sleep(Duration::from_millis(3_500)).await;
    stop.store(true, Ordering::Relaxed);
    for task in load {
        task.await.expect("load task");
    }

    // The old process must have drained and exited cleanly on its own.
    let exit = wait_for_exit(&mut child, Duration::from_secs(30));
    let logged = log.lock().expect("log").join("\n");
    assert!(
        exit.map(|status| status.success()).unwrap_or(false),
        "the predecessor should drain and exit 0 after handing over; logs:\n{logged}"
    );

    // ---- the guarantees --------------------------------------------------
    // Taken by value: the load tasks are joined, so nothing else holds these,
    // and a guard must not be alive across the awaits further down.
    let reads = std::mem::take(&mut *reads.lock().expect("reads"));
    let writes = std::mem::take(&mut *writes.lock().expect("writes"));

    let refused = refused_errors.load(Ordering::Relaxed);
    let hard = hard_failures.load(Ordering::Relaxed);
    let retried = reset_retries.load(Ordering::Relaxed);
    let startup_barrier = startup_barrier_hits.load(Ordering::Relaxed);
    println!(
        "connection failures across cutover: refused={refused} hard_failures_after_retry={hard} mid_flight_resets_retried={retried} startup_barrier_hits_retried={startup_barrier}"
    );
    // Not bounded like `retried` below: unlike a mid-flight reset (which has
    // no known-benign cause at all), a startup-barrier hit is *expected* to
    // scale with how long the successor's own `on_startup` hooks take —
    // dozens of hits across 8 polling tasks during a slow (contended)
    // startup is normal, not evidence of a defect. `with_startup_barrier_retry`
    // limits each request to `STARTUP_BARRIER_BUDGET`. If the barrier never
    // clears, the resulting
    // still-503 observation still fails the `failed.is_empty()` assertion
    // below — that is the real safety net for "this never recovered".
    assert_eq!(
        refused, 0,
        "no connection may be refused across the cutover; logs:\n{logged}"
    );
    assert_eq!(
        hard, 0,
        "a request failed even after retrying once past a mid-flight reset \
         (ECONNRESET/ECONNABORTED); logs:\n{logged}"
    );
    // Bounded, not unconditional: the successor is handed the *same*
    // listening socket (a duplicated fd, not a second `SO_REUSEPORT`
    // listener — see `is_mid_flight_reset`), so there is no known-benign
    // mechanism that should reset a connection at all. Tolerating a lone
    // reset keeps an unreproduced macOS anomaly (#2462) from failing the
    // run; this bound keeps a *systemic* source of resets — real evidence of
    // a defect in the handoff or drain path — from silently passing.
    assert!(
        retried <= MAX_TOLERATED_RESETS,
        "{retried} mid-flight resets is too many to be an isolated anomaly \
         (bound: {MAX_TOLERATED_RESETS}) — this looks like a real handoff/drain \
         defect, not noise; logs:\n{logged}"
    );
    // Six readers over five seconds put this in the thousands on any machine
    // that is not pathologically loaded; the floor is set well under that so a
    // busy CI runner cannot fail the run for being slow, while still proving
    // the load was sustained rather than a handful of probes.
    assert!(
        reads.len() >= 300,
        "expected sustained load across the cutover, saw {} reads",
        reads.len()
    );
    let failed: Vec<_> = reads.iter().filter(|o| o.status != 200).collect();
    assert!(
        failed.is_empty(),
        "every read must be served across the cutover, saw {failed:?}; logs:\n{logged}"
    );

    // Both builds served traffic on the same socket: the cutover really happened
    // under load rather than after it went quiet.
    let versions: Vec<String> = reads
        .iter()
        .filter_map(|o| parse_line(&o.body).map(|r| r.version))
        .collect();
    assert!(
        versions.iter().any(|v| v == "v1"),
        "the old build should have served part of the load"
    );
    assert!(
        versions.iter().any(|v| v == "v2"),
        "the new build should have served part of the load; logs:\n{logged}"
    );

    // A write is either served or explicitly refused as retryable — never lost
    // silently, and never a connection failure.
    for write in writes.iter() {
        assert!(
            write.status == 200 || write.status == 503,
            "unexpected write outcome {write:?}"
        );
    }
    assert!(
        writes
            .iter()
            .any(|w| w.status == 200 && parse_line(&w.body).is_some_and(|r| r.version == "v2")),
        "the new build must accept writes after the cutover"
    );

    // 100% carry-over: the value written before the upgrade, read from the new
    // binary, whose state shape is a different type.
    let after = get(&addr, "/").await.expect("post-cutover read");
    let after = parse_line(&after.body).expect("parseable");
    assert_eq!(after.version, "v2");
    assert_eq!(after.note, nonce, "the pre-upgrade value must survive");
    assert_eq!(
        after.upgrades, 1,
        "the migration must have run exactly once"
    );

    let highest_v1_hits = reads
        .iter()
        .chain(writes.iter())
        .filter_map(|o| parse_line(&o.body))
        .filter(|r| r.version == "v1")
        .map(|r| r.hits)
        .max()
        .expect("v1 served some traffic");
    assert!(
        after.hits >= highest_v1_hits,
        "the counter must not go backwards across the upgrade: {} < {highest_v1_hits}",
        after.hits
    );

    // The cutover latency spike stays inside the drain window a graceful
    // restart would have cost (prestop grace + shutdown timeout).
    let mut latencies: Vec<Duration> = reads.iter().map(|o| o.latency).collect();
    latencies.sort_unstable();
    let p99 = latencies[latencies.len() * 99 / 100];
    let worst = *latencies.last().expect("latencies");
    println!(
        "cutover latency: p99={p99:?} max={worst:?} over {} reads",
        latencies.len()
    );
    assert!(
        worst < Duration::from_secs(5),
        "cutover latency spike {worst:?} exceeded the graceful-restart drain window"
    );

    // The handoff directory holds application state; it must not be left behind.
    let leftovers: Vec<_> = std::fs::read_dir(&handoff_dir)
        .expect("handoff dir readable")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .collect();
    assert!(
        leftovers.is_empty(),
        "the handoff directory must be cleaned up, found {leftovers:?}"
    );
    let _ = std::fs::remove_dir_all(&handoff_dir);
}

/// Wait for `child` to exit, without blocking forever if it hangs.
fn wait_for_exit(
    child: &mut std::process::Child,
    budget: Duration,
) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(_) => return None,
        }
    }
    let _ = child.kill();
    None
}
