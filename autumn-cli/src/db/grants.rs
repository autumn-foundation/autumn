//! `autumn db grants`: database grants from `boundaries.toml` (#3550).
//!
//! A boundary with a `role` gets access to its own tables and its
//! `may_read` / `may_write` exceptions only. This module reads the live
//! catalog, makes the plan with `autumn_web::graph::grants`, and prints the
//! SQL or applies it. `autumn doctor` uses `probe_role` to check that each
//! role really cannot read a table outside its grant.
//!
//! See `docs/guide/database-boundaries.md`.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use autumn_web::config::Env;
use autumn_web::db_roles::DatabaseRoleConfig;
use autumn_web::graph::boundaries::BoundaryConfig;
use autumn_web::graph::grants::{self, CatalogTable, GrantPlan, QualifiedName, RoleGrants};

use crate::pg::{self, CommandError};

/// How long one connection may take to open.
pub const CONNECT_BUDGET: Duration = Duration::from_secs(10);

/// The SQLSTATE of `permission denied`.
const INSUFFICIENT_PRIVILEGE: &str = "42501";

/// The tables of the catalog, with the sequences they own. System schemas
/// and the members of an extension are left out: no boundary owns them.
const TABLES_SQL: &str = "\
SELECT n.nspname::text, c.relname::text, c.relkind IN ('v', 'm')
FROM pg_class c
JOIN pg_namespace n ON n.oid = c.relnamespace
WHERE c.relkind IN ('r', 'p', 'v', 'm', 'f')
  AND n.nspname NOT IN ('pg_catalog', 'information_schema')
  AND n.nspname NOT LIKE 'pg\\_toast%'
  AND n.nspname NOT LIKE 'pg\\_temp\\_%'
  AND NOT EXISTS (
    SELECT 1 FROM pg_depend d
    WHERE d.classid = 'pg_class'::regclass AND d.objid = c.oid AND d.deptype = 'e'
  )
ORDER BY 1, 2";

/// The sequences that a table uses: owned ones (`serial` and identity
/// columns), and the ones a column default calls with `nextval`.
const SEQUENCES_SQL: &str = "\
SELECT tn.nspname::text, t.relname::text, sn.nspname::text, s.relname::text
FROM pg_depend d
JOIN pg_class s ON s.oid = d.objid AND s.relkind = 'S'
JOIN pg_namespace sn ON sn.oid = s.relnamespace
JOIN pg_class t ON t.oid = d.refobjid
JOIN pg_namespace tn ON tn.oid = t.relnamespace
WHERE d.classid = 'pg_class'::regclass
  AND d.refclassid = 'pg_class'::regclass
  AND d.deptype IN ('a', 'i')
UNION
SELECT tn.nspname::text, t.relname::text, sn.nspname::text, s.relname::text
FROM pg_attrdef ad
JOIN pg_depend d ON d.classid = 'pg_attrdef'::regclass AND d.objid = ad.oid
JOIN pg_class s ON s.oid = d.refobjid AND s.relkind = 'S'
JOIN pg_namespace sn ON sn.oid = s.relnamespace
JOIN pg_class t ON t.oid = ad.adrelid
JOIN pg_namespace tn ON tn.oid = t.relnamespace
WHERE d.refclassid = 'pg_class'::regclass
ORDER BY 1, 2, 3, 4";

/// The arguments of `autumn db grants`.
#[derive(Debug, Clone)]
pub struct GrantsArgs {
    /// The profile that selects the database URL.
    pub profile: Option<String>,
    /// The boundary config.
    pub config: PathBuf,
    /// Run the SQL, not only print it.
    pub apply: bool,
    /// Print the plan as JSON.
    pub json: bool,
}

/// Run `autumn db grants`. Exits 1 on an error.
pub fn run(args: &GrantsArgs) {
    if let Err(message) = run_inner(args) {
        eprintln!("\u{2717} autumn db grants: {message}");
        std::process::exit(1);
    }
}

