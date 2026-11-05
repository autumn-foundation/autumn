//! A Postgres schema and a role per boundary (#3550), on a real database.
//!
//! **Requires Docker.** The test migrates three sets: core into `public`,
//! marketing into the `marketing` schema, and a plugin set into the `admin`
//! schema. It applies the generated grants. Then each boundary role reads
//! its own table, and gets `permission denied` on another boundary's table.

use autumn_web::AppState;
use autumn_web::config::DatabaseConfig;
use autumn_web::db_roles::{Boundary, DatabaseRoleConfig};
use autumn_web::graph::boundaries::BoundaryConfig;
use autumn_web::graph::grants::{self, CatalogTable};
use autumn_web::migrate::{EmbeddedMigrations, SchemaMigrations, embed_migrations, run_pending};
use autumn_web::reexports::diesel_migrations;
use diesel::QueryableByName;
use diesel::sql_types::{BigInt, Text};
use diesel_async::{AsyncConnection, AsyncPgConnection, RunQueryDsl, SimpleAsyncConnection};
use testcontainers::ImageExt as _;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

const CORE: EmbeddedMigrations = embed_migrations!("tests/fixtures/boundary_core_migrations");
const MARKETING: EmbeddedMigrations =
    embed_migrations!("tests/fixtures/boundary_marketing_migrations");
const PLUGIN: EmbeddedMigrations = embed_migrations!("tests/fixtures/boundary_plugin_migrations");

const BOUNDARIES: &str = r#"
[boundaries.core]
owns = ["contacts"]
role = "core"

[boundaries.marketing]
owns = ["campaigns"]
schema = "marketing"
role = "mkt"
may_read = ["contacts"]

[boundaries.admin]
owns = ["admin_audit_log"]
crates = ["autumn-admin-plugin"]
schema = "admin"
role = "adm"
"#;

struct Marketing;
impl Boundary for Marketing {
    const NAME: &'static str = "marketing";
}

struct Admin;
impl Boundary for Admin {
    const NAME: &'static str = "admin";
}

diesel::table! {
    marketing.campaigns (id) {
        id -> BigInt,
        name -> Text,
    }
}

#[derive(QueryableByName)]
struct Count {
    #[diesel(sql_type = BigInt)]
    n: i64,
}

#[derive(QueryableByName)]
struct Name {
    #[diesel(sql_type = Text)]
    name: String,
}

async fn start_postgres() -> (testcontainers::ContainerAsync<Postgres>, String) {
    let container = Postgres::default()
        .with_tag("16-alpine")
        .start()
        .await
        .expect("start Postgres container");
    let host = container.get_host().await.expect("container host");
    let port = container
        .get_host_port_ipv4(5432)
        .await
        .expect("container port");
    let url = format!("postgres://postgres:postgres@{host}:{port}/postgres");
    (container, url)
}

fn role(user: &str) -> DatabaseRoleConfig {
    toml::from_str(&format!("user = \"{user}\"\npassword = \"{user}-pw\"")).expect("role")
}

async fn count(conn: &mut AsyncPgConnection, sql: &str) -> Result<i64, String> {
    diesel::sql_query(sql)
        .get_result::<Count>(conn)
        .await
        .map(|c| c.n)
        .map_err(|e| e.to_string())
}

fn migrate(url: &str, set: EmbeddedMigrations, schema: Option<&'static str>) {
    run_pending(url, SchemaMigrations::new(set, schema)).expect("migrations run");
}

/// Core into `public`, marketing into `marketing` (with a non-transactional
/// index), and a plugin set into `admin`. A second run applies nothing.
async fn migrate_all(url: &str) {
    let url = url.to_owned();
    tokio::task::spawn_blocking(move || {
        migrate(&url, CORE, None);
        migrate(&url, MARKETING, Some("marketing"));
        migrate(&url, PLUGIN, Some("admin"));
        assert!(
            run_pending(&url, SchemaMigrations::new(MARKETING, Some("marketing")))
                .expect("second run")
                .applied
                .is_empty()
        );
    })
    .await
    .expect("migration thread");
}

/// Each table is in its schema; the migration history stays in `public`.
async fn assert_schemas(owner: &mut AsyncPgConnection) {
    let tables = diesel::sql_query(
        "SELECT table_schema || '.' || table_name AS name FROM information_schema.tables \
         WHERE table_name IN ('contacts', 'campaigns', 'admin_audit_log', \
         '__diesel_schema_migrations') ORDER BY 1",
    )
    .load::<Name>(owner)
    .await
    .expect("tables")
    .into_iter()
    .map(|n| n.name)
    .collect::<Vec<_>>();
    assert_eq!(
        tables,
        [
            "admin.admin_audit_log",
            "marketing.campaigns",
            "public.__diesel_schema_migrations",
            "public.contacts",
        ]
    );
    assert_eq!(
        count(
            owner,
            "SELECT count(*) AS n FROM pg_indexes \
             WHERE schemaname = 'marketing' AND indexname = 'campaigns_name_idx'"
        )
        .await,
        Ok(1),
        "the non-transactional migration ran in the schema"
    );
}

