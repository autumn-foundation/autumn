use autumn_web::idempotency::{IdempotencyRecord, IdempotencyStore, MemoryIdempotencyStore};
use std::time::Duration;

#[test]
fn idempotency_ttl_overflow_crash() {
    let store = MemoryIdempotencyStore::new(Duration::from_secs(10));
    let rec = IdempotencyRecord {
        status: 200,
        headers: vec![],
        body: vec![],
        metadata: vec![],
    };
    // Should panic if Instant::now() + Duration::MAX is executed
    store.set("key", rec, vec![], Duration::MAX);
}

#[test]
fn idempotency_lock_ttl_overflow_crash() {
    let store = MemoryIdempotencyStore::new(Duration::from_secs(10));
    // Should panic if Instant::now() + Duration::MAX is executed
    store.try_lock("key_lock", Duration::MAX);
}
