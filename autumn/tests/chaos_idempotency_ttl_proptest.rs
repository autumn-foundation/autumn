use autumn_web::idempotency::{IdempotencyRecord, IdempotencyStore, MemoryIdempotencyStore};
use proptest::prelude::*;
use std::time::Duration;

proptest! {
    #[test]
    fn test_memory_idempotency_store_does_not_panic(ttl in any::<Duration>()) {
        let store = MemoryIdempotencyStore::new(Duration::from_secs(60));
        let record = IdempotencyRecord {
            status: 200,
            headers: vec![],
            body: vec![],
            metadata: vec![],
        };
        // This should not panic now with the safe math applied
        let _ = store.try_set("test_key", record, vec![], ttl);
    }
}
