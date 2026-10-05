//! Request-scoped log context — autumn's batteries-included MDC.
//!
//! Every HTTP request runs inside a [`LogContext`] that is seeded with the
//! request's `request_id` (the same value used by the `x-request-id` header and
//! error pages) and, once known, the authenticated `user_id` and resolved
//! tenant id. Handler and service code can attach additional fields with
//! [`with_log_field`]; those fields are then observable by every
//! context-aware consumer for the remainder of the request — the actuator log
//! buffer (#1168), the per-request access line (#999), and any custom
//! [`tracing`] layer.
//!
//! The context lives in a [`tokio::task_local`], mirroring the pattern already
//! used by tenancy and the database connection scope. It is **always-on** and
//! is not gated behind any telemetry feature.
//!
//! # Isolation
//!
//! Each request gets a fresh context; nothing leaks between requests. A future
//! moved onto a new task with [`tokio::spawn`] does **not** inherit the current
//! request context — call [`in_current_context`] to propagate it explicitly.
//!
//! # What renders where
//!
//! The well-known correlation ids (`request_id`/`user_id`/`tenant_id`) ride the
//! request span, so they appear in ordinary `tracing` output for every event.
//! Custom fields added via [`with_log_field`] live in the request context and
//! are surfaced by **structured** consumers — the actuator log buffer (#1168),
//! the access line (#999), or any context-aware layer — rather than the default
//! stdout formatter.
//!
//! # Example
//!
//! ```rust,no_run
//! use autumn_web::log::context;
//!
//! // deep inside a handler
//! context::with_log_field("order_id", "A-1001");
//! // request_id/user_id render on this line via the request span; order_id is
//! // carried in the context for structured consumers.
//! tracing::info!("charged card");
//! ```

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::{Arc, RwLock};

use serde::Serialize;
use tracing::Instrument as _;

use crate::log::filter::{FILTERED_PLACEHOLDER, ParameterFilter};

tokio::task_local! {
    static CURRENT: LogContext;
}

/// Field keys reserved for the built-in correlation ids. Custom fields using
/// these names are ignored so they can never shadow the authoritative values
/// when [`LogFields`] flattens custom fields alongside the core ids.
const RESERVED_FIELD_KEYS: [&str; 3] = ["request_id", "user_id", "tenant_id"];

/// A snapshot of the fields carried by the current request context.
///
/// Returned by [`LogContext::snapshot`] / [`snapshot`]. Serializes to a flat
/// JSON object suitable for embedding in structured log output.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct LogFields {
    /// Correlation id for the request (matches the `x-request-id` header).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    /// Authenticated user id, when the request is authenticated.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    /// Resolved tenant id, when multi-tenancy is active.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tenant_id: Option<String>,
    /// Custom fields added during the request via [`with_log_field`].
    #[serde(flatten)]
    pub fields: BTreeMap<String, String>,
}

impl LogFields {
    /// Returns `true` when no field of any kind is set.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.request_id.is_none()
            && self.user_id.is_none()
            && self.tenant_id.is_none()
            && self.fields.is_empty()
    }
}

/// A cheap-to-clone handle to one request's log context.
///
/// Cloning shares the same underlying storage, so a clone captured before a
/// [`tokio::spawn`] and re-scoped with [`in_current_context`] observes later
/// mutations made on the original.
#[derive(Clone)]
pub struct LogContext {
    inner: Arc<RwLock<Inner>>,
    filter: Arc<ParameterFilter>,
    /// The request span carrying the well-known correlation fields. Holding it
    /// directly (rather than relying on [`tracing::Span::current`]) means
    /// `set_user_id` / `set_tenant_id` record onto the request span even when
    /// invoked from inside a nested child span.
    span: tracing::Span,
}

#[derive(Default)]
struct Inner {
    request_id: Option<String>,
    user_id: Option<String>,
    tenant_id: Option<String>,
    fields: BTreeMap<String, String>,
}

