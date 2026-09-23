//! Regression test for issue #2854: generated repository code must never decode
//! rows positionally against the table's physical column order.
//!
//! The fixture's struct field order deliberately differs from the physical
//! column order of its `table!` — `name` comes before `tenant_id` in the
//! struct, after it in the table — exactly the `SaaS` `Project` shape from the
//! issue. Before the fix, `save` decoded the `RETURNING *` row positionally,
//! so the tenant ID came back in `name` (and the dashboard rendered
//! `founder@acme.test` as the project name); the generated reads that decoded
//! `SELECT *` positionally had the same hazard.
//!
//! The test pins the physical ground truth with a raw column-ordered read, so
//! a swap can never hide behind two wrongs making a right.

#![cfg(feature = "db")]
#![allow(clippy::must_use_candidate, clippy::missing_const_for_fn)]

use autumn_web::Patch;
use diesel::QueryDsl;
use diesel_async::pooled_connection::AsyncDieselConnectionManager;
use diesel_async::pooled_connection::deadpool::Pool;
use diesel_async::{AsyncPgConnection, RunQueryDsl};
use testcontainers::ContainerAsync;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

diesel::table! {
    co_projects (id) {
        id -> Int8,
        tenant_id -> Text,
        name -> Text,
    }
}

/// Struct field order differs from the physical column order above on purpose:
/// this is the exact shape that triggered #2854.
#[autumn_web::model(table = "co_projects")]
pub struct CoProject {
    #[id]
    pub id: i64,
    pub name: String,
    pub tenant_id: String,
}

#[autumn_web::repository(CoProject, table = "co_projects")]
pub trait CoProjectRepository {}

static DB_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Keeps whichever backing Postgres alive for the duration of the test.
enum PgHandle {
    Container(#[allow(dead_code)] Box<ContainerAsync<Postgres>>),
    External,
}

async fn start_postgres() -> (PgHandle, Pool<AsyncPgConnection>) {
    if let Ok(url) = std::env::var("AUTUMN_TEST_PG_URL") {
        let manager = AsyncDieselConnectionManager::<AsyncPgConnection>::new(url);
        let pool = Pool::builder(manager)
            .max_size(5)
            .build()
            .expect("build pool");
        return (PgHandle::External, pool);
    }
    let container = Postgres::default()
        .start()
        .await
        .expect("start Postgres container");
    let host = container.get_host().await.expect("host");
    let port = container
        .get_host_port_ipv4(5432)
        .await
        .expect("Postgres port");
    let url = format!("postgres://postgres:postgres@{host}:{port}/postgres");
    let manager = AsyncDieselConnectionManager::<AsyncPgConnection>::new(url);
    let pool = Pool::builder(manager)
        .max_size(5)
        .build()
        .expect("build pool");
    (PgHandle::Container(Box::new(container)), pool)
}

async fn setup() -> (
    tokio::sync::MutexGuard<'static, ()>,
    PgHandle,
    Pool<AsyncPgConnection>,
) {
    let guard = DB_LOCK.lock().await;
    let (handle, pool) = start_postgres().await;
    let mut conn = pool.get().await.expect("conn");
    // Physical column order is id, tenant_id, name — deliberately different
    // from `CoProject`'s field order (id, name, tenant_id).
    if std::env::var("AUTUMN_TEST_PG_URL").is_ok() {
        diesel::sql_query("DROP TABLE IF EXISTS co_projects CASCADE")
            .execute(&mut conn)
            .await
            .expect("drop co_projects");
    }
    diesel::sql_query(
        "CREATE TABLE IF NOT EXISTS co_projects \
         (id BIGSERIAL PRIMARY KEY, tenant_id TEXT NOT NULL, name TEXT NOT NULL)",
    )
    .execute(&mut conn)
    .await
    .expect("create co_projects");
    diesel::sql_query("TRUNCATE co_projects RESTART IDENTITY")
        .execute(&mut conn)
        .await
        .expect("truncate co_projects");
    drop(conn);
    (guard, handle, pool)
}

/// `save`, the reads, and `update` must all keep `name` and `tenant_id` in
/// their own fields when the struct order differs from the column order.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn generated_code_does_not_swap_fields_when_struct_order_differs_from_column_order() {
    let (_guard, _pg, pool) = setup().await;
    let repo = PgCoProjectRepository::with_pool_untracked(pool.clone());

    // save: the RETURNING row must decode name/tenant_id into the right fields.
    let saved = repo
        .save(&NewCoProject {
            name: "Existing".to_string(),
            tenant_id: "founder@acme.test".to_string(),
        })
        .await
        .expect("save");
    assert_eq!(
        saved.name, "Existing",
        "save decoded tenant_id into name (#2854)"
    );
    assert_eq!(
        saved.tenant_id, "founder@acme.test",
        "save decoded name into tenant_id (#2854)"
    );

    // Physical ground truth: the columns themselves hold the inserted values.
    let mut conn = pool.get().await.expect("conn");
    let raw = co_projects::table
        .select((co_projects::tenant_id, co_projects::name))
        .first::<(String, String)>(&mut conn)
        .await
        .expect("raw read");
    assert_eq!(raw.0, "founder@acme.test");
    assert_eq!(raw.1, "Existing");

    // find_by_id and find_all decode through the SELECT column list.
    let loaded = repo
        .find_by_id(saved.id)
        .await
        .expect("find_by_id")
        .expect("row exists");
    assert_eq!(loaded.name, "Existing");
    assert_eq!(loaded.tenant_id, "founder@acme.test");
    let all = repo.find_all().await.expect("find_all");
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].name, "Existing");
    assert_eq!(all[0].tenant_id, "founder@acme.test");

    // update: the UPDATE ... RETURNING row must decode correctly too.
    let updated = repo
        .update(
            saved.id,
            &UpdateCoProject {
                name: Patch::Set("Renamed".to_string()),
                ..Default::default()
            },
        )
        .await
        .expect("update");
    assert_eq!(updated.name, "Renamed");
    assert_eq!(updated.tenant_id, "founder@acme.test");
}
