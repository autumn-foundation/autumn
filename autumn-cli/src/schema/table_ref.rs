//! A table name and its schema. This module has the only functions that
//! match a foreign key target to a table.
//!
//! A name is `table` or `schema.table`. The schema `public` is the default
//! schema: `public.users` and `users` identify the same table.
//!
//! A qualified name identifies the table with that name in that schema only.
//! A name without a schema (a bare name) identifies the table with that name.
//! In the tables that the schema tools read, a table name can be in only one
//! schema (ADR 0047). Thus a bare name identifies one of these tables.

use std::collections::BTreeMap;

use autumn_schema_core::{ForeignKey, Table};

/// A table name, split into its schema and its name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TableRef<'a> {
    /// The schema, or `None` for a bare name.
    schema: Option<&'a str>,
    /// The table name.
    name: &'a str,
}

impl<'a> TableRef<'a> {
    /// Split `qualified` (`table` or `schema.table`).
    #[must_use]
    pub fn parse(qualified: &'a str) -> Self {
        qualified.rsplit_once('.').map_or(
            Self {
                schema: None,
                name: qualified,
            },
            |(schema, name)| Self {
                schema: Some(schema),
                name,
            },
        )
    }

    /// The schema as written, or `None` for a bare name.
    #[must_use]
    pub const fn schema(&self) -> Option<&'a str> {
        self.schema
    }

    /// The table name, without the schema.
    #[must_use]
    pub const fn name(&self) -> &'a str {
        self.name
    }

    /// Returns true when this name identifies `table`.
    #[must_use]
    pub fn names(&self, table: &Table) -> bool {
        self.name == table.name
            && self
                .schema
                .is_none_or(|schema| not_default(Some(schema)) == named_schema(table))
    }
}

/// The schema of `table` when it is not the default schema.
#[must_use]
pub fn named_schema(table: &Table) -> Option<&str> {
    not_default(table.schema.as_deref())
}

/// `schema`, or `None` when it is the default schema.
fn not_default(schema: Option<&str>) -> Option<&str> {
    schema.filter(|s| *s != "public")
}

/// True when `a` and `b` are in the same schema.
#[must_use]
pub fn same_schema(a: &Table, b: &Table) -> bool {
    named_schema(a) == named_schema(b)
}

/// True when the foreign key target `fk_table` points at `target`.
#[must_use]
pub fn fk_points_at(fk_table: &str, target: &Table) -> bool {
    TableRef::parse(fk_table).names(target)
}

/// The table in `tables` (keyed by table name) that the foreign key target
/// `fk_table` points at.
#[must_use]
pub fn fk_target_in<'t>(fk_table: &str, tables: &BTreeMap<&str, &'t Table>) -> Option<&'t Table> {
    let target = TableRef::parse(fk_table);
    tables
        .get(target.name())
        .copied()
        .filter(|t| target.names(t))
}

/// The tables on each side of a diff, by name. They give the schema of a bare
/// foreign key target on that side.
#[derive(Debug, Default)]
pub struct FkTables<'a> {
    /// The baseline tables.
    pub baseline: BTreeMap<&'a str, &'a Table>,
    /// The model tables, and the baseline tables that the model does not hold.
    pub desired: BTreeMap<&'a str, &'a Table>,
}

impl FkTables<'_> {
    /// Returns true when the baseline foreign key `base` and the model foreign
    /// key `want` are the same: the same target table, column and actions.
    #[must_use]
    pub fn same_foreign_key(&self, base: &ForeignKey, want: &ForeignKey) -> bool {
        base.column == want.column
            && base.on_delete == want.on_delete
            && base.on_update == want.on_update
            && same_fk_target(&base.table, &self.baseline, &want.table, &self.desired)
    }
}

/// Returns true when the foreign key targets `a` and `b` identify the same
/// table. `a_tables` and `b_tables` give the schema of a bare target. If a
/// bare target is not in its tables, the targets must be equal text.
#[must_use]
pub fn same_fk_target(
    a: &str,
    a_tables: &BTreeMap<&str, &Table>,
    b: &str,
    b_tables: &BTreeMap<&str, &Table>,
) -> bool {
    match (resolve(a, a_tables), resolve(b, b_tables)) {
        (Some(a), Some(b)) => a == b,
        _ => a == b,
    }
}

