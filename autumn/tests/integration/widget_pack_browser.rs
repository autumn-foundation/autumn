//! Widget pack conformance in a real browser (issue #3400).
//!
//! - A good pack passes the static and the browser checks.
//! - Three canary packs prove that the axe, keyboard and console checks
//!   fail.
//! - Pack JS gets `autumn:init` on page load and after an htmx swap,
//!   before roving init.
//!
//! Run:
//!
//! ```text
//! cargo test -p autumn-web --features system-tests \
//!   --test integration_tests widget_pack_browser -- --include-ignored
//! ```

#![cfg(all(feature = "system-tests", feature = "maud", feature = "htmx"))]

use std::time::Duration;

use autumn_web::assets::PluginAssets;
use autumn_web::form::{Changeset, FieldContext, FieldOptions, FieldRenderer, custom_input_with};
use autumn_web::plugin_conformance::widgets::browser::run_widget_browser_conformance;
use autumn_web::plugin_conformance::widgets::run_widget_conformance;
use autumn_web::plugin_conformance::{CheckStatus, ConformanceReport};
use autumn_web::prelude::*;
use autumn_web::stories::Story;
use autumn_web::system_test::{Page, SystemTest};
use autumn_web::widget_pack::WidgetPack;
use maud::{DOCTYPE, Markup, html};

// ── The good pack ───────────────────────────────────────────────────────

const KGOOD_CSS: &[u8] = b"\
.autumn-kgood-toolbar { display: flex; gap: var(--space-2); }
.autumn-kgood-chip {
    color: var(--on-primary);
    background: var(--primary);
    border: 1px solid var(--control-border);
    border-radius: var(--radius);
    padding: var(--space-1) var(--space-3);
}
.autumn-kgood-chip[aria-pressed='true'] { outline: 2px solid var(--text); }
";

/// Marks each toolbar on `autumn:init`, and toggles `aria-pressed` on a
/// click. No arrow-key code: the keyboard core moves focus.
const KGOOD_JS: &[u8] = b"\
(function () {
  function init(root) {
    var all = root.querySelectorAll ? root.querySelectorAll('[data-kgood-toolbar]') : [];
    Array.prototype.forEach.call(all, function (t) {
      t.setAttribute('data-kgood-ready', String(t.querySelector('[tabindex]') === null));
    });
    if (root.matches && root.matches('[data-kgood-toolbar]')) {
      root.setAttribute('data-kgood-ready', String(root.querySelector('[tabindex]') === null));
    }
  }
  document.addEventListener('autumn:init', function (e) { init(e.target); });
  document.addEventListener('click', function (e) {
    var chip = e.target.closest && e.target.closest('.autumn-kgood-chip');
    if (chip) chip.setAttribute('aria-pressed', String(chip.getAttribute('aria-pressed') !== 'true'));
  });
})();
";

static KGOOD_ASSETS: PluginAssets =
    PluginAssets::from_files("kgood", &[("kgood.css", KGOOD_CSS), ("kgood.js", KGOOD_JS)]);

fn kgood_toolbar() -> Markup {
    html! {
        div class="autumn-kgood-toolbar" role="toolbar" aria-label="Colors"
            data-kgood-toolbar data-autumn-roving="button" data-autumn-orientation="horizontal" {
            @for name in ["Primary", "Success", "Danger"] {
                button type="button" class="autumn-kgood-chip" aria-pressed="false" { (name) }
            }
        }
    }
}

/// A right-to-left toolbar that does not wrap. `ArrowLeft` moves forward,
/// so the keyboard check must not press `ArrowRight` on the first item.
fn kgood_rtl_toolbar() -> Markup {
    html! {
        div dir="rtl" class="autumn-kgood-toolbar" role="toolbar" aria-label="Colors (RTL)"
            data-autumn-roving="button" data-autumn-orientation="horizontal"
            data-autumn-wrap="false" {
            @for name in ["Primary", "Success", "Danger"] {
                button type="button" class="autumn-kgood-chip" aria-pressed="false" { (name) }
            }
        }
    }
}

/// A closed popup list. The keyboard check skips a list that is not shown.
fn kgood_closed_list() -> Markup {
    html! {
        button type="button" class="autumn-kgood-chip" aria-controls="kgood-list"
            aria-expanded="false" { "Choose" }
        ul id="kgood-list" class="autumn-kgood-list" role="listbox" aria-label="Choices"
            hidden data-autumn-roving data-autumn-dismiss {
            li role="option" { "Apple" }
            li role="option" { "Banana" }
        }
    }
}

