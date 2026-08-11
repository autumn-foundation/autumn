//! First-class system tests with a headless Chromium browser.
//!
//! This module provides [`SystemTest`] — a builder that boots your Autumn
//! application on an ephemeral TCP port, launches a managed headless Chromium
//! instance, and gives you a [`Page`] with htmx-aware auto-waiting assertions.
//!
//! # Feature flag
//!
//! Gated behind `autumn-web = { features = ["system-tests"] }`.  Add it as a
//! **dev-dependency** only:
//!
//! ```toml
//! [dev-dependencies]
//! autumn-web = { version = "0.4", features = ["system-tests"] }
//! ```
//!
//! # Quick start
//!
//! ```rust,no_run
//! use autumn_web::prelude::*;
//! use autumn_web::system_test::SystemTest;
//!
//! #[get("/")]
//! async fn index() -> &'static str {
//!     "<html><body><h1>Hello</h1></body></html>"
//! }
//!
//! #[tokio::test]
//! #[ignore = "requires Chromium"]
//! async fn hello_renders() {
//!     let runner = SystemTest::new()
//!         .routes(routes![index])
//!         .build()
//!         .await
//!         .expect("start runner");
//!
//!     let page = runner.page().await.expect("open page");
//!     page.visit("/").await.expect("visit");
//!     page.expect_text("Hello").await.expect("text");
//! }
//! ```
//!
//! # Custom middleware
//!
//! Routes that depend on app-wide middleware (tenant scoping, an auth shim,
//! request enrichment) can register it with [`SystemTest::layer`], which
//! takes the same layers as
//! [`AppBuilder::layer`](crate::app::AppBuilder::layer) and puts them in the
//! same position in the stack:
//!
//! ```rust,no_run
//! # use autumn_web::system_test::SystemTest;
//! # use autumn_web::prelude::*;
//! # #[get("/")] async fn index() -> &'static str { "" }
//! # async fn scope_to_tenant(
//! #     req: axum::extract::Request,
//! #     next: axum::middleware::Next,
//! # ) -> axum::response::Response { next.run(req).await }
//! # async fn example() {
//! let runner = SystemTest::new()
//!     .routes(routes![index])
//!     .layer(axum::middleware::from_fn(scope_to_tenant))
//!     .build()
//!     .await
//!     .expect("start runner");
//! # }
//! ```
//!
//! # Browser resolution order
//!
//! 1. `AUTUMN_CHROMIUM` environment variable (full binary path)
//! 2. `PLAYWRIGHT_BROWSERS_PATH` — scans `<path>/chromium-*/chrome-linux/chrome`
//! 3. `PATH`-based lookup (`chrome`, `google-chrome`, `chromium`, …)
//! 4. Well-known per-platform install locations — `/usr/bin/chromium-browser`,
//!    `/usr/bin/google-chrome`, … on Linux; `/Applications/Google
//!    Chrome.app/…` on macOS; `%ProgramFiles%` / `%LOCALAPPDATA%`
//!    `…\Application\chrome.exe` on Windows
//!
//! Resolution lives in [`crate::browser_detect`], shared with `autumn doctor`
//! so the two can never disagree about whether this host can run system
//! tests. On POSIX each candidate is confirmed with a `--version` probe run
//! against its own throwaway `--user-data-dir`; on Windows the candidate is
//! accepted on file evidence alone, because `chrome.exe` is a GUI-subsystem
//! binary that cannot answer over the parent console and executing it would
//! start a real browser (#1456). `autumn doctor` then reports `version
//! unavailable` rather than claiming no browser is installed.
//!
//! When the browser cannot be found the error message prints all searched
//! paths and the `apt-get install chromium-browser` remediation hint.
//!
//! # Failure artifacts
//!
//! On any assertion failure, autumn writes a `.png` screenshot and `.html`
//! dump to `target/system-tests/<test-name>/` so you can post-mortem the
//! failure without rerunning the test.
//!
//! # htmx settle detection
//!
//! All page-mutating helpers (`click`, form submits) auto-wait for htmx to
//! finish its settle phase before returning.  This is implemented by polling
//! until `.htmx-request`, `.htmx-settling`, and `.htmx-swapping` are all
//! absent, with a configurable timeout (default 2 s).  Use
//! [`Page::expect_hx_settle`] when you need an explicit fence.

#![cfg(feature = "system-tests")]

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chromiumoxide::cdp::browser_protocol::log::{EventEntryAdded, LogEntryLevel};
use chromiumoxide::cdp::js_protocol::runtime::{
    ConsoleApiCalledType, EventConsoleApiCalled, EventExceptionThrown,
};
use chromiumoxide::{Browser, BrowserConfig};
use futures::StreamExt as _;

use crate::config::AutumnConfig;
use crate::route::Route;

// ── Constants ──────────────────────────────────────────────────────────────

/// Default timeout while waiting for the browser binary to launch and connect.
const DEFAULT_BROWSER_TIMEOUT: Duration = Duration::from_secs(30);

/// Default timeout for htmx settle auto-wait after every mutating action.
const DEFAULT_HX_SETTLE_TIMEOUT: Duration = Duration::from_secs(2);

/// Default assertion polling interval.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// How long auto-waiting assertions (`expect_text`, `expect_url`,
/// `expect_attribute`) poll before giving up.
const ASSERTION_TIMEOUT: Duration = Duration::from_secs(5);

/// Grace period `expect_no_console_errors` polls for any in-flight CDP
/// console/exception events to be delivered before declaring the page
/// clean. Generous relative to typical sub-10ms local CDP round-trips so it
/// tolerates CI resource contention (Docker + Chromium + concurrent builds);
/// an error observed at any point during the window still fails immediately
/// rather than waiting out the rest of it.
const CONSOLE_ERROR_GRACE: Duration = Duration::from_millis(500);

// ── Browser detection ──────────────────────────────────────────────────────

// Browser discovery and the `--version` probe live in `crate::browser_detect`
// so `autumn doctor` (which must not depend on chromiumoxide) runs exactly
// the same resolution logic. Re-exported here because
// `autumn_web::system_test::BrowserCheck` is the documented path.
pub use crate::browser_detect::BrowserCheck;
use crate::browser_detect::{browser_candidates, find_chromium};

// ── SystemTestError ────────────────────────────────────────────────────────

