//! Red-phase tests for the `system-tests` feature.
//!
//! These tests verify the API surface of `autumn_web::system_test` compiles
//! and behaves correctly for non-browser parts (browser-dependent paths are
//! `#[ignore]` and require a Chromium binary).
//!
//! Run:
//!   cargo test -p autumn-web --features system-tests --test `system_test_api`

#![cfg(feature = "system-tests")]

use autumn_web::system_test::{BrowserCheck, SystemTest, SystemTestError};

// ── BrowserCheck unit tests ────────────────────────────────────────────────

#[test]
fn browser_check_reports_result() {
    let result = BrowserCheck::run();
    // Always returns a result; variant depends on whether Chrome is installed.
    match result {
        BrowserCheck::Found { path, version } => {
            assert!(!path.as_os_str().is_empty());
            assert!(!version.is_empty(), "version string must not be empty");
        }
        BrowserCheck::NotFound { searched_paths } => {
            assert!(
                !searched_paths.is_empty(),
                "must report at least one path that was searched"
            );
        }
    }
}

#[test]
fn browser_check_displays_actionable_message() {
    let check = BrowserCheck::run();
    let msg = check.to_string();
    assert!(!msg.is_empty());
    if matches!(check, BrowserCheck::NotFound { .. }) {
        // The error must mention how to install Chrome.
        assert!(
            msg.contains("apt-get") || msg.contains("brew") || msg.contains("AUTUMN_CHROMIUM"),
            "not-found message must include install hint; got: {msg}"
        );
    }
}

// ── Artifact directory convention ─────────────────────────────────────────

#[test]
fn artifact_dir_is_under_target() {
    let dir = autumn_web::system_test::artifact_dir("my_test");
    let s = dir.to_string_lossy();
    assert!(
        s.contains("system-tests"),
        "artifact dir must be under target/system-tests; got: {s}"
    );
    assert!(
        s.contains("my_test"),
        "artifact dir must include the test name; got: {s}"
    );
}

// ── SystemTest builder compiles with the expected fluent API ───────────────

#[test]
fn system_test_builder_has_expected_methods() {
    // This test exists purely to verify the public API compiles.
    // It does NOT launch a browser.
    fn _assert_api_shape() {
        let _builder = SystemTest::new()
            .artifact_dir("/tmp/artifacts")
            .browser_timeout(std::time::Duration::from_secs(30))
            .hx_settle_timeout(std::time::Duration::from_millis(500));
        // We don't call .build().await here to avoid needing a browser.
    }
}

// ── SystemTestError formatting ─────────────────────────────────────────────

#[test]
fn system_test_error_displays() {
    let e = SystemTestError::BrowserNotFound {
        searched: vec!["/usr/bin/chromium".into()],
    };
    let msg = e.to_string();
    assert!(msg.contains("browser") || msg.contains("Chromium") || msg.contains("chromium"));
}

#[test]
fn assertion_error_includes_selector() {
    let e = SystemTestError::AssertionFailed {
        message: "expected text 'hello' in DOM".into(),
        artifact_path: None,
    };
    let msg = e.to_string();
    assert!(msg.contains("hello"));
}

// ── Browser-dependent tests (require Chrome, marked #[ignore]) ────────────

/// Boots a minimal app, opens a page, asserts on rendered text.
/// Requires Chromium on the host.
#[tokio::test]
#[ignore = "requires Chromium — set AUTUMN_CHROMIUM or install chromium-browser"]
async fn system_test_boots_and_visits_page() {
    use autumn_web::prelude::*;

    #[get("/")]
    async fn index() -> &'static str {
        "<html><body><h1 id='greeting'>Hello from system test</h1></body></html>"
    }

    let runner = SystemTest::new()
        .routes(routes![index])
        .build()
        .await
        .expect("failed to start system test runner");

    let page = runner.page().await.expect("failed to open page");
    page.visit("/").await.expect("visit failed");
    page.expect_text("Hello from system test")
        .await
        .expect("text assertion failed");
}

/// Verifies that assertion failures write artifacts.
#[tokio::test]
#[ignore = "requires Chromium"]
async fn assertion_failure_writes_artifacts() {
    use autumn_web::prelude::*;
    use std::path::Path;

    #[get("/")]
    async fn index() -> &'static str {
        "<html><body><p>Only this text</p></body></html>"
    }

    let runner = SystemTest::new()
        .routes(routes![index])
        .artifact_dir("/tmp/autumn-system-test-artifacts")
        .build()
        .await
        .expect("start runner");

    let page = runner.page().await.expect("open page");
    page.visit("/").await.expect("visit");

    let result = page.expect_text("NOT IN PAGE").await;
    assert!(result.is_err(), "should fail for missing text");

    if let Err(SystemTestError::AssertionFailed {
        artifact_path: Some(p),
        ..
    }) = result
    {
        assert!(
            Path::new(&p).with_extension("png").exists()
                || Path::new(&p).with_extension("html").exists(),
            "artifact file not written at {p}"
        );
    }
}

/// Verifies htmx settle waiting.
#[tokio::test]
#[ignore = "requires Chromium"]
async fn expect_hx_settle_waits_for_htmx() {
    use autumn_web::prelude::*;

    #[get("/")]
    async fn index() -> Markup {
        maud::html! {
            html {
                head {
                    script src="/static/js/htmx.min.js" {}
                }
                body {
                    div id="result" {}
                    button
                        hx-get="/swap"
                        hx-target="#result"
                        hx-swap="innerHTML" { "Click me" }
                }
            }
        }
    }

    #[get("/swap")]
    async fn swap() -> &'static str {
        "<span>Swapped!</span>"
    }

    let runner = SystemTest::new()
        .routes(routes![index, swap])
        .build()
        .await
        .expect("start");

    let page = runner.page().await.expect("page");
    page.visit("/").await.expect("visit");
    page.click("button").await.expect("click");
    page.expect_hx_settle().await.expect("settle");
    page.expect_text("Swapped!").await.expect("assert swap");
}

