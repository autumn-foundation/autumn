//! Popover and dropdown menu in a real browser (issue #3402).
//!
//! The tests check opening and closing with the mouse and the keyboard,
//! the APG menu-button keys, focus return to the trigger, anchor
//! positioning (side, align, flip, right-to-left) and the page without the
//! runtime script.
//!
//! Run:
//!
//! ```text
//! cargo test -p autumn-web --features system-tests \
//!   --test integration_tests widgets_popover_browser -- --include-ignored
//! ```

#![cfg(all(feature = "system-tests", feature = "maud"))]

use std::time::Duration;

use autumn_web::prelude::*;
use autumn_web::system_test::{Page, SystemTest, SystemTestRunner};
use autumn_web::widgets::{
    MenuEntry, PopoverAlign, PopoverConfig, PopoverSide, dropdown_menu, popover,
};
use http::Method;
use maud::{DOCTYPE, Markup, html};

fn layout(dir: &str, body: &Markup) -> Markup {
    page(dir, body, true)
}

/// `runtime` off is the no-JS case. `Page::click` and `Page::press` wait on
/// a script, so a test cannot turn JavaScript off and still send input.
fn page(dir: &str, body: &Markup, runtime: bool) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" dir=(dir) {
            head {
                meta charset="utf-8";
                title { "Popover" }
                link rel="stylesheet" href="/static/css/autumn-widgets.css";
                @if runtime { script src="/static/js/autumn-widgets.js" defer {} }
            }
            body { (body) }
        }
    }
}

fn actions() -> Markup {
    dropdown_menu(
        "Actions",
        &[
            MenuEntry::link("Edit", "#edit"),
            MenuEntry::link("Duplicate", "#duplicate"),
            MenuEntry::separator(),
            MenuEntry::action("Archive", "/echo", Method::GET, "").destructive(),
        ],
        &PopoverConfig::new("actions"),
    )
}

fn index_body() -> Markup {
    html! {
        h1 id="title" { "Popover" }
        (popover(
            "Help",
            &html! { p { "Help text" } button id="inner" type="button" { "Inner" } },
            &PopoverConfig::new("help"),
        ))
        (actions())
        button id="after" type="button" { "After" }
    }
}

#[get("/")]
async fn index() -> Markup {
    layout("ltr", &index_body())
}

/// The same page with no `autumn-widgets.js`.
#[get("/bare")]
async fn bare() -> Markup {
    page("ltr", &index_body(), false)
}

/// One popover per side and align, in the middle of the page.
#[get("/pos")]
async fn pos() -> Markup {
    let cell = |id: &str, side, align| {
        popover(
            id,
            &html! { p { "Body" } },
            &PopoverConfig::new(id).side(side).align(align),
        )
    };
    layout(
        "ltr",
        &html! {
            @for _ in 0..12 { br; }
            div id="grid" {
                (cell("p-bottom-start", PopoverSide::Bottom, PopoverAlign::Start))
                (cell("p-bottom-center", PopoverSide::Bottom, PopoverAlign::Center))
                (cell("p-bottom-end", PopoverSide::Bottom, PopoverAlign::End))
                (cell("p-top-start", PopoverSide::Top, PopoverAlign::Start))
                (cell("p-end-start", PopoverSide::End, PopoverAlign::Start))
                (cell("p-start-start", PopoverSide::Start, PopoverAlign::Start))
            }
        },
    )
}

/// A trigger at the bottom edge: the panel must flip above it.
#[get("/flip")]
async fn flip() -> Markup {
    layout(
        "ltr",
        &html! {
            @for _ in 0..300 { br; }
            (popover(
                "Low",
                &html! { p { "Body" } p { "Body" } p { "Body" } },
                &PopoverConfig::new("low"),
            ))
            @for _ in 0..300 { br; }
        },
    )
}

#[get("/rtl")]
async fn rtl() -> Markup {
    layout(
        "rtl",
        &html! {
            @for _ in 0..12 { br; }
            (popover(
                "Start",
                &html! { p { "Body" } },
                &PopoverConfig::new("rtl").side(PopoverSide::End),
            ))
        },
    )
}

#[get("/echo")]
async fn echo() -> &'static str {
    "echoed"
}

