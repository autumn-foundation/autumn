// autumn-panic-gate: request-path module — production code path must be panic-free.
// See CONTRIBUTING.md "Request-path panic gate". Justify exceptions with
// #[allow(clippy::<lint>, reason = "…")] at the narrowest scope.
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::todo,
        clippy::unimplemented,
        clippy::indexing_slicing,
        clippy::string_slice,
        clippy::arithmetic_side_effects,
    )
)]
//! Plugin routes, mounted under `config.route_prefix`.
//!
//! | Method | Path             | Purpose                                   |
//! |--------|------------------|-------------------------------------------|
//! | POST   | `/checkout`      | Start hosted checkout for a plan          |
//! | POST   | `/portal`        | Open the hosted billing portal            |
//! | GET    | `/subscription`  | Current subscription + plan (mirror only) |
//! | POST   | `/webhook`       | Signed provider webhook receiver          |

use autumn_web::AppState;
use autumn_web::reexports::axum::Router;
use autumn_web::route_listing::{RouteClassification, RouteInfo, RouteSource};

use crate::config::BillingConfig;

/// The plugin router (paths relative to the prefix).
pub fn router(config: &BillingConfig) -> Router<AppState> {
    let _ = config;
    Router::new()
}

/// Route listing for `autumn routes` and the audit gate.
#[must_use]
pub fn route_infos(config: &BillingConfig) -> Vec<RouteInfo> {
    let prefix = config.route_prefix.trim_end_matches('/');
    let info =
        |method: &str, path: &str, handler: &str, classification: RouteClassification| RouteInfo {
            method: method.to_owned(),
            path: format!("{prefix}{path}"),
            handler: format!("autumn_billing::routes::{handler}"),
            source: RouteSource::Plugin("autumn-billing".to_owned()),
            classification,
            ..Default::default()
        };
    vec![
        info("POST", "/checkout", "checkout", RouteClassification::Gated),
        info("POST", "/portal", "portal", RouteClassification::Gated),
        info(
            "GET",
            "/subscription",
            "subscription",
            RouteClassification::Gated,
        ),
        info("POST", "/webhook", "webhook", RouteClassification::Public),
    ]
}
