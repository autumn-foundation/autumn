//! Isolated: the capsule cache seam (issue #1634), split out of
//! `tests/integration/failure_capsule_effects.rs`.
//!
//! This test is the one capsule-effect case that installs a **process-global**
//! cache backend (`set_global_cache`) and then depends on it surviving across
//! two requests. `TestApp::build` clears that global unconditionally — under
//! `GLOBAL_CACHE_TEST_LOCK`, but that lock only guards `build`'s own critical
//! section, not another test's set-then-read window — so any concurrently
//! building test in the same process wipes the backend mid-flight. When that
//! happens the second request finds no global cache, falls through to the
//! per-function Moka store, and records **no** cache effects at all, so the
//! capsule that should carry the hit comes back empty:
//!
//! ```text
//! one capsule must record the hit *with the value it served*:
//!   [[], [Get { key: "widgets:count", value: None }, Insert { .. }]]
//! ```
//!
//! That is a process-wide side effect, which per CLAUDE.md's "Isolated
//! Integration Tests" rule belongs in its own binary — where it is the only
//! test running and nothing else can clear the global out from under it. The
//! two other global-cache suites (`cache_global_integration`,
//! `cached_global_backend`) are isolated for exactly this reason; this one was
//! the outlier, and it only stayed green because the consolidated binary used
//! to run at low instantaneous concurrency.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use autumn_web::capsule::{
    CacheEffect, Capsule, DivergenceLog, ReplayFixtures, Verdict, execute, load_capsule,
};
use autumn_web::config::AutumnConfig;
use autumn_web::prelude::*;
use autumn_web::test::TestApp;

/// Reads the cache, then fails quoting what it found.
#[get("/cached")]
async fn cached() -> Result<&'static str, AutumnError> {
    let Some(cache) = autumn_web::cache::global_cache() else {
        return Err(AutumnError::internal_server_error_msg("no cache"));
    };
    let hit: Option<u32> = autumn_web::cache::get_cached(cache.as_ref(), "widgets:count");
    let Some(count) = hit else {
        autumn_web::cache::insert_cached(cache.as_ref(), "widgets:count", 41_u32, None);
        return Err(AutumnError::internal_server_error_msg("cache miss"));
    };
    Err(AutumnError::internal_server_error_msg(format!(
        "cache said {count}"
    )))
}

fn capture_config(dir: &Path) -> AutumnConfig {
    let mut config = AutumnConfig {
        profile: Some("test".into()),
        ..AutumnConfig::default()
    };
    config.security.csrf.enabled = false;
    config.failure_capture.enabled = true;
    config.failure_capture.dir = dir.to_string_lossy().into_owned();
    config
}

fn replay_config() -> AutumnConfig {
    let mut config = AutumnConfig {
        profile: Some("test".into()),
        ..AutumnConfig::default()
    };
    config.security.csrf.enabled = false;
    config
}

fn capsule_paths(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    paths.sort();
    paths
}

async fn await_capsules(dir: &Path, expected: usize) -> Vec<PathBuf> {
    for _ in 0..200 {
        let paths = capsule_paths(dir);
        if paths.len() >= expected {
            return paths;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!(
        "expected {expected} capsule(s) in {}, found {}",
        dir.display(),
        capsule_paths(dir).len()
    );
}

#[tokio::test]
async fn a_cache_hit_is_captured_and_replayed_without_a_backend() {
    let dir = tempfile::tempdir().expect("tempdir");
    // The backend is installed *after* `build`, not before: `TestApp::build`
    // clears the process-global cache (and takes the lock that guards it), so a
    // backend set up front would be wiped — and a test holding that lock across
    // `build` would deadlock on it.
    let client = TestApp::new()
        .config(capture_config(dir.path()))
        .routes(routes![cached])
        .build();
    autumn_web::cache::set_global_cache(Arc::new(autumn_web::cache::MokaCache::new(16, None)));
    // First call misses and fills, second call hits: two capsules, one per
    // branch, so both the write and the read are on tape.
    client.get("/cached").send().await.assert_status(500);
    client.get("/cached").send().await.assert_status(500);
    let capsules: Vec<Capsule> = await_capsules(dir.path(), 2)
        .await
        .iter()
        .map(|path| load_capsule(path).expect("the capsule loads"))
        .collect();
    // Matched by content, not by file order: persistence runs on a detached
    // blocking task, so the two capsules can land in either order.
    assert!(
        capsules
            .iter()
            .any(|capsule| capsule.effects.cache.iter().any(
                |entry| matches!(entry, CacheEffect::Insert { key, .. } if key == "widgets:count")
            )),
        "the fill must be recorded: {:?}",
        capsules
            .iter()
            .map(|capsule| &capsule.effects.cache)
            .collect::<Vec<_>>()
    );
    let hit = capsules
        .iter()
        .find(|capsule| {
            capsule.effects.cache.iter().any(|entry| {
                matches!(entry, CacheEffect::Get { key, value: Some(_) } if key == "widgets:count")
            })
        })
        .cloned()
        .unwrap_or_else(|| {
            panic!(
                "one capsule must record the hit *with the value it served*: {:?}",
                capsules
                    .iter()
                    .map(|capsule| &capsule.effects.cache)
                    .collect::<Vec<_>>()
            )
        });

    // Replay against an **empty** backend. The seam sits inside `get_cached`,
    // so a cache object still has to exist for the handler to read through —
    // but it holds nothing, so a hit can only have come from the capsule.
    let fixtures = ReplayFixtures::from_capsule(&hit);
    let router = TestApp::new()
        .config(replay_config())
        .routes(routes![cached])
        .with_clock(fixtures.clock())
        .build()
        .into_router();
    autumn_web::cache::set_global_cache(Arc::new(autumn_web::cache::MokaCache::new(16, None)));
    let outcome = execute(router, &hit, Arc::new(DivergenceLog::new()), &fixtures).await;

    assert_eq!(
        outcome.verdict,
        Verdict::Reproduced,
        "the recorded cache hit must replay without a backend: {outcome:?}"
    );

    autumn_web::cache::clear_global_cache();
}
