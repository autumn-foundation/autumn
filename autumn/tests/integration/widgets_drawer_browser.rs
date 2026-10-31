//! Drawer and modal behaviour in a real browser (issue #3403).
//!
//! The tests check that a modal drawer traps focus and returns it to the
//! trigger, that Escape and a backdrop click dismiss it, that the close
//! button works, that sizes and edges apply, that a non-modal drawer
//! leaves the page usable, and that a modal drawer works without JS.
//!
//! Run:
//!
//! ```text
//! cargo test -p autumn-web --features system-tests \
//!   --test integration_tests widgets_drawer_browser -- --include-ignored
//! ```

#![cfg(all(feature = "system-tests", feature = "maud"))]

use std::time::Duration;

use autumn_web::prelude::*;
use autumn_web::system_test::{Page, SystemTest, SystemTestRunner};
use autumn_web::widgets::{
    DialogSize, DrawerConfig, DrawerSide, ModalConfig, drawer, drawer_trigger, modal, modal_trigger,
};
use maud::{DOCTYPE, Markup, html};

fn layout(scripts: bool, body: &Markup) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                title { "Drawer" }
                link rel="stylesheet" href="/static/css/autumn-widgets.css";
                @if scripts {
                    script src="/static/js/autumn-widgets.js" defer {}
                }
            }
            body { (body) }
        }
    }
}

fn content(scripts: bool) -> Markup {
    let body = html! {
        p { "Body" }
        input id="inside" type="text" aria-label="Inside";
    };
    let right = DrawerConfig::new().close_button("Close drawer");
    let left = DrawerConfig::new()
        .side(DrawerSide::Left)
        .size(DialogSize::Sm);
    let bottom = DrawerConfig::new().side(DrawerSide::Bottom);
    let panel = DrawerConfig::new().modal(false).close_button("Close panel");
    let sticky = DrawerConfig::new().light_dismiss(false);
    let wide = ModalConfig::new()
        .size(DialogSize::Lg)
        .close_button("Close modal");
    let small = ModalConfig::new().size(DialogSize::Sm);
    layout(
        scripts,
        &html! {
            h1 id="title" { "Drawer" }
            button id="before" type="button" { "Before" }
            div id="open-right" { (modal_trigger("Open right", "right", None)) }
            (modal_trigger("Open left", "left", None))
            (modal_trigger("Open bottom", "bottom", None))
            (modal_trigger("Open sticky", "sticky", None))
            (modal_trigger("Open wide", "wide", None))
            (modal_trigger("Open small", "small", None))
            (drawer_trigger("Open panel", "panel", &panel, None))
            button id="after" type="button" { "After" }
            (drawer("right", "Right", &body, &right))
            (drawer("left", "Left", &body, &left))
            (drawer("bottom", "Bottom", &body, &bottom))
            (drawer("sticky", "Sticky", &body, &sticky))
            (drawer("panel", "Panel", &body, &panel))
            (modal("wide", "Wide", &body, &wide))
            (modal("small", "Small", &body, &small))
        },
    )
}

#[get("/")]
async fn index() -> Markup {
    content(true)
}

/// The same page with no `autumn-widgets.js`.
#[get("/no-js")]
async fn no_js() -> Markup {
    content(false)
}

async fn runner() -> SystemTestRunner {
    Box::pin(SystemTest::new().routes(routes![index, no_js]).build())
        .await
        .expect("start runner")
}

async fn eval_bool(page: &Page, js: &str) -> bool {
    page.evaluate(js)
        .await
        .ok()
        .and_then(|r| r.into_value::<bool>().ok())
        .unwrap_or(false)
}

async fn eval_f64(page: &Page, js: &str) -> f64 {
    page.evaluate(js)
        .await
        .ok()
        .and_then(|r| r.into_value::<f64>().ok())
        .unwrap_or(f64::NAN)
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
}

/// Focus `selector` and press Enter, as a keyboard user does.
async fn activate(page: &Page, selector: &str) {
    assert!(
        eval_bool(
            page,
            &format!("(document.querySelector({selector:?}).focus(), true)")
        )
        .await
    );
    page.press("Enter").await.expect("enter");
}

/// Wait for the slide-in animation to end, so positions are final.
async fn settle(page: &Page) {
    wait_for(page, "document.getAnimations().length === 0").await;
}

fn is_open(id: &str) -> String {
    format!("document.getElementById({id:?}).open")
}

