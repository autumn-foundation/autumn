//! Cross-tenant `cache_fragment_global` read (Warden 2026-09-21).
//!
//! Composes two independently documented Autumn features exactly as their own
//! guides show them, with no deviation:
//!
//! - `docs/guide/fragment-caching.md`'s own Quick Start caches a fragment
//!   keyed on the record's primary key: `format_args!("post_card:{}", post.id)`.
//! - `docs/guide/conditional-get.md`'s "Integration with `#[lock_version]`"
//!   section promotes `post.lock_version` as *the* idiomatic per-record
//!   version token — "the idiomatic combo when a model has both" a timestamp
//!   and an optimistic-lock counter — as an alternative to a microsecond
//!   timestamp for exactly this kind of "version changed → re-render" key.
//! - `docs/guide/sharding.md`'s resharding runbook says outright that a
//!   sharded table's primary key is a **shard-local `BIGSERIAL`**: "the
//!   shard-local `BIGSERIAL` id is not copied, so re-runs never collide on
//!   the primary key" (of the *destination* shard) — i.e. every shard hands
//!   out its own `1, 2, 3, …` independently. `#[repository(tenant_scoped,
//!   sharded)]` is a first-class, documented Autumn feature
//!   (`docs/guide/sharding.md`).
//!
//! Put together: on a sharded, tenant-scoped deployment, two different
//! tenants' *first ever* row of the same model land on different shards and
//! independently get `id = 1`. A freshly inserted row's `#[lock_version]`
//! also starts at the same initial value for every tenant, deterministically
//! — no timing, no guessing, no brute force. So tenant A's first post and
//! tenant B's first post can both resolve to `cache_fragment_global`'s exact
//! same `(identity, version)` pair the instant both exist, purely from
//! following the framework's own two guides. `cache_fragment_global`'s key
//! (`autumn/src/cache/fragment.rs`) is built *only* from the caller-supplied
//! `identity`/`version` against the **process-global** cache backend
//! (`cache-coherence.md`'s own words: "shared across replicas") — it never
//! consults the ambient `CURRENT_TENANT` task-local the way every other
//! tenant-scoped Autumn primitive does (`tenant_scoped` repository finders,
//! `save()`, preload, retention sweeps, and — after Warden's 2026-09-05 fix —
//! `#[cached]` itself, which now folds `CURRENT_TENANT` into its key
//! unconditionally; see `autumn-macros/src/cached.rs` and
//! `docs/security/2026-09-05-cached-tenant-key/`). `cache_fragment_global` is
//! the one place left where that ambient-scoping idiom silently does not
//! apply, and nothing in the docs warns that a record's own primary key is
//! not a safe fragment identity once sharding is in play.
//!
//! This test does not need a real sharded Postgres cluster to prove the
//! bug: the vulnerable code path is entirely inside `cache_fragment_global`'s
//! key construction, which is agnostic to *how* the caller's `id` and
//! `lock_version` arrived at the same values — only that they did. Two
//! synthetic "first post" records (one per tenant), each `id = 1` and
//! `lock_version = 1`, reproduce exactly the state a real shard-local
//! `BIGSERIAL` + a fresh `#[lock_version]` row produce on their respective
//! shards.
//!
//! Isolated (own `[[test]]` binary, not the consolidated `integration_tests`
//! binary): `cache_fragment_global` reads and writes the **process-global**
//! cache backend (`autumn_web::cache::{set_global_cache, global_cache,
//! clear_global_cache}`), which `TestApp::build` clears unconditionally and
//! which any concurrently running consolidated-binary test could otherwise
//! stomp on — the same reason `cache_global_integration` and
//! `cached_global_backend` are isolated (CLAUDE.md, "Integration Test Layout
//! Guidelines" § Isolated tests, "Has process-wide side effects").

use std::sync::Arc;

use autumn_web::cache::{MokaCache, clear_global_cache, set_global_cache};
use autumn_web::config::AutumnConfig;
use autumn_web::prelude::{Markup, Tenant, cache_fragment_global, html};
use autumn_web::test::TestApp;
use autumn_web::{AutumnResult, get, public, routes};

