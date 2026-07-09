#![allow(clippy::type_complexity, clippy::too_many_lines)]
//! First-party integration-testing utilities for Autumn applications.
//!
//! This module brings Autumn's testing story to parity with frameworks like
//! Spring Boot's `@SpringBootTest` + `MockMvc` and Django's `TestCase` +
//! `Client`. Import it in your integration tests:
//!
//! ```rust,ignore
//! use autumn_web::test::{TestApp, TestClient};
//! ```
//!
//! # Quick start
//!
//! ```rust,no_run
//! use autumn_web::prelude::*;
//! use autumn_web::test::TestApp;
//!
//! #[get("/hello")]
//! async fn hello() -> &'static str { "hi" }
//!
//! #[tokio::test]
//! async fn hello_returns_200() {
//!     let client = TestApp::new()
//!         .routes(routes![hello])
//!         .build();
//!
//!     client.get("/hello").send().await
//!         .assert_status(200)
//!         .assert_body_contains("hi");
//! }
//! ```
//!
//! # What's included
//!
//! | Type | Spring Boot equivalent | Purpose |
//! |------|----------------------|---------|
//! | [`TestApp`] | `@SpringBootTest` | Boot a fully-configured app for testing |
//! | [`TestClient`] | `MockMvc` / `WebTestClient` | Fluent HTTP request builder |
//! | [`TestResponse`] | `MvcResult` | Response with assertion helpers |
//! | `TestDb` | `@DataJpaTest` | Shared Postgres testcontainer with pool |
//!
//! # Structural HTML assertions
//!
//! Autumn renders server-side HTML (Maud + htmx), so tests should assert on a
//! page's *structure* — "the table has exactly N rows", "this link points at
//! `/notes/1`" — rather than brittle substrings. [`TestResponse`] parses the
//! body with a real HTML parser and matches against a CSS-selector subset
//! (tag, `.class`, `#id`, `[attr=…]`, plus descendant/child combinators), so
//! assertions survive cosmetic template changes (whitespace, attribute order,
//! wrapping markup) that would break [`TestResponse::assert_body_contains`].
//! They work for full documents and for partial/fragment responses (htmx
//! swaps) alike.
//!
//! The worked example below asserts a scaffolded notes-index page's row count
//! and the link target of each row. Every assertion returns `&Self`, so they
//! chain with the status/header/body matchers:
//!
//! ```rust
//! use autumn_web::test::TestResponse;
//! use axum::http::StatusCode;
//!
//! // The HTML a scaffolded `notes#index` view renders: a table with one
//! // `<tr>` per note, each linking to `/notes/{id}`.
//! let resp = TestResponse {
//!     status: StatusCode::OK,
//!     headers: vec![("content-type".into(), "text/html; charset=utf-8".into())],
//!     body: br#"
//!         <table class="notes">
//!           <tbody>
//!             <tr class="note-row"><td><a href="/notes/1">First note</a></td></tr>
//!             <tr class="note-row"><td><a href="/notes/2">Second note</a></td></tr>
//!             <tr class="note-row"><td><a href="/notes/3">Third note</a></td></tr>
//!           </tbody>
//!         </table>
//!     "#.to_vec(),
//! };
//!
//! resp.assert_ok()
//!     .assert_selector("table.notes")               // the table is present
//!     .assert_selector_count("tbody tr.note-row", 3) // exactly three rows
//!     .assert_attr("tr.note-row a", "href", "/notes/1") // first row's link target
//!     .assert_text("tr.note-row a", "First note")    // …and its visible text
//!     .assert_no_selector(".flash--error");          // no error flash rendered
//!
//! // Non-asserting accessors compose for custom checks:
//! assert_eq!(
//!     resp.selector_attr("tbody tr.note-row a", "href"),
//!     vec![Some("/notes/1".into()), Some("/notes/2".into()), Some("/notes/3".into())],
//! );
//! assert_eq!(resp.selector_count("tr.note-row"), 3);
//! ```
//!
//! # Test-data factories
//!
//! `#[model]` generates a `{Model}Factory` builder so tests only declare the
//! fields that matter for the scenario under test — all others stay at
//! `Default::default()`:
//!
//! ```rust
//! mod schema {
//!     autumn_web::reexports::diesel::table! {
//!         notes (id) {
//!             id -> Int8,
//!             title -> Text,
//!             body -> Text,
//!             pinned -> Bool,
//!         }
//!     }
//! }
//! use schema::notes;
//!
//! #[autumn_web::model]
//! pub struct Note {
//!     #[id]
//!     pub id: i64,
//!     pub title: String,
//!     pub body: String,
//!     pub pinned: bool,
//! }
//!
//! // Zero required args — every field defaults to its type's `Default`.
//! let draft: NewNote = Note::factory().build();
//! assert_eq!(draft.title, "");
//! assert!(!draft.pinned);
//!
//! // Override only the fields relevant to your test.
//! let draft = Note::factory().title("Hello").pinned(true).build();
//! assert_eq!(draft.title, "Hello");
//! assert!(draft.pinned);
//! assert_eq!(draft.body, ""); // untouched
//! ```
//!
//! To persist the record call `.create(&pool)` instead of `.build()` — it
//! inserts via Diesel and returns the fully-populated model (PK included).
//! Pair it with `TestDb` for a self-contained DB test:
//!
//! ```rust,ignore
//! #[tokio::test]
//! #[ignore = "requires Docker (testcontainers)"]
//! async fn note_round_trip() {
//!     let db = TestDb::shared().await;
//!     // run CREATE TABLE ... against db.pool() first, then:
//!     let note = Note::factory().title("TDD").create(&db.pool()).await;
//!     assert!(note.id > 0);
//!     assert_eq!(note.title, "TDD");
//! }
//! ```
//!
//! # Database testing
//!
//! For tests that need a real database, use `TestDb` to share a single
//! Postgres container across your test suite (rather than one per test):
//!
//! ```rust,ignore
//! use autumn_web::test::{TestApp, TestDb};
//!
//! #[tokio::test]
//! async fn creates_user_in_db() {
//!     let db = TestDb::shared().await;
//!     let client = TestApp::new()
//!         .routes(routes![create_user, get_user])
//!         .with_db(db.pool())
//!         .build();
//!
//!     client.post("/users")
//!         .json(&serde_json::json!({"name": "Alice"}))
//!         .send().await
//!         .assert_status(201);
//! }
//! ```

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use tower::ServiceExt;

use crate::config::AutumnConfig;
use crate::route::Route;

use crate::state::AppState;

#[cfg(feature = "db")]
use diesel_async::AsyncPgConnection;
#[cfg(feature = "db")]
use diesel_async::RunQueryDsl;
#[cfg(feature = "db")]
use diesel_async::pooled_connection::deadpool::Pool;

// ── Mail recording helpers ─────────────────────────────────────

/// Snapshot of an email captured by the built-in test mail recorder.
///
/// Available on [`TestClient`] via [`TestClient::sent_mail()`] when the `mail`
/// feature is enabled.
///
/// # Example
///
/// ```rust,ignore
/// use autumn_web::test::TestApp;
///
/// let client = TestApp::new().config(cfg).routes(routes![handler]).build();
/// client.post("/signup").json(&body).send().await.assert_ok();
///
/// // ≤ 3 lines to assert an email was sent:
/// client.assert_email_count(1);
/// client.assert_email_sent(|m| m.to.iter().any(|a| a == "alice@example.com"));
/// client.assert_email_sent(|m| m.subject == "Welcome!");
/// ```
#[cfg(feature = "mail")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SentMail {
    /// `From` header value (after mailer defaults are applied).
    pub from: Option<String>,
    /// `Reply-To` header value.
    pub reply_to: Option<String>,
    /// `To` recipients.
    pub to: Vec<String>,
    /// `Subject` header.
    pub subject: String,
    /// HTML body, if provided.
    pub html: Option<String>,
    /// Plain-text body, if provided.
    pub text: Option<String>,
    /// Files attached to this message, in declared order.
    pub attachments: Vec<crate::mail::MailAttachment>,
}

#[cfg(feature = "mail")]
impl From<&crate::mail::Mail> for SentMail {
    fn from(m: &crate::mail::Mail) -> Self {
        Self {
            from: m.from.clone(),
            reply_to: m.reply_to.clone(),
            to: m.to.clone(),
            subject: m.subject.clone(),
            html: m.html.clone(),
            text: m.text.clone(),
            attachments: m.attachments.clone(),
        }
    }
}

/// Built-in per-`TestClient` recording mail interceptor.
///
/// Auto-installed by [`TestApp::build`] — no `.with_mail_interceptor()` needed.
/// Composes with any user-supplied interceptor (the user's interceptor still runs).
#[cfg(feature = "mail")]
#[derive(Clone, Default)]
struct MailRecorder {
    mails: std::sync::Arc<std::sync::Mutex<Vec<SentMail>>>,
}

#[cfg(feature = "mail")]
impl MailRecorder {
    fn new() -> Self {
        Self::default()
    }

    fn get_sent(&self) -> Vec<SentMail> {
        self.mails.lock().unwrap().clone()
    }
}

#[cfg(feature = "mail")]
impl crate::interceptor::MailInterceptor for MailRecorder {
    fn intercept<'a>(
        &'a self,
        mail: &'a crate::mail::Mail,
        next: std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<(), crate::mail::MailError>> + Send + 'a>,
        >,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<(), crate::mail::MailError>> + Send + 'a>,
    > {
        let snapshot = SentMail::from(mail);
        let mails = std::sync::Arc::clone(&self.mails);
        Box::pin(async move {
            let result = next.await;
            if result.is_ok() {
                mails.lock().unwrap().push(snapshot);
            }
            result
        })
    }
}

/// Chains two [`MailInterceptor`](crate::interceptor::MailInterceptor)s so that
/// `first` runs before `second`, both before the underlying transport.
#[cfg(feature = "mail")]
struct ChainedMailInterceptor {
    first: std::sync::Arc<dyn crate::interceptor::MailInterceptor>,
    second: std::sync::Arc<dyn crate::interceptor::MailInterceptor>,
}

#[cfg(feature = "mail")]
impl crate::interceptor::MailInterceptor for ChainedMailInterceptor {
    fn intercept<'a>(
        &'a self,
        mail: &'a crate::mail::Mail,
        next: std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<(), crate::mail::MailError>> + Send + 'a>,
        >,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<(), crate::mail::MailError>> + Send + 'a>,
    > {
        let second_next = self.second.intercept(mail, next);
        self.first.intercept(mail, second_next)
    }
}

// ── TestApp ────────────────────────────────────────────────────

