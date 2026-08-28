//! Maud fragment caching keyed by (identity, version).
//!
//! [`cache_fragment`] returns cached markup on a hit and renders + stores it
//! on a miss. Bumping the `version` token (e.g. `record.updated_at`) causes
//! a miss so writes auto-invalidate without manual eviction.
//!
//! # Usage
//!
//! ```rust,ignore
//! use autumn_web::cache::{cache_fragment, MokaCache};
//! use std::sync::Arc;
//!
//! let cache: Arc<dyn autumn_web::cache::Cache> = Arc::new(MokaCache::new(1_000, None));
//!
//! let markup = cache_fragment(
//!     Some(cache.as_ref()),
//!     format_args!("post:{}", post.id),
//!     post.updated_at.timestamp(),
//!     None,
//!     || html! { h1 { (post.title) } },
//! );
//! ```
//!
//! Pass `None` for the cache (e.g. in dev without Redis/moka) and the helper
//! renders directly without panicking.
//!
//! # Russian-doll nesting
//!
//! Inner fragments cached by their own `(identity, version)` are reused when
//! an outer fragment re-renders. Only the changed inner record's closure runs;
//! unchanged siblings are served from cache. Derive the outer fragment's
//! version from its children (e.g. `max(child.updated_at)`) so the outer
//! re-renders whenever any child changes.

use maud::{Markup, PreEscaped};

use super::{Cache, get_cached, insert_cached};

/// Cache a rendered Maud fragment keyed by `(identity, version)`.
///
/// - **Hit**: returns the cached `Markup` without running `render`.
/// - **Miss**: calls `render()`, stores the result, and returns it.
/// - **No cache** (`cache = None`): calls `render()` on every call — no panic.
///
/// The cache key combines `identity` and `version` (the identity is
/// length-prefixed so a `:` inside it can't alias two fragments). Storing the rendered
/// `String` via [`insert_cached`] means the fragment works with both the
/// moka in-process backend and the Redis shared backend (which serializes via
/// serde JSON), and honours an optional TTL.
///
/// # Arguments
///
/// * `cache`    — the backing store; `None` → fallback render (dev / no cache configured)
/// * `identity` — uniquely identifies *which* fragment (e.g. `"post:42"`)
/// * `version`  — a token that changes when the underlying record changes
///   (e.g. `record.updated_at.timestamp()` or a sequence number)
/// * `ttl`      — optional time-to-live forwarded to the backend (Redis etc.)
/// * `render`   — closure that produces the `Markup`; **not called on a hit**
pub fn cache_fragment(
    cache: Option<&dyn Cache>,
    identity: impl std::fmt::Display,
    version: impl std::fmt::Display,
    ttl: Option<std::time::Duration>,
    render: impl FnOnce() -> Markup,
) -> Markup {
    let Some(cache) = cache else {
        // Graceful fallback: no cache configured → render every time, no panic.
        return render();
    };

    // Length-prefix the identity so a `:` inside it cannot shift the
    // identity/version boundary and alias two distinct fragments — e.g.
    // (identity="a:b", version="c") must not collide with (identity="a",
    // version="b:c"). The byte length pins where the identity ends.
    let identity = identity.to_string();
    let key = format!("fragment:{}:{identity}:{version}", identity.len());

    if let Some(html) = get_cached::<String>(cache, &key) {
        // Hit: reconstruct Markup from the cached String without re-escaping.
        return PreEscaped(html);
    }

    // Miss: render once, store the inner String (serde-transparent for Redis).
    let markup = render();
    insert_cached(cache, &key, markup.0.clone(), ttl);
    markup
}

