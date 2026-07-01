//! Convenience re-exports for Autumn applications.
//!
//! Import everything commonly needed with a single glob:
//!
//! ```rust,ignore
//! use autumn_web::prelude::*;
//! ```
//!
//! This brings the following into scope:
//!
//! | Category | Items |
//! |----------|-------|
//! | Route macros | [`get`], [`post`], [`put`], [`delete`], [`routes`], [`main`] |
//! | HTML rendering | [`Markup`], [`PreEscaped`], [`html!`](maud::html) |
//! | Extractors | [`Db`], [`Json`], [`Form`], [`Path`], [`Query`] |
//! | Error handling | [`AutumnError`], [`AutumnResult`] |
//! | State | [`AppState`] |
//!
//! For less common types (configuration, middleware, upstream crate access),
//! use targeted imports from [`autumn_web::config`](crate::config),
//! [`autumn_web::middleware`](crate::middleware), or
//! [`autumn_web::reexports`](crate::reexports).

// ── Route macros ─────────────────────────────────────────────────
/// HTTP redirect response.
pub use crate::Redirect;
/// Typed path helper extension trait (`.with_query()`).
pub use crate::paths::PathExt;
/// WebSocket route macro.
#[cfg(feature = "ws")]
pub use autumn_macros::ws;
/// HTTP method route macros, main macro, and route collection.
pub use autumn_macros::{
    api_doc, authorize, cached, delete, event, feature_flag, get, job, jobs, listener, listeners,
    main, oauth2_callback, one_off_tasks, patch, paths, post, put, routes, scheduled, secured,
    service, static_get, static_routes, step_up, task, tasks,
};
#[cfg(feature = "mail")]
pub use autumn_macros::{mail_previews, mailer, mailer_preview};

// ── Rendering ────────────────────────────────────────────────────
/// Resolve a logical static asset path to a fingerprinted URL in release builds.
pub use crate::assets::asset_url;
/// Render a `<script>` tag with SRI integrity for a named vendored JS dependency.
#[cfg(feature = "maud")]
pub use crate::assets::javascript_include_tag;
/// Cache a rendered Maud fragment keyed by `(identity, version)`.
#[cfg(feature = "maud")]
pub use crate::cache::{cache_fragment, cache_fragment_global};
/// Maud HTML templating types.
#[cfg(feature = "maud")]
pub use maud::{Markup, PreEscaped, html};

// ── Extractors ───────────────────────────────────────────────────
/// Canary traffic-routing extractor (reads the `X-Canary` header).
pub use crate::canary::CanaryRoute;
/// Database connection extractor.
#[cfg(feature = "db")]
pub use crate::db::Db;
/// Typed domain event bus publisher extractor. The `Event` trait it works with
/// lives at [`crate::events::Event`] (kept out of the prelude to avoid clashing
/// with [`crate::sse::Event`]).
pub use crate::events::Events;
/// Form data extractor.
pub use crate::extract::Form;
/// JSON request/response type.
pub use crate::extract::Json;
/// Multipart extractor with upload policy helpers.
#[cfg(feature = "multipart")]
pub use crate::extract::Multipart;
/// Path extractor.
pub use crate::extract::Path;
/// Query extractor.
pub use crate::extract::Query;
/// Flash message extractor.
#[cfg(feature = "flash")]
pub use crate::flash::{Flash, FlashLevel, FlashMessage};
/// Extension trait for adding htmx response headers.
#[cfg(feature = "htmx")]
pub use crate::htmx::HxResponseExt;
/// htmx request extractor and asset paths.
#[cfg(feature = "htmx")]
pub use crate::htmx::{
    HTMX_CSRF_JS_PATH, HTMX_JS_PATH, HTMX_SSE_JS_PATH, HxRequest, IDIOMORPH_JS_PATH,
};
/// Out-of-band multi-region swaps response builder.
#[cfg(all(feature = "htmx", feature = "maud"))]
pub use crate::htmx::{HtmxFragments, OobSwap};
/// Trait for live-broadcasting model fragments via `#[repository(Model, broadcasts = "topic")]`.
#[cfg(all(feature = "htmx", feature = "maud"))]
pub use crate::live::LiveFragment;
/// Transactional email types and extractor.
#[cfg(feature = "mail")]
pub use crate::mail::{
    Mail, MailConfig, MailDeliveryQueue, MailDeliveryQueueHandle, MailError, MailPreview,
    MailPreviewError, MailPreviewRegistry, MailTransport, Mailer, SmtpConfig, TlsMode, Transport,
};
#[cfg(all(feature = "presence", feature = "maud"))]
pub use crate::presence_badge;
#[cfg(all(
    feature = "presence",
    feature = "ws",
    feature = "maud",
    feature = "htmx"
))]
pub use crate::presence_stream;
/// Shard routing extractors and types for `[[database.shards]]` apps.
#[cfg(feature = "db")]
pub use crate::sharding::{ShardKey, ShardKeyOverride, ShardedDb, ShardedReadDb, Shards};
/// Server-Sent Events (SSE) support.
pub use crate::sse::{Event, Sse};
/// Structured CLI argument extractor for one-off `#[task]` handlers.
pub use crate::task::TaskArgs;
/// Real-time broadcast facade and channel registry.
#[cfg(feature = "ws")]
pub use crate::{
    Broadcast, BroadcastError, ChannelMessage, ChannelStats, Channels, ChannelsBackend,
    LocalChannelsBackend,
};
/// Distributed presence tracking extractor and related types.
#[cfg(feature = "presence")]
pub use crate::{Presence, PresenceEntry, PresenceEvent, PresenceHandle};
/// State extractor.
pub use axum::extract::State;
/// Trait for types that can be converted into an HTTP response.
pub use axum::response::IntoResponse;
/// HTTP status codes.
pub use http::StatusCode;

