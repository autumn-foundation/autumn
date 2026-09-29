//! Database integration tests using Autumn's `TestApp` + `TestClient`
//! with a real Postgres testcontainer.
//!
//! Demonstrates the "shared container" pattern where one Postgres
//! instance is reused across all tests in the binary, dramatically
//! reducing test suite runtime compared to one-container-per-test.
//!
//! **Requires Docker** to be running.

#[cfg(feature = "db")]
mod db_tests {
    use autumn_web::prelude::*;
    use autumn_web::test::TestApp;
    use diesel::prelude::*;
    use diesel_async::pooled_connection::AsyncDieselConnectionManager;
    use diesel_async::pooled_connection::deadpool::Pool;
    use diesel_async::{AsyncPgConnection, RunQueryDsl};
    use testcontainers::runners::AsyncRunner;
    use testcontainers_modules::postgres::Postgres;
    use tokio::sync::OnceCell;

    // ── Schema ─────────────────────────────────────────────────

    diesel::table! {
        test_items (id) {
            id -> Int8,
            name -> Text,
            quantity -> Int4,
        }
    }

    #[derive(Debug, Queryable, Selectable, serde::Serialize)]
    #[diesel(table_name = test_items)]
    struct Item {
        pub id: i64,
        pub name: String,
        pub quantity: i32,
    }

    #[derive(Debug, Insertable, serde::Deserialize)]
    #[diesel(table_name = test_items)]
    struct NewItem {
        pub name: String,
        pub quantity: i32,
    }

    // ── Shared container ───────────────────────────────────────

    struct SharedDb {
        _container: testcontainers::ContainerAsync<Postgres>,
        pool: Pool<AsyncPgConnection>,
        #[allow(dead_code)]
        url: String,
    }

    static SHARED_DB: OnceCell<SharedDb> = OnceCell::const_new();

    async fn shared_db() -> &'static SharedDb {
        SHARED_DB
            .get_or_init(|| async {
                let container = Postgres::default()
                    .start()
                    .await
                    .expect("failed to start Postgres container");

                let host = container.get_host().await.unwrap();
                let port = container.get_host_port_ipv4(5432).await.unwrap();
                let url = format!("postgres://postgres:postgres@{host}:{port}/postgres");

                let manager = AsyncDieselConnectionManager::<AsyncPgConnection>::new(&url);
                let pool = Pool::builder(manager)
                    .max_size(5)
                    .build()
                    .expect("failed to build pool");

                SharedDb {
                    _container: container,
                    pool,
                    url,
                }
            })
            .await
    }

    async fn setup() -> Pool<AsyncPgConnection> {
        let db = shared_db().await;
        let mut conn = db.pool.get().await.unwrap();
        diesel::sql_query(
            "CREATE TABLE IF NOT EXISTS test_items (
                id BIGSERIAL PRIMARY KEY,
                name TEXT NOT NULL,
                quantity INT NOT NULL DEFAULT 0
            )",
        )
        .execute(&mut *conn)
        .await
        .unwrap();
        diesel::sql_query("TRUNCATE test_items RESTART IDENTITY")
            .execute(&mut *conn)
            .await
            .unwrap();
        db.pool.clone()
    }

    // ── Handlers that use the Db extractor ─────────────────────

    #[get("/items")]
    async fn list_items(mut db: Db) -> AutumnResult<Json<Vec<Item>>> {
        let items = test_items::table
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
        let item = diesel::insert_into(test_items::table)
            .values(&new_item)
            .returning(Item::as_returning())
            .get_result(&mut *db)
            .await?;
        Ok((axum::http::StatusCode::CREATED, Json(item)))
    }

    #[get("/items/{id}")]
    async fn get_item(
        mut db: Db,
        axum::extract::Path(id): axum::extract::Path<i64>,
    ) -> AutumnResult<Json<Item>> {
        let item = test_items::table
            .find(id)
            .select(Item::as_select())
            .first(&mut *db)
            .await
            .map_err(|_| AutumnError::not_found_msg("item not found"))?;
        Ok(Json(item))
    }

    fn build_client(pool: Pool<AsyncPgConnection>) -> autumn_web::test::TestClient {
        TestApp::new()
            .routes(routes![list_items, create_item, get_item])
            .with_db(pool)
            .build()
    }

    // ── Tests ──────────────────────────────────────────────────

    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn list_items_empty() {
        let pool = setup().await;
        let client = build_client(pool);

        client
            .get("/items")
            .send()
            .await
            .assert_ok()
            .assert_body_eq("[]");
    }

    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn create_and_retrieve_item() {
        let pool = setup().await;
        let client = build_client(pool);

        // Create
        let resp = client
            .post("/items")
            .json(&serde_json::json!({"name": "Widget", "quantity": 10}))
            .send()
            .await;
        resp.assert_status(201)
            .assert_json::<serde_json::Value, _>(|item| {
                assert_eq!(item["name"], "Widget");
                assert_eq!(item["quantity"], 10);
                assert!(item["id"].as_i64().unwrap() > 0);
            });

        let created: serde_json::Value = resp.json();
        let id = created["id"].as_i64().unwrap();

        // Retrieve
        client
            .get(&format!("/items/{id}"))
            .send()
            .await
            .assert_ok()
            .assert_json::<serde_json::Value, _>(|item| {
                assert_eq!(item["name"], "Widget");
                assert_eq!(item["id"], id);
            });
    }

    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn create_multiple_items_and_list() {
        let pool = setup().await;
        let client = build_client(pool);

        client
            .post("/items")
            .json(&serde_json::json!({"name": "Sprocket", "quantity": 5}))
            .send()
            .await
            .assert_status(201);

        client
            .post("/items")
            .json(&serde_json::json!({"name": "Gadget", "quantity": 3}))
            .send()
            .await
            .assert_status(201);

        client
            .get("/items")
            .send()
            .await
            .assert_ok()
            .assert_json::<Vec<serde_json::Value>, _>(|items| {
                assert_eq!(items.len(), 2);
            });
    }

    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn get_nonexistent_item_returns_404() {
        let pool = setup().await;
        let client = build_client(pool);

        client.get("/items/9999").send().await.assert_status(404);
    }

    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn shared_container_is_reused() {
        let db1 = shared_db().await;
        let db2 = shared_db().await;
        assert_eq!(
            db1.url, db2.url,
            "shared_db() should return the same container"
        );
    }
}
