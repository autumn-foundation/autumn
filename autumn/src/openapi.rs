// OpenAPI/JSON/JSON-schema all appear frequently here and are legitimate
// acronyms, so silence clippy::doc_markdown rather than wrapping every
// mention in backticks.
#![allow(clippy::doc_markdown)]

//! OpenAPI (Swagger) specification auto-generation.
//!
//! Autumn automatically infers an OpenAPI 3.1 document from your
//! annotated routes ([`get`](crate::get), [`post`](crate::post), etc.),
//! their path parameters, and the extractor / response types in each
//! handler signature. The generated spec is served at `/openapi.json` and
//! a Swagger UI is served at `/swagger-ui` when the feature is enabled.
//!
//! Narrative guide: `docs/guide/openapi.md`.
//!
//! # Quick start
//!
//! Enable the `openapi` feature in `Cargo.toml`, then:
//!
//! ```toml
//! [dependencies]
//! autumn-web = { version = "0.2", features = ["openapi"] }
//! ```
//!
//! ```rust,ignore
//! use autumn_web::prelude::*;
//!
//! #[get("/hello")]
//! async fn hello() -> &'static str { "hi" }
//!
//! #[autumn_web::main]
//! async fn main() {
//!     autumn_web::app()
//!         .routes(routes![hello])
//!         .openapi(autumn_web::openapi::OpenApiConfig::new("My API", "1.0.0"))
//!         .run()
//!         .await;
//! }
//! ```
//!
//! With `.openapi(...)` enabled, the following endpoints are mounted:
//! * `GET /openapi.json` — serves the generated spec document.
//! * `GET /swagger-ui` — serves a Swagger UI HTML page loading the JSON
//!   above.
//!
//! # Enriching the auto-generated docs
//!
//! Decorate handlers with [`#[api_doc(...)]`](crate::api_doc) to override
//! or add documentation fields that cannot be inferred from the signature
//! (summaries, descriptions, tags, custom status codes, etc.):
//!
//! ```rust,no_run
//! use autumn_web::prelude::*;
//!
//! #[get("/users/{id}")]
//! #[api_doc(summary = "Fetch a user by id", tag = "users")]
//! async fn get_user(_id: Path<i32>) -> &'static str { "user" }
//! ```
//!
//! # Custom schemas
//!
//! Types that need rich schemas (beyond the generic "object" fallback)
//! implement the `OpenApiSchema` trait and are registered with
//! `OpenApiConfig::register_schema`.

use std::collections::BTreeMap;

#[cfg(feature = "openapi")]
use serde::{Deserialize, Serialize};

// ──────────────────────────────────────────────────────────────────
// `autumn openapi export` dump protocol
// ──────────────────────────────────────────────────────────────────

/// Machine-readable stderr marker saying this binary cannot produce a spec.
///
/// Emitted by the `AUTUMN_DUMP_OPENAPI` dump instead of a spec document, with a
/// human-readable reason following on the same line. `autumn openapi export`
/// scans stderr for it so it can turn "this app has no OpenAPI surface" into an
/// actionable message rather than a JSON parse failure on empty stdout.
///
/// Two things produce it: the binary was built without the `openapi` feature,
/// or it was built with the feature but never called
/// [`AppBuilder::openapi`](crate::app::AppBuilder::openapi).
pub const OPENAPI_UNAVAILABLE_MARKER: &str = "[autumn:openapi-unavailable] ";

/// Reason text following [`OPENAPI_UNAVAILABLE_MARKER`] when the binary was
/// compiled without the `openapi` feature.
pub const OPENAPI_UNAVAILABLE_FEATURE: &str = "this binary was built without the `openapi` feature";

/// Reason text following [`OPENAPI_UNAVAILABLE_MARKER`] when the app never
/// configured a spec.
pub const OPENAPI_UNAVAILABLE_UNCONFIGURED: &str =
    "this app never called `.openapi(OpenApiConfig::new(..))`";

// ──────────────────────────────────────────────────────────────────
// Public metadata attached to each Route
// ──────────────────────────────────────────────────────────────────

/// OpenAPI metadata emitted alongside every annotated route.
///
/// Populated by the route macros ([`get`](crate::get),
/// [`post`](crate::post), etc.) from the handler's path, signature, and
/// any [`#[api_doc(...)]`](crate::api_doc) overrides.
#[derive(Clone, Debug, Default)]
// A flat, generated metadata descriptor; the independent boolean flags
// (hidden, secured, sunset_opt_out, has_policy, public, mcp_tool, mcp_exclude)
// each model a distinct, orthogonal route property, so grouping them into a
// sub-struct would obscure rather than clarify.
#[allow(clippy::struct_excessive_bools)]
pub struct ApiDoc {
    /// HTTP method as an uppercase string (e.g. `"GET"`).
    pub method: &'static str,
    /// Raw route path with `{param}` placeholders (e.g. `"/users/{id}"`).
    pub path: &'static str,
    /// Handler function name — used as the default `operationId`.
    pub operation_id: &'static str,
    /// Short human-readable summary (from `#[api_doc(summary = ...)]`).
    pub summary: Option<&'static str>,
    /// Longer free-form description.
    pub description: Option<&'static str>,
    /// Grouping tags. Defaults to the first path segment when unset.
    pub tags: &'static [&'static str],
    /// Path parameter names extracted from the URL template.
    ///
    /// Built at compile time from `{...}` segments in the route path.
    pub path_params: &'static [&'static str],
    /// Optional schema for the request body (typically the inner type of
    /// a `Json<T>` extractor).
    pub request_body: Option<SchemaEntry>,
    /// Optional schema for the success response (typically the inner type
    /// of a `Json<T>` return value).
    pub response: Option<SchemaEntry>,
    /// Success HTTP status code, defaults to `200`.
    pub success_status: u16,
    /// When `true`, the route is excluded from the generated spec.
    pub hidden: bool,
    /// Optional query-parameter schema inferred from `Query<T>` extractors.
    pub query_schema: Option<SchemaEntry>,
    /// True when the route requires authentication (`#[secured]`).
    pub secured: bool,
    /// Roles required by `#[secured("role1")]`. Empty means any authenticated user.
    pub required_roles: &'static [&'static str],
    /// Scopes required by `#[secured(scopes = ["scope"])]`. When non-empty the
    /// route is documented as `BearerAuth` instead of `SessionAuth`.
    pub required_scopes: &'static [&'static str],
    /// Optional runtime hook that lets a handler register any extra
    /// component schemas with the generator.
    pub register_schemas: Option<fn(&mut SchemaRegistry)>,
    /// Optional API version associated with this route.
    pub api_version: Option<&'static str>,
    /// Whether this route opts out of sunset 410 responses.
    pub sunset_opt_out: bool,
    /// Whether this route uses dynamic policy authorization.
    pub has_policy: bool,
    /// Record-level authorization bindings declared by `#[authorize]` on the
    /// handler, in source order — one entry per attribute, empty when the
    /// handler declares none.
    ///
    /// This is the provable subset of [`Self::has_policy`], never a
    /// replacement for it: that boolean is also `true` for a hand-written
    /// `__check_policy` call in the body, which carries no binding a macro can
    /// recover.
    pub authorize_bindings: &'static [AuthorizeBinding],
    /// Pool tags the handler's declared extractors prove it holds for the
    /// length of the request (`"db"`, `"mail"`, …), sorted and deduplicated.
    ///
    /// The statically derived half of the capacity contract (issue #1733):
    /// `autumn calibrate` folds these into `capacity.lock` so a contract says
    /// *why* a route costs what it costs, not just what the aggregate
    /// envelope was. Empty is the honest default — it means "no pool proven",
    /// not "no pool touched" (see `route_listing::RouteInfo::pools`).
    pub pools: &'static [&'static str],
    /// True when the handler is explicitly declared public via `#[public]`.
    ///
    /// Populated by the route macros from the `#[public]` marker. Used by the
    /// route-listing security classifier (`autumn routes audit`) to
    /// distinguish a *deliberately* open route from one whose auth posture was
    /// simply never declared.
    pub public: bool,
    /// Module path of the handler (`module_path!()` captured at the handler's
    /// definition site), used to name a route in security-audit diagnostics.
    /// Empty for routes constructed without the route macros.
    pub module_path: &'static str,
    /// Source file of the handler (`file!()` captured at the handler's
    /// definition site), used alongside [`Self::source_line`] to point a
    /// security-audit diagnostic straight at the offending handler. Empty for
    /// routes constructed without the route macros.
    pub source_file: &'static str,
    /// Source line of the handler (`line!()` captured at the handler's
    /// definition site). `0` when [`Self::source_file`] is empty.
    pub source_line: u32,
    /// True when the endpoint opts in to MCP tool exposure via
    /// `#[api_doc(mcp)]`. Opt-in is per-endpoint and never implicit.
    pub mcp_tool: bool,
    /// True when the endpoint explicitly opts *out* of MCP exposure via
    /// `#[api_doc(mcp = false)]`. Honored even under the whole-API hatch
    /// (`AppBuilder::expose_all_as_mcp`). Not an intra-doc link: this field is
    /// always compiled, but the builder method is gated behind the `mcp`
    /// feature, so a hard link would break docs built without it.
    pub mcp_exclude: bool,
    /// True when the endpoint opts in to *streaming* MCP exposure via
    /// `#[api_doc(mcp, stream)]`. A streaming tool returns an Autumn `Sse`
    /// stream that the MCP endpoint projects onto the Streamable-HTTP SSE
    /// channel as `notifications/progress` messages terminated by the final
    /// `tools/call` result. Because an `Sse` handler has no JSON response
    /// schema, this flag also exempts the tool from the JSON-out eligibility
    /// gate that otherwise excludes schema-less routes.
    pub mcp_stream: bool,
    /// Build-time authority envelope proved for this handler by
    /// `#[agent_operable(grant = ...)]` (issue #1691), or `None` for a handler
    /// that declares none.
    ///
    /// Always compiled — the field is metadata about the *handler*, not about
    /// any particular transport, so the agent-authority manifest and the
    /// `autumn routes` listing can read it without the `mcp` feature. The MCP
    /// endpoint copies it onto the derived tool so `tools/call` can record the
    /// compile-known reversibility in its audit trail and derive
    /// `destructiveHint` from the grant instead of guessing from the verb.
    ///
    /// A `Some` here is a *proved* envelope: the macro walked the handler body,
    /// const-asserted every detected effect against the named grant, and
    /// refused to expand when an effect could not be proven. `None` therefore
    /// means "ungoverned", never "no effects".
    pub agent_authority: Option<&'static crate::agent_authority::AgentAuthority>,
}

