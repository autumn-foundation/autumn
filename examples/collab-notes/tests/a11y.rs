//! Regression test for the a11y fix (`autumn check --a11y` `html-has-lang`,
//! `bypass`, `landmark-one-main`): both pages in this example are full HTML
//! documents with a language, a single `<main>` landmark as the first thing
//! in `<body>`, and (because there is no nav/header to skip past) correctly
//! no skip link.
//!
//! Spawns the real, unmodified `collab-notes` binary (this example needs no
//! database, so no Docker/testcontainer is required) and asserts on the raw
//! HTML served over plain HTTP — no headless browser needed either.
//!
//! Run: `cargo test -p collab-notes --test a11y`

use std::time::Duration;

use example_e2e::spawn_example;

#[tokio::test]
async fn index_and_editor_pages_have_a_document_shell_with_one_main_landmark() {
    let app = spawn_example(
        env!("CARGO_BIN_EXE_collab-notes"),
        env!("CARGO_MANIFEST_DIR"),
        &[],
        Duration::from_secs(30),
    )
    .await
    .expect("spawn collab-notes example — is it built?");

    let client = reqwest::Client::new();

    for path in ["/", "/notes/1"] {
        let url = format!("{}{path}", app.base_url());
        let body = client
            .get(&url)
            .send()
            .await
            .unwrap_or_else(|err| panic!("GET {url}: {err}"))
            .text()
            .await
            .unwrap_or_else(|err| panic!("read body of {url}: {err}"));

        assert!(
            body.contains("<!DOCTYPE html>"),
            "{path}: missing doctype:\n{body}"
        );
        assert!(
            body.contains("<html lang=\"en\">"),
            "{path}: <html> is missing a lang attribute:\n{body}"
        );
        // Exactly one <main>, and nothing but the doctype/<html>/<head>/
        // <body> opening tags precedes it — the shape `autumn check --a11y`
        // exempts from also requiring a skip link (there is no nav/header
        // to bypass).
        assert_eq!(
            body.matches("<main").count(),
            1,
            "{path}: expected exactly one <main> landmark:\n{body}"
        );
        let before_main = &body[..body.find("<main").expect("has <main>")];
        assert!(
            !before_main.contains("<a "),
            "{path}: a link precedes <main>, so a keyboard user would need a skip link:\n{body}"
        );
    }
}
