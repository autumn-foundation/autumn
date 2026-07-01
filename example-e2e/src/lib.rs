//! Shared support for issue #1192's per-example Chromium e2e smokes.
//!
//! Each example's smoke test spawns the example's **real, unmodified
//! binary** (built normally by `cargo build`, never re-registered
//! in-process) against ephemeral testcontainer Postgres, then attaches a
//! managed headless-Chromium [`SystemTest`] runner at its base URL via
//! [`SystemTest::attach`]. This runs `main.rs` exactly as a release would:
//! real routes, real builder chain, real auto-migration on boot — so a
//! regression in any wired-up feature (shard routing, primary/replica
//! split, mutation hooks, ...) is caught the same way a human clicking
//! through the app would catch it.
//!
//! Not published; workspace-internal tooling only (`publish = false`).

pub use autumn_web::system_test::{Page, SystemTest, SystemTestError, SystemTestRunner};

use std::io;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, ImageExt};
use testcontainers_modules::postgres::Postgres;

/// Every example's own `docker-compose.yml` targets Postgres 16 — the
/// testcontainers-modules default (`11-alpine`) predates generated columns
/// (used by e.g. wiki's full-text-search migration) and other features
/// examples rely on, so smokes must provision the same major version the
/// examples are actually built and deployed against.
const POSTGRES_TAG: &str = "16-alpine";

// ── Postgres provisioning ───────────────────────────────────────────────────

/// One or more independent, ephemeral Postgres databases provisioned for a
/// single example's smoke test. Dropping this stops every container.
///
/// Callers pass the appropriate [`urls`](Self::urls) into the spawned
/// example's config via env vars (e.g. `AUTUMN_DATABASE__PRIMARY_URL`,
/// `AUTUMN_DATABASE__SHARDS__0__PRIMARY_URL`) — the exact wiring is
/// example-specific, so this stays a plain list rather than baking in any
/// one example's topology.
pub struct PgTopology {
    // Held only for its `Drop` impl, which tears the container down.
    _containers: Vec<ContainerAsync<Postgres>>,
    urls: Vec<String>,
}

impl PgTopology {
    /// The connection URL(s), in provisioning order.
    #[must_use]
    pub fn urls(&self) -> &[String] {
        &self.urls
    }
}

/// Start `count` independent Postgres containers and return their
/// connection URLs (`postgres://postgres:postgres@<host>:<port>/postgres`).
///
/// Containers are started concurrently (they're fully independent — no data
/// dependency between them), so provisioning latency is roughly the slowest
/// single container rather than the sum of all `count`. The returned
/// [`PgTopology::urls`] preserve the same order as `0..count` regardless of
/// which container actually finishes starting first — `join_all` resolves
/// futures out of order but always returns results in input order.
///
/// The connection host is resolved via testcontainers' own `get_host()`
/// (mirroring [`autumn_web::test::TestDb`]) rather than assumed to be
/// `127.0.0.1`, since the Docker-mapped host isn't always the literal
/// loopback address (e.g. Docker Desktop, remote Docker contexts).
///
/// # Panics
/// If Docker is unavailable or a container fails to start. Callers running
/// under the fan-out harness should probe Docker availability first and
/// skip with a visible notice instead of calling this — see
/// `scripts/check-examples-e2e.sh`.
pub async fn provision_postgres(count: usize) -> PgTopology {
    let started = futures::future::join_all((0..count).map(|_| async {
        let container = Postgres::default()
            .with_tag(POSTGRES_TAG)
            .start()
            .await
            .expect("start Postgres testcontainer");
        let host = container
            .get_host()
            .await
            .expect("resolve Postgres testcontainer host");
        let port = container
            .get_host_port_ipv4(5432)
            .await
            .expect("resolve Postgres testcontainer host port");
        let url = format!("postgres://postgres:postgres@{host}:{port}/postgres");
        (container, url)
    }))
    .await;

    let mut containers = Vec::with_capacity(count);
    let mut urls = Vec::with_capacity(count);
    for (container, url) in started {
        containers.push(container);
        urls.push(url);
    }

    PgTopology {
        _containers: containers,
        urls,
    }
}

// ── Example process boot ────────────────────────────────────────────────────