/// A radio group with a hidden first radio and no checked radio. The Tab
/// stop is the first visible radio.
fn kgood_hidden_radio() -> Markup {
    html! {
        fieldset {
            legend { "Size" }
            label hidden { input type="radio" name="size" value="xs" hidden; "Extra small" }
            label { input type="radio" name="size" value="s"; "Small" }
            label { input type="radio" name="size" value="m"; "Medium" }
        }
    }
}

/// A text input bound with `FieldRenderer`.
struct KgoodInput;

impl FieldRenderer for KgoodInput {
    fn name(&self) -> &'static str {
        "kgood"
    }

    fn render(&self, field: &FieldContext<'_>) -> Markup {
        field.control(
            "input",
            &[("type", "text"), ("class", "autumn-kgood-input")],
            html! {},
        )
    }
}

#[derive(serde::Serialize)]
struct Paint {
    color: String,
}

fn kgood_field() -> Markup {
    custom_input_with(
        &Changeset::new(Paint {
            color: "primary".into(),
        }),
        "color",
        "Color",
        &KgoodInput,
        &FieldOptions::new().hint("A token name."),
    )
}

fn kgood_pack() -> WidgetPack {
    WidgetPack::new("kgood")
        .extend([
            Story::new("Kgood", "Kgood toolbar", kgood_toolbar, "kgood_toolbar()"),
            Story::new("Kgood", "Kgood field", kgood_field, "kgood_field()"),
            Story::new(
                "Kgood",
                "Kgood rtl toolbar",
                kgood_rtl_toolbar,
                "kgood_rtl_toolbar()",
            ),
            Story::new(
                "Kgood",
                "Kgood closed list",
                kgood_closed_list,
                "kgood_closed_list()",
            ),
            Story::new(
                "Kgood",
                "Kgood hidden radio",
                kgood_hidden_radio,
                "kgood_hidden_radio()",
            ),
        ])
        .assets(&KGOOD_ASSETS)
}

// ── Canary packs ────────────────────────────────────────────────────────

fn kaxe_story() -> Markup {
    html! { input type="text" name="q" class="autumn-kaxe-input"; }
}

fn ktrap_story() -> Markup {
    html! {
        div class="autumn-ktrap-box" {
            button type="button" class="autumn-ktrap-a" { "One" }
            button type="button" class="autumn-ktrap-b" { "Two" }
        }
    }
}

/// Holds focus in the box: Tab never leaves it.
static KTRAP_ASSETS: PluginAssets = PluginAssets::from_files(
    "ktrap",
    &[(
        "ktrap.js",
        b"document.addEventListener('keydown', function (e) {\n\
          if (e.key === 'Tab' && e.target.closest && e.target.closest('.autumn-ktrap-box')) {\n\
            e.preventDefault();\n\
          }\n\
        });\n",
    )],
);

fn kerr_story() -> Markup {
    html! { p class="autumn-kerr-text" { "Text" } }
}

static KERR_ASSETS: PluginAssets =
    PluginAssets::from_files("kerr", &[("kerr.js", b"throw new Error('kerr boom');\n")]);

// ── Helpers ─────────────────────────────────────────────────────────────

fn axe_source() -> String {
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/axe-core/axe.min.js");
    std::fs::read_to_string(&path).unwrap_or_else(|err| panic!("read {}: {err}", path.display()))
}

async fn browser_report(pack: &WidgetPack) -> ConformanceReport {
    run_widget_browser_conformance(pack, &axe_source())
        .await
        .expect("browser conformance runs")
}

fn check<'a>(
    report: &'a ConformanceReport,
    name: &str,
) -> &'a autumn_web::plugin_conformance::CheckResult {
    report
        .checks
        .iter()
        .find(|c| c.name == name)
        .unwrap_or_else(|| panic!("no check {name}: {}", report.to_text_report()))
}

// ── Tests ───────────────────────────────────────────────────────────────