/// Builder for constructing a fully-configured Autumn application in tests.
///
/// Analogous to Spring Boot's `@SpringBootTest` -- it wires up routes,
/// middleware, config, and optionally a database pool, then produces a
/// [`TestClient`] ready to fire requests.
///
/// # Examples
///
/// ```rust,no_run
/// use autumn_web::prelude::*;
/// use autumn_web::test::TestApp;
///
/// #[get("/ping")]
/// async fn ping() -> &'static str { "pong" }
///
/// #[tokio::test]
/// async fn ping_works() {
///     let client = TestApp::new()
///         .routes(routes![ping])
///         .build();
///
///     client.get("/ping").send().await.assert_ok();
/// }
/// ```
pub struct TestApp {
    routes: Vec<Route>,
    scoped_groups: Vec<crate::app::ScopedGroup>,
    merge_routers: Vec<axum::Router<crate::state::AppState>>,
    nest_routers: Vec<(String, axum::Router<crate::state::AppState>)>,
    custom_layers: Vec<crate::app::CustomLayerRegistration>,
    static_gate_layers: Vec<crate::app::CustomLayerRegistration>,
    config: AutumnConfig,
    #[cfg(feature = "openapi")]
    openapi: Option<crate::openapi::OpenApiConfig>,
    #[cfg(feature = "mcp")]
    mcp: Option<crate::mcp::McpRuntime>,
    #[cfg(feature = "db")]
    pool: Option<Pool<AsyncPgConnection>>,
    #[cfg(feature = "db")]
    replica_pool: Option<Pool<AsyncPgConnection>>,
    #[cfg(feature = "db")]
    transactional: bool,
    #[cfg(feature = "db")]
    transactional_url: Option<String>,
    /// Deferred policy / scope registrations applied during
    /// [`TestApp::build`].
    policy_registrations: Vec<TestPolicyRegistration>,
    /// Override for [`AppState::forbidden_response`]. Defaults to
    /// the value derived from
    /// [`SecurityConfig::forbidden_response`](crate::security::SecurityConfig::forbidden_response).
    forbidden_response_override: Option<crate::authorization::ForbiddenResponse>,
    #[cfg(feature = "mail")]
    mail_interceptor: Option<std::sync::Arc<dyn crate::interceptor::MailInterceptor>>,
    #[cfg(feature = "mail")]
    mail_recorder: MailRecorder,
    job_interceptor: Option<std::sync::Arc<dyn crate::interceptor::JobInterceptor>>,
    #[cfg(feature = "db")]
    db_interceptor: Option<std::sync::Arc<dyn crate::interceptor::DbConnectionInterceptor>>,
    #[cfg(feature = "ws")]
    channels_interceptor: Option<std::sync::Arc<dyn crate::interceptor::ChannelsInterceptor>>,
    #[cfg(feature = "oauth2")]
    http_interceptor: Option<std::sync::Arc<dyn crate::interceptor::HttpInterceptor>>,
    /// Shared mock registry installed into `AppState` during [`build`](Self::build)
    /// so that any [`Client`](crate::http_client::Client) extracted inside a
    /// handler intercepts matching requests.
    #[cfg(feature = "http-client")]
    http_mock_registry: Option<std::sync::Arc<crate::http_client::MockRegistry>>,
    state_initializers: Vec<Box<dyn FnOnce(&AppState) + Send>>,
    jobs: Vec<crate::job::JobInfo>,
    listeners: Vec<crate::events::ListenerInfo>,
    exception_filters: Vec<std::sync::Arc<dyn crate::middleware::ExceptionFilter>>,
    #[cfg(feature = "mail")]
    suppression_store: Option<crate::mail::SuppressionStoreHandle>,
    registered_plugins: std::collections::HashSet<String>,
    extensions: std::collections::HashMap<std::any::TypeId, Box<dyn std::any::Any + Send>>,
    /// Injected clock; `None` means use [`crate::time::SystemClock`].
    clock: Option<std::sync::Arc<dyn crate::time::ClockSource>>,
    /// Retained as `Arc<dyn Any>` so `TestClient::advance_clock` can downcast
    /// to [`crate::time::TickingClock`] at runtime.
    clock_as_any: Option<std::sync::Arc<dyn std::any::Any + Send + Sync>>,
    api_versions: Vec<crate::app::ApiVersion>,
    /// Plugin-contributed metrics sources registered via [`AppBuilder::metrics_source`].
    metrics_sources: Vec<(String, std::sync::Arc<dyn crate::actuator::MetricsSource>)>,
    /// Plugin-contributed health indicators registered via [`AppBuilder::health_indicator`].
    health_indicators: Vec<(
        String,
        crate::actuator::IndicatorGroup,
        std::sync::Arc<dyn crate::actuator::HealthIndicator>,
    )>,
    /// Inbound mail router registered via [`TestApp::inbound_mail_router`].
    #[cfg(feature = "inbound-mail")]
    inbound_mail_router: Option<std::sync::Arc<crate::inbound_mail::InboundMailRouter>>,
}

type TestPolicyRegistration = Box<dyn FnOnce(&crate::authorization::PolicyRegistry) + Send>;

impl TestApp {
    /// Create a new test app builder with default configuration.
    #[must_use]
    pub fn new() -> Self {
        let mut config = AutumnConfig::default();
        config.profile = Some("test".into());
        // Disable CSRF for tests by default (like Spring Security's test support)
        config.security.csrf.enabled = false;

        Self {
            routes: Vec::new(),
            scoped_groups: Vec::new(),
            merge_routers: Vec::new(),
            nest_routers: Vec::new(),
            custom_layers: Vec::new(),
            static_gate_layers: Vec::new(),
            config,
            #[cfg(feature = "openapi")]
            openapi: None,
            #[cfg(feature = "mcp")]
            mcp: None,
            #[cfg(feature = "db")]
            pool: None,
            #[cfg(feature = "db")]
            replica_pool: None,
            #[cfg(feature = "db")]
            transactional: false,
            #[cfg(feature = "db")]
            transactional_url: None,
            policy_registrations: Vec::new(),
            forbidden_response_override: None,
            #[cfg(feature = "mail")]
            mail_interceptor: None,
            #[cfg(feature = "mail")]
            mail_recorder: MailRecorder::new(),
            job_interceptor: None,
            #[cfg(feature = "db")]
            db_interceptor: None,
            #[cfg(feature = "ws")]
            channels_interceptor: None,
            #[cfg(feature = "oauth2")]
            http_interceptor: None,
            #[cfg(feature = "http-client")]
            http_mock_registry: None,
            state_initializers: Vec::new(),
            jobs: Vec::new(),
            listeners: Vec::new(),
            exception_filters: Vec::new(),
            #[cfg(feature = "mail")]
            suppression_store: None,
            registered_plugins: std::collections::HashSet::new(),
            extensions: std::collections::HashMap::new(),
            clock: None,
            clock_as_any: None,
            api_versions: Vec::new(),
            metrics_sources: Vec::new(),
            health_indicators: Vec::new(),
            #[cfg(feature = "inbound-mail")]
            inbound_mail_router: None,
        }
    }

    /// Register a [`Policy`](crate::authorization::Policy) for
    /// resource type `R`. Mirrors
    /// [`AppBuilder::policy`](crate::app::AppBuilder::policy).
    #[must_use]
    pub fn policy<R, P>(mut self, policy: P) -> Self
    where
        R: Send + Sync + 'static,
        P: crate::authorization::Policy<R>,
    {
        self.policy_registrations.push(Box::new(move |registry| {
            registry.register_policy::<R, _>(policy);
        }));
        self
    }

    /// Register a [`Scope`](crate::authorization::Scope) for resource
    /// type `R`. Mirrors
    /// [`AppBuilder::scope`](crate::app::AppBuilder::scope).
    #[must_use]
    pub fn scope<R, S>(mut self, scope: S) -> Self
    where
        R: Send + Sync + 'static,
        S: crate::authorization::Scope<R>,
    {
        self.policy_registrations.push(Box::new(move |registry| {
            registry.register_scope::<R, _>(scope);
        }));
        self
    }

    /// Register an inbound mail router for this test app.
    ///
    /// Mirrors [`crate::app::AppBuilder::inbound_mail_router`].
    #[cfg(feature = "inbound-mail")]
    #[must_use]
    pub fn inbound_mail_router(mut self, router: crate::inbound_mail::InboundMailRouter) -> Self {
        self.inbound_mail_router = Some(std::sync::Arc::new(router));
        self
    }

    /// Override the deny-response shape used by `#[authorize]` and
    /// `#[repository(policy = ...)]` handlers. Useful for
    /// round-tripping the `403`-vs-`404` decision in tests.
    #[must_use]
    pub const fn forbidden_response(
        mut self,
        value: crate::authorization::ForbiddenResponse,
    ) -> Self {
        self.forbidden_response_override = Some(value);
        self
    }

    /// Enable `OpenAPI` spec generation for the test app.
    ///
    /// Mirrors [`crate::app::AppBuilder::openapi`] so integration tests
    /// can exercise the `/v3/api-docs` and `/swagger-ui` endpoints.
    ///
    /// Gated behind the `openapi` Cargo feature.
    #[cfg(feature = "openapi")]
    #[must_use]
    pub fn openapi(mut self, config: crate::openapi::OpenApiConfig) -> Self {
        self.openapi = Some(config);
        self
    }

    /// Mount an MCP endpoint at `path`, mirroring
    /// [`AppBuilder::mount_mcp`](crate::app::AppBuilder::mount_mcp) so
    /// integration tests can drive `initialize`/`tools/list`/`tools/call`
    /// through the in-process pipeline.
    ///
    /// Gated behind the `mcp` Cargo feature.
    #[cfg(feature = "mcp")]
    #[must_use]
    pub fn mount_mcp(mut self, path: impl Into<String>) -> Self {
        let path = path.into();
        if let Some(rt) = self.mcp.as_mut() {
            rt.mount_path = path;
        } else {
            self.mcp = Some(crate::mcp::McpRuntime::new(path));
        }
        self
    }

    /// Enable the whole-API MCP hatch, mirroring
    /// [`AppBuilder::expose_all_as_mcp`](crate::app::AppBuilder::expose_all_as_mcp).
    ///
    /// Gated behind the `mcp` Cargo feature.
    #[cfg(feature = "mcp")]
    #[must_use]
    pub fn expose_all_as_mcp(mut self) -> Self {
        if let Some(rt) = self.mcp.as_mut() {
            rt.expose_all = true;
        } else {
            let mut rt = crate::mcp::McpRuntime::new("/mcp");
            rt.expose_all = true;
            self.mcp = Some(rt);
        }
        self
    }

    /// Gate the entire MCP endpoint behind a tower `layer`, mirroring
    /// [`AppBuilder::secure_mcp`](crate::app::AppBuilder::secure_mcp).
    ///
    /// Gated behind the `mcp` Cargo feature.
    #[cfg(feature = "mcp")]
    #[must_use]
    pub fn secure_mcp<L>(mut self, layer: L) -> Self
    where
        L: tower::Layer<axum::routing::Route> + Clone + Send + Sync + 'static,
        L::Service: tower::Service<
                axum::http::Request<axum::body::Body>,
                Response = axum::http::Response<axum::body::Body>,
                Error = std::convert::Infallible,
            > + Clone
            + Send
            + Sync
            + 'static,
        <L::Service as tower::Service<axum::http::Request<axum::body::Body>>>::Future:
            Send + 'static,
    {
        let applier: crate::mcp::McpEndpointLayer = Box::new(move |router| router.layer(layer));
        if let Some(rt) = self.mcp.as_mut() {
            rt.endpoint_layer = Some(applier);
        } else {
            let mut rt = crate::mcp::McpRuntime::new("/mcp");
            rt.endpoint_layer = Some(applier);
            self.mcp = Some(rt);
        }
        self
    }

    /// Merge a router into the internal application state.
    ///
    /// This is useful when testing modular route definitions without building
    /// the full application.
    #[must_use]
    pub fn merge(mut self, router: axum::Router<crate::state::AppState>) -> Self {
        self.merge_routers.push(router);
        self
    }

    /// Mount routes under a scoped prefix with a route-local layer.
    #[must_use]
    pub fn scoped<L>(mut self, prefix: &str, layer: L, routes: Vec<Route>) -> Self
    where
        L: tower::Layer<axum::routing::Route> + Clone + Send + Sync + 'static,
        L::Service: tower::Service<
                axum::http::Request<axum::body::Body>,
                Response = axum::http::Response<axum::body::Body>,
                Error = std::convert::Infallible,
            > + Clone
            + Send
            + Sync
            + 'static,
        <L::Service as tower::Service<axum::http::Request<axum::body::Body>>>::Future:
            Send + 'static,
    {
        self.scoped_groups.push(crate::app::ScopedGroup {
            prefix: prefix.to_owned(),
            routes,
            source: crate::route_listing::RouteSource::User,
            apply_layer: Box::new(move |router| router.layer(layer)),
        });
        self
    }

    /// Nest a router under a specific path prefix for testing.
    ///
    /// This is useful for testing sub-applications or API versions.
    #[must_use]
    pub fn nest(mut self, path: &str, router: axum::Router<crate::state::AppState>) -> Self {
        self.nest_routers.push((path.to_owned(), router));
        self
    }

    /// Apply a custom [`tower::Layer`] to the entire test application.
    ///
    /// Mirrors [`crate::app::AppBuilder::layer`] so tests can exercise the
    /// exact middleware wiring that `AppBuilder::run()` produces.
    #[must_use]
    pub fn layer<L: crate::app::IntoAppLayer>(mut self, layer: L) -> Self {
        self.custom_layers
            .push(crate::app::CustomLayerRegistration {
                type_id: std::any::TypeId::of::<L>(),
                type_name: std::any::type_name::<L>(),
                apply: Box::new(move |router| layer.apply_to(router)),
            });
        self
    }

    /// Register a pre-static gate layer for this test application.
    ///
    /// Mirrors [`crate::app::AppBuilder::static_gate`]: the layer runs
    /// outermost (outside session and before the static cache lookup) so tests
    /// can exercise auth-gating wiring that protects cached SSG/ISG pages.
    #[must_use]
    pub fn static_gate<L: crate::app::IntoAppLayer>(mut self, layer: L) -> Self {
        self.static_gate_layers
            .push(crate::app::CustomLayerRegistration {
                type_id: std::any::TypeId::of::<L>(),
                type_name: std::any::type_name::<L>(),
                apply: Box::new(move |router| layer.apply_to(router)),
            });
        self
    }

    /// Register an [`ErrorReporter`](crate::reporting::ErrorReporter) for this
    /// test app.
    ///
    /// Mirrors [`crate::app::AppBuilder::with_error_reporter`]. Call multiple
    /// times to chain reporters; each receives every panic + 5xx event.
    #[cfg(feature = "reporting")]
    #[must_use]
    pub fn with_error_reporter<R: crate::reporting::ErrorReporter>(mut self, reporter: R) -> Self {
        let reporter =
            std::sync::Arc::new(reporter) as std::sync::Arc<dyn crate::reporting::ErrorReporter>;
        self.state_initializers.push(Box::new(move |state| {
            let mut reporters = state
                .extension::<crate::reporting::RegisteredReporters>()
                .map(|registered| registered.0.clone())
                .unwrap_or_default();
            reporters.push(reporter.clone());
            state.insert_extension(crate::reporting::RegisteredReporters(reporters));
        }));
        self
    }

