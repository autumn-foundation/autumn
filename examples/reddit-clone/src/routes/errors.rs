//! Intentional error routes for smoke-testing the dev error overlay.
//!
//! These routes are registered only in the dev profile. In production they
//! return 404 (via the standard fallback handler). Contributors can hit them
//! to verify the overlay renders correctly without needing to introduce a bug
//! in application code.
//!
//! They are `#[public]`: none reads or writes data, so a visitor gains nothing
//! by reaching one. The release build still mounts them, so the security
//! posture manifest lists them.
//!
//! Visit http://localhost:3000/dev/trigger-error to see the overlay.

use autumn_web::error::AutumnError;
use autumn_web::prelude::*;

/// Intentionally returns a 500 Internal Server Error.
///
/// Use this to smoke-test the dev error overlay. The handler propagates
/// a real Rust error via `?` so the overlay captures a backtrace, displays
/// the source context for this file, and shows the full request context.
#[public]
#[get("/dev/trigger-error")]
pub async fn trigger_error() -> AutumnResult<&'static str> {
    let result: Result<i32, _> = "not_a_number".parse::<i32>().map_err(AutumnError::from);
    // Propagate through `?` — this is the line the overlay should highlight.
    let _n = result?;
    Ok("(this line is never reached)")
}

/// Intentionally panics.
///
/// Demonstrates that the reporting layer catches the panic, turns it into a
/// 500, and the dev overlay renders the panic message and backtrace.
#[public]
#[get("/dev/trigger-panic")]
pub async fn trigger_panic() -> AutumnResult<&'static str> {
    panic!("intentional dev-mode panic — safe to ignore in tests");
}

/// Returns a 404 Not Found via an explicit `Err`.
///
/// Useful for testing the 404 overlay style.
#[public]
#[get("/dev/trigger-404")]
pub async fn trigger_404() -> AutumnResult<&'static str> {
    Err(AutumnError::not_found_msg(
        "this resource was intentionally not found",
    ))
}
