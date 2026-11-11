//! `direct_upload_input` in a real browser (issue #3411).
//!
//! An in-memory store stands in for the storage backend. `/presign` hands out
//! a PUT URL on that store, like the app route in `docs/guide/storage.md`.
//!
//! Run:
//!
//! ```text
//! cargo test -p autumn-web --features "system-tests,storage" \
//!   --test integration_tests widgets_direct_upload_browser -- --include-ignored
//! ```

#![cfg(all(feature = "system-tests", feature = "maud", feature = "storage"))]

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use autumn_web::prelude::*;
use autumn_web::storage::form_helper::{DirectUploadConfig, direct_upload_field};
use autumn_web::system_test::{Page, SystemTest, SystemTestRunner};
use autumn_web::widgets::{ButtonConfig, button};
use maud::{DOCTYPE, Markup, html};

/// A stored object: its content type and bytes.
type Stored = (String, Vec<u8>);

static STORE: LazyLock<Mutex<HashMap<String, Stored>>> = LazyLock::new(Mutex::default);
static PUT_TOKENS: LazyLock<Mutex<HashMap<String, String>>> = LazyLock::new(Mutex::default);
static PRESIGNS: LazyLock<Mutex<Vec<String>>> = LazyLock::new(Mutex::default);

/// The presign log. A panic in another test must not poison it.
fn presign_log() -> Vec<String> {
    PRESIGNS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

#[derive(serde::Deserialize)]
struct PresignRequest {
    filename: String,
    content_type: String,
    size: u64,
}

#[derive(serde::Serialize)]
struct PresignResponse {
    url: String,
    method: &'static str,
    headers: HashMap<String, String>,
    key: String,
}

fn page(script: bool, body: &Markup) -> Markup {
    page_with(script, false, body)
}

fn page_with(script: bool, htmx: bool, body: &Markup) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="csrf-token" content="tok-123";
                title { "Direct upload" }
                link rel="stylesheet" href="/static/css/autumn-widgets.css";
                @if htmx {
                    script src="/static/js/htmx.min.js" defer {}
                }
                @if script {
                    script src="/static/js/autumn-widgets.js" defer {}
                }
            }
            body { main { h1 { "Direct upload" } (body) } }
        }
    }
}

fn form(action: &str, field: &Markup) -> Markup {
    html! {
        form id="f" method="post" action=(action) enctype="multipart/form-data" {
            (field)
            button type="submit" id="send" { "Save" }
        }
    }
}

#[get("/")]
async fn index() -> Markup {
    let field = direct_upload_field(
        "avatar",
        "/presign",
        &DirectUploadConfig::new()
            .label("Avatar")
            .accept("image/*,.txt"),
    );
    page(true, &form("/save", &field))
}

#[get("/multi")]
async fn multi() -> Markup {
    let field = direct_upload_field(
        "photos",
        "/presign",
        &DirectUploadConfig::new().label("Photos").multiple(true),
    );
    page(true, &form("/save", &field))
}

#[get("/images")]
async fn images() -> Markup {
    let field = direct_upload_field(
        "pic",
        "/presign",
        &DirectUploadConfig::new().label("Picture").accept("image/*"),
    );
    page(true, &form("/save", &field))
}

