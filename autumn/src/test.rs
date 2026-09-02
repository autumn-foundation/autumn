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
//!     ..Default::default()
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
//!
//! # Asserting channel broadcasts
//!
//! Opt in with `TestApp::record_broadcasts` to capture every channel
//! publication a request makes — no hand-written spy needed — then assert on
//! it with `TestClient::assert_broadcast`,
//! `TestClient::assert_broadcast_count`,
//! `TestClient::assert_no_broadcasts`, or read them back in order with
//! `TestClient::broadcasts` / `TestClient::broadcasts_on`. Both raw
//! `publish` text and `publish_html` HTML/OOB payloads are recorded. The
//! recorder is scoped to the client, so parallel tests never leak into one
//! another, and nothing is installed unless you call it.
//!
//! ```rust
//! # #[cfg(feature = "ws")]
//! # mod broadcast_example {
//! use autumn_web::prelude::*;
//! use autumn_web::test::TestApp;
//!
//! #[post("/notes")]
//! async fn create_note(State(state): State<AppState>) -> &'static str {
//!     state.broadcast().publish("notes", "created").unwrap();
//!     "ok"
//! }
//!
//! pub fn run() {
//!     tokio::runtime::Runtime::new().unwrap().block_on(async {
//!         let client = TestApp::new()
//!             .routes(routes![create_note])
//!             .record_broadcasts()
//!             .build();
//!
//!         client.post("/notes").send().await.assert_ok();
//!
//!         client
//!             .assert_broadcast_count("notes", 1)
//!             .assert_broadcast("notes", |b| b.payload() == "created");
//!     });
//! }
//! # }
//! # #[cfg(feature = "ws")]
//! # broadcast_example::run();
//! ```
//!
//! # Testing authenticated routes
//!
//! [`TestClient`] carries a **cookie jar**: every response's `Set-Cookie` is
//! stored and replayed on later requests from the same client, so a real
//! `POST /login` → `GET /dashboard` flow works with zero manual header
//! threading — exactly like a browser session.
//!
//! When you only need an authenticated *identity* (not the login endpoint
//! under test), [`TestClient::acting_as`] mints the session directly, so a
//! secured route can be tested in ≤2 lines of setup:
//!
//! ```rust
//! # mod acting_as_example {
//! use autumn_web::prelude::*;
//! use autumn_web::test::TestApp;
//!
//! #[get("/dashboard")]
//! #[secured]
//! async fn dashboard() -> &'static str {
//!     "welcome"
//! }
//!
//! pub fn run() {
//!     tokio::runtime::Runtime::new().unwrap().block_on(async {
//!         let client = TestApp::new().routes(routes![dashboard]).build();
//!         client.acting_as(42).await; // ← authenticated as user 42
//!
//!         client.get("/dashboard").send().await.assert_ok();
//!     });
//! }
//! # }
//! # acting_as_example::run();
//! ```
//!
//! `acting_as` sets **identity only** — authorization still runs, so a user it
//! acts as who lacks a required role or scope is still denied.
//! [`TestClient::log_out`] clears the session cookie, reverting the client to
//! an unauthenticated state. These helpers mirror the auth-testing story in
//! other frameworks:
//!
//! | Autumn | Laravel | Rails | Django | Phoenix |
//! |--------|---------|-------|--------|---------|
//! | [`acting_as`](TestClient::acting_as) / [`login_as`](TestClient::login_as) | `actingAs` | `sign_in` | `force_login` | `log_in_user` |
//! | [`log_out`](TestClient::log_out) | `Auth::logout` | `sign_out` | `logout` | `log_out_user` |

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use tower::ServiceExt;

use crate::config::AutumnConfig;
use crate::route::Route;

use crate::state::AppState;

// Only the `test-support`-gated `TestDb` (a Postgres testcontainer helper) names
// `AsyncPgConnection` by its short name now; the `TestClient` pool fields use the
// `RuntimeConnection` alias, and the transactional establish path uses the fully
// qualified path — so without `test-support` this import would be unused.
#[cfg(all(feature = "db", feature = "test-support"))]
use diesel_async::AsyncPgConnection;
// Used by the Postgres transactional establish path (the `.get_result()` on
// `TransactionalDbInterceptor`), which is itself gated `not(feature = "sqlite")`;
// every other `RunQueryDsl` method call in this module brings the trait in via a
// local `use`, so this import is unused under any `sqlite` build.
#[cfg(all(feature = "db", not(feature = "sqlite")))]
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

/// A single background-job enqueue captured by the built-in test job recorder.
///
/// Available on [`TestClient`] via [`TestClient::enqueued_jobs`]. The recorder
/// is always on for [`TestApp`]-built clients — no `.with_job_interceptor()`
/// boilerplate is required. Both the registered job `name` and the fully
/// serialized `payload` (the exact `serde_json::Value` handed to the backend)
/// are captured, so assertions can match on name alone or name-and-payload.
///
/// # Example
///
/// ```rust,ignore
/// use autumn_web::test::TestApp;
/// use serde_json::json;
///
/// let client = TestApp::new().plugin(MyJobs).routes(routes![signup]).build();
/// client.post("/signup").json(&body).send().await.assert_ok();
///
/// client.assert_job_enqueued_with("send_welcome", json!({ "user_id": 7 }));
/// ```
#[derive(Clone, Debug)]
pub struct RecordedJob {
    /// The registered name of the enqueued job.
    pub name: String,
    /// The JSON payload the job was enqueued with (the real serialized args).
    pub payload: serde_json::Value,
}

/// Built-in per-`TestApp` recording job interceptor.
///
/// Auto-installed by [`TestApp::build`] — no `.with_job_interceptor()` needed.
/// Composes with any user-supplied interceptor (the user's interceptor still
/// runs, after the recorder). Records every enqueue — across `enqueue`,
/// `enqueue_after_commit`, and `enqueue_in_tx`, which all funnel through the
/// same enqueue interceptor seam — in the order they were enqueued.
#[derive(Clone, Default)]
struct JobRecorder {
    jobs: std::sync::Arc<std::sync::Mutex<Vec<RecordedJob>>>,
}

impl JobRecorder {
    fn new() -> Self {
        Self::default()
    }

    fn recorded(&self) -> Vec<RecordedJob> {
        self.jobs.lock().unwrap().clone()
    }

    /// Take the captured jobs, leaving the recorder empty — used by
    /// [`TestClient::perform_enqueued_jobs`] to drain the queue exactly once.
    fn drain(&self) -> Vec<RecordedJob> {
        std::mem::take(&mut *self.jobs.lock().unwrap())
    }
}

impl crate::interceptor::JobInterceptor for JobRecorder {
    fn intercept_enqueue<'a>(
        &'a self,
        name: &'a str,
        payload: &'a serde_json::Value,
        next: std::pin::Pin<
            Box<dyn std::future::Future<Output = crate::AutumnResult<()>> + Send + 'a>,
        >,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = crate::AutumnResult<()>> + Send + 'a>>
    {
        let record = RecordedJob {
            name: name.to_string(),
            payload: payload.clone(),
        };
        let jobs = std::sync::Arc::clone(&self.jobs);
        Box::pin(async move {
            // Record the enqueue intent up front, then let delivery proceed so
            // the app's real backend/worker still sees the job (mirroring how
            // the mail recorder does not suppress the underlying transport).
            jobs.lock().unwrap().push(record);
            next.await
        })
    }

    fn intercept_execute<'a>(
        &'a self,
        _name: &'a str,
        _payload: &'a serde_json::Value,
        next: std::pin::Pin<
            Box<dyn std::future::Future<Output = crate::AutumnResult<()>> + Send + 'a>,
        >,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = crate::AutumnResult<()>> + Send + 'a>>
    {
        // The recorder only observes enqueues; execution passes straight through.
        next
    }
}

/// Chains two [`JobInterceptor`](crate::interceptor::JobInterceptor)s so that
/// `first` runs before `second`, both before the actual enqueue/execute.
struct ChainedJobInterceptor {
    first: std::sync::Arc<dyn crate::interceptor::JobInterceptor>,
    second: std::sync::Arc<dyn crate::interceptor::JobInterceptor>,
}

impl crate::interceptor::JobInterceptor for ChainedJobInterceptor {
    fn intercept_enqueue<'a>(
        &'a self,
        name: &'a str,
        payload: &'a serde_json::Value,
        next: std::pin::Pin<
            Box<dyn std::future::Future<Output = crate::AutumnResult<()>> + Send + 'a>,
        >,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = crate::AutumnResult<()>> + Send + 'a>>
    {
        let second_next = self.second.intercept_enqueue(name, payload, next);
        self.first.intercept_enqueue(name, payload, second_next)
    }

    fn intercept_execute<'a>(
        &'a self,
        name: &'a str,
        payload: &'a serde_json::Value,
        next: std::pin::Pin<
            Box<dyn std::future::Future<Output = crate::AutumnResult<()>> + Send + 'a>,
        >,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = crate::AutumnResult<()>> + Send + 'a>>
    {
        let second_next = self.second.intercept_execute(name, payload, next);
        self.first.intercept_execute(name, payload, second_next)
    }
}

/// Outcome report returned by [`TestClient::perform_enqueued_jobs`].
///
/// Holds one `(job name, result)` entry per drained job, in the order the jobs
/// were enqueued. Per-job handler errors are surfaced here rather than
/// swallowed: inspect them with [`Self::failures`], or fail the test outright
/// with [`Self::assert_all_succeeded`]. A captured job whose name has no
/// registered handler is reported as a failure too — never silently skipped.
///
/// # Example
///
/// ```rust,ignore
/// let report = client.perform_enqueued_jobs().await;
/// report.assert_all_succeeded();
/// ```
#[derive(Debug)]
pub struct PerformedJobs {
    outcomes: Vec<(String, crate::AutumnResult<()>)>,
}

impl PerformedJobs {
    /// Every performed job's `(name, result)`, in the order they were enqueued.
    pub fn outcomes(&self) -> &[(String, crate::AutumnResult<()>)] {
        &self.outcomes
    }

