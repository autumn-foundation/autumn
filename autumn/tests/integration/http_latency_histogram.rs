//! Prometheus text-format tests for the HTTP latency histogram (issue #3064).
//!
//! These tests send requests through the full framework stack. Then they read
//! `/actuator/prometheus` and check:
//! - the histogram buckets are present and cumulative,
//! - the label set is bounded,
//! - unmatched routes and unknown methods collapse into one label each.

use autumn_web::test::TestApp;
use autumn_web::{get, routes};
use axum::body::Body;
use http::{Method, Request};
use tower::ServiceExt as _;

const FAMILY: &str = "autumn_http_request_duration_seconds";

#[get("/items/{id}")]
async fn item(axum::extract::Path(id): axum::extract::Path<u32>) -> String {
    format!("item {id}")
}

/// Return the sample lines of one metric name (no comments).
fn samples<'a>(text: &'a str, name: &str) -> Vec<&'a str> {
    let prefix = format!("{name}{{");
    text.lines().filter(|l| l.starts_with(&prefix)).collect()
}

/// Return the value of one label in a sample line.
fn label<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!("{key}=\"");
    let start = line.find(&needle)? + needle.len();
    let end = line[start..].find('"')? + start;
    Some(&line[start..end])
}

/// Return the sample value at the end of a line.
fn value(line: &str) -> f64 {
    line.rsplit(' ').next().unwrap().parse().unwrap()
}

async fn scrape(router: &axum::Router) -> String {
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/actuator/prometheus")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

async fn send(router: &axum::Router, method: Method, uri: &str) -> u16 {
    router
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
        .as_u16()
}

#[tokio::test]
async fn histogram_buckets_are_present_and_cumulative() {
    let router = TestApp::new().routes(routes![item]).build().into_router();
    assert_eq!(send(&router, Method::GET, "/items/1").await, 200);
    assert_eq!(send(&router, Method::GET, "/items/2").await, 200);

    let text = scrape(&router).await;
    assert_eq!(
        text.matches(&format!("# TYPE {FAMILY} histogram\n"))
            .count(),
        1,
        "one TYPE line for the histogram:\n{text}"
    );

    let series = "version=\"stable\",method=\"GET\",route=\"/items/{id}\",status_class=\"2xx\"";
    let buckets: Vec<&str> = samples(&text, &format!("{FAMILY}_bucket"))
        .into_iter()
        .filter(|l| l.contains(series))
        .collect();
    let bounds: Vec<&str> = buckets.iter().map(|l| label(l, "le").unwrap()).collect();
    assert_eq!(
        bounds,
        [
            "0.001", "0.005", "0.01", "0.025", "0.05", "0.1", "0.25", "0.5", "1", "2.5", "5", "10",
            "+Inf"
        ],
        "bucket lines for {series}:\n{text}"
    );
    let last = buckets.last().unwrap();

    // Buckets are cumulative: each count is not less than the one before.
    let counts: Vec<f64> = buckets.iter().map(|l| value(l)).collect();
    assert!(
        counts.windows(2).all(|w| w[0] <= w[1]),
        "bucket counts are not cumulative: {counts:?}"
    );
    assert!((value(last) - 2.0).abs() < f64::EPSILON, "+Inf counts both");

    let count_line = format!("{FAMILY}_count{{{series}}} 2");
    assert!(text.contains(&count_line), "missing {count_line}:\n{text}");
    let sum = samples(&text, &format!("{FAMILY}_sum"))
        .into_iter()
        .find(|l| l.contains(series))
        .unwrap_or_else(|| panic!("missing _sum for {series}:\n{text}"));
    assert!(value(sum) >= 0.0);
}

#[tokio::test]
async fn unmatched_routes_collapse_into_one_label() {
    let router = TestApp::new().routes(routes![item]).build().into_router();
    for n in 0..25 {
        assert_eq!(
            send(&router, Method::GET, &format!("/probe-{n}/x")).await,
            404
        );
    }

    let text = scrape(&router).await;
    assert!(
        !text.contains("/probe-"),
        "a raw unmatched path leaked into a label:\n{text}"
    );
    let unmatched: Vec<&str> = samples(&text, &format!("{FAMILY}_count"))
        .into_iter()
        .filter(|l| label(l, "route") == Some("_unmatched"))
        .collect();
    assert_eq!(unmatched.len(), 1, "one unmatched series:\n{text}");
    assert_eq!(label(unmatched[0], "status_class"), Some("4xx"));
    assert!((value(unmatched[0]) - 25.0).abs() < f64::EPSILON);
}

#[tokio::test]
async fn unknown_methods_collapse_into_one_label() {
    let router = TestApp::new().routes(routes![item]).build().into_router();
    for n in 0..25 {
        let method = Method::from_bytes(format!("PROBE{n}").as_bytes()).unwrap();
        send(&router, method, "/items/1").await;
    }

    let text = scrape(&router).await;
    assert!(
        !text.contains("PROBE"),
        "a raw extension method leaked into a label:\n{text}"
    );
    let other: Vec<&str> = samples(&text, &format!("{FAMILY}_count"))
        .into_iter()
        .filter(|l| label(l, "method") == Some("_other"))
        .collect();
    assert!(!other.is_empty(), "no _other method series:\n{text}");
    let total: f64 = other.iter().map(|l| value(l)).sum();
    assert!((total - 25.0).abs() < f64::EPSILON);
}

#[tokio::test]
async fn label_values_come_from_bounded_sets() {
    let router = TestApp::new().routes(routes![item]).build().into_router();
    send(&router, Method::GET, "/items/1").await;
    send(&router, Method::GET, "/items/not-a-number").await;
    send(&router, Method::POST, "/items/1").await;
    send(&router, Method::GET, "/nowhere").await;

    let text = scrape(&router).await;
    let lines = samples(&text, &format!("{FAMILY}_count"));
    assert!(lines.len() >= 3, "too few series:\n{text}");
    for line in lines {
        let class = label(line, "status_class").unwrap();
        assert!(
            ["1xx", "2xx", "3xx", "4xx", "5xx", "other"].contains(&class),
            "unexpected status_class in {line}"
        );
        let method = label(line, "method").unwrap();
        assert!(
            [
                "GET", "HEAD", "POST", "PUT", "DELETE", "PATCH", "OPTIONS", "CONNECT", "TRACE",
                "_other"
            ]
            .contains(&method),
            "unexpected method in {line}"
        );
        let route = label(line, "route").unwrap();
        assert!(
            route == "_unmatched" || route.starts_with('/'),
            "unexpected route in {line}"
        );
        assert!(line.contains("version=\"stable\""), "no version in {line}");
    }
}

#[tokio::test]
async fn deprecated_summary_keeps_its_quantiles_under_a_new_name() {
    let router = TestApp::new().routes(routes![item]).build().into_router();
    send(&router, Method::GET, "/items/1").await;

    let text = scrape(&router).await;
    let summary = "autumn_http_request_duration_quantiles_seconds";
    assert!(
        text.contains(&format!("# TYPE {summary} summary\n")),
        "{text}"
    );
    assert!(
        text.contains(&format!(
            "{summary}{{version=\"stable\",quantile=\"0.99\"}}"
        )),
        "{text}"
    );
    assert!(
        samples(&text, FAMILY).is_empty(),
        "the histogram family has no bare (quantile) samples:\n{text}"
    );
}
