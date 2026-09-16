//! Prove that an application model drives the admin plugin on BOTH backends
//! (issue #2108).
//!
//! This is the user-facing claim of the issue. The plugin now compiles against
//! `autumn_web::RuntimeConnection`, so an app on `SQLite` can register its own
//! [`AdminModel`] and get the admin UI over its own tables. The three built-in
//! models (`tokens`, `experiments`, `feature_flags`) stay Postgres-only,
//! because the substrate they read is Postgres-only — see the README.
//!
//! `WidgetAdminModel` below is the model such an app writes. It shows the four
//! rules that portable admin SQL obeys:
//!
//! 1. **Write `$N` placeholders in ascending order, each one once.** Postgres
//!    reads the digits. `SQLite` numbers `$N` by FIRST APPEARANCE, and gives
//!    one index per distinct name. `SET name = $2 … WHERE id = $1` therefore
//!    binds the id into `name` on `SQLite`. To repeat a value, bind it twice
//!    under two placeholders.
//! 2. **No `ILIKE`.** Write `LOWER(col) LIKE LOWER($1)`. Both backends then
//!    match without case for ASCII text. `SQLite`'s `lower()` folds ASCII only.
//! 3. **No `::type` cast, no `ANY($1)` array bind, and no writable CTE.**
//!    `SQLite` has none of the three. Use `CAST(x AS TEXT)`, an `IN` list, and
//!    separate statements in a transaction.
//! 4. **`NOW()` is `CURRENT_TIMESTAMP`**, and a timestamp column reads as
//!    `Timestamp`/`NaiveDateTime`, never `Timestamptz`.
//!
//! `autumn_web::backend_select!` picks the pool URL and the DDL, so one test
//! body runs on Postgres and on `SQLite`.
//!
//! Run it:
//!
//! ```text
//! # SQLite — no server, no Docker
//! cargo test -p autumn-admin-plugin --features autumn-web/sqlite \
//!   --test custom_admin_model -- --ignored
//!
//! # Postgres — Docker, or a URL in AUTUMN_ADMIN_TEST_PG_URL
//! cargo test -p autumn-admin-plugin --test custom_admin_model -- --ignored
//! ```
//!
//! The database-backed test is `#[ignore]`d, so a default
//! `cargo test --workspace` never starts a database. Each lane names the target
//! in `.github/workflows/ci.yml`.

use autumn_admin_plugin::{
    AdminError, AdminField, AdminFieldKind, AdminFuture, AdminModel, AdminPlugin, ListParams,
    ListResult, SortDirection,
};
use diesel_async::pooled_connection::deadpool::Pool;
use serde_json::Value;

#[path = "support/pg_fixture.rs"]
#[allow(dead_code, reason = "the Postgres arm alone uses this module")]
mod pg_fixture;

// ── The application's own admin model ────────────────────────────────────────

/// One row of `widgets`.
#[derive(diesel::QueryableByName)]
struct WidgetRow {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    id: i64,
    #[diesel(sql_type = diesel::sql_types::Text)]
    name: String,
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    quantity: i64,
}

impl WidgetRow {
    fn into_json(self) -> Value {
        serde_json::json!({ "id": self.id, "name": self.name, "quantity": self.quantity })
    }
}

/// One `COUNT(*)` row.
#[derive(diesel::QueryableByName)]
struct CountRow {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    count: i64,
}

/// An admin model an application writes over its own `widgets` table.
///
/// Every statement is portable: `$N` placeholders, `LIKE` on a lowered column
/// instead of Postgres `ILIKE`, and no cast. Both backends run it as written.
#[derive(Debug, Default, Clone)]
struct WidgetAdminModel;

type AdminPool = Pool<::autumn_web::RuntimeConnection>;

