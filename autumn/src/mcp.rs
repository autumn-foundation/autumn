//! Project typed Autumn endpoints as [Model Context Protocol][mcp] tools.
//!
//! Autumn already builds a route registry of [`ApiDoc`](crate::openapi::ApiDoc)
//! metadata — handler name, summary/description, and the request-body /
//! `Query` / path-param JSON Schemas — the same data that feeds
//! [`openapi`](crate::openapi). This module *projects* that registry into an
//! MCP server the way `openapi` projects it into an OpenAPI document, so an
//! existing JSON API becomes agent-callable with near-zero new code.
//!
//! What you write:
//!
//! ```ignore
//! #[get("/api/todos")]
//! #[api_doc(mcp, summary = "List todos")]
//! async fn list_todos() -> AutumnResult<Json<Vec<Todo>>> { /* ... */ }
//!
//! autumn_web::app()
//!     .routes(routes![list_todos])
//!     .mount_mcp("/mcp")        // serves a Streamable-HTTP MCP endpoint
//!     .run().await;
//! ```
//!
//! Key properties (issue #1117):
//!
//! * **Opt-in per endpoint** via `#[api_doc(mcp)]`; nothing is exposed
//!   implicitly. A whole-API hatch ([`AppBuilder::expose_all_as_mcp`]) is an
//!   explicit, separate call and still requires opt-in for mutating verbs.
//! * **No second schema.** Each tool's `inputSchema` is derived from the
//!   handler's typed `ApiDoc`, so it cannot drift from the handler.
//! * **Real pipeline.** `tools/call` dispatches through the exact same router
//!   an HTTP request hits, so `#[secured]`, authorization, tenancy, rate
//!   limits, and validation all apply identically.
//! * **Bearer auth reuse.** Agents present an API token via the existing
//!   [`RequireApiToken`](crate::auth::RequireApiToken) surface; the
//!   `Authorization` header is forwarded into the dispatched call.
//!
//! Results are buffered by default. A tool tagged `#[api_doc(mcp, stream)]`
//! (issue #1118) returns an Autumn [`Sse`](crate::sse::Sse) stream that this
//! module projects onto the Streamable-HTTP SSE channel as
//! `notifications/progress` messages terminated by the final `tools/call`
//! result — see `serve_tools_call` / `stream_tool_result`. Streaming is
//! strictly opt-in per tool; the buffered path is unchanged.
//!
//! [`AppBuilder::expose_all_as_mcp`]: crate::app::AppBuilder::expose_all_as_mcp
//!
//! [mcp]: https://modelcontextprotocol.io

#![cfg(feature = "mcp")]

use std::collections::{HashMap, VecDeque};
use std::convert::Infallible;
use std::pin::Pin;
use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures::{Stream, StreamExt as _};
use serde_json::{Value, json};
use tower::ServiceExt as _;

use crate::sse::{Event, Sse};

use crate::openapi::{ApiDoc, schema_entry_to_value};

/// Protocol version advertised when a client requests an unsupported one (or
/// none). Also the newest version this server implements.
const DEFAULT_PROTOCOL_VERSION: &str = "2025-06-18";

/// MCP protocol revisions whose semantics this tools-only server honors
/// (results are buffered by default, with opt-in SSE streaming per tool — see
/// [`serve_tools_call`]). A client's requested version is echoed only if it
/// appears here;
/// otherwise the server replies with [`DEFAULT_PROTOCOL_VERSION`] and the
/// client decides whether it can proceed.
const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

/// MCP Streamable-HTTP transport headers a browser client attaches to its
/// JSON-RPC requests. They are always added to the `OPTIONS` preflight's
/// `Access-Control-Allow-Headers` (on top of the app's configured list) so a
/// default CORS config doesn't block the follow-up `POST`. See
/// <https://modelcontextprotocol.io/specification/2025-06-18/basic/transports#protocol-version-header>.
const MCP_REQUEST_HEADERS: &[&str] = &["mcp-protocol-version", "mcp-session-id"];

/// Upper bound on a tool's buffered response body (10 MiB). MCP tool results
/// are structured JSON; this guards the in-process dispatch path against a
/// handler that would otherwise buffer an unbounded body into memory.
const MAX_TOOL_RESPONSE_BYTES: usize = 10 * 1024 * 1024;

/// Request headers copied verbatim from the `POST /mcp` envelope onto the
/// in-process request a `tools/call` replays, so the dispatched call
/// authenticates, resolves its tenant, and is rate-limited/deduped exactly as
/// the equivalent direct HTTP request would. (The configured header-based
/// tenant header, whose name is dynamic, is forwarded separately.)
const FORWARDED_HEADERS: &[&str] = &[
    "authorization",
    "cookie",
    "idempotency-key",
    "host",
    "forwarded",
    "x-forwarded-for",
    "x-forwarded-host",
    "x-forwarded-proto",
    "x-real-ip",
    // Locale negotiation: the `Locale` extractor falls back to `Accept-Language`
    // when no locale query/cookie is present, so forward it for the tool result
    // to match the localized data a direct HTTP call would return.
    "accept-language",
];

/// Layer applier for the optional whole-endpoint auth gate (e.g.
/// `RequireApiToken`). Boxed so any `tower::Layer` can be erased; applied to
/// the `/mcp` router before it is merged.
pub(crate) type McpEndpointLayer = Box<
    dyn FnOnce(axum::Router<crate::state::AppState>) -> axum::Router<crate::state::AppState> + Send,
>;

/// Runtime MCP configuration carried from the [`AppBuilder`](crate::app::AppBuilder)
/// through router assembly.
pub struct McpRuntime {
    /// Path the Streamable-HTTP endpoint is mounted at (e.g. `/mcp`).
    pub mount_path: String,
    /// When `true`, every eligible `GET` route is exposed without a
    /// per-endpoint tag (the whole-API hatch). Mutating verbs still require
    /// an explicit `#[api_doc(mcp)]` opt-in, and `#[api_doc(mcp = false)]`
    /// exclusions are always honored.
    pub expose_all: bool,
    /// Optional layer applied to the *entire* `/mcp` endpoint — gating the
    /// catalog (`initialize`/`tools/list`) as well as tool dispatch. Set via
    /// [`AppBuilder::secure_mcp`](crate::app::AppBuilder::secure_mcp).
    pub(crate) endpoint_layer: Option<McpEndpointLayer>,
}

impl McpRuntime {
    /// Create a runtime config for a per-endpoint-opt-in MCP server.
    #[must_use]
    pub fn new(mount_path: impl Into<String>) -> Self {
        Self {
            mount_path: mount_path.into(),
            expose_all: false,
            endpoint_layer: None,
        }
    }
}

/// A single derived MCP tool plus the metadata needed to replay it as an
/// in-process HTTP request.
#[derive(Clone, Debug)]
struct McpTool {
    name: String,
    description: Option<String>,
    input_schema: Value,
    annotations: Value,
    // ── dispatch metadata ──
    method: String,
    /// Full route path with `{param}` placeholders.
    path_template: String,
    path_params: Vec<String>,
    has_body: bool,
    has_query: bool,
    /// True for a `#[api_doc(mcp, stream)]` tool whose handler returns an
    /// Autumn `Sse` stream, projected onto the Streamable-HTTP SSE channel.
    streams: bool,
}

impl McpTool {
    /// The JSON object advertised in `tools/list`.
    fn descriptor(&self) -> Value {
        let mut obj = serde_json::Map::new();
        obj.insert("name".into(), json!(self.name));
        if let Some(desc) = &self.description {
            obj.insert("description".into(), json!(desc));
        }
        obj.insert("inputSchema".into(), self.input_schema.clone());
        obj.insert("annotations".into(), self.annotations.clone());
        Value::Object(obj)
    }
}

/// Configuration threaded from router assembly into the MCP endpoint.
pub(crate) struct McpWiring {
    /// The app's CORS config: `allowed_origins` is the cross-origin `Origin`
    /// allowlist; the methods/headers/credentials/max-age fields answer this
    /// endpoint's own `OPTIONS` preflight (it is mounted outside the global
    /// CORS layer, so it must serve preflight for allowlisted browser clients).
    pub cors: crate::config::CorsConfig,
    /// The app's trusted-Host policy, gating the same-origin shortcut.
    pub trusted_hosts: crate::router::TrustedHostPolicy,
    /// Configured tenant header to forward (header-based tenancy), else `None`.
    pub tenant_header: Option<String>,
    /// Configured CSRF token header name (default `x-csrf-token`). Forwarded on
    /// dispatch so a session-authenticated caller passes `CsrfLayer`, which
    /// reads `CsrfConfig::token_header` — not a hard-coded name.
    pub csrf_header: String,
    /// Whether a [`RateLimitLayer`](crate::security::RateLimitLayer) wraps the
    /// `/mcp` envelope (true iff rate limiting is enabled). When set, a
    /// `tools/call` is counted once at the envelope, so its replayed dispatch is
    /// marked [`RateLimitExempt`](crate::security::RateLimitExempt) to avoid
    /// double-counting against the dispatch pipeline's own limiter.
    pub envelope_rate_limited: bool,
}

/// The shared MCP server state attached to the endpoint handler. Holds the
/// derived tool catalog and a clone of the fully-assembled application router
/// to dispatch `tools/call` against.
pub struct McpServer {
    tools: Vec<McpTool>,
    by_name: HashMap<String, usize>,
    /// The real application router (state already applied) — the same path an
    /// HTTP request traverses. `tools/call` replays requests through it.
    dispatch: axum::Router,
    /// The app's CORS config. `cors.allowed_origins` is the cross-origin
    /// `Origin` allowlist (DNS-rebinding protection, per the MCP
    /// Streamable-HTTP spec); a present `Origin` that is neither same-origin
    /// (trusted-host-gated) nor allowlisted is rejected with 403. The remaining
    /// fields answer the endpoint's own `OPTIONS` preflight.
    cors: crate::config::CorsConfig,
    /// The app's trusted-Host policy. The same-origin shortcut only fires when
    /// the request's Host is trusted by this policy, so a DNS-rebinding request
    /// (whose `Origin` and `Host` are both the attacker's domain) cannot bypass
    /// `Origin` validation by Host-match alone — it must still be an explicitly
    /// trusted host, exactly as normal routes require.
    trusted_hosts: crate::router::TrustedHostPolicy,
    /// The configured tenant header name (e.g. `x-tenant-id`) when the app uses
    /// header-based tenancy (`[tenancy] enabled = true, source = "header"`).
    /// `tools/call` forwards this header onto the dispatched request so the
    /// `Tenant` extractor resolves the same tenant a direct HTTP call would.
    /// `None` for any other tenancy source (which keys off headers Autumn
    /// already forwards — `Authorization` for JWT, `Cookie`/Host otherwise).
    tenant_header: Option<String>,
    /// The configured CSRF token header name forwarded on dispatch.
    csrf_header: String,
    /// Whether the `/mcp` envelope is rate-limited; gates exempting the
    /// replayed `tools/call` dispatch from the pipeline limiter.
    envelope_rate_limited: bool,
    server_name: String,
    server_version: String,
}

impl McpServer {
    /// Whether a browser `Origin` header value is permitted.
    ///
    /// A same-origin request — one whose `Origin` matches the request's own
    /// host (proxy/scheme-aware) **and** whose host is trusted by the app's
    /// trusted-Host policy — is always allowed: the CORS allowlist governs
    /// *cross*-origin access, and a browser MCP client served by this same
    /// Autumn app should not have to enable CORS for its own origin. The
    /// trusted-Host gate is essential: without it, a DNS-rebinding request
    /// (`Origin: http://attacker.example`, `Host: attacker.example`) would
    /// match by Host alone and defeat the very protection `Origin` validation
    /// exists to provide. Otherwise `*` in the allowlist permits any origin; an
    /// empty allowlist permits none (so any present cross-origin `Origin` is
    /// rejected).
    fn origin_allowed(&self, origin: &str, host: Option<&str>, scheme: Option<&str>) -> bool {
        if let Some(host) = host
            && is_same_origin(origin, host, scheme)
            && crate::router::extract_host_without_port(host)
                .is_some_and(|h| self.trusted_hosts.allows_host(&h.to_ascii_lowercase()))
        {
            return true;
        }
        self.cors
            .allowed_origins
            .iter()
            .any(|allowed| allowed == "*" || allowed == origin)
    }
}

