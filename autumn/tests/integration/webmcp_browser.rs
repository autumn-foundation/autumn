//! `WebMCP` in a real browser (issue #2619): the default CSP, route tools,
//! form tools, and htmx swaps.
//!
//! The page loads a small stub of `document.modelContext` before the shim, so
//! the test does not need a Chromium build with `WebMCP` turned on.
//!
//! Run:
//!
//! ```text
//! cargo test -p autumn-web --features "system-tests,webmcp" \
//!   --test integration_tests webmcp_browser -- --include-ignored
//! ```

#![cfg(all(feature = "system-tests", feature = "webmcp"))]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use autumn_web::prelude::*;
use autumn_web::system_test::{Page, SystemTest};
use axum::response::{Html, IntoResponse};

static DELETES: AtomicUsize = AtomicUsize::new(0);
/// Deletes of item 9, which only the `drop_item` form sends. A separate
/// count keeps the parallel tests apart.
static DROPS: AtomicUsize = AtomicUsize::new(0);
/// Posts to `/quiet`, which only the `quiet_note` form sends.
static QUIET: AtomicUsize = AtomicUsize::new(0);
/// Requests to `/override`, which only the `override_drop` form sends.
static OVERRIDES: AtomicUsize = AtomicUsize::new(0);

const STUB_JS: &str = r#"
window.__reg = {};
window.__count = {};
Object.defineProperty(document, "modelContext", {
  configurable: true,
  value: {
    registerTool(tool, options) {
      const entry = { tool, aborted: false };
      window.__reg[tool.name] = entry;
      window.__count[tool.name] = (window.__count[tool.name] || 0) + 1;
      if (options && options.signal) {
        options.signal.addEventListener("abort", () => { entry.aborted = true; });
      }
      return Promise.resolve();
    },
  },
});
"#;

fn form(tool: &str) -> String {
    format!(
        r##"<form hx-post="/notes" hx-target="#out">
  <span hidden data-webmcp-tool="{tool}" data-webmcp-description="Add a note"></span>
  <label for="t">Title</label>
  <input id="t" name="title" required maxlength="40">
  <select name="color"><option value="red">Red</option><option value="blue">Blue</option></select>
  <button>Add</button>
</form>"##
    )
}

fn page_html(stub: bool) -> String {
    let stub = if stub {
        r#"<script src="/stub.js"></script>"#
    } else {
        ""
    };
    format!(
        r##"<!doctype html><html><head><meta charset="utf-8">
{stub}
<script src="/static/js/htmx.min.js"></script>
<script src="/static/js/autumn-webmcp.js" defer></script>
</head><body>
<div id="panel">{}</div>
<div id="out"></div>
<form hx-delete="/items/9" hx-target="#out">
  <span hidden data-webmcp-tool="drop_item" data-webmcp-description="Drop item 9"></span>
  <input name="reason">
  <button>Drop</button>
</form>
<button id="swap" hx-get="/panel" hx-target="#panel">Swap</button>
<form id="slow" hx-post="/slow" hx-target="#out">
  <span hidden data-webmcp-tool="slow_note" data-webmcp-description="Add a note slowly"></span>
  <input name="title">
  <button>Slow</button>
</form>
<form hx-post="/quiet" hx-target="#out">
  <span hidden data-webmcp-tool="quiet_note" data-webmcp-description="Add a quiet note"></span>
  <input name="title">
  <button>Quiet</button>
</form>
<form hx-post="/override" hx-target="#out">
  <span hidden data-webmcp-tool="override_drop" data-webmcp-description="Drop by override"></span>
  <input type="hidden" name="_method" value="DELETE">
  <input name="reason">
  <button>Drop</button>
</form>
</body></html>"##,
        form("add_note")
    )
}

#[get("/")]
async fn index() -> Html<String> {
    Html(page_html(true))
}

#[get("/bare")]
async fn bare() -> Html<String> {
    Html(page_html(false))
}

#[get("/stub.js")]
async fn stub_js() -> impl IntoResponse {
    ([("content-type", "text/javascript")], STUB_JS)
}

#[get("/panel")]
async fn panel() -> Html<String> {
    Html(form("add_note"))
}

#[derive(serde::Deserialize)]
struct Note {
    title: String,
}

#[post("/notes")]
async fn add_note(axum::Form(note): axum::Form<Note>) -> Html<String> {
    Html(format!("<p>Saved {}</p>", note.title))
}

#[post("/slow")]
async fn slow_note(axum::Form(note): axum::Form<Note>) -> Html<String> {
    tokio::time::sleep(Duration::from_millis(800)).await;
    Html(format!("<p>Slow {}</p>", note.title))
}

#[post("/quiet")]
async fn quiet_note() -> Html<&'static str> {
    QUIET.fetch_add(1, Ordering::SeqCst);
    Html("<p>Quiet</p>")
}

#[post("/override")]
async fn override_post() -> Html<&'static str> {
    OVERRIDES.fetch_add(1, Ordering::SeqCst);
    Html("<p>Posted</p>")
}

