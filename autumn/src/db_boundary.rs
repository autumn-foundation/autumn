//! One Postgres role and pool per table-ownership boundary (#3550).
//!
//! A boundary in `boundaries.toml` can name a Postgres `schema`.
//! `autumn db grants` then gives the boundary's role access to that schema and
//! to its exceptions only. The app connects as that role through
//! `[database.roles.<boundary>]`:
//!
//! ```toml
//! [database.roles.marketing]
//! user = "marketing"          # on the primary URL; or `url = "postgres://…"`
//! ```
//!
//! Code in the boundary gets the role's pool with
//! [`AppState::pool_for`](crate::AppState::pool_for):
//!
//! ```rust,ignore
//! struct Marketing;
//! impl autumn_web::db_boundary::DbBoundary for Marketing {
//!     const NAME: &'static str = "marketing";
//! }
//!
//! let pool = state.pool_for::<Marketing>().expect("database configured");
//! ```
//!
//! The database then refuses a query on another boundary's table.
//!
//! [`load_catalog`] and [`check_role`] read the catalog and run the negative
//! check of `autumn doctor`.
//!
//! See `docs/guide/database-boundaries.md`.

use std::collections::BTreeMap;
use std::sync::Arc;

use diesel_async::pooled_connection::deadpool::Pool;

use crate::config::DatabaseConfig;
use crate::db::RuntimeConnection;
use crate::state::AppState;

/// A boundary that has its own database role.
///
/// `NAME` is the boundary name in `boundaries.toml` and the key under
/// `[database.roles]`.
pub trait DbBoundary {
    /// The boundary name.
    const NAME: &'static str;
}

/// The pools of the boundary roles, by boundary name. An [`AppState`]
/// extension.
#[derive(Clone, Default)]
pub struct BoundaryPools(Arc<BTreeMap<String, Pool<RuntimeConnection>>>);

impl BoundaryPools {
    /// Build one pool for each `[database.roles.<name>]`. A pool connects on
    /// first use, not here.
    ///
    /// # Errors
    ///
    /// Returns the reason when a role URL does not resolve or a pool cannot
    /// be built. The reason has no credentials.
    pub fn from_config(config: &DatabaseConfig) -> Result<Self, String> {
        let mut pools = BTreeMap::new();
        for name in config.roles.keys() {
            let url = config
                .role_url(name)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("database.roles.{name}: no connection URL"))?;
            let pool = crate::db::build_pool(
                &url,
                config.role_pool_size(name),
                config.connect_timeout_secs,
            )
            .map_err(|e| {
                format!(
                    "database.roles.{name}: {}",
                    crate::db_url::redact_driver_error(&e.to_string(), &url)
                )
            })?;
            pools.insert(name.clone(), pool);
        }
        Ok(Self(Arc::new(pools)))
    }

    /// The pool of the boundary `name`, when it has a role.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Pool<RuntimeConnection>> {
        self.0.get(name)
    }

    /// The boundary names that have a role.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.0.keys().map(String::as_str)
    }

    /// True when no boundary has a role.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl AppState {
    /// Add the pools of the boundary roles. See [`BoundaryPools`].
    #[must_use]
    pub fn with_boundary_pools(self, pools: BoundaryPools) -> Self {
        self.insert_extension(pools);
        self
    }

    /// The pool of the boundary `B` (#3550).
    ///
    /// With `[database.roles.<B::NAME>]`, this is the pool of the boundary's
    /// role. Without it, this is the primary pool, as before. `None` when the
    /// app has no database.
    #[must_use]
    pub fn pool_for<B: DbBoundary>(&self) -> Option<Pool<RuntimeConnection>> {
        self.boundary_pool(B::NAME)
    }

    /// The pool of the boundary `name`. See [`Self::pool_for`].
    #[must_use]
    pub fn boundary_pool(&self, name: &str) -> Option<Pool<RuntimeConnection>> {
        self.extension::<BoundaryPools>()
            .and_then(|pools| pools.get(name).cloned())
            .or_else(|| self.pool().cloned())
    }

    /// True when the boundary `name` has its own role pool.
    #[must_use]
    pub fn has_boundary_role(&self, name: &str) -> bool {
        self.extension::<BoundaryPools>()
            .is_some_and(|pools| pools.get(name).is_some())
    }
}

#[cfg(not(feature = "sqlite"))]
pub use postgres::{
    ReadProbe, RoleConnection, RoleReport, check_role, connect, load_catalog, probe_read,
};

/// The catalog reader and the negative check. Postgres only.
#[cfg(not(feature = "sqlite"))]
mod postgres {
    use std::fmt::Write as _;

