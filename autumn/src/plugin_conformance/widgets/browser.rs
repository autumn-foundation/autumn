//! Widget pack conformance in a browser (issue #3400).
//!
//! [`run_widget_browser_conformance`] serves the pack stories in the
//! `/_stories` gallery and opens each one in headless Chromium:
//!
//! | Check | Fails when |
//! |---|---|
//! | `widget-axe` | axe-core finds a serious or critical violation, in the light or the dark scheme. The core gate uses the same [`Page::axe_violations`]. |
//! | `widget-console` | The page logs a console error or an uncaught exception (for example a CSP violation). |
//! | `widget-keyboard` | Tab does not reach each focusable element of the story, focus cannot leave the story (a trap), or an arrow key does not move focus in a `data-autumn-roving` list. |
//!
//! Interactive widgets also need their own keyboard tests with
//! [`Page::press`]. This check finds the common faults only.
//!
//! ```rust,no_run
//! # async fn example(pack: autumn_web::widget_pack::WidgetPack) {
//! use autumn_web::plugin_conformance::widgets::browser::run_widget_browser_conformance;
//!
//! let axe = std::fs::read_to_string("tests/fixtures/axe.min.js").expect("axe-core");
//! let report = run_widget_browser_conformance(&pack, &axe)
//!     .await
//!     .expect("browser starts");
//! assert!(report.passed(), "{}", report.to_text_report());
//! # }
//! ```

use std::time::Duration;

use crate::AppState;
use crate::config::AutumnConfig;
use crate::plugin_conformance::ConformanceReport;
use crate::system_test::{Page, SystemTest, SystemTestError};
use crate::widget_pack::WidgetPack;

use super::result;

/// The schemes the gallery switches between.
const SCHEMES: [&str; 2] = ["light", "dark"];

/// Run the axe, console and keyboard checks on each story of `pack`.
///
/// `axe_source` is the axe-core 4.x script. Read it from your own vendored
/// copy.
///
/// # Errors
///
/// [`SystemTestError`] when Chromium cannot start or a page does not load.
/// A failed check is a `Fail` in the report, not an error.
pub async fn run_widget_browser_conformance(
    pack: &WidgetPack,
    axe_source: &str,
) -> Result<ConformanceReport, SystemTestError> {
    let mut config = AutumnConfig {
        profile: Some("test".into()),
        ..Default::default()
    };
    config.security.csrf.enabled = false;
    config.stories.enabled = true;
    let state = AppState::for_test().with_profile("test");
    state.insert_extension(config);
    state.insert_extension(pack.gallery().into_registry());

    let mut harness = SystemTest::new().state(state);
    if let Some(assets) = pack.asset_bundle() {
        harness = harness.plugin_assets(assets);
    }
    let runner = Box::pin(harness.build()).await?;
    let page = runner.page().await?;

    let mut axe = Vec::new();
    let mut console = Vec::new();
    let mut keyboard = Vec::new();
    for story in pack.stories() {
        let slug = story.slug();
        let path = format!("{}/{slug}", crate::stories::STORIES_PATH);
        let errors_before = page.console_errors().len();
        open(&page, &path).await?;
        for scheme in SCHEMES {
            select_scheme(&page, scheme).await?;
            axe.extend(
                page.axe_violations(axe_source)
                    .await?
                    .into_iter()
                    .map(|violation| format!("story `{slug}` [{scheme}]: {violation}")),
            );
        }
        keyboard.extend(
            tab_problems(&page)
                .await?
                .into_iter()
                .map(|problem| format!("story `{slug}`: {problem}")),
        );
        keyboard.extend(
            roving_problems(&page)
                .await?
                .into_iter()
                .map(|problem| format!("story `{slug}`: {problem}")),
        );
        // A late error (a deferred script, a CSP report) needs a moment.
        tokio::time::sleep(Duration::from_millis(250)).await;
        console.extend(
            page.console_errors()
                .into_iter()
                .skip(errors_before)
                .map(|error| format!("story `{slug}`: {error}")),
        );
    }

    Ok(ConformanceReport {
        plugin_name: pack.name().to_owned(),
        checks: vec![
            result(
                "widget-axe",
                axe,
                "axe finds no serious or critical violation in either scheme".to_owned(),
                "axe violations",
            ),
            result(
                "widget-console",
                console,
                "no console error".to_owned(),
                "console errors",
            ),
            result(
                "widget-keyboard",
                keyboard,
                "Tab reaches each control and roving lists move".to_owned(),
                "keyboard problems",
            ),
        ],
    })
}

/// Load `path` and wait for the page, the widget runtime and htmx.
async fn open(page: &Page, path: &str) -> Result<(), SystemTestError> {
    page.visit(path).await?;
    wait_for(page, "document.readyState === 'complete'").await?;
    wait_for(page, "window.__autumnWidgets === true").await?;
    page.expect_hx_settle().await?;
    Ok(())
}