impl LogContext {
    /// Create a new context seeded with an optional `request_id`, using the
    /// default sensitive-key filter.
    #[must_use]
    pub fn new(request_id: Option<String>) -> Self {
        Self::with_filter(request_id, Arc::new(ParameterFilter::default()))
    }

    /// Create a new context seeded with an optional `request_id` and a shared
    /// [`ParameterFilter`] used to scrub sensitive custom fields.
    #[must_use]
    pub fn with_filter(request_id: Option<String>, filter: Arc<ParameterFilter>) -> Self {
        Self {
            inner: Arc::new(RwLock::new(Inner {
                request_id,
                ..Inner::default()
            })),
            filter,
            span: tracing::Span::none(),
        }
    }

    /// Attach the request span that carries the well-known correlation fields.
    ///
    /// Used by the middleware so [`set_user_id`](Self::set_user_id) /
    /// [`set_tenant_id`](Self::set_tenant_id) record onto the request span
    /// regardless of any nested child span that happens to be current.
    #[must_use]
    pub fn with_span(mut self, span: tracing::Span) -> Self {
        self.span = span;
        self
    }

    /// Record the authenticated user id on this context.
    ///
    /// Also records `user_id` on the attached request span (if any) so it
    /// surfaces in standard log output for every event in the request.
    pub fn set_user_id(&self, user_id: impl Into<String>) {
        let user_id = user_id.into();
        self.span
            .record("user_id", tracing::field::display(&user_id));
        if let Ok(mut guard) = self.inner.write() {
            guard.user_id = Some(user_id);
        }
    }

    /// Record the resolved tenant id on this context.
    ///
    /// Also records `tenant_id` on the attached request span (if any).
    pub fn set_tenant_id(&self, tenant_id: impl Into<String>) {
        let tenant_id = tenant_id.into();
        self.span
            .record("tenant_id", tracing::field::display(&tenant_id));
        if let Ok(mut guard) = self.inner.write() {
            guard.tenant_id = Some(tenant_id);
        }
    }

    /// Attach a custom field. Values under a sensitive key (per the configured
    /// [`ParameterFilter`]) are replaced with `[FILTERED]` before storage.
    pub fn insert_field(&self, key: impl Into<String>, value: impl Into<String>) {
        let key = key.into();
        // Never let a custom field shadow a built-in correlation id: the core
        // ids have dedicated setters and are flattened alongside `fields`.
        if RESERVED_FIELD_KEYS.contains(&key.as_str()) {
            return;
        }
        let value = if self.filter.matches_key(&key) {
            FILTERED_PLACEHOLDER.to_owned()
        } else {
            value.into()
        };
        if let Ok(mut guard) = self.inner.write() {
            guard.fields.insert(key, value);
        }
    }

    /// Clone the request span attached to this context (or a disabled span when
    /// none is attached). Used to re-enter the span while re-establishing the
    /// context during streaming response-body production.
    #[must_use]
    pub fn span(&self) -> tracing::Span {
        self.span.clone()
    }

    /// Take a point-in-time snapshot of all fields on this context.
    #[must_use]
    pub fn snapshot(&self) -> LogFields {
        self.inner.read().map_or_else(
            |_| LogFields::default(),
            |guard| LogFields {
                request_id: guard.request_id.clone(),
                user_id: guard.user_id.clone(),
                tenant_id: guard.tenant_id.clone(),
                fields: guard.fields.clone(),
            },
        )
    }
}

impl std::fmt::Debug for LogContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LogContext")
            .field("fields", &self.snapshot())
            .finish()
    }
}

/// Run `future` with `ctx` installed as the current request context.
pub async fn scope<F: Future>(ctx: LogContext, future: F) -> F::Output {
    CURRENT.scope(ctx, future).await
}

/// Wrap `future` so it runs with `ctx` installed, without awaiting it.
///
/// Returns the scoping future directly so middleware can name it as a concrete
/// associated `Future` type (avoiding a boxed allocation per request).
pub fn scoped<F: Future>(
    ctx: LogContext,
    future: F,
) -> tokio::task::futures::TaskLocalFuture<LogContext, F> {
    CURRENT.scope(ctx, future)
}

