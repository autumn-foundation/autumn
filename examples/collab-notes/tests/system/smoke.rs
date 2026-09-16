//! Two browsers, one field, no lost characters (issue #1806, AC2).
//!
//! Spawns the real `collab-notes` binary, opens the **same note in two
//! Chromium pages**, types in both, and asserts both pages settle on one text
//! that holds every character. This is the acceptance criterion the hub-level
//! and socket-level tests in `autumn` approximate: real browsers, real
//! WebSockets, real concurrent editing.
//!
//! Run (requires Chromium):
//!   cargo test -p collab-notes --features system-tests --test smoke -- --include-ignored

#![cfg(feature = "system-tests")]

use std::time::Duration;

#[tokio::test]
#[ignore = "requires Chromium — set AUTUMN_CHROMIUM or install chromium-browser"]
async fn two_browser_sessions_converge_on_the_same_text() {
    let app = example_e2e::spawn_example(
        env!("CARGO_BIN_EXE_collab-notes"),
        env!("CARGO_MANIFEST_DIR"),
        &[],
        example_e2e::DEFAULT_READY_TIMEOUT,
    )
    .await
    .expect("spawn collab-notes example — is it built?");

    let runner = app
        .attach_browser()
        .await
        .expect("attach browser — is Chromium installed?");

    let ada = runner.page().await.expect("open Ada's page");
    let linus = runner.page().await.expect("open Linus's page");

    for page in [&ada, &linus] {
        page.visit("/notes/1").await.expect("visit the note");
        page.expect_text("Shopping list")
            .await
            .expect("the editor renders");
        // The socket has to be up before an edit can be sent.
        page.expect_text("connected")
            .await
            .expect("the collaboration socket connects");
    }

    // Both sessions see the seeded text.
    for page in [&ada, &linus] {
        let value = editor_value(page).await;
        assert!(value.contains("eggs"), "seeded body is rendered: {value:?}");
    }

    // Each session types at a different place, at the same time.
    type_at_end(&ada, "ada was here\n").await;
    type_at_start(&linus, "TODO: ").await;

    // Give the round trip a moment, then assert convergence.
    let ada_text = settle(&ada, "ada was here").await;
    let linus_text = settle(&linus, "ada was here").await;

    assert_eq!(
        ada_text, linus_text,
        "both browser sessions converge on one text"
    );
    assert!(
        ada_text.contains("ada was here") && ada_text.contains("TODO: "),
        "no edit was lost: {ada_text:?}"
    );
    assert!(
        ada_text.contains("eggs") && ada_text.contains("milk"),
        "the seeded text survived: {ada_text:?}"
    );

    // AC3: each session lists both editors.
    for page in [&ada, &linus] {
        let roster: i64 = page
            .evaluate("document.querySelectorAll('#roster li').length")
            .await
            .expect("read the roster")
            .into_value()
            .expect("the roster count is a number");
        assert_eq!(roster, 2, "both editors are listed");
    }

    for page in [&ada, &linus] {
        page.expect_no_console_errors()
            .await
            .expect("no console errors while editing");
    }
}

async fn editor_value(page: &autumn_web::system_test::Page) -> String {
    page.evaluate("document.getElementById('editor').value")
        .await
        .expect("read the editor")
        .into_value::<String>()
        .unwrap_or_default()
}

/// Type at the very end of the textarea, the way a person would.
async fn type_at_end(page: &autumn_web::system_test::Page, text: &str) {
    let script = format!(
        "(() => {{ const e = document.getElementById('editor'); \
          e.focus(); e.setSelectionRange(e.value.length, e.value.length); \
          e.value = e.value + {text}; \
          e.dispatchEvent(new Event('input', {{ bubbles: true }})); return true; }})()",
        text = serde_json::to_string(text).expect("encode")
    );
    page.evaluate(&script).await.expect("type at the end");
}

/// Type at the very start, which is what makes the other session's index
/// stale — the case an index-based protocol gets wrong.
async fn type_at_start(page: &autumn_web::system_test::Page, text: &str) {
    let script = format!(
        "(() => {{ const e = document.getElementById('editor'); \
          e.focus(); e.setSelectionRange(0, 0); \
          e.value = {text} + e.value; \
          e.dispatchEvent(new Event('input', {{ bubbles: true }})); return true; }})()",
        text = serde_json::to_string(text).expect("encode")
    );
    page.evaluate(&script).await.expect("type at the start");
}

/// Poll the editor until it holds `needle`, then return its text.
async fn settle(page: &autumn_web::system_test::Page, needle: &str) -> String {
    for _ in 0..60 {
        let value = editor_value(page).await;
        if value.contains(needle) && value.contains("TODO: ") {
            return value;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("the editor never settled on the merged text (waiting for {needle:?})");
}
