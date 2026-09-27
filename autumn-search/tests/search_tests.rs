//! Consolidated integration-test binary for `autumn-search` (issue #1191).
//!
//! Mirrors the workspace convention (CLAUDE.md "Integration Test Layout
//! Guidelines"): every suite is a module under `tests/integration/` so the
//! whole crate links **one** test binary instead of a dozen.

mod integration;