    /// Enable HTTP idempotency-key middleware for this test app.
    ///
    /// Mirrors [`crate::app::AppBuilder::idempotent`]: sets the
    /// `config.idempotency.enabled` flag so that the router wires up the layer
    /// with the same `MemoryIdempotencyStore` and `MetricsCollector` that
    /// production uses.
    #[must_use]
    pub const fn idempotent(mut self) -> Self {
        self.config.idempotency.enabled = Some(true);
        self
    }

    /// Construct a [`TestClient`] directly from an `axum::Router`.
    ///
    /// Useful for bypassing `TestApp` builder if you just want to write requests
    /// against a standard axum Router.  The probe state returned by
    /// [`TestClient::probes`] will be in the default ready state; it is not
    /// connected to any handler in the supplied router.
    ///
    /// **Note:** [`TestClient::sent_mail`] will always return an empty list for
    /// clients built this way.  The built-in mail recorder is wired in during
    /// [`TestApp::build`]; because `from_router` receives an already-constructed
    /// `AppState` (with the mailer already installed), the recorder cannot be
    /// injected into its interceptor chain.  Use [`TestApp::new().merge(router).build()`](TestApp::merge)
    /// to get recording support.
    #[must_use]
    pub fn from_router(router: axum::Router, state: AppState) -> TestClient {
        TestClient {
            router,
            probes: crate::probe::ProbeState::ready_for_test(),
            state,
            _job_runtime: None,
            clock_as_any: None,
            #[cfg(feature = "mail")]
            mail_recorder: None,
        }
    }

    /// Register a collection of routes to be built into the `TestApp`.
    #[must_use]
    pub fn routes(mut self, routes: Vec<Route>) -> Self {
        self.routes.extend(routes);
        self
    }

    /// Register a callback to configure/initialize the application state before building the router.
    #[must_use]
    pub fn state_initializer<F>(mut self, f: F) -> Self
    where
        F: FnOnce(&AppState) + Send + 'static,
    {
        self.state_initializers.push(Box::new(f));
        self
    }

    /// Register a [`FlagStore`](crate::feature_flags::FlagStore) backend so
    /// the [`Flags`](crate::feature_flags::Flags) extractor works in test handlers.
    ///
    /// Mirrors [`crate::app::AppBuilder::with_flag_store`].
    #[must_use]
    pub fn with_flag_store<S>(mut self, store: S) -> Self
    where
        S: crate::feature_flags::FlagStore,
    {
        use std::sync::Arc;
        let service = crate::feature_flags::FeatureFlagService::new(Arc::new(store) as Arc<_>);
        self.state_initializers.push(Box::new(move |state| {
            state.insert_extension(service);
        }));
        self
    }

    /// Apply a plugin directly to the test app.
    #[must_use]
    pub fn plugin<P: crate::plugin::Plugin>(mut self, plugin: P) -> Self {
        let name = plugin.name().into_owned();
        if self.registered_plugins.contains(&name) {
            tracing::warn!(plugin = %name, "Duplicate plugin registration in TestApp; skipping");
            return self;
        }

        let mut app_builder = crate::app();
        app_builder
            .registered_plugins
            .clone_from(&self.registered_plugins);
        app_builder.extensions = self.extensions;
        app_builder.state_initializers = std::mem::take(&mut self.state_initializers);

        app_builder = app_builder.plugin(plugin);

        self.registered_plugins = app_builder.registered_plugins;
        self.extensions = app_builder.extensions;
        self.state_initializers = app_builder.state_initializers;

        // Merge properties from the plugin's app_builder into self:
        self.routes.extend(app_builder.routes);
        self.scoped_groups.extend(app_builder.scoped_groups);
        self.merge_routers.extend(app_builder.merge_routers);
        self.nest_routers.extend(app_builder.nest_routers);
        self.custom_layers.extend(app_builder.custom_layers);
        self.static_gate_layers
            .extend(app_builder.static_gate_layers);
        self.jobs.extend(app_builder.jobs);
        self.listeners.extend(app_builder.listeners);
        self.exception_filters.extend(app_builder.exception_filters);
        self.metrics_sources.extend(app_builder.metrics_sources);
        self.health_indicators.extend(app_builder.health_indicators);
        // Carry plugin-registered inbound mail router into the test app so
        // webhook plugins behave identically under TestApp.
        #[cfg(feature = "inbound-mail")]
        if let Some(router) = app_builder.inbound_mail_router {
            self.inbound_mail_router = Some(router);
        }

        // Carry a plugin-registered suppression store (List-Unsubscribe storage)
        // into the test app so unsubscribe POSTs and send-time suppression behave
        // under TestApp exactly as they do under AppBuilder::run.
        #[cfg(feature = "mail")]
        if let Some(handle) = app_builder.suppression_store {
            self.suppression_store = Some(handle);
        }

        // Carry a plugin's `mount_unsubscribe_endpoint()` opt-in: production copies
        // this builder flag into config.mail before router assembly, so a plugin
        // that mounts the default unsubscribe endpoint must mount it under TestApp
        // too (otherwise /_autumn/unsubscribe 404s in tests but works in prod).
        #[cfg(feature = "mail")]
        if app_builder.mount_unsubscribe_endpoint {
            self.config.mail.mount_unsubscribe_endpoint = true;
        }

        // Carry plugin-registered error reporters into the test app so
        // reporting-enabled plugins exercise the same behavior under `TestApp`
        // that they get from `AppBuilder::run`.
        #[cfg(feature = "reporting")]
        {
            let reporters = std::mem::take(&mut app_builder.error_reporters);
            if !reporters.is_empty() {
                self.state_initializers.push(Box::new(move |state| {
                    let mut existing = state
                        .extension::<crate::reporting::RegisteredReporters>()
                        .map(|registered| registered.0.clone())
                        .unwrap_or_default();
                    existing.extend(reporters.iter().cloned());
                    state.insert_extension(crate::reporting::RegisteredReporters(existing));
                }));
            }
        }

        for hook in app_builder.startup_hooks {
            self.state_initializers.push(Box::new(move |state| {
                let state_owned = state.clone();
                if let Ok(handle) = tokio::runtime::Handle::try_current() {
                    let thread_handle =
                        std::thread::spawn(move || handle.block_on(hook(state_owned)));
                    thread_handle
                        .join()
                        .expect("Plugin startup hook thread panicked")
                        .expect("Plugin startup hook failed");
                } else {
                    let thread_handle = std::thread::spawn(move || {
                        let rt = tokio::runtime::Builder::new_multi_thread()
                            .enable_all()
                            .build()
                            .expect("failed to build tokio runtime for test plugin startup hook");
                        rt.block_on(hook(state_owned))
                    });
                    thread_handle
                        .join()
                        .expect("Plugin startup hook thread panicked")
                        .expect("Plugin startup hook failed");
                }
            }));
        }
        self
    }

    #[cfg(feature = "mail")]
    #[must_use]
    pub fn with_mail_interceptor(
        mut self,
        interceptor: impl crate::interceptor::MailInterceptor,
    ) -> Self {
        self.mail_interceptor = Some(std::sync::Arc::new(interceptor));
        self
    }

    /// Register a [`SuppressionStore`](crate::mail::SuppressionStore) so
    /// List-Unsubscribe sends skip suppressed recipients and the unsubscribe
    /// endpoint records opt-outs. Mirrors
    /// [`AppBuilder::with_suppression_store`](crate::app::AppBuilder::with_suppression_store).
    #[cfg(feature = "mail")]
    #[must_use]
    pub fn with_suppression_store(
        mut self,
        store: impl crate::mail::SuppressionStore + 'static,
    ) -> Self {
        self.suppression_store = Some(crate::mail::SuppressionStoreHandle::new(store));
        self
    }

    /// Mount the framework's default one-click unsubscribe endpoint (opt-in).
    /// Mirrors
    /// [`AppBuilder::mount_unsubscribe_endpoint`](crate::app::AppBuilder::mount_unsubscribe_endpoint).
    #[cfg(feature = "mail")]
    #[must_use]
    pub const fn mount_unsubscribe_endpoint(mut self) -> Self {
        self.config.mail.mount_unsubscribe_endpoint = true;
        self
    }

    #[must_use]
    pub fn with_job_interceptor(
        mut self,
        interceptor: impl crate::interceptor::JobInterceptor,
    ) -> Self {
        self.job_interceptor = Some(std::sync::Arc::new(interceptor));
        self
    }

    /// Register event listeners with the test app.
    ///
    /// Collect them with `listeners![..]`, exactly as in `AppBuilder::listeners`.
    /// Durable listeners run under the in-process test job runtime; sync
    /// listeners run in-request. Published events are always recorded, so
    /// [`TestClient::assert_event_published`] works without standing up jobs.
    #[must_use]
    pub fn listeners(mut self, listeners: Vec<crate::events::ListenerInfo>) -> Self {
        self.listeners.extend(listeners);
        self
    }

    #[cfg(feature = "db")]
    #[must_use]
    pub fn with_db_interceptor(
        mut self,
        interceptor: impl crate::interceptor::DbConnectionInterceptor,
    ) -> Self {
        self.db_interceptor = Some(std::sync::Arc::new(interceptor));
        self
    }

    #[cfg(feature = "ws")]
    #[must_use]
    pub fn with_channels_interceptor(
        mut self,
        interceptor: impl crate::interceptor::ChannelsInterceptor,
    ) -> Self {
        self.channels_interceptor = Some(std::sync::Arc::new(interceptor));
        self
    }

    #[cfg(feature = "oauth2")]
    #[must_use]
    pub fn with_http_interceptor(
        mut self,
        interceptor: impl crate::interceptor::HttpInterceptor,
    ) -> Self {
        self.http_interceptor = Some(std::sync::Arc::new(interceptor));
        self
    }

    /// Override the default test configuration.
    #[must_use]
    pub fn config(mut self, config: AutumnConfig) -> Self {
        self.config = config;
        self
    }

    /// Set the active profile (default is `"test"`).
    #[must_use]
    pub fn profile(mut self, profile: &str) -> Self {
        self.config.profile = Some(profile.to_owned());
        self
    }

    /// Inject a custom clock into the test app.
    ///
    /// All handlers that take a [`crate::time::Clock`] extractor will see time
    /// as reported by `clock`. Use [`crate::time::FixedClock`] to pin time to
    /// a known instant, or [`crate::time::TickingClock`] when you need to step
    /// the clock forward between requests via
    /// [`TestClient::advance_clock`].
    ///
    /// ```rust,no_run
    /// use autumn_web::test::TestApp;
    /// use autumn_web::time::{FixedClock, TickingClock};
    /// use chrono::{TimeZone, Utc};
    ///
    /// // Pin to a fixed instant:
    /// let _client = TestApp::new()
    ///     .with_clock(FixedClock::at(Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap()))
    ///     .build();
    ///
    /// // Step forward in time:
    /// let clock = TickingClock::starting_at(Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap());
    /// let client = TestApp::new()
    ///     .with_clock(clock.clone())
    ///     .build();
    /// client.advance_clock(std::time::Duration::from_secs(3600));
    /// ```
    #[must_use]
    pub fn with_clock<C>(mut self, clock: C) -> Self
    where
        C: crate::time::ClockSource + 'static,
    {
        let arc: std::sync::Arc<C> = std::sync::Arc::new(clock);
        // Retain as dyn Any so TestClient::advance_clock can downcast to TickingClock.
        self.clock_as_any = Some(arc.clone() as std::sync::Arc<dyn std::any::Any + Send + Sync>);
        self.clock = Some(arc as std::sync::Arc<dyn crate::time::ClockSource>);
        self
    }

    /// Register a single API version for testing.
    #[must_use]
    pub fn api_version(mut self, version: crate::app::ApiVersion) -> Self {
        self.api_versions.push(version);
        self
    }

    /// Register multiple API versions for testing.
    #[must_use]
    pub fn api_versions(
        mut self,
        versions: impl IntoIterator<Item = crate::app::ApiVersion>,
    ) -> Self {
        self.api_versions.extend(versions);
        self
    }

    /// Attach a database connection pool to the test app.
    #[cfg(feature = "db")]
    #[must_use]
    pub fn with_db(mut self, pool: Pool<AsyncPgConnection>) -> Self {
        self.pool = Some(pool);
        self
    }

    /// Enable transactional test isolation using the database URL configured
    /// in the application's configuration.
    #[cfg(feature = "db")]
    #[must_use]
    pub const fn transactional(mut self) -> Self {
        self.transactional = true;
        self
    }

    /// Enable transactional test isolation with an explicit database URL.
    #[cfg(feature = "db")]
    #[must_use]
    pub fn with_transactional_db(mut self, url: impl Into<String>) -> Self {
        self.transactional = true;
        self.transactional_url = Some(url.into());
        self
    }