/// Whether `origin` (an `Origin` header value like `https://app.example:8443`)
/// is the same origin as the request's own host.
///
/// The authority (`host[:port]`) must match exactly; when the request's own
/// scheme is known (resolved proxy-aware from `X-Forwarded-Proto`/URI), it must
/// match too. If the scheme is unknown we accept on the authority alone — the
/// host match is what matters for DNS-rebinding protection, and a stricter
/// scheme check would wrongly reject same-origin clients behind a
/// TLS-terminating proxy.
fn is_same_origin(origin: &str, host: &str, scheme: Option<&str>) -> bool {
    let Some((origin_scheme, origin_authority)) = origin.split_once("://") else {
        return false;
    };
    // When the request's own scheme is known, it must match the Origin's.
    if scheme.is_some_and(|s| !s.eq_ignore_ascii_case(origin_scheme)) {
        return false;
    }
    // Compare host + port with default-port normalization, so e.g.
    // `Host: app.example:443` (https) is the same origin as
    // `Origin: https://app.example`. When the request scheme is unknown we
    // assume the Origin's for the host's default-port resolution.
    let host_scheme = scheme.unwrap_or(origin_scheme);
    authority_matches(origin_authority, origin_scheme, host, host_scheme)
}

/// Compare two `host[:port]` authorities for origin equality, treating an
/// omitted port as the scheme's default (443 for https, 80 for http). The host
/// comparison is case-insensitive; IPv6 literals (`[::1]`) are handled.
fn authority_matches(a: &str, a_scheme: &str, b: &str, b_scheme: &str) -> bool {
    let (a_host, a_port) = split_host_port(a);
    let (b_host, b_port) = split_host_port(b);
    if !a_host.eq_ignore_ascii_case(b_host) {
        return false;
    }
    a_port.or_else(|| default_port(a_scheme)) == b_port.or_else(|| default_port(b_scheme))
}

/// Split an authority into its host and optional port. Bracketed IPv6 literals
/// keep their brackets in the host part; a trailing `:digits` is the port.
fn split_host_port(authority: &str) -> (&str, Option<&str>) {
    if authority.starts_with('[') {
        // IPv6: `[::1]` or `[::1]:8080`. The host is everything through `]`.
        if let Some(close) = authority.find(']') {
            let host = &authority[..=close];
            let port = authority[close + 1..]
                .strip_prefix(':')
                .filter(|p| !p.is_empty());
            return (host, port);
        }
        return (authority, None);
    }
    match authority.rsplit_once(':') {
        Some((host, port)) if !port.is_empty() && port.bytes().all(|c| c.is_ascii_digit()) => {
            (host, Some(port))
        }
        _ => (authority, None),
    }
}

/// The default TCP port for a URL scheme, used to normalize authorities.
fn default_port(scheme: &str) -> Option<&'static str> {
    match scheme.to_ascii_lowercase().as_str() {
        "https" => Some("443"),
        "http" => Some("80"),
        _ => None,
    }
}

/// Decide whether a route's `ApiDoc` should be projected as a tool.
///
/// Eligibility (JSON-out) gates everything: HTML/Maud routes have no response
/// schema and are silently ineligible. On top of that:
/// * `mcp_exclude` always wins.
/// * an explicit `mcp` opt-in always exposes (any verb).
/// * under the whole-API hatch, un-tagged `GET`s are auto-included but
///   mutating verbs are not.
fn should_expose(doc: &ApiDoc, expose_all: bool) -> bool {
    if doc.hidden || doc.mcp_exclude {
        return false;
    }
    // A streaming tool (`#[api_doc(mcp, stream)]`) returns an `Sse` body, so it
    // has no JSON response schema by nature. It is eligible purely on its
    // explicit opt-in (or the hatch, for a read-only verb), bypassing the
    // JSON-out gate below that would otherwise exclude every schema-less route.
    if doc.mcp_stream {
        if doc.mcp_tool {
            return true;
        }
        return expose_all && is_read_only(doc.method);
    }
    // JSON-out only: a response schema is the structural signal that this is a
    // JSON endpoint rather than an HTML/Maud route.
    //
    // Note this gates on the *response* shape only. The macro infers a request
    // body solely from `Json<T>`, so a route returning `Json<T>` but reading a
    // non-JSON body (`Form<T>`/multipart/`Bytes`/`String`) is indistinguishable
    // here from a legitimately body-less route — both leave `request_body`
    // unset. Such routes are a documented non-target for MCP exposure (see
    // `AppBuilder::mount_mcp`): opting one in yields a tool with no body input.
    // Exception: 204 No Content is a deliberate empty success contract (e.g.
    // the repository macro's generated DELETE), structurally distinct from an
    // HTML route's schema-less 200-with-body. It stays eligible.
    if doc.response.is_none() && doc.success_status != 204 {
        return false;
    }
    if doc.mcp_tool {
        return true;
    }
    if expose_all {
        return is_read_only(doc.method);
    }
    false
}

/// `GET` (and `HEAD`) are read-only; everything else mutates.
fn is_read_only(method: &str) -> bool {
    matches!(method.to_ascii_uppercase().as_str(), "GET" | "HEAD")
}

/// MCP safety annotations derived purely from the HTTP verb.
fn annotations_for(method: &str, title: &str) -> Value {
    let upper = method.to_ascii_uppercase();
    let read_only = is_read_only(&upper);
    let mut obj = serde_json::Map::new();
    obj.insert("title".into(), json!(title));
    obj.insert("readOnlyHint".into(), json!(read_only));
    // DELETE is the destructive verb; flag it so agents/UIs can warn.
    if upper == "DELETE" {
        obj.insert("destructiveHint".into(), json!(true));
    }
    Value::Object(obj)
}

/// Build the `inputSchema` for a tool from the handler's typed contract.
///
/// Path params become required string properties, the `Query<T>` extractor
/// becomes a `query` object property, and the JSON request body becomes a
/// `body` property. Named component refs are inlined into `$defs` so the
/// schema is self-contained.
fn build_input_schema(doc: &ApiDoc, components: &serde_json::Map<String, Value>) -> Value {
    let mut properties = serde_json::Map::new();
    let mut required: Vec<Value> = Vec::new();
    let mut defs = serde_json::Map::new();

    // Path params, the `Query<T>` extractor, and the JSON body share one flat
    // argument object, keyed by the param name for path params and by the
    // reserved keys `query`/`body` for the other two.
    //
    // KNOWN LIMITATION: a path param literally named `query` or `body` collides
    // with those reserved keys — the inserts below overwrite the path-param
    // property, and `build_request` then feeds the `query`/`body` value to the
    // path slot. Such routes (e.g. `/search/{query}` with a `Query<T>`) are
    // vanishingly rare; the tool they generate is unusable, but the collision is
    // left undisambiguated rather than reshaping the argument contract for every
    // path-param tool.
    for param in doc.path_params {
        // axum catch-all params (`{*rest}`) surface with a leading `*`; clients
        // address them by the bare name, so advertise the stripped name.
        let name = param.strip_prefix('*').unwrap_or(param);
        properties.insert(name.to_owned(), json!({ "type": "string" }));
        required.push(json!(name));
    }

    if let Some(query) = &doc.query_schema {
        let schema = rewrite_refs(schema_entry_to_value(query), components, &mut defs);
        properties.insert("query".to_owned(), schema);
    }

    if let Some(body) = &doc.request_body {
        let schema = rewrite_refs(schema_entry_to_value(body), components, &mut defs);
        properties.insert("body".to_owned(), schema);
        required.push(json!("body"));
    }

    let mut schema = serde_json::Map::new();
    schema.insert("type".into(), json!("object"));
    schema.insert("properties".into(), Value::Object(properties));
    if !required.is_empty() {
        schema.insert("required".into(), Value::Array(required));
    }
    if !defs.is_empty() {
        schema.insert("$defs".into(), Value::Object(defs));
    }
    Value::Object(schema)
}

/// Recursively rewrite `#/components/schemas/X` refs to local `#/$defs/X`
/// refs, pulling each referenced component (resolved from `components`) into
/// `defs` so the resulting schema stands alone.
fn rewrite_refs(
    value: Value,
    components: &serde_json::Map<String, Value>,
    defs: &mut serde_json::Map<String, Value>,
) -> Value {
    match value {
        Value::Object(map) => {
            if let Some(Value::String(reference)) = map.get("$ref")
                && let Some(name) = reference.strip_prefix("#/components/schemas/")
            {
                let name = name.to_owned();
                let local = format!("#/$defs/{name}");
                if !defs.contains_key(&name) {
                    // Insert a placeholder first to break ref cycles, then
                    // resolve the real component schema (if registered).
                    defs.insert(name.clone(), Value::Null);
                    let resolved = components
                        .get(&name)
                        .cloned()
                        .unwrap_or_else(|| json!({ "type": "object", "title": name.clone() }));
                    let resolved = rewrite_refs(resolved, components, defs);
                    defs.insert(name, resolved);
                }
                return json!({ "$ref": local });
            }
            let rewritten: serde_json::Map<String, Value> = map
                .into_iter()
                .map(|(k, v)| (k, rewrite_refs(v, components, defs)))
                .collect();
            Value::Object(rewritten)
        }
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(|v| rewrite_refs(v, components, defs))
                .collect(),
        ),
        other => other,
    }
}

/// Derive the tool catalog from collected route docs.
///
/// Emits a build-time `tracing::warn` for any endpoint that opts into MCP but
/// is ineligible (e.g. an HTML/Maud route with no JSON response schema), so it
/// is a logged note rather than a runtime surprise.
#[must_use]
pub fn derive_tools(
    docs: &[ApiDoc],
    expose_all: bool,
    openapi: Option<&crate::openapi::OpenApiConfig>,
) -> Vec<McpToolInfo> {
    // Reuse the OpenAPI generator to resolve component schemas exactly once,
    // so tool input schemas share the handler's typed contract. Crucially,
    // reuse the *app's* OpenApiConfig when present so component schemas the
    // user registered via `OpenApiConfig::register_schema` resolve identically
    // to the served OpenAPI document, instead of drifting to placeholders.
    let refs: Vec<&ApiDoc> = docs.iter().collect();
    let config = openapi.cloned().unwrap_or_else(|| {
        crate::openapi::OpenApiConfig::new("autumn-mcp", env!("CARGO_PKG_VERSION"))
    });
    let spec = crate::openapi::generate_spec(&config, &refs);
    let components = spec
        .components
        .as_ref()
        .map(|c| serde_json::to_value(&c.schemas).unwrap_or(Value::Null))
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();

    let mut tools = Vec::new();
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for doc in docs {
        // Surface the "opted in but ineligible" case as a build-time note.
        // Streaming tools legitimately have no JSON response schema, and a
        // 204 No Content route's empty body is its contract, so both are
        // exempt from this "missing response" warning/skip.
        if (doc.mcp_tool || (expose_all && is_read_only(doc.method)))
            && doc.response.is_none()
            && doc.success_status != 204
            && !doc.mcp_stream
            && !doc.mcp_exclude
            && !doc.hidden
        {
            tracing::warn!(
                operation_id = doc.operation_id,
                method = doc.method,
                path = doc.path,
                "skipping MCP exposure: endpoint has no JSON response schema \
                 (HTML/Maud routes are not eligible as MCP tools)"
            );
            continue;
        }
        if !should_expose(doc, expose_all) {
            continue;
        }
        // Tool names (operation ids) must be unique: the same handler mounted
        // under two scoped prefixes, or a reused explicit operation_id, would
        // otherwise advertise a duplicate that dispatch can't disambiguate.
        // Keep the first registration deterministically and warn on the rest.
        if !seen.insert(doc.operation_id) {
            tracing::warn!(
                operation_id = doc.operation_id,
                method = doc.method,
                path = doc.path,
                "duplicate MCP tool name; keeping the first registration and \
                 skipping this duplicate (set a distinct operation_id to expose both)"
            );
            continue;
        }
        let title = doc.summary.unwrap_or(doc.operation_id);
        tools.push(McpToolInfo {
            name: doc.operation_id.to_owned(),
            description: doc.description.or(doc.summary).map(str::to_owned),
            input_schema: build_input_schema(doc, &components),
            annotations: annotations_for(doc.method, title),
            method: doc.method.to_owned(),
            path_template: doc.path.to_owned(),
            path_params: doc.path_params.iter().map(|p| (*p).to_owned()).collect(),
            has_body: doc.request_body.is_some(),
            has_query: doc.query_schema.is_some(),
            streams: doc.mcp_stream,
        });
    }
    tools
}