async fn runner() -> SystemTestRunner {
    Box::pin(
        SystemTest::new()
            .routes(routes![index, bare, pos, flip, rtl, echo])
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
        .and_then(|v| v.into_value::<bool>().ok())
        .unwrap_or(false)
}

async fn wait_for(page: &Page, js: &str) {
    for _ in 0..100 {
        if eval_bool(page, js).await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for `{js}`");
}

async fn open(page: &Page, path: &str) {
    page.visit(path).await.expect("visit");
    wait_for(page, "document.readyState === 'complete'").await;
    wait_for(page, "window.__autumnWidgets === true").await;
}

async fn press(page: &Page, key: &str) {
    page.press(key).await.expect("press");
}

async fn focus(page: &Page, selector: &str) {
    assert!(
        eval_bool(
            page,
            &format!(
                "(() => {{ const e = document.querySelector({selector:?}); \
                 if (!e) return false; e.focus(); return document.activeElement === e; }})()"
            ),
        )
        .await,
        "could not focus {selector}"
    );
}

/// The runtime reacts to the `toggle` event, which the browser queues as a
/// task. Let it run before the test reads the page.
async fn settle() {
    tokio::time::sleep(Duration::from_millis(150)).await;
}

async fn attr(page: &Page, selector: &str, name: &str) -> String {
    settle().await;
    eval_string(
        page,
        &format!(
            "(() => {{ const e = document.querySelector({selector:?}); \
             return e ? (e.getAttribute({name:?}) || '') : ''; }})()"
        ),
    )
    .await
}

async fn is_open(page: &Page, selector: &str) -> bool {
    settle().await;
    eval_bool(
        page,
        &format!("document.querySelector({selector:?}).matches(':popover-open')"),
    )
    .await
}

async fn active_id(page: &Page) -> String {
    settle().await;
    eval_string(
        page,
        "document.activeElement ? document.activeElement.id : ''",
    )
    .await
}

async fn active_text(page: &Page) -> String {
    settle().await;
    eval_string(
        page,
        "document.activeElement ? document.activeElement.textContent.trim() : ''",
    )
    .await
}

/// The panel and trigger boxes: `[trigger, panel]` as `(left, top, right, bottom)`.
async fn boxes(page: &Page, id: &str) -> [(f64, f64, f64, f64); 2] {
    let js = format!(
        "(() => {{ const f = r => [r.left, r.top, r.right, r.bottom]; \
         return JSON.stringify([f(document.getElementById('{id}-trigger').getBoundingClientRect()), \
         f(document.getElementById('{id}').getBoundingClientRect())]); }})()"
    );
    let v: Vec<[f64; 4]> = serde_json::from_str(&eval_string(page, &js).await).expect("rects");
    [
        (v[0][0], v[0][1], v[0][2], v[0][3]),
        (v[1][0], v[1][1], v[1][2], v[1][3]),
    ]
}

async fn open_and_measure(page: &Page, id: &str) -> [(f64, f64, f64, f64); 2] {
    // An open panel can cover the next trigger. Close it first.
    page.evaluate("document.querySelectorAll(':popover-open').forEach(p => p.hidePopover())")
        .await
        .expect("close");
    page.click(&format!("#{id}-trigger")).await.expect("open");
    wait_for(
        page,
        &format!("document.getElementById('{id}').matches(':popover-open')"),
    )
    .await;
    boxes(page, id).await
}

const EPS: f64 = 2.0;

// ── popover: mouse and keyboard ─────────────────────────────────────────

#[tokio::test]
#[ignore = "requires Chromium"]
async fn popover_opens_and_closes_with_the_mouse() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/").await;

    assert!(!is_open(&page, "#help").await, "closed at first");
    page.click("#help-trigger").await.expect("open");
    assert!(is_open(&page, "#help").await, "a click opens it");
    page.click("#help-trigger").await.expect("toggle");
    assert!(!is_open(&page, "#help").await, "a second click closes it");

    page.click("#help-trigger").await.expect("open");
    page.click("#title").await.expect("click outside");
    assert!(!is_open(&page, "#help").await, "light dismiss closes it");

    page.click("#help-trigger").await.expect("open");
    page.click("#inner").await.expect("click inside");
    assert!(
        is_open(&page, "#help").await,
        "a click inside keeps it open"
    );
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn popover_opens_and_closes_with_the_keyboard_and_returns_focus() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/").await;

    focus(&page, "#help-trigger").await;
    press(&page, "Enter").await;
    assert!(is_open(&page, "#help").await, "Enter opens it");
    press(&page, "Escape").await;
    assert!(!is_open(&page, "#help").await, "Escape closes it");
    assert_eq!(active_id(&page).await, "help-trigger", "focus returns");

    press(&page, " ").await;
    assert!(is_open(&page, "#help").await, "Space opens it");
    focus(&page, "#inner").await;
    press(&page, "Escape").await;
    assert!(!is_open(&page, "#help").await);
    assert_eq!(
        active_id(&page).await,
        "help-trigger",
        "focus returns from inside the panel"
    );
    page.expect_no_console_errors().await.expect("no errors");
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn opening_one_popover_closes_another() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/").await;
    page.click("#help-trigger").await.expect("open help");
    page.click("#actions-trigger").await.expect("open menu");
    assert!(!is_open(&page, "#help").await);
    assert!(is_open(&page, "#actions").await);
}

// ── popover: anchor positioning ─────────────────────────────────────────

#[tokio::test]
#[ignore = "requires Chromium"]
async fn popover_is_placed_by_side_and_align() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/pos").await;

    let [t, p] = open_and_measure(&page, "p-bottom-start").await;
    assert!(p.1 >= t.3 - EPS, "below the trigger: {t:?} {p:?}");
    assert!((p.0 - t.0).abs() <= EPS, "start edges meet: {t:?} {p:?}");

    let [t, p] = open_and_measure(&page, "p-bottom-end").await;
    assert!(p.1 >= t.3 - EPS, "below: {t:?} {p:?}");
    assert!((p.2 - t.2).abs() <= EPS, "end edges meet: {t:?} {p:?}");

    let [t, p] = open_and_measure(&page, "p-bottom-center").await;
    let (tc, pc) = (f64::midpoint(t.0, t.2), f64::midpoint(p.0, p.2));
    assert!((tc - pc).abs() <= EPS, "centered: {t:?} {p:?}");

    let [t, p] = open_and_measure(&page, "p-top-start").await;
    assert!(p.3 <= t.1 + EPS, "above the trigger: {t:?} {p:?}");

    let [t, p] = open_and_measure(&page, "p-end-start").await;
    assert!(p.0 >= t.2 - EPS, "after the trigger: {t:?} {p:?}");
    assert!((p.1 - t.1).abs() <= EPS, "top edges meet: {t:?} {p:?}");

    let [t, p] = open_and_measure(&page, "p-start-start").await;
    assert!(p.2 <= t.0 + EPS, "before the trigger: {t:?} {p:?}");
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn popover_flips_at_the_viewport_edge() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/flip").await;
    page.evaluate("document.getElementById('low-trigger').scrollIntoView({block: 'end'})")
        .await
        .expect("scroll");
    let [t, p] = open_and_measure(&page, "low").await;
    assert!(p.3 <= t.1 + EPS, "flipped above the trigger: {t:?} {p:?}");
    assert!(p.1 >= 0.0, "inside the viewport: {p:?}");
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn popover_end_side_follows_the_writing_direction() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/rtl").await;
    let [t, p] = open_and_measure(&page, "rtl").await;
    assert!(p.2 <= t.0 + EPS, "end is the left side in rtl: {t:?} {p:?}");
}