/// A record-level authorization binding declared by `#[authorize]`.
///
/// One entry per `#[authorize("action", resource = Type)]` attribute on a
/// handler, recovered from the macro expansion, so a binding disappears from
/// the metadata exactly when the attribute disappears from the source.
///
/// This is deliberately **not** the `Policy` implementation that serves the
/// check. `#[authorize]` names only the resource `R`; the concrete
/// `impl Policy<R>` is resolved from the `PolicyRegistry` at boot
/// (`AppBuilder::policy::<R, _>(...)`) and is therefore not knowable at build
/// time. [`Self::resource`] is likewise the identifier as written at the use
/// site rather than a resolved path, so two same-named types in different
/// modules are indistinguishable here.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct AuthorizeBinding {
    /// Action verb passed to the policy check — the first `#[authorize]`
    /// argument, recorded verbatim.
    pub action: &'static str,
    /// Resource type identifier exactly as written in `resource = Type`.
    pub resource: &'static str,
}

/// Reference to a schema definition, produced by the route macros.
#[derive(Copy, Clone, Debug)]
pub struct SchemaEntry {
    /// Short human-readable type name, used as the *default* component
    /// display key (`#/components/schemas/Name`) when it does not collide.
    pub name: &'static str,
    /// Whether this is a primitive JSON type (string/number/bool/array) as
    /// opposed to a named object ref.
    pub kind: SchemaKind,
    /// Globally-unique schema *identity* for a `Ref` entry, as a fn pointer to
    /// [`type_name_of`] (i.e. `::core::any::type_name::<T>()`). This is what
    /// matches a route reference to its producer (`#[derive(OpenApiSchema)]`
    /// descriptor / registered schema) and disambiguates two distinct types
    /// that share a last path segment (e.g. `create::Args` vs `update::Args`)
    /// so neither silently shadows the other (issue #1972).
    ///
    /// A fn pointer (rather than the `&'static str` directly) keeps a nested
    /// `SchemaEntry` const-promotable to `&'static` in `Array` / `Nullable`
    /// wrappers, since `type_name` is not yet a stably-const fn.
    ///
    /// `None` for primitives and for the `Array` / `Nullable` wrapper entries
    /// (whose `name` is the sentinel `"array"` / `"nullable"`), and for legacy
    /// short-name refs (e.g. the repository macro's model refs) which keep their
    /// last-segment display key.
    pub identity: Option<fn() -> &'static str>,
}

impl SchemaEntry {
    /// The globally-unique identity key for this entry: its `type_name` when an
    /// `identity` fn is present, otherwise the short `name` (legacy behavior).
    #[must_use]
    pub fn identity_key(&self) -> &'static str {
        self.identity.map_or(self.name, |f| f())
    }
}

// `PartialEq`/`Eq` are implemented by hand rather than derived: deriving them
// would compare the `identity` field's fn *pointers*, which the
// `unpredictable_function_pointer_comparisons` lint (rightly) flags as
// meaningless. Comparing the *resolved* identity strings is both meaningful and
// what callers actually want (two entries are equal iff they describe the same
// type the same way).
impl PartialEq for SchemaEntry {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
            && self.kind == other.kind
            && self.identity_key() == other.identity_key()
    }
}

impl Eq for SchemaEntry {}

/// Monomorphized `::core::any::type_name::<T>()` behind a fn pointer.
///
/// The route macros emit `Some(type_name_of::<T>)` as a [`SchemaEntry::identity`]
/// so producer and consumer agree on a globally-unique schema identity by
/// construction (both are the `type_name` of the same `T`). Using a fn pointer
/// keeps nested entries const-promotable to `&'static` (see
/// [`SchemaEntry::identity`]).
#[must_use]
pub fn type_name_of<T: ?Sized>() -> &'static str {
    core::any::type_name::<T>()
}