/// htmx sends this form: from buttons in it, from a button outside it with
/// `hx-include`, and from live validation on a text field.
#[get("/hx")]
async fn hx() -> Markup {
    let field = direct_upload_field(
        "avatar",
        "/presign",
        &DirectUploadConfig::new().label("Avatar"),
    );
    let send = ButtonConfig::new()
        .submit()
        .id("send")
        .attrs(&[("hx-post", "/save"), ("hx-target", "#out")]);
    let draft = ButtonConfig::new()
        .id("draft")
        .attrs(&[("hx-post", "/save"), ("hx-target", "#out")]);
    let preview = ButtonConfig::new()
        .id("preview")
        .attrs(&[("hx-get", "/preview"), ("hx-target", "#out")]);
    let outside = ButtonConfig::new().id("outside").attrs(&[
        ("hx-post", "/save"),
        ("hx-include", "#f"),
        ("hx-target", "#out"),
    ]);
    // htmx reads `<form, main/>` as one selector: a comma in `<.../>` does
    // not split it.
    let outside_data = ButtonConfig::new().id("outside-data").attrs(&[
        ("hx-post", "/save"),
        ("data-hx-include", "closest <form, main/>"),
        ("hx-target", "#out"),
    ]);
    // htmx keywords: the next sibling (the form), and the whole page.
    let sibling = ButtonConfig::new().id("sibling").attrs(&[
        ("hx-post", "/save?sibling"),
        ("hx-include", "nextElementSibling"),
        ("hx-target", "#out"),
    ]);
    let page_wide = ButtonConfig::new().id("page-wide").attrs(&[
        ("hx-post", "/save?page"),
        ("hx-include", "global body"),
        ("hx-target", "#out"),
    ]);
    let lone = ButtonConfig::new()
        .id("lone")
        .attrs(&[("hx-post", "/lone"), ("hx-target", "#out")]);
    let body = html! {
        (button("Save with the next form", &sibling))
        form id="f" method="post" action="/save" {
            (field)
            label for="title" { "Title" }
            input id="title" name="title" hx-post="/check" hx-trigger="keyup" hx-target="#check";
            (button("Save", &send))
            (button("Save draft", &draft))
            (button("Preview", &preview))
        }
        (button("Save from outside", &outside))
        (button("Save the page", &outside_data))
        (button("Ping", &lone))
        (button("Save everything", &page_wide))
        div id="check" {}
        div id="out" {}
    };
    page_with(true, true, &body)
}

/// An autosave form. It sends on each change and on each new key.
#[get("/hx-auto")]
async fn hx_auto() -> Markup {
    let field = direct_upload_field(
        "avatar",
        "/presign",
        &DirectUploadConfig::new().label("Avatar"),
    );
    let body = html! {
        form id="f" method="post" action="/save" hx-post="/save"
            hx-trigger="change, autumn:upload" hx-target="#out" {
            (field)
        }
        div id="out" {}
    };
    page_with(true, true, &body)
}

#[post("/lone")]
async fn lone() -> &'static str {
    "lone"
}

#[post("/check")]
async fn check() -> &'static str {
    "checked"
}

#[get("/preview")]
async fn preview() -> &'static str {
    "preview"
}

/// The same form, with no script: the no-JS fallback.
#[get("/nojs")]
async fn nojs() -> Markup {
    let field = direct_upload_field(
        "avatar",
        "/presign",
        &DirectUploadConfig::new().label("Avatar"),
    );
    page(false, &form("/save", &field))
}

#[post("/presign")]
async fn presign(
    headers: axum::http::HeaderMap,
    Json(req): Json<PresignRequest>,
) -> Result<Json<PresignResponse>, (axum::http::StatusCode, &'static str)> {
    let token = headers
        .get("x-csrf-token")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    PRESIGNS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(format!(
            "{}|{}|{}|{token}",
            req.filename, req.content_type, req.size
        ));
    if req.filename.starts_with("fail") {
        return Err((axum::http::StatusCode::INTERNAL_SERVER_ERROR, "no"));
    }
    let key = format!("up-{}", req.filename);
    Ok(Json(PresignResponse {
        url: format!("/mem/{key}?ct={}", req.content_type.replace('/', "%2F")),
        method: "PUT",
        headers: HashMap::new(),
        key,
    }))
}

