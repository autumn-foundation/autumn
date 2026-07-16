//! AC-7 end-to-end deploy test for `autumn deploy` (issue #1607, Slice 4).
//!
//! This is the top-of-the-pyramid test the earlier slices' unit tests stand in
//! for: it drives the REAL `autumn deploy` binary over REAL ssh/scp against a
//! privileged systemd container that stands in for a freshly-provisioned VPS, and
//! asserts the acceptance criteria end to end. Every primitive is real —
//! `ssh`/`scp` transport, `systemd` units + `systemd-run` migrate one-shot,
//! `curl`-based `/ready` gating, and `kamal-proxy` health-gated traffic flips.
//! Nothing is mocked.
//!
//! ## Why an isolated test binary (not the consolidated `cli_tests`)
//!
//! Per `CLAUDE.md`, a test gets its own binary when it (a) has process-wide side
//! effects or (b) is targeted independently in CI. This one spawns privileged
//! containers and is run as its own `--test deploy_e2e` slice in the Docker CI
//! step, so it lives in `tests/deploy_e2e.rs` with a `[[test]]` entry and is NOT
//! declared in `tests/integration/mod.rs`.
//!
//! ## What is real vs simulated/deferred (honest annotations)
//!
//! * REAL: first deploy, zero-downtime redeploy (AC-2), forced-failure
//!   auto-rollback (AC-4), and on-demand rollback — each asserted against the
//!   public port served through `kamal-proxy`.
//! * SIMULATED: reboot survival (AC — "comes back after reboot") is asserted via
//!   `systemctl is-enabled {service}-{slot}` returning `enabled` (boot
//!   persistence), NOT a real kernel reboot of the container.
//! * DEFERRED to a real-VPS follow-up: bare-Ubuntu host preparation from scratch
//!   and the "<15 min / ≤3 commands" onboarding metric — neither is meaningful
//!   against a pre-baked fixture image and both belong in a manual/where-a-real-VPS
//!   -exists follow-up (see the report / tracking issue).
//!
//! Run it with:
//! ```text
//! cargo test -p autumn-cli --test deploy_e2e -- --ignored --nocapture
//! ```

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use testcontainers::core::{CgroupnsMode, ContainerPort, Mount, WaitFor};
use testcontainers::runners::SyncRunner as _;
use testcontainers::{GenericImage, ImageExt};

/// App name used throughout — the deploy derives every remote path and the
/// systemd unit name from it, and uploads `target/release/{APP_NAME}`.
const APP_NAME: &str = "e2eapp";
/// Public port the reverse proxy binds INSIDE the container (`server.port`). The
/// app slots bind loopback `PUBLIC_PORT + 1` / `+ 2`.
const PUBLIC_PORT: u16 = 8080;
/// Readiness window kept short so the forced-failure gate times out fast.
const READINESS_TIMEOUT_SECS: u64 = 12;
/// Strong (64 hex char) signing secret so preflight passes cleanly.
const SIGNING_SECRET: &str = "a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90";

// ── Small process/HTTP helpers ───────────────────────────────────────────────