/// Cache a Maud fragment using the **process-global** cache backend.
///
/// Resolves the cache registered via
/// [`AppBuilder::with_cache_backend`](crate::app::AppBuilder) or
/// [`AppState::set_cache`](crate::state::AppState::set_cache). When no global
/// cache is registered (e.g. during local dev without Redis/moka) the helper
/// renders directly and never panics.
///
/// This is the ergonomic variant for use inside handler/component functions
/// where you don't hold an `Arc<dyn Cache>` directly:
///
/// ```rust,ignore
/// fn post_card(post: &Post) -> Markup {
///     cache_fragment_global(
///         format_args!("post:{}", post.id),
///         post.updated_at.timestamp(),
///         None,
///         || html! { article { h2 { (post.title) } } },
///     )
/// }
/// ```
pub fn cache_fragment_global(
    identity: impl std::fmt::Display,
    version: impl std::fmt::Display,
    ttl: Option<std::time::Duration>,
    render: impl FnOnce() -> Markup,
) -> Markup {
    let global = super::global_cache();
    cache_fragment(global.as_deref(), identity, version, ttl, render)
}

/// Cache a Maud fragment under a **cache-key namespace**, so the entries can be
/// invalidated as a group.
///
/// Identical to [`cache_fragment`] except that the key is prefixed with
/// `namespace`, which is what makes
/// [`invalidate_namespace`](super::coherence::invalidate_namespace) able to
/// reach it. [`cache_fragment`] keys entries under a bare `fragment:` prefix
/// that carries no per-read identity, so *every* fragment shares it and a
/// namespace sweep for one read matches none of them; a
/// [`declare_cached_read!`](crate::declare_cached_read) entry with
/// `kind = Fragment` that wants a working `invalidates(...)` edge has to key
/// through here (#1716).
///
/// `namespace` should be the same string the declaration uses as its `id`.
///
/// ```rust,ignore
/// autumn_web::declare_cached_read! {
///     id = "blog::sidebar_fragment",
///     kind = Fragment,
///     reads = [crate::models::Post],
/// }
///
/// let markup = cache_fragment_in(
///     Some(cache.as_ref()),
///     "blog::sidebar_fragment",
///     format_args!("post:{}", post.id),
///     post.updated_at.timestamp(),
///     None,
///     || html! { h1 { (post.title) } },
/// );
/// ```
///
/// Version-token invalidation still works exactly as it does for
/// [`cache_fragment`]; the namespace is the *additional* handle a repository
/// write can pull.
pub fn cache_fragment_in(
    cache: Option<&dyn Cache>,
    namespace: &str,
    identity: impl std::fmt::Display,
    version: impl std::fmt::Display,
    ttl: Option<std::time::Duration>,
    render: impl FnOnce() -> Markup,
) -> Markup {
    let Some(cache) = cache else {
        return render();
    };

    // Same length-prefixed identity as `cache_fragment`, behind the namespace
    // that `invalidate_namespace` scans for: `{namespace}:` is exactly the
    // prefix `MokaCache` matches on and `RedisCache` builds its `SCAN MATCH`
    // from, so the declared id and the runtime key space finally agree.
    let identity = identity.to_string();
    let key = format!(
        "{namespace}:fragment:{}:{identity}:{version}",
        identity.len()
    );

    if let Some(html) = get_cached::<String>(cache, &key) {
        return PreEscaped(html);
    }

    let markup = render();
    insert_cached(cache, &key, markup.0.clone(), ttl);
    markup
}

/// [`cache_fragment_in`] against the **process-global** cache backend.
///
/// The namespaced counterpart of [`cache_fragment_global`]: use it when the
/// fragment is declared with [`declare_cached_read!`](crate::declare_cached_read)
/// and a repository write is expected to invalidate it.
pub fn cache_fragment_global_in(
    namespace: &str,
    identity: impl std::fmt::Display,
    version: impl std::fmt::Display,
    ttl: Option<std::time::Duration>,
    render: impl FnOnce() -> Markup,
) -> Markup {
    let global = super::global_cache();
    cache_fragment_in(global.as_deref(), namespace, identity, version, ttl, render)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(all(test, feature = "cache-moka"))]
mod tests {
    use std::sync::Arc;
    use std::sync::PoisonError;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use maud::{Markup, html};