fn run_inner(args: &GrantsArgs) -> Result<(), String> {
    let mut config = crate::boundaries::load_config(&args.config)?;
    if !config.boundaries.values().any(|b| b.role.is_some()) {
        eprintln!(
            "No boundary in {} declares a `role`.",
            args.config.display()
        );
        return Ok(());
    }
    add_plugins(&mut config, &args.config);
    let url = crate::migrate::resolve_primary_url(args.profile.as_deref())
        .ok_or("no database URL; set database.url or AUTUMN_DATABASE__URL")?;
    let plan = pg::block_on(async {
        let client = connect(&url).await?;
        let catalog = read_catalog(&client).await?;
        let plan = grants::plan(&config, &catalog).map_err(|e| e.to_string())?;
        if args.apply {
            let roles = role_facts(&client, &plan).await?;
            if let Some(message) = refuse_roles(&plan, &roles) {
                return Err(message);
            }
            if let Some(sql) = plan.sql() {
                client
                    .batch_execute(&sql)
                    .await
                    .map_err(|e| format!("the grants failed: {}", db_error(&e)))?;
            }
        }
        Ok::<_, String>(plan)
    })?;
    for warning in &plan.warnings {
        eprintln!("\u{26a0} {warning}");
    }
    if args.json {
        let json = serde_json::to_string_pretty(&plan).map_err(|e| e.to_string())?;
        println!("{json}");
    } else if !args.apply
        && let Some(sql) = plan.sql()
    {
        print!("{sql}");
    }
    if args.apply {
        eprintln!(
            "\u{2713} Applied the grants of {} role(s): {}",
            plan.roles.len(),
            role_list(&plan)
        );
    }
    Ok(())
}

/// Add the tables that plugin crates declare, from `cargo metadata`. When
/// Cargo cannot run, warn and go on: a plugin table then has no owner, so
/// no role gets it.
fn add_plugins(config: &mut BoundaryConfig, config_path: &std::path::Path) {
    let dir = config_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."));
    match crate::boundaries::cargo_facts(dir, &[], true, Some(crate::boundaries::DOCTOR_BUDGET)) {
        Ok(facts) => {
            for (krate, owns) in &facts.plugins {
                config.add_plugin(krate, owns);
            }
        }
        Err(e) => eprintln!("\u{26a0} plugin tables not read: {e}"),
    }
}

