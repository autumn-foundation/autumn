//! Database grants for table-ownership boundaries (#3550).
//!
//! A boundary with a `role` in `boundaries.toml` gets a Postgres role. That
//! role can use its own tables and its `may_read` / `may_write` exceptions.
//! It cannot use any other table. [`plan`] reads the config and the live
//! catalog and gives a [`GrantPlan`]. [`GrantPlan::sql`] gives the `REVOKE` and
//! `GRANT` statements in one transaction.
//!
//! The `owns` lists are globs, so the plan needs the live table list. The CLI
//! reads it from `pg_class`. This module does no IO.
//!
//! See `docs/guide/database-boundaries.md`.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{self, Write as _};

use serde::Serialize;

use super::boundaries::{BoundaryConfig, BoundaryError, glob, pick, specificity};

/// A schema-qualified name of a table or sequence.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct QualifiedName {
    /// The Postgres schema.
    pub schema: String,
    /// The name in the schema.
    pub name: String,
}

impl QualifiedName {
    /// A name in `schema`.
    #[must_use]
    pub fn new(schema: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            schema: schema.into(),
            name: name.into(),
        }
    }

    /// The name as SQL: `"schema"."name"`.
    #[must_use]
    pub fn quoted(&self) -> String {
        format!("{}.{}", quote_ident(&self.schema), quote_ident(&self.name))
    }
}

impl fmt::Display for QualifiedName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.schema, self.name)
    }
}

/// One table of the live catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogTable {
    /// The table.
    pub table: QualifiedName,
    /// The sequences that the table uses: `serial` and identity columns,
    /// and `nextval` defaults.
    pub sequences: Vec<QualifiedName>,
    /// A view or a materialized view. A view runs with the rights of its
    /// owner, so a schema does not give a boundary its views. Only an `owns`
    /// pattern does.
    pub view: bool,
}

impl CatalogTable {
    /// A table with no sequences.
    #[must_use]
    pub fn new(schema: &str, name: &str) -> Self {
        Self {
            table: QualifiedName::new(schema, name),
            sequences: Vec::new(),
            view: false,
        }
    }

    /// Mark the table as a view.
    #[must_use]
    pub const fn as_view(mut self) -> Self {
        self.view = true;
        self
    }

    /// Add an owned sequence in the schema of the table.
    #[must_use]
    pub fn with_sequence(mut self, name: &str) -> Self {
        self.sequences
            .push(QualifiedName::new(self.table.schema.clone(), name));
        self
    }
}

/// The grants of one role.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct RoleGrants {
    /// The boundary.
    pub boundary: String,
    /// The Postgres role.
    pub role: String,
    /// The schema of the boundary.
    pub schema: String,
    /// The tables that the boundary owns. The role can read and write them.
    pub own: Vec<QualifiedName>,
    /// The `may_read` tables. The role can read them.
    pub read: Vec<QualifiedName>,
    /// The `may_write` tables. The role can read and write them.
    pub write: Vec<QualifiedName>,
    /// The sequences of the `own` and `write` tables.
    pub sequences: Vec<QualifiedName>,
    /// Every other table of the catalog. The role must not read them.
    pub denied: Vec<QualifiedName>,
}

impl RoleGrants {
    /// Whether the role may read `table`.
    #[must_use]
    pub fn may_read(&self, table: &QualifiedName) -> bool {
        self.own.contains(table) || self.read.contains(table) || self.write.contains(table)
    }
}

/// The grants of every role in `boundaries.toml`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct GrantPlan {
    /// One entry for each boundary with a `role`, by boundary name.
    pub roles: Vec<RoleGrants>,
    /// Every schema that a `REVOKE` covers.
    pub schemas: Vec<String>,
    /// Things that are probably mistakes. They do not stop the plan.
    pub warnings: Vec<String>,
}

/// Why a grant plan cannot be made.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GrantError {
    /// The boundary config is not valid.
    #[error(transparent)]
    Boundary(#[from] BoundaryError),
    /// Two boundaries declare one role.
    #[error("role `{role}` is declared by boundary `{first}` and boundary `{second}`")]
    SharedRole {
        /// The role.
        role: String,
        /// One boundary.
        first: String,
        /// The other boundary.
        second: String,
    },
}

