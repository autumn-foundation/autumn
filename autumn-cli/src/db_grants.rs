//! `autumn db grants` and the doctor's `database_roles` check (#3550).
//!
//! A boundary with a `schema` in `boundaries.toml` gets a Postgres role. This
//! module reads the live catalog and makes the grant plan
//! (`autumn_web::graph::grants`). Then it prints the SQL, runs the SQL, or
//! connects as each role to make sure that the role cannot read the tables
//! of a different boundary.
//!
//! See `docs/guide/database-boundaries.md`.

use std::path::Path;

use autumn_web::config::{AutumnConfig, DatabaseConfig, Env};
use autumn_web::graph::boundaries::BoundaryConfig;
#[cfg(not(feature = "sqlite"))]
use autumn_web::graph::grants::{self, GrantPlan};

/// Parsed `autumn db grants` arguments.
#[derive(Debug, Clone)]
pub struct GrantsArgs {
    /// Resolve the connection through a profile overlay.
    pub profile: Option<String>,
    /// The `boundaries.toml` file.
    pub config: std::path::PathBuf,
    /// Run the SQL instead of printing it.
    pub apply: bool,
}

/// True when a boundary in `config` has a `schema`.
#[must_use]
pub fn has_database_boundaries(config: &BoundaryConfig) -> bool {
    config.boundaries.values().any(|b| b.schema.is_some())
}

/// The `[database]` config of the app: `autumn.toml` with its profile
/// overlay, then the environment. `primary` is the primary URL as
/// `autumn migrate` resolves it. When set, it replaces the URL of the config.
///
/// # Errors
///
/// Returns the reason when the TOML does not fit the config schema.
pub fn database_config(
    table: Option<toml::Table>,
    env: &dyn Env,
    primary: Option<String>,
) -> Result<DatabaseConfig, String> {
    let mut config: AutumnConfig = toml::Value::Table(table.unwrap_or_default())
        .try_into()
        .map_err(|e| format!("autumn.toml: {e}"))?;
    config.apply_env_overrides_with_env(env);
    if primary.is_some() {
        config.database.primary_url = primary;
    }
    config.database.validate().map_err(|e| e.to_string())?;
    Ok(config.database)
}

/// A doctor verdict.
#[cfg(not(feature = "sqlite"))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Every role holds.
    Pass(String),
    /// A role leaks or cannot be checked.
    Warn(String),
    /// The config is broken.
    Fail(String),
}

/// What the doctor learned, before grading.
#[cfg(not(feature = "sqlite"))]
#[derive(Debug, Clone, Default)]
pub struct RolesOutcome {
    /// The plan, or why it could not be made.
    pub plan: Option<Result<GrantPlan, String>>,
    /// Why the check could not run at all, for example no database URL.
    pub skipped: Option<String>,
    /// A report per role that has a `[database.roles]` entry.
    pub reports: Vec<autumn_web::db_boundary::RoleReport>,
    /// Boundaries with a schema and no `[database.roles]` entry.
    pub unconfigured: Vec<String>,
    /// Roles the doctor could not connect as, with the reason.
    pub unreachable: Vec<(String, String)>,
}

/// Grade the outcome of the role check.
#[cfg(not(feature = "sqlite"))]
#[must_use]
pub fn grade(outcome: &RolesOutcome) -> Verdict {
    if let Some(reason) = &outcome.skipped {
        return Verdict::Warn(format!("role check not run: {reason}"));
    }
    let plan = match &outcome.plan {
        Some(Ok(plan)) => plan,
        Some(Err(e)) => return Verdict::Fail(e.clone()),
        None => return Verdict::Warn("role check not run: no plan".to_owned()),
    };
    let mut problems: Vec<String> = outcome
        .reports
        .iter()
        .filter(|r| !r.passed())
        .map(autumn_web::db_boundary::RoleReport::summary)
        .collect();
    problems.extend(
        outcome
            .unreachable
            .iter()
            .map(|(boundary, e)| format!("cannot connect as the role of `{boundary}`: {e}")),
    );
    problems.extend(outcome.unconfigured.iter().map(|boundary| {
        format!(
            "boundary `{boundary}` has a schema but no [database.roles.{boundary}]; \
             the app uses the primary role for it"
        )
    }));
    if problems.is_empty() {
        let roles = outcome.reports.len();
        Verdict::Pass(if plan.warnings.is_empty() {
            format!("{roles} role(s) read only their grants")
        } else {
            format!(
                "{roles} role(s) read only their grants ({} plan warning(s); run `autumn db grants`)",
                plan.warnings.len()
            )
        })
    } else {
        Verdict::Warn(problems.join("\n"))
    }
}

