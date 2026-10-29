//! Toast behaviour in a real browser (issue #3399).
//!
//! The tests check that hover and focus pause dismissal, that the close
//! button removes the toast and moves focus correctly, that a custom
//! duration applies, and that toasts show above a modal `<dialog>`.
//!
//! Run:
//!
//! ```text
//! cargo test -p autumn-web --features system-tests \
//!   --test integration_tests widgets_toast_browser -- --include-ignored
//! ```

#![cfg(all(feature = "system-tests", feature = "maud"))]

use std::time::Duration;

use autumn_web::prelude::*;
use autumn_web::system_test::{Page, SystemTest, SystemTestRunner};
use autumn_web::widgets::{
    AlertVariant, DEFAULT_TOAST_REGION_ID, ModalConfig, ToastConfig, modal, modal_trigger,
    toast_region, toast_with,
};
use maud::{DOCTYPE, Markup, html};

fn layout(body: &Markup) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                title { "Toasts" }
                link rel="stylesheet" href="/static/css/autumn-widgets.css";
                script src="/static/js/htmx.min.js" defer {}
                script src="/static/js/autumn-widgets.js" defer {}
            }
            body { (body) }
        }
    }
}

#[get("/")]
async fn index() -> Markup {
    let in_modal = html! {
        button id="modal-toast" hx-get="/toast/modal" hx-swap="none" { "Toast from the modal" }
    };
    layout(&html! {
        h1 id="title" { "Toasts" }
        button id="two" hx-get="/toast/two" hx-swap="none" { "Two toasts" }
        (toast_region(DEFAULT_TOAST_REGION_ID))
        button id="short" hx-get="/toast/short" hx-swap="none" { "Short toast" }
        button id="custom" hx-get="/toast/custom" hx-swap="none" { "Custom toast" }
        button id="sticky" hx-get="/toast/sticky" hx-swap="none" { "Sticky toast" }
        (modal_trigger("Open modal", "dlg", None))
        (modal("dlg", "A modal", &in_modal, &ModalConfig::new()))
    })
}

/// A page with one inline toast, for the no-JS check.
#[get("/inline")]
async fn inline() -> Markup {
    layout(&html! {
        h1 { "Inline" }
        (toast_region(DEFAULT_TOAST_REGION_ID))
        (toast_with("Saved", AlertVariant::Success, &ToastConfig::new()))
    })
}

/// A toast and a modal with no `autumn-widgets.js`, so nothing moves the
/// region into the top layer. The canary for `expect_on_top`.
#[get("/no-runtime")]
async fn no_runtime() -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                title { "No runtime" }
                link rel="stylesheet" href="/static/css/autumn-widgets.css";
            }
            body {
                (toast_region(DEFAULT_TOAST_REGION_ID))
                (modal("dlg", "A modal", &html! { p { "Body" } }, &ModalConfig::new()))
                (toast_with("Covered", AlertVariant::Info, &sticky()))
            }
        }
    }
}

fn sticky() -> ToastConfig<'static> {
    ToastConfig::new().persistent(true)
}

#[get("/toast/two")]
async fn two() -> Markup {
    html! {
        (toast_with("First", AlertVariant::Info, &sticky()))
        (toast_with("Second", AlertVariant::Error, &sticky()))
    }
}

#[get("/toast/short")]
async fn short() -> Markup {
    let config = ToastConfig::new().duration(Duration::from_millis(1_200));
    toast_with("Short", AlertVariant::Success, &config)
}

#[get("/toast/custom")]
async fn custom() -> Markup {
    let config = ToastConfig::new().duration(Duration::from_millis(8_250));
    toast_with("Custom", AlertVariant::Info, &config)
}

#[get("/toast/sticky")]
async fn sticky_toast() -> Markup {
    toast_with("Sticky", AlertVariant::Warning, &sticky())
}

#[get("/toast/modal")]
async fn from_modal() -> Markup {
    toast_with("From the modal", AlertVariant::Success, &sticky())
}

async fn runner() -> SystemTestRunner {
    Box::pin(
        SystemTest::new()
            .routes(routes![
                index,
                inline,
                no_runtime,
                two,
                short,
                custom,
                sticky_toast,
                from_modal
            ])
            .build(),
    )
    .await
    .expect("start runner")
}

async fn eval_string(page: &Page, js: &str) -> String {
    page.evaluate(js)
        .await
        .expect("evaluate")
        .into_value::<String>()
        .unwrap_or_default()
}