/// Select the gallery color scheme and wait until no CSS transition runs.
async fn select_scheme(page: &Page, scheme: &str) -> Result<(), SystemTestError> {
    wait_for(
        page,
        &format!(
            "(() => {{ const r = document.getElementById('story-theme-{scheme}'); \
             if (!r) return false; r.click(); return r.checked; }})()"
        ),
    )
    .await?;
    wait_for(
        page,
        &format!("getComputedStyle(document.documentElement).colorScheme === '{scheme}'"),
    )
    .await?;
    wait_for(
        page,
        "document.getAnimations().every(a => !(a instanceof CSSTransition))",
    )
    .await
}

/// Mark each Tab stop in the story preview.
///
/// Each stop gets `data-conformance-stop`. Return a label for each one. A radio group is one stop: the checked
/// radio, else the first. An `<iframe>` is not a stop: its own document
/// holds the focus.
const MARK_STOPS_JS: &str = r#"(() => {
  const preview = document.querySelector('.story-preview');
  if (!preview) return '[]';
  const sel = 'a[href],area[href],button,input:not([type=hidden]),select,textarea,summary,[tabindex],[contenteditable=""],[contenteditable="true"]';
  const groups = new Set();
  const stops = [];
  for (const el of preview.querySelectorAll(sel)) {
    if (el.matches(':disabled') || el.tabIndex < 0 || el.closest('[inert]')) continue;
    if (!el.checkVisibility({ visibilityProperty: true })) continue;
    if (el.type === 'radio' && el.name) {
      const key = el.name + '|' + (el.form ? el.form.id : '');
      if (groups.has(key)) continue;
      groups.add(key);
      const all = [...preview.querySelectorAll('input[type=radio]')]
        .filter(r => r.name === el.name && r.form === el.form && !r.matches(':disabled'));
      const pick = all.find(r => r.checked) || all[0];
      if (pick !== el) { pick.setAttribute('data-conformance-stop', String(stops.length)); stops.push(pick); continue; }
    }
    el.setAttribute('data-conformance-stop', String(stops.length));
    stops.push(el);
  }
  return JSON.stringify(stops.map(el => {
    const text = (el.getAttribute('aria-label') || el.textContent || el.value || '').trim().slice(0, 30);
    return '<' + el.tagName.toLowerCase() + (el.id ? '#' + el.id : '') + '> ' + JSON.stringify(text);
  }));
})()"#;

/// The stop under focus: its index, `-1` for another element in the
/// preview, `-2` for focus outside the preview.
const ACTIVE_STOP_JS: &str = r"(() => {
  const el = document.activeElement;
  const preview = document.querySelector('.story-preview');
  if (!el || !preview || !preview.contains(el)) return -2;
  const index = el.getAttribute('data-conformance-stop');
  return index === null ? -1 : Number(index);
})()";

/// Tab from the breadcrumb through the preview. Each stop must get focus,
/// and focus must leave the preview.
async fn tab_problems(page: &Page) -> Result<Vec<String>, SystemTestError> {
    let labels: Vec<String> = serde_json::from_str(&eval_string(page, MARK_STOPS_JS).await?)
        .map_err(|err| SystemTestError::AssertionFailed {
            message: format!("tab stops: {err}"),
            artifact_path: None,
        })?;
    if labels.is_empty() {
        return Ok(Vec::new());
    }
    if !eval_bool(
        page,
        "(() => { const a = document.querySelector('.story-breadcrumb a'); \
         if (!a) return false; a.focus(); return document.activeElement === a; })()",
    )
    .await?
    {
        return Ok(vec![
            "the gallery breadcrumb link cannot take focus".to_owned(),
        ]);
    }
    let mut reached = vec![false; labels.len()];
    let mut entered = false;
    let mut left = false;
    // Each stop once, plus room for one extra element, then the exit.
    for _ in 0..labels.len().saturating_mul(2).saturating_add(2) {
        page.press("Tab").await?;
        let active = eval_i64(page, ACTIVE_STOP_JS).await?;
        if active == -2 {
            if entered {
                left = true;
                break;
            }
            continue;
        }
        entered = true;
        if let Some(seen) = usize::try_from(active)
            .ok()
            .and_then(|i| reached.get_mut(i))
        {
            *seen = true;
        }
    }
    let mut problems: Vec<String> = labels
        .iter()
        .zip(&reached)
        .filter(|(_, reached)| !**reached)
        .map(|(label, _)| format!("Tab does not reach {label}"))
        .collect();
    if entered && !left {
        problems.push("focus cannot leave the story with Tab (a keyboard trap)".to_owned());
    }
    Ok(problems)
}

