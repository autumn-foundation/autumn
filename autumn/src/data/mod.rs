//! Data import/export utilities.
//!
//! This module provides first-class CSV import and export for Autumn models.
//!
//! # Modules
//!
//! - `csv` — CSV schema trait, streaming export, row-by-row import with
//!   structured error reporting.

#[cfg(feature = "csv")]
pub mod csv;