/// Make the grant plan for `config` over the live `catalog`.
///
/// # Errors
///
/// Returns [`GrantError::Boundary`] when two boundaries own a table equally,
/// and [`GrantError::SharedRole`] when two boundaries declare one role.
pub fn plan(config: &BoundaryConfig, catalog: &[CatalogTable]) -> Result<GrantPlan, GrantError> {
    let mut by_role: BTreeMap<&str, &str> = BTreeMap::new();
    for (name, boundary) in &config.boundaries {
        if let Some(role) = boundary.role.as_deref()
            && let Some(first) = by_role.insert(role, name)
        {
            return Err(GrantError::SharedRole {
                role: role.to_owned(),
                first: first.to_owned(),
                second: name.clone(),
            });
        }
    }

    let mut tables: Vec<&CatalogTable> = catalog.iter().collect();
    tables.sort_by(|a, b| a.table.cmp(&b.table));
    tables.dedup_by(|a, b| a.table == b.table);
    let owners = tables
        .iter()
        .map(|t| owner(config, t))
        .collect::<Result<Vec<_>, _>>()?;

    let mut roles = Vec::new();
    let mut warnings = Vec::new();
    for (name, boundary) in &config.boundaries {
        let Some(role) = boundary.role.as_deref() else {
            continue;
        };
        let mut grants = RoleGrants {
            boundary: name.clone(),
            role: role.to_owned(),
            schema: boundary.schema_name().to_owned(),
            ..RoleGrants::default()
        };
        for (table, owner) in tables.iter().zip(&owners) {
            let hit = |list: &[String]| list.iter().any(|p| exception_matches(p, &table.table));
            if *owner == Some(name.as_str()) {
                grants.own.push(table.table.clone());
                grants.sequences.extend(table.sequences.iter().cloned());
            } else if hit(&boundary.may_write) {
                grants.write.push(table.table.clone());
                grants.sequences.extend(table.sequences.iter().cloned());
            } else if hit(&boundary.may_read) {
                grants.read.push(table.table.clone());
            } else {
                grants.denied.push(table.table.clone());
            }
        }
        grants.sequences.sort();
        grants.sequences.dedup();
        if grants.own.is_empty() {
            warnings.push(format!(
                "boundary `{name}` declares role `{role}`, but owns no table in the database"
            ));
        }
        for pattern in boundary.may_read.iter().chain(&boundary.may_write) {
            if !tables.iter().any(|t| exception_matches(pattern, &t.table)) {
                warnings.push(format!(
                    "boundary `{name}`: exception `{pattern}` matches no table in the database"
                ));
            }
        }
        roles.push(grants);
    }

    // Only schemas that exist: a REVOKE on a missing schema stops the whole
    // transaction.
    let schemas: BTreeSet<String> = tables.iter().map(|t| t.table.schema.clone()).collect();
    Ok(GrantPlan {
        roles,
        schemas: schemas.into_iter().collect(),
        warnings,
    })
}

/// The boundary that owns a catalog table.
///
/// An `owns` pattern with no `.` matches a table name in the schema of its
/// boundary. A pattern with a `.` matches `schema.table`. The most specific
/// pattern wins. When no pattern matches, a table (not a view) in a schema
/// other than `public` belongs to the one boundary with that schema. Schema
/// names match without case.
fn owner<'a>(
    config: &'a BoundaryConfig,
    entry: &CatalogTable,
) -> Result<Option<&'a str>, BoundaryError> {
    let table = &entry.table;
    let mut candidates = Vec::new();
    for (name, boundary) in &config.boundaries {
        let in_schema = boundary.schema_name().eq_ignore_ascii_case(&table.schema);
        for pattern in &boundary.owns {
            let hit = if pattern.contains('.') {
                exception_matches(pattern, table)
            } else {
                in_schema
                    && glob(
                        &pattern.to_ascii_lowercase(),
                        &table.name.to_ascii_lowercase(),
                    )
            };
            if hit {
                candidates.push((name.as_str(), specificity(pattern)));
            }
        }
    }
    if let Some(found) = pick("table", &table.to_string(), &candidates)? {
        return Ok(Some(found));
    }
    if entry.view
        || table
            .schema
            .eq_ignore_ascii_case(super::boundaries::DEFAULT_SCHEMA)
    {
        return Ok(None);
    }
    let mut in_schema = config.boundaries.iter().filter(|(_, b)| {
        b.schema
            .as_deref()
            .is_some_and(|s| s.eq_ignore_ascii_case(&table.schema))
    });
    Ok(match (in_schema.next(), in_schema.next()) {
        (Some((name, _)), None) => Some(name.as_str()),
        _ => None,
    })
}