    use diesel::QueryableByName;
    use diesel::sql_types::Text;
    use diesel_async::{AsyncPgConnection, RunQueryDsl, SimpleAsyncConnection as _};
    use serde::Serialize;

    use crate::graph::grants::{Catalog, OwnedSequence, QualifiedName, RolePlan};

    /// A connection made by [`connect`].
    pub type RoleConnection = diesel_async::pooled_connection::deadpool::Object<AsyncPgConnection>;

    /// Connect to `url` for [`load_catalog`] or [`check_role`]. Honors
    /// `sslmode` as the app pool does.
    ///
    /// # Errors
    ///
    /// Returns the reason, with the credentials of `url` removed.
    pub async fn connect(url: &str, timeout_secs: u64) -> Result<RoleConnection, String> {
        let redact =
            |e: &dyn std::fmt::Display| crate::db_url::redact_driver_error(&e.to_string(), url);
        let pool = crate::db::build_pool(url, 1, timeout_secs).map_err(|e| redact(&e))?;
        pool.get().await.map_err(|e| redact(&e))
    }

    #[derive(QueryableByName)]
    struct TableRow {
        #[diesel(sql_type = Text)]
        schema: String,
        #[diesel(sql_type = Text)]
        name: String,
    }

    #[derive(QueryableByName)]
    struct SequenceRow {
        #[diesel(sql_type = Text)]
        seq_schema: String,
        #[diesel(sql_type = Text)]
        seq_name: String,
        #[diesel(sql_type = Text)]
        table_schema: String,
        #[diesel(sql_type = Text)]
        table_name: String,
    }

    #[derive(QueryableByName)]
    struct UserRow {
        #[diesel(sql_type = Text)]
        name: String,
    }

    /// Tables, views and partitioned tables outside the system schemas.
    const TABLES_SQL: &str = "SELECT n.nspname::text AS schema, c.relname::text AS name \
         FROM pg_catalog.pg_class c \
         JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
         WHERE c.relkind IN ('r', 'p', 'v', 'm', 'f') \
           AND n.nspname <> 'information_schema' \
           AND n.nspname NOT LIKE 'pg\\_%' \
         ORDER BY 1, 2";

    /// Sequences that a table column owns (`serial` and identity).
    const SEQUENCES_SQL: &str = "SELECT sn.nspname::text AS seq_schema, \
           s.relname::text AS seq_name, \
           tn.nspname::text AS table_schema, t.relname::text AS table_name \
         FROM pg_catalog.pg_class s \
         JOIN pg_catalog.pg_namespace sn ON sn.oid = s.relnamespace \
         JOIN pg_catalog.pg_depend d ON d.objid = s.oid \
           AND d.classid = 'pg_catalog.pg_class'::regclass \
           AND d.refclassid = 'pg_catalog.pg_class'::regclass \
           AND d.deptype IN ('a', 'i') \
         JOIN pg_catalog.pg_class t ON t.oid = d.refobjid \
         JOIN pg_catalog.pg_namespace tn ON tn.oid = t.relnamespace \
         WHERE s.relkind = 'S' \
           AND sn.nspname <> 'information_schema' \
           AND sn.nspname NOT LIKE 'pg\\_%' \
         ORDER BY 1, 2";

    /// Read the tables and owned sequences of the database.
    ///
    /// Connect as a role that can see the catalog, for example the migration
    /// role. `pg_catalog` is readable by every role, so the result does not
    /// depend on table grants.
    ///
    /// # Errors
    ///
    /// Returns the query error.
    pub async fn load_catalog(conn: &mut AsyncPgConnection) -> diesel::QueryResult<Catalog> {
        let tables: Vec<TableRow> = diesel::sql_query(TABLES_SQL).load(conn).await?;
        let sequences: Vec<SequenceRow> = diesel::sql_query(SEQUENCES_SQL).load(conn).await?;
        Ok(Catalog {
            tables: tables
                .into_iter()
                .map(|r| QualifiedName::new(r.schema, r.name))
                .collect(),
            sequences: sequences
                .into_iter()
                .map(|r| OwnedSequence {
                    sequence: QualifiedName::new(r.seq_schema, r.seq_name),
                    table: QualifiedName::new(r.table_schema, r.table_name),
                })
                .collect(),
        })
    }

    /// The result of one read attempt.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize)]
    #[serde(rename_all = "snake_case", tag = "result", content = "detail")]
    pub enum ReadProbe {
        /// The read worked.
        Allowed,
        /// Postgres refused it: `insufficient_privilege` (SQLSTATE 42501).
        Denied,
        /// Another error, for example a missing table.
        Error(String),
    }

