//! Baseline Chromium smoke — issue #1192.
//!
//! Spawns the real `saas` binary against an ephemeral testcontainer
//! Postgres (migrated automatically on boot — `AUTUMN_ENV=development`),
//! drives a headless-Chromium browser against the public login page
//! (tenancy-public per `autumn.toml`), and asserts expected content with
//! no uncaught console errors.
//!
//! Run (requires Chromium + Docker):
//!   cargo test -p saas --features system-tests --test smoke -- --include-ignored

#![cfg(feature = "system-tests")]

#[tokio::test]
#[ignore = "requires Chromium + Docker — set AUTUMN_CHROMIUM or install chromium-browser"]
async fn saas_boots_and_serves_login_page() {
    let db = example_e2e::provision_postgres(1).await;

    let app = example_e2e::spawn_example(
        env!("CARGO_BIN_EXE_saas"),
        env!("CARGO_MANIFEST_DIR"),
        &[("AUTUMN_DATABASE__URL", &db.urls()[0])],
        example_e2e::DEFAULT_READY_TIMEOUT,
    )
    .await
    .expect("spawn saas example — is it built?");

    let runner = app
        .attach_browser()
        .await
        .expect("attach browser — is Chromium installed?");
    let page = runner.page().await.expect("open page");

    page.visit("/login").await.expect("visit /login");
    page.expect_text("Log in")
        .await
        .expect("login page renders (public under tenancy middleware)");
    page.expect_no_console_errors()
        .await
        .expect("no console errors on /login");
}