// ── dropdown menu: mouse ────────────────────────────────────────────────

#[tokio::test]
#[ignore = "requires Chromium"]
async fn menu_opens_and_closes_with_the_mouse() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/").await;

    assert_eq!(
        attr(&page, "#actions-trigger", "aria-expanded").await,
        "false"
    );
    page.click("#actions-trigger").await.expect("open");
    assert!(is_open(&page, "#actions").await);
    assert_eq!(
        attr(&page, "#actions-trigger", "aria-expanded").await,
        "true"
    );
    page.click("#title").await.expect("click outside");
    assert!(!is_open(&page, "#actions").await);
    assert_eq!(
        attr(&page, "#actions-trigger", "aria-expanded").await,
        "false"
    );

    page.click("#actions-trigger").await.expect("open");
    page.click("#actions a[href='#duplicate']")
        .await
        .expect("choose an item");
    assert!(
        !is_open(&page, "#actions").await,
        "choosing an item closes the menu"
    );
    assert_eq!(
        active_id(&page).await,
        "actions-trigger",
        "focus returns to the trigger"
    );
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn menu_form_item_submits() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/").await;
    page.click("#actions-trigger").await.expect("open");
    page.click("#actions button[role=menuitem]")
        .await
        .expect("submit");
    page.expect_url("/echo").await.expect("the form posted");
}

// ── dropdown menu: APG keyboard ─────────────────────────────────────────