    /// Configure the application's horizontal shards programmatically, as if
    /// they were declared via `[[database.shards]]` in `autumn.toml`.
    ///
    /// This is the escape hatch for tests that spin up shard databases at
    /// runtime (e.g. one Postgres container per shard) and need to point the
    /// app at them without writing a config file. Combine with
    /// [`transactional`](Self::transactional) to get rolled-back shard writes.
    ///
    /// ```rust,no_run
    /// use autumn_web::test::TestApp;
    /// use autumn_web::config::ShardConfig;
    ///
    /// # fn example(shard0: String, shard1: String) {
    /// let client = TestApp::new()
    ///     .with_transactional_db("postgres://localhost/control")
    ///     .with_shards(vec![
    ///         ShardConfig { name: "shard0".into(), primary_url: shard0, ..Default::default() },
    ///         ShardConfig { name: "shard1".into(), primary_url: shard1, ..Default::default() },
    ///     ])
    ///     .build();
    /// # let _ = client;
    /// # }
    /// ```
    #[cfg(feature = "db")]
    #[must_use]
    pub fn with_shards(mut self, shards: Vec<crate::config::ShardConfig>) -> Self {
        self.config.database.shards = shards;
        self
    }

    /// Register a canned HTTP response for outbound requests made via the
    /// [`Client`](crate::http_client::Client) extractor during this test.
    ///
    /// `alias` identifies the named service (must match the alias passed to
    /// [`Client::named`](crate::http_client::Client::named) in the handler, or
    /// the key used in `[http.client.base_urls]`).
    ///
    /// Returns a [`MockSetupBuilder`](crate::http_client::MockSetupBuilder) on
    /// which you chain the HTTP method and path before calling
    /// [`respond_with`](crate::http_client::MockSetupBuilder::respond_with) to
    /// register the entry and get a
    /// [`MockHandle`](crate::http_client::MockHandle) for later assertions.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// use autumn_web::test::TestApp;
    /// use serde_json::json;
    ///
    /// # async fn example() {
    /// let mut app = TestApp::new();
    /// let mock = app
    ///     .http_mock("stripe")
    ///     .post("/v1/charges")
    ///     .respond_with(200, json!({"id": "ch_123", "amount": 1000}));
    ///
    /// let client = app.build();
    /// // … fire requests …
    /// mock.expect_called(1);
    /// # }
    /// ```
    #[cfg(feature = "http-client")]
    pub fn http_mock(&mut self, alias: &str) -> crate::http_client::MockSetupBuilder {
        let registry = self
            .http_mock_registry
            .get_or_insert_with(|| std::sync::Arc::new(crate::http_client::MockRegistry::new()))
            .clone();

        crate::http_client::MockSetupBuilder {
            registry,
            alias: alias.to_owned(),
            method: None,
            path: None,
        }
    }

    /// Build the application and return a [`TestClient`] ready for requests.
    ///
    /// This constructs the full Axum router with all middleware applied,
    /// identical to what `AppBuilder::run()` produces -- without binding
    /// a TCP listener.
    ///
    /// The process-level global cache is cleared unconditionally so that
    /// `#[cached]` functions inside this test app always use their
    /// per-function Moka stores and do not accidentally inherit a Redis or
    /// other shared backend installed by a previous test.
    #[must_use]
    #[cfg_attr(not(feature = "inbound-mail"), allow(unused_mut))]
    pub fn build(mut self) -> TestClient {
        // Reset the global cache to prevent cross-test contamination.
        crate::cache::clear_global_cache();
        // Reset the global event bus so a prior test's listeners/recorder do not
        // leak into this one (it is re-installed below).
        crate::events::clear_global_event_bus();

        #[cfg(feature = "db")]
        let (pool, replica_pool, db_interceptor) = if self.transactional {
            let url = self.transactional_url.as_deref()
                .or_else(|| self.config.database.effective_primary_url())
                .expect("Transactional isolation enabled but database URL is not configured. Use `with_transactional_db(url)` or configure database.primary_url/database.url");

            let connect_timeout_secs = self.config.database.connect_timeout_secs;
            let timeout = std::time::Duration::from_secs(connect_timeout_secs);

            let manager = diesel_async::pooled_connection::AsyncDieselConnectionManager::<
                diesel_async::AsyncPgConnection,
            >::new(url);
            let pool = Pool::builder(manager)
                .max_size(1)
                .wait_timeout(Some(timeout))
                .create_timeout(Some(timeout))
                .runtime(deadpool::Runtime::Tokio1)
                .post_create(deadpool::managed::Hook::async_fn(
                    |conn: &mut diesel_async::AsyncPgConnection, _metrics| {
                        Box::pin(async move {
                            use diesel_async::AsyncConnection;
                            use diesel_async::RunQueryDsl;

                            conn.begin_test_transaction().await.map_err(|e| {
                                deadpool::managed::HookError::Backend(
                                    diesel_async::pooled_connection::PoolError::QueryError(e),
                                )
                            })?;

                            diesel::sql_query("SET autumn.test_transaction_started = 'true'")
                                .execute(conn)
                                .await
                                .map_err(|e| {
                                    deadpool::managed::HookError::Backend(
                                        diesel_async::pooled_connection::PoolError::QueryError(e),
                                    )
                                })?;

                            Ok(())
                        })
                    },
                ))
                .build()
                .expect("failed to build transactional pool of size 1");

            let trans_interceptor = std::sync::Arc::new(TransactionalDbInterceptor);
            let interceptor = if let Some(user_interceptor) = self.db_interceptor {
                std::sync::Arc::new(ComposedDbInterceptor {
                    first: user_interceptor,
                    second: trans_interceptor,
                })
                    as std::sync::Arc<dyn crate::interceptor::DbConnectionInterceptor>
            } else {
                trans_interceptor as std::sync::Arc<dyn crate::interceptor::DbConnectionInterceptor>
            };

            (Some(pool), None, Some(interceptor))
        } else {
            (self.pool, self.replica_pool, self.db_interceptor)
        };

        // Mirror production router selection (see `setup_database`): when the
        // test config enables directory routing, build a `DirectoryShardRouter`
        // over the control pool so tests that pin tenants in
        // `_autumn_shard_directory` route the same way production would.
        #[cfg(feature = "db")]
        let shard_router: std::sync::Arc<dyn crate::sharding::ShardRouter> =
            match (self.config.database.directory_shard_router, &pool) {
                (true, Some(control_pool)) => {
                    let timeout_ms = self.config.database.statement_timeout.map_or(0, |d| {
                        u64::try_from(d.as_millis())
                            .unwrap_or(i32::MAX as u64)
                            .min(i32::MAX as u64)
                    });
                    std::sync::Arc::new(
                        crate::sharding::DirectoryShardRouter::new(control_pool.clone())
                            .with_statement_timeout_ms(timeout_ms),
                    )
                }
                // Production `setup_database` errors here (the directory router
                // needs a control DB), so fail the test app the same way rather
                // than silently routing by hash and passing a test the deployed
                // app would fail.
                (true, None) => panic!(
                    "directory_shard_router is enabled but TestApp has no control database pool; \
                     configure a control pool (with_db) or disable directory routing"
                ),
                (false, _) => std::sync::Arc::new(crate::sharding::HashShardRouter),
            };

        let probes = crate::probe::ProbeState::ready_for_test();
        #[cfg(feature = "ws")]
        let test_channels = crate::channels::Channels::new(32);
        #[cfg_attr(not(feature = "ws"), allow(unused_mut))]
        let mut state = AppState {
            extensions: std::sync::Arc::new(std::sync::RwLock::new(
                std::collections::HashMap::new(),
            )),
            #[cfg(feature = "db")]
            pool,
            #[cfg(feature = "db")]
            replica_pool,
            // Build the shard set from the test config so handlers using
            // the sharding extractors behave as they would in production.
            // Pools are lazy, so this needs no running databases.
            //
            // Under transactional isolation each shard primary pool is built
            // with `max_size(1)` and a `begin_test_transaction` hook (mirroring
            // the control pool above) so writes routed to a shard are rolled
            // back at the end of the test — the same isolation the control pool
            // gets. Replicas are skipped; all shard reads run on the primary.
            #[cfg(feature = "db")]
            shards: if self.transactional {
                crate::sharding::create_shard_set_transactional(
                    &self.config.database,
                    shard_router.clone(),
                )
                .expect("transactional test shard pools should build from config")
            } else {
                crate::sharding::create_shard_set(&self.config.database, shard_router.clone())
                    .expect("test shard pools should build from config")
            },
            profile: self.config.profile.clone(),
            started_at: std::time::Instant::now(),
            health_detailed: self.config.health.detailed,
            probes: probes.clone(),
            metrics: crate::middleware::MetricsCollector::new(),
            log_levels: crate::actuator::LogLevels::new(&self.config.log.level),
            task_registry: crate::actuator::TaskRegistry::new(),
            job_registry: crate::actuator::JobRegistry::new(),
            config_props: crate::actuator::ConfigProperties::default(),
            metrics_source_registry: crate::actuator::MetricsSourceRegistry::new(),
            health_indicator_registry: crate::actuator::HealthIndicatorRegistry::new(),
            #[cfg(feature = "presence")]
            presence: crate::presence::Presence::new(test_channels.clone()),
            #[cfg(feature = "ws")]
            channels: test_channels,

            #[cfg(feature = "ws")]
            shutdown: tokio_util::sync::CancellationToken::new(),
            policy_registry: crate::authorization::PolicyRegistry::default(),
            forbidden_response: self
                .forbidden_response_override
                .unwrap_or(self.config.security.forbidden_response),
            auth_session_key: self.config.auth.session_key.clone(),
            shared_cache: None,
            clock: self
                .clock
                .unwrap_or_else(|| std::sync::Arc::new(crate::time::SystemClock)),
        };

        for register in self.policy_registrations {
            register(state.policy_registry());
        }
        state.insert_extension(crate::app::RegisteredApiVersions(self.api_versions));
        crate::app::install_webhook_registry(&state, &self.config);

        // Install AutumnConfig so DbState::statement_timeout / slow_query_threshold
        // and HTTP Client resilience can read the test-supplied config.
        state.insert_extension(self.config.clone());

        #[cfg(feature = "mail")]
        let mail_recorder_for_client = {
            let recorder_for_client = self.mail_recorder.clone();
            let recorder = std::sync::Arc::new(self.mail_recorder);
            let effective: std::sync::Arc<dyn crate::interceptor::MailInterceptor> =
                if let Some(user) = self.mail_interceptor {
                    std::sync::Arc::new(ChainedMailInterceptor {
                        first: recorder,
                        second: user,
                    })
                } else {
                    recorder
                };
            state.insert_extension(effective);
            recorder_for_client
        };
        if let Some(interceptor) = self.job_interceptor {
            state.insert_extension(interceptor);
        }
        #[cfg(feature = "db")]
        if let Some(interceptor) = db_interceptor {
            state.insert_extension(interceptor);
        }
        #[cfg(feature = "ws")]
        if let Some(interceptor) = self.channels_interceptor {
            state.insert_extension(interceptor.clone());
            state.channels = crate::channels::Channels::with_shared_backend(std::sync::Arc::new(
                crate::channels::InterceptedChannelsBackend::new(
                    state.channels.backend().clone(),
                    vec![interceptor],
                ),
            ));
            #[cfg(feature = "presence")]
            {
                state.presence = crate::presence::Presence::new(state.channels.clone());
            }
        }
        #[cfg(feature = "oauth2")]
        if let Some(interceptor) = self.http_interceptor {
            state.insert_extension(interceptor);
        }

        #[cfg(feature = "mail")]
        {
            if let Some(handle) = self.suppression_store.clone() {
                state.insert_extension(handle);
            }
            crate::mail::install_mailer(&state, &self.config.mail, false)
                .expect("Failed to configure test mailer");
        }

        // Install HTTP client config so the Client extractor can read it.
        #[cfg(feature = "http-client")]
        state.insert_extension(self.config.http.clone());

        // Register the shared reqwest::Client so Client::from_state reuses the
        // connection pool in tests, mirroring the production build_state path.
        #[cfg(feature = "http-client")]
        state.insert_extension(crate::http_client::SharedReqwestClient {
            client: crate::http_client::Client::build_inner(&self.config.http.client),
            timeout_secs: self.config.http.client.timeout_secs,
        });

        // Install mock registry when http_mock() was called.
        #[cfg(feature = "http-client")]
        if let Some(registry) = self.http_mock_registry {
            state.insert_extension(crate::http_client::HttpMockRegistryExt(registry));
        }

        // Register metrics sources before state initializers — mirrors production
        // AppBuilder::run ordering so initializers can observe the registry.
        for (name, source) in self.metrics_sources {
            if let Err(e) = state.metrics_source_registry.register(name, source) {
                tracing::warn!("{e}");
            }
        }
        for (name, group, indicator) in self.health_indicators {
            if let Err(e) = state
                .health_indicator_registry
                .register(name, group, indicator)
            {
                tracing::warn!("{e}");
            }
        }

        // Mirror production `AppBuilder` wiring: surface each configured shard's
        // replica readiness as a `db:shard:<name>` indicator so `/ready`
        // refreshes shard replica health (gating `fail_readiness` shards and
        // marking healthy replicas ready for `ShardedDb` read routing).
        #[cfg(feature = "db")]
        if let Some(set) = state.shards() {
            crate::sharding::register_shard_health_indicators(
                set,
                &state.health_indicator_registry,
            );
        }

        for initializer in self.state_initializers {
            initializer(&state);
        }

        // Wire the event bus: always install a recorder so tests can assert on
        // published events without a job runner, register the listener registry
        // for the `Events` extractor, and fold durable listeners into the jobs
        // started below so they dispatch through the in-process test runtime.
        state.insert_extension(crate::events::EventRecorder::default());
        let event_recorder = state
            .extension::<crate::events::EventRecorder>()
            .expect("event recorder just installed");
        let event_registry =
            crate::events::EventRegistry::from_listeners(std::mem::take(&mut self.listeners));
        self.jobs.extend(event_registry.durable_job_infos());
        state.insert_extension(event_registry.clone());
        crate::events::init_global_event_bus(&event_registry, &state, Some(event_recorder));

        for job in &self.jobs {
            state.job_registry.register(&job.name);
        }

        let job_runtime = if self.jobs.is_empty() {
            None
        } else {
            let shutdown = tokio_util::sync::CancellationToken::new();
            crate::job::start_runtime(self.jobs.clone(), &state, &shutdown, &self.config.jobs)
                .expect("Failed to start job runtime in test");
            Some(TestJobRuntime { shutdown })
        };

        #[cfg_attr(not(feature = "inbound-mail"), allow(unused_mut))]
        let mut merge_routers = self.merge_routers;
        #[cfg(feature = "inbound-mail")]
        if let Some(ref im_router) = self.inbound_mail_router {
            let mut registered_inbound: std::collections::HashSet<String> =
                std::collections::HashSet::new();
            for (path, axum_router) in crate::inbound_mail::build_routes(im_router) {
                if self
                    .routes
                    .iter()
                    .any(|r| r.method == Method::POST && r.path == path)
                    || self.scoped_groups.iter().any(|g| {
                        g.routes.iter().any(|r| {
                            r.method == Method::POST
                                && crate::router::join_nested_path(&g.prefix, r.path)
                                    == path.as_str()
                        })
                    })
                    || self.nest_routers.iter().any(|(nest_path, _)| {
                        let p = nest_path.as_str();
                        path.as_str() == p
                            || path.starts_with(p)
                                && (p.ends_with('/') || path.as_bytes().get(p.len()) == Some(&b'/'))
                    })
                {
                    tracing::warn!(
                        path = %path,
                        "inbound_mail: skipping webhook route — a POST handler is \
                         already registered at this path by the application"
                    );
                    continue;
                }
                if !registered_inbound.insert(path.clone()) {
                    tracing::warn!(
                        path = %path,
                        "inbound_mail: skipping duplicate inbound webhook path"
                    );
                    continue;
                }
                self.config.security.csrf.exempt_paths.push(path.clone());
                self.config.security.captcha_exempt_paths.push(path);
                merge_routers.push(axum_router);
            }
        }

        let router = crate::router::try_build_router_inner(
            self.routes,
            &self.config,
            state.clone(),
            crate::router::RouterContext {
                exception_filters: self.exception_filters,
                scoped_groups: self.scoped_groups,
                merge_routers,
                nest_routers: self.nest_routers,
                custom_layers: self.custom_layers,
                static_gate_layers: self.static_gate_layers,
                #[cfg(feature = "maud")]
                error_page_renderer: None,
                session_store: None,
                #[cfg(feature = "openapi")]
                openapi: self.openapi,
                #[cfg(feature = "mcp")]
                mcp: self.mcp,
            },
        )
        .expect("failed to build test router");
        // Mirror production's outermost access-log fallback (#999): in
        // production it is applied in `apply_startup_barrier`, outside the
        // session and exception-filter layers, and emits only for responses
        // the primary in-stack layer never saw (e.g. session-store outage
        // 503s), so tests observe the same access-log behavior an operator
        // would.
        let router = if self.config.log.access_log {
            router.layer(crate::middleware::AccessLogLayer::fallback(
                self.config.log.access_log_exclude.clone(),
            ))
        } else {
            router
        };
        // Mirror production's outermost Server-Timing fallback (#1348): in
        // production it is applied in `apply_startup_barrier`, outside the
        // primary `ServerTimingLayer` and the late `/mcp` merge, and appends a
        // `total` only for responses the primary never saw — short-circuits and
        // the late-merged `/mcp` envelope. Without mirroring it here a
        // `tools/call` would carry no outer `total` in tests, unlike production,
        // so tests would not observe the real `/mcp` timing an operator sees.
        // Applied outer to the access-log fallback, matching production order.
        let router = if crate::config::server_timing_enabled(&self.config) {
            router.layer(crate::middleware::ServerTimingLayer::fallback(true))
        } else {
            router
        };
        TestClient {
            router,
            probes,
            state,
            _job_runtime: job_runtime,
            clock_as_any: self.clock_as_any,
            #[cfg(feature = "mail")]
            mail_recorder: Some(mail_recorder_for_client),
        }
    }
}