    use super::{
        cache_fragment, cache_fragment_global, cache_fragment_global_in, cache_fragment_in,
    };
    use crate::cache::{
        Cache, GLOBAL_CACHE_TEST_LOCK, MokaCache, clear_global_cache, set_global_cache,
    };

    fn make_cache(capacity: u64) -> MokaCache {
        MokaCache::new(capacity, None)
    }

    // ── AC1 + AC5: miss renders+stores; closure NOT executed on a hit ──────

    #[test]
    fn hit_does_not_run_closure() {
        let cache = make_cache(100);
        let counter = Arc::new(AtomicUsize::new(0));

        // First call → miss; closure runs once.
        let first = {
            let counter = counter.clone();
            cache_fragment(Some(&cache), "post:1", "v1", None, move || {
                counter.fetch_add(1, Ordering::SeqCst);
                html! { p { "rendered" } }
            })
        };
        assert_eq!(
            counter.load(Ordering::SeqCst),
            1,
            "miss must run closure once"
        );
        assert!(first.into_string().contains("rendered"));

        // Second call (same identity + version) → hit; closure must NOT run.
        let second = {
            let counter = counter.clone();
            cache_fragment(Some(&cache), "post:1", "v1", None, move || {
                counter.fetch_add(1, Ordering::SeqCst);
                html! { p { "SHOULD NOT APPEAR" } }
            })
        };
        assert_eq!(
            counter.load(Ordering::SeqCst),
            1,
            "hit must not run the render closure"
        );
        assert!(second.into_string().contains("rendered"));
    }

    // ── AC2: version bump → miss → re-render ──────────────────────────────

    #[test]
    fn version_bump_causes_miss() {
        let cache = make_cache(100);
        let counter = Arc::new(AtomicUsize::new(0));

        let v1 = {
            let counter = counter.clone();
            cache_fragment(Some(&cache), "post:7", "2024-01-01", None, move || {
                counter.fetch_add(1, Ordering::SeqCst);
                html! { p { "version-one" } }
            })
        };
        assert_eq!(counter.load(Ordering::SeqCst), 1);
        assert!(v1.into_string().contains("version-one"));

        // Same identity, NEW version → must miss and re-render.
        let v2 = {
            let counter = counter.clone();
            cache_fragment(Some(&cache), "post:7", "2024-06-01", None, move || {
                counter.fetch_add(1, Ordering::SeqCst);
                html! { p { "version-two" } }
            })
        };
        assert_eq!(
            counter.load(Ordering::SeqCst),
            2,
            "version bump must produce a miss"
        );
        assert!(v2.into_string().contains("version-two"));
    }

    // ── AC6: no cache → renders directly, never panics ────────────────────

    #[test]
    fn no_cache_renders_directly_without_panic() {
        let counter = Arc::new(AtomicUsize::new(0));

        for _ in 0..3 {
            let counter = counter.clone();
            let result = cache_fragment(None, "post:99", "v1", None, move || {
                counter.fetch_add(1, Ordering::SeqCst);
                html! { span { "fallback" } }
            });
            assert!(result.into_string().contains("fallback"));
        }

        // No cache → every call renders.
        assert_eq!(
            counter.load(Ordering::SeqCst),
            3,
            "no cache must render on every call"
        );
    }

    // ── AC3: TTL forwarded to backend (insert_cached honours ttl arg) ─────

    #[test]
    fn ttl_parameter_accepted_and_hit_still_works() {
        let cache = make_cache(100);
        let counter = Arc::new(AtomicUsize::new(0));

        // Store with a TTL (in-process moka ignores per-call TTL, but must not panic).
        {
            let counter = counter.clone();
            cache_fragment(
                Some(&cache),
                "post:2",
                "v1",
                Some(Duration::from_secs(60)),
                move || {
                    counter.fetch_add(1, Ordering::SeqCst);
                    html! { em { "ttl-test" } }
                },
            );
        }
        assert_eq!(counter.load(Ordering::SeqCst), 1);

        // Hit must still work immediately after.
        {
            let counter = counter.clone();
            cache_fragment(
                Some(&cache),
                "post:2",
                "v1",
                Some(Duration::from_secs(60)),
                move || {
                    counter.fetch_add(1, Ordering::SeqCst);
                    html! { em { "ttl-test" } }
                },
            );
        }
        assert_eq!(counter.load(Ordering::SeqCst), 1, "hit must not re-render");
    }