/// Errors booting an example binary under test.
#[derive(Debug, thiserror::Error)]
pub enum SpawnError {
    /// Could not reserve an ephemeral loopback port.
    #[error("failed to reserve an ephemeral port: {0}")]
    Port(#[source] io::Error),
    /// The example binary itself failed to start (e.g. not built, not
    /// executable).
    #[error("failed to spawn example binary at {path}: {source}")]
    Spawn {
        /// Path to the binary that failed to spawn.
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    /// The spawned process never answered `GET /health` within the timeout.
    #[error("example at {base_url} did not report healthy within {timeout:?}")]
    NotReady {
        /// The base URL that was polled.
        base_url: String,
        /// How long we waited.
        timeout: Duration,
    },
    /// The spawned process exited before ever reporting healthy — e.g. a
    /// panic during config/DB setup, before the HTTP listener ever bound.
    /// Detected immediately rather than waiting out the full timeout.
    #[error("example exited with {status} before reporting healthy at {base_url}")]
    ExitedBeforeReady {
        /// The base URL that was being polled when the process exited.
        base_url: String,
        /// The process's exit status.
        status: std::process::ExitStatus,
    },
}

/// A running example binary, spawned as a real OS process bound to an
/// ephemeral loopback port.
///
/// Dropping this terminates the process (`SIGTERM` then a short grace
/// period, then a hard kill on Unix; `kill()` elsewhere) so a panicking or
/// early-returning smoke never leaks the spawned server.
pub struct ExampleProcess {
    // `Option` so `Drop` can synchronously `take()` the child and hand it to
    // a background thread — see the `Drop` impl below.
    child: Option<Child>,
    base_url: String,
}

impl ExampleProcess {
    /// The base URL of the running example, e.g. `http://127.0.0.1:49832`.
    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Attach a managed headless-Chromium [`SystemTest`] runner to this
    /// process's base URL, using the framework's default browser-launch
    /// timeout. Use [`Self::attach_browser_with_timeout`] to override it.
    ///
    /// # Errors
    /// [`SystemTestError::BrowserNotFound`] or CDP launch errors — see
    /// [`SystemTest::attach`].
    pub async fn attach_browser(&self) -> Result<SystemTestRunner, SystemTestError> {
        SystemTest::attach(self.base_url.clone()).await
    }

    /// Like [`Self::attach_browser`], but with an explicit browser-launch
    /// timeout — useful when CI resource contention (several Postgres
    /// testcontainers plus Chromium competing for CPU) makes the default
    /// too tight for a particular example's smoke.
    ///
    /// # Errors
    /// [`SystemTestError::BrowserNotFound`] or CDP launch errors — see
    /// [`SystemTest::attach_with_timeout`].
    pub async fn attach_browser_with_timeout(
        &self,
        browser_timeout: Duration,
    ) -> Result<SystemTestRunner, SystemTestError> {
        SystemTest::attach_with_timeout(self.base_url.clone(), browser_timeout).await
    }
}

impl Drop for ExampleProcess {
    fn drop(&mut self) {
        // Deliberately synchronous, unlike `SystemTestRunner`'s Drop: every
        // example smoke is the sole `#[tokio::test]` in its own integration-
        // test binary (see `tests/system/smoke.rs` in each example), so
        // there is no sibling task on the runtime for a blocking sleep here
        // to starve. Backgrounding this onto a detached thread would trade
        // that non-issue for a real one — the test binary's `main` can
        // return and the process exit before a detached thread ever runs,
        // leaking the spawned example server across the e2e fan-out.
        let Some(mut child) = self.child.take() else {
            return;
        };
        #[cfg(unix)]
        {
            // Mirrors the graceful-shutdown sequence the reddit-clone Tauri
            // sidecar uses: SIGTERM first so autumn's on_shutdown hooks run
            // (draining connections, stopping schedulers), then force-kill
            // after a short grace period rather than blocking teardown
            // indefinitely on a hung process.
            let _ = Command::new("kill")
                .args(["-TERM", &child.id().to_string()])
                .status();
            std::thread::sleep(Duration::from_millis(500));
        }
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// Default budget for a spawned example to report healthy.
pub const DEFAULT_READY_TIMEOUT: Duration = Duration::from_secs(30);

/// Spawn `bin_path` (typically `env!("CARGO_BIN_EXE_<name>")`) as the
/// application under test, on a fresh ephemeral loopback port, with `envs`
/// applied on top of the current process's environment. `AUTUMN_ENV` is set
/// to `development` by default (so the example auto-migrates any registered
/// migrations against a fresh testcontainer DB on boot, matching a
/// developer's local `cargo run`) and `AUTUMN_SERVER__PORT`/`__HOST` are
/// always set to the reserved port — both can be overridden via `envs` if a
/// smoke needs to.
///
/// The child's stdout/stderr are inherited (not piped) so example logs
/// appear directly in the test/CI output — piping without draining risks a
/// deadlock if the app logs more than the OS pipe buffer before becoming
/// healthy, which busier examples (jobs, tracing) can do.
///
/// `current_dir` should be the example crate's own root — typically
/// `env!("CARGO_MANIFEST_DIR")` evaluated in the *example's* smoke test
/// (not in this shared crate, where it would resolve to the wrong
/// directory). Config env vars override file-based config, but several
/// examples resolve other paths relative to their crate root at runtime
/// (`autumn.toml` itself, wiki's `content/`, static assets), so the child's
/// working directory must be set explicitly rather than inherited from
/// wherever `cargo test` happens to run.
///
/// Polls `GET {base_url}/health` until it responds successfully or
/// `timeout` elapses.
///
/// # Errors
/// See [`SpawnError`].
pub async fn spawn_example(
    bin_path: impl AsRef<Path>,
    current_dir: impl AsRef<Path>,
    envs: &[(&str, &str)],
    timeout: Duration,
) -> Result<ExampleProcess, SpawnError> {
    let bin_path = bin_path.as_ref();

    // Reserve an ephemeral port: bind :0, read the assigned port, then drop
    // the listener so the spawned process can bind it. There is a brief
    // window between dropping the listener and the child binding the port;
    // in practice (loopback, immediately-following spawn) this race is
    // exceedingly rare — the same trade-off the reddit-clone Tauri sidecar
    // and `SystemTest::build`'s own ephemeral-port binding make.
    let port = {
        let listener = TcpListener::bind("127.0.0.1:0").map_err(SpawnError::Port)?;
        listener.local_addr().map_err(SpawnError::Port)?.port()
    };
    let base_url = format!("http://127.0.0.1:{port}");

    let mut command = Command::new(bin_path);
    command
        .current_dir(current_dir.as_ref())
        .env("AUTUMN_ENV", "development")
        .env("AUTUMN_SERVER__PORT", port.to_string())
        .env("AUTUMN_SERVER__HOST", "127.0.0.1")
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    for (key, value) in envs {
        command.env(key, value);
    }

    let mut child = command.spawn().map_err(|source| SpawnError::Spawn {
        path: bin_path.to_path_buf(),
        source,
    })?;

    if let Err(err) = wait_until_healthy(&base_url, &mut child, timeout).await {
        // `child` is a plain `std::process::Child` here, not yet wrapped in
        // an `ExampleProcess` (whose `Drop` impl does this same
        // kill-then-reap) — without an explicit kill on this early-return
        // path, a `NotReady` timeout would leak a still-running orphaned
        // process. `ExitedBeforeReady` means it's already dead, so `kill()`
        // here is a harmless no-op for that case.
        let _ = child.kill();
        let _ = child.wait();
        return Err(err);
    }

    Ok(ExampleProcess {
        child: Some(child),
        base_url,
    })
}

/// Poll `{base_url}/health` until it returns a successful status.
///
/// Also checks `child`'s exit status on every iteration so a process that
/// dies early (e.g. panics during config/DB setup before ever binding its
/// listener) is detected and reported immediately, instead of polling a
/// dead process for the entire `timeout`.
async fn wait_until_healthy(
    base_url: &str,
    child: &mut Child,
    timeout: Duration,
) -> Result<(), SpawnError> {
    let deadline = tokio::time::Instant::now() + timeout;
    let url = format!("{base_url}/health");
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            return Err(SpawnError::ExitedBeforeReady {
                base_url: base_url.to_string(),
                status,
            });
        }
        if let Ok(Ok(response)) =
            tokio::time::timeout(Duration::from_secs(1), reqwest::get(&url)).await
            && response.status().is_success()
        {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(SpawnError::NotReady {
                base_url: base_url.to_string(),
                timeout,
            });
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}
