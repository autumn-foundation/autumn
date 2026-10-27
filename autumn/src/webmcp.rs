//! `WebMCP`: route tools for the agent in the user's browser (issue #2619).
//!
//! `/mcp` serves agents on the other side of the network. `WebMCP` serves the
//! agent in the user's own tab. Autumn projects the same route registry for
//! both:
//!
//! * [`AppBuilder::mount_webmcp`](crate::app::AppBuilder::mount_webmcp)
//!   serves the tool list at [`TOOLS_PATH`].
//! * The framework serves the page script at [`SHIM_PATH`]. It registers
//!   each tool with `document.modelContext.registerTool()`.
//!
//! A tool call is a same-origin `fetch` to the route, with the session cookie
//! and the CSRF token of the page. So `#[secured]`, policies, tenancy,
//! validation and rate limits apply as for any request from the page.
//!
//! A route is a tool only with `#[api_doc(webmcp)]` (or `Route::webmcp`, or
//! the `GET`-only hatch
//! [`expose_all_as_webmcp`](crate::app::AppBuilder::expose_all_as_webmcp)).
//! `#[api_doc(mcp)]` does not imply it: a tool for a server agent is not
//! always safe for an agent in the browser of the user.
//!
//! The `WebMCP` API is a W3C community-group draft, not a standard. The shim
//! tracks the draft and can change in a minor release. See `STABILITY.md`.
//!
//! See `docs/guide/webmcp.md`.

#![cfg(feature = "webmcp")]

use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::http::{HeaderValue, Method, Request, StatusCode, header};
use axum::response::Response;
use serde_json::{Value, json};

use crate::mcp::McpToolInfo;
use crate::openapi::{ApiDoc, OpenApiConfig};

/// Path of the tool list.
pub const TOOLS_PATH: &str = "/_autumn/webmcp/tools.json";

/// Plain path of the page script. Prefer
/// `asset_url("js/autumn-webmcp.js")`, which gives the hashed path.
pub const SHIM_PATH: &str = "/static/js/autumn-webmcp.js";

/// The page script. It tracks the `WebMCP` draft of 2026-04-23.
pub const SHIM_JS: &str = include_str!("../vendor/autumn-webmcp.js");

/// Version of the [`TOOLS_PATH`] document.
pub const MANIFEST_VERSION: u32 = 1;

/// `WebMCP` settings from the [`AppBuilder`](crate::app::AppBuilder).
#[derive(Debug, Clone, Copy, Default)]
pub struct WebMcpRuntime {
    /// Make each eligible `GET` a tool, as `expose_all_as_webmcp` asks.
    pub expose_all: bool,
}

/// Derive the `WebMCP` tools from the route docs.
///
/// This calls [`mcp::derive_tools`](crate::mcp::derive_tools) with the
/// `WebMCP` flags in the place of the `mcp` flags. So the eligibility rule,
/// the tool names and each `inputSchema` are the ones `/mcp` uses. Pass all
/// the docs, as for `/mcp`: the shared `$defs` names come from the full set.
///
/// A stream tool has no `WebMCP` form, so it is not a tool here. A task tool
/// gives a normal result.
#[must_use]
pub fn derive_tools(
    docs: &[ApiDoc],
    expose_all: bool,
    openapi: Option<&OpenApiConfig>,
) -> Vec<McpToolInfo> {
    let projected: Vec<ApiDoc> = docs
        .iter()
        .map(|doc| {
            if doc.mcp_stream && doc.webmcp_tool {
                tracing::warn!(
                    operation_id = doc.operation_id,
                    "skipping WebMCP exposure: a stream tool has no WebMCP form"
                );
            }
            // A stream route is excluded here, before the name dedupe, so
            // it cannot take the name of a route that is a tool.
            ApiDoc {
                mcp_tool: doc.webmcp_tool && !doc.mcp_stream,
                mcp_exclude: doc.webmcp_exclude || doc.mcp_stream,
                mcp_task: false,
                mcp_core: false,
                ..doc.clone()
            }
        })
        .collect();
    crate::mcp::derive_tools(&projected, expose_all, openapi)
}

