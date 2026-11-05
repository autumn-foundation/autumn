//! Database grants for table-ownership boundaries (#3550).
//!
//! A boundary with a `schema` in `boundaries.toml` gets a Postgres role. The
//! role may use its own schema, plus the tables its `may_read` and
//! `may_write` lists name. [`plan`] turns the config and the live catalog into
//! a [`GrantPlan`]. [`GrantPlan::to_sql`] writes the `REVOKE` and `GRANT`
//! statements. Each [`RolePlan`] also lists the tables the role must not read,
//! for the doctor's negative check.
//!
//! This module does no IO. `autumn db grants` and `autumn doctor` read the
//! catalog.
//!
//! See `docs/guide/database-boundaries.md`.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

use super::boundaries::{BoundaryConfig, glob};

/// Postgres truncates an identifier longer than this many bytes.
const MAX_IDENT_BYTES: usize = 63;

/// A schema-qualified table or sequence name.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct QualifiedName {
    /// The schema.
    pub schema: String,
    /// The table or sequence.
    pub name: String,
}

impl QualifiedName {
    /// A name in `schema`.
    pub fn new(schema: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            schema: schema.into(),
            name: name.into(),
        }
    }

    /// The quoted SQL form, `"schema"."name"`.
    #[must_use]
    pub fn quoted(&self) -> String {
        format!("{}.{}", quote_ident(&self.schema), quote_ident(&self.name))
    }
}

impl std::fmt::Display for QualifiedName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}", self.schema, self.name)
    }
}

/// A sequence that a table owns (a `serial` or identity column).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct OwnedSequence {
    /// The sequence.
    pub sequence: QualifiedName,
    /// The table that owns it.
    pub table: QualifiedName,
}

/// The tables and owned sequences of a database, outside the system schemas.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Catalog {
    /// Tables, views and partitioned tables.
    pub tables: Vec<QualifiedName>,
    /// Sequences that a table owns.
    pub sequences: Vec<OwnedSequence>,
}

/// The grants of one boundary role.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct RolePlan {
    /// The boundary name.
    pub boundary: String,
    /// The boundary's schema.
    pub schema: String,
    /// The Postgres role.
    pub role: String,
    /// The tables in the boundary's own schema. The role may read and write
    /// them.
    pub own: Vec<QualifiedName>,
    /// Tables of other schemas that the role may read (`may_read`).
    pub read: Vec<QualifiedName>,
    /// Tables of other schemas that the role may read and write
    /// (`may_write`).
    pub write: Vec<QualifiedName>,
    /// The sequences of the `write` tables.
    pub sequences: Vec<QualifiedName>,
    /// Tables of other boundaries that the role must not read.
    pub denied: Vec<QualifiedName>,
}

impl RolePlan {
    /// The tables that the role must read: own, `read` and `write`.
    #[must_use]
    pub fn allowed(&self) -> Vec<QualifiedName> {
        let mut all: Vec<QualifiedName> = self
            .own
            .iter()
            .chain(&self.read)
            .chain(&self.write)
            .cloned()
            .collect();
        all.sort();
        all.dedup();
        all
    }
}

/// The grants of every boundary role.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct GrantPlan {
    /// One entry per boundary with a `schema`, by boundary name.
    pub roles: Vec<RolePlan>,
    /// The schemas of the boundaries.
    pub schemas: Vec<String>,
    /// The schemas in which each role loses its old grants first: the
    /// boundary schemas and every schema in the catalog.
    pub revoke_schemas: Vec<String>,
    /// Problems that do not stop the plan, for example a `may_read` pattern
    /// that matches no table.
    pub warnings: Vec<String>,
}