/// The schema (`None` for the default) and name of the table that `fk_table`
/// identifies. `None` when `fk_table` is bare and not in `tables`.
fn resolve<'a>(
    fk_table: &'a str,
    tables: &BTreeMap<&str, &'a Table>,
) -> Option<(Option<&'a str>, &'a str)> {
    let target = TableRef::parse(fk_table);
    target.schema.map_or_else(
        || {
            tables
                .get(target.name)
                .map(|t| (named_schema(t), target.name))
        },
        |schema| Some((not_default(Some(schema)), target.name)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use autumn_schema_core::Backend;

    fn table(name: &str, schema: Option<&str>) -> Table {
        let mut t = Table::new(name, Backend::Postgres);
        t.schema = schema.map(str::to_owned);
        t
    }

    #[test]
    fn parse_splits_on_the_last_dot() {
        let deep = TableRef::parse("db.blog.posts");
        assert_eq!((deep.schema(), deep.name()), (Some("db.blog"), "posts"));
        let r = TableRef::parse("blog.posts");
        assert_eq!((r.schema(), r.name()), (Some("blog"), "posts"));
        let bare = TableRef::parse("posts");
        assert_eq!((bare.schema(), bare.name()), (None, "posts"));
    }

    #[test]
    fn a_bare_target_points_at_the_table_of_that_name_in_any_schema() {
        assert!(fk_points_at("users", &table("users", None)));
        assert!(fk_points_at("users", &table("users", Some("blog"))));
        assert!(!fk_points_at("users", &table("accounts", None)));
    }

    #[test]
    fn a_qualified_target_points_only_at_its_schema() {
        assert!(fk_points_at("blog.users", &table("users", Some("blog"))));
        assert!(!fk_points_at("auth.users", &table("users", None)));
        assert!(!fk_points_at("auth.users", &table("users", Some("blog"))));
        assert!(!fk_points_at(
            "blog.users",
            &table("accounts", Some("blog"))
        ));
    }

    #[test]
    fn public_is_the_default_schema() {
        assert!(fk_points_at("public.users", &table("users", None)));
        assert!(fk_points_at(
            "public.users",
            &table("users", Some("public"))
        ));
        assert!(fk_points_at("users", &table("users", Some("public"))));
        assert!(!fk_points_at("public.users", &table("users", Some("blog"))));
        assert_eq!(named_schema(&table("users", Some("public"))), None);
        assert_eq!(named_schema(&table("users", Some("blog"))), Some("blog"));
    }

    #[test]
    fn same_schema_treats_public_as_the_default() {
        assert!(same_schema(&table("a", None), &table("b", Some("public"))));
        assert!(same_schema(
            &table("a", Some("blog")),
            &table("b", Some("blog"))
        ));
        assert!(!same_schema(&table("a", None), &table("b", Some("blog"))));
    }

    #[test]
    fn same_fk_target_resolves_a_bare_target_by_name() {
        let authors = table("authors", Some("blog"));
        let users = table("users", None);
        let tables: BTreeMap<&str, &Table> = [("authors", &authors), ("users", &users)].into();
        let same = |a, b| same_fk_target(a, &tables, b, &tables);
        assert!(same("authors", "blog.authors"));
        assert!(same("users", "public.users"));
        assert!(!same("users", "auth.users"));
        assert!(!same("authors", "public.authors"));
        assert!(!same("users", "authors"));
    }

    #[test]
    fn an_unknown_bare_target_matches_only_the_same_text() {
        let authors = table("authors", Some("blog"));
        let none = BTreeMap::new();
        let tables: BTreeMap<&str, &Table> = [("authors", &authors)].into();
        assert!(same_fk_target("posts", &none, "posts", &none));
        assert!(!same_fk_target("posts", &none, "public.posts", &none));
        // Each side reads its own tables.
        assert!(!same_fk_target("authors", &none, "blog.authors", &tables));
        assert!(same_fk_target("authors", &tables, "blog.authors", &none));
    }

    #[test]
    fn same_foreign_key_compares_the_column_and_the_actions_too() {
        let authors = table("authors", Some("blog"));
        let tables: BTreeMap<&str, &Table> = [("authors", &authors)].into();
        let both = FkTables {
            baseline: tables.clone(),
            desired: tables,
        };
        let bare = ForeignKey::new("authors", "id");
        assert!(both.same_foreign_key(&bare, &ForeignKey::new("blog.authors", "id")));
        assert!(!both.same_foreign_key(&bare, &ForeignKey::new("authors", "code")));
        let cascade = ForeignKey::new("blog.authors", "id")
            .with_on_delete(Some(autumn_schema_core::ForeignKeyAction::Cascade));
        assert!(!both.same_foreign_key(&bare, &cascade));
    }

    #[test]
    fn fk_target_in_finds_only_the_table_the_target_names() {
        let users = table("users", None);
        let authors = table("authors", Some("blog"));
        let tables: BTreeMap<&str, &Table> = [("users", &users), ("authors", &authors)].into();
        assert_eq!(
            fk_target_in("users", &tables).map(|t| &t.name),
            Some(&users.name)
        );
        assert_eq!(
            fk_target_in("blog.authors", &tables).map(|t| &t.name),
            Some(&authors.name)
        );
        assert!(fk_target_in("auth.users", &tables).is_none());
        assert!(fk_target_in("posts", &tables).is_none());
    }
}
