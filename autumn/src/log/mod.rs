//! Structured logging, context enrichment, and log capturing for testing.
//!
//! This module provides the tools to manage application logs effectively. It builds
//! on top of `tracing` to offer:
//!
//! - **Context Enrichment**: Attaching request IDs, tenant IDs, and user IDs to logs
//!   automatically via `LogContextLayer`.
//! - **Capturing**: Utilities like `capture::CapturedLogs` to assert against
//!   emitted logs within unit tests, ensuring that critical events are recorded.
//! - **Filtering**: Configurable verbosity and component-level filters to manage
//!   log volume.

#[doc(inline)]
pub mod capture;
#[doc(inline)]
pub mod context;
#[doc(inline)]
pub mod filter;
