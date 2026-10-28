//! The widget keyboard core in a real browser (issue #3397).
//!
//! Each test sends real key events and checks the WAI-ARIA APG keyboard
//! model for tabs, the combobox and the menu button. Other tests check
//! the no-JS fallback and the default CSP.
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
    AutocompleteConfig, NavBarConfig, NavItem, NavMenu, autocomplete_empty_state,
    autocomplete_input, autocomplete_option, nav_bar, tabs,
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
    html! {
        (nav_bar("/", &nav))
        h1 id="title" { "Keyboard" }
        (settings_tabs("t"))
        form id="pick" action="/submitted" method="get" {
            (autocomplete_input("ac", "Color", &ac))
            button type="submit" { "Save" }
        }
        button id="swap" hx-get="/fragment" hx-target="#slot" { "Swap" }
        div id="slot" {}
        button id="swap-outer" hx-get="/fragment-outer" hx-target="#slot-outer" hx-swap="outerHTML" {
            "Swap outer"
        }
        div id="slot-outer" {}
        a id="jump" href="#t-billing" { "Go to billing" }
    }
}

#[get("/")]
async fn index() -> Markup {
    layout("ltr", &page_body())
}

#[get("/rtl")]
async fn rtl() -> Markup {
    layout("rtl", &settings_tabs("r"))
}

#[get("/fragment")]
async fn fragment() -> Markup {
    settings_tabs("s")
}

#[get("/fragment-outer")]
async fn fragment_outer() -> Markup {
    settings_tabs("o")
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

#[get("/submitted")]
async fn submitted() -> &'static str {
    "submitted"
}

async fn runner() -> SystemTestRunner {
    Box::pin(
        SystemTest::new()
            .routes(routes![
                index,
                rtl,
                fragment,
                fragment_outer,
                colors,
                submitted
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
    eval_string(
        page,
        "document.activeElement ? document.activeElement.id : ''",
    )
    .await
}

async fn active_text(page: &Page) -> String {
    eval_string(
        page,
        "document.activeElement ? document.activeElement.textContent.trim() : ''",
    )
    .await
}

async fn attr(page: &Page, id: &str, name: &str) -> String {
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

// ── Menu button ─────────────────────────────────────────────────────────

const TOGGLE: &str = "autumn-nav-menu-1-toggle";
const MENU: &str = "autumn-nav-menu-1";

#[tokio::test]
#[ignore = "requires Chromium"]
async fn the_runtime_adds_menu_button_roles() {
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

    // Nav: every dropdown link is visible.
    assert!(shown(&page, MENU).await);
    assert_eq!(attr(&page, TOGGLE, "aria-expanded").await, "true");
    assert_eq!(
        attr(&page, MENU, "role").await,
        "",
        "no role=menu without JS"
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
    page.expect_no_console_errors()
        .await
        .expect("no CSP errors");
}