// ── attach(): browser-only mode against an already-running server ─────────
//
// Issue #1192: the fan-out example harness spawns each example's real
// binary as a subprocess (so `main.rs`'s actual routes, migrations, and
// builder chain run unmodified) and only needs the browser half of
// `SystemTest`. `attach(base_url)` must launch managed Chromium and target
// an externally-supplied base URL instead of booting an in-process router.

/// `attach()` must launch a browser and successfully visit a page served by
/// an independently-running server. We stand up that server via a normal
/// `SystemTest::build()` (which owns its own in-process axum server) purely
/// to get *something* listening on an ephemeral port, then discard that
/// runner's browser/page and instead attach a **second**, independent
/// `SystemTest::attach()` runner at the same `base_url()` — exercising the
/// exact shape the fan-out harness uses: browser half only, server owned
/// elsewhere (there, a spawned subprocess; here, the first runner's server).
#[tokio::test]
#[ignore = "requires Chromium — set AUTUMN_CHROMIUM or install chromium-browser"]
async fn attach_visits_externally_running_server() {
    use autumn_web::prelude::*;

    #[get("/")]
    async fn index() -> &'static str {
        "<html><body><h1>Externally booted</h1></body></html>"
    }

    let server = SystemTest::new()
        .routes(routes![index])
        .build()
        .await
        .expect("boot stand-in server");
    let base_url = server.base_url().to_string();

    let runner = SystemTest::attach(base_url)
        .await
        .expect("attach to running server");

    let page = runner.page().await.expect("open page");
    page.visit("/").await.expect("visit");
    page.expect_text("Externally booted")
        .await
        .expect("text assertion failed");
}

// ── Console-error capture ──────────────────────────────────────────────────
//
// AC3 requires every baseline smoke to assert "no uncaught console errors".
// `Page` must accumulate console/runtime errors as they occur so a smoke can
// fail loudly on a broken page instead of only checking rendered text.

/// A page that throws an uncaught JS error must fail
/// `expect_no_console_errors()`.
#[tokio::test]
#[ignore = "requires Chromium"]
async fn expect_no_console_errors_fails_on_uncaught_exception() {
    use autumn_web::prelude::*;

    #[get("/")]
    async fn index() -> &'static str {
        "<html><body><h1>Page loads fine</h1></body></html>"
    }

    let runner = SystemTest::new()
        .routes(routes![index])
        .build()
        .await
        .expect("start runner");

    let page = runner.page().await.expect("open page");
    page.visit("/").await.expect("visit");

    // Autumn's default CSP (`script-src 'self'`) blocks inline `<script>`
    // tags in served HTML, so an inline-script fixture would never actually
    // run and this test would trivially pass for the wrong reason. Trigger
    // the exception via CDP `Runtime.evaluate` instead (`Page::evaluate`),
    // which — like the DevTools console itself — is not subject to the
    // page's CSP, so it reliably reaches V8 as an uncaught exception.
    let _ = page
        .evaluate("setTimeout(() => { throw new Error('boom'); }, 0)")
        .await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let result = page.expect_no_console_errors().await;
    assert!(
        result.is_err(),
        "a page that throws an uncaught exception must fail the console-error assertion"
    );
}

/// A clean page (no console errors) must pass `expect_no_console_errors()`.
#[tokio::test]
#[ignore = "requires Chromium"]
async fn expect_no_console_errors_passes_on_clean_page() {
    use autumn_web::prelude::*;

    #[get("/")]
    async fn index() -> &'static str {
        "<html><body><h1>All good</h1></body></html>"
    }

    let runner = SystemTest::new()
        .routes(routes![index])
        .build()
        .await
        .expect("start runner");

    let page = runner.page().await.expect("open page");
    page.visit("/").await.expect("visit");
    page.expect_text("All good").await.expect("text");

    page.expect_no_console_errors()
        .await
        .expect("clean page must not report console errors");
}

/// `console_errors()` must return the accumulated messages for inspection
/// (not just a boolean), so a failing smoke's CI output is actionable.
#[tokio::test]
#[ignore = "requires Chromium"]
async fn console_errors_returns_accumulated_messages() {
    use autumn_web::prelude::*;

    #[get("/")]
    async fn index() -> &'static str {
        "<html><body><h1>Page loads fine</h1></body></html>"
    }

    let runner = SystemTest::new()
        .routes(routes![index])
        .build()
        .await
        .expect("start runner");

    let page = runner.page().await.expect("open page");
    page.visit("/").await.expect("visit");

    // Autumn's default CSP (`script-src 'self'`) blocks inline `<script>`
    // tags, so trigger the console call via CDP `Runtime.evaluate`
    // (`Page::evaluate`) instead — it is not subject to page CSP, same as
    // the DevTools console.
    let _ = page.evaluate("console.error('bad thing happened')").await;

    // Give the CDP event a moment to be delivered and recorded.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let errors = page.console_errors();
    assert!(
        errors.iter().any(|e| e.contains("bad thing happened")),
        "expected captured console.error message; got: {errors:?}"
    );
}
