//! Structured logging, context enrichment, and log capturing for testing.
//!
//! This module provides the tools to manage application logs effectively. It builds
//! on top of `tracing` to offer:
//!
//! - **Context Enrichment**: Attaching request IDs, tenant IDs, and user IDs to logs
//!   automatically via [`crate::middleware::LogContextLayer`] and [`context::LogContext`].
//! - **Capturing**: [`capture::LogCaptureLayer`] records emitted events into a
//!   [`capture::LogBuffer`], so tests can assert that critical events were logged.
//! - **Filtering**: Configurable verbosity and component-level filters to manage
//!   log volume.

pub mod capture;
pub mod context;
pub mod filter;