/// `true` when the tool asks the user before it sends: the derived MCP
/// annotations say `destructiveHint`. A `DELETE`, or a grant that is not
/// `reversible`, sets it.
#[must_use]
pub fn needs_confirmation(tool: &McpToolInfo) -> bool {
    tool.annotations()
        .get("destructiveHint")
        .and_then(Value::as_bool)
        == Some(true)
}

/// The [`TOOLS_PATH`] document for `tools`.
#[must_use]
pub fn manifest_json(tools: &[McpToolInfo], csrf_header: &str) -> Value {
    let tools: Vec<Value> = tools
        .iter()
        .map(|tool| {
            json!({
                "name": tool.name(),
                "description": tool.description().unwrap_or_else(|| tool.name()),
                "inputSchema": tool.input_schema(),
                "annotations": tool.annotations(),
                "webmcp_annotations": crate::aeo::webmcp::webmcp_annotations(tool.annotations()),
                "method": tool.method(),
                "path_template": tool.path_template(),
                "path_params": tool.path_params(),
                "has_query": tool.has_query(),
                "has_body": tool.has_body(),
                "confirm": needs_confirmation(tool),
            })
        })
        .collect();
    json!({
        "version": MANIFEST_VERSION,
        "csrfHeader": csrf_header,
        "tools": tools,
    })
}

/// The tool list, built once when the router is built. Kept as an
/// [`AppState`](crate::state::AppState) extension.
#[derive(Debug, Clone)]
pub(crate) struct WebMcpManifest {
    body: Bytes,
    names: Vec<String>,
}

impl WebMcpManifest {
    pub(crate) fn new(tools: &[McpToolInfo], csrf_header: &str) -> Self {
        let body = serde_json::to_vec(&manifest_json(tools, csrf_header)).unwrap_or_default();
        Self {
            body: Bytes::from(body),
            names: tools.iter().map(|tool| tool.name().to_owned()).collect(),
        }
    }

    /// The names of the tools in the list.
    pub(crate) fn names(&self) -> &[String] {
        &self.names
    }
}

/// Derive the tools from `docs` and keep the list in `state`, where the
/// router fallback finds it.
pub(crate) fn install(
    state: &crate::state::AppState,
    docs: &[ApiDoc],
    runtime: WebMcpRuntime,
    openapi: Option<&OpenApiConfig>,
    config: &crate::config::AutumnConfig,
) {
    let tools = derive_tools(docs, runtime.expose_all, openapi);
    state.insert_extension(WebMcpManifest::new(
        &tools,
        &config.security.csrf.token_header.to_ascii_lowercase(),
    ));
}

/// Keep an empty tool list in `state` when no list is there. With the
/// `webmcp` feature on, a page with only form tools then gets no `404` (and
/// no console error) for the list.
pub(crate) fn install_empty(state: &crate::state::AppState, config: &crate::config::AutumnConfig) {
    if state.extension::<WebMcpManifest>().is_none() {
        state.insert_extension(WebMcpManifest::new(
            &[],
            &config.security.csrf.token_header.to_ascii_lowercase(),
        ));
    }
}

/// The response for `req` when it asks for the tool list, else `None`.
///
/// The router fallback calls this, so a user route at [`TOOLS_PATH`] wins.
pub(crate) fn serve_manifest(
    manifest: Option<Arc<WebMcpManifest>>,
    req: &Request<Body>,
) -> Option<Response> {
    let manifest = manifest?;
    let method = req.method();
    if req.uri().path() != TOOLS_PATH || (method != Method::GET && method != Method::HEAD) {
        return None;
    }
    let body = if method == Method::HEAD {
        Body::empty()
    } else {
        Body::from(manifest.body.clone())
    };
    let mut res = Response::new(body);
    *res.status_mut() = StatusCode::OK;
    let headers = res.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    // Each user gets the same list, but a session layer can add a cookie.
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-cache"),
    );
    headers.insert("x-robots-tag", HeaderValue::from_static("noindex"));
    Some(res)
}

