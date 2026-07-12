//! #1801 — validate the effective *merged model* on the plain / no-hooks blind
//! update path, opt-in via `validate_on_update = fetch`.
//!
//! A no-hooks repo without `hooks = ...` normally does a blind
//! `changes.__to_changeset()` UPDATE and never builds a draft, so it skips the
//! merged-model validation that #1804 wired into the hooked `from_patch` path.
//! With `validate_on_update = fetch` the generated `update` loads the current
//! row (only on the otherwise-blind branches) and runs `from_patch` purely for
//! its 422 side-effect before the unchanged blind write.
//!
//! These exercise the write path end-to-end against Postgres:
//! - a patch that only becomes invalid once merged → 422 (blurb / slug rules);
//! - a valid patch → Ok;
//! - an untouched-but-invalid field on merge → 422 (whole-merged-model
//!   semantic, matching #1804);
//! - a second plain repo on a distinct model *without* the knob still accepts
//!   the invalid-once-merged patch (locks the opt-in AC — no merged validation
//!   without the knob).
//!
//! **Requires Docker** to be running.

#![cfg(feature = "db")]
#![allow(clippy::must_use_candidate, clippy::missing_const_for_fn)]

use autumn_web::hooks::Patch;
use diesel_async::pooled_connection::AsyncDieselConnectionManager;
use diesel_async::pooled_connection::deadpool::Pool;
use diesel_async::{AsyncPgConnection, RunQueryDsl};
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

/// Reject any value containing an uppercase letter — stands in for the
/// cross-field / struct-level `custom` validators that have no `Patch<T>` impl
/// and are therefore create-only on the patch-struct path, but run here against
/// the concrete merged value.
fn no_uppercase(value: &str) -> Result<(), validator::ValidationError> {
    if value.chars().any(char::is_uppercase) {
        return Err(validator::ValidationError::new("no_uppercase"));
    }
    Ok(())
}

// ── Opt-in repo (`validate_on_update = fetch`) ────────────────────────────────

mod validated_schema {
    autumn_web::reexports::diesel::table! {
        blind_hosts (id) {
            id -> Int8,
            blurb -> Text,
            slug -> Text,
        }
    }
}
use validated_schema::blind_hosts;

#[autumn_web::model(table = "blind_hosts")]
pub struct BlindHost {
    #[id]
    pub id: i64,
    // `does_not_contain`: blanket-inversion coherence wall on `Patch<T>`.
    // Dropped from the patch field, enforced only on the merged concrete `String`.
    #[validate(does_not_contain(pattern = "bad"))]
    pub blurb: String,
    // `custom`: no `Patch<T>` impl (struct/cross-field class). Dropped from the
    // patch field, enforced on the merged concrete value.
    #[validate(custom(function = "no_uppercase"))]
    pub slug: String,
}

#[autumn_web::repository(BlindHost, table = "blind_hosts", validate_on_update = fetch)]
pub trait BlindHostRepository {}

// ── Control repo on a distinct model WITHOUT the knob ─────────────────────────

mod lax_schema {
    autumn_web::reexports::diesel::table! {
        lax_hosts (id) {
            id -> Int8,
            blurb -> Text,
            slug -> Text,
        }
    }
}
use lax_schema::lax_hosts;

#[autumn_web::model(table = "lax_hosts")]
pub struct LaxHost {
    #[id]
    pub id: i64,
    #[validate(does_not_contain(pattern = "bad"))]
    pub blurb: String,
    #[validate(custom(function = "no_uppercase"))]
    pub slug: String,
}

#[autumn_web::repository(LaxHost, table = "lax_hosts")]
pub trait LaxHostRepository {}

// ── Setup & helpers ──────────────────────────────────────────────────────────

async fn setup_pool() -> (
    Pool<AsyncPgConnection>,
    testcontainers::ContainerAsync<Postgres>,
) {
    let container = Postgres::default()
        .start()
        .await
        .expect("failed to start postgres container");

    let host = container.get_host().await.expect("host");
    let port = container.get_host_port_ipv4(5432).await.expect("port");
    let url = format!("postgres://postgres:postgres@{host}:{port}/postgres");

    let manager = AsyncDieselConnectionManager::<AsyncPgConnection>::new(&url);
    let pool = Pool::builder(manager).max_size(5).build().expect("pool");

    let mut conn = pool.get().await.expect("conn");
    diesel::sql_query(
        "CREATE TABLE IF NOT EXISTS blind_hosts \
         (id BIGSERIAL PRIMARY KEY, blurb TEXT NOT NULL, slug TEXT NOT NULL)",
    )
    .execute(&mut conn)
    .await
    .expect("create blind_hosts");
    diesel::sql_query(
        "CREATE TABLE IF NOT EXISTS lax_hosts \
         (id BIGSERIAL PRIMARY KEY, blurb TEXT NOT NULL, slug TEXT NOT NULL)",
    )
    .execute(&mut conn)
    .await
    .expect("create lax_hosts");

    (pool, container)
}