/// Sanitize a schema identity into a valid OpenAPI component key.
///
/// OpenAPI restricts component keys to `^[A-Za-z0-9._-]+$`, so a Rust
/// `type_name` (`crate::module::Args`, `Vec<T>`) cannot be used verbatim. `::`
/// collapses to `.` (utoipa-style) and any other out-of-range character maps to
/// `_`. Centralizing this here means the registration side and the `$ref` side
/// always derive the exact same key from the same identity.
#[must_use]
pub fn component_key(raw: &str) -> String {
    let dotted = raw.replace("::", ".");
    dotted
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Classifier for how a type should appear in the spec.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SchemaKind {
    /// Refers to a named component schema.
    Ref,
    /// A primitive JSON type inlined at the reference site.
    Primitive(&'static str),
    /// A JSON array whose items follow the referenced sub-schema. Used
    /// for handlers that return `Json<Vec<T>>` (or accept one as a
    /// request body) — emitting `Ref` for those would produce an
    /// object schema instead of the array the endpoint actually
    /// serializes.
    Array(&'static SchemaEntry),
    /// A nullable schema — used when the handler wraps the payload in
    /// `Option<T>`. The referenced sub-entry describes `T`.
    Nullable(&'static SchemaEntry),
}

// ──────────────────────────────────────────────────────────────────
// Configuration — users opt into OpenAPI generation explicitly.
// ──────────────────────────────────────────────────────────────────

/// User-facing configuration for OpenAPI generation.
///
/// Passed to [`AppBuilder::openapi`](crate::app::AppBuilder::openapi)
/// to enable spec generation and mount the documentation endpoints.
#[cfg(feature = "openapi")]
#[derive(Clone)]
pub struct OpenApiConfig {
    /// API title that appears in the Swagger UI header.
    pub title: String,
    /// API version (e.g. `"1.0.0"`).
    pub version: String,
    /// Optional free-form API description (Markdown permitted in UI).
    pub description: Option<String>,
    /// Path serving the raw `openapi.json`. Defaults to `/openapi.json`.
    pub openapi_json_path: String,
    /// Path serving the Swagger UI HTML. Defaults to `/swagger-ui`. Set
    /// to `None` to disable the UI while still exposing the JSON.
    pub swagger_ui_path: Option<String>,
    /// Session cookie name used by secured route security docs.
    ///
    /// Runtime OpenAPI mounting replaces this with `session.cookie_name`
    /// from the loaded app config.
    pub session_cookie_name: String,
    /// User-registered component schemas keyed by schema name.
    pub additional_schemas: BTreeMap<String, serde_json::Value>,
    /// API versions registry.
    pub api_versions: Vec<crate::app::ApiVersion>,
}

#[cfg(feature = "openapi")]
impl OpenApiConfig {
    /// Create a new config with the required `title` and `version`.
    #[must_use]
    pub fn new(title: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            version: version.into(),
            description: None,
            openapi_json_path: "/openapi.json".to_owned(),
            swagger_ui_path: Some("/swagger-ui".to_owned()),
            session_cookie_name: "autumn.sid".to_owned(),
            additional_schemas: BTreeMap::new(),
            api_versions: Vec::new(),
        }
    }

    /// Set a free-form API description.
    #[must_use]
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Override the path serving `openapi.json`.
    #[must_use]
    pub fn openapi_json_path(mut self, path: impl Into<String>) -> Self {
        self.openapi_json_path = path.into();
        self
    }

    /// Override the Swagger UI path (or `None` to disable it).
    #[must_use]
    pub fn swagger_ui_path(mut self, path: Option<String>) -> Self {
        self.swagger_ui_path = path;
        self
    }

    /// Override the session cookie name documented for secured routes.
    #[must_use]
    pub fn session_cookie_name(mut self, name: impl Into<String>) -> Self {
        self.session_cookie_name = name.into();
        self
    }

    /// Register a custom component schema. Useful when a handler's
    /// payload type does not implement `OpenApiSchema`.
    #[must_use]
    pub fn register_schema(mut self, name: impl Into<String>, schema: serde_json::Value) -> Self {
        self.additional_schemas.insert(name.into(), schema);
        self
    }
}

// ──────────────────────────────────────────────────────────────────
// Schema trait + primitive impls (feature-gated)
// ──────────────────────────────────────────────────────────────────

/// Describes a type's JSON schema for OpenAPI generation.
///
/// Provide a manual implementation for complex types to expose rich
/// schemas in the generated spec. A blanket default is not provided —
/// routes whose types do not implement this trait simply emit a generic
/// `object` placeholder referring to the type name.
///
/// This trait is always available (no feature gate) so that `#[model]`-generated
/// types can implement it unconditionally. The spec generation machinery that
/// consumes implementations is still gated behind the `openapi` feature.
pub trait OpenApiSchema {
    /// Component schema name (appears under `#/components/schemas/`).
    fn schema_name() -> &'static str;

    /// Produce the JSON schema for this type.
    fn schema() -> serde_json::Value;
}

/// Derive a field-accurate [`OpenApiSchema`] impl for a plain struct with named
/// fields (issue #1972), so a handler-arg struct used in `Query<T>` / `Json<T>`
/// advertises its real fields in the OpenAPI spec and the MCP tool `inputSchema`
/// instead of collapsing to a generic `{"type":"object"}` placeholder — with no
/// hand-written impl or `OpenApiConfig::register_schema` call.
///
/// Each field becomes a JSON-schema property (nullable `Option<T>` via
/// `oneOf [T, null]`, `Vec<T>` as an array, primitives inline, other named types
/// as `$ref`s), and every non-`Option` field is listed as `required` — mirroring
/// the schema `#[model]` already generates. The derive also registers the schema
/// in the compile-time inventory the spec/MCP back-fill consults, so a
/// `Query<MyArgs>` / `Json<MyArgs>` handler picks it up automatically.
///
/// Bring it into scope alongside the trait: `use autumn_web::openapi::OpenApiSchema;`.
pub use autumn_macros::OpenApiSchema;

macro_rules! impl_primitive_schema {
    ($ty:ty, $name:literal, $json:literal) => {
        impl OpenApiSchema for $ty {
            fn schema_name() -> &'static str {
                $name
            }
            fn schema() -> serde_json::Value {
                serde_json::json!({ "type": $json })
            }
        }
    };
}

impl_primitive_schema!(bool, "boolean", "boolean");
impl_primitive_schema!(String, "string", "string");
impl_primitive_schema!(&'static str, "string", "string");
impl_primitive_schema!(i8, "integer", "integer");
impl_primitive_schema!(i16, "integer", "integer");
impl_primitive_schema!(i32, "integer", "integer");
impl_primitive_schema!(i64, "integer", "integer");
impl_primitive_schema!(u8, "integer", "integer");
impl_primitive_schema!(u16, "integer", "integer");
impl_primitive_schema!(u32, "integer", "integer");
impl_primitive_schema!(u64, "integer", "integer");
impl_primitive_schema!(f32, "number", "number");
impl_primitive_schema!(f64, "number", "number");
impl_primitive_schema!(serde_json::Value, "object", "object");

// ──────────────────────────────────────────────────────────────────
// Compile-time inventory of `#[derive(OpenApiSchema)]` component schemas.
// ──────────────────────────────────────────────────────────────────

/// Compile-time registration of a plain struct's derived `OpenApiSchema`,
/// emitted by `#[derive(OpenApiSchema)]` (issue #1972).
///
/// The spec generator and the MCP tool-catalog builder both back-fill component
/// schemas for referenced type names they did not otherwise register. Without a
/// hand-written `OpenApiSchema` impl + `OpenApiConfig::register_schema`, a plain
/// handler-arg struct (a `Query<T>` param struct or a non-`#[model]` `Json<T>`
/// body) used to resolve to a generic `{"type":"object","title":"X"}`
/// placeholder — so the argument's real fields lived only in prose. This
/// descriptor lets a `#[derive(OpenApiSchema)]` struct advertise its
/// field-accurate schema by name, which the back-fill loops pick up
/// automatically (no manual registration).
///
/// This is deliberately not feature-gated: `#[derive(OpenApiSchema)]` submits an
/// entry unconditionally, and the `openapi`-gated spec builder consults it only
/// when that feature is compiled in.
pub struct DerivedSchemaDescriptor {
    /// Short display hint (the type's last path segment / `schema_name()`).
    pub name: &'static str,
    /// The type's globally-unique *identity* (`type_name`), behind a fn pointer.
    /// The spec/MCP back-fill matches a route reference to this descriptor by
    /// identity, so two distinct types sharing a last segment resolve to their
    /// own schema instead of whichever inventory entry link-order hit first
    /// (issue #1972).
    pub identity: fn() -> &'static str,
    /// Produces the JSON schema for the type (the type's `OpenApiSchema::schema`).
    pub schema: fn() -> serde_json::Value,
}

inventory::collect!(DerivedSchemaDescriptor);

/// Look up the derived component schema for a schema *identity* (`type_name`).
///
/// Returns the schema when a `#[derive(OpenApiSchema)]` type with that identity
/// was linked into the binary, or `None` when no such derive exists (so callers
/// fall back to the generic placeholder). Matching by identity — not the short
/// last-segment name — is what keeps two distinct `Args` types from shadowing
/// each other in the back-fill.
#[must_use]
pub fn registered_derived_schema(identity: &str) -> Option<serde_json::Value> {
    inventory::iter::<DerivedSchemaDescriptor>
        .into_iter()
        .find(|descriptor| (descriptor.identity)() == identity)
        .map(|descriptor| (descriptor.schema)())
}

// ──────────────────────────────────────────────────────────────────
// Runtime registry of component schemas populated while building the spec.
// ──────────────────────────────────────────────────────────────────

/// Accumulates component schemas while a spec is being built.
#[derive(Default)]
pub struct SchemaRegistry {
    schemas: BTreeMap<String, serde_json::Value>,
}

impl SchemaRegistry {
    /// Register a type via its `OpenApiSchema` implementation. A
    /// duplicate insertion is a no-op (the existing entry wins).
    pub fn register<T: OpenApiSchema>(&mut self) {
        let name = T::schema_name().to_owned();
        self.schemas.entry(name).or_insert_with(T::schema);
    }

    /// Insert a raw pre-built schema by name.
    pub fn insert(&mut self, name: impl Into<String>, schema: serde_json::Value) {
        self.schemas.insert(name.into(), schema);
    }

    /// Drain the collected schemas, consuming the registry.
    #[must_use]
    pub fn into_map(self) -> BTreeMap<String, serde_json::Value> {
        self.schemas
    }

    /// Peek at the collected schemas without consuming the registry.
    #[must_use]
    pub const fn schemas(&self) -> &BTreeMap<String, serde_json::Value> {
        &self.schemas
    }
}

// ──────────────────────────────────────────────────────────────────
// Serializable OpenAPI 3.1 document types.
//
// Only the fields Autumn actually populates are modelled — unused
// OpenAPI keys (callbacks, links, discriminators…) are intentionally
// omitted so the generated JSON stays clean. Gated behind the
// `openapi` feature so the runtime spec builder doesn't add code
// size / dependency pressure to apps that never serve a JSON spec.
// ──────────────────────────────────────────────────────────────────

#[cfg(feature = "openapi")]
/// Represents a root OpenAPI 3.1 specification document.
#[derive(Debug, Serialize, Deserialize)]
pub struct OpenApiSpec {
    /// The OpenAPI version string (e.g., `3.1.0`).
    pub openapi: String,
    /// General information about the API.
    pub info: Info,
    /// The available paths and operations for the API.
    pub paths: BTreeMap<String, PathItem>,
    /// Reusable schemas, parameters, and other components.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub components: Option<Components>,
}

#[cfg(feature = "openapi")]
/// Provides metadata about the API.
#[derive(Debug, Serialize, Deserialize)]
pub struct Info {
    /// The title of the API.
    pub title: String,
    /// The version of the OpenAPI document.
    pub version: String,
    /// A description of the API.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[cfg(feature = "openapi")]
/// Describes the operations available on a single path.
#[derive(Default, Debug, Serialize, Deserialize)]
pub struct PathItem {
    /// A definition of a GET operation on this path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub get: Option<Operation>,
    /// A definition of a POST operation on this path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub post: Option<Operation>,
    /// A definition of a PUT operation on this path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub put: Option<Operation>,
    /// A definition of a DELETE operation on this path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delete: Option<Operation>,
    /// A definition of a PATCH operation on this path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub patch: Option<Operation>,
}

#[cfg(feature = "openapi")]
/// Describes a single API operation on a path.
#[derive(Debug, Serialize, Deserialize)]
pub struct Operation {
    /// Unique string used to identify the operation.
    #[serde(rename = "operationId")]
    pub operation_id: String,
    /// A short summary of what the operation does.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// A verbose explanation of the operation behavior.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// A list of tags for API documentation control.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// A list of parameters that are applicable for this operation.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub parameters: Vec<Parameter>,
    /// The request body applicable for this operation.
    #[serde(rename = "requestBody", skip_serializing_if = "Option::is_none")]
    pub request_body: Option<RequestBody>,
    /// The list of possible responses as they are returned from executing this operation.
    pub responses: BTreeMap<String, Response>,
    /// Security requirements for this operation. Non-empty when the route uses `#[secured]`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub security: Vec<BTreeMap<String, Vec<String>>>,
    /// Declares this operation to be deprecated.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deprecated: Option<bool>,
    /// Vendor extension: bearer-token scope strings required by this operation.
    /// Empty for session-only or unsecured routes.
    #[serde(rename = "x-required-scopes", skip_serializing_if = "Vec::is_empty")]
    pub x_required_scopes: Vec<String>,
}

#[cfg(feature = "openapi")]
/// Describes a single operation parameter.
#[derive(Debug, Serialize, Deserialize)]
pub struct Parameter {
    /// The name of the parameter.
    pub name: String,
    /// The location of the parameter. Possible values are "query", "header", "path" or "cookie".
    #[serde(rename = "in")]
    pub location: String,
    /// Determines whether this parameter is mandatory.
    pub required: bool,
    /// The schema defining the type used for the parameter.
    pub schema: serde_json::Value,
    /// Serialization style. `"form"` with `explode: true` makes each object
    /// property a separate query key — the accurate mapping for a `Query<T>`
    /// whose fields are scalars or scalar arrays. A **nested** field decodes
    /// from the bracketed form (`?filter[status]=open`) that
    /// [`crate::query_string`] defines, which `form`/`explode` leaves
    /// undefined; see the "Known gaps" note in `docs/guide/openapi.md`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub style: Option<String>,
    /// When `true` with `style: "form"`, each schema property becomes an
    /// independent query parameter (e.g. `?q=foo&page=2`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub explode: Option<bool>,
}

#[cfg(feature = "openapi")]
/// Describes a single request body.
#[derive(Debug, Serialize, Deserialize)]
pub struct RequestBody {
    /// Determines if the request body is required in the request.
    pub required: bool,
    /// The content of the request body, keyed by media type.
    pub content: BTreeMap<String, MediaType>,
}

#[cfg(feature = "openapi")]
/// Describes a single response from an API Operation.
#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    /// A short description of the response.
    pub description: String,
    /// A map containing descriptions of potential response payloads, keyed by media type.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub content: BTreeMap<String, MediaType>,
}

#[cfg(feature = "openapi")]
/// Provides schema and examples for the media type identified by its key.
#[derive(Debug, Serialize, Deserialize)]
pub struct MediaType {
    /// The schema defining the content of the request, response, or parameter.
    pub schema: serde_json::Value,
}

#[cfg(feature = "openapi")]
/// Holds a set of reusable objects for different aspects of the OAS.
#[derive(Debug, Serialize, Deserialize)]
pub struct Components {
    /// Reusable Schema Objects.
    pub schemas: BTreeMap<String, serde_json::Value>,
    /// Security scheme definitions (e.g. SessionAuth).
    #[serde(rename = "securitySchemes", skip_serializing_if = "BTreeMap::is_empty")]
    pub security_schemes: BTreeMap<String, serde_json::Value>,
}

// ──────────────────────────────────────────────────────────────────
// Spec generator
// ──────────────────────────────────────────────────────────────────

/// Write the generated OpenAPI spec to `dist/openapi.json` and
/// `dist/openapi.yaml` inside `dist_dir`.
///
/// Called during `autumn build` (when `AUTUMN_BUILD_STATIC=1`) to emit
/// a machine-readable API contract alongside the pre-rendered HTML pages.
///
/// # Errors
///
/// Returns an [`std::io::Error`] if the directory cannot be created or
/// either file cannot be written.
#[cfg(feature = "openapi")]
pub fn write_openapi_spec_to_dist(
    spec: &OpenApiSpec,
    dist_dir: &std::path::Path,
) -> std::io::Result<()> {
    std::fs::create_dir_all(dist_dir)?;

    let json = serde_json::to_string_pretty(spec).map_err(std::io::Error::other)?;
    std::fs::write(dist_dir.join("openapi.json"), &json)?;

    let yaml = serde_yaml::to_string(spec).map_err(std::io::Error::other)?;
    std::fs::write(dist_dir.join("openapi.yaml"), yaml)?;

    Ok(())
}

/// Resolves each referenced schema *identity* (`type_name`) to a readable,
/// collision-free OpenAPI component *display key* (issue #1972).
///
/// Built once at spec-finalize (and re-derivable purely from the routes so the
/// MCP tool builder computes the identical mapping). A short last-segment key
/// (`Args`) is used whenever it is unambiguous; only when two *distinct*
/// identities would collide on the same last segment is each qualified with
/// enough trailing module segments to disambiguate (`create.Args` /
/// `update.Args`). Both the component registration and every `$ref` go through
/// this map, so a route reference can never resolve to the wrong schema.
#[cfg(feature = "openapi")]
#[derive(Default, Debug, Clone)]
pub struct SchemaComponentIndex {
    /// identity key (`type_name` or legacy short name) → display component key.
    by_identity: BTreeMap<String, String>,
}

#[cfg(feature = "openapi")]
impl SchemaComponentIndex {
    /// The component display key for a `Ref` entry — the value emitted in its
    /// `#/components/schemas/{key}` `$ref`. Falls back to the sanitized short
    /// name for an identity that was not part of the indexed route set (e.g. a
    /// hand-built entry in a unit test).
    #[must_use]
    pub fn display_key(&self, entry: &SchemaEntry) -> String {
        let identity = entry.identity_key();
        self.by_identity
            .get(identity)
            .cloned()
            .unwrap_or_else(|| component_key(entry.name))
    }

    /// Resolve a raw schema *identity* string (`type_name`) to its display key,
    /// or `None` when the identity is not part of this index. Used by the
    /// finalize body-ref rewrite to map a nested derived-schema `$ref` (emitted
    /// as the field type's full `type_name`) to its collision-resolved component
    /// key (issue #1972).
    fn display_key_for_identity(&self, identity: &str) -> Option<&str> {
        self.by_identity.get(identity).map(String::as_str)
    }

    /// Iterate `(identity, display_key)` pairs — used by the back-fill to
    /// register a component under its display key keyed by identity.
    fn iter(&self) -> impl Iterator<Item = (&String, &String)> {
        self.by_identity.iter()
    }
}

/// The trailing `n` `::`-segments of a `type_name`, joined with `.` and
/// sanitized into a component key (`a::b::Args`, n=2 → `b.Args`).
#[cfg(feature = "openapi")]
fn qualified_suffix_key(identity: &str, depth: usize) -> String {
    let segments: Vec<&str> = identity.split("::").collect();
    let start = segments.len().saturating_sub(depth);
    component_key(&segments[start..].join("::"))
}

/// Build the identity→display-key map for every schema referenced by `routes`.
///
/// Pure over the route set, so [`generate_spec_at`] and the MCP tool builder
/// derive the exact same keys.
#[cfg(feature = "openapi")]
#[must_use]
pub fn build_schema_component_index(routes: &[&ApiDoc]) -> SchemaComponentIndex {
    // Collect (identity, base-display) for every referenced Ref entry, deduped
    // by identity. `seen` gives the identity-graph closure below an O(1) "already
    // queued?" check.
    let mut refs: Vec<(String, String)> = Vec::new();
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for api_doc in routes {
        if api_doc.hidden {
            continue;
        }
        for entry in [
            api_doc.request_body.as_ref(),
            api_doc.response.as_ref(),
            api_doc.query_schema.as_ref(),
        ]
        .into_iter()
        .flatten()
        {
            for e in flatten_ref_entries(entry) {
                let identity = e.identity_key().to_owned();
                if seen.insert(identity.clone()) {
                    refs.push((identity, component_key(e.name)));
                }
            }
        }
    }

    // Close the identity graph: a `$ref` emitted *inside* a derived schema body
    // (a nested named-struct field) can reference an identity that no route
    // mentions directly. Fetch each derived body and recursively collect the
    // identities it refers to, to a fixpoint, so nested-only types participate
    // in collision detection and each earns its own component (issue #1972).
    let mut queue: Vec<String> = refs.iter().map(|(id, _)| id.clone()).collect();
    while let Some(identity) = queue.pop() {
        let Some(body) = registered_derived_schema(&identity) else {
            continue;
        };
        let mut nested: Vec<String> = Vec::new();
        collect_body_ref_identities(&body, &mut nested);
        for n in nested {
            if seen.insert(n.clone()) {
                let base = base_display_for_identity(&n);
                refs.push((n.clone(), base));
                queue.push(n);
            }
        }
    }

    SchemaComponentIndex {
        by_identity: assign_display_keys(&refs),
    }
}

/// Assign a **unique, deterministic** display component key to every referenced
/// schema identity. `refs` is `(identity, base_display)` pairs; identities are
/// assumed already deduped. Pure over its input and independent of the order the
/// pairs are supplied (identities are qualified in a stable sorted order), so the
/// same route set always yields the same keys across runs and link orders.
///
/// Every distinct identity is guaranteed a distinct display key: the collision
/// ladder (fewest trailing `::`-segments that make the key unique → full
/// sanitized fallback) can be exhausted, and a colliding identity's fallback key
/// can equal a key another colliding identity already claimed (e.g. crate `app`
/// with `app::app::Args` claiming `app.Args` at depth 2 while `app::Args`
/// exhausts its ladder and falls back to the same `app.Args`). When the best
/// candidate is still taken, a deterministic `-N` disambiguator is appended until
/// the key is free. `-` never appears in a `component_key` output derived from a
/// real Rust `type_name` (Rust paths contain no `-`), so the disambiguator can
/// never collide with a naturally-produced key (issue #1972).
#[cfg(feature = "openapi")]
fn assign_display_keys(refs: &[(String, String)]) -> BTreeMap<String, String> {
    // Which base display keys are shared by more than one distinct identity?
    let mut by_base: BTreeMap<String, std::collections::BTreeSet<&str>> = BTreeMap::new();
    for (identity, base) in refs {
        by_base
            .entry(base.clone())
            .or_default()
            .insert(identity.as_str());
    }

    let mut by_identity: BTreeMap<String, String> = BTreeMap::new();
    let mut used: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();

    // First pass: assign the plain last-segment key to every identity whose base
    // is unambiguous, and to legacy short-name identities (no `::` path — the
    // repository macro's model refs) which cannot be qualified anyway.
    for (identity, base) in refs {
        let collides = by_base[base].len() > 1 && identity.contains("::");
        if !collides {
            by_identity.entry(identity.clone()).or_insert_with(|| {
                used.insert(base.clone());
                base.clone()
            });
        }
    }

    // Second pass: qualify each genuinely-colliding real type path with the
    // fewest trailing module segments that make its key unique. Iterate in a
    // stable sorted order so the assignment is deterministic regardless of the
    // order pairs were collected, and guarantee the final key is free even when
    // the suffix ladder and the full-sanitized fallback are both exhausted.
    let mut pending: Vec<&String> = refs
        .iter()
        .map(|(identity, _)| identity)
        .filter(|identity| !by_identity.contains_key(*identity))
        .collect();
    pending.sort_unstable();
    for identity in pending {
        let depth_max = identity.split("::").count();
        let mut display = (2..=depth_max)
            .map(|depth| qualified_suffix_key(identity, depth))
            .find(|candidate| !used.contains(candidate))
            .unwrap_or_else(|| component_key(identity));
        if used.contains(&display) {
            let base = display.clone();
            let mut n = 2u32;
            loop {
                let candidate = format!("{base}-{n}");
                if !used.contains(&candidate) {
                    display = candidate;
                    break;
                }
                n += 1;
            }
        }
        used.insert(display.clone());
        by_identity.insert(identity.clone(), display);
    }

    by_identity
}

/// The base (short) display key for a raw schema *identity* string: its last
/// `::`-segment (ignoring any generic argument list), sanitized into a component
/// key. Mirrors what the macros derive from `last_segment_name` for a top-level
/// route ref, so a nested-only identity groups under the same base as a route
/// ref of the same short name (issue #1972).
#[cfg(feature = "openapi")]
fn base_display_for_identity(identity: &str) -> String {
    let without_generics = identity.split('<').next().unwrap_or(identity);
    let last = without_generics
        .rsplit("::")
        .next()
        .unwrap_or(without_generics);
    component_key(last)
}

/// Recursively collect every `#/components/schemas/<identity>` ref target found
/// inside a (derived) schema body, pushing each raw identity string into `out`.
///
/// The macro emits a nested named-struct field's `$ref` as the field type's full
/// `type_name` identity, so the strings gathered here are exactly the identity
/// keys the collision index resolves.
#[cfg(feature = "openapi")]
fn collect_body_ref_identities(value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::Object(map) => {
            if let Some(serde_json::Value::String(reference)) = map.get("$ref")
                && let Some(id) = reference.strip_prefix("#/components/schemas/")
            {
                out.push(id.to_owned());
            }
            for v in map.values() {
                collect_body_ref_identities(v, out);
            }
        }
        serde_json::Value::Array(items) => {
            for v in items {
                collect_body_ref_identities(v, out);
            }
        }
        _ => {}
    }
}

/// Rewrite every body-internal `#/components/schemas/<identity>` ref in each
/// registered component to its collision-resolved display key.
///
/// A ref whose target is not a known identity (e.g. the pagination envelope's
/// short `#/components/schemas/<Model>` ref, which is already a display key) is
/// left untouched, so the common non-colliding case produces exactly the short
/// key it did before (no churn — issue #1972).
#[cfg(feature = "openapi")]
fn rewrite_component_body_refs(
    components: &mut BTreeMap<String, serde_json::Value>,
    index: &SchemaComponentIndex,
) {
    for schema in components.values_mut() {
        rewrite_identity_refs(schema, index);
    }
}

#[cfg(feature = "openapi")]
fn rewrite_identity_refs(value: &mut serde_json::Value, index: &SchemaComponentIndex) {
    match value {
        serde_json::Value::Object(map) => {
            if let Some(serde_json::Value::String(reference)) = map.get_mut("$ref") {
                let replacement = reference
                    .strip_prefix("#/components/schemas/")
                    .and_then(|identity| index.display_key_for_identity(identity))
                    .map(|display| format!("#/components/schemas/{display}"));
                if let Some(new_ref) = replacement {
                    *reference = new_ref;
                }
            }
            for v in map.values_mut() {
                rewrite_identity_refs(v, index);
            }
        }
        serde_json::Value::Array(items) => {
            for v in items {
                rewrite_identity_refs(v, index);
            }
        }
        _ => {}
    }
}

/// Flatten an entry, yielding each leaf `Ref` entry reached through
/// `Array` / `Nullable` wrappers (so a `Json<Vec<User>>` contributes `User`).
#[cfg(feature = "openapi")]
fn flatten_ref_entries(entry: &SchemaEntry) -> Vec<&SchemaEntry> {
    match entry.kind {
        SchemaKind::Ref => vec![entry],
        SchemaKind::Array(inner) | SchemaKind::Nullable(inner) => flatten_ref_entries(inner),
        SchemaKind::Primitive(_) => Vec::new(),
    }
}

/// Build an [`OpenApiSpec`] from a collection of routes and user config.
///
/// This is the core of the auto-generation: every route's [`ApiDoc`] is
/// translated into an [`Operation`] under the matching [`PathItem`].
#[cfg(feature = "openapi")]
#[must_use]
pub fn generate_spec(config: &OpenApiConfig, routes: &[&ApiDoc]) -> OpenApiSpec {
    generate_spec_at(config, routes, chrono::Utc::now())
}

#[cfg(feature = "openapi")]
#[must_use]
pub fn generate_spec_at(
    config: &OpenApiConfig,
    routes: &[&ApiDoc],
    now: chrono::DateTime<chrono::Utc>,
) -> OpenApiSpec {
    let mut paths: BTreeMap<String, PathItem> = BTreeMap::new();
    let mut registry = SchemaRegistry::default();

    for (name, schema) in &config.additional_schemas {
        registry.insert(name.clone(), schema.clone());
    }
    registry.insert("ProblemDetails", problem_details_schema());

    // Resolve every referenced schema identity to a collision-free component
    // display key up front, so both the `$ref` sites (via `operation_for`) and
    // the back-fill below register/reference the exact same key (issue #1972).
    let index = build_schema_component_index(routes);

    let mut any_secured = false;
    let mut any_scoped = false;

    for api_doc in routes {
        if api_doc.hidden {
            continue;
        }
        if api_doc.secured {
            any_secured = true;
        }
        if !api_doc.required_scopes.is_empty() {
            any_scoped = true;
        }
        if let Some(register) = api_doc.register_schemas {
            (register)(&mut registry);
        }

        let operation = operation_for(api_doc, &config.api_versions, now, &index);
        let entry = paths.entry(api_doc.path.to_owned()).or_default();
        match api_doc.method {
            "GET" => entry.get = Some(operation),
            "POST" => entry.post = Some(operation),
            "PUT" => entry.put = Some(operation),
            "DELETE" => entry.delete = Some(operation),
            "PATCH" => entry.patch = Some(operation),
            // Unknown methods are silently skipped; Autumn's route macros
            // only emit the five verbs above today.
            _ => {}
        }
    }

    // Back-fill a schema for every referenced identity the user didn't already
    // register, under its resolved display key. A `#[derive(OpenApiSchema)]`
    // type advertises a real field-accurate schema through the compile-time
    // inventory (matched by identity, so two same-named types never shadow each
    // other — issue #1972); only an identity with no derived schema (and no
    // explicit `OpenApiConfig::register_schema`) falls back to the minimal
    // `{"type": "object", "title": "X"}` placeholder.
    for (identity, display_key) in index.iter() {
        if !registry.schemas().contains_key(display_key) {
            let schema = registered_derived_schema(identity).unwrap_or_else(|| {
                serde_json::json!({
                    "type": "object",
                    "title": display_key,
                })
            });
            registry.insert(display_key.clone(), schema);
        }
    }

    // Register auth security schemes used by secured routes.
    let mut security_schemes: BTreeMap<String, serde_json::Value> = BTreeMap::new();
    if any_secured {
        security_schemes.insert(
            "SessionAuth".to_owned(),
            serde_json::json!({
                "type": "apiKey",
                "in": "cookie",
                "name": config.session_cookie_name.clone(),
                "description": "Autumn session cookie. Secured routes check the configured auth.session_key inside the server-side session.",
            }),
        );
    }
    if any_scoped {
        security_schemes.insert(
            "BearerAuth".to_owned(),
            serde_json::json!({
                "type": "http",
                "scheme": "bearer",
                "description": "API bearer token. Scope-secured routes require a valid token whose scopes include all required values.",
            }),
        );
    }

    let mut components_map = registry.into_map();
    // Rewrite every `$ref` that appears *inside* a component body from its raw
    // schema identity (`type_name`) to its collision-resolved display key, so a
    // nested derived-schema ref resolves to the same component the top-level
    // route refs use (issue #1972). Top-level operation refs are already emitted
    // as display keys by `operation_for`/`schema_value_for`; this closes the
    // body-internal half of the `$ref` graph.
    rewrite_component_body_refs(&mut components_map, &index);
    let components = if !components_map.is_empty() || !security_schemes.is_empty() {
        Some(Components {
            schemas: components_map,
            security_schemes,
        })
    } else {
        None
    };

    OpenApiSpec {
        openapi: "3.1.0".to_owned(),
        info: Info {
            title: config.title.clone(),
            version: config.version.clone(),
            description: config.description.clone(),
        },
        paths,
        components,
    }
}

#[cfg(feature = "openapi")]
#[allow(clippy::too_many_lines)]
fn operation_for(
    api_doc: &ApiDoc,
    api_versions: &[crate::app::ApiVersion],
    now: chrono::DateTime<chrono::Utc>,
    index: &SchemaComponentIndex,
) -> Operation {
    let mut tags = if api_doc.tags.is_empty() {
        default_tag(api_doc.path)
            .map(|t| vec![t.to_owned()])
            .unwrap_or_default()
    } else {
        api_doc.tags.iter().map(|s| (*s).to_owned()).collect()
    };

    if let Some(version) = api_doc.api_version {
        tags.push(version.to_string());
    }

    let is_deprecated = api_doc.api_version.is_some_and(|version| {
        api_versions
            .iter()
            .find(|av| av.version == version)
            .is_some_and(|av| {
                let is_dep = av.deprecated_at.is_some_and(|d| now >= d);
                let is_sun = av.sunset_at.is_some_and(|s| now >= s);
                is_dep || is_sun
            })
    });
    let deprecated = if is_deprecated { Some(true) } else { None };

    // Path parameters — always required.
    let mut parameters: Vec<Parameter> = api_doc
        .path_params
        .iter()
        .map(|name| Parameter {
            name: (*name).to_owned(),
            location: "path".to_owned(),
            required: true,
            schema: serde_json::json!({ "type": "string" }),
            style: None,
            explode: None,
        })
        .collect();

    // Query parameters from `Query<T>` extractor.
    // Use `style: form, explode: true` so each field of the query struct
    // is serialized as an independent query key (e.g. `?q=foo&page=2`).
    // That is exact for scalar and scalar-array fields. A nested field
    // (an object, or an array of objects) is decoded from the bracketed form
    // `crate::query_string` defines — `?filter[status]=open` — which no OpenAPI
    // style expresses in full (`deepObject` covers one object level but not an
    // array of objects, and would also re-introduce the parameter name that
    // `form`/`explode` correctly drops). Documented in docs/guide/openapi.md
    // rather than misdescribed here (issue #1972).
    if let Some(query_entry) = &api_doc.query_schema {
        parameters.push(Parameter {
            name: query_entry.name.to_owned(),
            location: "query".to_owned(),
            required: false,
            schema: schema_value_for(query_entry, index),
            style: Some("form".to_owned()),
            explode: Some(true),
        });
    }

    let request_body = api_doc.request_body.as_ref().map(|entry| RequestBody {
        required: true,
        content: std::iter::once((
            "application/json".to_owned(),
            MediaType {
                schema: schema_value_for(entry, index),
            },
        ))
        .collect(),
    });

    let mut responses: BTreeMap<String, Response> = BTreeMap::new();
    let status = if api_doc.success_status == 0 {
        200
    } else {
        api_doc.success_status
    };
    let response_content = api_doc
        .response
        .as_ref()
        .map(|entry| {
            let mut content = BTreeMap::new();
            content.insert(
                "application/json".to_owned(),
                MediaType {
                    schema: schema_value_for(entry, index),
                },
            );
            content
        })
        .unwrap_or_default();
    responses.insert(
        status.to_string(),
        Response {
            description: status_description(status).to_owned(),
            content: response_content,
        },
    );
    insert_problem_responses(&mut responses);

    // If this route version has a sunset schedule and is not opted out, document 410 Gone
    let is_subject_to_sunset = api_doc.api_version.is_some_and(|version| {
        api_versions
            .iter()
            .find(|av| av.version == version)
            .is_some_and(|av| av.sunset_at.is_some())
            && !api_doc.sunset_opt_out
    });

    if is_subject_to_sunset {
        responses.entry("410".to_owned()).or_insert_with(|| {
            let mut content = BTreeMap::new();
            content.insert(
                "application/problem+json".to_owned(),
                MediaType {
                    schema: serde_json::json!({
                        "$ref": "#/components/schemas/ProblemDetails",
                    }),
                },
            );
            Response {
                description: status_description(410).to_owned(),
                content,
            }
        });
    }

    // Security requirements:
    //   - scopes-only  (#[secured(scopes=[…])])            → BearerAuth
    //   - roles+scopes (#[secured("r", scopes=[…])])       → SessionAuth AND BearerAuth
    //   - roles-only / bare #[secured]                     → SessionAuth
    // Both entries in one BTreeMap object means AND per the OpenAPI spec.
    // HTTP-bearer scheme value arrays must be empty (non-empty arrays are OAuth2 scopes).
    let security = if api_doc.secured {
        let mut req = BTreeMap::new();
        if !api_doc.required_scopes.is_empty() {
            req.insert("BearerAuth".to_owned(), Vec::<String>::new());
        }
        if api_doc.required_scopes.is_empty() || !api_doc.required_roles.is_empty() {
            req.insert("SessionAuth".to_owned(), Vec::<String>::new());
        }
        vec![req]
    } else {
        Vec::new()
    };

    Operation {
        operation_id: api_doc.operation_id.to_owned(),
        summary: api_doc.summary.map(str::to_owned),
        description: api_doc.description.map(str::to_owned),
        tags,
        parameters,
        request_body,
        responses,
        security,
        deprecated,
        x_required_scopes: api_doc
            .required_scopes
            .iter()
            .map(ToString::to_string)
            .collect(),
    }
}

/// Render a [`SchemaEntry`] into its JSON Schema value.
///
/// Produces the same shape the OpenAPI generator emits. Exposed so the MCP
/// projection can derive a tool's `inputSchema` from the exact same typed
/// contract — guaranteeing the tool schema cannot drift from the handler.
///
/// `index` resolves each `Ref` to its collision-free component display key
/// (issue #1972); build it once with [`build_schema_component_index`] over the
/// same route set so tool `$ref`s match the served OpenAPI components exactly.
#[cfg(feature = "openapi")]
#[must_use]
pub fn schema_entry_to_value(
    entry: &SchemaEntry,
    index: &SchemaComponentIndex,
) -> serde_json::Value {
    schema_value_for(entry, index)
}

#[cfg(feature = "openapi")]
fn schema_value_for(entry: &SchemaEntry, index: &SchemaComponentIndex) -> serde_json::Value {
    match entry.kind {
        SchemaKind::Primitive(json_type) => serde_json::json!({ "type": json_type }),
        SchemaKind::Ref => {
            serde_json::json!({ "$ref": format!("#/components/schemas/{}", index.display_key(entry)) })
        }
        SchemaKind::Array(items) => serde_json::json!({
            "type": "array",
            "items": schema_value_for(items, index),
        }),
        SchemaKind::Nullable(inner) => {
            // OpenAPI 3.1 aligns with JSON Schema 2020-12, which supports
            // `type: "null"` natively:
            //   * For a `$ref`, use `oneOf: [{$ref: ...}, {type: "null"}]`
            //     so the ref can stand alone without `allOf` workarounds.
            //   * For primitives, use the compact type-array form: `type: ["T", "null"]`.
            //   * For all other schemas (arrays, nested nullable, etc.), use `oneOf`
            //     so the full inner schema (e.g. `items`) is preserved.
            match inner.kind {
                SchemaKind::Ref | SchemaKind::Array(_) | SchemaKind::Nullable(_) => {
                    serde_json::json!({
                        "oneOf": [
                            schema_value_for(inner, index),
                            { "type": "null" },
                        ],
                    })
                }
                SchemaKind::Primitive(base_type) => {
                    serde_json::json!({ "type": [base_type, "null"] })
                }
            }
        }
    }
}

#[cfg(feature = "openapi")]
fn insert_problem_responses(responses: &mut BTreeMap<String, Response>) {
    for status in [400_u16, 401, 403, 404, 409, 413, 415, 422, 500, 503] {
        responses.entry(status.to_string()).or_insert_with(|| {
            let mut content = BTreeMap::new();
            content.insert(
                "application/problem+json".to_owned(),
                MediaType {
                    schema: serde_json::json!({
                        "$ref": "#/components/schemas/ProblemDetails",
                    }),
                },
            );
            Response {
                description: status_description(status).to_owned(),
                content,
            }
        });
    }
}

#[cfg(feature = "openapi")]
fn problem_details_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "required": [
            "type",
            "title",
            "status",
            "detail",
            "instance",
            "code",
            "request_id",
            "errors",
        ],
        "properties": {
            "type": {
                "type": "string",
                "format": "uri-reference",
            },
            "title": {
                "type": "string",
            },
            "status": {
                "type": "integer",
                "minimum": 400,
                "maximum": 599,
            },
            "detail": {
                "type": "string",
            },
            "instance": {
                "type": ["string", "null"],
            },
            "code": {
                "type": "string",
                "pattern": "^autumn\\.[a-z0-9_]+$",
            },
            "request_id": {
                "type": ["string", "null"],
            },
            "errors": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["field", "messages"],
                    "properties": {
                        "field": {
                            "type": "string",
                        },
                        "messages": {
                            "type": "array",
                            "items": {
                                "type": "string",
                            },
                        },
                    },
                },
            },
        },
    })
}

