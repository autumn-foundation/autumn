//! Maintained derived read models on the `SQLite` runtime backend (#1769).
//!
//! The Postgres behaviour suite (`tests/integration/model_derivation.rs`) needs
//! a Postgres and is `#[ignore]`d. Nothing there compiles the `sqlite` arms of
//! the statements a derivation adds, let alone runs them: the `IS NOT` spelling
//! of NULL-safe inequality, the positional `?` binds, the `BEGIN IMMEDIATE` each
//! backfill batch takes, and the `CAST(updated_at AS TEXT)` status read against
//! a `TEXT` column. This file is the CI-backed evidence that all of them work.
//!
//! What it pins:
//!
//! * **Filtered count and filtered sum.** Only qualifying rows contribute, and
//!   the sum is weighted by the row's field.
//! * **Delete and reparent.** The set-based decrement filters too, so deleting
//!   a rejected row moves nothing.
//! * **Filter flip.** Publishing a row already attached to its parent is `+1`
//!   with no foreign-key change. A plain key diff cannot see that case.
//! * **Resumable backfill.** `max_batches` stops a sweep mid-table, the
//!   checkpoint survives, and resuming finishes without double counting. The
//!   budget also reports the derivations it never reached.
//! * **Status and drift.** The state row round-trips through the `SQLite` state
//!   table, a stale row is reported as unregistered, and the drift aggregate
//!   reaches 0 after a recompute.
//!
//! Uses an in-memory shared-cache `SQLite` database, so it needs no Docker.
//!
//! Only meaningful under `--features sqlite`. The file is
//! `#![cfg(feature = "sqlite")]`, so a default `cargo test` compiles it to an
//! empty (passing) binary. Run explicitly:
//! `cargo test -p autumn-web --features sqlite --test sqlite_derivation`.
#![cfg(feature = "sqlite")]