/// Public, transport-agnostic description of a derived tool. Returned by
/// [`derive_tools`] and consumed by the framework when assembling the MCP
/// endpoint router.
#[derive(Clone, Debug)]
pub struct McpToolInfo {
    name: String,
    description: Option<String>,
    input_schema: Value,
    annotations: Value,
    method: String,
    path_template: String,
    path_params: Vec<String>,
    has_body: bool,
    has_query: bool,
    streams: bool,
}

impl McpServer {
    /// Assemble the server state from derived tools, a dispatch router, and the
    /// router-supplied [`McpWiring`] (CORS, trusted hosts, tenant/CSRF headers).
    #[must_use]
    pub(crate) fn new(tools: Vec<McpToolInfo>, dispatch: axum::Router, wiring: McpWiring) -> Self {
        let tools: Vec<McpTool> = tools
            .into_iter()
            .map(|t| McpTool {
                name: t.name,
                description: t.description,
                input_schema: t.input_schema,
                annotations: t.annotations,
                method: t.method,
                path_template: t.path_template,
                path_params: t.path_params,
                has_body: t.has_body,
                has_query: t.has_query,
                streams: t.streams,
            })
            .collect();
        let by_name = tools
            .iter()
            .enumerate()
            .map(|(i, t)| (t.name.clone(), i))
            .collect();
        Self {
            tools,
            by_name,
            dispatch,
            cors: wiring.cors,
            trusted_hosts: wiring.trusted_hosts,
            tenant_header: wiring.tenant_header,
            csrf_header: wiring.csrf_header,
            envelope_rate_limited: wiring.envelope_rate_limited,
            server_name: "autumn-mcp".to_owned(),
            server_version: env!("CARGO_PKG_VERSION").to_owned(),
        }
    }
}

/// Build an axum sub-router serving the MCP endpoint at `mount_path`.
///
/// `dispatch` must be the fully-assembled application router (state applied)
/// so `tools/call` traverses the real handler pipeline. `wiring` carries the
/// CORS config (cross-origin `Origin` allowlist + preflight settings), the
/// trusted-Host policy gating the same-origin shortcut, and the tenant/CSRF
/// header names forwarded on dispatch.
pub(crate) fn build_mcp_router(
    mount_path: &str,
    tools: Vec<McpToolInfo>,
    dispatch: axum::Router,
    wiring: McpWiring,
    endpoint_layer: Option<McpEndpointLayer>,
) -> axum::Router<crate::state::AppState> {
    let server = Arc::new(McpServer::new(tools, dispatch, wiring));
    tracing::debug!(
        path = mount_path,
        tools = server.tools.len(),
        "Mounted MCP endpoint"
    );
    // The JSON-RPC surface (GET probe + POST) carries the optional whole-endpoint
    // auth gate (`secure_mcp`). `OPTIONS` is deliberately mounted on a *separate*
    // sub-router so the auth layer never wraps it: a CORS preflight is sent
    // unauthenticated by the browser, so gating it would 401 the preflight and
    // the real POST would never fire. Disjoint methods on the same path merge
    // into one `MethodRouter` without overlap.
    let mut rpc = axum::Router::<crate::state::AppState>::new()
        .route(
            mount_path,
            axum::routing::get(serve_mcp_get).post(serve_mcp),
        )
        .layer(axum::extract::Extension(Arc::clone(&server)));
    if let Some(layer_fn) = endpoint_layer {
        rpc = layer_fn(rpc);
    }
    // Host/Origin gate, applied outermost on the JSON-RPC surface so an
    // untrusted Host or disallowed Origin is rejected before the optional auth
    // gate runs and before `serve_mcp` buffers the body (see
    // `mcp_host_origin_guard`). Mounted only on `rpc`, not the `OPTIONS`
    // preflight below, which a browser sends unauthenticated and host-agnostic.
    let guard_server = Arc::clone(&server);
    rpc = rpc.layer(axum::middleware::from_fn(move |req, next| {
        mcp_host_origin_guard(Arc::clone(&guard_server), req, next)
    }));
    let preflight = axum::Router::<crate::state::AppState>::new()
        .route(mount_path, axum::routing::options(serve_mcp_options))
        .layer(axum::extract::Extension(server));
    rpc.merge(preflight)
}

/// Wrap a fully-assembled MCP envelope so **every** response carries the CORS
/// grant — including ones produced by outer layers *before* `serve_mcp` runs:
/// `secure_mcp` auth rejections (401/403), the request-body limit (413), and
/// rate limiting (429). Applied as the outermost layer so it sees the final
/// response regardless of which inner layer produced it; without it an
/// allowlisted browser client's preflight succeeds but the rejection is masked
/// as an opaque CORS failure instead of surfacing the real status. The grant is
/// only added for allowlisted origins (see [`apply_cors_headers`]).
pub(crate) fn apply_mcp_cors_layer(
    router: axum::Router<crate::state::AppState>,
    cors: &crate::config::CorsConfig,
) -> axum::Router<crate::state::AppState> {
    let cors = cors.clone();
    router.layer(axum::middleware::from_fn(
        move |req: axum::extract::Request, next: axum::middleware::Next| {
            let cors = cors.clone();
            async move {
                let origin = req
                    .headers()
                    .get(header::ORIGIN)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned);
                let mut response = next.run(req).await;
                apply_cors_headers(&cors, origin.as_deref(), &mut response);
                response
            }
        },
    ))
}

/// Answer a CORS preflight (`OPTIONS`) for the MCP endpoint. Because the
/// endpoint is mounted outside the global CORS layer, an allowlisted browser
/// MCP client's preflight would otherwise get no `Access-Control-Allow-*`
/// headers and the browser would block the real `POST`. We reuse the app's CORS
/// config to answer it: only an explicitly allowlisted `Origin` (or `*`) gets
/// the allow headers; anything else gets a bare `204` with no CORS grant.
async fn serve_mcp_options(
    axum::extract::Extension(server): axum::extract::Extension<Arc<McpServer>>,
    headers: HeaderMap,
) -> Response {
    use axum::http::HeaderValue;

    let cors = &server.cors;
    let mut out = HeaderMap::new();
    // `Vary: Origin` since the response depends on the request Origin.
    out.insert(header::VARY, HeaderValue::from_static("origin"));

    let origin = headers.get(header::ORIGIN).and_then(|o| o.to_str().ok());

    // No Origin (non-CORS probe) or a non-allowlisted origin: advertise the
    // allowed methods but grant no cross-origin access.
    let Some(allow_origin) = cors_allow_origin(cors, origin) else {
        out.insert(
            header::ALLOW,
            HeaderValue::from_static("GET, POST, OPTIONS"),
        );
        return (StatusCode::NO_CONTENT, out).into_response();
    };

    if let Ok(v) = HeaderValue::from_str(&allow_origin) {
        out.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, v);
    }
    if let Ok(v) = HeaderValue::from_str(&cors.allowed_methods.join(", ")) {
        out.insert(header::ACCESS_CONTROL_ALLOW_METHODS, v);
    }
    // Mirror the app's configured allow-headers, but always include the MCP
    // Streamable-HTTP transport headers a browser client sends on follow-up
    // requests (`MCP-Protocol-Version`, and `Mcp-Session-Id` for stateful
    // clients). The default `allowed_headers` (`Content-Type, Authorization`)
    // omits them, which would otherwise make the browser block the POST.
    let mut allow_headers = cors.allowed_headers.clone();
    for extra in MCP_REQUEST_HEADERS {
        if !allow_headers.iter().any(|h| h.eq_ignore_ascii_case(extra)) {
            allow_headers.push((*extra).to_owned());
        }
    }
    if let Ok(v) = HeaderValue::from_str(&allow_headers.join(", ")) {
        out.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, v);
    }
    if let Ok(v) = HeaderValue::from_str(&cors.max_age_secs.to_string()) {
        out.insert(header::ACCESS_CONTROL_MAX_AGE, v);
    }
    if cors.allow_credentials {
        out.insert(
            header::ACCESS_CONTROL_ALLOW_CREDENTIALS,
            HeaderValue::from_static("true"),
        );
    }
    (StatusCode::NO_CONTENT, out).into_response()
}

/// Compute the `Access-Control-Allow-Origin` value to grant a request from
/// `origin`, mirroring the preflight's allowlist logic. Returns `None` when no
/// `Origin` is present or it is not allowlisted (a same-origin browser request
/// needs no CORS grant). With credentials the spec forbids `*`, so the concrete
/// origin is echoed instead.
fn cors_allow_origin(cors: &crate::config::CorsConfig, origin: Option<&str>) -> Option<String> {
    let origin = origin?;
    let allow_any = cors.allowed_origins.iter().any(|a| a == "*");
    if !(allow_any || cors.allowed_origins.iter().any(|a| a == origin)) {
        return None;
    }
    Some(if allow_any && !cors.allow_credentials {
        "*".to_owned()
    } else {
        origin.to_owned()
    })
}

/// Attach the actual-request CORS grant to a response. The MCP endpoint is
/// mounted outside the global CORS layer, so without this an allowlisted
/// browser client's preflight would pass but the browser would still block
/// reading the `POST`/`GET` body for lack of `Access-Control-Allow-Origin`.
fn apply_cors_headers(
    cors: &crate::config::CorsConfig,
    origin: Option<&str>,
    response: &mut Response,
) {
    use axum::http::HeaderValue;
    let headers = response.headers_mut();
    // The response varies by `Origin` whenever an origin is in play.
    headers.insert(header::VARY, HeaderValue::from_static("origin"));
    if let Some(allow_origin) = cors_allow_origin(cors, origin)
        && let Ok(v) = HeaderValue::from_str(&allow_origin)
    {
        headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, v);
        if cors.allow_credentials {
            headers.insert(
                header::ACCESS_CONTROL_ALLOW_CREDENTIALS,
                HeaderValue::from_static("true"),
            );
        }
    }
}

/// MCP over Streamable HTTP: a `GET` opens a *server-initiated* stream (for
/// unsolicited server→client messages). Autumn only streams *in response to a
/// `tools/call`* — a streaming tool's SSE rides the POST response (see
/// [`serve_tools_call`]) — so there is nothing to serve on a bare `GET`, and we
/// decline it per spec.
async fn serve_mcp_get() -> Response {
    (
        StatusCode::METHOD_NOT_ALLOWED,
        [(header::ALLOW, "POST")],
        "MCP server-initiated streaming is not supported (POST JSON-RPC only)",
    )
        .into_response()
}

