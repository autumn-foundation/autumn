//! Traits for intercepting and inspecting system operations.
//!
//! Interceptors provide a hook mechanism for testing, instrumentation, and observability.
//! By implementing these traits and registering them during application startup (e.g., via
//! `TestApp::with_db_interceptor`), you can spy on or mutate operations like database
//! queries, background jobs, and outbound emails before they are executed.
//!
//! # Examples
//!
//! ```rust
//! use autumn_web::interceptor::JobInterceptor;
//! use autumn_web::AutumnResult;
//!
//! #[derive(Clone)]
//! struct JobSpy;
//!
//! impl JobInterceptor for JobSpy {
//!     fn intercept_enqueue<'a>(
//!         &'a self,
//!         name: &'a str,
//!         payload: &'a serde_json::Value,
//!         next: std::pin::Pin<
//!             Box<dyn std::future::Future<Output = AutumnResult<()>> + Send + 'a>,
//!         >,
//!     ) -> std::pin::Pin<Box<dyn std::future::Future<Output = AutumnResult<()>> + Send + 'a>> {
//!         println!("Intercepted enqueue of job {}", name);
//!         next
//!     }
//!
//!     fn intercept_execute<'a>(
//!         &'a self,
//!         name: &'a str,
//!         payload: &'a serde_json::Value,
//!         next: std::pin::Pin<
//!             Box<dyn std::future::Future<Output = AutumnResult<()>> + Send + 'a>,
//!         >,
//!     ) -> std::pin::Pin<Box<dyn std::future::Future<Output = AutumnResult<()>> + Send + 'a>> {
//!         Box::pin(async move {
//!             println!("Intercepted execute of job {}", name);
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