/// Whether an exception pattern matches a table. A pattern with a `.`
/// matches `schema.table`; else it matches the table name in any schema.
fn exception_matches(pattern: &str, table: &QualifiedName) -> bool {
    let pattern = pattern.to_ascii_lowercase();
    if pattern.contains('.') {
        glob(&pattern, &table.to_string().to_ascii_lowercase())
    } else {
        glob(&pattern, &table.name.to_ascii_lowercase())
    }
}

impl GrantPlan {
    /// The `REVOKE` and `GRANT` statements, in one transaction. `None` when no
    /// boundary declares a role.
    ///
    /// Each role first loses every privilege on every table, sequence and
    /// schema in [`Self::schemas`]. Then it gets its own tables and its
    /// exceptions back. A role in a schema other than `public` also gets that
    /// schema first on its `search_path`.
    #[must_use]
    pub fn sql(&self) -> Option<String> {
        if self.roles.is_empty() {
            return None;
        }
        let schemas = join(self.schemas.iter().map(|s| quote_ident(s)));
        let mut out = String::from(
            "-- Generated by `autumn db grants` from boundaries.toml. Do not edit.\nBEGIN;\n",
        );
        for g in &self.roles {
            let role = quote_ident(&g.role);
            let comment = format!(
                "-- Boundary `{}`: role {role}, schema {}.",
                g.boundary,
                quote_ident(&g.schema)
            );
            let _ = writeln!(out, "\n{}", comment_text(&comment));
            if !self.schemas.is_empty() {
                let _ = writeln!(
                    out,
                    "REVOKE ALL ON ALL TABLES IN SCHEMA {schemas} FROM {role};"
                );
                let _ = writeln!(
                    out,
                    "REVOKE ALL ON ALL SEQUENCES IN SCHEMA {schemas} FROM {role};"
                );
                let _ = writeln!(out, "REVOKE ALL ON SCHEMA {schemas} FROM {role};");
            }
            // Every schema that holds a granted table or sequence. A
            // qualified `owns` pattern can name a table outside `g.schema`.
            let usage: BTreeSet<&str> = g
                .own
                .iter()
                .chain(&g.read)
                .chain(&g.write)
                .chain(&g.sequences)
                .map(|t| t.schema.as_str())
                .collect();
            if !usage.is_empty() {
                let _ = writeln!(
                    out,
                    "GRANT USAGE ON SCHEMA {} TO {role};",
                    join(usage.into_iter().map(quote_ident))
                );
            }
            for (privileges, tables) in [(DML, &g.own), ("SELECT", &g.read), (DML, &g.write)] {
                if !tables.is_empty() {
                    let _ = writeln!(
                        out,
                        "GRANT {privileges} ON TABLE {} TO {role};",
                        join(tables.iter().map(QualifiedName::quoted))
                    );
                }
            }
            if !g.sequences.is_empty() {
                let _ = writeln!(
                    out,
                    "GRANT USAGE, SELECT ON SEQUENCE {} TO {role};",
                    join(g.sequences.iter().map(QualifiedName::quoted))
                );
            }
            if g.schema != super::boundaries::DEFAULT_SCHEMA {
                let _ = writeln!(
                    out,
                    "ALTER ROLE {role} SET search_path = {}, {};",
                    quote_ident(&g.schema),
                    quote_ident(super::boundaries::DEFAULT_SCHEMA)
                );
            }
        }
        out.push_str("\nCOMMIT;\n");
        Some(out)
    }
}

/// The table privileges of an own or `may_write` table.
const DML: &str = "SELECT, INSERT, UPDATE, DELETE";

/// Join SQL items with `, `.
fn join(items: impl Iterator<Item = String>) -> String {
    items.collect::<Vec<_>>().join(", ")
}