    // ── AC4: Russian-doll nesting ─────────────────────────────────────────
    //
    // Two inner fragments live inside an outer fragment. We derive the outer
    // version from its children. When one inner record changes:
    //   - the outer re-renders (its version bumped),
    //   - the changed inner re-renders,
    //   - the *unchanged sibling* is served from cache (closure not run).

    /// Render inner fragment `id` at `version`, counting closure invocations.
    fn inner(cache: &dyn Cache, id: &str, version: &str, counter: &Arc<AtomicUsize>) -> Markup {
        let counter = counter.clone();
        let version_owned = version.to_owned();
        cache_fragment(Some(cache), id, version, None, move || {
            counter.fetch_add(1, Ordering::SeqCst);
            html! { li { "fragment " (version_owned) } }
        })
    }

    #[test]
    fn russian_doll_nesting_sibling_hit_unchanged_inner() {
        let cache = make_cache(100);
        let cache_ref: &dyn Cache = &cache;

        let inner_a = Arc::new(AtomicUsize::new(0));
        let inner_b = Arc::new(AtomicUsize::new(0));
        let outer = Arc::new(AtomicUsize::new(0));

        // --- Pass 1: warm everything (outer v1, a v1, b v1) ---
        {
            let a = inner(cache_ref, "inner:a", "v1", &inner_a);
            let b = inner(cache_ref, "inner:b", "v1", &inner_b);
            let outer_c = outer.clone();
            cache_fragment(Some(cache_ref), "outer:list", "outer-v1", None, move || {
                outer_c.fetch_add(1, Ordering::SeqCst);
                html! { ul { (a) (b) } }
            });
        }
        assert_eq!(inner_a.load(Ordering::SeqCst), 1, "inner-a warmed once");
        assert_eq!(inner_b.load(Ordering::SeqCst), 1, "inner-b warmed once");
        assert_eq!(outer.load(Ordering::SeqCst), 1, "outer warmed once");

        // --- Pass 2: identical → outer hit, inner closures never invoked ---
        {
            let a = inner(cache_ref, "inner:a", "v1", &inner_a);
            let b = inner(cache_ref, "inner:b", "v1", &inner_b);
            let outer_c = outer.clone();
            cache_fragment(Some(cache_ref), "outer:list", "outer-v1", None, move || {
                outer_c.fetch_add(1, Ordering::SeqCst);
                html! { ul { (a) (b) } }
            });
        }
        // The inner() helper above *does* run when the outer is a hit, but it
        // hits its own cache so the closures don't run. The outer itself hits.
        assert_eq!(inner_a.load(Ordering::SeqCst), 1, "inner-a stays cached");
        assert_eq!(inner_b.load(Ordering::SeqCst), 1, "inner-b stays cached");
        assert_eq!(
            outer.load(Ordering::SeqCst),
            1,
            "outer hit: not re-rendered"
        );

        // --- Pass 3: inner-a changes (v2), outer version bumps. ---
        // inner-a re-renders, inner-b sibling stays cached, outer re-renders.
        {
            let a = inner(cache_ref, "inner:a", "v2", &inner_a); // bumped
            let b = inner(cache_ref, "inner:b", "v1", &inner_b); // unchanged
            let outer_c = outer.clone();
            cache_fragment(Some(cache_ref), "outer:list", "outer-v2", None, move || {
                outer_c.fetch_add(1, Ordering::SeqCst);
                html! { ul { (a) (b) } }
            });
        }
        assert_eq!(
            inner_a.load(Ordering::SeqCst),
            2,
            "inner-a re-renders on version bump"
        );
        assert_eq!(
            inner_b.load(Ordering::SeqCst),
            1,
            "inner-b sibling stays cached"
        );
        assert_eq!(
            outer.load(Ordering::SeqCst),
            2,
            "outer re-renders when its version bumped"
        );
    }

