//! Tooltip behaviour in a real browser (issue #3404).
//!
//! Each test runs in two modes:
//!
//! - **CSS.** A script removes `interestfor` support before the runtime
//!   loads. CSS shows the tooltip. The keyboard core closes it on Escape.
//! - **Interest.** Chromium's `interestfor` and `popover="hint"` show it.
//!
//! The tests check: focus shows the text, Escape hides it, the hover bridge
//! keeps it open, it shows again after a dismiss, it closes before a modal,
//! it flips at the viewport edge, and axe passes.
//!
//! Run:
//!
//! ```text
//! cargo test -p autumn-web --features system-tests \
//!   --test integration_tests widgets_tooltip_browser -- --include-ignored
//! ```

#![cfg(all(feature = "system-tests", feature = "maud"))]

use std::time::Duration;

use autumn_web::assets::{PluginAssets, asset_url};
use autumn_web::prelude::*;
use autumn_web::system_test::{Page, SystemTest, SystemTestRunner};
use autumn_web::widgets::{
    ModalConfig, TooltipConfig, TooltipPlacement, modal, modal_trigger, tooltip, tooltip_with,
};
use maud::{DOCTYPE, Markup, html};

/// `no-interest.js` removes `interestfor` support, so the runtime keeps the
/// CSS mode. `page.css` gives the page token colors, for axe.
static FIXTURE: PluginAssets = PluginAssets::from_files(
    "tipmode",
    &[
        (
            "no-interest.js",
            b"delete HTMLButtonElement.prototype.interestForElement;\n\
              delete HTMLAnchorElement.prototype.interestForElement;\n",
        ),
        (
            "page.css",
            b"body { background: var(--bg); color: var(--text); }\n\
              a { color: var(--primary); }\n",
        ),
    ],
);

#[derive(Clone, Copy, Debug)]
enum Mode {
    Css,
    Interest,
}

const MODES: [Mode; 2] = [Mode::Css, Mode::Interest];

impl Mode {
    const fn path(self) -> &'static str {
        match self {
            Self::Css => "/css",
            Self::Interest => "/interest",
        }
    }
}

fn copy_tip() -> Markup {
    tooltip("copy-tip", "Copy the link", |tip| {
        html! { button id="copy" type="button" aria-describedby=(tip) { "Copy" } }
    })
}

fn body() -> Markup {
    let in_modal = html! {
        (tooltip("modal-tip", "Saves a draft", |tip| {
            html! { button id="modal-btn" type="button" aria-describedby=(tip) { "Draft" } }
        }))
    };
    html! {
        // At the top edge: a `Top` tooltip has no room and flips below.
        (tooltip("edge-tip", "Flips below", |tip| {
            html! { button id="edge" type="button" aria-describedby=(tip) { "Edge" } }
        }))
        main {
            h1 { "Tooltips" }
            p { button id="before" type="button" { "Before" } }
            p { "Share: " (copy_tip()) }
            // Away from the left edge, so the text can centre on the link.
            p {
                "Read the full guide here: "
                (tooltip_with(
                    "docs-tip",
                    "Opens the guide",
                    &TooltipConfig::new().placement(TooltipPlacement::Bottom),
                    |tip| html! { a id="docs" href="#guide" aria-describedby=(tip) { "Docs" } },
                ))
            }
            p {
                (tooltip("del-tip", "Delete", |tip| {
                    html! {
                        button id="del" type="button" aria-labelledby=(tip) {
                            span aria-hidden="true" { "×" }
                        }
                    }
                }))
            }
            p { button id="after" type="button" { "After" } }
            (modal_trigger("Open modal", "dlg", None))
            (modal("dlg", "A modal", &in_modal, &ModalConfig::new()))
            button id="load" type="button" hx-get="/fragment" hx-target="#slot"
                hx-swap="innerHTML" { "Load" }
            div id="slot" {}
        }
    }
}

fn layout(css_mode: bool, js: bool) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                title { "Tooltips" }
                link rel="stylesheet" href="/static/css/autumn-widgets.css";
                link rel="stylesheet" href=(asset_url("_plugins/tipmode/page.css"));
                @if css_mode {
                    script src=(asset_url("_plugins/tipmode/no-interest.js")) defer {}
                }
                @if js {
                    script src="/static/js/htmx.min.js" defer {}
                    script src="/static/js/autumn-widgets.js" defer {}
                }
            }
            body { (body()) }
        }
    }
}