fn role_list(plan: &GrantPlan) -> String {
    plan.roles
        .iter()
        .map(|r| r.role.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Connect, with [`CONNECT_BUDGET`].
async fn connect(url: &str) -> Result<tokio_postgres::Client, String> {
    match tokio::time::timeout(CONNECT_BUDGET, pg::connect("db grants", url)).await {
        Err(_) => Err("the database did not answer in time".to_owned()),
        Ok(Err(CommandError::Connect(m) | CommandError::Other(m))) => Err(m),
        Ok(Ok(client)) => Ok(client),
    }
}

/// Read every table and its owned sequences.
///
/// # Errors
///
/// Returns the database error.
pub async fn read_catalog(client: &tokio_postgres::Client) -> Result<Vec<CatalogTable>, String> {
    let rows = client
        .query(TABLES_SQL, &[])
        .await
        .map_err(|e| format!("cannot read the catalog: {}", db_error(&e)))?;
    let mut tables: BTreeMap<QualifiedName, CatalogTable> = rows
        .iter()
        .map(|row| {
            let table = QualifiedName::new(row.get::<_, String>(0), row.get::<_, String>(1));
            (
                table.clone(),
                CatalogTable {
                    table,
                    sequences: Vec::new(),
                    view: row.get::<_, bool>(2),
                },
            )
        })
        .collect();
    let rows = client
        .query(SEQUENCES_SQL, &[])
        .await
        .map_err(|e| format!("cannot read the sequences: {}", db_error(&e)))?;
    for row in rows {
        let table = QualifiedName::new(row.get::<_, String>(0), row.get::<_, String>(1));
        if let Some(entry) = tables.get_mut(&table) {
            entry.sequences.push(QualifiedName::new(
                row.get::<_, String>(2),
                row.get::<_, String>(3),
            ));
        }
    }
    Ok(tables.into_values().collect())
}

/// What the database knows about a role of the plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleFacts {
    /// The role name.
    pub name: String,
    /// A superuser passes every privilege check.
    pub superuser: bool,
    /// The role is the one that runs this command.
    pub current: bool,
    /// The role owns a table, so it is the owner of a schema, not a boundary.
    pub owns_tables: bool,
}

async fn role_facts(
    client: &tokio_postgres::Client,
    plan: &GrantPlan,
) -> Result<Vec<RoleFacts>, String> {
    let names: Vec<String> = plan.roles.iter().map(|r| r.role.clone()).collect();
    let rows = client
        .query(
            "SELECT r.rolname::text, r.rolsuper, r.rolname = current_user, \
             EXISTS (SELECT 1 FROM pg_class c WHERE c.relowner = r.oid \
             AND c.relkind IN ('r', 'p', 'v', 'm', 'f')) \
             FROM pg_roles r WHERE r.rolname = ANY($1)",
            &[&names],
        )
        .await
        .map_err(|e| format!("cannot read the roles: {}", db_error(&e)))?;
    Ok(rows
        .iter()
        .map(|r| RoleFacts {
            name: r.get(0),
            superuser: r.get(1),
            current: r.get(2),
            owns_tables: r.get(3),
        })
        .collect())
}

/// Why `--apply` must not run, or `None`.
///
/// A role that does not exist cannot get a grant. A superuser, the role that
/// runs the command, or a role that owns tables is not a boundary role: the
/// `REVOKE` would take the rights of the owner, or have no effect.
#[must_use]
pub fn refuse_roles(plan: &GrantPlan, facts: &[RoleFacts]) -> Option<String> {
    let missing: Vec<String> = plan
        .roles
        .iter()
        .filter(|r| !facts.iter().any(|f| f.name == r.role))
        .map(|r| r.role.clone())
        .collect();
    if !missing.is_empty() {
        return Some(missing_roles_message(&missing));
    }
    let unsafe_roles: Vec<String> = facts
        .iter()
        .filter_map(|f| {
            let why = if f.superuser {
                "a superuser"
            } else if f.current {
                "the role that runs this command"
            } else if f.owns_tables {
                "the owner of tables"
            } else {
                return None;
            };
            Some(format!("`{}` is {why}", f.name))
        })
        .collect();
    (!unsafe_roles.is_empty()).then(|| {
        format!(
            "a boundary role must be a role of its own: {}. Make a new role for the boundary",
            unsafe_roles.join("; ")
        )
    })
}

/// The error for roles that the database does not have.
#[must_use]
pub fn missing_roles_message(missing: &[String]) -> String {
    let create = missing
        .iter()
        .map(|r| {
            format!(
                "CREATE ROLE {} LOGIN PASSWORD '...';",
                grants::quote_ident(r)
            )
        })
        .collect::<Vec<_>>()
        .join("\n  ");
    format!(
        "the database has no role {}. Make it first:\n  {create}",
        missing.join(", ")
    )
}

/// The message of a database error, with its SQLSTATE.
fn db_error(e: &tokio_postgres::Error) -> String {
    e.as_db_error().map_or_else(
        || e.to_string(),
        |db| format!("{} ({})", db.message(), db.code().code()),
    )
}

// ── Role connections ─────────────────────────────────────────────────

/// The URL of each `[database.roles.<boundary>]`, or why it has none.
///
/// `table` is the merged `autumn.toml` of the profile. The env overrides
/// of the runtime apply.
///
/// # Errors
///
/// Returns a message when `[database.roles]` is not valid TOML for a role.
pub fn role_urls(
    table: Option<&toml::Table>,
    env: &dyn Env,
    primary: Option<&str>,
) -> Result<BTreeMap<String, Result<String, String>>, String> {
    let mut roles: BTreeMap<String, DatabaseRoleConfig> = match table
        .and_then(|t| t.get("database"))
        .and_then(|d| d.get("roles"))
    {
        Some(value) => value
            .clone()
            .try_into()
            .map_err(|e| format!("[database.roles] is not valid: {e}"))?,
        None => BTreeMap::new(),
    };
    autumn_web::db_roles::apply_env_overrides(&mut roles, env);
    Ok(roles
        .into_iter()
        .map(|(name, role)| (name, role.effective_url(primary)))
        .collect())
}

/// What one table access gave.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Access {
    /// The role can read the table.
    Allowed,
    /// The role cannot read the table, but it has another privilege on it,
    /// for example `INSERT`.
    Writable,
    /// The database refused the read, and the role has no privilege on it.
    Denied,
    /// Another error, with its message.
    Error(String),
}