    // ── cache_fragment_global: uses process-global cache ─────────────────

    #[test]
    fn global_variant_hits_process_global_cache() {
        let _guard = GLOBAL_CACHE_TEST_LOCK
            .lock()
            .unwrap_or_else(PoisonError::into_inner);

        clear_global_cache();

        let moka = Arc::new(MokaCache::new(100, None));
        set_global_cache(moka as Arc<dyn Cache>);

        let counter = Arc::new(AtomicUsize::new(0));

        {
            let counter = counter.clone();
            cache_fragment_global("post:global", "v1", None, move || {
                counter.fetch_add(1, Ordering::SeqCst);
                html! { div { "global" } }
            });
        }
        assert_eq!(counter.load(Ordering::SeqCst), 1, "first call must miss");

        {
            let counter = counter.clone();
            cache_fragment_global("post:global", "v1", None, move || {
                counter.fetch_add(1, Ordering::SeqCst);
                html! { div { "global" } }
            });
        }
        assert_eq!(counter.load(Ordering::SeqCst), 1, "second call must hit");

        clear_global_cache();
    }

    // ── cache_fragment_global: no global → renders without panicking ──────

    #[test]
    fn global_variant_no_global_cache_renders_fallback() {
        let _guard = GLOBAL_CACHE_TEST_LOCK
            .lock()
            .unwrap_or_else(PoisonError::into_inner);

        clear_global_cache();

        let result = cache_fragment_global("post:fallback", "v1", None, || html! { span { "ok" } });
        assert!(
            result.into_string().contains("ok"),
            "must render when no global cache"
        );
    }

    // ── Returned Markup is byte-identical to the rendered Markup ──────────

    #[test]
    fn cached_markup_matches_rendered_markup() {
        let cache = make_cache(10);

        let first: Markup = cache_fragment(Some(&cache), "post:html", "v1", None, || {
            html! { article { h1 { "Hello" } p { "World" } } }
        });
        let first_html = first.into_string();

        // Retrieve from cache; the alternate closure must be ignored.
        let second: Markup = cache_fragment(Some(&cache), "post:html", "v1", None, || {
            html! { span { "WRONG" } }
        });
        let second_html = second.into_string();

        assert_eq!(first_html, second_html, "cached markup must equal original");
        assert!(second_html.contains("Hello"));
        assert!(second_html.contains("World"));
        assert!(!second_html.contains("WRONG"));
    }

    // ── Different identities are cached independently ─────────────────────

    #[test]
    fn different_identities_are_independent() {
        let cache = make_cache(100);
        let counter = Arc::new(AtomicUsize::new(0));

        let render = |id: &str| {
            let counter = counter.clone();
            cache_fragment(Some(&cache), id, "v1", None, move || {
                counter.fetch_add(1, Ordering::SeqCst);
                html! { span { "x" } }
            });
        };

        render("post:A");
        render("post:B");
        render("post:A"); // hit
        render("post:B"); // hit

        assert_eq!(
            counter.load(Ordering::SeqCst),
            2,
            "each identity must be cached independently"
        );
    }

    // ── Key boundary is unambiguous even when identity contains `:` ───────

