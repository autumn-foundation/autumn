use autumn_web::idempotency::{IdempotencyRecord, IdempotencyStore, MemoryIdempotencyStore};
use proptest::prelude::*;
use proptest::test_runner::Config;
use std::time::Duration;

proptest! {
    #![proptest_config(Config::with_cases(100))]
    #[test]
    fn test_try_lock_ttl_panic(
        ttl_secs in any::<u64>(),
    ) {
        let store = MemoryIdempotencyStore::new(Duration::from_secs(60));
        let _ = store.try_lock_owned("key1", "owner1", Duration::from_secs(ttl_secs));
    }

    #[test]
    fn test_set_ttl_panic(
        ttl_secs in any::<u64>(),
    ) {
        let store = MemoryIdempotencyStore::new(Duration::from_secs(60));
        let record = IdempotencyRecord {
            status: 200,
            headers: Default::default(),
            body: vec![],
            metadata: Default::default(),
        };
        store.set("key1", record, vec![], Duration::from_secs(ttl_secs));
    }
}