/// Run a command, returning its output and asserting success with a readable
/// message (stdout+stderr) on failure.
fn run_ok(cmd: &mut Command, what: &str) -> Output {
    let out = cmd
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn {what}: {e}"));
    assert!(
        out.status.success(),
        "{what} failed (status {:?})\n--- stdout ---\n{}\n--- stderr ---\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    out
}

/// Build the ssh argv the HARNESS uses for its own probes (boot poll, marker
/// assertions). Unlike the product's `SshExecutor` — which passes no `-i` and
/// relies on default key discovery via `$HOME` — the harness passes `-i` and a
/// per-run known-hosts file explicitly so its own calls never depend on `$HOME`.
fn ssh_cmd(key: &Path, known_hosts: &Path, port: u16, remote: &str) -> Command {
    let mut c = Command::new("ssh");
    c.args([
        "-i",
        &key.display().to_string(),
        "-p",
        &port.to_string(),
        "-o",
        "BatchMode=yes",
        "-o",
        "StrictHostKeyChecking=accept-new",
        "-o",
        &format!("UserKnownHostsFile={}", known_hosts.display()),
        "-o",
        "ConnectTimeout=5",
        "root@127.0.0.1",
        remote,
    ]);
    c
}

/// A per-run `ssh-agent` bound to a private socket, holding the throwaway key.
///
/// The product's `SshExecutor` builds its `ssh`/`scp` argv with NO `-i` and no
/// `-o IdentityFile`, so it relies on the ambient ssh identity. A temp `$HOME`
/// does NOT work here: OpenSSH resolves `~/.ssh` from the passwd database
/// (`getpwuid`), not `$HOME`, so it would look under the real home regardless.
/// The robust, env-based hook is `SSH_AUTH_SOCK`: this agent holds the key, and
/// the deploy child inherits `SSH_AUTH_SOCK` so its keyless `ssh` authenticates
/// via the agent — no product code change.
struct SshAgent {
    sock: PathBuf,
    pid: Option<u32>,
}

impl SshAgent {
    fn start(key: &Path, dir: &Path) -> Self {
        let sock = dir.join("agent.sock");
        let _ = std::fs::remove_file(&sock);
        let out = run_ok(
            Command::new("ssh-agent").args(["-a", &sock.display().to_string()]),
            "ssh-agent",
        );
        // ssh-agent prints `SSH_AGENT_PID=<pid>; export SSH_AGENT_PID;`.
        let stdout = String::from_utf8_lossy(&out.stdout);
        let pid = stdout
            .split("SSH_AGENT_PID=")
            .nth(1)
            .and_then(|s| s.split(';').next())
            .and_then(|s| s.trim().parse::<u32>().ok());
        run_ok(
            Command::new("ssh-add")
                .arg(key.display().to_string())
                .env("SSH_AUTH_SOCK", &sock),
            "ssh-add throwaway key",
        );
        Self { sock, pid }
    }
}

impl Drop for SshAgent {
    fn drop(&mut self) {
        if let Some(pid) = self.pid {
            let _ = Command::new("kill").arg(pid.to_string()).output();
        }
        let _ = std::fs::remove_file(&self.sock);
    }
}

/// Perform a bare HTTP/1.0 GET over a fresh TCP connection, returning
/// `(status_code, body)`. Used both by the assertions and (in a tight loop) by
/// the zero-downtime prober — a fresh connection per request is the worst case
/// for a cutover, so a clean 0-failure result is a strong signal.
fn http_get(host_port: u16, path: &str) -> std::io::Result<(u16, String)> {
    let mut stream = TcpStream::connect_timeout(
        &format!("127.0.0.1:{host_port}").parse().unwrap(),
        Duration::from_secs(3),
    )?;
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    stream.set_write_timeout(Some(Duration::from_secs(3)))?;
    write!(
        stream,
        "GET {path} HTTP/1.0\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n"
    )?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw)?;
    let text = String::from_utf8_lossy(&raw).into_owned();
    let status = text
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse::<u16>().ok())
        .ok_or_else(|| std::io::Error::other("no status line"))?;
    let body = text
        .split_once("\r\n\r\n")
        .map_or("", |(_, b)| b)
        .to_string();
    Ok((status, body))
}

/// Poll `path` on the public port until it returns 200, returning the body.
/// Panics on timeout — a healthy release must serve within the window.
fn wait_for_http_ok(host_port: u16, path: &str, timeout: Duration) -> String {
    let deadline = Instant::now() + timeout;
    let mut last = String::from("(never connected)");
    while Instant::now() < deadline {
        match http_get(host_port, path) {
            Ok((200, body)) => return body,
            Ok((code, body)) => last = format!("status {code}: {body}"),
            Err(e) => last = format!("error: {e}"),
        }
        thread::sleep(Duration::from_millis(200));
    }
    panic!("timed out waiting for 200 on {path} (public port {host_port}); last = {last}");
}

/// A background prober that hammers `/` on the public port with fresh
/// connections, counting total requests and failures (any non-200 or transport
/// error). Used to prove the zero-downtime redeploy (AC-2) and that the old
/// release keeps serving through a forced-failure attempt (AC-4).
struct Prober {
    stop: Arc<AtomicBool>,
    total: Arc<AtomicU64>,
    failures: Arc<AtomicU64>,
    handle: Option<thread::JoinHandle<()>>,
}

