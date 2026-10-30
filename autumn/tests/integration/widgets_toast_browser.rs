//! Toast behaviour in a real browser (issue #3399).
//!
//! The tests check that hover and focus pause dismissal, that the close
//! button removes the toast and moves focus correctly, that a custom
//! duration applies, and that toasts show above a modal `<dialog>`.
//!
//! Timers are read from the `autumn-toast-out` animation `currentTime`. A
//! paused timer does not advance.
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
use autumn_web::system_test::{Page, SystemTest, SystemTestError, SystemTestRunner};
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
        button id="modal-own" hx-get="/toast/modal-own" hx-swap="none" { "Toast in the modal" }
        // A region in the modal is not inert while the modal is open.
        (toast_region("modal-toasts"))
    };
    layout(&html! {
        h1 id="title" { "Toasts" }
        button id="two" hx-get="/toast/two" hx-swap="none" { "Two toasts" }
        (toast_region(DEFAULT_TOAST_REGION_ID))
        button id="short" hx-get="/toast/short" hx-swap="none" { "Short toast" }
        button id="pair" hx-get="/toast/pair" hx-swap="none" { "Two timed toasts" }
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

/// The dwell of the timed test toasts. Long, so a slow runner still acts
/// before the toast fades.
const DWELL: Duration = Duration::from_secs(4);

#[get("/toast/short")]
async fn short() -> Markup {
    toast_with(
        "Short",
        AlertVariant::Success,
        &ToastConfig::new().duration(DWELL),
    )
}

#[get("/toast/pair")]
async fn pair() -> Markup {
    let config = ToastConfig::new().duration(DWELL);
    html! {
        (toast_with("One", AlertVariant::Success, &config))
        (toast_with("Two", AlertVariant::Info, &config))
    }
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

#[get("/toast/modal-own")]
async fn modal_own() -> Markup {
    let config = sticky().region("modal-toasts");
    toast_with("In the modal", AlertVariant::Success, &config)
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
                pair,
                custom,
                sticky_toast,
                from_modal,
                modal_own
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

/// The exit-animation times (ms) of the toasts in the region, as one string.
async fn timers(page: &Page) -> String {
    eval_string(
        page,
        "[...document.querySelectorAll('#toast-region [data-autumn-toast]')].map(t => { \
         const a = t.getAnimations().find(a => a.animationName === 'autumn-toast-out'); \
         return a ? String(Math.round(a.currentTime)) : 'none'; }).join(',')",
    )
    .await
}

/// Wait until every exit animation in the region is paused. A CSS pause
/// takes effect at the next frame.
async fn wait_paused(page: &Page) {
    wait_for(
        page,
        "[...document.querySelectorAll('#toast-region [data-autumn-toast]')].every(t => \
         t.getAnimations().filter(a => a.animationName === 'autumn-toast-out') \
         .every(a => a.playState === 'paused' && !a.pending))",
    )
    .await;
}

/// Wait up to 15 s until the region has no toast.
async fn wait_until_empty(page: &Page) {
    for _ in 0..300 {
        if count(page).await == "0" {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the toasts did not go");
}

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
    page.click("#pair").await.expect("click");
    wait_for(
        &page,
        "document.querySelectorAll('#toast-region [data-autumn-toast]').length === 2",
    )
    .await;
    // Hover the first toast. The second toast pauses too.
    page.hover(FIRST_TOAST).await.expect("hover");
    wait_paused(&page).await;
    let before = timers(&page).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(timers(&page).await, before, "hover pauses both timers");
    assert_eq!(count(&page).await, "2", "hovered toasts stay");

    // Move away: the timers run again and the runtime removes the toasts.
    page.hover("#title").await.expect("hover away");
    wait_until_empty(&page).await;
    page.expect_no_console_errors().await.expect("no errors");
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn a_toast_that_arrives_during_hover_still_fades_in() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/").await;
    page.click("#sticky").await.expect("click");
    wait_for(
        &page,
        "!!document.querySelector('#toast-region [data-autumn-toast]')",
    )
    .await;
    page.hover(FIRST_TOAST).await.expect("hover");
    // A new toast arrives while the region is hovered. The pause must not
    // hold its entrance at `opacity: 0`.
    assert!(
        eval_bool(
            &page,
            "(() => { htmx.ajax('GET', '/toast/short', { swap: 'none' }); return true; })()"
        )
        .await
    );
    wait_for(
        &page,
        "(() => { const t = document.querySelectorAll('#toast-region [data-autumn-toast]')[1]; \
         return !!t && getComputedStyle(t).opacity === '1'; })()",
    )
    .await;
    wait_paused(&page).await;
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn a_restored_message_id_is_not_reused() {
    // An htmx history restore can bring back a toast with an id that the
    // runtime counter gives out again after a reload.
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/").await;
    assert!(
        eval_bool(
            &page,
            "(() => { for (let i = 1; i <= 100; i++) { const old = document.createElement('span'); \
             old.id = 'autumn-toast-' + i; document.body.prepend(old); } return true; })()"
        )
        .await
    );
    page.click("#sticky").await.expect("click");
    wait_for(
        &page,
        "(() => { const b = document.querySelector('#toast-region [data-autumn-toast-close]'); \
         return !!b && !b.hidden; })()",
    )
    .await;
    assert_eq!(
        eval_string(
            &page,
            "document.getElementById(document.querySelector('#toast-region \
             [data-autumn-toast-close]').getAttribute('aria-describedby')).textContent"
        )
        .await,
        "Sticky",
        "the close button describes its own message"
    );
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn a_toast_removed_after_the_history_copy_does_not_hang() {
    // htmx saves the history copy, then removes the old page before the
    // runtime's microtask runs. A removed toast must not freeze the tab.
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
    let save_then_remove = eval_bool(
        &page,
        "(() => { document.body.dispatchEvent(new CustomEvent('htmx:beforeHistorySave', \
         { bubbles: true })); \
         document.querySelector('#toast-region [data-autumn-toast]').remove(); return true; })()",
    );
    assert!(
        tokio::time::timeout(Duration::from_secs(5), save_then_remove)
            .await
            .unwrap_or(false),
        "the page froze"
    );
    assert!(
        tokio::time::timeout(Duration::from_secs(5), eval_bool(&page, "true"))
            .await
            .unwrap_or(false),
        "the page froze"
    );
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
    wait_paused(&page).await;
    let before = timers(&page).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(timers(&page).await, before, "focus pauses the timer");

    // Focus goes out of the toast: the timer runs again.
    assert!(
        eval_bool(
            &page,
            "(() => { const b = document.getElementById('short'); b.focus(); \
             return document.activeElement === b; })()"
        )
        .await
    );
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_ne!(timers(&page).await, before, "the timer runs again");
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
    // The button names its own message.
    assert_eq!(
        eval_string(
            &page,
            "document.getElementById(document.activeElement.getAttribute('aria-describedby')) \
             .textContent"
        )
        .await,
        "First"
    );
    page.press("Tab").await.expect("tab");
    assert_eq!(active_text(&page).await, "close:Second");

    // Closing the last toast in the list moves focus to the one before.
    page.press("Enter").await.expect("close second");
    assert_eq!(count(&page).await, "1");
    assert_eq!(active_text(&page).await, "close:First");

    // Closing the last toast returns focus to where the user was.
    page.press(" ").await.expect("close first");
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
    page.click("#two").await.expect("click");
    wait_for(
        &page,
        "[...document.querySelectorAll('#toast-region [data-autumn-toast-close]')] \
         .filter(b => !b.hidden).length === 2",
    )
    .await;
    page.click("#toast-region [data-autumn-toast-close]")
        .await
        .expect("close");
    assert_eq!(count(&page).await, "1");
    // Focus in a toast pauses every timer, so a mouse close does not move
    // focus to the next toast. It goes back to the origin.
    assert_eq!(active_text(&page).await, "two");
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
    page.click("#custom").await.expect("toast");
    wait_for(
        &page,
        "!!document.querySelector('#toast-region [data-autumn-toast]')",
    )
    .await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    let before: u64 = timers(&page).await.parse().expect("one timer");
    assert!(before >= 900, "the timer runs: {before}");

    page.click("[data-modal-open='dlg']").await.expect("open");
    wait_for(&page, "document.getElementById('dlg').matches(':modal')").await;
    // The modal makes the page inert, so `elementFromPoint` cannot see the
    // toast. `expect_on_top` hit-tests through CDP instead.
    page.expect_on_top(FIRST_TOAST).await.expect("toast on top");
    // Moving the region up must not restart the timer.
    let after = timers(&page).await;
    let after: u64 = after
        .parse()
        .unwrap_or_else(|_| panic!("one timer: {after:?}"));
    assert!(after >= before, "the timer restarted: {before} -> {after}");
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn a_region_in_the_modal_is_above_it_and_usable() {
    const OWN: &str = "#modal-toasts [data-autumn-toast]";
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/").await;
    page.click("[data-modal-open='dlg']").await.expect("open");
    wait_for(&page, "document.getElementById('dlg').matches(':modal')").await;
    page.click("#modal-own").await.expect("toast");
    wait_for(&page, &format!("!!document.querySelector({OWN:?})")).await;
    page.expect_on_top(OWN).await.expect("toast on top");
    // Not inert: a real hit test finds it, and its close button works.
    assert!(
        eval_bool(
            &page,
            &format!(
                "(() => {{ const t = document.querySelector({OWN:?}); \
                 const r = t.getBoundingClientRect(); \
                 return t.contains(document.elementFromPoint(r.left + r.width / 2, \
                 r.top + r.height / 2)); }})()"
            ),
        )
        .await,
        "the toast in the modal is not inert"
    );
    page.click("#modal-toasts [data-autumn-toast-close]")
        .await
        .expect("close");
    wait_for(&page, &format!("!document.querySelector({OWN:?})")).await;
    page.expect_no_console_errors().await.expect("no errors");
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
        matches!(
            page.expect_on_top("[data-autumn-toast]").await,
            Err(SystemTestError::AssertionFailed { .. })
        ),
        "the modal backdrop must cover a toast that is not in the top layer"
    );
}