// ── Conditional GET / ETag ───────────────────────────────────────
/// `ETag` type for conditional-GET responses.
pub use crate::etag::ETag;
/// Tower middleware that auto-derives weak `ETag`s from response bodies.
pub use crate::etag::EtagLayer;
/// The outcome of a `fresh_when` call — call `.or(response)` to resolve.
pub use crate::etag::FreshWhen;
/// Conversion trait — implemented for `String`, `&str`, `i64`, `(NaiveDateTime, i64)`, `ETag`.
pub use crate::etag::IntoETag;
/// One-liner conditional-GET helper; returns a [`FreshWhen`] resolved with `.or(response)`.
pub use crate::etag::fresh_when;
/// Derive a weak `ETag` from any [`Hash`] value.
pub use crate::etag::hash_etag;

// ── Error handling ───────────────────────────────────────────────
/// Structured audit event types.
pub use crate::audit::{AuditEvent, AuditStatus};
/// Framework error and result types.
pub use crate::error::{AutumnError, AutumnResult};

// ── Pagination ──────────────────────────────────────────────────
/// Pagination primitives — offset and cursor extractors and wrappers.
pub use crate::pagination::{CursorPage, CursorRequest, Page, PageRequest};
/// Reusable Maud pager renderers and options — render an accessible,
/// filter-preserving, htmx-ready pager from a [`Page`]/[`CursorPage`] in one
/// line. See [`crate::ui::pagination`] for the full API.
#[cfg(feature = "maud")]
pub use crate::ui::pagination::{PagerOptions, cursor_pagination_nav, pagination_nav};

// ── Validation ──────────────────────────────────────────────────
/// Auto-validating extractor and proof-of-validation newtype.
pub use crate::validation::{Valid, ValidateExt, Validated};
/// Validation trait — derive with `#[derive(Validate)]` on form/model types.
pub use validator::Validate;

// ── Form ─────────────────────────────────────────────────────────
/// Changeset-style form helpers: [`Changeset`], [`ChangesetForm`], [`IntoChangeset`].
///
/// See [`crate::form`] for the full surface including Maud rendering helpers.
pub use crate::form::{Changeset, ChangesetForm, IntoChangeset};

// ── Display & search widgets ───────────────────────────────────────
/// Card, stat tile, hero, active search, autocomplete, data table, property
/// list, and breadcrumb configuration types and rendering helpers.
///
/// See [`crate::widgets`] for the full API.
#[cfg(feature = "maud")]
pub use crate::widgets::{
    ActiveSearchConfig, AutocompleteConfig, CardConfig, Column, Crumb, Cta, CtaStyle,
    DataTableConfig, HeadingLevel, HeroConfig, SearchMethod, SortDir, active_search,
    active_search_empty_state, active_search_input, active_search_results,
    autocomplete_empty_state, autocomplete_input, autocomplete_option, breadcrumb, card,
    data_table, hero, property_list, stat_card,
};

// ── Hooks ───────────────────────────────────────────────────────
/// Mutation hook types for repository lifecycle callbacks.
#[cfg(feature = "db")]
pub use crate::hooks::{
    DraftField, FieldDiff, MutationContext, MutationHooks, MutationOp, Patch, UpdateDraft,
};