impl Default for TestApp {
    fn default() -> Self {
        Self::new()
    }
}

// ── TestClient ─────────────────────────────────────────────────

/// Fluent HTTP client for integration tests.
///
/// Analogous to Spring Boot's `MockMvc` or Django's `Client`.
/// Fires requests through the full Axum middleware pipeline using
/// `tower::ServiceExt::oneshot()` -- no TCP listener required.
///
/// Created by [`TestApp::build()`].
///
/// # Examples
///
/// ```rust,ignore
/// let client = TestApp::new().routes(routes![handler]).build();
///
/// // GET request
/// client.get("/path").send().await.assert_ok();
///
/// // POST with JSON body
/// client.post("/items")
///     .json(&serde_json::json!({"name": "foo"}))
///     .send().await
///     .assert_status(201);
///
/// // PUT with header
/// client.put("/items/1")
///     .header("authorization", "Bearer token")
///     .json(&serde_json::json!({"name": "bar"}))
///     .send().await
///     .assert_ok();
/// ```
pub struct TestClient {
    router: axum::Router,
    probes: crate::probe::ProbeState,
    pub(crate) state: AppState,
    _job_runtime: Option<TestJobRuntime>,
    /// Retained so `advance_clock` can downcast to [`crate::time::TickingClock`].
    clock_as_any: Option<std::sync::Arc<dyn std::any::Any + Send + Sync>>,
    /// `None` when built via [`TestApp::from_router`], which bypasses recorder
    /// wiring. `Some` for all clients produced by [`TestApp::build`].
    #[cfg(feature = "mail")]
    mail_recorder: Option<MailRecorder>,
}

struct TestJobRuntime {
    shutdown: tokio_util::sync::CancellationToken,
}

impl Drop for TestJobRuntime {
    fn drop(&mut self) {
        self.shutdown.cancel();
        crate::job::clear_global_job_client();
    }
}

impl TestClient {
    /// Returns a reference to the [`AppState`] wired into this test app's router.
    #[must_use]
    pub const fn state(&self) -> &AppState {
        &self.state
    }

    /// Every recorded publication of event type `E`, deserialized.
    ///
    /// Events are recorded synchronously at publish time, so this works whether
    /// or not the listeners (sync or durable) have run.
    #[must_use]
    pub fn published_events<E: crate::events::Event>(&self) -> Vec<E> {
        self.state
            .extension::<crate::events::EventRecorder>()
            .map(|recorder| recorder.published::<E>())
            .unwrap_or_default()
    }

    /// Assert that at least one event of type `E` was published during the test.
    ///
    /// # Panics
    ///
    /// Panics if no event of type `E` was recorded.
    pub fn assert_event_published<E: crate::events::Event>(&self) {
        let count = self
            .state
            .extension::<crate::events::EventRecorder>()
            .map_or(0, |recorder| recorder.count::<E>());
        assert!(
            count > 0,
            "expected event `{}` to have been published, but none were recorded",
            E::NAME,
        );
    }

    /// Step the test clock forward by `duration`.
    ///
    /// Only effective when the app was configured with a
    /// [`crate::time::TickingClock`] via [`TestApp::with_clock`]. Calling this
    /// with a [`crate::time::FixedClock`] or without any custom clock is a
    /// safe no-op — time stays where it is.
    ///
    /// This method only affects the wall-clock time reported by the
    /// [`crate::time::Clock`] extractor. Tokio's runtime timer (used by
    /// `tokio::time::sleep`, `tokio::time::Instant`, etc.) is not affected.
    ///
    /// ```rust,no_run
    /// use autumn_web::test::TestApp;
    /// use autumn_web::time::TickingClock;
    /// use chrono::{TimeZone, Utc};
    /// use std::time::Duration;
    ///
    /// # #[tokio::main]
    /// # async fn main() {
    /// let clock = TickingClock::starting_at(Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap());
    /// let client = TestApp::new().with_clock(clock).build();
    ///
    /// client.advance_clock(Duration::from_secs(86400)); // advance 1 day
    /// # }
    /// ```
    pub fn advance_clock(&self, duration: std::time::Duration) {
        if let Some(any) = &self.clock_as_any {
            let cloned = std::sync::Arc::clone(any);
            if let Ok(ticking) = cloned.downcast::<crate::time::TickingClock>() {
                ticking.advance(duration);
            }
            // FixedClock or other types: advance_clock is a no-op.
        }
        // No clock installed: also a no-op.
    }

    /// Unwrap the underlying [`axum::Router`] out of the [`TestClient`].
    pub fn into_router(self) -> axum::Router {
        self.router
    }

    /// Return the [`crate::probe::ProbeState`] wired into this test app's router.
    ///
    /// Use this to drive readiness/liveness transitions in integration tests
    /// and verify the HTTP probe endpoints reflect state changes.
    pub const fn probes(&self) -> &crate::probe::ProbeState {
        &self.probes
    }

    /// Returns all emails sent during this test, in the order they were sent.
    ///
    /// The built-in recorder is installed automatically — no
    /// `.with_mail_interceptor(…)` call is required.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// client.post("/signup").json(&body).send().await.assert_ok();
    /// let mail = &client.sent_mail()[0];
    /// assert_eq!(mail.subject, "Welcome!");
    /// ```
    #[cfg(feature = "mail")]
    #[must_use]
    pub fn sent_mail(&self) -> Vec<SentMail> {
        self.mail_recorder
            .as_ref()
            .expect("sent_mail() is not available on a TestClient built via from_router(); use TestApp::new().merge(router).build() instead")
            .get_sent()
    }

    /// Asserts that exactly `n` emails were sent, panicking with a list of
    /// what was actually sent on failure.
    ///
    /// Returns `&self` for chaining.
    ///
    /// # Panics
    ///
    /// Panics when the count does not match.
    #[cfg(feature = "mail")]
    pub fn assert_email_count(&self, n: usize) -> &Self {
        let sent = self.sent_mail();
        assert_eq!(
            sent.len(),
            n,
            "expected {n} email(s) to have been sent, got {};\nactually sent: {sent:#?}",
            sent.len(),
        );
        self
    }

    /// Asserts that no emails were sent.
    ///
    /// Returns `&self` for chaining.
    ///
    /// # Panics
    ///
    /// Panics when any emails were sent.
    #[cfg(feature = "mail")]
    pub fn assert_no_email_sent(&self) -> &Self {
        self.assert_email_count(0)
    }

    /// Asserts that at least one sent email satisfies `predicate`, panicking
    /// with a list of what was actually sent on failure.
    ///
    /// Returns `&self` for chaining.
    ///
    /// # Panics
    ///
    /// Panics when no sent email matches.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// client
    ///     .assert_email_sent(|m| m.to.iter().any(|a| a == "alice@example.com"))
    ///     .assert_email_sent(|m| m.subject == "Welcome!");
    /// ```
    #[cfg(feature = "mail")]
    pub fn assert_email_sent(&self, predicate: impl Fn(&SentMail) -> bool) -> &Self {
        let sent = self.sent_mail();
        assert!(
            sent.iter().any(predicate),
            "no sent email matched the predicate;\nactually sent: {sent:#?}",
        );
        self
    }

    /// Start building a GET request.
    #[must_use]
    pub fn get(&self, uri: &str) -> RequestBuilder {
        RequestBuilder::new(self.router.clone(), Method::GET, uri)
    }

    /// Start building a POST request.
    #[must_use]
    pub fn post(&self, uri: &str) -> RequestBuilder {
        RequestBuilder::new(self.router.clone(), Method::POST, uri)
    }