#[tokio::test]
#[ignore = "requires Chromium"]
async fn menu_button_follows_the_apg_keyboard_model() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/").await;

    focus(&page, "#actions-trigger").await;
    press(&page, "Enter").await;
    assert!(is_open(&page, "#actions").await);
    assert_eq!(
        active_text(&page).await,
        "Edit",
        "Enter focuses the first item"
    );

    press(&page, "ArrowDown").await;
    assert_eq!(active_text(&page).await, "Duplicate");
    press(&page, "ArrowDown").await;
    assert_eq!(
        active_text(&page).await,
        "Archive",
        "the separator is skipped"
    );
    press(&page, "ArrowDown").await;
    assert_eq!(active_text(&page).await, "Edit", "ArrowDown wraps");
    press(&page, "ArrowUp").await;
    assert_eq!(active_text(&page).await, "Archive", "ArrowUp wraps");
    press(&page, "Home").await;
    assert_eq!(active_text(&page).await, "Edit");
    press(&page, "End").await;
    assert_eq!(active_text(&page).await, "Archive");
    press(&page, "d").await;
    assert_eq!(active_text(&page).await, "Duplicate", "typeahead");

    press(&page, "Escape").await;
    assert!(!is_open(&page, "#actions").await);
    assert_eq!(
        active_id(&page).await,
        "actions-trigger",
        "Escape returns focus"
    );

    press(&page, "ArrowDown").await;
    assert_eq!(
        active_text(&page).await,
        "Edit",
        "ArrowDown opens on the first item"
    );
    press(&page, "Escape").await;
    press(&page, "ArrowUp").await;
    assert_eq!(
        active_text(&page).await,
        "Archive",
        "ArrowUp opens on the last item"
    );
    press(&page, "Escape").await;
    press(&page, " ").await;
    assert_eq!(
        active_text(&page).await,
        "Edit",
        "Space opens on the first item"
    );
    page.expect_no_console_errors().await.expect("no errors");
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn menu_button_closes_on_tab_and_activates_a_link_with_enter() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/").await;

    focus(&page, "#actions-trigger").await;
    press(&page, "Enter").await;
    settle().await;
    press(&page, "Tab").await;
    assert!(!is_open(&page, "#actions").await, "Tab closes the menu");
    assert_eq!(active_id(&page).await, "after", "focus moves on");

    focus(&page, "#actions-trigger").await;
    press(&page, "Enter").await;
    settle().await;
    press(&page, "ArrowDown").await;
    press(&page, "Enter").await;
    assert!(
        !is_open(&page, "#actions").await,
        "Enter on an item closes the menu"
    );
    assert_eq!(attr_url(&page).await, "#duplicate");
    assert_eq!(active_id(&page).await, "actions-trigger");
}

async fn attr_url(page: &Page) -> String {
    eval_string(page, "location.hash").await
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn menu_opened_with_an_arrow_key_is_anchored_to_the_trigger() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/").await;

    focus(&page, "#actions-trigger").await;
    press(&page, "ArrowDown").await;
    assert!(is_open(&page, "#actions").await);
    let [t, p] = boxes(&page, "actions").await;
    assert!(
        p.1 >= t.3 - EPS && (p.0 - t.0).abs() <= EPS,
        "below the trigger, start edges meet: {t:?} {p:?}"
    );
}

// ── no JavaScript ───────────────────────────────────────────────────────

#[tokio::test]
#[ignore = "requires Chromium"]
async fn popover_and_menu_work_without_the_runtime() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    page.visit("/bare").await.expect("visit");
    wait_for(&page, "document.readyState === 'complete'").await;
    assert!(
        eval_bool(&page, "window.__autumnWidgets === undefined").await,
        "the runtime must not run"
    );

    page.click("#help-trigger").await.expect("open");
    assert!(
        is_open(&page, "#help").await,
        "commandfor opens the popover"
    );
    page.click("#help-trigger").await.expect("close");
    assert!(!is_open(&page, "#help").await, "commandfor closes it");

    page.click("#actions-trigger").await.expect("open menu");
    assert!(is_open(&page, "#actions").await);
    assert_eq!(attr(&page, "#actions", "role").await, "menu");
    page.click("#title").await.expect("click outside");
    assert!(
        !is_open(&page, "#actions").await,
        "light dismiss without JS"
    );

    page.click("#actions-trigger").await.expect("open menu");
    page.click("#actions button[role=menuitem]")
        .await
        .expect("submit");
    page.expect_url("/echo").await.expect("the form works");
}