#[cfg(feature = "openapi")]
fn default_tag(path: &str) -> Option<&str> {
    path.trim_start_matches('/')
        .split('/')
        .find(|seg| !seg.is_empty() && !seg.starts_with('{'))
}

#[cfg(feature = "openapi")]
const fn status_description(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        413 => "Payload Too Large",
        415 => "Unsupported Media Type",
        422 => "Unprocessable Entity",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Response",
    }
}

// ──────────────────────────────────────────────────────────────────
// Swagger UI HTML
// ──────────────────────────────────────────────────────────────────

#[cfg(feature = "openapi")]
pub(crate) const SWAGGER_UI_VERSION: &str = "5.32.4";
#[cfg(feature = "openapi")]
pub(crate) const SWAGGER_UI_CSS: &str = include_str!("../vendor/swagger-ui/swagger-ui.css");
#[cfg(feature = "openapi")]
pub(crate) const SWAGGER_UI_BUNDLE: &[u8] =
    include_bytes!("../vendor/swagger-ui/swagger-ui-bundle.js");
#[cfg(feature = "openapi")]
const SWAGGER_UI_CSS_FILE: &str = "swagger-ui.css";
#[cfg(feature = "openapi")]
const SWAGGER_UI_BUNDLE_FILE: &str = "swagger-ui-bundle.js";
#[cfg(feature = "openapi")]
const SWAGGER_UI_INITIALIZER_FILE: &str = "swagger-initializer.js";