// No cross-test mutex around the process-global cache here (contrast
// `tests/cached_global_backend.rs`, which has several tests sharing one
// process): this file is its own isolated `[[test]]` binary — its own OS
// process — with exactly one test function, so there is no other in-process
// test to race with `GLOBAL_CACHE`.

/// A synthetic "first post" row. Stands in for a real `#[repository(tenant_scoped,
/// sharded)]` model row — see the module doc for why `id` and `lock_version`
/// naturally collide across tenants for a first row on two different shards.
struct FirstPost {
    id: i64,
    lock_version: i64,
    sentinel_body: &'static str,
}

const fn tenant_a_first_post() -> FirstPost {
    FirstPost {
        id: 1,
        lock_version: 1,
        sentinel_body: "tenant-a-private-sentinel",
    }
}

const fn tenant_b_first_post() -> FirstPost {
    FirstPost {
        id: 1,
        lock_version: 1,
        sentinel_body: "tenant-b-private-sentinel",
    }
}

/// Exactly `docs/guide/fragment-caching.md`'s Quick Start
/// (`format_args!("post_card:{}", post.id)`) as the identity, and
/// `post.lock_version` — `docs/guide/conditional-get.md`'s documented
/// idiomatic version token — as the version.
fn post_card(post: &FirstPost) -> Markup {
    cache_fragment_global(
        format_args!("post_card:{}", post.id),
        post.lock_version,
        None,
        || html! { p { (post.sentinel_body) } },
    )
}

/// Mirrors the framework's own tenancy idiom: the handler never names a
/// tenant explicitly, exactly like a `tenant_scoped` repository read would
/// resolve it ambiently. Which tenant's "first post" gets rendered is decided
/// purely by which tenant's request this is.
#[get("/post-card")]
#[public]
async fn post_card_route(tenant: Tenant) -> AutumnResult<Markup> {
    let post = match tenant.0.as_str() {
        "tenant-a" => tenant_a_first_post(),
        "tenant-b" => tenant_b_first_post(),
        other => panic!("unexpected tenant in test: {other}"),
    };
    Ok(post_card(&post))
}

fn tenancy_config() -> AutumnConfig {
    let mut config = AutumnConfig::default();
    config.tenancy.enabled = true;
    "header".clone_into(&mut config.tenancy.source);
    "x-tenant-id".clone_into(&mut config.tenancy.header_name);
    config
}

/// Issue: tenant B receives tenant A's cached `post_card` fragment because
/// `cache_fragment_global`'s key is `(identity, version)` alone — no tenant
/// component — and both tenants' first post is `id = 1, lock_version = 1`.
#[tokio::test]
async fn fragment_cache_leaks_across_tenants_on_first_row_collision() {
    clear_global_cache();
    set_global_cache(Arc::new(MokaCache::new(100, None)));

    let app = TestApp::new()
        .config(tenancy_config())
        .routes(routes![post_card_route])
        .build();

    // `TestApp::build` clears the process-global cache unconditionally (so
    // `#[cached]` functions in unrelated tests don't inherit a stale
    // backend) — (re-)install ours *after* build, before any request.
    set_global_cache(Arc::new(MokaCache::new(100, None)));

    let first = app
        .get("/post-card")
        .header("x-tenant-id", "tenant-a")
        .send()
        .await;
    first.assert_ok();
    assert!(
        first.text().contains("tenant-a-private-sentinel"),
        "tenant A's own request must see its own sentinel: {:?}",
        first.text()
    );

    let second = app
        .get("/post-card")
        .header("x-tenant-id", "tenant-b")
        .send()
        .await;
    second.assert_ok();
    assert!(
        !second.text().contains("tenant-a-private-sentinel"),
        "tenant B received tenant A's cached fragment: {:?} \
         (cache_fragment_global's key never carried the resolved tenant, and \
         both tenants' first row is id=1, lock_version=1)",
        second.text()
    );
    assert!(
        second.text().contains("tenant-b-private-sentinel"),
        "tenant B must see its own content once isolated: {:?}",
        second.text()
    );

    clear_global_cache();
}
