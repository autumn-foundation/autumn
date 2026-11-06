//! The widget keyboard core in a real browser (issue #3397).
//!
//! Each test sends real key events and checks the WAI-ARIA APG keyboard
//! model for tabs, the combobox and the menu button. The tags mode of the
//! combobox (issue #3410) has tests for its chips. Other tests check the
//! no-JS fallback and the default CSP.
//!
//! Run:
//!
//! ```text
//! cargo test -p autumn-web --features system-tests \
//!   --test integration_tests widgets_keyboard_browser -- --include-ignored
//! ```

#![cfg(all(feature = "system-tests", feature = "maud"))]

use std::time::Duration;

use autumn_web::prelude::*;
use autumn_web::system_test::{Page, SystemTest, SystemTestRunner};
use autumn_web::widgets::{
    AutocompleteConfig, NavBarConfig, NavItem, NavMenu, autocomplete_create_option,
    autocomplete_empty_state, autocomplete_input, autocomplete_option, nav_bar, tabs,
};
use maud::{DOCTYPE, Markup, html};

const COLORS: &[(&str, &str)] = &[("1", "Green"), ("2", "Grey"), ("3", "Gold"), ("4", "Blue")];

fn layout(dir: &str, body: &Markup) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" dir=(dir) {
            head {
                meta charset="utf-8";
                title { "Keyboard" }
                link rel="stylesheet" href="/static/css/autumn-widgets.css";
                script src="/static/js/htmx.min.js" defer {}
                script src="/static/js/autumn-widgets.js" defer {}
            }
            body { (body) }
        }
    }
}

fn settings_tabs(id: &str) -> Markup {
    let panels = [
        (
            format!("{id}-profile"),
            "Profile",
            html! { p { "Profile body" } },
        ),
        (
            format!("{id}-security"),
            "Security",
            html! { p { "Security body" } },
        ),
        (
            format!("{id}-billing"),
            "Billing",
            html! { p { "Billing body" } },
        ),
    ];
    let panels: Vec<(&str, &str, Markup)> = panels
        .iter()
        .map(|(panel, label, body)| (panel.as_str(), *label, body.clone()))
        .collect();
    tabs(id, None, &panels)
}

fn page_body() -> Markup {
    let nav = NavBarConfig::new()
        .brand("Acme", "/")
        .item(NavItem::link("/", "Home"))
        .item(NavItem::menu(
            NavMenu::new("Products")
                .link("/widgets", "Widgets")
                .link("/gadgets", "Gadgets")
                .link("/gizmos", "Gizmos"),
        ));
    let ac = AutocompleteConfig::new("/colors", "color_id")
        .debounce(0)
        .fallback_options(COLORS);
    // A long debounce holds the old options while the user edits the query.
    let slow = AutocompleteConfig::new("/colors", "slow_id").debounce(1500);
    // Tags mode (#3410): Blue is a server-side pick.
    let tags = AutocompleteConfig::new("/colors", "color_ids")
        .debounce(0)
        .multiple()
        .fallback_options(COLORS)
        .selected(&[("4", "Blue")]);
    let words = AutocompleteConfig::new("/colors-create", "words")
        .debounce(0)
        .multiple()
        .free_text();
    html! {
        (nav_bar("/", &nav))
        h1 id="title" { "Keyboard" }
        (settings_tabs("t"))
        form id="pick" action="/submitted" method="get" {
            (autocomplete_input("ac", "Color", &ac))
            button type="submit" { "Save" }
        }
        (autocomplete_input("slow", "Slow color", &slow))
        form id="multi" action="/submitted-multi" method="get" {
            (autocomplete_input("tags", "Colors", &tags))
            (autocomplete_input("words", "Words", &words))
            button id="multi-save" type="submit" { "Save colors" }
        }
        button id="swap" hx-get="/fragment" hx-target="#slot" { "Swap" }
        div id="slot" {}
        button id="swap-outer" hx-get="/fragment-outer" hx-target="#slot-outer" hx-swap="outerHTML" {
            "Swap outer"
        }
        div id="slot-outer" {}
        button id="swap-tags" hx-get="/fragment-tags" hx-target="#slot-tags" { "Swap tags" }
        div id="slot-tags" {}
        a id="jump" href="#t-billing" { "Go to billing" }
        button id="swap-inner" hx-get="/fragment-inner" hx-target="#t" hx-swap="innerHTML" {
            "Swap inner"
        }
        button id="swap-same" hx-get="/fragment-same" hx-target="#t" hx-swap="outerHTML" {
            "Swap same"
        }
    }
}

#[get("/")]
async fn index() -> Markup {
    layout("ltr", &page_body())
}

#[get("/rtl")]
async fn rtl() -> Markup {
    let tags = AutocompleteConfig::new("/colors", "rtl_ids")
        .multiple()
        .selected(&[("1", "Green"), ("4", "Blue")]);
    layout(
        "rtl",
        &html! {
            (settings_tabs("r"))
            (autocomplete_input("rtags", "Colors", &tags))
        },
    )
}

#[get("/fragment")]
async fn fragment() -> Markup {
    settings_tabs("s")
}

/// Tabs `t` again, for an outerHTML swap that replaces the same root id.
#[get("/fragment-same")]
async fn fragment_same() -> Markup {
    settings_tabs("t")
}

/// The inner HTML of tabs `t`, for an innerHTML swap into its root.
#[get("/fragment-inner")]
async fn fragment_inner() -> Markup {
    let html = settings_tabs("t").into_string();
    let start = html.find('>').map_or(0, |i| i + 1);
    let end = html.rfind("</div>").unwrap_or(html.len());
    maud::PreEscaped(html[start..end].to_owned())
}

#[get("/fragment-outer")]
async fn fragment_outer() -> Markup {
    settings_tabs("o")
}

/// A tags form for an htmx swap. htmx parses it with scripts off, so its
/// `<noscript>` select is live markup.
#[get("/fragment-tags")]
async fn fragment_tags() -> Markup {
    let tags = AutocompleteConfig::new("/colors", "color_ids")
        .multiple()
        .fallback_options(COLORS)
        .selected(&[("2", "Grey")]);
    html! {
        form id="swapped" action="/submitted-multi" method="get" {
            (autocomplete_input("swapped-tags", "Swapped colors", &tags))
            button id="swapped-save" type="submit" { "Save swapped" }
        }
    }
}

#[derive(serde::Deserialize)]
struct Query {
    q: String,
}

#[get("/colors")]
async fn colors(axum::extract::Query(query): axum::extract::Query<Query>) -> Markup {
    let q = query.q.to_lowercase();
    let hits: Vec<_> = COLORS
        .iter()
        .filter(|(_, label)| !q.is_empty() && label.to_lowercase().starts_with(&q))
        .collect();
    html! {
        @if hits.is_empty() {
            (autocomplete_empty_state("No colors"))
        } @else {
            @for (value, label) in hits { (autocomplete_option(value, label)) }
        }
    }
}

