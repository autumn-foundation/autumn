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

async fn attr(page: &Page, selector: &str, name: &str) -> String {
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
            &format!("document.querySelector('{MENU}').offsetParent !== null"),
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
