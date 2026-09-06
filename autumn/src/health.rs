//! Compatibility health endpoint.
//!
//! Automatically mounted by [`AppBuilder::run`](crate::app::AppBuilder::run) at
//! the path configured in [`HealthConfig`](crate::config::HealthConfig)
//! (default: `/health`). This endpoint is a compatibility alias for the
//! readiness probe during the `v0.x` transition to explicit `/live`,
//! `/ready`, and `/startup` probe contracts.
//!
//! # Response format
//!
//! **Without a database:**
//!
//! ```json
//! { "status": "ok", "version": "0.1.0" }
//! ```
//!
//! **With a database pool:**
//!
//! ```json
//! {
//!   "status": "ok",
//!   "version": "0.1.0",
//!   "pool": { "size": 10, "available": 8, "waiting": 0 }
//! }
//! ```
//!
//! Returns the same response as the readiness probe.

use crate::extract::State;
use crate::probe::ProvideProbeState;
use axum::response::IntoResponse;

/// Health check handler.
///
/// Returns the readiness response at the compatibility `/health` path.
///
/// This handler is auto-mounted by the framework and does not need to be
/// registered manually.
///
/// # Response codes
///
/// - `200 OK` -- application is healthy.
/// - `503 Service Unavailable` -- database pool is exhausted (all
///   connections in use and requests are queuing).
pub async fn handler<S: ProvideProbeState + Send + Sync + 'static>(
    State(state): State<S>,
) -> impl IntoResponse {
    crate::probe::readiness_response(&state).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    #[derive(Clone)]
    struct TestProbeState {
        #[cfg(feature = "db")]
        pool: Option<diesel_async::pooled_connection::deadpool::Pool<crate::db::RuntimeConnection>>,
        profile: String,
        health_detailed: bool,
        probes: crate::probe::ProbeState,
    }

    impl ProvideProbeState for TestProbeState {
        fn probes(&self) -> &crate::probe::ProbeState {
            &self.probes
        }
        fn health_detailed(&self) -> bool {
            self.health_detailed
        }
        fn profile(&self) -> &str {
            &self.profile
        }
        fn uptime_display(&self) -> String {
            "test_uptime".to_string()
        }
        #[cfg(feature = "db")]
        fn pool(
            &self,
        ) -> Option<&diesel_async::pooled_connection::deadpool::Pool<crate::db::RuntimeConnection>>
        {
            self.pool.as_ref()
        }
    }

    fn test_state() -> TestProbeState {
        TestProbeState {
            #[cfg(feature = "db")]
            pool: None,
            profile: "dev".into(),
            health_detailed: true,
            probes: crate::probe::ProbeState::ready_for_test(),
        }
    }

    #[tokio::test]
    async fn health_no_database_returns_ok() -> Result<(), Box<dyn std::error::Error>> {
        let app = axum::Router::new()
            .route("/health", axum::routing::get(handler::<TestProbeState>))
            .with_state(test_state());

        let response = app
            .oneshot(Request::builder().uri("/health").body(Body::empty())?)
            .await?;

        assert_eq!(response.status(), StatusCode::OK);

        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await?;
        let json: serde_json::Value = serde_json::from_slice(&body)?;

        assert_eq!(json["status"], "ok");
        assert!(json["version"].is_string());
        assert_eq!(json["profile"], "dev");
        assert!(json["uptime"].is_string());
        assert!(json.get("pool").is_none());
        Ok(())
    }

    #[tokio::test]
    async fn health_no_database_returns_json_content_type() -> Result<(), Box<dyn std::error::Error>>
    {
        let app = axum::Router::new()
            .route("/health", axum::routing::get(handler::<TestProbeState>))
            .with_state(test_state());

        let response = app
            .oneshot(Request::builder().uri("/health").body(Body::empty())?)
            .await?;

        let content_type = response
            .headers()
            .get("content-type")
            .ok_or("missing content type")?
            .to_str()?;
        assert!(
            content_type.contains("application/json"),
            "Expected application/json, got {content_type}"
        );
        Ok(())
    }

    #[cfg(feature = "db")]
    #[tokio::test]
    async fn health_with_pool_returns_pool_status() -> Result<(), Box<dyn std::error::Error>> {
        let config = crate::config::DatabaseConfig {
            url: Some(crate::test_urls::primary("test")),
            pool_size: 5,
            ..Default::default()
        };
        let pool = crate::db::create_pool(&config)?.ok_or("no pool created")?;

        let app = axum::Router::new()
            .route("/health", axum::routing::get(handler::<TestProbeState>))
            .with_state(TestProbeState {
                #[cfg(feature = "db")]
                pool: Some(pool),
                profile: "prod".into(),
                health_detailed: true,
                probes: crate::probe::ProbeState::ready_for_test(),
            });

        let response = app
            .oneshot(Request::builder().uri("/health").body(Body::empty())?)
            .await?;

        assert_eq!(response.status(), StatusCode::OK);

        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await?;
        let json: serde_json::Value = serde_json::from_slice(&body)?;

        assert_eq!(json["status"], "ok");
        assert!(json["version"].is_string());
        assert_eq!(json["pool"]["size"], 5);
        assert!(json["pool"]["available"].is_number());
        Ok(())
    }

    #[tokio::test]
    async fn health_response_includes_version() -> Result<(), Box<dyn std::error::Error>> {
        let app = axum::Router::new()
            .route("/health", axum::routing::get(handler::<TestProbeState>))
            .with_state(test_state());

        let response = app
            .oneshot(Request::builder().uri("/health").body(Body::empty())?)
            .await?;

        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await?;
        let json: serde_json::Value = serde_json::from_slice(&body)?;

        assert_eq!(json["version"], env!("CARGO_PKG_VERSION"));
        Ok(())
    }

    #[tokio::test]
    async fn health_detailed_false_omits_details() -> Result<(), Box<dyn std::error::Error>> {
        let mut state = test_state();
        state.health_detailed = false;

        let app = axum::Router::new()
            .route("/health", axum::routing::get(handler::<TestProbeState>))
            .with_state(state);

        let response = app
            .oneshot(Request::builder().uri("/health").body(Body::empty())?)
            .await?;

        assert_eq!(response.status(), StatusCode::OK);

        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await?;
        let json: serde_json::Value = serde_json::from_slice(&body)?;

        assert_eq!(json["status"], "ok");
        assert!(json.get("version").is_none());
        assert!(json.get("profile").is_none());
        assert!(json.get("uptime").is_none());
        assert!(json.get("pool").is_none());
        Ok(())
    }
}