/// Run the role check against the live database. Blocking.
#[cfg(not(feature = "sqlite"))]
#[must_use]
pub fn check_roles(boundaries: &BoundaryConfig, database: &DatabaseConfig) -> RolesOutcome {
    let Some(primary) = database.effective_primary_url().map(str::to_owned) else {
        return RolesOutcome {
            skipped: Some("no database URL".to_owned()),
            ..RolesOutcome::default()
        };
    };
    let timeout = database.connect_timeout_secs;
    crate::pg::block_on(async {
        let mut outcome = RolesOutcome::default();
        let plan = match load_plan(boundaries, &primary, timeout).await {
            Ok(plan) => plan,
            Err(e) => {
                outcome.plan = Some(Err(e));
                return outcome;
            }
        };
        for role in &plan.roles {
            let url = match database.role_url(&role.boundary) {
                Ok(Some(url)) => url,
                Ok(None) => {
                    outcome.unconfigured.push(role.boundary.clone());
                    continue;
                }
                Err(e) => {
                    outcome
                        .unreachable
                        .push((role.boundary.clone(), e.to_string()));
                    continue;
                }
            };
            let mut conn = match autumn_web::db_boundary::connect(&url, timeout).await {
                Ok(conn) => conn,
                Err(e) => {
                    outcome.unreachable.push((role.boundary.clone(), e));
                    continue;
                }
            };
            match autumn_web::db_boundary::check_role(&mut conn, role).await {
                Ok(report) => outcome.reports.push(report),
                Err(e) => outcome
                    .unreachable
                    .push((role.boundary.clone(), e.to_string())),
            }
        }
        outcome.plan = Some(Ok(plan));
        outcome
    })
}

/// Read the catalog as the primary role and make the plan.
#[cfg(not(feature = "sqlite"))]
async fn load_plan(
    boundaries: &BoundaryConfig,
    primary: &str,
    timeout: u64,
) -> Result<GrantPlan, String> {
    let mut conn = autumn_web::db_boundary::connect(primary, timeout).await?;
    let catalog = autumn_web::db_boundary::load_catalog(&mut conn)
        .await
        .map_err(|e| format!("cannot read the catalog: {e}"))?;
    grants::plan(boundaries, &catalog).map_err(|e| e.to_string())
}

/// `autumn db grants`.
pub fn run(args: &GrantsArgs) {
    if let Err(e) = run_inner(args) {
        eprintln!("\u{2717} {e}");
        std::process::exit(1);
    }
}

fn run_inner(args: &GrantsArgs) -> Result<(), String> {
    let boundaries = crate::boundaries::load_config(&args.config)?;
    if !has_database_boundaries(&boundaries) {
        return Err(format!(
            "no boundary in {} sets `schema`; there is nothing to grant",
            args.config.display()
        ));
    }
    let database = resolve_database(args.profile.as_deref())?;
    run_with(&boundaries, &database, args.apply, Path::new("."))
}

/// The `[database]` config for `profile`, resolved as `autumn migrate` does:
/// the profile overlay, the `.env` of that profile, then the environment.
///
/// # Errors
///
/// Returns the reason when `.env` or the config is not valid.
pub fn resolve_database(profile: Option<&str>) -> Result<DatabaseConfig, String> {
    let effective = crate::migrate::effective_profile(profile);
    let table = crate::migrate::read_autumn_toml_table_with_profile(Some(&effective));
    let env = autumn_web::dotenv::os_env_with_dotenv_for_profile(&effective)
        .map_err(|e| format!(".env: {e}"))?;
    database_config(table, &env, crate::migrate::resolve_primary_url(profile))
}

#[cfg(feature = "sqlite")]
fn run_with(_: &BoundaryConfig, _: &DatabaseConfig, _: bool, _: &Path) -> Result<(), String> {
    Err("`autumn db grants` needs Postgres; this CLI is a SQLite build".to_owned())
}