    /// Start building a PUT request.
    #[must_use]
    pub fn put(&self, uri: &str) -> RequestBuilder {
        RequestBuilder::new(self.router.clone(), Method::PUT, uri)
    }

    /// Start building a DELETE request.
    #[must_use]
    pub fn delete(&self, uri: &str) -> RequestBuilder {
        RequestBuilder::new(self.router.clone(), Method::DELETE, uri)
    }

    /// Start building a PATCH request.
    #[must_use]
    pub fn patch(&self, uri: &str) -> RequestBuilder {
        RequestBuilder::new(self.router.clone(), Method::PATCH, uri)
    }

    /// Start building an OPTIONS request (e.g. a CORS preflight).
    #[must_use]
    pub fn options(&self, uri: &str) -> RequestBuilder {
        RequestBuilder::new(self.router.clone(), Method::OPTIONS, uri)
    }
}

// ── RequestBuilder ─────────────────────────────────────────────

/// Fluent builder for composing an HTTP request in tests.
///
/// Created by [`TestClient::get()`], [`TestClient::post()`], etc.
/// Call [`.send()`](Self::send) to fire the request and get a
/// [`TestResponse`].
pub struct RequestBuilder {
    router: axum::Router,
    method: Method,
    uri: String,
    headers: Vec<(String, String)>,
    body: Body,
}

impl RequestBuilder {
    fn new(router: axum::Router, method: Method, uri: &str) -> Self {
        Self {
            router,
            method,
            uri: uri.to_owned(),
            headers: Vec::new(),
            body: Body::empty(),
        }
    }

    /// Add a header to the request.
    #[must_use]
    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_owned(), value.to_owned()));
        self
    }

    /// Set the request body to a JSON-serialized value.
    ///
    /// Automatically sets `Content-Type: application/json`.
    #[must_use]
    pub fn json(mut self, value: &serde_json::Value) -> Self {
        self.headers
            .push(("content-type".to_owned(), "application/json".to_owned()));
        self.body = Body::from(serde_json::to_vec(value).expect("failed to serialize JSON body"));
        self
    }

    /// Set the request body to URL-encoded form data.
    ///
    /// Automatically sets `Content-Type: application/x-www-form-urlencoded`
    /// and `Sec-Fetch-Site: same-origin` to mirror what a real browser
    /// would send for a same-origin `<form method="post">` — which is
    /// what the method-override middleware requires to honour
    /// `_method=PUT|PATCH|DELETE` overrides.
    #[must_use]
    pub fn form(mut self, body: &str) -> Self {
        self.headers.push((
            "content-type".to_owned(),
            "application/x-www-form-urlencoded".to_owned(),
        ));
        self.headers
            .push(("sec-fetch-site".to_owned(), "same-origin".to_owned()));
        self.body = Body::from(body.to_owned());
        self
    }

    /// Set a raw string body.
    #[must_use]
    pub fn body(mut self, body: impl Into<Body>) -> Self {
        self.body = body.into();
        self
    }

    /// Fire the request through the full middleware pipeline and return
    /// a [`TestResponse`].
    pub async fn send(self) -> TestResponse {
        let mut builder = Request::builder().method(self.method).uri(&self.uri);

        for (name, value) in &self.headers {
            builder = builder.header(name.as_str(), value.as_str());
        }

        let request = builder.body(self.body).expect("failed to build request");

        // Wrap the router with MethodOverrideLayer the same way the production
        // serve site does, so a POST with a `_method=DELETE` form field reaches
        // the declared DELETE handler in tests too. The layer is a no-op for
        // non-POST methods and non-form bodies, so it's safe to apply
        // unconditionally.
        let service =
            tower::Layer::layer(&crate::middleware::MethodOverrideLayer::new(), self.router);
        let response = service.oneshot(request).await.expect("request failed");

        let status = response.status();
        let headers: Vec<(String, String)> = response
            .headers()
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_owned()))
            .collect();
        let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("failed to read response body");

        TestResponse {
            status,
            headers,
            body: body_bytes.to_vec(),
        }
    }
}

// ── TestResponse ───────────────────────────────────────────────

/// HTTP response from a test request with fluent assertion helpers.
///
/// All assertion methods return `&Self` for chaining:
///
/// ```rust,ignore
/// client.get("/users/1").send().await
///     .assert_ok()
///     .assert_header("content-type", "application/json")
///     .assert_body_contains("Alice");
/// ```
///
/// Fields are public so you can construct a `TestResponse` directly in unit
/// tests that don't need a full HTTP round-trip:
///
/// ```rust
/// use autumn_web::test::TestResponse;
/// use axum::http::StatusCode;
///
/// let resp = TestResponse {
///     status: StatusCode::OK,
///     headers: vec![
///         ("content-type".into(), "application/json".into()),
///         ("x-request-id".into(), "abc-123".into()),
///     ],
///     body: br#"{"name":"Alice"}"#.to_vec(),
/// };
///
/// resp.assert_ok()
///     .assert_header_contains("content-type", "json")
///     .assert_body_contains("Alice");
///
/// assert_eq!(resp.header("x-request-id"), Some("abc-123"));
/// ```
pub struct TestResponse {
    /// HTTP status code.
    pub status: StatusCode,
    /// Response headers as `(name, value)` pairs.
    pub headers: Vec<(String, String)>,
    /// Raw response body bytes.
    pub body: Vec<u8>,
}

impl TestResponse {
    /// Get the response body as a UTF-8 string.
    ///
    /// # Panics
    ///
    /// Panics if the body is not valid UTF-8.
    #[must_use]
    pub fn text(&self) -> String {
        String::from_utf8(self.body.clone()).unwrap_or_else(|e| {
            panic!(
                "response body is not valid UTF-8: {e}\nRaw bytes: {:?}",
                self.body
            )
        })
    }

    /// Deserialize the response body as JSON.
    ///
    /// # Panics
    ///
    /// Panics if the body is not valid JSON or cannot be deserialized
    /// into `T`.
    #[must_use]
    pub fn json<T: serde::de::DeserializeOwned>(&self) -> T {
        serde_json::from_slice(&self.body).unwrap_or_else(|e| {
            panic!(
                "failed to parse response body as JSON: {e}\nBody: {}",
                String::from_utf8_lossy(&self.body)
            )
        })
    }

    /// Get the value of a response header.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        let name_lower = name.to_lowercase();
        self.headers
            .iter()
            .find(|(k, _)| k.to_lowercase() == name_lower)
            .map(|(_, v)| v.as_str())
    }

    // ── Assertion helpers ──────────────────────────────────────

    /// Assert the response status is 200 OK.
    #[track_caller]
    pub fn assert_ok(&self) -> &Self {
        assert_eq!(
            self.status,
            StatusCode::OK,
            "expected 200 OK, got {}.\nBody: {}",
            self.status,
            String::from_utf8_lossy(&self.body)
        );
        self
    }

    /// Assert the response status matches the given code.
    #[track_caller]
    pub fn assert_status(&self, expected: u16) -> &Self {
        assert_eq!(
            self.status.as_u16(),
            expected,
            "expected status {expected}, got {}.\nBody: {}",
            self.status,
            String::from_utf8_lossy(&self.body)
        );
        self
    }

    /// Assert the response status indicates a successful request (2xx).
    #[track_caller]
    pub fn assert_success(&self) -> &Self {
        assert!(
            self.status.is_success(),
            "expected 2xx success, got {}.\nBody: {}",
            self.status,
            String::from_utf8_lossy(&self.body)
        );
        self
    }

    /// Assert a response header exists and equals the expected value.
    #[track_caller]
    pub fn assert_header(&self, name: &str, expected: &str) -> &Self {
        let value = self.header(name).unwrap_or_else(|| {
            panic!(
                "expected header `{name}` to be present.\nAvailable headers: {:?}",
                self.headers
            )
        });
        assert_eq!(
            value, expected,
            "header `{name}`: expected `{expected}`, got `{value}`"
        );
        self
    }

    /// Assert a response header exists and contains the expected substring.
    #[track_caller]
    pub fn assert_header_contains(&self, name: &str, substring: &str) -> &Self {
        let value = self.header(name).unwrap_or_else(|| {
            panic!(
                "expected header `{name}` to be present.\nAvailable headers: {:?}",
                self.headers
            )
        });
        assert!(
            value.contains(substring),
            "header `{name}`: expected `{value}` to contain `{substring}`"
        );
        self
    }

    /// Assert the response body contains the given substring.
    #[track_caller]
    pub fn assert_body_contains(&self, substring: &str) -> &Self {
        let body = self.text();
        assert!(
            body.contains(substring),
            "expected body to contain `{substring}`.\nBody: {body}"
        );
        self
    }

    /// Assert the response body exactly equals the given string.
    #[track_caller]
    pub fn assert_body_eq(&self, expected: &str) -> &Self {
        let body = self.text();
        assert_eq!(body, expected, "body mismatch.\nActual Body: {body}");
        self
    }

    /// Assert the response body deserializes to JSON matching the predicate.
    #[track_caller]
    pub fn assert_json<T, F>(&self, predicate: F) -> &Self
    where
        T: serde::de::DeserializeOwned,
        F: FnOnce(&T),
    {
        let value: T = self.json();
        predicate(&value);
        self
    }

    /// Assert the response body is empty.
    #[track_caller]
    pub fn assert_body_empty(&self) -> &Self {
        assert!(
            self.body.is_empty(),
            "expected empty body, got {} bytes: {}",
            self.body.len(),
            String::from_utf8_lossy(&self.body)
        );
        self
    }

    // ── CSS-selector HTML assertions ────────────────────────────
    //
    // Autumn renders server-side HTML (Maud + htmx), so tests want to assert on
    // page *structure* — "the table has exactly 3 rows", "there is a `<form>`
    // posting to `/notes`" — rather than brittle substrings. These helpers parse
    // the body with a real HTML parser and match against a CSS-selector subset
    // (tag, `.class`, `#id`, `[attr=…]`, plus descendant/child combinators), so
    // assertions survive cosmetic template changes (whitespace, attribute order,
    // wrapping markup) that would break [`assert_body_contains`].
    //
    // They work for full documents and for partial/fragment responses (htmx
    // swaps) alike, and compose with the other matchers — every method returns
    // `&Self` for chaining.
    //
    // ```rust,ignore
    // client.get("/notes").send().await
    //     .assert_ok()
    //     .assert_selector_count("tbody tr.note-row", 3)   // exactly 3 rows
    //     .assert_attr("tr.note-row:first-child a", "href", "/notes/1")
    //     .assert_text("h1", "Notes");
    // ```

    /// Parse the response body as HTML once for a selector assertion.
    fn parse_html(&self) -> Vec<crate::test_html::Node> {
        crate::test_html::parse(&self.text())
    }

    /// Compile a CSS selector, panicking with an actionable message on a
    /// malformed selector.
    #[track_caller]
    fn compile_selector(css: &str) -> crate::test_html::SelectorList {
        crate::test_html::SelectorList::parse(css)
            .unwrap_or_else(|e| panic!("invalid CSS selector `{css}`: {e}"))
    }

    /// A truncated, indented outline of the parsed HTML for failure messages.
    fn html_outline(nodes: &[crate::test_html::Node]) -> String {
        crate::test_html::outline(nodes, 1200)
    }

    /// Return the normalized text content of every element matching `css`, in
    /// document order. Non-asserting accessor for custom assertions.
    ///
    /// Whitespace within each element's text is collapsed and trimmed so values
    /// are stable across indentation and line-wrapping changes.
    #[must_use]
    #[track_caller]
    pub fn selector_text(&self, css: &str) -> Vec<String> {
        let selector = Self::compile_selector(css);
        let nodes = self.parse_html();
        selector
            .matches(&nodes)
            .iter()
            .map(|el| crate::test_html::normalize_ws(&el.text()))
            .collect()
    }

    /// Return the value of attribute `attr` for every element matching `css`,
    /// in document order (`None` for matches lacking the attribute).
    /// Non-asserting accessor for custom assertions.
    #[must_use]
    #[track_caller]
    pub fn selector_attr(&self, css: &str, attr: &str) -> Vec<Option<String>> {
        let selector = Self::compile_selector(css);
        let nodes = self.parse_html();
        selector
            .matches(&nodes)
            .iter()
            .map(|el| el.attr(attr).map(str::to_string))
            .collect()
    }

    /// Return the number of elements matching `css`. Non-asserting accessor.
    #[must_use]
    #[track_caller]
    pub fn selector_count(&self, css: &str) -> usize {
        let selector = Self::compile_selector(css);
        let nodes = self.parse_html();
        selector.matches(&nodes).len()
    }

    /// Assert at least one element matches the CSS selector.
    #[track_caller]
    pub fn assert_selector(&self, css: &str) -> &Self {
        let selector = Self::compile_selector(css);
        let nodes = self.parse_html();
        let count = selector.matches(&nodes).len();
        assert!(
            count > 0,
            "no elements matched selector `{css}`.\nParsed HTML:\n{}",
            Self::html_outline(&nodes)
        );
        self
    }

    /// Assert that *no* element matches the CSS selector.
    #[track_caller]
    pub fn assert_no_selector(&self, css: &str) -> &Self {
        let selector = Self::compile_selector(css);
        let nodes = self.parse_html();
        let count = selector.matches(&nodes).len();
        assert!(
            count == 0,
            "expected no elements matching selector `{css}`, but found {count}.\nParsed HTML:\n{}",
            Self::html_outline(&nodes)
        );
        self
    }

    /// Assert exactly `expected` elements match the CSS selector.
    #[track_caller]
    pub fn assert_selector_count(&self, css: &str, expected: usize) -> &Self {
        let selector = Self::compile_selector(css);
        let nodes = self.parse_html();
        let actual = selector.matches(&nodes).len();
        assert!(
            actual == expected,
            "expected {expected} element(s) matching selector `{css}`, found {actual}.\n\
             Parsed HTML:\n{}",
            Self::html_outline(&nodes)
        );
        self
    }

    /// Assert the first element matching `css` has text content equal to
    /// `expected` (whitespace-normalized on both sides).
    #[track_caller]
    pub fn assert_text(&self, css: &str, expected: &str) -> &Self {
        let selector = Self::compile_selector(css);
        let nodes = self.parse_html();
        let matched = selector.matches(&nodes);
        let Some(first) = matched.into_iter().next() else {
            panic!(
                "no elements matched selector `{css}`.\nParsed HTML:\n{}",
                Self::html_outline(&nodes)
            );
        };
        let actual = crate::test_html::normalize_ws(&first.text());
        let expected_norm = crate::test_html::normalize_ws(expected);
        assert!(
            actual == expected_norm,
            "text mismatch for selector `{css}`:\n  expected: {expected_norm:?}\n  \
             actual:   {actual:?}\nParsed HTML:\n{}",
            Self::html_outline(&nodes)
        );
        self
    }

    /// Assert the first element matching `css` has text content containing
    /// `substring` (whitespace-normalized on both sides).
    #[track_caller]
    pub fn assert_text_contains(&self, css: &str, substring: &str) -> &Self {
        let selector = Self::compile_selector(css);
        let nodes = self.parse_html();
        let matched = selector.matches(&nodes);
        let Some(first) = matched.into_iter().next() else {
            panic!(
                "no elements matched selector `{css}`.\nParsed HTML:\n{}",
                Self::html_outline(&nodes)
            );
        };
        let actual = crate::test_html::normalize_ws(&first.text());
        let needle = crate::test_html::normalize_ws(substring);
        assert!(
            actual.contains(&needle),
            "text for selector `{css}` did not contain {needle:?}.\n  actual: {actual:?}\n\
             Parsed HTML:\n{}",
            Self::html_outline(&nodes)
        );
        self
    }

    /// Assert the first element matching `css` has attribute `attr` equal to
    /// `expected`.
    #[track_caller]
    pub fn assert_attr(&self, css: &str, attr: &str, expected: &str) -> &Self {
        let selector = Self::compile_selector(css);
        let nodes = self.parse_html();
        let matched = selector.matches(&nodes);
        let Some(first) = matched.into_iter().next() else {
            panic!(
                "no elements matched selector `{css}`.\nParsed HTML:\n{}",
                Self::html_outline(&nodes)
            );
        };
        match first.attr(attr) {
            Some(actual) => assert!(
                actual == expected,
                "attribute `{attr}` mismatch for selector `{css}`:\n  expected: {expected:?}\n  \
                 actual:   {actual:?}\nParsed HTML:\n{}",
                Self::html_outline(&nodes)
            ),
            None => panic!(
                "element matching selector `{css}` has no `{attr}` attribute.\n\
                 Parsed HTML:\n{}",
                Self::html_outline(&nodes)
            ),
        }
        self
    }
}