/// What [`probe_role`] found for one role.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleProbe {
    /// The boundary.
    pub boundary: String,
    /// The role that `boundaries.toml` declares.
    pub role: String,
    /// The role of the connection, or why there is no connection.
    pub connected_as: Result<String, String>,
    /// Each table the role may read, and what a read gave.
    pub allowed: Vec<(QualifiedName, Access)>,
    /// Each table outside the grant, and what a check gave.
    pub denied: Vec<(QualifiedName, Access)>,
    /// The `may_read` tables that the role can also write.
    pub writes_read_only: Vec<QualifiedName>,
    /// The schemas in which the role can create objects.
    pub creates_in: Vec<String>,
    /// Other roles that this role is a member of.
    pub member_of: Vec<String>,
}

impl Default for RoleProbe {
    fn default() -> Self {
        Self {
            boundary: String::new(),
            role: String::new(),
            connected_as: Err("not connected".to_owned()),
            allowed: Vec::new(),
            denied: Vec::new(),
            writes_read_only: Vec::new(),
            creates_in: Vec::new(),
            member_of: Vec::new(),
        }
    }
}

impl RoleProbe {
    /// A probe that could not connect.
    #[must_use]
    pub fn unreachable(grants: &RoleGrants, why: String) -> Self {
        Self {
            boundary: grants.boundary.clone(),
            role: grants.role.clone(),
            connected_as: Err(why),
            ..Self::default()
        }
    }

    /// The tables outside the grant that the role can read or change.
    #[must_use]
    pub fn leaks(&self) -> Vec<&QualifiedName> {
        self.denied
            .iter()
            .filter(|(_, a)| matches!(a, Access::Allowed | Access::Writable))
            .map(|(t, _)| t)
            .collect()
    }
}

/// The time limits of the check connection. A table lock of a running
/// migration must not stop doctor.
const PROBE_LIMITS_SQL: &str = "SET statement_timeout = '5s'; SET lock_timeout = '2s'";

/// The privileges that a table outside the grant must not have.
const ANY_TABLE_PRIVILEGE: &str = "SELECT, INSERT, UPDATE, DELETE, TRUNCATE, REFERENCES, TRIGGER";

/// The privileges that a `may_read` table must not have.
const WRITE_PRIVILEGE: &str = "INSERT, UPDATE, DELETE, TRUNCATE";

/// The column privileges that a table outside the grant must not have.
const ANY_COLUMN_PRIVILEGE: &str = "SELECT, INSERT, UPDATE, REFERENCES";

/// The column privileges that a `may_read` table must not have.
const WRITE_COLUMN_PRIVILEGE: &str = "INSERT, UPDATE";