    /// The marker the probe raises on `insufficient_privilege`. It does not
    /// depend on `lc_messages`.
    pub(super) const DENIED_MARKER: &str = "autumn-boundary-probe: denied";

    /// Puts the probed table in two settings of the probe transaction. Bind
    /// parameters keep one statement text for every table.
    const PROBE_TARGET_SQL: &str = "SELECT set_config('autumn.probe_schema', $1, true), \
         set_config('autumn.probe_table', $2, true)";

    /// Reads the table that [`PROBE_TARGET_SQL`] names. `%I` quotes each name.
    pub(super) const PROBE_SQL: &str = "DO $autumn_probe$ BEGIN \
         EXECUTE format('SELECT 1 FROM %I.%I LIMIT 0', \
           current_setting('autumn.probe_schema'), current_setting('autumn.probe_table')); \
         EXCEPTION WHEN insufficient_privilege THEN \
           RAISE EXCEPTION 'autumn-boundary-probe: denied'; \
         END $autumn_probe$";

    /// Try to read `table` as the connected role.
    ///
    /// The read is `SELECT 1 FROM <table> LIMIT 0` in a read-only transaction
    /// that is rolled back. It reads no row and writes nothing. It waits at
    /// most 5 seconds for a lock.
    pub async fn probe_read(conn: &mut AsyncPgConnection, table: &QualifiedName) -> ReadProbe {
        use diesel::sql_types::Text;

        if let Err(e) = conn
            .batch_execute("BEGIN READ ONLY; SET LOCAL lock_timeout = '5s'")
            .await
        {
            return ReadProbe::Error(e.to_string());
        }
        let target = diesel::sql_query(PROBE_TARGET_SQL)
            .bind::<Text, _>(&table.schema)
            .bind::<Text, _>(&table.name)
            .execute(conn)
            .await;
        let outcome = match target {
            Ok(_) => conn.batch_execute(PROBE_SQL).await,
            Err(e) => Err(e),
        };
        let _ = conn.batch_execute("ROLLBACK").await;
        match outcome {
            Ok(()) => ReadProbe::Allowed,
            Err(e) if e.to_string().contains(DENIED_MARKER) => ReadProbe::Denied,
            Err(e) => ReadProbe::Error(e.to_string()),
        }
    }

    /// The result of [`check_role`] for one boundary role.
    #[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
    pub struct RoleReport {
        /// The boundary name.
        pub boundary: String,
        /// The role in `boundaries.toml`.
        pub role: String,
        /// The role the connection logged in as.
        pub connected_as: String,
        /// The number of granted tables that the role could read.
        pub allowed: usize,
        /// The number of other tables that Postgres refused, as it must.
        pub denied: usize,
        /// Tables outside the grant that the role could read.
        pub leaks: Vec<QualifiedName>,
        /// Tables inside the grant that Postgres refused.
        pub blocked: Vec<QualifiedName>,
        /// Tables the probe could not check, with the error.
        pub errors: Vec<(QualifiedName, String)>,
    }

    impl RoleReport {
        /// True when every probe gave the expected result and the
        /// connection used the expected role.
        #[must_use]
        pub fn passed(&self) -> bool {
            self.leaks.is_empty()
                && self.blocked.is_empty()
                && self.errors.is_empty()
                && self.connected_as == self.role
        }

        /// One line for a report.
        #[must_use]
        pub fn summary(&self) -> String {
            let mut line = format!(
                "role `{}` (boundary `{}`): reads {} granted table(s), denied {} other table(s)",
                self.role, self.boundary, self.allowed, self.denied
            );
            if self.connected_as != self.role {
                let _ = write!(
                    line,
                    "; connected as `{}`, not `{}`",
                    self.connected_as, self.role
                );
            }
            let list = |tables: &[QualifiedName]| {
                tables
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            if !self.leaks.is_empty() {
                let _ = write!(line, "; CAN READ {}", list(&self.leaks));
            }
            if !self.blocked.is_empty() {
                let _ = write!(line, "; cannot read granted {}", list(&self.blocked));
            }
            for (table, error) in &self.errors {
                let _ = write!(line, "; {table}: {error}");
            }
            line
        }
    }

