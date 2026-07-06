//! Structured logging and telemetry configuration.
//!
//! This module configures the `tracing` subscriber for the application, integrating
//! structured JSON logging, dynamic log filtering, OpenTelemetry traces, and in-memory
//! log capture for testing.
//!
//! # Examples
//!
//! ```rust
//! use tracing::{info, warn, error};
//!
//! // The log module configures `tracing` globally during app startup,
//! // allowing you to use standard tracing macros anywhere in your code.
//! info!(
//!     user_id = 123,
//!     action = "login",
//!     "User authenticated successfully"
//! );
//! ```

pub mod capture;
pub mod context;
pub mod filter;