use autumn_web::Patch;
use autumn_web::config::DatabaseConfig;
use autumn_web::db::{RuntimeConnection, create_pool};
use autumn_web::derivation::{
    BackfillOptions, BackfillState, DerivationDef, derivation_status, drift, ensure_derivations,
    recompute, registered_derivations, resweep, run_backfill,
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
            wanted_tag_count -> Int8,
        }
    }

    autumn_web::reexports::diesel::table! {
        sd_tags (id) {
            id -> Int8,
            post_id -> Int8,
            label -> Text,
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

use schema::{sd_comments, sd_posts, sd_tags};

#[autumn_web::model(table = "sd_posts")]
pub struct SdPost {
    #[id]
    pub id: i64,
    pub title: String,
    #[default]
    pub published_comment_count: i64,
    #[default]
    pub visible_score: i64,
    #[default]
    pub wanted_tag_count: i64,
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

/// A string filter over a `COLLATE NOCASE` column: the one shape where SQL's
/// idea of equality and Rust's would part ways unless the lowering pins the
/// comparison to a bytewise collation.
#[autumn_web::model(table = "sd_tags")]
#[derivation(SdPost, column = "wanted_tag_count", fk = post_id, filter = label == "wanted")]
pub struct SdTag {
    #[id]
    pub id: i64,
    pub post_id: i64,
    pub label: String,
}

#[autumn_web::repository(SdTag, table = "sd_tags")]
pub trait SdTagRepository {}

const COUNT_DERIVATION: &str = "sd_posts.published_comment_count";
const SUM_DERIVATION: &str = "sd_posts.visible_score";
// Named so it sorts after the other two: the backfill tests below rely on
// the count derivation being the first one a sweep reaches.
const TAG_DERIVATION: &str = "sd_posts.wanted_tag_count";

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
         visible_score BIGINT NOT NULL DEFAULT 0, \
         wanted_tag_count BIGINT NOT NULL DEFAULT 0\
     )",
    "CREATE TABLE sd_comments (\
         id INTEGER PRIMARY KEY, \
         post_id BIGINT NOT NULL REFERENCES sd_posts(id), \
         published BOOLEAN NOT NULL DEFAULT 0, \
         score BIGINT NOT NULL DEFAULT 0\
     )",
    // `NOCASE`: the collation under which SQL alone would call `'WANTED'`
    // equal to `'wanted'`.
    "CREATE TABLE sd_tags (\
         id INTEGER PRIMARY KEY, \
         post_id BIGINT NOT NULL REFERENCES sd_posts(id), \
         label TEXT NOT NULL COLLATE NOCASE\
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
    // too, or a batch of drafts would drive the value negative.
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
    assert_eq!(
        first,
        vec![COUNT_DERIVATION, SUM_DERIVATION, TAG_DERIVATION]
    );
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

    // Five parents with one published comment each that nobody counted. This is
    // the shape of a table adopting a derivation it did not have before.
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

    // One batch of two, then stop: the kill.
    let first = run_backfill(
        &mut conn,
        &BackfillOptions {
            batch_size: 2,
            max_batches: Some(1),
        },
    )
    .await
    .expect("first pass");
    assert_eq!(first.rows_repaired, 2, "{first:?}");
    assert!(first.completed.is_empty(), "{first:?}");
    // The budget stops the call, not the report: the derivation the batch ran
    // for AND the one it never reached are both still pending, so both are
    // named. A `return` here would have hidden the second.
    assert_eq!(
        first.in_progress,
        vec![
            COUNT_DERIVATION.to_owned(),
            SUM_DERIVATION.to_owned(),
            TAG_DERIVATION.to_owned()
        ],
        "{first:?}"
    );

    let stopped = state_of(&pool, COUNT_DERIVATION).await;
    assert_eq!(stopped.backfill_state, "running");
    assert_eq!(stopped.checkpoint, Some(posts[1]));
    assert_eq!(stopped.backfilled_rows, 2);

    // The same three facts, read back through the reported surface an operator
    // actually sees.
    let mid = derivation_status(&mut conn).await.expect("status");
    let reported = mid
        .iter()
        .find(|entry| entry.name == COUNT_DERIVATION)
        .expect("the stopped derivation is reported");
    assert_eq!(reported.backfill_state, Some(BackfillState::Running));
    assert_eq!(reported.checkpoint, Some(posts[1]));
    assert_eq!(reported.backfilled_rows, 2);

    // Resume to completion: both derivations end up complete and correct.
    let second = run_backfill(&mut conn, &BackfillOptions::default())
        .await
        .expect("resumed pass");
    assert_eq!(
        second.rows_repaired, 8,
        "three parents left for the count plus five for the sum, each repaired \
         once: {second:?}"
    );
    assert_eq!(
        second.completed,
        vec![
            COUNT_DERIVATION.to_owned(),
            SUM_DERIVATION.to_owned(),
            TAG_DERIVATION.to_owned()
        ],
        "{second:?}"
    );
    assert!(second.in_progress.is_empty(), "{second:?}");
    for name in [COUNT_DERIVATION, SUM_DERIVATION, TAG_DERIVATION] {
        let done = state_of(&pool, name).await;
        assert_eq!(done.backfill_state, "complete", "{name}");
        assert_eq!(
            done.backfilled_rows, 5,
            "five parents, visited once each, with no double counting across \
             the resume ({name})"
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
async fn a_state_row_with_no_derivation_is_reported_as_unregistered() {
    let pool = boot_pool("sd_unregistered").await;
    let mut conn = pool.get().await.expect("conn");
    ensure_derivations(&mut conn).await.expect("enqueue");

    // The row a removed or renamed derivation leaves behind. It is reported
    // rather than deleted: only an operator can tell a removed derivation apart
    // from a rolling deploy that has not finished.
    diesel::sql_query(
        "INSERT INTO _autumn_derivations \
           (name, definition_hash, backfill_state, checkpoint, backfilled_rows) \
         VALUES ('sd_posts.gone', 'deadbeef', 'complete', 41, 7)",
    )
    .execute(&mut conn)
    .await
    .expect("seed a stale row");

    let status = derivation_status(&mut conn).await.expect("status");
    let names: Vec<&str> = status.iter().map(|entry| entry.name.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "sd_posts.gone",
            COUNT_DERIVATION,
            SUM_DERIVATION,
            TAG_DERIVATION
        ],
        "every row is reported, sorted by name"
    );

    let stale = &status[0];
    assert_eq!(stale.backfill_state, Some(BackfillState::Unregistered));
    assert_eq!(
        stale.definition_hash, None,
        "this binary declares no derivation of that name"
    );
    assert_eq!(stale.stored_hash.as_deref(), Some("deadbeef"));
    assert_eq!(stale.checkpoint, Some(41));
    assert_eq!(stale.backfilled_rows, 7);
    assert_eq!(
        stale.drift, None,
        "there is no definition left to measure drift against"
    );
    assert_eq!(stale.drift_error, None);

    // A stale row must not hide the derivations this binary does declare.
    for entry in &status[1..] {
        assert!(entry.definition_hash.is_some(), "{entry:?}");
        assert_eq!(entry.drift, Some(0), "{entry:?}");
    }
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
    assert_eq!(drifted.len(), 3, "every derivation is reported");
    let count = drifted
        .iter()
        .find(|entry| entry.name == COUNT_DERIVATION)
        .expect("the count is reported");
    assert_eq!(count.stored_hash, count.definition_hash);
    assert_eq!(count.backfill_state, Some(BackfillState::Complete));
    assert!(
        count.updated_at.is_some(),
        "the SQLite TEXT timestamp must still round-trip"
    );
    assert_eq!(count.drift, Some(1));
    assert_eq!(count.drift_error, None, "the scan ran");
    assert_eq!(
        count.definition_hash.as_deref().map(str::len),
        Some(64),
        "sha256 renders as 64 hex characters"
    );

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
        assert_eq!(entry.drift, Some(0), "{entry:?}");
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

#[tokio::test]
async fn a_string_filter_compares_bytes_whatever_the_column_collation() {
    // `sd_tags.label` is `COLLATE NOCASE`, so a bare `label = 'wanted'` in SQL
    // would accept `'WANTED'`. The Rust lowering of the same filter compares
    // bytes and rejects it. Unless the SQL side is pinned to a bytewise
    // collation the record path contributes 0 while the drift scan, the
    // backfill and `recompute` all count the row — a derivation disagreeing
    // with itself.
    let pool = boot_pool("sd_collation").await;
    let repo = PgSdTagRepository::with_pool_untracked(pool.clone());
    let post = seed_post(&pool, "tagged").await;

    for label in ["WANTED", "Wanted", "wanted", "unwanted"] {
        repo.save(&NewSdTag {
            post_id: post,
            label: label.to_owned(),
        })
        .await
        .expect("save tag");
    }
    assert_eq!(
        derived(&pool, "wanted_tag_count", post).await,
        1,
        "the record path counts the one byte-equal label"
    );

    let mut conn = pool.get().await.expect("conn");
    assert_eq!(
        drift(&mut conn, def(TAG_DERIVATION)).await.expect("drift"),
        0,
        "the set-based scan must agree with the record path on a NOCASE column"
    );
    assert_eq!(
        recompute(&mut conn, TAG_DERIVATION)
            .await
            .expect("recompute"),
        0,
        "nothing to repair: SQL counted the same row Rust did"
    );
    assert_eq!(derived(&pool, "wanted_tag_count", post).await, 1);

    // The inequality form is pinned the same way: `!=` under NOCASE would
    // reject `'WANTED'`, Rust accepts it.
    let lowered = def(TAG_DERIVATION).filter_sql;
    assert!(
        lowered.contains("{bin}"),
        "the lowered filter carries the collation placeholder: {lowered}"
    );
}

#[tokio::test]
async fn a_renamed_derivation_keeps_its_finished_backfill() {
    // `definition_hash` leaves the name out so that a rename does not enqueue a
    // backfill. That promise needs the state row to follow the name: a row
    // under the old name with this exact hash is the same derivation.
    let pool = boot_pool("sd_rename").await;
    let mut conn = pool.get().await.expect("conn");
    ensure_derivations(&mut conn).await.expect("first boot");
    run_backfill(&mut conn, &BackfillOptions::default())
        .await
        .expect("finish every backfill");
    assert_eq!(
        state_of(&pool, COUNT_DERIVATION).await.backfill_state,
        "complete"
    );

    // The binary that wrote this row called the derivation something else.
    diesel::sql_query(
        "UPDATE _autumn_derivations SET name = 'sd_posts.legacy_name', backfilled_rows = 7 \
         WHERE name = ?",
    )
    .bind::<Text, _>(COUNT_DERIVATION)
    .execute(&mut conn)
    .await
    .expect("rename the row as an older binary would have spelled it");

    assert!(
        ensure_derivations(&mut conn)
            .await
            .expect("boot after the rename")
            .is_empty(),
        "a rename must not enqueue a backfill"
    );
    let adopted = state_of(&pool, COUNT_DERIVATION).await;
    assert_eq!(adopted.backfill_state, "complete");
    assert_eq!(
        adopted.backfilled_rows, 7,
        "the old row's progress is carried over, not rebuilt"
    );
    let status = derivation_status(&mut conn).await.expect("status");
    assert!(
        !status
            .iter()
            .any(|entry| entry.name == "sd_posts.legacy_name"),
        "no unregistered leftover: the old row IS the new row: {status:?}"
    );

    // A stale hash under the old name is not a rename, so it is not adopted:
    // the derivation is enqueued fresh and the old row stays as a leftover.
    diesel::sql_query(
        "UPDATE _autumn_derivations SET name = 'sd_posts.legacy_name', definition_hash = 'stale' \
         WHERE name = ?",
    )
    .bind::<Text, _>(COUNT_DERIVATION)
    .execute(&mut conn)
    .await
    .expect("leave a row with a foreign hash");
    assert_eq!(
        ensure_derivations(&mut conn).await.expect("boot"),
        vec![COUNT_DERIVATION]
    );
    assert_eq!(
        state_of(&pool, COUNT_DERIVATION).await.backfill_state,
        "pending"
    );
    assert_eq!(
        state_of(&pool, "sd_posts.legacy_name").await.backfill_state,
        "complete",
        "a row with another definition's hash is left for the unregistered report"
    );
}

#[tokio::test]
async fn a_zero_batch_size_is_an_error_rather_than_a_silent_completion() {
    // `LIMIT 0` yields an empty page, and an empty page is how a sweep learns
    // it has reached the end of the table: a zero batch would mark every
    // derivation complete having repaired nothing. Refused at run time, not
    // only by a debug assertion.
    let pool = boot_pool("sd_zero_batch").await;
    let mut conn = pool.get().await.expect("conn");
    ensure_derivations(&mut conn).await.expect("first boot");
    let error = run_backfill(
        &mut conn,
        &BackfillOptions {
            batch_size: 0,
            max_batches: None,
        },
    )
    .await
    .expect_err("a zero batch must be refused");
    assert!(error.to_string().contains("batch_size"), "{error}");
    assert_eq!(
        state_of(&pool, COUNT_DERIVATION).await.backfill_state,
        "pending",
        "nothing was marked complete"
    );
}

#[tokio::test]
async fn a_resweep_re_enqueues_a_finished_derivation_under_its_own_hash() {
    // The settling pass after a rolling deployment: the definition has not
    // changed, so reconciliation would leave the row alone, but every parent
    // has to be visited once more. `resweep` puts the row back on the queue
    // with its hash intact, and the next sweep runs it to completion again.
    let pool = boot_pool("sd_resweep").await;
    let mut conn = pool.get().await.expect("conn");
    let post = seed_post(&pool, "settle").await;
    diesel::sql_query("INSERT INTO sd_comments (post_id, published, score) VALUES (?, 1, 3)")
        .bind::<BigInt, _>(post)
        .execute(&mut conn)
        .await
        .expect("a comment nobody counted");
    ensure_derivations(&mut conn).await.expect("first boot");
    run_backfill(&mut conn, &BackfillOptions::default())
        .await
        .expect("first sweep");
    let done = state_of(&pool, COUNT_DERIVATION).await;
    assert_eq!(done.backfill_state, "complete");
    assert_eq!(derived(&pool, "published_comment_count", post).await, 1);

    // An old replica's delta lands after the sweep passed this parent.
    diesel::sql_query("UPDATE sd_posts SET published_comment_count = 4 WHERE id = ?")
        .bind::<BigInt, _>(post)
        .execute(&mut conn)
        .await
        .expect("stale delta");

    resweep(&mut conn, COUNT_DERIVATION).await.expect("resweep");
    let queued = state_of(&pool, COUNT_DERIVATION).await;
    assert_eq!(queued.backfill_state, "pending");
    assert_eq!(queued.checkpoint, None);
    assert!(
        ensure_derivations(&mut conn)
            .await
            .expect("boot")
            .is_empty(),
        "the hash is unchanged, so reconciliation neither re-enqueues nor undoes the resweep"
    );
    assert_eq!(
        state_of(&pool, COUNT_DERIVATION).await.backfill_state,
        "pending"
    );

    let report = run_backfill(&mut conn, &BackfillOptions::default())
        .await
        .expect("settling sweep");
    assert!(
        report.completed.contains(&COUNT_DERIVATION.to_owned()),
        "{report:?}"
    );
    assert_eq!(derived(&pool, "published_comment_count", post).await, 1);
    assert_eq!(
        state_of(&pool, COUNT_DERIVATION).await.backfill_state,
        "complete"
    );

    resweep(&mut conn, "sd_posts.nope")
        .await
        .expect_err("an unregistered name is refused");
}