/// Why a grant plan cannot be made.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GrantError {
    /// A schema or role name is not usable.
    #[error("boundary `{boundary}`: {what} `{name}` is not valid: {reason}")]
    InvalidName {
        /// The boundary.
        boundary: String,
        /// `schema` or `role`.
        what: &'static str,
        /// The name.
        name: String,
        /// Why.
        reason: &'static str,
    },
    /// A boundary sets `role` and no `schema`.
    #[error("boundary `{0}` sets `role` but no `schema`")]
    RoleWithoutSchema(String),
    /// Two boundaries use one schema or one role.
    #[error("boundaries `{first}` and `{second}` both use {what} `{name}`")]
    Shared {
        /// `schema` or `role`.
        what: &'static str,
        /// The shared name.
        name: String,
        /// One boundary.
        first: String,
        /// The other boundary.
        second: String,
    },
    /// An unqualified exception pattern matches one table name in two
    /// schemas.
    #[error(
        "boundary `{boundary}`: `{pattern}` matches `{name}` in schemas {schemas}; \
         write `<schema>.{name}`"
    )]
    Ambiguous {
        /// The boundary.
        boundary: String,
        /// The pattern.
        pattern: String,
        /// The table name.
        name: String,
        /// The schemas, comma separated.
        schemas: String,
    },
}

/// Quote a Postgres identifier.
#[must_use]
pub fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Make the grant plan for `config` against `catalog`.
///
/// Only boundaries with a `schema` take part. A config with none gives an
/// empty plan.
///
/// # Errors
///
/// Returns a [`GrantError`] for a bad name, a shared schema or role, or an
/// ambiguous exception pattern.
pub fn plan(config: &BoundaryConfig, catalog: &Catalog) -> Result<GrantPlan, GrantError> {
    let roles = database_boundaries(config)?;
    if roles.is_empty() {
        return Ok(GrantPlan::default());
    }
    let mut tables = catalog.tables.clone();
    tables.sort();
    tables.dedup();
    let schemas: BTreeSet<String> = roles.values().map(|(schema, _)| schema.clone()).collect();
    let mut warnings = Vec::new();

    // A table that a boundary owns by name, but that is not in its schema.
    // The boundary role cannot reach it, and the other roles must not.
    let mut owned_elsewhere: BTreeMap<&str, Vec<&QualifiedName>> = BTreeMap::new();
    for (name, (schema, _)) in &roles {
        let owns = &config.boundaries[name].owns;
        for table in &tables {
            if table.schema != *schema
                && !schemas.contains(&table.schema)
                && owns.iter().any(|p| matches_name(p, &table.name))
            {
                warnings.push(format!(
                    "table {table}: boundary `{name}` owns it, but it is not in schema \
                     `{schema}`; role of `{name}` cannot reach it"
                ));
                owned_elsewhere.entry(name).or_default().push(table);
            }
        }
    }
    for schema in &schemas {
        if !tables.iter().any(|t| &t.schema == schema) {
            warnings.push(format!(
                "schema `{schema}` has no tables; run the migrations first"
            ));
        }
    }

    let mut plans = Vec::new();
    for (name, (schema, role)) in &roles {
        let boundary = &config.boundaries[name];
        let own: Vec<QualifiedName> = tables
            .iter()
            .filter(|t| &t.schema == schema)
            .cloned()
            .collect();
        let write = resolve(name, schema, &boundary.may_write, &tables, &mut warnings)?;
        let read: Vec<QualifiedName> =
            resolve(name, schema, &boundary.may_read, &tables, &mut warnings)?
                .into_iter()
                .filter(|t| !write.contains(t))
                .collect();
        let mut sequences: Vec<QualifiedName> = catalog
            .sequences
            .iter()
            .filter(|s| write.contains(&s.table))
            .map(|s| s.sequence.clone())
            .collect();
        sequences.sort();
        sequences.dedup();
        let granted: BTreeSet<&QualifiedName> = own.iter().chain(&read).chain(&write).collect();
        let mut denied: Vec<QualifiedName> = tables
            .iter()
            .filter(|t| &t.schema != schema && schemas.contains(&t.schema))
            .chain(
                owned_elsewhere
                    .iter()
                    .filter(|(owner, _)| *owner != name)
                    .flat_map(|(_, list)| list.iter().copied()),
            )
            .filter(|t| !granted.contains(t))
            .cloned()
            .collect();
        denied.sort();
        denied.dedup();
        plans.push(RolePlan {
            boundary: name.clone(),
            schema: schema.clone(),
            role: role.clone(),
            own,
            read,
            write,
            sequences,
            denied,
        });
    }

    let mut revoke_schemas = schemas.clone();
    revoke_schemas.extend(tables.iter().map(|t| t.schema.clone()));
    Ok(GrantPlan {
        roles: plans,
        schemas: schemas.into_iter().collect(),
        revoke_schemas: revoke_schemas.into_iter().collect(),
        warnings,
    })
}