/// Mark each visible `data-autumn-roving` list in the preview and return,
/// for each one, its item count.
const ROVING_LISTS_JS: &str = r#"(() => {
  const ITEMS = '[role="tab"],[role="menuitem"],[role="option"],[role="radio"],[role="treeitem"]';
  const LIST = '[data-autumn-roving],[data-autumn-activedescendant]';
  const preview = document.querySelector('.story-preview');
  const lists = preview ? [...preview.querySelectorAll('[data-autumn-roving]')]
    .filter(list => list.checkVisibility({ visibilityProperty: true })) : [];
  return JSON.stringify(lists.map((list, i) => {
    list.setAttribute('data-conformance-list', String(i));
    return [...list.querySelectorAll(list.getAttribute('data-autumn-roving') || ITEMS)]
      .filter(el => !el.hidden && el.getAttribute('aria-disabled') !== 'true' && el.closest(LIST) === list)
      .length;
  }));
})()"#;

/// Focus and mark the tab stop of list `LIST_INDEX`.
///
/// Return the arrow key to
/// press: forward, or back when the stop is the last item of a list that
/// does not wrap. Return `""` when the list has no tab stop.
const FOCUS_LIST_JS: &str = r#"(() => {
  const ITEMS = '[role="tab"],[role="menuitem"],[role="option"],[role="radio"],[role="treeitem"]';
  const LIST = '[data-autumn-roving],[data-autumn-activedescendant]';
  const list = document.querySelector('[data-conformance-list="LIST_INDEX"]');
  const items = [...list.querySelectorAll(list.getAttribute('data-autumn-roving') || ITEMS)]
    .filter(el => !el.hidden && el.getAttribute('aria-disabled') !== 'true' && el.closest(LIST) === list);
  const stop = items.find(el => el.tabIndex === 0);
  if (!stop) return '';
  stop.focus();
  if (document.activeElement !== stop) return '';
  document.querySelectorAll('[data-conformance-from]').forEach(el => el.removeAttribute('data-conformance-from'));
  stop.setAttribute('data-conformance-from', '');
  const horizontal = list.getAttribute('data-autumn-orientation') === 'horizontal';
  const back = list.getAttribute('data-autumn-wrap') === 'false' && items.indexOf(stop) === items.length - 1;
  if (horizontal) return back ? 'ArrowLeft' : 'ArrowRight';
  return back ? 'ArrowUp' : 'ArrowDown';
})()"#;

/// `true` when focus moved to another item of list `LIST_INDEX`, and that item
/// is now the tab stop.
const MOVED_JS: &str = r#"(() => {
  const list = document.querySelector('[data-conformance-list="LIST_INDEX"]');
  const a = document.activeElement;
  return !!a && list.contains(a) && !a.hasAttribute('data-conformance-from') && a.tabIndex === 0;
})()"#;

/// Check that an arrow key moves focus in each visible roving list with
/// two or more items.
async fn roving_problems(page: &Page) -> Result<Vec<String>, SystemTestError> {
    let counts: Vec<usize> = serde_json::from_str(&eval_string(page, ROVING_LISTS_JS).await?)
        .map_err(|err| SystemTestError::AssertionFailed {
            message: format!("roving lists: {err}"),
            artifact_path: None,
        })?;
    let mut problems = Vec::new();
    for (i, count) in counts.into_iter().enumerate() {
        if count < 2 {
            continue;
        }
        let index = i.to_string();
        let key = eval_string(page, &FOCUS_LIST_JS.replace("LIST_INDEX", &index)).await?;
        if key.is_empty() {
            problems.push(format!("roving list {i} has no item with tabindex=\"0\""));
            continue;
        }
        page.press(&key).await?;
        if !eval_bool(page, &MOVED_JS.replace("LIST_INDEX", &index)).await? {
            problems.push(format!("{key} does not move focus in roving list {i}"));
        }
    }
    Ok(problems)
}

async fn eval_string(page: &Page, js: &str) -> Result<String, SystemTestError> {
    Ok(page
        .evaluate(js)
        .await?
        .into_value::<String>()
        .unwrap_or_default())
}

async fn eval_bool(page: &Page, js: &str) -> Result<bool, SystemTestError> {
    Ok(page
        .evaluate(js)
        .await?
        .into_value::<bool>()
        .unwrap_or(false))
}

async fn eval_i64(page: &Page, js: &str) -> Result<i64, SystemTestError> {
    Ok(page.evaluate(js).await?.into_value::<i64>().unwrap_or(-2))
}

/// Poll `js` until it is true. Fail after 5 s.
async fn wait_for(page: &Page, js: &str) -> Result<(), SystemTestError> {
    for _ in 0..100 {
        if page
            .evaluate(js)
            .await
            .ok()
            .and_then(|r| r.into_value::<bool>().ok())
            .unwrap_or(false)
        {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Err(SystemTestError::Timeout {
        message: format!("waiting for `{js}`"),
        timeout: Duration::from_secs(5),
    })
}
