//! Maintained derived read models on the `SQLite` runtime backend (#1769).
//!
//! The Postgres behaviour suite (`tests/integration/model_derivation.rs`) needs
//! a Postgres and is `#[ignore]`d, so nothing there ever compiles — let alone
//! runs — the `sqlite` arms of the statements a derivation adds: the `IS NOT`
//! spelling of NULL-safe inequality, the positional `?` binds, the
//! `BEGIN IMMEDIATE` the backfill's per-batch transaction takes, and the
//! `CAST(updated_at AS TEXT)` the status read uses against a `TEXT` column.
//! This file is the CI-backed evidence that all of them work.
//!
//! What it pins:
//!
//! * **Filtered count and filtered sum.** Only qualifying rows contribute, and
//!   the sum is weighted by the row's field.
//! * **Delete and reparent.** The set-based decrement filters too, so deleting
//!   a rejected row moves nothing.
//! * **Filter flip.** Publishing a row already attached to its parent is `+1`
//!   with no foreign-key change at all — the case a plain key diff cannot see.
//! * **Resumable backfill.** `max_batches` stops a sweep mid-table, the
//!   checkpoint survives, and resuming finishes without double counting.
//! * **Status and drift.** The state row round-trips through the `SQLite` state
//!   table and the drift aggregate reaches 0 after a recompute.
//!
//! Uses an in-memory shared-cache `SQLite` database — no Docker.
//!
//! Only meaningful under `--features sqlite`; the file is
//! `#![cfg(feature = "sqlite")]` so a default `cargo test` compiles it to an
//! empty (passing) binary. Run explicitly:
//! `cargo test -p autumn-web --features sqlite --test sqlite_derivation`.
#![cfg(feature = "sqlite")]

use autumn_web::Patch;
use autumn_web::config::DatabaseConfig;
use autumn_web::db::{RuntimeConnection, create_pool};
use autumn_web::derivation::{
    BackfillOptions, BackfillState, DerivationDef, derivation_status, drift, ensure_derivations,
    recompute, registered_derivations, run_backfill,
};
use autumn_web::reexports::{diesel, diesel_async};

use diesel::sql_types::{BigInt, Text};
use diesel_async::RunQueryDsl as _;
use diesel_async::pooled_connection::deadpool::Pool;

type SqlitePool = Pool<RuntimeConnection>;

mod schema {
    autumn_web::reexports::diesel::table! {
        sd_posts (id) {
            id -> Int8,
            title -> Text,
            published_comment_count -> Int8,
            visible_score -> Int8,
        }
    }

    autumn_web::reexports::diesel::table! {
        sd_comments (id) {
            id -> Int8,
            post_id -> Int8,
            published -> Bool,
            score -> Int8,
        }
    }
}

use schema::{sd_comments, sd_posts};

#[autumn_web::model(table = "sd_posts")]
pub struct SdPost {
    #[id]
    pub id: i64,
    pub title: String,
    #[default]
    pub published_comment_count: i64,
    #[default]
    pub visible_score: i64,
}

#[autumn_web::repository(SdPost, table = "sd_posts")]
pub trait SdPostRepository {}

// No `#[belongs_to]`: its generated association loader is typed to
// `AsyncPgConnection`, so declaring one here would not compile under the
// SQLite backend flip. The derivations therefore name their foreign key
// explicitly, which also covers the `fk = <column>` override.
#[autumn_web::model(table = "sd_comments")]
#[derivation(SdPost, column = "published_comment_count", fk = post_id, filter = published)]
#[derivation(SdPost, column = "visible_score", fk = post_id, transform = sum(score), filter = published && score > 0)]
pub struct SdComment {
    #[id]
    pub id: i64,
    pub post_id: i64,
    pub published: bool,
    pub score: i64,
}

#[autumn_web::repository(SdComment, table = "sd_comments")]
pub trait SdCommentRepository {}

const COUNT_DERIVATION: &str = "sd_posts.published_comment_count";
const SUM_DERIVATION: &str = "sd_posts.visible_score";