impl Prober {
    fn start(host_port: u16) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let total = Arc::new(AtomicU64::new(0));
        let failures = Arc::new(AtomicU64::new(0));
        let (s, t, f) = (stop.clone(), total.clone(), failures.clone());
        let handle = thread::spawn(move || {
            while !s.load(Ordering::Relaxed) {
                t.fetch_add(1, Ordering::Relaxed);
                match http_get(host_port, "/") {
                    Ok((200, _)) => {}
                    _ => {
                        f.fetch_add(1, Ordering::Relaxed);
                    }
                }
                thread::sleep(Duration::from_millis(100));
            }
        });
        Self {
            stop,
            total,
            failures,
            handle: Some(handle),
        }
    }

    /// Stop probing and return `(total, failures)`.
    fn finish(mut self) -> (u64, u64) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        (
            self.total.load(Ordering::Relaxed),
            self.failures.load(Ordering::Relaxed),
        )
    }
}

// ── Fixture build ────────────────────────────────────────────────────────────

/// Extract the static `kamal-proxy` binary from the `basecamp/kamal-proxy` Docker
/// Hub image into `dest` via `docker create` + `docker cp`. GitHub release
/// downloads are blocked in this environment; the image ships the same binary.
fn extract_kamal_proxy(dest: &Path) {
    let create = run_ok(
        Command::new("docker").args(["create", "basecamp/kamal-proxy:latest"]),
        "docker create basecamp/kamal-proxy",
    );
    let cid = String::from_utf8_lossy(&create.stdout).trim().to_string();
    let cp = Command::new("docker")
        .args([
            "cp",
            &format!("{cid}:/usr/local/bin/kamal-proxy"),
            &dest.display().to_string(),
        ])
        .output()
        .expect("docker cp kamal-proxy");
    // Always remove the scratch container, even if cp failed.
    let _ = Command::new("docker").args(["rm", "-f", &cid]).output();
    assert!(
        cp.status.success(),
        "docker cp kamal-proxy failed: {}",
        String::from_utf8_lossy(&cp.stderr)
    );
}

/// Build the fixture image into a unique tag, writing the Dockerfile, the
/// generated `authorized_keys`, and the extracted kamal-proxy into a fresh build
/// context. Returns the image tag.
fn build_fixture_image(ctx: &Path, pubkey: &str) -> String {
    std::fs::write(
        ctx.join("Dockerfile"),
        include_str!("fixtures/deploy/Dockerfile"),
    )
    .expect("write Dockerfile");
    std::fs::write(ctx.join("authorized_keys"), pubkey).expect("write authorized_keys");
    extract_kamal_proxy(&ctx.join("kamal-proxy"));

    // A STABLE tag (rather than a unique one) so at most one fixture image ever
    // exists: each run overwrites it and reuses the cached apt layers, and the
    // end-of-test `docker rmi` keeps disk bounded even if a previous run's
    // cleanup raced with container teardown.
    let tag = "autumn-deploy-e2e:test".to_string();
    run_ok(
        Command::new("docker").args(["build", "-t", &tag, &ctx.display().to_string()]),
        "docker build fixture image",
    );
    tag
}

// ── Test-app compilation ─────────────────────────────────────────────────────

/// Compile the template HTTP app into a fully static binary with the given
/// version marker and readiness behaviour. Static linking
/// (`-C target-feature=+crt-static`) lets a host-built binary run in the older
/// -glibc fixture container.
fn compile_app(src_dir: &Path, out: &Path, version: &str, refuse_ready: bool) {
    let src = include_str!("fixtures/deploy/app_template.rs")
        .replace("__VERSION__", version)
        .replace(
            "__REFUSE_READY__",
            if refuse_ready { "true" } else { "false" },
        );
    let src_path = src_dir.join(format!("app_{version}.rs"));
    std::fs::write(&src_path, src).expect("write app source");
    run_ok(
        Command::new("rustc").args([
            "-O",
            "-C",
            "target-feature=+crt-static",
            &src_path.display().to_string(),
            "-o",
            &out.display().to_string(),
        ]),
        &format!("rustc compile app {version}"),
    );
}

// ── Harness scaffolding ──────────────────────────────────────────────────────

/// The per-run on-disk layout: a throwaway `$HOME` (for the deploy child's ssh
/// key discovery), a project dir (cwd for the deploy child, holds `autumn.toml`
/// and `target/release/{APP_NAME}`), and the three compiled app binaries.
struct Workspace {
    _root: tempfile::TempDir,
    home: PathBuf,
    project: PathBuf,
    key: PathBuf,
    known_hosts: PathBuf,
    pubkey: String,
    agent: SshAgent,
    app_v1: PathBuf,
    app_v2: PathBuf,
    app_bad: PathBuf,
}

