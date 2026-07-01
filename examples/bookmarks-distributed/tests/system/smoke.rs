//! Baseline Chromium smoke — issue #1192.
//!
//! Spawns the real `bookmarks-distributed` binary against **two** ephemeral
//! testcontainer Postgres instances standing in for the primary/replica
//! pair, drives a headless-Chromium browser against the bookmarks list, and
//! asserts expected content with no uncaught console errors — proving the
//! dual-pool primary/replica config still boots and serves.
//!
//! `bookmarks-distributed`'s database config (`src/config.rs`) is a bespoke
//! loader (`DistributedConfig::load()`), not the framework's own
//! `AutumnConfig` — it only reads `primary_url`/`replica_url` from
//! `autumn.toml` (+ profile overlay) on disk, with no env-var override.
//! `AUTUMN_MANIFEST_DIR` redirects **both** that loader and the framework's
//! own config to a scratch directory containing a minimal `autumn.toml`
//! carrying the two testcontainer URLs.
//!
//! `AUTUMN_MANIFEST_DIR` also redirects the framework's `/static` file
//! serving (`project_dir("static", ...)` in `router.rs`) to
//! `<AUTUMN_MANIFEST_DIR>/static` — so the scratch dir gets a symlink to the
//! real project's `static/` alongside the scratch `autumn.toml`, keeping the
//! rendered page's actual `<link>`/`<script>` asset paths (e.g.
//! `static/css/autumn.css`) served exactly as the unmodified binary would.
//!
//! The framework only auto-migrates the *primary* on boot (a real replica
//! already has the schema via streaming replication); since the "replica"
//! here is a second independent empty database rather than a true
//! replication target, this test migrates both explicitly before spawning
//! so replica-routed reads see the same schema.
//!
//! Out of scope (per issue #1192): the real docker-compose topology
//! (nginx + multiple web replicas + actual Postgres streaming replication).
//! This proves the *primary/replica dual-pool feature* boots correctly in a
//! single process, not the full compose integration.
//!
//! Run (requires Chromium + Docker):
//!   cargo test -p bookmarks-distributed --features system-tests --test smoke -- --include-ignored

#![cfg(feature = "system-tests")]

use diesel_migrations::{EmbeddedMigrations, embed_migrations};

const MIGRATIONS: EmbeddedMigrations = embed_migrations!("migrations");

/// A dev-only placeholder signing secret — 64 hex chars (32 bytes), the
/// framework's `security.signing_secret` minimum. Never used outside this
/// process; each test run gets an ephemeral testcontainer database anyway.
const TEST_SIGNING_SECRET: &str = "e2e0123456789abcdef0123456789abcdef0123456789abcdef0123456789a";

/// Build a scratch directory unique to `(this process, test_name)` — plain
/// `std::process::id()` alone would collide if `cargo test` runs more than
/// one `#[tokio::test]` from this binary concurrently (the default).
fn scratch_dir(test_name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "bookmarks-distributed-smoke-{}-{test_name}",
        std::process::id()
    ))
}

/// Write a scratch `autumn.toml` (`database` config plus whatever
/// `extra_toml` sections the caller needs) and symlink `static/` into it —
/// see module docs for why both are necessary under `AUTUMN_MANIFEST_DIR`.
fn setup_scratch_dir(
    dir: &std::path::Path,
    primary_url: &str,
    replica_url: &str,
    extra_toml: &str,
) {
    std::fs::create_dir_all(dir).expect("create scratch config dir");
    std::fs::write(
        dir.join("autumn.toml"),
        format!(
            r#"
[health]
path = "/health"

[database]
primary_url = "{primary_url}"
replica_url = "{replica_url}"
primary_pool_size = 2
replica_pool_size = 2
replica_fallback = "fail_readiness"

{extra_toml}
"#
        ),
    )
    .expect("write scratch autumn.toml");

    #[cfg(unix)]
    std::os::unix::fs::symlink(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("static"),
        dir.join("static"),
    )
    .expect("symlink scratch static dir to the real project's static dir");
}