#[get("/css")]
async fn css_page() -> Markup {
    layout(true, true)
}

#[get("/interest")]
async fn interest_page() -> Markup {
    layout(false, true)
}

/// No widget runtime: the CSS baseline only.
#[get("/no-runtime")]
async fn no_runtime_page() -> Markup {
    layout(false, false)
}

#[get("/fragment")]
async fn fragment() -> Markup {
    tooltip("late-tip", "Swapped in", |tip| {
        html! { button id="late" type="button" aria-describedby=(tip) { "Late" } }
    })
}

async fn runner() -> SystemTestRunner {
    Box::pin(
        SystemTest::new()
            .routes(routes![css_page, interest_page, no_runtime_page, fragment])
            .plugin_assets(&FIXTURE)
            .build(),
    )
    .await
    .expect("start runner")
}

// ── Helpers ─────────────────────────────────────────────────────────────

async fn eval_bool(page: &Page, js: &str) -> bool {
    page.evaluate(js)
        .await
        .ok()
        .and_then(|r| r.into_value::<bool>().ok())
        .unwrap_or(false)
}

async fn eval_string(page: &Page, js: &str) -> String {
    page.evaluate(js)
        .await
        .ok()
        .and_then(|r| r.into_value::<String>().ok())
        .unwrap_or_default()
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

/// JS: true when the element with `id` shows on screen.
fn visible_js(id: &str) -> String {
    format!(
        "(() => {{ const e = document.getElementById({id:?}); if (!e) return false; \
         const s = getComputedStyle(e); \
         return s.display !== 'none' && s.visibility === 'visible' && \
         parseFloat(s.opacity) > 0.5; }})()"
    )
}

async fn expect_shown(page: &Page, id: &str, why: &str) {
    assert!(
        poll(page, &visible_js(id)).await,
        "{id} does not show: {why}"
    );
}

async fn expect_hidden(page: &Page, id: &str, why: &str) {
    assert!(
        poll(page, &format!("!{}", visible_js(id))).await,
        "{id} does not hide: {why}"
    );
}

/// Stay hidden for 600 ms: longer than the Chromium interest delay.
async fn expect_stays_hidden(page: &Page, id: &str, why: &str) {
    for _ in 0..12 {
        assert!(!eval_bool(page, &visible_js(id)).await, "{id} shows: {why}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Load the page for `mode`. Check that the runtime picked that mode.
async fn open(page: &Page, mode: Mode) {
    open_path(page, mode.path()).await;
    let enhanced = eval_bool(
        page,
        "document.getElementById('copy-tip').getAttribute('popover') === 'hint'",
    )
    .await;
    assert_eq!(
        enhanced,
        matches!(mode, Mode::Interest),
        "{mode:?}: the wrong tooltip mode"
    );
}

async fn open_path(page: &Page, path: &str) {
    page.visit(path).await.expect("visit");
    assert!(
        poll(page, "document.readyState === 'complete'").await,
        "{path} did not load"
    );
    if path != "/no-runtime" {
        assert!(
            poll(page, "window.__autumnWidgets === true").await,
            "the runtime did not start"
        );
    }
    // Move the mouse to a corner with no tooltip.
    page.move_mouse(1.0, 400.0).await.expect("move mouse");
}

/// Focus `before_id` by script, then press Tab: a keyboard focus.
async fn tab_from(page: &Page, before_id: &str) {
    assert!(
        eval_bool(
            page,
            &format!(
                "(() => {{ const e = document.getElementById({before_id:?}); e.focus(); \
                 return document.activeElement === e; }})()"
            ),
        )
        .await,
        "could not focus {before_id}"
    );
    page.press("Tab").await.expect("Tab");
}

async fn active_id(page: &Page) -> String {
    eval_string(
        page,
        "document.activeElement ? document.activeElement.id : ''",
    )
    .await
}

/// The centre of the element with `id`, and its box.
async fn rect(page: &Page, id: &str) -> [f64; 4] {
    let r: Vec<f64> = page
        .evaluate(format!(
            "(() => {{ const r = document.getElementById({id:?}).getBoundingClientRect(); \
             return [r.left, r.top, r.width, r.height]; }})()"
        ))
        .await
        .expect("evaluate")
        .into_value()
        .expect("rect");
    [r[0], r[1], r[2], r[3]]
}

/// Move the mouse from the trigger to the text in small steps, through the
/// gap. Return the number of steps where the text did not show.
async fn walk_to_bubble(page: &Page, trigger: &str, bubble: &str) -> usize {
    let [tx, ty, tw, _] = rect(page, trigger).await;
    let [bx, by, bw, bh] = rect(page, bubble).await;
    let (x0, y0) = (tx + tw / 2.0, ty + 2.0);
    let (x1, y1) = (bx + bw / 2.0, by + bh / 2.0);
    let mut misses = 0;
    for step in 0..=10 {
        let f = f64::from(step) / 10.0;
        page.move_mouse(x0 + (x1 - x0) * f, y0 + (y1 - y0) * f)
            .await
            .expect("move mouse");
        tokio::time::sleep(Duration::from_millis(30)).await;
        if !eval_bool(page, &visible_js(bubble)).await {
            misses += 1;
        }
    }
    misses
}

fn axe_source() -> &'static str {
    static SOURCE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    SOURCE.get_or_init(|| {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/axe-core/axe.min.js");
        std::fs::read_to_string(&path)
            .unwrap_or_else(|err| panic!("read {}: {err}", path.display()))
    })
}

// ── Tests ───────────────────────────────────────────────────────────────

/// A keyboard focus shows the text. The trigger's description is the text.
#[tokio::test]
#[ignore = "requires Chromium"]
async fn tooltip_shows_on_keyboard_focus() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    for mode in MODES {
        open(&page, mode).await;
        expect_hidden(&page, "copy-tip", "before focus").await;
        tab_from(&page, "before").await;
        assert_eq!(active_id(&page).await, "copy");
        expect_shown(&page, "copy-tip", &format!("{mode:?}: on focus")).await;
        assert_eq!(
            eval_string(
                &page,
                "document.getElementById(document.getElementById('copy') \
                 .getAttribute('aria-describedby')).textContent"
            )
            .await,
            "Copy the link"
        );
        // Focus that goes away hides the text.
        page.press("Tab").await.expect("Tab");
        expect_hidden(&page, "copy-tip", &format!("{mode:?}: after Tab")).await;
    }
    page.expect_no_console_errors().await.expect("no errors");
}

/// WCAG 1.4.13 (dismissible): Escape hides a focused tooltip. Focus stays
/// on the trigger. The next focus shows the text again.
#[tokio::test]
#[ignore = "requires Chromium"]
async fn escape_hides_a_focused_tooltip() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    for mode in MODES {
        open(&page, mode).await;
        tab_from(&page, "before").await;
        expect_shown(&page, "copy-tip", &format!("{mode:?}: on focus")).await;
        page.press("Escape").await.expect("Escape");
        expect_hidden(&page, "copy-tip", &format!("{mode:?}: Escape")).await;
        assert_eq!(active_id(&page).await, "copy", "focus stays on the trigger");
        expect_stays_hidden(&page, "copy-tip", &format!("{mode:?}: after Escape")).await;
        // Tab away and back: the tooltip shows again.
        page.press("Tab").await.expect("Tab");
        tab_from(&page, "before").await;
        expect_shown(&page, "copy-tip", &format!("{mode:?}: focus again")).await;
    }
    page.expect_no_console_errors().await.expect("no errors");
}

/// Escape also hides a tooltip that shows for hover only. The pointer does
/// not move. A new hover shows the text again.
#[tokio::test]
#[ignore = "requires Chromium"]
async fn escape_hides_a_hovered_tooltip() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    for mode in MODES {
        open(&page, mode).await;
        page.hover("#copy").await.expect("hover");
        expect_shown(&page, "copy-tip", &format!("{mode:?}: on hover")).await;
        assert_ne!(active_id(&page).await, "copy", "the trigger has no focus");
        page.press("Escape").await.expect("Escape");
        expect_hidden(&page, "copy-tip", &format!("{mode:?}: Escape")).await;
        expect_stays_hidden(&page, "copy-tip", &format!("{mode:?}: pointer stays")).await;
        page.move_mouse(1.0, 400.0).await.expect("move away");
        page.hover("#copy").await.expect("hover again");
        expect_shown(&page, "copy-tip", &format!("{mode:?}: hover again")).await;
    }
    page.expect_no_console_errors().await.expect("no errors");
}