/// A line that is safe as a `--` comment: no line breaks or other control
/// characters, which would end the comment.
fn comment_text(line: &str) -> String {
    line.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// Quote a Postgres identifier: `"name"`, with each `"` doubled.
#[must_use]
pub fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: &str = r#"
[boundaries.core]
owns = ["contacts", "deals"]
crates = ["crm-core"]
role = "core"

[boundaries.marketing]
owns = ["campaigns", "attribution_*"]
crates = ["crm-marketing"]
schema = "marketing"
role = "marketing"
may_read = ["contacts"]
may_write = ["autumn_jobs"]
"#;

    fn config(text: &str) -> BoundaryConfig {
        BoundaryConfig::parse(text).expect("config")
    }

    fn catalog() -> Vec<CatalogTable> {
        vec![
            CatalogTable::new("public", "contacts").with_sequence("contacts_id_seq"),
            CatalogTable::new("public", "deals"),
            CatalogTable::new("public", "autumn_jobs").with_sequence("autumn_jobs_id_seq"),
            CatalogTable::new("marketing", "campaigns").with_sequence("campaigns_id_seq"),
            CatalogTable::new("marketing", "attribution_clicks"),
            CatalogTable::new("marketing", "drafts"),
        ]
    }

    fn role<'a>(plan: &'a GrantPlan, name: &str) -> &'a RoleGrants {
        plan.roles
            .iter()
            .find(|r| r.role == name)
            .expect("role in plan")
    }

    fn names(list: &[QualifiedName]) -> Vec<String> {
        list.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn own_tables_are_resolved_in_the_boundary_schema() {
        let plan = plan(&config(CONFIG), &catalog()).unwrap();
        let marketing = role(&plan, "marketing");
        assert_eq!(marketing.schema, "marketing");
        // `drafts` matches no pattern; the schema implies the owner.
        assert_eq!(
            names(&marketing.own),
            [
                "marketing.attribution_clicks",
                "marketing.campaigns",
                "marketing.drafts"
            ]
        );
        assert_eq!(
            names(&role(&plan, "core").own),
            ["public.contacts", "public.deals"]
        );
    }

    #[test]
    fn exceptions_grant_read_and_write() {
        let plan = plan(&config(CONFIG), &catalog()).unwrap();
        let marketing = role(&plan, "marketing");
        assert_eq!(names(&marketing.read), ["public.contacts"]);
        assert_eq!(names(&marketing.write), ["public.autumn_jobs"]);
        assert_eq!(
            names(&marketing.sequences),
            ["marketing.campaigns_id_seq", "public.autumn_jobs_id_seq"]
        );
    }

    #[test]
    fn every_other_table_is_denied() {
        let plan = plan(&config(CONFIG), &catalog()).unwrap();
        assert_eq!(names(&role(&plan, "marketing").denied), ["public.deals"]);
        let core = role(&plan, "core");
        assert_eq!(
            names(&core.denied),
            [
                "marketing.attribution_clicks",
                "marketing.campaigns",
                "marketing.drafts",
                "public.autumn_jobs"
            ]
        );
        assert!(!core.may_read(&QualifiedName::new("marketing", "campaigns")));
        assert!(core.may_read(&QualifiedName::new("public", "contacts")));
    }

    #[test]
    fn an_owns_pattern_matches_only_in_the_boundary_schema() {
        let mut tables = catalog();
        tables.push(CatalogTable::new("archive", "contacts"));
        let plan = plan(&config(CONFIG), &tables).unwrap();
        assert!(!names(&role(&plan, "core").own).contains(&"archive.contacts".to_owned()));
        // A bare exception pattern matches the name in any schema.
        assert!(names(&role(&plan, "marketing").read).contains(&"archive.contacts".to_owned()));
    }

    #[test]
    fn a_qualified_pattern_matches_one_schema() {
        let text = CONFIG.replace(
            r#"may_read = ["contacts"]"#,
            r#"may_read = ["public.contacts"]"#,
        );
        let mut tables = catalog();
        tables.push(CatalogTable::new("archive", "contacts"));
        let plan = plan(&config(&text), &tables).unwrap();
        assert_eq!(names(&role(&plan, "marketing").read), ["public.contacts"]);
    }

    #[test]
    fn the_most_specific_owns_pattern_wins() {
        let text = r#"
[boundaries.core]
owns = ["*"]
role = "core"

[boundaries.billing]
owns = ["invoice*"]
role = "billing"
"#;
        let tables = [
            CatalogTable::new("public", "invoices"),
            CatalogTable::new("public", "users"),
        ];
        let plan = plan(&config(text), &tables).unwrap();
        assert_eq!(names(&role(&plan, "billing").own), ["public.invoices"]);
        assert_eq!(names(&role(&plan, "core").own), ["public.users"]);
    }

    #[test]
    fn an_equal_owner_tie_is_an_error() {
        let text = r#"
[boundaries.a]
owns = ["t*"]
role = "a"

[boundaries.b]
owns = ["t*"]
role = "b"
"#;
        let err = plan(&config(text), &[CatalogTable::new("public", "tags")]).unwrap_err();
        assert!(matches!(
            err,
            GrantError::Boundary(BoundaryError::Ambiguous { .. })
        ));
    }

    #[test]
    fn two_boundaries_cannot_share_a_role() {
        let text = r#"
[boundaries.a]
owns = ["x"]
role = "app"

[boundaries.b]
owns = ["y"]
role = "app"
"#;
        let err = plan(&config(text), &[]).unwrap_err();
        assert_eq!(
            err,
            GrantError::SharedRole {
                role: "app".into(),
                first: "a".into(),
                second: "b".into()
            }
        );
    }

    #[test]
    fn a_boundary_with_no_role_gets_no_grants() {
        let text = CONFIG.replace("role = \"core\"\n", "");
        let plan = plan(&config(&text), &catalog()).unwrap();
        assert_eq!(plan.roles.len(), 1);
        assert_eq!(plan.roles[0].role, "marketing");
        // Core still owns its tables, so marketing cannot read `deals`.
        assert_eq!(names(&plan.roles[0].denied), ["public.deals"]);
    }

    #[test]
    fn no_role_gives_no_sql() {
        let text = CONFIG
            .replace("role = \"core\"\n", "")
            .replace("role = \"marketing\"\n", "");
        let plan = plan(&config(&text), &catalog()).unwrap();
        assert!(plan.roles.is_empty());
        assert_eq!(plan.sql(), None);
    }

    #[test]
    fn sql_revokes_first_then_grants_in_one_transaction() {
        let plan = plan(&config(CONFIG), &catalog()).unwrap();
        let sql = plan.sql().unwrap();
        let expected = r#"-- Generated by `autumn db grants` from boundaries.toml. Do not edit.
BEGIN;

-- Boundary `core`: role "core", schema "public".
REVOKE ALL ON ALL TABLES IN SCHEMA "marketing", "public" FROM "core";
REVOKE ALL ON ALL SEQUENCES IN SCHEMA "marketing", "public" FROM "core";
REVOKE ALL ON SCHEMA "marketing", "public" FROM "core";
GRANT USAGE ON SCHEMA "public" TO "core";
GRANT SELECT, INSERT, UPDATE, DELETE ON TABLE "public"."contacts", "public"."deals" TO "core";
GRANT USAGE, SELECT ON SEQUENCE "public"."contacts_id_seq" TO "core";

-- Boundary `marketing`: role "marketing", schema "marketing".
REVOKE ALL ON ALL TABLES IN SCHEMA "marketing", "public" FROM "marketing";
REVOKE ALL ON ALL SEQUENCES IN SCHEMA "marketing", "public" FROM "marketing";
REVOKE ALL ON SCHEMA "marketing", "public" FROM "marketing";
GRANT USAGE ON SCHEMA "marketing", "public" TO "marketing";
GRANT SELECT, INSERT, UPDATE, DELETE ON TABLE "marketing"."attribution_clicks", "marketing"."campaigns", "marketing"."drafts" TO "marketing";
GRANT SELECT ON TABLE "public"."contacts" TO "marketing";
GRANT SELECT, INSERT, UPDATE, DELETE ON TABLE "public"."autumn_jobs" TO "marketing";
GRANT USAGE, SELECT ON SEQUENCE "marketing"."campaigns_id_seq", "public"."autumn_jobs_id_seq" TO "marketing";
ALTER ROLE "marketing" SET search_path = "marketing", "public";

COMMIT;
"#;
        assert_eq!(sql, expected);
    }

    #[test]
    fn a_qualified_owns_pattern_gets_usage_on_its_schema() {
        let text = r#"
[boundaries.marketing]
owns = ["shared.contacts"]
schema = "marketing"
role = "marketing"
"#;
        let tables = [CatalogTable::new("shared", "contacts")];
        let plan = plan(&config(text), &tables).unwrap();
        assert_eq!(names(&role(&plan, "marketing").own), ["shared.contacts"]);
        let sql = plan.sql().unwrap();
        assert!(
            sql.contains(r#"GRANT USAGE ON SCHEMA "marketing", "shared" TO "marketing";"#),
            "{sql}"
        );
    }

    #[test]
    fn a_view_is_owned_only_through_a_pattern() {
        let mut tables = catalog();
        tables.push(CatalogTable::new("marketing", "deal_stats").as_view());
        tables.push(CatalogTable::new("marketing", "attribution_report").as_view());
        let plan = plan(&config(CONFIG), &tables).unwrap();
        let marketing = role(&plan, "marketing");
        assert!(names(&marketing.denied).contains(&"marketing.deal_stats".to_owned()));
        assert!(names(&marketing.own).contains(&"marketing.attribution_report".to_owned()));
    }

    #[test]
    fn only_schemas_with_tables_are_revoked_or_granted() {
        // `marketing` has no table yet: no statement names it.
        let tables = [CatalogTable::new("public", "contacts")];
        let sql = plan(&config(CONFIG), &tables).unwrap().sql().unwrap();
        assert!(!sql.contains(r#"SCHEMA "marketing""#), "{sql}");
        assert!(
            sql.contains(r#"REVOKE ALL ON SCHEMA "public" FROM "marketing";"#),
            "{sql}"
        );
        // The search_path still names it; a missing schema there is no error.
        assert!(
            sql.contains(r#"SET search_path = "marketing", "public";"#),
            "{sql}"
        );
    }

    #[test]
    fn schema_names_match_without_case() {
        let text = CONFIG.replace(r#"schema = "marketing""#, r#"schema = "Marketing""#);
        let plan = plan(&config(&text), &catalog()).unwrap();
        assert!(names(&role(&plan, "marketing").own).contains(&"marketing.drafts".to_owned()));
    }

    #[test]
    fn identifiers_are_quoted() {
        assert_eq!(quote_ident(r#"we"ird"#), r#""we""ird""#);
        let text = r#"
[boundaries.x]
owns = ["t"]
schema = "s\"; DROP TABLE t; --"
role = "r\""
"#;
        let plan = plan(
            &config(text),
            &[CatalogTable::new("s\"; DROP TABLE t; --", "t")],
        )
        .unwrap();
        let sql = plan.sql().unwrap();
        assert!(sql.contains(r#"FROM "r""";"#), "{sql}");
        assert!(sql.contains(r#""s""; DROP TABLE t; --""#), "{sql}");
        // A boundary name with a line break stays in its comment.
        let text = "[boundaries.\"x\\nDROP TABLE t;\"]\nowns = [\"t\"]\nrole = \"r\"\n";
        let sql = super::plan(&config(text), &[CatalogTable::new("public", "t")])
            .unwrap()
            .sql()
            .unwrap();
        assert!(!sql.lines().any(|l| l.starts_with("DROP")), "{sql}");
    }

    #[test]
    fn mistakes_are_warnings() {
        let text = CONFIG.replace(r#"may_read = ["contacts"]"#, r#"may_read = ["contact"]"#);
        let tables = [CatalogTable::new("public", "contacts")];
        let plan = plan(&config(&text), &tables).unwrap();
        assert!(
            plan.warnings
                .iter()
                .any(|w| w.contains("`marketing`") && w.contains("owns no table")),
            "{:?}",
            plan.warnings
        );
        assert!(
            plan.warnings
                .iter()
                .any(|w| w.contains("`contact`") && w.contains("matches no table")),
            "{:?}",
            plan.warnings
        );
    }

    #[test]
    fn an_empty_schema_or_role_is_a_parse_error() {
        for key in ["schema", "role"] {
            let text = format!("[boundaries.x]\nowns = [\"t\"]\n{key} = \" \"\n");
            let err = BoundaryConfig::parse(&text).unwrap_err();
            assert!(err.to_string().contains(key), "{err}");
        }
    }

    #[test]
    fn the_plan_is_sorted_and_stable() {
        let mut tables = catalog();
        tables.reverse();
        assert_eq!(
            plan(&config(CONFIG), &tables).unwrap(),
            plan(&config(CONFIG), &catalog()).unwrap()
        );
    }

    #[test]
    fn table_names_match_without_case() {
        let tables = [CatalogTable::new("public", "Contacts")];
        let plan = plan(&config(CONFIG), &tables).unwrap();
        assert_eq!(names(&role(&plan, "core").own), ["public.Contacts"]);
    }
}
