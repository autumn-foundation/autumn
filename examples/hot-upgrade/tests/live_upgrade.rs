//! Live in-place upgrade under sustained load (issue #1674).
//!
//! Boots the real `hot-upgrade-v1` binary, drives it with concurrent HTTP
//! clients, upgrades it in place to the real `hot-upgrade-v2` binary with
//! `SIGUSR2`, and proves the properties the issue asks for:
//!
//! * zero refused connections and zero failed requests across the cutover;
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
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .expect("the v1 binary starts");
    let pid = child.id();
    let log = Arc::new(Mutex::new(Vec::new()));
    let addr = capture_bound_addr(child.stdout.take().expect("piped stdout"), Arc::clone(&log))
        .expect("v1 logs the address it bound");

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
    let connect_errors = Arc::new(AtomicU64::new(0));

    let mut load = Vec::new();
    for i in 0..8 {
        let addr = addr.clone();
        let stop = Arc::clone(&stop);
        let reads = Arc::clone(&reads);
        let writes = Arc::clone(&writes);
        let connect_errors = Arc::clone(&connect_errors);
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
                match get(&addr, path).await {
                    Ok(observation) => {
                        if let Some(reported) = parse_line(&observation.body)
                            && reported.version == "v2"
                        {
                            *successor_pid.lock().expect("successor pid") = Some(reported.pid);
                        }
                        sink.lock().expect("sink").push(observation);
                    }
                    Err(error) => {
                        connect_errors.fetch_add(1, Ordering::Relaxed);
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

    // Keep the load running well past the cutover.
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

    assert_eq!(
        connect_errors.load(Ordering::Relaxed),
        0,
        "no connection may be refused across the cutover; logs:\n{logged}"
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
