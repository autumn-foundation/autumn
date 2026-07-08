//! `OpenAPI` versioning integration tests.
//!
//! Verifies that `OpenAPI` spec generation correctly tags operations with their
//! API version names and sets the `deprecated: true` property when a route
//! is associated with a version whose deprecation date is in the past.

#![cfg(feature = "openapi")]

use autumn_web::app::ApiVersion;
use autumn_web::openapi::OpenApiConfig;
use autumn_web::prelude::*;
use chrono::{TimeZone, Utc};

#[get("/v1/items", api_version = "v1")]
async fn get_v1_items() -> &'static str {
    "v1"
}

#[get("/v2/items", api_version = "v2")]
async fn get_v2_items() -> &'static str {
    "v2"
}

#[get("/v1/sunset", api_version = "v1")]
async fn get_v1_sunset() -> &'static str {
    "sunset"
}

#[get("/v1/sunset-opt-out", api_version = "v1", sunset_opt_out = true)]
async fn get_v1_sunset_opt_out() -> &'static str {
    "opt-out"
}

#[get("/v1/sunset-only-dep", api_version = "v_sunset_only")]
async fn get_sunset_only_dep() -> &'static str {
    "sunset-only-dep"
}

#[test]
fn test_openapi_spec_version_tagging_and_deprecation() {
    let route_v1 = __autumn_route_info_get_v1_items();
    let route_v2 = __autumn_route_info_get_v2_items();
    let route_sunset = __autumn_route_info_get_v1_sunset();
    let route_sunset_opt_out = __autumn_route_info_get_v1_sunset_opt_out();
    let route_sunset_only = __autumn_route_info_get_sunset_only_dep();

    // v1 is deprecated (2020), v2 is active (2035)
    let v1 = ApiVersion {
        version: "v1".to_string(),
        deprecated_at: Some(Utc.with_ymd_and_hms(2020, 1, 1, 0, 0, 0).unwrap()),
        sunset_at: Some(Utc.with_ymd_and_hms(2040, 1, 1, 0, 0, 0).unwrap()),
    };
    let v2 = ApiVersion {
        version: "v2".to_string(),
        deprecated_at: Some(Utc.with_ymd_and_hms(2035, 1, 1, 0, 0, 0).unwrap()),
        sunset_at: Some(Utc.with_ymd_and_hms(2040, 1, 1, 0, 0, 0).unwrap()),
    };
    let v_sunset_only = ApiVersion {
        version: "v_sunset_only".to_string(),
        deprecated_at: None,
        sunset_at: Some(Utc.with_ymd_and_hms(2020, 1, 1, 0, 0, 0).unwrap()),
    };

    let mut config = OpenApiConfig::new("Versioning Test API", "1.0.0");
    config.api_versions = vec![v1, v2, v_sunset_only];

    let docs = vec![
        &route_v1.api_doc,
        &route_v2.api_doc,
        &route_sunset.api_doc,
        &route_sunset_opt_out.api_doc,
        &route_sunset_only.api_doc,
    ];
    let spec = autumn_web::openapi::generate_spec(&config, &docs);

    // v1 route path should exist
    assert!(spec.paths.contains_key("/v1/items"));
    let op_v1 = spec.paths["/v1/items"].get.as_ref().unwrap();
    // v1 tag should be appended
    assert!(op_v1.tags.contains(&"v1".to_string()));
    // v1 should be marked deprecated: true
    assert_eq!(op_v1.deprecated, Some(true));

    // v2 route path should exist
    assert!(spec.paths.contains_key("/v2/items"));
    let op_v2 = spec.paths["/v2/items"].get.as_ref().unwrap();
    // v2 tag should be appended
    assert!(op_v2.tags.contains(&"v2".to_string()));
    // v2 should NOT be marked deprecated (None)
    assert_eq!(op_v2.deprecated, None);

    // /v1/sunset should have 410 Gone response documented
    assert!(spec.paths.contains_key("/v1/sunset"));
    let op_sunset = spec.paths["/v1/sunset"].get.as_ref().unwrap();
    assert!(op_sunset.responses.contains_key("410"));

    // /v1/sunset-opt-out should NOT have 410 Gone response documented
    assert!(spec.paths.contains_key("/v1/sunset-opt-out"));
    let op_sunset_opt_out = spec.paths["/v1/sunset-opt-out"].get.as_ref().unwrap();
    assert!(!op_sunset_opt_out.responses.contains_key("410"));

    // /v1/sunset-only-dep should be marked deprecated: true because its sunset_at is in the past
    assert!(spec.paths.contains_key("/v1/sunset-only-dep"));
    let op_sunset_only = spec.paths["/v1/sunset-only-dep"].get.as_ref().unwrap();
    assert_eq!(op_sunset_only.deprecated, Some(true));
}