/// Errors produced by the system-test harness.
#[derive(Debug, thiserror::Error)]
pub enum SystemTestError {
    /// Chromium binary could not be located on this host.
    #[error(
        "Chromium browser not found. Searched:\n{}\n\n\
         To install: apt-get install chromium-browser\n\
         Or set AUTUMN_CHROMIUM=/path/to/chrome",
        searched.iter().map(|p| format!("  {}", p.display())).collect::<Vec<_>>().join("\n")
    )]
    BrowserNotFound {
        /// Paths that were checked.
        searched: Vec<PathBuf>,
    },

    /// An assertion on the page content or URL failed.
    #[error("{message}")]
    AssertionFailed {
        /// Human-readable description of what was expected vs. found.
        message: String,
        /// Path prefix for the `.png` and `.html` artifacts (if they were
        /// written successfully).
        artifact_path: Option<String>,
    },

    /// The assertion did not resolve within the allowed time.
    #[error("assertion timed out after {timeout:?}: {message}")]
    Timeout {
        /// Human-readable description of the assertion.
        message: String,
        /// How long we waited.
        timeout: Duration,
    },

    /// An I/O error while writing failure artifacts.
    #[error("artifact write error: {0}")]
    ArtifactIo(#[from] std::io::Error),

    /// An error from the underlying chromiumoxide browser library.
    #[error("browser error: {0}")]
    Browser(#[from] chromiumoxide::error::CdpError),
}

/// Substrings Chrome uses to report "whatever you were evaluating against no
/// longer exists because the page moved".
///
/// Deliberately an explicit list rather than a blanket "any `Chrome` error is
/// transient": treating a genuine failure (`"No node with given id found"`)
/// as transient would replace a fast, precise error with a silent five-second
/// timeout and a worse message.
/// Compared case-insensitively against the CDP error message.
///
/// Notably **absent**: `"Target closed"` and `"Session with given id not
/// found"`. Chrome emits those for a renderer that died (a sad tab under CI
/// memory pressure) just as readily as for a navigation, and
/// [`Page::wait_for_hx_settle`] maps a transient error to *settled* — so
/// swallowing them would turn a crashed page into a five-second
/// `expect_text` timeout with no screenshot, which is exactly the failure
/// mode this list exists to avoid.
///
/// [`Page::wait_for_hx_settle`]: Page::wait_for_hx_settle
const TRANSIENT_NAVIGATION_MARKERS: &[&str] = &[
    // The error reported in #1456.
    "cannot find context with specified id",
    // Same race, reported against the context rather than the lookup.
    "execution context was destroyed",
    // A navigation that swapped the inspected target out from under the
    // in-flight command — no "context" in the text at all.
    "inspected target navigated or closed",
];

/// True for CDP errors that just mean "the page navigated away mid-poll" —
/// e.g. a plain (non-htmx) form submit's full-page redirect racing an
/// in-flight `evaluate()` call, which briefly leaves no valid JS execution
/// context to evaluate against. Transient by construction, not a real
/// failure: assertion polling loops treat these as "not ready yet" and keep
/// polling rather than aborting on the first unlucky tick.
fn is_transient_navigation_error(err: &chromiumoxide::error::CdpError) -> bool {
    // `Chrome` carries a typed CDP error object; `ChromeMessage` is the same
    // class of failure for responses that did not deserialise into one, so
    // both must be inspected or the error leaks through under the other name.
    let message = match err {
        chromiumoxide::error::CdpError::Chrome(inner) => inner.message.as_str(),
        chromiumoxide::error::CdpError::ChromeMessage(message) => message.as_str(),
        _ => return false,
    };
    let message = message.to_ascii_lowercase();
    TRANSIENT_NAVIGATION_MARKERS
        .iter()
        .any(|marker| message.contains(marker))
}

/// Poll `probe` every [`POLL_INTERVAL`] until it reports success or
/// `deadline` passes.
///
/// This is the retry loop every auto-waiting assertion shares, and the reason
/// it exists as a named function: a transient CDP error (see
/// [`is_transient_navigation_error`]) must be indistinguishable from "not
/// true yet" — the page merely navigated out from under an in-flight
/// `evaluate()` — while a genuine CDP error must abort immediately rather
/// than decay into a timeout with a worse message.
///
/// `transient_means` is what a transient error should be read as. Assertions
/// pass `false` ("not true yet — keep polling on the page we land on");
/// [`Page::wait_for_hx_settle`] passes `true`, because a page that navigated
/// away has no htmx activity left to wait for.
///
/// Returns `Ok(true)` on success, `Ok(false)` if the deadline passed without
/// one (the caller owns the failure message and artifacts), and `Err` for a
/// non-transient CDP error.
///
/// [`Page::wait_for_hx_settle`]: Page::wait_for_hx_settle
async fn poll_until_deadline<F, Fut>(
    deadline: tokio::time::Instant,
    transient_means: bool,
    mut probe: F,
) -> Result<bool, SystemTestError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<bool, chromiumoxide::error::CdpError>>,
{
    loop {
        match probe().await {
            Ok(true) => return Ok(true),
            Ok(false) => {}
            Err(e) if is_transient_navigation_error(&e) => {
                if transient_means {
                    return Ok(true);
                }
            }
            Err(e) => return Err(e.into()),
        }

        if tokio::time::Instant::now() >= deadline {
            return Ok(false);
        }

        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

// ── Artifact directory ─────────────────────────────────────────────────────

/// Return the canonical artifact directory for a given test name.
///
/// Output: `target/system-tests/<test_name>/`
#[must_use]
pub fn artifact_dir(test_name: &str) -> PathBuf {
    // Walk up from the crate root (or use CARGO_TARGET_DIR if set).
    let base =
        std::env::var("CARGO_TARGET_DIR").map_or_else(|_| PathBuf::from("target"), PathBuf::from);
    base.join("system-tests").join(test_name)
}

// ── SystemTest builder ─────────────────────────────────────────────────────

/// Builder for a system test run.
///
/// Boots an Autumn application on an ephemeral port, launches a managed
/// headless Chromium, and returns a [`SystemTestRunner`].
///
/// # Example
///
/// ```rust,no_run
/// # use autumn_web::system_test::SystemTest;
/// # use autumn_web::prelude::*;
/// # #[get("/")] async fn index() -> &'static str { "" }
/// # async fn example() {
/// let runner = SystemTest::new()
///     .routes(routes![index])
///     .build()
///     .await
///     .expect("start runner");
/// # }
/// ```
#[must_use]
pub struct SystemTest {
    routes: Vec<Route>,
    /// Config for the default-state path; ignored when the caller supplies
    /// their own `AppState` via [`SystemTest::state`], whose embedded config
    /// wins so handlers and middleware agree on one set of settings.
    config: AutumnConfig,
    artifact_dir_override: Option<PathBuf>,
    browser_timeout: Duration,
    hx_settle_timeout: Duration,
    /// Optional pre-configured state; overrides the default `AppState::for_test()`.
    state_override: Option<crate::state::AppState>,
    /// App-wide Tower layers registered via [`SystemTest::layer`], applied in
    /// the same router position as [`crate::app::AppBuilder::layer`].
    custom_layers: Vec<crate::app::CustomLayerRegistration>,
}

impl Default for SystemTest {
    fn default() -> Self {
        Self::new()
    }
}

impl SystemTest {
    /// Create a new builder with default configuration.
    pub fn new() -> Self {
        let mut security = crate::security::SecurityConfig::default();
        security.csrf.enabled = false;
        let config = AutumnConfig {
            profile: Some("test".into()),
            security,
            ..Default::default()
        };

        Self {
            routes: Vec::new(),
            config,
            artifact_dir_override: None,
            browser_timeout: DEFAULT_BROWSER_TIMEOUT,
            hx_settle_timeout: DEFAULT_HX_SETTLE_TIMEOUT,
            state_override: None,
            custom_layers: Vec::new(),
        }
    }

    /// Register routes to serve.  May be called multiple times; each call
    /// appends to the route list rather than replacing it.
    pub fn routes(mut self, routes: impl Into<Vec<Route>>) -> Self {
        self.routes.extend(routes.into());
        self
    }

    /// Supply a pre-configured [`AppState`] to use instead of
    /// [`AppState::for_test()`].
    ///
    /// Use this when the routes under test require a real database pool, API
    /// version registrations, authorization policies, or any other state that
    /// is set up by your `AppBuilder`.  The system test will use this state
    /// as-is without further modification.
    ///
    /// [`AppState`]: crate::state::AppState
    pub fn state(mut self, state: crate::state::AppState) -> Self {
        self.state_override = Some(state);
        self
    }

    /// Register an app-wide [`tower::Layer`] on the router the system test
    /// serves — the harness equivalent of
    /// [`AppBuilder::layer`](crate::app::AppBuilder::layer).
    ///
    /// Without this, routes that depend on global middleware (tenant
    /// scoping, an auth shim, request enrichment) cannot be exercised
    /// end-to-end: the only workaround is hand-mapping each layer onto
    /// individual handlers before passing them to [`routes`](Self::routes),
    /// which tests a stack the real app never serves.
    ///
    /// The layer is applied in the **same router position** as
    /// `AppBuilder::layer` — inside Autumn's request-ID and session layers,
    /// outside CSRF/CORS — so middleware that reads the request ID or session
    /// finds them here exactly as it does in production. (The CSRF layer is
    /// only present when you supply a config via [`state`](Self::state) that
    /// enables it; [`SystemTest::new`] disables CSRF by default.)
    ///
    /// # Ordering
    ///
    /// Also matching `AppBuilder::layer`: when called multiple times the
    /// **first** call is the outermost layer on ingress (it sees the request
    /// first and the response last).
    ///
    /// # Bounds
    ///
    /// [`IntoAppLayer`](crate::app::IntoAppLayer) is the same sealed trait
    /// `AppBuilder::layer` uses: any `tower::Layer` whose service satisfies
    /// axum's requirements (`Error = Infallible`). Layers with real errors
    /// (e.g. `TimeoutLayer`'s `BoxError`) must be wrapped with
    /// [`axum::error_handling::HandleErrorLayer`] first.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// # use autumn_web::system_test::SystemTest;
    /// # use autumn_web::prelude::*;
    /// # #[get("/")] async fn index() -> &'static str { "" }
    /// # async fn scope_to_tenant(
    /// #     req: axum::extract::Request,
    /// #     next: axum::middleware::Next,
    /// # ) -> axum::response::Response { next.run(req).await }
    /// # async fn example() {
    /// let runner = SystemTest::new()
    ///     .routes(routes![index])
    ///     .layer(axum::middleware::from_fn(scope_to_tenant))
    ///     .build()
    ///     .await
    ///     .expect("start runner");
    /// # }
    /// ```
    pub fn layer<L: crate::app::IntoAppLayer>(mut self, layer: L) -> Self {
        self.custom_layers
            .push(crate::app::CustomLayerRegistration {
                type_id: std::any::TypeId::of::<L>(),
                type_name: std::any::type_name::<L>(),
                apply: Box::new(move |router| layer.apply_to(router)),
            });
        self
    }

    /// Override the directory where failure artifacts are written.
    ///
    /// Defaults to `target/system-tests/<test_name>/`.
    pub fn artifact_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.artifact_dir_override = Some(dir.into());
        self
    }

    /// Override how long to wait for the browser to launch.
    pub const fn browser_timeout(mut self, t: Duration) -> Self {
        self.browser_timeout = t;
        self
    }

    /// Override how long to wait for htmx to finish settling after each action.
    pub const fn hx_settle_timeout(mut self, t: Duration) -> Self {
        self.hx_settle_timeout = t;
        self
    }

    /// Boot the server and launch the browser, returning a [`SystemTestRunner`].
    ///
    /// # Errors
    /// - [`SystemTestError::BrowserNotFound`] when no Chromium binary is available.
    /// - [`SystemTestError::Browser`] for CDP launch/connect errors.
    pub async fn build(self) -> Result<SystemTestRunner, SystemTestError> {
        // 1. Bind the app to an ephemeral port.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(SystemTestError::ArtifactIo)?;
        let addr = listener.local_addr().map_err(SystemTestError::ArtifactIo)?;
        let base_url = format!("http://127.0.0.1:{}", addr.port());

        // Read the settings the runner (not the router) needs before
        // `into_router` consumes the builder.
        let browser_timeout = self.browser_timeout;
        let hx_settle_timeout = self.hx_settle_timeout;
        let artifact_dir = self
            .artifact_dir_override
            .clone()
            .unwrap_or_else(default_artifact_dir);

        // 2. Build the axum router from the registered routes.
        let router = self.into_router();
        let service = tower::Layer::layer(&crate::middleware::MethodOverrideLayer::new(), router);
        let make_service = axum::ServiceExt::<axum::extract::Request>::into_make_service(service);

        // 3. Spawn the server in a background task.
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let server_handle = tokio::spawn(async move {
            let _ = axum::serve(listener, make_service)
                .with_graceful_shutdown(async move {
                    let _ = shutdown_rx.await;
                })
                .await;
        });

        // 4. Launch Chromium.
        let (browser, user_data_dir) = launch_browser(browser_timeout).await?;

        Ok(SystemTestRunner {
            base_url,
            browser: Some(browser),
            artifact_dir,
            user_data_dir,
            hx_settle_timeout,
            _shutdown: Some(shutdown_tx),
            _server_handle: Some(server_handle),
        })
    }

    /// Assemble the axum router this builder will serve, without binding a
    /// port or launching a browser.
    ///
    /// Split out of [`build`](Self::build) so router assembly — route
    /// registration, state selection, and custom-layer application — is
    /// exercisable without a Chromium binary.
    fn into_router(self) -> axum::Router {
        // Both arms only decide *which* (config, state) pair to build from;
        // the single build call below is deliberately shared so a future edit
        // cannot wire `custom_layers` into one path and forget the other.
        let (config, state) = if let Some(state) = self.state_override {
            // Use the config already embedded in the caller-supplied state so
            // that middleware (tenancy, auth, rate-limiting, CSRF) is built
            // from the same settings handlers observe via AppState::config().
            // Headless Chromium handles cookies normally, so CSRF works
            // end-to-end: the browser receives the CSRF cookie on first visit
            // and replays it on form submissions, exactly as a real user does.
            let config = state
                .extension::<AutumnConfig>()
                .map(|arc| (*arc).clone())
                .unwrap_or_default();
            (config, state)
        } else {
            // The builder's own config: `test` profile with CSRF disabled, so
            // a fixture form does not have to thread a token through its HTML.
            let state = crate::state::AppState::for_test().with_profile("test");
            state.insert_extension(self.config.clone());
            (self.config, state)
        };

        crate::router::try_build_router_with_layers(self.routes, &config, state, self.custom_layers)
            .unwrap_or_else(|error| panic!("invalid router configuration: {error}"))
    }

    /// Launch a managed headless Chromium and attach it to an **already
    /// running** application at `base_url`, without booting any in-process
    /// server.
    ///
    /// Use this when the application under test is a separately-spawned
    /// process (e.g. an example's real binary, booted against its own
    /// database and real config) rather than a set of routes registered
    /// in-process via [`routes`](Self::routes). All [`Page`] assertions
    /// (including [`Page::expect_no_console_errors`]) work identically to
    /// the in-process [`build`](Self::build) path.
    ///
    /// Uses [`DEFAULT_BROWSER_TIMEOUT`]; use [`Self::attach_with_timeout`] to
    /// override it (e.g. under CI resource contention).
    ///
    /// # Errors
    /// - [`SystemTestError::BrowserNotFound`] when no Chromium binary is available.
    /// - [`SystemTestError::Browser`] for CDP launch/connect errors.
    pub async fn attach(base_url: impl Into<String>) -> Result<SystemTestRunner, SystemTestError> {
        Self::attach_with_timeout(base_url, DEFAULT_BROWSER_TIMEOUT).await
    }

    /// Like [`Self::attach`], but with an explicit browser-launch timeout
    /// instead of [`DEFAULT_BROWSER_TIMEOUT`] — useful when the target
    /// environment (e.g. several Postgres testcontainers and Chromium
    /// competing for CPU in CI) makes the default too tight.
    ///
    /// # Errors
    /// - [`SystemTestError::BrowserNotFound`] when no Chromium binary is available.
    /// - [`SystemTestError::Browser`] for CDP launch/connect errors.
    pub async fn attach_with_timeout(
        base_url: impl Into<String>,
        browser_timeout: Duration,
    ) -> Result<SystemTestRunner, SystemTestError> {
        let (browser, user_data_dir) = launch_browser(browser_timeout).await?;
        Ok(SystemTestRunner {
            base_url: base_url.into(),
            browser: Some(browser),
            artifact_dir: default_artifact_dir(),
            user_data_dir,
            hx_settle_timeout: DEFAULT_HX_SETTLE_TIMEOUT,
            _shutdown: None,
            _server_handle: None,
        })
    }
}

/// Locate and launch a managed headless Chromium, driving its CDP event loop
/// in a background task. Shared by [`SystemTest::build`] and
/// [`SystemTest::attach`]. Returns the browser and the profile directory it
/// was launched with, so the caller can remove it once the browser closes.
async fn launch_browser(browser_timeout: Duration) -> Result<(Browser, PathBuf), SystemTestError> {
    let browser_path = find_chromium().ok_or_else(|| {
        let searched = browser_candidates();
        SystemTestError::BrowserNotFound { searched }
    })?;

    let user_data_dir = unique_user_data_dir();

    let config = BrowserConfig::builder()
        .chrome_executable(browser_path)
        // chromiumoxide defaults every browser to the SAME fixed
        // `<tmp>/chromiumoxide-runner` profile directory. Two browsers alive
        // at once (e.g. two SystemTest runners in the same process, or an
        // `attach()` runner alongside the `build()` server it targets) then
        // collide on Chrome's profile singleton lock and one launch fails.
        // Give every launch its own directory.
        .user_data_dir(&user_data_dir)
        // `chromiumoxide::browser::argument::Arg`'s `From<&str>` treats the
        // whole string as the flag *name* and prepends its own `--`, so a
        // literal `"--no-sandbox"` here renders as the four-dash garbage flag
        // `----no-sandbox` that Chrome silently ignores. Use the dedicated
        // builder method for the sandbox flag and bare (no-dash) flag names
        // for the rest; headless mode is already the builder default.
        .no_sandbox()
        .arg("disable-dev-shm-usage")
        .arg("disable-gpu")
        // Forward the configured timeout into chromiumoxide's own launch
        // watchdog so the inner and outer timeouts are consistent and the
        // outer tokio::time::timeout always wins.
        .launch_timeout(browser_timeout)
        .build()
        .map_err(|msg| SystemTestError::Browser(chromiumoxide::error::CdpError::msg(msg)))?;

    let (browser, handler) = tokio::time::timeout(browser_timeout, Browser::launch(config))
        .await
        .map_err(|_| SystemTestError::Timeout {
            message: "timed out waiting for Chromium to launch".into(),
            timeout: browser_timeout,
        })??;

    // Drive the CDP event loop in a background task.
    tokio::spawn(async move {
        handler.for_each(|_| async {}).await;
    });

    Ok((browser, user_data_dir))
}

/// A fresh, never-reused Chrome profile directory for one browser launch.
fn unique_user_data_dir() -> PathBuf {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    std::env::temp_dir().join(format!("chromiumoxide-runner-{}-{n}", std::process::id()))
}

/// Default artifact directory: the Rust test thread name (set by the test
/// harness) as the subdirectory, so concurrent tests don't overwrite each
/// other's screenshots and HTML dumps.
fn default_artifact_dir() -> PathBuf {
    let name = std::thread::current()
        .name()
        .unwrap_or("system_test")
        .replace("::", "__");
    artifact_dir(&name)
}

// ── SystemTestRunner ───────────────────────────────────────────────────────

/// A running system-test session.
///
/// Returned by [`SystemTest::build`] or [`SystemTest::attach`]. When it owns
/// an in-process server (the `build` path), that server shuts down when the
/// runner is dropped; the `attach` path owns only the browser and leaves the
/// externally-managed application process untouched. Either way, dropping
/// the runner also removes its per-launch Chrome profile directory.
pub struct SystemTestRunner {
    base_url: String,
    // `Option` so `Drop` can synchronously extract and drop the browser
    // *before* removing its profile directory — see the `Drop` impl below.
    browser: Option<Browser>,
    artifact_dir: PathBuf,
    user_data_dir: PathBuf,
    hx_settle_timeout: Duration,
    _shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    _server_handle: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for SystemTestRunner {
    fn drop(&mut self) {
        // chromiumoxide sets `kill_on_drop(true)` on the underlying child
        // process, so dropping `Browser` synchronously *sends* the kill
        // signal (only the OS reaping the process afterward is
        // asynchronous/unguaranteed-timing). Rust drops a struct's own
        // fields only *after* a custom `Drop::drop` body returns, so without
        // this explicit `take()` the profile directory would be removed
        // before the browser (and thus the signal) is even dropped.
        // Dropping the taken `Browser` here, before touching the directory,
        // ensures the kill is at least sent first.
        drop(self.browser.take());

        // Best-effort cleanup for the per-launch profile directory created
        // in `launch_browser`/`unique_user_data_dir`. The kill signal above
        // is sent synchronously, but the OS reaping the process (and
        // releasing any file handles it held open under this directory) is
        // not instantaneous or guaranteed-ordered from Rust's perspective;
        // a short grace period trades a little latency for meaningfully
        // higher odds the removal actually succeeds. Errors (including
        // "still in use") are ignored — this is best-effort, not a
        // correctness guarantee. Since `SystemTestRunner` is typically
        // dropped at the end of an async test, the sleep and removal run on
        // a background thread rather than blocking a Tokio worker thread.
        let user_data_dir = self.user_data_dir.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            let _ = std::fs::remove_dir_all(user_data_dir);
        });
    }
}

impl SystemTestRunner {
    /// Open a new browser page connected to the running application.
    ///
    /// Console errors and uncaught exceptions are captured from page-load
    /// onward so [`Page::expect_no_console_errors`] can assert on them.
    ///
    /// # Errors
    /// Propagates CDP errors from `chromiumoxide`.
    ///
    /// # Panics
    /// Only if called while the runner is being dropped — not reachable
    /// through normal use, since `page()` takes `&self` and a caller can't
    /// hold that reference once the runner itself has started dropping.
    pub async fn page(&self) -> Result<Page, SystemTestError> {
        let browser = self
            .browser
            .as_ref()
            .expect("SystemTestRunner::browser is only None during Drop");
        let cdp_page = browser.new_page("about:blank").await?;
        cdp_page.enable_runtime().await?;
        cdp_page.enable_log().await?;

        let console_errors: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        spawn_console_error_capture(&cdp_page, Arc::clone(&console_errors)).await?;

        Ok(Page {
            inner: cdp_page,
            base_url: self.base_url.clone(),
            artifact_dir: self.artifact_dir.clone(),
            hx_settle_timeout: self.hx_settle_timeout,
            console_errors,
        })
    }

    /// The base URL of the application under test, e.g. `http://127.0.0.1:49832`.
    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }
}