/// The colors that start with `q`, then a "create" entry when no color
/// has the exact name.
#[get("/colors-create")]
async fn colors_create(axum::extract::Query(query): axum::extract::Query<Query>) -> Markup {
    let q = query.q.trim();
    let lower = q.to_lowercase();
    let hits: Vec<_> = COLORS
        .iter()
        .filter(|(_, label)| !lower.is_empty() && label.to_lowercase().starts_with(&lower))
        .collect();
    let exact = hits.iter().any(|(_, label)| label.eq_ignore_ascii_case(q));
    html! {
        @for (_, label) in &hits { (autocomplete_option(label, label)) }
        @if !q.is_empty() && !exact {
            (autocomplete_create_option(q, &format!("Create \u{201c}{q}\u{201d}")))
        }
    }
}

#[get("/submitted")]
async fn submitted() -> &'static str {
    "submitted"
}

#[derive(serde::Deserialize)]
struct Multi {
    #[serde(default)]
    color_ids: Vec<String>,
    #[serde(default)]
    words: Vec<String>,
}

/// Repeated keys decode into `Vec<T>` (#3394).
#[get("/submitted-multi")]
async fn submitted_multi(
    autumn_web::extract::Query(multi): autumn_web::extract::Query<Multi>,
) -> String {
    format!("color_ids={:?} words={:?}", multi.color_ids, multi.words)
}

/// An APG tree with nested groups: `src` (`lib.rs`, `bin` (`main.rs`)),
/// `docs` (`guide.md`), `README.md`. All parents are closed.
fn file_tree() -> Markup {
    html! {
        ul id="t" role="tree" aria-label="Files" data-autumn-roving
            data-autumn-orientation="vertical" data-autumn-typeahead {
            li id="t-src" role="treeitem" aria-expanded="false" {
                span { "src" }
                ul role="group" hidden {
                    li id="t-lib" role="treeitem" { "lib.rs" }
                    li id="t-bin" role="treeitem" aria-expanded="false" {
                        span { "bin" }
                        ul role="group" hidden {
                            li id="t-main" role="treeitem" { "main.rs" }
                        }
                    }
                }
            }
            li id="t-docs" role="treeitem" aria-expanded="false" {
                span { "docs" }
                ul role="group" hidden {
                    li id="t-guide" role="treeitem" { "guide.md" }
                }
            }
            li id="t-readme" role="treeitem" { "README.md" }
        }
    }
}

/// The same tree with row items: `li role="none"` holds the item and its
/// group, and the item names the group in `aria-owns`.
fn row_tree() -> Markup {
    html! {
        ul id="r" role="tree" aria-label="Files" data-autumn-roving
            data-autumn-orientation="vertical" {
            li role="none" {
                div id="r-src" role="treeitem" aria-expanded="false" aria-owns="r-src-group" { "src" }
                ul id="r-src-group" role="group" hidden {
                    li role="none" { div id="r-lib" role="treeitem" { "lib.rs" } }
                    li role="none" {
                        div id="r-bin" role="treeitem" aria-expanded="false" aria-owns="r-bin-group" { "bin" }
                        ul id="r-bin-group" role="group" hidden {
                            li role="none" { div id="r-main" role="treeitem" { "main.rs" } }
                        }
                    }
                }
            }
            li role="none" {
                div id="r-docs" role="treeitem" aria-expanded="false" aria-owns="r-docs-group" { "docs" }
                ul id="r-docs-group" role="group" hidden {
                    li role="none" { div id="r-guide" role="treeitem" { "guide.md" } }
                }
            }
        }
    }
}

/// A tree with a disabled parent: `d` (`x`) is disabled, `e` is not.
fn disabled_tree() -> Markup {
    html! {
        ul id="dt" role="tree" aria-label="Files" data-autumn-roving
            data-autumn-orientation="vertical" {
            li id="dt-d" role="treeitem" aria-expanded="false" aria-disabled="true" {
                span { "d" }
                ul role="group" hidden {
                    li id="dt-x" role="treeitem" aria-disabled="true" { "x" }
                    li id="dt-z" role="treeitem" { "z" }
                }
            }
            li id="dt-e" role="treeitem" { "e" }
        }
    }
}

#[get("/tree-disabled")]
async fn tree_disabled_page() -> Markup {
    layout("ltr", &disabled_tree())
}

#[get("/tree-rows")]
async fn tree_rows_page() -> Markup {
    layout("ltr", &row_tree())
}

#[get("/tree")]
async fn tree_page() -> Markup {
    layout("ltr", &file_tree())
}

#[get("/tree-rtl")]
async fn tree_rtl_page() -> Markup {
    layout("rtl", &file_tree())
}

