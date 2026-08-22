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
//! * REAL: first deploy (including its pre-start migration, AC-3), zero-downtime
//!   redeploy (AC-2), forced-failure auto-rollback (AC-4), and on-demand rollback
//!   — each asserted against the public port served through `kamal-proxy`.
//! * SIMULATED: reboot survival (AC — "comes back after reboot") is asserted via
//!   `systemctl is-enabled {service}-{slot}` returning `enabled` (boot
//!   persistence), NOT a real kernel reboot of the container.
//! * DEFERRED to a real-VPS follow-up: bare-Ubuntu host preparation from scratch
//!   and the "<15 min / ≤3 commands" onboarding metric — neither is meaningful
//!   against a pre-baked fixture image and both belong in a manual/where-a-real-VPS
//!   -exists follow-up (see the report / tracking issue).
//!
//! ## The fleet lane (issue #1621, Slice 6a)
//!
//! [`fleet_rolling_deploy_lifecycle`] is the multi-server sibling: TWO fixture
//! containers named by `[deploy] hosts`, driven through a serial rolling deploy,
//! a forced pre-boundary halt, and a fleet auto-rollback compensation — again over
//! real ssh, with nothing mocked. See the note on [`FleetNode`] for why it
//! addresses its hosts by docker BRIDGE IP where the single-host tests use mapped
//! loopback ports.
//!
//! Run it with:
//! ```text
//! cargo test -p autumn-cli --test deploy_e2e -- --ignored --nocapture
//! ```

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use testcontainers::core::{CgroupnsMode, ContainerPort, Mount, WaitFor};
use testcontainers::runners::SyncRunner as _;
use testcontainers::{Container, GenericImage, ImageExt};

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

/// Stable fixture-image tag for [`deploy_e2e_full_lifecycle`] (`pam_systemd` off).
const FIXTURE_TAG_DEFAULT: &str = "autumn-deploy-e2e:test";
/// Stable fixture-image tag for [`deploy_e2e_pam_systemd_control_socket`]
/// (`pam_systemd` left enabled — the real-host `XDG_RUNTIME_DIR` shape).
const FIXTURE_TAG_PAM: &str = "autumn-deploy-e2e:pam";
/// Stable fixture-image tag for [`fleet_rolling_deploy_lifecycle`].
///
/// **A distinct tag is a correctness requirement, not a nicety.** Each test's
/// [`Workspace`] mints its OWN throwaway keypair and bakes its public half into the
/// image, and libtest runs these `#[ignore]`d tests in PARALLEL threads (the CI
/// Docker step passes no `--test-threads=1` for this binary). Two tests sharing one
/// stable tag would therefore race: the second `docker build` re-points the tag at an
/// image carrying the *other* run's `authorized_keys`, and whichever test starts its
/// container after that build cannot authenticate to it. Distinct tags also stop one
/// test's end-of-run `docker rmi -f` from yanking the tag out from under a sibling
/// that has not started its containers yet. The fixture shape is identical to
/// `FIXTURE_TAG_DEFAULT`'s, so the expensive apt layer is a cache hit.
const FIXTURE_TAG_FLEET: &str = "autumn-deploy-e2e:fleet";

/// Build the fixture image under the stable tag matching `enable_pam`.
///
/// `enable_pam` selects the `ENABLE_PAM_SYSTEMD` build arg (issue #1948 item 4):
/// `false` (default fixture shape) disables `pam_systemd` so ssh sessions inherit
/// no `XDG_RUNTIME_DIR`; `true` leaves `pam_systemd` ENABLED so ssh sessions get
/// `XDG_RUNTIME_DIR=/run/user/0` exactly like a real host — the honest
/// control-socket regression shape. The two shapes get DISTINCT stable tags so
/// they never clobber each other's cache.
fn build_fixture_image(ctx: &Path, pubkey: &str, enable_pam: bool) -> String {
    let tag = if enable_pam {
        FIXTURE_TAG_PAM
    } else {
        FIXTURE_TAG_DEFAULT
    };
    build_fixture_image_as(ctx, pubkey, enable_pam, tag)
}