#[delete("/override")]
async fn override_delete() -> Html<&'static str> {
    OVERRIDES.fetch_add(1, Ordering::SeqCst);
    Html("<p>Deleted</p>")
}

#[derive(serde::Serialize, autumn_web::openapi::OpenApiSchema)]
struct Item {
    id: i64,
}

#[get("/items")]
#[api_doc(webmcp, summary = "List items")]
async fn list_items() -> Json<Vec<Item>> {
    Json(vec![Item { id: 7 }])
}

#[delete("/items/{id}")]
#[api_doc(webmcp, summary = "Delete an item")]
async fn delete_item(axum::extract::Path(id): axum::extract::Path<i64>) -> Json<bool> {
    if id == 9 { &DROPS } else { &DELETES }.fetch_add(1, Ordering::SeqCst);
    Json(true)
}

async fn eval_string(page: &Page, js: &str) -> String {
    page.evaluate(js)
        .await
        .expect("evaluate")
        .into_value::<String>()
        .expect("a string result")
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

async fn runner() -> autumn_web::system_test::SystemTestRunner {
    Box::pin(
        SystemTest::new()
            .routes(routes![
                index,
                bare,
                stub_js,
                panel,
                add_note,
                slow_note,
                quiet_note,
                override_post,
                override_delete,
                list_items,
                delete_item
            ])
            .mount_webmcp()
            .build(),
    )
    .await
    .expect("start runner")
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn route_tools_run_under_the_default_csp_and_call_the_route() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    page.visit("/").await.expect("visit");
    wait_for(&page, "!!(window.__reg && window.__reg.list_items)").await;

    let out = eval_string(
        &page,
        "window.__reg.list_items.tool.execute({}, {}).then(r => JSON.stringify(r))",
    )
    .await;
    let out: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(out["isError"], false, "{out}");
    assert!(
        out["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("\"id\":7"),
        "{out}"
    );

    page.expect_no_console_errors()
        .await
        .expect("no console errors");
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn a_declined_confirmation_sends_nothing() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    page.visit("/").await.expect("visit");
    wait_for(&page, "!!(window.__reg && window.__reg.delete_item)").await;
    let before = DELETES.load(Ordering::SeqCst);

    let out = eval_string(
        &page,
        "(() => { window.confirm = () => false; \
           return window.__reg.delete_item.tool.execute({ id: '7' }, {}) \
             .then(r => JSON.stringify(r)); })()",
    )
    .await;
    assert!(out.contains("\"isError\":true"), "{out}");
    assert_eq!(DELETES.load(Ordering::SeqCst), before, "nothing was sent");

    let out = eval_string(
        &page,
        "window.__reg.delete_item.tool.execute({ id: '7' }, { \
           requestUserInteraction: async (cb) => true }).then(r => JSON.stringify(r))",
    )
    .await;
    assert!(out.contains("\"isError\":false"), "{out}");
    assert_eq!(DELETES.load(Ordering::SeqCst), before + 1);
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn a_form_tool_fills_and_submits_through_htmx() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    page.visit("/").await.expect("visit");
    wait_for(&page, "!!(window.__reg && window.__reg.add_note)").await;

    let schema = eval_string(
        &page,
        "JSON.stringify(window.__reg.add_note.tool.inputSchema)",
    )
    .await;
    let schema: serde_json::Value = serde_json::from_str(&schema).unwrap();
    assert_eq!(schema["properties"]["title"]["type"], "string", "{schema}");
    assert_eq!(schema["properties"]["title"]["maxLength"], 40, "{schema}");
    assert_eq!(
        schema["properties"]["title"]["description"], "Title",
        "{schema}"
    );
    assert_eq!(
        schema["properties"]["color"]["enum"],
        serde_json::json!(["red", "blue"])
    );
    assert_eq!(schema["required"], serde_json::json!(["title"]));

    let out = eval_string(
        &page,
        "window.__reg.add_note.tool.execute({ title: 'milk', color: 'blue' }, {}) \
           .then(r => JSON.stringify(r))",
    )
    .await;
    assert!(out.contains("Saved milk"), "{out}");
    assert!(out.contains("\"isError\":false"), "{out}");

    let out = eval_string(
        &page,
        "window.__reg.add_note.tool.execute({ title: '' }, {}).then(r => JSON.stringify(r))",
    )
    .await;
    assert!(
        out.contains("\"isError\":true"),
        "an invalid form is refused: {out}"
    );
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn an_hx_delete_form_asks_first_and_a_no_fills_nothing() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    page.visit("/").await.expect("visit");
    wait_for(&page, "!!(window.__reg && window.__reg.drop_item)").await;
    let hints = eval_string(
        &page,
        "JSON.stringify(window.__reg.drop_item.tool.annotations)",
    )
    .await;
    assert!(hints.contains("\"readOnlyHint\":false"), "{hints}");
    assert!(hints.contains("\"consequentialHint\":true"), "{hints}");
    let before = DROPS.load(Ordering::SeqCst);

    let out = eval_string(
        &page,
        "(() => { window.confirm = () => false; \
           return window.__reg.drop_item.tool.execute({ reason: 'spam' }, {}) \
             .then(r => JSON.stringify(r)); })()",
    )
    .await;
    assert!(out.contains("\"isError\":true"), "{out}");
    assert!(
        eval_bool(
            &page,
            "document.querySelector('input[name=reason]').value === ''"
        )
        .await,
        "a declined call fills nothing"
    );
    assert_eq!(DROPS.load(Ordering::SeqCst), before);

    let out = eval_string(
        &page,
        "(() => { window.confirm = () => true; \
           return window.__reg.drop_item.tool.execute({ reason: 'spam' }, {}) \
             .then(r => JSON.stringify(r)); })()",
    )
    .await;
    assert!(out.contains("\"isError\":false"), "{out}");
    assert_eq!(DROPS.load(Ordering::SeqCst), before + 1);
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn a_value_the_field_cannot_hold_is_an_error() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    page.visit("/").await.expect("visit");
    wait_for(&page, "!!(window.__reg && window.__reg.add_note)").await;
    let out = eval_string(
        &page,
        "window.__reg.add_note.tool.execute({ title: 'x', color: 'green' }, {}) \
           .then(r => JSON.stringify(r))",
    )
    .await;
    assert!(out.contains("\"isError\":true"), "{out}");
    assert!(out.contains("color"), "{out}");
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn an_htmx_swap_unregisters_the_form_tool_and_the_new_form_registers() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    page.visit("/").await.expect("visit");
    wait_for(&page, "!!(window.__reg && window.__reg.add_note)").await;
    eval_bool(&page, "window.__first = window.__reg.add_note; true").await;

    page.click("#swap").await.expect("swap");
    page.expect_hx_settle().await.expect("settle");

    wait_for(&page, "window.__first.aborted === true").await;
    wait_for(&page, "window.__count.add_note === 2").await;
    assert!(eval_bool(&page, "window.__reg.add_note.aborted === false").await);
    page.expect_no_console_errors()
        .await
        .expect("no console errors");
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn a_second_call_while_the_form_is_busy_is_refused() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    page.visit("/").await.expect("visit");
    wait_for(&page, "!!(window.__reg && window.__reg.slow_note)").await;

    let out = eval_string(
        &page,
        "(() => { const t = window.__reg.slow_note.tool; \
           return Promise.all([t.execute({ title: 'one' }, {}), t.execute({ title: 'two' }, {})]) \
             .then(r => JSON.stringify(r)); })()",
    )
    .await;
    let out: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(out[0]["isError"], false, "{out}");
    assert!(
        out[0]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Slow one"),
        "{out}"
    );
    assert_eq!(out[1]["isError"], true, "{out}");
    assert!(
        out[1]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("busy"),
        "{out}"
    );
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn a_cancelled_call_sends_nothing() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    page.visit("/").await.expect("visit");
    wait_for(&page, "!!(window.__reg && window.__reg.quiet_note)").await;
    let before = QUIET.load(Ordering::SeqCst);

    let out = eval_string(
        &page,
        "(() => { const c = new AbortController(); c.abort(); \
           return window.__reg.quiet_note.tool.execute({ title: 'x' }, { signal: c.signal }) \
             .then(r => JSON.stringify(r)); })()",
    )
    .await;
    assert!(out.contains("\"isError\":true"), "{out}");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(QUIET.load(Ordering::SeqCst), before, "nothing was sent");
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn cancelling_a_form_call_aborts_the_htmx_request() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    page.visit("/").await.expect("visit");
    wait_for(&page, "!!(window.__reg && window.__reg.slow_note)").await;

    let out = eval_string(
        &page,
        "(() => { window.__xhrAborted = false; \
           document.getElementById('slow').addEventListener('htmx:xhr:abort', \
             () => { window.__xhrAborted = true; }); \
           const c = new AbortController(); \
           const p = window.__reg.slow_note.tool.execute({ title: 'x' }, { signal: c.signal }); \
           setTimeout(() => c.abort(), 200); \
           return p.then(r => JSON.stringify(r)); })()",
    )
    .await;
    assert!(out.contains("\"isError\":true"), "{out}");
    wait_for(&page, "window.__xhrAborted === true").await;
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn an_hx_post_form_with_a_delete_override_asks_first() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    page.visit("/").await.expect("visit");
    wait_for(&page, "!!(window.__reg && window.__reg.override_drop)").await;
    let before = OVERRIDES.load(Ordering::SeqCst);

    let out = eval_string(
        &page,
        "(() => { window.__asked = 0; \
           window.confirm = () => { window.__asked++; return false; }; \
           return window.__reg.override_drop.tool.execute({ reason: 'spam' }, {}) \
             .then(r => JSON.stringify(r)); })()",
    )
    .await;
    assert!(out.contains("\"isError\":true"), "{out}");
    assert!(
        eval_bool(&page, "window.__asked === 1").await,
        "a DELETE override asks the user"
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(OVERRIDES.load(Ordering::SeqCst), before, "nothing was sent");
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn without_model_context_the_shim_does_nothing() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    page.visit("/bare").await.expect("visit");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(eval_bool(&page, "typeof window.__reg === 'undefined'").await);
    page.expect_no_console_errors()
        .await
        .expect("no console errors");
}