// ── Session & Auth ──────────────────────────────────────────────
/// Extractor for the verified principal ID on bearer-token-protected routes.
pub use crate::auth::ApiToken;
/// Auth extractor for retrieving the authenticated user (session-based).
pub use crate::auth::Auth;
/// Tower layer that validates `Authorization: Bearer <token>` on API routes.
pub use crate::auth::RequireApiToken;
/// Request-scoped log context helper: attach a custom field to the current
/// request so it is carried in the context for structured log consumers (the
/// actuator log buffer, the access line, any context-aware layer). See
/// [`crate::log::context`] for the full surface.
pub use crate::log::context::with_log_field;
/// Session extractor for accessing per-user session data.
pub use crate::session::Session;
/// Tenant extractor and context helpers.
pub use crate::tenancy::{Tenant, with_tenant};

// ── Authorization ────────────────────────────────────────────────
/// Record-level authorization primitives. See
/// [`crate::authorization`] for the full surface.
pub use crate::authorization::{Policy, PolicyContext, Scope, ScopeQuery, Scoped};

// ── Security ───────────────────────────────────────────────────
/// Per-request CSP nonce extractor for embedding in inline `<script>` and `<style>` tags.
pub use crate::security::CspNonce;
/// Configured CSRF form field name; use alongside [`CsrfToken`] to honour
/// custom `security.csrf.form_field` values in hand-written templates.
pub use crate::security::CsrfFormField;
/// CSRF token extractor for embedding in forms.
pub use crate::security::CsrfToken;
/// CSRF token header name extractor.
pub use crate::security::CsrfTokenHeader;
/// CAPTCHA widget helper — emits provider-specific markup (Turnstile or hCaptcha).
/// Requires `bot_protection.enabled = true` in `autumn.toml`.
#[cfg(feature = "maud")]
pub use crate::security::bot_protection_widget;
/// Signed webhook extractor and configuration helpers.
pub use crate::webhook::{
    SignedWebhook, WebhookEndpointConfig, WebhookProvider, WebhookReplayBackend,
    WebhookReplayConfig,
};

// ── Outbound HTTP client ─────────────────────────────────────────
/// Traced outbound HTTP client with automatic retries and test-mock support.
///
/// Declare it as a handler parameter to get a client pre-configured from
/// `[http.client]` config and wired into the test mock harness.
#[cfg(feature = "http-client")]
pub use crate::http_client::Client;

// ── Circuit Breaker ──────────────────────────────────────────────
pub use crate::circuit_breaker::{
    CircuitBreaker, CircuitBreakerError, CircuitBreakerPolicy, CircuitState,
};

// ── SEO helpers ──────────────────────────────────────────────────
/// Per-page SEO meta tag builder (title, description, canonical, OG, Twitter).
pub use crate::seo::SeoMeta;
/// Sitemap change frequency values.
pub use crate::seo::SitemapChangefreq;
/// A single sitemap entry (URL, lastmod, changefreq, priority).
pub use crate::seo::SitemapEntry;
/// Trait for dynamic sitemap URL providers (e.g. database-driven blog posts).
pub use crate::seo::SitemapSource;

// ── Application state ────────────────────────────────────────────
/// Shared application state (for custom extractors).
pub use crate::state::AppState;

// ── Time ─────────────────────────────────────────────────────────
/// Deterministic, injectable wall-clock extractor.
///
/// Use in handlers instead of `chrono::Utc::now()` to make time-sensitive
/// logic testable without sleeping. Override via `TestApp::with_clock`.
pub use crate::time::Clock;

// ── Feature flags ─────────────────────────────────────────────────
/// The main feature-flag service, typically stored as an `AppState` extension.
pub use crate::feature_flags::FeatureFlagService;
/// Request-scoped feature flag extractor — call `flags.enabled("my_flag")`
/// in handlers to gate behaviour without a redeploy.
pub use crate::feature_flags::Flags;
/// In-memory flag store — use in tests and `dev` profile; swap for
/// `autumn_web::feature_flags::pg::PgFlagStore` in production.
pub use crate::feature_flags::InMemoryFlagStore;

// ── Internationalization ───────────────────────────────────────
/// Request-scoped locale extractor (resolves from query, cookie,
/// `Accept-Language`, and default in that order).
#[cfg(feature = "i18n")]
pub use crate::i18n::Locale;
/// Translation lookup macro with compile-time key validation — see
/// [`crate::i18n`] for usage.
#[cfg(feature = "i18n")]
pub use crate::i18n::t;