#[cfg(not(feature = "sqlite"))]
fn run_with(
    boundaries: &BoundaryConfig,
    database: &DatabaseConfig,
    apply: bool,
    _root: &Path,
) -> Result<(), String> {
    use autumn_web::reexports::diesel_async::SimpleAsyncConnection as _;

    let primary = database
        .effective_primary_url()
        .ok_or("no database URL: set database.url or DATABASE_URL")?
        .to_owned();
    let timeout = database.connect_timeout_secs;
    crate::pg::block_on(async {
        let plan = load_plan(boundaries, &primary, timeout).await?;
        for warning in &plan.warnings {
            eprintln!("\u{26A0} {warning}");
        }
        let sql = plan.to_sql();
        if !apply {
            print!("{sql}");
            return Ok(());
        }
        let mut conn = autumn_web::db_boundary::connect(&primary, timeout).await?;
        conn.batch_execute(&sql)
            .await
            .map_err(|e| format!("the grants did not apply, and nothing changed: {e}"))?;
        for role in &plan.roles {
            eprintln!(
                "\u{2713} role `{}`: schema `{}`, {} read, {} write exception(s)",
                role.role,
                role.schema,
                role.read.len(),
                role.write.len()
            );
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use autumn_web::config::MockEnv;

    fn table(text: &str) -> toml::Table {
        toml::from_str(text).expect("toml")
    }

    #[test]
    fn database_boundaries_need_a_schema() {
        let plain = BoundaryConfig::parse("[boundaries.a]\nowns = [\"x\"]\n").expect("parse");
        assert!(!has_database_boundaries(&plain));
        let with = BoundaryConfig::parse("[boundaries.a]\nowns = [\"x\"]\nschema = \"a\"\n")
            .expect("parse");
        assert!(has_database_boundaries(&with));
    }

    #[test]
    fn database_config_reads_roles_and_env() {
        let env = MockEnv::new().with("AUTUMN_DATABASE__ROLES__MARKETING__PASSWORD", "pw");
        let config = database_config(
            Some(table(
                "[database]\nurl = \"postgres://toml@db/crm\"\n\
                 [database.roles.marketing]\nuser = \"marketing\"\n",
            )),
            &env,
            Some("postgres://app@db/crm".to_owned()),
        )
        .expect("config");
        assert_eq!(
            config.role_url("marketing").expect("url").as_deref(),
            Some("postgres://marketing:pw@db/crm")
        );
    }

    #[test]
    fn database_config_rejects_a_bad_role() {
        let err = database_config(
            Some(table("[database.roles.m]\npool_size = 1\n")),
            &MockEnv::new(),
            None,
        )
        .expect_err("bad role");
        assert!(err.contains("database.roles.m"), "{err}");
    }

    #[cfg(not(feature = "sqlite"))]
    mod grading {
        use super::super::*;
        use autumn_web::db_boundary::RoleReport;
        use autumn_web::graph::grants::QualifiedName;

        fn report(leak: bool) -> RoleReport {
            RoleReport {
                boundary: "marketing".into(),
                role: "marketing".into(),
                connected_as: "marketing".into(),
                allowed: 2,
                denied: 1,
                leaks: if leak {
                    vec![QualifiedName::new("core", "contacts")]
                } else {
                    Vec::new()
                },
                ..RoleReport::default()
            }
        }

        #[test]
        fn all_roles_hold() {
            let outcome = RolesOutcome {
                plan: Some(Ok(GrantPlan::default())),
                reports: vec![report(false)],
                ..RolesOutcome::default()
            };
            assert!(matches!(grade(&outcome), Verdict::Pass(d) if d.contains("1 role(s)")));
        }

        #[test]
        fn a_leak_is_a_warning() {
            let outcome = RolesOutcome {
                plan: Some(Ok(GrantPlan::default())),
                reports: vec![report(true)],
                ..RolesOutcome::default()
            };
            assert!(
                matches!(grade(&outcome), Verdict::Warn(d) if d.contains("CAN READ core.contacts"))
            );
        }

        #[test]
        fn an_unconfigured_or_unreachable_role_is_a_warning() {
            let outcome = RolesOutcome {
                plan: Some(Ok(GrantPlan::default())),
                unconfigured: vec!["core".into()],
                unreachable: vec![("marketing".into(), "refused".into())],
                ..RolesOutcome::default()
            };
            let Verdict::Warn(detail) = grade(&outcome) else {
                panic!("warn");
            };
            assert!(detail.contains("[database.roles.core]"), "{detail}");
            assert!(detail.contains("refused"), "{detail}");
        }

        #[test]
        fn a_bad_plan_fails_and_a_skip_warns() {
            let bad = RolesOutcome {
                plan: Some(Err("shared schema".into())),
                ..RolesOutcome::default()
            };
            assert_eq!(grade(&bad), Verdict::Fail("shared schema".into()));
            let skipped = RolesOutcome {
                skipped: Some("no database URL".into()),
                ..RolesOutcome::default()
            };
            assert!(matches!(grade(&skipped), Verdict::Warn(d) if d.contains("no database URL")));
        }
    }
}