/// Subscribe to `Runtime.consoleAPICalled` (level `error`),
/// `Runtime.exceptionThrown`, and `Log.entryAdded` (level `error`) CDP
/// events on `page` and append formatted messages to `sink` as they arrive,
/// for the lifetime of the page.
///
/// `Log.entryAdded` is a separate CDP domain from `Runtime` and is where
/// Chrome reports browser-level errors — failed resource loads (e.g. a
/// missing `/static` asset), CSP violations, mixed-content warnings — none
/// of which fire a `Runtime.consoleAPICalled` or `exceptionThrown` event.
async fn spawn_console_error_capture(
    page: &chromiumoxide::page::Page,
    sink: Arc<Mutex<Vec<String>>>,
) -> Result<(), SystemTestError> {
    let mut console_events = page.event_listener::<EventConsoleApiCalled>().await?;
    let mut exception_events = page.event_listener::<EventExceptionThrown>().await?;
    let mut log_events = page.event_listener::<EventEntryAdded>().await?;

    tokio::spawn(async move {
        // Track each stream's liveness independently: a `select!` branch
        // guarded by `if <flag>` is skipped once that flag is false, so one
        // stream ending (e.g. a transient CDP session hiccup) only stops
        // polling *that* stream — it must not end capture on the others.
        // The task only exits once all streams are done.
        let mut console_open = true;
        let mut exception_open = true;
        let mut log_open = true;
        while console_open || exception_open || log_open {
            tokio::select! {
                event = console_events.next(), if console_open => {
                    match event {
                        Some(event) => {
                            if event.r#type == ConsoleApiCalledType::Error {
                                let text = event
                                    .args
                                    .iter()
                                    .map(remote_object_to_string)
                                    .collect::<Vec<_>>()
                                    .join(" ");
                                sink.lock().unwrap().push(text);
                            }
                        }
                        None => console_open = false,
                    }
                }
                event = exception_events.next(), if exception_open => {
                    match event {
                        Some(event) => {
                            sink.lock().unwrap().push(event.exception_details.text.clone());
                        }
                        None => exception_open = false,
                    }
                }
                event = log_events.next(), if log_open => {
                    match event {
                        Some(event) => {
                            if event.entry.level == LogEntryLevel::Error {
                                sink.lock().unwrap().push(event.entry.text.clone());
                            }
                        }
                        None => log_open = false,
                    }
                }
            }
        }
    });

    Ok(())
}