impl AdminModel for WidgetAdminModel {
    fn slug(&self) -> &'static str {
        "widgets"
    }

    fn display_name(&self) -> &'static str {
        "Widget"
    }

    fn display_name_plural(&self) -> &'static str {
        "Widgets"
    }

    fn fields(&self) -> Vec<AdminField> {
        vec![
            AdminField::new("name", AdminFieldKind::Text)
                .label("Name")
                .searchable(),
            AdminField::new("quantity", AdminFieldKind::Integer).label("Quantity"),
        ]
    }

    fn list(&self, pool: &AdminPool, params: ListParams) -> AdminFuture<'_, ListResult> {
        use diesel_async::RunQueryDsl;

        let pool = pool.clone();
        Box::pin(async move {
            let mut conn = pool
                .get()
                .await
                .map_err(|e| AdminError::Database(e.to_string()))?;

            let per_page = params.per_page;
            let (offset, limit) = params.sql_offset_limit();
            let pattern = format!(
                "%{}%",
                params.search.as_deref().unwrap_or("").to_lowercase()
            );

            // One placeholder, one bind. SQLite gives each distinct `$name`
            // one index, so a statement that writes `$1` two times accepts
            // one bind only. A second bind then fails with a range error.
            let total: i64 = diesel::sql_query(
                "SELECT COUNT(*) AS count FROM widgets WHERE LOWER(name) LIKE $1",
            )
            .bind::<diesel::sql_types::Text, _>(&pattern)
            .get_result::<CountRow>(&mut conn)
            .await
            .map_or(0, |r| r.count);

            let records: Vec<Value> = diesel::sql_query(
                "SELECT id, name, quantity FROM widgets WHERE LOWER(name) LIKE $1 \
                 ORDER BY name LIMIT $2 OFFSET $3",
            )
            .bind::<diesel::sql_types::Text, _>(&pattern)
            .bind::<diesel::sql_types::BigInt, _>(limit)
            .bind::<diesel::sql_types::BigInt, _>(offset)
            .load::<WidgetRow>(&mut conn)
            .await
            .map(|rows| rows.into_iter().map(WidgetRow::into_json).collect())
            .map_err(|e| AdminError::Database(e.to_string()))?;

            Ok(ListResult {
                total: u64::try_from(total).unwrap_or(0),
                page: params.page,
                per_page,
                records,
            })
        })
    }

    fn get(&self, pool: &AdminPool, id: i64) -> AdminFuture<'_, Option<Value>> {
        use diesel::prelude::*;
        use diesel_async::RunQueryDsl;

        let pool = pool.clone();
        Box::pin(async move {
            let mut conn = pool
                .get()
                .await
                .map_err(|e| AdminError::Database(e.to_string()))?;
            diesel::sql_query("SELECT id, name, quantity FROM widgets WHERE id = $1")
                .bind::<diesel::sql_types::BigInt, _>(id)
                .get_result::<WidgetRow>(&mut conn)
                .await
                .optional()
                .map(|r| r.map(WidgetRow::into_json))
                .map_err(|e| AdminError::Database(e.to_string()))
        })
    }

    fn create(&self, pool: &AdminPool, data: Value) -> AdminFuture<'_, Value> {
        use diesel_async::RunQueryDsl;

        let pool = pool.clone();
        Box::pin(async move {
            let mut conn = pool
                .get()
                .await
                .map_err(|e| AdminError::Database(e.to_string()))?;
            let name = data
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| AdminError::Validation("'name' is required".into()))?;
            let quantity = data.get("quantity").and_then(Value::as_i64).unwrap_or(0);

            diesel::sql_query(
                "INSERT INTO widgets (name, quantity) VALUES ($1, $2) \
                 RETURNING id, name, quantity",
            )
            .bind::<diesel::sql_types::Text, _>(name)
            .bind::<diesel::sql_types::BigInt, _>(quantity)
            .get_result::<WidgetRow>(&mut conn)
            .await
            .map(WidgetRow::into_json)
            .map_err(|e| AdminError::Database(e.to_string()))
        })
    }

    fn update(&self, pool: &AdminPool, id: i64, data: Value) -> AdminFuture<'_, Value> {
        use diesel_async::RunQueryDsl;

        let pool = pool.clone();
        Box::pin(async move {
            let mut conn = pool
                .get()
                .await
                .map_err(|e| AdminError::Database(e.to_string()))?;
            let name = data
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| AdminError::Validation("'name' is required".into()))?;
            let quantity = data.get("quantity").and_then(Value::as_i64).unwrap_or(0);

            // The placeholders ascend in the order they are written. SQLite
            // numbers `$N` by first appearance, NOT by the digits, so
            // `SET name = $2 … WHERE id = $1` would bind the id into `name`.
            diesel::sql_query(
                "UPDATE widgets SET name = $1, quantity = $2 WHERE id = $3 \
                 RETURNING id, name, quantity",
            )
            .bind::<diesel::sql_types::Text, _>(name)
            .bind::<diesel::sql_types::BigInt, _>(quantity)
            .bind::<diesel::sql_types::BigInt, _>(id)
            .get_result::<WidgetRow>(&mut conn)
            .await
            .map(WidgetRow::into_json)
            .map_err(|e| AdminError::Database(e.to_string()))
        })
    }

    fn delete(&self, pool: &AdminPool, id: i64) -> AdminFuture<'_, ()> {
        use diesel_async::RunQueryDsl;

        let pool = pool.clone();
        Box::pin(async move {
            let mut conn = pool
                .get()
                .await
                .map_err(|e| AdminError::Database(e.to_string()))?;
            diesel::sql_query("DELETE FROM widgets WHERE id = $1")
                .bind::<diesel::sql_types::BigInt, _>(id)
                .execute(&mut conn)
                .await
                .map(|_| ())
                .map_err(|e| AdminError::Database(e.to_string()))
        })
    }
}