#[cfg(feature = "db")]
struct TransactionalDbInterceptor;

#[cfg(feature = "db")]
impl crate::interceptor::DbConnectionInterceptor for TransactionalDbInterceptor {
    fn intercept_checkout<'a>(
        &'a self,
        _ctx: crate::interceptor::DbCheckoutContext,
        next: std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<crate::db::PooledConnection, crate::AutumnError>,
                    > + Send
                    + 'a,
            >,
        >,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<crate::db::PooledConnection, crate::AutumnError>,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            let mut conn = next.await?;

            // Check if transaction has already been started on this connection
            let guc_result = diesel::select(diesel::dsl::sql::<
                diesel::sql_types::Nullable<diesel::sql_types::Text>,
            >(
                "current_setting('autumn.test_transaction_started', true)",
            ))
            .get_result::<Option<String>>(&mut *conn)
            .await;

            match guc_result {
                Ok(Some(ref s)) if s == "true" => {
                    // Already started and healthy
                }
                Ok(_) => {
                    use diesel_async::AsyncConnection;
                    use diesel_async::RunQueryDsl;

                    conn.begin_test_transaction().await.map_err(|e| {
                        crate::AutumnError::internal_server_error_msg(format!(
                            "failed to start test transaction: {e}"
                        ))
                    })?;

                    diesel::sql_query("SET autumn.test_transaction_started = 'true'")
                        .execute(&mut *conn)
                        .await
                        .map_err(|e| {
                            crate::AutumnError::internal_server_error_msg(format!(
                                "failed to set transaction session GUC: {e}"
                            ))
                        })?;
                }
                Err(_) => {
                    // The GUC query failed. This happens when the connection is in a failed/aborted transaction block.
                    // Since the transaction is already active (but aborted), do not retry begin_test_transaction!
                }
            }
            Ok(conn)
        })
    }

    fn is_transactional_test(&self) -> bool {
        true
    }
}

#[cfg(feature = "db")]
struct ComposedDbInterceptor {
    first: std::sync::Arc<dyn crate::interceptor::DbConnectionInterceptor>,
    second: std::sync::Arc<dyn crate::interceptor::DbConnectionInterceptor>,
}

#[cfg(feature = "db")]
impl crate::interceptor::DbConnectionInterceptor for ComposedDbInterceptor {
    fn intercept_checkout<'a>(
        &'a self,
        ctx: crate::interceptor::DbCheckoutContext,
        next: std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<crate::db::PooledConnection, crate::AutumnError>,
                    > + Send
                    + 'a,
            >,
        >,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<crate::db::PooledConnection, crate::AutumnError>,
                > + Send
                + 'a,
        >,
    > {
        let next_wrapped = self.second.intercept_checkout(ctx.clone(), next);
        self.first.intercept_checkout(ctx, next_wrapped)
    }

    fn is_transactional_test(&self) -> bool {
        self.first.is_transactional_test() || self.second.is_transactional_test()
    }
}

// ── TestDb ─────────────────────────────────────────────────────

/// Shared Postgres testcontainer for database integration tests.
///
/// Rather than spinning up a new container per test (slow!), `TestDb`
/// provides a shared container that all tests in a binary can reuse.
/// This mirrors Spring Boot's `@Testcontainers` with `@Container` +
/// `static` pattern.
///
/// Requires the `test-support` feature (and `db`):
///
/// ```toml
/// [dev-dependencies]
/// autumn-web = { path = "..", features = ["test-support"] }
/// ```
///
/// # Examples
///
/// ```rust,ignore
/// use autumn_web::test::{TestApp, TestDb};
///
/// #[tokio::test]
/// #[ignore = "requires Docker"]
/// async fn db_test() {
///     let db = TestDb::shared().await;
///     let client = TestApp::new()
///         .routes(routes![my_handler])
///         .with_db(db.pool())
///         .build();
///
///     // Run migrations or seed data via db.pool()
///     client.get("/data").send().await.assert_ok();
/// }
/// ```
#[cfg(all(feature = "db", feature = "test-support"))]
pub struct TestDb {
    _container: testcontainers::ContainerAsync<testcontainers_modules::postgres::Postgres>,
    pool: Pool<AsyncPgConnection>,
    url: String,
}

#[cfg(all(feature = "db", feature = "test-support"))]
impl TestDb {
    /// Start a new Postgres testcontainer and create a connection pool.
    ///
    /// For most test suites, prefer [`TestDb::shared()`] to reuse a
    /// single container across all tests.
    pub async fn new() -> Self {
        use diesel_async::pooled_connection::AsyncDieselConnectionManager;
        use testcontainers::runners::AsyncRunner;
        use testcontainers_modules::postgres::Postgres;

        let container = Postgres::default()
            .start()
            .await
            .expect("failed to start Postgres testcontainer (is Docker running?)");

        let host = container
            .get_host()
            .await
            .expect("failed to build test router");
        let port = container
            .get_host_port_ipv4(5432)
            .await
            .expect("failed to build test router");
        let url = format!("postgres://postgres:postgres@{host}:{port}/postgres");

        let manager = AsyncDieselConnectionManager::<AsyncPgConnection>::new(&url);
        let pool = Pool::builder(manager)
            .max_size(5)
            .build()
            .expect("failed to build connection pool");

        Self {
            _container: container,
            pool,
            url,
        }
    }

    /// Get a shared `TestDb` instance, starting the container on first use.
    ///
    /// Uses a process-global `OnceLock` so the container is started only
    /// once per test binary, regardless of how many tests call this method.
    /// This dramatically speeds up test suites with multiple DB tests.
    ///
    /// The container is automatically cleaned up when the process exits.
    pub async fn shared() -> &'static Self {
        use std::sync::OnceLock;
        use tokio::sync::OnceCell;