/// `<head>` tags for the `WebMCP` shim.
///
/// It writes the CSRF token as `<meta name="csrf-token">` (the htmx CSRF
/// helper reads it too) and the script at its hashed URL. No nonce is
/// necessary: the default CSP allows `script-src 'self'`.
///
/// Pass the request's CSRF token (the
/// [`CsrfToken`](crate::security::CsrfToken) extractor). Without it, a write
/// tool fails the CSRF check.
///
/// ```rust,ignore
/// html! { head { (autumn_web::webmcp::head_tags(csrf.as_ref().map(CsrfToken::token))) } }
/// ```
#[cfg(feature = "maud")]
#[must_use]
pub fn head_tags(csrf_token: Option<&str>) -> maud::Markup {
    let src = crate::assets::asset_url("js/autumn-webmcp.js");
    maud::html! {
        @if let Some(token) = csrf_token {
            meta name="csrf-token" content=(token);
        }
        script src=(src) defer {}
    }
}

/// A hidden marker that makes its `<form>` a `WebMCP` tool.
///
/// Put it inside the form. The shim reads the form fields to make the
/// `inputSchema`, and `execute` fills the fields and submits the form. An
/// htmx form gives the response text as the result.
///
/// A `DELETE` form asks the user first. Use [`form_tool_confirm`] to ask
/// for another form.
///
/// ```rust,ignore
/// form.form_tag("/todos", "post", html! {
///     (autumn_web::webmcp::form_tool("add_todo", "Add an item to the todo list"))
///     input name="title" required;
/// })
/// ```
#[cfg(feature = "maud")]
#[must_use]
pub fn form_tool(name: &str, description: &str) -> maud::Markup {
    maud::html! {
        span hidden data-webmcp-tool=(name) data-webmcp-description=(description) {}
    }
}