    /// The number of jobs that were drained and performed.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.outcomes.len()
    }

    /// Whether no jobs were performed (the queue was empty).
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.outcomes.is_empty()
    }

    /// The `(name, error)` pairs for every job whose handler returned `Err`
    /// (or that had no registered handler).
    #[must_use]
    pub fn failures(&self) -> Vec<(&str, &crate::AutumnError)> {
        self.outcomes
            .iter()
            .filter_map(|(name, result)| result.as_ref().err().map(|e| (name.as_str(), e)))
            .collect()
    }

    /// Assert every performed job succeeded.
    ///
    /// # Panics
    ///
    /// Panics, listing each failing job's name and error, if any performed job
    /// returned an error or had no registered handler.
    pub fn assert_all_succeeded(&self) -> &Self {
        let failures = self.failures();
        assert!(
            failures.is_empty(),
            "expected all performed jobs to succeed, but {} failed:\n{}",
            failures.len(),
            failures
                .iter()
                .map(|(name, err)| format!("  - {name}: {err:?}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
        self
    }
}

/// Render a captured-job list for self-diagnosing assertion failures.
fn format_recorded_jobs(jobs: &[RecordedJob]) -> String {
    if jobs.is_empty() {
        return "  (no jobs were enqueued)".to_string();
    }
    jobs.iter()
        .map(|j| format!("  - {} {}", j.name, j.payload))
        .collect::<Vec<_>>()
        .join("\n")
}

/// A single channel publication captured by the broadcast recorder.
///
/// Recorded by [`TestApp::record_broadcasts`] through the channels
/// interceptor seam. Both raw `publish` text and `publish_html` HTML/OOB
/// payloads are captured (they funnel through the same `ChannelMessage`).
#[cfg(feature = "ws")]
#[derive(Clone, Debug)]
pub struct RecordedBroadcast {
    /// The topic the message was published to.
    pub topic: String,
    /// The UTF-8 payload of the published `ChannelMessage`.
    pub payload: String,
}

#[cfg(feature = "ws")]
impl RecordedBroadcast {
    /// The topic the message was published to.
    #[must_use]
    pub fn topic(&self) -> &str {
        &self.topic
    }

    /// The UTF-8 payload of the published message.
    #[must_use]
    pub fn payload(&self) -> &str {
        &self.payload
    }
}

/// Built-in per-`TestClient` recording channels interceptor.
///
/// Opt-in via [`TestApp::record_broadcasts`] — no interceptor is installed
/// unless the builder is called (zero-cost when unused). Records every
/// publication in order, including publishes to zero subscribers.
#[cfg(feature = "ws")]
#[derive(Clone, Default)]
struct BroadcastRecorder {
    events: std::sync::Arc<std::sync::Mutex<Vec<RecordedBroadcast>>>,
}

#[cfg(feature = "ws")]
impl BroadcastRecorder {
    fn new() -> Self {
        Self::default()
    }

    fn recorded(&self) -> Vec<RecordedBroadcast> {
        self.events.lock().unwrap().clone()
    }
}

#[cfg(feature = "ws")]
impl crate::interceptor::ChannelsInterceptor for BroadcastRecorder {
    fn intercept_publish(
        &self,
        topic: &str,
        msg: &crate::channels::ChannelMessage,
        next: &dyn Fn(
            &str,
            &crate::channels::ChannelMessage,
        ) -> Result<usize, crate::channels::ChannelPublishError>,
    ) -> Result<usize, crate::channels::ChannelPublishError> {
        let result = next(topic, msg);
        // Record the publication even when it reached zero subscribers — the
        // publish still happened and tests assert on intent, not delivery.
        self.events.lock().unwrap().push(RecordedBroadcast {
            topic: topic.into(),
            payload: msg.as_str().into(),
        });
        result
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
    pool: Option<Pool<crate::db::RuntimeConnection>>,
    #[cfg(feature = "db")]
    replica_pool: Option<Pool<crate::db::RuntimeConnection>>,
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
    /// Always-on job recorder capturing every enqueue. Composed ahead of any
    /// user-supplied [`with_job_interceptor`](Self::with_job_interceptor).
    job_recorder: JobRecorder,
    /// Authored fault schedule attached via
    /// [`with_fault_plan`](Self::with_fault_plan) (issue #1680); `None` means no
    /// fault interceptors are installed at all.
    fault_plan: Option<crate::sim::fault::FaultPlan>,
    #[cfg(feature = "db")]
    db_interceptor: Option<std::sync::Arc<dyn crate::interceptor::DbConnectionInterceptor>>,
    #[cfg(feature = "ws")]
    channels_interceptor: Option<std::sync::Arc<dyn crate::interceptor::ChannelsInterceptor>>,
    /// Opt-in broadcast recorder, installed only when
    /// [`record_broadcasts`](Self::record_broadcasts) is called.
    #[cfg(feature = "ws")]
    broadcast_recorder: Option<BroadcastRecorder>,
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
    #[cfg(feature = "mail")]
    mail_suppression_store: Option<crate::mail::suppression::SuppressionStoreHandle>,
    registered_plugins: std::collections::HashSet<String>,
    extensions: std::collections::HashMap<std::any::TypeId, Box<dyn std::any::Any + Send>>,
    /// Injected clock; `None` means use [`crate::time::SystemClock`].
    clock: Option<std::sync::Arc<dyn crate::time::ClockSource>>,
    /// Injected entropy source; `None` means use [`crate::entropy::OsEntropy`].
    entropy: Option<std::sync::Arc<dyn crate::entropy::Entropy>>,
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
            job_recorder: JobRecorder::new(),
            fault_plan: None,
            #[cfg(feature = "db")]
            db_interceptor: None,
            #[cfg(feature = "ws")]
            channels_interceptor: None,
            #[cfg(feature = "ws")]
            broadcast_recorder: None,
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
            #[cfg(feature = "mail")]
            mail_suppression_store: None,
            registered_plugins: std::collections::HashSet::new(),
            extensions: std::collections::HashMap::new(),
            clock: None,
            entropy: None,
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
    /// can exercise the `/openapi.json` and `/swagger-ui` endpoints.
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
                layer: layer.erase(),
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
                layer: layer.erase(),
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
        let auth_session_key = state.auth_session_key().to_owned();
        // Resolve the session cookie name from the router's config (installed in
        // state extensions by `build()`), falling back to the framework default
        // when it isn't present — so `log_out` clears the right cookie even when
        // the app configured a custom `session.cookie_name`.
        let session_cookie_name = state.extension::<AutumnConfig>().map_or_else(
            || crate::session::SessionConfig::default().cookie_name,
            |cfg| cfg.session.cookie_name.clone(),
        );
        TestClient {
            router,
            probes: crate::probe::ProbeState::ready_for_test(),
            state,
            _job_runtime: None,
            clock_as_any: None,
            #[cfg(feature = "mail")]
            mail_recorder: None,
            #[cfg(feature = "ws")]
            broadcast_recorder: None,
            job_recorder: None,
            jobs: Vec::new(),
            cookie_jar: std::sync::Arc::new(
                std::sync::Mutex::new(std::collections::HashMap::new()),
            ),
            // `from_router` receives an already-built router, so we have no
            // handle to whatever session store (if any) it installed. The jar
            // still works — cookies from real requests round-trip — but
            // `acting_as` cannot mint a session and panics, mirroring how
            // `sent_mail()` degrades for `from_router` clients.
            session_store: None,
            session_cookie_name,
            auth_session_key,
            session_signing_keys: None,
            fault_ledger: None,
            observed_server_errors: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
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

    /// Install a designated live-state block, so handlers that read it through
    /// [`AppState::live_state`](crate::AppState::live_state) can be tested.
    ///
    /// Mirrors [`crate::app::AppBuilder::with_live_state`]. A test app never
    /// adopts a snapshot from a predecessor — there is no upgrade in flight —
    /// so `initial` is always the value handlers see.
    #[must_use]
    pub fn with_live_state<T>(mut self, initial: T) -> Self
    where
        T: crate::upgrade::LiveState,
    {
        self.state_initializers.push(Box::new(move |state| {
            assert!(
                state
                    .extension::<crate::upgrade::LiveStateRegistry>()
                    .is_none(),
                "an app may designate only one block of live state; the real builder \
                 refuses a second one at startup, so a test app does too"
            );
            let handle = crate::upgrade::LiveStateHandle::new(initial);
            state.insert_extension(crate::upgrade::LiveStateRegistry::new(&handle));
            state.insert_extension(handle);
        }));
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

    /// Mirrors [`crate::app::AppBuilder::with_notification_store`].
    #[must_use]
    pub fn with_notification_store<S>(mut self, store: S) -> Self
    where
        S: crate::notifications::NotificationStore,
    {
        let service = crate::notifications::Notifications::new(store);
        self.state_initializers.push(Box::new(move |state| {
            state.insert_extension(service);
        }));
        self
    }

    /// Mirrors
    /// [`crate::app::AppBuilder::with_push_subscription_store`].
    #[must_use]
    pub fn with_push_subscription_store<S>(mut self, store: S) -> Self
    where
        S: crate::push::PushSubscriptionStore,
    {
        self.state_initializers.push(Box::new(move |state| {
            state.insert_extension(crate::push::WebPush::from_state_with_store(state, store));
        }));
        self
    }

    /// Register an explicit [`WebPush`](crate::push::WebPush) service,
    /// overriding key, store and transport at once.
    ///
    /// The usual reason is a
    /// [`RecordingPushTransport`](crate::push::RecordingPushTransport), so a
    /// test can assert exactly what would have gone to the push service.
    #[must_use]
    pub fn with_web_push(mut self, push: crate::push::WebPush) -> Self {
        self.state_initializers.push(Box::new(move |state| {
            state.insert_extension(push);
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

        // Carry a plugin-registered bounce/complaint suppression store (issue
        // #1247) into the test app so send-time suppression is consulted under
        // TestApp exactly as under AppBuilder::run — otherwise a plugin/app that
        // wired a PgSuppressionStore would silently test against the in-memory
        // default and hide production failures (e.g. a missing table).
        #[cfg(feature = "mail")]
        if let Some(handle) = app_builder.mail_suppression_store {
            self.mail_suppression_store = Some(handle);
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

    /// Register a bounce/complaint
    /// [`SuppressionStore`](crate::mail::suppression::SuppressionStore) so
    /// [`Mailer::send`](crate::mail::Mailer::send) skips hard-bounced/complained
    /// addresses under `TestApp` exactly as it does under `AppBuilder::run`.
    /// Mirrors
    /// [`AppBuilder::with_mail_suppression_store`](crate::app::AppBuilder::with_mail_suppression_store).
    #[cfg(feature = "mail")]
    #[must_use]
    pub fn with_mail_suppression_store(
        mut self,
        store: impl crate::mail::suppression::SuppressionStore + 'static,
    ) -> Self {
        self.mail_suppression_store =
            Some(crate::mail::suppression::SuppressionStoreHandle::new(store));
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

    /// Attach an authored, seed-deterministic fault schedule
    /// ([`FaultPlan`](crate::sim::FaultPlan), issue #1680).
    ///
    /// The plan's faults are injected through the existing
    /// [`interceptor`](crate::interceptor) seams, so no application code
    /// changes: the ordinal-th database checkout or job execution fails, exactly
    /// as a real transient failure would, and everything the run did is recorded
    /// into a serializable [`FaultOutcome`](crate::sim::FaultOutcome) reachable
    /// through [`TestClient::fault_outcome`] (or
    /// [`TestClient::fault_ledger`]).
    ///
    /// It **composes with**, and never replaces, the interceptors already in
    /// play: the always-on enqueue recorder still records, a
    /// [`with_job_interceptor`](Self::with_job_interceptor) still runs (and
    /// observes the injected error like a real handler failure), transactional
    /// database isolation is preserved, and [`Sim::chaos`](crate::sim::Sim::chaos)
    /// keeps working alongside. The fault decision is innermost of each chain.
    ///
    /// ```rust,ignore
    /// use autumn_web::sim::FaultPlan;
    ///
    /// let client = TestApp::new()
    ///     .jobs(jobs![charge_card])
    ///     .with_fault_plan(FaultPlan::from_seed(0x5EED).fail_job("charge_card", 1))
    ///     .build();
    /// ```
    ///
    /// # Determinism
    ///
    /// Attaching a plan also defaults the app's entropy source to
    /// `SeededEntropy::shared(plan.seed())` when the test supplied none, so
    /// request ids and job-retry jitter replay from the same seed. Run the
    /// scenario under [`#[sim_test]`](crate::sim_test) (a paused,
    /// single-threaded runtime with a virtual clock) for the ordinals to be
    /// reproducible; see the [`fault`](crate::sim::fault) module docs.
    ///
    /// # Panics
    ///
    /// [`build`](Self::build) panics if the config would make the fault schedule
    /// non-reproducible: more than one job worker (`jobs.workers`), or error
    /// reporting disabled / sampled below `1.0` (the sampler draws OS
    /// randomness, so a sampled-out 5xx would be missing from the outcome at
    /// random).
    #[must_use]
    pub fn with_fault_plan(mut self, plan: crate::sim::fault::FaultPlan) -> Self {
        self.fault_plan = Some(plan);
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

    /// Opt in to recording every channel broadcast published while requests
    /// run, enabling [`TestClient::broadcasts`],
    /// [`TestClient::broadcasts_on`], and the `assert_broadcast*` helpers.
    ///
    /// No interceptor is installed — and channel publishing is untouched —
    /// unless this is called. Composes with a user-supplied
    /// [`with_channels_interceptor`](Self::with_channels_interceptor): the
    /// recorder runs first, then the user's interceptor.
    #[cfg(feature = "ws")]
    #[must_use]
    pub fn record_broadcasts(mut self) -> Self {
        self.broadcast_recorder = Some(BroadcastRecorder::new());
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

    /// Inject a custom entropy source into the test app.
    ///
    /// All handlers that take a [`crate::entropy::Rng`] extractor — and every
    /// framework-minted identifier (request ids, session ids, idempotency lock
    /// owners, job ids) — draw from `entropy`. Pass a
    /// [`crate::entropy::SeededEntropy`] to make the whole app's identifier
    /// stream byte-for-byte reproducible under a fixed seed. Mirrors
    /// [`Self::with_clock`].
    ///
    /// ```rust,no_run
    /// use autumn_web::entropy::SeededEntropy;
    /// use autumn_web::test::TestApp;
    ///
    /// let _client = TestApp::new()
    ///     .with_entropy(SeededEntropy::new(0x5eed))
    ///     .build();
    /// ```
    #[must_use]
    pub fn with_entropy<E>(mut self, entropy: E) -> Self
    where
        E: crate::entropy::Entropy + 'static,
    {
        self.entropy = Some(std::sync::Arc::new(entropy));
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
    pub fn with_db(mut self, pool: Pool<crate::db::RuntimeConnection>) -> Self {
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
        // Reset the global cache to prevent cross-test contamination. Briefly
        // held so this can't land mid-flight inside another same-process
        // test's own global-cache critical section (issue #2218) — see
        // `GLOBAL_CACHE_TEST_LOCK`'s doc comment.
        {
            let _guard = crate::cache::GLOBAL_CACHE_TEST_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            crate::cache::clear_global_cache();
        }
        // Reset the global event bus so a prior test's listeners/recorder do not
        // leak into this one (it is re-installed below).
        crate::events::clear_global_event_bus();

        // An attached fault plan (issue #1680) only replays byte-for-byte when
        // the surrounding config cannot reorder or drop what it records, so the
        // two knobs that would are checked up front rather than silently
        // producing a flaky scenario.
        if let Some(plan) = self.fault_plan.as_ref() {
            assert_eq!(
                self.config.jobs.workers, 1,
                "a fault plan needs `jobs.workers = 1`: concurrent workers can swap \
                 which execution is the Nth, so the ordinals would not replay"
            );
            #[cfg(feature = "reporting")]
            assert!(
                self.config.reporting.enabled && self.config.reporting.sample_rate >= 1.0,
                "a fault plan needs `reporting.enabled = true` and \
                 `reporting.sample_rate = 1.0`: the sampler draws OS randomness, so a \
                 sampled-out 5xx would drop out of `FaultOutcome::server_errors` at random"
            );
            // Replay the app's identifier stream (request ids, job-retry jitter)
            // from the plan's seed unless the test injected its own source.
            if self.entropy.is_none() {
                self.entropy = Some(crate::entropy::SeededEntropy::shared(plan.seed()));
            }
        }

        // Postgres transactional test isolation (`begin_test_transaction` +
        // SAVEPOINT rollback on a `max_size(1)` control pool) is Postgres-only;
        // SQLite has no equivalent, so under the `sqlite` feature the harness
        // uses the configured pool directly (no per-test rollback isolation).
        #[cfg(all(feature = "db", feature = "sqlite"))]
        let (pool, replica_pool, db_interceptor) = {
            let _ = self.transactional;
            // SQLite has no equivalent of the Postgres transactional-rollback
            // isolation (`begin_test_transaction` + SAVEPOINT on a `max_size(1)`
            // control pool), so a SQLite test DB gets a real pool but NOT
            // per-test transactional isolation. Even so, `with_transactional_db`
            // records an explicit SQLite database URL, and dropping it here would
            // leave a `TestApp` built that way with no pool at all -- every route
            // using the `Db` extractor would then return 503. So when no pool was
            // attached via `with_db` but an explicit URL was given, build a plain
            // (non-transactional) SQLite pool from it, reusing the runtime
            // `create_pool` path so the pool matches production behavior.
            let pool = if let Some(pool) = self.pool {
                Some(pool)
            } else if let Some(url) = self.transactional_url.as_deref() {
                let mut db_config = self.config.database.clone();
                db_config.primary_url = Some(url.to_owned());
                Some(
                    crate::db::create_pool(&db_config)
                        .expect("failed to build SQLite test pool from with_transactional_db URL")
                        .expect(
                            "with_transactional_db URL did not yield a SQLite pool (empty URL?)",
                        ),
                )
            } else {
                None
            };
            (pool, self.replica_pool, self.db_interceptor)
        };
        #[cfg(all(feature = "db", not(feature = "sqlite")))]
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
        // Resolve the injected clock BEFORE the state literal so `started_at`
        // is stamped on the same timeline the app will read time from. A sim
        // installs a virtual clock here, and uptime has to start at that
        // clock's origin rather than at real process time.
        let clock: std::sync::Arc<dyn crate::time::ClockSource> = self
            .clock
            .unwrap_or_else(|| std::sync::Arc::new(crate::time::SystemClock));
        let started_at = clock.monotonic();

        // The fault ledger is created here, per build, from the RESOLVED clock:
        // a sim installs its virtual clock immediately before `build`, and a
        // `Sim::kill`/`restart` rebuilds, so counting restarts with the app.
        let fault_ledger = self.fault_plan.as_ref().map(|plan| {
            crate::sim::fault::FaultLedger::new(plan, std::sync::Arc::clone(&clock), started_at)
        });

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
            #[cfg(all(feature = "db", not(feature = "sqlite")))]
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
            // The transactional shard-set builder is Postgres-only (per-shard
            // `begin_test_transaction` isolation); under the `sqlite` feature the
            // harness always uses the plain builder (no shard rollback isolation).
            #[cfg(all(feature = "db", feature = "sqlite"))]
            shards: crate::sharding::create_shard_set(&self.config.database, shard_router.clone())
                .expect("test shard pools should build from config"),
            // The test harness attaches pools directly (`with_pool`), without
            // a topology to carry a capture gap; a DB test that needs the gap
            // noted asserts through the production seam instead.
            #[cfg(all(feature = "db", feature = "reporting"))]
            db_capture_gap: None,
            profile: self.config.profile.as_deref().map(std::sync::Arc::from),
            role: self.config.role,
            started_at,
            health_detailed: self.config.health.detailed,
            probes: probes.clone(),
            metrics: crate::middleware::MetricsCollector::new(),
            log_levels: crate::actuator::LogLevels::new(&self.config.log.level),
            task_registry: crate::actuator::TaskRegistry::new(),
            // Built from the resolved clock, not `JobRegistry::new()`: the queue
            // gauges compare ready-at marks the job runtime stamps from this
            // same clock. This literal bypasses `AppState::with_clock`, so
            // leaving it on the default real clock is what made a sim's delayed
            // job read as ready the instant it was enqueued.
            job_registry: crate::actuator::JobRegistry::new()
                .with_clock(std::sync::Arc::clone(&clock)),
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
            auth_session_key: std::sync::Arc::from(self.config.auth.session_key.as_str()),
            shared_cache: None,
            clock,
            entropy: self
                .entropy
                .unwrap_or_else(|| std::sync::Arc::new(crate::entropy::OsEntropy)),
            app_id: crate::state::AppState::next_app_id(),
        };

        // Mirror `App::run`'s failure-capsule clock wiring (#1598): the layer
        // itself is installed by the shared router builder, but the recording
        // clock replaces the state's clock, which the router never owns.
        #[cfg(feature = "reporting")]
        if self.config.failure_capture.enabled {
            let recording =
                std::sync::Arc::new(crate::capsule::RecordingClock::new(state.clock_arc()))
                    as std::sync::Arc<dyn crate::time::ClockSource>;
            state = state.with_clock(recording);
        }
        // Same for the entropy source (#1634): a handler that mints a session
        // id, a token or a job id must mint the *recorded* one on replay, or
        // the identifier in the capsule's SQL binds will not be the one the
        // replayed code produced.
        #[cfg(feature = "reporting")]
        if self.config.failure_capture.enabled {
            let recording =
                std::sync::Arc::new(crate::capsule::RecordingEntropy::new(state.entropy_arc()))
                    as std::sync::Arc<dyn crate::entropy::Entropy>;
            state = state.with_entropy(recording);
        }

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
        // Always install the job recorder so `enqueued_jobs`/`assert_job_*` and
        // `perform_enqueued_jobs` work with no opt-in. The recorder runs first
        // and composes with any user-supplied `with_job_interceptor` (which
        // still runs, after the recorder). A single `Arc<dyn JobInterceptor>`
        // extension is what the job runtime reads, so we chain rather than
        // install two.
        let job_recorder_for_client = {
            let recorder_for_client = self.job_recorder.clone();
            let recorder: std::sync::Arc<dyn crate::interceptor::JobInterceptor> =
                std::sync::Arc::new(self.job_recorder);
            // Chain order is recorder → user → fault plan, so an attached
            // `FaultPlan` sits INNERMOST: a user interceptor observes the
            // injected error exactly as it would a real handler failure, and
            // the recorder still sees every enqueue.
            let mut inner = self.job_interceptor;
            if let Some(ledger) = fault_ledger.as_ref() {
                let fault = ledger.job_interceptor();
                inner = Some(match inner {
                    Some(user) => std::sync::Arc::new(ChainedJobInterceptor {
                        first: user,
                        second: fault,
                    }),
                    None => fault,
                });
            }
            let effective: std::sync::Arc<dyn crate::interceptor::JobInterceptor> =
                if let Some(inner) = inner {
                    std::sync::Arc::new(ChainedJobInterceptor {
                        first: recorder,
                        second: inner,
                    })
                } else {
                    recorder
                };
            state.insert_extension(effective);
            recorder_for_client
        };
        #[cfg(feature = "db")]
        {
            // The single `Arc<dyn DbConnectionInterceptor>` extension the
            // checkout path reads. An attached `FaultPlan` WRAPS whatever was
            // already composed (the user's interceptor, transactional test
            // isolation, or both) and runs its decision innermost, forwarding
            // `is_transactional_test` so rollback isolation survives. Written
            // without `ComposedDbInterceptor`, which does not exist under the
            // `sqlite` feature.
            let db_interceptor = match fault_ledger.as_ref() {
                Some(ledger) => Some(ledger.db_interceptor(db_interceptor)),
                None => db_interceptor,
            };
            if let Some(interceptor) = db_interceptor {
                state.insert_extension(interceptor);
            }
        }
        #[cfg(feature = "ws")]
        let broadcast_recorder_for_client = {
            let mut interceptors: Vec<std::sync::Arc<dyn crate::interceptor::ChannelsInterceptor>> =
                Vec::new();

            // Recorder runs first so it observes every publish before any
            // user-supplied interceptor can short-circuit the chain.
            let recorder_for_client = self.broadcast_recorder.clone();
            if let Some(recorder) = self.broadcast_recorder {
                interceptors.push(std::sync::Arc::new(recorder));
            }
            if let Some(interceptor) = self.channels_interceptor {
                // Preserve the existing `insert_extension` behavior so the
                // user's interceptor is discoverable from state.
                state.insert_extension(interceptor.clone());
                interceptors.push(interceptor);
            }

            // AC6: install nothing (and leave production `Channels` untouched)
            // unless at least one interceptor was requested.
            if !interceptors.is_empty() {
                state.channels = crate::channels::Channels::with_shared_backend(
                    std::sync::Arc::new(crate::channels::InterceptedChannelsBackend::new(
                        state.channels.backend().clone(),
                        interceptors,
                    )),
                );
                #[cfg(feature = "presence")]
                {
                    state.presence = crate::presence::Presence::new(state.channels.clone());
                }
            }
            recorder_for_client
        };
        #[cfg(feature = "oauth2")]
        if let Some(interceptor) = self.http_interceptor {
            state.insert_extension(interceptor);
        }

        #[cfg(feature = "mail")]
        {
            if let Some(handle) = self.suppression_store.clone() {
                state.insert_extension(handle);
            }
            // Mirror AppBuilder::run: register the bounce/complaint suppression
            // handle before install_mailer so the test mailer actually consults
            // it (install_mailer reads it back via the extension).
            if let Some(handle) = self.mail_suppression_store.clone() {
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

        // Register the fault plan's 5xx projector the same way
        // `with_error_reporter` does — appended to the registered chain, so the
        // app's own reporters keep receiving every event. Must land before the
        // router is built, which is where `ReportingLayer` reads the chain.
        #[cfg(feature = "reporting")]
        if let Some(ledger) = fault_ledger.as_ref() {
            let mut reporters = state
                .extension::<crate::reporting::RegisteredReporters>()
                .map(|registered| registered.0.clone())
                .unwrap_or_default();
            reporters.push(ledger.reporter());
            state.insert_extension(crate::reporting::RegisteredReporters(reporters));
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
            crate::job::start_runtime(
                self.jobs.clone(),
                &state,
                &shutdown,
                &self.config.jobs,
                true,
            )
            .expect("Failed to start job runtime in test");
            Some(TestJobRuntime { shutdown })
        };

        // Retain the registered job metadata so `perform_enqueued_jobs` can look
        // up each captured job's handler by name and dispatch it directly.
        let jobs_for_client = self.jobs.clone();

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

        // Explicitly build the session store the router's `SessionLayer` will
        // use, so the client keeps a handle for `acting_as` to mint sessions
        // (#1359). For the default in-memory backend we install a `MemoryStore`
        // and pass it as the custom store; for other backends we leave it to
        // config-driven selection (`None`) and the client's `session_store`
        // handle stays `None`, so `acting_as` panics with a clear message.
        let session_backed_by_memory = matches!(
            self.config.session.backend,
            crate::session::SessionBackend::Memory
        );
        let test_session_store: Option<std::sync::Arc<dyn crate::session::BoxedSessionStore>> =
            if session_backed_by_memory {
                Some(std::sync::Arc::new(crate::session::MemoryStore::new()))
            } else {
                None
            };
        let session_cookie_name = self.config.session.cookie_name.clone();
        let auth_session_key = self.config.auth.session_key.clone();
        // Mirror the router's session-cookie signing decision (router.rs): only
        // thread signing keys when a secret is configured or in production.
        let session_signing_keys = {
            let is_production =
                matches!(self.config.profile.as_deref(), Some("prod" | "production"));
            if self.config.security.signing_secret.secret.is_some() || is_production {
                Some(std::sync::Arc::new(
                    crate::security::config::resolve_signing_keys(
                        &self.config.security.signing_secret,
                    ),
                ))
            } else {
                None
            }
        };

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
                session_store: test_session_store.clone(),
                #[cfg(feature = "openapi")]
                openapi: self.openapi,
                #[cfg(feature = "mcp")]
                mcp: self.mcp,
            },
        )
        .expect("failed to build test router");
        // Mirror production's two outermost fallbacks, which `apply_startup_barrier`
        // applies outside the session and exception-filter layers:
        //
        //  * access-log fallback (#999) — emits only for responses the primary
        //    in-stack layer never saw (e.g. session-store outage 503s), so tests
        //    observe the same access-log behavior an operator would;
        //  * Server-Timing fallback (#1348) — appends a `total` only for responses
        //    the primary never saw (short-circuits and the late-merged `/mcp`
        //    envelope). Without it a `tools/call` would carry no outer `total` in
        //    tests, unlike production.
        //
        // Composed into ONE `Router::layer` call, exactly as production does, so a
        // test router has the same nesting depth as the real one (issue #2193).
        // Tuple order is OUTERMOST FIRST: Server-Timing wraps the access log,
        // matching production order.
        let server_timing_fallback = crate::config::server_timing_enabled(&self.config)
            .then(|| crate::middleware::ServerTimingLayer::fallback(true));
        let access_log_fallback = self.config.log.access_log.then(|| {
            crate::middleware::AccessLogLayer::fallback(self.config.log.access_log_exclude.clone())
        });
        // Guarded, because `Router::layer` re-boxes every route even when the
        // tuple contributes no service: with both fallbacks off this would
        // otherwise add a nesting level production does not have.
        let router = if server_timing_fallback.is_some() || access_log_fallback.is_some() {
            router.layer((
                tower::util::option_layer(server_timing_fallback),
                tower::util::option_layer(access_log_fallback),
            ))
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
            #[cfg(feature = "ws")]
            broadcast_recorder: broadcast_recorder_for_client,
            job_recorder: Some(job_recorder_for_client),
            jobs: jobs_for_client,
            cookie_jar: std::sync::Arc::new(
                std::sync::Mutex::new(std::collections::HashMap::new()),
            ),
            session_store: test_session_store,
            session_cookie_name,
            auth_session_key,
            session_signing_keys,
            fault_ledger,
            observed_server_errors: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
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
    /// `Some` only when [`TestApp::record_broadcasts`] opted in; otherwise
    /// `None` (also for clients built via [`TestApp::from_router`]).
    #[cfg(feature = "ws")]
    broadcast_recorder: Option<BroadcastRecorder>,
    /// Built-in job recorder. `None` for clients built via
    /// [`TestApp::from_router`], which bypasses recorder wiring; `Some` for all
    /// clients produced by [`TestApp::build`].
    job_recorder: Option<JobRecorder>,
    /// Registered job metadata, retained so [`TestClient::perform_enqueued_jobs`]
    /// can dispatch each captured job through its handler. Empty for
    /// [`TestApp::from_router`] clients.
    jobs: Vec<crate::job::JobInfo>,
    /// Per-client cookie jar (`name → value + optional expiry`). Every
    /// response's `Set-Cookie` is folded in here, its `Max-Age`/`Expires`
    /// recorded, and it is replayed on subsequent requests until it expires
    /// against the client's clock, so a real
    /// `POST /login` → `GET /dashboard` flow works with no manual header
    /// threading. Shared with each [`RequestBuilder`] via a cloned `Arc`.
    cookie_jar: CookieJar,
    /// Handle to the session store the router's `SessionLayer` reads, so
    /// [`TestClient::acting_as`] can mint an authenticated session directly.
    /// `None` for clients built via [`TestApp::from_router`] or configured
    /// with a non-memory session backend; `acting_as` panics for those.
    session_store: Option<std::sync::Arc<dyn crate::session::BoxedSessionStore>>,
    /// Name of the session cookie (`session.cookie_name`, default
    /// `"autumn.sid"`); the cookie `acting_as` seeds and `log_out` clears.
    session_cookie_name: String,
    /// Session key the auth stack reads for identity (`auth.session_key`,
    /// default `"user_id"`); the key `acting_as` writes.
    auth_session_key: String,
    /// Session cookie signing keys when `security.signing_secret` is set (or
    /// in production), mirroring how the router signs session cookies. When
    /// present, `acting_as` signs the seeded cookie so the `SessionLayer`
    /// accepts it.
    session_signing_keys: Option<std::sync::Arc<crate::security::config::ResolvedSigningKeys>>,
    /// The runtime ledger for an attached [`crate::sim::FaultPlan`] (issue
    /// #1680); `None` when no plan was attached (and for
    /// [`TestApp::from_router`] clients).
    fault_ledger: Option<crate::sim::fault::FaultLedger>,
    /// How many 5xx responses this client has seen on its own
    /// [`RequestBuilder::send`] calls. [`TestClient::fault_outcome`] settles the
    /// detached reporter tasks against this count, so an outcome is read only
    /// once the 5xx the test actually observed have reached the ledger. Only
    /// incremented while a ledger exists.
    observed_server_errors: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

/// A cookie stored in the jar: its value plus an optional absolute expiry.
///
/// `expires_at: None` is a session cookie that never client-expires; `Some(t)`
/// records the instant (from `Max-Age`/`Expires`) past which the cookie must no
/// longer be replayed, evaluated against the client's (possibly virtual) clock.
#[derive(Clone)]
struct StoredCookie {
    value: String,
    expires_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Shared per-client cookie store: cookie name → stored cookie (value + expiry).
type CookieJar = std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, StoredCookie>>>;

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

    // ── Broadcast recorder accessors & assertions (issue #1043) ──────────

    /// Every recorded channel publication, in publish order.
    ///
    /// Requires opting in with [`TestApp::record_broadcasts`]. Captures both
    /// raw `publish` text and `publish_html` HTML/OOB payloads.
    ///
    /// # Panics
    ///
    /// Panics if [`TestApp::record_broadcasts`] was not called.
    #[cfg(feature = "ws")]
    #[must_use]
    pub fn broadcasts(&self) -> Vec<RecordedBroadcast> {
        self.broadcast_recorder
            .as_ref()
            .expect(
                "broadcasts() requires opting in via TestApp::record_broadcasts() before build()",
            )
            .recorded()
    }

    /// Recorded publications on `topic`, in publish order.
    ///
    /// # Panics
    ///
    /// Panics if [`TestApp::record_broadcasts`] was not called.
    #[cfg(feature = "ws")]
    #[must_use]
    pub fn broadcasts_on(&self, topic: &str) -> Vec<RecordedBroadcast> {
        self.broadcasts()
            .into_iter()
            .filter(|b| b.topic == topic)
            .collect()
    }

    /// Builds a self-diagnosing failure message listing what was actually
    /// published to `topic` and, grouped, to every other topic.
    #[cfg(feature = "ws")]
    fn broadcast_failure_message(&self, topic: &str, headline: &str) -> String {
        use std::collections::BTreeMap;
        use std::fmt::Write as _;
        let all = self.broadcasts();
        let on_topic: Vec<&str> = all
            .iter()
            .filter(|b| b.topic == topic)
            .map(|b| b.payload.as_str())
            .collect();

        let mut others: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for b in &all {
            if b.topic != topic {
                others
                    .entry(b.topic.as_str())
                    .or_default()
                    .push(b.payload.as_str());
            }
        }

        let mut msg = format!("{headline}\n");
        let _ = writeln!(
            msg,
            "published to {topic:?} ({} total): {on_topic:#?}",
            on_topic.len(),
        );
        if others.is_empty() {
            msg.push_str("no publications on any other topic");
        } else {
            let _ = write!(msg, "other topics published: {others:#?}");
        }
        msg
    }

    /// Asserts that at least one publication on `topic` satisfies `predicate`.
    ///
    /// Returns `&Self` for chaining.
    ///
    /// # Panics
    ///
    /// Panics when no matching publication is found, dumping what *was*
    /// published to `topic` and nearby topics.
    #[cfg(feature = "ws")]
    pub fn assert_broadcast(
        &self,
        topic: &str,
        predicate: impl Fn(&RecordedBroadcast) -> bool,
    ) -> &Self {
        let matched = self.broadcasts_on(topic).iter().any(predicate);
        assert!(
            matched,
            "{}",
            self.broadcast_failure_message(
                topic,
                &format!("no broadcast on {topic:?} matched the predicate;"),
            )
        );
        self
    }

    /// Asserts that exactly `n` publications were made to `topic`.
    ///
    /// Returns `&Self` for chaining.
    ///
    /// # Panics
    ///
    /// Panics when the count does not match, dumping what *was* published to
    /// `topic` and nearby topics.
    #[cfg(feature = "ws")]
    pub fn assert_broadcast_count(&self, topic: &str, n: usize) -> &Self {
        let count = self.broadcasts_on(topic).len();
        assert!(
            count == n,
            "{}",
            self.broadcast_failure_message(
                topic,
                &format!("expected {n} broadcast(s) on {topic:?}, got {count};"),
            )
        );
        self
    }

    /// Asserts that nothing was published to `topic`.
    ///
    /// Returns `&Self` for chaining.
    ///
    /// # Panics
    ///
    /// Panics when any publication was made to `topic`, dumping what *was*
    /// published to `topic` and nearby topics.
    #[cfg(feature = "ws")]
    pub fn assert_no_broadcasts(&self, topic: &str) -> &Self {
        self.assert_broadcast_count(topic, 0)
    }

    // ── Background-job recorder ────────────────────────────────────

    /// Every background-job enqueue captured by the built-in recorder, in the
    /// order they were enqueued (across `enqueue`, `enqueue_after_commit`, and
    /// `enqueue_in_tx`).
    ///
    /// The recorder is always on for [`TestApp::build`] clients — no opt-in.
    ///
    /// # Panics
    ///
    /// Panics if called on a [`TestClient`] built via [`TestApp::from_router`],
    /// which bypasses recorder wiring.
    #[must_use]
    pub fn enqueued_jobs(&self) -> Vec<RecordedJob> {
        self.job_recorder
            .as_ref()
            .expect(
                "enqueued_jobs() is not available on a TestClient built via from_router(); use TestApp::new().merge(router).build() instead",
            )
            .recorded()
    }

    /// Assert at least one job with the given registered `name` was enqueued.
    ///
    /// # Panics
    ///
    /// Panics, listing every job that *was* enqueued, if no enqueue with that
    /// name was captured.
    pub fn assert_job_enqueued(&self, name: &str) -> &Self {
        let jobs = self.enqueued_jobs();
        assert!(
            jobs.iter().any(|j| j.name == name),
            "expected a job named '{name}' to have been enqueued, but it was not.\nEnqueued jobs:\n{}",
            format_recorded_jobs(&jobs)
        );
        self
    }

    /// Assert at least one job was enqueued with **both** the given registered
    /// `name` and an exactly-equal JSON `payload`.
    ///
    /// # Panics
    ///
    /// Panics, listing every job that *was* enqueued, if no enqueue matched
    /// both the name and payload.
    // Takes the payload by value for call-site ergonomics — `json!({..})`
    // reads cleanly without a leading `&`, mirroring the acceptance criteria.
    #[allow(clippy::needless_pass_by_value)]
    pub fn assert_job_enqueued_with(&self, name: &str, payload: serde_json::Value) -> &Self {
        let jobs = self.enqueued_jobs();
        // Strip the opt-in schema-version envelope (issue #1205) so payload
        // assertions stay on clean args even for `#[job(version = N)]` jobs
        // whose stored payload is wrapped as `{__autumn_schema_version, args}`.
        assert!(
            jobs.iter().any(|j| j.name == name
                && *crate::payload_version::split_version(&j.payload).1 == payload),
            "expected a job named '{name}' enqueued with payload {payload}, but no match was found.\nEnqueued jobs:\n{}",
            format_recorded_jobs(&jobs)
        );
        self
    }

    /// Assert no jobs were enqueued at all.
    ///
    /// # Panics
    ///
    /// Panics, listing every captured enqueue, if any job was enqueued.
    pub fn assert_no_jobs_enqueued(&self) -> &Self {
        let jobs = self.enqueued_jobs();
        assert!(
            jobs.is_empty(),
            "expected no jobs to have been enqueued, but {} were:\n{}",
            jobs.len(),
            format_recorded_jobs(&jobs)
        );
        self
    }

    /// Drain every captured job and dispatch it through its registered handler,
    /// awaiting each in enqueue order, so a test can assert the resulting side
    /// effects synchronously.
    ///
    /// Each captured payload is handed to the same handler the runtime would
    /// invoke, so the real deserialization path runs: a payload that cannot be
    /// deserialized into the job's args surfaces as a per-job failure (not a
    /// silent miss). The queue is emptied — a second call performs nothing
    /// until more jobs are enqueued.
    ///
    /// Returns a [`PerformedJobs`] report carrying each job's `(name, result)`;
    /// per-job handler errors (and captured jobs with no registered handler)
    /// are surfaced there rather than swallowed. See
    /// [`PerformedJobs::assert_all_succeeded`].
    ///
    /// # Note
    ///
    /// [`TestApp::build`] starts the in-process job worker by default, and that
    /// worker *also* drains and runs the same enqueued jobs. Calling this method
    /// therefore executes a job's side effect an **additional** time, on top of
    /// the worker's own run. It is primarily for asserting that a job runs to
    /// completion — surfacing handler/deserialization errors synchronously — not
    /// for counting side effects. Any assertion on a side effect's *count* must
    /// account for the worker's run as well (as the job-recorder integration
    /// tests do: they settle the worker's run first, then attribute the next
    /// increment to this call).
    ///
    /// The helper invokes each job's registered handler directly and does *not*
    /// run it through a user-installed
    /// [`JobInterceptor::intercept_execute`](crate::interceptor::JobInterceptor::intercept_execute),
    /// so
    /// execution-interceptor effects (context injection, metrics, error
    /// injection) are exercised by the in-process worker path, not by this
    /// helper.
    ///
    /// # Panics
    ///
    /// Panics if called on a [`TestClient`] built via [`TestApp::from_router`].
    pub async fn perform_enqueued_jobs(&self) -> PerformedJobs {
        let recorder = self.job_recorder.as_ref().expect(
            "perform_enqueued_jobs() is not available on a TestClient built via from_router(); use TestApp::new().merge(router).build() instead",
        );
        let drained = recorder.drain();
        let mut outcomes = Vec::with_capacity(drained.len());
        for job in drained {
            let handler = self
                .jobs
                .iter()
                .find(|info| info.name == job.name)
                .map(|info| info.handler);
            let result = match handler {
                Some(handler) => (handler)(self.state.clone(), job.payload).await,
                None => Err(crate::AutumnError::internal_server_error(
                    std::io::Error::other(format!(
                        "no registered handler for enqueued job '{}'; register it via AppBuilder::jobs()",
                        job.name
                    )),
                )),
            };
            outcomes.push((job.name, result));
        }
        PerformedJobs { outcomes }
    }

    /// The app's configured N+1 detection threshold
    /// (`dev.inspector_n_plus_one_threshold`), threaded into every
    /// [`RequestBuilder`] so the resulting [`TestResponse`] can default
    /// [`TestResponse::assert_no_n_plus_one`] to it.
    ///
    /// Reads through [`AppState::config_arc`]: this runs on every
    /// `TestClient` request, so a [`AppState::config`] deep clone here would
    /// tax the whole suite — and the committed `request_pipeline` benchmark
    /// (issue #2198).
    fn n_plus_one_threshold(&self) -> usize {
        self.state.config_arc().dev.inspector_n_plus_one_threshold
    }

    /// The shared 5xx counter handed to each [`RequestBuilder`], or `None` when
    /// no [`crate::sim::FaultPlan`] is attached (so an ordinary test app never
    /// touches an atomic per request).
    fn fault_error_counter(&self) -> Option<std::sync::Arc<std::sync::atomic::AtomicU64>> {
        self.fault_ledger
            .as_ref()
            .map(|_| std::sync::Arc::clone(&self.observed_server_errors))
    }

    /// The runtime ledger for the [`crate::sim::FaultPlan`] attached with
    /// [`TestApp::with_fault_plan`], or `None` when none was attached.
    ///
    /// The handle is cheap to clone and shares the underlying ledger, so it can
    /// be read mid-run. For a scenario that drove HTTP requests, prefer
    /// [`fault_outcome`](Self::fault_outcome), which settles the detached
    /// reporter tasks first.
    #[must_use]
    pub fn fault_ledger(&self) -> Option<crate::sim::fault::FaultLedger> {
        self.fault_ledger.clone()
    }

    /// Settle the reporting lane, then snapshot the
    /// [`FaultOutcome`](crate::sim::FaultOutcome) for this run.
    ///
    /// [`reporting`](crate::reporting) dispatches on a **detached** task, so a
    /// 5xx this client already saw on the wire may not have reached the ledger
    /// yet. This yields cooperatively (never sleeping and never advancing the
    /// virtual clock, which would corrupt a sim's timeline) until the ledger has
    /// recorded at least as many server errors as this client observed, up to a
    /// bounded number of yields — then snapshots regardless, so a 5xx the
    /// reporting layer never sees can only cost a bounded spin, not a hang.
    ///
    /// # Panics
    ///
    /// Panics if no [`crate::sim::FaultPlan`] was attached with
    /// [`TestApp::with_fault_plan`].
    pub async fn fault_outcome(&self) -> crate::sim::fault::FaultOutcome {
        /// Cooperative yields spent waiting for the detached reporter tasks.
        const MAX_SETTLE_YIELDS: usize = 10_000;

        let ledger = self.fault_ledger.as_ref().expect(
            "no fault plan attached to this TestApp; call `TestApp::with_fault_plan(..)` before `build()`",
        );
        // Fully-qualified: a `diesel_async::RunQueryDsl` glob import in this
        // module also offers a `.load(..)` method by that name.
        let observed = usize::try_from(std::sync::atomic::AtomicU64::load(
            &self.observed_server_errors,
            std::sync::atomic::Ordering::SeqCst,
        ))
        .unwrap_or(usize::MAX);
        for _ in 0..MAX_SETTLE_YIELDS {
            if ledger.server_errors_len() >= observed {
                break;
            }
            tokio::task::yield_now().await;
        }
        ledger.outcome()
    }

    /// Start building a GET request.
    #[must_use]
    pub fn get(&self, uri: &str) -> RequestBuilder {
        RequestBuilder::new(
            self.router.clone(),
            Method::GET,
            uri,
            self.cookie_jar.clone(),
            Some(self.state.clock.clone()),
            self.n_plus_one_threshold(),
            self.fault_error_counter(),
        )
    }

    /// Start building a POST request.
    #[must_use]
    pub fn post(&self, uri: &str) -> RequestBuilder {
        RequestBuilder::new(
            self.router.clone(),
            Method::POST,
            uri,
            self.cookie_jar.clone(),
            Some(self.state.clock.clone()),
            self.n_plus_one_threshold(),
            self.fault_error_counter(),
        )
    }

    /// Start building a PUT request.
    #[must_use]
    pub fn put(&self, uri: &str) -> RequestBuilder {
        RequestBuilder::new(
            self.router.clone(),
            Method::PUT,
            uri,
            self.cookie_jar.clone(),
            Some(self.state.clock.clone()),
            self.n_plus_one_threshold(),
            self.fault_error_counter(),
        )
    }

    /// Start building a DELETE request.
    #[must_use]
    pub fn delete(&self, uri: &str) -> RequestBuilder {
        RequestBuilder::new(
            self.router.clone(),
            Method::DELETE,
            uri,
            self.cookie_jar.clone(),
            Some(self.state.clock.clone()),
            self.n_plus_one_threshold(),
            self.fault_error_counter(),
        )
    }

    /// Start building a PATCH request.
    #[must_use]
    pub fn patch(&self, uri: &str) -> RequestBuilder {
        RequestBuilder::new(
            self.router.clone(),
            Method::PATCH,
            uri,
            self.cookie_jar.clone(),
            Some(self.state.clock.clone()),
            self.n_plus_one_threshold(),
            self.fault_error_counter(),
        )
    }

    /// Start building an OPTIONS request (e.g. a CORS preflight).
    #[must_use]
    pub fn options(&self, uri: &str) -> RequestBuilder {
        RequestBuilder::new(
            self.router.clone(),
            Method::OPTIONS,
            uri,
            self.cookie_jar.clone(),
            Some(self.state.clock.clone()),
            self.n_plus_one_threshold(),
            self.fault_error_counter(),
        )
    }

    // ── Authentication helpers (#1359) ─────────────────────────

    /// Establish an authenticated session for `user_id` *without* calling the
    /// login endpoint, then return `&Self` for chaining.
    ///
    /// Mints a fresh session containing the app's configured
    /// `auth.session_key` (default `"user_id"`) set to `user_id`, saves it to
    /// the session store the router reads, and seeds the cookie jar with the
    /// session cookie. A subsequent request to a `#[secured]` / [`Auth`](crate::auth::Auth)-gated
    /// route then extracts the same identity a real login would produce.
    ///
    /// This sets **identity only** — authorization still runs. A user acted-as
    /// here who lacks a required role or scope is still denied.
    ///
    /// Analogous to Laravel's `actingAs`, Rails' `sign_in`, Django's
    /// `force_login`, and Phoenix's `log_in_user`.
    ///
    /// # Panics
    ///
    /// Panics if the client has no handle to a session store — i.e. it was
    /// built via [`TestApp::from_router`], or configured with a non-memory
    /// session backend. Use [`TestApp::build`] with the default (memory)
    /// session backend for `acting_as` support.
    ///
    /// # Examples
    ///
    /// ```rust,ignore
    /// let client = TestApp::new().routes(routes![dashboard]).build();
    /// client.acting_as(42).await;
    /// client.get("/dashboard").send().await.assert_ok();
    /// ```
    pub async fn acting_as(&self, user_id: impl std::fmt::Display) -> &Self {
        let store = self.session_store.as_ref().unwrap_or_else(|| {
            panic!(
                "acting_as requires a session store handle, which is only available on clients \
                 built via `TestApp::build()` with the default in-memory session backend. \
                 Clients from `TestApp::from_router` or configured with a non-memory backend \
                 cannot mint sessions this way."
            )
        });

        let session_id = uuid::Uuid::new_v4().to_string();
        let mut data = std::collections::HashMap::new();
        data.insert(self.auth_session_key.clone(), user_id.to_string());
        store
            .boxed_save(&session_id, data)
            .await
            .expect("failed to save acting_as session to the test session store");

        // Match the router's cookie encoding: sign the id when signing keys
        // are active, otherwise store the raw id.
        let cookie_value = self.session_signing_keys.as_ref().map_or_else(
            || session_id.clone(),
            |keys| format!("{session_id}.{}", keys.sign(session_id.as_bytes())),
        );
        self.cookie_jar
            .lock()
            .expect("cookie jar mutex poisoned")
            .insert(
                self.session_cookie_name.clone(),
                StoredCookie {
                    value: cookie_value,
                    expires_at: None,
                },
            );

        self
    }

    /// Alias for [`acting_as`](Self::acting_as).
    ///
    /// Provided for readers coming from frameworks whose helper is spelled
    /// `login_as` / `sign_in`.
    ///
    /// # Panics
    ///
    /// See [`acting_as`](Self::acting_as).
    pub async fn login_as(&self, user_id: impl std::fmt::Display) -> &Self {
        self.acting_as(user_id).await
    }

    /// Clear the session cookie from the jar, reverting the client to an
    /// unauthenticated state, then return `&Self` for chaining.
    ///
    /// After `log_out`, a request to a secured route returns its
    /// unauthenticated status (401 / redirect) again. The corresponding
    /// server-side session (if any) is left to expire naturally.
    ///
    /// Analogous to Laravel's `Auth::logout`, Rails' `sign_out`, and Django's
    /// `logout`.
    pub fn log_out(&self) -> &Self {
        self.cookie_jar
            .lock()
            .expect("cookie jar mutex poisoned")
            .remove(&self.session_cookie_name);
        self
    }
}

/// Fold a single `Set-Cookie` header value into the cookie jar.
///
/// Stores `name=value` along with any absolute expiry parsed from the header's
/// `Max-Age`/`Expires` attributes (so a live cookie stops being replayed once
/// the clock passes it), or removes the cookie when the header marks it for
/// immediate deletion (`Max-Age=0`, a non-positive `Max-Age`, or an `Expires`
/// in the past — the encodings the session layer and CSRF layer use to clear
/// cookies).
///
/// When both `Max-Age` and `Expires` are present, `Max-Age` wins (per
/// RFC 6265). A live cookie with no expiry attributes is stored as a session
/// cookie (`expires_at: None`) that never client-expires.
///
/// `now` is the reference instant for evaluating `Max-Age`/`Expires`; callers
/// pass the framework's (possibly virtual) clock so a test that pins or
/// advances time sees deterministic expiry.
fn apply_set_cookie(
    jar: &mut std::collections::HashMap<String, StoredCookie>,
    header: &str,
    now: chrono::DateTime<chrono::Utc>,
) {
    let mut parts = header.split(';');
    let Some(pair) = parts.next() else {
        return;
    };
    let Some((name, value)) = pair.split_once('=') else {
        return;
    };
    let name = name.trim();
    let value = value.trim();
    if name.is_empty() {
        return;
    }

    // The jar reliably recognizes Autumn's own cookie-clear encodings: an empty
    // value (handled below) and a non-positive `Max-Age`, which the session and
    // CSRF layers use to delete cookies. Third-party `Expires`-based deletions
    // are only best-effort — an RFC 2822 timestamp in the past is honored, but
    // other date encodings a foreign server might send are not fully parsed.
    let mut deletes = false;
    // Absolute expiry parsed from a positive `Max-Age`/future `Expires`. When
    // both are present, `Max-Age` wins (RFC 6265), so track them separately and
    // resolve at the end.
    let mut max_age_expiry: Option<chrono::DateTime<chrono::Utc>> = None;
    let mut expires_expiry: Option<chrono::DateTime<chrono::Utc>> = None;
    let mut saw_max_age = false;
    for attr in parts {
        let attr = attr.trim();
        // Both `Max-Age=` and `Expires=` are 8-byte ASCII prefixes; match them
        // case-insensitively (`EXPIRES=` etc. are equally valid per RFC 6265).
        let prefix = attr.get(..8);
        if prefix.is_some_and(|p| p.eq_ignore_ascii_case("Max-Age=")) {
            if let Ok(secs) = attr[8..].trim().parse::<i64>() {
                saw_max_age = true;
                if secs <= 0 {
                    deletes = true;
                } else {
                    max_age_expiry = now.checked_add_signed(chrono::Duration::seconds(secs));
                }
            }
        } else if prefix.is_some_and(|p| p.eq_ignore_ascii_case("Expires="))
            && let Ok(when) = chrono::DateTime::parse_from_rfc2822(attr[8..].trim())
        {
            let when = when.with_timezone(&chrono::Utc);
            if when <= now {
                deletes = true;
            } else {
                expires_expiry = Some(when);
            }
        }
    }

    if deletes || value.is_empty() {
        jar.remove(name);
    } else {
        // `Max-Age` takes precedence over `Expires` when both are present.
        let expires_at = if saw_max_age {
            max_age_expiry
        } else {
            expires_expiry
        };
        jar.insert(
            name.to_owned(),
            StoredCookie {
                value: value.to_owned(),
                expires_at,
            },
        );
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
    /// Shared with the originating [`TestClient`]: cookies are read from here
    /// to compose the request `Cookie` header, and `Set-Cookie` from the
    /// response is folded back in. `None` when the builder was constructed
    /// without a client (not reachable through the public API today).
    cookie_jar: Option<CookieJar>,
    /// The originating client's clock, used to evaluate `Expires` when folding
    /// `Set-Cookie` back into the jar. `None` falls back to [`chrono::Utc::now`].
    clock: Option<std::sync::Arc<dyn crate::time::ClockSource>>,
    /// Default N+1 detection threshold (`dev.inspector_n_plus_one_threshold`),
    /// propagated to the resulting [`TestResponse`] so
    /// [`TestResponse::assert_no_n_plus_one`] can honour the app's config.
    n_plus_one_threshold: usize,
    /// Shared with the originating [`TestClient`] when a
    /// [`crate::sim::FaultPlan`] is attached: every 5xx this request produces is
    /// counted here, and [`TestClient::fault_outcome`] settles the detached
    /// reporter tasks against that count. `None` when no plan is attached, so a
    /// plain test app pays nothing.
    observed_server_errors: Option<std::sync::Arc<std::sync::atomic::AtomicU64>>,
}

impl RequestBuilder {
    fn new(
        router: axum::Router,
        method: Method,
        uri: &str,
        cookie_jar: CookieJar,
        clock: Option<std::sync::Arc<dyn crate::time::ClockSource>>,
        n_plus_one_threshold: usize,
        observed_server_errors: Option<std::sync::Arc<std::sync::atomic::AtomicU64>>,
    ) -> Self {
        Self {
            router,
            method,
            uri: uri.to_owned(),
            headers: Vec::new(),
            body: Body::empty(),
            cookie_jar: Some(cookie_jar),
            clock,
            n_plus_one_threshold,
            observed_server_errors,
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
        // Captured for failure messages and the N+1 default threshold on the
        // resulting `TestResponse`.
        let request_method = self.method.to_string();
        let request_path = self.uri.clone();
        let n_plus_one_threshold = self.n_plus_one_threshold;
        // Cloned up front: `self` is partially moved below (the router and body
        // are consumed building the request).
        let observed_server_errors = self.observed_server_errors.clone();

        let mut builder = Request::builder().method(self.method).uri(&self.uri);

        // Replay the cookie jar: compose a `Cookie` header from stored cookies
        // unless the caller already set one explicitly (an explicit header
        // wins, so tests can still exercise raw cookie behavior).
        let caller_set_cookie = self
            .headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("cookie"));
        if !caller_set_cookie && let Some(jar) = &self.cookie_jar {
            // Evaluate expiry against the same clock the jar folds `Set-Cookie`
            // with, so a virtual-clock test sees cookies stop replaying once it
            // advances past their `Max-Age`/`Expires`. Prune expired entries in
            // passing so they don't linger.
            let now = self
                .clock
                .as_ref()
                .map_or_else(chrono::Utc::now, |c| c.now());
            let cookie_header = {
                let mut jar = jar.lock().expect("cookie jar mutex poisoned");
                jar.retain(|_, cookie| cookie.expires_at.is_none_or(|t| t > now));
                jar.iter()
                    .map(|(name, cookie)| format!("{name}={}", cookie.value))
                    .collect::<Vec<_>>()
                    .join("; ")
            };
            if !cookie_header.is_empty() {
                builder = builder.header(http::header::COOKIE, cookie_header);
            }
        }

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

        // Drive the request under a per-request `REQUEST_QUERY_CAPTURE` scope so
        // the connection-level `RequestQueryTimer` (installed at `Db::checkout`
        // whenever this capture lane is active) records every SQL statement the
        // handler issues into the capture sink — no manual `DbInterceptor`
        // wiring required. This lane is independent of the `Server-Timing`
        // timing accumulator (`REQUEST_DB_TIMINGS`), so query capture is
        // unaffected by however `ServerTimingLayer` scopes (and nests) its
        // per-scope DB metrics. `oneshot` runs on this same task, so the
        // task-local is visible to the checkout. When the `db` feature is off
        // there is no DB, so the captured query list is simply empty.
        //
        // The response body is drained (`to_bytes`) *inside* the scope so that
        // handlers returning a lazy or streaming body (`Sse`, `Body::from_stream`,
        // …) which perform DB work when the stream is polled still record those
        // body-time checkouts into the capture sink. The sink is read only after
        // the body is fully collected, so nothing is missed.
        #[cfg(feature = "db")]
        let (status, headers, body_bytes, queries) = {
            let capture = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let (status, headers, body_bytes) = crate::db::REQUEST_QUERY_CAPTURE
                .scope(std::sync::Arc::clone(&capture), async move {
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
                    (status, headers, body_bytes)
                })
                .await;
            let queries = capture.lock().map(|v| v.clone()).unwrap_or_default();
            (status, headers, body_bytes, queries)
        };
        #[cfg(not(feature = "db"))]
        let (status, headers, body_bytes, queries): (
            _,
            _,
            _,
            Vec<crate::inspector::QueryRecord>,
        ) = {
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
            (status, headers, body_bytes, Vec::new())
        };

        // Fold every `Set-Cookie` from the response back into the jar so the
        // next request from the same client replays it. Cookies whose
        // attributes mark them for deletion (`Max-Age=0` or a past `Expires`)
        // are removed instead of stored.
        if let Some(jar) = &self.cookie_jar {
            let now = self
                .clock
                .as_ref()
                .map_or_else(chrono::Utc::now, |c| c.now());
            let mut jar = jar.lock().expect("cookie jar mutex poisoned");
            for (name, value) in &headers {
                if name.eq_ignore_ascii_case("set-cookie") {
                    apply_set_cookie(&mut jar, value, now);
                }
            }
        }

        // Count the 5xx an attached fault plan will want to see reflected in
        // `FaultOutcome::server_errors`; `fault_outcome()` settles against it.
        if status.is_server_error()
            && let Some(counter) = observed_server_errors.as_ref()
        {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }

        TestResponse {
            status,
            headers,
            body: body_bytes.to_vec(),
            queries,
            request_method,
            request_path,
            n_plus_one_threshold,
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
/// The `status`, `headers`, and `body` fields are public so you can construct a
/// `TestResponse` directly in unit tests that don't need a full HTTP
/// round-trip. Fill the remaining (query-capture) fields with
/// `..Default::default()`:
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
///     ..Default::default()
/// };
///
/// resp.assert_ok()
///     .assert_header_contains("content-type", "json")
///     .assert_body_contains("Alice");
///
/// assert_eq!(resp.header("x-request-id"), Some("abc-123"));
/// ```
///
/// # Query-count and N+1 assertions
///
/// When the response was produced by [`RequestBuilder::send`] against a
/// database-backed app, every SQL statement the handler issued is captured
/// automatically (no manual interceptor wiring). Assert on it with
/// [`TestResponse::query_count`], [`TestResponse::assert_max_queries`], and
/// [`TestResponse::assert_no_n_plus_one`]:
///
/// ```rust,no_run
/// # async fn ex(client: autumn_web::test::TestClient) {
/// client.get("/posts").send().await
///     .assert_ok()
///     .assert_max_queries(3)   // fails, naming GET /posts, if > 3 queries ran
///     .assert_no_n_plus_one(); // fails if a query repeats >= the dev threshold
/// # }
/// ```
pub struct TestResponse {
    /// HTTP status code.
    pub status: StatusCode,
    /// Response headers as `(name, value)` pairs.
    pub headers: Vec<(String, String)>,
    /// Raw response body bytes.
    pub body: Vec<u8>,
    /// SQL queries captured while handling the request, in execution order.
    ///
    /// Populated automatically by [`RequestBuilder::send`] for
    /// database-backed apps; empty for directly-constructed responses or when
    /// the `db` feature is disabled. Prefer the [`TestResponse::queries`]
    /// accessor for reading.
    pub queries: Vec<crate::inspector::QueryRecord>,
    /// HTTP method of the originating request, for assertion failure messages.
    pub request_method: String,
    /// Path of the originating request, for assertion failure messages.
    pub request_path: String,
    /// Default N+1 threshold (`dev.inspector_n_plus_one_threshold`) used by
    /// [`TestResponse::assert_no_n_plus_one`].
    pub n_plus_one_threshold: usize,
}

impl Default for TestResponse {
    fn default() -> Self {
        Self {
            status: StatusCode::OK,
            headers: Vec::new(),
            body: Vec::new(),
            queries: Vec::new(),
            request_method: String::new(),
            request_path: String::new(),
            // Inherit the detector's default (5) — not a zero-filled `0`, which
            // `inspector::detect_n_plus_one` treats as DISABLED — so the
            // documented `TestResponse { .. ..Default::default() }` construction
            // still catches N+1 patterns.
            n_plus_one_threshold: crate::inspector::DEFAULT_N_PLUS_ONE_THRESHOLD,
        }
    }
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

    /// Assert a rendered PDF response's extracted text contains the given
    /// substring — e.g. `resp.assert_pdf_contains("Total: $42.00")`.
    ///
    /// Extracts text via [`crate::pdf::extract_text`], which reads back
    /// exactly what [`Pdf`](crate::pdf::Pdf) (or any well-formed PDF) wrote,
    /// so this works against the in-process test client with no headless
    /// browser involved.
    ///
    /// # Panics
    ///
    /// Panics if the body isn't a parseable PDF, or doesn't contain
    /// `substring`.
    #[cfg(feature = "pdf")]
    #[track_caller]
    pub fn assert_pdf_contains(&self, substring: &str) -> &Self {
        let text = crate::pdf::extract_text(&self.body).unwrap_or_else(|e| {
            panic!(
                "response body is not a parseable PDF: {e}\n{} bytes",
                self.body.len()
            )
        });
        assert!(
            text.contains(substring),
            "expected PDF text to contain `{substring}`.\nExtracted text: {text}"
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

    // ── Database query assertions (#1262) ──────────────────────

    /// Number of SQL queries the request issued.
    ///
    /// Captured automatically by [`RequestBuilder::send`] for database-backed
    /// apps; `0` for directly-constructed responses or when the `db` feature
    /// is disabled.
    #[must_use]
    pub const fn query_count(&self) -> usize {
        self.queries.len()
    }

    /// The SQL queries the request issued, in execution order.
    ///
    /// Lets a test assert on specific normalized SQL. Empty for
    /// directly-constructed responses or when the `db` feature is disabled.
    #[must_use]
    pub fn queries(&self) -> &[crate::inspector::QueryRecord] {
        &self.queries
    }

    /// A per-query listing for assertion failure messages: one line per query
    /// (`#N  <elapsed>ms  <sql>`), followed by repetition counts per
    /// normalized statement so the offending pattern is obvious.
    fn query_report(&self) -> String {
        use std::collections::BTreeMap;
        use std::fmt::Write as _;
        let mut out = String::new();
        for (i, q) in self.queries.iter().enumerate() {
            let _ = write!(
                out,
                "\n  #{n:<3} {ms:>4}ms  {sql}",
                n = i + 1,
                ms = q.elapsed_ms,
                sql = q.sql,
            );
        }
        // Counts per normalized statement (stable, sorted for determinism).
        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        for q in &self.queries {
            *counts
                .entry(q.sql.split_whitespace().collect::<Vec<_>>().join(" "))
                .or_insert(0) += 1;
        }
        if counts.len() != self.queries.len() {
            out.push_str("\n  ── counts per statement ──");
            for (sql, count) in &counts {
                let _ = write!(out, "\n  {count}x  {sql}");
            }
        }
        out
    }

    /// Assert the request issued at most `n` SQL queries.
    ///
    /// Passes when `query_count() <= n`. Panics otherwise with a message
    /// naming the request (method + path), the expected and actual counts, and
    /// the full query list.
    #[track_caller]
    pub fn assert_max_queries(&self, n: usize) -> &Self {
        let actual = self.queries.len();
        assert!(
            actual <= n,
            "assert_max_queries failed for {method} {path}: expected <= {n} queries, issued {actual}.{report}",
            method = self.request_method,
            path = self.request_path,
            report = self.query_report(),
        );
        self
    }

    /// Assert the request contains no N+1 query pattern, using the app's
    /// configured `dev.inspector_n_plus_one_threshold` (default 5).
    ///
    /// Reuses [`crate::inspector::detect_n_plus_one`]. Panics, naming the
    /// request and the offending normalized query + repetition count, when a
    /// single normalized statement was issued at least `threshold` times.
    ///
    /// Use [`TestResponse::assert_no_n_plus_one_with_threshold`] to override
    /// the threshold explicitly.
    #[track_caller]
    pub fn assert_no_n_plus_one(&self) -> &Self {
        self.assert_no_n_plus_one_with_threshold(self.n_plus_one_threshold)
    }

    /// Like [`TestResponse::assert_no_n_plus_one`] but with an explicit
    /// repetition `threshold` instead of the configured default.
    #[track_caller]
    pub fn assert_no_n_plus_one_with_threshold(&self, threshold: usize) -> &Self {
        match crate::inspector::detect_n_plus_one(&self.queries, threshold) {
            Some(w) => panic!(
                "assert_no_n_plus_one failed for {method} {path}: query repeated {count} times \
                 (threshold {threshold}):\n  {sql}{report}",
                method = self.request_method,
                path = self.request_path,
                count = w.count,
                sql = w.sql_template,
                report = self.query_report(),
            ),
            None => self,
        }
    }
}

// Constructed only by the Postgres transactional test-isolation establish path,
// which is cfg'd out under the `sqlite` feature — so gate these out too.
#[cfg(all(feature = "db", not(feature = "sqlite")))]
struct TransactionalDbInterceptor;

#[cfg(all(feature = "db", not(feature = "sqlite")))]
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

// See `TransactionalDbInterceptor`: only the Postgres transactional establish
// path composes interceptors, so this is dead under the `sqlite` feature.
#[cfg(all(feature = "db", not(feature = "sqlite")))]
struct ComposedDbInterceptor {
    first: std::sync::Arc<dyn crate::interceptor::DbConnectionInterceptor>,
    second: std::sync::Arc<dyn crate::interceptor::DbConnectionInterceptor>,
}

#[cfg(all(feature = "db", not(feature = "sqlite")))]
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

/// Deterministically claims and runs up to `max_rows` ready durable repository
/// commit hooks, returning the number of ready hooks selected for this drain
/// pass.
///
/// Intended for integration tests that need to drive the real worker→drain
/// wiring (claim → run the registered runner → ack/nack) **without** the
/// timing-based background commit-hook worker that a served app starts. It
/// generates its own worker id and delegates to the same backend-appropriate
/// drain the production worker uses, so a test can enqueue a durable hook,
/// assert its side effect has not happened, drain once, and assert the side
/// effect deterministically — no `sleep`, no polling.
///
/// Pass a `max_rows` >= the number of enqueued hooks to fully drain in one
/// call. Hooks whose `run_at` is still in the future, or whose handler runner
/// is not registered in this process, are left untouched.
///
/// The returned count is the size of the ready set measured *before* the pass
/// (`status = 'enqueued'` and due), capped at `max_rows`
/// (`min(ready_hooks, max_rows)`). Because it is measured up front — the
/// underlying private drains return `()` and expose no per-hook success tally —
/// it reflects the rows *selected* for draining, not a success count. In the
/// intended single-threaded / private-pool test (no competing worker) every
/// selected hook runs, so this equals the number processed; a hook that fails
/// and is re-queued with a future backoff during the pass is still counted here.
///
/// # Example
///
/// ```rust,ignore
/// use autumn_web::test::TestDb;
///
/// let db = TestDb::shared().await;
/// // ... enqueue a durable repository commit hook and register its runner ...
///
/// let processed = autumn_web::test::drain_ready_repository_commit_hooks(&db.pool(), 16).await;
/// assert_eq!(processed, 1);
/// // ... assert the hook's side effect now exists ...
/// ```
///
/// # Panics
///
/// Panics if a pooled database connection cannot be acquired or the ready-hook
/// count query fails — this helper is for tests, where surfacing such a
/// database failure loudly is the desired behavior.
#[cfg(feature = "db")]
pub async fn drain_ready_repository_commit_hooks(
    pool: &Pool<crate::db::RuntimeConnection>,
    max_rows: usize,
) -> usize {
    use diesel_async::RunQueryDsl as _;

    #[derive(diesel::QueryableByName)]
    struct ReadyCount {
        #[diesel(sql_type = diesel::sql_types::BigInt)]
        ready: i64,
    }

    // The private drains return `()`, so measure the ready set up-front and
    // report how many this pass will claim-and-run. In a single-threaded test
    // (no competing worker) this equals the number processed, capped at
    // `max_rows`. Predicate mirrors the claim query's readiness gate
    // (`status = 'enqueued' AND run_at <= now`); `CURRENT_TIMESTAMP` is standard
    // SQL on both the Postgres and SQLite backends.
    let ready_before: usize = {
        let mut conn = pool
            .get()
            .await
            .expect("drain_ready_repository_commit_hooks: acquire pooled connection");
        let row = diesel::sql_query(
            "SELECT COUNT(*) AS ready \
             FROM autumn_repository_commit_hooks \
             WHERE status = 'enqueued' AND run_at <= CURRENT_TIMESTAMP",
        )
        .get_result::<ReadyCount>(&mut *conn)
        .await
        .expect(
            "drain_ready_repository_commit_hooks: count ready hooks \
             (querying autumn_repository_commit_hooks). An app mounted on a sim \
             substrate must have the framework repository-commit-hook migrations \
             applied — SqliteSubstrate applies them automatically, so a bare \
             SqliteSubstrate satisfies this; a custom DB substrate must apply them \
             too, or run_to_idle cannot drain durable commit hooks",
        );
        usize::try_from(row.ready).unwrap_or(0)
    };

    let worker_id = crate::repository_commit_hooks::repository_commit_hook_worker_id();

    #[cfg(not(feature = "sqlite"))]
    crate::repository_commit_hooks::drain_ready_repository_commit_hooks(pool, &worker_id, max_rows)
        .await;
    #[cfg(feature = "sqlite")]
    crate::repository_commit_hooks::sqlite_drain_ready_repository_commit_hooks(
        pool, &worker_id, max_rows,
    )
    .await;

    ready_before.min(max_rows)
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
                version: 1,
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
                seo: crate::seo::SeoRouteDefaults::EMPTY,
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
                seo: crate::seo::SeoRouteDefaults::EMPTY,
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
                seo: crate::seo::SeoRouteDefaults::EMPTY,
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
            seo: crate::seo::SeoRouteDefaults::EMPTY,
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

    // ── #1262: query-count / N+1 assertions (pure, no Postgres) ─────────
    //
    // These exercise the assertion *logic* on a directly-constructed
    // `TestResponse`, so they run in the always-on CI lane without a database
    // — the framework self-test that guarantees the assertions actually fire.

    fn resp_with_queries(sqls: &[&str], threshold: usize) -> TestResponse {
        TestResponse {
            queries: sqls
                .iter()
                .map(|s| crate::inspector::QueryRecord {
                    sql: (*s).to_owned(),
                    params: Vec::new(),
                    elapsed_ms: 1,
                    location: String::new(),
                })
                .collect(),
            request_method: "GET".to_owned(),
            request_path: "/posts".to_owned(),
            n_plus_one_threshold: threshold,
            ..Default::default()
        }
    }

    fn panic_message(err: &(dyn std::any::Any + Send)) -> String {
        err.downcast_ref::<String>()
            .cloned()
            .or_else(|| err.downcast_ref::<&str>().map(|s| (*s).to_owned()))
            .unwrap_or_default()
    }

    #[test]
    fn query_count_and_queries_reflect_captured_list() {
        let resp = resp_with_queries(&["SELECT 1", "SELECT 2"], 5);
        assert_eq!(resp.query_count(), 2);
        assert_eq!(resp.queries().len(), 2);
        assert_eq!(resp.queries()[0].sql, "SELECT 1");
    }

    /// Regression guard: the `REQUEST_QUERY_CAPTURE` scope must stay active
    /// while a lazy/streaming response body is drained, so DB work performed
    /// *during* body polling (as `Sse` / `Body::from_stream` handlers do) is
    /// still captured. `service.oneshot` returns the response head without
    /// polling the stream; `send()` drains the body with `to_bytes` — if that
    /// drain happened after the scope closed, these body-time queries would be
    /// recorded against an unset task-local and silently dropped, so
    /// `query_count()` would under-report (here: read 0 instead of 3).
    #[cfg(feature = "db")]
    #[tokio::test]
    async fn query_capture_stays_active_while_draining_streaming_body() {
        use futures::StreamExt as _;

        // Each streamed chunk records a DB query when it is polled. The stream is
        // lazy: nothing runs until `to_bytes` polls it inside `send()`.
        async fn stream_handler() -> axum::response::Response {
            let body_stream = futures::stream::iter(0..3).map(|_| {
                crate::db::record_request_db_query(
                    std::time::Duration::from_millis(1),
                    Some("SELECT 1"),
                );
                Ok::<_, std::convert::Infallible>(bytes::Bytes::from_static(b"x"))
            });
            axum::response::Response::new(Body::from_stream(body_stream))
        }

        let router = axum::Router::new().route("/stream", axum::routing::get(stream_handler));
        let resp = RequestBuilder {
            router,
            method: Method::GET,
            uri: "/stream".to_owned(),
            headers: Vec::new(),
            body: Body::empty(),
            cookie_jar: None,
            clock: None,
            n_plus_one_threshold: 5,
            observed_server_errors: None,
        }
        .send()
        .await;

        resp.assert_ok();
        assert_eq!(
            resp.query_count(),
            3,
            "body-time DB queries must be captured while the streaming body is \
             drained inside the active capture scope"
        );
    }

    #[test]
    fn assert_max_queries_passes_at_boundary() {
        // len == n is within budget and must not panic.
        resp_with_queries(&["SELECT 1", "SELECT 2"], 5).assert_max_queries(2);
    }

    #[test]
    fn assert_max_queries_panics_when_exceeded() {
        let resp = resp_with_queries(&["SELECT 1", "SELECT 2", "SELECT 3"], 5);
        let err = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            resp.assert_max_queries(2);
        }))
        .expect_err("assert_max_queries must panic when the query count exceeds the limit");
        let msg = panic_message(err.as_ref());
        assert!(
            msg.contains("GET /posts"),
            "message names the request: {msg}"
        );
        assert!(
            msg.contains("issued 3"),
            "message reports the actual count: {msg}"
        );
        assert!(msg.contains("<= 2"), "message reports the limit: {msg}");
    }

    #[test]
    fn assert_no_n_plus_one_passes_for_distinct_queries() {
        // Three distinct statements: no normalized template repeats.
        resp_with_queries(&["SELECT 1", "SELECT 2", "SELECT 3"], 2).assert_no_n_plus_one();
    }

    #[test]
    fn assert_no_n_plus_one_panics_on_repetition() {
        // Same statement modulo whitespace/case, repeated `threshold` times.
        let resp = resp_with_queries(
            &[
                "SELECT * FROM comments WHERE post_id = $1",
                "SELECT  * FROM comments WHERE post_id = $1",
                "select * from comments where post_id = $1",
            ],
            3,
        );
        let err = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            resp.assert_no_n_plus_one();
        }))
        .expect_err("assert_no_n_plus_one must panic on an N+1 pattern");
        let msg = panic_message(err.as_ref());
        assert!(msg.contains("GET /posts"), "names the request: {msg}");
        assert!(
            msg.contains("3 times"),
            "reports the repetition count: {msg}"
        );
        assert!(
            msg.contains("select * from comments where post_id = $1"),
            "reports the normalized SQL template: {msg}"
        );
    }

    #[test]
    fn assert_no_n_plus_one_with_threshold_overrides_default() {
        // Two identical queries; the configured default threshold (10) does not
        // fire, but an explicit override of 2 does.
        let resp = resp_with_queries(&["SELECT 1", "SELECT 1"], 10);
        resp.assert_no_n_plus_one(); // default threshold 10 -> passes
        let err = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            resp.assert_no_n_plus_one_with_threshold(2);
        }))
        .expect_err("an explicit threshold override must be honoured");
        assert!(
            panic_message(err.as_ref()).contains("2 times"),
            "override fires at the explicit threshold"
        );
    }

    #[test]
    fn default_test_response_inherits_detector_threshold() {
        // A directly-constructed `TestResponse` must inherit the shared detector
        // default (5), not a zero-filled `0` — otherwise `..Default::default()`
        // would silently DISABLE N+1 detection.
        assert_eq!(
            TestResponse::default().n_plus_one_threshold,
            crate::inspector::DEFAULT_N_PLUS_ONE_THRESHOLD,
        );
    }

    #[test]
    fn default_constructed_response_catches_n_plus_one() {
        // Build a response purely via the documented `{ .., ..Default::default() }`
        // pattern (no explicit threshold). With a zero-filled default this passed
        // silently (0 == DISABLED); with the detector default (5) it must panic on
        // the normalized template repeated to the threshold.
        let resp = TestResponse {
            queries: [
                "SELECT * FROM comments WHERE post_id = $1",
                "SELECT  * FROM comments WHERE post_id = $1",
                "select * from comments where post_id = $1",
                "SELECT * FROM  comments WHERE post_id = $1",
                "Select * From comments Where post_id = $1",
            ]
            .iter()
            .map(|s| crate::inspector::QueryRecord {
                sql: (*s).to_owned(),
                params: Vec::new(),
                elapsed_ms: 1,
                location: String::new(),
            })
            .collect(),
            ..Default::default()
        };
        let err = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            resp.assert_no_n_plus_one();
        }))
        .expect_err(
            "a default-constructed TestResponse must inherit the non-zero detector \
             threshold and fire on an N+1 pattern",
        );
        assert!(
            panic_message(err.as_ref()).contains("select * from comments where post_id = $1"),
            "reports the normalized SQL template",
        );
    }
}