        // Two-phase init: OnceLock for the OnceCell, OnceCell for the async init.
        static CELL: OnceLock<OnceCell<TestDb>> = OnceLock::new();
        let once = CELL.get_or_init(OnceCell::new);
        once.get_or_init(Self::new).await
    }

    /// Get the database connection pool.
    #[must_use]
    pub fn pool(&self) -> Pool<AsyncPgConnection> {
        self.pool.clone()
    }

    /// Get the Postgres connection URL.
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Execute raw SQL against the test database.
    ///
    /// Useful for creating tables, seeding data, or running migrations
    /// in tests.
    ///
    /// # Examples
    ///
    /// ```rust,ignore
    /// let db = TestDb::shared().await;
    /// db.execute_sql("CREATE TABLE IF NOT EXISTS users (id SERIAL PRIMARY KEY, name TEXT NOT NULL)")
    ///     .await;
    /// ```
    pub async fn execute_sql(&self, sql: &str) {
        use diesel_async::RunQueryDsl;
        let mut conn = self.pool.get().await.expect("failed to get connection");
        diesel::sql_query(sql)
            .execute(&mut *conn)
            .await
            .unwrap_or_else(|e| panic!("SQL execution failed: {e}\nSQL: {sql}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cleanup_probe_job(
        _state: crate::state::AppState,
        _payload: serde_json::Value,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = crate::AutumnResult<()>> + Send + 'static>,
    > {
        Box::pin(async move { Ok(()) })
    }

    struct CleanupJobPlugin;

    impl crate::plugin::Plugin for CleanupJobPlugin {
        fn build(self, app: crate::app::AppBuilder) -> crate::app::AppBuilder {
            app.jobs(vec![crate::job::JobInfo {
                name: "cleanup_probe".to_string(),
                max_attempts: 1,
                initial_backoff_ms: 1,
                queue: "default".to_string(),
                uniqueness: None,
                concurrency: None,
                handler: cleanup_probe_job,
            }])
        }
    }

    fn test_routes() -> Vec<Route> {
        use axum::routing;

        async fn hello() -> &'static str {
            "hello"
        }

        async fn echo_json(
            axum::Json(value): axum::Json<serde_json::Value>,
        ) -> axum::Json<serde_json::Value> {
            axum::Json(value)
        }

        async fn status_201() -> (StatusCode, &'static str) {
            (StatusCode::CREATED, "created")
        }

        vec![
            Route {
                method: Method::GET,
                path: "/hello",
                handler: routing::get(hello),
                name: "hello",
                api_doc: crate::openapi::ApiDoc {
                    method: "GET",
                    path: "/hello",
                    operation_id: "hello",
                    success_status: 200,
                    ..Default::default()
                },
                repository: None,
                idempotency: crate::route::RouteIdempotency::Direct,
                timeout: crate::route::RouteTimeout::Inherit,
                api_version: None,
                sunset_opt_out: false,
            },
            Route {
                method: Method::POST,
                path: "/echo",
                handler: routing::post(echo_json),
                name: "echo",
                api_doc: crate::openapi::ApiDoc {
                    method: "POST",
                    path: "/echo",
                    operation_id: "echo",
                    success_status: 200,
                    ..Default::default()
                },
                repository: None,
                idempotency: crate::route::RouteIdempotency::Direct,
                timeout: crate::route::RouteTimeout::Inherit,
                api_version: None,
                sunset_opt_out: false,
            },
            Route {
                method: Method::POST,
                path: "/create",
                handler: routing::post(status_201),
                name: "create",
                api_doc: crate::openapi::ApiDoc {
                    method: "POST",
                    path: "/create",
                    operation_id: "create",
                    success_status: 201,
                    ..Default::default()
                },
                repository: None,
                idempotency: crate::route::RouteIdempotency::Direct,
                timeout: crate::route::RouteTimeout::Inherit,
                api_version: None,
                sunset_opt_out: false,
            },
        ]
    }

    #[tokio::test]
    async fn test_app_get_request() {
        let client = TestApp::new().routes(test_routes()).build();
        client.get("/hello").send().await.assert_ok();
    }

    #[tokio::test]
    async fn test_app_post_json() {
        let client = TestApp::new().routes(test_routes()).build();

        client
            .post("/echo")
            .json(&serde_json::json!({"key": "value"}))
            .send()
            .await
            .assert_ok()
            .assert_body_contains("key");
    }

    #[tokio::test]
    async fn test_response_assert_status() {
        let client = TestApp::new().routes(test_routes()).build();

        client
            .post("/create")
            .send()
            .await
            .assert_status(201)
            .assert_body_eq("created");
    }

    #[tokio::test]
    async fn test_response_assert_success() {
        let client = TestApp::new().routes(test_routes()).build();
        client.get("/hello").send().await.assert_success();
    }

    #[tokio::test]
    async fn test_not_found() {
        let client = TestApp::new().routes(test_routes()).build();
        client.get("/nonexistent").send().await.assert_status(404);
    }

    #[tokio::test]
    async fn test_response_json_deserialization() {
        let client = TestApp::new().routes(test_routes()).build();

        let resp = client
            .post("/echo")
            .json(&serde_json::json!({"count": 42}))
            .send()
            .await;

        resp.assert_ok().assert_json::<serde_json::Value, _>(|v| {
            assert_eq!(v["count"], 42);
        });
    }

    #[tokio::test]
    async fn test_custom_header() {
        let client = TestApp::new().routes(test_routes()).build();

        let resp = client
            .get("/hello")
            .header("x-custom", "test-value")
            .send()
            .await;
        resp.assert_ok();
    }

    #[tokio::test]
    async fn test_client_default() {
        let _app = TestApp::default();
    }

    #[tokio::test]
    async fn dropping_test_client_stops_test_started_job_runtime() {
        let _guard = crate::job::global_job_runtime_test_lock().lock().await;
        crate::job::clear_global_job_client();

        let client = TestApp::new().plugin(CleanupJobPlugin).build();
        let leaked_client = crate::job::global_job_client().expect("test job runtime should start");

        drop(client);

        assert!(
            crate::job::global_job_client().is_none(),
            "dropping a TestClient with jobs must clear its global job client"
        );

        let mut last_enqueue_error = None;
        for _ in 0..25 {
            match leaked_client
                .enqueue("cleanup_probe", serde_json::json!({}))
                .await
            {
                Ok(()) => tokio::time::sleep(std::time::Duration::from_millis(10)).await,
                Err(error) => {
                    last_enqueue_error = Some(error.to_string());
                    break;
                }
            }
        }

        assert!(
            last_enqueue_error
                .as_deref()
                .is_some_and(|message| message.contains("failed to enqueue job")),
            "captured pre-drop job client must stop accepting jobs after TestClient drop; \
             last error: {last_enqueue_error:?}"
        );

        crate::job::clear_global_job_client();
    }

    #[cfg(feature = "mail")]
    #[test]
    fn plugin_suppression_store_and_endpoint_optin_carry_into_test_app() {
        struct SuppressionPlugin;
        impl crate::plugin::Plugin for SuppressionPlugin {
            fn build(self, app: crate::app::AppBuilder) -> crate::app::AppBuilder {
                app.with_suppression_store(crate::mail::InMemorySuppressionStore::new())
                    .mount_unsubscribe_endpoint()
            }
        }

        // A plugin that wires List-Unsubscribe storage and opts into the default
        // endpoint must propagate both into the TestApp, so unsubscribe POSTs /
        // send-time suppression behave under TestApp exactly as in production
        // without every test repeating the setup manually.
        let app = TestApp::new().plugin(SuppressionPlugin);
        assert!(
            app.suppression_store.is_some(),
            "plugin-registered suppression store must be carried into TestApp"
        );
        assert!(
            app.config.mail.mount_unsubscribe_endpoint,
            "plugin endpoint opt-in must be carried into TestApp config"
        );
    }

    /// End-to-end acceptance for issue #605: a plain `<form method="post">`
    /// carrying `_method=DELETE` reaches the declared DELETE handler when
    /// dispatched through the same router/middleware stack the production
    /// app builder uses.
    #[tokio::test]
    async fn test_app_routes_html_method_override_to_delete() {
        use axum::routing;
        async fn deleted() -> &'static str {
            "deleted"
        }
        let routes = vec![Route {
            method: Method::DELETE,
            path: "/items/{id}",
            handler: routing::delete(deleted),
            name: "items_delete",
            api_doc: crate::openapi::ApiDoc {
                method: "DELETE",
                path: "/items/{id}",
                operation_id: "items_delete",
                success_status: 200,
                ..Default::default()
            },
            repository: None,
            idempotency: crate::route::RouteIdempotency::Direct,
            timeout: crate::route::RouteTimeout::Inherit,
            api_version: None,
            sunset_opt_out: false,
        }];
        let client = TestApp::new().routes(routes).build();

        client
            .post("/items/1")
            .form("_method=DELETE")
            .send()
            .await
            .assert_ok()
            .assert_body_eq("deleted");
    }

    // ── CSS-selector HTML assertions (issue #1147) ─────────────────────────
    //
    // These tests are the executable specification for the selector-aware
    // assertions on [`TestResponse`]. They exercise the success metric:
    // a structural assertion against a notes index survives a cosmetic
    // template refactor (indentation, attribute order, wrapping markup)
    // that would break the equivalent `assert_body_contains` substring test.
    #[cfg(feature = "maud")]
    mod html_assertions {
        use super::*;
        use axum::routing::get;

        /// The "original" notes index: a 3-row table where each `<tr>` links
        /// to `/notes/{id}`.
        async fn notes_index_v1() -> maud::Markup {
            maud::html! {
                table.notes {
                    tbody {
                        @for id in 1..=3u32 {
                            tr.note-row {
                                td.title { a href=(format!("/notes/{id}")) { "Note " (id) } }
                            }
                        }
                    }
                }
            }
        }

        /// The same index after a cosmetic refactor: attribute order changed,
        /// extra wrapping markup and classes, different nesting — but the same
        /// structural facts (3 rows, each linking to `/notes/{id}`).
        async fn notes_index_v2() -> maud::Markup {
            maud::html! {
                div.card {
                    table.notes.striped {
                        thead { tr { th { "Title" } } }
                        tbody.rows {
                            @for id in 1..=3u32 {
                                tr.note-row.is-clickable data-id=(id) {
                                    td.title {
                                        span.wrap {
                                            a.link href=(format!("/notes/{id}")) data-turbo="true" {
                                                "Note " (id)
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

        /// An htmx swap fragment: a bare `<tr>` with no enclosing `<table>`.
        async fn note_row_fragment() -> maud::Markup {
            maud::html! {
                tr.note-row #note-7 {
                    td.title { a.link href="/notes/7" { "Note 7" } }
                }
            }
        }

        fn client(
            path: &str,
            handler: axum::routing::MethodRouter<crate::state::AppState>,
        ) -> TestClient {
            let router = axum::Router::<crate::state::AppState>::new().route(path, handler);
            TestApp::new().merge(router).build()
        }

        #[tokio::test]
        async fn counts_rows_by_tag_and_class() {
            let resp = client("/notes", get(notes_index_v1))
                .get("/notes")
                .send()
                .await;
            resp.assert_ok()
                .assert_selector("table.notes")
                .assert_selector_count("tbody tr", 3)
                .assert_selector_count("tr.note-row", 3)
                .assert_no_selector("form");
        }

        #[tokio::test]
        async fn reads_text_and_attributes() {
            let resp = client("/notes", get(notes_index_v1))
                .get("/notes")
                .send()
                .await;
            resp.assert_text("tr.note-row td.title a", "Note 1")
                .assert_text_contains("tr.note-row", "Note 1")
                .assert_attr("tr.note-row td a", "href", "/notes/1");

            // Non-asserting accessors compose for custom assertions.
            let links = resp.selector_text("tr.note-row a");
            assert_eq!(links, vec!["Note 1", "Note 2", "Note 3"]);
            let hrefs = resp.selector_attr("tr.note-row a", "href");
            assert_eq!(
                hrefs,
                vec![
                    Some("/notes/1".to_string()),
                    Some("/notes/2".to_string()),
                    Some("/notes/3".to_string()),
                ]
            );
            assert_eq!(resp.selector_count("tr.note-row"), 3);
        }

        /// The success metric: identical structural assertions pass against
        /// both the original and the refactored template.
        #[tokio::test]
        async fn survives_cosmetic_refactor() {
            for handler in [get(notes_index_v1), get(notes_index_v2)] {
                let resp = client("/notes", handler).get("/notes").send().await;
                resp.assert_ok()
                    // Exactly three data rows, each linking to /notes/{id}.
                    .assert_selector_count("tbody tr.note-row", 3);
                let hrefs = resp.selector_attr("tbody tr.note-row a", "href");
                assert_eq!(
                    hrefs,
                    vec![
                        Some("/notes/1".to_string()),
                        Some("/notes/2".to_string()),
                        Some("/notes/3".to_string()),
                    ],
                    "row links must survive the refactor"
                );
            }
        }

        /// AC: works for partial/fragment responses (htmx swaps) — a bare
        /// `<tr>` with no enclosing table must still be selectable.
        #[tokio::test]
        async fn works_for_htmx_fragment() {
            let resp = client("/rows/7", get(note_row_fragment))
                .get("/rows/7")
                .send()
                .await;
            resp.assert_selector("tr.note-row")
                .assert_selector("tr#note-7")
                .assert_attr("tr#note-7 a", "href", "/notes/7")
                .assert_text("tr#note-7 a.link", "Note 7");
        }

        #[tokio::test]
        async fn id_and_attribute_selectors() {
            let resp = client("/rows/7", get(note_row_fragment))
                .get("/rows/7")
                .send()
                .await;
            resp.assert_selector("#note-7")
                .assert_selector("a[href=\"/notes/7\"]")
                .assert_selector("a[href^=\"/notes/\"]")
                .assert_no_selector("a[href=\"/other\"]");
        }

        #[tokio::test]
        #[should_panic(expected = "expected 5 element(s) matching selector")]
        async fn count_mismatch_panics_with_actionable_message() {
            let resp = client("/notes", get(notes_index_v1))
                .get("/notes")
                .send()
                .await;
            resp.assert_selector_count("tr.note-row", 5);
        }

        #[tokio::test]
        #[should_panic(expected = "no elements matched selector `table.missing`")]
        async fn missing_selector_panics() {
            let resp = client("/notes", get(notes_index_v1))
                .get("/notes")
                .send()
                .await;
            resp.assert_selector("table.missing");
        }
    }

    /// Companion to the override test: an invalid `_method` value rejects
    /// with `400 Bad Request` before reaching any handler.
    #[tokio::test]
    async fn test_app_routes_invalid_method_override_rejected() {
        let client = TestApp::new().routes(test_routes()).build();

        client
            .post("/create")
            .form("_method=BREW")
            .send()
            .await
            .assert_status(400);
    }

    /// The outer `MethodOverrideLayer` stamps a `MethodOverrideRejection`
    /// extension instead of short-circuiting, so the inner
    /// `method_override_rejection_filter` produces the `400` from inside
    /// the per-route layer chain. Verify that framework response
    /// middleware (request-ID header, security headers) still wraps that
    /// `400` — i.e. malformed requests inherit the same response middleware
    /// as ordinary handler responses, rather than bypassing it.
    #[tokio::test]
    async fn invalid_method_override_response_carries_framework_middleware() {
        let client = TestApp::new().routes(test_routes()).build();

        let response = client.post("/create").form("_method=BREW").send().await;
        response.assert_status(400);

        // RequestIdLayer is applied via `Router::layer` in
        // `apply_middleware` and stamps a response header on every
        // request that flows through the inner router. If the override
        // layer short-circuited at the outer wrapper, this header would
        // be absent.
        assert!(
            response.header("x-request-id").is_some(),
            "framework request-id header must wrap method-override rejections; \
             observed headers: {:?}",
            response.headers
        );
        // SecurityHeadersLayer applies a default set of headers; pick a
        // representative one to assert the layer ran on this response.
        assert!(
            response.header("x-content-type-options").is_some(),
            "framework security headers must wrap method-override rejections; \
             observed headers: {:?}",
            response.headers
        );
    }
}
