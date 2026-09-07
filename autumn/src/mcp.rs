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
use std::fmt::Write as _;
use std::pin::Pin;
use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
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
#[allow(clippy::struct_excessive_bools)] // independent dispatch flags mirroring ApiDoc metadata
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
    /// True for a tool derived from an empty-body-contract route (declared
    /// 204/205, no response schema). Dispatch enforces the contract by
    /// returning empty text on success, so a route that mislabels its status
    /// (e.g. an HTML handler tagged `status = 204`) can't leak its body into
    /// the tool result.
    empty_body: bool,
    /// The handler's build-time authority envelope (#1691), or `None` for an
    /// ungoverned tool. Drives the invocation's audit record and the
    /// reversibility-derived `destructiveHint`.
    agent_authority: Option<&'static crate::agent_authority::AgentAuthority>,
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
    /// Whether a [`LoadShedLayer`](crate::middleware::LoadShedLayer) wraps the
    /// `/mcp` envelope (true iff `server.max_concurrent_requests` is
    /// configured). The dispatch clone (cloned from the already-middleware-
    /// wrapped router) carries the SAME shared layer instance, so a
    /// `tools/call` is counted once at the envelope; its replayed dispatch is
    /// marked [`LoadShedExempt`](crate::middleware::LoadShedExempt) to avoid
    /// consuming a second slot for the same logical request.
    pub envelope_load_shed: bool,
    /// The application state, so `tools/call` can reach the installed
    /// [`AuditLogger`](crate::audit::AuditLogger) and the injected entropy seam
    /// without any per-handler wiring (#1691).
    pub state: crate::state::AppState,
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
    /// Whether the `/mcp` envelope is load-shed gated; gates exempting the
    /// replayed `tools/call` dispatch from double-counting against the same
    /// shared `LoadShedLayer` instance.
    envelope_load_shed: bool,
    /// The application state. `tools/call` writes its agent-authority audit
    /// records through this (#1691); an app with no `AuditLogger` installed
    /// makes those writes a no-op, so nothing here is conditional on one.
    state: crate::state::AppState,
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
/// `pub(crate)` for one reason: `agent_authority::manifest` carries an
/// unconditional copy of this rule (it must report the agent surface even when
/// the `mcp` feature is off), and a unit test there pins the copy equal to this
/// original so the two cannot drift.
///
/// Eligibility (JSON-out) gates everything: HTML/Maud routes have no response
/// schema and are silently ineligible. On top of that:
/// * `mcp_exclude` always wins.
/// * an explicit `mcp` opt-in always exposes (any verb).
/// * under the whole-API hatch, un-tagged `GET`s are auto-included but
///   mutating verbs are not.
pub(crate) fn should_expose(doc: &ApiDoc, expose_all: bool) -> bool {
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
    // Exception: a status whose body is empty *by contract* (e.g. the
    // repository macro's generated DELETE returning 204) is structurally
    // distinct from an HTML route's schema-less 200-with-body. It stays
    // eligible, and dispatch enforces the empty result (see `call_tool`).
    if doc.response.is_none() && !has_empty_body_contract(doc.success_status) {
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

/// A declared success status whose body is empty *by contract* (RFC 9110):
/// `204 No Content` and `205 Reset Content`. For these, a missing response
/// schema is the deliberate shape of the endpoint, not the signal of an
/// HTML/Maud route. Shared by [`should_expose`] and the warn/skip gate in
/// [`derive_tools`] so the exemption list lives in exactly one place.
const fn has_empty_body_contract(status: u16) -> bool {
    matches!(status, 204 | 205)
}

/// MCP safety annotations for a derived tool.
///
/// `readOnlyHint` is a statement about the HTTP verb and nothing else.
///
/// `destructiveHint` takes the verb as a **floor**, never as a fallback the
/// grant can talk its way under. A declared
/// [`Reversibility`](crate::agent_authority::Reversibility) can only ever *add*
/// the warning: it raises a `POST`/`PATCH` that the verb alone says nothing
/// about, and it cannot clear the one `DELETE` already carries.
///
/// That asymmetry is deliberate. `reversible` means the compiler proved the
/// effect set is bounded writes only — it does **not** mean the application can
/// put the row back, and nothing here checks for soft-delete or versioning. An
/// MCP client skips its confirmation prompt on `destructiveHint: false`, so
/// letting one unproved adjective in a grant clear a `DELETE`'s warning would
/// trade a real signal for an unverified claim (issue #1691 review, P2-1).
fn annotations_for(
    method: &str,
    title: &str,
    authority: Option<&'static crate::agent_authority::AgentAuthority>,
) -> Value {
    use crate::agent_authority::Reversibility;

    let upper = method.to_ascii_uppercase();
    let read_only = is_read_only(&upper);
    let mut obj = serde_json::Map::new();
    obj.insert("title".into(), json!(title));
    obj.insert("readOnlyHint".into(), json!(read_only));
    // DELETE is the destructive verb; flag it so agents/UIs can warn.
    let verb_is_destructive = upper == "DELETE";
    if let Some(authority) = authority {
        let destructive =
            authority.grant.reversibility != Reversibility::Reversible || verb_is_destructive;
        obj.insert("destructiveHint".into(), json!(destructive));
    } else if verb_is_destructive {
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
fn build_input_schema(
    doc: &ApiDoc,
    components: &serde_json::Map<String, Value>,
    index: &crate::openapi::SchemaComponentIndex,
) -> Value {
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
        let schema = rewrite_refs(schema_entry_to_value(query, index), components, &mut defs);
        properties.insert("query".to_owned(), schema);
    }

    if let Some(body) = &doc.request_body {
        let schema = rewrite_refs(schema_entry_to_value(body, index), components, &mut defs);
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

/// A build-time quality problem detected in a tool's derived `inputSchema`.
///
/// Surfaced as `tracing::warn` from [`derive_tools`] (mirroring the existing
/// ineligible-route warning) so an author sees it when assembling `/mcp`,
/// rather than a runtime surprise when an LLM client calls the tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SchemaDegradation {
    /// The `query` property resolved to a bare `{"type":"object"}` placeholder
    /// (the arg type has no `OpenApiSchema`), so the tool advertises no query
    /// fields.
    OpaqueQuery,
    /// The `body` property resolved to a bare `{"type":"object"}` placeholder,
    /// so the tool advertises no body fields.
    OpaqueBody,
}

/// Resolve a schema node through a single `#/$defs/X` indirection (the form
/// [`rewrite_refs`] inlines), returning the node itself when it is not a ref.
fn resolve_local_ref<'a>(root: &'a Value, node: &'a Value) -> &'a Value {
    if let Some(name) = node
        .get("$ref")
        .and_then(Value::as_str)
        .and_then(|r| r.strip_prefix("#/$defs/"))
        && let Some(def) = root.get("$defs").and_then(|defs| defs.get(name))
    {
        return def;
    }
    node
}

/// Inspect a built `inputSchema` for the degradations in [`SchemaDegradation`].
///
/// Pure and self-contained (no logging) so it is unit-testable; [`derive_tools`]
/// turns the results into `tracing::warn` lines with route context.
fn detect_schema_degradations(
    input_schema: &Value,
    has_query: bool,
    has_body: bool,
) -> Vec<SchemaDegradation> {
    let mut out = Vec::new();
    let props = input_schema.get("properties");

    if has_body
        && let Some(body) = props.and_then(|p| p.get("body"))
        && crate::openapi::is_opaque_object_schema(resolve_local_ref(input_schema, body))
    {
        out.push(SchemaDegradation::OpaqueBody);
    }

    // A nested/array query field is NOT a degradation: `Query<T>` decodes the
    // bracketed dialect `build_request` renders it into (issue #1972), so the
    // advertised structure round-trips. Only a missing field-level schema is.
    if has_query
        && let Some(query) = props.and_then(|p| p.get("query"))
        && crate::openapi::is_opaque_object_schema(resolve_local_ref(input_schema, query))
    {
        out.push(SchemaDegradation::OpaqueQuery);
    }

    out
}

/// Emit a `tracing::warn` for each degradation in a tool's derived
/// `inputSchema`, naming the tool and the recommended fix (issue #1972).
fn warn_on_degraded_input_schema(doc: &ApiDoc, input_schema: &Value) {
    for degradation in detect_schema_degradations(
        input_schema,
        doc.query_schema.is_some(),
        doc.request_body.is_some(),
    ) {
        match degradation {
            SchemaDegradation::OpaqueBody => tracing::warn!(
                operation_id = doc.operation_id,
                method = doc.method,
                path = doc.path,
                "MCP tool body has no field-level schema (opaque object); derive or \
                 impl `OpenApiSchema` on the `Json<T>` body type so the tool advertises \
                 its real fields instead of a bare `{{\"type\":\"object\"}}` placeholder"
            ),
            SchemaDegradation::OpaqueQuery => tracing::warn!(
                operation_id = doc.operation_id,
                method = doc.method,
                path = doc.path,
                "MCP tool query has no field-level schema (opaque object); derive or \
                 impl `OpenApiSchema` on the `Query<T>` type so the tool advertises \
                 its real query parameters instead of a bare `{{\"type\":\"object\"}}` placeholder"
            ),
        }
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
    // Resolve `$ref` component keys through the exact same identity→display-key
    // mapping the OpenAPI generator used, so a tool's inlined `$defs` refs match
    // the served component names even when two types share a last segment
    // (issue #1972).
    let index = crate::openapi::build_schema_component_index(&refs);

    let mut tools = Vec::new();
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for doc in docs {
        // Surface the "opted in but ineligible" case as a build-time note.
        // Streaming tools legitimately have no JSON response schema, and an
        // empty-body status (204/205) makes the missing schema the route's
        // contract, so both are exempt from this "missing response" warn/skip.
        if (doc.mcp_tool || (expose_all && is_read_only(doc.method)))
            && doc.response.is_none()
            && !has_empty_body_contract(doc.success_status)
            && !doc.mcp_stream
            && !doc.mcp_exclude
            && !doc.hidden
        {
            tracing::warn!(
                operation_id = doc.operation_id,
                method = doc.method,
                path = doc.path,
                "skipping MCP exposure: endpoint has no JSON response schema; \
                 eligible tools return Json<T>, declare an empty-body status \
                 (204/205), or opt in as streaming (`stream`/`Route::mcp_stream`) \
                 — HTML/Maud routes are not eligible"
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
        let input_schema = build_input_schema(doc, &components, &index);
        warn_on_degraded_input_schema(doc, &input_schema);
        tools.push(McpToolInfo {
            name: doc.operation_id.to_owned(),
            description: doc.description.or(doc.summary).map(str::to_owned),
            input_schema,
            annotations: annotations_for(doc.method, title, doc.agent_authority),
            method: doc.method.to_owned(),
            path_template: doc.path.to_owned(),
            path_params: doc.path_params.iter().map(|p| (*p).to_owned()).collect(),
            has_body: doc.request_body.is_some(),
            has_query: doc.query_schema.is_some(),
            streams: doc.mcp_stream,
            empty_body: doc.response.is_none()
                && has_empty_body_contract(doc.success_status)
                && !doc.mcp_stream,
            agent_authority: doc.agent_authority,
        });
    }
    tools
}

/// Public, transport-agnostic description of a derived tool. Returned by
/// [`derive_tools`] and consumed by the framework when assembling the MCP
/// endpoint router.
#[derive(Clone, Debug)]
#[allow(clippy::struct_excessive_bools)] // independent dispatch flags mirroring ApiDoc metadata
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
    empty_body: bool,
    agent_authority: Option<&'static crate::agent_authority::AgentAuthority>,
}

impl McpToolInfo {
    /// Tool name advertised in `tools/list` (the route's operation id).
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Human-readable description derived from the route's
    /// `description`/`summary`.
    #[must_use]
    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    /// JSON Schema for the tool's arguments, built from the handler's typed
    /// contract (path params, query, JSON body).
    #[must_use]
    pub const fn input_schema(&self) -> &Value {
        &self.input_schema
    }

    /// MCP safety annotations (`readOnlyHint`, `destructiveHint`, `title`)
    /// derived from the HTTP verb.
    #[must_use]
    pub const fn annotations(&self) -> &Value {
        &self.annotations
    }

    /// HTTP method the tool dispatches with.
    #[must_use]
    pub fn method(&self) -> &str {
        &self.method
    }

    /// Route path template the tool dispatches to (e.g. `/api/todos/{id}`).
    #[must_use]
    pub fn path_template(&self) -> &str {
        &self.path_template
    }

    /// Whether this is a streaming (`Sse`) tool.
    #[must_use]
    pub const fn streams(&self) -> bool {
        self.streams
    }

    /// The build-time authority envelope proved for the handler behind this
    /// tool by `#[agent_operable(grant = ...)]` (issue #1691), or `None` for an
    /// **ungoverned** tool — one exposed to agents with no grant declared.
    ///
    /// Copied verbatim from [`ApiDoc::agent_authority`], so a tool is governed
    /// exactly when its handler is. `tools/call` reads it to record the
    /// compile-known grant and reversibility on the invocation's audit events,
    /// and the manifest reads it to tell `actions` from `ungoverned_tools`.
    #[must_use]
    pub const fn agent_authority(&self) -> Option<&'static crate::agent_authority::AgentAuthority> {
        self.agent_authority
    }
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
                empty_body: t.empty_body,
                agent_authority: t.agent_authority,
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
            envelope_load_shed: wiring.envelope_load_shed,
            state: wiring.state,
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
/// Header name for the `Server-Timing` response header (#1348), forwarded from
/// the dispatch clone onto the rebuilt JSON-RPC response.
static SERVER_TIMING: HeaderName = HeaderName::from_static("server-timing");

struct ReplayContext<'a> {
    headers: &'a HeaderMap,
    identity: Option<&'a crate::security::ResolvedClientIdentity>,
    peer: Option<std::net::SocketAddr>,
}

/// Reject an untrusted `Host`/`:authority` or a disallowed browser `Origin`
/// before the request body is buffered.
///
/// The `/mcp` envelope is merged after `apply_middleware`, so it does not pass
/// through the global `TrustedHostLayer` in [`crate::router`] that every direct
/// route runs; this layer restores that gate for the endpoint. Running as a
/// layer (rather than inside `serve_mcp`) means a bad-`Host` request is rejected
/// before the handler's `Bytes` extractor reads up to the configured
/// `max_request_size_bytes`, exactly as a direct route rejects in middleware
/// before handler extraction.
///
/// Host resolution mirrors `TrustedHostService`: the proxy-resolved
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
    // mirroring how direct routes reject in `TrustedHostService` before
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

// ──────────────────────────────────────────────────────────────────
// Agent-authority audit (#1691)
// ──────────────────────────────────────────────────────────────────

/// Most effects recorded on an agent audit event. An action with more than this
/// is already far outside what a reviewer reads inline; the manifest holds the
/// complete set.
const MAX_AUDITED_EFFECTS: usize = 16;

/// Byte cap on the `argument_names` metadata value, so a pathological argument
/// object cannot bloat every row it touches.
const MAX_ARGUMENT_NAMES_BYTES: usize = 512;

/// Actor recorded when no principal was resolved for the call. Never empty: an
/// audit row that cannot name who acted is still a row that must be greppable.
const ANONYMOUS_AGENT: &str = "agent:anonymous";

/// The tool error returned when a non-reversible action cannot be recorded.
const UNAUDITABLE_REFUSAL: &str =
    "audit attempt record could not be written; refusing a non-reversible action";

/// Deadline for the pre-dispatch audit write.
///
/// This write sits inline on the request path, ahead of the handler, so a sink
/// that hangs — a saturated DB pool, a remote collector that accepted the
/// connection and went quiet — would otherwise stall every `tools/call` until
/// the envelope's `request_timeout_ms` fires. Expiry is treated as a write
/// failure, which means a non-reversible action is refused rather than served
/// unrecorded (issue #1691 review, P3-6).
const AUDIT_WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Everything both of an invocation's audit events share, resolved once before
/// dispatch.
///
/// `Clone` so a streaming projection's drop guard can hand a copy to a spawned
/// task: `Drop` cannot await, and the outcome of an aborted stream still has to
/// reach the sink.
///
/// Two events are written per `tools/call` — `agent.tool.<name>.attempt` before
/// dispatch and `agent.tool.<name>` after it — because an invocation that never
/// returns (a panic, a hang, a killed process) must still leave evidence that
/// an agent reached for the action. They share a correlation id, minted here so
/// it exists before anything can go wrong.
#[derive(Clone)]
struct AgentAudit {
    tool: String,
    path_template: String,
    correlation_id: String,
    reversibility: Option<crate::agent_authority::Reversibility>,
    grant: Option<&'static str>,
    actor: String,
    ip: Option<std::net::IpAddr>,
    /// Metadata common to both events. Deliberately built once: the pair must
    /// agree field for field, or joining them is guesswork.
    shared: std::collections::BTreeMap<String, String>,
}

impl AgentAudit {
    /// Resolve the invocation's audit context from the tool's compile-time
    /// authority and the call's *shape*.
    fn new(server: &McpServer, tool: &McpTool, arguments: &Value, ctx: &ReplayContext<'_>) -> Self {
        let authority = tool.agent_authority;
        let grant = authority.map(|a| a.grant.name);
        let reversibility = authority.map(|a| a.grant.reversibility);
        let correlation_id = server.state.entropy().uuid_v4().to_string();

        let mut shared = std::collections::BTreeMap::new();
        shared.insert("correlation_id".to_owned(), correlation_id.clone());
        shared.insert("transport".to_owned(), "mcp".to_owned());
        shared.insert("tool".to_owned(), tool.name.clone());
        if let Some(grant) = grant {
            shared.insert("grant".to_owned(), grant.to_owned());
        }
        // "unknown" rather than a guess: an ungoverned tool's blast radius is
        // exactly what nobody has established, and calling it `reversible`
        // would be the one wrong answer.
        shared.insert(
            "reversibility".to_owned(),
            reversibility.map_or_else(|| "unknown".to_owned(), |r| r.as_str().to_owned()),
        );
        if let Some(authority) = authority {
            shared.insert("effects".to_owned(), render_effects(authority.effects));
        }
        shared.insert("argument_names".to_owned(), argument_names(tool, arguments));

        Self {
            tool: tool.name.clone(),
            path_template: tool.path_template.clone(),
            correlation_id,
            reversibility,
            grant,
            // The principal the *envelope* resolved, if any. `Current` is set by
            // the auth layer on the request scope, so a `secure_mcp` app names
            // its caller and an open one falls back rather than recording "".
            actor: crate::current::Current::scoped_actor()
                .unwrap_or_else(|| ANONYMOUS_AGENT.to_owned()),
            ip: ctx.peer.map(|peer| peer.ip()),
            shared,
        }
    }

    /// The extension handlers read via `Extension<AgentInvocation>`.
    fn invocation(&self) -> crate::agent_authority::AgentInvocation {
        crate::agent_authority::AgentInvocation {
            correlation_id: self.correlation_id.clone(),
            tool: self.tool.clone(),
            grant: self.grant,
            reversibility: self.reversibility,
        }
    }

    /// Whether a lost audit record is grounds for refusing the action.
    ///
    /// Only a *proved* `reversible` grant earns the benefit of the doubt.
    /// Ungoverned (`None`) counts as non-reversible: the whole point of the
    /// unknown marker is that we do not get to assume.
    fn refuse_when_unauditable(&self) -> bool {
        self.reversibility != Some(crate::agent_authority::Reversibility::Reversible)
    }

    /// Build one of this invocation's events.
    ///
    /// `phase` is what tells the three rows apart once they are in the sink:
    /// `attempt` is written before anything runs, `outcome` after the handler
    /// returned, and `refused` when the attempt could not be recorded and the
    /// action was therefore not taken. Without it an operator filtering on
    /// `status` alone cannot distinguish "an attempt was logged" from "the call
    /// succeeded" — the attempt is deliberately `Success` (the *write*
    /// succeeded; nothing has been tried yet) — nor a refusal from a 5xx.
    fn event(
        &self,
        action: String,
        status: crate::audit::AuditStatus,
        phase: &str,
    ) -> crate::audit::AuditEvent {
        let mut event = crate::audit::AuditEvent::new(
            self.actor.clone(),
            action,
            self.path_template.clone(),
            self.ip,
            status,
        );
        event.metadata = self.shared.clone();
        event.metadata.insert("phase".to_owned(), phase.to_owned());
        event
    }

    /// Write one event under [`AUDIT_WRITE_TIMEOUT`], flattening an expiry into
    /// the same `Err` shape a sink failure produces.
    ///
    /// Takes the state rather than the server because the streaming drop guard
    /// outlives the borrow of `McpServer` — it writes from a spawned task.
    async fn write_bounded(
        state: &crate::state::AppState,
        event: crate::audit::AuditEvent,
    ) -> Result<(), String> {
        match tokio::time::timeout(
            AUDIT_WRITE_TIMEOUT,
            crate::audit::write_from_state(state, event),
        )
        .await
        {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(error.to_string()),
            Err(_elapsed) => Err(format!(
                "audit sink did not respond within {AUDIT_WRITE_TIMEOUT:?}"
            )),
        }
    }

    /// The fields every `autumn.agent` line carries, so the attempt and the
    /// outcome lines describe one invocation in the same vocabulary.
    fn reversibility_field(&self) -> Option<&str> {
        self.shared.get("reversibility").map(String::as_str)
    }

    /// Record the attempt, before anything has run.
    ///
    /// `Err` carries the tool-error text: the record could not be written and
    /// the action is not reversible, so it must not happen at all.
    async fn record_attempt(&self, state: &crate::state::AppState) -> Result<(), &'static str> {
        let event = self.event(
            format!("agent.tool.{}.attempt", self.tool),
            // The *write* is what succeeded here; `phase` carries the fact that
            // nothing has been attempted yet.
            crate::audit::AuditStatus::Success,
            "attempt",
        );
        let Err(error) = Self::write_bounded(state, event).await else {
            // Mirrored to the trace so an app with no sink installed still sees
            // both halves of every invocation, not just the outcome.
            tracing::info!(
                target: "autumn.agent",
                tool = %self.tool,
                correlation_id = %self.correlation_id,
                transport = "mcp",
                phase = "attempt",
                grant = self.grant,
                reversibility = self.reversibility_field(),
                effects = self.shared.get("effects").map(String::as_str),
                argument_names = self.shared.get("argument_names").map(String::as_str),
                actor = %self.actor,
                target = %self.path_template,
                "agent tool call attempted"
            );
            return Ok(());
        };
        if self.refuse_when_unauditable() {
            tracing::error!(
                target: "autumn.agent",
                tool = %self.tool,
                correlation_id = %self.correlation_id,
                transport = "mcp",
                phase = "refused",
                grant = self.grant,
                reversibility = self.reversibility_field(),
                actor = %self.actor,
                target = %self.path_template,
                error = %error,
                "refusing an agent tool call: its attempt record could not be written \
                 and the action is not reversible"
            );
            self.record_refusal(state, &error).await;
            return Err(UNAUDITABLE_REFUSAL);
        }
        tracing::warn!(
            target: "autumn.agent",
            tool = %self.tool,
            correlation_id = %self.correlation_id,
            phase = "attempt",
            error = %error,
            "agent tool attempt record could not be written; the action is reversible, \
             so the call proceeds"
        );
        Ok(())
    }

    /// Best-effort record that the invocation was refused for want of an audit
    /// trail.
    ///
    /// The write that just failed was an aggregate across every configured sink
    /// ([`AuditLogger::write`](crate::audit::AuditLogger::write) attempts them
    /// all and joins the errors), so one broken sink fails the whole call —
    /// while the healthy ones would happily have taken the row. Retrying here
    /// gives them the refusal, which is the single most interesting thing that
    /// happened. Its own failure is ignored on purpose: there is nowhere left to
    /// report it, the `tracing::error!` above already fired, and the refusal
    /// itself does not depend on this landing.
    async fn record_refusal(&self, state: &crate::state::AppState, error: &str) {
        let mut event = self.event(
            // Its own action, not the outcome action with a different `phase`:
            // a refusal and a completed call are different things, and an
            // operator filtering for one should not have to parse metadata to
            // exclude the other.
            format!("agent.tool.{}.refused", self.tool),
            crate::audit::AuditStatus::Failure,
            "refused",
        );
        // No `http_status`: nothing was dispatched.
        event
            .metadata
            .insert("refused_reason".to_owned(), error.to_owned());
        let _ = Self::write_bounded(state, event).await;
    }

    /// Record how the invocation ended. Never alters the tool result: a broken
    /// sink is an operational problem, not a reason to fail a call that already
    /// happened.
    async fn record_outcome(
        &self,
        state: &crate::state::AppState,
        status: StatusCode,
        request_id: Option<&str>,
        disposition: Disposition,
    ) {
        // The status line alone is not the verdict: a streaming tool answers
        // `200` before it has done anything, and a buffered tool can answer
        // `200` and then fail to hand its body over. `disposition` carries what
        // actually became of the action.
        let succeeded = status.is_success() && disposition.is_success();
        let audit_status = if succeeded {
            crate::audit::AuditStatus::Success
        } else {
            crate::audit::AuditStatus::Failure
        };
        let mut event = self.event(format!("agent.tool.{}", self.tool), audit_status, "outcome");
        event
            .metadata
            .insert("http_status".to_owned(), status.as_u16().to_string());
        if let Some(stream) = disposition.stream_state() {
            event
                .metadata
                .insert("stream_state".to_owned(), stream.to_owned());
        }
        if let Some(result) = disposition.result() {
            event
                .metadata
                .insert("result".to_owned(), result.to_owned());
        }
        // The replayed pipeline's own id, so an audit row joins to the access
        // log line and the traces for the same dispatch.
        if let Some(request_id) = request_id {
            event
                .metadata
                .insert("request_id".to_owned(), request_id.to_owned());
        }
        // Emitted whether or not a sink is installed, so an app that configured
        // none still has a trace of every agent action it served.
        tracing::info!(
            target: "autumn.agent",
            tool = %self.tool,
            correlation_id = %self.correlation_id,
            transport = "mcp",
            phase = "outcome",
            grant = self.grant,
            reversibility = self.reversibility_field(),
            effects = self.shared.get("effects").map(String::as_str),
            argument_names = self.shared.get("argument_names").map(String::as_str),
            actor = %self.actor,
            target = %self.path_template,
            http_status = status.as_u16(),
            stream_state = disposition.stream_state(),
            result = disposition.result(),
            request_id = request_id,
            "agent tool call"
        );
        // Bounded like the attempt write: the handler has already run, so a
        // hanging sink here cannot change what happened — but it can still hold
        // the response open, and the caller is owed one.
        if let Err(error) = Self::write_bounded(state, event).await {
            tracing::warn!(
                target: "autumn.agent",
                tool = %self.tool,
                correlation_id = %self.correlation_id,
                phase = "outcome",
                error = %error,
                "agent tool outcome record could not be written"
            );
        }
    }
}

/// Why the buffered path could not hand the handler's body to the agent.
///
/// The buffered branch answers the agent from a body it has to read *after* the
/// handler's status is known, so a `200` is not yet proof the call succeeded
/// (issue #1691 review round 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BufferedFailure {
    /// The body exceeded [`MAX_TOOL_RESPONSE_BYTES`].
    Overflow,
    /// The body stream itself errored part-way through.
    BodyError,
}

impl BufferedFailure {
    /// Classify what [`axum::body::to_bytes`] returned.
    ///
    /// It collects through `http_body_util::Limited`, so a `LengthLimitError`
    /// anywhere in the source chain means the cap was hit; anything else is a
    /// transport failure on the body itself.
    fn classify(error: &axum::Error) -> Self {
        let mut source: Option<&(dyn std::error::Error + 'static)> = Some(error);
        while let Some(err) = source {
            if err.is::<http_body_util::LengthLimitError>() {
                return Self::Overflow;
            }
            source = err.source();
        }
        Self::BodyError
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Overflow => "body_overflow",
            Self::BodyError => "body_error",
        }
    }
}

/// What became of an invocation once its HTTP status was known.
///
/// The status line is necessary but not sufficient: both the streaming and the
/// buffered branch can still fail the agent after a `200`, and an audit row
/// that only reflected the status would claim a success neither delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Disposition {
    /// The handler answered and its result reached the agent unaltered — the
    /// status line is the whole story.
    Settled,
    /// A streaming projection ended this way.
    Stream(StreamState),
    /// The buffered path could not deliver the handler's body.
    Buffered(BufferedFailure),
}

impl Disposition {
    /// Whether this disposition permits the outcome to be recorded as a
    /// success (the HTTP status still has to agree).
    const fn is_success(self) -> bool {
        match self {
            Self::Settled => true,
            Self::Stream(state) => state.is_success(),
            Self::Buffered(_) => false,
        }
    }

    const fn stream_state(self) -> Option<&'static str> {
        match self {
            Self::Stream(state) => Some(state.as_str()),
            _ => None,
        }
    }

    const fn result(self) -> Option<&'static str> {
        match self {
            Self::Buffered(failure) => Some(failure.as_str()),
            _ => None,
        }
    }
}

