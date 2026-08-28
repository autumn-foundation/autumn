//! "Around" hooks for the pipelines a tower layer cannot reach.
//!
//! An `AppBuilder::layer` (or a `#[intercept(...)]` route layer) wraps an
//! **inbound HTTP request**. Four other pipelines never pass through that
//! stack at all — an outgoing email, a job enqueue or execution, a pooled
//! database checkout, a channel publish, an outbound HTTP call — so each has
//! its own interceptor trait here.
//!
//! Every trait in this module shares one shape: you receive the operation plus
//! a `next`, and you decide whether, when, and how to invoke it. That is the
//! same "around" contract as a tower layer, on a non-HTTP pipeline.
//!
//! # `next` comes in two forms, and the difference decides what you can build
//!
//! | `next` is | Traits | You can |
//! |---|---|---|
//! | an owned `Pin<Box<dyn Future>>` | [`MailInterceptor`], [`JobInterceptor`], [`DbConnectionInterceptor`] | await it **once**, or drop it to suppress the operation |
//! | a callable `&dyn Fn(..)` | [`ChannelsInterceptor`], [`HttpInterceptor`] | invoke it zero, one, or **many** times |
//!
//! Awaiting an owned future consumes it, and there is no factory to build
//! another — so **retry is not implementable** on the first three. Wrap, time,
//! log, or refuse; to retry a mail send or a job, use the subsystem's own retry
//! policy rather than the interceptor. The callable form is what makes a
//! retrying outbound-HTTP or channel-publish interceptor possible.
//!
//! | Builder method | Trait | Wraps |
//! |---|---|---|
//! | [`with_mail_interceptor`](crate::app::AppBuilder::with_mail_interceptor) | [`MailInterceptor`] | every outgoing [`Mail`](crate::mail::Mail) delivery |
//! | [`with_job_interceptor`](crate::app::AppBuilder::with_job_interceptor) | [`JobInterceptor`] | every job enqueue **and** every job execution |
//! | [`with_db_interceptor`](crate::app::AppBuilder::with_db_interceptor) | [`DbConnectionInterceptor`] | every pooled connection checkout |
//! | [`with_channels_interceptor`](crate::app::AppBuilder::with_channels_interceptor) | [`ChannelsInterceptor`] | every channel publish |
//! | [`with_http_interceptor`](crate::app::AppBuilder::with_http_interceptor) | [`HttpInterceptor`] | every outbound HTTP request |
//!
//! # Last one wins
//!
//! Unlike `AppBuilder::layer`, these are **installs, not a stack**: a second
//! `with_job_interceptor` replaces the first rather than nesting inside it.
//! Compose two behaviours inside a single implementation.
//!
//! # Choosing this over a tower layer
//!
//! Reach for an interceptor when the thing you want to wrap is not an inbound
//! request. If it *is* an inbound request, a layer is the right tool and it
//! composes with the rest of the framework stack. The full decision table lives
//! in `docs/guide/middleware.md`.
//!
//! ```rust,ignore
//! use autumn_web::interceptor::JobInterceptor;
//!
//! struct LoggingJobs;
//!
//! impl JobInterceptor for LoggingJobs {
//!     fn intercept_enqueue<'a>(
//!         &'a self,
//!         name: &'a str,
//!         _payload: &'a serde_json::Value,
//!         next: std::pin::Pin<Box<dyn std::future::Future<Output = autumn_web::AutumnResult<()>> + Send + 'a>>,
//!     ) -> std::pin::Pin<Box<dyn std::future::Future<Output = autumn_web::AutumnResult<()>> + Send + 'a>> {
//!         Box::pin(async move {
//!             tracing::info!(job = name, "enqueuing");
//!             next.await
//!         })
//!     }
//!
//!     fn intercept_execute<'a>(
//!         &'a self,
//!         _name: &'a str,
//!         _payload: &'a serde_json::Value,
//!         next: std::pin::Pin<Box<dyn std::future::Future<Output = autumn_web::AutumnResult<()>> + Send + 'a>>,
//!     ) -> std::pin::Pin<Box<dyn std::future::Future<Output = autumn_web::AutumnResult<()>> + Send + 'a>> {
//!         next
//!     }
//! }
//! ```

#[cfg(feature = "oauth2")]
use std::sync::Arc;

/// Wraps every outgoing mail delivery.
///
/// Await `next` to send, or drop it to swallow the mail (a staging-environment
/// mail trap). The [`Mail`](crate::mail::Mail) is borrowed, so an
/// implementation can inspect the recipient and subject without taking
/// ownership.
///
/// `next` is an owned single-shot future, so this hook **cannot retry** a
/// failed delivery — awaiting it consumes the only attempt and nothing here can
/// start another. Use it to rate-limit, record, redirect, or refuse; leave
/// retries to the mail subsystem's own policy. See the two `next` forms in the
/// [module documentation](self).
#[cfg(feature = "mail")]
pub trait MailInterceptor: Send + Sync + 'static {
    /// Run around one delivery attempt.
    ///
    /// # Errors
    ///
    /// Propagates whatever `next` returns, or any
    /// [`MailError`](crate::mail::MailError) the interceptor itself decides on.
    fn intercept<'a>(
        &'a self,
        mail: &'a crate::mail::Mail,
        next: std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<(), crate::mail::MailError>> + Send + 'a>,
        >,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<(), crate::mail::MailError>> + Send + 'a>,
    >;
}