/// Compute the same-origin asset URLs mounted beneath the Swagger UI HTML path.
#[cfg(feature = "openapi")]
#[must_use]
pub(crate) fn swagger_ui_asset_paths(swagger_path: &str) -> [String; 3] {
    [
        swagger_ui_asset_path(swagger_path, SWAGGER_UI_CSS_FILE),
        swagger_ui_asset_path(swagger_path, SWAGGER_UI_BUNDLE_FILE),
        swagger_ui_asset_path(swagger_path, SWAGGER_UI_INITIALIZER_FILE),
    ]
}

#[cfg(feature = "openapi")]
#[must_use]
fn swagger_ui_asset_path(swagger_path: &str, asset_file: &str) -> String {
    let base = swagger_path.trim_end_matches('/');
    if base.is_empty() || base == "/" {
        format!("/{asset_file}")
    } else {
        format!("{base}/{asset_file}")
    }
}

/// Minimal Swagger UI bootstrap HTML that loads same-origin vendored assets.
#[cfg(feature = "openapi")]
#[must_use]
pub fn swagger_ui_html(
    title: &str,
    css_url: &str,
    bundle_url: &str,
    initializer_url: &str,
) -> String {
    let title = html_escape(title);
    let css_url = html_escape(css_url);
    let bundle_url = html_escape(bundle_url);
    let initializer_url = html_escape(initializer_url);
    let mut out = String::with_capacity(1024);
    out.push_str("<!DOCTYPE html>\n");
    out.push_str("<html lang=\"en\">\n");
    out.push_str("  <head>\n");
    out.push_str("    <meta charset=\"utf-8\" />\n");
    out.push_str("    <title>");
    out.push_str(&title);
    out.push_str("</title>\n");
    out.push_str("    <link rel=\"stylesheet\" href=\"");
    out.push_str(&css_url);
    out.push_str("\" />\n");
    out.push_str("  </head>\n");
    out.push_str("  <body>\n");
    out.push_str("    <div id=\"swagger-ui\"></div>\n");
    out.push_str("    <script src=\"");
    out.push_str(&bundle_url);
    out.push_str("\" charset=\"UTF-8\"></script>\n");
    out.push_str("    <script src=\"");
    out.push_str(&initializer_url);
    out.push_str("\" charset=\"UTF-8\"></script>\n");
    out.push_str("  </body>\n");
    out.push_str("</html>\n");
    out
}