// ── Fixture ──────────────────────────────────────────────────────────────────

/// The `widgets` DDL. `BIGSERIAL` has no `SQLite` spelling, so the two forms
/// differ; the columns they make are the same.
#[allow(
    dead_code,
    reason = "one backend arm is dropped in any given build; the other uses this"
)]
const PG_DDL: &str = "CREATE TABLE widgets ( \
     id BIGSERIAL PRIMARY KEY, \
     name TEXT NOT NULL, \
     quantity BIGINT NOT NULL DEFAULT 0 \
 )";
#[allow(
    dead_code,
    reason = "one backend arm is dropped in any given build; the other uses this"
)]
const SQLITE_DDL: &str = "CREATE TABLE widgets ( \
     id INTEGER PRIMARY KEY AUTOINCREMENT, \
     name TEXT NOT NULL, \
     quantity BIGINT NOT NULL DEFAULT 0 \
 )";

/// Give each `SQLite` test its own shared-cache in-memory database.
#[allow(
    dead_code,
    reason = "one backend arm is dropped in any given build; the other uses this"
)]
static NEXT_DB: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Build a pool with a `widgets` table on the active backend.
///
/// The returned guard keeps the fixture alive. On `SQLite` it is the pool
/// itself: a shared-cache in-memory database lives only while a connection to
/// it is open.
async fn setup() -> (AdminPool, Box<dyn std::any::Any + Send>) {
    ::autumn_web::backend_select! {
        pg => {{
            let fixture = pg_fixture::setup(PG_DDL).await;
            (fixture.pool.clone(), Box::new(fixture) as Box<dyn std::any::Any + Send>)
        }},
        sqlite => {{
            use diesel_async::RunQueryDsl;

            let n = NEXT_DB.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let config = ::autumn_web::config::DatabaseConfig {
                url: Some(format!(
                    "sqlite://file:admin_widgets_{n}?mode=memory&cache=shared"
                )),
                primary_pool_size: Some(1),
                ..Default::default()
            };
            let pool: AdminPool = ::autumn_web::db::create_pool(&config)
                .expect("build a sqlite pool")
                .expect("a url is configured");
            {
                let mut conn = pool.get().await.expect("checkout a sqlite connection");
                diesel::sql_query(SQLITE_DDL)
                    .execute(&mut *conn)
                    .await
                    .expect("create widgets");
            }
            (pool.clone(), Box::new(pool) as Box<dyn std::any::Any + Send>)
        }},
    }
}

