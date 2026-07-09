use autumn_web::middleware::MetricsCollector;
use loom::sync::Arc;
use loom::thread;

#[test]
fn metrics_concurrent_recording() {
    loom::model(|| {
        let collector = Arc::new(MetricsCollector::new());

        let c1 = collector.clone();
        let t1 = thread::spawn(move || {
            c1.record("GET", "/test", 200, 10);
        });

        let c2 = collector.clone();
        let t2 = thread::spawn(move || {
            c2.record("POST", "/test", 201, 20);
        });

        t1.join().unwrap();
        t2.join().unwrap();

        let snapshot = collector.snapshot();
        assert_eq!(snapshot.http.requests_total, 2);
    });
}
