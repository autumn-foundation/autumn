//! #2586 — the model's `#[validate(...)]` rules run on the repository **insert**
//! path, for every caller.
//!
//! `docs/guide/forms.md` promises a rule put on the `#[model]` covers every write
//! path — a form, an API endpoint, a seed, a job, a CSV import. Only the
//! generated `--api` handlers ran them, so a GraphQL resolver, a `#[task]`, a
//! seed or an admin action reached `save` and wrote a row the model forbids.
//!
//! Covered here, end to end against Postgres:
//! - `save` rejects an invalid payload with 422 and writes nothing;
//! - `#[normalize]` runs first, so a title of only spaces is blank (the issue's
//!   own reproduction) — normalize, then validate, then insert;
//! - a valid payload still stores the canonical value;
//! - `save_many` rejects the whole batch and leaves nothing behind;
//! - `save_many_skip_invalid` reports the rejected row by index and stores the
//!   rest, keeping its partial-success contract;
//! - on a hooked repository the rules run *before* `before_create`;
//! - `find_or_create_by_*` validates only when it is about to insert, so an
//!   existing row is still returned;
//! - update paths are untouched: the blind `update` still writes a value the
//!   model would reject.
//!
//! **Requires Docker** to be running.

#![cfg(feature = "db")]
#![allow(clippy::must_use_candidate, clippy::missing_const_for_fn)]

use autumn_web::hooks::{MutationContext, MutationHooks, Patch};
use autumn_web::prelude::*;
use autumn_web::tenancy::with_tenant;
use diesel::prelude::*;
use diesel_async::pooled_connection::AsyncDieselConnectionManager;
use diesel_async::pooled_connection::deadpool::Pool;
use diesel_async::{AsyncPgConnection, RunQueryDsl};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

// ── Plain repository (no hooks, no knob) ─────────────────────────────────────

diesel::table! {
    vi_notes (id) {
        id -> Int8,
        title -> Text,
        body -> Text,
    }
}

/// The issue's shape: a normalized, validated title on a plain repository.
#[autumn_web::model(table = "vi_notes")]
pub struct ViNote {
    #[id]
    pub id: i64,
    #[normalize(trim)]
    #[validate(length(min = 1, max = 120))]
    pub title: String,
    pub body: String,
}

#[autumn_web::repository(ViNote, table = "vi_notes")]
pub trait ViNoteRepository {
    /// Race-safe get-or-insert on the unique `title` column.
    fn find_or_create_by_title(title: String);
}

// ── Hooked repository ────────────────────────────────────────────────────────

diesel::table! {
    vi_hooked_notes (id) {
        id -> Int8,
        title -> Text,
        body -> Text,
    }
}

#[autumn_web::model(table = "vi_hooked_notes")]
pub struct ViHookedNote {
    #[id]
    pub id: i64,
    #[normalize(trim)]
    #[validate(length(min = 1, max = 120))]
    pub title: String,
    pub body: String,
}

/// Counts `before_create` calls, so a test can prove the hook never saw a row
/// the model rejects.
///
/// Read it back with [`hook_calls`]: a bare `calls.load(..)` resolves to
/// `diesel_async::RunQueryDsl::load`, which is blanket-implemented for every
/// `Sized` type and so wins method resolution over `AtomicUsize::load`.
#[derive(Clone, Default)]
pub struct ViHookedNoteHooks {
    before_create_calls: Arc<AtomicUsize>,
}

impl MutationHooks for ViHookedNoteHooks {
    type Model = ViHookedNote;
    type NewModel = NewViHookedNote;
    type UpdateModel = UpdateViHookedNote;

