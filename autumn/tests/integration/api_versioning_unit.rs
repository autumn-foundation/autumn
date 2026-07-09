use autumn_web::error::AutumnError;
use autumn_web::openapi::ApiDoc;
use http::StatusCode;

#[test]
fn test_route_version_fields() {
    let route = autumn_web::Route {
        method: http::Method::GET,
        path: "/test",
        handler: axum::routing::get(|| async { "test" }),
        name: "test_handler",
        api_version: Some("v1"),
        sunset_opt_out: true,
        api_doc: ApiDoc {
            method: "GET",
            path: "/test",
            operation_id: "test_handler",
            api_version: Some("v1"),
            ..Default::default()
        },
        repository: None,
        idempotency: autumn_web::RouteIdempotency::default(),
        timeout: autumn_web::RouteTimeout::Inherit,
    };

    assert_eq!(route.api_version, Some("v1"));
    assert!(route.sunset_opt_out);
    assert_eq!(route.api_doc.api_version, Some("v1"));
}

#[test]
fn test_autumn_error_gone() {
    let err = AutumnError::gone_msg("The resource is gone permanently.");
    assert_eq!(err.status(), StatusCode::GONE);
}

#[test]
fn test_app_builder_api_version_registry() {
    use chrono::TimeZone;
    let deprecated_date = chrono::Utc.with_ymd_and_hms(2026, 6, 2, 0, 0, 0).unwrap();

    let app = autumn_web::app()
        .api_version(autumn_web::app::ApiVersion {
            version: "v1".to_string(),
            deprecated_at: None,
            sunset_at: None,
        })
        .api_version(autumn_web::app::ApiVersion {
            version: "v1".to_string(),
            deprecated_at: Some(deprecated_date),
            sunset_at: None,
        });

    assert_eq!(app.api_versions.len(), 1);
    assert_eq!(app.api_versions[0].deprecated_at, Some(deprecated_date));
}

#[tokio::test]
async fn test_startup_validation_rejects_unregistered_version() {
    use autumn_web::test::TestApp;
    use autumn_web::{get, routes};

    #[get("/v2/test", api_version = "v2")]
    async fn versioned_handler() -> &'static str {
        "v2"
    }

    let result = std::panic::catch_unwind(|| {
        let _client = TestApp::new().routes(routes![versioned_handler]).build();
    });

    assert!(result.is_err());
}

#[test]
fn test_route_listing_with_version_and_status() {
    use autumn_web::app::ApiVersion;
    use autumn_web::route_listing::collect_route_infos;
    use chrono::{TimeZone, Utc};

    let active_version = ApiVersion {
        version: "v1".to_string(),
        deprecated_at: Some(Utc.with_ymd_and_hms(2035, 1, 1, 0, 0, 0).unwrap()),
        sunset_at: Some(Utc.with_ymd_and_hms(2040, 1, 1, 0, 0, 0).unwrap()),
    };
    let deprecated_version = ApiVersion {
        version: "v2_dep".to_string(),
        deprecated_at: Some(Utc.with_ymd_and_hms(2020, 1, 1, 0, 0, 0).unwrap()),
        sunset_at: Some(Utc.with_ymd_and_hms(2040, 1, 1, 0, 0, 0).unwrap()),
    };
    let sunset_version = ApiVersion {
        version: "v3_sun".to_string(),
        deprecated_at: Some(Utc.with_ymd_and_hms(2020, 1, 1, 0, 0, 0).unwrap()),
        sunset_at: Some(Utc.with_ymd_and_hms(2021, 1, 1, 0, 0, 0).unwrap()),
    };

    let routes = vec![
        autumn_web::Route {
            method: http::Method::GET,
            path: "/v1/info",
            handler: axum::routing::get(|| async { "v1" }),
            name: "v1_handler",
            api_version: Some("v1"),
            sunset_opt_out: false,
            api_doc: ApiDoc::default(),
            repository: None,
            idempotency: autumn_web::RouteIdempotency::default(),
            timeout: autumn_web::RouteTimeout::Inherit,
        },
        autumn_web::Route {
            method: http::Method::GET,
            path: "/v2/info",
            handler: axum::routing::get(|| async { "v2" }),
            name: "v2_handler",
            api_version: Some("v2_dep"),
            sunset_opt_out: true,
            api_doc: ApiDoc::default(),
            repository: None,
            idempotency: autumn_web::RouteIdempotency::default(),
            timeout: autumn_web::RouteTimeout::Inherit,
        },
        autumn_web::Route {
            method: http::Method::GET,
            path: "/v3/info",
            handler: axum::routing::get(|| async { "v3" }),
            name: "v3_handler",
            api_version: Some("v3_sun"),
            sunset_opt_out: false,
            api_doc: ApiDoc::default(),
            repository: None,
            idempotency: autumn_web::RouteIdempotency::default(),
            timeout: autumn_web::RouteTimeout::Inherit,
        },
        autumn_web::Route {
            method: http::Method::GET,
            path: "/plain",
            handler: axum::routing::get(|| async { "plain" }),
            name: "plain_handler",
            api_version: None,
            sunset_opt_out: false,
            api_doc: ApiDoc::default(),
            repository: None,
            idempotency: autumn_web::RouteIdempotency::default(),
            timeout: autumn_web::RouteTimeout::Inherit,
        },
    ];

    let registry = vec![active_version, deprecated_version, sunset_version];
    let infos = collect_route_infos(&routes, &[], &[], &registry).unwrap();

    assert_eq!(infos.len(), 4);

    // v1: active
    assert_eq!(infos[0].api_version, Some("v1".to_string()));
    assert_eq!(infos[0].status, Some("active".to_string()));
    assert_eq!(infos[0].sunset_opt_out, Some(false));

    // v2: deprecated
    assert_eq!(infos[1].api_version, Some("v2_dep".to_string()));
    assert_eq!(infos[1].status, Some("deprecated".to_string()));
    assert_eq!(infos[1].sunset_opt_out, Some(true));

    // v3: sunset
    assert_eq!(infos[2].api_version, Some("v3_sun".to_string()));
    assert_eq!(infos[2].status, Some("sunset".to_string()));
    assert_eq!(infos[2].sunset_opt_out, Some(false));

    // plain: None
    assert_eq!(infos[3].api_version, None);
    assert_eq!(infos[3].status, None);
    assert_eq!(infos[3].sunset_opt_out, None);
}