/// WCAG 1.4.13 (hoverable): the pointer moves from the trigger, through the
/// gap, onto the text. The text stays. It hides when the pointer leaves.
#[tokio::test]
#[ignore = "requires Chromium"]
async fn the_hover_bridge_keeps_the_tooltip_open() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    for mode in MODES {
        open(&page, mode).await;
        page.hover("#copy").await.expect("hover");
        expect_shown(&page, "copy-tip", &format!("{mode:?}: on hover")).await;
        let [_, ty, _, _] = rect(&page, "copy").await;
        let [_, by, _, bh] = rect(&page, "copy-tip").await;
        assert!(by + bh < ty, "{mode:?}: the text is above the trigger");
        assert!(ty - (by + bh) >= 4.0, "{mode:?}: there is a gap to bridge");
        let misses = walk_to_bubble(&page, "copy", "copy-tip").await;
        assert_eq!(misses, 0, "{mode:?}: the text hid on the way to it");
        // Stay on the text: it does not time out (WCAG 1.4.13, persistent).
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert!(
            eval_bool(&page, &visible_js("copy-tip")).await,
            "{mode:?}: persistent"
        );
        page.move_mouse(1.0, 400.0).await.expect("move away");
        expect_hidden(&page, "copy-tip", &format!("{mode:?}: pointer left")).await;
    }
    page.expect_no_console_errors().await.expect("no errors");
}