impl Workspace {
    fn build() -> Self {
        let root = tempfile::tempdir().expect("tempdir");
        let base = root.path().to_path_buf();
        let home = base.join("home");
        let ssh_dir = home.join(".ssh");
        let project = base.join("project");
        let bins = base.join("bins");
        let src = base.join("src");
        for d in [&ssh_dir, &project, &bins, &src] {
            std::fs::create_dir_all(d).expect("mkdir");
        }

        // Throwaway ed25519 keypair. The PUBLIC half is baked into the image; the
        // PRIVATE half stays here for the ssh client. It lands at the default
        // `~/.ssh/id_ed25519` name so the deploy child's `ssh` (which passes no
        // `-i`) discovers it automatically from `$HOME`.
        let key = ssh_dir.join("id_ed25519");
        run_ok(
            Command::new("ssh-keygen").args([
                "-t",
                "ed25519",
                "-N",
                "",
                "-q",
                "-f",
                &key.display().to_string(),
            ]),
            "ssh-keygen",
        );
        let pubkey = std::fs::read_to_string(ssh_dir.join("id_ed25519.pub")).expect("read pubkey");
        let known_hosts = ssh_dir.join("known_hosts");
        std::fs::write(&known_hosts, "").expect("touch known_hosts");
        // ssh refuses a group/world-readable key.
        set_mode(&ssh_dir, 0o700);
        set_mode(&key, 0o600);

        // The deploy child's keyless `ssh` authenticates through this agent.
        let agent = SshAgent::start(&key, &base);

        let app_v1 = bins.join("app_v1");
        let app_v2 = bins.join("app_v2");
        let app_bad = bins.join("app_bad");
        compile_app(&src, &app_v1, "v1", false);
        compile_app(&src, &app_v2, "v2", false);
        // The bad candidate refuses /ready forever (readiness gate times out).
        compile_app(&src, &app_bad, "bad", true);

        Self {
            _root: root,
            home,
            project,
            key,
            known_hosts,
            pubkey,
            agent,
            app_v1,
            app_v2,
            app_bad,
        }
    }

    /// Write `autumn.toml` into the project dir with the mapped ssh port.
    fn write_config(&self, ssh_port: u16) {
        std::fs::write(
            self.project.join("autumn.toml"),
            format!(
                "[server]\nport = {PUBLIC_PORT}\n\n\
                 [deploy]\nhost = \"127.0.0.1\"\nuser = \"root\"\nssh_port = {ssh_port}\n\
                 app_name = \"{APP_NAME}\"\nreadiness_timeout_secs = {READINESS_TIMEOUT_SECS}\n",
            ),
        )
        .expect("write autumn.toml");
    }

    /// Stage `bin` as the release binary the next `autumn deploy up` uploads
    /// (`{project}/target/release/{APP_NAME}`).
    fn stage_release(&self, bin: &Path) {
        let rel = self.project.join("target").join("release");
        std::fs::create_dir_all(&rel).expect("mkdir target/release");
        std::fs::copy(bin, rel.join(APP_NAME)).expect("stage release binary");
    }

    /// Run `autumn deploy <args...>` as a real child, cwd = project dir, with
    /// `SSH_AUTH_SOCK` pointing at the throwaway agent (so its keyless `ssh`
    /// authenticates) and the signing secret in the env. Returns the child's
    /// Output (status may be non-zero by design).
    fn autumn_deploy(&self, args: &[&str]) -> Output {
        let mut c = Command::new(env!("CARGO_BIN_EXE_autumn"));
        c.arg("deploy")
            .args(args)
            .current_dir(&self.project)
            .env("HOME", &self.home)
            .env("SSH_AUTH_SOCK", &self.agent.sock)
            .env("AUTUMN_SECURITY__SIGNING_SECRET", SIGNING_SECRET);
        c.output().expect("spawn autumn deploy")
    }

    fn ssh(&self, port: u16, remote: &str) -> Output {
        ssh_cmd(&self.key, &self.known_hosts, port, remote)
            .output()
            .expect("spawn ssh")
    }
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmod");
}
#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) {}