    async fn before_create(
        &self,
        _ctx: &mut MutationContext,
        _new: &mut NewViHookedNote,
    ) -> AutumnResult<()> {
        self.before_create_calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[autumn_web::repository(ViHookedNote, table = "vi_hooked_notes", hooks = ViHookedNoteHooks)]
pub trait ViHookedNoteRepository {}

// ── Tenant-scoped repository ─────────────────────────────────────────────────
//
// Present so the consolidated binary type-checks the tenant-filtered expansion
// of every insert path with validation spliced in.

diesel::table! {
    vi_tenant_notes (id) {
        id -> Int8,
        title -> Text,
        tenant_id -> Text,
    }
}

#[autumn_web::model(table = "vi_tenant_notes")]
pub struct ViTenantNote {
    #[id]
    pub id: i64,
    #[normalize(trim)]
    #[validate(length(min = 1, max = 120))]
    pub title: String,
    #[default]
    pub tenant_id: String,
}

#[autumn_web::repository(ViTenantNote, table = "vi_tenant_notes", tenant_scoped)]
pub trait ViTenantNoteRepository {}

// ── Sharded + tenant-scoped repository — compile-only ────────────────────────
//
// The sharded branch composes the cross-shard write guard with the new
// validation fragment. Compiling this fixture monomorphizes that branch.

diesel::table! {
    vi_sharded_notes (id) {
        id -> Int8,
        title -> Text,
        tenant_id -> Text,
    }
}

#[autumn_web::model(table = "vi_sharded_notes")]
pub struct ViShardedNote {
    #[id]
    pub id: i64,
    #[normalize(trim)]
    #[validate(length(min = 1, max = 120))]
    pub title: String,
    #[default]
    pub tenant_id: String,
}

#[autumn_web::repository(ViShardedNote, table = "vi_sharded_notes", tenant_scoped, sharded)]
pub trait ViShardedNoteRepository {
    /// Race-safe get-or-insert, guarded across shards.
    fn find_or_create_by_title(title: String);
}

// ── Model with no `#[validate]` columns ──────────────────────────────────────
//
// The autoref probe's no-op arm. This is what "no repository needs migrating"
// rests on, so it needs a repository-level fixture, not just a trait-level one.

diesel::table! {
    vi_lax_notes (id) {
        id -> Int8,
        title -> Text,
    }
}

#[autumn_web::model(table = "vi_lax_notes")]
pub struct ViLaxNote {
    #[id]
    pub id: i64,
    pub title: String,
}

#[autumn_web::repository(ViLaxNote, table = "vi_lax_notes")]
pub trait ViLaxNoteRepository {}

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
        "CREATE TABLE IF NOT EXISTS vi_notes \
         (id BIGSERIAL PRIMARY KEY, title TEXT NOT NULL UNIQUE, body TEXT NOT NULL)",
    )
    .execute(&mut conn)
    .await
    .expect("create vi_notes");
    diesel::sql_query(
        "CREATE TABLE IF NOT EXISTS vi_hooked_notes \
         (id BIGSERIAL PRIMARY KEY, title TEXT NOT NULL, body TEXT NOT NULL)",
    )
    .execute(&mut conn)
    .await
    .expect("create vi_hooked_notes");
    diesel::sql_query(
        "CREATE TABLE IF NOT EXISTS vi_tenant_notes \
         (id BIGSERIAL PRIMARY KEY, title TEXT NOT NULL, tenant_id TEXT NOT NULL)",
    )
    .execute(&mut conn)
    .await
    .expect("create vi_tenant_notes");
    diesel::sql_query(
        "CREATE TABLE IF NOT EXISTS vi_lax_notes \
         (id BIGSERIAL PRIMARY KEY, title TEXT NOT NULL)",
    )
    .execute(&mut conn)
    .await
    .expect("create vi_lax_notes");

    (pool, container)
}

/// `before_create` call count, read without tripping over `RunQueryDsl::load`.
fn hook_calls(counter: &AtomicUsize) -> usize {
    AtomicUsize::load(counter, Ordering::SeqCst)
}

fn new_note(title: &str) -> NewViNote {
    NewViNote {
        title: title.to_string(),
        body: "b".to_string(),
    }
}

async fn note_count(pool: &Pool<AsyncPgConnection>) -> i64 {
    let mut conn = pool.get().await.expect("conn");
    vi_notes::table
        .count()
        .get_result(&mut conn)
        .await
        .expect("count")
}

/// A `#[validate]` failure surfaces as 422 with the per-field map.
fn assert_unprocessable(err: &AutumnError, field: &str) {
    assert_eq!(
        err.status(),
        autumn_web::reexports::http::StatusCode::UNPROCESSABLE_ENTITY,
        "a model rule must fail with 422, got {err}"
    );
    let rendered = format!("{err:?}");
    assert!(
        rendered.contains(field),
        "the 422 must name the offending field `{field}`: {rendered}"
    );
}

