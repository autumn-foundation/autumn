//! The `/_stories` gallery in a real browser (issue #3398).
//!
//! - axe-core checks every story page in the light and the dark scheme. A
//!   serious or critical violation fails the test.
//! - Each interactive story has a `keyboard_<slug>` test. The
//!   `every_interactive_story_has_a_keyboard_test` gate in `stories.rs` keeps
//!   this list complete.
//! - Two canary tests prove that the checks fail on a seeded regression.
//!
//! axe-core is vendored at `tests/fixtures/axe-core/` and injected with CDP,
//! so the page CSP does not block it.
//!
//! Run:
//!
//! ```text
//! cargo test -p autumn-web --features system-tests \
//!   --test integration_tests stories_browser -- --include-ignored
//! ```

#![cfg(all(feature = "system-tests", feature = "maud", feature = "htmx"))]

use std::time::Duration;

use autumn_web::AppState;
use autumn_web::config::AutumnConfig;
use autumn_web::prelude::*;
use autumn_web::stories::{self, Story};
use autumn_web::system_test::{Page, SystemTest, SystemTestRunner};
use autumn_web::widgets::tabs;
use maud::{DOCTYPE, Markup, html};

// ── Harness ─────────────────────────────────────────────────────────────

/// A page with a known axe violation: an unlabelled input and an image
/// with no `alt`.
#[get("/seeded/axe")]
async fn seeded_axe() -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head { meta charset="utf-8"; title { "Seeded" } }
            body { main { input type="text" name="q"; img src="/x.png"; } }
        }
    }
}

/// Tabs without `autumn-widgets.js`, so arrow keys do nothing.
#[get("/seeded/tabs")]
async fn seeded_tabs() -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                title { "Seeded" }
                link rel="stylesheet" href="/static/css/autumn-widgets.css";
            }
            body { (story_tabs()) }
        }
    }
}

/// The markup of the Tabs story.
fn story_tabs() -> Markup {
    let panels = [
        (
            "demo-profile",
            "Profile",
            html! { p { "Profile settings" } },
        ),
        (
            "demo-security",
            "Security",
            html! { p { "Security settings" } },
        ),
        (
            "demo-billing",
            "Billing",
            html! { p { "Billing settings" } },
        ),
    ];
    tabs("settings-tabs", None, &panels)
}

/// Serve the builtin gallery plus the seeded pages.
async fn runner() -> SystemTestRunner {
    let mut config = AutumnConfig {
        profile: Some("test".into()),
        ..Default::default()
    };
    config.security.csrf.enabled = false;
    config.stories.enabled = true;
    let state = AppState::for_test().with_profile("test");
    state.insert_extension(config);
    state.insert_extension(stories::builtin());
    Box::pin(
        SystemTest::new()
            .state(state)
            .routes(routes![seeded_axe, seeded_tabs])
            .build(),
    )
    .await
    .expect("start runner")
}

/// Every builtin story slug.
fn slugs() -> Vec<String> {
    stories::builtin()
        .stories()
        .iter()
        .map(Story::slug)
        .map(str::to_owned)
        .collect()
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

/// Load `path`. Wait for the page, the widget runtime and htmx.
async fn open(page: &Page, path: &str) {
    page.visit(path).await.expect("visit");
    wait_for(page, "document.readyState === 'complete'").await;
    wait_for(page, "window.__autumnWidgets === true").await;
    page.expect_hx_settle().await.expect("htmx settles");
}

async fn press(page: &Page, key: &str) {
    page.press(key).await.expect("press");
}

/// Focus the first element that matches `selector`. The element must be in
/// the Tab order, so a keyboard user can reach it.
async fn focus(page: &Page, selector: &str) {
    assert!(
        eval_bool(
            page,
            &format!(
                "(() => {{ const e = document.querySelector({selector:?}); \
                 if (!e || e.tabIndex < 0) return false; \
                 e.focus(); return document.activeElement === e; }})()"
            ),
        )
        .await,
        "could not focus {selector}, or it is not in the Tab order"
    );
}

/// The runtime reacts to the `toggle` event, which the browser queues as a
/// task. Let it run before the test reads the page.
async fn settle() {
    tokio::time::sleep(Duration::from_millis(100)).await;
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

async fn shown(page: &Page, selector: &str) -> bool {
    eval_bool(
        page,
        &format!(
            "(() => {{ const e = document.querySelector({selector:?}); \
             return !!e && getComputedStyle(e).display !== 'none'; }})()"
        ),
    )
    .await
}

// ── axe-core ────────────────────────────────────────────────────────────

/// The vendored axe-core source.
fn axe_source() -> &'static str {
    static SOURCE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    SOURCE.get_or_init(|| {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/axe-core/axe.min.js");
        std::fs::read_to_string(&path)
            .unwrap_or_else(|err| panic!("read {}: {err}", path.display()))
    })
}

/// The serious and critical axe violations. `Page::axe_violations` is the
/// same check that widget pack conformance runs (#3400).
async fn axe_violations(page: &Page) -> Vec<String> {
    page.axe_violations(axe_source())
        .await
        .expect("run axe-core")
}

/// Select a gallery color scheme (`light` or `dark`). Check that the page
/// uses it, and wait until no CSS transition runs.
async fn select_scheme(page: &Page, scheme: &str) {
    assert!(
        eval_bool(
            page,
            &format!(
                "(() => {{ const r = document.getElementById('story-theme-{scheme}'); \
                 if (!r) return false; r.click(); return r.checked; }})()"
            ),
        )
        .await,
        "could not select the {scheme} scheme"
    );
    wait_for(
        page,
        &format!("getComputedStyle(document.documentElement).colorScheme === '{scheme}'"),
    )
    .await;
    // axe must not read a color in the middle of a transition. Wait for CSS
    // transitions only: the toast has a long exit animation.
    wait_for(
        page,
        "document.getAnimations().every(a => !(a instanceof CSSTransition))",
    )
    .await;
}

/// Check an open layer (dialog, menu, popup) with axe, in the light scheme
/// and in the dark scheme. axe does not check hidden content, so the page
/// sweep does not see these layers.
///
/// For each scheme: select it, call `open_layer`, run axe, press Escape.
/// The layer must be closed when the scheme changes: an open modal makes
/// the scheme switch inert.
async fn axe_in_both_schemes<F, Fut>(page: &Page, what: &str, open_layer: F)
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    for scheme in ["light", "dark"] {
        select_scheme(page, scheme).await;
        open_layer().await;
        let violations = axe_violations(page).await;
        assert!(violations.is_empty(), "{what} [{scheme}]: {violations:#?}");
        press(page, "Escape").await;
    }
}