/// Per-request context threaded into a replayed `tools/call` so the in-process
/// dispatch sees the same client identity a direct HTTP request would: the
/// caller's headers, the proxy-resolved client identity, and the connection
/// peer address (for the IP-keyed rate limiter).
struct ReplayContext<'a> {
    headers: &'a HeaderMap,
    identity: Option<&'a crate::security::ResolvedClientIdentity>,
    peer: Option<std::net::SocketAddr>,
}

/// Reject an untrusted `Host`/`:authority` or a disallowed browser `Origin`
/// before the request body is buffered.
///
/// The `/mcp` envelope is merged after `apply_middleware`, so it does not pass
/// through the global [`trusted_host_middleware`](crate::router) every direct
/// route runs; this layer restores that gate for the endpoint. Running as a
/// layer (rather than inside `serve_mcp`) means a bad-`Host` request is rejected
/// before the handler's `Bytes` extractor reads up to the configured
/// `max_request_size_bytes`, exactly as a direct route rejects in middleware
/// before handler extraction.
///
/// Host resolution mirrors `trusted_host_middleware`: the proxy-resolved
/// identity first (honouring `X-Forwarded-Host` from trusted upstreams), then
/// the HTTP/2 `:authority` carried in the request URI, then the `Host` header —
/// so an HTTP/2 client that sends `:authority` without a `Host` header is not
/// wrongly rejected as missing-host. The same proxy-resolved host drives the
/// same-origin `Origin` shortcut, so a same-origin client behind a
/// TLS-terminating proxy isn't 403'd.
async fn mcp_host_origin_guard(
    server: Arc<McpServer>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let identity = req
        .extensions()
        .get::<crate::security::ResolvedClientIdentity>();
    let host = identity
        .and_then(|id| id.host.as_deref())
        .or_else(|| req.uri().authority().map(http::uri::Authority::as_str))
        .or_else(|| {
            req.headers()
                .get(header::HOST)
                .and_then(|h| h.to_str().ok())
        });

    // Trusted-Host enforcement. Without it, because the DNS-rebinding `Origin`
    // check below only fires for browsers, a no-`Origin` agent could call
    // `initialize`/`tools/list` with an arbitrary `Host` and enumerate the tool
    // catalog — even though the same request to a direct route would be rejected.
    let host_trusted = host
        .and_then(crate::router::extract_host_without_port)
        .map(|h| h.trim_end_matches('.').to_ascii_lowercase())
        .filter(|h| !h.is_empty())
        .map_or_else(
            || server.trusted_hosts.allows_missing_host(),
            |h| server.trusted_hosts.allows_host(&h),
        );
    if !host_trusted {
        return (StatusCode::BAD_REQUEST, "Invalid Host header").into_response();
    }

    // DNS-rebinding protection (MCP Streamable-HTTP spec MUST): reject a
    // browser-supplied `Origin` that is neither same-origin nor allowlisted.
    // Non-browser agents send no `Origin` and are unaffected.
    if let Some(origin) = req.headers().get(header::ORIGIN) {
        let origin = origin.to_str().unwrap_or("");
        let scheme = identity.and_then(|id| id.scheme.as_deref());
        if !server.origin_allowed(origin, host, scheme) {
            return (StatusCode::FORBIDDEN, "origin not allowed").into_response();
        }
    }

    next.run(req).await
}

/// The Streamable-HTTP POST handler: parses one JSON-RPC message (or a batch)
/// and responds with `application/json`.
async fn serve_mcp(
    axum::extract::Extension(server): axum::extract::Extension<Arc<McpServer>>,
    identity: Option<axum::extract::Extension<crate::security::ResolvedClientIdentity>>,
    // The connection peer is stored as a `ConnectInfo<SocketAddr>` request
    // extension by axum's connect-info make-service; read it via `Extension`
    // (which is optional-friendly) rather than the `ConnectInfo` extractor.
    connect_info: Option<
        axum::extract::Extension<axum::extract::ConnectInfo<std::net::SocketAddr>>,
    >,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let identity = identity.as_ref().map(|ext| &ext.0);

    // Capture the request `Origin` (if any) so the actual JSON-RPC response can
    // carry the matching CORS grant, mirroring the `OPTIONS` preflight.
    let origin = headers
        .get(header::ORIGIN)
        .and_then(|o| o.to_str().ok())
        .map(str::to_owned);

    // Trusted-Host enforcement and DNS-rebinding `Origin` validation run in the
    // `mcp_host_origin_guard` layer (applied in `build_mcp_router`) rather than
    // here, so an untrusted Host or disallowed Origin is rejected *before* this
    // handler buffers `body` up to the configured `max_request_size_bytes` —
    // mirroring how direct routes reject in `trusted_host_middleware` before
    // handler extraction.
    let parsed: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return json_response(&parse_error(&e.to_string()));
        }
    };

    if let Some(rejection) = reject_unsupported_protocol_version(&headers, &parsed) {
        return rejection;
    }

    let ctx = ReplayContext {
        headers: &headers,
        identity,
        peer: connect_info.map(|ext| (ext.0).0),
    };

    let mut response = match parsed {
        // JSON-RPC 2.0: an empty batch is itself an Invalid Request.
        Value::Array(batch) if batch.is_empty() => {
            json_response(&error(Value::Null, -32600, "Invalid Request: empty batch"))
        }
        // A batch carrying a `tools/call` is refused outright. Batching would
        // let one envelope amplify two budgets a sequence of direct HTTP calls
        // can't: memory (each replayed call buffers up to
        // `MAX_TOOL_RESPONSE_BYTES`, and every response object is retained in
        // `out` until the whole batch serializes) and rate limiting (the
        // envelope is counted once, so each replayed call below would carry
        // `RateLimitExempt` and skip the per-route limiter). The newest protocol
        // revision dropped JSON-RPC batching entirely, so no conformant client
        // batches calls; rejecting here keeps `tools/call` a single-message
        // request — where the per-call limiter and the 10 MiB cap both still
        // apply. Harmless metadata methods (initialize/tools/list/ping) may
        // still be batched.
        Value::Array(batch)
            if batch
                .iter()
                .any(|msg| msg.get("method").and_then(Value::as_str) == Some("tools/call")) =>
        {
            json_response(&error(
                Value::Null,
                -32600,
                "Invalid Request: batched tools/call is not supported; \
                 send each tools/call as a single JSON-RPC request",
            ))
        }
        Value::Array(batch) => {
            let mut out = Vec::new();
            for msg in batch {
                // Only metadata methods reach here (a batched `tools/call` is
                // rejected above), so none set a `Set-Cookie`.
                if let Some(resp) = handle_message(&server, &msg) {
                    out.push(resp);
                }
            }
            // An all-notification batch produces no responses → empty 202.
            if out.is_empty() {
                StatusCode::ACCEPTED.into_response()
            } else {
                json_response(&Value::Array(out))
            }
        }
        // A single request object. A `tools/call` is dispatched through a path
        // that can answer with the Streamable-HTTP SSE channel (a streaming
        // tool) or buffered JSON; everything else (initialize/tools/list/ping)
        // is buffered. A notification (no `id`) yields `None` → 202.
        msg @ Value::Object(_) => {
            if let Some((id, params)) = single_tools_call(&msg) {
                serve_tools_call(&server, &ctx, id, params).await
            } else {
                handle_message(&server, &msg).map_or_else(
                    || StatusCode::ACCEPTED.into_response(),
                    |v| json_response(&v),
                )
            }
        }
        // Anything else (scalar, null) is not a valid JSON-RPC message.
        _ => json_response(&error(
            Value::Null,
            -32600,
            "Invalid Request: expected a JSON object or array",
        )),
    };

    // The endpoint sits outside the global CORS layer, so an allowlisted
    // browser client needs the grant on the actual response to read the body.
    apply_cors_headers(&server.cors, origin.as_deref(), &mut response);
    response
}

/// Enforce the Streamable-HTTP `MCP-Protocol-Version` header. Returns a 400
/// response when a non-`initialize` request carries an unsupported version —
/// otherwise a future client (e.g. a `2025-11-25` one) could run `tools/call`
/// under semantics this server never negotiated. A missing header means "assume
/// the pre-header default" (2025-03-26), which this server supports, so absence
/// is allowed. The `initialize` handshake is exempt: that is where the version
/// is negotiated (in the body), so pre-validating its header would make
/// negotiating a newer client down to a supported version impossible.
fn reject_unsupported_protocol_version(headers: &HeaderMap, parsed: &Value) -> Option<Response> {
    let is_initialize = parsed
        .as_object()
        .and_then(|o| o.get("method"))
        .and_then(Value::as_str)
        == Some("initialize");
    if is_initialize {
        return None;
    }
    let version = headers.get("mcp-protocol-version")?.to_str().unwrap_or("");
    if SUPPORTED_PROTOCOL_VERSIONS.contains(&version) {
        return None;
    }
    Some(
        (
            StatusCode::BAD_REQUEST,
            format!("unsupported MCP-Protocol-Version: {version}"),
        )
            .into_response(),
    )
}

/// Handle a single buffered JSON-RPC message (everything except `tools/call`,
/// which [`serve_tools_call`] handles so it can stream). Returns `None` only
/// for a *valid* notification (a `2.0` message with a `method` and no `id`).
fn handle_message(server: &McpServer, msg: &Value) -> Option<Value> {
    let id = msg.get("id").cloned();

    // A JSON-RPC 2.0 `id`, when present, must be a string, number, or null;
    // an object/array id is invalid and must not reach dispatch.
    let id_ok = id
        .as_ref()
        .is_none_or(|v| v.is_string() || v.is_number() || v.is_null());

    // Reject anything that isn't a well-formed JSON-RPC 2.0 request/notification
    // object (e.g. `5`, `{}`, a message missing `jsonrpc`/`method`, or one with
    // a structured `id`). A bare notification-shaped-but-invalid item must still
    // produce an error rather than being silently swallowed.
    let is_valid = msg.is_object()
        && msg.get("jsonrpc").and_then(Value::as_str) == Some("2.0")
        && msg.get("method").and_then(Value::as_str).is_some()
        && id_ok;
    if !is_valid {
        // Echo the id only when it is a usable string/number; otherwise (missing
        // or structurally invalid) the spec requires `id: null`.
        let err_id = match &id {
            Some(v) if v.is_string() || v.is_number() => v.clone(),
            _ => Value::Null,
        };
        return Some(error(err_id, -32600, "Invalid Request"));
    }

    // A valid notification (method present, no `id`) gets no response.
    let id = id?;
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
    let params = msg.get("params").cloned().unwrap_or(Value::Null);

    let result = match method {
        "initialize" => Ok(initialize_result(server, &params)),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(tools_list_result(server)),
        // A single `tools/call` is diverted to `serve_tools_call` before reaching
        // here, and a batched one is rejected upstream; this arm is defensive.
        "tools/call" => Err((
            -32600,
            "tools/call must be sent as a single JSON-RPC request".to_owned(),
        )),
        other => Err((-32601, format!("method not found: {other}"))),
    };

    Some(match result {
        Ok(value) => success(id, value),
        Err((code, message)) => error(id, code, &message),
    })
}

fn initialize_result(server: &McpServer, params: &Value) -> Value {
    // Echo the client's requested version only if we actually implement it;
    // otherwise advertise our newest supported version (MCP negotiation).
    let protocol = match params.get("protocolVersion").and_then(Value::as_str) {
        Some(requested) if SUPPORTED_PROTOCOL_VERSIONS.contains(&requested) => requested,
        _ => DEFAULT_PROTOCOL_VERSION,
    };
    json!({
        "protocolVersion": protocol,
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": {
            "name": server.server_name,
            "version": server.server_version,
        },
    })
}

