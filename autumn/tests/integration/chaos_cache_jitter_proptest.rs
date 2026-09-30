use autumn_web::cache::jittered_ttl;
use proptest::prelude::*;
use std::time::Duration;

proptest! {
    #[test]
    fn test_jittered_ttl_no_panic(
        secs in 0u64..u64::MAX,
        nanos in 0u32..1_000_000_000,
        fraction in 0.0f64..2.0f64
    ) {
        let base = Duration::new(secs, nanos);
        // This will panic if base is very large and the random factor > 1.0
        let _ = jittered_ttl(base, fraction);
    }
}