/// The framework's own `SQLite` state-table DDL, so this suite proves the
/// shipped migration rather than a copy of it.
const DERIVATIONS_DDL: &str =
    include_str!("../derivation_migrations_sqlite/20260907000000_create_derivations/up.sql");

const DDL: &[&str] = &[
    // `INTEGER PRIMARY KEY` is the rowid alias that autoincrements; `BIGSERIAL`
    // has mere NUMERIC affinity here, so an id-less INSERT would write NULL.
    "CREATE TABLE sd_posts (\
         id INTEGER PRIMARY KEY, \
         title TEXT NOT NULL, \
         published_comment_count BIGINT NOT NULL DEFAULT 0, \
         visible_score BIGINT NOT NULL DEFAULT 0\
     )",
    "CREATE TABLE sd_comments (\
         id INTEGER PRIMARY KEY, \
         post_id BIGINT NOT NULL REFERENCES sd_posts(id), \
         published BOOLEAN NOT NULL DEFAULT 0, \
         score BIGINT NOT NULL DEFAULT 0\
     )",
];

async fn boot_pool(db_name: &str) -> SqlitePool {
    // A shared-cache in-memory database so every pooled checkout observes the
    // same schema (a bare `:memory:` target is private per connection).
    let config = DatabaseConfig {
        url: Some(format!("sqlite://file:{db_name}?mode=memory&cache=shared")),
        primary_pool_size: Some(2),
        ..Default::default()
    };
    let pool: SqlitePool = create_pool(&config)
        .expect("sqlite pool builds via build_sqlite_pool")
        .expect("a url is configured");

    {
        let mut conn = pool.get().await.expect("checkout a sqlite connection");
        for stmt in DDL {
            diesel::sql_query(*stmt)
                .execute(&mut *conn)
                .await
                .unwrap_or_else(|e| panic!("DDL failed ({stmt}): {e}"));
        }
        // The migration file is a single statement, so it needs no batch split.
        diesel::sql_query(DERIVATIONS_DDL)
            .execute(&mut *conn)
            .await
            .expect("derivation state table DDL");
    }

    pool
}

#[derive(diesel::QueryableByName)]
struct CountRow {
    #[diesel(sql_type = BigInt)]
    count: i64,
}

#[derive(diesel::QueryableByName)]
struct IdRow {
    #[diesel(sql_type = BigInt)]
    id: i64,
}

#[derive(diesel::QueryableByName)]
struct StateRow {
    #[diesel(sql_type = Text)]
    backfill_state: String,
    #[diesel(sql_type = diesel::sql_types::Nullable<BigInt>)]
    checkpoint: Option<i64>,
    #[diesel(sql_type = BigInt)]
    backfilled_rows: i64,
}

/// A derived column, read with raw SQL so the assertion never depends on the
/// repository's own read path.
async fn derived(pool: &SqlitePool, column: &str, id: i64) -> i64 {
    let mut conn = pool.get().await.expect("conn");
    diesel::sql_query(format!(
        "SELECT {column} AS count FROM sd_posts WHERE id = ?"
    ))
    .bind::<BigInt, _>(id)
    .get_result::<CountRow>(&mut *conn)
    .await
    .expect("read derived column")
    .count
}

async fn seed_post(pool: &SqlitePool, title: &str) -> i64 {
    let mut conn = pool.get().await.expect("conn");
    diesel::sql_query("INSERT INTO sd_posts (title) VALUES (?) RETURNING id")
        .bind::<Text, _>(title)
        .get_result::<IdRow>(&mut *conn)
        .await
        .expect("seed post")
        .id
}

async fn state_of(pool: &SqlitePool, name: &str) -> StateRow {
    let mut conn = pool.get().await.expect("conn");
    diesel::sql_query(
        "SELECT backfill_state, checkpoint, backfilled_rows \
         FROM _autumn_derivations WHERE name = ?",
    )
    .bind::<Text, _>(name)
    .get_result::<StateRow>(&mut *conn)
    .await
    .unwrap_or_else(|e| panic!("no state row for `{name}`: {e}"))
}