/// Apply the generated grants twice: the second apply changes nothing.
async fn apply_grants(owner: &mut AsyncPgConnection) {
    let config = BoundaryConfig::parse(BOUNDARIES).expect("config");
    let catalog = [
        CatalogTable::new("public", "contacts").with_sequence("contacts_id_seq"),
        CatalogTable::new("public", "__diesel_schema_migrations"),
        CatalogTable::new("marketing", "campaigns").with_sequence("campaigns_id_seq"),
        CatalogTable::new("admin", "admin_audit_log").with_sequence("admin_audit_log_id_seq"),
    ];
    let sql = grants::plan(&config, &catalog)
        .expect("plan")
        .sql()
        .expect("sql");
    owner.batch_execute(&sql).await.expect("grants apply");
    owner.batch_execute(&sql).await.expect("grants apply twice");
}

/// An app state with one pool per role, on the primary URL.
///
/// The boundary name keys the pool. The role name differs from the schema
/// name, so a bare table name resolves only through the role `search_path`
/// that the grants set.
fn role_state(url: &str) -> AppState {
    let mut database = DatabaseConfig {
        url: Some(url.to_owned()),
        pool_size: 2,
        ..Default::default()
    };
    for (boundary, user) in [("core", "core"), ("marketing", "mkt"), ("admin", "adm")] {
        database.roles.insert(boundary.to_owned(), role(user));
    }
    database.validate().expect("valid config");
    let state = AppState::for_test().with_pool(
        autumn_web::db::create_pool(&database)
            .expect("pool")
            .expect("url"),
    );
    state.insert_extension(autumn_web::db::create_role_pools(&database).expect("role pools"));
    state
}

fn assert_denied(result: Result<i64, String>) {
    let denied = result.expect_err("the role must not read the table");
    assert!(denied.contains("permission denied"), "{denied}");
}

/// Marketing: its own table by bare name and through the schema-qualified
/// `table!`; `may_read` on core; no admin table.
async fn assert_marketing(state: &AppState) {
    let pool = state.pool_for::<Marketing>().expect("marketing pool");
    let mut conn = pool.get().await.expect("marketing connection");
    diesel::sql_query("INSERT INTO campaigns (name) VALUES ('fall')")
        .execute(&mut conn)
        .await
        .expect("marketing writes its own table");
    let names: Vec<String> = {
        use diesel::QueryDsl as _;
        campaigns::table
            .select(campaigns::name)
            .load(&mut conn)
            .await
            .expect("marketing reads through the qualified table!")
    };
    assert_eq!(names, ["fall"]);
    assert_eq!(
        count(&mut conn, "SELECT count(*) AS n FROM public.contacts").await,
        Ok(0)
    );
    assert_denied(count(&mut conn, "SELECT count(*) AS n FROM admin.admin_audit_log").await);
    let denied = diesel::sql_query("INSERT INTO public.contacts (email) VALUES ('x@y')")
        .execute(&mut conn)
        .await
        .expect_err("may_read gives no write")
        .to_string();
    assert!(denied.contains("permission denied"), "{denied}");
}

/// Admin (the plugin set): its own table, not marketing's or core's.
async fn assert_admin(state: &AppState) {
    let pool = state.pool_for::<Admin>().expect("admin pool");
    let mut conn = pool.get().await.expect("admin connection");
    assert_eq!(
        count(&mut conn, "SELECT count(*) AS n FROM admin_audit_log").await,
        Ok(0)
    );
    assert_denied(count(&mut conn, "SELECT count(*) AS n FROM marketing.campaigns").await);
    assert_denied(count(&mut conn, "SELECT count(*) AS n FROM public.contacts").await);
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn boundary_roles_read_their_own_tables_and_are_denied_others() {
    let (_container, url) = start_postgres().await;
    let mut owner = AsyncPgConnection::establish(&url).await.expect("connect");
    owner
        .batch_execute(
            "CREATE ROLE core LOGIN PASSWORD 'core-pw'; \
             CREATE ROLE mkt LOGIN PASSWORD 'mkt-pw'; \
             CREATE ROLE adm LOGIN PASSWORD 'adm-pw';",
        )
        .await
        .expect("roles");
    migrate_all(&url).await;
    assert_schemas(&mut owner).await;
    apply_grants(&mut owner).await;
    let state = role_state(&url);
    assert_marketing(&state).await;
    assert_admin(&state).await;
}