/// Build the fixture image under an EXPLICIT tag, writing the Dockerfile, the
/// generated `authorized_keys`, and the extracted kamal-proxy into a fresh build
/// context. Returns the image tag.
///
/// Tags are STABLE (rather than unique per run) so at most one fixture image per
/// tag ever exists: each run overwrites it and reuses the cached apt layers, and the
/// end-of-test `docker rmi` keeps disk bounded even if a previous run's cleanup
/// raced with container teardown. Every concurrently-running test needs its OWN tag
/// — see [`FIXTURE_TAG_FLEET`] for why.
fn build_fixture_image_as(ctx: &Path, pubkey: &str, enable_pam: bool, tag: &str) -> String {
    std::fs::write(
        ctx.join("Dockerfile"),
        include_str!("fixtures/deploy/Dockerfile"),
    )
    .expect("write Dockerfile");
    std::fs::write(ctx.join("authorized_keys"), pubkey).expect("write authorized_keys");
    extract_kamal_proxy(&ctx.join("kamal-proxy"));

    run_ok(
        Command::new("docker").args([
            "build",
            "--build-arg",
            &format!("ENABLE_PAM_SYSTEMD={}", u8::from(enable_pam)),
            "-t",
            tag,
            &ctx.display().to_string(),
        ]),
        "docker build fixture image",
    );
    tag.to_owned()
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

    /// Write `autumn.toml` into the project dir naming a FLEET (issue #1621):
    /// `[deploy] hosts = [...]` in rollout order, sharing one `ssh_port`.
    ///
    /// `hosts` and `ssh_port` are separate parameters because `[deploy]` has exactly
    /// ONE `ssh_port` for the whole fleet — `ResolvedFleet::from_targets` clones the
    /// resolved config and varies only `host` — which is precisely why the fleet test
    /// cannot address its containers by their (distinct, random) mapped loopback
    /// ports. See the note on [`FleetNode`].
    fn write_config_fleet(&self, hosts: &[&str], ssh_port: u16) {
        let list = hosts
            .iter()
            .map(|host| format!("\"{host}\""))
            .collect::<Vec<_>>()
            .join(", ");
        std::fs::write(
            self.project.join("autumn.toml"),
            format!(
                "[server]\nport = {PUBLIC_PORT}\n\n\
                 [deploy]\nhosts = [{list}]\nuser = \"root\"\nssh_port = {ssh_port}\n\
                 app_name = \"{APP_NAME}\"\nreadiness_timeout_secs = {READINESS_TIMEOUT_SECS}\n",
            ),
        )
        .expect("write fleet autumn.toml");
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
const fn set_mode(_path: &Path, _mode: u32) {}

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
    let image_tag = build_fixture_image(ctx.path(), &ws.pubkey, false);
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
    let first_deploy_log = String::from_utf8_lossy(&out.stderr).into_owned();
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

    // AC-3 (#1607) on the FIRST deploy: pending migrations run before the new
    // version takes traffic. The migrate one-shot writes a marker next to the
    // release binary it was launched from (see `fixtures/deploy/app_template.rs`),
    // so this proves the real `systemd-run --wait … AUTUMN_MIGRATE=1` one-shot ran
    // against THIS release — not that the CLI merely planned it.
    let migrated = ws.ssh(
        ssh_port,
        &format!(
            "test -f /srv/autumn/{APP_NAME}/current/{APP_NAME}.migrated && echo present \
             || echo missing"
        ),
    );
    assert_eq!(
        String::from_utf8_lossy(&migrated.stdout).trim(),
        "present",
        "the first deploy must run its pending migrations (#1607, AC-3)"
    );
    // …and it ran BEFORE the release was started, so the app never boots against a
    // schema that was never applied. The deploy echoes each op label as it runs.
    let migrate_at = first_deploy_log
        .find("migrate")
        .expect("the first deploy logs its migrate op");
    let start_at = first_deploy_log
        .find("enable-now")
        .expect("the first deploy logs its app start");
    assert!(
        migrate_at < start_at,
        "the first deploy must migrate before starting the release:\n{first_deploy_log}"
    );
    eprintln!("[e2e]    (#1607 AC-3: pending migrations ran before the first release started)");

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

/// Real-host kamal-proxy control-socket regression (issue #1948 item 4), the
/// cheap in-container half of the deferred real-VPS validation.
///
/// The default fixture DISABLES `pam_systemd` to sidestep the `XDG_RUNTIME_DIR`
/// mismatch between the supervised `kamal-proxy run` systemd service (no
/// `XDG_RUNTIME_DIR` -> `/tmp/kamal-proxy.sock`) and the deploy's ssh sessions
/// (`pam_systemd` sets `XDG_RUNTIME_DIR=/run/user/0` -> a different socket path).
/// That workaround meant the container harness never actually exercised the
/// real-host socket shape. This test builds the fixture with `pam_systemd` LEFT
/// ENABLED (`ENABLE_PAM_SYSTEMD=1`), so the ssh sessions get
/// `XDG_RUNTIME_DIR=/run/user/0` exactly like a real host, and asserts a first
/// deploy STILL reaches the control socket and flips traffic — proving the
/// product's fix (the `env -u XDG_RUNTIME_DIR` prefix on the `kamal-proxy deploy`
/// invocation in `KamalProxyController`) holds without the fixture papering over
/// the mismatch.
///
/// Runs as part of the default Docker CI `--ignored` sweep (same as the sibling
/// `deploy_e2e_full_lifecycle`): the "Run Docker-dependent tests" job executes
/// `cargo test -p autumn-cli --test deploy_e2e -- --ignored`, which picks this up
/// automatically. Run it locally the same way:
/// ```text
/// cargo test -p autumn-cli --test deploy_e2e -- --ignored --nocapture
/// ```
/// The real-VPS GitHub Actions job (`.github/workflows/deploy-real-vps.yml`)
/// covers the same fidelity end-to-end on an actual VM.
#[test]
#[ignore = "requires Docker + ssh client; run with --ignored"]
fn deploy_e2e_pam_systemd_control_socket() {
    let ws = Workspace::build();

    // Build the fixture with pam_systemd ENABLED (the real-host shape).
    let ctx = tempfile::tempdir().expect("ctx tempdir");
    let image_tag = build_fixture_image(ctx.path(), &ws.pubkey, true);
    let (repo, tag) = image_tag.split_once(':').unwrap();

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
        .expect("start pam fixture container");

    let ssh_port = container
        .get_host_port_ipv4(ContainerPort::Tcp(22))
        .expect("mapped ssh port");
    let public_host_port = container
        .get_host_port_ipv4(ContainerPort::Tcp(PUBLIC_PORT))
        .expect("mapped public port");

    wait_for_boot(&ws, ssh_port);
    ws.write_config(ssh_port);

    // Sanity: with pam_systemd ENABLED, an ssh session must carry
    // `XDG_RUNTIME_DIR=/run/user/0` — i.e. we really are exercising the real-host
    // shape, not the disabled-pam workaround.
    let xdg = ws.ssh(ssh_port, "printf %s \"${XDG_RUNTIME_DIR:-<unset>}\"");
    let xdg = String::from_utf8_lossy(&xdg.stdout).into_owned();
    assert_eq!(
        xdg.trim(),
        "/run/user/0",
        "pam_systemd should set XDG_RUNTIME_DIR=/run/user/0 in ssh sessions (real-host shape)"
    );

    // First deploy: the health-gated `kamal-proxy deploy` flip touches the
    // control socket over an ssh session carrying the pam XDG_RUNTIME_DIR. If the
    // product did not pin the CLI to the service's socket it would fail with
    // "connect: no such file or directory"; a served v1 proves the socket was
    // reached.
    ws.stage_release(&ws.app_v1);
    let out = ws.autumn_deploy(&["up"]);
    assert!(
        out.status.success(),
        "first `deploy up` under pam_systemd failed (control-socket mismatch?):\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let body = wait_for_http_ok(public_host_port, "/", Duration::from_secs(30));
    assert!(
        body.contains("e2eapp v1"),
        "deploy under real pam_systemd should serve v1 through kamal-proxy, got: {body:?}"
    );

    drop(container);
    let _ = Command::new("docker")
        .args(["rmi", "-f", &image_tag])
        .output();
}

// ── Fleet harness (issue #1621, Slice 6a) ────────────────────────────────────

/// One booted fixture container in the fleet, with the three addresses this test
/// needs — and the reason there are three rather than one.
///
/// ## Why the fleet is addressed by BRIDGE IP, not by mapped loopback port
///
/// `[deploy]` carries exactly ONE `ssh_port` for the whole fleet:
/// `ResolvedFleet::from_targets` resolves the shared shape once and then varies
/// **only** `host` per entry, and `SshTarget::from_resolved` reads `cfg.ssh_port`
/// for every host. `[deploy] hosts` entries are bare addresses — there is no
/// `host:port` spelling — so two containers published on two DIFFERENT random host
/// ports simply cannot both be named in one fleet. (Extending the config to carry a
/// per-host port is product work, deliberately out of scope for a test slice.)
///
/// The shape the config DOES support is N addresses at one shared port, and each
/// container already has exactly that: its own docker-bridge IP, with sshd on the
/// container's own port 22. So `[deploy] hosts = ["<ip-a>", "<ip-b>"]` with
/// `ssh_port = 22`.
///
/// **Limitation, stated rather than hidden:** container bridge IPs are routable from
/// the docker host only on Linux (and only when the runner IS the docker host rather
/// than a sibling container). That is exactly the environment this test runs in —
/// the Linux-only "Run Docker-dependent tests" CI step — and
/// [`assert_bridge_reachable`] fails fast with an explicit message if it ever is not,
/// instead of surfacing as an opaque ssh failure mid-rollout.
///
/// The HARNESS's own probes never depend on the bridge: [`Workspace::ssh`] and
/// [`http_get`] keep using the MAPPED loopback ports, exactly like the single-host
/// tests. Only the product's ssh/scp path uses the bridge, which is the thing under
/// test.
struct FleetNode {
    /// Held for the lifetime of the test; dropping it removes the container.
    _container: Container<GenericImage>,
    /// Mapped host port for the container's sshd — the HARNESS's own probes only.
    ssh_port: u16,
    /// Mapped host port for the container's public (kamal-proxy) port — the
    /// harness's HTTP assertions and zero-downtime probers.
    public_port: u16,
    /// The container's docker-bridge IP: what `[deploy] hosts` names, reached by
    /// the product on the container's own port 22.
    ip: String,
}

/// Read a container's docker-bridge IPv4 address.
///
/// Goes through the `docker` CLI (as [`extract_kamal_proxy`] already does) rather
/// than `Container::get_bridge_ip_address`, whose implementation additionally
/// inspects the NETWORK named by `HostConfig.NetworkMode` — a lookup that has its
/// own failure modes. This template reads the address straight off the container.
fn container_bridge_ip(id: &str) -> String {
    let out = run_ok(
        Command::new("docker").args([
            "inspect",
            "-f",
            "{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}",
            id,
        ]),
        "docker inspect container bridge ip",
    );
    let ip = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    assert!(
        ip.parse::<std::net::Ipv4Addr>().is_ok(),
        "expected an IPv4 bridge address for container {id}, got {ip:?} \
         (the fleet e2e names hosts by bridge IP — see the note on FleetNode)"
    );
    ip
}

/// Start one fleet container from the (already built) fixture image.
///
/// Identical to the single-host launch except for `.with_network("bridge")`, which
/// pins the container to docker's DEFAULT bridge — the only network whose addresses
/// are routable from the docker host, and therefore the one the fleet's bridge-IP
/// addressing depends on.
fn start_fleet_node(image_tag: &str) -> FleetNode {
    let (repo, tag) = image_tag.split_once(':').unwrap();
    let container = GenericImage::new(repo.to_string(), tag.to_string())
        .with_exposed_port(ContainerPort::Tcp(22))
        .with_exposed_port(ContainerPort::Tcp(PUBLIC_PORT))
        .with_wait_for(WaitFor::Nothing)
        .with_privileged(true)
        .with_cgroupns_mode(CgroupnsMode::Host)
        .with_network("bridge")
        .with_mount(Mount::bind_mount("/sys/fs/cgroup", "/sys/fs/cgroup"))
        .with_mount(Mount::tmpfs_mount("/run"))
        .with_mount(Mount::tmpfs_mount("/run/lock"))
        .with_startup_timeout(Duration::from_secs(180))
        .start()
        .expect("start fleet fixture container");

    let ssh_port = container
        .get_host_port_ipv4(ContainerPort::Tcp(22))
        .expect("mapped ssh port");
    let public_port = container
        .get_host_port_ipv4(ContainerPort::Tcp(PUBLIC_PORT))
        .expect("mapped public port");
    let ip = container_bridge_ip(container.id());
    FleetNode {
        _container: container,
        ssh_port,
        public_port,
        ip,
    }
}

/// Fail fast (with the reason) unless this host can reach `ip:22` directly.
///
/// The fleet addresses its hosts by bridge IP out of necessity (see [`FleetNode`]),
/// and that is the one environmental assumption this test makes. Proving it up front
/// turns an otherwise baffling `ssh: connect to host … Connection timed out` in the
/// middle of a rollout into one sentence naming the assumption.
fn assert_bridge_reachable(ip: &str) {
    let addr: SocketAddr = format!("{ip}:22")
        .parse()
        .unwrap_or_else(|e| panic!("bad bridge address {ip}:22: {e}"));
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut last = String::from("(never attempted)");
    while Instant::now() < deadline {
        match TcpStream::connect_timeout(&addr, Duration::from_secs(3)) {
            Ok(_) => return,
            Err(e) => last = e.to_string(),
        }
        thread::sleep(Duration::from_millis(300));
    }
    panic!(
        "container bridge address {ip}:22 is not reachable from this host ({last}). The fleet \
         e2e must address its hosts by docker bridge IP because `[deploy]` carries ONE \
         fleet-wide `ssh_port` (see the FleetNode note); that needs a Linux docker host whose \
         default bridge is routable, which is what the CI Docker-dependent step provides."
    );
}

/// The release id a host is currently serving: the basename of its `current`
/// symlink — the exact identity `deploy status` reports (`release_id_from_dir` over
/// `readlink -f current`), so the two can be compared directly.
fn current_release(ws: &Workspace, ssh_port: u16) -> String {
    let out = ws.ssh(
        ssh_port,
        &format!("readlink -f /srv/autumn/{APP_NAME}/current 2>/dev/null || true"),
    );
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .to_owned()
}

/// A host's sorted release-dir listing — the "was this host touched at all?"
/// fingerprint, used to prove a halted rollout never reached the hosts after the
/// failing one.
fn releases_listing(ws: &Workspace, ssh_port: u16) -> String {
    let out = ws.ssh(
        ssh_port,
        &format!("ls -1 /srv/autumn/{APP_NAME}/releases 2>/dev/null | sort || true"),
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// How many times the deploy's pre-cutover migrate one-shot has actually RUN on a
/// host.
///
/// The deploy runs migrations as `systemd-run --wait … --setenv=AUTUMN_MIGRATE=1
/// {release}/{app}`, and the fixture app answers that trigger by printing
/// `migrate: no-op (version …)` and exiting 0. The transient unit's stdout lands in
/// the container's journal, so counting that line per host is direct evidence for
/// AC-4 — the fleet migrates on exactly ONE host and never on hosts 2..N. Counting
/// over the whole journal (rather than one transient unit, which `--collect` removes
/// on exit) makes the count cumulative across scenarios, which is what the
/// assertions want: host 2's count must stay 0 for the entire test.
fn migrate_run_count(ws: &Workspace, ssh_port: u16) -> u32 {
    let out = ws.ssh(
        ssh_port,
        "journalctl --no-pager --output=cat 2>/dev/null | grep -c 'migrate: no-op' || true",
    );
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .unwrap_or(0)
}

/// Assert some stderr line names `host` and contains `state` — used against both
/// the rollout header's per-host lines and the shared fleet state table.
///
/// Matched line-wise rather than as one substring because `state_table_lines`
/// pads the host column to the widest host name in the fleet, so the exact spacing
/// between a host and its state depends on which two bridge IPs this run happened to
/// get.
fn assert_state_row(stderr: &str, host: &str, state: &str) {
    assert!(
        stderr
            .lines()
            .any(|line| line.contains(host) && line.contains(state)),
        "expected a fleet row naming {host} with state {state:?}; full stderr:\n{stderr}"
    );
}

// ── The fleet lifecycle ──────────────────────────────────────────────────────

/// Two orchestrated containers exercise the whole FLEET lifecycle in order
/// (issue #1621): fleet first deploy → zero-downtime rolling redeploy with
/// exactly-once migrate → forced pre-boundary halt on host 1 (host 2 never touched)
/// → forced failure on host 2 with host 1 auto-rolled back → `deploy status`.
///
/// ORDERING NOTE (the fleet analogue of the single-host test's): the two failure
/// scenarios run in this order on purpose, and each depends on what the previous one
/// left behind.
///
/// * Scenario 3 fails on the FIRST host. No earlier host has cut over, so
///   `fleet_rollback_set` is empty and NO compensation runs — which is what makes it
///   a clean, deterministic proof of "halt, and never touch the hosts after the
///   failing one". Putting a first-host failure later, after other hosts were on the
///   new release, would conflate halt with compensation.
/// * Scenario 4 then fails on the SECOND host, which is the only way to observe
///   compensation at all: serial rollout guarantees host 2 is touched **only after
///   host 1's cutover completes**, so ANY pre-scripted breakage of host 2 yields a
///   deterministic "host 1 is live on the new release when the rollout halts" — no
///   timing, no racing `docker stop`. The injection used is a read-only bind mount
///   over host 2's `releases` dir: every read-only probe in the all-hosts probe phase
///   still succeeds (so the rollout is NOT refused before host 1 is touched), and the
///   first MUTATING op of host 2's cutover — `prepare-dirs`' `mkdir -p {release_dir}`
///   — fails with EROFS. That is pre-boundary, so host 2 tears its own candidate down
///   and keeps serving, while host 1 is compensated back to its previous release.
/// * Scenario 4 is also safe to run AFTER scenario 3 despite the single-host
///   ordering hazard (a torn-down candidate leaves its slot's unit pointing at the
///   failed release): scenario 4's own cutover rewrites host 1's candidate-slot unit
///   before starting it, and the compensating rollback goes through `rollback_ops`,
///   which re-renders the TARGET slot's unit from the target release dir
///   (`write-target-unit`) rather than trusting whatever is on disk.
#[test]
#[ignore = "requires Docker + ssh client; run with --ignored"]
// A single, deliberately linear orchestration (two booted containers, four ordered
// scenarios, one status report) reads more honestly end-to-end than six fragmented
// helpers that would each have to re-establish the same fleet state.
#[allow(clippy::too_many_lines)]
fn fleet_rolling_deploy_lifecycle() {
    let ws = Workspace::build();

    // Own stable tag: this test runs CONCURRENTLY with its two single-host siblings
    // and bakes its own throwaway key into the image (see `FIXTURE_TAG_FLEET`).
    let ctx = tempfile::tempdir().expect("ctx tempdir");
    let image_tag = build_fixture_image_as(ctx.path(), &ws.pubkey, false, FIXTURE_TAG_FLEET);

    // Both containers are started before either is waited on, so they boot in
    // parallel rather than serially.
    let host_a = start_fleet_node(&image_tag);
    let host_b = start_fleet_node(&image_tag);
    eprintln!(
        "[fleet] host 1: ip={} ssh={} public={}",
        host_a.ip, host_a.ssh_port, host_a.public_port
    );
    eprintln!(
        "[fleet] host 2: ip={} ssh={} public={}",
        host_b.ip, host_b.ssh_port, host_b.public_port
    );
    wait_for_boot(&ws, host_a.ssh_port);
    wait_for_boot(&ws, host_b.ssh_port);
    eprintln!("[fleet] both containers booted");

    // The one environmental assumption, proved before anything else runs.
    assert_bridge_reachable(&host_a.ip);
    assert_bridge_reachable(&host_b.ip);

    // The PRODUCT's ssh passes no `-o UserKnownHostsFile`, so it pins host keys in
    // the ambient `~/.ssh/known_hosts` (resolved from the passwd database, not
    // `$HOME` — see the `SshAgent` note). Bridge IPs are recycled across runs while
    // each run's fixture image carries fresh host keys, so a PERSISTENT runner can
    // hold a stale pin for this run's IP, and `StrictHostKeyChecking=accept-new`
    // would then correctly refuse to connect. Drop any stale pin first; the
    // harness's own ssh is immune (it passes a per-run known-hosts file).
    for ip in [host_a.ip.as_str(), host_b.ip.as_str()] {
        let _ = Command::new("ssh-keygen").args(["-R", ip]).output();
    }

    ws.write_config_fleet(&[host_a.ip.as_str(), host_b.ip.as_str()], 22);

    // ── 1. Fleet FIRST deploy (v1) ──────────────────────────────────────────
    ws.stage_release(&ws.app_v1);
    let out = ws.autumn_deploy(&["up"]);
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(out.status.success(), "fleet first deploy failed:\n{stderr}");
    assert!(
        stderr.contains("across 2 hosts, ONE AT A TIME"),
        "the rollout header should announce a serial 2-host rollout:\n{stderr}"
    );
    assert_state_row(&stderr, &host_a.ip, "first deploy");
    assert_state_row(&stderr, &host_b.ip, "first deploy");
    assert!(
        stderr.contains(&format!("[1/2 {}] deploying release", host_a.ip))
            && stderr.contains(&format!("[2/2 {}] deploying release", host_b.ip)),
        "hosts must roll out in `[deploy] hosts` declaration order:\n{stderr}"
    );
    assert!(
        stderr.contains("Fleet deploy complete") && stderr.contains("all 2 hosts serving"),
        "a clean fleet rollout should report every host serving:\n{stderr}"
    );

    let body_a = wait_for_http_ok(host_a.public_port, "/", Duration::from_secs(30));
    let body_b = wait_for_http_ok(host_b.public_port, "/", Duration::from_secs(30));
    assert!(
        body_a.contains("e2eapp v1") && body_b.contains("e2eapp v1"),
        "both hosts should serve v1 after the fleet first deploy, got {body_a:?} / {body_b:?}"
    );

    // ONE release id per fleet run: every host's `current` resolves to the same
    // release, which is what makes drift reporting and a fleet rollback meaningful.
    let rel_v1 = current_release(&ws, host_a.ssh_port);
    assert!(!rel_v1.is_empty(), "host 1 should have a `current` release");
    assert_eq!(
        rel_v1,
        current_release(&ws, host_b.ssh_port),
        "a fleet run mints exactly ONE release id for every host (#1621)"
    );
    eprintln!("[fleet] 1. fleet first deploy OK — both hosts serve v1 as release {rel_v1}");

    // ── 2. Rolling redeploy to v2, both hosts under load (AC-2 + AC-4) ──────
    thread::sleep(Duration::from_millis(1200)); // distinct per-second release id
    ws.stage_release(&ws.app_v2);
    let prober_a = Prober::start(host_a.public_port);
    let prober_b = Prober::start(host_b.public_port);
    let out = ws.autumn_deploy(&["up"]);
    let (total_a, failures_a) = prober_a.finish();
    let (total_b, failures_b) = prober_b.finish();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        out.status.success(),
        "fleet rolling redeploy failed:\n{stderr}"
    );
    assert_eq!(
        failures_a, 0,
        "host 1 dropped {failures_a}/{total_a} requests during the rolling redeploy"
    );
    assert_eq!(
        failures_b, 0,
        "host 2 dropped {failures_b}/{total_b} requests during the rolling redeploy"
    );
    assert!(
        total_a > 0 && total_b > 0,
        "both probers must have made requests ({total_a} / {total_b})"
    );
    assert_state_row(&stderr, &host_a.ip, "zero-downtime redeploy");
    assert_state_row(&stderr, &host_b.ip, "zero-downtime redeploy");

    // AC-4, as the driver PLANS it: the fleet-wide schema moves exactly once, on the
    // first host still on a previous release, and every other host is told to skip.
    assert!(
        stderr.contains(&format!("migrate ({} only", host_a.ip))
            && stderr.contains("the schema is fleet-wide")
            && stderr.contains(&format!("{} skip it", host_b.ip)),
        "the rollout header should place the single migration on host 1 only:\n{stderr}"
    );
    assert!(
        stderr.contains(
            "the schema has moved; from here an automatic rollback restores BINARIES only"
        ),
        "the fleet should say out loud that a rollback will not undo the migration:\n{stderr}"
    );

    // AC-4, as it actually EXECUTED: the `AUTUMN_MIGRATE=1` one-shot ran on host 1
    // and never on host 2.
    let migrated_a = migrate_run_count(&ws, host_a.ssh_port);
    let migrated_b = migrate_run_count(&ws, host_b.ssh_port);
    eprintln!("[fleet]    migrate one-shot runs: host 1 = {migrated_a}, host 2 = {migrated_b}");
    assert!(
        migrated_a >= 1,
        "the migrate one-shot must have run on host 1 (journal showed {migrated_a} runs)"
    );
    assert_eq!(
        migrated_b, 0,
        "hosts 2..N must NEVER run the fleet migration (journal showed {migrated_b} runs)"
    );

    let rel_v2 = current_release(&ws, host_a.ssh_port);
    assert_eq!(
        rel_v2,
        current_release(&ws, host_b.ssh_port),
        "both hosts must converge on the redeploy's single release id"
    );
    assert_ne!(rel_v2, rel_v1, "the redeploy must mint a NEW release id");
    let body_a = wait_for_http_ok(host_a.public_port, "/", Duration::from_secs(30));
    let body_b = wait_for_http_ok(host_b.public_port, "/", Duration::from_secs(30));
    assert!(
        body_a.contains("e2eapp v2") && body_b.contains("e2eapp v2"),
        "both hosts should serve v2 after the rolling redeploy, got {body_a:?} / {body_b:?}"
    );
    eprintln!(
        "[fleet] 2. rolling redeploy OK — 0/{total_a} and 0/{total_b} requests failed; \
         both hosts serve v2 as release {rel_v2}"
    );

    // ── 3. Forced PRE-boundary failure on host 1: halt, host 2 never touched ─
    thread::sleep(Duration::from_millis(1200));
    let releases_b_before = releases_listing(&ws, host_b.ssh_port);
    ws.stage_release(&ws.app_bad); // refuses /ready forever
    let prober_a = Prober::start(host_a.public_port);
    let prober_b = Prober::start(host_b.public_port);
    let out = ws.autumn_deploy(&["up"]);
    let (total_a, failures_a) = prober_a.finish();
    let (total_b, failures_b) = prober_b.finish();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        !out.status.success(),
        "a readiness-gate timeout on host 1 must exit non-zero:\n{stderr}"
    );
    assert_eq!(
        failures_a, 0,
        "host 1's old release must keep serving through the failed attempt \
         ({failures_a}/{total_a} dropped)"
    );
    assert_eq!(
        failures_b, 0,
        "host 2 was never touched and must not drop a request \
         ({failures_b}/{total_b} dropped)"
    );
    assert!(
        stderr.contains(&format!(
            "rollout halted at {} (`readiness-gate`)",
            host_a.ip
        )) && stderr.contains("the remaining hosts were not touched"),
        "the rollout must halt at host 1's readiness gate:\n{stderr}"
    );
    assert!(
        stderr.contains(&format!(
            "fleet rollout halted at {} during `readiness-gate`",
            host_a.ip
        )),
        "the process error must name the failing host and step:\n{stderr}"
    );
    assert_state_row(
        &stderr,
        &host_a.ip,
        "previous release still serving (rolled back at `readiness-gate`)",
    );
    assert_state_row(&stderr, &host_b.ip, "untouched (not reached)");
    // Host 1 is the FIRST host, so no earlier host is on the new release: the
    // compensation set is empty and the driver must not print a compensation pass.
    assert!(
        !stderr.contains("Compensating"),
        "a failure on the FIRST host has nothing to compensate:\n{stderr}"
    );

    // Host 2 is untouched at the filesystem level, not just in the report.
    assert_eq!(
        releases_listing(&ws, host_b.ssh_port),
        releases_b_before,
        "host 2 must have no release dir from the halted rollout"
    );
    assert_eq!(
        current_release(&ws, host_b.ssh_port),
        rel_v2,
        "host 2's `current` must be unchanged by the halted rollout"
    );
    assert_eq!(
        migrate_run_count(&ws, host_b.ssh_port),
        0,
        "host 2 must still never have migrated"
    );
    assert_eq!(
        current_release(&ws, host_a.ssh_port),
        rel_v2,
        "host 1's candidate was torn down, so its `current` must still be the v2 release"
    );
    let body_a = wait_for_http_ok(host_a.public_port, "/", Duration::from_secs(15));
    let body_b = wait_for_http_ok(host_b.public_port, "/", Duration::from_secs(15));
    assert!(
        body_a.contains("e2eapp v2") && body_b.contains("e2eapp v2"),
        "both hosts must still serve v2 after the halted rollout, got {body_a:?} / {body_b:?}"
    );
    eprintln!(
        "[fleet] 3. halt-on-host-1 OK — exit non-zero, host 2 untouched, both still serve v2"
    );

    // ── 4. Failure on host 2 AFTER host 1 cut over: host 1 auto-rolled back ──
    thread::sleep(Duration::from_millis(1200));
    // Pre-scripted, timing-free injection (see the ORDERING NOTE): make host 2's
    // release dir read-only. Every read-only probe still succeeds, so the all-hosts
    // probe phase passes and host 1 is genuinely deployed and cut over; host 2's
    // first MUTATING op (`prepare-dirs`) then fails with EROFS — pre-boundary, so
    // host 2 keeps serving and host 1 is the host that needs compensating.
    let releases_dir = format!("/srv/autumn/{APP_NAME}/releases");
    let mounted = ws.ssh(
        host_b.ssh_port,
        &format!(
            "mount --bind {releases_dir} {releases_dir} && \
             mount -o remount,bind,ro {releases_dir} {releases_dir}"
        ),
    );
    assert!(
        mounted.status.success(),
        "could not remount host 2's releases dir read-only: {}",
        String::from_utf8_lossy(&mounted.stderr)
    );
    let writable = ws.ssh(
        host_b.ssh_port,
        &format!(
            "touch {releases_dir}/.e2e-write-probe >/dev/null 2>&1 && echo writable \
             || echo readonly"
        ),
    );
    assert_eq!(
        String::from_utf8_lossy(&writable.stdout).trim(),
        "readonly",
        "the scenario-4 injection is inoperative: host 2's releases dir is still writable, \
         so host 2 would deploy successfully and nothing would be compensated"
    );

    let releases_b_before = releases_listing(&ws, host_b.ssh_port);
    // A HEALTHY release, so host 1 genuinely cuts over before host 2 fails.
    ws.stage_release(&ws.app_v2);
    let out = ws.autumn_deploy(&["up"]);
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        !out.status.success(),
        "a failed host 2 must exit non-zero even though host 1 succeeded:\n{stderr}"
    );
    // Serial ordering: host 1 finished its cutover BEFORE host 2 was started.
    assert!(
        stderr.contains(&format!("[1/2 {}] serving", host_a.ip)),
        "host 1 must have cut over before host 2 was touched:\n{stderr}"
    );
    assert!(
        stderr.contains(&format!("rollout halted at {} (", host_b.ip))
            && stderr.contains(&format!("fleet rollout halted at {} during ", host_b.ip)),
        "the halt (and the process error) must name host 2:\n{stderr}"
    );
    // The compensation AC-3 turns on: the host that already cut over is undone, in
    // reverse rollout order, binaries only.
    assert!(
        stderr.contains("Compensating 1 host(s) in reverse rollout order"),
        "host 1 was on the new release, so the fleet must compensate it:\n{stderr}"
    );
    assert!(
        stderr.contains(&format!(
            "[{}] rolling back to the previous release",
            host_a.ip
        )),
        "the compensation must name host 1:\n{stderr}"
    );
    assert_state_row(
        &stderr,
        &host_a.ip,
        "previous release restored (rolled back by the fleet)",
    );
    assert_state_row(
        &stderr,
        &host_b.ip,
        "previous release still serving (rolled back at `",
    );
    assert!(
        stderr.contains("the compensating rollback restored BINARIES only"),
        "the fleet must say the migration was NOT rolled back with the binaries:\n{stderr}"
    );

    assert_eq!(
        current_release(&ws, host_a.ssh_port),
        rel_v2,
        "host 1 must be back on its pre-scenario release after the compensating rollback"
    );
    assert_eq!(
        current_release(&ws, host_b.ssh_port),
        rel_v2,
        "host 2's `current` must be untouched by the failed cutover"
    );
    assert_eq!(
        releases_listing(&ws, host_b.ssh_port),
        releases_b_before,
        "host 2's pre-boundary failure must leave no release dir behind"
    );
    let body_a = wait_for_http_ok(host_a.public_port, "/", Duration::from_secs(30));
    let body_b = wait_for_http_ok(host_b.public_port, "/", Duration::from_secs(30));
    assert!(
        body_a.contains("e2eapp v2") && body_b.contains("e2eapp v2"),
        "the fleet must converge back on v2, got {body_a:?} / {body_b:?}"
    );
    eprintln!("[fleet] 4. compensation OK — host 1 auto-rolled back, fleet converged on {rel_v2}");

    // ── 5. `deploy status`: one converged fleet, no drift (AC-6) ────────────
    let out = ws.autumn_deploy(&["status", "--json"]);
    assert!(
        out.status.success(),
        "`deploy status --json` failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "`deploy status --json` did not emit JSON on stdout ({e}):\n{}",
            String::from_utf8_lossy(&out.stdout)
        )
    });
    let hosts = report["hosts"].as_array().expect("a `hosts` array");
    assert_eq!(
        hosts.len(),
        2,
        "status must report every fleet host: {report}"
    );
    assert_eq!(
        hosts[0]["host"],
        serde_json::json!(host_a.ip),
        "status rows keep `[deploy] hosts` declaration order: {report}"
    );
    assert_eq!(hosts[1]["host"], serde_json::json!(host_b.ip));
    for host in hosts {
        assert_eq!(host["reachable"], serde_json::json!(true), "{report}");
        assert_eq!(host["mode"], serde_json::json!("deployed"), "{report}");
        assert_eq!(
            host["release"],
            serde_json::json!(rel_v2),
            "both hosts must report the SAME release: {report}"
        );
        assert_eq!(host["ready"], serde_json::json!(200), "{report}");
        assert_eq!(host["maintenance"], serde_json::json!(false), "{report}");
        assert_eq!(
            host["proxy_port"],
            serde_json::json!(PUBLIC_PORT),
            "{report}"
        );
        assert!(!host["live_slot"].is_null(), "{report}");
        assert_eq!(host["drift"], serde_json::json!([]), "{report}");
    }
    assert_eq!(
        report["version_drift"],
        serde_json::json!(false),
        "{report}"
    );
    assert_eq!(report["state_drift"], serde_json::json!([]), "{report}");
    assert_eq!(report["drifted"], serde_json::json!(false), "{report}");

    // `--strict`'s exit condition IS `drifted`, so a converged fleet exits 0.
    let out = ws.autumn_deploy(&["status", "--strict"]);
    assert!(
        out.status.success(),
        "`deploy status --strict` must exit 0 on an undrifted fleet:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    eprintln!("[fleet] 5. deploy status OK — 2 hosts, one release, no drift");
    eprintln!("[fleet] all #1621 fleet rollout assertions passed");

    // Best-effort image cleanup (the containers are removed on drop).
    drop(host_a);
    drop(host_b);
    let _ = Command::new("docker")
        .args(["rmi", "-f", &image_tag])
        .output();
}