fn def(name: &str) -> &'static DerivationDef {
    registered_derivations()
        .into_iter()
        .find(|def| def.name == name)
        .unwrap_or_else(|| panic!("`{name}` must be a registered derivation"))
}

// ── Behaviour ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_filtered_count_and_sum_are_maintained_on_insert() {
    let pool = boot_pool("sd_insert").await;
    let repo = PgSdCommentRepository::with_pool_untracked(pool.clone());
    let post = seed_post(&pool, "hello").await;

    for (published, score) in [(true, 5), (true, 7), (false, 100), (true, -3)] {
        repo.save(&NewSdComment {
            post_id: post,
            published,
            score,
        })
        .await
        .expect("save comment");
    }

    assert_eq!(
        derived(&pool, "published_comment_count", post).await,
        3,
        "the unpublished comment is invisible to the count"
    );
    assert_eq!(
        derived(&pool, "visible_score", post).await,
        12,
        "only published, positively-scored comments are summed"
    );

    let mut conn = pool.get().await.expect("conn");
    assert_eq!(
        drift(&mut conn, def(COUNT_DERIVATION))
            .await
            .expect("drift"),
        0
    );
    assert_eq!(
        drift(&mut conn, def(SUM_DERIVATION)).await.expect("drift"),
        0
    );
}

#[tokio::test]
async fn deleting_a_rejected_row_moves_nothing_and_a_qualifying_one_moves_both() {
    let pool = boot_pool("sd_delete").await;
    let repo = PgSdCommentRepository::with_pool_untracked(pool.clone());
    let post = seed_post(&pool, "delete").await;

    let kept = repo
        .save(&NewSdComment {
            post_id: post,
            published: true,
            score: 5,
        })
        .await
        .expect("save published");
    let draft = repo
        .save(&NewSdComment {
            post_id: post,
            published: false,
            score: 90,
        })
        .await
        .expect("save draft");
    let second_draft = repo
        .save(&NewSdComment {
            post_id: post,
            published: false,
            score: 91,
        })
        .await
        .expect("save second draft");

    repo.delete_by_id(draft.id).await.expect("delete the draft");
    assert_eq!(derived(&pool, "published_comment_count", post).await, 1);
    assert_eq!(derived(&pool, "visible_score", post).await, 5);

    // The bulk path computes its delta with one aggregate, so it has to filter
    // too — otherwise a batch of drafts would drive the value negative.
    repo.delete_many(&[second_draft.id])
        .await
        .expect("delete_many");
    assert_eq!(derived(&pool, "published_comment_count", post).await, 1);
    assert_eq!(derived(&pool, "visible_score", post).await, 5);

    repo.delete_by_id(kept.id).await.expect("delete published");
    assert_eq!(derived(&pool, "published_comment_count", post).await, 0);
    assert_eq!(derived(&pool, "visible_score", post).await, 0);
}

#[tokio::test]
async fn reparenting_moves_the_old_and_the_new_parent() {
    let pool = boot_pool("sd_reparent").await;
    let repo = PgSdCommentRepository::with_pool_untracked(pool.clone());
    let old = seed_post(&pool, "old").await;
    let new = seed_post(&pool, "new").await;

    let comment = repo
        .save(&NewSdComment {
            post_id: old,
            published: true,
            score: 6,
        })
        .await
        .expect("save");

    repo.update(
        comment.id,
        &UpdateSdComment {
            post_id: Patch::Set(new),
            ..Default::default()
        },
    )
    .await
    .expect("reparent");

    assert_eq!(derived(&pool, "published_comment_count", old).await, 0);
    assert_eq!(derived(&pool, "visible_score", old).await, 0);
    assert_eq!(derived(&pool, "published_comment_count", new).await, 1);
    assert_eq!(derived(&pool, "visible_score", new).await, 6);
}

