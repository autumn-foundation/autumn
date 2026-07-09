//! Built-in middleware for Autumn applications.
//!
//! Currently provides:
//!
//! - [`RequestIdLayer`] / [`RequestId`] -- assigns a unique UUID v4 to every
//!   incoming HTTP request, available in handler extensions and the
//!   `X-Request-Id` response header.
//! - [`AccessLogLayer`] -- emits one structured access-log event per served
//!   request (method, route template, status, `duration_ms`, `request_id`).
//! - [`ExceptionFilterLayer`] / [`ExceptionFilter`] -- intercepts error
//!   responses and runs a user-registered filter chain for logging,
//!   transformation, or replacement.
//! - Dev-only live reload helpers used by `autumn dev`.
//!
//! The [`RequestIdLayer`] is applied automatically by
//! [`AppBuilder::run`](crate::app::AppBuilder::run); you do not need to
//! add it manually. The [`ExceptionFilterLayer`] is applied automatically
//! when at least one exception filter is registered via
//! [`AppBuilder::exception_filter`](crate::app::AppBuilder::exception_filter).

pub(crate) mod access_log;
pub(crate) mod dev;
pub(crate) mod error_page_filter;
pub(crate) mod exception_filter;
pub(crate) mod load_shed;
pub(crate) mod log_context;
pub mod maintenance;
pub(crate) mod method_override;
pub(crate) mod metrics;
pub(crate) mod request_id;
pub(crate) mod server_timing;
#[cfg(feature = "telemetry-otlp")]
pub(crate) mod trace_context;

pub use access_log::{
    ACCESS_LOG_TARGET, AccessLogEmitted, AccessLogLayer, AccessLogService, UNMATCHED_ROUTE,
};
pub use exception_filter::{AutumnErrorInfo, ExceptionFilter, ExceptionFilterLayer};
pub use load_shed::{LoadShedExempt, LoadShedLayer, LoadShedService};
pub use log_context::{LogContextLayer, LogContextService};
pub use maintenance::{DEFAULT_HEALTH_PREFIX, MaintenanceLayer, MaintenanceService};
pub use method_override::{
    DEFAULT_METHOD_OVERRIDE_FIELD, MethodOverrideConfig, MethodOverrideLayer,
    MethodOverrideRejection, MethodOverrideService, OverriddenMethod,
    method_override_rejection_filter,
};
pub use metrics::{MetricsCollector, MetricsLayer};
pub use request_id::{RequestId, RequestIdLayer};
pub use server_timing::{ServerTimingEmitted, ServerTimingLayer, ServerTimingService};
#[cfg(feature = "telemetry-otlp")]
pub use trace_context::{TraceContextLayer, TraceContextService};