/// Default list parameters for one page of `per_page` records.
fn page_of(per_page: u64, search: Option<&str>) -> ListParams {
    ListParams {
        page: 1,
        per_page,
        search: search.map(str::to_owned),
        sort_by: None,
        sort_dir: SortDirection::default(),
        filters: Vec::new(),
    }
}

// ── The proof ────────────────────────────────────────────────────────────────

/// Drive the whole `AdminModel` surface of an application model.
///
/// Green on `SQLite` means an app on that backend can use the admin plugin.
#[tokio::test]
#[ignore = "needs a database: SQLite under --features autumn-web/sqlite, else Postgres"]
async fn a_custom_admin_model_runs_on_the_active_backend() {
    let (pool, _guard) = setup().await;
    let model = WidgetAdminModel;

    // create
    let created = model
        .create(
            &pool,
            serde_json::json!({ "name": "Sprocket", "quantity": 7 }),
        )
        .await
        .expect("create");
    let id = created["id"].as_i64().expect("id");
    assert_eq!(created["name"], "Sprocket");
    assert_eq!(created["quantity"], 7);

    // get
    let fetched = model.get(&pool, id).await.expect("get").expect("record");
    assert_eq!(fetched["name"], "Sprocket");
    assert!(
        model.get(&pool, 9_999_999).await.expect("get").is_none(),
        "an unknown id gives None, not an error"
    );

    // update
    let updated = model
        .update(
            &pool,
            id,
            serde_json::json!({ "name": "Cog", "quantity": 3 }),
        )
        .await
        .expect("update");
    assert_eq!(updated["name"], "Cog");
    assert_eq!(updated["quantity"], 3);

    // list, with search and pagination
    for name in ["Bolt", "Nut", "Washer"] {
        model
            .create(&pool, serde_json::json!({ "name": name, "quantity": 1 }))
            .await
            .expect("create");
    }
    let all = model.list(&pool, page_of(10, None)).await.expect("list");
    assert_eq!(all.total, 4);
    assert_eq!(all.records.len(), 4);

    // The search is case-insensitive on both backends.
    let one = model
        .list(&pool, page_of(10, Some("wash")))
        .await
        .expect("list");
    assert_eq!(one.total, 1);
    assert_eq!(one.records[0]["name"], "Washer");

    let first_page = model.list(&pool, page_of(2, None)).await.expect("list");
    assert_eq!(first_page.total, 4);
    assert_eq!(first_page.records.len(), 2);

    // count() defaults to list(per_page: 0).total
    assert_eq!(model.count(&pool).await.expect("count"), 4);

    // The trait default bulk "delete" action (traits.rs), which an application
    // model inherits. The built-in models override it and refuse on SQLite —
    // `every_built_in_model_method_refuses_on_sqlite` covers that.
    let ids: Vec<i64> = all
        .records
        .iter()
        .filter_map(|r| r["id"].as_i64())
        .collect();
    let applied = model
        .execute_action(&pool, "delete", ids.clone())
        .await
        .expect("bulk delete");
    assert_eq!(applied, ids.len() as u64);
    assert_eq!(model.count(&pool).await.expect("count"), 0);

    // An unknown action is refused, not silently ignored.
    assert!(
        model.execute_action(&pool, "nope", vec![1]).await.is_err(),
        "an unhandled action must error"
    );
}

/// `AdminPlugin::register` accepts the application model.
///
/// A COMPILE-time check, and the only test here that needs no database. A
/// plugin-level change that broke an app's own model fails to build this.
#[test]
fn the_plugin_accepts_the_custom_model() {
    // `register` takes `M: AdminModel`, so this line failing to compile is the
    // regression. Nothing here can assert more than that.
    let _plugin = AdminPlugin::new().register(WidgetAdminModel);
}