#[tokio::test]
async fn a_filter_flip_on_the_same_parent_moves_the_derived_value() {
    let pool = boot_pool("sd_flip").await;
    let repo = PgSdCommentRepository::with_pool_untracked(pool.clone());
    let post = seed_post(&pool, "flip").await;

    let comment = repo
        .save(&NewSdComment {
            post_id: post,
            published: false,
            score: 4,
        })
        .await
        .expect("save draft");
    assert_eq!(derived(&pool, "published_comment_count", post).await, 0);

    repo.update(
        comment.id,
        &UpdateSdComment {
            published: Patch::Set(true),
            ..Default::default()
        },
    )
    .await
    .expect("publish");
    assert_eq!(
        derived(&pool, "published_comment_count", post).await,
        1,
        "publishing a row already attached to its parent is +1"
    );
    assert_eq!(derived(&pool, "visible_score", post).await, 4);

    // A score edit on a qualifying row moves the sum by the difference only.
    repo.update(
        comment.id,
        &UpdateSdComment {
            score: Patch::Set(9),
            ..Default::default()
        },
    )
    .await
    .expect("rescore");
    assert_eq!(derived(&pool, "visible_score", post).await, 9);
    assert_eq!(derived(&pool, "published_comment_count", post).await, 1);
}

#[tokio::test]
async fn reconciliation_enqueues_only_the_derivation_whose_definition_changed() {
    let pool = boot_pool("sd_ensure").await;
    let mut conn = pool.get().await.expect("conn");

    // First boot: nothing is recorded, so every derivation is enqueued.
    let mut first = ensure_derivations(&mut conn).await.expect("first boot");
    first.sort_unstable();
    assert_eq!(first, vec![COUNT_DERIVATION, SUM_DERIVATION]);
    assert_eq!(
        state_of(&pool, COUNT_DERIVATION).await.backfill_state,
        "pending"
    );

    // Second boot: nothing changed, so nothing is re-enqueued.
    assert!(
        ensure_derivations(&mut conn)
            .await
            .expect("second boot")
            .is_empty()
    );

    // Finish one, then stale the other's hash: only the stale one moves.
    diesel::sql_query("UPDATE _autumn_derivations SET backfill_state = 'complete'")
        .execute(&mut conn)
        .await
        .expect("mark complete");
    diesel::sql_query("UPDATE _autumn_derivations SET definition_hash = 'stale' WHERE name = ?")
        .bind::<Text, _>(SUM_DERIVATION)
        .execute(&mut conn)
        .await
        .expect("stale one hash");

    assert_eq!(
        ensure_derivations(&mut conn).await.expect("third boot"),
        vec![SUM_DERIVATION]
    );
    assert_eq!(
        state_of(&pool, COUNT_DERIVATION).await.backfill_state,
        "complete",
        "a sibling on the same tables must be left alone"
    );
    assert_eq!(
        state_of(&pool, SUM_DERIVATION).await.backfill_state,
        "pending"
    );
}