/// How a streaming tool's projection ended.
///
/// A streaming handler returns `200` before it has produced anything, so the
/// status line says nothing about whether the work happened. Recording the
/// outcome up front would durably claim success for a stream that then errored
/// or was cut off by a client disconnect, with no later event to correct it
/// (issue #1691 review round 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamState {
    /// The handler's body ended normally: everything it meant to emit, it
    /// emitted.
    Completed,
    /// The projection was dropped before the handler's body ended — a client
    /// disconnect, or the response being discarded mid-flight.
    Aborted,
    /// The handler's body yielded a transport error part-way through.
    Errored,
}

impl StreamState {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Aborted => "aborted",
            Self::Errored => "errored",
        }
    }

    /// Only a stream that ran to the end counts as a successful action.
    const fn is_success(self) -> bool {
        matches!(self, Self::Completed)
    }
}

/// Owns the one outcome record a streaming `tools/call` still owes.
///
/// The record is written when the projection reaches a terminal state — which
/// may be "never", if the client hangs up. `Drop` is therefore the backstop:
/// it cannot await, so it spawns the write instead. `recorded` makes the two
/// paths mutually exclusive, so exactly one outcome reaches the sink per call.
struct StreamAudit {
    audit: AgentAudit,
    state: crate::state::AppState,
    status: StatusCode,
    request_id: Option<String>,
    recorded: bool,
}

