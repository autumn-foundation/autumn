//! `autumn db grants`: database grants from `boundaries.toml` (#3550).
//!
//! A boundary with a `role` gets access to its own tables and its
//! `may_read` / `may_write` exceptions only. This module reads the live
//! catalog, makes the plan with `autumn_web::graph::grants`, and prints the
//! SQL or applies it. `autumn doctor` uses [`probe_role`] to check that each
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
SELECT n.nspname::text, c.relname::text
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

/// The sequences that a table owns (`serial` and identity columns).
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
            let missing = missing_roles(&client, &plan).await?;
            if !missing.is_empty() {
                return Err(missing_roles_message(&missing));
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
    } else if let Some(sql) = plan.sql() {
        if !args.apply {
            print!("{sql}");
        }
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

/// The roles of `plan` that the database does not have.
async fn missing_roles(
    client: &tokio_postgres::Client,
    plan: &GrantPlan,
) -> Result<Vec<String>, String> {
    let names: Vec<String> = plan.roles.iter().map(|r| r.role.clone()).collect();
    let rows = client
        .query(
            "SELECT rolname::text FROM pg_roles WHERE rolname = ANY($1)",
            &[&names],
        )
        .await
        .map_err(|e| format!("cannot read the roles: {}", db_error(&e)))?;
    let present: Vec<String> = rows.iter().map(|r| r.get(0)).collect();
    Ok(names.into_iter().filter(|n| !present.contains(n)).collect())
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
    /// The `SELECT` ran.
    Allowed,
    /// The database refused it with `permission denied`.
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
    /// Each table outside the grant, and what a read gave.
    pub denied: Vec<(QualifiedName, Access)>,
}

impl RoleProbe {
    /// A probe that could not connect.
    #[must_use]
    pub fn unreachable(grants: &RoleGrants, why: String) -> Self {
        Self {
            boundary: grants.boundary.clone(),
            role: grants.role.clone(),
            connected_as: Err(why),
            allowed: Vec::new(),
            denied: Vec::new(),
        }
    }

    /// The tables outside the grant that the role could read.
    #[must_use]
    pub fn leaks(&self) -> Vec<&QualifiedName> {
        self.denied
            .iter()
            .filter(|(_, a)| *a == Access::Allowed)
            .map(|(t, _)| t)
            .collect()
    }
}

/// Connect as one role and try to read each table: its own tables and
/// exceptions must open, every other table must give `permission denied`.
///
/// Each read is `SELECT 1 FROM <table> LIMIT 0`. It reads no row, but
/// Postgres checks the privilege first.
pub async fn probe_role(url: &str, grants: &RoleGrants) -> RoleProbe {
    let client = match connect(url).await {
        Ok(client) => client,
        Err(why) => return RoleProbe::unreachable(grants, why),
    };
    let connected_as = client
        .query_one("SELECT current_user::text", &[])
        .await
        .map(|row| row.get::<_, String>(0))
        .map_err(|e| db_error(&e));
    let mut allowed = Vec::new();
    for table in grants.own.iter().chain(&grants.read).chain(&grants.write) {
        allowed.push((table.clone(), read(&client, table).await));
    }
    let mut denied = Vec::new();
    for table in &grants.denied {
        denied.push((table.clone(), read(&client, table).await));
    }
    RoleProbe {
        boundary: grants.boundary.clone(),
        role: grants.role.clone(),
        connected_as,
        allowed,
        denied,
    }
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
            allowed: Vec::new(),
            denied: vec![
                (QualifiedName::new("public", "a"), Access::Denied),
                (QualifiedName::new("public", "b"), Access::Allowed),
                (QualifiedName::new("public", "c"), Access::Error("x".into())),
            ],
        };
        assert_eq!(probe.leaks(), [&QualifiedName::new("public", "b")]);
    }
}