/// The boundaries with a `schema`: name to `(schema, role)`. Checks the
/// names and that no two boundaries share a schema or a role.
fn database_boundaries(
    config: &BoundaryConfig,
) -> Result<BTreeMap<String, (String, String)>, GrantError> {
    let mut out = BTreeMap::new();
    let mut schema_owner: BTreeMap<String, &str> = BTreeMap::new();
    let mut role_owner: BTreeMap<String, &str> = BTreeMap::new();
    for (name, boundary) in &config.boundaries {
        let Some(schema) = boundary.schema.as_deref() else {
            if boundary.role.is_some() {
                return Err(GrantError::RoleWithoutSchema(name.clone()));
            }
            continue;
        };
        let role = boundary.role.as_deref().unwrap_or(name);
        check_name(name, "schema", schema)?;
        check_name(name, "role", role)?;
        if role.eq_ignore_ascii_case("public") {
            return Err(invalid(name, "role", role, "`public` names every role"));
        }
        for (what, value, owners) in [
            ("schema", schema, &mut schema_owner),
            ("role", role, &mut role_owner),
        ] {
            if let Some(first) = owners.insert(value.to_owned(), name) {
                return Err(GrantError::Shared {
                    what,
                    name: value.to_owned(),
                    first: first.to_owned(),
                    second: name.clone(),
                });
            }
        }
        out.insert(name.clone(), (schema.to_owned(), role.to_owned()));
    }
    Ok(out)
}