/// Render a CDP `RemoteObject` console-call argument as a human-readable
/// string, preferring the raw string value over its JSON-quoted form.
fn remote_object_to_string(obj: &chromiumoxide::cdp::js_protocol::runtime::RemoteObject) -> String {
    if let Some(value) = &obj.value {
        if let Some(s) = value.as_str() {
            return s.to_string();
        }
        return value.to_string();
    }
    obj.description
        .clone()
        .or_else(|| obj.class_name.clone())
        .unwrap_or_default()
}

// ── Page ───────────────────────────────────────────────────────────────────

/// Browser page with htmx-aware assertions.
///
/// Obtained from [`SystemTestRunner::page`].
pub struct Page {
    inner: chromiumoxide::page::Page,
    base_url: String,
    artifact_dir: PathBuf,
    hx_settle_timeout: Duration,
    console_errors: Arc<Mutex<Vec<String>>>,
}

impl Page {
    // ── Navigation ────────────────────────────────────────────────────────

    /// Navigate to `path` (relative to the app root, e.g. `"/todos"`).
    ///
    /// # Errors
    /// CDP navigation errors.
    pub async fn visit(&self, path: &str) -> Result<&Self, SystemTestError> {
        let url = format!("{}{path}", self.base_url);
        self.inner.goto(url).await?;
        Ok(self)
    }