/// [`form_tool`] for a form that asks the user before each call. Use it when
/// the form does something that the user cannot undo.
#[cfg(feature = "maud")]
#[must_use]
pub fn form_tool_confirm(name: &str, description: &str) -> maud::Markup {
    maud::html! {
        span hidden data-webmcp-tool=(name) data-webmcp-description=(description)
            data-webmcp-confirm {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::openapi::{SchemaEntry, SchemaKind};

    fn doc(method: &'static str, path: &'static str, op: &'static str) -> ApiDoc {
        ApiDoc {
            method,
            path,
            operation_id: op,
            success_status: 200,
            response: Some(SchemaEntry {
                name: "Todo",
                kind: SchemaKind::Ref,
                identity: None,
            }),
            ..Default::default()
        }
    }

    fn names(tools: &[McpToolInfo]) -> Vec<&str> {
        tools.iter().map(McpToolInfo::name).collect()
    }

    #[test]
    fn only_webmcp_docs_are_tools() {
        let mut both = doc("POST", "/a", "both");
        both.mcp_tool = true;
        both.webmcp_tool = true;
        let mut mcp_only = doc("GET", "/b", "mcp_only");
        mcp_only.mcp_tool = true;
        let mut web = doc("DELETE", "/c/{id}", "web");
        web.webmcp_tool = true;
        web.path_params = &["id"];
        let tools = derive_tools(&[both, mcp_only, web], false, None);
        assert_eq!(names(&tools), ["both", "web"]);
    }

    #[test]
    fn exclusion_wins_over_the_hatch_and_the_opt_in() {
        let mut excluded = doc("GET", "/a", "excluded");
        excluded.webmcp_tool = true;
        excluded.webmcp_exclude = true;
        let read = doc("GET", "/b", "read");
        let write = doc("POST", "/c", "write");
        let mut mcp_excluded = doc("GET", "/d", "mcp_excluded");
        mcp_excluded.mcp_exclude = true;
        let tools = derive_tools(&[excluded, read, write, mcp_excluded], true, None);
        assert_eq!(
            names(&tools),
            ["read", "mcp_excluded"],
            "the hatch takes reads only, and `mcp = false` is not `webmcp = false`"
        );
    }

    #[test]
    fn a_stream_route_does_not_take_the_name_of_a_tool() {
        let mut stream = doc("GET", "/s", "op");
        stream.webmcp_tool = true;
        stream.mcp_stream = true;
        stream.response = None;
        let mut plain = doc("GET", "/p", "op");
        plain.webmcp_tool = true;
        let tools = derive_tools(&[stream, plain], false, None);
        assert_eq!(names(&tools), ["op"]);
        assert_eq!(tools[0].path_template(), "/p");
    }

    #[test]
    fn a_stream_tool_is_not_a_webmcp_tool() {
        let mut stream = doc("GET", "/s", "stream");
        stream.webmcp_tool = true;
        stream.mcp_stream = true;
        stream.response = None;
        assert!(derive_tools(&[stream], false, None).is_empty());
    }

    #[test]
    fn a_html_route_is_not_a_tool() {
        let mut page = doc("GET", "/p", "page");
        page.webmcp_tool = true;
        page.response = None;
        assert!(derive_tools(&[page], false, None).is_empty());
    }

    #[test]
    fn the_schema_and_annotations_match_mcp() {
        let mut d = doc("PUT", "/items/{id}", "put_item");
        d.mcp_tool = true;
        d.webmcp_tool = true;
        d.path_params = &["id"];
        let mcp = crate::mcp::derive_tools(std::slice::from_ref(&d), false, None);
        let web = derive_tools(&[d], false, None);
        assert_eq!(mcp[0].input_schema(), web[0].input_schema());
        assert_eq!(mcp[0].annotations(), web[0].annotations());
    }

    #[test]
    fn a_delete_needs_confirmation_and_a_get_does_not() {
        let mut del = doc("DELETE", "/i/{id}", "del");
        del.webmcp_tool = true;
        del.path_params = &["id"];
        let mut get = doc("GET", "/i", "get");
        get.webmcp_tool = true;
        let mut post = doc("POST", "/i", "post");
        post.webmcp_tool = true;
        let tools = derive_tools(&[del, get, post], false, None);
        let confirm: Vec<bool> = tools.iter().map(needs_confirmation).collect();
        assert_eq!(confirm, [true, false, false]);
    }

    static IRREVERSIBLE_GRANT: crate::agent_authority::Grant = crate::agent_authority::Grant {
        name: "PayoutSender",
        writes: &[],
        unbounded_writes: &[],
        tenant_scope: crate::agent_authority::TenantScope::Scoped,
        outbound: &[],
        webhooks: &[],
        jobs: &[],
        rate: None,
        spend: None,
        reversibility: crate::agent_authority::Reversibility::Irreversible,
        location: "autumn/src/webmcp.rs",
    };
    static IRREVERSIBLE: crate::agent_authority::AgentAuthority =
        crate::agent_authority::AgentAuthority {
            action: "send_payout",
            module_path: "autumn_web::webmcp::tests",
            location: "autumn/src/webmcp.rs",
            grant: &IRREVERSIBLE_GRANT,
            effects: &[],
            asserted_effect_free_sites: 0,
            asserted_effect_free: &[],
        };

    #[test]
    fn an_irreversible_grant_needs_confirmation() {
        let mut d = doc("POST", "/payouts", "send_payout");
        d.webmcp_tool = true;
        d.agent_authority = Some(&IRREVERSIBLE);
        let tools = derive_tools(&[d], false, None);
        assert!(needs_confirmation(&tools[0]));
        let m = manifest_json(&tools, "x-csrf-token");
        assert_eq!(m["tools"][0]["confirm"], true);
        assert_eq!(
            m["tools"][0]["webmcp_annotations"]["consequentialHint"],
            true
        );
    }

    #[test]
    fn the_manifest_has_the_shape_the_shim_reads() {
        let mut d = doc("GET", "/files/{*rest}", "read_file");
        d.webmcp_tool = true;
        d.path_params = &["*rest"];
        d.summary = Some("Read a file");
        let m = manifest_json(&derive_tools(&[d], false, None), "x-token");
        assert_eq!(m["version"], MANIFEST_VERSION);
        assert_eq!(m["csrfHeader"], "x-token");
        let t = &m["tools"][0];
        assert_eq!(t["name"], "read_file");
        assert_eq!(t["description"], "Read a file");
        assert_eq!(t["method"], "GET");
        assert_eq!(t["path_template"], "/files/{*rest}");
        assert_eq!(t["path_params"], json!(["*rest"]));
        assert_eq!(t["has_query"], false);
        assert_eq!(t["has_body"], false);
        assert_eq!(t["confirm"], false);
        assert_eq!(t["inputSchema"]["properties"]["rest"]["type"], "string");
    }

    #[test]
    fn a_tool_with_no_text_is_described_by_its_name() {
        let mut d = doc("GET", "/x", "bare");
        d.webmcp_tool = true;
        let m = manifest_json(&derive_tools(&[d], false, None), "x");
        assert_eq!(m["tools"][0]["description"], "bare");
    }

    #[test]
    fn route_builders_set_the_flags() {
        let route = crate::Route {
            method: http::Method::GET,
            path: "/x",
            handler: axum::routing::get(|| async { "ok" }),
            name: "x",
            api_doc: ApiDoc::default(),
            repository: None,
            idempotency: crate::route::RouteIdempotency::Direct,
            timeout: crate::route::RouteTimeout::Inherit,
            criticality: crate::admission::Criticality::Default,
            seo: crate::seo::SeoRouteDefaults::EMPTY,
            api_version: None,
            sunset_opt_out: false,
        };
        let on = route.webmcp();
        assert!(on.api_doc.webmcp_tool && !on.api_doc.mcp_tool);
        let off = on.webmcp_exclude();
        assert!(off.api_doc.webmcp_exclude);
    }

    fn get(path: &str, method: Method) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(path)
            .body(Body::empty())
            .unwrap()
    }

    #[test]
    fn serve_answers_get_and_head_at_the_path_only() {
        let manifest = Arc::new(WebMcpManifest::new(&[], "x-csrf-token"));
        let res = serve_manifest(Some(manifest.clone()), &get(TOOLS_PATH, Method::GET)).unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert!(serve_manifest(Some(manifest.clone()), &get(TOOLS_PATH, Method::HEAD)).is_some());
        assert!(serve_manifest(Some(manifest.clone()), &get(TOOLS_PATH, Method::POST)).is_none());
        assert!(serve_manifest(Some(manifest), &get("/other", Method::GET)).is_none());
        assert!(serve_manifest(None, &get(TOOLS_PATH, Method::GET)).is_none());
    }

    #[test]
    fn the_manifest_keeps_the_tool_names() {
        let mut d = doc("GET", "/x", "x_tool");
        d.webmcp_tool = true;
        let manifest = WebMcpManifest::new(&derive_tools(&[d], false, None), "x");
        assert_eq!(manifest.names(), ["x_tool"]);
    }

    #[cfg(feature = "maud")]
    #[test]
    fn form_tool_renders_an_escaped_hidden_marker() {
        let html = form_tool("add_todo", "Add <one> todo").into_string();
        assert_eq!(
            html,
            "<span hidden data-webmcp-tool=\"add_todo\" \
             data-webmcp-description=\"Add &lt;one&gt; todo\"></span>"
        );
    }

    #[cfg(feature = "maud")]
    #[test]
    fn head_tags_render_the_token_and_the_hashed_script() {
        let html = head_tags(Some("t<1")).into_string();
        assert!(
            html.contains("<meta name=\"csrf-token\" content=\"t&lt;1\">"),
            "{html}"
        );
        let src = crate::assets::asset_url("js/autumn-webmcp.js");
        assert!(src.starts_with("/static/js/autumn-webmcp."), "{src}");
        assert!(
            html.contains(&format!("<script src=\"{src}\" defer>")),
            "{html}"
        );
        let html = head_tags(None).into_string();
        assert!(!html.contains("csrf-token"), "{html}");
    }

    #[cfg(feature = "maud")]
    #[test]
    fn form_tool_confirm_renders_the_confirm_flag() {
        let html = form_tool_confirm("drop", "Drop it").into_string();
        assert!(html.contains("data-webmcp-tool=\"drop\""), "{html}");
        assert!(html.contains("data-webmcp-confirm"), "{html}");
        assert!(!form_tool("drop", "x").into_string().contains("confirm"));
    }

    #[test]
    fn the_shim_tracks_both_spellings_of_model_context() {
        assert!(SHIM_JS.contains("document.modelContext"));
        assert!(SHIM_JS.contains("navigator.modelContext"));
        assert!(SHIM_JS.contains(TOOLS_PATH));
    }
}