/// Wraps job enqueue **and** job execution.
///
/// An interceptor **observes** a job; it does not rewrite one. Both methods
/// take the payload as `&serde_json::Value`, and `next` is a future that has
/// already captured the operation, so there is no way to stamp a value into the
/// payload on the way past.
///
/// To carry context from the enqueuing request to the worker, put it in the
/// job's own args — that payload is what crosses the boundary — and use this
/// trait to read it: typically to open a tracing span or set a task-local
/// around `next.await` on the execute side. In-process state established during
/// `intercept_enqueue` is not a channel to `intercept_execute`: when the app is
/// split into web and worker roles the two halves run in different processes.
pub trait JobInterceptor: Send + Sync + 'static {
    /// Run around one enqueue.
    ///
    /// # Errors
    ///
    /// Propagates whatever `next` returns, or the interceptor's own refusal.
    fn intercept_enqueue<'a>(
        &'a self,
        name: &'a str,
        payload: &'a serde_json::Value,
        next: std::pin::Pin<
            Box<dyn std::future::Future<Output = crate::AutumnResult<()>> + Send + 'a>,
        >,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = crate::AutumnResult<()>> + Send + 'a>>;

    /// Run around one execution of an already-enqueued job.
    ///
    /// # Errors
    ///
    /// Propagates whatever `next` returns. An error here fails the job
    /// attempt, so the runtime's retry policy applies.
    fn intercept_execute<'a>(
        &'a self,
        name: &'a str,
        payload: &'a serde_json::Value,
        next: std::pin::Pin<
            Box<dyn std::future::Future<Output = crate::AutumnResult<()>> + Send + 'a>,
        >,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = crate::AutumnResult<()>> + Send + 'a>>;
}

/// What a [`DbConnectionInterceptor`] is told about the checkout it is wrapping.
#[derive(Debug, Clone)]
pub struct DbCheckoutContext {
    /// Which pool the connection is being taken from — `"primary"`, a replica,
    /// or a shard name.
    pub pool_name: String,
}

/// Wraps every pooled database connection checkout.
///
/// This is the hook the test harness uses to implement transactional test
/// isolation: it hands every checkout the *same* connection, already inside a
/// transaction that is rolled back when the test ends — which is why the trait
/// carries the [`is_transactional_test`](Self::is_transactional_test) marker.
/// Application uses are narrower: recording checkout latency, or failing fast
/// when a request has already exceeded a budget.
#[cfg(feature = "db")]
pub trait DbConnectionInterceptor: Send + Sync + 'static {
    /// Run around one checkout.
    ///
    /// # Errors
    ///
    /// Propagates whatever `next` returns — typically a pool timeout.
    fn intercept_checkout<'a>(
        &'a self,
        ctx: DbCheckoutContext,
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
    >;

    /// Returns whether this interceptor enables transactional test isolation mode.
    fn is_transactional_test(&self) -> bool {
        false
    }
}

/// Wraps every channel publish.
///
/// Synchronous, unlike the other interceptors, because publishing to a channel
/// is itself synchronous — it hands the message to the configured bus and
/// returns the number of local subscribers reached.
#[cfg(feature = "ws")]
pub trait ChannelsInterceptor: Send + Sync + 'static {
    /// Intercepts a channel message publication.
    ///
    /// # Errors
    ///
    /// Returns a [`ChannelPublishError`](crate::channels::ChannelPublishError) if publication fails.
    fn intercept_publish(
        &self,
        topic: &str,
        msg: &crate::channels::ChannelMessage,
        next: &dyn Fn(
            &str,
            &crate::channels::ChannelMessage,
        ) -> Result<usize, crate::channels::ChannelPublishError>,
    ) -> Result<usize, crate::channels::ChannelPublishError>;
}
/// The future an [`HttpInterceptor`] returns, and the one `next` hands it.
#[cfg(feature = "oauth2")]
pub type HttpInterceptorFuture<'a> = std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<reqwest::Response, reqwest::Error>> + Send + 'a>,
>;

/// Wraps every outbound HTTP request the framework's client makes.
///
/// Use it to stamp a header on every call, to record timings, or to return a
/// canned response in tests without reaching the network. Unlike the other
/// interceptors this one is resolved from a task-local list
/// ([`ACTIVE_HTTP_INTERCEPTORS`]), so it follows the async task rather than the
/// application state.
#[cfg(feature = "oauth2")]
pub trait HttpInterceptor: Send + Sync + 'static {
    /// Run around one outbound request.
    fn intercept<'a>(
        &'a self,
        req: reqwest::Request,
        next: &'a dyn Fn(reqwest::Request) -> HttpInterceptorFuture<'a>,
    ) -> HttpInterceptorFuture<'a>;
}

#[cfg(feature = "oauth2")]
tokio::task_local! {
    /// The [`HttpInterceptor`]s in effect for the current async task.
    pub static ACTIVE_HTTP_INTERCEPTORS: Vec<Arc<dyn HttpInterceptor>>;
}
