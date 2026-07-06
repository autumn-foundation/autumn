//! §1e: commit-hook, version-history, and idempotency verification for
//! `#[repository(tenant_scoped, sharded)]`.
//!
//! These features run *inside the mutation transaction*, which the
//! self-routing extractor opens on the **shard primary**. This test proves
//! that for a sharded save:
//!
//!   - the `_autumn_version_history` row lands on the shard DB,
//!   - the `autumn_repository_commit_hooks` queue row lands on the shard DB,
//!   - an HTTP idempotency replay does not double-write the shard.
//!
//! No production code change is expected — Section 5a already ships the
//! version-history and commit-hook migrations to shard targets; this is the
//! end-to-end confirmation on a real shard database.
//!
//! Run with:
//!
//!     cargo test --test sharding_commit_hooks -- --include-ignored

#[cfg(all(feature = "db", feature = "test-support"))]
mod sharding_commit_hook_tests {
    // The #[repository(commit_hooks = true)] expansion builds the idempotency
    // discriminator with a `let mut x = None; if let Some(..) = .. { x = .. }`
    // sequence. This is the first real (non-trybuild) test target to exercise
    // that codegen under clippy, so allow the generated-code idiom here.
    #![allow(clippy::useless_let_if_seq)]

    use autumn_web::config::ShardConfig;
    use autumn_web::prelude::*;
    use autumn_web::test::{TestApp, TestDb};
    use diesel::prelude::*;
    use diesel_async::{RunQueryDsl, SimpleAsyncConnection};

    // The real framework migration SQL applied to the shard, so the test
    // exercises exactly the schema Section 5a ships to shard targets.
    const VERSION_HISTORY_UP: &str =
        include_str!("../../version_history_migrations/20260526000000_create_version_history/up.sql");
    const COMMIT_HOOK_UP: &str = include_str!(
        "../../repository_commit_hook_migrations/20260515000000_create_repository_commit_hook_queue/up.sql"
    );

    // ── Schema ─────────────────────────────────────────────────

    diesel::table! {
        sharded_notes (id) {
            id -> Int8,
            title -> Text,
            tenant_id -> Text,
        }
    }

    #[autumn_web::model(table = "sharded_notes")]
    pub struct Note {
        #[id]
        pub id: i64,
        pub title: String,
        // tenant_scoped: framework-managed, omitted from `NewNote` and stamped
        // from the current tenant on insert.
        #[default]
        pub tenant_id: String,
    }

    // ── Hooks (commit_hooks = true requires a hooks type) ──────

    #[derive(Clone, Default)]
    pub struct NoteHooks;

    impl autumn_web::hooks::MutationHooks for NoteHooks {
        type Model = Note;
        type NewModel = NewNote;
        type UpdateModel = UpdateNote;

        // Overriding an after-*-commit hook is what causes the generated save
        // to durably stage a row into `autumn_repository_commit_hooks` inside
        // the mutation transaction (i.e. on the shard primary).
        async fn after_create_commit(
            &self,
            _ctx: &mut autumn_web::hooks::MutationContext,
            _record: &Note,
        ) -> AutumnResult<()> {
            Ok(())
        }
    }