/// External Swagger UI initializer script so the default `script-src 'self'`
/// CSP can boot the docs UI without permitting inline JavaScript.
#[cfg(feature = "openapi")]
#[must_use]
pub fn swagger_ui_initializer_js(spec_url: &str) -> String {
    let spec_url = serde_json::to_string(spec_url)
        .unwrap_or_else(|e| format!("\"/openapi.json?serialization_error={e}\""));
    let mut out = String::with_capacity(256);
    out.push_str("window.onload = function() {\n");
    out.push_str("  window.ui = SwaggerUIBundle({\n");
    out.push_str("    url: ");
    out.push_str(&spec_url);
    out.push_str(",\n");
    out.push_str("    dom_id: \"#swagger-ui\",\n");
    out.push_str("    deepLinking: true\n");
    out.push_str("  });\n");
    out.push_str("};\n");
    out
}

#[cfg(feature = "openapi")]
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

// ──────────────────────────────────────────────────────────────────
// Opaque-schema (placeholder) detection
// ──────────────────────────────────────────────────────────────────

/// Keys the generated placeholder may carry. Anything else means the schema
/// says something real about its instances, so it is not a placeholder.
#[cfg(feature = "openapi")]
const PLACEHOLDER_KEYS: [&str; 3] = ["type", "title", "description"];

/// True when `schema` is exactly the opaque object placeholder
/// [`generate_spec`] emits for a referenced type that has no `OpenApiSchema`:
/// `{"type":"object","title":…}` and nothing else.
///
/// Matched by *shape*, not by the absence of `properties` alone. An object can
/// describe its instances without that key — `additionalProperties` (a map),
/// `oneOf`/`allOf`/`anyOf`, `patternProperties`, a bare `$ref` — and a schema
/// somebody registered deliberately through
/// [`OpenApiConfig::register_schema`] in one of those forms is a real contract
/// a client generator can render. Flagging it would make
/// `autumn openapi export --strict` fail CI over a fully typed map. So a
/// placeholder is recognised as an object carrying no key beyond `title` /
/// `description`, which is precisely what the back-fill emits.
///
/// This is the single canonical predicate: the MCP tool-catalog builder
/// ([`crate::mcp`]) applies it to a tool's `inputSchema`, and
/// [`opaque_component_schemas`] applies it to a built spec's components.
#[cfg(feature = "openapi")]
#[must_use]
pub fn is_opaque_object_schema(schema: &serde_json::Value) -> bool {
    if schema.get("type").and_then(serde_json::Value::as_str) != Some("object") {
        return false;
    }
    schema.as_object().is_none_or(|map| {
        map.keys()
            .all(|key| PLACEHOLDER_KEYS.contains(&key.as_str()))
    })
}

/// One component schema that degraded to the opaque object placeholder, with
/// the operations that reference it.
///
/// Produced by [`opaque_component_schemas`]. A consumer of the spec (a client
/// generator, Swagger UI, the MCP projection) can only render such a schema as
/// an untyped blob — `unknown` in TypeScript, `serde_json::Value` in Rust — so
/// `autumn openapi export` reports these rather than letting the contract
/// degrade silently.
#[cfg(feature = "openapi")]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpaqueSchema {
    /// Component key under `#/components/schemas/`.
    pub schema: String,
    /// `"METHOD /path"` for every operation referencing it, sorted and deduped.
    /// Empty when the component is registered but unreferenced.
    pub referenced_by: Vec<String>,
}

/// Report every component schema in `spec` that degraded to the opaque
/// placeholder, sorted by component name.
///
/// The fix for each entry is to add `#[derive(OpenApiSchema)]` to the offending
/// type (or register a hand-written schema via
/// [`OpenApiConfig::register_schema`]), which makes the back-fill resolve the
/// real field-accurate schema instead of the placeholder.
#[cfg(feature = "openapi")]
#[must_use]
pub fn opaque_component_schemas(spec: &OpenApiSpec) -> Vec<OpaqueSchema> {
    let Some(components) = spec.components.as_ref() else {
        return Vec::new();
    };

    let opaque: std::collections::BTreeSet<&str> = components
        .schemas
        .iter()
        .filter(|(_, schema)| is_opaque_object_schema(schema))
        .map(|(name, _)| name.as_str())
        .collect();
    if opaque.is_empty() {
        return Vec::new();
    }

    // Pre-compute each component's own outgoing refs, so attribution can follow
    // the component graph rather than stopping at an operation's direct refs.
    // An operation usually reaches an opaque type *indirectly* — `POST /orders`
    // takes a derived `Order` whose `address` field `$ref`s an underived
    // `Address` — and reporting `Address` with an empty `referenced_by` would
    // hide the very operation whose contract is degraded.
    let component_refs: BTreeMap<&str, Vec<String>> = components
        .schemas
        .iter()
        .map(|(name, schema)| {
            let mut out = Vec::new();
            collect_body_ref_identities(schema, &mut out);
            (name.as_str(), out)
        })
        .collect();

    // Map each opaque component to the operations that reach it. A reference
    // can sit anywhere in an operation (body, response, parameter schema, or
    // nested inside an array/nullable wrapper), so walk the serialized
    // operation wholesale rather than probing known slots, then close over the
    // component graph from whatever that turns up.
    let mut refs: BTreeMap<&str, std::collections::BTreeSet<String>> = BTreeMap::new();
    for (path, item) in &spec.paths {
        for (method, operation) in path_item_operations(item) {
            let Ok(value) = serde_json::to_value(operation) else {
                continue;
            };
            let mut frontier = Vec::new();
            collect_body_ref_identities(&value, &mut frontier);

            // Breadth-first over components, `seen` guarding the cycles a
            // self- or mutually-recursive schema creates.
            let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
            while let Some(identity) = frontier.pop() {
                if !seen.insert(identity.clone()) {
                    continue;
                }
                if let Some(name) = opaque.get(identity.as_str()) {
                    refs.entry(name)
                        .or_default()
                        .insert(format!("{method} {path}"));
                }
                if let Some(nested) = component_refs.get(identity.as_str()) {
                    frontier.extend(nested.iter().cloned());
                }
            }
        }
    }

    opaque
        .into_iter()
        .map(|schema| OpaqueSchema {
            schema: schema.to_owned(),
            referenced_by: refs
                .get(schema)
                .map(|set| set.iter().cloned().collect())
                .unwrap_or_default(),
        })
        .collect()
}