fn tools_list_result(server: &McpServer) -> Value {
    let tools: Vec<Value> = server.tools.iter().map(McpTool::descriptor).collect();
    json!({ "tools": tools })
}

/// Whether `msg` is a well-formed single `tools/call` request (JSON-RPC 2.0
/// object, `method == "tools/call"`, with a usable `id`). Returns the cloned
/// `id` and `params`. A `tools/call` is handled apart from [`handle_message`]
/// so its (possibly streaming) result can ride the Streamable-HTTP SSE channel.
///
/// A malformed one (bad `jsonrpc`/`id`) returns `None` and falls through to
/// [`handle_message`], which produces the standard `-32600` error; a
/// `tools/call` *notification* (no `id`) likewise falls through and is treated
/// as a no-op notification (`202`), matching the pre-streaming behavior.
fn single_tools_call(msg: &Value) -> Option<(Value, Value)> {
    let obj = msg.as_object()?;
    if obj.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return None;
    }
    if obj.get("method").and_then(Value::as_str) != Some("tools/call") {
        return None;
    }
    let id = obj.get("id")?;
    if !(id.is_string() || id.is_number() || id.is_null()) {
        return None;
    }
    let params = obj.get("params").cloned().unwrap_or(Value::Null);
    Some((id.clone(), params))
}

/// Dispatch a `tools/call` through the real router and shape the response as an
/// MCP tool result — buffered JSON for an ordinary tool, or a progressive SSE
/// stream for a `#[api_doc(mcp, stream)]` tool whose handler returns `Sse`.
async fn serve_tools_call(
    server: &McpServer,
    ctx: &ReplayContext<'_>,
    id: Value,
    params: Value,
) -> Response {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    // `inputSchema` is always an object; reject a non-object `arguments`
    // (null/string/array) rather than coercing it to `{}` and dispatching.
    let arguments = match params.get("arguments") {
        None => json!({}),
        Some(value) if value.is_object() => value.clone(),
        Some(_) => return json_response(&error(id, -32602, "`arguments` must be a JSON object")),
    };

    let Some(&idx) = server.by_name.get(name) else {
        return json_response(&error(id, -32602, &format!("unknown tool: {name}")));
    };
    let tool = &server.tools[idx];

    let mut request = match build_request(
        tool,
        ctx.headers,
        &arguments,
        &server.csrf_header,
        server.tenant_header.as_deref(),
    ) {
        Ok(req) => req,
        Err(message) => return json_response(&error(id, -32602, &message)),
    };

    // Carry the caller's resolved identity and connection peer into the replay
    // so the dispatch pipeline attributes it like a direct request would — the
    // proxy-aware tenancy host and the IP-keyed rate limiter both read these.
    if let Some(identity) = ctx.identity {
        request.extensions_mut().insert(identity.clone());
    }
    if let Some(peer) = ctx.peer {
        request
            .extensions_mut()
            .insert(axum::extract::ConnectInfo(peer));
    }
    // When the `/mcp` envelope is itself rate-limited, this call was already
    // counted there; mark the replay exempt so the dispatch pipeline's own
    // limiter doesn't charge a second token for the same tool call.
    if server.envelope_rate_limited {
        request
            .extensions_mut()
            .insert(crate::security::RateLimitExempt);
    }

    let response = match server.dispatch.clone().oneshot(request).await {
        Ok(resp) => resp,
        Err(e) => {
            return json_response(&success(id, tool_error(&format!("dispatch failed: {e}"))));
        }
    };

    let status = response.status();
    let is_event_stream = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|c| {
            c.trim_start()
                .to_ascii_lowercase()
                .starts_with("text/event-stream")
        });
    let client_accepts_sse = ctx
        .headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(accept_includes_event_stream);

    // Capture any `Set-Cookie` the inner handler/middleware set (session
    // renewal, CSRF-cookie refresh, login) before the body is consumed, so a
    // single call replays them onto the outer HTTP response — matching what the
    // equivalent direct call would have sent.
    let mut cookies: Vec<HeaderValue> = Vec::new();
    for value in response.headers().get_all(header::SET_COOKIE) {
        cookies.push(value.clone());
    }

    // A streaming tool whose handler streamed (text/event-stream) and whose
    // client can read SSE: project the stream onto the MCP SSE channel. This is
    // the only path that escapes buffering; every other case (a buffered tool, a
    // streaming handler that errored before streaming, or a client that can't
    // read SSE) falls through to the buffered branch below — so the base #1117
    // path and non-SSE clients are entirely unaffected.
    if tool.streams && status.is_success() && is_event_stream && client_accepts_sse {
        return stream_tool_result(id, &params, response, cookies);
    }

    // Unlike a normal HTTP response (streamed straight to the socket), the MCP
    // path buffers the whole body to repackage it as a tool result. Cap that
    // buffer so a runaway handler can't OOM the process; report an overflow as
    // an explicit tool error rather than silently truncating to an empty body.
    let Ok(bytes) = axum::body::to_bytes(response.into_body(), MAX_TOOL_RESPONSE_BYTES).await
    else {
        return json_response(&success(
            id,
            tool_error(&format!(
                "handler response exceeded the {MAX_TOOL_RESPONSE_BYTES}-byte MCP tool-result limit"
            )),
        ));
    };
    // A streaming handler buffered for a non-SSE client: collapse the SSE wire
    // frames into their concatenated data payload so the client still receives a
    // usable single result instead of raw `data:`-framed text.
    let text = if is_event_stream {
        collapse_sse_body(&bytes)
    } else {
        String::from_utf8_lossy(&bytes).into_owned()
    };

    let value = if status.is_success() {
        success(id, tool_ok(&text))
    } else {
        success(
            id,
            tool_error(&format!(
                "handler returned HTTP {}: {text}",
                status.as_u16()
            )),
        )
    };
    let mut resp = json_response(&value);
    for cookie in cookies {
        resp.headers_mut().append(header::SET_COOKIE, cookie);
    }
    resp
}

// ── Progressive (SSE) tool-result projection ──────────────────────
//
// A streaming tool is a normal Autumn route returning `Sse` (issue #1118). Its
// dispatched response is already SSE wire bytes (`event:`/`data:` frames). The
// MCP endpoint *re-projects* those frames onto the Streamable-HTTP SSE channel:
// each handler event becomes a `notifications/progress` message (when the
// client supplied `_meta.progressToken`), and the stream is terminated by the
// final id-correlated `tools/call` result. The developer writes a plain Autumn
// stream — zero hand-written JSON-RPC/SSE framing — and time-to-first-signal is
// decoupled from total tool duration because frames are forwarded as they
// arrive rather than buffered.

/// Whether an `Accept` header opts the client in to an SSE response. The MCP
/// Streamable-HTTP transport has a streaming client advertise
/// `Accept: ..., text/event-stream`; a client that does not is served a
/// buffered JSON result instead (see [`serve_tools_call`]). `*/*` is treated as
/// "does not explicitly accept SSE" so a plain JSON client isn't handed a body
/// it can't parse.
fn accept_includes_event_stream(accept: &str) -> bool {
    accept.split(',').any(|part| {
        let media = part
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        media == "text/event-stream" || media == "text/*"
    })
}

/// Build the JSON-RPC `notifications/progress` message for one streamed event.
///
/// When the event's data is a JSON object carrying a numeric `progress`, its
/// `progress`/`total`/`message` fields are forwarded verbatim (structured
/// progress). Otherwise the event text is the human-readable `message` and
/// `progress` is the running per-event counter.
fn progress_notification(token: &Value, progress: f64, message: &str) -> Value {
    if let Ok(Value::Object(map)) = serde_json::from_str::<Value>(message)
        && map.get("progress").is_some_and(Value::is_number)
    {
        let mut params = serde_json::Map::new();
        params.insert("progressToken".into(), token.clone());
        for key in ["progress", "total", "message"] {
            if let Some(v) = map.get(key) {
                params.insert((*key).to_owned(), v.clone());
            }
        }
        return json!({
            "jsonrpc": "2.0",
            "method": "notifications/progress",
            "params": Value::Object(params),
        });
    }
    json!({
        "jsonrpc": "2.0",
        "method": "notifications/progress",
        "params": { "progressToken": token, "progress": progress, "message": message },
    })
}

/// Phase of the SSE projection state machine.
enum ProjectionPhase {
    /// Reading frames from the handler's stream.
    Streaming,
    /// Handler stream ended; the final `tools/call` result is pending.
    Final,
    /// Final result emitted; the stream is complete.
    Done,
}

/// State threaded through the projection `unfold` (see [`stream_tool_result`]).
struct StreamProjection {
    /// The handler's SSE response body, as a byte stream of wire frames.
    body: Pin<Box<dyn Stream<Item = Result<Bytes, axum::Error>> + Send>>,
    parser: SseWireParser,
    /// MCP messages parsed but not yet emitted (one inner chunk may yield many).
    ready: VecDeque<Event>,
    /// The client's `_meta.progressToken`, if any; absent ⇒ no progress notes.
    progress_token: Option<Value>,
    /// The original request id, echoed on the terminating result.
    id: Value,
    /// Running per-event progress counter (used when the handler doesn't supply
    /// its own structured `progress`).
    progress: f64,
    /// Accumulated text of progress (default/`progress`-typed) events.
    progress_parts: Vec<String>,
    /// Accumulated text of explicit `event: result` frames, if the handler uses
    /// them to distinguish the final payload from incremental progress.
    result_parts: Vec<String>,
    phase: ProjectionPhase,
}

/// Project a streaming handler's `Sse` response onto the MCP SSE channel.
///
/// Back-pressure / disconnect: the returned [`Sse`] writes frames to the socket
/// as the agent consumes them; if the agent disconnects, axum drops the
/// response future, which drops this `unfold` state — and with it the boxed
/// handler body stream — so the handler's task unwinds with no leak and no panic
/// on a closed stream, exactly as `sse.rs` handles a dropped subscriber.
fn stream_tool_result(
    id: Value,
    params: &Value,
    response: Response,
    cookies: Vec<HeaderValue>,
) -> Response {
    let progress_token = params
        .get("_meta")
        .and_then(|m| m.get("progressToken"))
        .cloned();

    let state = StreamProjection {
        body: Box::pin(response.into_body().into_data_stream()),
        parser: SseWireParser::new(),
        ready: VecDeque::new(),
        progress_token,
        id,
        progress: 0.0,
        progress_parts: Vec::new(),
        result_parts: Vec::new(),
        phase: ProjectionPhase::Streaming,
    };

    let stream = futures::stream::unfold(state, project_next);
    let mut resp = Sse::new(stream)
        .keep_alive(crate::sse::keep_alive())
        .into_response();
    for cookie in cookies {
        resp.headers_mut().append(header::SET_COOKIE, cookie);
    }
    resp
}

/// Yield the next MCP message (as an SSE [`Event`]) for the projection.
async fn project_next(
    mut st: StreamProjection,
) -> Option<(Result<Event, Infallible>, StreamProjection)> {
    loop {
        if let Some(event) = st.ready.pop_front() {
            return Some((Ok(event), st));
        }
        match st.phase {
            ProjectionPhase::Done => return None,
            ProjectionPhase::Final => {
                // Prefer explicit `event: result` payloads; otherwise the joined
                // progress text is the complete result.
                let content = if st.result_parts.is_empty() {
                    st.progress_parts.join("\n")
                } else {
                    st.result_parts.concat()
                };
                let value = success(st.id.clone(), tool_ok(&content));
                st.phase = ProjectionPhase::Done;
                return Some((Ok(Event::default().data(value.to_string())), st));
            }
            ProjectionPhase::Streaming => {
                if let Some(Ok(bytes)) = st.body.next().await {
                    let events = st.parser.push(&bytes);
                    enqueue_projected(&mut st, events);
                } else {
                    // End of stream (or a body error): flush any trailing frame
                    // and move on to the terminating result.
                    let trailing = st.parser.finish();
                    enqueue_projected(&mut st, trailing);
                    st.phase = ProjectionPhase::Final;
                }
            }
        }
    }
}