/// Connect as one role and check each table: its own tables and exceptions
/// must open, and every other table must give `permission denied`.
///
/// A read is `SELECT 1 FROM <table> LIMIT 0`. It reads no row, but Postgres
/// checks the privilege first. `has_table_privilege` then finds a write
/// privilege outside the grant, and on a `may_read` table. It counts grants
/// to `PUBLIC` and through role membership. The probe also lists the
/// `schemas` in which the role can create objects, and the roles it is a
/// member of.
pub async fn probe_role(url: &str, grants: &RoleGrants, schemas: &[String]) -> RoleProbe {
    let client = match connect(url).await {
        Ok(client) => client,
        Err(why) => return RoleProbe::unreachable(grants, why),
    };
    let connected_as = match client.batch_execute(PROBE_LIMITS_SQL).await {
        Err(e) => Err(db_error(&e)),
        Ok(()) => client
            .query_one("SELECT current_user::text", &[])
            .await
            .map(|row| row.get::<_, String>(0))
            .map_err(|e| db_error(&e)),
    };
    let mut probe = RoleProbe {
        boundary: grants.boundary.clone(),
        role: grants.role.clone(),
        connected_as,
        ..RoleProbe::default()
    };
    for table in grants.own.iter().chain(&grants.read).chain(&grants.write) {
        probe
            .allowed
            .push((table.clone(), read(&client, table).await));
    }
    for table in &grants.read {
        if has_privilege(&client, table, WRITE_PRIVILEGE, WRITE_COLUMN_PRIVILEGE).await
            == Some(true)
        {
            probe.writes_read_only.push(table.clone());
        }
    }
    for table in &grants.denied {
        let access = match read(&client, table).await {
            Access::Denied
                if has_privilege(&client, table, ANY_TABLE_PRIVILEGE, ANY_COLUMN_PRIVILEGE)
                    .await
                    == Some(true) =>
            {
                Access::Writable
            }
            other => other,
        };
        probe.denied.push((table.clone(), access));
    }
    for schema in schemas {
        let creates = client
            .query_one(
                "SELECT has_schema_privilege(current_user, $1, 'CREATE')",
                &[schema],
            )
            .await
            .is_ok_and(|row| row.get::<_, bool>(0));
        if creates {
            probe.creates_in.push(schema.clone());
        }
    }
    if let Ok(rows) = client
        .query(
            "SELECT r.rolname::text FROM pg_roles r \
             WHERE r.rolname <> current_user \
             AND pg_has_role(current_user, r.oid, 'MEMBER') ORDER BY 1",
            &[],
        )
        .await
    {
        probe.member_of = rows.iter().map(|r| r.get(0)).collect();
    }
    probe
}

async fn read(client: &tokio_postgres::Client, table: &QualifiedName) -> Access {
    let sql = format!("SELECT 1 FROM {} LIMIT 0", table.quoted());
    match client.batch_execute(&sql).await {
        Ok(()) => Access::Allowed,
        Err(e)
            if e.code().map(tokio_postgres::error::SqlState::code)
                == Some(INSUFFICIENT_PRIVILEGE) =>
        {
            Access::Denied
        }
        Err(e) => Access::Error(db_error(&e)),
    }
}