/// Yield each `(METHOD, operation)` pair present on a [`PathItem`].
#[cfg(feature = "openapi")]
fn path_item_operations(item: &PathItem) -> Vec<(&'static str, &Operation)> {
    [
        ("GET", item.get.as_ref()),
        ("POST", item.post.as_ref()),
        ("PUT", item.put.as_ref()),
        ("PATCH", item.patch.as_ref()),
        ("DELETE", item.delete.as_ref()),
    ]
    .into_iter()
    .filter_map(|(method, op)| op.map(|op| (method, op)))
    .collect()
}

// ──────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────

#[cfg(all(test, feature = "openapi"))]
mod tests {
    use super::*;

    fn make_doc() -> ApiDoc {
        ApiDoc {
            method: "GET",
            path: "/users/{id}",
            operation_id: "get_user",
            summary: Some("Fetch a user"),
            description: None,
            tags: &[],
            path_params: &["id"],
            request_body: None,
            response: None,
            success_status: 200,
            hidden: false,
            query_schema: None,
            secured: false,
            required_roles: &[],
            register_schemas: None,
            api_version: None,
            ..Default::default()
        }
    }

    #[test]
    fn config_builder_methods_work() {
        let config = OpenApiConfig::new("Demo", "1.0.0")
            .description("A cool API")
            .openapi_json_path("/api.json")
            .swagger_ui_path(None)
            .session_cookie_name("demo.sid");

        assert_eq!(config.title, "Demo");
        assert_eq!(config.version, "1.0.0");
        assert_eq!(config.description.unwrap(), "A cool API");
        assert_eq!(config.openapi_json_path, "/api.json");
        assert_eq!(config.swagger_ui_path, None);
        assert_eq!(config.session_cookie_name, "demo.sid");
    }

    #[test]
    fn secured_spec_uses_configured_session_cookie_name() {
        let mut doc = make_doc();
        doc.path = "/protected";
        doc.operation_id = "protected";
        doc.path_params = &[];
        doc.secured = true;

        let config = OpenApiConfig::new("Demo", "1.0.0").session_cookie_name("demo.sid");
        let spec = generate_spec(&config, &[&doc]);
        let scheme = &spec
            .components
            .as_ref()
            .expect("secured routes emit security components")
            .security_schemes["SessionAuth"];

        assert_eq!(scheme["type"], "apiKey");
        assert_eq!(scheme["in"], "cookie");
        assert_eq!(scheme["name"], "demo.sid");
    }

    #[test]
    fn generate_spec_builds_path_with_parameters() {
        let doc = make_doc();
        let config = OpenApiConfig::new("Demo", "1.0.0");
        let spec = generate_spec(&config, &[&doc]);

        assert_eq!(spec.openapi, "3.1.0");
        assert_eq!(spec.info.title, "Demo");
        assert!(spec.paths.contains_key("/users/{id}"));

        let op = spec.paths["/users/{id}"].get.as_ref().unwrap();
        assert_eq!(op.operation_id, "get_user");
        assert_eq!(op.parameters.len(), 1);
        assert_eq!(op.parameters[0].name, "id");
        assert_eq!(op.parameters[0].location, "path");
        assert_eq!(op.tags, vec!["users".to_owned()]);
    }

    #[test]
    fn generate_spec_skips_hidden_routes() {
        let mut doc = make_doc();
        doc.hidden = true;
        let config = OpenApiConfig::new("Demo", "1.0.0");
        let spec = generate_spec(&config, &[&doc]);
        assert!(spec.paths.is_empty());
    }

    #[test]
    fn generate_spec_writes_request_body_ref() {
        let mut doc = make_doc();
        doc.method = "POST";
        doc.path = "/users";
        doc.operation_id = "create_user";
        doc.path_params = &[];
        doc.request_body = Some(SchemaEntry {
            name: "CreateUser",
            kind: SchemaKind::Ref,
            identity: None,
        });
        doc.success_status = 201;

        let config = OpenApiConfig::new("Demo", "1.0.0");
        let spec = generate_spec(&config, &[&doc]);
        let op = spec.paths["/users"].post.as_ref().unwrap();
        let body = op.request_body.as_ref().unwrap();
        assert!(body.required);
        let media = body.content.get("application/json").unwrap();
        assert_eq!(
            media.schema,
            serde_json::json!({ "$ref": "#/components/schemas/CreateUser" }),
        );
        assert!(op.responses.contains_key("201"));
    }

    #[test]
    fn generate_spec_inlines_primitive_response() {
        let mut doc = make_doc();
        doc.response = Some(SchemaEntry {
            name: "string",
            kind: SchemaKind::Primitive("string"),
            identity: None,
        });
        let config = OpenApiConfig::new("Demo", "1.0.0");
        let spec = generate_spec(&config, &[&doc]);
        let op = spec.paths["/users/{id}"].get.as_ref().unwrap();
        let media = op.responses["200"].content.get("application/json").unwrap();
        assert_eq!(media.schema, serde_json::json!({ "type": "string" }));
    }

    #[test]
    fn swagger_ui_html_uses_same_origin_assets() {
        let html = swagger_ui_html(
            "Demo",
            "/swagger-ui/swagger-ui.css",
            "/swagger-ui/swagger-ui-bundle.js",
            "/swagger-ui/swagger-initializer.js",
        );
        assert!(html.contains("/swagger-ui/swagger-ui.css"));
        assert!(html.contains("/swagger-ui/swagger-ui-bundle.js"));
        assert!(html.contains("/swagger-ui/swagger-initializer.js"));
        assert!(!html.contains("unpkg.com"));
        assert!(!html.contains("window.onload = function()"));
    }

    #[test]
    fn swagger_ui_initializer_js_references_spec_url() {
        let js = swagger_ui_initializer_js("/openapi.json");
        assert!(js.contains("SwaggerUIBundle"));
        assert!(js.contains(r#""/openapi.json""#));
    }

    #[test]
    fn generate_spec_includes_additional_schemas() {
        let doc = make_doc();
        let config = OpenApiConfig::new("Demo", "1.0.0")
            .register_schema("Foo", serde_json::json!({ "type": "object" }));
        let spec = generate_spec(&config, &[&doc]);
        let components = spec.components.unwrap();
        assert!(components.schemas.contains_key("Foo"));
    }

    #[test]
    fn generate_spec_back_fills_unregistered_ref_schemas() {
        // A Json<CreateUser> handler emits a `$ref` with no component
        // schema registered. The generator must back-fill a placeholder
        // schema so the resulting OpenAPI document is valid.
        let mut doc = make_doc();
        doc.method = "POST";
        doc.path = "/users";
        doc.path_params = &[];
        doc.request_body = Some(SchemaEntry {
            name: "CreateUser",
            kind: SchemaKind::Ref,
            identity: None,
        });
        doc.response = Some(SchemaEntry {
            name: "User",
            kind: SchemaKind::Ref,
            identity: None,
        });

        let config = OpenApiConfig::new("Demo", "1.0.0");
        let spec = generate_spec(&config, &[&doc]);
        let components = spec.components.expect("components must be emitted");
        let create = components
            .schemas
            .get("CreateUser")
            .expect("CreateUser should be back-filled");
        let user = components
            .schemas
            .get("User")
            .expect("User should be back-filled");
        assert_eq!(create["type"], "object");
        assert_eq!(create["title"], "CreateUser");
        assert_eq!(user["type"], "object");
        assert_eq!(user["title"], "User");
    }

    #[test]
    fn generate_spec_preserves_user_registered_schemas_over_backfill() {
        let mut doc = make_doc();
        doc.response = Some(SchemaEntry {
            name: "User",
            kind: SchemaKind::Ref,
            identity: None,
        });

        let user_schema = serde_json::json!({
            "type": "object",
            "properties": {"id": {"type": "integer"}},
        });
        let config =
            OpenApiConfig::new("Demo", "1.0.0").register_schema("User", user_schema.clone());
        let spec = generate_spec(&config, &[&doc]);
        let components = spec.components.unwrap();
        let stored = components.schemas.get("User").unwrap();
        assert_eq!(stored, &user_schema, "user schema must not be overwritten");
    }

    #[test]
    fn status_description_returns_correct_strings() {
        assert_eq!(status_description(200), "OK");
        assert_eq!(status_description(201), "Created");
        assert_eq!(status_description(202), "Accepted");
        assert_eq!(status_description(204), "No Content");
        assert_eq!(status_description(301), "Moved Permanently");
        assert_eq!(status_description(302), "Found");
        assert_eq!(status_description(400), "Bad Request");
        assert_eq!(status_description(401), "Unauthorized");
        assert_eq!(status_description(403), "Forbidden");
        assert_eq!(status_description(404), "Not Found");
        assert_eq!(status_description(409), "Conflict");
        assert_eq!(status_description(413), "Payload Too Large");
        assert_eq!(status_description(415), "Unsupported Media Type");
        assert_eq!(status_description(422), "Unprocessable Entity");
        assert_eq!(status_description(500), "Internal Server Error");
        assert_eq!(status_description(503), "Service Unavailable");
        assert_eq!(status_description(418), "Response");
    }

    #[test]
    fn default_tag_picks_first_static_segment() {
        assert_eq!(default_tag("/users/{id}"), Some("users"));
        assert_eq!(default_tag("/api/v1/users"), Some("api"));
        assert_eq!(default_tag("/"), None);
        assert_eq!(default_tag("/{id}"), None);
    }

    // ── OpenAPI 3.1 compliance tests (RED phase) ───────────────────────────

    #[test]
    fn spec_version_is_3_1_0() {
        let config = OpenApiConfig::new("Demo", "1.0.0");
        let spec = generate_spec(&config, &[]);
        assert_eq!(
            spec.openapi, "3.1.0",
            "Autumn must emit OpenAPI 3.1.0, not {}",
            spec.openapi
        );
    }

    #[test]
    fn nullable_ref_uses_openapi_3_1_one_of() {
        // OpenAPI 3.1 aligns with JSON Schema 2020-12: nullable refs use
        // `oneOf: [{$ref: ...}, {type: "null"}]` instead of 3.0's
        // `nullable: true` + `allOf` workaround.
        static INNER: SchemaEntry = SchemaEntry {
            name: "User",
            kind: SchemaKind::Ref,
            identity: None,
        };
        let entry = SchemaEntry {
            name: "nullable",
            kind: SchemaKind::Nullable(&INNER),
            identity: None,
        };
        let value = schema_value_for(&entry, &SchemaComponentIndex::default());
        assert!(
            value.get("nullable").is_none(),
            "3.1 must not emit `nullable: true` (that is 3.0 only)"
        );
        assert!(
            value.get("allOf").is_none(),
            "3.1 must not use allOf for nullable refs"
        );
        let one_of = value["oneOf"]
            .as_array()
            .expect("3.1 nullable ref must use oneOf");
        assert_eq!(one_of.len(), 2);
        assert_eq!(
            one_of[0]["$ref"], "#/components/schemas/User",
            "first oneOf branch must be the $ref"
        );
        assert_eq!(
            one_of[1]["type"], "null",
            "second oneOf branch must be {{type: null}}"
        );
    }

    #[test]
    fn nullable_primitive_uses_type_array() {
        // OpenAPI 3.1 uses `type: ["integer", "null"]` for nullable
        // primitives instead of the 3.0 `nullable: true` flag.
        static INNER: SchemaEntry = SchemaEntry {
            name: "integer",
            kind: SchemaKind::Primitive("integer"),
            identity: None,
        };
        let entry = SchemaEntry {
            name: "nullable",
            kind: SchemaKind::Nullable(&INNER),
            identity: None,
        };
        let value = schema_value_for(&entry, &SchemaComponentIndex::default());
        assert!(
            value.get("nullable").is_none(),
            "3.1 must not emit `nullable: true`"
        );
        let types = value["type"]
            .as_array()
            .expect("3.1 nullable primitive must use a type array");
        assert!(
            types.contains(&serde_json::Value::String("integer".to_owned())),
            "type array must include the base type"
        );
        assert!(
            types.contains(&serde_json::Value::String("null".to_owned())),
            "type array must include null"
        );
    }

    #[test]
    fn write_openapi_spec_to_dist_creates_json_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dist = tmp.path().join("dist");
        std::fs::create_dir_all(&dist).unwrap();

        let config = OpenApiConfig::new("TestAPI", "2.0.0");
        let spec = generate_spec(&config, &[]);

        write_openapi_spec_to_dist(&spec, &dist).expect("write must succeed");

        let json_path = dist.join("openapi.json");
        assert!(json_path.exists(), "dist/openapi.json must be written");

        let content = std::fs::read_to_string(&json_path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed["openapi"], "3.1.0");
        assert_eq!(parsed["info"]["title"], "TestAPI");
    }

    #[test]
    fn write_openapi_spec_to_dist_creates_yaml_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dist = tmp.path().join("dist");
        std::fs::create_dir_all(&dist).unwrap();

        let config = OpenApiConfig::new("TestAPI", "2.0.0");
        let spec = generate_spec(&config, &[]);

        write_openapi_spec_to_dist(&spec, &dist).expect("write must succeed");

        let yaml_path = dist.join("openapi.yaml");
        assert!(yaml_path.exists(), "dist/openapi.yaml must be written");

        let content = std::fs::read_to_string(&yaml_path).unwrap();
        assert!(
            content.contains("openapi:"),
            "YAML must include the openapi field"
        );
        assert!(content.contains("3.1.0"), "YAML must include the version");
        assert!(content.contains("TestAPI"), "YAML must include the title");
    }