    // ── Interaction ───────────────────────────────────────────────────────

    /// Fill a form field identified by `selector` with `value`.
    ///
    /// Clears any existing content then types the new value character by
    /// character.  Auto-waits for htmx settle after the field change.
    ///
    /// # Errors
    /// CDP or assertion errors.
    pub async fn fill(&self, selector: &str, value: &str) -> Result<&Self, SystemTestError> {
        let element = self.inner.find_element(selector).await?;
        element.click().await?;
        // Clear via JS without dispatching any DOM events. type_str() below
        // fires native key events that trigger 'input' handlers naturally for
        // each character, so the app never sees a transient empty-value event
        // that could race with or overwrite the intended filled-value request.
        self.inner
            .evaluate(format!(
                "(function() {{ var el = document.querySelector({}); \
                 if (el) {{ el.value = ''; }} }})()",
                js_string_literal(selector)
            ))
            .await?;
        element.type_str(value).await?;
        // When value is empty, type_str() emits no key/input events, so
        // hx-trigger="input" handlers would never see the cleared state.
        // Dispatch an explicit input event to cover that case.
        if value.is_empty() {
            self.inner
                .evaluate(format!(
                    "(function() {{ var el = document.querySelector({}); \
                     if (el) {{ el.dispatchEvent(new Event('input', {{ bubbles: true }})); }} }})()",
                    js_string_literal(selector)
                ))
                .await?;
        }
        // Dispatch a final change event so `hx-trigger="change"` and validation
        // listeners see the fully typed value.
        self.inner
            .evaluate(format!(
                "(function() {{ var el = document.querySelector({}); \
                 if (el) {{ el.dispatchEvent(new Event('change', {{ bubbles: true }})); }} }})()",
                js_string_literal(selector)
            ))
            .await?;
        self.wait_for_hx_settle().await?;
        Ok(self)
    }

    /// Click the element identified by `selector_or_label`.
    ///
    /// Supports CSS selectors (e.g. `"button[type=submit]"`) or accessible
    /// text labels (e.g. `"Save"` matches `<button>Save</button>`).
    /// After clicking, auto-waits for htmx to settle.
    ///
    /// # Errors
    /// CDP or assertion errors.
    pub async fn click(&self, selector_or_label: &str) -> Result<&Self, SystemTestError> {
        // Try CSS selector first; fall back to JS text match.
        if let Ok(element) = self.inner.find_element(selector_or_label).await {
            element.click().await?;
        } else {
            // Compare normalized text in JS to avoid XPath string-literal
            // quoting issues (labels with ', ", or both). Walk interactive
            // elements in DOM order; skip hidden/disabled nodes so a visible
            // control is always preferred over a hidden template duplicate.
            let js = format!(
                "(function() {{ \
                 var want = {}; \
                 var normWant = want.replace(/\\s+/g, ' ').trim(); \
                 var nodes = Array.from(document.querySelectorAll( \
                   'button,a,input[value],label,[role=button],[role=link]')); \
                 for (var i = 0; i < nodes.length; i++) {{ \
                   var el = nodes[i]; \
                   if (el.disabled) {{ continue; }} \
                   if (el.getClientRects().length === 0) {{ continue; }} \
                   var cs = window.getComputedStyle(el); \
                   if (cs.visibility === 'hidden' || parseFloat(cs.opacity) === 0) {{ continue; }} \
                   var text = el.tagName === 'INPUT' \
                     ? (el.value || '') \
                     : (el.textContent || ''); \
                   if (text.replace(/\\s+/g, ' ').trim() === normWant) {{ \
                     el.click(); return true; \
                   }} \
                 }} \
                 return false; \
                 }})()",
                js_string_literal(selector_or_label)
            );
            let clicked: bool = self.inner.evaluate(js).await?.into_value().unwrap_or(false);
            if !clicked {
                return Err(SystemTestError::AssertionFailed {
                    message: format!("element not found by selector or text: {selector_or_label}"),
                    artifact_path: None,
                });
            }
        }
        self.wait_for_hx_settle().await?;
        Ok(self)
    }

    // ── Assertions ────────────────────────────────────────────────────────

    /// Assert that `text` appears somewhere in the visible page content.
    ///
    /// Polls until the text appears or the default assertion timeout (5 s)
    /// elapses.  On failure writes a screenshot + HTML artifact.
    ///
    /// # Errors
    /// [`SystemTestError::Timeout`] or [`SystemTestError::AssertionFailed`].
    pub async fn expect_text(&self, text: &str) -> Result<&Self, SystemTestError> {
        let deadline = tokio::time::Instant::now() + ASSERTION_TIMEOUT;
        let js = format!(
            "document.body && document.body.innerText.includes({})",
            js_string_literal(text)
        );

        if poll_until_deadline(deadline, false, || self.evaluate_bool(js.clone(), false)).await? {
            return Ok(self);
        }

        let artifact = self.write_failure_artifacts("expect_text").await.ok();
        Err(SystemTestError::AssertionFailed {
            message: format!("expected text {text:?} in page body"),
            artifact_path: artifact,
        })
    }

    /// Assert that the current page URL ends with or contains `pattern`.
    ///
    /// # Errors
    /// [`SystemTestError::Timeout`] or [`SystemTestError::AssertionFailed`].
    pub async fn expect_url(&self, pattern: &str) -> Result<&Self, SystemTestError> {
        let deadline = tokio::time::Instant::now() + ASSERTION_TIMEOUT;
        let js = format!(
            "window.location.href.includes({})",
            js_string_literal(pattern)
        );

        if poll_until_deadline(deadline, false, || self.evaluate_bool(js.clone(), false)).await? {
            return Ok(self);
        }

        let current_url: String = self
            .inner
            .evaluate("window.location.href")
            .await
            .ok()
            .and_then(|v| v.into_value::<String>().ok())
            .unwrap_or_else(|| "<unknown>".into());
        let artifact = self.write_failure_artifacts("expect_url").await.ok();
        Err(SystemTestError::AssertionFailed {
            message: format!("expected URL to contain {pattern:?}, got {current_url:?}"),
            artifact_path: artifact,
        })
    }

    /// Assert that an element matching `selector` has attribute `attr` equal
    /// to `value`.
    ///
    /// # Errors
    /// [`SystemTestError::AssertionFailed`] on mismatch.
    pub async fn expect_attribute(
        &self,
        selector: &str,
        attr: &str,
        value: &str,
    ) -> Result<&Self, SystemTestError> {
        let deadline = tokio::time::Instant::now() + ASSERTION_TIMEOUT;
        let js = format!(
            "(function() {{ \
               var el = document.querySelector({sel}); \
               return el && el.getAttribute({attr}) === {val}; \
             }})()",
            sel = js_string_literal(selector),
            attr = js_string_literal(attr),
            val = js_string_literal(value),
        );

        if poll_until_deadline(deadline, false, || self.evaluate_bool(js.clone(), false)).await? {
            return Ok(self);
        }

        let artifact = self.write_failure_artifacts("expect_attribute").await.ok();
        Err(SystemTestError::AssertionFailed {
            message: format!("expected [{attr}={value:?}] on {selector:?}"),
            artifact_path: artifact,
        })
    }

    // ── htmx helpers ──────────────────────────────────────────────────────

    /// Explicitly wait for htmx to finish all in-flight requests and settle.
    ///
    /// Polls until `.htmx-request`, `.htmx-settling`, and `.htmx-swapping`
    /// are all absent from the DOM.  Use this as an explicit fence; `click()`
    /// already calls it implicitly.
    ///
    /// **Limitation — delayed triggers:** controls using
    /// `hx-trigger="click delay:500ms"` or active-search debounce have a
    /// window after user interaction where none of these classes exist yet
    /// (htmx has scheduled the request but not yet sent it).  This helper
    /// returns immediately in that window.  Work around it with an explicit
    /// [`Page::evaluate`] call that waits for a specific DOM change, or by
    /// sleeping for at least the configured `delay:` before asserting.
    ///
    /// # Errors
    /// [`SystemTestError::Timeout`] if htmx does not settle within the
    /// configured [`SystemTest::hx_settle_timeout`].
    pub async fn expect_hx_settle(&self) -> Result<&Self, SystemTestError> {
        self.wait_for_hx_settle().await?;
        Ok(self)
    }