async fn runner() -> SystemTestRunner {
    Box::pin(
        SystemTest::new()
            .routes(routes![
                index,
                rtl,
                fragment,
                fragment_inner,
                fragment_outer,
                fragment_same,
                fragment_tags,
                colors,
                colors_create,
                submitted,
                submitted_multi,
                tree_page,
                tree_rtl_page,
                tree_rows_page,
                tree_disabled_page
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

async fn wait_for(page: &Page, js: &str) {
    for _ in 0..100 {
        if eval_bool(page, js).await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for `{js}`");
}

/// Load `path` and wait until the runtime has run.
async fn open(page: &Page, path: &str) {
    page.visit(path).await.expect("visit");
    wait_for(page, "window.__autumnWidgets === true").await;
    wait_for(page, "document.readyState === 'complete'").await;
}

async fn focus(page: &Page, id: &str) {
    assert!(
        eval_bool(
            page,
            &format!(
                "(() => {{ const e = document.getElementById('{id}'); \
                 if (!e) return false; e.focus(); return document.activeElement === e; }})()"
            ),
        )
        .await,
        "could not focus #{id}"
    );
}

async fn press(page: &Page, key: &str) {
    page.press(key).await.expect("press");
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

/// The runtime reacts to the `toggle` event, which the browser queues as a
/// task. Let it run before the test reads the page.
async fn settle() {
    tokio::time::sleep(Duration::from_millis(100)).await;
}

async fn attr(page: &Page, id: &str, name: &str) -> String {
    settle().await;
    eval_string(
        page,
        &format!("document.getElementById('{id}').getAttribute('{name}') || ''"),
    )
    .await
}

async fn shown(page: &Page, id: &str) -> bool {
    eval_bool(
        page,
        &format!(
            "(() => {{ const e = document.getElementById('{id}'); \
             return !!e && getComputedStyle(e).display !== 'none'; }})()"
        ),
    )
    .await
}

/// Assert which tab of widget `id` is selected, by panel suffix.
async fn assert_selected_tab(page: &Page, id: &str, selected: &str) {
    for name in ["profile", "security", "billing"] {
        let tab = format!("{id}-tab-{id}-{name}");
        let panel = format!("{id}-{name}");
        let want = name == selected;
        assert_eq!(
            attr(page, &tab, "aria-selected").await,
            if want { "true" } else { "false" },
            "aria-selected on {tab}"
        );
        assert_eq!(
            attr(page, &tab, "tabindex").await,
            if want { "0" } else { "-1" },
            "roving tabindex on {tab}"
        );
        assert_eq!(shown(page, &panel).await, want, "panel {panel} shown");
        assert_eq!(
            eval_bool(
                page,
                &format!(
                    "document.getElementById('{tab}').classList.contains('autumn-tabs__tab--active')"
                ),
            )
            .await,
            want,
            "active class on {tab}"
        );
    }
}

// ── Tabs ────────────────────────────────────────────────────────────────

#[tokio::test]
#[ignore = "requires Chromium"]
async fn tabs_arrow_keys_home_and_end_move_the_selection() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/").await;
    assert_selected_tab(&page, "t", "profile").await;

    focus(&page, "t-tab-t-profile").await;
    press(&page, "ArrowRight").await;
    assert_eq!(active_id(&page).await, "t-tab-t-security");
    assert_selected_tab(&page, "t", "security").await;

    press(&page, "ArrowRight").await;
    press(&page, "ArrowRight").await;
    assert_eq!(active_id(&page).await, "t-tab-t-profile", "wraps to first");

    press(&page, "ArrowLeft").await;
    assert_eq!(active_id(&page).await, "t-tab-t-billing", "wraps to last");
    assert_selected_tab(&page, "t", "billing").await;

    press(&page, "Home").await;
    assert_selected_tab(&page, "t", "profile").await;
    press(&page, "End").await;
    assert_selected_tab(&page, "t", "billing").await;
    assert_eq!(
        eval_string(&page, "location.hash").await,
        "#t-billing",
        "the URL keeps a deep link"
    );

    page.expect_no_console_errors().await.expect("no errors");
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn tab_key_moves_from_the_tablist_to_the_panel() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/").await;
    focus(&page, "t-tab-t-profile").await;
    press(&page, "ArrowRight").await;
    press(&page, "Tab").await;
    assert_eq!(active_id(&page).await, "t-security");
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn clicking_a_tab_selects_it() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/").await;
    page.click("#t-tab-t-billing").await.expect("click");
    assert_selected_tab(&page, "t", "billing").await;
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn a_deep_link_selects_its_tab() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/#t-security").await;
    assert_selected_tab(&page, "t", "security").await;
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn a_malformed_fragment_does_not_stop_the_runtime() {
    // `decodeURIComponent("100%")` throws. Every widget must still init.
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/#100%").await;
    assert_selected_tab(&page, "t", "profile").await;
    assert_eq!(attr(&page, MENU, "role").await, "menu");
    page.expect_no_console_errors().await.expect("no errors");
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn tabs_swapped_in_with_outer_html_get_the_keyboard_model() {
    // htmx fires afterSwap on the new element; `detail.target` is the old one.
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/").await;
    page.click("#swap-outer").await.expect("swap");
    wait_for(&page, "!!document.getElementById('o-tab-o-profile')").await;
    wait_for(
        &page,
        "document.getElementById('o').classList.contains('autumn-tabs--enhanced')",
    )
    .await;
    focus(&page, "o-tab-o-profile").await;
    press(&page, "ArrowRight").await;
    assert_selected_tab(&page, "o", "security").await;
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn tabs_whose_contents_htmx_replaces_init_again() {
    // innerHTML into the root: the root stays, the tablist is new.
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/").await;
    eval_bool(
        &page,
        "window.__oldList = document.querySelector('#t > [role=tablist]'); true",
    )
    .await;
    page.click("#swap-inner").await.expect("swap");
    wait_for(
        &page,
        "document.querySelector('#t > [role=tablist]') !== window.__oldList \
         && document.getElementById('t-tab-t-security').getAttribute('tabindex') === '-1'",
    )
    .await;
    assert_selected_tab(&page, "t", "profile").await;
    focus(&page, "t-tab-t-profile").await;
    press(&page, "ArrowRight").await;
    assert_selected_tab(&page, "t", "security").await;
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn replaced_tabs_keep_the_selection_after_htmx_settles() {
    // htmx puts the response's `class` back after afterSwap. The selected
    // tab must keep its highlight when that happens.
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/#t-billing").await;
    assert_selected_tab(&page, "t", "billing").await;
    eval_bool(
        &page,
        "window.__oldList = document.querySelector('#t > [role=tablist]'); true",
    )
    .await;
    page.click("#swap-inner").await.expect("swap");
    wait_for(
        &page,
        "document.querySelector('#t > [role=tablist]') !== window.__oldList",
    )
    .await;
    page.expect_hx_settle().await.expect("settle");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_selected_tab(&page, "t", "billing").await;
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn tabs_replaced_by_the_same_root_stay_enhanced_after_settle() {
    // outerHTML with the same id: htmx puts the response's root `class`
    // back after afterSwap, which drops `autumn-tabs--enhanced`.
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/#t-security").await;
    eval_bool(
        &page,
        "window.__oldRoot = document.getElementById('t'); true",
    )
    .await;
    page.click("#swap-same").await.expect("swap");
    wait_for(&page, "document.getElementById('t') !== window.__oldRoot").await;
    page.expect_hx_settle().await.expect("settle");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        eval_bool(
            &page,
            "document.getElementById('t').classList.contains('autumn-tabs--enhanced')"
        )
        .await,
        "the replaced root keeps the enhanced class"
    );
    assert_selected_tab(&page, "t", "security").await;
    focus(&page, "t-tab-t-security").await;
    press(&page, "ArrowRight").await;
    assert_selected_tab(&page, "t", "billing").await;
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn an_in_page_link_selects_the_tab_it_points_into() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/").await;
    page.click("#jump").await.expect("click link");
    wait_for(
        &page,
        "document.getElementById('t-tab-t-billing').getAttribute('aria-selected') === 'true'",
    )
    .await;
    assert_selected_tab(&page, "t", "billing").await;
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn rtl_tabs_swap_left_and_right() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/rtl").await;
    focus(&page, "r-tab-r-profile").await;
    press(&page, "ArrowLeft").await;
    assert_eq!(active_id(&page).await, "r-tab-r-security");
    press(&page, "ArrowRight").await;
    assert_eq!(active_id(&page).await, "r-tab-r-profile");
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn tabs_swapped_in_by_htmx_get_the_keyboard_model() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/").await;
    page.click("#swap").await.expect("swap");
    wait_for(&page, "!!document.getElementById('s-tab-s-profile')").await;
    wait_for(
        &page,
        "document.getElementById('s-tab-s-security').getAttribute('tabindex') === '-1'",
    )
    .await;
    focus(&page, "s-tab-s-profile").await;
    press(&page, "ArrowRight").await;
    assert_selected_tab(&page, "s", "security").await;
}

// ── Combobox ────────────────────────────────────────────────────────────

/// Type `text` into the combobox and wait for at least one option.
async fn search(page: &Page, text: &str) {
    page.fill("#ac-query", text).await.expect("fill");
    wait_for(
        page,
        "document.querySelectorAll('#ac-options [role=option]').length > 0 \
         && document.getElementById('ac-query').getAttribute('aria-expanded') === 'true'",
    )
    .await;
}

async fn active_option_text(page: &Page) -> String {
    eval_string(
        page,
        "(() => { const i = document.getElementById('ac-query'); \
           const id = i.getAttribute('aria-activedescendant'); \
           const o = id && document.getElementById(id); \
           return o && o.getAttribute('aria-selected') === 'true' ? o.textContent.trim() : ''; })()",
    )
    .await
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn combobox_arrow_keys_move_the_active_option_and_enter_selects() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/").await;
    assert_eq!(attr(&page, "ac-query", "aria-expanded").await, "false");

    search(&page, "g").await;
    assert_eq!(active_id(&page).await, "ac-query");

    press(&page, "ArrowDown").await;
    assert_eq!(active_option_text(&page).await, "Green");
    press(&page, "ArrowDown").await;
    assert_eq!(active_option_text(&page).await, "Grey");
    press(&page, "ArrowUp").await;
    assert_eq!(active_option_text(&page).await, "Green");
    press(&page, "ArrowUp").await;
    assert_eq!(active_option_text(&page).await, "Gold", "wraps to last");
    assert_eq!(active_id(&page).await, "ac-query", "focus stays on input");

    press(&page, "Enter").await;
    assert_eq!(
        eval_string(&page, "document.getElementById('ac-value').value").await,
        "3"
    );
    assert_eq!(
        eval_string(&page, "document.getElementById('ac-query').value").await,
        "Gold"
    );
    assert_eq!(attr(&page, "ac-query", "aria-expanded").await, "false");
    assert_eq!(attr(&page, "ac-query", "aria-activedescendant").await, "");
    assert!(
        eval_bool(&page, "location.pathname === '/'").await,
        "Enter on an option does not submit the form"
    );
    page.expect_no_console_errors().await.expect("no errors");
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn combobox_escape_closes_and_arrow_down_opens_again() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/").await;
    search(&page, "gr").await;
    press(&page, "ArrowDown").await;

    press(&page, "Escape").await;
    assert_eq!(attr(&page, "ac-query", "aria-expanded").await, "false");
    assert_eq!(attr(&page, "ac-query", "aria-activedescendant").await, "");
    assert!(!shown(&page, "ac-options").await, "listbox hidden");
    assert_eq!(active_id(&page).await, "ac-query");

    press(&page, "ArrowDown").await;
    assert_eq!(attr(&page, "ac-query", "aria-expanded").await, "true");
    assert_eq!(active_option_text(&page).await, "Green");
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn combobox_aria_expanded_is_false_without_options() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/").await;
    page.fill("#ac-query", "zz").await.expect("fill");
    wait_for(
        &page,
        "document.querySelector('#ac-options [role=status]') !== null",
    )
    .await;
    assert_eq!(attr(&page, "ac-query", "aria-expanded").await, "false");
    press(&page, "ArrowDown").await;
    assert_eq!(attr(&page, "ac-query", "aria-activedescendant").await, "");
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn editing_the_query_clears_the_active_option() {
    // Enter during the debounce must not commit an option of the old query.
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/").await;
    page.fill("#slow-query", "g").await.expect("fill");
    wait_for(
        &page,
        "document.querySelectorAll('#slow-options [role=option]').length > 0",
    )
    .await;
    press(&page, "ArrowDown").await;
    assert_ne!(attr(&page, "slow-query", "aria-activedescendant").await, "");

    press(&page, "r").await;
    assert_eq!(attr(&page, "slow-query", "aria-activedescendant").await, "");
    press(&page, "Enter").await;
    assert_eq!(
        eval_string(&page, "document.getElementById('slow-value').value").await,
        ""
    );
    assert_eq!(
        eval_string(&page, "document.getElementById('slow-query').value").await,
        "gr"
    );
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn a_late_response_does_not_reopen_a_dismissed_popup() {
    // Escape during the debounce: the pending response must stay closed.
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/").await;
    page.fill("#slow-query", "g").await.expect("fill");
    wait_for(
        &page,
        "document.getElementById('slow-query').getAttribute('aria-expanded') === 'true'",
    )
    .await;
    press(&page, "r").await;
    press(&page, "Escape").await;
    assert_eq!(attr(&page, "slow-query", "aria-expanded").await, "false");
    // The 1500 ms debounce ends and the "gr" results arrive.
    wait_for(
        &page,
        "[...document.querySelectorAll('#slow-options [role=option]')].every(o => \
           o.textContent.trim().toLowerCase().startsWith('gr'))",
    )
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(attr(&page, "slow-query", "aria-expanded").await, "false");
    assert!(
        !shown(&page, "slow-options").await,
        "the popup stays closed"
    );

    // ArrowDown is an explicit request to open it again.
    press(&page, "ArrowDown").await;
    assert_eq!(attr(&page, "slow-query", "aria-expanded").await, "true");
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn combobox_closes_when_the_user_clicks_outside() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/").await;
    search(&page, "g").await;
    page.click("#title").await.expect("click outside");
    assert_eq!(attr(&page, "ac-query", "aria-expanded").await, "false");
    assert!(!shown(&page, "ac-options").await);
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn clicking_an_option_keeps_focus_on_the_input() {
    // APG combobox: DOM focus stays on the input, also for a mouse pick.
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/").await;
    search(&page, "gr").await;
    page.click("#ac-options [data-value='2']")
        .await
        .expect("click option");
    assert_eq!(
        eval_string(&page, "document.getElementById('ac-value').value").await,
        "2"
    );
    assert_eq!(active_id(&page).await, "ac-query");
    press(&page, "Escape").await;
    assert_eq!(active_id(&page).await, "ac-query");
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn an_empty_listbox_stays_in_the_accessibility_tree() {
    // Its `aria-live` region must exist before the first results arrive.
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/").await;
    assert!(
        eval_bool(
            &page,
            "document.getElementById('ac-options').hidden === false"
        )
        .await,
        "an empty listbox is not hidden"
    );
}

// ── Tags mode (#3410) ───────────────────────────────────────────────────

/// The values that widget `id` submits: its enabled, named chip inputs.
async fn chip_values(page: &Page, id: &str) -> String {
    settle().await;
    eval_string(
        page,
        &format!(
            "[...document.querySelectorAll('#{id}-chips [data-ac-chip] input[type=hidden]')]\
             .filter(i => !i.disabled && i.name).map(i => i.value).join(',')"
        ),
    )
    .await
}

/// The `aria-label` of the focused element.
async fn focused_label(page: &Page) -> String {
    settle().await;
    eval_string(
        page,
        "document.activeElement ? (document.activeElement.getAttribute('aria-label') || '') : ''",
    )
    .await
}

/// Type `text` into widget `id` and wait for a shown option.
async fn search_in(page: &Page, id: &str, text: &str) {
    page.fill(&format!("#{id}-query"), text)
        .await
        .expect("fill");
    wait_for(
        page,
        &format!(
            "[...document.querySelectorAll('#{id}-options [role=option]')].some(o => !o.hidden) \
             && document.getElementById('{id}-query').getAttribute('aria-expanded') === 'true'"
        ),
    )
    .await;
}

/// Add the option of widget `id` whose text is `label`.
async fn pick(page: &Page, id: &str, query: &str, label: &str) {
    search_in(page, id, query).await;
    for _ in 0..COLORS.len() {
        press(page, "ArrowDown").await;
        let text = eval_string(
            page,
            &format!(
                "(() => {{ const i = document.getElementById('{id}-query'); \
                   const o = document.getElementById(i.getAttribute('aria-activedescendant') || ''); \
                   return o ? o.textContent.trim() : ''; }})()"
            ),
        )
        .await;
        if text == label {
            press(page, "Enter").await;
            return;
        }
    }
    panic!("no option {label} for {query}");
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn tags_enter_adds_a_chip_and_keeps_focus_on_the_input() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/").await;
    assert_eq!(chip_values(&page, "tags").await, "4", "server pick submits");
    assert!(
        shown(&page, "tags-chips").await,
        "the runtime shows the chips"
    );

    search_in(&page, "tags", "g").await;
    press(&page, "ArrowDown").await;
    press(&page, "Enter").await;
    assert_eq!(chip_values(&page, "tags").await, "4,1");
    assert_eq!(
        eval_string(&page, "document.getElementById('tags-query').value").await,
        "",
        "the query clears"
    );
    assert_eq!(attr(&page, "tags-query", "aria-expanded").await, "false");
    assert_eq!(active_id(&page).await, "tags-query");
    assert!(
        eval_bool(
            &page,
            "document.querySelector('#tags-chips [data-ac-chip]:last-child [data-ac-chip-remove]')\
             .getAttribute('aria-label') === 'Remove Green'"
        )
        .await,
        "a new chip has a labelled remove button"
    );
    assert!(
        eval_bool(&page, "location.pathname === '/'").await,
        "Enter on an option does not submit the form"
    );
    page.expect_no_console_errors().await.expect("no errors");
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn tags_hide_an_option_that_is_already_a_chip() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/").await;
    pick(&page, "tags", "g", "Green").await;
    search_in(&page, "tags", "g").await;
    press(&page, "ArrowDown").await;
    assert_eq!(
        eval_string(
            &page,
            "document.getElementById(document.getElementById('tags-query')\
             .getAttribute('aria-activedescendant')).textContent.trim()"
        )
        .await,
        "Grey",
        "Green is a chip, so the list skips it"
    );

    // Blue is the only match and a chip: the popup has nothing to show.
    // The runtime removes the option, so the empty listbox has no size.
    page.fill("#tags-query", "b").await.expect("fill");
    assert_eq!(attr(&page, "tags-query", "aria-expanded").await, "false");
    assert!(
        eval_bool(
            &page,
            "document.getElementById('tags-options').childElementCount === 0 \
             && document.getElementById('tags-options').getBoundingClientRect().height === 0"
        )
        .await,
        "no empty box"
    );
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn tags_backspace_in_an_empty_query_removes_the_last_chip() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/").await;
    pick(&page, "tags", "g", "Green").await;
    assert_eq!(chip_values(&page, "tags").await, "4,1");

    // Backspace with text edits the text.
    page.fill("#tags-query", "gr").await.expect("fill");
    press(&page, "Backspace").await;
    assert_eq!(chip_values(&page, "tags").await, "4,1");
    assert_eq!(
        eval_string(&page, "document.getElementById('tags-query').value").await,
        "g"
    );

    press(&page, "Backspace").await;
    assert_eq!(
        chip_values(&page, "tags").await,
        "4,1",
        "the text goes first"
    );
    press(&page, "Backspace").await;
    assert_eq!(chip_values(&page, "tags").await, "4");
    press(&page, "Backspace").await;
    assert_eq!(chip_values(&page, "tags").await, "");
    assert_eq!(active_id(&page).await, "tags-query");
    page.expect_no_console_errors().await.expect("no errors");
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn tags_arrow_keys_move_between_chips_and_delete_removes_one() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/").await;
    pick(&page, "tags", "g", "Green").await;
    pick(&page, "tags", "go", "Gold").await;
    assert_eq!(chip_values(&page, "tags").await, "4,1,3");

    // ArrowLeft at the start of the input goes to the last chip.
    press(&page, "ArrowLeft").await;
    assert_eq!(focused_label(&page).await, "Remove Gold");
    press(&page, "ArrowLeft").await;
    assert_eq!(focused_label(&page).await, "Remove Green");
    press(&page, "Home").await;
    assert_eq!(focused_label(&page).await, "Remove Blue");
    press(&page, "ArrowLeft").await;
    assert_eq!(focused_label(&page).await, "Remove Blue", "no wrap");
    press(&page, "End").await;
    assert_eq!(focused_label(&page).await, "Remove Gold");
    // ArrowRight on the last chip goes back to the input.
    press(&page, "ArrowRight").await;
    assert_eq!(active_id(&page).await, "tags-query");

    // Delete removes a chip. Focus goes to the next chip.
    press(&page, "ArrowLeft").await;
    press(&page, "ArrowLeft").await;
    assert_eq!(focused_label(&page).await, "Remove Green");
    press(&page, "Delete").await;
    assert_eq!(chip_values(&page, "tags").await, "4,3");
    assert_eq!(focused_label(&page).await, "Remove Gold");
    // Backspace on the last chip: focus goes to the previous chip.
    press(&page, "Backspace").await;
    assert_eq!(chip_values(&page, "tags").await, "4");
    assert_eq!(focused_label(&page).await, "Remove Blue");
    // Enter activates the button. With no chip left, focus goes to the input.
    press(&page, "Enter").await;
    assert_eq!(chip_values(&page, "tags").await, "");
    assert_eq!(active_id(&page).await, "tags-query");
    assert!(
        eval_bool(&page, "location.pathname === '/'").await,
        "no submit"
    );
    page.expect_no_console_errors().await.expect("no errors");
}

/// Send a repeated `keydown` of `key` to the focused element, as a held
/// key does. `Page::press` cannot set `repeat`.
async fn hold(page: &Page, key: &str) {
    page.evaluate(format!(
        "document.activeElement.dispatchEvent(new KeyboardEvent('keydown', \
         {{ key: '{key}', repeat: true, bubbles: true, cancelable: true }}))"
    ))
    .await
    .expect("dispatch a repeated key");
    settle().await;
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn tags_a_held_key_removes_no_extra_chips() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/").await;
    pick(&page, "tags", "g", "Green").await;
    pick(&page, "tags", "go", "Gold").await;
    assert_eq!(chip_values(&page, "tags").await, "4,1,3");

    // In the empty input: one press removes one chip. Repeats do nothing.
    press(&page, "Backspace").await;
    for _ in 0..3 {
        hold(&page, "Backspace").await;
    }
    assert_eq!(chip_values(&page, "tags").await, "4,1");

    // On a chip: focus moves to the next chip, which a repeat must keep.
    press(&page, "ArrowLeft").await;
    press(&page, "Home").await;
    assert_eq!(focused_label(&page).await, "Remove Blue");
    press(&page, "Delete").await;
    assert_eq!(focused_label(&page).await, "Remove Green");
    for key in ["Delete", "Backspace"] {
        hold(&page, key).await;
    }
    assert_eq!(chip_values(&page, "tags").await, "1");
    assert_eq!(focused_label(&page).await, "Remove Green");
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn tags_chip_list_is_one_tab_stop_before_the_input() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/").await;
    pick(&page, "tags", "g", "Green").await;
    assert_eq!(
        eval_string(
            &page,
            "[...document.querySelectorAll('#tags-chips [data-ac-chip-remove]')]\
             .map(b => b.tabIndex).join(',')"
        )
        .await,
        "0,-1",
        "one tab stop"
    );
    focus(&page, "tags-query").await;
    press(&page, "ArrowLeft").await;
    press(&page, "Tab").await;
    assert_eq!(
        active_id(&page).await,
        "tags-query",
        "Tab leaves the chip list"
    );
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn tags_clicking_a_remove_button_removes_its_chip() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/").await;
    pick(&page, "tags", "g", "Green").await;
    page.click("#tags-chips [data-ac-chip]:first-child [data-ac-chip-remove]")
        .await
        .expect("click remove");
    assert_eq!(chip_values(&page, "tags").await, "1");
    assert_eq!(
        active_id(&page).await,
        "tags-query",
        "focus goes to the input"
    );
    page.expect_no_console_errors().await.expect("no errors");
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn tags_free_text_enter_adds_the_typed_text_once() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/").await;
    focus(&page, "words-query").await;
    for _ in 0..2 {
        page.fill("#words-query", "teal").await.expect("fill");
        press(&page, "Enter").await;
    }
    assert_eq!(chip_values(&page, "words").await, "teal", "no duplicate");
    assert_eq!(
        eval_string(&page, "document.getElementById('words-query').value").await,
        ""
    );
    assert!(
        eval_bool(&page, "location.pathname === '/'").await,
        "Enter on typed text does not submit the form"
    );
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn tags_create_option_adds_its_value_not_its_label() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/").await;
    search_in(&page, "words", "te").await;
    press(&page, "ArrowDown").await;
    press(&page, "Enter").await;
    assert_eq!(chip_values(&page, "words").await, "te");
    assert!(
        eval_bool(
            &page,
            "document.querySelector('#words-chips [data-ac-chip-label]').textContent === 'te'"
        )
        .await,
        "the chip shows the value, not \"Create …\""
    );
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn tags_submit_as_repeated_keys() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/").await;
    pick(&page, "tags", "g", "Green").await;
    focus(&page, "words-query").await;
    page.fill("#words-query", "teal").await.expect("fill");
    press(&page, "Enter").await;
    page.click("#multi-save").await.expect("submit");
    wait_for(&page, "location.pathname === '/submitted-multi'").await;
    assert_eq!(
        eval_string(&page, "document.body.textContent.trim()").await,
        r#"color_ids=["4", "1"] words=["teal"]"#
    );
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn tags_free_text_submit_adds_the_typed_text() {
    // Text that the user typed but did not add with Enter is not lost.
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/").await;
    page.fill("#words-query", "teal").await.expect("fill");
    // Close the popup: it covers the Save button.
    press(&page, "Escape").await;
    page.click("#multi-save").await.expect("submit");
    wait_for(&page, "location.pathname === '/submitted-multi'").await;
    assert_eq!(
        eval_string(&page, "document.body.textContent.trim()").await,
        r#"color_ids=["4"] words=["teal"]"#
    );
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn tags_status_region_says_what_was_added_and_removed() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/").await;
    let status = "document.querySelector('#tags-wrapper [data-ac-status]').textContent";
    pick(&page, "tags", "g", "Green").await;
    assert_eq!(eval_string(&page, status).await, "Green added");
    press(&page, "Backspace").await;
    assert_eq!(eval_string(&page, status).await, "Green removed");
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn tags_swapped_in_by_htmx_send_each_value_once() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/").await;
    page.click("#swap-tags").await.expect("swap");
    wait_for(
        &page,
        "document.getElementById('swapped-tags-chips') !== null",
    )
    .await;
    wait_for(
        &page,
        "document.getElementById('swapped-tags-chips').hidden === false",
    )
    .await;
    assert!(
        eval_bool(
            &page,
            "[...document.querySelectorAll('#swapped select')].every(s => s.disabled)"
        )
        .await,
        "the runtime disables a live noscript select"
    );
    page.click("#swapped-save").await.expect("submit");
    wait_for(&page, "location.pathname === '/submitted-multi'").await;
    assert_eq!(
        eval_string(&page, "document.body.textContent.trim()").await,
        r#"color_ids=["2"] words=[]"#
    );
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn rtl_tags_swap_left_and_right() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/rtl").await;
    focus(&page, "rtags-query").await;
    press(&page, "ArrowRight").await;
    assert_eq!(focused_label(&page).await, "Remove Blue");
    press(&page, "ArrowRight").await;
    assert_eq!(focused_label(&page).await, "Remove Green");
    press(&page, "ArrowLeft").await;
    press(&page, "ArrowLeft").await;
    assert_eq!(active_id(&page).await, "rtags-query");
}

// ── Menu button ─────────────────────────────────────────────────────────

const TOGGLE: &str = "autumn-nav-menu-1-toggle";
const MENU: &str = "autumn-nav-menu-1";

#[tokio::test]
#[ignore = "requires Chromium"]
async fn the_menu_button_has_roles_and_one_tab_stop() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/").await;
    assert_eq!(attr(&page, TOGGLE, "aria-haspopup").await, "menu");
    assert_eq!(attr(&page, TOGGLE, "aria-expanded").await, "false");
    assert_eq!(attr(&page, MENU, "role").await, "menu");
    assert_eq!(attr(&page, MENU, "aria-labelledby").await, TOGGLE);
    assert!(
        eval_bool(
            &page,
            &format!(
                "(() => {{ const items = [...document.querySelectorAll('#{MENU} a')]; \
                 return items.every(a => a.getAttribute('role') === 'menuitem' \
                   && a.parentElement.getAttribute('role') === 'none') \
                 && items.filter(a => a.getAttribute('tabindex') === '0').length === 1 \
                 && items.filter(a => a.getAttribute('tabindex') === '-1').length === items.length - 1; }})()"
            ),
        )
        .await
    );
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn menu_button_keyboard_model() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/").await;

    focus(&page, TOGGLE).await;
    press(&page, "Enter").await;
    assert_eq!(attr(&page, TOGGLE, "aria-expanded").await, "true");
    assert!(shown(&page, MENU).await);
    assert_eq!(
        active_text(&page).await,
        "Widgets",
        "Enter focuses first item"
    );

    press(&page, "ArrowDown").await;
    assert_eq!(active_text(&page).await, "Gadgets");
    press(&page, "End").await;
    assert_eq!(active_text(&page).await, "Gizmos");
    press(&page, "ArrowDown").await;
    assert_eq!(active_text(&page).await, "Widgets", "wraps to first");
    press(&page, "ArrowUp").await;
    assert_eq!(active_text(&page).await, "Gizmos", "wraps to last");
    press(&page, "Home").await;
    assert_eq!(active_text(&page).await, "Widgets");

    // Typeahead: "g" goes to the next item that starts with "g".
    press(&page, "g").await;
    assert_eq!(active_text(&page).await, "Gadgets");
    press(&page, "g").await;
    assert_eq!(active_text(&page).await, "Gizmos");

    press(&page, "Escape").await;
    assert_eq!(attr(&page, TOGGLE, "aria-expanded").await, "false");
    assert!(!shown(&page, MENU).await);
    assert_eq!(active_id(&page).await, TOGGLE, "Escape returns focus");

    press(&page, "ArrowUp").await;
    assert_eq!(attr(&page, TOGGLE, "aria-expanded").await, "true");
    assert_eq!(
        active_text(&page).await,
        "Gizmos",
        "ArrowUp focuses last item"
    );

    press(&page, "Escape").await;
    press(&page, "ArrowDown").await;
    assert_eq!(
        active_text(&page).await,
        "Widgets",
        "ArrowDown focuses first"
    );

    press(&page, "Escape").await;
    press(&page, " ").await;
    assert_eq!(active_text(&page).await, "Widgets", "Space focuses first");
    page.expect_no_console_errors().await.expect("no errors");
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn menu_closes_on_tab_and_on_a_click_outside() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/").await;

    page.click(&format!("#{TOGGLE}")).await.expect("open");
    assert_eq!(attr(&page, TOGGLE, "aria-expanded").await, "true");
    page.click("#title").await.expect("click outside");
    assert_eq!(attr(&page, TOGGLE, "aria-expanded").await, "false");

    focus(&page, TOGGLE).await;
    press(&page, "Enter").await;
    assert_eq!(attr(&page, TOGGLE, "aria-expanded").await, "true");
    press(&page, "Tab").await;
    assert_eq!(attr(&page, TOGGLE, "aria-expanded").await, "false");
    assert!(!shown(&page, MENU).await);
}

// ── No-JS fallback and CSP ──────────────────────────────────────────────

#[tokio::test]
#[ignore = "requires Chromium"]
async fn every_fallback_works_with_javascript_disabled() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    page.set_javascript_enabled(false).await.expect("js off");
    page.visit("/#t-billing").await.expect("visit");
    wait_for(&page, "document.readyState === 'complete'").await;
    assert!(
        eval_bool(&page, "window.__autumnWidgets === undefined").await,
        "the runtime must not run"
    );

    // Tabs: `:target` shows the deep-linked panel only.
    assert!(shown(&page, "t-billing").await);
    assert!(!shown(&page, "t-profile").await);
    assert!(
        eval_bool(
            &page,
            "[...document.querySelectorAll('[role=tab]')].every(t => t.hasAttribute('href'))"
        )
        .await
    );

    // Nav: the dropdown is a closed popover with the menu roles. The popover
    // suite clicks it open; `Page::click` needs a script.
    assert!(!shown(&page, MENU).await, "closed at first");
    assert_eq!(attr(&page, MENU, "role").await, "menu");
    assert!(
        eval_bool(
            &page,
            &format!("document.getElementById('{TOGGLE}').getAttribute('commandfor') === '{MENU}'")
        )
        .await,
        "commandfor opens it with no JS"
    );

    // Autocomplete: the `<noscript>` select is live and inside the form.
    assert!(
        eval_bool(
            &page,
            "(() => { const s = document.querySelector('#pick select[name=color_id]'); \
             return !!s && s.options.length === 5 && getComputedStyle(s).display !== 'none'; })()"
        )
        .await,
        "noscript select must render"
    );

    // Tags (#3410): a `<select multiple>` holds the server pick. The chip
    // list is hidden and its inputs do not submit, so the form sends each value one time.
    assert!(
        eval_bool(
            &page,
            "(() => { const s = document.querySelector('#multi select[name=color_ids]'); \
             return !!s && s.multiple && [...s.selectedOptions].map(o => o.value).join() === '4' \
               && getComputedStyle(s).display !== 'none'; })()"
        )
        .await,
        "noscript select multiple must render with the pick"
    );
    assert!(
        !shown(&page, "tags-chips").await,
        "chips hidden with JS off"
    );
    page.evaluate("document.getElementById('multi').requestSubmit()")
        .await
        .expect("submit");
    wait_for(&page, "location.pathname === '/submitted-multi'").await;
    assert_eq!(
        eval_string(&page, "document.body.textContent.trim()").await,
        r#"color_ids=["4"] words=[]"#
    );
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn widgets_run_under_the_default_csp_with_no_inline_script() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/").await;
    let csp = eval_string(
        &page,
        "fetch('/').then(r => r.headers.get('content-security-policy') || '')",
    )
    .await;
    let script_src = csp
        .split(';')
        .map(str::trim)
        .find(|d| d.starts_with("script-src"))
        .unwrap_or_default();
    assert_eq!(script_src, "script-src 'self'", "{csp}");
    assert!(
        eval_bool(
            &page,
            "document.querySelectorAll('script:not([src])').length === 0 && \
             ![...document.querySelectorAll('*')].some(e => \
               [...e.attributes].some(a => a.name.startsWith('on')))"
        )
        .await,
        "no inline script and no on* handler"
    );

    // Drive every widget once, then check the console.
    focus(&page, "t-tab-t-profile").await;
    press(&page, "ArrowRight").await;
    focus(&page, TOGGLE).await;
    press(&page, "Enter").await;
    press(&page, "Escape").await;
    search(&page, "g").await;
    press(&page, "ArrowDown").await;
    press(&page, "Enter").await;
    pick(&page, "tags", "g", "Green").await;
    press(&page, "Backspace").await;
    press(&page, "ArrowLeft").await;
    press(&page, "Delete").await;
    page.expect_no_console_errors()
        .await
        .expect("no CSP errors");
}

// ── Tree view ───────────────────────────────────────────────────────────

/// Record each `autumn:expand` and `autumn:collapse` in `window.treeEvents`.
async fn count_tree_events(page: &Page) {
    assert!(
        eval_bool(
            page,
            "(() => { window.treeEvents = []; \
             ['autumn:expand', 'autumn:collapse'].forEach(n => document.addEventListener(n, \
             e => window.treeEvents.push(n + ' ' + e.target.id + ' ' + e.detail.item.id))); \
             return true; })()"
        )
        .await
    );
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn tree_down_and_up_skip_a_closed_branch() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/tree").await;
    assert_eq!(attr(&page, "t-src", "tabindex").await, "0");
    focus(&page, "t-src").await;
    press(&page, "ArrowDown").await;
    assert_eq!(active_id(&page).await, "t-docs");
    press(&page, "End").await;
    assert_eq!(active_id(&page).await, "t-readme");
    press(&page, "ArrowUp").await;
    assert_eq!(active_id(&page).await, "t-docs");
    // Typeahead skips `guide.md` in the closed `docs`.
    press(&page, "g").await;
    assert_eq!(active_id(&page).await, "t-docs");
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn tree_right_opens_a_branch_then_moves_to_its_first_child() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/tree").await;
    count_tree_events(&page).await;
    focus(&page, "t-src").await;
    press(&page, "ArrowRight").await;
    assert_eq!(attr(&page, "t-src", "aria-expanded").await, "true");
    assert_eq!(
        active_id(&page).await,
        "t-src",
        "Right on a closed node keeps focus"
    );
    assert!(
        eval_bool(
            &page,
            "!document.querySelector('#t-src > [role=group]').hidden"
        )
        .await
    );
    // A new visible item takes focus by script, and the tab stop stays.
    assert_eq!(attr(&page, "t-lib", "tabindex").await, "-1");
    assert_eq!(attr(&page, "t-src", "tabindex").await, "0");
    focus(&page, "t-lib").await;
    assert_eq!(
        attr(&page, "t-lib", "tabindex").await,
        "0",
        "focus moves the tab stop"
    );
    focus(&page, "t-src").await;
    press(&page, "ArrowRight").await;
    assert_eq!(active_id(&page).await, "t-lib");
    // Right on an end node does nothing.
    press(&page, "ArrowRight").await;
    assert_eq!(active_id(&page).await, "t-lib");
    press(&page, "ArrowDown").await;
    assert_eq!(active_id(&page).await, "t-bin");
    assert_eq!(
        eval_string(&page, "window.treeEvents.join(',')").await,
        "autumn:expand t-src t-src"
    );
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn tree_left_closes_a_branch_then_moves_to_its_parent() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/tree").await;
    count_tree_events(&page).await;
    focus(&page, "t-src").await;
    press(&page, "ArrowRight").await;
    press(&page, "ArrowRight").await;
    assert_eq!(active_id(&page).await, "t-lib");
    press(&page, "ArrowLeft").await;
    assert_eq!(
        active_id(&page).await,
        "t-src",
        "Left on a child moves to the parent"
    );
    assert_eq!(attr(&page, "t-src", "aria-expanded").await, "true");
    press(&page, "ArrowLeft").await;
    assert_eq!(attr(&page, "t-src", "aria-expanded").await, "false");
    assert_eq!(active_id(&page).await, "t-src");
    // Left on a closed root node does nothing.
    press(&page, "ArrowLeft").await;
    assert_eq!(active_id(&page).await, "t-src");
    assert_eq!(
        eval_string(&page, "window.treeEvents.join(',')").await,
        "autumn:expand t-src t-src,autumn:collapse t-src t-src"
    );
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn tree_star_opens_all_siblings() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/tree").await;
    focus(&page, "t-readme").await;
    press(&page, "*").await;
    assert_eq!(attr(&page, "t-src", "aria-expanded").await, "true");
    assert_eq!(attr(&page, "t-docs", "aria-expanded").await, "true");
    // Only the siblings: `bin` stays closed.
    assert_eq!(attr(&page, "t-bin", "aria-expanded").await, "false");
    assert_eq!(active_id(&page).await, "t-readme");
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn rtl_tree_swaps_left_and_right() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/tree-rtl").await;
    focus(&page, "t-src").await;
    press(&page, "ArrowLeft").await;
    assert_eq!(attr(&page, "t-src", "aria-expanded").await, "true");
    press(&page, "ArrowLeft").await;
    assert_eq!(active_id(&page).await, "t-lib");
    press(&page, "ArrowRight").await;
    assert_eq!(active_id(&page).await, "t-src");
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn tree_keys_follow_aria_owns_for_row_items() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/tree-rows").await;
    focus(&page, "r-src").await;
    press(&page, "ArrowDown").await;
    assert_eq!(
        active_id(&page).await,
        "r-docs",
        "the closed branch is skipped"
    );
    press(&page, "ArrowUp").await;
    press(&page, "ArrowRight").await;
    assert_eq!(attr(&page, "r-src", "aria-expanded").await, "true");
    assert!(eval_bool(&page, "!document.getElementById('r-src-group').hidden").await);
    press(&page, "ArrowRight").await;
    assert_eq!(active_id(&page).await, "r-lib");
    press(&page, "ArrowDown").await;
    // `*` opens each closed node at this level, the focused one too.
    press(&page, "*").await;
    assert_eq!(attr(&page, "r-bin", "aria-expanded").await, "true");
    assert_eq!(active_id(&page).await, "r-bin");
    press(&page, "ArrowRight").await;
    assert_eq!(active_id(&page).await, "r-main");
    press(&page, "ArrowLeft").await;
    assert_eq!(
        active_id(&page).await,
        "r-bin",
        "Left follows aria-owns to the parent"
    );
    press(&page, "ArrowLeft").await;
    press(&page, "ArrowLeft").await;
    assert_eq!(active_id(&page).await, "r-src");
    press(&page, "*").await;
    assert_eq!(attr(&page, "r-docs", "aria-expanded").await, "true");
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn tree_disabled_items_take_focus_so_their_children_stay_reachable() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/tree-disabled").await;
    assert_eq!(attr(&page, "dt-d", "tabindex").await, "0");
    focus(&page, "dt-d").await;
    press(&page, "ArrowRight").await;
    assert_eq!(attr(&page, "dt-d", "aria-expanded").await, "true");
    // The first child is disabled. Right still moves to it.
    press(&page, "ArrowRight").await;
    assert_eq!(active_id(&page).await, "dt-x");
    press(&page, "ArrowDown").await;
    assert_eq!(active_id(&page).await, "dt-z");
    press(&page, "ArrowLeft").await;
    assert_eq!(active_id(&page).await, "dt-d");
}