fn build_blind_repo(pool: Pool<AsyncPgConnection>) -> PgBlindHostRepository {
    PgBlindHostRepository {
        pool,
        __autumn_read_route: autumn_web::repository::ReadRoute::Primary,
        __autumn_statement_timeout_ms: 0,
        __autumn_slow_threshold: std::time::Duration::from_millis(500),
        __autumn_route: None,
    }
}

fn build_lax_repo(pool: Pool<AsyncPgConnection>) -> PgLaxHostRepository {
    PgLaxHostRepository {
        pool,
        __autumn_read_route: autumn_web::repository::ReadRoute::Primary,
        __autumn_statement_timeout_ms: 0,
        __autumn_slow_threshold: std::time::Duration::from_millis(500),
        __autumn_route: None,
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn blind_update_rejects_patch_invalid_once_merged() {
    let (pool, _container) = setup_pool().await;
    let repo = build_blind_repo(pool);

    let created = repo
        .save(&NewBlindHost {
            blurb: "all good".to_string(),
            slug: "host-one".to_string(),
        })
        .await
        .expect("insert a valid row");

    // `does_not_contain` is dropped from the `Patch<String>` field, so the
    // patch struct validates fine; only the merged-model check catches this.
    let err = repo
        .update(
            created.id,
            &UpdateBlindHost {
                blurb: Patch::Set("this is bad".to_string()),
                ..Default::default()
            },
        )
        .await
        .expect_err("a patch that only becomes invalid once merged must be rejected");
    assert_eq!(
        err.status(),
        autumn_web::reexports::http::StatusCode::UNPROCESSABLE_ENTITY,
        "merged-model validation failures on the blind path must surface as 422"
    );

    // `custom` (no uppercase) is likewise dropped from the patch field.
    let err = repo
        .update(
            created.id,
            &UpdateBlindHost {
                slug: Patch::Set("Has-Uppercase".to_string()),
                ..Default::default()
            },
        )
        .await
        .expect_err("a merged slug failing the custom rule must be rejected");
    assert_eq!(
        err.status(),
        autumn_web::reexports::http::StatusCode::UNPROCESSABLE_ENTITY
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn blind_update_accepts_valid_patch() {
    let (pool, _container) = setup_pool().await;
    let repo = build_blind_repo(pool);

    let created = repo
        .save(&NewBlindHost {
            blurb: "all good".to_string(),
            slug: "host-one".to_string(),
        })
        .await
        .expect("insert a valid row");

    let updated = repo
        .update(
            created.id,
            &UpdateBlindHost {
                blurb: Patch::Set("still fine".to_string()),
                slug: Patch::Set("host-two".to_string()),
            },
        )
        .await
        .expect("a valid merged model must pass and persist");
    assert_eq!(updated.blurb, "still fine");
    assert_eq!(updated.slug, "host-two");
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn blind_update_rejects_untouched_but_invalid_field() {
    // Whole-merged-model semantic (#1804 precedent): a field the patch never
    // touches but that already violates a rule must still surface on merge.
    let (pool, _container) = setup_pool().await;
    let repo = build_blind_repo(pool.clone());

    // Seed a row whose `blurb` already contains the forbidden pattern, bypassing
    // any create-path checks by inserting directly.
    let mut conn = pool.get().await.expect("conn");
    diesel::sql_query("INSERT INTO blind_hosts (blurb, slug) VALUES ('already bad', 'ok-slug')")
        .execute(&mut conn)
        .await
        .expect("seed an already-invalid row");
    let id: i64 = {
        use diesel::prelude::*;
        let query = blind_hosts::table.select(blind_hosts::id);
        diesel_async::RunQueryDsl::first(query, &mut conn)
            .await
            .expect("seeded row id")
    };

    // The patch only touches `slug`; `blurb` keeps the pre-existing invalid
    // value. Merged validation sees the whole record, so it is rejected.
    let err = repo
        .update(
            id,
            &UpdateBlindHost {
                slug: Patch::Set("fresh-slug".to_string()),
                ..Default::default()
            },
        )
        .await
        .expect_err("an untouched-but-invalid field must still be caught on merge");
    assert_eq!(
        err.status(),
        autumn_web::reexports::http::StatusCode::UNPROCESSABLE_ENTITY
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn plain_repo_without_knob_skips_merged_validation() {
    // AC #2: without `validate_on_update = fetch` the blind path stays blind —
    // no merged validation, so an invalid-once-merged patch is persisted.
    let (pool, _container) = setup_pool().await;
    let repo = build_lax_repo(pool);

    let created = repo
        .save(&NewLaxHost {
            blurb: "all good".to_string(),
            slug: "host-one".to_string(),
        })
        .await
        .expect("insert a valid row");

    let updated = repo
        .update(
            created.id,
            &UpdateLaxHost {
                blurb: Patch::Set("this is bad".to_string()),
                ..Default::default()
            },
        )
        .await
        .expect("a repo without the opt-in must skip merged validation and persist");
    assert_eq!(
        updated.blurb, "this is bad",
        "the blind write must persist the patch unchanged when the knob is off"
    );
}
