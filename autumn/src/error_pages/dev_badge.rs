//! Next.js-style dev error badge overlay.
//!
//! In dev mode, error responses get a floating badge injected into the HTML
//! that shows error details: status code, error type, message, stack trace,
//! source context, request info, and SQL queries. The badge uses **inline CSS
//! only** (no Tailwind) so it works even when the CSS build pipeline is broken.
//!
//! The badge is **never** injected in production.

use maud::{Markup, PreEscaped, html};

/// A single frame in the error's stack trace.
#[derive(Debug, Clone)]
pub struct StackFrame {
    /// Source file path (relative to workspace root when in workspace).
    pub file: String,
    /// 1-based line number within the file.
    pub line: u32,
    /// Fully qualified Rust function name.
    pub function: String,
    /// Source lines surrounding the failing line (~10 lines context).
    pub source_context: Vec<SourceLine>,
    /// Whether this frame is inside the project workspace (not stdlib/registry).
    pub is_in_workspace: bool,
}

/// A single line of source code in a stack frame's context.
#[derive(Debug, Clone)]
pub struct SourceLine {
    /// 1-based line number in the file.
    pub line_no: u32,
    /// Content of the line (no trailing newline).
    pub content: String,
    /// Whether this is the exact line where the error occurred.
    pub is_highlighted: bool,
}

/// An SQL query executed during the failing request.
///
/// Populated by the autumn-harvest query instrumentation when it is in the
/// dependency graph. Empty by default so the overlay degrades gracefully.
#[derive(Debug, Clone)]
pub struct SqlQueryInfo {
    /// The SQL statement text.
    pub statement: String,
    /// Number of bind parameters used.
    pub bind_count: usize,
    /// Query duration in milliseconds.
    pub duration_ms: f64,
}

/// Context for the dev error badge.
#[derive(Debug, Clone)]
pub struct DevBadgeContext {
    /// HTTP status code.
    pub status_code: u16,
    /// Status reason phrase (e.g., "Not Found").
    pub status_reason: String,
    /// Error message.
    pub message: String,
    /// Request path.
    pub path: String,
    /// Request ID.
    pub request_id: Option<String>,
    /// Optional file/line info if available (legacy field; prefer `stack_frames`).
    pub source_location: Option<String>,
    /// Optional query string.
    pub query: Option<String>,
    /// Scrubbed request headers.
    pub headers: serde_json::Value,
    // ── Extended fields (issue #811) ───────────────────────────────────────
    /// HTTP method (e.g. "GET", "POST").
    pub method: Option<String>,
    /// Matched route pattern (e.g. `/posts/:id`), when available.
    pub route_pattern: Option<String>,
    /// Parsed path parameters, scrubbed (e.g. `{"id": "42"}`).
    pub path_params: serde_json::Value,
    /// Scrubbed session cookies.
    pub cookies: serde_json::Value,
    /// Parsed stack frames with optional source context.
    pub stack_frames: Vec<StackFrame>,
    /// SQL queries executed during this request (from harvest instrumentation).
    pub sql_queries: Vec<SqlQueryInfo>,
    /// Parsed and scrubbed request body (JSON or form). `None` for GET or when
    /// body capture is disabled (production). `Some(Value::String(...))` is used
    /// to convey a truncation notice when the raw body exceeded the size cap.
    pub body_preview: Option<serde_json::Value>,
}

impl Default for DevBadgeContext {
    fn default() -> Self {
        Self {
            status_code: 500,
            status_reason: "Internal Server Error".into(),
            message: String::new(),
            path: String::new(),
            request_id: None,
            source_location: None,
            query: None,
            headers: serde_json::json!({}),
            method: None,
            route_pattern: None,
            path_params: serde_json::json!({}),
            cookies: serde_json::json!({}),
            stack_frames: Vec::new(),
            sql_queries: Vec::new(),
            body_preview: None,
        }
    }
}

