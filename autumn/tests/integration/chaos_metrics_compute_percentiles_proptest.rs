use autumn_web::middleware::MetricsCollector;
use proptest::prelude::*;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1000))]
    #[test]
    fn metrics_compute_percentiles_fuzz(
        latencies in proptest::collection::vec(0..1000u64, 0..1000)
    ) {
        let collector = MetricsCollector::new();
        for &latency in &latencies {
            collector.record("GET", "/test", 200, latency);
        }
        let snapshot = collector.snapshot();
        assert_eq!(snapshot.http.requests_total, latencies.len() as u64);

        // Verify global percentile invariants
        let global = snapshot.http.latency_ms;
        assert!(global.p50 <= global.p95);
        assert!(global.p95 <= global.p99);

        // Verify per-route percentile invariants
        if let Some(route_stats) = snapshot.http.by_route.get("GET /test") {
            assert!(route_stats.p50_ms <= route_stats.p95_ms);
            assert!(route_stats.p95_ms <= route_stats.p99_ms);
        }
    }
}