/// `text` with each control character replaced by a space, so it cannot end
/// a `--` comment.
fn comment_safe(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// Check a schema or role name.
fn check_name(boundary: &str, what: &'static str, name: &str) -> Result<(), GrantError> {
    let reason = if name.is_empty() {
        "it is empty"
    } else if name.chars().any(char::is_control) {
        "it holds a control character"
    } else if name.len() > MAX_IDENT_BYTES {
        "it is longer than 63 bytes"
    } else if name.to_ascii_lowercase().starts_with("pg_") {
        "the `pg_` prefix is reserved"
    } else {
        return Ok(());
    };
    Err(invalid(boundary, what, name, reason))
}

fn invalid(boundary: &str, what: &'static str, name: &str, reason: &'static str) -> GrantError {
    GrantError::InvalidName {
        boundary: boundary.to_owned(),
        what,
        name: name.to_owned(),
        reason,
    }
}

/// Match a table-name pattern without case.
fn matches_name(pattern: &str, name: &str) -> bool {
    glob(&pattern.to_ascii_lowercase(), &name.to_ascii_lowercase())
}

/// Resolve exception patterns to catalog tables outside `own_schema`.
///
/// `schema.table` matches both parts. A bare pattern matches the table name
/// in any schema; when one name matches in two schemas, that is an error.
fn resolve(
    boundary: &str,
    own_schema: &str,
    patterns: &[String],
    tables: &[QualifiedName],
    warnings: &mut Vec<String>,
) -> Result<Vec<QualifiedName>, GrantError> {
    let mut out = BTreeSet::new();
    for pattern in patterns {
        let found: Vec<&QualifiedName> = match pattern.split_once('.') {
            Some((schema, table)) => tables
                .iter()
                .filter(|t| matches_name(schema, &t.schema) && matches_name(table, &t.name))
                .collect(),
            None => tables
                .iter()
                .filter(|t| matches_name(pattern, &t.name))
                .collect(),
        };
        let found: Vec<&QualifiedName> = found
            .into_iter()
            .filter(|t| t.schema != own_schema)
            .collect();
        if found.is_empty() {
            warnings.push(format!(
                "boundary `{boundary}`: exception `{pattern}` matches no table"
            ));
            continue;
        }
        if !pattern.contains('.') {
            let mut by_name: BTreeMap<String, BTreeSet<&str>> = BTreeMap::new();
            for table in &found {
                by_name
                    .entry(table.name.to_ascii_lowercase())
                    .or_default()
                    .insert(&table.schema);
            }
            if let Some((name, schemas)) = by_name.into_iter().find(|(_, s)| s.len() > 1) {
                return Err(GrantError::Ambiguous {
                    boundary: boundary.to_owned(),
                    pattern: pattern.clone(),
                    name,
                    schemas: schemas.into_iter().collect::<Vec<_>>().join(", "),
                });
            }
        }
        out.extend(found.into_iter().cloned());
    }
    Ok(out.into_iter().collect())
}

/// Table privileges of a role in its own schema and through `may_write`.
const DML: &str = "SELECT, INSERT, UPDATE, DELETE";
/// Sequence privileges for `nextval`, `currval` and `setval`.
const SEQUENCE: &str = "USAGE, SELECT, UPDATE";

impl GrantPlan {
    /// The SQL that makes the database match the plan, in one transaction.
    /// Empty when no boundary has a `schema`.
    ///
    /// Each role first loses every grant in every schema of the plan, so an
    /// exception that left the config also leaves the database. The roles
    /// must exist.
    #[must_use]
    pub fn to_sql(&self) -> String {
        if self.roles.is_empty() {
            return String::new();
        }
        let mut sql = String::from(
            "-- Generated by `autumn db grants` from boundaries.toml. Do not edit.\n\
             -- The roles must exist. See docs/guide/database-boundaries.md.\nBEGIN;\n",
        );
        for schema in &self.schemas {
            let s = quote_ident(schema);
            let _ = writeln!(sql, "CREATE SCHEMA IF NOT EXISTS {s};");
            let _ = writeln!(sql, "REVOKE ALL ON SCHEMA {s} FROM PUBLIC;");
            let _ = writeln!(sql, "REVOKE ALL ON ALL TABLES IN SCHEMA {s} FROM PUBLIC;");
        }
        for role in &self.roles {
            let r = quote_ident(&role.role);
            let _ = writeln!(
                sql,
                "\n-- Boundary `{}`: role {r}, schema {}.",
                comment_safe(&role.boundary),
                quote_ident(&role.schema)
            );
            for schema in &self.revoke_schemas {
                let s = quote_ident(schema);
                let _ = writeln!(sql, "REVOKE ALL ON ALL TABLES IN SCHEMA {s} FROM {r};");
                let _ = writeln!(sql, "REVOKE ALL ON ALL SEQUENCES IN SCHEMA {s} FROM {r};");
                let _ = writeln!(sql, "REVOKE ALL ON SCHEMA {s} FROM {r};");
                let _ = writeln!(
                    sql,
                    "ALTER DEFAULT PRIVILEGES IN SCHEMA {s} REVOKE ALL ON TABLES FROM {r};"
                );
                let _ = writeln!(
                    sql,
                    "ALTER DEFAULT PRIVILEGES IN SCHEMA {s} REVOKE ALL ON SEQUENCES FROM {r};"
                );
            }
            let s = quote_ident(&role.schema);
            let _ = writeln!(sql, "GRANT USAGE ON SCHEMA {s} TO {r};");
            let _ = writeln!(sql, "GRANT {DML} ON ALL TABLES IN SCHEMA {s} TO {r};");
            let _ = writeln!(
                sql,
                "GRANT {SEQUENCE} ON ALL SEQUENCES IN SCHEMA {s} TO {r};"
            );
            let _ = writeln!(
                sql,
                "ALTER DEFAULT PRIVILEGES IN SCHEMA {s} GRANT {DML} ON TABLES TO {r};"
            );
            let _ = writeln!(
                sql,
                "ALTER DEFAULT PRIVILEGES IN SCHEMA {s} GRANT {SEQUENCE} ON SEQUENCES TO {r};"
            );
            let foreign: BTreeSet<&str> = role
                .read
                .iter()
                .chain(&role.write)
                .map(|t| t.schema.as_str())
                .collect();
            for schema in foreign {
                let _ = writeln!(sql, "GRANT USAGE ON SCHEMA {} TO {r};", quote_ident(schema));
            }
            for table in &role.read {
                let _ = writeln!(sql, "GRANT SELECT ON TABLE {} TO {r};", table.quoted());
            }
            for table in &role.write {
                let _ = writeln!(sql, "GRANT {DML} ON TABLE {} TO {r};", table.quoted());
            }
            for sequence in &role.sequences {
                let _ = writeln!(
                    sql,
                    "GRANT {SEQUENCE} ON SEQUENCE {} TO {r};",
                    sequence.quoted()
                );
            }
        }
        sql.push_str("COMMIT;\n");
        sql
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(schema: &str, name: &str) -> QualifiedName {
        QualifiedName::new(schema, name)
    }

    fn config(text: &str) -> BoundaryConfig {
        BoundaryConfig::parse(text).expect("config parses")
    }

    const CRM: &str = r#"
[boundaries.core]
owns = ["contacts", "deals"]
schema = "core"

[boundaries.marketing]
owns = ["campaigns", "attribution_*"]
schema = "marketing"
may_read = ["contacts"]
may_write = ["core.deals"]
"#;

    fn crm_catalog() -> Catalog {
        Catalog {
            tables: vec![
                q("core", "contacts"),
                q("core", "deals"),
                q("core", "notes"),
                q("marketing", "campaigns"),
                q("marketing", "attribution_touches"),
                q("public", "__diesel_schema_migrations"),
                q("public", "sessions"),
            ],
            sequences: vec![
                OwnedSequence {
                    sequence: q("core", "deals_id_seq"),
                    table: q("core", "deals"),
                },
                OwnedSequence {
                    sequence: q("core", "contacts_id_seq"),
                    table: q("core", "contacts"),
                },
            ],
        }
    }

    fn role<'a>(plan: &'a GrantPlan, boundary: &str) -> &'a RolePlan {
        plan.roles
            .iter()
            .find(|r| r.boundary == boundary)
            .expect("role plan")
    }

    #[test]
    fn no_schema_means_empty_plan() {
        let config = config("[boundaries.core]\nowns = [\"contacts\"]\n");
        let plan = plan(&config, &crm_catalog()).expect("plan");
        assert!(plan.roles.is_empty());
        assert!(plan.to_sql().is_empty());
    }

    #[test]
    fn role_defaults_to_boundary_name() {
        let plan = plan(&config(CRM), &crm_catalog()).expect("plan");
        assert_eq!(role(&plan, "marketing").role, "marketing");
        assert_eq!(role(&plan, "marketing").schema, "marketing");
        let named = config(
            "[boundaries.marketing]\nowns = [\"campaigns\"]\nschema = \"mkt\"\nrole = \"mkt_app\"\n",
        );
        let plan = super::plan(&named, &crm_catalog()).expect("plan");
        assert_eq!(role(&plan, "marketing").role, "mkt_app");
    }

    #[test]
    fn own_schema_tables_are_own() {
        let plan = plan(&config(CRM), &crm_catalog()).expect("plan");
        let marketing = role(&plan, "marketing");
        assert_eq!(
            marketing.own,
            vec![
                q("marketing", "attribution_touches"),
                q("marketing", "campaigns")
            ]
        );
    }

    #[test]
    fn exceptions_resolve_against_the_catalog() {
        let plan = plan(&config(CRM), &crm_catalog()).expect("plan");
        let marketing = role(&plan, "marketing");
        assert_eq!(marketing.read, vec![q("core", "contacts")]);
        assert_eq!(marketing.write, vec![q("core", "deals")]);
        assert_eq!(marketing.sequences, vec![q("core", "deals_id_seq")]);
    }

    #[test]
    fn denied_lists_other_boundary_tables_outside_the_grant() {
        let plan = plan(&config(CRM), &crm_catalog()).expect("plan");
        // `core.notes` is in core's schema and not in an exception.
        assert_eq!(role(&plan, "marketing").denied, vec![q("core", "notes")]);
        // Core has no exceptions: every marketing table is denied.
        assert_eq!(
            role(&plan, "core").denied,
            vec![
                q("marketing", "attribution_touches"),
                q("marketing", "campaigns")
            ]
        );
    }

    #[test]
    fn denied_includes_an_owned_table_outside_its_schema() {
        let mut catalog = crm_catalog();
        catalog.tables.push(q("public", "deals_archive"));
        let config = config(
            "[boundaries.core]\nowns = [\"deals*\"]\nschema = \"core\"\n\
             [boundaries.marketing]\nowns = [\"campaigns\"]\nschema = \"marketing\"\n",
        );
        let plan = plan(&config, &catalog).expect("plan");
        assert!(
            role(&plan, "marketing")
                .denied
                .contains(&q("public", "deals_archive"))
        );
        assert!(
            plan.warnings
                .iter()
                .any(|w| w.contains("public.deals_archive") && w.contains("core")),
            "{:?}",
            plan.warnings
        );
    }

    #[test]
    fn unmatched_exception_is_a_warning() {
        let config = config(
            "[boundaries.marketing]\nowns = [\"campaigns\"]\nschema = \"marketing\"\n\
             may_read = [\"nothing_*\"]\n",
        );
        let plan = plan(&config, &crm_catalog()).expect("plan");
        assert!(plan.warnings.iter().any(|w| w.contains("nothing_*")));
    }

    #[test]
    fn ambiguous_unqualified_pattern_is_an_error() {
        let mut catalog = crm_catalog();
        catalog.tables.push(q("audit", "contacts"));
        let err = plan(&config(CRM), &catalog).expect_err("ambiguous");
        assert!(matches!(err, GrantError::Ambiguous { .. }), "{err}");
        assert!(err.to_string().contains("<schema>.contacts"));
    }

    #[test]
    fn write_wins_over_read() {
        let config = config(
            "[boundaries.marketing]\nowns = [\"campaigns\"]\nschema = \"marketing\"\n\
             may_read = [\"deals\"]\nmay_write = [\"deals\"]\n",
        );
        let plan = plan(&config, &crm_catalog()).expect("plan");
        let marketing = role(&plan, "marketing");
        assert!(marketing.read.is_empty());
        assert_eq!(marketing.write, vec![q("core", "deals")]);
    }

    #[test]
    fn shared_schema_or_role_is_an_error() {
        let shared = config(
            "[boundaries.a]\nowns = [\"x\"]\nschema = \"s\"\n\
             [boundaries.b]\nowns = [\"y\"]\nschema = \"s\"\n",
        );
        assert!(matches!(
            plan(&shared, &Catalog::default()),
            Err(GrantError::Shared { what: "schema", .. })
        ));
        let shared_role = config(
            "[boundaries.a]\nowns = [\"x\"]\nschema = \"s\"\nrole = \"r\"\n\
             [boundaries.b]\nowns = [\"y\"]\nschema = \"t\"\nrole = \"r\"\n",
        );
        assert!(matches!(
            plan(&shared_role, &Catalog::default()),
            Err(GrantError::Shared { what: "role", .. })
        ));
    }

    #[test]
    fn role_without_schema_is_an_error() {
        let config = config("[boundaries.a]\nowns = [\"x\"]\nrole = \"r\"\n");
        assert_eq!(
            plan(&config, &Catalog::default()),
            Err(GrantError::RoleWithoutSchema("a".into()))
        );
    }

    #[test]
    fn bad_names_are_errors() {
        for (schema, role) in [
            ("", "ok"),
            ("pg_mine", "ok"),
            ("ok", "public"),
            ("ok", "PUBLIC"),
            ("ok", "pg_read_all_data"),
            ("bad\0name", "ok"),
        ] {
            let mut config = config("[boundaries.a]\nowns = [\"x\"]\n");
            let boundary = config.boundaries.get_mut("a").expect("boundary");
            boundary.schema = Some(schema.to_owned());
            boundary.role = Some(role.to_owned());
            assert!(
                matches!(
                    plan(&config, &Catalog::default()),
                    Err(GrantError::InvalidName { .. })
                ),
                "schema {schema:?} role {role:?}"
            );
        }
        let long = "s".repeat(64);
        let config = config(&format!(
            "[boundaries.a]\nowns = [\"x\"]\nschema = \"{long}\"\n"
        ));
        assert!(matches!(
            plan(&config, &Catalog::default()),
            Err(GrantError::InvalidName { .. })
        ));
    }

    #[test]
    fn sql_revokes_first_then_grants_in_one_transaction() {
        let sql = plan(&config(CRM), &crm_catalog()).expect("plan").to_sql();
        let at = |needle: &str| {
            sql.find(needle)
                .unwrap_or_else(|| panic!("missing `{needle}` in:\n{sql}"))
        };
        assert!(sql.trim_start().starts_with("--"));
        assert!(at("BEGIN;") < at("REVOKE"));
        assert!(sql.trim_end().ends_with("COMMIT;"));
        at("CREATE SCHEMA IF NOT EXISTS \"marketing\";");
        at("REVOKE ALL ON SCHEMA \"marketing\" FROM PUBLIC;");
        let revoke = at("REVOKE ALL ON ALL TABLES IN SCHEMA \"core\" FROM \"marketing\";");
        at("REVOKE ALL ON ALL TABLES IN SCHEMA \"public\" FROM \"marketing\";");
        at("REVOKE ALL ON ALL SEQUENCES IN SCHEMA \"core\" FROM \"marketing\";");
        let grant = at(
            "GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA \"marketing\" \
             TO \"marketing\";",
        );
        assert!(revoke < grant);
        at("GRANT USAGE ON SCHEMA \"marketing\" TO \"marketing\";");
        at(
            "GRANT USAGE, SELECT, UPDATE ON ALL SEQUENCES IN SCHEMA \"marketing\" TO \"marketing\";",
        );
        at(
            "ALTER DEFAULT PRIVILEGES IN SCHEMA \"marketing\" GRANT SELECT, INSERT, UPDATE, \
            DELETE ON TABLES TO \"marketing\";",
        );
        at("GRANT USAGE ON SCHEMA \"core\" TO \"marketing\";");
        at("GRANT SELECT ON TABLE \"core\".\"contacts\" TO \"marketing\";");
        at("GRANT SELECT, INSERT, UPDATE, DELETE ON TABLE \"core\".\"deals\" TO \"marketing\";");
        at("GRANT USAGE, SELECT, UPDATE ON SEQUENCE \"core\".\"deals_id_seq\" TO \"marketing\";");
        assert!(!sql.contains("\"core\".\"notes\" TO \"marketing\""));
    }

    #[test]
    fn sql_quotes_identifiers() {
        let config =
            config("[boundaries.a]\nowns = [\"x\"]\nschema = \"we\\\"ird\"\nrole = \"r; DROP\"\n");
        let sql = plan(&config, &Catalog::default()).expect("plan").to_sql();
        assert!(sql.contains("\"we\"\"ird\""), "{sql}");
        assert!(sql.contains("TO \"r; DROP\";"), "{sql}");
    }

    #[test]
    fn plan_is_deterministic() {
        let mut shuffled = crm_catalog();
        shuffled.tables.reverse();
        shuffled.sequences.reverse();
        assert_eq!(
            plan(&config(CRM), &crm_catalog()).expect("plan").to_sql(),
            plan(&config(CRM), &shuffled).expect("plan").to_sql()
        );
    }

    #[test]
    fn control_characters_in_names_are_errors() {
        for (schema, role) in [
            ("a\nb", "ok"),
            ("ok", "r\nDROP SCHEMA core; --"),
            ("ok", "r\rx"),
        ] {
            let mut config = config("[boundaries.a]\nowns = [\"x\"]\n");
            let boundary = config.boundaries.get_mut("a").expect("boundary");
            boundary.schema = Some(schema.to_owned());
            boundary.role = Some(role.to_owned());
            assert!(
                matches!(
                    plan(&config, &Catalog::default()),
                    Err(GrantError::InvalidName { .. })
                ),
                "schema {schema:?} role {role:?}"
            );
        }
    }

    #[test]
    fn a_boundary_name_cannot_leave_the_sql_comment() {
        let mut config = config("[boundaries.a]\nowns = [\"x\"]\nschema = \"s\"\nrole = \"r\"\n");
        let boundary = config.boundaries.remove("a").expect("boundary");
        config
            .boundaries
            .insert("a\nDROP SCHEMA core;\r--".to_owned(), boundary);
        let sql = plan(&config, &Catalog::default()).expect("plan").to_sql();
        assert!(
            !sql.lines().any(|l| l.trim_start().starts_with("DROP")),
            "{sql}"
        );
    }

    #[test]
    fn sql_revokes_default_privileges_and_public_table_grants() {
        let sql = plan(&config(CRM), &crm_catalog()).expect("plan").to_sql();
        for needle in [
            "REVOKE ALL ON ALL TABLES IN SCHEMA \"core\" FROM PUBLIC;",
            "ALTER DEFAULT PRIVILEGES IN SCHEMA \"core\" REVOKE ALL ON TABLES FROM \"marketing\";",
            "ALTER DEFAULT PRIVILEGES IN SCHEMA \"public\" REVOKE ALL ON SEQUENCES FROM \"marketing\";",
        ] {
            assert!(sql.contains(needle), "missing `{needle}` in:\n{sql}");
        }
        let revoke = sql
            .find("ALTER DEFAULT PRIVILEGES IN SCHEMA \"marketing\" REVOKE ALL ON TABLES FROM \"marketing\";")
            .expect("revoke");
        let grant = sql
            .find("ALTER DEFAULT PRIVILEGES IN SCHEMA \"marketing\" GRANT")
            .expect("grant");
        assert!(revoke < grant);
    }

    #[test]
    fn exceptions_match_globs_and_ignore_case() {
        let config = config(
            "[boundaries.marketing]\nowns = [\"campaigns\"]\nschema = \"marketing\"\n\
             may_read = [\"CORE.*\", \"Sessions\"]\n",
        );
        let plan = plan(&config, &crm_catalog()).expect("plan");
        assert_eq!(
            role(&plan, "marketing").read,
            vec![
                q("core", "contacts"),
                q("core", "deals"),
                q("core", "notes"),
                q("public", "sessions"),
            ]
        );
    }

    #[test]
    fn an_exception_on_an_own_table_is_a_warning() {
        let config = config(
            "[boundaries.marketing]\nowns = [\"campaigns\"]\nschema = \"marketing\"\n\
             may_read = [\"campaigns\"]\n",
        );
        let plan = plan(&config, &crm_catalog()).expect("plan");
        assert!(role(&plan, "marketing").read.is_empty());
        assert!(plan.warnings.iter().any(|w| w.contains("`campaigns`")));
    }

    #[test]
    fn revoke_schemas_cover_every_catalog_schema() {
        let mut catalog = crm_catalog();
        catalog.tables.push(q("audit", "events"));
        let plan = plan(&config(CRM), &catalog).expect("plan");
        assert_eq!(
            plan.revoke_schemas,
            vec!["audit", "core", "marketing", "public"]
        );
        assert_eq!(plan.schemas, vec!["core", "marketing"]);
    }

    #[test]
    fn allowed_is_own_read_and_write() {
        let plan = plan(&config(CRM), &crm_catalog()).expect("plan");
        assert_eq!(
            role(&plan, "marketing").allowed(),
            vec![
                q("core", "contacts"),
                q("core", "deals"),
                q("marketing", "attribution_touches"),
                q("marketing", "campaigns"),
            ]
        );
    }
}
