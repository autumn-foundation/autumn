// OpenAPI/JSON/JSON-schema all appear frequently here and are legitimate
// acronyms, so silence clippy::doc_markdown rather than wrapping every
// mention in backticks.
#![allow(clippy::doc_markdown)]

//! OpenAPI (Swagger) specification auto-generation.
//!
//! Autumn automatically infers an OpenAPI 3.0 document from your
//! annotated routes ([`get`](crate::get), [`post`](crate::post), etc.),
//! their path parameters, and the extractor / response types in each
//! handler signature. The generated spec is served at `/v3/api-docs` and
//! a Swagger UI is served at `/swagger-ui` when the feature is enabled.
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
//! * `GET /v3/api-docs` — serves the generated `openapi.json`.
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
// Public metadata attached to each Route
// ──────────────────────────────────────────────────────────────────

/// OpenAPI metadata emitted alongside every annotated route.
///
/// Populated by the route macros ([`get`](crate::get),
/// [`post`](crate::post), etc.) from the handler's path, signature, and
/// any [`#[api_doc(...)]`](crate::api_doc) overrides.
#[derive(Clone, Debug, Default)]
// A flat, generated metadata descriptor; the independent boolean flags
// (hidden, secured, sunset_opt_out, has_policy, mcp_tool, mcp_exclude) each
// model a distinct, orthogonal route property, so grouping them into a
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
}

/// Reference to a schema definition, produced by the route macros.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct SchemaEntry {
    /// Short human-readable type name (used as `#/components/schemas/Name`).
    pub name: &'static str,
    /// Whether this is a primitive JSON type (string/number/bool/array) as
    /// opposed to a named object ref.
    pub kind: SchemaKind,
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
    /// Path serving the raw `openapi.json`. Defaults to `/v3/api-docs`.
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
// Serializable OpenAPI 3.0 document types.
//
// Only the fields Autumn actually populates are modelled — unused
// OpenAPI keys (callbacks, links, discriminators…) are intentionally
// omitted so the generated JSON stays clean. Gated behind the
// `openapi` feature so the runtime spec builder doesn't add code
// size / dependency pressure to apps that never serve a JSON spec.
// ──────────────────────────────────────────────────────────────────

#[cfg(feature = "openapi")]
/// Represents a root OpenAPI 3.0 specification document.
#[derive(Debug, Serialize, Deserialize)]
pub struct OpenApiSpec {
    /// The OpenAPI version string (e.g., `3.0.3`).
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
    /// property a separate query key — the correct mapping for `Query<T>`.
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

    // Collect every named schema reference produced by any operation so
    // we can back-fill component entries for types the user didn't
    // explicitly register. Without this, auto-inferred `Json<MyDto>`
    // payloads would emit `$ref`s pointing at nonexistent component
    // schemas — an invalid OpenAPI document.
    let mut referenced_names: std::collections::BTreeSet<&'static str> =
        std::collections::BTreeSet::new();

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

        if let Some(entry) = &api_doc.request_body {
            collect_ref_names(entry, &mut referenced_names);
        }
        if let Some(entry) = &api_doc.response {
            collect_ref_names(entry, &mut referenced_names);
        }
        if let Some(entry) = &api_doc.query_schema {
            collect_ref_names(entry, &mut referenced_names);
        }

        let operation = operation_for(api_doc, &config.api_versions, now);
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

    // Back-fill a minimal `{"type": "object", "title": "X"}` schema for
    // every referenced name the user didn't already register. Types that
    // implement OpenApiSchema can be registered explicitly via
    // OpenApiConfig::register_schema to replace the placeholder.
    for name in referenced_names {
        if !registry.schemas().contains_key(name) {
            registry.insert(
                name,
                serde_json::json!({
                    "type": "object",
                    "title": name,
                }),
            );
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

    let components_map = registry.into_map();
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
    // is serialized as an independent query key (e.g. `?q=foo&page=2`),
    // which matches what the server's `Query<T>` deserialization expects.
    if let Some(query_entry) = &api_doc.query_schema {
        parameters.push(Parameter {
            name: query_entry.name.to_owned(),
            location: "query".to_owned(),
            required: false,
            schema: schema_value_for(query_entry),
            style: Some("form".to_owned()),
            explode: Some(true),
        });
    }

    let request_body = api_doc.request_body.as_ref().map(|entry| RequestBody {
        required: true,
        content: std::iter::once((
            "application/json".to_owned(),
            MediaType {
                schema: schema_value_for(entry),
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
                    schema: schema_value_for(entry),
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
#[cfg(feature = "openapi")]
#[must_use]
pub fn schema_entry_to_value(entry: &SchemaEntry) -> serde_json::Value {
    schema_value_for(entry)
}

#[cfg(feature = "openapi")]
fn schema_value_for(entry: &SchemaEntry) -> serde_json::Value {
    match entry.kind {
        SchemaKind::Primitive(json_type) => serde_json::json!({ "type": json_type }),
        SchemaKind::Ref => {
            serde_json::json!({ "$ref": format!("#/components/schemas/{}", entry.name) })
        }
        SchemaKind::Array(items) => serde_json::json!({
            "type": "array",
            "items": schema_value_for(items),
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
                            schema_value_for(inner),
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

/// Walk into a `SchemaEntry` and yield every named ref reached through
/// `Array` / `Nullable` wrappers. Back-fill logic uses this so a
/// `Json<Vec<User>>` response registers a `User` component schema.
#[cfg(feature = "openapi")]
fn collect_ref_names(entry: &SchemaEntry, out: &mut std::collections::BTreeSet<&'static str>) {
    match entry.kind {
        SchemaKind::Ref => {
            out.insert(entry.name);
        }
        SchemaKind::Array(inner) | SchemaKind::Nullable(inner) => collect_ref_names(inner, out),
        SchemaKind::Primitive(_) => {}
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
        });
        doc.response = Some(SchemaEntry {
            name: "User",
            kind: SchemaKind::Ref,
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
        };
        let entry = SchemaEntry {
            name: "nullable",
            kind: SchemaKind::Nullable(&INNER),
        };
        let value = schema_value_for(&entry);
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
        };
        let entry = SchemaEntry {
            name: "nullable",
            kind: SchemaKind::Nullable(&INNER),
        };
        let value = schema_value_for(&entry);
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
}