/// Run a synchronous closure with `ctx` installed as the current context.
///
/// Used by the middleware so a downstream layer's synchronous `Service::call`
/// work (which runs before the request future is polled) is also correlated.
pub fn sync_scope<R>(ctx: LogContext, f: impl FnOnce() -> R) -> R {
    CURRENT.sync_scope(ctx, f)
}

/// Return a handle to the current request context, if one is installed.
#[must_use]
pub fn current() -> Option<LogContext> {
    CURRENT.try_with(Clone::clone).ok()
}

/// The request id of the current request context, if any.
///
/// Cheaper than [`snapshot`]: it clones one field, not the whole context.
#[must_use]
pub fn current_request_id() -> Option<String> {
    CURRENT
        .try_with(|ctx| ctx.inner.read().ok()?.request_id.clone())
        .ok()
        .flatten()
}

/// Snapshot the current request context's fields, if any.
#[must_use]
pub fn snapshot() -> Option<LogFields> {
    current().map(|ctx| ctx.snapshot())
}

/// Attach a custom field to the current request context.
///
/// No-op when called outside of a request (no context installed).
pub fn with_log_field(key: impl Into<String>, value: impl Into<String>) {
    if let Some(ctx) = current() {
        ctx.insert_field(key, value);
    }
}

/// Record the authenticated user id on the current request context.
///
/// Also records `user_id` on the request span so it surfaces in standard log
/// output. No-op when called outside of a request.
pub fn set_user_id(user_id: impl Into<String>) {
    if let Some(ctx) = current() {
        ctx.set_user_id(user_id);
    }
}

/// Record the resolved tenant id on the current request context.
///
/// Also records `tenant_id` on the request span. No-op when called outside of a
/// request.
pub fn set_tenant_id(tenant_id: impl Into<String>) {
    if let Some(ctx) = current() {
        ctx.set_tenant_id(tenant_id);
    }
}

