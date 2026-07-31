//! System information plugin.
//!
//! Provides the underlying functionality to inspect basic host system information
//! (like OS, architecture, and core counts) at runtime. This is primarily exposed
//! via an actuator endpoint so administrators can check the server environment.
//!
//! This module exists to supply a standardized JSON payload detailing
//! the environment, which is highly useful when operating multiple nodes
//! across varying deployment architectures.
//!
//! # Examples
//!
//! ```rust
//! use autumn_web::system_info::SystemInfo;
//!
//! let info = SystemInfo {
//!     os: "linux".to_string(),
//!     arch: "x86_64".to_string(),
//!     available_parallelism: 8,
//! };
//! assert_eq!(info.os, "linux");
//! ```

use std::env;
use std::sync::OnceLock;
use std::thread;

use axum::Json;
use serde::{Deserialize, Serialize};

/// Represents the host system's hardware and operating system environment.
///
/// This struct is serialized into JSON and returned by the crate-private `system_info_handler`.
/// It contains basic metrics that do not change during the lifetime of the process.
#[derive(Clone, Serialize, Deserialize)]
pub struct SystemInfo {
    /// The operating system family (e.g., `"linux"`, `"macos"`, `"windows"`).
    pub os: String,
    /// The CPU architecture (e.g., `"x86_64"`, `"aarch64"`).
    pub arch: String,
    /// The number of logical CPU cores available to the application.
    pub available_parallelism: usize,
}

/// An [`axum`] route handler that returns the host's [`SystemInfo`] as JSON.
///
/// This function exists to provide a lightweight diagnostic endpoint.
/// The system info is computed once using a [`OnceLock`] and cached for all
/// subsequent requests to avoid unnecessary system calls.
///
/// # Returns
/// A [`Json`] wrapper containing the serialized [`SystemInfo`].
pub(crate) async fn system_info_handler() -> Json<SystemInfo> {
    static INFO: OnceLock<SystemInfo> = OnceLock::new();
    Json(
        INFO.get_or_init(|| SystemInfo {
            os: env::consts::OS.to_owned(),
            arch: env::consts::ARCH.to_owned(),
            available_parallelism: thread::available_parallelism()
                .map_or(1, std::num::NonZero::get),
        })
        .clone(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    #[tokio::test]
    async fn test_system_info_handler() {
        let app =
            axum::Router::new().route("/actuator/system", axum::routing::get(system_info_handler));
        let req = Request::builder()
            .uri("/actuator/system")
            .body(Body::empty())
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), 200);

        // Ensure it returns valid json
        let body_bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        let info: SystemInfo = serde_json::from_slice(&body_bytes).unwrap();
        assert!(!info.os.is_empty());
    }
}