    #[test]
    fn colon_in_identity_does_not_alias_distinct_fragments() {
        let cache = make_cache(100);
        let counter = Arc::new(AtomicUsize::new(0));

        // These two pairs join to the same naive "fragment:{id}:{ver}" string
        // ("fragment:a:b:c") but are semantically distinct fragments. The
        // length prefix must keep them apart.
        let first = {
            let counter = counter.clone();
            cache_fragment(Some(&cache), "a:b", "c", None, move || {
                counter.fetch_add(1, Ordering::SeqCst);
                html! { p { "left" } }
            })
        };
        assert_eq!(counter.load(Ordering::SeqCst), 1, "first pair is a miss");
        assert!(first.into_string().contains("left"));

        let second = {
            let counter = counter.clone();
            cache_fragment(Some(&cache), "a", "b:c", None, move || {
                counter.fetch_add(1, Ordering::SeqCst);
                html! { p { "right" } }
            })
        };
        // Must be a MISS (distinct key), not a hit serving "left".
        assert_eq!(
            counter.load(Ordering::SeqCst),
            2,
            "a `:` in the identity must not collide with a different (identity, version) split"
        );
        assert!(second.into_string().contains("right"));
    }

    // ── Namespaced fragments (#1716) ─────────────────────────────────────────

    /// The whole point of the namespaced variant: `invalidate_namespace` has to
    /// be able to reach the entry.
    ///
    /// `cache_fragment` keys under a bare `fragment:` prefix that carries no
    /// per-read identity, so a sweep for `blog::sidebar_fragment` matches
    /// nothing and reports success — a declared `invalidates(...)` edge that
    /// silently clears zero entries. The `_in` variant puts the declared id
    /// where the sweep actually looks.
    #[test]
    fn a_namespaced_fragment_is_reachable_by_namespace_invalidation() {
        let _guard = GLOBAL_CACHE_TEST_LOCK
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        clear_global_cache();

        let cache: Arc<dyn Cache> = Arc::new(MokaCache::new(64, None));
        set_global_cache(cache.clone());

        let counter = Arc::new(AtomicUsize::new(0));
        let render = |counter: Arc<AtomicUsize>| {
            move || -> Markup {
                counter.fetch_add(1, Ordering::SeqCst);
                html! { p { "sidebar" } }
            }
        };

        cache_fragment_global_in(
            "blog::sidebar_fragment",
            "post:1",
            "v1",
            None,
            render(counter.clone()),
        );
        assert_eq!(counter.load(Ordering::SeqCst), 1, "first call is a miss");

        cache_fragment_global_in(
            "blog::sidebar_fragment",
            "post:1",
            "v1",
            None,
            render(counter.clone()),
        );
        assert_eq!(counter.load(Ordering::SeqCst), 1, "second call is a hit");

        assert!(crate::cache::coherence::invalidate_namespace(
            "blog::sidebar_fragment"
        ));

        cache_fragment_global_in(
            "blog::sidebar_fragment",
            "post:1",
            "v1",
            None,
            render(counter.clone()),
        );
        assert_eq!(
            counter.load(Ordering::SeqCst),
            2,
            "the invalidation must have dropped the entry"
        );

        clear_global_cache();
    }

    /// The limitation the `_in` variants exist for, pinned so the docs stay
    /// true.
    ///
    /// `cache_fragment` keys under a bare `fragment:` prefix, which is not the
    /// declared read's id, so a namespace sweep matches nothing — and
    /// `invalidate_namespace` still returns `true`, because "matched no keys"
    /// and "there were none" are the same observation to a prefix scan. That
    /// combination is exactly why a `declare_cached_read!` fragment has to key
    /// through `cache_fragment_in`; if this test ever goes green the other way,
    /// the guide's warning is stale.
    #[test]
    fn a_plain_fragment_is_not_reachable_by_namespace_invalidation() {
        let _guard = GLOBAL_CACHE_TEST_LOCK
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        clear_global_cache();

        let cache: Arc<dyn Cache> = Arc::new(MokaCache::new(64, None));
        set_global_cache(cache.clone());

        let counter = Arc::new(AtomicUsize::new(0));
        let render = |counter: Arc<AtomicUsize>| {
            move || -> Markup {
                counter.fetch_add(1, Ordering::SeqCst);
                html! { p { "sidebar" } }
            }
        };

        cache_fragment_global("post:1", "v1", None, render(counter.clone()));
        assert_eq!(counter.load(Ordering::SeqCst), 1);

        // Reports success...
        assert!(crate::cache::coherence::invalidate_namespace(
            "blog::sidebar_fragment"
        ));

        // ...having cleared nothing: the entry is still served.
        cache_fragment_global("post:1", "v1", None, render(counter.clone()));
        assert_eq!(
            counter.load(Ordering::SeqCst),
            1,
            "if this became a miss, `cache_fragment` gained a namespace and the \
             docs steering declared fragments to `cache_fragment_in` are stale"
        );

        clear_global_cache();
    }

