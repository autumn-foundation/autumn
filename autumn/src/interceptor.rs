//! Framework interceptors for low-level services like jobs and mail.
//!
//! While HTTP middleware operates on the request/response lifecycle, interceptors
//! hook into internal framework components to observe, modify, or block actions
//! before they occur. They are useful for cross-cutting concerns like telemetry,
//! logging, authorization checks, or testing assertions.
//!
//! Currently supported interceptors:
//!
//! - `MailInterceptor`: Intercept outgoing emails before they are sent.
//! - [`JobInterceptor`]: Intercept background jobs before they are enqueued.
//! - `OAuth2Interceptor`: Intercept OAuth2 authentication flows (if enabled).
//!
//! ## Examples
//!
//! Implementing a simple job logger:
//!
//! ```rust
//! use autumn_web::interceptor::JobInterceptor;
//! use std::pin::Pin;
//! use std::future::Future;
//!
//! struct JobLogger;
//!
//! impl JobInterceptor for JobLogger {
//!     fn intercept_enqueue<'a>(
//!         &'a self,
//!         name: &'a str,
//!         payload: &'a serde_json::Value,
//!         next: Pin<Box<dyn Future<Output = Result<(), autumn_web::job::JobError>> + Send + 'a>>,
//!     ) -> Pin<Box<dyn Future<Output = Result<(), autumn_web::job::JobError>> + Send + 'a>> {
//!         Box::pin(async move {
//!             println!("Enqueuing job {} with payload {:?}", name, payload);
//!             next.await
//!         })
//!     }
//! }
//! ```

#[cfg(feature = "oauth2")]
use std::sync::Arc;

#[cfg(feature = "mail")]
pub trait MailInterceptor: Send + Sync + 'static {
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

pub trait JobInterceptor: Send + Sync + 'static {
    fn intercept_enqueue<'a>(
        &'a self,
        name: &'a str,
        payload: &'a serde_json::Value,
        next: std::pin::Pin<
            Box<dyn std::future::Future<Output = crate::AutumnResult<()>> + Send + 'a>,
        >,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = crate::AutumnResult<()>> + Send + 'a>>;

    fn intercept_execute<'a>(
        &'a self,
        name: &'a str,
        payload: &'a serde_json::Value,
        next: std::pin::Pin<
            Box<dyn std::future::Future<Output = crate::AutumnResult<()>> + Send + 'a>,
        >,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = crate::AutumnResult<()>> + Send + 'a>>;
}

#[derive(Debug, Clone)]
pub struct DbCheckoutContext {
    pub pool_name: String,
}

#[cfg(feature = "db")]
pub trait DbConnectionInterceptor: Send + Sync + 'static {
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
#[cfg(feature = "oauth2")]
pub type HttpInterceptorFuture<'a> = std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<reqwest::Response, reqwest::Error>> + Send + 'a>,
>;

#[cfg(feature = "oauth2")]
pub trait HttpInterceptor: Send + Sync + 'static {
    fn intercept<'a>(
        &'a self,
        req: reqwest::Request,
        next: &'a dyn Fn(reqwest::Request) -> HttpInterceptorFuture<'a>,
    ) -> HttpInterceptorFuture<'a>;
}

#[cfg(feature = "oauth2")]
tokio::task_local! {
    pub static ACTIVE_HTTP_INTERCEPTORS: Vec<Arc<dyn HttpInterceptor>>;
}