/// A mouse click focuses the trigger without `:focus-visible`. The text does
/// not stay after the pointer leaves.
#[tokio::test]
#[ignore = "requires Chromium"]
async fn a_mouse_click_does_not_pin_the_tooltip() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    for mode in MODES {
        open(&page, mode).await;
        page.click("#copy").await.expect("click");
        assert_eq!(active_id(&page).await, "copy");
        page.move_mouse(1.0, 400.0).await.expect("move away");
        expect_hidden(&page, "copy-tip", &format!("{mode:?}: after a click")).await;
    }
    page.expect_no_console_errors().await.expect("no errors");
}

/// In a modal, the first Escape closes the tooltip. The second closes the
/// modal.
#[tokio::test]
#[ignore = "requires Chromium"]
async fn escape_closes_the_tooltip_before_the_modal() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    for mode in MODES {
        open(&page, mode).await;
        tab_from(&page, "after").await;
        page.press("Enter").await.expect("open modal");
        assert!(
            poll(&page, "document.getElementById('dlg').open").await,
            "the modal opens"
        );
        assert!(
            eval_bool(
                &page,
                "(() => { const b = document.getElementById('modal-btn'); b.focus(); \
                 return document.activeElement === b; })()"
            )
            .await,
            "focus the trigger in the modal"
        );
        // A script focus after a key press is a keyboard focus.
        expect_shown(&page, "modal-tip", &format!("{mode:?}: in the modal")).await;
        page.press("Escape").await.expect("Escape");
        expect_hidden(&page, "modal-tip", &format!("{mode:?}: first Escape")).await;
        assert!(
            eval_bool(&page, "document.getElementById('dlg').open").await,
            "{mode:?}: the first Escape must not close the modal"
        );
        page.press("Escape").await.expect("Escape");
        assert!(
            poll(&page, "!document.getElementById('dlg').open").await,
            "{mode:?}: the second Escape closes the modal"
        );
    }
    page.expect_no_console_errors().await.expect("no errors");
}

/// Anchor positioning: a `Top` tooltip with no room above flips below. A
/// `Bottom` tooltip shows below its trigger.
#[tokio::test]
#[ignore = "requires Chromium"]
async fn anchor_positioning_places_and_flips_the_text() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    for mode in MODES {
        open(&page, mode).await;
        page.hover("#edge").await.expect("hover");
        expect_shown(&page, "edge-tip", &format!("{mode:?}: edge")).await;
        let [_, ty, _, th] = rect(&page, "edge").await;
        let [_, by, _, _] = rect(&page, "edge-tip").await;
        assert!(by >= ty + th, "{mode:?}: the edge tooltip flips below");

        page.hover("#docs").await.expect("hover");
        expect_shown(&page, "docs-tip", &format!("{mode:?}: bottom")).await;
        let [tx, ty, tw, th] = rect(&page, "docs").await;
        let [bx, by, bw, _] = rect(&page, "docs-tip").await;
        assert!(by >= ty + th, "{mode:?}: Bottom shows below");
        let centre = (bx + bw / 2.0) - (tx + tw / 2.0);
        assert!(
            centre.abs() < 2.0,
            "{mode:?}: centred on the trigger ({centre})"
        );
    }
    page.expect_no_console_errors().await.expect("no errors");
}