/// Check the gallery index and every story page in `scheme`.
async fn axe_sweep(scheme: &str) {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    let mut failures = Vec::new();
    let mut paths = vec![stories::STORIES_PATH.to_owned()];
    paths.extend(
        slugs()
            .iter()
            .map(|s| format!("{}/{s}", stories::STORIES_PATH)),
    );

    for path in &paths {
        let errors_before = page.console_errors().len();
        open(&page, path).await;
        if path != stories::STORIES_PATH {
            assert!(
                eval_bool(
                    &page,
                    "(() => { const p = document.querySelector('.story-preview'); \
                     return !!p && p.children.length > 0; })()"
                )
                .await,
                "{path}: the story preview is empty"
            );
        }
        select_scheme(&page, scheme).await;
        for violation in axe_violations(&page).await {
            failures.push(format!("{path} [{scheme}] {violation}"));
        }
        for error in page.console_errors().iter().skip(errors_before) {
            failures.push(format!("{path} [{scheme}] console error: {error}"));
        }
    }

    // A late console error from the last page arrives after the loop.
    let errors_seen = page.console_errors().len();
    tokio::time::sleep(Duration::from_millis(500)).await;
    for error in page.console_errors().iter().skip(errors_seen) {
        failures.push(format!("[{scheme}] late console error: {error}"));
    }

    assert!(
        failures.is_empty(),
        "{} problem(s):\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn axe_finds_no_serious_violations_in_the_light_scheme() {
    axe_sweep("light").await;
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn axe_finds_no_serious_violations_in_the_dark_scheme() {
    axe_sweep("dark").await;
}

/// Canary: the axe check must fail on a page with a known violation.
#[tokio::test]
#[ignore = "requires Chromium"]
async fn axe_check_fails_on_a_seeded_violation() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    page.visit("/seeded/axe").await.expect("visit");
    wait_for(&page, "document.readyState === 'complete'").await;
    let violations = axe_violations(&page).await;
    for rule in ["label ", "image-alt "] {
        assert!(
            violations.iter().any(|v| v.starts_with(rule)),
            "axe did not report `{rule}`: {violations:?}"
        );
    }
}

// ── Keyboard: one test per interactive story ────────────────────────────

/// Tab ids of the Tabs story, and the panel each one controls.
const STORY_TABS: [(&str, &str); 3] = [
    ("settings-tabs-tab-demo-profile", "#demo-profile"),
    ("settings-tabs-tab-demo-security", "#demo-security"),
    ("settings-tabs-tab-demo-billing", "#demo-billing"),
];

/// Press `key` and check that tab `want` has focus, is selected and shows
/// its panel.
async fn tab_step(page: &Page, key: &str, want: usize) -> Result<(), String> {
    let (tab, _) = STORY_TABS[want];
    page.press(key).await.map_err(|e| e.to_string())?;
    let focused = active_id(page).await;
    let selected = eval_bool(
        page,
        &format!("document.getElementById('{tab}').getAttribute('aria-selected') === 'true'"),
    )
    .await;
    if focused != tab || !selected {
        return Err(format!(
            "{key}: want {tab} focused and selected, got `{focused}`"
        ));
    }
    for (i, (other, other_panel)) in STORY_TABS.iter().enumerate() {
        let want_index = if i == want { "0" } else { "-1" };
        let index = attr(page, &format!("#{other}"), "tabindex").await;
        if index != want_index {
            return Err(format!(
                "{key}: {other} has tabindex `{index}`, want `{want_index}`"
            ));
        }
        if shown(page, other_panel).await != (i == want) {
            return Err(format!(
                "{key}: panel {other_panel} shown is not {}",
                i == want
            ));
        }
    }
    Ok(())
}

/// Check the APG tabs model on the Tabs story markup. Return an error
/// instead of a panic, so the canary can expect a failure.
async fn check_tabs_keyboard(page: &Page) -> Result<(), String> {
    let first = STORY_TABS[0].0;
    let focused = eval_bool(
        page,
        &format!(
            "(() => {{ const e = document.getElementById('{first}'); \
             if (!e) return false; e.focus(); return document.activeElement === e; }})()"
        ),
    )
    .await;
    if !focused {
        return Err("could not focus the first tab".into());
    }
    tab_step(page, "ArrowRight", 1).await?;
    tab_step(page, "ArrowRight", 2).await?;
    tab_step(page, "ArrowRight", 0).await?;
    tab_step(page, "ArrowLeft", 2).await?;
    tab_step(page, "Home", 0).await?;
    tab_step(page, "End", 2).await
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn keyboard_tabs() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/_stories/tabs").await;
    check_tabs_keyboard(&page)
        .await
        .expect("tabs keyboard model");
    page.expect_no_console_errors().await.expect("no errors");
}

/// Canary: the tabs check must fail when the runtime is missing.
#[tokio::test]
#[ignore = "requires Chromium"]
async fn keyboard_check_fails_without_the_widget_runtime() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    page.visit("/seeded/tabs").await.expect("visit");
    wait_for(&page, "document.readyState === 'complete'").await;
    // The first arrow key must fail, not the page load or the focus.
    let result = check_tabs_keyboard(&page).await;
    assert!(
        result.as_ref().is_err_and(|e| e.starts_with("ArrowRight:")),
        "want an ArrowRight failure with no widget runtime, got {result:?}"
    );
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn keyboard_nav_bar() {
    const TOGGLE: &str = "#autumn-nav-menu-1-toggle";
    const MENU: &str = "#autumn-nav-menu-1";
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/_stories/nav-bar").await;

    focus(&page, TOGGLE).await;
    assert_eq!(attr(&page, TOGGLE, "aria-expanded").await, "false");
    press(&page, "Enter").await;
    assert_eq!(attr(&page, TOGGLE, "aria-expanded").await, "true");
    assert!(shown(&page, MENU).await, "Enter opens the menu");
    assert_eq!(
        active_text(&page).await,
        "About",
        "Enter focuses the first item"
    );
    press(&page, "ArrowDown").await;
    assert_eq!(active_text(&page).await, "Docs");
    press(&page, "ArrowDown").await;
    assert_eq!(active_text(&page).await, "About", "ArrowDown wraps");
    press(&page, "Escape").await;
    assert!(!shown(&page, MENU).await, "Escape closes the menu");
    assert_eq!(
        active_id(&page).await,
        "autumn-nav-menu-1-toggle",
        "Escape returns focus to the toggle"
    );

    axe_in_both_schemes(&page, "open nav menu", || async {
        focus(&page, TOGGLE).await;
        press(&page, "Enter").await;
        wait_for(
            &page,
            &format!("document.querySelector('{MENU}').matches(':popover-open')"),
        )
        .await;
    })
    .await;
    page.expect_no_console_errors().await.expect("no errors");
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn keyboard_popover() {
    const TRIGGER: &str = "#help-popover-trigger";
    const PANEL: &str = "#help-popover";
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/_stories/popover").await;

    focus(&page, TRIGGER).await;
    assert!(!shown(&page, PANEL).await, "closed at first");
    press(&page, "Enter").await;
    assert!(shown(&page, PANEL).await, "Enter opens the popover");
    press(&page, "Escape").await;
    assert!(!shown(&page, PANEL).await, "Escape closes the popover");
    assert_eq!(
        active_id(&page).await,
        "help-popover-trigger",
        "Escape returns focus to the trigger"
    );

    axe_in_both_schemes(&page, "open popover", || async {
        focus(&page, TRIGGER).await;
        press(&page, "Enter").await;
        wait_for(
            &page,
            &format!("document.querySelector('{PANEL}').matches(':popover-open')"),
        )
        .await;
    })
    .await;
    page.expect_no_console_errors().await.expect("no errors");
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn keyboard_dropdown_menu() {
    const TRIGGER: &str = "#post-actions-trigger";
    const MENU: &str = "#post-actions";
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/_stories/dropdown-menu").await;

    focus(&page, TRIGGER).await;
    assert_eq!(attr(&page, TRIGGER, "aria-expanded").await, "false");
    press(&page, "Enter").await;
    assert_eq!(attr(&page, TRIGGER, "aria-expanded").await, "true");
    assert_eq!(
        active_text(&page).await,
        "Edit",
        "Enter focuses the first item"
    );
    press(&page, "ArrowDown").await;
    assert_eq!(active_text(&page).await, "Copy link");
    press(&page, "End").await;
    assert_eq!(active_text(&page).await, "Delete");
    press(&page, "ArrowDown").await;
    assert_eq!(active_text(&page).await, "Edit", "ArrowDown wraps");
    press(&page, "Escape").await;
    assert!(!shown(&page, MENU).await, "Escape closes the menu");
    assert_eq!(active_id(&page).await, "post-actions-trigger");

    axe_in_both_schemes(&page, "open dropdown menu", || async {
        focus(&page, TRIGGER).await;
        press(&page, "Enter").await;
        wait_for(
            &page,
            &format!("document.querySelector('{MENU}').matches(':popover-open')"),
        )
        .await;
    })
    .await;
    page.expect_no_console_errors().await.expect("no errors");
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn keyboard_autocomplete() {
    const INPUT: &str = "#tag-picker-query";
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/_stories/autocomplete").await;

    page.fill(INPUT, "ru").await.expect("type a query");
    wait_for(
        &page,
        "document.querySelectorAll('#tag-picker-options [role=option]').length > 0",
    )
    .await;
    focus(&page, INPUT).await;
    press(&page, "ArrowDown").await;
    assert_eq!(attr(&page, INPUT, "aria-expanded").await, "true");
    let active = attr(&page, INPUT, "aria-activedescendant").await;
    assert!(!active.is_empty(), "ArrowDown sets aria-activedescendant");
    assert_eq!(
        active_id(&page).await,
        "tag-picker-query",
        "focus stays on the input"
    );
    press(&page, "Enter").await;
    assert_eq!(
        eval_string(&page, "document.getElementById('tag-picker-value').value").await,
        "42",
        "Enter selects the option"
    );
    assert_eq!(
        eval_string(&page, "document.querySelector('#tag-picker-query').value").await,
        "rust"
    );

    page.fill(INPUT, "w").await.expect("type again");
    wait_for(
        &page,
        "document.querySelector('#tag-picker-query').getAttribute('aria-expanded') === 'true'",
    )
    .await;
    press(&page, "Escape").await;
    assert_eq!(
        attr(&page, INPUT, "aria-expanded").await,
        "false",
        "Escape closes"
    );

    axe_in_both_schemes(&page, "open autocomplete popup", || async {
        focus(&page, INPUT).await;
        press(&page, "ArrowDown").await;
        wait_for(
            &page,
            "document.querySelector('#tag-picker-query').getAttribute('aria-expanded') === 'true'",
        )
        .await;
    })
    .await;
    page.expect_no_console_errors().await.expect("no errors");
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn keyboard_tags_input() {
    const INPUT: &str = "#tag-list-query";
    const VALUES: &str = "[...document.querySelectorAll('#tag-list-chips [data-ac-chip] \
                          input[type=hidden]')].filter(i => !i.disabled).map(i => i.value).join()";
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/_stories/tags-input").await;
    assert_eq!(
        eval_string(&page, VALUES).await,
        "rust",
        "the story pre-fills a chip"
    );

    // Pick an option with the keyboard.
    page.fill(INPUT, "a").await.expect("type a query");
    wait_for(
        &page,
        "document.querySelector('#tag-list-query').getAttribute('aria-expanded') === 'true'",
    )
    .await;
    focus(&page, INPUT).await;
    press(&page, "ArrowDown").await;
    press(&page, "Enter").await;
    assert_eq!(eval_string(&page, VALUES).await, "rust,axum");
    assert_eq!(active_id(&page).await, "tag-list-query");

    // The "create" entry adds a new tag.
    page.fill(INPUT, "zig").await.expect("type a new tag");
    wait_for(
        &page,
        "document.querySelector('#tag-list-options [data-ac-create]') !== null",
    )
    .await;
    focus(&page, INPUT).await;
    press(&page, "ArrowDown").await;
    press(&page, "Enter").await;
    assert_eq!(eval_string(&page, VALUES).await, "rust,axum,zig");

    // Backspace removes the last chip. ArrowLeft goes to the chips.
    press(&page, "Backspace").await;
    assert_eq!(eval_string(&page, VALUES).await, "rust,axum");
    press(&page, "ArrowLeft").await;
    assert_eq!(
        eval_string(
            &page,
            "document.activeElement.getAttribute('aria-label') || ''"
        )
        .await,
        "Remove axum"
    );
    press(&page, "Delete").await;
    assert_eq!(eval_string(&page, VALUES).await, "rust");

    // Pointer removal.
    page.click("#tag-list-chips [data-ac-chip-remove]")
        .await
        .expect("click remove");
    assert_eq!(eval_string(&page, VALUES).await, "");
    assert_eq!(active_id(&page).await, "tag-list-query");

    // A chip and a popup with options for the axe check.
    page.fill(INPUT, "web").await.expect("type");
    wait_for(
        &page,
        "document.querySelector('#tag-list-query').getAttribute('aria-expanded') === 'true'",
    )
    .await;
    focus(&page, INPUT).await;
    press(&page, "ArrowDown").await;
    press(&page, "Enter").await;
    page.fill(INPUT, "r").await.expect("type");
    wait_for(
        &page,
        "document.querySelectorAll('#tag-list-options [role=option]').length > 0",
    )
    .await;
    axe_in_both_schemes(&page, "tags input with chips and an open popup", || async {
        focus(&page, INPUT).await;
        press(&page, "ArrowDown").await;
        wait_for(
            &page,
            "document.querySelector('#tag-list-query').getAttribute('aria-expanded') === 'true'",
        )
        .await;
    })
    .await;
    assert_eq!(eval_string(&page, VALUES).await, "web");
    page.expect_no_console_errors().await.expect("no errors");
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn keyboard_active_search() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/_stories/active-search").await;

    page.fill("#post-search", "leaves")
        .await
        .expect("type a query");
    wait_for(
        &page,
        "document.querySelector('#post-search-results').textContent.includes('Falling Leaves')",
    )
    .await;
    assert!(
        !eval_bool(
            &page,
            "document.querySelector('#post-search-results').textContent.includes('Shipping Widgets')"
        )
        .await,
        "the results are filtered"
    );
    page.expect_no_console_errors().await.expect("no errors");
}

/// Open the dialog `id` with the keyboard, check it with axe, close it
/// with Escape and check that focus goes back to the trigger.
async fn check_dialog_keyboard(path: &str, id: &str) {
    let trigger = format!("[data-modal-open='{id}']");
    let dialog = format!("#{id}");
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, path).await;

    focus(&page, &trigger).await;
    press(&page, "Enter").await;
    wait_for(&page, &format!("document.querySelector('{dialog}').open")).await;
    assert!(
        eval_bool(
            &page,
            &format!("document.querySelector('{dialog}').contains(document.activeElement)")
        )
        .await,
        "focus moves into the dialog"
    );

    press(&page, "Escape").await;
    wait_for(&page, &format!("!document.querySelector('{dialog}').open")).await;
    assert!(
        eval_bool(
            &page,
            &format!("document.activeElement === document.querySelector(\"{trigger}\")")
        )
        .await,
        "Escape returns focus to the trigger"
    );

    axe_in_both_schemes(&page, "open dialog", || async {
        focus(&page, &trigger).await;
        press(&page, "Enter").await;
        wait_for(&page, &format!("document.querySelector('{dialog}').open")).await;
    })
    .await;
    page.expect_no_console_errors().await.expect("no errors");
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn keyboard_modal() {
    check_dialog_keyboard("/_stories/modal", "delete-confirm").await;
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn keyboard_drawer() {
    check_dialog_keyboard("/_stories/drawer", "filters").await;
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn keyboard_confirm_action() {
    check_dialog_keyboard("/_stories/confirm-action", "delete-post").await;
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn keyboard_theme_toggle() {
    const DARK: &str = "form[data-autumn-theme-toggle] button[value='dark']";
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/_stories/theme-toggle").await;

    assert_eq!(attr(&page, DARK, "aria-pressed").await, "false");
    focus(&page, DARK).await;
    press(&page, "Enter").await;
    wait_for(
        &page,
        "document.documentElement.getAttribute('data-theme') === 'dark'",
    )
    .await;
    assert_eq!(attr(&page, DARK, "aria-pressed").await, "true");
    assert_eq!(
        eval_string(&page, "location.pathname").await,
        "/_stories/theme-toggle",
        "the runtime applies the choice in place"
    );
    page.expect_no_console_errors().await.expect("no errors");
}

#[cfg(feature = "flash")]
#[tokio::test]
#[ignore = "requires Chromium"]
async fn keyboard_flash() {
    const TOGGLE: &str = ".autumn-flash__dismiss-toggle";
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/_stories/flash").await;

    let before = eval_string(
        &page,
        "String(document.querySelectorAll('.autumn-flash').length)",
    )
    .await;
    assert_ne!(before, "0", "the story shows banners");
    focus(&page, TOGGLE).await;
    press(&page, " ").await;
    assert!(
        eval_bool(
            &page,
            &format!(
                "(() => {{ const t = document.querySelector('{TOGGLE}'); \
                 return t.checked && getComputedStyle(t.closest('.autumn-flash')).display === 'none'; }})()"
            ),
        )
        .await,
        "Space on the dismiss control hides the banner"
    );
    page.expect_no_console_errors().await.expect("no errors");
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn keyboard_accessible_primitives() {
    const MENU: &str = "ul[role='menu']";
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/_stories/accessible-primitives").await;

    // Roving tabindex: one Tab stop for the whole menu.
    assert_eq!(
        eval_string(
            &page,
            &format!(
                "[...document.querySelectorAll(\"{MENU} [role=menuitem]\")] \
                 .map(e => e.getAttribute('tabindex')).join(',')"
            ),
        )
        .await,
        "0,-1"
    );
    focus(&page, &format!("{MENU} [role=menuitem]")).await;
    assert_eq!(active_text(&page).await, "Archive");
    press(&page, "ArrowDown").await;
    assert_eq!(active_text(&page).await, "View");
    press(&page, "ArrowDown").await;
    assert_eq!(active_text(&page).await, "Archive", "ArrowDown wraps");
    press(&page, "End").await;
    assert_eq!(active_text(&page).await, "View");
    press(&page, "Home").await;
    assert_eq!(active_text(&page).await, "Archive");
    press(&page, "v").await;
    assert_eq!(active_text(&page).await, "View", "typeahead");
    page.expect_no_console_errors().await.expect("no errors");
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn keyboard_accordion() {
    const ITEM: &str = "#demo-faq > details:nth-child";
    const MULTI: &str = "#demo-faq-multi > details:nth-child";
    let is_open = |sel: String| format!("document.querySelector({sel:?}).open");
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/_stories/accordion").await;

    assert!(eval_bool(&page, &is_open(format!("{ITEM}(1)"))).await);
    focus(&page, &format!("{ITEM}(2) > summary")).await;
    press(&page, "Enter").await;
    assert!(
        eval_bool(&page, &is_open(format!("{ITEM}(2)"))).await,
        "Enter opens"
    );
    assert!(
        !eval_bool(&page, &is_open(format!("{ITEM}(1)"))).await,
        "an exclusive accordion closes the other item"
    );
    press(&page, " ").await;
    assert!(
        !eval_bool(&page, &is_open(format!("{ITEM}(2)"))).await,
        "Space closes"
    );

    // Multi mode: items 1 and 2 stay open when item 3 opens.
    focus(&page, &format!("{MULTI}(3) > summary")).await;
    press(&page, "Enter").await;
    for n in 1..=3 {
        assert!(
            eval_bool(&page, &is_open(format!("{MULTI}({n})"))).await,
            "multi item {n} is open"
        );
    }
    page.expect_no_console_errors().await.expect("no errors");
}

/// Native choice controls (#3405): no script, only the browser keys.
#[tokio::test]
#[ignore = "requires Chromium"]
async fn keyboard_choice_controls() {
    let checked = |sel: &str| format!("document.querySelector({sel:?}).checked");
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/_stories/choice-controls").await;

    // Space toggles the switch.
    assert!(
        eval_bool(&page, &checked("#alerts")).await,
        "the switch starts on"
    );
    focus(&page, "#alerts").await;
    press(&page, " ").await;
    settle().await;
    assert!(
        !eval_bool(&page, &checked("#alerts")).await,
        "Space turns the switch off"
    );

    // The arrow keys move the radio selection.
    focus(&page, "#plan-1").await;
    press(&page, "ArrowDown").await;
    assert_eq!(active_id(&page).await, "plan-2");
    assert!(
        eval_bool(&page, &checked("#plan-2")).await,
        "ArrowDown selects the next radio"
    );

    // A segment radio is visually hidden, but it takes focus and the arrow
    // keys. Its label shows the focus ring.
    focus(&page, "#view-1").await;
    press(&page, "ArrowRight").await;
    assert_eq!(active_id(&page).await, "view-2");
    assert!(
        eval_bool(&page, &checked("#view-2")).await,
        "ArrowRight selects the next segment"
    );
    assert!(
        eval_bool(
            &page,
            "document.querySelector('#view-2').matches(':focus-visible')"
        )
        .await,
        "a keyboard move makes the segment radio :focus-visible"
    );
    assert_eq!(
        eval_string(
            &page,
            "getComputedStyle(document.querySelector('label[for=view-2]')).outlineStyle",
        )
        .await,
        "solid",
        "the focused segment shows a focus ring"
    );

    // Space checks a box of the checkbox group.
    focus(&page, "#topics-1").await;
    press(&page, " ").await;
    settle().await;
    assert!(
        eval_bool(&page, &checked("#topics-1")).await,
        "Space checks a box"
    );
    page.expect_no_console_errors().await.expect("no errors");
}

/// Password and one-time code (#3412): a paste fills the one OTP input,
/// and the reveal button toggles the password from the keyboard.
#[allow(clippy::too_many_lines, reason = "one flow over the story page")]
#[tokio::test]
#[ignore = "requires Chromium"]
async fn keyboard_sign_in_inputs() {
    const REVEAL: &str = "[data-autumn-reveal][aria-controls=current]";
    let value = |sel: &str| format!("document.querySelector({sel:?}).value");
    let prop = |sel: &str, name: &str| format!("String(document.querySelector({sel:?}).{name})");
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/_stories/sign-in-inputs").await;

    // The runtime removes the no-JS `maxlength`, so a paste is not cut.
    assert_eq!(attr(&page, "#code", "maxlength").await, "");
    // A paste fills the one input. The runtime keeps only the digits. A
    // listener on the input (an htmx trigger) already reads the digits.
    assert!(
        eval_bool(
            &page,
            "(() => { const i = document.querySelector('#code'); \
             i.addEventListener('input', () => { window.__seen = i.value; }); return true; })()",
        )
        .await
    );
    focus(&page, "#code").await;
    page.paste("123-456").await.expect("paste");
    assert_eq!(eval_string(&page, &value("#code")).await, "123456");
    assert_eq!(eval_string(&page, "String(window.__seen)").await, "123456");
    assert!(
        eval_bool(&page, "document.querySelector('#code').validity.valid").await,
        "the pasted code matches the pattern"
    );
    // A longer paste is cut to the length.
    page.fill("#code", "").await.expect("clear");
    page.paste("Your code is 987654 (valid 5 min)")
        .await
        .expect("paste");
    assert_eq!(eval_string(&page, &value("#code")).await, "987654");
    // Full-width digits become ASCII digits.
    page.fill("#code", "").await.expect("clear");
    page.paste("１２３４５６").await.expect("paste");
    assert_eq!(eval_string(&page, &value("#code")).await, "123456");
    // An IME entry is formatted once, when the composition ends.
    assert_eq!(
        eval_string(
            &page,
            "(() => { const i = document.querySelector('#code'); i.value = '12a'; \
             i.dispatchEvent(new InputEvent('input', { bubbles: true, isComposing: true })); \
             const during = i.value; \
             i.dispatchEvent(new CompositionEvent('compositionend', { bubbles: true })); \
             return during + '|' + i.value; })()",
        )
        .await,
        "12a|12"
    );

    // A grouped code shows a space between groups and the separator cell.
    focus(&page, "#backup").await;
    page.paste("1234 5678").await.expect("paste");
    assert_eq!(eval_string(&page, &value("#backup")).await, "1234 5678");
    assert_eq!(
        eval_string(&page, &prop("#backup", "pattern")).await,
        "[0-9]{4} [0-9]{4}"
    );
    assert!(
        eval_bool(&page, "document.querySelector('#backup').validity.valid").await,
        "the grouped code matches the grouped pattern"
    );
    assert!(
        shown(&page, "#backup-field .autumn-otp__sep").await,
        "the runtime shows the separator"
    );
    // The form data has the digits only.
    assert_eq!(
        eval_string(
            &page,
            "(() => { const f = document.createElement('form'); f.id = 'probe'; \
             document.body.append(f); const i = document.querySelector('#backup'); \
             i.setAttribute('form', 'probe'); const v = new FormData(f).get('backup'); \
             i.removeAttribute('form'); f.remove(); return v; })()",
        )
        .await,
        "12345678"
    );
    // An app pattern wins: the runtime does not group that code.
    assert_eq!(
        eval_string(
            &page,
            "(() => { const d = document.createElement('div'); \
             d.innerHTML = '<input id=\"own\" data-autumn-otp=\"6\" data-autumn-otp-group=\"3\" pattern=\"[1-9][0-9]{5}\">'; \
             document.body.append(d); \
             d.dispatchEvent(new CustomEvent('htmx:afterSwap', { bubbles: true })); \
             const i = d.querySelector('input'); i.value = '123456'; \
             i.dispatchEvent(new InputEvent('input', { bubbles: true })); \
             const out = i.pattern + '|' + i.value + '|' + i.hasAttribute('data-autumn-otp-grouped'); \
             d.remove(); return out; })()",
        )
        .await,
        "[1-9][0-9]{5}|123456|false"
    );
    // Backspace after a group space deletes the digit before the space.
    assert!(
        eval_bool(
            &page,
            "(() => { const i = document.querySelector('#backup'); i.focus(); \
             i.setSelectionRange(5, 5); return document.activeElement === i; })()",
        )
        .await
    );
    press(&page, "Backspace").await;
    assert_eq!(eval_string(&page, &value("#backup")).await, "1235 678");

    // The runtime shows the reveal button. It toggles the input type.
    wait_for(
        &page,
        &format!("!document.querySelector({REVEAL:?}).hidden"),
    )
    .await;
    assert_eq!(
        eval_string(&page, &prop("#current", "type")).await,
        "password"
    );
    focus(&page, REVEAL).await;
    press(&page, "Enter").await;
    assert_eq!(eval_string(&page, &prop("#current", "type")).await, "text");
    assert_eq!(attr(&page, REVEAL, "aria-pressed").await, "true");
    assert_eq!(
        active_text(&page).await,
        "Show password",
        "focus stays on the button and its label does not change"
    );
    press(&page, " ").await;
    assert_eq!(
        eval_string(&page, &prop("#current", "type")).await,
        "password"
    );
    assert_eq!(attr(&page, REVEAL, "aria-pressed").await, "false");

    // A real submit of the form with the password, a reset, a page hide and
    // an htmx history snapshot hide the password again.
    let in_form = |run: &str| {
        format!(
            "(() => {{ const field = document.querySelector('#current-field'); \
             const f = document.createElement('form'); \
             f.addEventListener('submit', e => e.preventDefault()); \
             field.before(f); f.append(field); f.{run}; \
             f.before(field); f.remove(); }})()"
        )
    };
    for event in [
        in_form("requestSubmit()"),
        in_form("reset()"),
        "window.dispatchEvent(new Event('pagehide'))".to_owned(),
        "document.dispatchEvent(new Event('htmx:beforeHistorySave'))".to_owned(),
    ] {
        // A moved field loses focus, so focus the button again.
        focus(&page, REVEAL).await;
        press(&page, "Enter").await;
        assert_eq!(eval_string(&page, &prop("#current", "type")).await, "text");
        page.evaluate(event.as_str()).await.expect("dispatch");
        assert_eq!(
            eval_string(&page, &prop("#current", "type")).await,
            "password",
            "after `{event}`"
        );
        assert_eq!(attr(&page, REVEAL, "aria-pressed").await, "false");
    }

    // The strength hint follows the typed password.
    assert!(
        attr(&page, "#fresh", "aria-describedby")
            .await
            .contains("fresh-strength")
    );
    page.fill("#fresh", "abc").await.expect("fill");
    assert_eq!(
        eval_string(
            &page,
            "document.querySelector('#fresh-strength').textContent"
        )
        .await,
        "Strength: weak"
    );
    // One kind of character does not score Strong.
    page.fill("#fresh", "aaaaaaaaaaaaaaaa").await.expect("fill");
    assert_eq!(
        eval_string(
            &page,
            "document.querySelector('#fresh-strength').textContent"
        )
        .await,
        "Strength: fair"
    );
    page.fill("#fresh", "Correct-Horse-9").await.expect("fill");
    assert_eq!(
        eval_string(
            &page,
            "document.querySelector('#fresh-strength').textContent"
        )
        .await,
        "Strength: strong"
    );
    assert_eq!(
        attr(&page, "#fresh-strength", "data-autumn-strength-level").await,
        "3"
    );
    page.expect_no_console_errors().await.expect("no errors");

    // Without the script, the reveal button stays hidden.
    let plain = runner.page().await.expect("page");
    plain.set_javascript_enabled(false).await.expect("no JS");
    plain
        .visit("/_stories/sign-in-inputs")
        .await
        .expect("visit");
    wait_for(&plain, "document.readyState === 'complete'").await;
    assert!(
        eval_bool(
            &plain,
            &format!("document.querySelector({REVEAL:?}).hidden")
        )
        .await,
        "the reveal button is hidden without JS"
    );
}

/// Select (#3409): the native `<select>`, with no script. Only the
/// opted-in single select gets `appearance: base-select`.
#[tokio::test]
#[ignore = "requires Chromium"]
async fn keyboard_select() {
    const OPEN: &str = "document.querySelector('#plan').matches(':open')";
    let appearance =
        |sel: &str| format!("getComputedStyle(document.querySelector({sel:?})).appearance");
    let value = |sel: &str| format!("document.querySelector({sel:?}).value");
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/_stories/select").await;

    // The fallback and the multiple select keep the native control.
    for sel in ["#plan_native", "#tags"] {
        assert_ne!(
            eval_string(&page, &appearance(sel)).await,
            "base-select",
            "{sel} keeps the native control"
        );
    }
    let supported = eval_bool(&page, "CSS.supports('appearance', 'base-select')").await;
    if !supported {
        page.expect_no_console_errors().await.expect("no errors");
        return;
    }
    for sel in ["#plan", "#city"] {
        assert_eq!(
            eval_string(&page, &appearance(sel)).await,
            "base-select",
            "{sel} is styled"
        );
    }

    // Enter opens the picker on Linux and Windows. The browser gate runs
    // on Linux. The open picker passes axe in both schemes.
    axe_in_both_schemes(&page, "open select picker", || async {
        focus(&page, "#plan").await;
        press(&page, "Enter").await;
        wait_for(&page, OPEN).await;
    })
    .await;

    // Escape closes the picker and keeps the value.
    focus(&page, "#plan").await;
    press(&page, "Enter").await;
    wait_for(&page, OPEN).await;
    press(&page, "Escape").await;
    wait_for(&page, &format!("!{OPEN}")).await;
    assert_eq!(eval_string(&page, &value("#plan")).await, "pro");
    assert_eq!(
        active_id(&page).await,
        "plan",
        "focus goes back to the select"
    );

    // ArrowDown on the closed select opens the picker. The value stays.
    press(&page, "ArrowDown").await;
    wait_for(&page, OPEN).await;
    assert_eq!(eval_string(&page, &value("#plan")).await, "pro");
    press(&page, "Escape").await;
    wait_for(&page, &format!("!{OPEN}")).await;

    // In the picker, ArrowDown moves to the next option. Enter chooses it.
    press(&page, "Enter").await;
    wait_for(&page, OPEN).await;
    press(&page, "ArrowDown").await;
    press(&page, "Enter").await;
    wait_for(&page, &format!("!{OPEN}")).await;
    assert_eq!(
        eval_string(&page, &value("#plan")).await,
        "team",
        "the keyboard chooses the next option"
    );
    page.expect_no_console_errors().await.expect("no errors");
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn keyboard_toast() {
    const CLOSE: &str = ".autumn-toast--persistent [data-autumn-toast-close]";
    const LEFT: &str = "String(document.querySelectorAll('.autumn-toast--persistent').length)";
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/_stories/toast").await;

    assert_eq!(eval_string(&page, LEFT).await, "2", "two persistent toasts");
    wait_for(&page, &format!("!document.querySelector({CLOSE:?}).hidden")).await;
    assert_eq!(
        attr(&page, CLOSE, "aria-label").await,
        "Dismiss notification"
    );
    focus(&page, CLOSE).await;
    press(&page, "Enter").await;
    assert_eq!(
        eval_string(&page, LEFT).await,
        "1",
        "Enter closes the toast"
    );
    assert!(
        eval_bool(
            &page,
            "document.activeElement.matches('.autumn-toast--persistent [data-autumn-toast-close]')"
        )
        .await,
        "focus moves to the next toast"
    );
    press(&page, " ").await;
    assert_eq!(
        eval_string(&page, LEFT).await,
        "0",
        "Space closes the toast"
    );
    // No toast after it: focus goes to the toast before.
    assert_eq!(
        eval_string(
            &page,
            "(() => { const t = document.activeElement.closest('[data-autumn-toast]'); \
             return t ? t.querySelector('.autumn-toast__message').textContent : ''; })()"
        )
        .await,
        "Report export started"
    );
    page.expect_no_console_errors().await.expect("no errors");
}

/// Focus the tab stop before `selector`, then press Tab: a keyboard focus.
async fn tab_to(page: &Page, selector: &str) {
    assert!(
        eval_bool(
            page,
            &format!(
                "(() => {{ const t = document.querySelector({selector:?}); \
                 const all = [...document.querySelectorAll('a[href],button,input,select,textarea,[tabindex]')] \
                   .filter(e => e.tabIndex >= 0 && !e.disabled && e.getClientRects().length); \
                 const i = all.indexOf(t); if (i < 1) return false; \
                 all[i - 1].focus(); return true; }})()"
            ),
        )
        .await,
        "no tab stop before {selector}"
    );
    press(page, "Tab").await;
    assert!(
        eval_bool(
            page,
            &format!("document.activeElement.matches({selector:?})")
        )
        .await,
        "Tab does not reach {selector}"
    );
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn keyboard_tooltip() {
    const TRIGGER: &str = "[data-autumn-tooltip] button[aria-describedby]";
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/_stories/tooltip").await;

    let tip = attr(&page, TRIGGER, "aria-describedby").await;
    let bubble = format!("#{tip}");
    let visible = format!(
        "(() => {{ const s = getComputedStyle(document.getElementById({tip:?})); \
         return s.display !== 'none' && s.visibility === 'visible'; }})()"
    );
    assert_eq!(attr(&page, &bubble, "role").await, "tooltip");
    assert!(!eval_bool(&page, &visible).await, "hidden at rest");
    tab_to(&page, TRIGGER).await;
    wait_for(&page, &visible).await;
    press(&page, "Escape").await;
    wait_for(&page, &format!("!{visible}")).await;
    assert!(
        eval_bool(
            &page,
            &format!("document.activeElement.matches({TRIGGER:?})")
        )
        .await,
        "focus stays on the trigger"
    );

    // A new focus shows the text again. axe checks it in both schemes.
    axe_in_both_schemes(&page, "shown tooltip", || async {
        tab_to(&page, TRIGGER).await;
        wait_for(&page, &visible).await;
        wait_for(&page, "document.getAnimations().length === 0").await;
    })
    .await;
    page.expect_no_console_errors().await.expect("no errors");
}

/// The file inputs are in the Tab order, and the script adds no trap. A
/// visible label names each input. The hint shows because the script ran.
#[cfg(feature = "storage")]
#[tokio::test]
#[ignore = "requires Chromium"]
async fn keyboard_direct_upload() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/_stories/direct-upload").await;
    wait_for(
        &page,
        "document.querySelector('[data-autumn-upload-hint]') && \
         !document.querySelector('[data-autumn-upload-hint]').hidden",
    )
    .await;
    focus(&page, "#upload-cover_image").await;
    assert!(
        eval_bool(
            &page,
            "document.activeElement.labels[0].textContent === 'Cover image'"
        )
        .await,
        "a visible label names the input"
    );
    // Tab leaves the first field and reaches the second.
    press(&page, "Tab").await;
    assert!(
        eval_bool(&page, "document.activeElement.id === 'upload-attachments'").await,
        "Tab reaches the next upload field"
    );
    page.expect_no_console_errors().await.expect("no errors");
}

/// The busy state (#3406). Enter sends the htmx request. While it runs, the
/// button has `aria-busy="true"` and shows its spinner. The label is
/// transparent, not removed, so it stays the name. After the response,
/// `aria-busy` goes and focus stays on the button.
#[tokio::test]
#[ignore = "requires Chromium"]
async fn keyboard_button() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/_stories/button").await;
    // The button sends the request, then it submits the form that sends it.
    for (button, status) in [
        ("story-save", "story-save-status"),
        ("story-publish", "story-publish-status"),
    ] {
        check_busy_button(&page, button, status).await;
    }
    page.expect_no_console_errors().await.expect("no errors");
}

/// Press Enter on `#button` and check the busy state while the request
/// runs: the button and the swap target have `aria-busy="true"`, the
/// spinner shows, and the label is transparent but keeps its text. After
/// the response, `aria-busy` goes and focus stays on the button.
///
/// `Page::press` waits for htmx to settle, so the test cannot read the page
/// during the request. A `MutationObserver` in the page records the busy
/// state when it happens.
async fn check_busy_button(page: &Page, button: &str, status: &str) {
    let state = |busy: bool| {
        format!(
            "(() => {{ const b = document.getElementById({button:?}); \
             const s = document.getElementById({status:?}); \
             const spin = getComputedStyle(b.querySelector('.autumn-button__spinner')).display; \
             const label = b.querySelector('.autumn-button__label'); \
             return {busy} \
               ? b.getAttribute('aria-busy') === 'true' && s.getAttribute('aria-busy') === 'true' \
                 && spin === 'block' && getComputedStyle(label).opacity === '0' \
                 && label.textContent === b.id.replace('story-', '').replace(/^./, c => c.toUpperCase()) \
               : !b.hasAttribute('aria-busy') && !s.hasAttribute('aria-busy') && spin === 'none'; }})()"
        )
    };
    assert!(
        eval_bool(page, &state(false)).await,
        "#{button} is busy at rest"
    );
    assert!(
        eval_bool(
            page,
            &format!(
                "(() => {{ window.__busySeen = false; \
                 const check = () => {{ if ({}) window.__busySeen = true; }}; \
                 new MutationObserver(check).observe(document.body, \
                   {{ attributes: true, subtree: true, attributeFilter: ['aria-busy', 'class'] }}); \
                 return true; }})()",
                state(true)
            ),
        )
        .await,
        "could not watch #{button}"
    );
    focus(page, &format!("#{button}")).await;
    press(page, "Enter").await;
    wait_for(
        page,
        &format!("document.getElementById({status:?}).textContent.includes('Saved.')"),
    )
    .await;
    assert!(
        eval_bool(page, "window.__busySeen === true").await,
        "#{button} never showed the busy state during the request"
    );
    wait_for(page, &state(false)).await;
    assert_eq!(active_id(page).await, button, "focus left #{button}");
}

/// An `hx-indicator` skeleton or spinner (#3406) is hidden at rest, shows
/// while the request runs, and hides again after the response.
#[tokio::test]
#[ignore = "requires Chromium"]
async fn display_set_indicators_show_only_during_a_request() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    for (path, trigger, indicator, result) in [
        (
            "/_stories/skeleton",
            "Reload",
            "#story-skeleton",
            "#story-skeleton-result",
        ),
        (
            "/_stories/spinner",
            "Load",
            "#story-spinner",
            "#story-spinner-status",
        ),
    ] {
        // A spinner indicator keeps its root. Its content shows and hides.
        let indicator = &if indicator.contains("spinner") {
            format!("{indicator} > :first-child")
        } else {
            indicator.to_owned()
        };
        open(&page, path).await;
        assert!(!shown(&page, indicator).await, "{indicator} shows at rest");
        assert!(
            eval_bool(
                &page,
                &format!(
                    "(() => {{ const b = Array.from(document.querySelectorAll('.autumn-button')) \
                     .find(b => b.textContent.trim() === {trigger:?}); \
                     if (!b) return false; b.click(); return true; }})()"
                ),
            )
            .await,
            "no {trigger} button on {path}"
        );
        wait_for(
            &page,
            &format!("getComputedStyle(document.querySelector({indicator:?})).display !== 'none'"),
        )
        .await;
        wait_for(
            &page,
            &format!("document.querySelector({result:?}).textContent.includes('Saved.')"),
        )
        .await;
        wait_for(
            &page,
            &format!("getComputedStyle(document.querySelector({indicator:?})).display === 'none'"),
        )
        .await;
    }
    page.expect_no_console_errors().await.expect("no errors");
}