#[tokio::test]
#[ignore = "requires Chromium"]
async fn a_good_pack_passes_the_static_and_browser_checks() {
    let pack = kgood_pack();
    let report = run_widget_conformance(&pack);
    assert!(report.passed(), "{}", report.to_text_report());
    let report = browser_report(&pack).await;
    assert!(report.passed(), "{}", report.to_text_report());
    for name in ["widget-axe", "widget-console", "widget-keyboard"] {
        assert_eq!(check(&report, name).status, CheckStatus::Pass, "{name}");
    }
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn canary_axe_fails_on_an_unlabelled_input() {
    let pack = WidgetPack::new("kaxe").extend([Story::new("K", "Kaxe input", kaxe_story, "")]);
    let report = browser_report(&pack).await;
    let axe = check(&report, "widget-axe");
    assert_eq!(axe.status, CheckStatus::Fail, "{}", report.to_text_report());
    assert!(
        axe.diagnostics
            .iter()
            .any(|d| d.contains("[light]") && d.contains("label "))
            && axe.diagnostics.iter().any(|d| d.contains("[dark]")),
        "{}",
        report.to_text_report()
    );
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn canary_keyboard_fails_on_a_focus_trap() {
    let pack = WidgetPack::new("ktrap")
        .extend([Story::new("K", "Ktrap box", ktrap_story, "")])
        .assets(&KTRAP_ASSETS);
    let report = browser_report(&pack).await;
    let keyboard = check(&report, "widget-keyboard");
    assert_eq!(
        keyboard.status,
        CheckStatus::Fail,
        "{}",
        report.to_text_report()
    );
    assert!(
        keyboard
            .diagnostics
            .iter()
            .any(|d| d.contains("keyboard trap")),
        "{}",
        report.to_text_report()
    );
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn canary_console_fails_on_a_script_error() {
    let pack = WidgetPack::new("kerr")
        .extend([Story::new("K", "Kerr text", kerr_story, "")])
        .assets(&KERR_ASSETS);
    let report = browser_report(&pack).await;
    let console = check(&report, "widget-console");
    assert_eq!(
        console.status,
        CheckStatus::Fail,
        "{}",
        report.to_text_report()
    );
    // The harness records an uncaught exception as `Uncaught`.
    assert!(
        console
            .diagnostics
            .iter()
            .any(|d| d.starts_with("story `kerr-text`: Uncaught")),
        "{}",
        report.to_text_report()
    );
}

// ── `autumn:init` (gap 3) ───────────────────────────────────────────────

#[get("/kinit")]
async fn kinit_page() -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                title { "Init" }
                link rel="stylesheet" href="/static/css/autumn-widgets.css";
                script src="/static/js/htmx.min.js" defer {}
                script src="/static/js/autumn-widgets.js" defer {}
                (WidgetPack::new("kgood").assets(&KGOOD_ASSETS).head())
            }
            body {
                main {
                    div id="first" { (kgood_toolbar()) }
                    button id="load" type="button" hx-get="/kinit/fragment"
                        hx-target="#slot" hx-swap="innerHTML" { "Load" }
                    div id="slot" {}
                }
            }
        }
    }
}

#[get("/kinit/fragment")]
async fn kinit_fragment() -> Markup {
    kgood_toolbar()
}

async fn poll(page: &Page, js: &str) -> bool {
    for _ in 0..100 {
        if page
            .evaluate(js)
            .await
            .ok()
            .and_then(|r| r.into_value::<bool>().ok())
            .unwrap_or(false)
        {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

/// A deferred pack script that loads after the runtime gets the page-load
/// `autumn:init`, and gets it again for an htmx swap. It runs before roving
/// init: the toolbar has no `tabindex` yet when the pack sees it.
#[tokio::test]
#[ignore = "requires Chromium"]
async fn pack_js_gets_autumn_init_on_load_and_after_a_swap() {
    let runner = Box::pin(
        SystemTest::new()
            .routes(routes![kinit_page, kinit_fragment])
            .plugin_assets(&KGOOD_ASSETS)
            .build(),
    )
    .await
    .expect("start runner");
    let page = runner.page().await.expect("page");
    page.visit("/kinit").await.expect("visit");
    assert!(
        poll(&page, "document.readyState === 'complete'").await,
        "page did not load"
    );
    assert!(
        poll(
            &page,
            "document.querySelector('#first [data-kgood-toolbar]').getAttribute('data-kgood-ready') === 'true'"
        )
        .await,
        "the pack did not get autumn:init on page load, before roving init"
    );
    assert!(
        poll(
            &page,
            "document.querySelector('#first .autumn-kgood-chip').tabIndex === 0"
        )
        .await,
        "roving init did not run after autumn:init"
    );

    page.click("#load").await.expect("click");
    assert!(
        poll(
            &page,
            "(() => { const t = document.querySelector('#slot [data-kgood-toolbar]'); \
             return !!t && t.getAttribute('data-kgood-ready') === 'true'; })()"
        )
        .await,
        "the pack did not get autumn:init after the htmx swap"
    );
    assert!(
        poll(
            &page,
            "document.querySelector('#slot .autumn-kgood-chip').tabIndex === 0"
        )
        .await,
        "roving init did not run on the swapped toolbar"
    );
    page.expect_no_console_errors()
        .await
        .expect("no console errors");
}