    // Self-routing, tenant-scoped, sharded, versioned, with commit hooks.
    #[autumn_web::repository(
        Note,
        table = "sharded_notes",
        tenant_scoped,
        sharded,
        versioned = true,
        hooks = NoteHooks,
        commit_hooks = true
    )]
    pub trait NoteRepository {}

    // ── Handler ────────────────────────────────────────────────

    #[derive(serde::Deserialize)]
    struct NoteInput {
        title: String,
    }

    /// Save through the self-routing repository extractor. No `ShardedDb` in
    /// the signature — the generated `FromRequestParts` resolves tenant →
    /// shard and opens the write transaction on the routed shard primary.
    #[post("/notes")]
    async fn create_note(
        repo: PgNoteRepository,
        Json(input): Json<NoteInput>,
    ) -> AutumnResult<(axum::http::StatusCode, Json<Note>)> {
        // tenant_id is stamped from the current tenant by the tenant_scoped repo.
        let note = repo.save(&NewNote { title: input.title }).await?;
        Ok((axum::http::StatusCode::CREATED, Json(note)))
    }

    /// Establish the current tenant from an `X-Tenant` header for the whole
    /// request. `with_tenant` sets the `CURRENT_TENANT` task-local, which both
    /// the sharding extractor (shard routing key) and the `tenant_scoped`
    /// repository (`tenant_id` stamping) read — no `[tenancy]` config needed.
    async fn inject_tenant(
        request: axum::extract::Request,
        next: axum::middleware::Next,
    ) -> axum::response::Response {
        match request
            .headers()
            .get("X-Tenant")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
        {
            Some(tenant) => autumn_web::tenancy::with_tenant(tenant, next.run(request)).await,
            None => next.run(request).await,
        }
    }

    // ── Setup ──────────────────────────────────────────────────

    async fn setup_shard(db: &TestDb) {
        let mut conn = db.pool().get().await.expect("shard connection");
        conn.batch_execute(VERSION_HISTORY_UP)
            .await
            .expect("apply version-history migration to shard");
        conn.batch_execute(COMMIT_HOOK_UP)
            .await
            .expect("apply commit-hook migration to shard");
        conn.batch_execute(
            "CREATE TABLE IF NOT EXISTS sharded_notes (
                id BIGSERIAL PRIMARY KEY,
                title TEXT NOT NULL,
                tenant_id TEXT NOT NULL
            )",
        )
        .await
        .expect("create sharded_notes table");
    }

    async fn count(db: &TestDb, sql: &str) -> i64 {
        let mut conn = db.pool().get().await.expect("count connection");
        diesel::sql_query(sql)
            .get_result::<CountRow>(&mut *conn)
            .await
            .expect("count query")
            .n
    }

    #[derive(QueryableByName)]
    struct CountRow {
        #[diesel(sql_type = diesel::sql_types::BigInt)]
        n: i64,
    }

    // ── Tests ──────────────────────────────────────────────────

    /// A sharded save writes the version-history and commit-hook-queue rows on
    /// the shard database, and an HTTP idempotency replay does not double-write.
    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn sharded_save_records_history_and_hooks_on_shard() {
        let db = TestDb::shared().await;
        setup_shard(db).await;

        let shard = ShardConfig {
            name: "shard0".to_owned(),
            primary_url: db.url().to_owned(),
            ..Default::default()
        };

        let client = TestApp::new()
            .routes(routes![create_note])
            .layer(axum::middleware::from_fn(inject_tenant))
            .with_shards(vec![shard])
            .idempotent()
            .build();

        // 1. First create with idempotency key k1.
        client
            .post("/notes")
            .header("X-Tenant", "tenant-a")
            .header("idempotency-key", "note-k1")
            .json(&serde_json::json!({"title": "first"}))
            .send()
            .await
            .assert_status(201);

        // The version-history row is on the shard DB.
        assert_eq!(
            count(
                db,
                "SELECT COUNT(*) AS n FROM _autumn_version_history \
                 WHERE table_name = 'sharded_notes' AND op = 'insert'",
            )
            .await,
            1,
            "one version-history insert row must land on the shard"
        );

        // The commit-hook queue row is on the shard DB (staged inside the same
        // mutation transaction that wrote the note).
        assert!(
            count(
                db,
                "SELECT COUNT(*) AS n FROM autumn_repository_commit_hooks"
            )
            .await
                >= 1,
            "the after-commit hook must be staged on the shard's commit-hook queue"
        );

        // 2. Replay with the same idempotency key: the HTTP middleware replays
        //    the cached 201 without re-running the handler, so no second write.
        client
            .post("/notes")
            .header("X-Tenant", "tenant-a")
            .header("idempotency-key", "note-k1")
            .json(&serde_json::json!({"title": "first"}))
            .send()
            .await
            .assert_status(201);

        assert_eq!(
            count(db, "SELECT COUNT(*) AS n FROM sharded_notes").await,
            1,
            "idempotent replay must not insert a second note on the shard"
        );
        assert_eq!(
            count(
                db,
                "SELECT COUNT(*) AS n FROM _autumn_version_history \
                 WHERE table_name = 'sharded_notes'",
            )
            .await,
            1,
            "idempotent replay must not append a second version-history row"
        );

        // 3. A different idempotency key writes a second note and history row.
        client
            .post("/notes")
            .header("X-Tenant", "tenant-a")
            .header("idempotency-key", "note-k2")
            .json(&serde_json::json!({"title": "second"}))
            .send()
            .await
            .assert_status(201);

        assert_eq!(
            count(db, "SELECT COUNT(*) AS n FROM sharded_notes").await,
            2,
            "a fresh idempotency key writes a second note on the shard"
        );
    }
}
