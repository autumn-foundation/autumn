//! Integration tests for Maud fragment caching (issue #1040).
//!
//! Verifies the end-to-end story:
//! - a fragment cached via the registered `AppState` backend is reused on a hit,
//! - a "write" (version-token bump, e.g. `record.updated_at`) re-renders the
//!   fragment on the very next read — **0 stale renders** (the Success Metric).

#![cfg(all(feature = "maud", feature = "cache-moka"))]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use autumn_web::AppState;
use autumn_web::cache::{Cache, MokaCache, cache_fragment};
use maud::html;

/// A toy record with an `updated_at`-style version token.
struct Post {
    id: i64,
    title: &'static str,
    version: i64, // stands in for `updated_at.timestamp()`
}

/// Render a post card, caching it keyed by `(post.id, post.version)`.
/// `counter` increments only when the render closure actually runs (a miss).
fn render_post_card(cache: &dyn Cache, post: &Post, counter: &Arc<AtomicUsize>) -> String {
    let counter = counter.clone();
    let title = post.title;
    cache_fragment(
        Some(cache),
        format_args!("post:{}", post.id),
        post.version,
        None,
        move || {
            counter.fetch_add(1, Ordering::SeqCst);
            html! { article { h2 { (title) } } }
        },
    )
    .into_string()
}

#[test]
fn fragment_served_from_app_state_cache_on_hit() {
    let moka = Arc::new(MokaCache::new(100, None));
    let state = AppState::for_test().with_cache(moka as Arc<dyn Cache>);
    let cache = state.cache().expect("cache registered");

    let counter = Arc::new(AtomicUsize::new(0));
    let post = Post {
        id: 1,
        title: "Hello",
        version: 1,
    };

    // First read → miss → render once.
    let first = render_post_card(cache.as_ref(), &post, &counter);
    assert_eq!(counter.load(Ordering::SeqCst), 1, "first read is a miss");
    assert!(first.contains("Hello"));

    // Second read of the same version → hit → no re-render.
    let second = render_post_card(cache.as_ref(), &post, &counter);
    assert_eq!(
        counter.load(Ordering::SeqCst),
        1,
        "second read is a cache hit"
    );
    assert_eq!(first, second, "cached markup is identical");
}

#[test]
fn write_then_read_produces_zero_stale_renders() {
    let moka = Arc::new(MokaCache::new(100, None));
    let state = AppState::for_test().with_cache(moka as Arc<dyn Cache>);
    let cache = state.cache().expect("cache registered");

    let counter = Arc::new(AtomicUsize::new(0));

    // v1 of the record, warmed into the cache.
    let v1 = Post {
        id: 42,
        title: "Original title",
        version: 1,
    };
    let rendered_v1 = render_post_card(cache.as_ref(), &v1, &counter);
    assert!(rendered_v1.contains("Original title"));
    assert_eq!(counter.load(Ordering::SeqCst), 1);

    // A write bumps the version token (updated_at changes) and the title.
    let v2 = Post {
        id: 42,
        title: "Edited title",
        version: 2,
    };

    // The very next read must reflect the new content — no stale render.
    let rendered_v2 = render_post_card(cache.as_ref(), &v2, &counter);
    assert_eq!(
        counter.load(Ordering::SeqCst),
        2,
        "write must force a re-render"
    );
    assert!(
        rendered_v2.contains("Edited title"),
        "next read after a write must show fresh content, not the stale cached fragment"
    );
    assert!(
        !rendered_v2.contains("Original title"),
        "0 stale renders: the old fragment must never be served after a write"
    );
}

#[test]
fn list_view_reuses_unchanged_rows() {
    // A 3-row list. Re-rendering the whole list a second time should hit the
    // cache for every unchanged row (closures never re-run).
    let moka = Arc::new(MokaCache::new(100, None));
    let state = AppState::for_test().with_cache(moka as Arc<dyn Cache>);
    let cache = state.cache().expect("cache registered");

    let counter = Arc::new(AtomicUsize::new(0));
    let posts = [
        Post {
            id: 1,
            title: "One",
            version: 1,
        },
        Post {
            id: 2,
            title: "Two",
            version: 1,
        },
        Post {
            id: 3,
            title: "Three",
            version: 1,
        },
    ];

    // Cold render of the list: 3 misses.
    for p in &posts {
        render_post_card(cache.as_ref(), p, &counter);
    }
    assert_eq!(
        counter.load(Ordering::SeqCst),
        3,
        "cold list renders every row"
    );

    // Warm render of the same list: 0 additional renders.
    for p in &posts {
        render_post_card(cache.as_ref(), p, &counter);
    }
    assert_eq!(
        counter.load(Ordering::SeqCst),
        3,
        "warm list serves every unchanged row from cache"
    );
}

#[test]
fn no_cache_configured_falls_back_gracefully() {
    // AppState with no cache backend → cache() is None → render directly.
    let state = AppState::for_test();
    assert!(state.cache().is_none());

    let counter = Arc::new(AtomicUsize::new(0));
    let post = Post {
        id: 1,
        title: "Dev",
        version: 1,
    };

    for _ in 0..2 {
        let rendered = render_post_card_optional(state.cache().as_deref(), &post, &counter);
        assert!(rendered.contains("Dev"));
    }
    // No cache → renders every time, never panics.
    assert_eq!(counter.load(Ordering::SeqCst), 2);
}

/// Variant that accepts an optional cache, mirroring `AppState::cache()`.
fn render_post_card_optional(
    cache: Option<&dyn Cache>,
    post: &Post,
    counter: &Arc<AtomicUsize>,
) -> String {
    let counter = counter.clone();
    let title = post.title;
    cache_fragment(
        cache,
        format_args!("post:{}", post.id),
        post.version,
        None,
        move || {
            counter.fetch_add(1, Ordering::SeqCst);
            html! { article { h2 { (title) } } }
        },
    )
    .into_string()
}
