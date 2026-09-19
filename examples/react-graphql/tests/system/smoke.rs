//! Chromium smoke for the `react-graphql` example.
//!
//! Spawns the real binary against an ephemeral testcontainer Postgres
//! (migrated and seeded on boot — `AUTUMN_ENV=development`) and drives a
//! headless browser through the whole loop: the Autumn-rendered shell loads
//! the committed React bundle, the bundle fetches the seeded rows over
//! GraphQL, and adding a note through the form round-trips a mutation into
//! the database — with no console errors.
//!
//! Run (requires Chromium + Docker):
//!   cargo test -p react-graphql --features system-tests --test smoke -- --include-ignored
//!
//!   AUTUMN_CHROMIUM=/path/to/chrome cargo test ...   # custom binary

#![cfg(feature = "system-tests")]

#[tokio::test]
#[ignore = "requires Chromium + Docker — set AUTUMN_CHROMIUM or install chromium-browser"]
async fn react_app_renders_and_mutates_over_graphql() {
    let db = example_e2e::provision_postgres(1).await;

    let app = example_e2e::spawn_example(
        env!("CARGO_BIN_EXE_react-graphql"),
        env!("CARGO_MANIFEST_DIR"),
        &[("AUTUMN_DATABASE__URL", &db.urls()[0])],
        example_e2e::DEFAULT_READY_TIMEOUT,
    )
    .await
    .expect("spawn react-graphql example — is it built?");

    let runner = app
        .attach_browser()
        .await
        .expect("attach browser — is Chromium installed?");
    let page = runner.page().await.expect("open page");

    // The shell is server-rendered; the heading and the seeded notes are not —
    // seeing them proves the bundle loaded under the default CSP and the
    // GraphQL query reached the freshly migrated, seeded table.
    page.visit("/").await.expect("visit /");
    page.expect_text("Autumn Notes")
        .await
        .expect("React rendered the heading");
    page.expect_text("Welcome to Autumn Notes")
        .await
        .expect("seeded rows fetched over GraphQL and rendered");
    page.expect_text("2 notes, 1 pinned")
        .await
        .expect("note count reflects the seeded table");
    page.expect_no_console_errors()
        .await
        .expect("no console errors on first load");

    // A mutation through the form: the new row appears without a reload.
    page.fill("input[name=title]", "Written by the smoke test")
        .await
        .expect("fill the title");
    page.click("button[type=submit]")
        .await
        .expect("submit the composer");
    page.expect_text("Written by the smoke test")
        .await
        .expect("created row rendered from the mutation's return value");
    page.expect_text("3 notes, 1 pinned")
        .await
        .expect("note count updated");

    // A second mutation shape — `togglePinned(id: ID!)` — through the Pin
    // button on the note just created (id 3: BIGSERIAL after the two seeds;
    // the pinned section renders first, so "first Pin button" would be the
    // welcome note's Unpin). This is the client's variable-typed operation, so
    // a schema/client drift in the `ID` scalar surfaces here, not in
    // production.
    page.click(r#"li[data-note-id="3"] button:not(.danger)"#)
        .await
        .expect("click Pin on the newest note");
    page.expect_text("3 notes, 2 pinned")
        .await
        .expect("toggle mutation round-tripped");
    page.expect_no_console_errors()
        .await
        .expect("no console errors after the mutations");
}

/// The same journey under the `prod` profile: CSRF on, trusted hosts and the
/// repository-API gate satisfied by `autumn.toml`, the signing secret from
/// the environment, migrations auto-applied by the example's own opt-in. The
/// mutations succeed only because the shell hands the client the CSRF token.
#[tokio::test]
#[ignore = "requires Chromium + Docker — set AUTUMN_CHROMIUM or install chromium-browser"]
async fn react_app_works_under_the_prod_profile() {
    let db = example_e2e::provision_postgres(1).await;
    // Any 32 random bytes; `prod` refuses to start without one.
    let secret = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    let app = example_e2e::spawn_example(
        env!("CARGO_BIN_EXE_react-graphql"),
        env!("CARGO_MANIFEST_DIR"),
        &[
            ("AUTUMN_DATABASE__URL", &db.urls()[0]),
            ("AUTUMN_ENV", "prod"),
            ("AUTUMN_SECURITY__SIGNING_SECRET", secret),
        ],
        example_e2e::DEFAULT_READY_TIMEOUT,
    )
    .await
    .expect("spawn react-graphql under prod — is it built?");

    let runner = app
        .attach_browser()
        .await
        .expect("attach browser — is Chromium installed?");
    let page = runner.page().await.expect("open page");

    page.visit("/").await.expect("visit /");
    page.expect_text("Welcome to Autumn Notes")
        .await
        .expect("seeded rows rendered under prod");
    page.fill("input[name=title]", "Written under prod")
        .await
        .expect("fill the title");
    page.click("button[type=submit]")
        .await
        .expect("submit the composer");
    page.expect_text("Written under prod")
        .await
        .expect("mutation accepted — the CSRF token reached the server");
    page.click(r#"li[data-note-id="3"] button:not(.danger)"#)
        .await
        .expect("click Pin");
    page.expect_text("3 notes, 2 pinned")
        .await
        .expect("toggle accepted under prod");
    page.expect_no_console_errors()
        .await
        .expect("no console errors under prod");
}
