//! `#[cached_impl]` keeps same-named associated functions apart (#2358).

use autumn_web::cache::coherence::{invalidate_namespace, registered_reads};
use autumn_web::{cached, cached_impl};
use std::sync::atomic::{AtomicU32, Ordering};

mod catalog {
    use super::*;

    pub static CALLS: AtomicU32 = AtomicU32::new(0);

    pub struct Products;
    pub struct Reviews;

    #[cached_impl]
    impl Products {
        #[cached]
        pub async fn get(id: i64) -> String {
            CALLS.fetch_add(1, Ordering::SeqCst);
            format!("product-{id}")
        }
    }

    #[cached_impl]
    impl Reviews {
        #[cached(ttl = "5m")]
        pub async fn get(id: i64) -> String {
            CALLS.fetch_add(1, Ordering::SeqCst);
            format!("review-{id}")
        }
    }
}

#[test]
fn same_named_methods_get_distinct_identities() {
    let products = catalog::Products::__AUTUMN_CACHE_READ_ID__get;
    let reviews = catalog::Reviews::__AUTUMN_CACHE_READ_ID__get;
    assert_ne!(products, reviews);
    assert!(products.ends_with("catalog::Products::get"), "{products}");
    assert!(reviews.ends_with("catalog::Reviews::get"), "{reviews}");
}

#[test]
fn the_manifest_lists_each_read_once() {
    let reads = registered_reads();
    let ids: Vec<&str> = reads.iter().map(|r| r.id.as_str()).collect();
    for id in [
        catalog::Products::__AUTUMN_CACHE_READ_ID__get,
        catalog::Reviews::__AUTUMN_CACHE_READ_ID__get,
    ] {
        assert_eq!(ids.iter().filter(|i| **i == id).count(), 1, "{id}");
    }
}

#[tokio::test]
async fn each_method_serves_its_own_value() {
    assert_eq!(catalog::Products::get(1).await, "product-1");
    assert_eq!(catalog::Reviews::get(1).await, "review-1");
    assert_eq!(catalog::Products::get(1).await, "product-1");
    // Invalidating one identity leaves the other untouched.
    let _ = invalidate_namespace(catalog::Products::__AUTUMN_CACHE_READ_ID__get);
    assert_eq!(catalog::Reviews::get(1).await, "review-1");
}
