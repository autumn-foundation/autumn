//! A table name with its schema, and the one way to match a foreign key
//! target to a table.
//!
//! A name is `table` or `schema.table`. The schema `public` is the default
//! schema: `public.users` and `users` name the same default-schema table.
//!
//! A bare name names the one table of that name. The schema tools keep one
//! table name for all schemas (ADR 0047), so a bare name is never ambiguous.
//! A qualified name names the table of that name in that schema only.

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

    /// True when this name is `table`.
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

/// True when the foreign key targets `a` and `b` name the same table.
/// `tables` (keyed by table name) gives the schema of a bare target. A bare
/// target that is not in `tables` is in the default schema.
#[must_use]
pub fn same_fk_target(a: &str, b: &str, tables: &BTreeMap<&str, &Table>) -> bool {
    let key = |fk_table| {
        let target = TableRef::parse(fk_table);
        let schema = target.schema.map_or_else(
            || tables.get(target.name).and_then(|t| named_schema(t)),
            |schema| not_default(Some(schema)),
        );
        (schema, target.name)
    };
    key(a) == key(b)
}

/// True when `a` and `b` are the same foreign key: the same target table (see
/// [`same_fk_target`]), column and actions.
#[must_use]
pub fn same_foreign_key(a: &ForeignKey, b: &ForeignKey, tables: &BTreeMap<&str, &Table>) -> bool {
    a.column == b.column
        && a.on_delete == b.on_delete
        && a.on_update == b.on_update
        && same_fk_target(&a.table, &b.table, tables)
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
        assert!(same_fk_target("authors", "blog.authors", &tables));
        assert!(same_fk_target("users", "public.users", &tables));
        assert!(same_fk_target("posts", "public.posts", &tables));
        assert!(!same_fk_target("users", "auth.users", &tables));
        assert!(!same_fk_target("authors", "public.authors", &tables));
        assert!(!same_fk_target("users", "authors", &tables));
    }

    #[test]
    fn same_foreign_key_compares_the_column_and_the_actions_too() {
        let authors = table("authors", Some("blog"));
        let tables: BTreeMap<&str, &Table> = [("authors", &authors)].into();
        let bare = ForeignKey::new("authors", "id");
        assert!(same_foreign_key(
            &bare,
            &ForeignKey::new("blog.authors", "id"),
            &tables
        ));
        assert!(!same_foreign_key(
            &bare,
            &ForeignKey::new("authors", "code"),
            &tables
        ));
        let cascade = ForeignKey::new("blog.authors", "id")
            .with_on_delete(Some(autumn_schema_core::ForeignKeyAction::Cascade));
        assert!(!same_foreign_key(&bare, &cascade, &tables));
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
