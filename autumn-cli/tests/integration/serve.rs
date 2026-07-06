//! Integration test for the `autumn serve` daemon lifecycle over a Unix socket.
//!
//! `#[ignore]` because it scaffolds and compiles a fresh project. Run with:
//!
//! ```sh
//! cargo test -p autumn-cli --test serve -- --ignored
//! ```

#![cfg(unix)]

use std::fmt::Write as _;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

fn patch_generated_cargo_toml(project_dir: &Path) {
    let cargo_toml_path = project_dir.join("Cargo.toml");
    let mut content = std::fs::read_to_string(&cargo_toml_path).expect("read generated Cargo.toml");
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root");
    let autumn_web_crate = workspace_root.join("autumn");
    write!(
        content,
        "\n[patch.crates-io]\nautumn-web = {{ path = \"{}\" }}\n",
        autumn_web_crate.display().to_string().replace('\\', "/")
    )
    .expect("write to String");
    std::fs::write(&cargo_toml_path, content).expect("patch Cargo.toml");
}

/// Send a minimal HTTP/1.1 request over the Unix socket and return the raw
/// response. Proves the server is reachable on the discovered socket.
fn http_over_unix_socket(socket: &Path, path: &str) -> std::io::Result<String> {
    let mut stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
    )?;
    let mut buf = String::new();
    stream.read_to_string(&mut buf)?;
    Ok(buf)
}

#[test]
#[ignore = "slow: compiles a fresh project — run with `cargo test -p autumn-cli --test serve -- --ignored`"]
fn serve_daemon_start_status_stop_over_unix_socket() {
    let autumn_bin = env!("CARGO_BIN_EXE_autumn");
    let tmp = tempfile::tempdir().expect("tempdir");
    let runtime = tempfile::tempdir().expect("runtime tempdir");

    // ── Scaffold + patch + build a model-free app ───────────────────
    let out = Command::new(autumn_bin)
        .args(["new", "svc"])
        .current_dir(tmp.path())
        .output()
        .expect("run autumn new");
    assert!(out.status.success(), "autumn new failed: {out:?}");
    let project = tmp.path().join("svc");
    patch_generated_cargo_toml(&project);

    // ── Start the daemon (builds, binds the socket, writes addr file) ─
    let out = Command::new(autumn_bin)
        .arg("serve")
        .arg("--daemon")
        .current_dir(&project)
        .env("AUTUMN_RUNTIME_DIR", runtime.path())
        .output()
        .expect("run autumn serve --daemon");
    assert!(
        out.status.success(),
        "serve --daemon failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    // The runtime subdir is namespaced as `<project>-<dir-hash>`; discover it
    // rather than hard-coding the hash.
    let proj_dir = std::fs::read_dir(runtime.path())
        .expect("read runtime dir")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .find(|p| p.is_dir())
        .expect("a project runtime dir under AUTUMN_RUNTIME_DIR");
    let socket = proj_dir.join("serve.sock");
    let pidfile = proj_dir.join("serve.pid");
    let addrfile = proj_dir.join("serve.addr");
    assert!(socket.exists(), "socket should exist after start");
    assert!(pidfile.exists(), "pidfile should exist after start");
    assert!(addrfile.exists(), "address file should exist after start");

    // Ensure we clean up the detached process even if an assertion fails.
    // The lockfile is `<pid> <start_time>`; take the first field.
    let pid = std::fs::read_to_string(&pidfile)
        .ok()
        .and_then(|s| s.split_whitespace().next()?.parse::<u32>().ok());
    let _guard = scopeguard(pid);

    // ── Reachable over the discovered socket ────────────────────────
    let resp = http_over_unix_socket(&socket, "/health").expect("request over unix socket");
    assert!(
        resp.starts_with("HTTP/1.1 2") || resp.contains(" 200 "),
        "expected a 2xx from /health, got:\n{resp}"
    );

    // ── status: running ─────────────────────────────────────────────
    let out = Command::new(autumn_bin)
        .args(["serve", "status"])
        .current_dir(&project)
        .env("AUTUMN_RUNTIME_DIR", runtime.path())
        .output()
        .expect("run autumn serve status");
    assert!(out.status.success(), "status should exit 0 while running");
    assert!(String::from_utf8_lossy(&out.stdout).contains("running"));

    // ── Reject a second start ───────────────────────────────────────
    let out = Command::new(autumn_bin)
        .args(["serve", "--daemon"])
        .current_dir(&project)
        .env("AUTUMN_RUNTIME_DIR", runtime.path())
        .output()
        .expect("run second serve --daemon");
    assert!(
        !out.status.success(),
        "a second start must be rejected, not double-bind"
    );

    // ── stop: drains and removes socket/pidfile ─────────────────────
    let out = Command::new(autumn_bin)
        .args(["serve", "stop"])
        .current_dir(&project)
        .env("AUTUMN_RUNTIME_DIR", runtime.path())
        .output()
        .expect("run autumn serve stop");
    assert!(out.status.success(), "stop should succeed");
    assert!(!socket.exists(), "socket should be removed after stop");
    assert!(!pidfile.exists(), "pidfile should be removed after stop");

    // ── status: stopped (exit code 3) ───────────────────────────────
    let out = Command::new(autumn_bin)
        .args(["serve", "status"])
        .current_dir(&project)
        .env("AUTUMN_RUNTIME_DIR", runtime.path())
        .output()
        .expect("run autumn serve status (stopped)");
    assert_eq!(out.status.code(), Some(3), "stopped status exits 3");
    assert!(String::from_utf8_lossy(&out.stdout).contains("stopped"));
}

/// Minimal drop guard: SIGKILL a leaked detached child on test-failure unwind.
fn scopeguard(pid: Option<u32>) -> impl Drop {
    struct Guard(Option<u32>);
    impl Drop for Guard {
        fn drop(&mut self) {
            if let Some(pid) = self.0
                && let Ok(p) = i32::try_from(pid)
            {
                let _ = nix::sys::signal::kill(
                    nix::unistd::Pid::from_raw(p),
                    nix::sys::signal::Signal::SIGKILL,
                );
            }
        }
    }
    Guard(pid)
}