/// Whether the role has any of the table `privileges` on `table`, or any of
/// the `column` privileges on one of its columns. `None` on an error.
async fn has_privilege(
    client: &tokio_postgres::Client,
    table: &QualifiedName,
    privileges: &str,
    column: &str,
) -> Option<bool> {
    // A column grant does not show in `has_table_privilege`.
    client
        .query_one(
            "SELECT has_table_privilege(current_user, $1::text::regclass, $2) \
             OR has_any_column_privilege(current_user, $1::text::regclass, $3)",
            &[&table.quoted(), &privileges, &column],
        )
        .await
        .ok()
        .map(|row| row.get(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NoEnv;

    impl Env for NoEnv {
        fn var(&self, _: &str) -> Result<String, std::env::VarError> {
            Err(std::env::VarError::NotPresent)
        }
    }

    #[test]
    fn role_urls_read_the_roles_table() {
        let table: toml::Table = toml::from_str(
            r#"
[database.roles.marketing]
user = "marketing"
password = "pw"

[database.roles.billing]
url = "postgres://billing@h/app"

[database.roles.broken]
"#,
        )
        .unwrap();
        let urls = role_urls(Some(&table), &NoEnv, Some("postgres://o:opw@h:5432/app")).unwrap();
        assert_eq!(
            urls["marketing"].as_deref(),
            Ok("postgres://marketing:pw@h:5432/app")
        );
        assert_eq!(urls["billing"].as_deref(), Ok("postgres://billing@h/app"));
        assert!(urls["broken"].is_err());
    }

    #[test]
    fn role_urls_with_no_table_are_empty() {
        assert!(role_urls(None, &NoEnv, None).unwrap().is_empty());
        let table: toml::Table = toml::from_str("[database]\nurl = \"postgres://h/a\"").unwrap();
        assert!(role_urls(Some(&table), &NoEnv, None).unwrap().is_empty());
    }

    #[test]
    fn a_bad_roles_table_is_an_error() {
        let table: toml::Table = toml::from_str("[database.roles.m]\nuser = 7\n").unwrap();
        let err = role_urls(Some(&table), &NoEnv, None).unwrap_err();
        assert!(err.contains("[database.roles]"), "{err}");
    }

    fn facts(name: &str) -> RoleFacts {
        RoleFacts {
            name: name.into(),
            superuser: false,
            current: false,
            owns_tables: false,
        }
    }

    fn plan_for(roles: &[&str]) -> GrantPlan {
        GrantPlan {
            roles: roles
                .iter()
                .map(|r| RoleGrants {
                    boundary: (*r).into(),
                    role: (*r).into(),
                    ..RoleGrants::default()
                })
                .collect(),
            ..GrantPlan::default()
        }
    }

    #[test]
    fn apply_refuses_missing_and_unsafe_roles() {
        let plan = plan_for(&["core", "marketing"]);
        assert_eq!(
            refuse_roles(&plan, &[facts("core"), facts("marketing")]),
            None
        );
        let missing = refuse_roles(&plan, &[facts("core")]).unwrap();
        assert!(missing.contains("no role marketing"), "{missing}");
        for (field, why) in [
            ("superuser", "a superuser"),
            ("current", "the role that runs this command"),
            ("owns_tables", "the owner of tables"),
        ] {
            let mut f = facts("marketing");
            match field {
                "superuser" => f.superuser = true,
                "current" => f.current = true,
                _ => f.owns_tables = true,
            }
            let message = refuse_roles(&plan, &[facts("core"), f]).unwrap();
            assert!(
                message.contains(&format!("`marketing` is {why}")),
                "{message}"
            );
        }
    }

    #[test]
    fn missing_roles_message_shows_the_create_statement() {
        let message = missing_roles_message(&["marketing".to_owned()]);
        assert!(message.contains("no role marketing"), "{message}");
        assert!(
            message.contains(r#"CREATE ROLE "marketing" LOGIN"#),
            "{message}"
        );
    }

    #[test]
    fn leaks_are_the_readable_denied_tables() {
        let probe = RoleProbe {
            boundary: "m".into(),
            role: "m".into(),
            connected_as: Ok("m".into()),
            denied: vec![
                (QualifiedName::new("public", "a"), Access::Denied),
                (QualifiedName::new("public", "b"), Access::Allowed),
                (QualifiedName::new("public", "c"), Access::Error("x".into())),
                (QualifiedName::new("public", "d"), Access::Writable),
            ],
            ..RoleProbe::default()
        };
        assert_eq!(
            probe.leaks(),
            [
                &QualifiedName::new("public", "b"),
                &QualifiedName::new("public", "d")
            ]
        );
    }
}