/// Poll `systemctl is-system-running` over ssh until the container's systemd has
/// finished booting (returns `running`/`degraded`), tolerating the early-boot
/// window where sshd/pam refuses logins with "System is booting up".
fn wait_for_boot(ws: &Workspace, ssh_port: u16) {
    let deadline = Instant::now() + Duration::from_secs(150);
    let mut last = String::new();
    while Instant::now() < deadline {
        let out = ws.ssh(ssh_port, "systemctl is-system-running || true");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        last = format!("stdout={stdout:?} stderr={stderr:?}");
        let state = stdout.trim();
        // `degraded` is acceptable: unrelated units (e.g. a console-getty) may
        // fail in a container; what matters is the boot transition is complete so
        // ssh logins and `systemctl` operate normally.
        if state == "running" || state == "degraded" {
            return;
        }
        thread::sleep(Duration::from_millis(500));
    }
    panic!("container systemd never finished booting; last = {last}");
}

// ── The end-to-end lifecycle ─────────────────────────────────────────────────

/// One orchestrated container exercises the whole deploy lifecycle in order:
/// first deploy → zero-downtime redeploy (AC-2) → on-demand rollback →
/// forced-failure auto-rollback (AC-4).
///
/// ORDERING NOTE: the on-demand rollback runs BEFORE the forced-failure case on
/// purpose. `rollback_ops` restarts the previous release *by slot* using that
/// slot's on-disk unit; a forced-failure candidate is torn down but leaves its
/// slot's unit rewritten to point at the failed release, so rolling back to a
/// target on that slot afterwards would (correctly, per the current design)
/// restart the failed binary. Exercising the clean rollback first keeps every
/// assertion honest; the forced-failure runs last and leaves the good release
/// serving.
#[test]
#[ignore = "requires Docker + ssh client; run with --ignored"]
// A single, deliberately linear orchestration (one booted container, four ordered
// scenarios) reads more honestly end-to-end than four fragmented helpers.
#[allow(clippy::too_many_lines)]
fn deploy_e2e_full_lifecycle() {
    let ws = Workspace::build();

    // Build the fixture image from a fresh build context.
    let ctx = tempfile::tempdir().expect("ctx tempdir");
    let image_tag = build_fixture_image(ctx.path(), &ws.pubkey);
    let (repo, tag) = image_tag.split_once(':').unwrap();

    // Launch the privileged systemd container (a stand-in VPS).
    let container = GenericImage::new(repo.to_string(), tag.to_string())
        .with_exposed_port(ContainerPort::Tcp(22))
        .with_exposed_port(ContainerPort::Tcp(PUBLIC_PORT))
        .with_wait_for(WaitFor::Nothing)
        .with_privileged(true)
        .with_cgroupns_mode(CgroupnsMode::Host)
        .with_mount(Mount::bind_mount("/sys/fs/cgroup", "/sys/fs/cgroup"))
        .with_mount(Mount::tmpfs_mount("/run"))
        .with_mount(Mount::tmpfs_mount("/run/lock"))
        .with_startup_timeout(Duration::from_secs(180))
        .start()
        .expect("start fixture container");

    let ssh_port = container
        .get_host_port_ipv4(ContainerPort::Tcp(22))
        .expect("mapped ssh port");
    let public_host_port = container
        .get_host_port_ipv4(ContainerPort::Tcp(PUBLIC_PORT))
        .expect("mapped public port");

    eprintln!("[e2e] container up: ssh_port={ssh_port} public_port={public_host_port}");
    wait_for_boot(&ws, ssh_port);
    eprintln!("[e2e] container systemd booted");
    ws.write_config(ssh_port);

    // ── 1. First deploy (v1) ────────────────────────────────────────────────
    ws.stage_release(&ws.app_v1);
    let out = ws.autumn_deploy(&["up"]);
    assert!(
        out.status.success(),
        "first `deploy up` failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let body = wait_for_http_ok(public_host_port, "/", Duration::from_secs(30));
    assert!(
        body.contains("e2eapp v1"),
        "first deploy should serve v1, got: {body:?}"
    );
    eprintln!(
        "[e2e] 1. first deploy OK — public port serves {:?}",
        body.trim()
    );

    // AC (simulated reboot survival): the blue slot unit is `enabled`, so systemd
    // would relaunch it after a reboot. Asserted here rather than by a real kernel
    // reboot of the container.
    let en = ws.ssh(
        ssh_port,
        &format!("systemctl is-enabled {APP_NAME}-blue.service || true"),
    );
    assert!(
        String::from_utf8_lossy(&en.stdout).trim() == "enabled",
        "blue slot unit should be enabled for boot survival: {:?}",
        String::from_utf8_lossy(&en.stdout)
    );
    eprintln!("[e2e]    (simulated reboot survival: {APP_NAME}-blue.service is enabled)");

    // AC (#1952): the project `autumn.toml` is uploaded into the per-release dir
    // (coupled to the binary) and the slot unit points AUTUMN_MANIFEST_DIR at that
    // release dir, so the deployed app loads the intended config instead of silent
    // built-in defaults — and a rollback reads the rolled-back release's own copy.
    // The live release dir is reachable through the `current` symlink.
    let manifest_present = ws.ssh(
        ssh_port,
        &format!(
            "test -f /srv/autumn/{APP_NAME}/current/autumn.toml && echo present || echo missing"
        ),
    );
    assert_eq!(
        String::from_utf8_lossy(&manifest_present.stdout).trim(),
        "present",
        "the project autumn.toml should be uploaded to the release dir (#1952)"
    );
    let unit_has_manifest_dir = ws.ssh(
        ssh_port,
        &format!("grep -c 'AUTUMN_MANIFEST_DIR=/srv/autumn/{APP_NAME}/releases/' /etc/systemd/system/{APP_NAME}-blue.service || true"),
    );
    assert!(
        String::from_utf8_lossy(&unit_has_manifest_dir.stdout)
            .trim()
            .parse::<u32>()
            .unwrap_or(0)
            >= 1,
        "the slot unit should set AUTUMN_MANIFEST_DIR to the release dir (#1952)"
    );
    eprintln!(
        "[e2e]    (#1952: autumn.toml uploaded to the release dir and AUTUMN_MANIFEST_DIR set)"
    );

    // ── 2. Zero-downtime redeploy (v2) under load (AC-2) ────────────────────
    thread::sleep(Duration::from_millis(1200)); // distinct per-second release id
    ws.stage_release(&ws.app_v2);
    let prober = Prober::start(public_host_port);
    let out = ws.autumn_deploy(&["up"]);
    let (total, failures) = prober.finish();
    assert!(
        out.status.success(),
        "redeploy `deploy up` failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        failures, 0,
        "zero-downtime redeploy dropped {failures}/{total} requests"
    );
    assert!(total > 0, "prober made no requests");
    let body = wait_for_http_ok(public_host_port, "/", Duration::from_secs(30));
    assert!(
        body.contains("e2eapp v2"),
        "redeploy should serve v2, got: {body:?}"
    );
    eprintln!(
        "[e2e] 2. zero-downtime redeploy OK — {failures}/{total} requests failed during cutover; now serves {:?}",
        body.trim()
    );

    // ── 3. On-demand rollback → previous release (v1) ───────────────────────
    let out = ws.autumn_deploy(&["rollback"]);
    assert!(
        out.status.success(),
        "`deploy rollback` failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let body = wait_for_http_ok(public_host_port, "/", Duration::from_secs(30));
    assert!(
        body.contains("e2eapp v1"),
        "rollback should restore v1, got: {body:?}"
    );
    eprintln!(
        "[e2e] 3. on-demand rollback OK — restored {:?}",
        body.trim()
    );

    // ── 4. Forced-failure auto-rollback (AC-4): candidate refuses /ready ─────
    thread::sleep(Duration::from_millis(1200));
    ws.stage_release(&ws.app_bad);
    let prober = Prober::start(public_host_port);
    let out = ws.autumn_deploy(&["up"]);
    let (total, failures) = prober.finish();
    assert!(
        !out.status.success(),
        "forced-failure deploy should exit non-zero (readiness gate must time out)"
    );
    assert_eq!(
        failures, 0,
        "old release should keep serving through the failed attempt: {failures}/{total} dropped"
    );
    // The old (v1) release is still live — the candidate never passed /ready, so
    // the proxy never flipped.
    let body = wait_for_http_ok(public_host_port, "/", Duration::from_secs(10));
    assert!(
        body.contains("e2eapp v1"),
        "after forced-failure the old v1 release must still serve, got: {body:?}"
    );
    eprintln!(
        "[e2e] 4. forced-failure auto-rollback OK — deploy exited non-zero, {failures}/{total} \
         requests failed, old release still serves {:?}",
        body.trim()
    );
    eprintln!("[e2e] all deploy AC-7 lifecycle assertions passed");

    // Best-effort image cleanup (the container itself is removed on drop).
    drop(container);
    let _ = Command::new("docker")
        .args(["rmi", "-f", &image_tag])
        .output();
}
