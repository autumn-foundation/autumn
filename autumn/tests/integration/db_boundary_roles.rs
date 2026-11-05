//! Database-enforced boundaries (#3550): a Postgres schema and role per
//! boundary.
//!
//! One test, against a real Postgres:
//!
//! 1. Migrations go into named schemas: an app set that names `core.` in its
//!    SQL, an app set run in `marketing` by `SchemaMigrations`, and a plugin
//!    set run in `admin`.
//! 2. `autumn_web::graph::grants` makes the GRANT and REVOKE SQL from the
//!    boundaries and the live catalog.
//! 3. `AppState::pool_for` gives each boundary a pool for its own role. That
//!    role reads its own table and gets `permission denied` on another
//!    boundary's table.
//! 4. The doctor's negative check (`db_boundary::check_role`) passes, finds a
//!    leak that a manual GRANT makes, and passes again after the grants are
//!    applied again.
//!
//! **Requires Docker**, or set `AUTUMN_TEST_PG_URL` to a Postgres superuser
//! URL. The test makes its own database and roles there.

use autumn_web::AppState;
use autumn_web::config::AutumnConfig;
use autumn_web::db_boundary::{BoundaryPools, DbBoundary, ReadProbe, check_role, connect};
use autumn_web::graph::boundaries::BoundaryConfig;
use autumn_web::graph::grants::{self, GrantPlan, QualifiedName};
use autumn_web::migrate::{EmbeddedMigrations, SchemaMigrations, embed_migrations};
use diesel_async::{RunQueryDsl, SimpleAsyncConnection};
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, ImageExt};
use testcontainers_modules::postgres::Postgres;

const CORE: EmbeddedMigrations = embed_migrations!("tests/fixtures/boundary_core_migrations");
const MARKETING: EmbeddedMigrations =
    embed_migrations!("tests/fixtures/boundary_marketing_migrations");
const PLUGIN: EmbeddedMigrations = embed_migrations!("tests/fixtures/plugin_migrations_ok");

struct Marketing;
impl DbBoundary for Marketing {
    const NAME: &'static str = "marketing";
}

struct Core;
impl DbBoundary for Core {
    const NAME: &'static str = "core";
}