impl StreamAudit {
    /// Record the outcome inline, from the projection's own async context.
    async fn record(&mut self, stream: StreamState) {
        if self.recorded {
            return;
        }
        self.recorded = true;
        self.audit
            .record_outcome(
                &self.state,
                self.status,
                self.request_id.as_deref(),
                Disposition::Stream(stream),
            )
            .await;
    }
}

impl Drop for StreamAudit {
    fn drop(&mut self) {
        if self.recorded {
            return;
        }
        self.recorded = true;
        // The projection never reached an end: the response body was dropped
        // while the handler still had output to give. That is an aborted
        // action, and the trail has to say so rather than stay silent.
        let audit = self.audit.clone();
        let state = self.state.clone();
        let status = self.status;
        let request_id = self.request_id.clone();
        // `try_current` rather than `spawn`: during runtime shutdown there is
        // no reactor left to spawn onto, and a panic in `Drop` would be far
        // worse than a missing row on a process that is already going away.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                audit
                    .record_outcome(
                        &state,
                        status,
                        request_id.as_deref(),
                        Disposition::Stream(StreamState::Aborted),
                    )
                    .await;
            });
        }
    }
}

/// Render an authority's proved effects as `kind:subject`, comma-joined.
fn render_effects(effects: &[crate::agent_authority::Effect]) -> String {
    let mut out = String::new();
    for effect in effects.iter().take(MAX_AUDITED_EFFECTS) {
        if !out.is_empty() {
            out.push(',');
        }
        out.push_str(effect.kind.as_str());
        out.push(':');
        out.push_str(effect.subject);
    }
    if effects.len() > MAX_AUDITED_EFFECTS {
        out.push_str(",…");
    }
    out
}

