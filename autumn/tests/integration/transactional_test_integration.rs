//! Integration tests for transactional DB test isolation.
//!
//! Run with:
//!
//!     cargo test --test transactional_test_integration -- --include-ignored

#[cfg(all(feature = "db", feature = "test-support"))]
mod transactional_tests {
    use autumn_web::config::ShardConfig;
    use autumn_web::prelude::*;
    use autumn_web::test::{TestApp, TestDb};
    use diesel::prelude::*;
    use diesel_async::RunQueryDsl;

    // ── Schema ─────────────────────────────────────────────────

    diesel::table! {
        transactional_items (id) {
            id -> Int8,
            name -> Text,
        }
    }

    #[derive(Debug, Queryable, Selectable, serde::Serialize)]
    #[diesel(table_name = transactional_items)]
    struct Item {
        pub id: i64,
        pub name: String,
    }

    #[derive(Debug, Insertable, serde::Deserialize)]
    #[diesel(table_name = transactional_items)]
    struct NewItem {
        pub name: String,
    }

    // ── Handlers ───────────────────────────────────────────────

    #[get("/items")]
    async fn list_items(mut db: Db) -> AutumnResult<Json<Vec<Item>>> {
        let items = transactional_items::table
            .select(Item::as_select())
            .load(&mut *db)
            .await?;
        Ok(Json(items))
    }

    #[post("/items")]
    async fn create_item(
        mut db: Db,
        Json(new_item): Json<NewItem>,
    ) -> AutumnResult<(axum::http::StatusCode, Json<Item>)> {
        let item = diesel::insert_into(transactional_items::table)
            .values(&new_item)
            .returning(Item::as_returning())
            .get_result(&mut *db)
            .await?;
        Ok((axum::http::StatusCode::CREATED, Json(item)))
    }