/// Wrap `future` so it runs inside a clone of the current request context.
///
/// Use this to carry request context across a [`tokio::spawn`] boundary, which
/// otherwise starts with no context:
///
/// ```rust,no_run
/// use autumn_web::log::context;
///
/// tokio::spawn(context::in_current_context(async move {
///     tracing::info!("still correlated to the originating request");
/// }));
/// ```
pub fn in_current_context<F: Future>(future: F) -> impl Future<Output = F::Output> {
    let ctx = current();
    async move {
        match ctx {
            Some(ctx) => {
                // Re-enter the request span too, so ordinary `tracing` output
                // from the spawned work carries request_id/user_id/tenant_id —
                // not just consumers that read the task-local snapshot.
                let span = ctx.span.clone();
                scope(ctx, future.instrument(span)).await
            }
            None => future.await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn seeds_request_id_and_exposes_it_via_current() {
        let ctx = LogContext::new(Some("req-123".to_owned()));
        scope(ctx, async {
            let snap = snapshot().expect("context should be installed");
            assert_eq!(snap.request_id.as_deref(), Some("req-123"));
            assert_eq!(snap.user_id, None);
        })
        .await;
    }

    #[tokio::test]
    async fn user_and_tenant_are_added_to_current_context() {
        let ctx = LogContext::new(Some("req-1".to_owned()));
        scope(ctx, async {
            set_user_id("42");
            set_tenant_id("acme");
            let snap = snapshot().unwrap();
            assert_eq!(snap.user_id.as_deref(), Some("42"));
            assert_eq!(snap.tenant_id.as_deref(), Some("acme"));
        })
        .await;
    }

    #[tokio::test]
    async fn custom_fields_appear_on_subsequent_snapshots() {
        let ctx = LogContext::new(Some("req-1".to_owned()));
        scope(ctx, async {
            with_log_field("order_id", "A-1001");
            // A later read in the same request sees the field.
            let snap = snapshot().unwrap();
            assert_eq!(
                snap.fields.get("order_id").map(String::as_str),
                Some("A-1001")
            );
        })
        .await;
    }

    #[tokio::test]
    async fn sensitive_custom_fields_are_scrubbed() {
        let ctx = LogContext::new(Some("req-1".to_owned()));
        scope(ctx, async {
            with_log_field("password", "hunter2");
            with_log_field("order_id", "ok");
            let snap = snapshot().unwrap();
            assert_eq!(
                snap.fields.get("password").map(String::as_str),
                Some(FILTERED_PLACEHOLDER)
            );
            assert_eq!(snap.fields.get("order_id").map(String::as_str), Some("ok"));
        })
        .await;
    }

    #[tokio::test]
    async fn custom_fields_cannot_shadow_core_correlation_ids() {
        let ctx = LogContext::new(Some("real-req".to_owned()));
        scope(ctx, async {
            set_user_id("real-user");
            // Attempt to override core ids via the custom-field channel.
            with_log_field("request_id", "spoofed");
            with_log_field("user_id", "spoofed");
            with_log_field("tenant_id", "spoofed");
            with_log_field("order_id", "kept");

            let snap = snapshot().unwrap();
            assert_eq!(snap.request_id.as_deref(), Some("real-req"));
            assert_eq!(snap.user_id.as_deref(), Some("real-user"));
            assert!(!snap.fields.contains_key("request_id"));
            assert!(!snap.fields.contains_key("user_id"));
            assert!(!snap.fields.contains_key("tenant_id"));
            assert_eq!(
                snap.fields.get("order_id").map(String::as_str),
                Some("kept")
            );

            // Serialized form has exactly one request_id, carrying the real value.
            let v = serde_json::to_value(&snap).unwrap();
            assert_eq!(v["request_id"], "real-req");
        })
        .await;
    }

    #[tokio::test]
    async fn no_context_outside_a_request() {
        // Outside of any scope, helpers are inert and current() is None.
        assert!(current().is_none());
        assert!(snapshot().is_none());
        with_log_field("k", "v"); // must not panic
        set_user_id("u"); // must not panic
    }

    #[tokio::test]
    async fn contexts_do_not_leak_between_requests() {
        let first = LogContext::new(Some("req-A".to_owned()));
        scope(first, async {
            with_log_field("k", "from-A");
        })
        .await;

        let second = LogContext::new(Some("req-B".to_owned()));
        scope(second, async {
            let snap = snapshot().unwrap();
            assert_eq!(snap.request_id.as_deref(), Some("req-B"));
            assert!(
                snap.fields.is_empty(),
                "fields from request A leaked into B"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn spawned_task_does_not_inherit_context_unless_propagated() {
        let ctx = LogContext::new(Some("req-1".to_owned()));
        scope(ctx, async {
            // A bare spawn starts with no context.
            let bare = tokio::spawn(async { current().is_some() }).await.unwrap();
            assert!(!bare, "spawned task silently inherited request context");

            // Explicit propagation carries it across the spawn boundary.
            let propagated = tokio::spawn(in_current_context(async {
                snapshot().and_then(|s| s.request_id)
            }))
            .await
            .unwrap();
            assert_eq!(propagated.as_deref(), Some("req-1"));
        })
        .await;
    }

    #[test]
    fn log_fields_serialize_flat() {
        let mut fields = BTreeMap::new();
        fields.insert("order_id".to_owned(), "A-1".to_owned());
        let f = LogFields {
            request_id: Some("r".to_owned()),
            user_id: Some("42".to_owned()),
            tenant_id: None,
            fields,
        };
        let v = serde_json::to_value(&f).unwrap();
        assert_eq!(v["request_id"], "r");
        assert_eq!(v["user_id"], "42");
        assert_eq!(v["order_id"], "A-1");
        assert!(v.get("tenant_id").is_none());
    }
}