/// Fold parsed handler frames into the projection: accumulate their text for the
/// final result and, when a `progressToken` is present, enqueue a
/// `notifications/progress` message per incremental frame.
fn enqueue_projected(st: &mut StreamProjection, events: Vec<ParsedSseEvent>) {
    for ev in events {
        if ev.event.as_deref() == Some("result") {
            // Explicit final-result content — not surfaced as progress.
            st.result_parts.push(ev.data);
            continue;
        }
        st.progress_parts.push(ev.data.clone());
        if let Some(token) = &st.progress_token {
            st.progress += 1.0;
            let note = progress_notification(token, st.progress, &ev.data);
            st.ready.push_back(Event::default().data(note.to_string()));
        }
    }
}

/// One logical SSE frame parsed off the wire.
struct ParsedSseEvent {
    /// The `event:` field, if any (`None` ⇒ the default unnamed event).
    event: Option<String>,
    /// The `data:` payload (multiple `data:` lines joined by `\n`).
    data: String,
}

/// Incremental parser for the SSE wire format (`event:`/`data:` lines, blank
/// line dispatches, `:`-comment keep-alives ignored). Fed arbitrary byte chunks;
/// emits one [`ParsedSseEvent`] per completed frame.
struct SseWireParser {
    /// Bytes received but not yet split into a complete line.
    buffer: String,
    event_type: Option<String>,
    data_lines: Vec<String>,
    /// Whether any field line has been seen since the last dispatch (so a lone
    /// blank line or a comment doesn't emit an empty frame).
    has_fields: bool,
}

impl SseWireParser {
    const fn new() -> Self {
        Self {
            buffer: String::new(),
            event_type: None,
            data_lines: Vec::new(),
            has_fields: false,
        }
    }

    /// Feed a chunk; return any frames completed by it.
    fn push(&mut self, bytes: &[u8]) -> Vec<ParsedSseEvent> {
        self.buffer.push_str(&String::from_utf8_lossy(bytes));
        let mut out = Vec::new();
        while let Some(pos) = self.buffer.find('\n') {
            let line: String = self.buffer.drain(..=pos).collect();
            if let Some(event) = self.process_line(line.trim_end_matches(['\n', '\r'])) {
                out.push(event);
            }
        }
        out
    }

    /// Flush the trailing partial line and any pending (unterminated) frame.
    fn finish(&mut self) -> Vec<ParsedSseEvent> {
        let mut out = Vec::new();
        if !self.buffer.is_empty() {
            let line = std::mem::take(&mut self.buffer);
            if let Some(event) = self.process_line(line.trim_end_matches(['\n', '\r'])) {
                out.push(event);
            }
        }
        if let Some(event) = self.dispatch() {
            out.push(event);
        }
        out
    }

    fn process_line(&mut self, line: &str) -> Option<ParsedSseEvent> {
        if line.is_empty() {
            return self.dispatch();
        }
        // A leading colon marks a comment line (SSE keep-alive); ignore it.
        if line.starts_with(':') {
            return None;
        }
        let (field, value) = match line.split_once(':') {
            // One optional leading space after the colon is stripped per spec.
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            None => (line, ""),
        };
        match field {
            "event" => {
                self.event_type = Some(value.to_owned());
                self.has_fields = true;
            }
            "data" => {
                self.data_lines.push(value.to_owned());
                self.has_fields = true;
            }
            // `id:`/`retry:` and any unknown field are irrelevant to projection.
            _ => {}
        }
        None
    }

    fn dispatch(&mut self) -> Option<ParsedSseEvent> {
        if !self.has_fields {
            return None;
        }
        let event = ParsedSseEvent {
            event: self.event_type.take(),
            data: self.data_lines.join("\n"),
        };
        self.data_lines.clear();
        self.has_fields = false;
        Some(event)
    }
}

/// Collapse a fully-buffered SSE body into a single result string — the
/// non-SSE-client fallback for a streaming tool. Mirrors the streaming final
/// content: explicit `event: result` frames win, else the joined frame data.
fn collapse_sse_body(bytes: &[u8]) -> String {
    let mut parser = SseWireParser::new();
    let mut events = parser.push(bytes);
    events.extend(parser.finish());
    let (results, progress): (Vec<_>, Vec<_>) = events
        .into_iter()
        .partition(|e| e.event.as_deref() == Some("result"));
    if results.is_empty() {
        progress
            .into_iter()
            .map(|e| e.data)
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        results.into_iter().map(|e| e.data).collect()
    }
}

/// Reconstruct an in-process HTTP request from a tool call's arguments.
fn build_request(
    tool: &McpTool,
    headers: &HeaderMap,
    arguments: &Value,
    csrf_header: &str,
    tenant_header: Option<&str>,
) -> Result<axum::http::Request<Body>, String> {
    // Fill the path template from top-level string-ish arguments.
    let mut path = tool.path_template.clone();
    for param in &tool.path_params {
        // axum catch-all params (`/files/{*rest}`) surface from `ApiDoc` with a
        // leading `*`. Clients address them by the bare name, and their value is
        // a multi-segment path whose `/` separators must be preserved (each
        // segment is still percent-encoded individually).
        let is_catch_all = param.starts_with('*');
        let arg_key = param.strip_prefix('*').unwrap_or(param);
        let raw = arguments
            .get(arg_key)
            .ok_or_else(|| format!("missing required path parameter `{arg_key}`"))?;
        // The tool schema advertises every path param as `{"type":"string"}`.
        // A string passes through; a number/bool coerces to a single safe
        // segment. `null`, an object, or an array has no valid single-segment
        // representation — replaying its literal `null`/JSON text as a path
        // segment could hit a real (possibly mutating) resource, so reject it
        // as invalid params (mapped to `-32602`) instead.
        let value = match raw {
            Value::String(s) => s.clone(),
            Value::Number(_) | Value::Bool(_) => raw.to_string(),
            _ => return Err(format!("path parameter `{arg_key}` must be a string")),
        };
        // Use the same full segment encoder the typed path helpers use, so an
        // MCP call accepts the same values a direct HTTP caller could pass.
        let encoded = if is_catch_all {
            value
                .split('/')
                .map(crate::paths::encode_path_segment)
                .collect::<Vec<_>>()
                .join("/")
        } else {
            crate::paths::encode_path_segment(&value)
        };
        path = replace_path_param(&path, param, &encoded);
    }

    // Build the query string from the `query` object, if any. The advertised
    // `inputSchema` types `query` as an object, so a present-but-non-object
    // value (`null`, a string, an array) is an invalid-params error rather than
    // being silently dropped — which would otherwise replay the tool with
    // defaulted/unfiltered query parameters.
    if tool.has_query
        && let Some(query) = arguments.get("query")
    {
        let Value::Object(map) = query else {
            return Err("`query` must be a JSON object".to_owned());
        };
        let mut pairs: Vec<(String, String)> = Vec::new();
        for (key, value) in map {
            match value {
                // Form/explode semantics: an array field expands to repeated
                // keys (`tags=a&tags=b`), matching the OpenAPI query model the
                // tool schema advertises — not a single `tags=["a","b"]`.
                Value::Array(items) => {
                    for item in items {
                        pairs.push((key.clone(), query_scalar(item)));
                    }
                }
                other => pairs.push((key.clone(), query_scalar(other))),
            }
        }
        if !pairs.is_empty() {
            let qs = serde_urlencoded::to_string(&pairs)
                .map_err(|e| format!("invalid query arguments: {e}"))?;
            path = format!("{path}?{qs}");
        }
    }

    let mut builder = axum::http::Request::builder()
        .method(tool.method.as_str())
        .uri(&path);

    // Replay the caller's headers verbatim so the dispatched request
    // authenticates and is attributed exactly as a direct HTTP call would:
    //  * `Authorization` — bearer-token (`RequireApiToken`) auth.
    //  * `Cookie` — session-based `#[secured]` routes / session tenancy.
    //  * `Idempotency-Key` — `IdempotencyLayer` dedupe on retried writes.
    //  * `Host` / `Forwarded` / `X-Forwarded-*` / `X-Real-IP` — subdomain
    //    tenancy host resolution and the rate limiter's client-IP attribution.
    for name in FORWARDED_HEADERS {
        // Forward *every* value, not just the first: a header like `Cookie` can
        // appear multiple times, and `CsrfLayer` inspects all `Cookie` headers
        // to detect cookie-tossing (duplicate CSRF cookies). Collapsing them to
        // one value here would let a replayed write slip past that check.
        for value in headers.get_all(*name) {
            builder = builder.header(*name, value);
        }
    }
    // Forward the configured CSRF token header (default `x-csrf-token`) so a
    // session-authenticated write tool passes `CsrfLayer`, which reads
    // `CsrfConfig::token_header` — not a hard-coded name.
    if let Some(value) = headers.get(csrf_header) {
        builder = builder.header(csrf_header, value);
    }
    // Header-based tenancy: forward the configured tenant header (default
    // `x-tenant-id`) so the `Tenant` extractor on the dispatched request
    // resolves the same tenant a direct HTTP caller would.
    if let Some(name) = tenant_header
        && let Some(value) = headers.get(name)
    {
        builder = builder.header(name, value);
    }

    let body = if tool.has_body {
        // The tool schema marks `body` required; reject a call that omits it
        // rather than dispatching an empty `{}` that a defaults-only DTO would
        // silently accept (violating the advertised contract).
        let payload = arguments
            .get("body")
            .ok_or_else(|| "missing required `body` argument".to_owned())?;
        builder = builder.header(header::CONTENT_TYPE, "application/json");
        Body::from(serde_json::to_vec(payload).unwrap_or_default())
    } else {
        Body::empty()
    };

    builder
        .body(body)
        .map_err(|e| format!("invalid request: {e}"))
}

/// Render a single query-argument value as a string for the query string.
/// Strings pass through unquoted; other scalars use their JSON text.
fn query_scalar(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Replace a single `{name}` / `{name:regex}` capture in a path template.
fn replace_path_param(path: &str, name: &str, value: &str) -> String {
    let mut out = String::with_capacity(path.len());
    let mut rest = path;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        if let Some(end) = after.find('}') {
            let inner = &after[..end];
            let capture = inner.split(':').next().unwrap_or(inner).trim();
            if capture == name {
                out.push_str(value);
            } else {
                out.push('{');
                out.push_str(inner);
                out.push('}');
            }
            rest = &after[end + 1..];
        } else {
            out.push_str(&rest[start..]);
            return out;
        }
    }
    out.push_str(rest);
    out
}

// ── MCP tool-result helpers ───────────────────────────────────────

fn tool_ok(text: &str) -> Value {
    json!({
        "content": [ { "type": "text", "text": text } ],
        "isError": false,
    })
}

fn tool_error(text: &str) -> Value {
    json!({
        "content": [ { "type": "text", "text": text } ],
        "isError": true,
    })
}

// ── JSON-RPC envelope helpers ─────────────────────────────────────

fn success(id: Value, result: Value) -> Value {
    // Build by hand so `id`/`result` are moved (not borrowed via `json!`).
    let mut obj = serde_json::Map::new();
    obj.insert("jsonrpc".into(), json!("2.0"));
    obj.insert("id".into(), id);
    obj.insert("result".into(), result);
    Value::Object(obj)
}

fn error(id: Value, code: i64, message: &str) -> Value {
    let mut obj = serde_json::Map::new();
    obj.insert("jsonrpc".into(), json!("2.0"));
    obj.insert("id".into(), id);
    obj.insert("error".into(), json!({ "code": code, "message": message }));
    Value::Object(obj)
}

