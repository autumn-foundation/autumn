//! Baseline Chromium smoke — issue #1192.
//!
//! Spawns the real `todo-app` binary against an ephemeral testcontainer
//! Postgres (migrated automatically on boot — `AUTUMN_ENV=development`),
//! drives a headless-Chromium browser against the todo list page, and
//! asserts expected content with no uncaught console errors.
//!
//! `tests/system/todo_htmx_flow.rs` already covers the htmx interaction
//! journey with a self-contained in-memory store; this test instead proves
//! the *real* Diesel/Postgres-backed route boots and renders end to end.
//!
//! Run (requires Chromium + Docker):
//!   cargo test -p todo-app --features system-tests --test smoke -- --include-ignored

#![cfg(feature = "system-tests")]

#[tokio::test]
#[ignore = "requires Chromium + Docker — set AUTUMN_CHROMIUM or install chromium-browser"]
async fn todo_app_boots_and_serves_todo_list() {
    let db = example_e2e::provision_postgres(1).await;

    let app = example_e2e::spawn_example(
        env!("CARGO_BIN_EXE_todo-app"),
        env!("CARGO_MANIFEST_DIR"),
        &[("AUTUMN_DATABASE__URL", &db.urls()[0])],
        example_e2e::DEFAULT_READY_TIMEOUT,
    )
    .await
    .expect("spawn todo-app example — is it built?");

    let runner = app
        .attach_browser()
        .await
        .expect("attach browser — is Chromium installed?");
    let page = runner.page().await.expect("open page");

    page.visit("/")
        .await
        .expect("visit / (redirects to /todos)");
    page.expect_text("Autumn Todos")
        .await
        .expect("todo list heading renders");
    page.expect_text("No todos yet. Add one above!")
        .await
        .expect("empty-state renders against the freshly migrated DB");
    page.expect_no_console_errors()
        .await
        .expect("no console errors on the todo list page");
}