#[tokio::test]
#[ignore = "requires Chromium + Docker — set AUTUMN_CHROMIUM or install chromium-browser"]
async fn bookmarks_distributed_boots_with_dual_pools() {
    let db = example_e2e::provision_postgres(2).await;
    let primary_url = &db.urls()[0];
    let replica_url = &db.urls()[1];

    // Mirror the schema onto both — see module docs for why.
    autumn_web::migrate::run_pending(primary_url, MIGRATIONS)
        .expect("migrate primary testcontainer");
    autumn_web::migrate::run_pending(replica_url, MIGRATIONS)
        .expect("migrate replica-stand-in testcontainer");

    let scratch_dir = scratch_dir("boots_with_dual_pools");
    setup_scratch_dir(&scratch_dir, primary_url, replica_url, "");

    let app = example_e2e::spawn_example(
        env!("CARGO_BIN_EXE_bookmarks-distributed"),
        env!("CARGO_MANIFEST_DIR"),
        &[(
            "AUTUMN_MANIFEST_DIR",
            scratch_dir.to_str().expect("scratch dir path is UTF-8"),
        )],
        example_e2e::DEFAULT_READY_TIMEOUT,
    )
    .await
    .expect("spawn bookmarks-distributed example — is it built?");

    let runner = app
        .attach_browser()
        .await
        .expect("attach browser — is Chromium installed?");
    let page = runner.page().await.expect("open page");

    page.visit("/").await.expect("visit /");
    page.expect_text("All Bookmarks")
        .await
        .expect("bookmarks list heading renders via the replica-routed read pool");
    page.expect_no_console_errors()
        .await
        .expect("no console errors on /");

    let _ = std::fs::remove_dir_all(&scratch_dir);
}

/// Regression guard for the RYWW wiring in `BookmarkRepository::conn()`
/// (see `src/repositories.rs`): a client that just created a bookmark must
/// see it on the very next page load, even though `list()` normally reads
/// from the replica pool.
///
/// This example's "replica" is a second, wholly independent testcontainer
/// database (no real streaming replication — see module docs), so without
/// RYWW correctly redirecting the post-write read to primary, the create ->
/// redirect -> list flow below would deterministically show "No bookmarks
/// yet." rather than the bookmark just created — there is no lag/flakiness
/// to race against, it either redirects or it doesn't.
#[tokio::test]
#[ignore = "requires Chromium + Docker — set AUTUMN_CHROMIUM or install chromium-browser"]
async fn bookmarks_distributed_read_your_own_write_after_create() {
    let db = example_e2e::provision_postgres(2).await;
    let primary_url = &db.urls()[0];
    let replica_url = &db.urls()[1];

    autumn_web::migrate::run_pending(primary_url, MIGRATIONS)
        .expect("migrate primary testcontainer");
    autumn_web::migrate::run_pending(replica_url, MIGRATIONS)
        .expect("migrate replica-stand-in testcontainer");

    let scratch_dir = scratch_dir("read_your_own_write_after_create");
    setup_scratch_dir(
        &scratch_dir,
        primary_url,
        replica_url,
        &format!(
            r#"
read_your_writes = "session"
pin_after_write_secs = 10

[security.signing_secret]
secret = "{TEST_SIGNING_SECRET}"
"#
        ),
    );

    let app = example_e2e::spawn_example(
        env!("CARGO_BIN_EXE_bookmarks-distributed"),
        env!("CARGO_MANIFEST_DIR"),
        &[(
            "AUTUMN_MANIFEST_DIR",
            scratch_dir.to_str().expect("scratch dir path is UTF-8"),
        )],
        example_e2e::DEFAULT_READY_TIMEOUT,
    )
    .await
    .expect("spawn bookmarks-distributed example — is it built?");

    let runner = app
        .attach_browser()
        .await
        .expect("attach browser — is Chromium installed?");
    let page = runner.page().await.expect("open page");

    // A fresh page load hits the replica-routed list before any write has
    // happened in this browser session, so there is nothing to see yet.
    page.visit("/").await.expect("visit /");
    page.expect_text("No bookmarks yet.")
        .await
        .expect("list starts empty on the replica-stand-in database");

    // Real form submission — a plain (non-htmx) POST that 303-redirects to
    // the list, the same create -> redirect -> list flow a real user drives.
    page.visit("/new").await.expect("visit /new");
    page.fill("#url", "https://example.com/ryww")
        .await
        .expect("fill url");
    page.fill("#title", "RYWW smoke bookmark")
        .await
        .expect("fill title");
    page.fill("#tag", "ryww").await.expect("fill tag");
    page.click("button[type=submit]")
        .await
        .expect("submit create form");

    // `list()` is the only route that renders bookmark cards, so this text
    // appearing is itself proof the 303 redirect landed back on `/`.
    page.expect_text("RYWW smoke bookmark").await.expect(
        "created bookmark visible on the very next list read — RYWW must have redirected \
             this replica-routed read to primary, since the replica-stand-in database never \
             receives this write",
    );
    page.expect_no_console_errors()
        .await
        .expect("no console errors across the create -> redirect -> list flow");

    let _ = std::fs::remove_dir_all(&scratch_dir);
}
