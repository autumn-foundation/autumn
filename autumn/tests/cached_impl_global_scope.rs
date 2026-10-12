//! `#[cached_impl]` against a shared backend (#2358).
//!
//! Isolated: it installs the process-global cache.

use std::sync::atomic::{AtomicU32, Ordering};

use autumn_web::cache::{MokaCache, clear_global_cache, set_global_cache};
use autumn_web::prelude::*;

static CALLS: AtomicU32 = AtomicU32::new(0);

struct Products;
struct Reviews;

#[cached_impl]
impl Products {
    #[cached]
    async fn get(id: i64) -> String {
        CALLS.fetch_add(1, Ordering::SeqCst);
        format!("product-{id}")
    }
}

#[cached_impl]
impl Reviews {
    #[cached]
    async fn get(id: i64) -> String {
        CALLS.fetch_add(1, Ordering::SeqCst);
        format!("review-{id}")
    }
}

#[tokio::test]
async fn same_named_methods_do_not_share_a_global_slot() {
    set_global_cache(std::sync::Arc::new(MokaCache::new(100, None)));

    assert_eq!(Products::get(1).await, "product-1");
    assert_eq!(Reviews::get(1).await, "review-1");
    assert_eq!(CALLS.load(Ordering::SeqCst), 2);

    // Both are hits now.
    assert_eq!(Products::get(1).await, "product-1");
    assert_eq!(Reviews::get(1).await, "review-1");
    assert_eq!(CALLS.load(Ordering::SeqCst), 2);

    clear_global_cache();
}