/// Generate the dev error badge HTML snippet.
///
/// This returns a self-contained HTML fragment with inline CSS and no
/// JavaScript, so it works under Autumn's default `script-src 'self'` CSP.
/// It should be injected just before `</body>` in HTML error responses.
///
/// Uses inline CSS (not Tailwind) so it works even if the CSS build fails.
/// Styled like Next.js/Phoenix: dark overlay, monospace font, red accent,
/// expandable stack frames with source context.
#[allow(clippy::too_many_lines)]
fn render_stack_frames(frames: &[StackFrame]) -> Markup {
    html! {
                    // ── Stack trace with source context ───────────────
                    @if !frames.is_empty() {
                        div class="autumn-dev-overlay-section" {
                            div class="autumn-dev-overlay-label" {
                                "Stack Trace (" (frames.len()) " frames)"
                            }
                            div class="autumn-dev-stack-frames" {
                                @for (i, frame) in frames.iter().enumerate() {
                                    @let frame_id = format!("autumn-dev-frame-{i}");
                                    @let is_primary = i == 0 && frame.is_in_workspace;
                                    @if frame.is_in_workspace && !frame.source_context.is_empty() {
                                        div class=(if is_primary { "autumn-dev-frame autumn-dev-frame-primary" } else { "autumn-dev-frame" }) {
                                            input id=(frame_id) type="checkbox" class="autumn-dev-toggle" checked[is_primary];
                                            label for=(frame_id) class="autumn-dev-frame-header autumn-dev-frame-expandable" {
                                                span class="autumn-dev-frame-arrow" { "▶" }
                                                span class="autumn-dev-frame-file" { (frame.file) }
                                                ":" (frame.line)
                                                " "
                                                span class="autumn-dev-frame-fn" { (frame.function) }
                                            }
                                            div class="autumn-dev-frame-source" {
                                                @for line in &frame.source_context {
                                                    div class=(if line.is_highlighted {
                                                        "autumn-dev-source-line autumn-dev-source-line-highlight"
                                                    } else {
                                                        "autumn-dev-source-line"
                                                    }) {
                                                        span class="autumn-dev-source-lineno" { (line.line_no) }
                                                        span class="autumn-dev-source-content" { (line.content) }
                                                    }
                                                }
                                            }
                                        }
                                    } @else {
                                        div class="autumn-dev-frame autumn-dev-frame-external" {
                                            div class="autumn-dev-frame-header" {
                                                span class="autumn-dev-frame-arrow autumn-dev-frame-arrow-dim" { "·" }
                                                @if frame.file.is_empty() {
                                                    span class="autumn-dev-frame-fn-ext" { (frame.function) }
                                                } @else {
                                                    span class="autumn-dev-frame-file-ext" { (frame.file) }
                                                    ":" (frame.line) " "
                                                    span class="autumn-dev-frame-fn-ext" { (frame.function) }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }

    }
}

fn render_request_details(ctx: &DevBadgeContext) -> Markup {
    let path = &ctx.path;
    let request_id = ctx.request_id.as_deref().unwrap_or("n/a");
    let source_loc = ctx.source_location.as_deref().unwrap_or("");
    let query = ctx.query.as_deref().unwrap_or("n/a");
    let headers_str = ctx.headers.to_string();
    let has_path_params = !ctx
        .path_params
        .as_object()
        .is_none_or(serde_json::Map::is_empty);
    let has_cookies = !ctx
        .cookies
        .as_object()
        .is_none_or(serde_json::Map::is_empty);

    html! {
                    // ── Request section ───────────────────────────────
                    @if let Some(method) = &ctx.method {
                        div class="autumn-dev-overlay-section" {
                            div class="autumn-dev-overlay-label" { "Method" }
                            div class="autumn-dev-overlay-value autumn-dev-mono" { (method) }
                        }
                    }
                    div class="autumn-dev-overlay-section" {
                        div class="autumn-dev-overlay-label" { "Path" }
                        div class="autumn-dev-overlay-value autumn-dev-mono" { (path) }
                    }
                    @if let Some(pattern) = &ctx.route_pattern {
                        div class="autumn-dev-overlay-section" {
                            div class="autumn-dev-overlay-label" { "Route" }
                            div class="autumn-dev-overlay-value autumn-dev-mono" { (pattern) }
                        }
                    }
                    div class="autumn-dev-overlay-section" {
                        div class="autumn-dev-overlay-label" { "Request ID" }
                        div class="autumn-dev-overlay-value autumn-dev-mono" { (request_id) }
                    }
                    div class="autumn-dev-overlay-section" {
                        div class="autumn-dev-overlay-label" { "Query" }
                        div class="autumn-dev-overlay-value autumn-dev-mono" { (query) }
                    }
                    @if has_path_params {
                        div class="autumn-dev-overlay-section" {
                            div class="autumn-dev-overlay-label" { "Path Params" }
                            div class="autumn-dev-overlay-value autumn-dev-mono" { (ctx.path_params.to_string()) }
                        }
                    }
                    div class="autumn-dev-overlay-section" {
                        div class="autumn-dev-overlay-label" { "Headers" }
                        div class="autumn-dev-overlay-value autumn-dev-mono" { (headers_str) }
                    }
                    @if has_cookies {
                        div class="autumn-dev-overlay-section" {
                            div class="autumn-dev-overlay-label" { "Cookies" }
                            div class="autumn-dev-overlay-value autumn-dev-mono" { (ctx.cookies.to_string()) }
                        }
                    }
                    @if let Some(body) = &ctx.body_preview {
                        div class="autumn-dev-overlay-section" {
                            div class="autumn-dev-overlay-label" { "Body" }
                            div class="autumn-dev-overlay-value autumn-dev-mono" { (body.to_string()) }
                        }
                    }
                    @if !source_loc.is_empty() {
                        div class="autumn-dev-overlay-section" {
                            div class="autumn-dev-overlay-label" { "Source" }
                            div class="autumn-dev-overlay-value autumn-dev-mono" { (source_loc) }
                        }
                    }

    }
}

fn render_sql_queries(queries: &[SqlQueryInfo]) -> Markup {
    html! {
                    // ── SQL queries ───────────────────────────────────
                    @if !queries.is_empty() {
                        div class="autumn-dev-overlay-section" {
                            div class="autumn-dev-overlay-label" {
                                "SQL Queries (" (queries.len()) ")"
                            }
                            div class="autumn-dev-sql-list" {
                                @for query in queries {
                                    div class="autumn-dev-sql-item" {
                                        div class="autumn-dev-sql-stmt" { (query.statement) }
                                        div class="autumn-dev-sql-meta" {
                                            (query.bind_count) " bind(s) · "
                                            (format!("{:.2}", query.duration_ms)) "ms"
                                        }
                                    }
                                }
                            }
                        }
                    }
    }
}

pub fn dev_error_badge_html(ctx: &DevBadgeContext) -> Markup {
    let status = ctx.status_code;
    let reason = &ctx.status_reason;
    let message = &ctx.message;

    html! {
        (PreEscaped(DEV_BADGE_STYLES))
        div #autumn-dev-error-badge {
            input #autumn-dev-badge-toggle type="checkbox" class="autumn-dev-toggle";

            // Collapsed badge (always visible)
            label #autumn-dev-badge-collapsed
                for="autumn-dev-badge-toggle"
                tabindex="0"
            {
                span class="autumn-dev-badge-dot" {}
                span class="autumn-dev-badge-code" { (status) }
                span class="autumn-dev-badge-label" { (reason) }
            }

            // Expanded overlay
            div #autumn-dev-badge-expanded style="display:none" {
                div class="autumn-dev-overlay-header" {
                    div class="autumn-dev-overlay-title" {
                        span class="autumn-dev-badge-dot" {}
                        (status) " " (reason)
                    }
                    label class="autumn-dev-overlay-close"
                        for="autumn-dev-badge-toggle"
                        role="button"
                        aria-label="Close error details"
                    {
                        "\u{00d7}"
                    }
                }
                div class="autumn-dev-overlay-body" {

                    // ── Error message ─────────────────────────────────
                    div class="autumn-dev-overlay-section" {
                        div class="autumn-dev-overlay-label" { "Message" }
                        div class="autumn-dev-overlay-value" { (message) }
                    }

                    (render_stack_frames(&ctx.stack_frames))
                    (render_request_details(ctx))
                    (render_sql_queries(&ctx.sql_queries))
                }
            }
        }
    }
}

/// All inline CSS for the dev badge. Uses a unique prefix to avoid
/// colliding with application styles.
const DEV_BADGE_STYLES: &str = r#"<style>
#autumn-dev-error-badge {
    position: fixed;
    bottom: 16px;
    left: 16px;
    z-index: 99999;
    font-family: ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, "Liberation Mono", "Courier New", monospace;
    font-size: 13px;
    line-height: 1.5;
}
.autumn-dev-toggle {
    position: absolute;
    opacity: 0;
    pointer-events: none;
}
#autumn-dev-badge-toggle:not(:checked) ~ #autumn-dev-badge-expanded {
    display: none;
}
#autumn-dev-badge-toggle:checked ~ #autumn-dev-badge-collapsed {
    display: none;
}
#autumn-dev-badge-collapsed {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 8px 14px;
    background: #1a1a2e;
    border: 1px solid #e53e3e;
    border-radius: 8px;
    color: #e2e8f0;
    cursor: pointer;
    box-shadow: 0 4px 24px rgba(0,0,0,0.4);
    transition: background 0.15s;
    user-select: none;
}
#autumn-dev-badge-collapsed:hover {
    background: #2d2d4a;
}
.autumn-dev-badge-dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: #e53e3e;
    flex-shrink: 0;
}
.autumn-dev-badge-code {
    font-weight: 700;
    color: #fc8181;
}
.autumn-dev-badge-label {
    color: #a0aec0;
    font-size: 12px;
}
#autumn-dev-badge-expanded {
    display: flex;
    flex-direction: column;
    width: 620px;
    max-width: calc(100vw - 32px);
    max-height: calc(100vh - 100px);
    background: #1a1a2e;
    border: 1px solid #e53e3e;
    border-radius: 12px;
    color: #e2e8f0;
    box-shadow: 0 8px 32px rgba(0,0,0,0.6);
    overflow: hidden;
}
.autumn-dev-overlay-header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 12px 16px;
    background: #16162a;
    border-bottom: 1px solid #2d2d4a;
    flex-shrink: 0;
}
.autumn-dev-overlay-title {
    display: flex;
    align-items: center;
    gap: 8px;
    font-weight: 700;
    color: #fc8181;
    font-size: 14px;
}
.autumn-dev-overlay-close {
    background: none;
    border: none;
    color: #a0aec0;
    font-size: 20px;
    cursor: pointer;
    padding: 0 4px;
    line-height: 1;
}
.autumn-dev-overlay-close:hover {
    color: #e2e8f0;
}
.autumn-dev-overlay-body {
    padding: 16px;
    overflow-y: auto;
    flex: 1;
}
.autumn-dev-overlay-section {
    margin-bottom: 12px;
}
.autumn-dev-overlay-section:last-child {
    margin-bottom: 0;
}
.autumn-dev-overlay-label {
    font-size: 11px;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.05em;
    color: #718096;
    margin-bottom: 4px;
}
.autumn-dev-overlay-value {
    color: #e2e8f0;
    word-break: break-word;
}
.autumn-dev-mono {
    font-family: ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace;
    font-size: 12px;
    color: #a0aec0;
}
/* Stack frames */
.autumn-dev-stack-frames {
    display: flex;
    flex-direction: column;
    gap: 2px;
}
.autumn-dev-frame {
    border: 1px solid #2d2d4a;
    border-radius: 4px;
    overflow: hidden;
}
.autumn-dev-frame-primary {
    border-color: #e53e3e;
}
.autumn-dev-frame-external {
    opacity: 0.6;
}
.autumn-dev-frame-header {
    display: flex;
    align-items: center;
    gap: 6px;
    padding: 4px 8px;
    background: #16162a;
    font-size: 11px;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
}
.autumn-dev-frame-expandable {
    cursor: pointer;
    user-select: none;
}
.autumn-dev-frame-expandable:hover {
    background: #2d2d4a;
}
.autumn-dev-frame-arrow {
    color: #fc8181;
    font-size: 9px;
    flex-shrink: 0;
    transition: transform 0.1s;
}
.autumn-dev-frame-arrow-dim {
    color: #4a5568;
}
.autumn-dev-frame-file {
    color: #90cdf4;
    font-weight: 600;
}
.autumn-dev-frame-fn {
    color: #fbd38d;
    overflow: hidden;
    text-overflow: ellipsis;
}
.autumn-dev-frame-file-ext {
    color: #4a5568;
}
.autumn-dev-frame-fn-ext {
    color: #4a5568;
    overflow: hidden;
    text-overflow: ellipsis;
}
/* source lines inside a frame */
.autumn-dev-frame-source {
    background: #0d0d1a;
    padding: 4px 0;
    overflow-x: auto;
    font-size: 11px;
}
.autumn-dev-toggle:not(:checked) ~ .autumn-dev-frame-header ~ .autumn-dev-frame-source {
    display: none;
}
.autumn-dev-source-line {
    display: flex;
    align-items: stretch;
    padding: 1px 0;
}
.autumn-dev-source-line-highlight {
    background: rgba(229, 62, 62, 0.25);
    border-left: 3px solid #e53e3e;
}
.autumn-dev-source-lineno {
    color: #4a5568;
    min-width: 40px;
    text-align: right;
    padding: 0 8px;
    flex-shrink: 0;
    user-select: none;
}
.autumn-dev-source-content {
    color: #e2e8f0;
    white-space: pre;
    padding: 0 8px;
}
/* SQL queries */
.autumn-dev-sql-list {
    display: flex;
    flex-direction: column;
    gap: 6px;
}
.autumn-dev-sql-item {
    background: #0d0d1a;
    border: 1px solid #2d2d4a;
    border-radius: 4px;
    padding: 6px 8px;
}
.autumn-dev-sql-stmt {
    color: #90cdf4;
    font-size: 11px;
    white-space: pre-wrap;
    word-break: break-all;
}
.autumn-dev-sql-meta {
    color: #718096;
    font-size: 10px;
    margin-top: 3px;
}
</style>"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn test_ctx() -> DevBadgeContext {
        DevBadgeContext {
            status_code: 404,
            status_reason: "Not Found".into(),
            message: "page missing".into(),
            path: "/test".into(),
            request_id: Some("req-abc".into()),
            source_location: None,
            query: None,
            headers: serde_json::json!({}),
            method: None,
            route_pattern: None,
            path_params: serde_json::json!({}),
            cookies: serde_json::json!({}),
            stack_frames: Vec::new(),
            sql_queries: Vec::new(),
            body_preview: None,
        }
    }

    #[test]
    fn badge_contains_status_code() {
        let html = dev_error_badge_html(&test_ctx());
        let s = html.into_string();
        assert!(s.contains("404"));
        assert!(s.contains("Not Found"));
    }

    #[test]
    fn badge_contains_message() {
        let html = dev_error_badge_html(&test_ctx());
        let s = html.into_string();
        assert!(s.contains("page missing"));
    }

    #[test]
    fn badge_contains_request_id() {
        let html = dev_error_badge_html(&test_ctx());
        let s = html.into_string();
        assert!(s.contains("req-abc"));
    }

    #[test]
    fn badge_uses_inline_css() {
        let html = dev_error_badge_html(&test_ctx());
        let s = html.into_string();
        assert!(s.contains("<style>"), "badge must use inline CSS");
        assert!(
            s.contains("#autumn-dev-error-badge"),
            "badge uses namespaced CSS selectors"
        );
    }

    #[test]
    fn badge_does_not_use_inline_javascript_handlers() {
        let html = dev_error_badge_html(&test_ctx());
        let s = html.into_string();
        assert!(!s.contains("onclick="));
        assert!(!s.contains("<script"));
        assert!(s.contains("autumn-dev-badge-toggle"));
    }

    #[test]
    fn badge_shows_source_location_when_present() {
        let mut ctx = test_ctx();
        ctx.source_location = Some("src/main.rs:42".into());
        let html = dev_error_badge_html(&ctx);
        let s = html.into_string();
        assert!(s.contains("src/main.rs:42"));
    }

    #[test]
    fn badge_hides_source_section_when_absent() {
        let ctx = test_ctx();
        let html = dev_error_badge_html(&ctx);
        let s = html.into_string();
        // The label ">Source<" in the HTML body must not appear when no source_location is set.
        // CSS class names containing "source" (lowercase) are always present.
        assert!(
            !s.contains(">Source<"),
            "no source section label without location"
        );
    }

    // ── RED-phase tests: new fields and types ───────────────────────

    #[test]
    fn badge_renders_stack_frames_when_present() {
        let mut ctx = test_ctx();
        ctx.stack_frames = vec![StackFrame {
            file: "src/routes/posts.rs".into(),
            line: 42,
            function: "reddit_clone::routes::posts::create".into(),
            source_context: vec![
                SourceLine {
                    line_no: 41,
                    content: "    let user = get_user(&db);".into(),
                    is_highlighted: false,
                },
                SourceLine {
                    line_no: 42,
                    content: r#"    panic!("oops");"#.into(),
                    is_highlighted: true,
                },
                SourceLine {
                    line_no: 43,
                    content: "}".into(),
                    is_highlighted: false,
                },
            ],
            is_in_workspace: true,
        }];
        let html = dev_error_badge_html(&ctx);
        let s = html.into_string();
        assert!(s.contains("src/routes/posts.rs"), "should show file path");
        assert!(
            s.contains("reddit_clone::routes::posts::create"),
            "should show function"
        );
    }

    #[test]
    fn badge_highlights_failing_line_in_source_context() {
        let mut ctx = test_ctx();
        ctx.stack_frames = vec![StackFrame {
            file: "src/lib.rs".into(),
            line: 10,
            function: "my::func".into(),
            source_context: vec![
                SourceLine {
                    line_no: 9,
                    content: "fn foo() {".into(),
                    is_highlighted: false,
                },
                SourceLine {
                    line_no: 10,
                    content: "    panic!();".into(),
                    is_highlighted: true,
                },
                SourceLine {
                    line_no: 11,
                    content: "}".into(),
                    is_highlighted: false,
                },
            ],
            is_in_workspace: true,
        }];
        let html = dev_error_badge_html(&ctx);
        let s = html.into_string();
        assert!(
            s.contains("autumn-dev-source-line-highlight"),
            "highlighted line must have distinct CSS class"
        );
    }

    #[test]
    fn badge_renders_route_pattern_when_present() {
        let mut ctx = test_ctx();
        ctx.route_pattern = Some("/posts/:id".into());
        let html = dev_error_badge_html(&ctx);
        let s = html.into_string();
        assert!(s.contains("/posts/:id"), "should show route pattern");
        assert!(s.contains("Route"), "should have Route label");
    }

    #[test]
    fn badge_does_not_render_route_section_when_absent() {
        let ctx = test_ctx();
        let html = dev_error_badge_html(&ctx);
        let s = html.into_string();
        assert!(!s.contains(">Route<"), "no route section when absent");
    }

    #[test]
    fn badge_renders_path_params_when_present() {
        let mut ctx = test_ctx();
        ctx.path_params = serde_json::json!({"id": "42", "slug": "hello-world"});
        let html = dev_error_badge_html(&ctx);
        let s = html.into_string();
        assert!(
            s.contains("hello-world") || s.contains("slug"),
            "should show path params"
        );
        assert!(
            s.contains("Params") || s.contains("Path"),
            "should have params label"
        );
    }

    #[test]
    fn badge_renders_sql_queries_when_present() {
        let mut ctx = test_ctx();
        ctx.sql_queries = vec![SqlQueryInfo {
            statement: "SELECT * FROM posts WHERE id = $1".into(),
            bind_count: 1,
            duration_ms: 3.2,
        }];
        let html = dev_error_badge_html(&ctx);
        let s = html.into_string();
        assert!(
            s.contains("SELECT * FROM posts"),
            "should show SQL statement"
        );
        assert!(
            s.contains("SQL") || s.contains("Queries"),
            "should have SQL section label"
        );
    }

    #[test]
    fn badge_hides_sql_section_when_empty() {
        let ctx = test_ctx();
        let html = dev_error_badge_html(&ctx);
        let s = html.into_string();
        assert!(!s.contains("SQL Queries"), "no SQL section when no queries");
    }

    #[test]
    fn badge_renders_cookies_when_present() {
        let mut ctx = test_ctx();
        ctx.cookies = serde_json::json!({"session_id": "[FILTERED]", "theme": "dark"});
        let html = dev_error_badge_html(&ctx);
        let s = html.into_string();
        assert!(s.contains("Cookies"), "should have Cookies section");
        assert!(
            s.contains("theme"),
            "should show non-sensitive cookie names"
        );
    }

    #[test]
    fn badge_renders_method_when_present() {
        let mut ctx = test_ctx();
        ctx.method = Some("POST".into());
        let html = dev_error_badge_html(&ctx);
        let s = html.into_string();
        assert!(s.contains("POST"), "should show HTTP method");
    }

    #[test]
    fn badge_renders_multiple_sql_queries_with_duration() {
        let mut ctx = test_ctx();
        ctx.sql_queries = vec![
            SqlQueryInfo {
                statement: "SELECT * FROM users".into(),
                bind_count: 0,
                duration_ms: 1.5,
            },
            SqlQueryInfo {
                statement: "SELECT * FROM posts WHERE user_id = $1".into(),
                bind_count: 1,
                duration_ms: 2.3,
            },
        ];
        let html = dev_error_badge_html(&ctx);
        let s = html.into_string();
        assert!(s.contains("SELECT * FROM users"), "should show first query");
        assert!(
            s.contains("SELECT * FROM posts"),
            "should show second query"
        );
        assert!(
            s.contains("ms") || s.contains("1.50") || s.contains("2.30"),
            "should show duration"
        );
    }
}