    // ── Console-error assertions ─────────────────────────────────────────

    /// Return every browser console `error`-level message and uncaught
    /// exception observed on this page since it was opened.
    ///
    /// Useful for inspecting *why* [`Page::expect_no_console_errors`] failed,
    /// or for asserting on specific error text.
    ///
    /// # Panics
    /// If the internal lock is poisoned (i.e. the console-capture task
    /// panicked while holding it).
    #[must_use]
    pub fn console_errors(&self) -> Vec<String> {
        self.console_errors.lock().unwrap().clone()
    }

    /// Assert that the page has produced no `console.error(...)` calls and no
    /// uncaught JavaScript exceptions.
    ///
    /// Polls for the [`CONSOLE_ERROR_GRACE`] window (checking every
    /// [`POLL_INTERVAL`]) for any in-flight console/exception CDP events to
    /// be delivered before declaring the page clean, so this can be called
    /// immediately after [`Page::visit`] without a race. An error observed
    /// at any point during the window fails immediately rather than waiting
    /// out the rest of it.
    ///
    /// # Errors
    /// [`SystemTestError::AssertionFailed`] listing every captured message,
    /// with a screenshot + HTML dump written to the artifact directory.
    pub async fn expect_no_console_errors(&self) -> Result<&Self, SystemTestError> {
        let deadline = tokio::time::Instant::now() + CONSOLE_ERROR_GRACE;
        loop {
            let errors = self.console_errors();
            if !errors.is_empty() {
                let artifact = self
                    .write_failure_artifacts("expect_no_console_errors")
                    .await
                    .ok();
                return Err(SystemTestError::AssertionFailed {
                    message: format!(
                        "page produced {} console error(s): {errors:?}",
                        errors.len()
                    ),
                    artifact_path: artifact,
                });
            }
            if tokio::time::Instant::now() >= deadline {
                return Ok(self);
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }

    // ── SSE helper ────────────────────────────────────────────────────────

    /// Wait until an element addressed by `stream_id` contains content
    /// satisfying `predicate`, indicating that an SSE stream has rendered new
    /// content into the DOM.
    ///
    /// `stream_id` may be a CSS `id` attribute or selector.
    ///
    /// # Errors
    /// [`SystemTestError::Timeout`] or [`SystemTestError::AssertionFailed`].
    pub async fn expect_sse_event(
        &self,
        stream_id: &str,
        predicate: impl Fn(&str) -> bool,
    ) -> Result<&Self, SystemTestError> {
        let timeout = Duration::from_secs(10);
        let deadline = tokio::time::Instant::now() + timeout;

        loop {
            // Try getElementById(stream_id) first — this handles IDs with CSS
            // metacharacters (e.g. "notifications:main") that would break
            // querySelector. Fall back to querySelector only when getElementById
            // returns null, so full selectors like ".sse-target" still work.
            let js = format!(
                "(function() {{ \
                   var raw = {id}; \
                   var el = document.getElementById(raw); \
                   if (!el) {{ \
                     var sel = raw.startsWith('#') ? raw : raw; \
                     try {{ el = document.querySelector(raw); }} catch(e) {{}} \
                   }} \
                   return el ? el.innerText : null; \
                 }})()",
                id = js_string_literal(stream_id)
            );

            let text: Option<String> = match self.inner.evaluate(js).await {
                Ok(result) => result.into_value().ok(),
                Err(e) if is_transient_navigation_error(&e) => None,
                Err(e) => return Err(e.into()),
            };
            if let Some(ref t) = text
                && predicate(t)
            {
                return Ok(self);
            }

            if tokio::time::Instant::now() >= deadline {
                let artifact = self.write_failure_artifacts("expect_sse_event").await.ok();
                return Err(SystemTestError::AssertionFailed {
                    message: format!(
                        "SSE event: element {stream_id:?} content {:?} did not satisfy predicate",
                        text.as_deref().unwrap_or("<not found>")
                    ),
                    artifact_path: artifact,
                });
            }

            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }

    // ── Screenshot / snapshot ─────────────────────────────────────────────

    /// Take a screenshot of the current page and save it to the artifact
    /// directory.
    ///
    /// Returns the path to the saved `.png` file.
    ///
    /// # Errors
    /// CDP or I/O errors.
    pub async fn snapshot(&self) -> Result<PathBuf, SystemTestError> {
        self.write_screenshot("snapshot").await
    }

    /// Evaluate a JavaScript expression on the page and return the result.
    ///
    /// Unlike the `expect_*` assertions this is a **single, raw** evaluation:
    /// it does not poll and does not absorb the transient CDP errors a
    /// concurrent navigation produces. If you are using it to wait for
    /// something (see the [`expect_hx_settle`](Self::expect_hx_settle)
    /// delayed-trigger note), retry it yourself rather than assuming one call
    /// lands on a live execution context.
    ///
    /// # Errors
    /// Propagates CDP errors, including transient navigation ones.
    pub async fn evaluate(
        &self,
        js: impl Into<String>,
    ) -> Result<chromiumoxide::js::EvaluationResult, SystemTestError> {
        let js_str: String = js.into();
        let res = self.inner.evaluate(js_str).await?;
        Ok(res)
    }

    // ── Internal helpers ──────────────────────────────────────────────────

    async fn wait_for_hx_settle(&self) -> Result<(), SystemTestError> {
        let deadline = tokio::time::Instant::now() + self.hx_settle_timeout;
        // A destroyed execution context (the page navigated away, e.g. a plain
        // form submit's full-page redirect) means there is no more htmx
        // activity on that page to wait for — that counts as settled, not
        // "keep polling a page that no longer exists". Hence
        // `transient_means: true`. An undecodable result reads as settled for
        // the same reason: a page with no htmx at all must not stall here.
        let settled = poll_until_deadline(deadline, true, || {
            self.evaluate_bool(
                "document.querySelectorAll('.htmx-request,.htmx-settling,.htmx-swapping').length === 0"
                    .to_owned(),
                true,
            )
        })
        .await?;

        if settled {
            return Ok(());
        }
        Err(SystemTestError::Timeout {
            message: "htmx did not settle".into(),
            timeout: self.hx_settle_timeout,
        })
    }

    /// Evaluate `js` and read the result as a bool, using `default` when the
    /// page returns something that is not one (e.g. `undefined` on a page
    /// that has not finished loading).
    async fn evaluate_bool(
        &self,
        js: String,
        default: bool,
    ) -> Result<bool, chromiumoxide::error::CdpError> {
        Ok(self
            .inner
            .evaluate(js)
            .await?
            .into_value()
            .unwrap_or(default))
    }

    /// Write screenshot + HTML artifacts and return the base path (without
    /// extension) on success.
    async fn write_failure_artifacts(&self, label: &str) -> Result<String, SystemTestError> {
        let dir = &self.artifact_dir;
        tokio::fs::create_dir_all(dir).await?;

        let base = dir.join(label);
        let base_str = base.to_string_lossy().into_owned();

        // Screenshot
        let png_path = base.with_extension("png");
        if let Ok(bytes) = self
            .inner
            .screenshot(chromiumoxide::page::ScreenshotParams::builder().build())
            .await
        {
            let _ = tokio::fs::write(&png_path, bytes).await;
        }

        // HTML dump
        let html_path = base.with_extension("html");
        if let Ok(html) = self.inner.content().await {
            let _ = tokio::fs::write(&html_path, html).await;
        }

        Ok(base_str)
    }

    async fn write_screenshot(&self, label: &str) -> Result<PathBuf, SystemTestError> {
        let dir = &self.artifact_dir;
        tokio::fs::create_dir_all(dir).await?;
        let png_path = dir.join(label).with_extension("png");
        let bytes = self
            .inner
            .screenshot(chromiumoxide::page::ScreenshotParams::builder().build())
            .await?;
        tokio::fs::write(&png_path, bytes).await?;
        Ok(png_path)
    }
}

// ── system_test! macro ─────────────────────────────────────────────────────

/// Convenience macro that builds a [`SystemTest`] runner and binds it to a
/// named variable so the server stays alive for the full test body.
///
/// ```rust,ignore
/// let runner = system_test!(SystemTest::new().routes(routes![index]));
/// let page = runner.page().await.expect("page");
/// ```
///
/// **Important:** always bind the result with `let runner = system_test!(…);`.
/// Chaining directly (`system_test!(…).page()…`) drops the runner immediately,
/// shutting the server down before any page interaction can occur.
#[macro_export]
macro_rules! system_test {
    ($builder:expr) => {{
        $builder
            .build()
            .await
            .expect("system_test! failed to start runner")
    }};
}

// ── Internal utilities ─────────────────────────────────────────────────────

/// Escape a string as a JSON-safe JavaScript string literal.
fn js_string_literal(s: &str) -> String {
    let escaped = s
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r");
    format!("\"{escaped}\"")
}

// ── Tests (non-browser, always run) ───────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn js_string_literal_escapes_quotes() {
        assert_eq!(js_string_literal(r#"say "hi""#), r#""say \"hi\"""#);
    }

    #[test]
    fn js_string_literal_escapes_backslashes() {
        assert_eq!(js_string_literal(r"a\b"), r#""a\\b""#);
    }

    #[test]
    fn artifact_dir_contains_test_name() {
        let d = artifact_dir("my_test_name");
        assert!(d.to_string_lossy().contains("my_test_name"));
        assert!(d.to_string_lossy().contains("system-tests"));
    }

    // ── Issue #1456 (2): transient CDP errors during page transitions ────
    //
    // When a user action navigates the page (a submit button that redirects),
    // Chrome tears down the JS execution context. An `evaluate()` already in
    // flight from an assertion poll then fails with a CDP error that means
    // "the page moved" — not "the assertion is false". Those must be
    // swallowed by the polling loops and retried until the deadline, while
    // genuine CDP errors still surface immediately.

    /// Build the `CdpError` shape Chrome actually returns for these.
    fn chrome_err(message: &str) -> chromiumoxide::error::CdpError {
        chromiumoxide::error::CdpError::Chrome(chromiumoxide::types::Error {
            // The predicate matches on the message, never the code: -32000 is
            // Chrome's catch-all server error and covers genuine failures too.
            code: -32000,
            message: message.to_owned(),
        })
    }

    #[test]
    fn transient_navigation_errors_are_recognised() {
        for message in [
            // The exact error reported in #1456.
            "Cannot find context with specified id",
            "Execution context was destroyed.",
            // Same race, reported against the target — note it contains no
            // "context" at all, which is why the pre-#1456 substring match
            // let it through.
            "Inspected target navigated or closed",
            // Casing must not matter.
            "cannot find CONTEXT with specified id",
        ] {
            assert!(
                is_transient_navigation_error(&chrome_err(message)),
                "{message:?} means the page navigated mid-poll and must be \
                 retried, not propagated"
            );
        }
    }

    #[test]
    fn transient_detection_covers_untyped_chrome_messages() {
        // chromiumoxide reports a CDP response error that does not
        // deserialise into its typed `Error` as `ChromeMessage` instead —
        // same failure, different variant, must not leak through.
        assert!(
            is_transient_navigation_error(&chromiumoxide::error::CdpError::msg(
                "Cannot find context with specified id"
            )),
            "the untyped ChromeMessage variant carries the same transient errors"
        );
    }

    #[test]
    fn genuine_cdp_errors_are_not_swallowed() {
        // Treating these as transient would convert a fast, precise failure
        // into a silent five-second timeout with a worse message.
        for message in [
            "No node with given id found",
            "Could not compute content quads.",
            "Node is not a HTMLElement",
            // A JS error that merely mentions the word — the pre-#1456 bare
            // `contains("context")` match would have swallowed this.
            "ReferenceError: context is not defined",
        ] {
            assert!(
                !is_transient_navigation_error(&chrome_err(message)),
                "{message:?} is a real failure and must propagate immediately"
            );
        }
        assert!(
            !is_transient_navigation_error(&chromiumoxide::error::CdpError::Timeout),
            "a CDP timeout is not a navigation race"
        );
        assert!(
            !is_transient_navigation_error(&chromiumoxide::error::CdpError::NoResponse),
            "a dead connection is not a navigation race"
        );
    }

    #[test]
    fn a_dead_target_is_not_treated_as_a_navigation() {
        // Chrome emits these when the renderer died (a sad tab under CI
        // memory pressure), not only when the page moved. `wait_for_hx_settle`
        // reads a transient error as *settled*, so swallowing these would let
        // a crashed page sail through `click()` and fail five seconds later in
        // `expect_text` with no screenshot — the exact decay this predicate
        // exists to prevent.
        for message in ["Target closed", "Session with given id not found."] {
            assert!(
                !is_transient_navigation_error(&chrome_err(message)),
                "{message:?} is indistinguishable from a dead renderer and must \
                 surface immediately"
            );
        }
    }

    // ── The retry loop itself (AC2's actual behaviour) ───────────────────
    //
    // The predicate above only classifies an error. What #1456 asks for is
    // that assertion helpers *keep polling* past one — these drive the shared
    // loop with a scripted probe so the behaviour is covered without a
    // browser.

    /// A probe that replays `script`, one entry per call, then repeats the
    /// last entry forever. Also records how many times it was called.
    fn scripted_probe(
        script: Vec<Result<bool, chromiumoxide::error::CdpError>>,
    ) -> (
        impl FnMut() -> std::future::Ready<Result<bool, chromiumoxide::error::CdpError>>,
        Arc<std::sync::atomic::AtomicUsize>,
    ) {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let script = Arc::new(script);
        let probe = move || {
            let n = counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let step = script.get(n).unwrap_or_else(|| script.last().unwrap());
            let replay = match step {
                Ok(v) => Ok(*v),
                Err(e) => Err(chromiumoxide::error::CdpError::msg(e.to_string())),
            };
            std::future::ready(replay)
        };
        (probe, calls)
    }

    #[tokio::test(start_paused = true)]
    async fn polling_retries_past_transient_errors_and_succeeds() {
        // The #1456 scenario: a redirect destroys the execution context under
        // two in-flight evaluations, then the post-navigation page answers.
        let (probe, calls) = scripted_probe(vec![
            Err(chrome_err("Cannot find context with specified id")),
            Err(chrome_err("Cannot find context with specified id")),
            Ok(true),
        ]);
        let deadline = tokio::time::Instant::now() + ASSERTION_TIMEOUT;

        let outcome = poll_until_deadline(deadline, false, probe).await;

        assert!(
            matches!(outcome, Ok(true)),
            "a transient error must not abort the poll; got {outcome:?}"
        );
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::Relaxed),
            3,
            "the loop must have polled past both transient errors"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn polling_aborts_immediately_on_a_genuine_error() {
        let (probe, calls) = scripted_probe(vec![Err(chrome_err("No node with given id found"))]);
        let deadline = tokio::time::Instant::now() + ASSERTION_TIMEOUT;

        let outcome = poll_until_deadline(deadline, false, probe).await;

        assert!(
            matches!(outcome, Err(SystemTestError::Browser(_))),
            "a genuine CDP error must propagate rather than decay into a \
             timeout; got {outcome:?}"
        );
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "it must fail fast, not keep polling"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn polling_gives_up_at_the_deadline() {
        // Unrelenting transient errors must still terminate — otherwise a
        // permanently broken page hangs the test instead of failing it.
        let (probe, _) = scripted_probe(vec![Err(chrome_err(
            "Cannot find context with specified id",
        ))]);
        let deadline = tokio::time::Instant::now() + ASSERTION_TIMEOUT;

        let outcome = poll_until_deadline(deadline, false, probe).await;

        assert!(
            matches!(outcome, Ok(false)),
            "the caller must get the deadline signal so it can write artifacts \
             and report a real assertion failure; got {outcome:?}"
        );
        assert!(
            tokio::time::Instant::now() >= deadline,
            "it must actually have waited out the assertion timeout"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn settle_treats_a_navigated_page_as_settled() {
        // `wait_for_hx_settle` passes `transient_means: true`: a page that
        // navigated away has no htmx activity left to wait for, so the fence
        // must return at once rather than poll a page that no longer exists.
        let (probe, calls) =
            scripted_probe(vec![Err(chrome_err("Execution context was destroyed."))]);
        let deadline = tokio::time::Instant::now() + DEFAULT_HX_SETTLE_TIMEOUT;

        let outcome = poll_until_deadline(deadline, true, probe).await;

        assert!(matches!(outcome, Ok(true)), "got {outcome:?}");
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "settle must not keep polling a destroyed context"
        );
    }

    // ── Issue #1456 (3): custom middleware layers on the test router ──────
    //
    // Apps whose routes depend on global middleware (tenant scoping, auth
    // shims, request enrichment) had no way to register it on `SystemTest`,
    // forcing callers to hand-map layers onto individual handlers before
    // passing them to `.routes()`. `SystemTest::layer` must register a
    // Tower layer that the served router actually runs — on *both* the
    // default-state and `state()`-override paths — and must compose
    // multiple registrations with the same ordering contract as
    // `AppBuilder::layer` (first call = outermost on ingress).

    /// Records the ingress order of every middleware that ran.
    type IngressLog = Arc<Mutex<Vec<&'static str>>>;

    /// A tenant identifier a middleware inserts and a handler reads back —
    /// the shape of the tenant-scoping middleware from the original report.
    #[derive(Clone)]
    struct TenantId(&'static str);

    /// A `from_fn` layer that appends `name` to `log` on the way *in* and
    /// again (as `"<name>:out"`) on the way *out*, so both halves of the
    /// documented ordering contract are observable.
    fn recording_layer(
        log: &IngressLog,
        name: &'static str,
        out: &'static str,
    ) -> impl crate::app::IntoAppLayer + Clone {
        let log = Arc::clone(log);
        axum::middleware::from_fn(
            move |req: axum::extract::Request, next: axum::middleware::Next| {
                let log = Arc::clone(&log);
                async move {
                    log.lock().unwrap().push(name);
                    let response = next.run(req).await;
                    log.lock().unwrap().push(out);
                    response
                }
            },
        )
    }

    /// A route that echoes the [`TenantId`] a layer inserted, or says it saw
    /// none. Proves the layer wraps *registered routes*, not just the
    /// fallback, and that its request extensions reach handlers.
    fn tenant_echo_route() -> Route {
        Route {
            method: http::Method::GET,
            path: "/",
            handler: axum::routing::get(|tenant: Option<axum::Extension<TenantId>>| async move {
                tenant.map_or_else(
                    || "no-tenant".to_owned(),
                    |axum::Extension(t)| format!("tenant={}", t.0),
                )
            }),
            name: "tenant_echo",
            api_doc: crate::openapi::ApiDoc {
                method: "GET",
                path: "/",
                operation_id: "tenant_echo",
                success_status: 200,
                ..Default::default()
            },
            repository: None,
            idempotency: crate::route::RouteIdempotency::Direct,
            timeout: crate::route::RouteTimeout::Inherit,
            seo: crate::seo::SeoRouteDefaults::EMPTY,
            api_version: None,
            sunset_opt_out: false,
        }
    }

    /// Drive one `GET /` through `router` and return its status and body.
    async fn probe_router(router: axum::Router) -> (axum::http::StatusCode, String) {
        use tower::ServiceExt as _;
        let request = axum::http::Request::builder()
            .uri("/")
            .body(axum::body::Body::empty())
            .unwrap();
        let response = router.oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    /// A layer that inserts a [`TenantId`] for the handler to read.
    fn tenant_layer() -> impl crate::app::IntoAppLayer + Clone {
        axum::middleware::from_fn(
            |mut req: axum::extract::Request, next: axum::middleware::Next| async move {
                req.extensions_mut().insert(TenantId("acme-corp"));
                next.run(req).await
            },
        )
    }

    #[tokio::test]
    async fn registered_layer_reaches_registered_route_handlers() {
        // The whole point of the feature: middleware must wrap the routes
        // under test, and what it puts on the request must reach the handler.
        let builder = SystemTest::new()
            .routes(vec![tenant_echo_route()])
            .layer(tenant_layer());
        let (status, body) = probe_router(builder.into_router()).await;

        assert_eq!(status, axum::http::StatusCode::OK);
        assert_eq!(
            body, "tenant=acme-corp",
            "the handler must observe the extension inserted by the layer \
             registered via SystemTest::layer"
        );
    }

    #[tokio::test]
    async fn without_a_layer_the_same_route_sees_nothing() {
        // Guards the test above against passing for the wrong reason.
        let builder = SystemTest::new().routes(vec![tenant_echo_route()]);
        let (_, body) = probe_router(builder.into_router()).await;
        assert_eq!(body, "no-tenant");
    }

    #[tokio::test]
    async fn registered_layer_runs_on_the_state_override_path() {
        // Layers must not silently vanish because the caller supplied their
        // own AppState — the branch a future edit is most likely to forget.
        let state = crate::state::AppState::for_test();
        state.insert_extension(AutumnConfig::default());
        let builder = SystemTest::new()
            .routes(vec![tenant_echo_route()])
            .state(state)
            .layer(tenant_layer());
        let (_, body) = probe_router(builder.into_router()).await;
        assert_eq!(body, "tenant=acme-corp");
    }

    #[tokio::test]
    async fn a_layer_can_short_circuit_the_handler() {
        // Proves the layer genuinely *wraps* the handler rather than merely
        // running alongside it.
        let builder =
            SystemTest::new()
                .routes(vec![tenant_echo_route()])
                .layer(axum::middleware::from_fn(
                    |_req: axum::extract::Request, _next: axum::middleware::Next| async move {
                        axum::http::StatusCode::IM_A_TEAPOT
                    },
                ));
        let (status, body) = probe_router(builder.into_router()).await;

        assert_eq!(status, axum::http::StatusCode::IM_A_TEAPOT);
        assert!(
            !body.contains("tenant"),
            "the handler must not have run; got body {body:?}"
        );
    }

    #[tokio::test]
    async fn layers_compose_first_registered_outermost() {
        let log: IngressLog = Arc::new(Mutex::new(Vec::new()));
        let builder = SystemTest::new()
            .routes(vec![tenant_echo_route()])
            .layer(recording_layer(&log, "first", "first:out"))
            .layer(recording_layer(&log, "second", "second:out"));
        probe_router(builder.into_router()).await;

        assert_eq!(
            *log.lock().unwrap(),
            vec!["first", "second", "second:out", "first:out"],
            "ordering must match AppBuilder::layer: the first registration is \
             the outermost layer, so it sees the request first and the \
             response last"
        );
    }

    #[tokio::test]
    async fn a_registered_layer_observes_the_framework_request_id() {
        // The stack-position claim in the docs — layers sit *inside*
        // Autumn's RequestId layer — is what makes a system test faithful to
        // production for middleware that reads it. Mirrors
        // `custom_layer_sees_request_id` in tests/integration/custom_layer.rs.
        let seen: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let sink = Arc::clone(&seen);
        let builder =
            SystemTest::new()
                .routes(vec![tenant_echo_route()])
                .layer(axum::middleware::from_fn(
                    move |req: axum::extract::Request, next: axum::middleware::Next| {
                        let sink = Arc::clone(&sink);
                        async move {
                            let id = req
                                .extensions()
                                .get::<crate::middleware::RequestId>()
                                .map(ToString::to_string);
                            *sink.lock().unwrap() = id;
                            next.run(req).await
                        }
                    },
                ));
        probe_router(builder.into_router()).await;

        let observed = seen.lock().unwrap().clone();
        assert!(
            observed.is_some_and(|id| !id.is_empty()),
            "a layer registered on SystemTest must see the request ID, as it \
             does under AppBuilder::layer"
        );
    }

    #[test]
    fn build_router_default_state_does_not_panic() {
        // Exercises the None arm of `into_router`.
        let _router = SystemTest::new().into_router();
    }

    #[test]
    fn build_router_with_state_override_uses_embedded_config() {
        // Exercises the Some(state) arm: config is read from the supplied
        // state rather than constructed from scratch.
        let config = AutumnConfig {
            profile: Some("custom".into()),
            ..Default::default()
        };
        let state = crate::state::AppState::for_test();
        state.insert_extension(config);
        let _router = SystemTest::new().state(state).into_router();
    }

    #[test]
    fn build_router_with_state_override_no_embedded_config_uses_default() {
        // When the supplied state has no AutumnConfig extension, a default is used.
        let state = crate::state::AppState::for_test();
        let _router = SystemTest::new().state(state).into_router();
    }
}
