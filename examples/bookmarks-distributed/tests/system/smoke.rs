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

    // Redirect both `DistributedConfig::load()` and the framework's own
    // `AutumnConfig` to a scratch autumn.toml carrying the testcontainer
    // URLs — see module docs.
    let scratch_dir = std::env::temp_dir().join(format!(
        "bookmarks-distributed-smoke-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&scratch_dir).expect("create scratch config dir");
    std::fs::write(
        scratch_dir.join("autumn.toml"),
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
"#
        ),
    )
    .expect("write scratch autumn.toml");

    // See module docs: redirecting AUTUMN_MANIFEST_DIR to the scratch dir
    // also redirects `/static` file serving there, so without this the page
    // would 404 on its real CSS/JS instead of exercising them.
    #[cfg(unix)]
    std::os::unix::fs::symlink(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("static"),
        scratch_dir.join("static"),
    )
    .expect("symlink scratch static dir to the real project's static dir");

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