/// The call's top-level argument **names**, sorted and comma-joined, followed
/// by a count of the ones this tool does not recognise.
///
/// Names only, never values: an audit sink is a long-lived, widely-readable
/// store, and the arguments of an agent call routinely carry exactly the
/// payloads that must not be duplicated into one. The shape of a call is what
/// makes a row reviewable; its contents are the handler's business.
///
/// Only names drawn from the tool's own argument surface — the reserved
/// `body`/`query` keys and its declared path params — are echoed verbatim.
/// Everything else is counted, never quoted (`body,id,+2 unknown`).
///
/// The caller chooses those keys and nothing validates them against the
/// advertised schema, so an un-intersected name list is an
/// attacker-controlled string landing in a durable audit row *and* in the
/// `autumn.agent` trace line, where an embedded newline forges a log entry and
/// a key like `ssn-123-45-6789` smuggles the very payload the names-only rule
/// exists to keep out (issue #1691 review, P2-2). Counting the unknowns keeps
/// the useful signal — "this call carried arguments the tool ignores" — without
/// reproducing a single byte the caller wrote.
fn argument_names(tool: &McpTool, arguments: &Value) -> String {
    let Some(object) = arguments.as_object() else {
        return String::new();
    };

    // The keys `build_request` actually reads. Catch-all params (`{*rest}`)
    // surface from `ApiDoc` with a leading `*` but are addressed by the bare
    // name, exactly as `build_request` strips it.
    let is_known = |name: &str| {
        name == "body"
            || name == "query"
            || tool
                .path_params
                .iter()
                .any(|param| param.strip_prefix('*').unwrap_or(param) == name)
    };

    let mut known: Vec<&str> = Vec::new();
    let mut unknown: usize = 0;
    for name in object.keys() {
        if is_known(name) {
            known.push(name);
        } else {
            unknown += 1;
        }
    }
    known.sort_unstable();

    let mut out = String::new();
    for name in known {
        // +1 for the separator this name would need.
        if out.len() + name.len() + 1 > MAX_ARGUMENT_NAMES_BYTES {
            out.push_str(",…");
            break;
        }
        if !out.is_empty() {
            out.push(',');
        }
        out.push_str(name);
    }
    if unknown > 0 {
        if !out.is_empty() {
            out.push(',');
        }
        // A count, not the names: the whole point is that these are the ones
        // nobody vouched for.
        let _ = write!(out, "+{unknown} unknown");
    }
    out
}

/// Carry the envelope's context onto the request the `tools/call` replays.
///
/// Everything here exists because the replay traverses the *real* pipeline: it
/// has to look to that pipeline like the direct HTTP call it stands in for,
/// while not being double-charged for work the envelope already did.
fn apply_replay_extensions(
    request: &mut axum::extract::Request,
    server: &McpServer,
    ctx: &ReplayContext<'_>,
    audit: &AgentAudit,
) {
    let extensions = request.extensions_mut();

    // Inserted for EVERY tool — a handler is entitled to know it is being
    // driven by an agent whether or not anyone declared a grant for it.
    extensions.insert(audit.invocation());

    // The caller's resolved identity and connection peer, so the dispatch
    // pipeline attributes the call like a direct request would — the
    // proxy-aware tenancy host and the IP-keyed rate limiter both read these.
    if let Some(identity) = ctx.identity {
        extensions.insert(identity.clone());
    }
    if let Some(peer) = ctx.peer {
        extensions.insert(axum::extract::ConnectInfo(peer));
    }
    // When the `/mcp` envelope is itself rate-limited, this call was already
    // counted there; mark the replay envelope-counted so the framework-default
    // limiter (which shares the envelope bucket) doesn't charge a second token
    // for the same tool call. User/per-route limiters (path overrides,
    // `#[throttle]`) don't share that bucket and still charge the replay.
    if server.envelope_rate_limited {
        extensions.insert(crate::security::RateLimitEnvelopeCounted);
    }
    // Likewise for load shedding: the envelope and the dispatch pipeline share
    // the SAME `LoadShedLayer` instance (same `Arc` in-flight counter), so
    // without this a `tools/call` would acquire one slot at the envelope and a
    // second at this replay for the same logical request — silently halving the
    // effective ceiling for MCP traffic.
    if server.envelope_load_shed {
        extensions.insert(crate::middleware::LoadShedExempt);
    }
}

/// What must be read off a dispatched response *before* its body is consumed or
/// handed to the SSE projection.
///
/// Grouped because they share one deadline, not one purpose: once the body is
/// taken, the response is gone and none of these can be recovered.
struct Dispatched {
    status: StatusCode,
    /// The replayed pipeline's own `x-request-id`, so the audit row joins to
    /// the access log line for the same dispatch.
    request_id: Option<String>,
    /// Whether the handler answered with `text/event-stream`.
    is_event_stream: bool,
    /// Any `Set-Cookie` the inner handler or middleware set (session renewal,
    /// CSRF-cookie refresh, login), replayed onto the outer HTTP response so a
    /// tool call sends what the equivalent direct call would have.
    cookies: Vec<HeaderValue>,
}

impl Dispatched {
    fn inspect(response: &Response) -> Self {
        let headers = response.headers();
        Self {
            status: response.status(),
            request_id: headers
                .get("x-request-id")
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned),
            is_event_stream: headers
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|c| {
                    c.trim_start()
                        .to_ascii_lowercase()
                        .starts_with("text/event-stream")
                }),
            cookies: headers
                .get_all(header::SET_COOKIE)
                .iter()
                .cloned()
                .collect(),
        }
    }
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

    // Agent-authority envelope (#1691). Resolved before dispatch so the
    // correlation id exists ahead of anything that can go wrong.
    let audit = AgentAudit::new(server, tool, &arguments, ctx);
    apply_replay_extensions(&mut request, server, ctx, &audit);

    // The last thing before the action runs: if this record cannot be written
    // and the action is not provably reversible, it does not run.
    if let Err(refusal) = audit.record_attempt(&server.state).await {
        return json_response(&success(id, tool_error(refusal)));
    }

    let response = match server.dispatch.clone().oneshot(request).await {
        Ok(resp) => resp,
        Err(e) => {
            audit
                .record_outcome(
                    &server.state,
                    StatusCode::INTERNAL_SERVER_ERROR,
                    None,
                    Disposition::Settled,
                )
                .await;
            return json_response(&success(id, tool_error(&format!("dispatch failed: {e}"))));
        }
    };

    let Dispatched {
        status,
        request_id,
        is_event_stream,
        cookies,
    } = Dispatched::inspect(&response);
    let client_accepts_sse = ctx
        .headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(accept_includes_event_stream);

    // A streaming tool whose handler streamed (text/event-stream) and whose
    // client can read SSE: project the stream onto the MCP SSE channel. This is
    // the only path that escapes buffering; every other case (a buffered tool, a
    // streaming handler that errored before streaming, or a client that can't
    // read SSE) falls through to the buffered branch below — so the base #1117
    // path and non-SSE clients are entirely unaffected.
    if tool.streams && status.is_success() && is_event_stream && client_accepts_sse {
        // The outcome is NOT recorded here. A streaming handler answers `200`
        // before it has produced anything, so recording now would durably claim
        // success for a stream that then errors or is cut off by a client
        // disconnect — with no later event to correct it. The guard travels
        // with the projection and writes exactly one outcome when it reaches a
        // terminal state, or from its `Drop` if the client hangs up first.
        let stream_audit = StreamAudit {
            audit,
            state: server.state.clone(),
            status,
            request_id,
            recorded: false,
        };
        return stream_tool_result(id, &params, response, cookies, stream_audit);
    }

    // Buffered path: the outcome is recorded only once the body has actually
    // been read and packaged. A `200` here is not yet proof the agent got
    // anything — the body can still overflow the tool-result cap or error
    // mid-read, and the agent would then see a tool error while the audit row
    // claimed success (issue #1691 review round 2).
    let (resp, failure) =
        buffered_tool_response(tool, id, status, is_event_stream, cookies, response).await;
    let disposition = failure.map_or(Disposition::Settled, Disposition::Buffered);
    audit
        .record_outcome(&server.state, status, request_id.as_deref(), disposition)
        .await;
    resp
}

/// Repackage a fully-buffered handler response as the JSON-RPC tool result.
///
/// Split out of [`serve_tools_call`] so that function stays readable (and under
/// clippy's `too_many_lines` ceiling): everything here runs only on the
/// buffered path.
///
/// Returns the response to send plus, when the handler's body could not be
/// delivered at all, why — the caller records that as the invocation's outcome,
/// so a tool error never sits behind an audit row claiming success.
async fn buffered_tool_response(
    tool: &McpTool,
    id: Value,
    status: StatusCode,
    is_event_stream: bool,
    cookies: Vec<HeaderValue>,
    response: Response,
) -> (Response, Option<BufferedFailure>) {
    // Capture the dispatch clone's `Server-Timing` header (#1348) so its
    // non-`total` metrics can be forwarded onto the rebuilt response. The clone
    // runs the primary `ServerTimingLayer`, which builds the full metric set —
    // including `db;dur;desc="N queries"` for a DB-backed tool — but that inner
    // response is discarded when the JSON-RPC envelope is rebuilt below. Without
    // forwarding, a DB-backed `tools/call` would lose the query count. Only the
    // non-`total` metrics are forwarded (see the append below); the inner
    // `total` measured the dispatch clone alone and is dropped in favour of the
    // outer fallback's real `/mcp` `total`. Captured before this function
    // consumes the body, exactly as the caller captured `Set-Cookie`.
    let mut server_timings: Vec<HeaderValue> = Vec::new();
    for value in response.headers().get_all(&SERVER_TIMING) {
        server_timings.push(value.clone());
    }

    // Unlike a normal HTTP response (streamed straight to the socket), the MCP
    // path buffers the whole body to repackage it as a tool result. Cap that
    // buffer so a runaway handler can't OOM the process; report an overflow as
    // an explicit tool error rather than silently truncating to an empty body.
    let bytes = match axum::body::to_bytes(response.into_body(), MAX_TOOL_RESPONSE_BYTES).await {
        Ok(bytes) => bytes,
        Err(error) => {
            // The agent gets a tool error, so the audit row must record one
            // too: the call did not deliver what the handler produced.
            let failure = BufferedFailure::classify(&error);
            let message = match failure {
                BufferedFailure::Overflow => format!(
                    "handler response exceeded the {MAX_TOOL_RESPONSE_BYTES}-byte MCP tool-result limit"
                ),
                BufferedFailure::BodyError => {
                    format!("handler response body failed mid-read: {error}")
                }
            };
            return (
                json_response(&success(id, tool_error(&message))),
                Some(failure),
            );
        }
    };
    // A streaming handler buffered for a non-SSE client: collapse the SSE wire
    // frames into their concatenated data payload so the client still receives a
    // usable single result instead of raw `data:`-framed text.
    let text = if is_event_stream {
        collapse_sse_body(&bytes)
    } else {
        String::from_utf8_lossy(&bytes).into_owned()
    };

    let value = buffered_tool_result(tool, id, status, &text);
    let mut resp = json_response(&value);
    for cookie in cookies {
        resp.headers_mut().append(header::SET_COOKIE, cookie);
    }
    // Forward only the inner dispatch's non-`total` `Server-Timing` metrics
    // (e.g. `db;dur=…;desc="N queries"`) onto the rebuilt response, dropping the
    // inner `total`. That inner `total` measured only the dispatch clone; it was
    // captured before this endpoint buffered the body (`to_bytes`), collapsed an
    // SSE body for a non-SSE client, and repackaged the JSON-RPC envelope, so it
    // under-reports `/mcp` latency. By NOT marking the outer `ServerTimingEmitted`
    // sentinel, the fallback `ServerTimingLayer` appends the real `/mcp` `total`
    // (which brackets that buffering/serialization). Net result on a DB-backed
    // call: the fallback's true `total` plus the inner `db` metric, with exactly
    // one `total`. A non-DB call forwards nothing and the fallback emits
    // total-only.
    for timing in server_timings {
        if let Ok(value) = timing.to_str()
            && let Some(kept) = strip_total_metric(value)
            && let Ok(hv) = HeaderValue::from_str(&kept)
        {
            resp.headers_mut().append(SERVER_TIMING.clone(), hv);
        }
    }
    (resp, None)
}