fn trigger(id: &str) -> String {
    format!("[data-modal-open={id:?}]")
}

fn focus_inside(id: &str) -> String {
    format!("document.getElementById({id:?}).contains(document.activeElement)")
}

/// The viewport point on the backdrop, away from the dialog.
async fn click_backdrop(page: &Page, id: &str, side: &str) {
    settle(page).await;
    let (x, y) = match side {
        "left" => ("innerWidth - 5", "innerHeight / 2"),
        "right" => ("5", "innerHeight / 2"),
        "top" => ("innerWidth / 2", "innerHeight - 5"),
        _ => ("innerWidth / 2", "5"),
    };
    let px = eval_f64(page, x).await;
    let py = eval_f64(page, y).await;
    assert!(
        !eval_bool(
            page,
            &format!(
                "(() => {{ const r = document.getElementById({id:?}).getBoundingClientRect(); \
                 return {px} >= r.left && {px} <= r.right && {py} >= r.top && {py} <= r.bottom; }})()"
            )
        )
        .await,
        "the point is on the dialog"
    );
    page.click_at(px, py).await.expect("click backdrop");
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn modal_drawer_traps_focus_and_returns_it_on_escape() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/").await;

    activate(&page, &trigger("right")).await;
    wait_for(&page, &is_open("right")).await;
    assert!(eval_bool(&page, "document.getElementById('right').matches(':modal')").await);
    assert!(
        eval_bool(&page, &focus_inside("right")).await,
        "focus moves in"
    );

    // Tab may leave for the browser UI (`body`). It never reaches the page behind.
    for _ in 0..6 {
        page.press("Tab").await.expect("tab");
        let js = format!(
            "{} || document.activeElement === document.body",
            focus_inside("right")
        );
        assert!(eval_bool(&page, &js).await, "Tab reached the page behind");
    }

    page.press("Escape").await.expect("escape");
    wait_for(&page, &format!("!{}", is_open("right"))).await;
    assert!(
        eval_bool(
            &page,
            &format!(
                "document.activeElement === document.querySelector({:?})",
                trigger("right")
            )
        )
        .await,
        "focus returns to the trigger"
    );
    page.expect_no_console_errors().await.expect("no errors");
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn backdrop_click_dismisses_each_edge() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/").await;

    for (id, side) in [("right", "right"), ("left", "left"), ("bottom", "bottom")] {
        activate(&page, &trigger(id)).await;
        wait_for(&page, &is_open(id)).await;
        click_backdrop(&page, id, side).await;
        wait_for(&page, &format!("!{}", is_open(id))).await;
        assert!(
            eval_bool(
                &page,
                &format!(
                    "document.activeElement === document.querySelector({:?})",
                    trigger(id)
                )
            )
            .await,
            "focus returns to the {id} trigger"
        );
    }
    page.expect_no_console_errors().await.expect("no errors");
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn a_click_inside_the_drawer_does_not_dismiss_it() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/").await;
    activate(&page, &trigger("right")).await;
    wait_for(&page, &is_open("right")).await;
    settle(&page).await;
    page.click("#right .autumn-drawer__body")
        .await
        .expect("click body");
    assert!(eval_bool(&page, &is_open("right")).await);
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn without_light_dismiss_a_backdrop_click_keeps_the_drawer_open() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/").await;
    activate(&page, &trigger("sticky")).await;
    wait_for(&page, &is_open("sticky")).await;
    click_backdrop(&page, "sticky", "right").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(eval_bool(&page, &is_open("sticky")).await);
    page.press("Escape").await.expect("escape");
    wait_for(&page, &format!("!{}", is_open("sticky"))).await;
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn close_button_has_a_name_and_closes_the_drawer() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/").await;
    activate(&page, &trigger("right")).await;
    wait_for(&page, &is_open("right")).await;
    assert!(
        eval_bool(
            &page,
            "document.querySelector('#right .autumn-drawer__close').getAttribute('aria-label') === 'Close drawer'"
        )
        .await
    );
    activate(&page, "#right .autumn-drawer__close").await;
    wait_for(&page, &format!("!{}", is_open("right"))).await;
    assert!(
        eval_bool(
            &page,
            &format!(
                "document.activeElement === document.querySelector({:?})",
                trigger("right")
            )
        )
        .await,
        "focus returns to the trigger"
    );
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn drawers_sit_on_their_edge_and_sizes_apply() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/").await;

    activate(&page, &trigger("right")).await;
    wait_for(&page, &is_open("right")).await;
    settle(&page).await;
    assert!(
        eval_bool(
            &page,
            "(() => { const r = document.getElementById('right').getBoundingClientRect(); \
             return Math.round(r.right) === innerWidth && r.top === 0 \
             && Math.round(r.height) === innerHeight && Math.round(r.width) === 384; })()"
        )
        .await,
        "right drawer is 24rem wide at the right edge"
    );
    page.press("Escape").await.expect("escape");

    activate(&page, &trigger("left")).await;
    wait_for(&page, &is_open("left")).await;
    settle(&page).await;
    assert!(
        eval_bool(
            &page,
            "(() => { const r = document.getElementById('left').getBoundingClientRect(); \
             return r.left === 0 && Math.round(r.width) === 256; })()"
        )
        .await,
        "small left drawer is 16rem wide at the left edge"
    );
    page.press("Escape").await.expect("escape");

    activate(&page, &trigger("bottom")).await;
    wait_for(&page, &is_open("bottom")).await;
    settle(&page).await;
    assert!(
        eval_bool(
            &page,
            "(() => { const r = document.getElementById('bottom').getBoundingClientRect(); \
             return Math.round(r.bottom) === innerHeight && Math.round(r.width) === innerWidth; })()"
        )
        .await,
        "bottom sheet spans the bottom edge"
    );
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn modal_sizes_apply_and_the_close_button_works() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/").await;

    activate(&page, &trigger("small")).await;
    wait_for(&page, &is_open("small")).await;
    let small = eval_f64(
        &page,
        "document.getElementById('small').getBoundingClientRect().width",
    )
    .await;
    page.press("Escape").await.expect("escape");

    activate(&page, &trigger("wide")).await;
    wait_for(&page, &is_open("wide")).await;
    let wide = eval_f64(
        &page,
        "document.getElementById('wide').getBoundingClientRect().width",
    )
    .await;
    // 20rem plus the 1px border on each side.
    assert!((small - 322.0).abs() < 1.0, "sm is 20rem: {small}");
    assert!(wide > 600.0, "lg is wider than 600px: {wide}");

    activate(&page, "#wide .autumn-modal__close").await;
    wait_for(&page, &format!("!{}", is_open("wide"))).await;
    page.expect_no_console_errors().await.expect("no errors");
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn non_modal_drawer_leaves_the_page_usable() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/").await;

    page.click("[data-drawer-open='panel']")
        .await
        .expect("open");
    wait_for(&page, &is_open("panel")).await;
    assert!(!eval_bool(&page, "document.getElementById('panel').matches(':modal')").await);
    // The page behind still takes focus.
    activate(&page, "#after").await;
    assert!(eval_bool(&page, "document.activeElement.id === 'after'").await);
    assert!(eval_bool(&page, &is_open("panel")).await, "it stays open");

    // Escape inside the drawer closes it.
    assert!(
        eval_bool(
            &page,
            "(document.querySelector('#panel input').focus(), true)"
        )
        .await
    );
    page.press("Escape").await.expect("escape");
    wait_for(&page, &format!("!{}", is_open("panel"))).await;

    page.click("[data-drawer-open='panel']")
        .await
        .expect("reopen");
    wait_for(&page, &is_open("panel")).await;
    activate(&page, "#panel .autumn-drawer__close").await;
    wait_for(&page, &format!("!{}", is_open("panel"))).await;
    page.expect_no_console_errors().await.expect("no errors");
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn modal_drawer_works_without_js() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    page.set_javascript_enabled(false).await.expect("js off");
    open(&page, "/no-js").await;

    // The invoker command opens and closes the dialog. No script runs.
    activate(&page, &trigger("right")).await;
    wait_for(&page, &is_open("right")).await;
    assert!(eval_bool(&page, "document.getElementById('right').matches(':modal')").await);
    assert!(
        eval_bool(&page, &focus_inside("right")).await,
        "focus moves in"
    );

    page.press("Escape").await.expect("escape");
    wait_for(&page, &format!("!{}", is_open("right"))).await;
    assert!(
        eval_bool(
            &page,
            &format!(
                "document.activeElement === document.querySelector({:?})",
                trigger("right")
            )
        )
        .await,
        "focus returns to the trigger"
    );

    activate(&page, &trigger("right")).await;
    wait_for(&page, &is_open("right")).await;
    activate(&page, "#right .autumn-drawer__close").await;
    wait_for(&page, &format!("!{}", is_open("right"))).await;
}