async fn eval_bool(page: &Page, js: &str) -> bool {
    page.evaluate(js)
        .await
        .ok()
        .and_then(|r| r.into_value::<bool>().ok())
        .unwrap_or(false)
}

/// Poll `js` until it is true. Return `false` after 5 s.
async fn poll(page: &Page, js: &str) -> bool {
    for _ in 0..100 {
        if eval_bool(page, js).await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

async fn wait_for(page: &Page, js: &str) {
    assert!(poll(page, js).await, "timed out waiting for `{js}`");
}

async fn open(page: &Page, path: &str) {
    page.visit(path).await.expect("visit");
    wait_for(page, "window.__autumnWidgets === true").await;
    wait_for(page, "document.readyState === 'complete'").await;
}

/// Count the toasts in the region.
async fn count(page: &Page) -> String {
    eval_string(
        page,
        "String(document.querySelectorAll('#toast-region [data-autumn-toast]').length)",
    )
    .await
}

async fn active_text(page: &Page) -> String {
    eval_string(
        page,
        "(() => { const a = document.activeElement; \
         if (!a) return ''; \
         const t = a.closest('[data-autumn-toast]'); \
         return t ? 'close:' + t.textContent.replace('×', '').trim() : a.id; })()",
    )
    .await
}

/// The first toast in the region.
const FIRST_TOAST: &str = "#toast-region [data-autumn-toast]";

#[tokio::test]
#[ignore = "requires Chromium"]
async fn region_opens_in_the_top_layer() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/").await;
    wait_for(
        &page,
        "document.getElementById('toast-region').matches(':popover-open')",
    )
    .await;
    page.expect_no_console_errors().await.expect("no errors");
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn hover_pauses_dismissal() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/").await;
    page.click("#short").await.expect("click");
    wait_for(
        &page,
        "!!document.querySelector('#toast-region [data-autumn-toast]')",
    )
    .await;
    page.hover("#toast-region [data-autumn-toast]")
        .await
        .expect("hover");

    // Hold the pointer for twice the 1.2 s dwell.
    tokio::time::sleep(Duration::from_millis(2_400)).await;
    assert_eq!(count(&page).await, "1", "a hovered toast stays");
    assert!(
        eval_bool(
            &page,
            "document.querySelector('#toast-region [data-autumn-toast]') \
             .getAnimations().every(a => a.playState === 'paused' || a.playState === 'finished')",
        )
        .await,
        "the timer is paused"
    );

    // Move away: the timer runs again and the runtime removes the toast.
    page.hover("#title").await.expect("hover away");
    wait_for(
        &page,
        "document.querySelectorAll('#toast-region [data-autumn-toast]').length === 0",
    )
    .await;
    page.expect_no_console_errors().await.expect("no errors");
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn focus_within_pauses_dismissal() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/").await;
    page.click("#short").await.expect("click");
    wait_for(
        &page,
        "(() => { const b = document.querySelector('#toast-region [data-autumn-toast-close]'); \
         if (!b || b.hidden) return false; b.focus(); return document.activeElement === b; })()",
    )
    .await;
    tokio::time::sleep(Duration::from_millis(2_400)).await;
    assert_eq!(count(&page).await, "1", "a toast with focus stays");
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn close_button_removes_the_toast_and_returns_focus() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/").await;
    assert!(
        eval_bool(
            &page,
            "(() => { const b = document.getElementById('two'); b.focus(); \
             return document.activeElement === b; })()"
        )
        .await
    );
    page.press("Enter").await.expect("press");
    wait_for(
        &page,
        "[...document.querySelectorAll('#toast-region [data-autumn-toast-close]')] \
         .filter(b => !b.hidden).length === 2",
    )
    .await;

    // The region follows the trigger in the DOM, so Tab reaches the toasts.
    page.press("Tab").await.expect("tab");
    assert_eq!(active_text(&page).await, "close:First");
    assert_eq!(
        eval_string(&page, "document.activeElement.getAttribute('aria-label')").await,
        "Dismiss notification"
    );

    // Closing one toast moves focus to the next toast.
    page.press("Enter").await.expect("close first");
    assert_eq!(count(&page).await, "1");
    assert_eq!(active_text(&page).await, "close:Second");

    // Closing the last toast returns focus to where the user was.
    page.press(" ").await.expect("close second");
    assert_eq!(count(&page).await, "0");
    assert_eq!(active_text(&page).await, "two");
    page.expect_no_console_errors().await.expect("no errors");
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn a_mouse_click_on_close_removes_the_toast() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/").await;
    page.click("#sticky").await.expect("click");
    wait_for(
        &page,
        "(() => { const b = document.querySelector('#toast-region [data-autumn-toast-close]'); \
         return !!b && !b.hidden; })()",
    )
    .await;
    page.click("#toast-region [data-autumn-toast-close]")
        .await
        .expect("close");
    assert_eq!(count(&page).await, "0");
    assert_eq!(active_text(&page).await, "sticky");
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn custom_duration_sets_the_dwell() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/").await;
    page.click("#custom").await.expect("click");
    wait_for(
        &page,
        "(() => { const t = document.querySelector('#toast-region [data-autumn-toast]'); \
         return !!t && getComputedStyle(t).animationDelay.split(', ')[1] === '8.25s'; })()",
    )
    .await;
    page.expect_no_console_errors().await.expect("no errors");
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn persistent_toast_has_no_timer() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/").await;
    page.click("#sticky").await.expect("click");
    wait_for(
        &page,
        "!!document.querySelector('#toast-region [data-autumn-toast]')",
    )
    .await;
    assert!(
        eval_bool(
            &page,
            "document.querySelector('#toast-region [data-autumn-toast]').getAnimations() \
             .every(a => a.animationName !== 'autumn-toast-out')",
        )
        .await,
        "no exit animation"
    );
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn a_toast_from_an_open_modal_shows_above_it() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/").await;
    page.click("[data-modal-open='dlg']").await.expect("open");
    wait_for(&page, "document.getElementById('dlg').matches(':modal')").await;
    page.click("#modal-toast").await.expect("toast");
    wait_for(
        &page,
        "!!document.querySelector('#toast-region [data-autumn-toast]')",
    )
    .await;
    // The modal makes the page inert, so `elementFromPoint` cannot see the
    // toast. `expect_on_top` hit-tests through CDP instead.
    page.expect_on_top(FIRST_TOAST).await.expect("toast on top");
    page.expect_no_console_errors().await.expect("no errors");
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn a_modal_opened_later_does_not_cover_the_toast() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/").await;
    page.click("#sticky").await.expect("toast");
    wait_for(
        &page,
        "!!document.querySelector('#toast-region [data-autumn-toast]')",
    )
    .await;
    // The entrance animation must not run again when the region moves up.
    wait_for(
        &page,
        "document.querySelector('#toast-region [data-autumn-toast]').getAnimations() \
         .every(a => a.playState === 'finished')",
    )
    .await;
    page.click("[data-modal-open='dlg']").await.expect("open");
    wait_for(&page, "document.getElementById('dlg').matches(':modal')").await;
    // The modal makes the page inert, so `elementFromPoint` cannot see the
    // toast. `expect_on_top` hit-tests through CDP instead.
    page.expect_on_top(FIRST_TOAST).await.expect("toast on top");
    assert!(
        eval_bool(
            &page,
            "document.querySelector('#toast-region [data-autumn-toast]').getAnimations() \
             .every(a => a.playState === 'finished')",
        )
        .await,
        "the toast animation did not restart"
    );
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn without_js_toasts_show_and_the_close_button_stays_hidden() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    page.set_javascript_enabled(false).await.expect("js off");
    page.visit("/inline").await.expect("visit");
    assert_eq!(
        eval_string(
            &page,
            "getComputedStyle(document.getElementById('toast-region')).display"
        )
        .await,
        "flex",
        "the region shows without JS"
    );
    assert!(
        eval_bool(
            &page,
            "(() => { const t = document.querySelector('[data-autumn-toast]'); \
             return getComputedStyle(t).display !== 'none' && \
             getComputedStyle(t.querySelector('.autumn-toast__close')).display === 'none'; })()"
        )
        .await,
        "the toast shows and its dead close button does not"
    );
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn the_top_layer_check_fails_without_the_runtime() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    page.visit("/no-runtime").await.expect("visit");
    assert!(
        eval_bool(
            &page,
            "(() => { document.getElementById('dlg').showModal(); \
             return document.getElementById('dlg').matches(':modal'); })()"
        )
        .await
    );
    assert!(
        page.expect_on_top("[data-autumn-toast]").await.is_err(),
        "the modal backdrop must cover a toast that is not in the top layer"
    );
}