#[tokio::test]
async fn a_killed_backfill_resumes_from_its_checkpoint() {
    let pool = boot_pool("sd_backfill").await;
    let mut conn = pool.get().await.expect("conn");

    // Five parents with one published comment each that nobody counted — the
    // shape of a table adopting a derivation it did not have before.
    let mut posts = Vec::new();
    for i in 0..5 {
        let post = seed_post(&pool, &format!("p{i}")).await;
        diesel::sql_query("INSERT INTO sd_comments (post_id, published, score) VALUES (?, 1, 2)")
            .bind::<BigInt, _>(post)
            .execute(&mut conn)
            .await
            .expect("legacy comment");
        posts.push(post);
    }

    ensure_derivations(&mut conn).await.expect("enqueue");
    assert_eq!(
        drift(&mut conn, def(COUNT_DERIVATION))
            .await
            .expect("drift"),
        5
    );

    // One batch of two, then stop — the kill.
    let first = run_backfill(
        &mut conn,
        &BackfillOptions {
            batch_size: 2,
            max_batches: Some(1),
        },
    )
    .await
    .expect("first pass");
    assert_eq!(first.rows_repaired, 2);
    assert_eq!(first.in_progress.len(), 1, "{first:?}");

    let stopped = state_of(&pool, first.in_progress[0].as_str()).await;
    assert_eq!(stopped.backfill_state, "running");
    assert_eq!(stopped.checkpoint, Some(posts[1]));
    assert_eq!(stopped.backfilled_rows, 2);

    // Resume to completion: both derivations end up complete and correct.
    let second = run_backfill(&mut conn, &BackfillOptions::default())
        .await
        .expect("resumed pass");
    assert!(second.rows_repaired > 0, "{second:?}");
    for name in [COUNT_DERIVATION, SUM_DERIVATION] {
        let done = state_of(&pool, name).await;
        assert_eq!(done.backfill_state, "complete", "{name}");
        assert_eq!(
            done.backfilled_rows, 5,
            "five parents, counted once each — no double counting across the \
             resume ({name})"
        );
    }
    for post in &posts {
        assert_eq!(derived(&pool, "published_comment_count", *post).await, 1);
        assert_eq!(derived(&pool, "visible_score", *post).await, 2);
    }
    assert_eq!(
        drift(&mut conn, def(COUNT_DERIVATION))
            .await
            .expect("drift"),
        0
    );

    // A completed derivation is not swept again.
    let third = run_backfill(&mut conn, &BackfillOptions::default())
        .await
        .expect("third pass");
    assert_eq!(third.rows_repaired, 0);
    assert!(third.completed.is_empty());
}

#[tokio::test]
async fn status_reports_state_and_recompute_clears_the_drift() {
    let pool = boot_pool("sd_status").await;
    let mut conn = pool.get().await.expect("conn");
    ensure_derivations(&mut conn).await.expect("enqueue");
    run_backfill(&mut conn, &BackfillOptions::default())
        .await
        .expect("backfill an empty table");

    let post = seed_post(&pool, "drifted").await;
    diesel::sql_query("INSERT INTO sd_comments (post_id, published, score) VALUES (?, 1, 7)")
        .bind::<BigInt, _>(post)
        .execute(&mut conn)
        .await
        .expect("legacy comment");
    diesel::sql_query("UPDATE sd_posts SET published_comment_count = 99 WHERE id = ?")
        .bind::<BigInt, _>(post)
        .execute(&mut conn)
        .await
        .expect("inflate");

    let drifted = derivation_status(&mut conn).await.expect("status");
    assert_eq!(drifted.len(), 2, "both derivations are reported");
    let count = drifted
        .iter()
        .find(|entry| entry.name == COUNT_DERIVATION)
        .expect("the count is reported");
    assert_eq!(
        count.stored_hash.as_deref(),
        Some(count.definition_hash.as_str())
    );
    assert_eq!(count.backfill_state, Some(BackfillState::Complete));
    assert!(
        count.updated_at.is_some(),
        "the SQLite TEXT timestamp must still round-trip"
    );
    assert_eq!(count.drift, 1);

    for name in [COUNT_DERIVATION, SUM_DERIVATION] {
        assert_eq!(
            recompute(&mut conn, name).await.expect("recompute"),
            1,
            "one parent is repaired ({name})"
        );
    }
    assert_eq!(derived(&pool, "published_comment_count", post).await, 1);
    assert_eq!(derived(&pool, "visible_score", post).await, 7);

    for entry in derivation_status(&mut conn).await.expect("status") {
        assert_eq!(entry.drift, 0, "{entry:?}");
    }
    // Idempotent: a healthy derivation is repaired zero times.
    assert_eq!(
        recompute(&mut conn, COUNT_DERIVATION)
            .await
            .expect("recompute again"),
        0
    );
    assert!(recompute(&mut conn, "nope.nope").await.is_err());
}
