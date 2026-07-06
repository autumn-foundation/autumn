use proptest::prelude::*;
use std::time::Duration;
use autumn_web::idempotency::MemoryIdempotencyStore;
use autumn_web::idempotency::IdempotencyStore;
use autumn_web::idempotency::IdempotencyRecord;

proptest! {
    #[test]
    fn idempotency_ttl_fuzz(
        lock_ttl_secs in any::<u64>(),
        ttl_secs in any::<u64>(),
    ) {
        let store = MemoryIdempotencyStore::new(Duration::from_secs(60));
        let lock_ttl = Duration::from_secs(lock_ttl_secs);
        store.try_lock_owned("my_key", "t1", lock_ttl);

        // If ttl_secs is large, adding it to Instant::now() panics.
        // This simulates a large value from a user configuring a route's TTL.
        let ttl = Duration::from_secs(ttl_secs);
        store.set("my_key", IdempotencyRecord {
            status: 200,
            headers: vec![],
            body: vec![],
            metadata: vec![],
        }, vec![], ttl);
    }
}