#[put("/mem/{key}")]
async fn put_mem(
    Path(key): Path<String>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> axum::http::StatusCode {
    if key.contains("slow") {
        tokio::time::sleep(Duration::from_millis(1500)).await;
    }
    let content_type = headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let token = headers
        .get("x-csrf-token")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    PUT_TOKENS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(key.clone(), token);
    STORE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(key, (content_type, body.to_vec()));
    axum::http::StatusCode::OK
}

/// Echo what the form sent.
#[post("/save")]
async fn save(headers: axum::http::HeaderMap, body: axum::body::Bytes) -> String {
    let content_type = headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    format!("[{content_type}] {}", String::from_utf8_lossy(&body))
}

async fn runner() -> SystemTestRunner {
    Box::pin(
        SystemTest::new()
            .routes(routes![
                index, multi, images, hx, hx_auto, lone, check, preview, nojs, presign, put_mem,
                save
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

async fn open(page: &Page, path: &str) {
    page.visit(path).await.expect("visit");
    wait_for(page, "document.readyState === 'complete'").await;
}

async fn open_with_script(page: &Page, path: &str) {
    open(page, path).await;
    wait_for(page, "window.__autumnWidgets === true").await;
}

const ROOT: &str = "[data-autumn-direct-upload]";

/// The JS to build a `File` list in `DataTransfer` `dt`.
fn files_js(files: &[(&str, &str, &str)]) -> String {
    let items: Vec<String> = files
        .iter()
        .map(|(name, kind, body)| format!("new File([{body:?}], {name:?}, {{ type: {kind:?} }})"))
        .collect();
    format!(
        "const dt = new DataTransfer(); [{}].forEach(f => dt.items.add(f));",
        items.join(",")
    )
}

/// Choose files in the input, as a file picker does.
async fn choose(page: &Page, files: &[(&str, &str, &str)]) {
    let js = format!(
        "(() => {{ {} const i = document.querySelector('{ROOT} input[type=file]'); \
         i.files = dt.files; i.dispatchEvent(new Event('change', {{ bubbles: true }})); \
         return true; }})()",
        files_js(files)
    );
    assert!(eval_bool(page, &js).await, "choose files");
}

/// Drop files on the widget.
async fn drop_files(page: &Page, files: &[(&str, &str, &str)]) {
    let js = format!(
        "(() => {{ {} const r = document.querySelector('{ROOT}'); \
         for (const t of ['dragenter', 'dragover', 'drop']) {{ \
           r.dispatchEvent(new DragEvent(t, {{ bubbles: true, cancelable: true, dataTransfer: dt }})); }} \
         return true; }})()",
        files_js(files)
    );
    assert!(eval_bool(page, &js).await, "drop files");
}

async fn hidden_values(page: &Page) -> String {
    eval_string(
        page,
        &format!(
            "[...document.querySelectorAll('{ROOT} input[type=hidden]')] \
             .map(i => i.name + '=' + i.value).join(',')"
        ),
    )
    .await
}

async fn text_of(page: &Page, selector: &str) -> String {
    eval_string(
        page,
        &format!(
            "(document.querySelector('{ROOT} {selector}') || {{ textContent: '' }}).textContent.trim()"
        ),
    )
    .await
}

fn stored(key: &str) -> Option<Stored> {
    STORE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(key)
        .cloned()
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn choosing_a_file_uploads_it_to_storage_and_adds_a_hidden_field() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open_with_script(&page, "/").await;
    choose(&page, &[("a-one.txt", "text/plain", "hello storage")]).await;
    wait_for(
        &page,
        &format!("!!document.querySelector('{ROOT} input[type=hidden]')"),
    )
    .await;
    assert_eq!(hidden_values(&page).await, "avatar=up-a-one.txt");
    let (kind, bytes) = stored("up-a-one.txt").expect("the bytes reached storage");
    assert_eq!(kind, "text/plain");
    assert_eq!(bytes, b"hello storage");
    // The presign call sent the file facts and the CSRF token.
    let log = presign_log().join(" ");
    assert!(log.contains("a-one.txt|text/plain|13|tok-123"), "{log}");
    assert!(text_of(&page, "[role=status]").await.contains("a-one.txt"));
    page.expect_no_console_errors().await.expect("no errors");
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn the_file_input_loses_its_name_so_the_bytes_are_not_sent_twice() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open_with_script(&page, "/").await;
    assert!(
        eval_bool(
            &page,
            &format!("!document.querySelector('{ROOT} input[type=file]').hasAttribute('name')")
        )
        .await
    );
    choose(&page, &[("a-two.txt", "text/plain", "two")]).await;
    wait_for(
        &page,
        &format!("!!document.querySelector('{ROOT} input[type=hidden]')"),
    )
    .await;
    page.click("#send").await.expect("submit");
    wait_for(&page, "document.body.textContent.includes('up-a-two.txt')").await;
    let body = eval_string(&page, "document.body.textContent").await;
    assert!(body.contains("avatar"), "{body}");
    assert!(!body.contains("filename="), "no file part: {body}");
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn progress_reaches_one_hundred_and_the_bar_hides_when_done() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open_with_script(&page, "/").await;
    // Record every `aria-valuenow` the bar gets.
    assert!(
        eval_bool(
            &page,
            &format!(
                "(() => {{ window.__seen = []; const bar = document.querySelector('{ROOT} [role=progressbar]'); \
                 new MutationObserver(() => window.__seen.push(bar.getAttribute('aria-valuenow'))) \
                 .observe(bar, {{ attributes: true, attributeFilter: ['aria-valuenow'] }}); return true; }})()"
            )
        )
        .await
    );
    choose(
        &page,
        &[("a-bar.txt", "text/plain", "x".repeat(200_000).as_str())],
    )
    .await;
    wait_for(&page, "window.__seen.includes('100')").await;
    wait_for(
        &page,
        &format!("document.querySelector('{ROOT} [role=progressbar]').hidden"),
    )
    .await;
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn dropping_a_file_uploads_it() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open_with_script(&page, "/").await;
    drop_files(&page, &[("a-drop.txt", "text/plain", "dropped")]).await;
    wait_for(
        &page,
        &format!("!!document.querySelector('{ROOT} input[type=hidden]')"),
    )
    .await;
    assert_eq!(hidden_values(&page).await, "avatar=up-a-drop.txt");
    assert_eq!(stored("up-a-drop.txt").expect("stored").1, b"dropped");
    // The drag state ends with the drop.
    assert!(
        eval_bool(
            &page,
            &format!("!document.querySelector('{ROOT}').hasAttribute('data-dragging')")
        )
        .await
    );
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn a_drag_over_marks_the_dropzone() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open_with_script(&page, "/").await;
    assert!(
        eval_bool(
            &page,
            &format!(
                "(() => {{ const r = document.querySelector('{ROOT}'); \
                 r.dispatchEvent(new DragEvent('dragover', {{ bubbles: true, cancelable: true, dataTransfer: (() => {{ const d = new DataTransfer(); d.items.add(new File(['x'], 'x.txt')); return d; }})() }})); \
                 const on = r.hasAttribute('data-dragging'); \
                 r.dispatchEvent(new DragEvent('dragleave', {{ bubbles: true }})); \
                 return on && !r.hasAttribute('data-dragging'); }})()"
            )
        )
        .await
    );
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn a_dropped_file_that_does_not_match_accept_is_refused() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open_with_script(&page, "/images").await;
    drop_files(&page, &[("b-doc.pdf", "application/pdf", "pdf")]).await;
    wait_for(
        &page,
        &format!("document.querySelector('{ROOT} [role=alert]').textContent.trim() !== ''"),
    )
    .await;
    let log = presign_log().join(" ");
    assert!(!log.contains("b-doc.pdf"), "no presign call: {log}");
    assert_eq!(hidden_values(&page).await, "");
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn a_presign_error_shows_an_alert_and_adds_no_field() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open_with_script(&page, "/").await;
    choose(&page, &[("fail.txt", "text/plain", "no")]).await;
    wait_for(
        &page,
        &format!("document.querySelector('{ROOT} [role=alert]').textContent.trim() !== ''"),
    )
    .await;
    assert_eq!(hidden_values(&page).await, "");
    assert!(
        eval_bool(
            &page,
            &format!("document.querySelector('{ROOT} [role=progressbar]').hidden")
        )
        .await,
        "the bar hides after an error"
    );
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn a_success_clears_an_earlier_error() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open_with_script(&page, "/").await;
    choose(&page, &[("fail.txt", "text/plain", "no")]).await;
    wait_for(
        &page,
        &format!("document.querySelector('{ROOT} [role=alert]').textContent.trim() !== ''"),
    )
    .await;
    choose(&page, &[("c-ok.txt", "text/plain", "ok")]).await;
    wait_for(
        &page,
        &format!("!!document.querySelector('{ROOT} input[type=hidden]')"),
    )
    .await;
    assert_eq!(text_of(&page, "[role=alert]").await, "");
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn submit_waits_for_a_running_upload() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open_with_script(&page, "/").await;
    choose(&page, &[("slow.txt", "text/plain", "slow")]).await;
    wait_for(
        &page,
        &format!("document.querySelector('{ROOT}').hasAttribute('aria-busy')"),
    )
    .await;
    page.click("#send").await.expect("submit");
    assert!(
        eval_bool(&page, "location.pathname === '/'").await,
        "submit was blocked"
    );
    assert!(
        !text_of(&page, "[role=alert]").await.is_empty(),
        "the alert says why"
    );
    // After the upload the form submits.
    wait_for(
        &page,
        &format!("!!document.querySelector('{ROOT} input[type=hidden]')"),
    )
    .await;
    assert!(
        eval_bool(
            &page,
            &format!("!document.querySelector('{ROOT}').hasAttribute('aria-busy')")
        )
        .await
    );
    page.click("#send").await.expect("submit");
    wait_for(&page, "document.body.textContent.includes('up-slow.txt')").await;
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn without_multiple_a_new_upload_replaces_the_field() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open_with_script(&page, "/").await;
    choose(&page, &[("d-first.txt", "text/plain", "1")]).await;
    wait_for(
        &page,
        &format!("!!document.querySelector('{ROOT} input[type=hidden]')"),
    )
    .await;
    choose(&page, &[("d-second.txt", "text/plain", "2")]).await;
    wait_for(
        &page,
        &format!(
            "document.querySelector('{ROOT} input[type=hidden]')?.value === 'up-d-second.txt'"
        ),
    )
    .await;
    assert_eq!(hidden_values(&page).await, "avatar=up-d-second.txt");
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn multiple_adds_one_field_for_each_blob() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open_with_script(&page, "/multi").await;
    choose(
        &page,
        &[
            ("e-1.txt", "text/plain", "1"),
            ("e-2.txt", "text/plain", "2"),
        ],
    )
    .await;
    wait_for(
        &page,
        &format!("document.querySelectorAll('{ROOT} input[type=hidden]').length === 2"),
    )
    .await;
    let values = hidden_values(&page).await;
    assert!(values.contains("photos=up-e-1.txt"), "{values}");
    assert!(values.contains("photos=up-e-2.txt"), "{values}");
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn without_the_script_the_form_submits_the_file_as_multipart() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open(&page, "/nojs").await;
    assert!(eval_bool(&page, "window.__autumnWidgets !== true").await);
    assert!(
        eval_bool(
            &page,
            &format!(
                "(() => {{ {} const i = document.querySelector('input[type=file]'); \
                 i.files = dt.files; document.getElementById('f').requestSubmit(); return true; }})()",
                files_js(&[("plain.txt", "text/plain", "no script bytes")])
            )
        )
        .await
    );
    wait_for(
        &page,
        "document.body.textContent.includes('no script bytes')",
    )
    .await;
    let body = eval_string(&page, "document.body.textContent").await;
    assert!(body.contains("multipart/form-data"), "{body}");
    assert!(body.contains("filename=\"plain.txt\""), "{body}");
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn the_widget_passes_axe_in_every_state() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open_with_script(&page, "/").await;
    let axe = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/axe-core/axe.min.js"),
    )
    .expect("axe source");
    let idle = page.axe_violations(&axe).await.expect("axe");
    assert!(idle.is_empty(), "idle: {idle:?}");
    choose(&page, &[("fail.txt", "text/plain", "no")]).await;
    wait_for(
        &page,
        &format!("document.querySelector('{ROOT} [role=alert]').textContent.trim() !== ''"),
    )
    .await;
    let failed = page.axe_violations(&axe).await.expect("axe");
    assert!(failed.is_empty(), "error: {failed:?}");
    choose(&page, &[("f-ok.txt", "text/plain", "ok")]).await;
    wait_for(
        &page,
        &format!("!!document.querySelector('{ROOT} input[type=hidden]')"),
    )
    .await;
    let done = page.axe_violations(&axe).await.expect("axe");
    assert!(done.is_empty(), "done: {done:?}");
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn an_early_run_does_not_end_the_state_of_a_later_run() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open_with_script(&page, "/multi").await;
    choose(&page, &[("g-fast.txt", "text/plain", "1")]).await;
    wait_for(
        &page,
        &format!("!!document.querySelector('{ROOT} input[type=hidden]')"),
    )
    .await;
    // A slow run starts. A fast run starts and ends while it runs.
    choose(&page, &[("slow-g.txt", "text/plain", "s")]).await;
    choose(&page, &[("g-quick.txt", "text/plain", "q")]).await;
    wait_for(
        &page,
        &format!("document.querySelectorAll('{ROOT} input[type=hidden]').length === 2"),
    )
    .await;
    assert!(
        eval_bool(
            &page,
            &format!(
                "document.querySelector('{ROOT}').hasAttribute('aria-busy') && \
                 !document.querySelector('{ROOT} [role=progressbar]').hidden"
            )
        )
        .await,
        "the slow run still shows its state"
    );
    wait_for(
        &page,
        &format!("!document.querySelector('{ROOT}').hasAttribute('aria-busy')"),
    )
    .await;
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn a_file_with_no_reported_type_passes_a_mime_accept() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open_with_script(&page, "/images").await;
    choose(&page, &[("h-notype.png", "", "bytes")]).await;
    wait_for(
        &page,
        &format!("!!document.querySelector('{ROOT} input[type=hidden]')"),
    )
    .await;
    assert_eq!(hidden_values(&page).await, "pic=up-h-notype.png");
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn a_refused_file_can_be_chosen_again() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open_with_script(&page, "/images").await;
    choose(&page, &[("i-doc.pdf", "application/pdf", "pdf")]).await;
    wait_for(
        &page,
        &format!("document.querySelector('{ROOT} [role=alert]').textContent.trim() !== ''"),
    )
    .await;
    assert!(
        eval_bool(
            &page,
            &format!("document.querySelector('{ROOT} input[type=file]').value === ''")
        )
        .await,
        "the input is clear, so the same file fires `change` again"
    );
    assert!(
        eval_bool(
            &page,
            &format!(
                "document.querySelector('{ROOT} input[type=file]').getAttribute('aria-invalid') === 'true'"
            )
        )
        .await
    );
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn a_slow_older_upload_does_not_replace_the_latest_choice() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open_with_script(&page, "/").await;
    // The first upload is slow. The second one ends first.
    choose(&page, &[("slow-j.txt", "text/plain", "old")]).await;
    choose(&page, &[("j-new.txt", "text/plain", "new")]).await;
    wait_for(
        &page,
        &format!("document.querySelector('{ROOT} input[type=hidden]')?.value === 'up-j-new.txt'"),
    )
    .await;
    // Wait until the slow upload has ended too.
    wait_for(
        &page,
        &format!("!document.querySelector('{ROOT}').hasAttribute('aria-busy')"),
    )
    .await;
    assert_eq!(hidden_values(&page).await, "avatar=up-j-new.txt");
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn a_same_origin_upload_carries_the_csrf_token() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open_with_script(&page, "/").await;
    choose(&page, &[("k-csrf.txt", "text/plain", "x")]).await;
    wait_for(
        &page,
        &format!("!!document.querySelector('{ROOT} input[type=hidden]')"),
    )
    .await;
    let token = PUT_TOKENS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get("up-k-csrf.txt")
        .cloned();
    assert_eq!(token.as_deref(), Some("tok-123"));
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn a_failed_replacement_leaves_no_old_key() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open_with_script(&page, "/").await;
    choose(&page, &[("l-old.txt", "text/plain", "old")]).await;
    wait_for(
        &page,
        &format!("!!document.querySelector('{ROOT} input[type=hidden]')"),
    )
    .await;
    choose(&page, &[("fail.txt", "text/plain", "no")]).await;
    wait_for(
        &page,
        &format!("document.querySelector('{ROOT} [role=alert]').textContent.trim() !== ''"),
    )
    .await;
    assert_eq!(hidden_values(&page).await, "");
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn a_form_reset_drops_the_keys_and_stops_a_running_upload() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open_with_script(&page, "/multi").await;
    choose(&page, &[("m-done.txt", "text/plain", "1")]).await;
    wait_for(
        &page,
        &format!("!!document.querySelector('{ROOT} input[type=hidden]')"),
    )
    .await;
    // A slow upload runs while the form resets.
    choose(&page, &[("slow-m.txt", "text/plain", "s")]).await;
    assert!(
        eval_bool(
            &page,
            "(() => { document.getElementById('f').reset(); return true; })()"
        )
        .await
    );
    assert_eq!(hidden_values(&page).await, "", "the reset drops the key");
    wait_for(
        &page,
        &format!("!document.querySelector('{ROOT}').hasAttribute('aria-busy')"),
    )
    .await;
    assert_eq!(
        hidden_values(&page).await,
        "",
        "the old upload adds no key after the reset"
    );
}

/// Open `/hx` and record each htmx request.
async fn open_hx(page: &Page) {
    open_hx_at(page, "/hx").await;
}

/// Open `path` and record each htmx request. `asked` gets each request that
/// htmx starts. `sent` gets only the requests that it sends.
async fn open_hx_at(page: &Page, path: &str) {
    open_with_script(page, path).await;
    wait_for(page, "!!window.htmx").await;
    assert!(
        eval_bool(
            page,
            "(() => { window.__asked = []; window.__sent = []; \
             window.addEventListener('htmx:beforeRequest', e => \
               window.__asked.push(e.detail.requestConfig.path), true); \
             document.addEventListener('htmx:beforeSend', e => \
               window.__sent.push(e.detail.requestConfig.path)); \
             return true; })()"
        )
        .await,
        "record the htmx requests"
    );
}

async fn text_by_id(page: &Page, id: &str) -> String {
    eval_string(
        page,
        &format!("document.getElementById('{id}').textContent.trim()"),
    )
    .await
}

/// Start a slow upload on `/hx` and wait until it runs.
async fn start_slow_upload(page: &Page, name: &str) {
    choose(page, &[(name, "text/plain", "slow")]).await;
    wait_for(
        page,
        &format!("document.querySelector('{ROOT}').hasAttribute('aria-busy')"),
    )
    .await;
}

/// htmx started a request to `path`, and the guard held it.
async fn assert_held(page: &Page, path: &str) {
    wait_for(page, &format!("window.__asked.includes('{path}')")).await;
    assert!(
        eval_bool(page, &format!("!window.__sent.includes('{path}')")).await,
        "htmx sent {path}"
    );
    assert_eq!(text_by_id(page, "out").await, "", "no swap");
    assert!(eval_bool(page, "!location.pathname.startsWith('/save')").await);
}

/// The guard showed the alert and moved focus to the file input.
async fn assert_told(page: &Page) {
    assert_eq!(
        text_of(page, "[role=alert]").await,
        "Wait for the upload to finish, then send the form again."
    );
    assert!(
        eval_bool(
            page,
            &format!(
                "document.activeElement === document.querySelector('{ROOT} input[type=file]')"
            )
        )
        .await,
        "focus is on the file input"
    );
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn an_htmx_submit_button_waits_for_a_running_upload() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open_hx(&page).await;
    start_slow_upload(&page, "slow-n1.txt").await;
    page.click("#send").await.expect("click");
    assert_held(&page, "/save").await;
    assert_told(&page).await;
    // After the upload the button sends the key.
    wait_for(
        &page,
        &format!("!!document.querySelector('{ROOT} input[type=hidden]')"),
    )
    .await;
    page.click("#send").await.expect("click");
    wait_for(
        &page,
        "document.getElementById('out').textContent.includes('up-slow-n1.txt')",
    )
    .await;
    page.expect_no_console_errors().await.expect("no errors");
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn an_htmx_button_with_type_button_waits_too() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open_hx(&page).await;
    start_slow_upload(&page, "slow-n2.txt").await;
    page.click("#draft").await.expect("click");
    assert_held(&page, "/save").await;
    assert_told(&page).await;
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn a_button_outside_the_form_with_hx_include_waits_too() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open_hx(&page).await;
    start_slow_upload(&page, "slow-n3.txt").await;
    page.click("#outside").await.expect("click");
    assert_held(&page, "/save").await;
    assert_told(&page).await;
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn live_validation_waits_with_no_message_and_keeps_focus() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open_hx(&page).await;
    start_slow_upload(&page, "slow-n4.txt").await;
    assert!(
        eval_bool(
            &page,
            "(() => { document.getElementById('title').focus(); return true; })()"
        )
        .await
    );
    page.press("g").await.expect("type");
    assert_held(&page, "/check").await;
    assert_eq!(text_by_id(&page, "check").await, "");
    assert_eq!(text_of(&page, "[role=alert]").await, "", "no message");
    assert!(
        eval_bool(&page, "document.activeElement.id === 'title'").await,
        "focus stays in the text field"
    );
    // After the upload, htmx sends the next request: the guard released it.
    wait_for(
        &page,
        &format!("!document.querySelector('{ROOT}').hasAttribute('aria-busy')"),
    )
    .await;
    page.press("h").await.expect("type");
    wait_for(
        &page,
        "document.getElementById('check').textContent === 'checked'",
    )
    .await;
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn an_htmx_get_button_does_not_wait() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open_hx(&page).await;
    start_slow_upload(&page, "slow-n5.txt").await;
    page.click("#preview").await.expect("click");
    wait_for(&page, "window.__sent.includes('/preview')").await;
    assert!(
        eval_bool(
            &page,
            &format!("document.querySelector('{ROOT}').hasAttribute('aria-busy')")
        )
        .await,
        "the upload still ran when htmx sent the request"
    );
    wait_for(
        &page,
        "document.getElementById('out').textContent === 'preview'",
    )
    .await;
    assert_eq!(text_of(&page, "[role=alert]").await, "");
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn an_hx_include_with_an_extended_selector_waits_too() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open_hx(&page).await;
    start_slow_upload(&page, "slow-n6.txt").await;
    page.click("#outside-data").await.expect("click");
    assert_held(&page, "/save").await;
    assert_told(&page).await;
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn a_request_that_does_not_send_the_form_does_not_wait() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open_hx(&page).await;
    start_slow_upload(&page, "slow-n7.txt").await;
    page.click("#lone").await.expect("click");
    wait_for(&page, "window.__sent.includes('/lone')").await;
    assert!(
        eval_bool(
            &page,
            &format!("document.querySelector('{ROOT}').hasAttribute('aria-busy')")
        )
        .await,
        "the upload still ran when htmx sent the request"
    );
    assert_eq!(text_of(&page, "[role=alert]").await, "");
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn an_autosave_form_waits_on_change_and_sends_the_key_on_autumn_upload() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open_hx_at(&page, "/hx-auto").await;
    // The `change` of the file input starts the upload and an autosave.
    start_slow_upload(&page, "slow-n8.txt").await;
    assert_held(&page, "/save").await;
    assert_eq!(text_of(&page, "[role=alert]").await, "", "no message");
    // `autumn:upload` sends the form with the new key.
    wait_for(
        &page,
        "document.getElementById('out').textContent.includes('up-slow-n8.txt')",
    )
    .await;
}

#[tokio::test]
#[ignore = "requires Chromium"]
async fn hx_include_keywords_wait_too() {
    let runner = runner().await;
    let page = runner.page().await.expect("page");
    open_hx(&page).await;
    start_slow_upload(&page, "slow-n9.txt").await;
    page.click("#sibling").await.expect("click");
    assert_held(&page, "/save?sibling").await;
    assert_told(&page).await;
    page.click("#page-wide").await.expect("click");
    assert_held(&page, "/save?page").await;
}
