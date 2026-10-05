//! H8: serve runtime files from their own folder.

use std::path::Path;

use autumn_web::AppState;
use autumn_web::route_listing::RouteInfo;

/// Where the files are served.
pub const PREFIX: &str = "/exports";

/// The router for the exports folder.
pub fn router(_dir: &Path) -> axum::Router<AppState> {
    todo!("H8")
}

/// The route that `router` serves, for `autumn routes audit`.
#[must_use]
pub fn routes() -> Vec<RouteInfo> {
    todo!("H8")
}