/// Strip the `total` metric from a `Server-Timing` header value, returning the
/// remaining comma-separated metrics (e.g. `db;dur=…;desc="N queries"`), or
/// `None` when only `total` (or nothing usable) was present.
///
/// A `Server-Timing` value is a comma-separated list of metrics, each of the
/// form `name` optionally followed by `;`-separated parameters (`dur`, `desc`).
/// The metric name is the token before the first `;` or `=`. This keeps the
/// output W3C-valid and preserves any future non-`total` metrics the inner
/// pipeline emits.
fn strip_total_metric(value: &str) -> Option<String> {
    let kept: Vec<&str> = value
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .filter(|entry| {
            let name = entry.split([';', '=']).next().unwrap_or("").trim();
            !name.eq_ignore_ascii_case("total")
        })
        .collect();
    if kept.is_empty() {
        None
    } else {
        Some(kept.join(", "))
    }
}

/// Package a buffered handler response as the JSON-RPC tool result.
///
/// An empty-body-contract tool (declared 204/205, no response schema)
/// advertises "empty text on success". Enforce that here rather than
/// trusting the declaration: a handler whose real response carries a body
/// (e.g. an HTML route mislabeled `status = 204`) must not leak it into the
/// tool result. Discarding silently would hide the mislabel, so surface it.
fn buffered_tool_result(tool: &McpTool, id: Value, status: StatusCode, text: &str) -> Value {
    if !status.is_success() {
        return success(
            id,
            tool_error(&format!(
                "handler returned HTTP {}: {text}",
                status.as_u16()
            )),
        );
    }
    if tool.empty_body {
        if !text.is_empty() {
            tracing::warn!(
                tool = %tool.name,
                body_len = text.len(),
                "empty-body-contract tool returned a non-empty body; \
                 discarding it (the route's declared 204/205 status does \
                 not match what its handler actually returns)"
            );
        }
        return success(id, tool_ok(""));
    }
    success(id, tool_ok(text))
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
    /// The invocation's outstanding outcome record. Dropped with the rest of
    /// the projection when the client hangs up, which is how an aborted stream
    /// still gets a row (#1691).
    audit: StreamAudit,
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
    audit: StreamAudit,
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
        audit,
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
            ProjectionPhase::Streaming => match st.body.next().await {
                Some(Ok(bytes)) => {
                    let events = st.parser.push(&bytes);
                    enqueue_projected(&mut st, events);
                }
                // The handler's body is finished — cleanly (`None`) or with a
                // transport error (`Some(Err)`, the only `Some` left here).
                // Either way this is the action's terminal state, so the
                // outcome is recorded now rather than left to the drop guard:
                // whatever the client does with the remaining projection, the
                // handler has already done everything it was going to do.
                terminal => {
                    let outcome = if terminal.is_some() {
                        StreamState::Errored
                    } else {
                        StreamState::Completed
                    };
                    let trailing = st.parser.finish();
                    enqueue_projected(&mut st, trailing);
                    st.phase = ProjectionPhase::Final;
                    st.audit.record(outcome).await;
                }
            },
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
        let mut pairs = QueryPairs::new();
        for (key, value) in map {
            // The tool's own `query` property names get the same rule its
            // nested object fields do — a dynamic query object can carry an
            // arbitrary key here, and `filter[x]` would otherwise reach the
            // decoder as structure rather than as the name the caller sent.
            if !is_expressible_field_name(key) {
                return Err(inexpressible_field_name("query"));
            }
            encode_query_arg(key, value, 1, &mut pairs)?;
        }
        if !pairs.pairs.is_empty() {
            let qs = serde_urlencoded::to_string(&pairs.pairs)
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

/// Upper bound on the pairs one tool call may expand its `query` object into.
///
/// `tools/call` arguments are client-supplied and only bounded by the request
/// body limit (32 MiB by default), while building a bracketed key copies its
/// whole prefix per child — quadratic in the argument size. Both this and
/// [`MAX_QUERY_BYTES`] bail out long before that matters, and well inside the
/// 64 KiB `http::Uri` limit the assembled request would hit anyway.
const MAX_QUERY_PAIRS: usize = 1024;

/// Upper bound on the total key+value bytes one tool call may expand into.
const MAX_QUERY_BYTES: usize = 8 * 1024;

/// Accumulator for the `key=value` pairs a tool call's `query` object expands
/// Ceiling on **nodes visited** while flattening, whether or not a node emits a
/// pair.
///
/// [`MAX_QUERY_PAIRS`] / [`MAX_QUERY_BYTES`] bound only what reaches the wire,
/// so a container of hundreds of thousands of `null` fields paid nothing: each
/// one still cost a formatted child key (copying the parent), and the all-null
/// check that rejects it runs only *after* the whole subtree is walked. Charging
/// every visit stops that traversal at a fixed bound instead, so a body inside
/// the documented limit cannot amplify into unbounded key-copying work.
const MAX_QUERY_NODES: usize = 4 * MAX_QUERY_PAIRS;

/// into, enforcing [`MAX_QUERY_PAIRS`] / [`MAX_QUERY_BYTES`].
struct QueryPairs {
    pairs: Vec<(String, String)>,
    bytes: usize,
    nodes: usize,
}

impl QueryPairs {
    const fn new() -> Self {
        Self {
            pairs: Vec::new(),
            bytes: 0,
            nodes: 0,
        }
    }

    /// Charge one visited node against [`MAX_QUERY_NODES`].
    ///
    /// Called for every node the encoder descends into — including the ones
    /// that emit nothing (`null`, and containers before their leaves are
    /// known) — so traversal cost is bounded independently of output size.
    fn visit(&mut self) -> Result<(), String> {
        self.nodes += 1;
        if self.nodes > MAX_QUERY_NODES {
            return Err(format!(
                "query arguments traverse more than {MAX_QUERY_NODES} values; \
                 move the field to a JSON body"
            ));
        }
        Ok(())
    }

    fn push(&mut self, key: &str, value: String) -> Result<(), String> {
        self.bytes = self
            .bytes
            .saturating_add(key.len())
            .saturating_add(value.len());
        if self.pairs.len() >= MAX_QUERY_PAIRS || self.bytes > MAX_QUERY_BYTES {
            return Err(format!(
                "query arguments expand past the dispatch limit of {MAX_QUERY_PAIRS} \
                 parameters / {MAX_QUERY_BYTES} bytes"
            ));
        }
        self.pairs.push((key.to_owned(), value));
        Ok(())
    }
}

/// True when a JSON field name survives the bracketed query encoding.
///
/// `[` and `]` are structure in that encoding and an empty name addresses the
/// append position, so a field called any of those cannot be carried: emitting
/// it verbatim would invent, move, or lose a nesting level on the way back in.
/// Applied to every name the encoder introduces — the tool's top-level `query`
/// properties as well as nested object fields.
fn is_expressible_field_name(name: &str) -> bool {
    !name.is_empty() && !name.contains('[') && !name.contains(']')
}

/// The error for a field name [`is_expressible_field_name`] rejects.
fn inexpressible_field_name(owner: &str) -> String {
    format!(
        "query argument `{owner}` has a field name that a query string cannot carry: a name \
         may not be empty or contain `[` or `]`, which are structure in the query encoding"
    )
}

/// Flatten one `query` argument into the `key=value` pairs the
/// [`Query<T>`](crate::extract::Query) extractor decodes (issue #1972).
///
/// The wire format is the extractor's own dialect
/// ([`query_string`](crate::query_string)), so a tool's advertised
/// `inputSchema` and the request dispatch actually agree:
///
/// * scalars render flat (`page=2`);
/// * an array of scalars expands to repeated keys (`tags=a&tags=b`) — the
///   `OpenAPI` `form`/`explode` shape;
/// * an array containing any container uses explicit positions
///   (`items[0][sku]=A-1`), so element order survives;
/// * an object nests by key (`filter[status]=open`).
///
/// A `null` **field** renders no pair at all: a query string has no null, and
/// emitting the literal text `null` would fail the handler's coercion instead
/// of decoding as the absent/`None` the caller meant.
///
/// # Errors
///
/// Refuses, rather than silently dispatching something the caller did not ask
/// for, when the argument cannot be expressed in a query string:
///
/// * an **empty** array or object — dropping it would turn a present-but-empty
///   value into an absent one (and 400 a required field downstream);
/// * a `null` **array element** — dropping it would shorten the sequence and
///   shift every later element;
/// * an object field name that is empty or contains `[` / `]` — those are
///   structure in the bracketed dialect, so interpolating one would invent or
///   lose a nesting level;
/// * nesting past
///   [`query_string::MAX_DEPTH`](crate::query_string::MAX_DEPTH), or an
///   expansion past [`MAX_QUERY_PAIRS`] / [`MAX_QUERY_BYTES`].
fn encode_query_arg(
    key: &str,
    value: &Value,
    depth: usize,
    out: &mut QueryPairs,
) -> Result<(), String> {
    if depth > crate::query_string::MAX_DEPTH {
        return Err(format!(
            "query argument `{key}` nests deeper than the maximum of {}",
            crate::query_string::MAX_DEPTH
        ));
    }
    // Bail before formatting any child key: a single absurd key would otherwise
    // be copied once per descendant.
    if key.len() > MAX_QUERY_BYTES {
        return Err(format!(
            "query argument key exceeds {MAX_QUERY_BYTES} bytes"
        ));
    }
    // Before descending, and so before any child key is formatted: a `null`
    // leaf emits no pair, so without this it would pay nothing for the parent
    // key its sibling count forces the encoder to copy.
    out.visit()?;
    let is_container = matches!(value, Value::Array(_) | Value::Object(_));
    let before = out.pairs.len();
    match value {
        Value::Null => {}
        Value::Array(items) if items.is_empty() => {
            return Err(format!(
                "query argument `{key}` is an empty array; a query string cannot express an \
                 empty sequence — omit the argument, or move the field to a JSON body"
            ));
        }
        Value::Array(items) => {
            if let Some(position) = items.iter().position(Value::is_null) {
                return Err(format!(
                    "query argument `{key}[{position}]` is null; a query string cannot \
                     express a null element — omit it, or move the field to a JSON body"
                ));
            }
            let all_scalar = items
                .iter()
                .all(|item| !matches!(item, Value::Array(_) | Value::Object(_)));
            if all_scalar {
                if let [only] = items.as_slice() {
                    // Repeated keys carry a sequence only from the *second*
                    // occurrence on: one `tags=only` pair is indistinguishable
                    // from a scalar, so `deserialize_any` (an untyped
                    // `serde_json::Value` target) would yield `"only"` for the
                    // `["only"]` the caller sent — and the field would turn
                    // back into an array on gaining a second element. The
                    // explicit position pins the kind. A typed `Vec` target
                    // decodes either form identically.
                    out.push(&format!("{key}[0]"), query_scalar(only))?;
                } else {
                    for item in items {
                        out.push(key, query_scalar(item))?;
                    }
                }
            } else {
                for (index, item) in items.iter().enumerate() {
                    encode_query_arg(&format!("{key}[{index}]"), item, depth + 1, out)?;
                }
            }
        }
        Value::Object(fields) if fields.is_empty() => {
            return Err(format!(
                "query argument `{key}` is an empty object; a query string cannot express an \
                 empty object — omit the argument, or move the field to a JSON body"
            ));
        }
        // Every key digit-only means every segment decodes as a *position*
        // (`query_string::classify_segment`), so the tree comes back a sequence
        // and an untyped target (`serde_json::Value`, which takes whatever
        // `deserialize_any` yields) receives an array where the caller sent an
        // object — with the tell that adding one named key silently flips it
        // back. A typed map target is unaffected, but the encoder cannot see
        // the target, so it refuses the shape rather than dispatch a value the
        // caller did not ask for. Mixed keys are fine: the named one forces the
        // decoder to promote the whole node to a map.
        // A `null` field emits no pair, so it cannot disambiguate anything: it
        // is invisible to the decoder. Judging the shape on *declared* keys let
        // `{"0": 5, "kind": null}` through, which reaches the wire as the bare
        // `counts[0]=5` this arm exists to refuse. So the check runs over the
        // fields that actually emit. An all-null object is left to fall through
        // to the collapse check below, which describes it better.
        Value::Object(fields)
            if {
                let mut emitting = fields
                    .iter()
                    .filter(|(_, value)| !value.is_null())
                    .peekable();
                emitting.peek().is_some()
                    && emitting.all(|(field, _)| {
                        !field.is_empty() && field.bytes().all(|b| b.is_ascii_digit())
                    })
            } =>
        {
            return Err(format!(
                "query argument `{key}` is an object whose field names are all numeric; a \
                 query string cannot tell that from a sequence — rename a field, or move the \
                 field to a JSON body"
            ));
        }
        Value::Object(fields) => {
            for (field, nested) in fields {
                if !is_expressible_field_name(field) {
                    return Err(inexpressible_field_name(key));
                }
                encode_query_arg(&format!("{key}[{field}]"), nested, depth + 1, out)?;
            }
        }
        scalar => out.push(key, query_scalar(scalar))?,
    }
    // A container every one of whose leaves is `null` emits no pair at all.
    // Left alone it would vanish — and inside an array the decoder would then
    // compact the gap, shortening the sequence. That is the same silent
    // alteration the empty-container and null-element checks above prevent, so
    // it is refused here rather than dispatched. A `null` **field** is exempt by
    // construction: it is not a container, so it stays the absent marker.
    if is_container && out.pairs.len() == before {
        return Err(format!(
            "query argument `{key}` carries no value; a query string cannot express a \
             container whose every field is null — omit it, or move the field to a JSON body"
        ));
    }
    Ok(())
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
                identity: None,
            }),
            ..Default::default()
        }
    }

    #[test]
    fn strip_total_metric_drops_only_total() {
        // Inner `total` is dropped; the `db` metric (with a comma-free quoted
        // desc) is preserved so the outer fallback's real `/mcp` `total` is the
        // only one on the response.
        assert_eq!(
            strip_total_metric("total;dur=5.000, db;dur=1.250;desc=\"2 queries\""),
            Some("db;dur=1.250;desc=\"2 queries\"".to_owned())
        );
        // Metric order and a leading non-total entry are preserved.
        assert_eq!(
            strip_total_metric("app;dur=1.500, total;dur=42.000"),
            Some("app;dur=1.500".to_owned())
        );
        // A total-only header forwards nothing.
        assert_eq!(strip_total_metric("total;dur=9.000"), None);
        // The metric-name token is matched case-insensitively before `;`/`=`,
        // so a bare `total` (no params) is also dropped.
        assert_eq!(strip_total_metric("Total"), None);
        // Empty / whitespace-only input yields nothing.
        assert_eq!(strip_total_metric("  "), None);
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
    fn reset_content_205_with_opt_in_is_eligible() {
        // 205 Reset Content is the other empty-body-by-contract status; the
        // exemption is a shared predicate, not a hard-coded 204 literal.
        let mut d = doc("POST", "/api/forms/clear", "clear_form");
        d.response = None;
        d.success_status = 205;
        d.mcp_tool = true;
        assert!(should_expose(&d, false));
        let tools = derive_tools(&[d], false, None);
        assert_eq!(tools.len(), 1, "opted-in 205 route must derive a tool");
    }

    // ── Agent authority carried onto derived tools (#1691) ──────────────────
    //
    // The authority envelope is what makes a `tools/call` auditable with a
    // *compile-known* blast radius: the grant name, the effect set, and the
    // reversibility all come from the const assertions the macro already made
    // against the handler body. Losing it between `ApiDoc` and `McpToolInfo`
    // would silently downgrade a governed tool to `reversibility=unknown` in
    // the audit trail and to `ungoverned_tools` in the manifest.

    /// One construction site for the test grants, so the shape of
    /// [`crate::agent_authority::Grant`] is pinned in exactly one place here.
    const fn test_grant(
        name: &'static str,
        reversibility: crate::agent_authority::Reversibility,
    ) -> crate::agent_authority::Grant {
        crate::agent_authority::Grant {
            name,
            writes: &[],
            unbounded_writes: &[],
            tenant_scope: crate::agent_authority::TenantScope::Scoped,
            outbound: &[],
            webhooks: &[],
            jobs: &[],
            rate: None,
            spend: None,
            reversibility,
            location: "autumn/src/mcp.rs",
        }
    }

    /// Likewise for [`crate::agent_authority::AgentAuthority`].
    const fn test_authority(
        action: &'static str,
        grant: &'static crate::agent_authority::Grant,
    ) -> crate::agent_authority::AgentAuthority {
        crate::agent_authority::AgentAuthority {
            action,
            module_path: "autumn_web::mcp::tests",
            location: "autumn/src/mcp.rs",
            grant,
            effects: &[],
            asserted_effect_free_sites: 0,
            asserted_effect_free: &[],
        }
    }

    static REVERSIBLE_GRANT: crate::agent_authority::Grant = test_grant(
        "DraftOnly",
        crate::agent_authority::Reversibility::Reversible,
    );
    static COMPENSABLE_GRANT: crate::agent_authority::Grant = test_grant(
        "RefundDrafter",
        crate::agent_authority::Reversibility::Compensable,
    );
    static IRREVERSIBLE_GRANT: crate::agent_authority::Grant = test_grant(
        "PayoutSender",
        crate::agent_authority::Reversibility::Irreversible,
    );

    static REVERSIBLE_AUTHORITY: crate::agent_authority::AgentAuthority =
        test_authority("draft_note", &REVERSIBLE_GRANT);
    static COMPENSABLE_AUTHORITY: crate::agent_authority::AgentAuthority =
        test_authority("draft_refund", &COMPENSABLE_GRANT);
    static IRREVERSIBLE_AUTHORITY: crate::agent_authority::AgentAuthority =
        test_authority("send_payout", &IRREVERSIBLE_GRANT);

    /// An opted-in tool doc carrying (or not carrying) an authority.
    fn governed_doc(
        method: &'static str,
        op: &'static str,
        authority: Option<&'static crate::agent_authority::AgentAuthority>,
    ) -> ApiDoc {
        let mut d = doc(method, "/api/refunds", op);
        d.mcp_tool = true;
        d.agent_authority = authority;
        d
    }

    #[test]
    fn derive_tools_carries_the_handler_authority_onto_the_tool() {
        let tools = derive_tools(
            &[governed_doc(
                "POST",
                "draft_refund",
                Some(&COMPENSABLE_AUTHORITY),
            )],
            false,
            None,
        );
        assert_eq!(tools.len(), 1, "the opted-in route must derive a tool");
        let authority = tools[0]
            .agent_authority()
            .expect("a governed handler's tool must carry its authority envelope");
        assert_eq!(authority.grant.name, "RefundDrafter");
        assert_eq!(
            authority.grant.reversibility,
            crate::agent_authority::Reversibility::Compensable
        );
        assert_eq!(authority.action, "draft_refund");
    }

    #[test]
    fn derive_tools_leaves_an_ungoverned_tool_without_an_authority() {
        // `None` must stay `None`: it is the signal the manifest uses to list a
        // mutating tool under `ungoverned_tools`, so inventing one here would
        // hide exactly the gap that check exists to find.
        let tools = derive_tools(&[governed_doc("POST", "draft_refund", None)], false, None);
        assert_eq!(tools.len(), 1);
        assert!(
            tools[0].agent_authority().is_none(),
            "a handler with no #[agent_operable] derives an ungoverned tool"
        );
    }

    #[test]
    fn destructive_hint_follows_declared_reversibility() {
        let hint = |op, authority| {
            let tools = derive_tools(&[governed_doc("POST", op, authority)], false, None);
            tools[0].annotations().get("destructiveHint").cloned()
        };

        // A declared authority can *raise* the warning on a verb that says
        // nothing by itself.
        assert_eq!(
            hint("send_payout", Some(&IRREVERSIBLE_AUTHORITY)),
            Some(json!(true)),
            "an irreversible action is destructive"
        );
        assert_eq!(
            hint("draft_refund", Some(&COMPENSABLE_AUTHORITY)),
            Some(json!(true)),
            "a compensable action still needs a compensating step, so warn"
        );
        assert_eq!(
            hint("draft_note", Some(&REVERSIBLE_AUTHORITY)),
            Some(json!(false)),
            "a POST proved to be bounded writes only is explicitly NOT destructive"
        );
    }

    #[test]
    fn destructive_hint_falls_back_to_the_verb_rule_when_ungoverned() {
        // Nothing changes for the ungoverned majority: POST carries no hint and
        // DELETE stays flagged, exactly as before #1691.
        let post = derive_tools(&[governed_doc("POST", "draft_refund", None)], false, None);
        assert!(
            post[0].annotations().get("destructiveHint").is_none(),
            "an ungoverned POST keeps the verb rule's silence: {:?}",
            post[0].annotations()
        );

        let delete = derive_tools(
            &[governed_doc("DELETE", "delete_refund", None)],
            false,
            None,
        );
        assert_eq!(
            delete[0].annotations().get("destructiveHint"),
            Some(&json!(true)),
            "DELETE stays the destructive verb when no authority is declared"
        );
    }

    #[test]
    fn the_delete_verb_is_a_floor_a_reversible_grant_cannot_clear() {
        // `reversible` means the analyser proved the effect set is bounded
        // writes only — and `delete_by_id` IS a bounded write, so a hard row
        // delete reaches this shape. It does not mean the row can be put back;
        // nothing checks for soft-delete or versioning. Since an MCP client
        // skips its confirmation prompt on `destructiveHint: false`, the verb
        // stays a floor: the grant may add the warning, never remove it
        // (issue #1691 review, P2-1).
        let tools = derive_tools(
            &[governed_doc(
                "DELETE",
                "archive_note",
                Some(&REVERSIBLE_AUTHORITY),
            )],
            false,
            None,
        );
        assert_eq!(
            tools[0].annotations().get("destructiveHint"),
            Some(&json!(true)),
            "one unproved adjective in a grant must not clear a DELETE's warning: {:?}",
            tools[0].annotations()
        );
    }

    // ── Argument names are intersected with the tool's surface (#1691 P2-2) ──

    /// A tool with one path param plus a body, so all three name categories
    /// (path param, reserved key, unknown) are reachable.
    fn tool_with_path_param() -> McpTool {
        let mut t = tool("POST", "/api/refunds/{id}", true, false);
        t.path_params = vec!["id".to_owned()];
        t
    }

    #[test]
    fn argument_names_records_the_tools_own_surface_verbatim() {
        let names = argument_names(
            &tool_with_path_param(),
            &json!({ "body": {}, "id": "7", "query": {} }),
        );
        assert_eq!(names, "body,id,query", "sorted, and no unknown count");
    }

    #[test]
    fn argument_names_counts_unrecognised_keys_instead_of_quoting_them() {
        // `build_request` reads only `body`/`query`/the path params and ignores
        // everything else, and nothing rejects extra properties — so every other
        // key is caller-authored text that must never reach a durable row.
        let hostile = "\n2026-09-02T00:00:00Z INFO agent tool call actor=admin";
        let mut args = serde_json::Map::new();
        args.insert("body".to_owned(), json!({}));
        args.insert(hostile.to_owned(), json!(1));
        args.insert("ssn-123-45-6789".to_owned(), json!(2));
        let names = argument_names(&tool_with_path_param(), &Value::Object(args));
        assert_eq!(names, "body,+2 unknown");
        assert!(
            !names.contains('\n') && !names.contains("ssn-"),
            "no caller-chosen byte may appear: {names}"
        );
    }

    #[test]
    fn argument_names_of_an_all_unknown_call_is_only_a_count() {
        let names = argument_names(
            &tool_with_path_param(),
            &json!({ "\u{1f648}": 1, "../../etc": 2 }),
        );
        assert_eq!(names, "+2 unknown");
    }

    #[test]
    fn argument_names_addresses_a_catch_all_param_by_its_bare_name() {
        // `ApiDoc` surfaces `/files/{*rest}` as `*rest`; clients send `rest`,
        // and `build_request` strips the star to match. The audit view must
        // strip it the same way or a legitimate param reads as unknown.
        let mut t = tool("GET", "/files/{*rest}", false, false);
        t.path_params = vec!["*rest".to_owned()];
        assert_eq!(argument_names(&t, &json!({ "rest": "a/b" })), "rest");
    }

    #[test]
    fn argument_names_of_a_non_object_is_empty() {
        assert_eq!(argument_names(&tool_with_path_param(), &json!(null)), "");
    }

    #[test]
    fn render_effects_caps_the_list_and_marks_the_truncation() {
        use crate::agent_authority::{Effect, EffectKind, EffectProvenance};

        const fn effect(subject: &'static str) -> Effect {
            Effect {
                kind: EffectKind::Write,
                subject,
                location: "mcp.rs:0",
                provenance: EffectProvenance::Syntactic,
            }
        }
        // One over the cap, so the elision is exercised rather than the
        // boundary being silently the whole list.
        const EFFECTS: [Effect; MAX_AUDITED_EFFECTS + 1] =
            [effect("Refund"); MAX_AUDITED_EFFECTS + 1];

        let rendered = render_effects(&EFFECTS);
        assert_eq!(
            rendered.matches("write:Refund").count(),
            MAX_AUDITED_EFFECTS,
            "an oversized effect set is capped, not spilled into every row: {rendered}"
        );
        assert!(
            rendered.ends_with(",\u{2026}"),
            "the cap must be visible in the row, not silent: {rendered}"
        );
        assert_eq!(render_effects(&EFFECTS[..1]), "write:Refund");
        assert_eq!(render_effects(&[]), "");
    }

    #[test]
    fn only_a_completed_stream_and_an_intact_body_count_as_success() {
        // The disposition is the half of the verdict the status line cannot
        // supply: a streaming tool answers `200` before it has done anything,
        // and a buffered one can answer `200` and then fail to hand its body
        // over (issue #1691 review rounds 1 and 2).
        assert!(Disposition::Settled.is_success());
        assert!(Disposition::Stream(StreamState::Completed).is_success());
        assert!(!Disposition::Stream(StreamState::Aborted).is_success());
        assert!(!Disposition::Stream(StreamState::Errored).is_success());
        assert!(!Disposition::Buffered(BufferedFailure::Overflow).is_success());
        assert!(!Disposition::Buffered(BufferedFailure::BodyError).is_success());

        // Each failing ending names itself in exactly one metadata key, so a
        // sink can tell the three stream endings apart from a lost body.
        assert_eq!(
            Disposition::Stream(StreamState::Aborted).stream_state(),
            Some("aborted")
        );
        assert_eq!(Disposition::Stream(StreamState::Aborted).result(), None);
        assert_eq!(
            Disposition::Buffered(BufferedFailure::Overflow).result(),
            Some("body_overflow")
        );
        assert_eq!(
            Disposition::Buffered(BufferedFailure::BodyError).result(),
            Some("body_error")
        );
        assert_eq!(
            Disposition::Buffered(BufferedFailure::Overflow).stream_state(),
            None
        );
        assert_eq!(Disposition::Settled.stream_state(), None);
        assert_eq!(Disposition::Settled.result(), None);
    }

    #[tokio::test]
    async fn a_length_limit_anywhere_in_the_source_chain_is_an_overflow() {
        // `to_bytes` collects through `http_body_util::Limited`, which reports
        // the cap as a `LengthLimitError` *nested* inside the error it hands
        // back. Classifying on the outer type alone would record every overflow
        // as a generic `body_error` — so the error under test is produced by the
        // very call the buffered path makes, not by a hand-built stand-in.
        let overflow = axum::body::to_bytes(axum::body::Body::from(vec![0_u8; 32]), 8)
            .await
            .expect_err("a body past the cap must not collect");
        assert_eq!(
            BufferedFailure::classify(&overflow),
            BufferedFailure::Overflow
        );
        assert_eq!(BufferedFailure::Overflow.as_str(), "body_overflow");

        let other = axum::Error::new(std::io::Error::other("peer went away"));
        assert_eq!(
            BufferedFailure::classify(&other),
            BufferedFailure::BodyError
        );
        assert_eq!(BufferedFailure::BodyError.as_str(), "body_error");
    }

    #[test]
    fn read_only_hint_is_unaffected_by_the_authority() {
        // `readOnlyHint` is a statement about the HTTP verb, not about how hard
        // the effect is to undo — the two hints must not be conflated.
        let tools = derive_tools(
            &[governed_doc(
                "POST",
                "draft_refund",
                Some(&REVERSIBLE_AUTHORITY),
            )],
            false,
            None,
        );
        assert_eq!(tools[0].annotations()["readOnlyHint"], json!(false));
    }

    #[test]
    fn opted_in_202_is_skipped_not_exposed() {
        // 202 Accepted does not guarantee an empty body, so a schema-less 202
        // route stays behind the JSON-out gate even when opted in.
        let mut d = doc("POST", "/api/enqueue", "enqueue");
        d.response = None;
        d.success_status = 202;
        d.mcp_tool = true;
        assert!(!should_expose(&d, false));
        assert!(derive_tools(&[d], false, None).is_empty());
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
        assert_eq!(
            annotations_for("GET", "t", None)["readOnlyHint"],
            json!(true)
        );
        assert_eq!(
            annotations_for("POST", "t", None)["readOnlyHint"],
            json!(false)
        );
        assert_eq!(
            annotations_for("DELETE", "t", None)["destructiveHint"],
            json!(true)
        );
        assert!(
            annotations_for("POST", "t", None)
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
            identity: None,
        });
        let schema = build_input_schema(
            &d,
            &serde_json::Map::new(),
            &crate::openapi::SchemaComponentIndex::default(),
        );
        assert_eq!(schema["type"], "object");
        assert!(schema["properties"]["id"].is_object());
        assert!(schema["properties"]["body"].is_object());
        let required = schema["required"].as_array().unwrap();
        assert!(required.contains(&json!("id")));
        assert!(required.contains(&json!("body")));
    }

    #[test]
    fn detect_degradation_flags_opaque_body_and_query() {
        // Both `query` and `body` resolve to the bare object placeholder.
        let schema = json!({
            "type": "object",
            "properties": {
                "query": { "type": "object", "title": "Q" },
                "body": { "type": "object", "title": "B" },
            },
        });
        let degradations = detect_schema_degradations(&schema, true, true);
        assert!(degradations.contains(&SchemaDegradation::OpaqueQuery));
        assert!(degradations.contains(&SchemaDegradation::OpaqueBody));
    }

    #[test]
    fn detect_degradation_ignores_field_accurate_schemas() {
        // A real query object (with `properties`) and a real body: no warning.
        let schema = json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "object",
                    "properties": { "page": { "type": "integer" }, "tags": { "type": "array", "items": { "type": "string" } } },
                },
                "body": { "$ref": "#/$defs/Real" },
            },
            "$defs": { "Real": { "type": "object", "properties": { "a": { "type": "string" } } } },
        });
        assert!(detect_schema_degradations(&schema, true, true).is_empty());
    }

    #[test]
    fn detect_degradation_accepts_structured_query_fields() {
        // Nested objects, arrays of objects, `$ref`s and their nullable
        // (`oneOf [.., {"type":"null"}]`) forms are all round-trippable now that
        // `Query<T>` decodes the bracketed dialect `build_request` emits
        // (issue #1972), so none of them may be reported as a degradation.
        let schema = json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "object",
                    "properties": {
                        "filter": { "type": "object", "properties": { "min": { "type": "integer" } } },
                        "rows": { "type": "array", "items": { "type": "object", "properties": { "k": { "type": "string" } } } },
                        "ref_filter": { "$ref": "#/$defs/Filter" },
                        "maybe_filter": { "oneOf": [ { "$ref": "#/$defs/Filter" }, { "type": "null" } ] },
                        "maybe_rows": { "oneOf": [
                            { "type": "array", "items": { "$ref": "#/$defs/Filter" } },
                            { "type": "null" },
                        ] },
                    },
                },
            },
            "$defs": { "Filter": { "type": "object", "properties": { "min": { "type": "integer" } } } },
        });
        assert!(
            detect_schema_degradations(&schema, true, false).is_empty(),
            "structured query fields are honored, not degraded"
        );
    }

    #[test]
    fn encode_query_arg_renders_the_extractor_dialect() {
        let mut pairs = QueryPairs::new();
        encode_query_arg("page", &json!(2), 1, &mut pairs).expect("scalar");
        encode_query_arg("tags", &json!(["a", "b"]), 1, &mut pairs).expect("scalar array");
        encode_query_arg("filter", &json!({ "status": "open" }), 1, &mut pairs).expect("object");
        encode_query_arg("items", &json!([{ "sku": "A" }]), 1, &mut pairs).expect("object array");
        assert_eq!(
            pairs.pairs,
            vec![
                ("page".to_owned(), "2".to_owned()),
                ("tags".to_owned(), "a".to_owned()),
                ("tags".to_owned(), "b".to_owned()),
                ("filter[status]".to_owned(), "open".to_owned()),
                ("items[0][sku]".to_owned(), "A".to_owned()),
            ]
        );
    }

    #[test]
    fn encode_query_arg_omits_null_fields_rather_than_stringifying_them() {
        // A null argument is the documented "absent" marker: no pair, no error.
        let mut pairs = QueryPairs::new();
        encode_query_arg("page", &json!(null), 1, &mut pairs).expect("null");
        assert!(
            pairs.pairs.is_empty(),
            "a null argument renders no pair: {:?}",
            pairs.pairs
        );

        // The same holds for a null field alongside a carried one — only a
        // container that collapses ENTIRELY is refused (see the collapse test).
        let mut pairs = QueryPairs::new();
        encode_query_arg(
            "filter",
            &json!({ "status": null, "kind": "a" }),
            1,
            &mut pairs,
        )
        .expect("null field beside a carried one");
        assert_eq!(
            pairs.pairs,
            vec![("filter[kind]".to_owned(), "a".to_owned())]
        );
    }

    #[test]
    fn encode_query_arg_refuses_values_a_query_string_cannot_carry() {
        // Each of these would otherwise dispatch something the caller did not
        // ask for: a vanished field, a shortened sequence, or an invented
        // nesting level (issue #1972 review follow-ups).
        for (label, value) in [
            ("empty array", json!({ "tags": [] })),
            ("empty object", json!({ "filter": {} })),
            ("empty nested array", json!({ "matrix": [[1], []] })),
            ("null array element", json!({ "tags": ["a", null] })),
            (
                "null object-array element",
                json!({ "items": [null, { "sku": "A" }] }),
            ),
            ("bracket in field name", json!({ "filter": { "a[b]": 1 } })),
            ("empty field name", json!({ "filter": { "": 1 } })),
        ] {
            let mut pairs = QueryPairs::new();
            let Value::Object(fields) = &value else {
                unreachable!()
            };
            let result = fields
                .iter()
                .try_for_each(|(key, v)| encode_query_arg(key, v, 1, &mut pairs));
            assert!(
                result.is_err(),
                "{label} must be refused, not silently dropped"
            );
        }
    }

    #[test]
    fn encode_query_arg_refuses_containers_that_collapse_to_nothing() {
        // Codex P2: every field of an element being null emits no pair, so the
        // element would vanish and the decoder would compact the gap — the same
        // silent shortening a direct null element is already refused for.
        let mut pairs = QueryPairs::new();
        assert!(
            encode_query_arg(
                "items",
                &json!([{ "note": null }, { "note": "x" }]),
                1,
                &mut pairs
            )
            .is_err(),
            "an all-null element must not silently shorten the sequence"
        );

        // Same one level up, for an object-typed field.
        let mut pairs = QueryPairs::new();
        assert!(encode_query_arg("filter", &json!({ "note": null }), 1, &mut pairs).is_err());

        // A `null` FIELD is still the documented absent marker, not a collapse.
        let mut pairs = QueryPairs::new();
        encode_query_arg("filter", &json!({ "a": 1, "b": null }), 1, &mut pairs).expect("partial");
        assert_eq!(pairs.pairs, vec![("filter[a]".to_owned(), "1".to_owned())]);
    }

    #[test]
    fn top_level_query_keys_are_validated_like_nested_field_names() {
        // Codex P2: a dynamic query object can carry an arbitrary top-level
        // key, and `filter[x]` would otherwise reach the decoder as structure.
        assert!(!is_expressible_field_name("filter[x]"));
        assert!(!is_expressible_field_name(""));
        assert!(!is_expressible_field_name("a]b"));
        assert!(is_expressible_field_name("filter"));
        assert!(is_expressible_field_name("sort_by"));
    }

    #[test]
    fn encode_query_arg_bounds_its_expansion() {
        // A wide array cannot be turned into an unbounded pair list: expansion
        // stops at the dispatch limit instead of building a giant URI.
        let wide: Vec<Value> = (0..(MAX_QUERY_PAIRS * 2)).map(|i| json!(i)).collect();
        let mut pairs = QueryPairs::new();
        assert!(encode_query_arg("tags", &Value::Array(wide), 1, &mut pairs).is_err());

        // A single absurd key is refused before any child key is formatted.
        let mut pairs = QueryPairs::new();
        let huge = "k".repeat(MAX_QUERY_BYTES + 1);
        assert!(encode_query_arg(&huge, &json!({ "a": 1 }), 1, &mut pairs).is_err());
    }

    #[test]
    fn encode_query_arg_charges_null_leaves_for_the_traversal_they_cost() {
        // Codex P2: `null` emits no pair, so a container of them paid nothing
        // against the pair/byte limits while still forcing one formatted child
        // key per field — the all-null check only fires after the full walk.
        // The traversal budget now stops it early.
        let nulls: serde_json::Map<String, Value> = (0..(MAX_QUERY_NODES * 2))
            .map(|i| (format!("f{i}"), Value::Null))
            .collect();
        let mut pairs = QueryPairs::new();
        let err = encode_query_arg("root", &Value::Object(nulls), 1, &mut pairs)
            .expect_err("a traversal this wide must be refused");
        assert!(err.contains("traverse"), "{err}");
        assert!(
            pairs.nodes <= MAX_QUERY_NODES + 1,
            "traversal must stop at the bound, visited {}",
            pairs.nodes
        );

        // The budget is generous enough that ordinary nesting is unaffected.
        let mut pairs = QueryPairs::new();
        encode_query_arg(
            "filter",
            &json!({ "status": "open", "tag": null }),
            1,
            &mut pairs,
        )
        .expect("a small object still encodes");
    }

    #[test]
    fn encode_query_arg_refuses_an_all_numeric_keyed_object() {
        // Codex P2: every key digit-only decodes as a sequence, so an untyped
        // target would receive an array where the caller sent an object.
        let mut pairs = QueryPairs::new();
        let err = encode_query_arg("counts", &json!({ "0": 5, "1": 6 }), 1, &mut pairs)
            .expect_err("an all-numeric-keyed object is ambiguous on the wire");
        assert!(err.contains("all numeric"), "{err}");

        // One named key is enough to force map promotion on the way back in,
        // so the mixed shape stays expressible.
        let mut pairs = QueryPairs::new();
        encode_query_arg("counts", &json!({ "0": 5, "total": 6 }), 1, &mut pairs)
            .expect("a mixed-key object round-trips as a map");

        // The check is per-object, so it also catches a nested one.
        let mut pairs = QueryPairs::new();
        assert!(encode_query_arg("filter", &json!({ "by": { "7": "x" } }), 1, &mut pairs).is_err());

        // An empty field name still gets its own, more specific error.
        let mut pairs = QueryPairs::new();
        let err = encode_query_arg("counts", &json!({ "": 1 }), 1, &mut pairs)
            .expect_err("an empty field name is inexpressible");
        assert!(err.contains("may not be empty"), "{err}");
    }

    #[test]
    fn a_singleton_scalar_array_keeps_its_container_kind() {
        // Codex P2: one repeated key is indistinguishable from a scalar, so a
        // one-element array must use the explicit position instead.
        let mut pairs = QueryPairs::new();
        encode_query_arg("tags", &json!(["only"]), 1, &mut pairs).expect("encodes");
        assert_eq!(
            pairs.pairs,
            vec![("tags[0]".to_owned(), "only".to_owned())],
            "a singleton must pin its kind"
        );

        // Two or more still use the repeated-key (OpenAPI form/explode) shape.
        let mut pairs = QueryPairs::new();
        encode_query_arg("tags", &json!(["a", "b"]), 1, &mut pairs).expect("encodes");
        assert_eq!(
            pairs.pairs,
            vec![
                ("tags".to_owned(), "a".to_owned()),
                ("tags".to_owned(), "b".to_owned())
            ]
        );
    }

    #[test]
    fn the_numeric_object_check_ignores_fields_that_emit_nothing() {
        // Codex P2: a `null` field emits no pair, so it cannot disambiguate —
        // judging on declared keys let this reach the wire as bare `counts[0]=5`.
        let mut pairs = QueryPairs::new();
        let err = encode_query_arg("counts", &json!({ "0": 5, "kind": null }), 1, &mut pairs)
            .expect_err("the null field cannot disambiguate");
        assert!(err.contains("all numeric"), "{err}");

        // A named field that *does* emit still disambiguates, as before.
        let mut pairs = QueryPairs::new();
        encode_query_arg("counts", &json!({ "0": 5, "kind": "x" }), 1, &mut pairs)
            .expect("an emitting named key resolves the shape");

        // An all-null object keeps its own, more specific collapse error.
        let mut pairs = QueryPairs::new();
        let err = encode_query_arg("counts", &json!({ "0": null }), 1, &mut pairs)
            .expect_err("an all-null object carries no value");
        assert!(err.contains("carries no value"), "{err}");
    }

    #[test]
    fn encode_query_arg_rejects_nesting_past_the_decoder_cap() {
        // Build an object nested one level deeper than the decoder accepts, so
        // the encoder refuses instead of emitting keys the extractor rejects.
        let mut value = json!("leaf");
        for _ in 0..crate::query_string::MAX_DEPTH {
            value = json!({ "deeper": value });
        }
        let mut pairs = QueryPairs::new();
        assert!(encode_query_arg("root", &value, 1, &mut pairs).is_err());
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
            empty_body: false,
            agent_authority: None,
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
                envelope_load_shed: false,
                state: crate::AppState::for_test(),
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