/// Keeps the Postgres alive for the test.
enum PgHandle {
    Container(#[allow(dead_code)] Box<ContainerAsync<Postgres>>),
    External,
}

async fn start_postgres() -> (PgHandle, String) {
    if let Ok(url) = std::env::var("AUTUMN_TEST_PG_URL") {
        return (PgHandle::External, url);
    }
    let container = Postgres::default()
        .with_tag("16-alpine")
        .start()
        .await
        .expect("start Postgres container");
    let host = container.get_host().await.expect("host");
    let port = container.get_host_port_ipv4(5432).await.expect("port");
    let url = format!("postgres://postgres:postgres@{host}:{port}/postgres");
    (PgHandle::Container(Box::new(container)), url)
}

/// `url` with its database and login replaced.
fn url_for(url: &str, user: &str, password: &str, database: &str) -> String {
    let mut url = url::Url::parse(url).expect("server URL");
    url.set_username(user).expect("user");
    url.set_password(Some(password)).expect("password");
    url.set_path(&format!("/{database}"));
    url.to_string()
}

async fn exec(url: &str, sql: &str) {
    let mut conn = connect(url, 10).await.expect("connect");
    conn.batch_execute(sql)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
}

/// Apply a migration set as the app does, off the async runtime.
async fn migrate(url: &str, set: EmbeddedMigrations, schema: Option<&'static str>) -> Vec<String> {
    let url = url.to_owned();
    tokio::task::spawn_blocking(move || {
        autumn_web::migrate::run_pending(&url, SchemaMigrations::new(set, schema))
            .expect("migrations apply")
            .applied
    })
    .await
    .expect("migration thread")
}

async fn plan(admin: &str, boundaries: &BoundaryConfig) -> GrantPlan {
    let mut conn = connect(admin, 10).await.expect("connect");
    let catalog = autumn_web::db_boundary::load_catalog(&mut conn)
        .await
        .expect("catalog");
    grants::plan(boundaries, &catalog).expect("plan")
}

#[derive(diesel::QueryableByName)]
struct Count {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    n: i64,
}

async fn count(state: &AppState, pool_of: &str, table: &str) -> Result<i64, String> {
    let pool = state.boundary_pool(pool_of).expect("pool");
    let mut conn = pool.get().await.map_err(|e| e.to_string())?;
    diesel::sql_query(format!("SELECT count(*) AS n FROM {table}"))
        .get_result::<Count>(&mut conn)
        .await
        .map(|c| c.n)
        .map_err(|e| e.to_string())
}

fn assert_denied(result: Result<i64, String>, what: &str) {
    let err = result.expect_err(what);
    assert!(err.contains("permission denied"), "{what}: {err}");
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
#[allow(clippy::too_many_lines)] // one scenario, step by step, reads best end to end
async fn boundary_roles_read_their_own_tables_only() {
    let (_pg, server) = start_postgres().await;
    let n = std::process::id()
        ^ std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .subsec_nanos();
    let db = format!("boundary_{n}");
    let (core_role, mkt_role, adm_role) =
        (format!("core_{n}"), format!("mkt_{n}"), format!("adm_{n}"));
    exec(&server, &format!("CREATE DATABASE {db}")).await;
    for role in [&core_role, &mkt_role, &adm_role] {
        exec(&server, &format!("CREATE ROLE {role} LOGIN PASSWORD 'pw'")).await;
    }
    let server_url = url::Url::parse(&server).expect("server URL");
    let admin = url_for(
        &server,
        server_url.username(),
        server_url.password().unwrap_or_default(),
        &db,
    );

    // 1. Migrations into named schemas.
    assert_eq!(migrate(&admin, CORE, None).await.len(), 1);
    assert_eq!(migrate(&admin, MARKETING, Some("marketing")).await.len(), 2);
    assert_eq!(migrate(&admin, PLUGIN, Some("admin")).await.len(), 1);
    assert!(
        migrate(&admin, MARKETING, Some("marketing"))
            .await
            .is_empty()
    );

    // 2. Grants from the boundaries and the live catalog.
    let boundaries = BoundaryConfig::parse(&format!(
        r#"
[boundaries.core]
owns = ["contacts", "deals", "notes"]
schema = "core"
role = "{core_role}"

[boundaries.marketing]
owns = ["campaigns"]
schema = "marketing"
role = "{mkt_role}"
may_read = ["contacts"]
may_write = ["deals"]

[boundaries.admin]
owns = ["gizmos"]
schema = "admin"
role = "{adm_role}"
"#
    ))
    .expect("boundaries");
    let plan = plan(&admin, &boundaries).await;
    let marketing = plan
        .roles
        .iter()
        .find(|r| r.boundary == "marketing")
        .expect("marketing plan");
    assert_eq!(
        marketing.own,
        vec![QualifiedName::new("marketing", "campaigns")],
        "the marketing set went into schema marketing"
    );
    assert!(
        plan.roles
            .iter()
            .any(|r| r.own == vec![QualifiedName::new("admin", "gizmos")]),
        "the plugin set went into schema admin"
    );
    assert_eq!(
        marketing.sequences,
        vec![QualifiedName::new("core", "deals_id_seq")]
    );
    exec(&admin, &plan.to_sql()).await;

    // 3. A pool per boundary role.
    let config: AutumnConfig = toml::from_str(&format!(
        r#"
[database]
url = "{admin}"
pool_size = 2

[database.roles.marketing]
user = "{mkt_role}"
password = "pw"

[database.roles.core]
user = "{core_role}"
password = "pw"

[database.roles.admin]
user = "{adm_role}"
password = "pw"
"#
    ))
    .expect("config");
    config.database.validate().expect("valid config");
    let primary = autumn_web::db::create_pool(&config.database)
        .expect("pool")
        .expect("url");
    let state = AppState::for_test()
        .with_pool(primary)
        .with_boundary_pools(BoundaryPools::from_config(&config.database).expect("role pools"));
    assert!(state.has_boundary_role(Marketing::NAME));

    // The marketing role reads its own table and its `may_read` table.
    assert_eq!(
        count(&state, Marketing::NAME, "marketing.campaigns").await,
        Ok(0)
    );
    assert_eq!(count(&state, Marketing::NAME, "core.contacts").await, Ok(0));
    // It gets `permission denied` on core's other table and on admin's.
    assert_denied(
        count(&state, Marketing::NAME, "core.notes").await,
        "mkt reads core.notes",
    );
    assert_denied(
        count(&state, Marketing::NAME, "admin.gizmos").await,
        "mkt reads admin.gizmos",
    );
    // `may_read` gives no write; `may_write` does, sequence included.
    let mkt_pool = state.pool_for::<Marketing>().expect("pool");
    let mut mkt = mkt_pool.get().await.expect("conn");
    let err = mkt
        .batch_execute("INSERT INTO core.contacts (name) VALUES ('x')")
        .await
        .expect_err("may_read is read only");
    assert!(err.to_string().contains("permission denied"), "{err}");
    mkt.batch_execute("INSERT INTO core.deals (title) VALUES ('d')")
        .await
        .expect("may_write inserts with the serial sequence");
    mkt.batch_execute("INSERT INTO marketing.campaigns (name) VALUES ('c')")
        .await
        .expect("own table insert");
    drop(mkt);
    // Core gets `permission denied` on marketing's table.
    assert_denied(
        count(&state, Core::NAME, "marketing.campaigns").await,
        "core reads campaigns",
    );
    assert_eq!(count(&state, Core::NAME, "core.notes").await, Ok(0));
    // The plugin's role reads the plugin table in its schema.
    assert_eq!(count(&state, "admin", "admin.gizmos").await, Ok(0));

    // 4. The doctor's negative check passes for every role.
    for role in &plan.roles {
        let url = config
            .database
            .role_url(&role.boundary)
            .expect("url")
            .expect("role");
        let mut conn = connect(&url, 10).await.expect("connect as role");
        let report = check_role(&mut conn, role).await.expect("report");
        assert!(report.passed(), "{}", report.summary());
        assert!(report.denied > 0, "{}", report.summary());
    }

    // A manual grant is a leak, and the check finds it.
    exec(&admin, &format!("GRANT SELECT ON core.notes TO {mkt_role}")).await;
    let mkt_url = config
        .database
        .role_url("marketing")
        .expect("url")
        .expect("role");
    let mut conn = connect(&mkt_url, 10).await.expect("connect");
    assert_eq!(
        autumn_web::db_boundary::probe_read(&mut conn, &QualifiedName::new("core", "notes")).await,
        ReadProbe::Allowed
    );
    let report = check_role(&mut conn, marketing).await.expect("report");
    assert_eq!(
        report.leaks,
        vec![QualifiedName::new("core", "notes")],
        "{}",
        report.summary()
    );
    drop(conn);

    // The grants revoke first, so applying them again removes the leak.
    let plan = self::plan(&admin, &boundaries).await;
    exec(&admin, &plan.to_sql()).await;
    let mut conn = connect(&mkt_url, 10).await.expect("connect");
    let marketing = plan
        .roles
        .iter()
        .find(|r| r.boundary == "marketing")
        .expect("marketing plan");
    let report = check_role(&mut conn, marketing).await.expect("report");
    assert!(report.passed(), "{}", report.summary());
    drop(conn);
    drop(state);

    // Leave the server as it was.
    exec(&server, &format!("DROP DATABASE {db} WITH (FORCE)")).await;
    for role in [&core_role, &mkt_role, &adm_role] {
        exec(&server, &format!("DROP ROLE {role}")).await;
    }
}
