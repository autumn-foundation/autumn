//! Cross-tenant `#[cached]` read (Warden 2026-09-05).
//!
//! Composes two documented framework features exactly as the framework's own
//! SaaS starter does (`examples/saas/src/repositories.rs`): `[tenancy]
//! enabled = true` (`docs/guide/tenant-cells.md`) and `#[cached]` memoizing a
//! read over a `tenant_scoped` `#[repository]` (`docs/guide/cache-coherence.md`).
//!
//! `#[cached]`'s generated cache key is built exclusively from the function's
//! own *explicit* parameters (every parameter by default, or exactly the
//! parameters named in `key(...)`). It never consults the ambient
//! `CURRENT_TENANT` task-local that a `tenant_scoped` repository read filters
//! by — so a cached function whose only *other* varying parameter is not the
//! tenant (a `page`, a `format`, a filter — anything that is not the literal
//! `tenant_id` the SaaS starter's own `cached_project_count` goes out of its
//! way to thread through `key(tenant_id)`) shares one cache slot across every
//! tenant that ever calls it with the same non-tenant arguments.
//!
//! The framework's own example shows the *correct* pattern (thread `tenant_id`
//! explicitly and name it in `key(...)`) but nothing in the macro, the
//! build-time cache-coherence gate (`autumn cache audit`), or `autumn routes
//! audit` detects or prevents its omission — and Autumn's own tenancy idiom
//! never requires threading `tenant_id` through a function signature for
//! every *other* tenant-scoped operation (`tenant_scoped` repository finders
//! resolve it from `CURRENT_TENANT` automatically). A developer who caches a
//! `tenant_scoped` read keyed on some other legitimate parameter (rather than
//! the tenant) has done nothing the documentation told them not to do.

#![cfg(all(feature = "db", feature = "cache-moka"))]

use autumn_web::config::AutumnConfig;
use autumn_web::test::TestApp;
use autumn_web::{AutumnResult, get, public, routes};
use diesel_async::pooled_connection::AsyncDieselConnectionManager;
use diesel_async::pooled_connection::deadpool::Pool;
use diesel_async::{AsyncPgConnection, RunQueryDsl};
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

mod schema {
    autumn_web::reexports::diesel::table! {
        cts_widgets (id) {
            id -> Int8,
            tenant_id -> Text,
            label -> Text,
        }
    }
}
use schema::cts_widgets;

#[autumn_web::model]
pub struct CtsWidget {
    #[id]
    pub id: i64,
    #[default]
    pub tenant_id: String,
    pub label: String,
}

/// Mirrors `examples/saas/src/repositories.rs`'s `ProjectRepository`: every
/// read is filtered by the ambient tenant, enforced at the SQL level.
#[autumn_web::repository(CtsWidget, table = "cts_widgets", tenant_scoped)]
pub trait CtsWidgetRepository {}

/// The buggy-but-natural sibling of the SaaS starter's `cached_project_count`:
/// keyed on a real, legitimate, non-tenant parameter (`format`) instead of
/// `tenant_id`. `repo` is correctly kept out of the key (it is a per-request
/// handle, not part of the value's identity — exactly per the starter's own
/// `key(tenant_id)` comment) but nothing forces `tenant_id` to be named too.
#[autumn_web::cached(ttl = "60s", key(format), result)]
pub async fn cached_widget_labels(
    format: &'static str,
    repo: &PgCtsWidgetRepository,
) -> AutumnResult<String> {
    let widgets = repo.find_all().await?;
    let labels = widgets
        .into_iter()
        .map(|w| w.label)
        .collect::<Vec<_>>()
        .join(",");
    Ok(format!("{format}:{labels}"))
}

/// `tenant_scoped` resolves the tenant from `CURRENT_TENANT` automatically —
/// nothing here ever names a tenant explicitly, exactly like
/// `examples/saas/src/routes/dashboard.rs`'s `dashboard` handler.
#[get("/widgets/summary")]
#[public]
async fn widgets_summary(repo: PgCtsWidgetRepository) -> AutumnResult<String> {
    cached_widget_labels("csv", &repo).await
}

fn tenancy_config() -> AutumnConfig {
    let mut config = AutumnConfig::default();
    config.tenancy.enabled = true;
    "header".clone_into(&mut config.tenancy.source);
    "x-tenant-id".clone_into(&mut config.tenancy.header_name);
    config
}

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
        "CREATE TABLE cts_widgets (\
           id BIGSERIAL PRIMARY KEY, \
           tenant_id TEXT NOT NULL, \
           label TEXT NOT NULL)",
    )
    .execute(&mut conn)
    .await
    .expect("DDL failed");
    drop(conn);

    (pool, container)
}

/// Issue: tenant B receives tenant A's cached `#[cached]` result because the
/// generated cache key is `hash(format)` alone — no tenant component.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn cached_tenant_scoped_read_leaks_across_tenants() {
    let (pool, _container) = setup_pool().await;
    let mut conn = pool.get().await.expect("conn");
    diesel::sql_query(
        "INSERT INTO cts_widgets (tenant_id, label) VALUES ('tenant-a', 'tenant-a-sentinel')",
    )
    .execute(&mut conn)
    .await
    .expect("seed tenant A");
    diesel::sql_query(
        "INSERT INTO cts_widgets (tenant_id, label) VALUES ('tenant-b', 'tenant-b-sentinel')",
    )
    .execute(&mut conn)
    .await
    .expect("seed tenant B");
    drop(conn);

    let app = TestApp::new()
        .config(tenancy_config())
        .with_db(pool)
        .routes(routes![widgets_summary])
        .build();

    let first = app
        .get("/widgets/summary")
        .header("x-tenant-id", "tenant-a")
        .send()
        .await;
    first.assert_ok();
    assert!(
        first.text().contains("tenant-a-sentinel"),
        "tenant A's own request must see its own sentinel: {:?}",
        first.text()
    );

    let second = app
        .get("/widgets/summary")
        .header("x-tenant-id", "tenant-b")
        .send()
        .await;
    second.assert_ok();
    assert!(
        !second.text().contains("tenant-a-sentinel"),
        "tenant B received tenant A's cached #[cached] response body: {:?} \
         (the cache key never carried the resolved tenant)",
        second.text()
    );
    assert!(
        second.text().contains("tenant-b-sentinel"),
        "tenant B must see its own data once isolated: {:?}",
        second.text()
    );
}