/// The Chromium mode makes the text a `popover="hint"` with `interestfor`.
/// The CSS mode adds neither. Tooltips that htmx swaps in get the same mode.
#[tokio::test]
#[ignore = "requires Chromium"]
async fn the_runtime_enhances_only_where_interestfor_works() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    for mode in MODES {
        open(&page, mode).await;
        let enhanced = matches!(mode, Mode::Interest);
        for (trigger, tip) in [
            ("copy", "copy-tip"),
            ("docs", "docs-tip"),
            ("del", "del-tip"),
        ] {
            assert_eq!(
                eval_bool(
                    &page,
                    &format!(
                        "document.getElementById({trigger:?}).getAttribute('interestfor') === {tip:?}"
                    ),
                )
                .await,
                enhanced,
                "{mode:?}: interestfor on #{trigger}"
            );
        }
        page.click("#load").await.expect("load");
        assert!(
            poll(&page, "!!document.getElementById('late-tip')").await,
            "the fragment loads"
        );
        assert_eq!(
            eval_bool(
                &page,
                "document.getElementById('late-tip').getAttribute('popover') === 'hint'"
            )
            .await,
            enhanced,
            "{mode:?}: a swapped-in tooltip"
        );
        tab_from(&page, "load").await;
        // `#slot` follows `#load`: Tab reaches the new trigger.
        assert_eq!(active_id(&page).await, "late");
        expect_shown(&page, "late-tip", &format!("{mode:?}: swapped in")).await;
        page.press("Escape").await.expect("Escape");
        expect_hidden(&page, "late-tip", &format!("{mode:?}: swapped in, Escape")).await;
    }
    page.expect_no_console_errors().await.expect("no errors");
}

/// An icon-only trigger takes its accessible name from the text.
#[tokio::test]
#[ignore = "requires Chromium"]
async fn an_icon_trigger_is_named_by_the_text_and_closes_on_escape() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    for mode in MODES {
        open(&page, mode).await;
        tab_from(&page, "docs").await;
        assert_eq!(active_id(&page).await, "del");
        expect_shown(&page, "del-tip", &format!("{mode:?}: icon trigger")).await;
        page.press("Escape").await.expect("Escape");
        expect_hidden(&page, "del-tip", &format!("{mode:?}: icon trigger, Escape")).await;
    }
    page.expect_no_console_errors().await.expect("no errors");
}

/// Without the runtime, CSS still shows the text on hover and focus.
/// Escape needs the runtime. This is the canary: the Escape tests above
/// fail without it.
#[tokio::test]
#[ignore = "requires Chromium"]
async fn without_the_runtime_css_shows_the_text_but_escape_does_nothing() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open_path(&page, "/no-runtime").await;
    page.hover("#copy").await.expect("hover");
    expect_shown(&page, "copy-tip", "hover, no runtime").await;
    page.move_mouse(1.0, 400.0).await.expect("move away");
    expect_hidden(&page, "copy-tip", "pointer left, no runtime").await;
    tab_from(&page, "before").await;
    expect_shown(&page, "copy-tip", "focus, no runtime").await;
    page.press("Escape").await.expect("Escape");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        eval_bool(&page, &visible_js("copy-tip")).await,
        "without the runtime Escape must do nothing (canary)"
    );
}

/// axe passes with each tooltip shown, in both modes and both schemes.
#[tokio::test]
#[ignore = "requires Chromium"]
async fn axe_passes_with_the_tooltip_shown() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    let mut failures = Vec::new();
    for mode in MODES {
        for scheme in ["light", "dark"] {
            open(&page, mode).await;
            assert!(
                eval_bool(
                    &page,
                    &format!(
                        "(() => {{ document.documentElement.setAttribute('data-theme', '{scheme}'); \
                         return getComputedStyle(document.documentElement).colorScheme === '{scheme}'; }})()"
                    ),
                )
                .await,
                "select the {scheme} scheme"
            );
            tab_from(&page, "before").await;
            expect_shown(&page, "copy-tip", "for axe").await;
            // axe must not read a color in the middle of a transition.
            assert!(poll(&page, "document.getAnimations().length === 0").await);
            for violation in page.axe_violations(axe_source()).await.expect("axe") {
                failures.push(format!("{mode:?} [{scheme}] {violation}"));
            }
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
    page.expect_no_console_errors().await.expect("no errors");
}