// ── save ─────────────────────────────────────────────────────────────────────

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn save_rejects_a_payload_the_model_forbids() {
    let (pool, _container) = setup_pool().await;
    let repo = PgViNoteRepository::with_pool_untracked(pool.clone());

    let err = repo
        .save(&new_note(&"x".repeat(121)))
        .await
        .expect_err("a title over the maximum must be refused");
    assert_unprocessable(&err, "title");
    assert_eq!(note_count(&pool).await, 0, "nothing may be written");
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn save_normalizes_before_it_validates() {
    // The issue's own reproduction: `#[normalize(trim)]` turns "   " into "",
    // which the `length(min = 1)` rule must then reject.
    let (pool, _container) = setup_pool().await;
    let repo = PgViNoteRepository::with_pool_untracked(pool.clone());

    let err = repo
        .save(&new_note("   "))
        .await
        .expect_err("a title of only spaces is blank and must be refused");
    assert_unprocessable(&err, "title");
    assert_eq!(note_count(&pool).await, 0, "nothing may be written");
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn save_stores_the_canonical_value_when_the_payload_is_valid() {
    let (pool, _container) = setup_pool().await;
    let repo = PgViNoteRepository::with_pool_untracked(pool.clone());

    let saved = repo.save(&new_note("  hello  ")).await.expect("valid save");
    assert_eq!(saved.title, "hello", "the stored value stays canonical");
    assert_eq!(note_count(&pool).await, 1);
}

// ── save_many ────────────────────────────────────────────────────────────────

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn save_many_refuses_the_whole_batch_on_one_bad_row() {
    let (pool, _container) = setup_pool().await;
    let repo = PgViNoteRepository::with_pool_untracked(pool.clone());

    let err = repo
        .save_many(&[new_note("ok-1"), new_note("  "), new_note("ok-2")])
        .await
        .expect_err("one invalid row must abort the batch");
    assert_unprocessable(&err, "title");
    assert_eq!(
        note_count(&pool).await,
        0,
        "an aborted batch must leave nothing behind"
    );
}

// ── save_many_skip_invalid ───────────────────────────────────────────────────

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn skip_invalid_reports_the_bad_row_and_stores_the_rest() {
    let (pool, _container) = setup_pool().await;
    let repo = PgViNoteRepository::with_pool_untracked(pool.clone());

    let (saved, failures) = repo
        .save_many_skip_invalid(&[new_note("  keep-1  "), new_note("   "), new_note("keep-2")])
        .await
        .expect("skip-invalid never aborts on a rule failure");

    assert_eq!(saved.len(), 2, "the valid rows must be written");
    assert_eq!(failures.len(), 1, "the rejected row must be reported");
    assert_eq!(failures[0].0, 1, "reported against the caller's own index");
    assert_unprocessable(&failures[0].1, "title");

    let mut titles: Vec<String> = saved.into_iter().map(|n| n.title).collect();
    titles.sort();
    assert_eq!(
        titles,
        vec!["keep-1".to_string(), "keep-2".to_string()],
        "surviving rows are stored canonical"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn skip_invalid_maps_database_failures_to_the_callers_index() {
    // A row dropped by validation shifts the rest, so the constraint-violation
    // fallback must still report against the index the caller passed.
    let (pool, _container) = setup_pool().await;
    let repo = PgViNoteRepository::with_pool_untracked(pool.clone());
    repo.save(&new_note("taken")).await.expect("seed");

    let (saved, failures) = repo
        .save_many_skip_invalid(&[new_note("   "), new_note("taken"), new_note("fresh")])
        .await
        .expect("skip-invalid isolates the unique violation");

    assert_eq!(saved.len(), 1, "only the free title lands");
    let mut indices: Vec<usize> = failures.iter().map(|(i, _)| *i).collect();
    indices.sort_unstable();
    assert_eq!(
        indices,
        vec![0, 1],
        "the blank row and the duplicate row keep the caller's indices"
    );
}

// ── Hooks ────────────────────────────────────────────────────────────────────

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn hooked_save_validates_before_the_hook_runs() {
    let (pool, _container) = setup_pool().await;
    let repo = PgViHookedNoteRepository::with_pool_untracked(pool.clone());
    let calls = Arc::clone(&repo.hooks.before_create_calls);

    let err = repo
        .save(&NewViHookedNote {
            title: "   ".to_string(),
            body: "b".to_string(),
        })
        .await
        .expect_err("the model rule must fire first");
    assert_unprocessable(&err, "title");
    assert_eq!(
        hook_calls(&calls),
        0,
        "before_create must never see a row the model forbids"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn hooked_skip_invalid_validates_before_the_hook_runs() {
    let (pool, _container) = setup_pool().await;
    let repo = PgViHookedNoteRepository::with_pool_untracked(pool.clone());
    let calls = Arc::clone(&repo.hooks.before_create_calls);

    let (saved, failures) = repo
        .save_many_skip_invalid(&[
            NewViHookedNote {
                title: "  keep  ".to_string(),
                body: "b".to_string(),
            },
            NewViHookedNote {
                title: "   ".to_string(),
                body: "b".to_string(),
            },
        ])
        .await
        .expect("skip-invalid reports rather than aborts");

    assert_eq!(saved.len(), 1);
    assert_eq!(saved[0].title, "keep");
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].0, 1);
    assert_eq!(
        hook_calls(&calls),
        1,
        "the hook runs for the surviving row only"
    );
}

// ── find_or_create_by ────────────────────────────────────────────────────────

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn find_or_create_by_validates_only_when_it_creates() {
    let (pool, _container) = setup_pool().await;
    let repo = PgViNoteRepository::with_pool_untracked(pool.clone());

    let err = repo
        .find_or_create_by_title("   ".to_string(), &new_note("   "))
        .await
        .expect_err("the create path must apply the model rules");
    assert_unprocessable(&err, "title");
    assert_eq!(note_count(&pool).await, 0);

    // An existing row is returned untouched: this call inserts nothing, so the
    // payload it would have inserted is never judged.
    let seeded = repo.save(&new_note("present")).await.expect("seed");
    let (found, created) = repo
        .find_or_create_by_title("present".to_string(), &new_note(&"x".repeat(121)))
        .await
        .expect("a found row is returned regardless of the unused payload");
    assert!(!created, "the row already existed");
    assert_eq!(found.id, seeded.id);
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn find_or_create_by_matches_the_row_it_normalized() {
    // The create half normalizes its payload, so the lookup has to look for the
    // canonical value too. Otherwise the second identical call misses the row
    // it just wrote, conflicts on insert, misses the re-lookup, and surfaces
    // the "no matching row on re-lookup" 500 instead of the existing row.
    let (pool, _container) = setup_pool().await;
    let repo = PgViNoteRepository::with_pool_untracked(pool.clone());

    let (first, created) = repo
        .find_or_create_by_title("  spaced  ".to_string(), &new_note("  spaced  "))
        .await
        .expect("first call creates");
    assert!(created);
    assert_eq!(first.title, "spaced", "the row is stored canonical");

    let (second, created_again) = repo
        .find_or_create_by_title("  spaced  ".to_string(), &new_note("  spaced  "))
        .await
        .expect("the second identical call must find the row it wrote");
    assert!(!created_again, "no second row may be created");
    assert_eq!(second.id, first.id);
    assert_eq!(note_count(&pool).await, 1);
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn find_or_create_by_returns_an_existing_row_over_a_rejected_payload() {
    // The preliminary lookup is replica-eligible and a concurrent caller can
    // insert between it and the insert, so a refusal must not overtake the
    // found path: an existing row wins regardless of the unused payload.
    let (pool, _container) = setup_pool().await;
    let repo = PgViNoteRepository::with_pool_untracked(pool.clone());
    let seeded = repo.save(&new_note("settled")).await.expect("seed");

    let (found, created) = repo
        .find_or_create_by_title("settled".to_string(), &new_note("   "))
        .await
        .expect("an existing row is returned even for a payload the model rejects");
    assert!(!created);
    assert_eq!(found.id, seeded.id);
}

// ── Tenant-scoped ────────────────────────────────────────────────────────────

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn tenant_scoped_save_validates() {
    let (pool, _container) = setup_pool().await;
    let repo = PgViTenantNoteRepository::with_pool_untracked(pool.clone());

    let err = with_tenant("tenant-a".to_string(), async {
        repo.save(&NewViTenantNote {
            title: "   ".to_string(),
        })
        .await
    })
    .await
    .expect_err("tenant scoping does not exempt the model rules");
    assert_unprocessable(&err, "title");

    let mut conn = pool.get().await.expect("conn");
    let count: i64 = vi_tenant_notes::table
        .count()
        .get_result(&mut conn)
        .await
        .expect("count");
    assert_eq!(count, 0, "nothing may be written");
}

// ── Generated-surface guard (no database) ────────────────────────────────────

/// Every behavioural test here needs Docker. This one does not: it proves the
/// generated surface exists on each fixture, which keeps the compile-only
/// repositories (sharded, tenant-scoped) from reading as dead code and catches
/// a codegen break even where Docker cannot run.
#[test]
fn validate_on_insert_repository_surface_is_generated() {
    fn assert_is_fn<T>(_: T) {}

    assert_is_fn(PgViNoteRepository::find_or_create_by_title);
    assert_is_fn(PgViShardedNoteRepository::find_or_create_by_title);
    assert_is_fn(<PgViNoteRepository as ViNoteRepository>::save);
    assert_is_fn(<PgViHookedNoteRepository as ViHookedNoteRepository>::save_many_skip_invalid);
    assert_is_fn(<PgViTenantNoteRepository as ViTenantNoteRepository>::save_many);
    // The no-`#[validate]` model still gets the same surface — the autoref
    // probe compiled its no-op arm rather than failing to resolve.
    assert_is_fn(<PgViLaxNoteRepository as ViLaxNoteRepository>::save);
    assert_is_fn(<PgViLaxNoteRepository as ViLaxNoteRepository>::save_many);
}

// ── Empty and all-invalid inputs ─────────────────────────────────────────────

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn bulk_paths_accept_an_empty_slice() {
    let (pool, _container) = setup_pool().await;
    let repo = PgViNoteRepository::with_pool_untracked(pool.clone());

    assert!(repo.save_many(&[]).await.expect("empty batch").is_empty());
    let (saved, failures) = repo
        .save_many_skip_invalid(&[])
        .await
        .expect("empty skip-invalid batch");
    assert!(saved.is_empty() && failures.is_empty());
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn skip_invalid_survives_every_row_being_rejected() {
    // Nothing survives the filter, so the chunking pass must not run at all —
    // it reads `new[0]` for the column count.
    let (pool, _container) = setup_pool().await;
    let repo = PgViNoteRepository::with_pool_untracked(pool.clone());

    let (saved, failures) = repo
        .save_many_skip_invalid(&[new_note("   "), new_note("")])
        .await
        .expect("an all-invalid batch reports rather than panicking");

    assert!(saved.is_empty(), "nothing may be written");
    assert_eq!(
        failures.iter().map(|(i, _)| *i).collect::<Vec<_>>(),
        vec![0, 1],
        "both rows reported against their own index"
    );
    assert_eq!(note_count(&pool).await, 0);
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_model_without_rules_still_saves() {
    let (pool, _container) = setup_pool().await;
    let repo = PgViLaxNoteRepository::with_pool_untracked(pool.clone());

    let saved = repo
        .save(&NewViLaxNote {
            title: String::new(),
        })
        .await
        .expect("a model with no rules accepts anything it used to");
    assert_eq!(saved.title, "");
}

// ── Guard rail: updates are unchanged ────────────────────────────────────────

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn blind_update_still_writes_a_value_the_model_forbids() {
    // #2586 is an insert-path fix. The documented blind update path (no hooks,
    // no `validate_on_update = fetch`) keeps validating nothing.
    let (pool, _container) = setup_pool().await;
    let repo = PgViNoteRepository::with_pool_untracked(pool.clone());

    let saved = repo.save(&new_note("good")).await.expect("seed");
    let updated = repo
        .update(
            saved.id,
            &UpdateViNote {
                title: Patch::Set(String::new()),
                ..Default::default()
            },
        )
        .await
        .expect("the blind update path is deliberately unvalidated");
    assert_eq!(updated.title, "", "the blind path writes the raw patch");
}