    /// Run the negative check for one role on a connection made as that
    /// role.
    ///
    /// Each table the plan grants must be readable. Each table in
    /// [`RolePlan::denied`] must be refused.
    ///
    /// # Errors
    ///
    /// Returns the error when `current_user` cannot be read.
    pub async fn check_role(
        conn: &mut AsyncPgConnection,
        plan: &RolePlan,
    ) -> diesel::QueryResult<RoleReport> {
        let user: UserRow = diesel::sql_query("SELECT current_user::text AS name")
            .get_result(conn)
            .await?;
        let mut report = RoleReport {
            boundary: plan.boundary.clone(),
            role: plan.role.clone(),
            connected_as: user.name,
            ..RoleReport::default()
        };
        for table in plan.allowed() {
            match probe_read(conn, &table).await {
                ReadProbe::Allowed => report.allowed += 1,
                ReadProbe::Denied => report.blocked.push(table),
                ReadProbe::Error(e) => report.errors.push((table, e)),
            }
        }
        for table in &plan.denied {
            match probe_read(conn, table).await {
                ReadProbe::Denied => report.denied += 1,
                ReadProbe::Allowed => report.leaks.push(table.clone()),
                ReadProbe::Error(e) => report.errors.push((table.clone(), e)),
            }
        }
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Marketing;
    impl DbBoundary for Marketing {
        const NAME: &'static str = "marketing";
    }

    #[cfg(not(feature = "sqlite"))]
    struct Core;
    #[cfg(not(feature = "sqlite"))]
    impl DbBoundary for Core {
        const NAME: &'static str = "core";
    }

    fn config(text: &str) -> DatabaseConfig {
        let config: crate::config::AutumnConfig = toml::from_str(text).expect("config parses");
        config.database
    }

    #[test]
    fn no_roles_means_no_pools() {
        let pools = BoundaryPools::from_config(&DatabaseConfig::default()).expect("pools");
        assert!(pools.is_empty());
    }

    #[cfg(not(feature = "sqlite"))]
    #[test]
    fn a_pool_per_role() {
        let config = config(
            "[database]\nurl = \"postgres://app@127.0.0.1:1/crm\"\npool_size = 9\n\
             [database.roles.marketing]\nuser = \"marketing\"\npool_size = 2\n",
        );
        let pools = BoundaryPools::from_config(&config).expect("pools");
        assert_eq!(pools.names().collect::<Vec<_>>(), vec!["marketing"]);
        assert_eq!(pools.get("marketing").expect("pool").status().max_size, 2);
    }

    #[cfg(not(feature = "sqlite"))]
    #[test]
    fn pool_for_uses_the_role_pool_else_the_primary() {
        let config = config(
            "[database]\nurl = \"postgres://app@127.0.0.1:1/crm\"\npool_size = 9\n\
             [database.roles.marketing]\nuser = \"marketing\"\npool_size = 2\n",
        );
        let primary = crate::db::create_pool(&config).expect("pool").expect("url");
        let state = AppState::for_test()
            .with_pool(primary)
            .with_boundary_pools(BoundaryPools::from_config(&config).expect("pools"));
        assert_eq!(
            state
                .pool_for::<Marketing>()
                .expect("pool")
                .status()
                .max_size,
            2
        );
        assert_eq!(state.pool_for::<Core>().expect("pool").status().max_size, 9);
        assert!(state.has_boundary_role("marketing"));
        assert!(!state.has_boundary_role("core"));
    }

    #[test]
    fn no_database_means_no_pool() {
        let state = AppState::for_test();
        assert!(state.pool_for::<Marketing>().is_none());
        assert!(!state.has_boundary_role("marketing"));
    }

    #[test]
    fn a_role_without_a_primary_url_fails_without_credentials() {
        let config = config("[database.roles.m]\nuser = \"m\"\npassword = \"hunter2\"\n");
        let Err(err) = BoundaryPools::from_config(&config) else {
            panic!("no primary URL must fail");
        };
        assert!(err.contains("database.roles.m"), "{err}");
        assert!(!err.contains("hunter2"), "{err}");
    }

    #[cfg(not(feature = "sqlite"))]
    #[test]
    fn the_probe_raises_the_marker_it_matches() {
        assert!(postgres::PROBE_SQL.contains(postgres::DENIED_MARKER));
        assert!(postgres::PROBE_SQL.contains("%I.%I"));
    }

    #[cfg(not(feature = "sqlite"))]
    #[test]
    fn report_passes_only_when_every_probe_matches() {
        use crate::graph::grants::QualifiedName;
        let mut report = RoleReport {
            boundary: "marketing".into(),
            role: "marketing".into(),
            connected_as: "marketing".into(),
            allowed: 2,
            denied: 3,
            ..RoleReport::default()
        };
        assert!(report.passed());
        assert!(
            report
                .summary()
                .contains("reads 2 granted table(s), denied 3")
        );
        report.leaks.push(QualifiedName::new("core", "contacts"));
        assert!(!report.passed());
        assert!(report.summary().contains("CAN READ core.contacts"));
        report.leaks.clear();
        report.connected_as = "postgres".into();
        assert!(!report.passed());
        assert!(report.summary().contains("connected as `postgres`"));
    }
}