fn parse_error(message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": Value::Null,
        "error": { "code": -32700, "message": format!("parse error: {message}") },
    })
}

fn json_response(value: &Value) -> Response {
    (
        [(header::CONTENT_TYPE, "application/json")],
        serde_json::to_string(value).unwrap_or_else(|_| "{}".to_owned()),
    )
        .into_response()
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
            }),
            ..Default::default()
        }
    }

    #[test]
    fn opt_in_required_without_hatch() {
        let mut d = doc("GET", "/a", "a");
        assert!(!should_expose(&d, false), "no opt-in => not exposed");
        d.mcp_tool = true;
        assert!(should_expose(&d, false));
    }

    #[test]
    fn exclude_always_wins() {
        let mut d = doc("GET", "/a", "a");
        d.mcp_tool = true;
        d.mcp_exclude = true;
        assert!(!should_expose(&d, false));
        assert!(!should_expose(&d, true));
    }

    #[test]
    fn hatch_includes_reads_excludes_unopted_writes() {
        let read = doc("GET", "/a", "a");
        let write = doc("POST", "/a", "b");
        assert!(should_expose(&read, true));
        assert!(!should_expose(&write, true), "mutating verb needs opt-in");
    }

    #[test]
    fn hatch_still_allows_opted_in_writes() {
        let mut write = doc("POST", "/a", "b");
        write.mcp_tool = true;
        assert!(should_expose(&write, true));
    }

    #[test]
    fn html_routes_are_ineligible() {
        let mut d = doc("GET", "/page", "page");
        d.response = None; // HTML/Maud route
        d.mcp_tool = true;
        assert!(!should_expose(&d, false));
    }

    #[test]
    fn no_content_delete_with_opt_in_is_eligible() {
        // A 204 No Content route (e.g. the repository macro's generated
        // DELETE) has no response schema *by contract*, not because it is an
        // HTML route. An explicit opt-in must expose it.
        let mut d = doc("DELETE", "/api/widgets/{id}", "widget_api_delete");
        d.response = None;
        d.success_status = 204;
        d.mcp_tool = true;
        assert!(
            should_expose(&d, false),
            "opted-in 204 route is a deliberate empty success contract"
        );
    }

    #[test]
    fn no_content_without_opt_in_stays_hidden() {
        let mut d = doc("DELETE", "/api/widgets/{id}", "widget_api_delete");
        d.response = None;
        d.success_status = 204;
        assert!(!should_expose(&d, false), "no opt-in => hidden");
    }

    #[test]
    fn no_content_under_hatch_pins_behavior() {
        // A read-only, deliberately body-less endpoint under the whole-API
        // hatch: 204 is a structural JSON-API signal (unlike an HTML route's
        // 200-with-body), so the hatch includes it.
        let mut d = doc("GET", "/api/ping", "ping");
        d.response = None;
        d.success_status = 204;
        assert!(should_expose(&d, true));
        assert!(!should_expose(&d, false), "still requires hatch or opt-in");
    }

    #[test]
    fn derive_tools_keeps_opted_in_no_content_route() {
        // The "opted in but no JSON response" warn/skip block in
        // `derive_tools` must not silently drop a 204 route that
        // `should_expose` accepts.
        let mut d = doc("DELETE", "/api/widgets/{id}", "widget_api_delete");
        d.response = None;
        d.success_status = 204;
        d.mcp_tool = true;
        let tools = derive_tools(&[d], false, None);
        assert_eq!(tools.len(), 1, "204 opt-in must derive a tool");
        assert_eq!(tools[0].name, "widget_api_delete");
        assert_eq!(tools[0].annotations["destructiveHint"], true);
    }

    #[test]
    fn streaming_tool_is_eligible_without_response_schema() {
        // A streaming tool returns `Sse` (no JSON response schema); the `stream`
        // flag exempts it from the JSON-out gate that excludes HTML routes.
        let mut d = doc("GET", "/api/search", "search");
        d.response = None;
        d.mcp_stream = true;
        // Still requires opt-in: `stream` alone (no `mcp`) is not exposed.
        assert!(
            !should_expose(&d, false),
            "stream without opt-in stays hidden"
        );
        d.mcp_tool = true;
        assert!(
            should_expose(&d, false),
            "opted-in streaming tool is exposed"
        );
        // Exclusion still wins.
        d.mcp_exclude = true;
        assert!(!should_expose(&d, false));
    }

    #[test]
    fn streaming_get_is_included_under_the_hatch() {
        // Under `expose_all`, a read-only streaming GET is auto-included even
        // without an explicit `mcp` tag (and despite having no response schema).
        let mut d = doc("GET", "/api/search", "search");
        d.response = None;
        d.mcp_stream = true;
        assert!(should_expose(&d, true));
        // A mutating streaming verb still needs an explicit opt-in.
        let mut w = doc("POST", "/api/search", "search2");
        w.response = None;
        w.mcp_stream = true;
        assert!(!should_expose(&w, true));
    }

    #[test]
    fn accept_header_gates_sse() {
        assert!(accept_includes_event_stream(
            "application/json, text/event-stream"
        ));
        assert!(accept_includes_event_stream("text/event-stream;q=1.0"));
        assert!(accept_includes_event_stream("text/*"));
        // A plain JSON client (or a generic `*/*`) does not opt in to SSE.
        assert!(!accept_includes_event_stream("application/json"));
        assert!(!accept_includes_event_stream("*/*"));
    }

    #[test]
    fn sse_parser_splits_frames_and_joins_data() {
        let mut p = SseWireParser::new();
        // A multi-data frame, an `event:`-typed frame, and a comment keep-alive.
        let mut events = p.push(b"data: line1\ndata: line2\n\n");
        events.extend(p.push(b": keep-alive\n\nevent: result\ndata: final\n\n"));
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event, None);
        assert_eq!(events[0].data, "line1\nline2");
        assert_eq!(events[1].event.as_deref(), Some("result"));
        assert_eq!(events[1].data, "final");
    }

    #[test]
    fn sse_parser_handles_chunk_boundaries_mid_frame() {
        // A frame split across chunk boundaries must still parse once complete.
        let mut p = SseWireParser::new();
        assert!(p.push(b"data: hel").is_empty());
        assert!(p.push(b"lo\n").is_empty());
        let events = p.push(b"\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "hello");
    }

    #[test]
    fn progress_notification_plain_and_structured() {
        let token = json!("tok");
        // Plain text → message + running counter.
        let plain = progress_notification(&token, 2.0, "working");
        assert_eq!(plain["method"], "notifications/progress");
        assert_eq!(plain["params"]["progressToken"], "tok");
        assert_eq!(plain["params"]["progress"], 2.0);
        assert_eq!(plain["params"]["message"], "working");
        // Structured JSON with a numeric `progress` → forwarded verbatim.
        let structured = progress_notification(
            &token,
            99.0,
            r#"{"progress":50,"total":100,"message":"half"}"#,
        );
        assert_eq!(structured["params"]["progress"], 50);
        assert_eq!(structured["params"]["total"], 100);
        assert_eq!(structured["params"]["message"], "half");
    }

    #[test]
    fn collapse_sse_body_prefers_result_frames() {
        // No `result` frame: data joined.
        let joined = collapse_sse_body(b"data: a\n\ndata: b\n\n");
        assert_eq!(joined, "a\nb");
        // With a `result` frame: only the result content is kept.
        let result = collapse_sse_body(b"data: progress\n\nevent: result\ndata: done\n\n");
        assert_eq!(result, "done");
    }

    #[test]
    fn annotations_track_method() {
        assert_eq!(annotations_for("GET", "t")["readOnlyHint"], json!(true));
        assert_eq!(annotations_for("POST", "t")["readOnlyHint"], json!(false));
        assert_eq!(
            annotations_for("DELETE", "t")["destructiveHint"],
            json!(true)
        );
        assert!(
            annotations_for("POST", "t")
                .get("destructiveHint")
                .is_none()
        );
    }

    #[test]
    fn input_schema_includes_path_param_and_body() {
        let mut d = doc("POST", "/users/{id}", "create");
        d.path_params = &["id"];
        d.request_body = Some(SchemaEntry {
            name: "NewUser",
            kind: SchemaKind::Ref,
        });
        let schema = build_input_schema(&d, &serde_json::Map::new());
        assert_eq!(schema["type"], "object");
        assert!(schema["properties"]["id"].is_object());
        assert!(schema["properties"]["body"].is_object());
        let required = schema["required"].as_array().unwrap();
        assert!(required.contains(&json!("id")));
        assert!(required.contains(&json!("body")));
    }

    #[test]
    fn replace_path_param_handles_regex_captures() {
        assert_eq!(replace_path_param("/u/{id}", "id", "7"), "/u/7");
        assert_eq!(replace_path_param("/u/{id:[0-9]+}", "id", "7"), "/u/7");
        assert_eq!(
            replace_path_param("/u/{id}/p/{pid}", "pid", "9"),
            "/u/{id}/p/9"
        );
    }

    fn tool(method: &str, path: &str, has_body: bool, has_query: bool) -> McpTool {
        McpTool {
            name: "t".to_owned(),
            description: None,
            input_schema: json!({}),
            annotations: json!({}),
            method: method.to_owned(),
            path_template: path.to_owned(),
            path_params: Vec::new(),
            has_body,
            has_query,
            streams: false,
        }
    }

    #[test]
    fn build_request_rejects_missing_required_body() {
        let t = tool("POST", "/api/todos", true, false);
        let err =
            build_request(&t, &HeaderMap::new(), &json!({}), "x-csrf-token", None).unwrap_err();
        assert!(err.contains("body"), "got: {err}");
    }

    #[test]
    fn build_request_explodes_array_query_into_repeated_keys() {
        let t = tool("GET", "/api/search", false, true);
        let req = build_request(
            &t,
            &HeaderMap::new(),
            &json!({ "query": { "tags": ["a", "b"], "q": "x" } }),
            "x-csrf-token",
            None,
        )
        .expect("request builds");
        let query = req.uri().query().unwrap_or_default();
        assert!(query.contains("tags=a"), "got: {query}");
        assert!(query.contains("tags=b"), "got: {query}");
        assert!(query.contains("q=x"), "got: {query}");
        assert!(
            !query.contains("%5B"), // no JSON `[` — i.e. not `tags=["a","b"]`
            "array must explode, not serialize as JSON: {query}"
        );
    }

    #[test]
    fn build_request_forwards_authorization_and_cookie() {
        let t = tool("GET", "/secure", false, false);
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, "Bearer tok".parse().unwrap());
        headers.insert(header::COOKIE, "autumn.sid=abc".parse().unwrap());
        let req =
            build_request(&t, &headers, &json!({}), "x-csrf-token", None).expect("request builds");
        assert_eq!(
            req.headers().get(header::AUTHORIZATION).unwrap(),
            "Bearer tok"
        );
        assert_eq!(req.headers().get(header::COOKIE).unwrap(), "autumn.sid=abc");
    }

    #[test]
    fn build_request_forwards_csrf_token() {
        let t = tool("POST", "/api/todos", true, false);
        let mut headers = HeaderMap::new();
        headers.insert("x-csrf-token", "csrf123".parse().unwrap());
        let req = build_request(
            &t,
            &headers,
            &json!({ "body": { "x": 1 } }),
            "x-csrf-token",
            None,
        )
        .expect("request builds");
        assert_eq!(req.headers().get("x-csrf-token").unwrap(), "csrf123");
    }

    #[test]
    fn build_request_forwards_configured_csrf_header() {
        // Apps that customize security.csrf.token_header must have that header
        // forwarded, not a hard-coded `x-csrf-token`.
        let t = tool("POST", "/api/todos", true, false);
        let mut headers = HeaderMap::new();
        headers.insert("x-xsrf-token", "csrf123".parse().unwrap());
        let req = build_request(
            &t,
            &headers,
            &json!({ "body": { "x": 1 } }),
            "x-xsrf-token",
            None,
        )
        .expect("request builds");
        assert_eq!(req.headers().get("x-xsrf-token").unwrap(), "csrf123");
    }

    #[test]
    fn build_request_preserves_slashes_for_catch_all_param() {
        // A catch-all route `/files/{*path}`: the argument is addressed by the
        // bare name `path`, and its `/` separators survive into the replay URI.
        let mut t = tool("GET", "/files/{*path}", false, false);
        t.path_params = vec!["*path".to_owned()];
        let req = build_request(
            &t,
            &HeaderMap::new(),
            &json!({ "path": "a/b c/d.txt" }),
            "x-csrf-token",
            None,
        )
        .expect("request builds");
        // Slashes preserved as separators; the space in a segment is encoded.
        assert_eq!(req.uri().path(), "/files/a/b%20c/d.txt");
    }

    #[test]
    fn build_request_forwards_configured_tenant_header() {
        let t = tool("GET", "/api/todos", false, false);
        let mut headers = HeaderMap::new();
        headers.insert("x-tenant-id", "acme".parse().unwrap());
        // With header-based tenancy configured, the tenant header is forwarded.
        let req = build_request(
            &t,
            &headers,
            &json!({}),
            "x-csrf-token",
            Some("x-tenant-id"),
        )
        .expect("request builds");
        assert_eq!(req.headers().get("x-tenant-id").unwrap(), "acme");
        // Without a configured tenant header, it is not forwarded.
        let req =
            build_request(&t, &headers, &json!({}), "x-csrf-token", None).expect("request builds");
        assert!(req.headers().get("x-tenant-id").is_none());
    }

    #[test]
    fn build_request_rejects_non_object_query() {
        let t = tool("GET", "/api/search", false, true);
        // `query` advertised as an object: a non-object value is invalid params,
        // not silently dropped (which would replay with defaulted parameters).
        for bad in [
            json!({ "query": null }),
            json!({ "query": "all" }),
            json!({ "query": [1, 2] }),
        ] {
            let err = build_request(&t, &HeaderMap::new(), &bad, "x-csrf-token", None).unwrap_err();
            assert!(err.contains("query"), "got: {err}");
        }
        // An absent `query` is fine (the field is optional).
        assert!(build_request(&t, &HeaderMap::new(), &json!({}), "x-csrf-token", None).is_ok());
    }

    #[test]
    fn build_request_forwards_identity_and_idempotency_headers() {
        let t = tool("POST", "/api/todos", true, false);
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, "tenant1.example.com".parse().unwrap());
        headers.insert("x-forwarded-for", "203.0.113.7".parse().unwrap());
        headers.insert("x-forwarded-host", "tenant1.example.com".parse().unwrap());
        headers.insert("x-real-ip", "203.0.113.7".parse().unwrap());
        headers.insert("idempotency-key", "abc-123".parse().unwrap());
        let req = build_request(
            &t,
            &headers,
            &json!({ "body": { "x": 1 } }),
            "x-csrf-token",
            None,
        )
        .expect("request builds");
        // Host/forwarding headers carry subdomain-tenancy host + rate-limit IP.
        assert_eq!(
            req.headers().get(header::HOST).unwrap(),
            "tenant1.example.com"
        );
        assert_eq!(req.headers().get("x-forwarded-for").unwrap(), "203.0.113.7");
        assert_eq!(req.headers().get("x-real-ip").unwrap(), "203.0.113.7");
        // Idempotency-Key is preserved for safe retries of mutating tools.
        assert_eq!(req.headers().get("idempotency-key").unwrap(), "abc-123");
    }

    #[test]
    fn build_request_forwards_accept_language() {
        // The `Locale` extractor falls back to `Accept-Language`; forwarding it
        // keeps an MCP tool's localized result matching a direct HTTP call.
        let t = tool("GET", "/api/todos", false, false);
        let mut headers = HeaderMap::new();
        headers.insert("accept-language", "fr-CA,fr;q=0.9".parse().unwrap());
        let req =
            build_request(&t, &headers, &json!({}), "x-csrf-token", None).expect("request builds");
        assert_eq!(
            req.headers().get("accept-language").unwrap(),
            "fr-CA,fr;q=0.9"
        );
    }

    #[test]
    fn build_request_preserves_repeated_cookie_headers() {
        // `CsrfLayer` inspects *all* Cookie headers to detect cookie-tossing
        // (duplicate CSRF cookies); forwarding only the first would let a
        // replayed write slip past that check. Every value must be carried.
        let t = tool("POST", "/api/todos", true, false);
        let mut headers = HeaderMap::new();
        headers.append("cookie", "session=abc".parse().unwrap());
        headers.append("cookie", "csrf=dup1".parse().unwrap());
        headers.append("cookie", "csrf=dup2".parse().unwrap());
        let req = build_request(
            &t,
            &headers,
            &json!({ "body": { "x": 1 } }),
            "x-csrf-token",
            None,
        )
        .expect("request builds");
        let cookies: Vec<_> = req
            .headers()
            .get_all("cookie")
            .iter()
            .map(|v| v.to_str().unwrap().to_owned())
            .collect();
        assert_eq!(cookies, ["session=abc", "csrf=dup1", "csrf=dup2"]);
    }

    /// A trusted-Host policy that trusts the given hosts (plus dev loopback,
    /// which `from_config` adds for non-production profiles).
    fn trusted(hosts: &[&str]) -> crate::router::TrustedHostPolicy {
        let mut config = crate::config::AutumnConfig::default();
        config.security.trusted_hosts.hosts = hosts.iter().map(|h| (*h).to_owned()).collect();
        crate::router::TrustedHostPolicy::from_config(&config)
    }

    fn server(allowed_origins: Vec<String>) -> McpServer {
        server_with_trusted(allowed_origins, &[])
    }

    fn server_with_trusted(allowed_origins: Vec<String>, hosts: &[&str]) -> McpServer {
        let cors = crate::config::CorsConfig {
            allowed_origins,
            ..crate::config::CorsConfig::default()
        };
        McpServer::new(
            Vec::new(),
            axum::Router::new(),
            McpWiring {
                cors,
                trusted_hosts: trusted(hosts),
                tenant_header: None,
                csrf_header: "x-csrf-token".to_owned(),
                envelope_rate_limited: false,
            },
        )
    }

    #[test]
    fn origin_allowlist_enforced() {
        let s = server(vec!["https://ok.example".to_owned()]);
        assert!(s.origin_allowed("https://ok.example", None, None));
        assert!(!s.origin_allowed("https://evil.example", None, None));
        // Empty allowlist permits no cross-origin browser request.
        assert!(!server(Vec::new()).origin_allowed("https://any.example", None, None));
        // Wildcard permits any.
        assert!(server(vec!["*".to_owned()]).origin_allowed("https://any.example", None, None));
    }

    #[test]
    fn same_origin_allowed_without_cors_allowlist() {
        // An empty CORS allowlist (the default/production posture) must still
        // permit a browser MCP client served by this same app — provided the
        // host is trusted by the trusted-Host policy.
        let s = server_with_trusted(Vec::new(), &["app.example"]);
        // Host matches the Origin authority → allowed, scheme unknown.
        assert!(s.origin_allowed("https://app.example", Some("app.example"), None));
        // Scheme known and matching → allowed.
        assert!(s.origin_allowed("https://app.example", Some("app.example"), Some("https")));
        // Host with a port matches exactly (loopback is trusted in dev).
        assert!(s.origin_allowed(
            "http://localhost:8080",
            Some("localhost:8080"),
            Some("http")
        ));
        // A different host is still rejected (DNS-rebinding protection holds).
        assert!(!s.origin_allowed("https://evil.example", Some("app.example"), None));
        // Same host but a confidently-known mismatched scheme is rejected.
        assert!(!s.origin_allowed("http://app.example", Some("app.example"), Some("https")));
    }

    #[test]
    fn same_origin_normalizes_default_ports() {
        let s = server_with_trusted(Vec::new(), &["app.example"]);
        // Host carries the explicit default https port; Origin omits it.
        assert!(s.origin_allowed(
            "https://app.example",
            Some("app.example:443"),
            Some("https")
        ));
        // ...and the reverse: Origin carries the default port, Host omits it.
        assert!(s.origin_allowed(
            "https://app.example:443",
            Some("app.example"),
            Some("https")
        ));
        // Explicit default http port likewise normalizes.
        assert!(s.origin_allowed("http://app.example", Some("app.example:80"), Some("http")));
        // A non-default explicit port is NOT the same origin.
        assert!(!s.origin_allowed(
            "https://app.example",
            Some("app.example:8443"),
            Some("https")
        ));
        // The https default (443) must not be conflated with the http default.
        assert!(!s.origin_allowed("http://app.example:443", Some("app.example"), Some("http")));
    }

    #[test]
    fn same_origin_rejected_for_untrusted_host() {
        // DNS rebinding: Origin and Host both name the attacker's domain. The
        // authority matches, but the host is not trusted, so the same-origin
        // shortcut must not fire — and with no CORS allowlist, it is rejected.
        let s = server_with_trusted(Vec::new(), &["app.example"]);
        assert!(!s.origin_allowed(
            "http://attacker.example",
            Some("attacker.example"),
            Some("http")
        ));
        // An explicit cross-origin allowlist entry still works regardless.
        let s = server_with_trusted(vec!["http://attacker.example".to_owned()], &["app.example"]);
        assert!(s.origin_allowed(
            "http://attacker.example",
            Some("attacker.example"),
            Some("http")
        ));
    }

    #[tokio::test]
    async fn options_preflight_grants_only_allowlisted_origin() {
        let s = Arc::new(server_with_trusted(
            vec!["https://app.example".to_owned()],
            &[],
        ));

        // Allowlisted origin → preflight grants the CORS headers.
        let mut headers = HeaderMap::new();
        headers.insert(header::ORIGIN, "https://app.example".parse().unwrap());
        let resp = serve_mcp_options(axum::extract::Extension(s.clone()), headers).await;
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            resp.headers()
                .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .unwrap(),
            "https://app.example"
        );
        assert!(
            resp.headers()
                .get(header::ACCESS_CONTROL_ALLOW_METHODS)
                .is_some()
        );
        // The MCP transport headers must be allowed even though the default
        // CORS `allowed_headers` omits them, or the browser blocks the POST.
        let allow_headers = resp
            .headers()
            .get(header::ACCESS_CONTROL_ALLOW_HEADERS)
            .and_then(|v| v.to_str().ok())
            .unwrap()
            .to_ascii_lowercase();
        assert!(
            allow_headers.contains("mcp-protocol-version"),
            "allow-headers missing MCP-Protocol-Version: {allow_headers}"
        );

        // Non-allowlisted origin → no CORS grant (browser will block the POST).
        let mut headers = HeaderMap::new();
        headers.insert(header::ORIGIN, "https://evil.example".parse().unwrap());
        let resp = serve_mcp_options(axum::extract::Extension(s), headers).await;
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        assert!(
            resp.headers()
                .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .is_none()
        );
    }

    #[test]
    fn initialize_negotiates_supported_protocol_version() {
        let s = server(Vec::new());
        // A supported version is echoed back.
        let echoed = initialize_result(&s, &json!({ "protocolVersion": "2024-11-05" }));
        assert_eq!(echoed["protocolVersion"], "2024-11-05");
        // An unsupported version falls back to the server's newest.
        let fallback = initialize_result(&s, &json!({ "protocolVersion": "3999-01-01" }));
        assert_eq!(fallback["protocolVersion"], DEFAULT_PROTOCOL_VERSION);
    }
}
