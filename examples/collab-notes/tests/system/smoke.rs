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

/// A keystroke typed during an outstanding round trip survives (issue #2843).
///
/// The report's repro is a race: a character typed while the one before it is
/// still in flight. Typing the characters in one JavaScript tick makes that
/// race certain instead of intermittent, because no echo can arrive between
/// them. `type_burst` models what a browser does with a read-only textarea —
/// the keystroke changes nothing and sends no `input` event — so the test
/// cannot type through a lock a person cannot type through.
#[tokio::test]
#[ignore = "requires Chromium — set AUTUMN_CHROMIUM or install chromium-browser"]
async fn a_keystroke_typed_during_a_round_trip_survives() {
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
    ada.visit("/notes/2").await.expect("visit the note");
    ada.expect_text("Release notes")
        .await
        .expect("the editor renders");
    ada.expect_text("connected")
        .await
        .expect("the collaboration socket connects");

    let seeded = editor_value(&ada).await;
    assert!(
        seeded.contains("Autumn ships"),
        "seeded body is rendered: {seeded:?}"
    );

    // Hebrew because the report reproduces with it most often. The mechanism
    // is not script-specific: the space between the two words is the
    // character that goes missing.
    let sample = "ש ש";
    let landed = type_burst(&ada, sample).await;
    assert_eq!(
        landed,
        sample.chars().count() as i64,
        "the editor accepted every keystroke of {sample:?}"
    );

    let want = format!("{seeded}{sample}");
    let ada_text = settle_on(&ada, &want).await;
    assert_eq!(ada_text, want, "no keystroke was lost");

    // A second session proves the characters reached the server, rather than
    // only the first textarea.
    let linus = runner.page().await.expect("open Linus's page");
    linus.visit("/notes/2").await.expect("visit the note");
    linus
        .expect_text("connected")
        .await
        .expect("the second socket connects");
    let linus_text = settle_on(&linus, &want).await;
    assert_eq!(linus_text, want, "the second session sees every character");

    for page in [&ada, &linus] {
        page.expect_no_console_errors()
            .await
            .expect("no console errors while editing");
    }
}

/// Type every character in one tick, the way a fast typist outruns the round
/// trip. Returns how many keystrokes the textarea accepted.
async fn type_burst(page: &autumn_web::system_test::Page, text: &str) -> i64 {
    let script = format!(
        "(() => {{ const e = document.getElementById('editor'); \
          e.focus(); e.setSelectionRange(e.value.length, e.value.length); \
          let landed = 0; \
          for (const ch of {text}) {{ \
            if (e.readOnly || e.disabled) continue; \
            const at = e.selectionStart; \
            e.value = e.value.slice(0, at) + ch + e.value.slice(e.selectionEnd); \
            e.setSelectionRange(at + ch.length, at + ch.length); \
            e.dispatchEvent(new Event('input', {{ bubbles: true }})); \
            landed += 1; \
          }} \
          return landed; }})()",
        text = serde_json::to_string(text).expect("encode")
    );
    page.evaluate(&script)
        .await
        .expect("type a burst")
        .into_value()
        .expect("the keystroke count is a number")
}

/// Poll the editor until it holds exactly `want`, then return its text. On
/// timeout, return what it really holds so the assertion can report it.
async fn settle_on(page: &autumn_web::system_test::Page, want: &str) -> String {
    let mut value = String::new();
    for _ in 0..60 {
        value = editor_value(page).await;
        if value == want {
            return value;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    value
}

/// A backspace during an outstanding round trip survives too (issue #2843).
///
/// The character it erases is still in flight, so the client cannot name it
/// yet. It marks the character and deletes it when the echo names it.
#[tokio::test]
#[ignore = "requires Chromium — set AUTUMN_CHROMIUM or install chromium-browser"]
async fn a_backspace_typed_during_a_round_trip_survives() {
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
    ada.visit("/notes/2").await.expect("visit the note");
    ada.expect_text("connected")
        .await
        .expect("the collaboration socket connects");

    let seeded = editor_value(&ada).await;

    // Type two characters and erase one, all before any echo can arrive.
    let landed = type_burst(&ada, "no").await;
    assert_eq!(landed, 2, "the editor accepted both keystrokes");
    let after_erase = backspace_burst(&ada, 1).await;
    assert_eq!(after_erase, 1, "the editor accepted the backspace");

    let want = format!("{seeded}n");
    let ada_text = settle_on(&ada, &want).await;
    assert_eq!(ada_text, want, "the erased character stayed erased");

    let linus = runner.page().await.expect("open Linus's page");
    linus.visit("/notes/2").await.expect("visit the note");
    linus
        .expect_text("connected")
        .await
        .expect("the second socket connects");
    let linus_text = settle_on(&linus, &want).await;
    assert_eq!(linus_text, want, "the server holds the same text");

    for page in [&ada, &linus] {
        page.expect_no_console_errors()
            .await
            .expect("no console errors while editing");
    }
}

/// Press Backspace `times` in one tick. Returns how many the textarea took.
async fn backspace_burst(page: &autumn_web::system_test::Page, times: usize) -> i64 {
    let script = format!(
        "(() => {{ const e = document.getElementById('editor'); \
          e.focus(); \
          let landed = 0; \
          for (let i = 0; i < {times}; i += 1) {{ \
            if (e.readOnly || e.disabled) continue; \
            const at = e.selectionStart; \
            if (at === 0) continue; \
            e.value = e.value.slice(0, at - 1) + e.value.slice(e.selectionEnd); \
            e.setSelectionRange(at - 1, at - 1); \
            e.dispatchEvent(new Event('input', {{ bubbles: true }})); \
            landed += 1; \
          }} \
          return landed; }})()"
    );
    page.evaluate(&script)
        .await
        .expect("press backspace")
        .into_value()
        .expect("the keystroke count is a number")
}