/// A Postgres-only built-in model refuses on `SQLite`, and answers on Postgres.
///
/// Before issue #2108 the crate did not compile under `autumn-web/sqlite`, so
/// registering `TokenAdminModel` there was a build error. It compiles now, so
/// the refusal must come from the model. Without it the operator sees a raw
/// driver message such as `near "ILIKE": syntax error`.
#[tokio::test]
#[ignore = "needs a database: SQLite under --features autumn-web/sqlite, else Postgres"]
async fn a_built_in_model_refuses_on_sqlite_and_answers_on_postgres() {
    use autumn_admin_plugin::tokens::TokenAdminModel;

    let (pool, _guard) = setup().await;
    let model = TokenAdminModel;
    let result = model.get(&pool, 1).await;

    ::autumn_web::backend_select! {
        pg => {{
            // `api_tokens` is absent from the widgets fixture, so Postgres
            // answers with its own error — not the guard's.
            let message = result
                .expect_err("api_tokens is absent from the widgets fixture")
                .to_string();
            assert!(
                !message.contains("needs the Postgres backend"),
                "the guard must not fire on Postgres, got: {message}"
            );
            assert!(
                message.contains("api_tokens"),
                "Postgres must report the missing relation, got: {message}"
            );
        }},
        sqlite => {{
            let message = result
                .expect_err("a Postgres-only model must refuse on SQLite")
                .to_string();
            assert!(
                message.contains("TokenAdminModel needs the Postgres backend"),
                "the refusal must name the model and the backend, got: {message}"
            );
            assert!(
                message.contains("README"),
                "the refusal must point at the README, got: {message}"
            );
        }},
    }
}

/// Every method of all three built-in models refuses on `SQLite`.
///
/// `require_postgres` has one call site for each. A missing one is what makes
/// an operator read `near "ILIKE": syntax error` off a 500 page, so check them
/// all, not one.
#[tokio::test]
#[ignore = "needs a database: SQLite under --features autumn-web/sqlite, else Postgres"]
async fn every_built_in_model_method_refuses_on_sqlite() {
    ::autumn_web::backend_select! {
        pg => {{
            // The refusal is a SQLite-only behaviour. On Postgres these models
            // are exercised by `token_admin_db`, `experiment_admin_db` and
            // `feature_flag_admin_db` against their real schemas.
        }},
        sqlite => {{
            use autumn_admin_plugin::experiments::ExperimentAdminModel;
            use autumn_admin_plugin::feature_flags::FeatureFlagAdminModel;
            use autumn_admin_plugin::tokens::TokenAdminModel;

            /// Assert one result is the refusal, and name the call that gave it.
            fn refuses<T: std::fmt::Debug>(what: &str, result: Result<T, AdminError>) {
                let message = result
                    .expect_err(&format!("{what} must refuse on SQLite"))
                    .to_string();
                assert!(
                    message.contains("needs the Postgres backend"),
                    "{what} must refuse with the guard's message, got: {message}"
                );
            }

            let (pool, _guard) = setup().await;
            let payload = serde_json::json!({ "name": "x", "key": "x", "principal_id": "x" });

            macro_rules! check {
                ($model:expr, $name:literal) => {{
                    let m = $model;
                    refuses(concat!($name, "::list"), m.list(&pool, page_of(10, None)).await);
                    refuses(concat!($name, "::get"), m.get(&pool, 1).await);
                    refuses(concat!($name, "::count"), m.count(&pool).await);
                    refuses(concat!($name, "::create"), m.create(&pool, payload.clone()).await);
                    refuses(concat!($name, "::update"), m.update(&pool, 1, payload.clone()).await);
                    refuses(concat!($name, "::delete"), m.delete(&pool, 1).await);
                    refuses(
                        concat!($name, "::execute_action"),
                        m.execute_action(&pool, "delete", vec![1]).await,
                    );
                }};
            }

            check!(TokenAdminModel, "TokenAdminModel");
            check!(ExperimentAdminModel, "ExperimentAdminModel");
            check!(FeatureFlagAdminModel, "FeatureFlagAdminModel");

            // `get_history` is only on the two models that have an audit trail.
            refuses(
                "ExperimentAdminModel::get_history",
                ExperimentAdminModel.get_history(&pool, 1, 1, 25).await,
            );
            refuses(
                "FeatureFlagAdminModel::get_history",
                FeatureFlagAdminModel.get_history(&pool, 1, 1, 25).await,
            );
        }},
    }
}