    static AFTER_COMMIT_RUN_COUNT: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0);

    #[post("/items-with-callback")]
    async fn create_item_with_callback(
        mut db: Db,
        Json(new_item): Json<NewItem>,
    ) -> AutumnResult<(axum::http::StatusCode, Json<Item>)> {
        use scoped_futures::ScopedFutureExt;
        db.tx(|conn| {
            async move {
                let item = diesel::insert_into(transactional_items::table)
                    .values(&new_item)
                    .returning(Item::as_returning())
                    .get_result(conn)
                    .await?;

                autumn_web::db::register_after_commit(|| async move {
                    AFTER_COMMIT_RUN_COUNT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Ok(())
                })
                .await;

                Ok::<_, diesel::result::Error>(item)
            }
            .scope_boxed()
        })
        .await
        .map(|item| (axum::http::StatusCode::CREATED, Json(item)))
    }

    // ── Setup ──────────────────────────────────────────────────

    static SETUP_CELL: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();

    async fn setup_table(db: &TestDb) {
        SETUP_CELL
            .get_or_init(|| async {
                db.execute_sql(
                    "CREATE TABLE IF NOT EXISTS transactional_items (
                    id BIGSERIAL PRIMARY KEY,
                    name TEXT NOT NULL
                )",
                )
                .await;
            })
            .await;
    }

    // ── Tests ──────────────────────────────────────────────────

    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn test_transactional_isolation() {
        let db = TestDb::shared().await;
        setup_table(db).await;

        // --- Phase 1: Insert item inside a transactional client ---
        {
            let client = TestApp::new()
                .routes(routes![list_items, create_item])
                .with_transactional_db(db.url())
                .build();

            // 1. Initially empty
            client
                .get("/items")
                .send()
                .await
                .assert_ok()
                .assert_body_eq("[]");

            // 2. Insert item
            client
                .post("/items")
                .json(&serde_json::json!({"name": "Persisted Item"}))
                .send()
                .await
                .assert_status(201);

            // 3. Verify it is visible inside the same test session
            client
                .get("/items")
                .send()
                .await
                .assert_ok()
                .assert_json::<Vec<serde_json::Value>, _>(|items| {
                    assert_eq!(items.len(), 1);
                    assert_eq!(items[0]["name"], "Persisted Item");
                });

            // client goes out of scope here and is dropped.
            // Under transactional isolation, the connection pool is dropped, closing the PostgreSQL connection and rolling back the transaction.
        }

        // --- Phase 2: Verify isolation in a fresh transactional client ---
        {
            let client = TestApp::new()
                .routes(routes![list_items])
                .with_transactional_db(db.url())
                .build();

            // Verify that the table is completely empty!
            // This proves that the first test client's uncommitted transaction was rolled back and did not leak any state.
            client
                .get("/items")
                .send()
                .await
                .assert_ok()
                .assert_body_eq("[]");
        }
    }

    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn test_after_commit_suppression() {
        let db = TestDb::shared().await;
        setup_table(db).await;

        // Reset the counter
        AFTER_COMMIT_RUN_COUNT.store(0, std::sync::atomic::Ordering::SeqCst);

        let client = TestApp::new()
            .routes(routes![create_item_with_callback])
            .with_transactional_db(db.url())
            .build();

        // Trigger the handler which registers an after-commit callback
        client
            .post("/items-with-callback")
            .json(&serde_json::json!({"name": "Item with Callback"}))
            .send()
            .await
            .assert_status(201);

        // Sleep to yield control and let any background task execute if suppression failed
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Verify that the callback was NOT run (suppressed due to transactional test mode)
        assert_eq!(
            std::sync::atomic::AtomicUsize::load(
                &AFTER_COMMIT_RUN_COUNT,
                std::sync::atomic::Ordering::SeqCst
            ),
            0,
            "after_commit callback should be suppressed in transactional test mode"
        );
    }

    // ── Sharded transactional isolation (§2) ───────────────────

    /// Insert a row through the routed shard's primary pool. Under a
    /// transactional `TestApp`, this write lands in the shard pool's
    /// rolled-back test transaction.
    #[post("/shard-items")]
    async fn create_item_on_shard(
        mut db: ShardedDb,
        Json(new_item): Json<NewItem>,
    ) -> AutumnResult<(axum::http::StatusCode, Json<Item>)> {
        let item = diesel::insert_into(transactional_items::table)
            .values(&new_item)
            .returning(Item::as_returning())
            .get_result(&mut *db)
            .await?;
        Ok((axum::http::StatusCode::CREATED, Json(item)))
    }

    /// Read rows back through the routed shard's pool.
    #[get("/shard-items")]
    async fn list_items_on_shard(mut db: ShardedDb) -> AutumnResult<Json<Vec<Item>>> {
        let items = transactional_items::table
            .select(Item::as_select())
            .load(&mut *db)
            .await?;
        Ok(Json(items))
    }

    /// Inject the shard routing key from an `X-Shard-Key` header so the
    /// sharding extractors can route without a full `[tenancy]` setup.
    async fn inject_shard_key(
        mut request: axum::extract::Request,
        next: axum::middleware::Next,
    ) -> axum::response::Response {
        if let Some(key) = request
            .headers()
            .get("X-Shard-Key")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
        {
            request
                .extensions_mut()
                .insert(autumn_web::prelude::ShardKeyOverride(key));
        }
        next.run(request).await
    }

    /// A transactional `TestApp` with `[[database.shards]]` rolls back writes
    /// routed to a shard's primary pool, exactly like the control pool. Phase 1
    /// inserts via `ShardedDb`; phase 2 (a fresh client) sees an empty table,
    /// proving the shard transaction was rolled back on client drop.
    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn test_sharded_transactional_isolation() {
        let db = TestDb::shared().await;
        setup_table(db).await;

        // One shard pointing at the shared container; the transactional shard
        // pool wraps it in a rolled-back test transaction (max_size 1).
        let shard = ShardConfig {
            name: "shard0".to_owned(),
            primary_url: db.url().to_owned(),
            ..Default::default()
        };

        // --- Phase 1: insert through the shard, observe it within the test ---
        {
            let client = TestApp::new()
                .routes(routes![create_item_on_shard, list_items_on_shard])
                .layer(axum::middleware::from_fn(inject_shard_key))
                .with_transactional_db(db.url())
                .with_shards(vec![shard.clone()])
                .build();

            client
                .get("/shard-items")
                .header("X-Shard-Key", "tenant-1")
                .send()
                .await
                .assert_ok()
                .assert_body_eq("[]");

            client
                .post("/shard-items")
                .header("X-Shard-Key", "tenant-1")
                .json(&serde_json::json!({"name": "Sharded Item"}))
                .send()
                .await
                .assert_status(201);

            client
                .get("/shard-items")
                .header("X-Shard-Key", "tenant-1")
                .send()
                .await
                .assert_ok()
                .assert_json::<Vec<serde_json::Value>, _>(|items| {
                    assert_eq!(items.len(), 1);
                    assert_eq!(items[0]["name"], "Sharded Item");
                });
        }

        // --- Phase 2: a fresh transactional client sees an empty shard ---
        {
            let client = TestApp::new()
                .routes(routes![list_items_on_shard])
                .layer(axum::middleware::from_fn(inject_shard_key))
                .with_transactional_db(db.url())
                .with_shards(vec![shard])
                .build();

            client
                .get("/shard-items")
                .header("X-Shard-Key", "tenant-1")
                .send()
                .await
                .assert_ok()
                .assert_body_eq("[]");
        }
    }
}