    #[test]
    fn schema_registry_into_map_returns_all_schemas() {
        let mut registry = SchemaRegistry::default();
        registry.insert("Foo", serde_json::json!({ "type": "string" }));
        registry.insert("Bar", serde_json::json!({ "type": "integer" }));

        let map = registry.into_map();
        assert_eq!(map.len(), 2);
        assert_eq!(
            map.get("Foo").unwrap(),
            &serde_json::json!({ "type": "string" })
        );
        assert_eq!(
            map.get("Bar").unwrap(),
            &serde_json::json!({ "type": "integer" })
        );
    }

    #[test]
    fn schema_registry_deduplicates() {
        struct Foo;
        impl OpenApiSchema for Foo {
            fn schema_name() -> &'static str {
                "Foo"
            }
            fn schema() -> serde_json::Value {
                serde_json::json!({ "type": "object", "title": "Foo" })
            }
        }

        let mut registry = SchemaRegistry::default();
        registry.register::<Foo>();
        registry.register::<Foo>();
        assert_eq!(registry.schemas().len(), 1);
    }

    #[test]
    fn primitive_impls_cover_common_types() {
        assert_eq!(<String as OpenApiSchema>::schema_name(), "string");
        assert_eq!(<i32 as OpenApiSchema>::schema_name(), "integer");
        assert_eq!(<bool as OpenApiSchema>::schema_name(), "boolean");
        assert_eq!(<f64 as OpenApiSchema>::schema_name(), "number");
    }

    #[test]
    fn swagger_ui_html_embeds_spec_url() {
        let html = swagger_ui_html(
            "My API",
            "/swagger-ui/swagger-ui.css",
            "/swagger-ui/swagger-ui-bundle.js",
            "/swagger-ui/swagger-initializer.js",
        );
        assert!(html.contains("/swagger-ui/swagger-ui.css"));
        assert!(html.contains("My API"));
    }

    #[test]
    fn swagger_ui_html_escapes_attributes() {
        let html = swagger_ui_html(
            "A \"cool\" & fun API",
            "/swagger-ui/swagger-ui.css?x=<y>",
            "/swagger-ui/swagger-ui-bundle.js",
            "/swagger-ui/swagger-initializer.js",
        );
        assert!(html.contains("/swagger-ui/swagger-ui.css?x=&lt;y&gt;"));
        assert!(html.contains("A &quot;cool&quot; &amp; fun API"));
    }

    // ── Security requirement generation (#1158) ──────────────────────────────

    fn make_secured_doc(
        secured: bool,
        required_roles: &'static [&'static str],
        required_scopes: &'static [&'static str],
    ) -> ApiDoc {
        let mut doc = make_doc();
        doc.path = "/secured";
        doc.operation_id = "secured_op";
        doc.path_params = &[];
        doc.secured = secured;
        doc.required_roles = required_roles;
        doc.required_scopes = required_scopes;
        doc
    }

    #[test]
    fn unsecured_route_has_no_security_requirement() {
        let doc = make_secured_doc(false, &[], &[]);
        let config = OpenApiConfig::new("Demo", "1.0.0");
        let spec = generate_spec(&config, &[&doc]);
        let op = spec.paths["/secured"].get.as_ref().unwrap();
        assert!(op.security.is_empty());
    }

    #[test]
    fn bare_secured_uses_session_auth() {
        let doc = make_secured_doc(true, &[], &[]);
        let config = OpenApiConfig::new("Demo", "1.0.0");
        let spec = generate_spec(&config, &[&doc]);
        let op = spec.paths["/secured"].get.as_ref().unwrap();
        assert_eq!(op.security.len(), 1);
        assert!(op.security[0].contains_key("SessionAuth"));
        assert!(!op.security[0].contains_key("BearerAuth"));
    }

    #[test]
    fn role_only_uses_session_auth() {
        let doc = make_secured_doc(true, &["admin"], &[]);
        let config = OpenApiConfig::new("Demo", "1.0.0");
        let spec = generate_spec(&config, &[&doc]);
        let op = spec.paths["/secured"].get.as_ref().unwrap();
        assert_eq!(op.security.len(), 1);
        assert!(op.security[0].contains_key("SessionAuth"));
        assert!(!op.security[0].contains_key("BearerAuth"));
    }

    #[test]
    fn scope_only_uses_bearer_auth_with_empty_array() {
        let doc = make_secured_doc(true, &[], &["posts:write"]);
        let config = OpenApiConfig::new("Demo", "1.0.0");
        let spec = generate_spec(&config, &[&doc]);
        let op = spec.paths["/secured"].get.as_ref().unwrap();
        assert_eq!(op.security.len(), 1);
        assert!(op.security[0].contains_key("BearerAuth"));
        assert!(!op.security[0].contains_key("SessionAuth"));
        // OpenAPI spec: HTTP bearer value array must be empty (not scope names).
        assert!(op.security[0]["BearerAuth"].is_empty());
        // BearerAuth scheme is registered in components.
        let schemes = &spec.components.as_ref().unwrap().security_schemes;
        assert!(schemes.contains_key("BearerAuth"));
        assert_eq!(schemes["BearerAuth"]["scheme"], "bearer");
    }

    #[test]
    fn mixed_role_and_scope_uses_both_auth_schemes() {
        let doc = make_secured_doc(true, &["admin"], &["posts:write"]);
        let config = OpenApiConfig::new("Demo", "1.0.0");
        let spec = generate_spec(&config, &[&doc]);
        let op = spec.paths["/secured"].get.as_ref().unwrap();
        assert_eq!(op.security.len(), 1);
        // Both in the same object = AND semantics.
        assert!(op.security[0].contains_key("SessionAuth"));
        assert!(op.security[0].contains_key("BearerAuth"));
    }

    #[test]
    fn bearer_auth_scheme_registered_only_for_scoped_routes() {
        let unscoped = make_secured_doc(true, &["admin"], &[]);
        let config = OpenApiConfig::new("Demo", "1.0.0");
        let spec = generate_spec(&config, &[&unscoped]);
        let schemes = &spec.components.as_ref().unwrap().security_schemes;
        assert!(!schemes.contains_key("BearerAuth"));
    }

    // Regression (issue #1972): the fallback display-key assignment must be
    // collision-proof. This generalizes the reviewer's `app::app::Args` /
    // `app::Args` example to a pair that still clashes under the deterministic
    // sorted assignment order: `a::x::Args` sorts first and claims the 2-segment
    // suffix `x.Args`, then `x::Args` exhausts its suffix ladder (its only
    // qualified candidate IS `x.Args`) and falls back to
    // `component_key("x::Args")` == `x.Args` — the exact key `a::x::Args` already
    // took. Without the disambiguator both identities would map to `x.Args`, so
    // the second schema would silently overwrite the first.
    #[test]
    fn colliding_fallback_keys_are_disambiguated() {
        let refs = vec![
            ("a::x::Args".to_owned(), "Args".to_owned()),
            ("x::Args".to_owned(), "Args".to_owned()),
        ];
        let by_identity = assign_display_keys(&refs);

        let a = by_identity.get("a::x::Args").expect("a::x::Args assigned");
        let b = by_identity.get("x::Args").expect("x::Args assigned");
        assert_ne!(
            a, b,
            "distinct identities must map to distinct display keys, got {a} == {b}"
        );
        // `a::x::Args` claims the suffix key; `x::Args` must be pushed onto the
        // deterministic `-N` disambiguator rather than overwriting it — pinning
        // this proves the disambiguator branch actually ran.
        assert_eq!(a, "x.Args");
        assert_eq!(b, "x.Args-2");
        // Both keys are valid OpenAPI component keys.
        for key in [a, b] {
            assert!(
                key.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_')),
                "display key {key} is not a valid component key"
            );
        }
    }

    // Determinism: the same identity set must yield the same keys regardless of
    // the order the `(identity, base)` pairs are supplied.
    #[test]
    fn assign_display_keys_is_order_independent() {
        let forward = vec![
            ("app::app::Args".to_owned(), "Args".to_owned()),
            ("app::Args".to_owned(), "Args".to_owned()),
            ("other::mod::Args".to_owned(), "Args".to_owned()),
        ];
        let mut reversed = forward.clone();
        reversed.reverse();
        assert_eq!(
            assign_display_keys(&forward),
            assign_display_keys(&reversed),
            "display-key assignment must not depend on input order"
        );
    }
}
