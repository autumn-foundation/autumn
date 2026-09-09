//! Baseline Chromium smoke.
//!
//! Spawns the real `cms` binary against an ephemeral testcontainer Postgres
//! (migrated automatically on boot — `AUTUMN_ENV=development`), drives a
//! headless Chromium against the public registration screen, and asserts the
//! page renders with no uncaught console errors.
//!
//! Run (requires Chromium + Docker):
//!   cargo test -p cms --features system-tests --test smoke -- --include-ignored

#![cfg(feature = "system-tests")]

#[tokio::test]
#[ignore = "requires Chromium + Docker — set AUTUMN_CHROMIUM or install chromium-browser"]
async fn cms_boots_and_serves_the_registration_screen() {
    let db = example_e2e::provision_postgres(1).await;

    let app = example_e2e::spawn_example(
        env!("CARGO_BIN_EXE_cms"),
        env!("CARGO_MANIFEST_DIR"),
        &[("AUTUMN_DATABASE__URL", &db.urls()[0])],
        example_e2e::DEFAULT_READY_TIMEOUT,
    )
    .await
    .expect("spawn cms example — is it built?");

    let runner = app
        .attach_browser()
        .await
        .expect("attach browser — is Chromium installed?");
    let page = runner.page().await.expect("open page");

    // A fresh install has no accounts, so registration is the first screen an
    // operator sees — the equivalent of WordPress's five-minute install.
    page.visit("/register").await.expect("visit /register");
    page.expect_text("Create an account")
        .await
        .expect("registration screen renders");
    page.expect_no_console_errors()
        .await
        .expect("no console errors on /register");
}