// ── Time zones ────────────────────────────────────────────────────
/// Request-scoped time zone extractor (resolves from user extension,
/// session, cookie, and query parameter — see [`crate::time_zone`]).
pub use crate::time_zone::TimeZone;
/// Newtype for auth middleware to publish the authenticated user's zone
/// into request extensions.
pub use crate::time_zone::UserTimeZone;
/// Render only the date portion in the given zone.
#[cfg(feature = "maud")]
pub use crate::time_zone::local_date;
/// Render a UTC timestamp as a `<time>` element in the given zone.
#[cfg(feature = "maud")]
pub use crate::time_zone::local_datetime;
/// Parse a browser `datetime-local` value as a local time in `tz` → UTC.
pub use crate::time_zone::parse_local_datetime;
/// Render a relative time string (e.g. "3 minutes ago") as a `<time>` element.
#[cfg(feature = "maud")]
pub use crate::time_zone::time_ago;
/// Format a UTC timestamp as a `datetime-local` input value in `tz`.
pub use crate::time_zone::to_local_input_value;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prelude_types_are_accessible() {
        #[cfg(feature = "db")]
        let _state = AppState {
            extensions: std::sync::Arc::new(std::sync::RwLock::new(
                std::collections::HashMap::new(),
            )),
            pool: None,
            replica_pool: None,
            shards: None,
            profile: None,
            started_at: std::time::Instant::now(),
            health_detailed: false,
            probes: crate::probe::ProbeState::ready_for_test(),
            metrics: crate::middleware::MetricsCollector::new(),
            log_levels: crate::actuator::LogLevels::new("info"),
            task_registry: crate::actuator::TaskRegistry::new(),
            job_registry: crate::actuator::JobRegistry::new(),
            config_props: crate::actuator::ConfigProperties::default(),
            metrics_source_registry: crate::actuator::MetricsSourceRegistry::new(),
            health_indicator_registry: crate::actuator::HealthIndicatorRegistry::new(),
            #[cfg(feature = "ws")]
            channels: crate::channels::Channels::new(32),
            #[cfg(feature = "presence")]
            presence: crate::presence::Presence::new(crate::channels::Channels::new(32)),
            #[cfg(feature = "ws")]
            shutdown: tokio_util::sync::CancellationToken::new(),
            policy_registry: crate::authorization::PolicyRegistry::default(),
            forbidden_response: crate::authorization::ForbiddenResponse::default(),
            auth_session_key: "user_id".to_owned(),
            shared_cache: None,
            clock: std::sync::Arc::new(crate::time::SystemClock),
        };
        #[cfg(not(feature = "db"))]
        let _state = AppState {
            extensions: std::sync::Arc::new(std::sync::RwLock::new(
                std::collections::HashMap::new(),
            )),
            profile: None,
            started_at: std::time::Instant::now(),
            health_detailed: false,
            probes: crate::probe::ProbeState::ready_for_test(),
            metrics: crate::middleware::MetricsCollector::new(),
            log_levels: crate::actuator::LogLevels::new("info"),
            task_registry: crate::actuator::TaskRegistry::new(),
            job_registry: crate::actuator::JobRegistry::new(),
            config_props: crate::actuator::ConfigProperties::default(),
            metrics_source_registry: crate::actuator::MetricsSourceRegistry::new(),
            health_indicator_registry: crate::actuator::HealthIndicatorRegistry::new(),
            #[cfg(feature = "ws")]
            channels: crate::channels::Channels::new(32),
            #[cfg(feature = "presence")]
            presence: crate::presence::Presence::new(crate::channels::Channels::new(32)),
            #[cfg(feature = "ws")]
            shutdown: tokio_util::sync::CancellationToken::new(),
            policy_registry: crate::authorization::PolicyRegistry::default(),
            forbidden_response: crate::authorization::ForbiddenResponse::default(),
            auth_session_key: "user_id".to_owned(),
            shared_cache: None,
            clock: std::sync::Arc::new(crate::time::SystemClock),
        };
        let _err: AutumnResult<()> = Ok(());
    }

    #[test]
    fn json_type_works_through_prelude() {
        let json: Json<&str> = Json("ok");
        assert_eq!(json.0, "ok");
    }

    #[test]
    fn error_types_work_through_prelude() {
        let err = AutumnError::bad_request_msg("test");
        let result: AutumnResult<()> = Err(err);
        assert!(result.is_err());
    }

    #[cfg(feature = "maud")]
    #[test]
    fn maud_types_work_through_prelude() {
        let markup: Markup = html! { "hello" };
        assert!(markup.into_string().contains("hello"));
    }
}