    /// A namespace sweep must not take another read's fragments with it.
    #[test]
    fn invalidating_one_fragment_namespace_leaves_the_others() {
        let _guard = GLOBAL_CACHE_TEST_LOCK
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        clear_global_cache();

        let cache: Arc<dyn Cache> = Arc::new(MokaCache::new(64, None));
        set_global_cache(cache.clone());

        let other = Arc::new(AtomicUsize::new(0));
        let render = |counter: Arc<AtomicUsize>| {
            move || -> Markup {
                counter.fetch_add(1, Ordering::SeqCst);
                html! { p { "footer" } }
            }
        };

        cache_fragment_global_in("blog::sidebar", "post:1", "v1", None, render(other.clone()));
        cache_fragment_global_in("blog::footer", "post:1", "v1", None, render(other.clone()));
        assert_eq!(other.load(Ordering::SeqCst), 2);

        assert!(crate::cache::coherence::invalidate_namespace(
            "blog::sidebar"
        ));

        cache_fragment_global_in("blog::footer", "post:1", "v1", None, render(other.clone()));
        assert_eq!(
            other.load(Ordering::SeqCst),
            2,
            "the footer fragment must survive a sidebar invalidation"
        );

        clear_global_cache();
    }

    /// The namespace prefix must not weaken the identity/version boundary the
    /// unnamespaced helper is careful about.
    #[test]
    fn a_namespaced_identity_containing_a_colon_still_cannot_alias() {
        let cache = MokaCache::new(64, None);
        let counter = Arc::new(AtomicUsize::new(0));

        let first = {
            let counter = counter.clone();
            cache_fragment_in(Some(&cache), "ns", "a:b", "c", None, move || {
                counter.fetch_add(1, Ordering::SeqCst);
                html! { p { "left" } }
            })
        };
        assert!(first.into_string().contains("left"));

        let second = {
            let counter = counter.clone();
            cache_fragment_in(Some(&cache), "ns", "a", "b:c", None, move || {
                counter.fetch_add(1, Ordering::SeqCst);
                html! { p { "right" } }
            })
        };
        assert_eq!(
            counter.load(Ordering::SeqCst),
            2,
            "a `:` in the identity must not collide with a different split"
        );
        assert!(second.into_string().contains("right"));
    }

    /// Two namespaces must not alias each other through the identity either.
    #[test]
    fn two_namespaces_do_not_share_a_fragment() {
        let cache = MokaCache::new(64, None);
        let counter = Arc::new(AtomicUsize::new(0));

        for ns in ["a", "b"] {
            let counter = counter.clone();
            cache_fragment_in(Some(&cache), ns, "post:1", "v1", None, move || {
                counter.fetch_add(1, Ordering::SeqCst);
                html! { p { "x" } }
            });
        }
        assert_eq!(
            counter.load(Ordering::SeqCst),
            2,
            "each namespace owns its own entry"
        );
    }

    /// The `None` fallback has to behave the same as the unnamespaced helper.
    #[test]
    fn a_namespaced_fragment_without_a_cache_still_renders() {
        let counter = Arc::new(AtomicUsize::new(0));
        for _ in 0..2 {
            let counter = counter.clone();
            cache_fragment_in(None, "ns", "post:1", "v1", None, move || {
                counter.fetch_add(1, Ordering::SeqCst);
                html! { p { "x" } }
            });
        }
        assert_eq!(
            counter.load(Ordering::SeqCst),
            2,
            "no cache → always render"
        );
    }
}
