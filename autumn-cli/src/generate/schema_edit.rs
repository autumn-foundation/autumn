//! Idempotent edits to `src/schema.rs`, `src/main.rs`, and the various
//! `mod.rs` files that the generators have to touch.
//!
//! All functions here are pure string transformations — no I/O. The
//! generator decides how to use them; the [`emit`] module decides when to
//! write them out.
//!
//! [`emit`]: super::emit

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use sha2::{Digest, Sha256};

use autumn_web::config::DatabaseBackend;

use super::GenerateError;
use super::dsl::{EncryptedMode, Field, FieldConstraints, FieldKind, IdType};

/// Append a `pub mod <name>;` line to a `mod.rs` file, returning the new
/// contents. Idempotent: a second call with the same name is a no-op.
#[must_use]
pub fn add_mod_declaration(existing: &str, name: &str) -> String {
    let line = format!("pub mod {name};");
    if existing
        .lines()
        .any(|l| l.trim() == line || l.trim() == format!("mod {name};"))
    {
        return existing.to_owned();
    }
    if existing.is_empty() {
        return format!("{line}\n");
    }
    let trimmed = existing.trim_end();
    format!("{trimmed}\n{line}\n")
}

/// Inverse of [`add_mod_declaration`] (`autumn destroy`, issue #1048).
///
/// Removes the exact `pub mod <name>;` line [`add_mod_declaration`] would
/// have inserted. A no-op (returns `existing` unchanged) if that line isn't
/// present — either because it was already destroyed, or because the module
/// pre-existed as a bare `mod <name>;` that `add_mod_declaration` itself
/// never touches (and destroy must not touch either).
#[must_use]
pub fn remove_mod_declaration(existing: &str, name: &str) -> String {
    let line = format!("pub mod {name};");
    let lines: Vec<&str> = existing.lines().collect();
    let Some(idx) = lines.iter().position(|l| l.trim() == line) else {
        return existing.to_owned();
    };
    remove_single_line(&lines, idx, existing.ends_with('\n'))
}

/// Build a new `diesel::table!` block for the given table, emitting the `id`
/// column with the caller-supplied `id_type`.
// Retained as a Postgres-default convenience wrapper for the test suite; the
// backend-aware `schema_table_block_with_id_for` is what production calls.
#[cfg(test)]
#[must_use]
pub fn schema_table_block_with_id(table: &str, fields: &[Field], id_type: IdType) -> String {
    schema_table_block_with_id_for(DatabaseBackend::Postgres, table, fields, id_type)
}

/// `schema_table_block_with_id` for a specific database `backend` (`SQLite`
/// foundation, issue #1614). The Postgres path is byte-for-byte identical to
/// the historical output; the `SQLite` path uses the diesel sql-types that
/// diesel's `SQLite` backend actually implements (`Text` for `Uuid`/`Jsonb`,
/// `Binary` for `Bytea`, `Timestamp` for `Timestamptz`, …) via
/// [`super::dsl::Field::schema_type_for`].
#[must_use]
pub fn schema_table_block_with_id_for(
    backend: DatabaseBackend,
    table: &str,
    fields: &[Field],
    id_type: IdType,
) -> String {
    let mut out = String::with_capacity(fields.len() * 40 + 128);
    out.push_str("diesel::table! {\n");
    let _ = writeln!(out, "    {table} (id) {{");
    let _ = writeln!(out, "        id -> {},", id_type.schema_type_for(backend));
    for f in fields {
        let _ = writeln!(out, "        {} -> {},", f.name, f.schema_type_for(backend));
    }
    out.push_str("        created_at -> Timestamp,\n");
    out.push_str("    }\n");
    out.push_str("}\n");
    out
}

/// Append a new `diesel::table!` block to `src/schema.rs`. Idempotent: if a
/// block defining `table` already exists, returns `existing` unchanged.
///
/// This is the BigSerial-default wrapper. Use [`append_schema_table_with_id`]
/// when the caller needs to control the primary-key type.
#[must_use]
pub fn append_schema_table(existing: &str, table: &str, fields: &[Field]) -> String {
    append_schema_table_with_id(existing, table, fields, IdType::BigSerial)
}

/// Like [`append_schema_table`] but honours the caller-supplied `id_type`.
#[must_use]
pub fn append_schema_table_with_id(
    existing: &str,
    table: &str,
    fields: &[Field],
    id_type: IdType,
) -> String {
    append_schema_table_with_id_for(DatabaseBackend::Postgres, existing, table, fields, id_type)
}

/// [`append_schema_table_with_id`] for a specific database `backend` (issue
/// #1614). The Postgres path stays byte-for-byte identical.
#[must_use]
pub fn append_schema_table_with_id_for(
    backend: DatabaseBackend,
    existing: &str,
    table: &str,
    fields: &[Field],
    id_type: IdType,
) -> String {
    if has_table(existing, table) {
        return existing.to_owned();
    }
    let block = schema_table_block_with_id_for(backend, table, fields, id_type);
    if existing.is_empty() {
        return block;
    }
    let trimmed = existing.trim_end();
    format!("{trimmed}\n\n{block}")
}

/// True iff `existing` already contains a `<table> (...)` definition.
fn has_table(existing: &str, table: &str) -> bool {
    let needle = format!("{table} (");
    existing.lines().any(|l| l.trim().starts_with(&needle))
}

/// Inverse of [`append_schema_table_with_id`]/[`append_schema_table`]
/// (`autumn destroy`, issue #1048).
///
/// Removes the whole `diesel::table! { <table> (...) { ... } }` block for
/// `table`, plus the single blank separator line
/// [`append_schema_table_with_id`] inserts before it when appending to a
/// non-empty file — so removing the last table restores the file byte-for-
/// byte to whatever preceded it (including becoming empty, in which case the
/// caller deletes the file rather than leaving a blank `src/schema.rs`).
///
/// A no-op (returns `existing` unchanged) if `table` isn't declared, if the
/// block's shape doesn't match what `append_schema_table_with_id` would have
/// produced (hand-edited/malformed), or if the block's content isn't
/// byte-identical to `expected_block` (the literal text this generator
/// invocation would append for `table`) — the last check protects a
/// same-named table that pre-existed with different columns from ever being
/// destroyed (issue #1048 PR review): destroy never corrupts, nor deletes, a
/// table it didn't itself produce.
#[must_use]
pub fn remove_schema_table(existing: &str, table: &str, expected_block: &str) -> String {
    if !has_table(existing, table) {
        return existing.to_owned();
    }
    let lines: Vec<&str> = existing.lines().collect();
    let needle = format!("{table} (");
    let Some(table_line_idx) = lines.iter().position(|l| l.trim().starts_with(&needle)) else {
        return existing.to_owned();
    };
    if table_line_idx == 0 || lines[table_line_idx - 1].trim() != "diesel::table! {" {
        return existing.to_owned();
    }
    let open_idx = table_line_idx - 1;
    let Some(inner_close_offset) = lines[table_line_idx + 1..]
        .iter()
        .position(|l| l.trim() == "}")
    else {
        return existing.to_owned();
    };
    let inner_close_idx = table_line_idx + 1 + inner_close_offset;
    let Some(outer_close_line) = lines.get(inner_close_idx + 1) else {
        return existing.to_owned();
    };
    if outer_close_line.trim() != "}" {
        return existing.to_owned();
    }
    let outer_close_idx = inner_close_idx + 1;

    let found_block = format!("{}\n", lines[open_idx..=outer_close_idx].join("\n"));
    if found_block != expected_block {
        return existing.to_owned();
    }

    // Also consume one preceding blank separator line, if present.
    let mut start = open_idx;
    if start > 0 && lines[start - 1].trim().is_empty() {
        start -= 1;
    }

    let mut new_lines: Vec<&str> = Vec::with_capacity(lines.len());
    new_lines.extend_from_slice(&lines[..start]);
    if outer_close_idx + 1 < lines.len() {
        new_lines.extend_from_slice(&lines[outer_close_idx + 1..]);
    }
    let mut out = new_lines.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    out
}

/// Public predicate: whether `schema` already declares a `<table>` block.
///
/// Generators use this to detect a collision before emitting a migration that
/// would otherwise `CREATE TABLE` a name the project already owns.
#[must_use]
pub fn schema_has_table(schema: &str, table: &str) -> bool {
    has_table(schema, table)
}

/// The column names already declared for `table` in `schema` (`src/schema.rs`
/// as [`append_schema_table`] shapes it), or an empty `Vec` if `table` isn't
/// declared there at all.
///
/// Used by `add_columns_up_sql`/`remove_columns_down_sql` (issue #1032
/// review follow-up) to extend their unique-index collision check beyond the
/// columns being added/removed in the current `AddXToY`/`RemoveXFromY`
/// migration: a plain index on some *other*, already-existing column named
/// `<field>_unique` would otherwise collide with a newly-added `unique`
/// field's own index name with no way for `unique_index_name` to see it,
/// since it only ever receives the fields touched by one `generate`
/// invocation. This generator has no DB introspection, so `schema.rs` (which
/// every model/scaffold generator keeps in sync with the migrations it
/// writes) is the closest thing to a durable record of a table's existing
/// columns across separate `generate` invocations — only as reliable as
/// `schema.rs` staying in sync with the real database, the same assumption
/// every other generator here already makes of it.
fn existing_schema_columns(schema: &str, table: &str) -> Vec<String> {
    let needle = format!("{table} (");
    let Some(start) = schema.lines().position(|l| l.trim().starts_with(&needle)) else {
        return Vec::new();
    };
    schema
        .lines()
        .skip(start + 1)
        .take_while(|l| !l.trim().starts_with('}'))
        .filter_map(|l| l.trim().trim_end_matches(',').split_once(" -> "))
        .map(|(name, _)| name.to_owned())
        .collect()
}

/// Build the full SQL for `up.sql` of a `CREATE TABLE` migration with optional
/// defaults, plain (non-unique) `--index` columns, and `unique`-marked
/// columns (their own `CREATE UNIQUE INDEX`, see [`unique_index_sql`]),
/// honouring the caller-supplied `id_type`.
/// For `Uuid`, prepends a comment documenting the index-locality trade-off and
/// the `UUIDv7` upgrade path.
#[must_use]
pub fn create_table_sql_with_metadata_and_id(
    table: &str,
    fields: &[Field],
    indexes: &BTreeSet<String>,
    defaults: &BTreeMap<String, String>,
    id_type: IdType,
) -> String {
    create_table_sql_with_metadata_and_id_for(
        DatabaseBackend::Postgres,
        table,
        fields,
        indexes,
        defaults,
        id_type,
    )
}

/// [`create_table_sql_with_metadata_and_id`] for a specific database `backend`
/// (`SQLite` foundation, issue #1614).
///
/// The Postgres path is byte-for-byte identical to the historical output. The
/// `SQLite` path swaps in `SQLite`-valid column types (via
/// [`super::dsl::Field::sql_column_type_for`] and
/// [`super::dsl::IdType::pk_sql_for`]) and the `SQLite` `created_at` default
/// (`TEXT ... DEFAULT CURRENT_TIMESTAMP` rather than Postgres's `TIMESTAMP ...
/// DEFAULT NOW()`, which `SQLite` lacks). `CHECK` constraints, `REFERENCES`, and
/// `CREATE INDEX` are portable and unchanged. Every DSL field kind maps to a
/// working `SQLite` column type, so nothing is rejected at generate time here
/// (see [`super::dsl::FieldKind::sqlite_sql_type`]).
#[must_use]
pub fn create_table_sql_with_metadata_and_id_for(
    backend: DatabaseBackend,
    table: &str,
    fields: &[Field],
    indexes: &BTreeSet<String>,
    defaults: &BTreeMap<String, String>,
    id_type: IdType,
) -> String {
    let mut sql = String::with_capacity(fields.len() * 64 + indexes.len() * 96 + 256);
    if let Some(comment) = id_type.migration_comment_for(backend) {
        sql.push_str(comment);
        sql.push('\n');
    }
    if let Some(comment) = encrypted_columns_comment(fields) {
        sql.push_str(&comment);
    }
    let _ = writeln!(sql, "CREATE TABLE {table} (");
    let _ = write!(sql, "    id {}", id_type.pk_sql_for(backend));
    for f in fields {
        sql.push_str(",\n");
        let _ = write!(
            sql,
            "    {} {} {}",
            f.name,
            f.sql_column_type_for(backend),
            f.sql_nullability()
        );
        if let Some(target) = f.reference_table() {
            let _ = write!(sql, " REFERENCES {target}(id)");
        }
        // An explicit `--default` wins; otherwise a field kind that carries its
        // own storage default supplies one. Today that is `{translatable}`
        // (#1384): the per-locale container column is `NOT NULL` and starts as
        // the empty JSON object `'{}'`.
        if let Some(default) = defaults
            .get(&f.name)
            .map(String::as_str)
            .or_else(|| f.sql_default())
        {
            let _ = write!(sql, " DEFAULT {default}");
        }
        if let Some(check) = column_check_suffix(f, backend) {
            let _ = write!(sql, " {check}");
        }
    }
    // Postgres uses `TIMESTAMP ... DEFAULT NOW()`; SQLite has neither a
    // timestamp type nor `NOW()`, so store ISO-8601 text defaulted to
    // `CURRENT_TIMESTAMP`.
    match backend {
        DatabaseBackend::Postgres => {
            sql.push_str(",\n    created_at TIMESTAMP NOT NULL DEFAULT NOW()\n);\n");
        }
        DatabaseBackend::Sqlite => {
            sql.push_str(",\n    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP\n);\n");
        }
    }
    // Every `references` field gets an index automatically (Rails' `add_reference`
    // behaviour), in addition to any explicit `--index` fields. Merging into the
    // same sorted set keeps `CREATE INDEX` output deterministic and de-duplicates
    // a reference field that was *also* passed via `--index`.
    let unique_fields: BTreeSet<&str> = fields
        .iter()
        .filter(|f| f.unique)
        .map(|f| f.name.as_str())
        .collect();
    // A `position` field (issue #1358) is never `:unique` (rejected at parse
    // time) and gets its own composite/plain index below, not the generic
    // single-column loop — excluded here the same way `unique_fields` is.
    let position_fields: BTreeSet<&str> = fields
        .iter()
        .filter(|f| f.kind.is_position())
        .map(|f| f.name.as_str())
        .collect();
    let mut index_fields = indexes.clone();
    for f in fields {
        if f.kind.is_reference() {
            index_fields.insert(f.name.clone());
        }
    }
    // A `unique` field's own `CREATE UNIQUE INDEX` (emitted below) already
    // covers lookups on that column, so an explicit `--index` on the same
    // field (or an auto-added `references` index, though `unique` +
    // `references` together is an unusual combination) must not also emit a
    // redundant plain index (issue #1032).
    index_fields.retain(|name| {
        !unique_fields.contains(name.as_str()) && !position_fields.contains(name.as_str())
    });
    for field_name in &index_fields {
        let _ = writeln!(
            sql,
            "CREATE INDEX idx_{table}_{field_name} ON {table} ({field_name});"
        );
    }
    // `unique` fields get their own named `CREATE UNIQUE INDEX`.
    for field_name in &unique_fields {
        sql.push_str(&unique_index_sql(table, field_name, fields));
    }
    // A `position` field gets an index automatically (issue #1358): scans
    // ordering by it (the scaffold index view, `move_*` neighbor lookups) are
    // the entire point of the column. When scoped (`{scope:col}`), the index
    // is composite `(scope, position)` — every real query filters by scope
    // first — rather than a single-column index on `position` alone.
    for f in fields {
        if f.kind.is_position() {
            match f.constraints.scope.as_deref() {
                Some(scope) => {
                    let _ = writeln!(
                        sql,
                        "CREATE INDEX idx_{table}_{scope}_{position} ON {table} ({scope}, {position});",
                        position = f.name
                    );
                }
                None => {
                    let _ = writeln!(
                        sql,
                        "CREATE INDEX idx_{table}_{position} ON {table} ({position});",
                        position = f.name
                    );
                }
            }
        }
    }
    sql
}

/// Backend-aware `up.sql` triggers that maintain a `position` field's
/// contiguous `0..len-1` ordering (issue #1358): assign the next value on
/// insert, compact the remaining rows' positions on delete — hard delete
/// always, plus soft-delete (a `deleted_at` transition from `NULL` to
/// non-`NULL`) when `fields` declares a `deleted_at` column (the `--soft-delete`
/// virtual field `generate::model` appends — see `append_soft_delete_field`)
/// — append a RESTORED row (the reverse `deleted_at` transition, reachable
/// from the scaffold's Trash page) to the end of its live sequence — and,
/// for a scoped position field, compact the old scope and re-append at the
/// end of the new one when an ordinary `UPDATE` reassigns the scope FK
/// (e.g. dragging a Kanban card to a different board).
///
/// Implemented as database triggers rather than application-level repository
/// hooks so the invariant holds for **every** insert/delete path (the
/// generated repository, raw SQL, an admin panel, a seed script) — not just
/// the one Rust code path that happens to run it. The column's migration
/// `DEFAULT 0` (see `generate::model`'s auto-inserted default) is a
/// placeholder only: `Postgres` corrects it before the row is ever written
/// (`BEFORE INSERT`, mutating `NEW` directly — cheaper than a follow-up
/// `UPDATE`); `SQLite` triggers cannot mutate `NEW`, so its `AFTER INSERT`
/// trigger corrects the just-inserted row with a single `UPDATE ... WHERE
/// id = new.id`, still inside the same statement/transaction, so the
/// placeholder is never visible outside it. The `restore` trigger mirrors
/// this exactly (same advisory lock, same append-at-the-end `MAX + 1`
/// read), just keyed off the `deleted_at` transition instead of `INSERT`.
///
/// Compaction only shifts still-live rows (`deleted_at IS NULL` on the
/// soft-delete branch) — a restored row does not get its old position
/// back (some other, still-live row may since have taken it); it is
/// appended fresh at the end of its scope's live sequence instead, like
/// any other row re-entering the live set.
///
/// Returns an empty string when `fields` has no `position` column (the
/// common case), so a model without one gets byte-identical migration output.
///
/// Known limitation (Codex review, issue #1358): the function/trigger names
/// this emits (`{table}_{position}_assign`, `_compact`, `_compact_soft`,
/// `_rescope`, each with a `_trg` trigger-name counterpart) are NOT given
/// [`unique_index_name`]'s truncate-and-hash treatment for Postgres's
/// 63-byte (`NAMEDATALEN - 1`) identifier limit — a `table`/`position` pair
/// long enough to make two of these names collide on truncation would fail
/// the migration with a duplicate-object error (or, for the two trigger
/// names, install one that shadows the other on the same table) rather
/// than a clear "name too long" message. Applying the same treatment here
/// would need the identical scheme reproduced bit-for-bit in
/// `autumn-macros`' `position_impl_methods`, whose `move_to` embeds the
/// SAME `{table}_{position}_assign` string as its `pg_advisory_xact_lock`
/// key and must keep contending on the identical lock Postgres actually
/// stored — a cross-crate synchronization this generator does not
/// currently attempt for any other identifier. In practice this requires a
/// `table`+`position` combined length in the high 40s of bytes, well past
/// typical naming; out of scope for this slice.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn position_triggers_up_sql_for(
    backend: DatabaseBackend,
    table: &str,
    fields: &[Field],
) -> String {
    let has_soft_delete = fields.iter().any(|f| f.name == "deleted_at");
    let mut out = String::new();
    for f in fields {
        if !f.kind.is_position() {
            continue;
        }
        let position = &f.name;
        let scope = f.constraints.scope.as_deref();
        match backend {
            DatabaseBackend::Postgres => {
                let scope_cond_new =
                    scope.map_or_else(|| "TRUE".to_owned(), |s| format!("\"{s}\" = NEW.\"{s}\""));
                let scope_cond_old =
                    scope.map_or_else(|| "TRUE".to_owned(), |s| format!("\"{s}\" = OLD.\"{s}\""));
                // The insert-assign trigger's `MAX(position)` scan must skip
                // soft-deleted rows: a soft-deleted row's position is stale
                // (excluded from the live compaction that ran when it was
                // deleted — see `compact_soft` below), so counting it here
                // would inflate the next assignment and leave a gap in the
                // live sequence the very first time a live insert follows a
                // soft delete.
                let live_cond_new = if has_soft_delete {
                    format!("{scope_cond_new} AND deleted_at IS NULL")
                } else {
                    scope_cond_new.clone()
                };
                // A transaction-scoped advisory lock keyed by table and scope, with
                // a constant second key when unscoped so every insert into the table
                // serializes against every other. Without it, two concurrent `BEFORE
                // INSERT`s under READ COMMITTED can both read the same
                // `MAX(position)` before either commits and be assigned the same
                // value. There is no UNIQUE constraint on `(scope, position)` to
                // catch it: positions are only ever maintained uniquely, by these
                // triggers and `move_to`'s own locking, and nothing at the schema
                // level enforces it. `pg_advisory_xact_lock` auto-releases at commit
                // or rollback, so it composes with the rest of the inserting
                // transaction with no separate unlock statement.
                let lock_key2 = scope.map_or_else(
                    || "0".to_owned(),
                    |s| format!("hashtext(NEW.\"{s}\"::text)"),
                );
                let _ = writeln!(
                    out,
                    "CREATE FUNCTION {table}_{position}_assign() RETURNS TRIGGER AS $$\n\
                     BEGIN\n  \
                     PERFORM pg_advisory_xact_lock(hashtext('{table}_{position}_assign'), {lock_key2});\n  \
                     NEW.\"{position}\" := COALESCE((SELECT MAX(\"{position}\") + 1 FROM \"{table}\" WHERE {live_cond_new}), 0);\n  \
                     RETURN NEW;\n\
                     END;\n\
                     $$ LANGUAGE plpgsql;"
                );
                let _ = writeln!(
                    out,
                    "CREATE TRIGGER {table}_{position}_assign_trg BEFORE INSERT ON \"{table}\" \
                     FOR EACH ROW EXECUTE FUNCTION {table}_{position}_assign();"
                );
                // The same advisory lock key as the assign trigger, keyed by
                // `OLD`'s scope value, unchanged from `NEW`'s for a row that is
                // not itself being re-scoped. Without it, a concurrent insert's
                // `SELECT MAX(position)` — a plain read, not row-locked — can run
                // against a snapshot taken before this compaction's shift commits,
                // computing a next-position that leaves a gap where the compacted
                // range used to end. The shared lock makes insert and
                // delete-compaction on one scope serialize fully.
                let lock_key2_old = scope.map_or_else(
                    || "0".to_owned(),
                    |s| format!("hashtext(OLD.\"{s}\"::text)"),
                );
                // On a `--soft-delete` model this trigger fires only from `purge`,
                // and every row it hard-deletes was already soft-deleted — the
                // scaffold's purge handler reaches only rows filtered to
                // `deleted_at IS NOT NULL`, whose position `compact_soft` already
                // excluded from the live sequence at soft-delete time. Running this
                // compaction unconditionally would shift the live rows a second time
                // for the same removal, producing a duplicate live position. Skip
                // entirely when `OLD.deleted_at` is set. On a non-soft-delete model
                // there is no such column and this is the only compaction path, so
                // it always runs.
                let (compact_guard_open, compact_guard_close) = if has_soft_delete {
                    ("IF OLD.deleted_at IS NULL THEN\n    ", "\n  END IF;")
                } else {
                    ("", "")
                };
                let _ = writeln!(
                    out,
                    "CREATE FUNCTION {table}_{position}_compact() RETURNS TRIGGER AS $$\n\
                     BEGIN\n  \
                     {compact_guard_open}PERFORM pg_advisory_xact_lock(hashtext('{table}_{position}_assign'), {lock_key2_old});\n    \
                     UPDATE \"{table}\" SET \"{position}\" = \"{position}\" - 1 WHERE {scope_cond_old} AND \"{position}\" > OLD.\"{position}\";{compact_guard_close}\n  \
                     RETURN OLD;\n\
                     END;\n\
                     $$ LANGUAGE plpgsql;"
                );
                let _ = writeln!(
                    out,
                    "CREATE TRIGGER {table}_{position}_compact_trg AFTER DELETE ON \"{table}\" \
                     FOR EACH ROW EXECUTE FUNCTION {table}_{position}_compact();"
                );
                if has_soft_delete {
                    let _ = writeln!(
                        out,
                        "CREATE FUNCTION {table}_{position}_compact_soft() RETURNS TRIGGER AS $$\n\
                         BEGIN\n  \
                         IF OLD.deleted_at IS NULL AND NEW.deleted_at IS NOT NULL THEN\n    \
                         PERFORM pg_advisory_xact_lock(hashtext('{table}_{position}_assign'), {lock_key2_old});\n    \
                         UPDATE \"{table}\" SET \"{position}\" = \"{position}\" - 1 WHERE {scope_cond_old} AND \"{position}\" > OLD.\"{position}\" AND deleted_at IS NULL;\n  \
                         END IF;\n  \
                         RETURN NEW;\n\
                         END;\n\
                         $$ LANGUAGE plpgsql;"
                    );
                    let _ = writeln!(
                        out,
                        "CREATE TRIGGER {table}_{position}_compact_soft_trg AFTER UPDATE OF deleted_at ON \"{table}\" \
                         FOR EACH ROW EXECUTE FUNCTION {table}_{position}_compact_soft();"
                    );
                    // `compact_soft` handles only the soft-delete transition,
                    // `deleted_at` NULL to non-NULL. The generated repository's
                    // `restore()`, reachable from the scaffold's Trash page, performs
                    // the opposite transition, and without a trigger of its own the
                    // restored row re-enters the live set still carrying whatever
                    // stale position it had when soft-deleted — a position some other
                    // still-live row may since have taken, producing a duplicate.
                    // This mirrors the insert-assign trigger exactly: same advisory
                    // lock key, same append-at-the-end `MAX(position) + 1` read, now
                    // keyed off `NEW`'s scope since a restore never changes scope. It
                    // does not try to recreate the row's old position, which this
                    // slice does not preserve across a soft-delete and restore.
                    let _ = writeln!(
                        out,
                        "CREATE FUNCTION {table}_{position}_restore() RETURNS TRIGGER AS $$\n\
                         BEGIN\n  \
                         PERFORM pg_advisory_xact_lock(hashtext('{table}_{position}_assign'), {lock_key2});\n  \
                         NEW.\"{position}\" := COALESCE((SELECT MAX(\"{position}\") + 1 FROM \"{table}\" WHERE {live_cond_new}), 0);\n  \
                         RETURN NEW;\n\
                         END;\n\
                         $$ LANGUAGE plpgsql;"
                    );
                    let _ = writeln!(
                        out,
                        "CREATE TRIGGER {table}_{position}_restore_trg BEFORE UPDATE OF deleted_at ON \"{table}\" \
                         FOR EACH ROW WHEN (OLD.deleted_at IS NOT NULL AND NEW.deleted_at IS NULL) \
                         EXECUTE FUNCTION {table}_{position}_restore();"
                    );
                }
                // #1358 review: an ordinary `UPDATE` can reassign a scoped row's
                // scope FK — dragging a Kanban card to a different board via
                // `board_id` — since nothing about `position` makes that column
                // immutable. Without this trigger the row keeps its old position,
                // leaving a gap in the old scope and usually a duplicate in the new
                // one. `BEFORE UPDATE`, not `AFTER`, because only a `BEFORE` trigger
                // can set `NEW`'s position; the compaction UPDATE and the
                // append-to-new-scope assignment both run inside the same function
                // and statement, so a hard crash mid-trigger cannot leave one done
                // without the other. It locks both the old and new scope's advisory
                // key, always in ascending-hash order, mirroring `move_to`'s fixed
                // id-ascending row-lock order, so two rows swapping scopes
                // concurrently cannot deadlock. A rescope racing a plain insert,
                // delete, or move_to on either scope is still safe: same lock key,
                // and Postgres's deadlock detector aborts one side of any residual
                // cycle rather than corrupting data. Skipped for soft-deleted rows on
                // either side of the change, where `compact_soft` and restore own the
                // transition, and for unscoped position fields, which have no scope
                // column to reassign.
                if let Some(scope_col) = scope {
                    let rescope_when = if has_soft_delete {
                        format!(
                            "NEW.\"{scope_col}\" IS DISTINCT FROM OLD.\"{scope_col}\" AND \
                             OLD.deleted_at IS NULL AND NEW.deleted_at IS NULL"
                        )
                    } else {
                        format!("NEW.\"{scope_col}\" IS DISTINCT FROM OLD.\"{scope_col}\"")
                    };
                    let _ = writeln!(
                        out,
                        "CREATE FUNCTION {table}_{position}_rescope() RETURNS TRIGGER AS $$\n\
                         BEGIN\n  \
                         IF hashtext(OLD.\"{scope_col}\"::text) <= hashtext(NEW.\"{scope_col}\"::text) THEN\n    \
                         PERFORM pg_advisory_xact_lock(hashtext('{table}_{position}_assign'), hashtext(OLD.\"{scope_col}\"::text));\n    \
                         PERFORM pg_advisory_xact_lock(hashtext('{table}_{position}_assign'), hashtext(NEW.\"{scope_col}\"::text));\n  \
                         ELSE\n    \
                         PERFORM pg_advisory_xact_lock(hashtext('{table}_{position}_assign'), hashtext(NEW.\"{scope_col}\"::text));\n    \
                         PERFORM pg_advisory_xact_lock(hashtext('{table}_{position}_assign'), hashtext(OLD.\"{scope_col}\"::text));\n  \
                         END IF;\n  \
                         UPDATE \"{table}\" SET \"{position}\" = \"{position}\" - 1 WHERE {scope_cond_old} AND \"{position}\" > OLD.\"{position}\";\n  \
                         NEW.\"{position}\" := COALESCE((SELECT MAX(\"{position}\") + 1 FROM \"{table}\" WHERE {live_cond_new}), 0);\n  \
                         RETURN NEW;\n\
                         END;\n\
                         $$ LANGUAGE plpgsql;"
                    );
                    let _ = writeln!(
                        out,
                        "CREATE TRIGGER {table}_{position}_rescope_trg BEFORE UPDATE OF \"{scope_col}\" ON \"{table}\" \
                         FOR EACH ROW WHEN ({rescope_when}) EXECUTE FUNCTION {table}_{position}_rescope();"
                    );
                }
            }
            DatabaseBackend::Sqlite => {
                let scope_cond_new =
                    scope.map_or_else(|| "1=1".to_owned(), |s| format!("\"{s}\" = new.\"{s}\""));
                let scope_cond_old =
                    scope.map_or_else(|| "1=1".to_owned(), |s| format!("\"{s}\" = old.\"{s}\""));
                // Same reasoning as the Postgres arm: skip soft-deleted rows
                // when computing the next position, or a soft delete
                // followed by a live insert leaves a gap in the live
                // sequence.
                let live_cond_new = if has_soft_delete {
                    format!("{scope_cond_new} AND deleted_at IS NULL")
                } else {
                    scope_cond_new.clone()
                };
                let _ = writeln!(
                    out,
                    "CREATE TRIGGER \"{table}_{position}_assign\" AFTER INSERT ON \"{table}\" BEGIN\n  \
                     UPDATE \"{table}\" SET \"{position}\" = (SELECT COALESCE(MAX(\"{position}\"), -1) + 1 FROM \"{table}\" WHERE {live_cond_new} AND id != new.id) WHERE id = new.id;\n\
                     END;"
                );
                // Same reasoning as the Postgres arm: on a `--soft-delete`
                // model this only ever fires from `purge`, whose target was
                // already soft-deleted and already compacted out of the live
                // sequence by `compact_soft` — running this unconditionally
                // would shift the live rows a second time for the same
                // removal. SQLite triggers support a `WHEN` clause directly
                // (unlike Postgres, no `IF`/`END IF` needed inside the body).
                let compact_when = if has_soft_delete {
                    " WHEN old.deleted_at IS NULL"
                } else {
                    ""
                };
                let _ = writeln!(
                    out,
                    "CREATE TRIGGER \"{table}_{position}_compact\" AFTER DELETE ON \"{table}\"{compact_when} BEGIN\n  \
                     UPDATE \"{table}\" SET \"{position}\" = \"{position}\" - 1 WHERE {scope_cond_old} AND \"{position}\" > old.\"{position}\";\n\
                     END;"
                );
                if has_soft_delete {
                    let _ = writeln!(
                        out,
                        "CREATE TRIGGER \"{table}_{position}_compact_soft\" AFTER UPDATE OF deleted_at ON \"{table}\" \
                         WHEN old.deleted_at IS NULL AND new.deleted_at IS NOT NULL BEGIN\n  \
                         UPDATE \"{table}\" SET \"{position}\" = \"{position}\" - 1 WHERE {scope_cond_old} AND \"{position}\" > old.\"{position}\" AND deleted_at IS NULL;\n\
                         END;"
                    );
                    // Same reasoning as the Postgres `restore` trigger above:
                    // a restored row must be appended to the end of its
                    // (unchanged) scope's live sequence, mirroring the
                    // insert-assign trigger's own AFTER-the-fact correction
                    // (SQLite can't mutate NEW directly either way).
                    let _ = writeln!(
                        out,
                        "CREATE TRIGGER \"{table}_{position}_restore\" AFTER UPDATE OF deleted_at ON \"{table}\" \
                         WHEN old.deleted_at IS NOT NULL AND new.deleted_at IS NULL BEGIN\n  \
                         UPDATE \"{table}\" SET \"{position}\" = (SELECT COALESCE(MAX(\"{position}\"), -1) + 1 FROM \"{table}\" WHERE {live_cond_new} AND id != new.id) WHERE id = new.id;\n\
                         END;"
                    );
                }
                // Same reasoning as the Postgres `rescope` trigger above: an ordinary
                // `UPDATE` reassigning a scoped row's scope FK must compact the old
                // scope's gap and append the row to the end of the new scope, or the
                // contiguous invariant breaks on a common "move card to another
                // board" operation. SQLite cannot mutate `NEW` in a `BEFORE` trigger,
                // so this runs `AFTER UPDATE` and corrects the already-written row
                // with a follow-up `UPDATE ... WHERE id = new.id`, mirroring the
                // `_assign` trigger's own after-the-fact correction. No locking is
                // needed: SQLite has none, and write-write correctness rests on
                // `scoped_immediate_transaction`'s `BEGIN IMMEDIATE`, as it does for
                // every other position trigger here.
                if let Some(scope_col) = scope {
                    let rescope_when = if has_soft_delete {
                        format!(
                            "old.\"{scope_col}\" IS NOT new.\"{scope_col}\" AND \
                             old.deleted_at IS NULL AND new.deleted_at IS NULL"
                        )
                    } else {
                        format!("old.\"{scope_col}\" IS NOT new.\"{scope_col}\"")
                    };
                    let _ = writeln!(
                        out,
                        "CREATE TRIGGER \"{table}_{position}_rescope\" AFTER UPDATE OF \"{scope_col}\" ON \"{table}\" \
                         WHEN {rescope_when} BEGIN\n  \
                         UPDATE \"{table}\" SET \"{position}\" = \"{position}\" - 1 WHERE {scope_cond_old} AND \"{position}\" > old.\"{position}\";\n  \
                         UPDATE \"{table}\" SET \"{position}\" = (SELECT COALESCE(MAX(\"{position}\"), -1) + 1 FROM \"{table}\" WHERE {live_cond_new} AND id != new.id) WHERE id = new.id;\n\
                         END;"
                    );
                }
            }
        }
    }
    out
}

/// `down.sql` companion to [`position_triggers_up_sql_for`].
///
/// `SQLite` triggers are dropped automatically when their table is dropped,
/// so this is a no-op there — matching [`sqlite_add_search_down_sql`]'s
/// analogous table-owned-object handling. `Postgres` triggers are also
/// dropped automatically with the table, but their backing `FUNCTION`
/// objects are standalone and must be dropped explicitly — `CASCADE` so
/// this is safe to run before or after the table drop regardless of
/// ordering.
#[must_use]
pub fn position_triggers_down_sql_for(
    backend: DatabaseBackend,
    table: &str,
    fields: &[Field],
) -> String {
    let has_soft_delete = fields.iter().any(|f| f.name == "deleted_at");
    let mut out = String::new();
    if backend != DatabaseBackend::Postgres {
        return out;
    }
    for f in fields {
        if !f.kind.is_position() {
            continue;
        }
        let position = &f.name;
        let _ = writeln!(
            out,
            "DROP FUNCTION IF EXISTS {table}_{position}_assign() CASCADE;"
        );
        let _ = writeln!(
            out,
            "DROP FUNCTION IF EXISTS {table}_{position}_compact() CASCADE;"
        );
        if has_soft_delete {
            let _ = writeln!(
                out,
                "DROP FUNCTION IF EXISTS {table}_{position}_compact_soft() CASCADE;"
            );
            let _ = writeln!(
                out,
                "DROP FUNCTION IF EXISTS {table}_{position}_restore() CASCADE;"
            );
        }
        if f.constraints.scope.is_some() {
            let _ = writeln!(
                out,
                "DROP FUNCTION IF EXISTS {table}_{position}_rescope() CASCADE;"
            );
        }
    }
    out
}

/// A leading comment block naming this table's `{encrypted}` columns (issue
/// #1340), or `None` when the model declares none — so an unencrypted
/// migration stays byte-for-byte identical.
///
/// The columns are already `TEXT` (every DSL kind that accepts `{encrypted}` is
/// a text column), which is exactly the point worth writing down: the stored
/// value is a base64 AES-256-GCM envelope, always larger than the plaintext, so
/// the column must stay **unbounded**. Anyone later tempted to "tighten" it to
/// a `VARCHAR(n)` sized for the plaintext would silently break writes once the
/// envelope overflows — the same trap the `Encrypt<Col>On<Table>` migration
/// warns about from the other direction (see
/// [`write_widen_bounded_columns_note`]).
fn encrypted_columns_comment(fields: &[Field]) -> Option<String> {
    let encrypted: Vec<&Field> = fields.iter().filter(|f| f.is_encrypted()).collect();
    if encrypted.is_empty() {
        return None;
    }
    let mut out = String::with_capacity(encrypted.len() * 80 + 320);
    let _ = writeln!(
        out,
        "-- At-rest encrypted column(s) (#805). The value stored here is a base64"
    );
    let _ = writeln!(
        out,
        "-- AES-256-GCM envelope (20-byte header + 16-byte tag, then base64 at ~1.37x),"
    );
    let _ = writeln!(
        out,
        "-- never the plaintext — so the column is unbounded TEXT, sized for the"
    );
    let _ = writeln!(
        out,
        "-- envelope rather than the plaintext. Do NOT narrow it to a bounded"
    );
    let _ = writeln!(
        out,
        "-- VARCHAR(n): the envelope will overflow a plaintext-sized limit."
    );
    for f in encrypted {
        let mode = match f.encrypted_mode() {
            Some(EncryptedMode::Deterministic) => {
                "deterministic (stable ciphertext; equality lookups work, equality leaks)"
            }
            _ => "randomized (fresh nonce per write; no equality lookups)",
        };
        let _ = writeln!(out, "--   {}: {mode}", f.name);
    }
    let _ = writeln!(
        out,
        "-- Key material lives in the credentials store; see \
         docs/guide/attribute-encryption.md."
    );
    Some(out)
}

/// `down.sql` companion to [`create_table_sql_with_metadata_and_id`].
#[must_use]
pub fn drop_table_sql(table: &str) -> String {
    format!("DROP TABLE {table};\n")
}

/// `PostgreSQL` silently truncates identifiers past `NAMEDATALEN - 1` bytes
/// (63 in a stock build) rather than erroring, so an unbounded
/// `idx_<table>_<field>_unique` can name-collide with what Postgres
/// actually stores once `table`/`field` are long enough. This is the one
/// place that name is computed — [`unique_index_sql`] and every generated
/// caller that needs to match a real constraint name at runtime
/// ([`super::scaffold`]'s `UNIQUE_CONSTRAINTS` const and its duplicate-
/// violation smoke test) all call through here so they stay byte-for-byte
/// in agreement with what Postgres will actually name the index.
const POSTGRES_MAX_IDENTIFIER_LEN: usize = 63;

/// The `CREATE UNIQUE INDEX` name for `table`/`field` (issue #1032),
/// distinct from the plain, non-unique `--index`/`references`-auto-index
/// output (`idx_<table>_<field>`, no `_unique` suffix) so a field that is
/// both `--index`ed and `unique` doesn't collide on the index name.
///
/// `fields` is the full field list `field` belongs to (not just the unique
/// ones) — passed so this can also detect the *coincidental-naming* case: a
/// plain index always names itself after its own column
/// (`idx_<table>_<other_field>`), so if some *other* field in the same
/// table happens to be named `<field>_unique`, its plain index would
/// collide with this one's unique index even though neither field's
/// `unique`-ness is otherwise related to the other. Every caller that
/// computes this name — the migration SQL and every generated caller that
/// needs to match a real constraint name at runtime ([`super::scaffold`]'s
/// `UNIQUE_CONSTRAINTS` const and its duplicate-violation smoke test) —
/// passes the same field list so they agree byte-for-byte on the same
/// (possibly disambiguated) name Postgres will actually store.
///
/// When `idx_<table>_<field>_unique` neither exceeds Postgres's identifier
/// limit nor collides with another field's plain-index name, it's used
/// verbatim. Otherwise, a `_`-prefixed 8-hex-char digest of the full
/// (untruncated) name is appended — truncating first if it's also too long
/// — so two names that would otherwise collide (on truncation or on a
/// coincidental match) don't collide with each other either.
///
/// Known limitation: the coincidental-naming check above compares literal
/// field names, not what Postgres actually ends up storing. A plain index's
/// own name (`idx_<table>_<other_field>`) is never truncated or
/// disambiguated by this generator the way a unique index's is — so a
/// sufficiently long *other* field's plain index can itself be silently
/// truncated by Postgres to 63 bytes and collide with this field's unique
/// index even when their un-truncated names don't literally match (e.g. a
/// unique field whose name makes `idx_<table>_<field>_unique` exactly 63
/// bytes, plus an indexed field literally named `<field>_unique_extra`,
/// whose own plain index Postgres truncates down to the same 63 bytes).
/// Closing this fully would mean giving *every* plain index name the same
/// truncate-on-63-bytes treatment this function already gives unique index
/// names — a broader, pre-existing gap in plain-index naming generally
/// (two long plain-indexed fields can already collide with *each other* the
/// same way, with no `unique` field involved at all), out of scope here.
#[must_use]
pub fn unique_index_name(table: &str, field: &str, fields: &[Field]) -> String {
    let full = format!("idx_{table}_{field}_unique");
    let collides_with_plain_index = fields.iter().any(|f| f.name == format!("{field}_unique"));
    if full.len() <= POSTGRES_MAX_IDENTIFIER_LEN && !collides_with_plain_index {
        return full;
    }
    let digest = hex::encode(Sha256::digest(full.as_bytes()));
    let suffix = format!("_{}", &digest[..8]);
    let prefix_len = (POSTGRES_MAX_IDENTIFIER_LEN.saturating_sub(suffix.len())).min(full.len());
    format!("{}{suffix}", &full[..prefix_len])
}

/// A `CREATE UNIQUE INDEX` statement enforcing single-column uniqueness on
/// `field` (issue #1032). See [`unique_index_name`] for the name it uses.
#[must_use]
pub fn unique_index_sql(table: &str, field: &str, fields: &[Field]) -> String {
    let name = unique_index_name(table, field, fields);
    format!("CREATE UNIQUE INDEX {name} ON {table} ({field});\n")
}

/// The trailing `CHECK (…)` clause a column needs to enforce at the database
/// layer what its declared type promises, or `None` when the column type
/// already enforces it.
///
/// Two kinds need one:
///
/// - `enum{…}` on both backends — the closed variant set, since the column is
///   `TEXT` either way.
/// - `decimal{p,s}` on `SQLite` only (issue #1924) — Postgres gets a real
///   `NUMERIC(p, s)`, but the `SQLite` column is `TEXT`, so without this the
///   declared precision and scale bind nothing and a repository write can
///   persist `123456.789` into a `decimal{5,2}`.
fn column_check_suffix(field: &Field, backend: DatabaseBackend) -> Option<String> {
    if field.kind.is_enum() {
        // Variants are validated `snake_case` identifiers (see
        // `super::dsl::parse_field`), so no SQL-escaping is needed here.
        let quoted = field
            .variants
            .iter()
            .map(|v| format!("'{v}'"))
            .collect::<Vec<_>>()
            .join(", ");
        return Some(format!("CHECK ({} IN ({quoted}))", field.name));
    }
    if backend == DatabaseBackend::Sqlite
        && let FieldKind::Decimal { precision, scale } = field.kind
    {
        return Some(sqlite_decimal_check(&field.name, precision, scale));
    }
    None
}

/// The `SQLite` `CHECK` that enforces `NUMERIC(precision, scale)` over a `TEXT`
/// column (issue #1924).
///
/// `SQLite` has no fixed-precision numeric type and no regular expressions, so
/// the constraint is spelled with string builtins over the stored text, which
/// `db::sqlite_types::SqliteDecimal` always writes as a plain, normalized
/// decimal literal. Four conditions, in order:
///
/// 1. only digits remain once the sign and the point are removed;
/// 2. at most one decimal point;
/// 3. a `-`, if present, is leading;
/// 4. the fractional part is at most `scale` digits, and the integer part at
///    most `precision - scale` digits once leading zeros are stripped.
///
/// (4) is the invariant `NUMERIC` enforces. It rejects rather than rounds,
/// unlike Postgres, which rounds a value to `scale` — a loud failure beats
/// silently storing what the schema says is out of range. `NULL` passes; the
/// column's own `NOT NULL` decides that.
fn sqlite_decimal_check(column: &str, precision: u32, scale: u32) -> String {
    // The unsigned text. Repeated rather than named: a SQLite `CHECK` has no `let`.
    let abs = format!("replace({column},'-','')");
    let frac_len =
        format!("CASE WHEN instr({abs},'.') = 0 THEN 0 ELSE length({abs}) - instr({abs},'.') END");
    let int_part = format!(
        "CASE WHEN instr({abs},'.') = 0 THEN {abs} ELSE substr({abs}, 1, instr({abs},'.') - 1) END"
    );
    // The digits alone — sign and point removed. Condition 1 proves it is all
    // digits, so its length is the digit count.
    let digits = format!("replace(replace({column},'-',''),'.','')");
    let conditions = [
        // 0: actually stored as TEXT. `TEXT` affinity does NOT convert a BLOB,
        // so `x'31392e3939'` keeps storage class blob while every string
        // function below reads it as `19.99` and waves it through — and diesel's
        // `FromSql<Text, Sqlite>` for `String` then refuses the blob before
        // `Decimal` ever sees it. Same unloadable-row failure as conditions 1-5,
        // one storage class further out.
        format!("typeof({column}) = 'text'"),
        // 1-5: a plain decimal literal. Without the digit count and the sign
        // count, `''`, `'-'`, `'.'`, `'--1'` and `'-1-'` all pass — values a
        // raw INSERT, an import or a hand-written migration can produce, which
        // would satisfy the constraint and then fail `SqliteDecimal::from_sql`,
        // leaving a row that cannot be loaded.
        format!("ltrim({digits}, '0123456789') = ''"),
        format!("length({digits}) >= 1"),
        format!("length({column}) - length(replace({column},'.','')) <= 1"),
        format!("length({column}) - length(replace({column},'-','')) <= 1"),
        format!("(instr({column},'-') = 0 OR instr({column},'-') = 1)"),
        // 6: scale.
        format!("{frac_len} <= {scale}"),
        // 7: precision, as the integer-digit budget NUMERIC(p, s) allows.
        format!(
            "length(ltrim({int_part}, '0')) <= {}",
            precision.saturating_sub(scale)
        ),
    ];
    format!("CHECK ({column} IS NULL OR ({}))", conditions.join(" AND "))
}

/// Result of inferring a migration shape from its name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MigrationShape {
    /// `AddXxxYyyToZZZ` — emit `ALTER TABLE … ADD COLUMN` per field.
    AddColumns { table: String },
    /// `RemoveXxxYyyFromZZZ` — emit `ALTER TABLE … DROP COLUMN` per field.
    RemoveColumns { table: String },
    /// `AddSearchTo<Table>` or `AddSearchableTo<Table>` or `AddSearchVectorTo<Table>`
    AddSearch { table: String },
    /// `Encrypt<Columns>On<Table>` — convert existing plaintext column(s) to
    /// at-rest encrypted (#805). Emits a documented offline-backfill migration
    /// and a rollback that restores plaintext from ciphertext given the keys.
    EncryptColumns { table: String, columns: Vec<String> },
    /// Anything else — emit empty `up.sql` / `down.sql` files.
    Empty,
}

/// Inspect a migration name (`PascalCase` from the CLI) and decide what shape
/// of SQL to emit.
#[must_use]
pub fn detect_migration_shape(pascal_name: &str) -> MigrationShape {
    if let Some(rest) = pascal_name.strip_prefix("AddSearchTo")
        && rest.chars().next().is_some_and(char::is_uppercase)
    {
        return MigrationShape::AddSearch {
            table: normalize_table_name(rest),
        };
    }
    if let Some(rest) = pascal_name.strip_prefix("AddSearchableTo")
        && rest.chars().next().is_some_and(char::is_uppercase)
    {
        return MigrationShape::AddSearch {
            table: normalize_table_name(rest),
        };
    }
    if let Some(rest) = pascal_name.strip_prefix("AddSearchVectorTo")
        && rest.chars().next().is_some_and(char::is_uppercase)
    {
        return MigrationShape::AddSearch {
            table: normalize_table_name(rest),
        };
    }

    if let Some(rest) = pascal_name.strip_prefix("Encrypt")
        && rest.chars().next().is_some_and(char::is_uppercase)
        && let Some((cols, table)) = split_on_keyword(rest, "On")
    {
        // `cols` is a PascalCase column name (the common case: one column per
        // encryption migration, e.g. `EncryptApiTokenOnAccounts`). Authors can
        // edit the emitted file to backfill additional columns.
        return MigrationShape::EncryptColumns {
            table: normalize_table_name(&table),
            columns: vec![super::naming::pascal_to_snake(&cols)],
        };
    }

    if let Some(rest) = pascal_name.strip_prefix("Add")
        && let Some((_, table)) = split_on_keyword(rest, "To")
    {
        return MigrationShape::AddColumns {
            table: normalize_table_name(&table),
        };
    }
    if let Some(rest) = pascal_name.strip_prefix("Remove")
        && let Some((_, table)) = split_on_keyword(rest, "From")
    {
        return MigrationShape::RemoveColumns {
            table: normalize_table_name(&table),
        };
    }
    MigrationShape::Empty
}

/// Snake-case the supplied table name, pluralising it if it isn't already
/// plural. `Posts` → `posts`; `Post` → `posts`.
fn normalize_table_name(table_pascal: &str) -> String {
    let snake = super::naming::pascal_to_snake(table_pascal);
    if snake.ends_with('s') {
        snake
    } else {
        super::naming::pluralize(&snake)
    }
}

/// Split `XxxYyy<keyword>Zzz` into (`XxxYyy`, `Zzz`) where `<keyword>` is
/// `"To"` or `"From"` and starts a new `PascalCase` chunk.
fn split_on_keyword(s: &str, keyword: &str) -> Option<(String, String)> {
    let mut idx = 0;
    while let Some(found) = s[idx..].find(keyword) {
        let abs = idx + found;
        // Word boundary: the keyword must start at a chunk boundary
        // (the previous char must be lowercase or it's the start of the
        // string, and the char after the keyword must be uppercase).
        let prev_ok = abs == 0
            || s.as_bytes()[abs - 1].is_ascii_lowercase()
            || s.as_bytes()[abs - 1].is_ascii_digit();
        let after_idx = abs + keyword.len();
        let after_ok = s
            .as_bytes()
            .get(after_idx)
            .is_some_and(u8::is_ascii_uppercase);
        if prev_ok && after_ok {
            return Some((s[..abs].to_owned(), s[after_idx..].to_owned()));
        }
        idx = abs + 1;
    }
    None
}

/// `fields`, extended with a placeholder [`Field`] per column `table`
/// already declares in `existing_schema` (see [`existing_schema_columns`])
/// that isn't already in `fields`. Only `name` is meaningful on the
/// placeholders — [`unique_index_name`]'s collision check is the only thing
/// that consumes this combined list, and it only ever looks at `.name`.
fn fields_with_existing_schema_columns(
    fields: &[Field],
    existing_schema: &str,
    table: &str,
) -> Vec<Field> {
    let mut combined = fields.to_vec();
    for name in existing_schema_columns(existing_schema, table) {
        if !combined.iter().any(|f| f.name == name) {
            combined.push(Field {
                name,
                kind: FieldKind::String,
                nullable: false,
                variants: Vec::new(),
                unique: false,
                constraints: FieldConstraints::default(),
                state_machine: None,
            });
        }
    }
    combined
}

/// SQL for adding columns to a table.
///
/// Prepends an `autumn-safety` comment for `NOT NULL` columns that have no
/// `DEFAULT` — those require a backfill or a default before the constraint can
/// be added safely on a live table.
///
/// `existing_schema` is `src/schema.rs`'s current content (or `""` if
/// unavailable) — passed through to [`fields_with_existing_schema_columns`]
/// so a `unique` field's index name can't collide with a plain index on some
/// other, already-existing column from an earlier migration (issue #1032
/// review follow-up).
// Retained as a Postgres-default convenience wrapper for the test suite; the
// backend-aware `add_columns_up_sql_for` is what production calls. The Postgres
// path never rejects, so this unwraps the `Ok` for terse test assertions.
#[cfg(test)]
#[must_use]
pub fn add_columns_up_sql(table: &str, fields: &[Field], existing_schema: &str) -> String {
    add_columns_up_sql_for(DatabaseBackend::Postgres, table, fields, existing_schema)
        .expect("Postgres ADD COLUMN generation never rejects")
}

/// `add_columns_up_sql` for a specific database `backend` (issue #1614). The
/// Postgres path stays byte-for-byte identical; the `SQLite` path emits
/// `SQLite`-valid column types via [`super::dsl::Field::sql_column_type_for`].
///
/// # Errors
/// Returns a generate-time rejection (issue #1614 AC #4) when the `backend` is
/// `SQLite` and a `NOT NULL` column has no default: `SQLite` rejects
/// `ALTER TABLE … ADD COLUMN … NOT NULL` without a `DEFAULT` once the table has
/// rows, so the migration would fail to apply. `generate migration Add…To…`
/// has no way to attach a column default, so every `NOT NULL` added column is
/// rejected on `SQLite` — the user must make the field nullable (or move it
/// into the table's `CREATE TABLE`, where `NOT NULL` is fine on `SQLite`). The
/// Postgres path never rejects and stays byte-for-byte identical.
pub fn add_columns_up_sql_for(
    backend: DatabaseBackend,
    table: &str,
    fields: &[Field],
    existing_schema: &str,
) -> Result<String, GenerateError> {
    let collision_fields = fields_with_existing_schema_columns(fields, existing_schema, table);
    let mut out = String::new();
    for f in fields {
        // A `position` field (issue #1358) can't be retrofit onto an existing
        // table through this codegen path: `NOT NULL` with no per-field
        // default (like every other column here) would leave every existing
        // row sharing the same value, silently violating the contiguous
        // `0..len-1`-per-scope invariant until the first `move_*` call
        // happens to fix it up. Reject with a clear message rather than emit
        // a migration that "succeeds" into a broken ordering.
        if f.kind.is_position() {
            return Err(GenerateError::Config(format!(
                "cannot add `position` column `{}` to table `{table}` via `generate migration \
                 Add...To...`: an existing table's rows would all need a contiguous `0..len-1` \
                 backfill, which this codegen path does not generate. Add the `position` field \
                 when first scaffolding the model (`generate model`/`generate scaffold`), or \
                 hand-write a migration that adds the column and backfills it with \
                 `ROW_NUMBER() OVER (...) - 1` before making it NOT NULL.",
                f.name
            )));
        }
        // SQLite rejects `ALTER TABLE … ADD COLUMN … NOT NULL` without a DEFAULT (#1614
        // AC #4). This path carries no per-field default, so any NOT NULL added column is
        // rejected at generate time rather than emitting DDL that breaks on SQLite.
        // Postgres is unaffected — its output stays byte for byte identical — and a NOT
        // NULL column inside CREATE TABLE, the `generate model` path, is likewise fine on
        // SQLite.
        //
        // #1318: `lock_version` is the one column this path can default on its own.
        // `#[lock_version]` makes it DB-managed, so the generated `New{Model}` never names
        // it and a bare `NOT NULL` add would leave every subsequent INSERT failing — and
        // the retrofit, "add optimistic locking to a resource I already shipped", is the
        // normal way this column arrives. `DEFAULT 0` also backfills existing rows in one
        // statement, which is why this add needs neither the blocking-safety banner nor
        // the SQLite refusal below.
        //
        // #1384: a `{translatable}` column is the same shape of retrofit — the container
        // column is `NOT NULL` and defaults to the empty JSON object `'{}'`, a constant
        // that backfills every existing row in one statement. Like `lock_version` it needs
        // neither the banner nor the refusal, and `autumn migrate check` classifies the
        // result as a plain, safe `ADD COLUMN NOT NULL DEFAULT <constant>`.
        let lock_version_default = super::model::is_lock_version_column(f);
        let inherent_default = f.sql_default();
        let has_default = lock_version_default || inherent_default.is_some();
        if backend == DatabaseBackend::Sqlite && !f.nullable && !has_default {
            return Err(super::sqlite_add_not_null_without_default_error(
                table, &f.name,
            ));
        }
        if !f.nullable && !has_default {
            let _ = writeln!(
                out,
                "-- autumn-safety: potentially-blocking \
                 -- add a DEFAULT or backfill existing rows before enforcing NOT NULL"
            );
        }
        let _ = write!(
            out,
            "ALTER TABLE {table} ADD COLUMN {} {} {}",
            f.name,
            f.sql_column_type_for(backend),
            f.sql_nullability()
        );
        if lock_version_default {
            out.push_str(" DEFAULT 0");
        } else if let Some(default) = inherent_default {
            let _ = write!(out, " DEFAULT {default}");
        }
        if let Some(target) = f.reference_table() {
            let _ = write!(out, " REFERENCES {target}(id)");
        }
        if let Some(check) = column_check_suffix(f, backend) {
            let _ = write!(out, " {check}");
        }
        out.push_str(";\n");
        // A `unique` field's own `CREATE UNIQUE INDEX` (emitted below)
        // already covers lookups on that column, so a `references` field
        // that is *also* `unique` must not get the plain auto-index too —
        // same dedup `create_table_sql_with_metadata_and_id` already applies
        // (issue #1032 review follow-up: this path emitted both, building
        // and maintaining a redundant second btree index).
        if f.kind.is_reference() && !f.unique {
            // Postgres auto-drops this index (and the FK constraint above) when
            // the column is dropped, so its `down.sql` needs no explicit DROP
            // INDEX. SQLite does NOT — it refuses to drop a column still used by
            // an index — so `add_columns_down_sql_for` emits a matching
            // `DROP INDEX idx_<table>_<col>` before the DROP COLUMN there.
            let _ = writeln!(
                out,
                "CREATE INDEX idx_{table}_{} ON {table} ({});",
                f.name, f.name
            );
        }
        if f.unique {
            // Same as the `references` auto-index above: Postgres cascades the
            // drop with the column, but SQLite needs an explicit DROP INDEX in
            // `add_columns_down_sql_for` (by the same `unique_index_name`).
            out.push_str(&unique_index_sql(table, &f.name, &collision_fields));
        }
    }
    Ok(out)
}

/// `down.sql` companion to [`add_columns_up_sql`]. Postgres-default wrapper
/// retained for the test suite; production calls the backend-aware
/// [`add_columns_down_sql_for`].
#[cfg(test)]
#[must_use]
pub fn add_columns_down_sql(table: &str, fields: &[Field]) -> String {
    add_columns_down_sql_for(DatabaseBackend::Postgres, table, fields, "")
}

/// `add_columns_down_sql` for a specific database `backend` (issue #1614).
///
/// On `SQLite`, [`add_columns_up_sql_for`] emits a `CREATE INDEX` for an added
/// nullable `references` field or a `unique` field, but `SQLite` refuses to
/// `DROP COLUMN` while an index still references it (`cannot drop column: used
/// in an index`). So on the `SQLite` path this emits `DROP INDEX <name>;` for
/// each index the up path created for a field, **before** its `DROP COLUMN`,
/// reusing the exact index-name derivation the up path used (`idx_<table>_<col>`
/// for a plain `references` index, [`unique_index_name`] for a `unique` index).
///
/// Postgres cascades index drops with the column automatically, so its output
/// stays byte-for-byte identical to the legacy `DROP COLUMN`-only rollback (no
/// explicit `DROP INDEX`). `existing_schema` mirrors [`add_columns_up_sql_for`]
/// so a `unique` field's index name matches the one the up path generated.
#[must_use]
pub fn add_columns_down_sql_for(
    backend: DatabaseBackend,
    table: &str,
    fields: &[Field],
    existing_schema: &str,
) -> String {
    let collision_fields = fields_with_existing_schema_columns(fields, existing_schema, table);
    let mut out = String::new();
    for f in fields.iter().rev() {
        // SQLite: drop the up path's index for this field first (see doc
        // comment). Only nullable fields reach a SQLite Add migration —
        // `add_columns_up_sql_for` rejects NOT NULL there — and the up path
        // indexes exactly a `unique` field or a non-unique `references` field.
        if backend == DatabaseBackend::Sqlite {
            if f.unique {
                let name = unique_index_name(table, &f.name, &collision_fields);
                let _ = writeln!(out, "DROP INDEX {name};");
            } else if f.kind.is_reference() {
                let _ = writeln!(out, "DROP INDEX idx_{table}_{};", f.name);
            }
        }
        let _ = writeln!(out, "ALTER TABLE {table} DROP COLUMN {};", f.name);
    }
    out
}

/// Document why bounded `VARCHAR(n)` columns must be widened to `TEXT` before an
/// encryption backfill, and emit the `ALTER … TYPE TEXT` statements per column.
fn write_widen_bounded_columns_note(out: &mut String, table: &str, columns: &[String]) {
    let _ = writeln!(
        out,
        "-- The envelope is base64 text. An UNBOUNDED `TEXT` column needs no type"
    );
    let _ = writeln!(
        out,
        "-- change, but a BOUNDED `VARCHAR(n)` column almost certainly does: the"
    );
    let _ = writeln!(
        out,
        "-- envelope adds a 20-byte header + 16-byte GCM tag and is then base64-"
    );
    let _ = writeln!(
        out,
        "-- encoded (~1.37x), so e.g. a VARCHAR(255) value can grow past 255 chars"
    );
    let _ = writeln!(
        out,
        "-- and the backfill (or later writes) will fail with a length violation."
    );
    let _ = writeln!(
        out,
        "-- Widen bounded columns to TEXT (or a sufficiently larger limit) FIRST:"
    );
    for col in columns {
        let _ = writeln!(
            out,
            "--      ALTER TABLE {table} ALTER COLUMN {col} TYPE TEXT;"
        );
    }
}

/// `up.sql` for converting plaintext column(s) to at-rest encrypted (#805).
///
/// Encrypted values are stored as a base64 AES-256-GCM envelope. An unbounded
/// `TEXT` column needs no type change, but a bounded `VARCHAR(n)` column must be
/// widened first (the envelope is larger than the plaintext), so the scaffold
/// emits the `ALTER … TYPE TEXT` statements. The actual encryption of existing
/// rows is an **offline backfill** that needs the application's key ring, so it
/// runs as a one-off task rather than raw SQL. This file documents the procedure
/// and serves as the migration record.
#[must_use]
pub fn encrypt_columns_up_sql(table: &str, columns: &[String]) -> String {
    let mut out = String::with_capacity(1024);
    let _ = writeln!(
        out,
        "-- autumn-safety: backfill \
         -- run the offline encryption backfill BEFORE deploying readers that \
         expect ciphertext"
    );
    let _ = writeln!(out, "--");
    let _ = writeln!(
        out,
        "-- Convert plaintext column(s) on `{table}` to at-rest encryption (#805)."
    );
    write_widen_bounded_columns_note(&mut out, table, columns);
    let _ = writeln!(out, "--");
    let _ = writeln!(out, "-- 1. Configure keys (once). The salt is required:");
    let _ = writeln!(out, "--      autumn credentials edit");
    let _ = writeln!(out, "--      [active_record_encryption]");
    let _ = writeln!(
        out,
        "--      primary_key         = \"<openssl rand -hex 32>\""
    );
    let _ = writeln!(
        out,
        "--      key_derivation_salt = \"<openssl rand -hex 16>\""
    );
    let _ = writeln!(
        out,
        "--      # deterministic_key = \"<openssl rand -hex 32>\"  # for deterministic / versioned_ciphertext"
    );
    let _ = writeln!(out, "--");
    let _ = writeln!(
        out,
        "-- 2. Backfill BEFORE adding `#[encrypted]` to the model field. Once the"
    );
    let _ = writeln!(
        out,
        "--    attribute is present the column's reader decrypts on load, so any"
    );
    let _ = writeln!(
        out,
        "--    still-plaintext row would fail with a malformed-envelope error."
    );
    let _ = writeln!(
        out,
        "--    Run a one-off task over a TEMPORARY plaintext model (no `#[encrypted]`)"
    );
    let _ = writeln!(
        out,
        "--    that reads each row's plaintext and writes the envelope produced by"
    );
    let _ = writeln!(
        out,
        "--    autumn_web::encryption::encrypt_text(<mode>, &plaintext), where <mode> is"
    );
    let _ = writeln!(
        out,
        "--    Mode::Deterministic for columns you will deploy as"
    );
    let _ = writeln!(
        out,
        "--    `#[encrypted(deterministic)]` (so existing rows are found by equality"
    );
    let _ = writeln!(out, "--    lookups) and Mode::Randomized otherwise:");
    for col in columns {
        let _ = writeln!(
            out,
            "--      UPDATE {table} SET {col} = <encrypt_text({col})>;"
        );
    }
    let _ = writeln!(out, "--");
    let _ = writeln!(
        out,
        "-- 3. Only after every row is ciphertext, add `#[encrypted]` to the field"
    );
    let _ = writeln!(out, "--    and deploy the encrypted reader.");
    let _ = writeln!(out, "--");
    let _ = writeln!(
        out,
        "-- Take a backup first: a row encrypted with a lost key is unrecoverable."
    );
    out
}

/// `down.sql` companion to [`encrypt_columns_up_sql`]: restore plaintext from
/// ciphertext, given the keys.
#[must_use]
pub fn encrypt_columns_down_sql(table: &str, columns: &[String]) -> String {
    let mut out = String::with_capacity(512);
    let _ = writeln!(
        out,
        "-- Rollback: restore plaintext from ciphertext on `{table}` (#805)."
    );
    let _ = writeln!(
        out,
        "-- Run a one-off task that decrypts each row with the configured keys via"
    );
    let _ = writeln!(
        out,
        "-- autumn_web::encryption::decrypt_text(&envelope) and writes plaintext back:"
    );
    for col in columns {
        let _ = writeln!(out, "--      UPDATE {table} SET {col} = <decrypt({col})>;");
    }
    let _ = writeln!(
        out,
        "-- Then remove the `#[encrypted]` attribute from the model field."
    );
    out
}

/// SQL for removing columns from a table (Postgres default).
///
/// Retained as a Postgres-default convenience wrapper for the test suite; the
/// backend-aware [`remove_columns_up_sql_for`] is what production calls.
#[cfg(test)]
#[must_use]
pub fn remove_columns_up_sql(table: &str, fields: &[Field]) -> String {
    remove_columns_up_sql_for(DatabaseBackend::Postgres, table, fields, "")
}

/// `remove_columns_up_sql` for a specific database `backend` (issue #1614).
///
/// Prepends an `autumn-safety` comment for each `DROP COLUMN` to make the
/// rolling-deploy risk visible at a glance and machine-parseable by
/// `autumn migrate check`.
///
/// On `SQLite`, the generator auto-creates an index named `idx_<table>_<col>`
/// for both a plain scaffold `--index <col>` field and a `references` field
/// (matching [`add_columns_up_sql_for`] /
/// [`create_table_sql_with_metadata_and_id`]), and a uniquely-named index for a
/// `unique` field ([`unique_index_name`]), and `SQLite` refuses to `DROP COLUMN`
/// while an index still references it (`cannot drop column: used in an index`).
/// The DSL/schema can't tell after the fact whether a removed column carried a
/// plain `--index`, so on the `SQLite` path this emits
/// `DROP INDEX IF EXISTS idx_<table>_<col>;` **unconditionally** before each
/// `DROP COLUMN` (`IF EXISTS` makes it a safe no-op for a column that was never
/// indexed, and the name is deterministic for both plain `--index` and
/// `references` fields), plus `DROP INDEX IF EXISTS <unique_index_name>;` for a
/// `unique` field — the same shape as the rollback path
/// ([`add_columns_down_sql_for`]), here on the forward `RemoveColumns` path.
/// Postgres cascades index drops with the column, so its output stays
/// byte-for-byte identical (no explicit `DROP INDEX`).
///
/// Scope boundary: a column indexed for reasons the generator cannot see (a
/// composite index spanning several columns) remains the documented limitation
/// of issue #1906.
///
/// `existing_schema` mirrors [`add_columns_down_sql_for`] so a `unique` field's
/// index name matches the one the up path generated.
#[must_use]
pub fn remove_columns_up_sql_for(
    backend: DatabaseBackend,
    table: &str,
    fields: &[Field],
    existing_schema: &str,
) -> String {
    let collision_fields = fields_with_existing_schema_columns(fields, existing_schema, table);
    let mut out = String::new();
    for f in fields {
        let _ = writeln!(
            out,
            "-- autumn-safety: destructive \
             -- old replicas that reference this column will fail until restarted; \
             use expand/contract"
        );
        // SQLite: drop the generator's index for this field first (see doc comment).
        // SQLite refuses `DROP COLUMN` while any index references the column. The
        // generator names a plain `--index` field's index and a `references` field's
        // auto-index identically (`idx_<table>_<col>`), and the DSL and schema cannot
        // tell after the fact whether a column carried a plain index, so emit `DROP
        // INDEX IF EXISTS idx_<table>_<col>;` unconditionally — a safe no-op for a
        // non-indexed column — plus the `unique` field's uniquely-named index. Names
        // come from the same helpers the ADD and CREATE paths use, so the DROP matches
        // the existing CREATE INDEX.
        if backend == DatabaseBackend::Sqlite {
            let _ = writeln!(out, "DROP INDEX IF EXISTS idx_{table}_{};", f.name);
            if f.unique {
                let name = unique_index_name(table, &f.name, &collision_fields);
                let _ = writeln!(out, "DROP INDEX IF EXISTS {name};");
            }
        }
        let _ = writeln!(out, "ALTER TABLE {table} DROP COLUMN {};", f.name);
    }
    out
}

/// `down.sql` companion to [`remove_columns_up_sql`]. Restores a `references`
/// field's `REFERENCES <table>(id)` constraint and automatic index (see
/// [`create_table_sql_with_metadata_and_id`]/[`add_columns_up_sql`]) — a
/// bare re-added column would silently drop the foreign-key relationship and
/// its lookup index on rollback (issue #1026). Likewise restores an `enum{…}`
/// field's `CHECK` constraint (issue #1030), and a `unique` field's `CREATE
/// UNIQUE INDEX` (issue #1032) — otherwise the closed set / uniqueness
/// constraint would silently stop being enforced after a rollback.
///
/// `existing_schema` is `src/schema.rs`'s current content (or `""` if
/// unavailable) — see [`add_columns_up_sql`]'s matching doc comment for why.
// Retained as a Postgres-default convenience wrapper for the test suite; the
// backend-aware `remove_columns_down_sql_for` is what production calls. The
// Postgres path never rejects, so this unwraps the `Ok` for terse test
// assertions.
#[cfg(test)]
#[must_use]
pub fn remove_columns_down_sql(table: &str, fields: &[Field], existing_schema: &str) -> String {
    remove_columns_down_sql_for(DatabaseBackend::Postgres, table, fields, existing_schema)
        .expect("Postgres ADD COLUMN generation never rejects")
}

/// `remove_columns_down_sql` for a specific database `backend` (issue #1614).
/// The Postgres path stays byte-for-byte identical; the `SQLite` path restores
/// the dropped column with a `SQLite`-valid type via
/// [`super::dsl::Field::sql_column_type_for`].
///
/// # Errors
/// Returns a generate-time rejection (issue #1614 AC #4) when the `backend` is
/// `SQLite` and a re-added column is `NOT NULL` with no default. The rollback
/// (`down.sql`) of a "remove columns" migration regenerates
/// `ALTER TABLE … ADD COLUMN …` to restore the dropped columns, and `SQLite`
/// rejects that DDL for a `NOT NULL` column without a `DEFAULT` once the table
/// has rows — the identical limit the forward path
/// ([`add_columns_up_sql_for`]) guards. This path carries no per-field default,
/// so every `NOT NULL` re-added column is rejected on `SQLite`; nullable
/// re-added columns are unaffected. The Postgres path never rejects and stays
/// byte-for-byte identical.
pub fn remove_columns_down_sql_for(
    backend: DatabaseBackend,
    table: &str,
    fields: &[Field],
    existing_schema: &str,
) -> Result<String, GenerateError> {
    let collision_fields = fields_with_existing_schema_columns(fields, existing_schema, table);
    let mut out = String::new();
    for f in fields.iter().rev() {
        // SQLite rejects `ALTER TABLE … ADD COLUMN … NOT NULL` without a DEFAULT
        // (#1614 AC #4). The rollback re-adds the dropped column with the same `ADD
        // COLUMN` DDL and carries no per-field default, so a NOT NULL re-added column
        // is rejected at generate time, mirroring the forward path
        // (`add_columns_up_sql_for`) for a consistent generate contract. Postgres is
        // unaffected, and a NOT NULL column inside CREATE TABLE is likewise fine on
        // SQLite. A `{translatable}` column (#1384) carries its own constant default
        // (`'{}'`), so its rollback re-add is valid on SQLite too.
        let inherent_default = f.sql_default();
        if backend == DatabaseBackend::Sqlite && !f.nullable && inherent_default.is_none() {
            return Err(super::sqlite_add_not_null_without_default_error(
                table, &f.name,
            ));
        }
        let _ = write!(
            out,
            "ALTER TABLE {table} ADD COLUMN {} {} {}",
            f.name,
            f.sql_column_type_for(backend),
            f.sql_nullability()
        );
        if let Some(default) = inherent_default {
            let _ = write!(out, " DEFAULT {default}");
        }
        if let Some(target) = f.reference_table() {
            let _ = write!(out, " REFERENCES {target}(id)");
        }
        if let Some(check) = column_check_suffix(f, backend) {
            let _ = write!(out, " {check}");
        }
        out.push_str(";\n");
        // See `add_columns_up_sql`'s matching comment: a `unique` field's
        // own `CREATE UNIQUE INDEX` already covers lookups, so a
        // `references` field that is also `unique` must not get the plain
        // auto-index restored too.
        if f.kind.is_reference() && !f.unique {
            let _ = writeln!(
                out,
                "CREATE INDEX idx_{table}_{} ON {table} ({});",
                f.name, f.name
            );
        }
        if f.unique {
            out.push_str(&unique_index_sql(table, &f.name, &collision_fields));
        }
    }
    Ok(out)
}

/// Add `mod <name>;` declarations to `src/main.rs` and route entries to the
/// `routes![...]` macro invocation, in a single pass.
///
/// Idempotent: existing `mod` declarations and route entries are preserved,
/// and adding the same set twice is a no-op.
#[must_use]
pub fn update_main_rs(existing: &str, mods: &[&str], route_entries: &[String]) -> String {
    let with_mods = ensure_mods(existing, mods);
    ensure_routes_entries(&with_mods, route_entries)
}

/// Insert `mod <name>;` lines near the top of `main.rs`, preserving any that
/// already exist.
///
/// ⚡ Bolt optimization: Pre-allocates string buffer based on mod count
/// and writes sequentially instead of creating intermediate vectors of strings.
fn ensure_mods(existing: &str, mods: &[&str]) -> String {
    use std::fmt::Write;
    let mut needed: Vec<&str> = mods
        .iter()
        .copied()
        .filter(|m| !has_mod_declaration(existing, m))
        .collect();
    if needed.is_empty() {
        return existing.to_owned();
    }
    needed.sort_unstable();
    let mut block = String::with_capacity(needed.len() * 15);
    for (i, m) in needed.iter().enumerate() {
        if i > 0 {
            block.push('\n');
        }
        write!(block, "mod {m};").unwrap();
    }

    // Mod declarations are *items* and must follow any crate-level inner
    // attributes (`#![allow(...)]`, `//!` doc comments) — Rust rejects the
    // file otherwise. Find the boundary between the leading attribute block
    // and the first ordinary item, and insert there.
    let split = existing
        .lines()
        .position(|l| {
            let t = l.trim_start();
            !t.is_empty() && !t.starts_with("//!") && !t.starts_with("#![")
            // Inner attributes can also be written `# ! [...]` with whitespace,
            // but in practice nobody does. Stick to the canonical shape.
        })
        .unwrap_or_else(|| existing.lines().count());

    if split == 0 {
        // No leading attributes — insert at the top.
        return format!("{block}\n\n{existing}");
    }

    let mut out = String::with_capacity(existing.len() + block.len() + 4);
    let lines: Vec<&str> = existing.lines().collect();
    for line in &lines[..split] {
        out.push_str(line);
        out.push('\n');
    }
    out.push_str(&block);
    out.push('\n');
    if split < lines.len() {
        out.push('\n');
        for line in &lines[split..] {
            out.push_str(line);
            out.push('\n');
        }
    }
    // Preserve the original trailing-newline status.
    if !existing.ends_with('\n') && out.ends_with('\n') {
        out.pop();
    }
    out
}

fn has_mod_declaration(existing: &str, name: &str) -> bool {
    let needles = [format!("mod {name};"), format!("pub mod {name};")];
    existing
        .lines()
        .map(str::trim)
        .any(|line| needles.iter().any(|n| line == n))
}

/// Inverse of [`ensure_mods`] (`autumn destroy`, issue #1048).
///
/// Removes each `mod <name>;` line (the bare, private form `ensure_mods`
/// inserts) for a name in `names`, then collapses any run of blank lines the
/// removal leaves behind down to at most one, and drops a leading blank line
/// at the very start of the file — restoring the file exactly as it was
/// before any of those declarations were added.
///
/// Unlike [`remove_mod_declaration`] (a resource's own `pub mod` entry in
/// `src/models/mod.rs`), this targets `src/main.rs`'s shared infrastructure
/// module names (`models`, `schema`, `repositories`, `routes`, …). The
/// caller is responsible for only passing names whose backing module no
/// longer exists on disk — these declarations are shared by every
/// generated resource, not owned by one.
#[must_use]
pub fn remove_main_mod_declarations(existing: &str, names: &[&str]) -> String {
    let lines: Vec<&str> = existing.lines().collect();
    let patterns: Vec<String> = names.iter().map(|n| format!("mod {n};")).collect();
    let matches_any = |line: &str| patterns.iter().any(|p| line.trim() == p);
    if !lines.iter().any(|l| matches_any(l)) {
        return existing.to_owned();
    }
    let kept: Vec<&str> = lines.into_iter().filter(|l| !matches_any(l)).collect();

    let mut collapsed: Vec<&str> = Vec::with_capacity(kept.len());
    for line in kept {
        if line.trim().is_empty() && collapsed.last().is_some_and(|l: &&str| l.trim().is_empty()) {
            continue;
        }
        collapsed.push(line);
    }
    while collapsed.first().is_some_and(|l| l.trim().is_empty()) {
        collapsed.remove(0);
    }

    let mut out = collapsed.join("\n");
    if existing.ends_with('\n') && !out.is_empty() {
        out.push('\n');
    }
    out
}

/// Inject the `#[path]`-qualified `mod schema;` / `mod models;` declarations
/// that link a scaffolded app's `src/schema.rs` and `src/models/` into the
/// standalone `src/bin/seed.rs` binary (issue #1718).
///
/// `autumn seed --count/--model` resolves a model by name through an
/// `inventory` registry that each `#[autumn_web::model]` submits into. That
/// registry is only populated with models actually **compiled into the seed
/// binary** — but `autumn new` emits `src/bin/seed.rs` as a separate `[[bin]]`
/// target that links neither `src/main.rs` nor `src/models/`, so a scaffolded
/// model was never visible to it and `--model M` returned
/// `unknown model M; available: (none)`. Declaring the models (and the
/// `schema` module they `use crate::schema::…` from) directly in `seed.rs`
/// pulls their `inventory::submit!`s into the seed binary.
///
/// The `#[path]` form is required because `src/bin/seed.rs`'s own module tree
/// has no `models`/`schema` child relative to `src/bin/`; the attribute points
/// each declaration at the real file under `src/`. Child modules of
/// `models/mod.rs` (`mod post;` → `src/models/post.rs`) resolve relative to
/// that file's directory, so no per-model edit is needed here — regenerating
/// or adding a model just extends `src/models/mod.rs`, which this single
/// declaration already re-exports into the seed binary.
///
/// Idempotent: a declaration already present (in any `mod x;` / `pub mod x;`
/// form, with or without a preceding `#[path]` attribute) is left untouched,
/// so repeated `generate` runs converge. The inverse
/// [`unlink_models_from_seed_bin`] removes these injected declarations at
/// destroy time — necessary because `autumn destroy` **deletes**
/// `src/models/mod.rs` / `src/schema.rs` once its `ModDecl`/`SchemaTable`
/// reverts empty them (destroying the last model), which would otherwise leave
/// `seed.rs`'s `#[path]` links dangling at missing files and break
/// `cargo check --bins` / `autumn seed`.
#[must_use]
pub fn link_models_into_seed_bin(existing: &str) -> String {
    // (module name, full declaration incl. its `#[path]` attribute)
    let entries: [(&str, &str); 2] = [
        ("schema", "#[path = \"../schema.rs\"]\nmod schema;"),
        ("models", "#[path = \"../models/mod.rs\"]\nmod models;"),
    ];
    let needed: Vec<&str> = entries
        .iter()
        .filter(|(name, _)| !has_mod_declaration(existing, name))
        .map(|(_, decl)| *decl)
        .collect();
    if needed.is_empty() {
        return existing.to_owned();
    }
    let block = needed.join("\n");

    // `mod` declarations are *items* and must follow any crate-level inner
    // attributes (`#![…]`) and `//!` doc comments — mirror `ensure_mods`.
    let split = existing
        .lines()
        .position(|l| {
            let t = l.trim_start();
            !t.is_empty() && !t.starts_with("//!") && !t.starts_with("#![")
        })
        .unwrap_or_else(|| existing.lines().count());

    if split == 0 {
        return format!("{block}\n\n{existing}");
    }

    let mut out = String::with_capacity(existing.len() + block.len() + 4);
    let lines: Vec<&str> = existing.lines().collect();
    for line in &lines[..split] {
        out.push_str(line);
        out.push('\n');
    }
    out.push_str(&block);
    out.push('\n');
    if split < lines.len() {
        out.push('\n');
        for line in &lines[split..] {
            out.push_str(line);
            out.push('\n');
        }
    }
    if !existing.ends_with('\n') && out.ends_with('\n') {
        out.pop();
    }
    out
}

/// Remove the `#[path]`-qualified `mod schema;` / `mod models;` declarations
/// that [`link_models_into_seed_bin`] injected into `src/bin/seed.rs`, the
/// destroy-time inverse of that link (issue #1718 follow-up).
///
/// `autumn destroy` deletes `src/schema.rs` and `src/models/mod.rs` once the
/// last model's `SchemaTable`/`ModDecl` reverts empty them, so the seed
/// binary's `#[path = "../schema.rs"] mod schema;` /
/// `#[path = "../models/mod.rs"] mod models;` links would then point at missing
/// files and fail `cargo check --bins`. This strips exactly those two injected
/// two-line blocks (attribute + `mod` declaration), matched on trimmed content
/// so only this generator's own `#[path]`-qualified form is touched — a
/// hand-written plain `mod schema;` without the injected attribute is left
/// alone. Idempotent: a block already absent is a no-op. Blank lines left at
/// the removal seam are collapsed so the reverted file stays tidy.
///
/// This is gated by [`Revert::SeedBinLinks`](crate::generate::emit::Revert::SeedBinLinks)'s `owner_dir` (`src/models`) so it
/// only runs when the *last* model is destroyed — destroying one of several
/// models leaves the links in place, matching the surviving `models/mod.rs`.
#[must_use]
pub fn unlink_models_from_seed_bin(existing: &str) -> String {
    // (attribute line, declaration line) for each injected block.
    let blocks: [[&str; 2]; 2] = [
        ["#[path = \"../schema.rs\"]", "mod schema;"],
        ["#[path = \"../models/mod.rs\"]", "mod models;"],
    ];
    let mut lines: Vec<String> = existing.lines().map(str::to_owned).collect();
    for [attr, decl] in blocks {
        let mut i = 0;
        while i + 1 < lines.len() {
            if lines[i].trim() == attr && lines[i + 1].trim() == decl {
                lines.drain(i..i + 2);
            } else {
                i += 1;
            }
        }
    }
    // Collapse runs of blank lines (and drop a leading blank) left where the
    // blocks were removed, so the reverted file matches its pre-link shape.
    let mut out_lines: Vec<String> = Vec::with_capacity(lines.len());
    let mut prev_blank = true; // seed `true` so a leading blank line is dropped
    for line in lines {
        let is_blank = line.trim().is_empty();
        if is_blank && prev_blank {
            continue;
        }
        prev_blank = is_blank;
        out_lines.push(line);
    }
    let mut out = out_lines.join("\n");
    if !out.is_empty() && existing.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// Whether the byte offset `pos` in `text` sits after a `//` on its own line —
/// i.e. the match at `pos` lives inside a line comment (`//`, `///`, or `//!`).
/// Used to skip `routes![` occurrences that appear in comments (e.g. a doc
/// comment explaining the macro) rather than in real code.
///
/// A `//` inside a double-quoted string literal is content, not a comment
/// marker — e.g. the URL in
/// `let url = "https://example.com"; app.routes(routes![index])` must not
/// make the line look commented out. This stays a line-local heuristic: it
/// tracks `"…"` state with `\`-escape handling but does not understand raw
/// strings (`r#"…"#`), char literals containing `"`, or block comments.
fn is_on_comment_line(text: &str, pos: usize) -> bool {
    let line_start = text[..pos].rfind('\n').map_or(0, |nl| nl + 1);
    let mut in_string = false;
    let mut chars = text[line_start..pos].chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if in_string => {
                // Skip the escaped character so `\"` doesn't end the string.
                chars.next();
            }
            '"' => in_string = !in_string,
            '/' if !in_string && chars.peek() == Some(&'/') => return true,
            _ => {}
        }
    }
    false
}

/// Locate the body span (byte offsets, exclusive of the enclosing
/// `routes![`/`]`) of the *first* `routes![ ... ]` macro invocation in
/// `existing`, skipping occurrences on comment lines (a doc comment such as
/// `//! routes![...]` must not be edited — injecting entries there breaks
/// compilation). Returns `None` if there is no non-comment `routes![` or its
/// brackets are unmatched. Shared by every function that reads or rewrites the
/// `routes![...]` body so the bracket-scan logic lives in exactly one place.
fn find_routes_body_range(existing: &str) -> Option<(usize, usize)> {
    let mut search_from = 0;
    let start = loop {
        let pos = search_from + existing[search_from..].find("routes![")?;
        if !is_on_comment_line(existing, pos) {
            break pos;
        }
        search_from = pos + "routes![".len();
    };
    let body_start = start + "routes![".len();
    // Find the matching closing bracket. The macro body cannot contain a
    // raw `]` outside of nested `[ ... ]`, so we just track depth.
    let mut depth: usize = 1;
    let bytes = existing.as_bytes();
    let mut i = body_start;
    while i < bytes.len() {
        match bytes[i] {
            b'[' => depth += 1,
            b']' => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            _ => {}
        }
        i += 1;
    }
    if depth != 0 {
        // Unmatched bracket.
        return None;
    }
    Some((body_start, i))
}

/// Insert each entry into the body of the *first* `routes![ ... ]` macro
/// invocation. Skips entries already present.
fn ensure_routes_entries(existing: &str, entries: &[String]) -> String {
    let Some((body_start, body_end)) = find_routes_body_range(existing) else {
        return existing.to_owned();
    };
    let body = &existing[body_start..body_end];
    let new_body = augment_routes_body(body, entries);
    let mut out = String::with_capacity(existing.len() + new_body.len());
    out.push_str(&existing[..body_start]);
    out.push_str(&new_body);
    out.push_str(&existing[body_end..]);
    out
}

/// Remove every entry in the *first* `routes![ ... ]` macro invocation whose
/// identifier starts with `prefix`. A no-op (returns `existing` unchanged)
/// if there is no `routes![...]` or no entry matches.
///
/// Used by generators that regenerate a resource whose route set can change
/// between runs (e.g. `autumn generate channel <Name> --force` switching
/// from the SSE transport's routes to the WS transport's) — call this
/// before [`update_main_rs`] so stale entries referencing functions the
/// regenerated file no longer defines are not left dangling in `main.rs`.
#[must_use]
pub fn remove_routes_entries_with_prefix(existing: &str, prefix: &str) -> String {
    let Some((body_start, body_end)) = find_routes_body_range(existing) else {
        return existing.to_owned();
    };
    let body = &existing[body_start..body_end];
    let original_entries: Vec<&str> = body
        .split([',', '\n'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    let kept: Vec<&str> = original_entries
        .iter()
        .copied()
        .filter(|s| !s.starts_with(prefix))
        .collect();
    if kept.len() == original_entries.len() {
        return existing.to_owned();
    }
    let indent = leading_indent(body);
    let mut new_body = String::with_capacity(body.len());
    for entry in &kept {
        new_body.push_str(&indent);
        new_body.push_str(entry);
        new_body.push_str(",\n");
    }
    let mut out = String::with_capacity(existing.len());
    out.push_str(&existing[..body_start]);
    out.push_str(&new_body);
    out.push_str(&existing[body_end..]);
    out
}

/// Inverse of [`augment_routes_body`] (`autumn destroy`, issue #1048).
///
/// Removes exactly `entries` from the first `routes![ ... ]` invocation and
/// restores the pre-existing entries' original layout — byte-identically
/// when they were on one line (the common case: a fresh `autumn new`
/// project's `routes![index, hello, hello_name]`), since `augment_routes_body`
/// only ever *appends* after existing content and never reformats it, so the
/// text preceding the first entry being removed is exactly what preceded
/// this generate call.
///
/// A no-op (returns `existing` unchanged) if there's no `routes![...]`, or if
/// NONE of `entries` is present any more (already destroyed). Removes
/// whichever of `entries` ARE currently present when only some are — e.g.
/// the user hand-removed one of this resource's routes before running
/// `destroy` — rather than abandoning the whole cleanup and leaving the
/// rest dangling (issue #1048 PR review): a route this resource's own file
/// deletion is about to orphan must not survive just because a sibling
/// entry from the same call was already gone.
#[must_use]
pub fn remove_routes_entries(existing: &str, entries: &[String]) -> String {
    if entries.is_empty() {
        return existing.to_owned();
    }
    let Some((body_start, body_end)) = find_routes_body_range(existing) else {
        return existing.to_owned();
    };
    let body = &existing[body_start..body_end];
    let present_entries: Vec<String> = body
        .split([',', '\n'])
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .collect();
    if !entries.iter().any(|e| present_entries.contains(e)) {
        return existing.to_owned();
    }
    let kept: Vec<&String> = present_entries
        .iter()
        .filter(|e| !entries.contains(e))
        .collect();

    // The pre-existing entries' original formatting survives untouched in
    // the current body, up to the point where this call's first removed
    // entry begins. Strip the separator (`,`/whitespace/newline) forward
    // added right after the original content to see whether that ORIGINAL
    // content itself spanned multiple lines.
    //
    // Locate that point via each entry's actual token span (from the same
    // comma/newline split `present_entries` was built from), not a raw
    // substring search — `body.find(e)` would also match `e` occurring
    // inside a *different*, kept entry that has it as a textual prefix
    // (e.g. removing `routes::posts::index` while `routes::posts::index_all`
    // is kept), misdetecting where the original layout ends.
    let spans = entry_spans(body);
    let first_removed_at = entries
        .iter()
        .filter_map(|e| {
            spans
                .iter()
                .find(|(_, _, text)| text == e)
                .map(|(start, ..)| *start)
        })
        .min()
        .unwrap_or(body.len());
    let original_segment = &body[..first_removed_at];
    let original_core = original_segment.trim_end_matches([' ', '\t', '\n', ',']);
    let multiline = original_core.contains('\n');

    let new_body = if multiline {
        let indent = leading_indent(body);
        let mut out = String::with_capacity(body.len());
        for entry in &kept {
            out.push_str(&indent);
            out.push_str(entry);
            out.push_str(",\n");
        }
        out
    } else {
        kept.iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };

    let mut out = String::with_capacity(existing.len());
    out.push_str(&existing[..body_start]);
    out.push_str(&new_body);
    out.push_str(&existing[body_end..]);
    out
}

/// Parse `body` into `(start, end, trimmed_text)` byte spans of each
/// comma/newline-delimited entry, so callers can locate an entry's actual
/// token occurrence rather than a raw substring search (which can match
/// inside a different, unrelated entry that has it as a textual prefix).
fn entry_spans(body: &str) -> Vec<(usize, usize, &str)> {
    let mut spans = Vec::new();
    let mut offset = 0usize;
    for piece in body.split_inclusive([',', '\n']) {
        let piece_start = offset;
        offset += piece.len();
        let trimmed = piece.trim_matches([',', '\n', ' ', '\t', '\r']);
        if trimmed.is_empty() {
            continue;
        }
        let Some(inner_offset) = piece.find(trimmed) else {
            continue;
        };
        let start = piece_start + inner_offset;
        spans.push((start, start + trimmed.len(), trimmed));
    }
    spans
}

fn augment_routes_body(body: &str, entries: &[String]) -> String {
    let existing_entries: Vec<String> = body
        .split([',', '\n'])
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .collect();
    let mut to_add: Vec<&String> = entries
        .iter()
        .filter(|e| !existing_entries.iter().any(|ex| ex == e.as_str()))
        .collect();
    if to_add.is_empty() {
        return body.to_owned();
    }
    // De-dup within `to_add` while preserving order.
    let mut seen = std::collections::HashSet::new();
    to_add.retain(|s| seen.insert(s.as_str()));

    // Detect leading whitespace inside the routes![] body so generated
    // entries match the existing indentation style.
    let indent = leading_indent(body);
    let trimmed = body.trim_end_matches([' ', '\t']);
    // Decide the insertion separator.
    let prefix = if trimmed.is_empty() || trimmed.ends_with(',') || trimmed.ends_with('\n') {
        ""
    } else {
        ","
    };
    let mut out = String::with_capacity(body.len() + to_add.len() * 32);
    out.push_str(trimmed);
    out.push_str(prefix);
    for entry in to_add {
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&indent);
        out.push_str(entry);
        out.push(',');
    }
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// Return the indent (spaces/tabs) of the most-indented non-blank line in
/// `body`. Falls back to 12 spaces (the default for a `routes![]` block
/// nested inside a builder chain inside `async fn main()`).
fn leading_indent(body: &str) -> String {
    body.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            l.chars()
                .take_while(|c| *c == ' ' || *c == '\t')
                .collect::<String>()
        })
        .max_by_key(String::len)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "            ".to_owned())
}

// ── Mail preview wiring ───────────────────────────────────────────────────

/// Insert or augment a `.mail_previews(mail_previews![...])` call in the app
/// builder chain inside `src/main.rs`.
///
/// - If `mail_previews![` already exists, `mailer_type` is appended to its
///   type list (idempotent when already present).
/// - Otherwise, a new `.mail_previews(mail_previews![mailer_type])` call is
///   inserted immediately before the first `.run()` line in the builder chain.
///
/// Returns `existing` unchanged when neither injection point can be found.
#[must_use]
pub fn add_mail_preview_to_app(existing: &str, mailer_type: &str) -> String {
    const PREVIEW_MACRO: &str = "mail_previews![";
    existing.find(PREVIEW_MACRO).map_or_else(
        || insert_mail_previews_call(existing, mailer_type),
        |macro_start| {
            augment_mail_previews_list(existing, macro_start + PREVIEW_MACRO.len(), mailer_type)
        },
    )
}

/// Splice `entry` into the body of a `macro![...]` call starting at `body_start`.
///
/// Idempotent: returns `existing` unchanged if `entry` is already present.
/// Returns `existing` unchanged if no closing `]` is found.
fn splice_into_macro_body(existing: &str, body_start: usize, entry: &str) -> String {
    let rest = &existing[body_start..];
    let Some(end_offset) = rest.find(']') else {
        return existing.to_owned();
    };
    let body = &rest[..end_offset];

    // Idempotency: skip if entry is already registered.
    if body.split(',').map(str::trim).any(|t| t == entry) {
        return existing.to_owned();
    }

    // Trim whitespace and any trailing comma so `cargo fmt`-formatted multi-line
    // macro bodies (which may end with a trailing comma) don't produce `entry1,, entry2`.
    let trimmed_body = body.trim().trim_end_matches(',');
    let separator = if trimmed_body.is_empty() { "" } else { ", " };
    let new_body = format!("{trimmed_body}{separator}{entry}");
    [
        &existing[..body_start],
        &new_body,
        &existing[body_start + end_offset..],
    ]
    .concat()
}

/// Append `mailer_type` inside an already-present `mail_previews![...]`.
fn augment_mail_previews_list(existing: &str, body_start: usize, mailer_type: &str) -> String {
    splice_into_macro_body(existing, body_start, mailer_type)
}

/// Insert `line_to_insert` (without trailing newline) before the first `.run()`
/// line in an `AppBuilder` chain, preserving the same indentation.
///
/// Returns `existing` unchanged when no `.run()` line can be found.
fn insert_before_run_call(existing: &str, line_to_insert: &str) -> String {
    let mut out = String::with_capacity(existing.len() + line_to_insert.len() + 4);
    let mut inserted = false;
    for line in existing.lines() {
        let trimmed = line.trim_start();
        if !inserted && trimmed.starts_with(".run()") {
            let indent_len = line.len() - trimmed.len();
            let indent = &line[..indent_len];
            out.push_str(indent);
            out.push_str(line_to_insert);
            out.push('\n');
            inserted = true;
        }
        out.push_str(line);
        out.push('\n');
    }
    // lines() doesn't yield the trailing empty segment that split('\n') would,
    // so remove the surplus '\n' only when the original had no trailing newline.
    if !existing.ends_with('\n') && out.ends_with('\n') {
        out.pop();
    }
    out
}

// ── Remember-me middleware wiring (issue #1397) ──────────────────────────────

/// The Tower layer that consumes a remember cookie and rotates it into a
/// session, auto-wired into the `AppBuilder` chain by `autumn generate auth`.
const REMEMBER_LAYER_CALL: &str = ".layer(axum::middleware::from_fn(routes::auth::remember_me))";
/// The startup hook that hands the remember middleware the pool + resolved
/// `[auth.remember]` config.
const REMEMBER_STARTUP_CALL: &str = ".on_startup(routes::auth::remember_me_startup)";

/// Inject the remember-me middleware layer and its startup hook into the
/// `AppBuilder` chain in `src/main.rs`, immediately before `.run()`.
///
/// Idempotent: a no-op when the layer is already present, and (like the jobs /
/// mail-preview injectors) a no-op when no standalone `.run()` line can be found
/// — a single-line builder chain is left untouched.
#[must_use]
pub fn add_remember_middleware_to_app(existing: &str) -> String {
    if existing.contains(REMEMBER_LAYER_CALL) {
        return existing.to_owned();
    }
    let with_startup = insert_before_run_call(existing, REMEMBER_STARTUP_CALL);
    insert_before_run_call(&with_startup, REMEMBER_LAYER_CALL)
}

/// Inverse of [`add_remember_middleware_to_app`] (`autumn destroy`, issue #1048).
///
/// Removes the two injected builder-call lines (whatever indentation they
/// carry), restoring `src/main.rs` exactly. A no-op when neither line is
/// present.
#[must_use]
pub fn remove_remember_middleware_from_app(existing: &str) -> String {
    let is_injected = |l: &str| {
        let t = l.trim();
        t == REMEMBER_LAYER_CALL || t == REMEMBER_STARTUP_CALL
    };
    if !existing.lines().any(is_injected) {
        return existing.to_owned();
    }
    let kept: Vec<&str> = existing.lines().filter(|l| !is_injected(l)).collect();
    let mut out = kept.join("\n");
    if existing.ends_with('\n') && !out.is_empty() {
        out.push('\n');
    }
    out
}

/// Insert `.mail_previews(mail_previews![mailer_type])` before `.run()`.
fn insert_mail_previews_call(existing: &str, mailer_type: &str) -> String {
    insert_before_run_call(
        existing,
        &format!(".mail_previews(mail_previews![{mailer_type}])"),
    )
}

/// Inverse of [`add_mail_preview_to_app`] (`autumn destroy`, issue #1048).
///
/// Removes `mailer_type` from the `mail_previews![...]` list. If it was the
/// only entry, the whole freshly-inserted
/// `.mail_previews(mail_previews![...])` line is removed too, rather than
/// leaving an empty `mail_previews![]` call behind.
///
/// A no-op if there's no `mail_previews![...]`, or `mailer_type` isn't
/// currently listed.
///
/// Locates the whole `.mail_previews(mail_previews![...])` call by balanced-
/// paren scan (rather than assuming it stays on one line) when the list
/// empties out, so a project that ran the call through `rustfmt` — which
/// commonly wraps it across several lines once it has more than a couple of
/// mailer names — doesn't end up with a dangling, now-meaningless remnant
/// (issue #1048 PR review).
#[must_use]
pub fn remove_mail_preview_from_app(existing: &str, mailer_type: &str) -> String {
    const PREVIEW_MACRO: &str = "mail_previews![";
    const CALL_PREFIX: &str = ".mail_previews(";
    let Some((spliced, now_empty)) =
        remove_entry_from_bracketed_list(existing, PREVIEW_MACRO, mailer_type)
    else {
        return existing.to_owned();
    };
    if !now_empty {
        return spliced;
    }
    // Only entry -- remove the whole freshly-inserted call, whichever lines
    // it spans.
    let Some(call_pos) = existing.find(CALL_PREFIX) else {
        return existing.to_owned();
    };
    let Some(call_end) = find_balanced_close_paren(existing, call_pos + CALL_PREFIX.len()) else {
        return existing.to_owned();
    };
    let start_line = existing[..call_pos].rfind('\n').map_or(0, |i| i + 1);
    let end_line = existing[call_end..]
        .find('\n')
        .map_or(existing.len(), |i| call_end + i + 1);
    let mut out = String::with_capacity(existing.len());
    out.push_str(&existing[..start_line]);
    out.push_str(&existing[end_line..]);
    out
}

/// Scan forward from `start` (the byte position just after an already-open
/// `(`, i.e. depth 1) for the matching closing paren, returning the index
/// just past it. `None` if the parens never balance (malformed/truncated
/// input — destroy never guesses).
const fn find_balanced_close_paren(src: &str, start: usize) -> Option<usize> {
    let bytes = src.as_bytes();
    let mut depth = 1usize;
    let mut i = start;
    while i < bytes.len() {
        match bytes[i] {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i + 1);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Shared bracket-list-entry removal behind [`remove_job_entry`] and
/// [`remove_mail_preview_from_app`]: locate `macro_literal` (e.g.
/// `"jobs!["`), remove `entry` from its comma-separated body, and rejoin.
///
/// Returns `None` if the macro isn't present or `entry` isn't currently
/// listed (destroy never guesses at a partial match). Otherwise returns
/// `Some((new_content, list_is_now_empty))` — callers that need to collapse
/// or remove a now-meaningless surrounding call/function when the list
/// empties out (which one text becomes home to.. differs — `jobs![]`
/// removes a whole `fn`, `mail_previews![]` removes a call line) check the
/// `bool` and discard `new_content` in that case, matching
/// [`remove_dep_feature_in_section`]'s `collapse_to_bare` pattern for the
/// analogous Cargo-feature-list case.
fn remove_entry_from_bracketed_list(
    existing: &str,
    macro_literal: &str,
    entry: &str,
) -> Option<(String, bool)> {
    let macro_start = existing.find(macro_literal)?;
    let body_start = macro_start + macro_literal.len();
    let rest = &existing[body_start..];
    let end_offset = rest.find(']')?;
    let body = &rest[..end_offset];
    let items: Vec<&str> = body
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if !items.contains(&entry) {
        return None;
    }
    let remaining: Vec<&str> = items.into_iter().filter(|s| s != &entry).collect();
    let now_empty = remaining.is_empty();
    let new_body = remaining.join(", ");
    let mut out = String::with_capacity(existing.len());
    out.push_str(&existing[..body_start]);
    out.push_str(&new_body);
    out.push_str(&existing[body_start + end_offset..]);
    Some((out, now_empty))
}

// ── Job registration helpers ──────────────────────────────────────────────

/// Inject `.jobs(jobs::registered_jobs())` into the `AppBuilder` chain in
/// `src/main.rs`, immediately before the `.run()` line.
///
/// Idempotent: if `.jobs(jobs::registered_jobs())` is already present the
/// function returns `existing` unchanged.  Returns `existing` unchanged when
/// no `.run()` line can be found.
#[must_use]
pub fn add_jobs_registration_to_app(existing: &str) -> String {
    const JOBS_CALL: &str = ".jobs(jobs::registered_jobs())";
    if existing.contains(JOBS_CALL) {
        return existing.to_owned();
    }
    insert_jobs_call(existing)
}

/// Insert `.jobs(jobs::registered_jobs())` before the first `.run()` line.
fn insert_jobs_call(existing: &str) -> String {
    insert_before_run_call(existing, ".jobs(jobs::registered_jobs())")
}

/// Idempotently add `entry` (e.g. `"send_welcome_email::send_welcome_email"`)
/// to the `jobs![...]` macro invocation inside `src/jobs/mod.rs`.
///
/// If no `jobs![` call exists yet, the full `registered_jobs()` function is
/// appended.  If it already exists, only the new entry is spliced in (using
/// the same bracket-scan logic as `augment_routes_body` / `augment_mail_previews_list`).
/// Idempotent: a second call with the same entry is a no-op.
#[must_use]
pub fn augment_registered_jobs(existing: &str, entry: &str) -> String {
    const JOBS_MACRO: &str = "jobs![";
    existing.find(JOBS_MACRO).map_or_else(
        || {
            // Append a fresh registered_jobs() fn.
            let trimmed = existing.trim_end();
            let sep = if trimmed.is_empty() { "" } else { "\n\n" };
            format!(
                "{trimmed}{sep}#[must_use]\npub fn registered_jobs() -> Vec<autumn_web::job::JobInfo> {{\n    autumn_web::jobs![{entry}]\n}}\n"
            )
        },
        |macro_start| {
            let body_start = macro_start + JOBS_MACRO.len();
            splice_jobs_list(existing, body_start, entry)
        },
    )
}

/// Splice `entry` into an already-present `jobs![...]` body.
fn splice_jobs_list(existing: &str, body_start: usize, entry: &str) -> String {
    splice_into_macro_body(existing, body_start, entry)
}

/// Inverse of [`augment_registered_jobs`] (`autumn destroy`, issue #1048).
///
/// Removes `entry` from the `jobs![...]` list. If it was the only entry, the
/// whole freshly-generated `registered_jobs()` function (plus its
/// `#[must_use]` attribute and the blank separator line before it) is
/// removed too, rather than leaving an empty `jobs![]` behind.
///
/// A no-op if there's no `jobs![...]`, or `entry` isn't currently listed.
#[must_use]
pub fn remove_job_entry(existing: &str, entry: &str) -> String {
    const JOBS_MACRO: &str = "jobs![";
    let Some((spliced, now_empty)) = remove_entry_from_bracketed_list(existing, JOBS_MACRO, entry)
    else {
        return existing.to_owned();
    };
    if now_empty {
        remove_registered_jobs_fn(existing)
    } else {
        spliced
    }
}

/// Remove the whole `registered_jobs()` function [`augment_registered_jobs`]
/// generates when it creates one from scratch, plus its `#[must_use]`
/// attribute and one preceding blank separator line.
fn remove_registered_jobs_fn(existing: &str) -> String {
    let lines: Vec<&str> = existing.lines().collect();
    let Some(fn_line_idx) = lines
        .iter()
        .position(|l| l.trim_start().starts_with("pub fn registered_jobs("))
    else {
        return existing.to_owned();
    };
    let start = if fn_line_idx > 0 && lines[fn_line_idx - 1].trim() == "#[must_use]" {
        fn_line_idx - 1
    } else {
        fn_line_idx
    };
    let Some(close_offset) = lines[fn_line_idx..].iter().position(|l| l.trim() == "}") else {
        return existing.to_owned();
    };
    let end = fn_line_idx + close_offset;

    let mut effective_start = start;
    if effective_start > 0 && lines[effective_start - 1].trim().is_empty() {
        effective_start -= 1;
    }

    let mut new_lines: Vec<&str> = Vec::with_capacity(lines.len());
    new_lines.extend_from_slice(&lines[..effective_start]);
    if end + 1 < lines.len() {
        new_lines.extend_from_slice(&lines[end + 1..]);
    }
    let mut out = new_lines.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    out
}

/// Inverse of [`add_jobs_registration_to_app`] (`autumn destroy`, issue #1048).
///
/// Removes the `.jobs(jobs::registered_jobs())` line from the `AppBuilder`
/// chain. A no-op if it isn't present.
#[must_use]
pub fn remove_jobs_registration_from_app(existing: &str) -> String {
    const JOBS_CALL: &str = ".jobs(jobs::registered_jobs())";
    let lines: Vec<&str> = existing.lines().collect();
    let Some(idx) = lines.iter().position(|l| l.trim() == JOBS_CALL) else {
        return existing.to_owned();
    };
    remove_single_line(&lines, idx, existing.ends_with('\n'))
}

// ── Policy registration helpers (issue #1125) ────────────────────────────

/// The `.policy::<...>(...)` builder call registering `{pascal}`'s policy.
///
/// `model_path` is the fully-qualified path to the model type — either
/// `crate::models::<snake>::<Pascal>` (per-resource `src/models/<snake>.rs`) or
/// `crate::models::<Pascal>` (single-file `src/models.rs`). The `crate::policies`
/// path is layout-independent (policies always live in `src/policies/<snake>.rs`).
fn policy_registration_call(model_path: &str, pascal: &str, snake: &str) -> String {
    format!(".policy::<{model_path}, _>(crate::policies::{snake}::{pascal}Policy::default())")
}

/// The `.scope::<...>(...)` builder call registering `{pascal}`'s scope.
fn scope_registration_call(model_path: &str, pascal: &str, snake: &str) -> String {
    format!(".scope::<{model_path}, _>(crate::policies::{snake}::{pascal}Scope::default())")
}

/// Whether `line` is the `.policy::<...>(...)` registration for `{pascal}`,
/// keyed on the layout-independent `crate::policies::<snake>::<Pascal>Policy`
/// suffix so it matches regardless of which model-file layout produced the
/// (variable) `crate::models::…` type argument — important for destroy, which
/// may run after the model (and thus the knowledge of its layout) is gone.
fn is_policy_registration_line(line: &str, pascal: &str, snake: &str) -> bool {
    let trimmed = line.trim();
    trimmed.starts_with(".policy::<")
        && trimmed.ends_with(&format!(
            "(crate::policies::{snake}::{pascal}Policy::default())"
        ))
}

/// Whether `line` is the `.scope::<...>(...)` registration for `{pascal}` — see
/// [`is_policy_registration_line`].
fn is_scope_registration_line(line: &str, pascal: &str, snake: &str) -> bool {
    let trimmed = line.trim();
    trimmed.starts_with(".scope::<")
        && trimmed.ends_with(&format!(
            "(crate::policies::{snake}::{pascal}Scope::default())"
        ))
}

/// Inject `.policy::<...>(...)` and `.scope::<...>(...)` for `{pascal}` into
/// the `AppBuilder` chain in `src/main.rs`, immediately before the `.run()`
/// line (issue #1125). `model_path` is the fully-qualified model type path,
/// honoring the project's model-file layout (see [`policy_registration_call`]).
///
/// Idempotent: if `{pascal}`'s policy registration is already present the
/// function returns `existing` unchanged. Returns `existing` unchanged when no
/// `.run()` line can be found.
#[must_use]
pub fn add_policy_registration_to_app(
    existing: &str,
    model_path: &str,
    pascal: &str,
    snake: &str,
) -> String {
    if existing
        .lines()
        .any(|l| is_policy_registration_line(l, pascal, snake))
    {
        return existing.to_owned();
    }
    let with_policy = insert_before_run_call(
        existing,
        &policy_registration_call(model_path, pascal, snake),
    );
    insert_before_run_call(
        &with_policy,
        &scope_registration_call(model_path, pascal, snake),
    )
}

/// Inverse of [`add_policy_registration_to_app`] (`autumn destroy`, issue #1048).
///
/// Removes the `.policy::<...>(...)` and `.scope::<...>(...)` lines for
/// `{pascal}` from the `AppBuilder` chain. A no-op if neither is present.
/// Unlike [`remove_jobs_registration_from_app`] (which shares one `.jobs(...)`
/// call across every job), each resource carries its own pair of registration
/// lines keyed by its type, so removal is per-resource — no sibling
/// directory check is needed.
#[must_use]
pub fn remove_policy_registration_from_app(existing: &str, pascal: &str, snake: &str) -> String {
    let is_reg = |l: &str| {
        is_policy_registration_line(l, pascal, snake)
            || is_scope_registration_line(l, pascal, snake)
    };
    if !existing.lines().any(is_reg) {
        return existing.to_owned();
    }
    let kept: Vec<&str> = existing.lines().filter(|l| !is_reg(l)).collect();
    let mut out = kept.join("\n");
    if existing.ends_with('\n') && !out.is_empty() {
        out.push('\n');
    }
    out
}

// ── Cargo.toml: feature injection ────────────────────────────────────────

/// Rewrite `lines` so that a feature is added at or near `feat_idx`.
///
/// Split a TOML line into its value portion and any trailing inline comment
/// (`# ...`).  The split point is the first `#` that is not inside a
/// double-quoted string.  Returns `(value, comment)` where `comment` is
/// empty when there is no inline comment.
fn split_toml_inline_comment(s: &str) -> (&str, &str) {
    let mut in_string = false;
    let mut prev_was_backslash = false;
    for (i, c) in s.char_indices() {
        if in_string {
            if c == '\\' && !prev_was_backslash {
                prev_was_backslash = true;
                continue;
            } else if c == '"' && !prev_was_backslash {
                in_string = false;
            }
        } else if c == '"' {
            in_string = true;
        } else if c == '#' {
            return (&s[..i], &s[i..]);
        }
        prev_was_backslash = false;
    }
    (s, "")
}

/// If `new_feat_line != original_line` the single-line `features = [...]` was
/// already rewritten; just splice it in. Otherwise the array is multiline:
/// scan forward for the closing `]` and insert a new entry before it.
fn splice_feature_at(
    lines: &[&str],
    feat_idx: usize,
    new_feat_line: &str,
    original_line: &str,
    feature_quoted: &str,
    ends_with_newline: bool,
) -> String {
    let mut out = String::with_capacity(lines.len() * 40);
    if new_feat_line == original_line {
        let close_idx = lines[feat_idx..]
            .iter()
            .position(|l| {
                // Strip a trailing TOML comment before comparing so that
                // `] # framework features` is recognised as the closing bracket.
                l.trim()
                    .split_once('#')
                    .map_or_else(|| l.trim(), |(before, _)| before.trim())
                    == "]"
            })
            .map_or(feat_idx, |p| feat_idx + p);
        let indent = lines
            .get(feat_idx + 1)
            .filter(|l| !l.trim().is_empty() && l.trim() != "]")
            .map_or("    ", |l| &l[..l.len() - l.trim_start().len()]);
        let new_entry = format!("{indent}{feature_quoted},");
        for (k, &l) in lines.iter().enumerate() {
            if k == close_idx {
                out.push_str(&new_entry);
                out.push('\n');
                out.push_str(l);
            } else if k + 1 == close_idx && k > feat_idx {
                // Ensure the last existing item has a trailing comma before the
                // new entry is inserted; without it the TOML would be invalid.
                // Split off any inline comment first so the comma lands in the
                // value portion, not inside `# ...` where TOML ignores it.
                let (code, comment) = split_toml_inline_comment(l);
                let code_trimmed = code.trim_end();
                if !code_trimmed.is_empty() && !code_trimmed.ends_with(',') {
                    out.push_str(code_trimmed);
                    out.push(',');
                    if !comment.is_empty() {
                        out.push(' ');
                        out.push_str(comment.trim_start());
                    }
                } else {
                    out.push_str(l);
                }
            } else {
                out.push_str(l);
            }
            out.push('\n');
        }
    } else {
        for (k, &l) in lines.iter().enumerate() {
            out.push_str(if k == feat_idx { new_feat_line } else { l });
            out.push('\n');
        }
    }
    if !ends_with_newline {
        out.pop();
    }
    out
}

/// Add `feature` to a multiline inline TOML table for `autumn-web`
/// (e.g. `autumn-web = {\n  ...\n}`).
///
/// Returns `None` if the table is malformed (no closing `}`).
/// Returns `Some(out)` with the (possibly modified) complete document otherwise.
fn add_feature_to_multiline_inline_table(
    lines: &[&str],
    open_idx: usize,
    existing: &str,
    feature: &str,
    feature_quoted: &str,
) -> Option<String> {
    let close_idx = lines[open_idx + 1..]
        .iter()
        .position(|l| l.trim_start().starts_with('}'))
        .map(|p| open_idx + 1 + p)?;

    if lines[open_idx..=close_idx].iter().any(|l| {
        let line_code = l.split_once('#').map_or(*l, |(before, _)| before);
        line_code.contains(feature_quoted)
    }) {
        return Some(existing.to_owned());
    }

    for (j, &sec_line) in lines[open_idx + 1..close_idx].iter().enumerate() {
        if sec_line.trim_start().starts_with("features") {
            return Some(splice_feature_at(
                lines,
                open_idx + 1 + j,
                &rewrite_features_line(sec_line, feature),
                sec_line,
                feature_quoted,
                existing.ends_with('\n'),
            ));
        }
    }

    let indent = lines[close_idx]
        .chars()
        .take_while(char::is_ascii_whitespace)
        .collect::<String>();
    let new_feat = format!("{indent}features = [{feature_quoted}],");

    // Ensure the last entry before `}` has a trailing comma (required between inline-table entries).
    let last_entry_idx = lines[open_idx + 1..close_idx]
        .iter()
        .rposition(|l| !l.trim().is_empty())
        .map(|p| open_idx + 1 + p);

    let mut out = String::with_capacity(existing.len() + 32);
    for (k, &l) in lines.iter().enumerate() {
        if k == close_idx {
            out.push_str(&new_feat);
            out.push('\n');
        }
        if Some(k) == last_entry_idx && !l.trim_end().ends_with(',') {
            out.push_str(l.trim_end());
            out.push(',');
        } else {
            out.push_str(l);
        }
        out.push('\n');
    }
    if !existing.ends_with('\n') {
        out.pop();
    }
    Some(out)
}

/// Handle the dotted key form `<dep_name>.* = ...` inside a dependency
/// section (`[dependencies]` or `[dev-dependencies]`, per `section`).
///
/// Looks for an existing `<dep_name>.features` key in the section to splice
/// into; if none is found, inserts one immediately after `dep_line_idx`.
fn patch_dotted_dep(
    lines: &[&str],
    dep_line_idx: usize,
    existing: &str,
    dep_name: &str,
    feature: &str,
    feature_quoted: &str,
    section: &str,
) -> String {
    let section_end = lines[dep_line_idx + 1..]
        .iter()
        .position(|l| is_section_boundary(l.trim(), section))
        .map_or(lines.len(), |p| dep_line_idx + 1 + p);

    if lines[dep_line_idx..section_end].iter().any(|l| {
        let line_code = l.split_once('#').map_or(*l, |(before, _)| before);
        // Require an exact dotted match (`<dep_name>.` ...) rather than a
        // bare prefix, so an unrelated dependency sharing the prefix (e.g.
        // `tokio-util = { features = ["rt"] }` when `dep_name` is "tokio")
        // can't be mistaken for evidence that `<dep_name>` already has the
        // feature -- that would skip actually adding it to the real dep.
        line_code
            .trim_start()
            .strip_prefix(dep_name)
            .is_some_and(|rest| rest.starts_with('.'))
            && line_code.contains(feature_quoted)
    }) {
        return existing.to_owned();
    }

    let features_key = format!("{dep_name}.features");
    for (j, &sec_line) in lines[dep_line_idx..section_end].iter().enumerate() {
        if sec_line.trim_start().starts_with(&features_key) {
            let new_line = rewrite_features_line(sec_line, feature);
            let mut out = String::with_capacity(existing.len() + 32);
            for (k, &l) in lines.iter().enumerate() {
                out.push_str(if k == dep_line_idx + j { &new_line } else { l });
                out.push('\n');
            }
            if !existing.ends_with('\n') {
                out.pop();
            }
            return out;
        }
    }

    let new_feat = format!("{features_key} = [{feature_quoted}]");
    let mut out = String::with_capacity(existing.len() + new_feat.len() + 2);
    for (k, &l) in lines.iter().enumerate() {
        out.push_str(l);
        out.push('\n');
        if k == dep_line_idx {
            out.push_str(&new_feat);
            out.push('\n');
        }
    }
    if !existing.ends_with('\n') {
        out.pop();
    }
    out
}

/// Ensure the `autumn-web` dependency in `Cargo.toml` includes `feature`.
///
/// Handles four common forms of the dependency declaration:
///
///   1. `autumn-web = "x.y.z"` → `autumn-web = { version = "x.y.z", features = ["mail"] }`
///   2. `autumn-web = { version = "x.y.z" }` → adds `features = ["mail"]`
///   3. `autumn-web = { ..., features = ["other"] }` → appends `"mail"` to the list
///   4. `[dependencies.autumn-web]` section with a separate `features` key (multiline form)
///
/// Idempotent: a second call with the same feature is a no-op.
/// Returns `existing` unchanged when the `autumn-web` dep cannot be found.
#[must_use]
pub fn ensure_autumn_web_feature(existing: &str, feature: &str) -> String {
    ensure_autumn_web_feature_status(existing, feature).0
}

/// Inverse of [`ensure_autumn_web_feature`] (`autumn destroy`, issue #1048),
/// scoped to `[dependencies]`.
///
/// Removes `feature` from `autumn-web`'s `features = [...]` list, in
/// whichever of the four shapes [`ensure_autumn_web_feature`] can add it to:
/// single-line inline table, dotted key (`autumn-web.features = [...]`),
/// multiline inline table, or a `[dependencies.autumn-web]` subtable. Only
/// the single-line inline-table case can collapse: when removing `feature`
/// empties the list and `version` is the only other key, the whole entry
/// collapses back to a bare string, restoring the pre-generate declaration
/// byte-for-byte — the other three shapes only ever lose the one list entry
/// (or the now-empty `features` line/key), since their surrounding
/// structure may predate `generate` entirely and destroy never restructures
/// content it didn't itself add.
///
/// A no-op for an entry `feature` doesn't currently list — destroy only
/// reverses what `generate` itself would have written. A renamed/aliased
/// `autumn-web` dependency (`autumn_web = { package = "autumn-web", ... }` or
/// `[dependencies.autumn_web]`) is resolved to its actual key first (issue
/// #1048 PR review) — [`ensure_autumn_web_feature_status_in_section`] adds
/// features there too, so destroy must look under the same key it did.
#[must_use]
pub fn remove_autumn_web_feature(existing: &str, feature: &str) -> String {
    let dep_key = resolve_autumn_web_dep_key(existing, "dependencies");
    remove_dep_feature_in_section(existing, dep_key, feature, "dependencies", true)
}

/// Inverse of [`ensure_dev_dependency_test_support`] (`autumn destroy`,
/// issue #1048), scoped to `[dev-dependencies]`.
///
/// Unlike [`remove_autumn_web_feature`], a fresh project's
/// `[dev-dependencies]` never has a prior `autumn-web` entry —
/// [`ensure_dev_dependency_test_support`] always *inserts* a brand-new line.
/// So when removing `feature` empties its features list, the whole line is
/// deleted outright rather than collapsed to a bare string.
///
/// A no-op for any other declaration shape, mirroring
/// [`remove_autumn_web_feature`].
#[must_use]
pub fn remove_autumn_web_dev_dependency_feature(existing: &str, feature: &str) -> String {
    let dep_key = resolve_autumn_web_dep_key(existing, "dev-dependencies");
    remove_dep_feature_in_section(existing, dep_key, feature, "dev-dependencies", false)
}

/// Which literal identifier `autumn-web`'s dependency entry uses in
/// `section` — either the plain crate name, or an importable alias declared
/// with an explicit `package = "autumn-web"` (mirrors the alias detection in
/// [`ensure_autumn_web_feature_status_in_section`], whose Pass 1 "else"
/// branch and Pass 2b add features under exactly these two alias shapes:
/// `autumn_web = { package = "autumn-web", ... }` and
/// `[<section>.autumn_web]` with `package = "autumn-web"` inside).
///
/// [`remove_dep_feature_in_section`] previously always searched for the
/// literal `"autumn-web"` key, so a project using either alias shape kept
/// its generator-added feature forever — `destroy` located no matching line
/// and silently made no edit (issue #1048 PR review).
///
/// Falls back to the literal key when no dependency entry is found at all —
/// `remove_dep_feature_in_section` is already a no-op in that case.
fn resolve_autumn_web_dep_key(existing: &str, section: &str) -> &'static str {
    let lines: Vec<&str> = existing.lines().collect();
    let mut in_section = false;
    for &line in &lines {
        let trimmed = line.trim();
        if is_section_header(trimmed, section) {
            in_section = true;
            continue;
        }
        if in_section && is_section_boundary(trimmed, section) {
            in_section = false;
            continue;
        }
        if !in_section || trimmed.starts_with('#') {
            continue;
        }
        let after_ws = line.trim_start();
        if let Some(rest) = after_ws.strip_prefix("autumn-web") {
            if rest.starts_with('.') || rest.trim_start().starts_with('=') {
                return "autumn-web";
            }
            continue;
        }
        let Some((key, val)) = after_ws.split_once('=') else {
            continue;
        };
        let key_trimmed = key.trim();
        let alias = key_trimmed
            .split_once('.')
            .map_or(key_trimmed, |(base, _)| base);
        if alias.replace('-', "_") == "autumn_web" && declares_package(val, "autumn-web") {
            return "autumn_web";
        }
    }

    let literal_subtable_key = format!("[{section}.autumn-web]");
    if lines
        .iter()
        .any(|l| l.trim().split('#').next().unwrap_or("").trim() == literal_subtable_key)
    {
        return "autumn-web";
    }
    let renamed_subtable_key = format!("[{section}.autumn_web]");
    if find_section_start_with_autumn_web_package(&lines, &renamed_subtable_key).is_some() {
        return "autumn_web";
    }

    "autumn-web"
}

/// How a single-line dependency declaration should be rewritten once a
/// feature has been removed from its `features = [...]` array.
enum FeatureRemovalEdit {
    /// Keep the line, with this new full text.
    Replace(String),
    /// Delete the line entirely.
    Delete,
    /// Leave the file completely unchanged — either `feature` wasn't found,
    /// or the shape doesn't confidently invert.
    Unchanged,
}

/// Shared implementation behind [`remove_autumn_web_feature`] and
/// [`remove_autumn_web_dev_dependency_feature`]. See their docs for the two
/// `collapse_to_bare` behaviours.
///
/// Beyond the single-line inline-table shape [`parse_feature_removal`]
/// handles, this also inverts the dotted-key (`<dep_name>.features =
/// [...]`), multiline inline-table, and `[<section>.<dep_name>]` subtable
/// forms [`ensure_autumn_web_feature_status_in_section`] can add a feature
/// to (issue #1048 PR review) — `generate mailer`/`channel --ws` otherwise
/// left the feature behind forever in a hand-maintained `Cargo.toml` using
/// one of those shapes. Each of those three only ever edits the `features`
/// line itself, never collapsing or restructuring the surrounding
/// declaration — unlike the single-line case, that structure may predate
/// `generate` entirely, so only what `generate` itself would have inserted
/// (an entry in the list) is reverted.
fn remove_dep_feature_in_section(
    existing: &str,
    dep_name: &str,
    feature: &str,
    section: &str,
    collapse_to_bare: bool,
) -> String {
    let feature_quoted = format!("\"{feature}\"");
    let lines: Vec<&str> = existing.lines().collect();
    let dotted_features_key = format!("{dep_name}.features");
    let mut in_section = false;
    for (i, &line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if is_section_header(trimmed, section) {
            in_section = true;
            continue;
        }
        if in_section && is_section_boundary(trimmed, section) {
            in_section = false;
            continue;
        }
        if !in_section {
            continue;
        }

        if line.trim_start().starts_with(&dotted_features_key)
            && let Some((new_line, now_empty)) =
                remove_feature_from_features_line(line, &feature_quoted)
        {
            return if now_empty {
                remove_single_line(&lines, i, existing.ends_with('\n'))
            } else {
                splice_single_line(&lines, i, &new_line, existing.ends_with('\n'))
            };
        }

        let edit = parse_feature_removal(line, dep_name, &feature_quoted, collapse_to_bare);
        match edit {
            FeatureRemovalEdit::Replace(new_line) => {
                return splice_single_line(&lines, i, &new_line, existing.ends_with('\n'));
            }
            FeatureRemovalEdit::Delete => {
                return remove_single_line(&lines, i, existing.ends_with('\n'));
            }
            FeatureRemovalEdit::Unchanged => {
                if let Some(result) = remove_feature_from_open_multiline_inline_table(
                    &lines,
                    i,
                    existing,
                    dep_name,
                    &feature_quoted,
                ) {
                    return result;
                }
            }
        }
    }

    // Pass 2: multiline subtable form `[<section>.<dep_name>]`.
    let subtable_key = format!("[{section}.{dep_name}]");
    for (i, &line) in lines.iter().enumerate() {
        let key_part = line.trim().split('#').next().unwrap_or("").trim();
        if key_part != subtable_key {
            continue;
        }
        let section_start = i + 1;
        let section_end = lines[section_start..]
            .iter()
            .position(|l| {
                let t = l.trim();
                t.starts_with('[') && !t.is_empty()
            })
            .map_or(lines.len(), |p| section_start + p);
        if let Some(result) = remove_feature_from_deps_section(
            &lines,
            section_start,
            section_end,
            existing,
            &feature_quoted,
        ) {
            return result;
        }
    }

    existing.to_owned()
}

/// Remove `feature_quoted` from a standalone `key = [...]` TOML line
/// (inverse of [`rewrite_features_line`]). `None` if the line has no
/// bracketed array, or it doesn't list `feature_quoted`. Otherwise returns
/// the rewritten line and whether the array is now empty.
fn remove_feature_from_features_line(line: &str, feature_quoted: &str) -> Option<(String, bool)> {
    let open = line.find('[')?;
    let close_rel = line[open..].find(']')?;
    let abs_end = open + close_rel;
    let body = &line[open + 1..abs_end];
    let items: Vec<&str> = body
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if !items.contains(&feature_quoted) {
        return None;
    }
    let remaining: Vec<&str> = items
        .into_iter()
        .filter(|it| *it != feature_quoted)
        .collect();
    let now_empty = remaining.is_empty();
    let new_line = format!(
        "{}{}{}",
        &line[..=open],
        remaining.join(", "),
        &line[abs_end..]
    );
    Some((new_line, now_empty))
}

/// If `lines[open_idx]` opens a multiline inline-table declaration for
/// `dep_name` (`<dep_name> = {` with no closing `}` on the same line),
/// removes `feature_quoted` from its `features = [...]` entry — inverse of
/// [`add_feature_to_multiline_inline_table`]. Only edits the `features`
/// line; never collapses the surrounding table, since it may predate
/// `generate` entirely. `None` if `lines[open_idx]` isn't that opening
/// shape, no closing `}` is found, no `features` line exists inside the
/// block, or that line doesn't list `feature_quoted`.
fn remove_feature_from_open_multiline_inline_table(
    lines: &[&str],
    open_idx: usize,
    existing: &str,
    dep_name: &str,
    feature_quoted: &str,
) -> Option<String> {
    let after_ws = lines[open_idx].trim_start();
    let rest = after_ws.strip_prefix(dep_name)?;
    let rest = rest.trim_start().strip_prefix('=')?;
    let rest = rest.trim_start().strip_prefix('{')?;
    if rest.contains('}') {
        return None; // closes on the same line -- not the multiline form.
    }
    let close_idx = lines[open_idx + 1..]
        .iter()
        .position(|l| l.trim_start().starts_with('}'))
        .map(|p| open_idx + 1 + p)?;
    remove_feature_from_deps_section(lines, open_idx + 1, close_idx, existing, feature_quoted)
}

/// Remove `feature_quoted` from a `features = [...]` line found inside
/// `lines[section_start..section_end)` (a multiline inline-table body or a
/// `[<section>.<dep_name>]` subtable body). Only that one line is ever
/// changed — every other key in the span is left untouched. `None` if no
/// `features` line is found there, or it doesn't list `feature_quoted`.
fn remove_feature_from_deps_section(
    lines: &[&str],
    section_start: usize,
    section_end: usize,
    existing: &str,
    feature_quoted: &str,
) -> Option<String> {
    for (j, &sect_line) in lines[section_start..section_end].iter().enumerate() {
        if !sect_line.trim_start().starts_with("features") {
            continue;
        }
        let idx = section_start + j;
        let (new_line, now_empty) = remove_feature_from_features_line(sect_line, feature_quoted)?;
        return Some(if now_empty {
            remove_single_line(lines, idx, existing.ends_with('\n'))
        } else {
            splice_single_line(lines, idx, &new_line, existing.ends_with('\n'))
        });
    }
    None
}

/// Find the `features` key inside an inline-table `body` (e.g.
/// `version = "1", default-features = false, features = ["mail"]`), as a
/// whole key — not a substring match that a plain `body.find("features")`
/// would also hit inside `default-features` when that key comes first
/// (issue #1048 PR review: `default-features = false, features = ["mail"]`
/// would otherwise truncate `before_features` mid-word at `default-` and
/// rewrite the dependency into invalid TOML). Requires the match to be
/// preceded by the body start or a non-identifier character, and followed
/// by optional whitespace then `=`.
fn find_features_key(body: &str) -> Option<usize> {
    let mut search_from = 0;
    while let Some(rel) = body[search_from..].find("features") {
        let idx = search_from + rel;
        let is_key_start = idx == 0
            || !matches!(body.as_bytes()[idx - 1], b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' | b'-');
        let is_key_end = body[idx + "features".len()..].trim_start().starts_with('=');
        if is_key_start && is_key_end {
            return Some(idx);
        }
        search_from = idx + "features".len();
    }
    None
}

/// Parse `line` as `{indent}{dep_name} = { ...features = [...] ... }` and
/// decide how to rewrite it once `feature_quoted` is removed from the
/// features array. Returns [`FeatureRemovalEdit::Unchanged`] whenever the
/// line doesn't match that exact single-line inline-table shape, or doesn't
/// currently list the feature.
fn parse_feature_removal(
    line: &str,
    dep_name: &str,
    feature_quoted: &str,
    collapse_to_bare: bool,
) -> FeatureRemovalEdit {
    let after_ws = line.trim_start();
    let indent = &line[..line.len() - after_ws.len()];
    let Some(rest) = after_ws.strip_prefix(dep_name) else {
        return FeatureRemovalEdit::Unchanged;
    };
    let Some(rest) = rest.trim_start().strip_prefix('=') else {
        return FeatureRemovalEdit::Unchanged;
    };
    let Some(rest) = rest.trim_start().strip_prefix('{') else {
        return FeatureRemovalEdit::Unchanged;
    };
    let Some(body_end) = rest.rfind('}') else {
        return FeatureRemovalEdit::Unchanged;
    };
    let body = rest[..body_end].trim();
    let Some(features_kw) = find_features_key(body) else {
        return FeatureRemovalEdit::Unchanged;
    };
    let before_features = body[..features_kw].trim().trim_end_matches(',').trim();
    let after_kw = &body[features_kw + "features".len()..];
    let Some(bracket_open) = after_kw.find('[') else {
        return FeatureRemovalEdit::Unchanged;
    };
    let Some(bracket_close) = after_kw.find(']') else {
        return FeatureRemovalEdit::Unchanged;
    };
    if bracket_close < bracket_open {
        return FeatureRemovalEdit::Unchanged;
    }
    let list_body = &after_kw[bracket_open + 1..bracket_close];
    let after_list = after_kw[bracket_close + 1..]
        .trim()
        .trim_start_matches(',')
        .trim();
    let items: Vec<&str> = list_body
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if !items.contains(&feature_quoted) {
        return FeatureRemovalEdit::Unchanged;
    }
    let remaining: Vec<&str> = items.into_iter().filter(|s| *s != feature_quoted).collect();

    if !remaining.is_empty() {
        let sep = if before_features.is_empty() { "" } else { ", " };
        let after_sep = if after_list.is_empty() { "" } else { ", " };
        return FeatureRemovalEdit::Replace(format!(
            "{indent}{dep_name} = {{ {before_features}{sep}features = [{}]{after_sep}{after_list} }}",
            remaining.join(", "),
        ));
    }

    if collapse_to_bare {
        if !after_list.is_empty() {
            return FeatureRemovalEdit::Unchanged;
        }
        if let Some(v) = before_features
            .strip_prefix("version")
            .map(str::trim_start)
            .and_then(|r| r.strip_prefix('='))
        {
            return FeatureRemovalEdit::Replace(format!("{indent}{dep_name} = {}", v.trim()));
        }
        // Some other key remains besides the now-empty `features` array —
        // e.g. a renamed dep's `package = "autumn-web"` (issue #1048 PR
        // review), or `default-features = false`. Collapsing to a bare
        // string would silently drop that key, so just remove the
        // `features` key and keep the rest of the inline table intact.
        if before_features.is_empty() {
            return FeatureRemovalEdit::Unchanged;
        }
        return FeatureRemovalEdit::Replace(format!(
            "{indent}{dep_name} = {{ {before_features} }}"
        ));
    }

    FeatureRemovalEdit::Delete
}

/// Replace line `idx` with `new_line`, preserving the file's trailing-newline status.
pub(super) fn splice_single_line(
    lines: &[&str],
    idx: usize,
    new_line: &str,
    ends_with_newline: bool,
) -> String {
    let mut out = String::with_capacity(lines.len() * 24 + new_line.len());
    for (i, &l) in lines.iter().enumerate() {
        out.push_str(if i == idx { new_line } else { l });
        out.push('\n');
    }
    if !ends_with_newline {
        out.pop();
    }
    out
}

/// Remove line `idx` entirely, preserving the file's trailing-newline status.
pub(super) fn remove_single_line(lines: &[&str], idx: usize, ends_with_newline: bool) -> String {
    let mut out = String::with_capacity(lines.len() * 24);
    for (i, &l) in lines.iter().enumerate() {
        if i == idx {
            continue;
        }
        out.push_str(l);
        out.push('\n');
    }
    if !ends_with_newline && out.ends_with('\n') {
        out.pop();
    }
    out
}

/// Like [`ensure_autumn_web_feature`], but also reports whether the `autumn-web`
/// dependency ends up carrying `feature`. The `bool` is `false` only when no
/// `autumn-web` dependency could be located (so the caller can warn that the
/// feature must be enabled by hand); it is `true` when the feature was added or
/// was already present.
#[must_use]
pub fn ensure_autumn_web_feature_status(existing: &str, feature: &str) -> (String, bool) {
    ensure_autumn_web_feature_status_in_section(existing, feature, "dependencies")
}

/// Like [`ensure_autumn_web_feature_status`], but targets an arbitrary
/// dependency section (`"dependencies"` or `"dev-dependencies"`) instead of
/// always assuming `[dependencies]`. Shared by [`ensure_autumn_web_feature_status`]
/// and [`ensure_dev_dependency_test_support`] so both sections get the same
/// handling of every `autumn-web` declaration shape (inline, dotted-key,
/// multiline subtable, renamed/aliased dep).
fn ensure_autumn_web_feature_status_in_section(
    existing: &str,
    feature: &str,
    section: &str,
) -> (String, bool) {
    let feature_quoted = format!("\"{feature}\"");
    let lines: Vec<&str> = existing.lines().collect();
    let mut in_section = false;

    // Pass 1: inline form under the section header.
    for (i, &line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if is_section_header(trimmed, section) {
            in_section = true;
            continue;
        }
        if in_section && is_section_boundary(trimmed, section) {
            in_section = false;
            continue;
        }
        if !in_section {
            continue;
        }
        // Skip commented-out lines so that a commented dep like
        //   # aw = { package = "autumn-web" }
        // does not shadow the real dependency below it.
        if trimmed.starts_with('#') {
            continue;
        }
        // Match either the exact `autumn-web` key or a renamed dep with `package = "autumn-web"`.
        let after_ws = line.trim_start();
        let dep_prefix = if let Some(rest) = after_ws.strip_prefix("autumn-web") {
            if rest.starts_with('.') {
                // Dotted key form: autumn-web.workspace = true, autumn-web.features = [...], etc.
                return (
                    patch_dotted_dep(
                        &lines,
                        i,
                        existing,
                        "autumn-web",
                        feature,
                        &feature_quoted,
                        section,
                    ),
                    true,
                );
            }
            if rest.starts_with(|c: char| c != '=' && !c.is_whitespace()) {
                continue;
            }
            "autumn-web"
        } else {
            // Check for a renamed dep: `aw = { package = "autumn-web", ... }`.
            let val = after_ws.split_once('=').map_or("", |x| x.1);
            if !declares_package(val, "autumn-web") {
                continue;
            }
            // The alias must be importable as `autumn_web`; an alias such as
            // `aw` produces a crate named `aw`, not `autumn_web`, so the
            // generated code (`use autumn_web::...`) would fail to compile.
            let alias = after_ws.split_once('=').map_or("", |(k, _)| k.trim());
            if alias.replace('-', "_") != "autumn_web" {
                continue;
            }
            alias
        };
        // Idempotency check: strip any trailing TOML comment so that a line such as
        //   autumn-web = { version = "0.6" } # add "inbound-mailgun" later
        // does not falsely appear to already have the feature enabled.
        let line_code = line.split_once('#').map_or(line, |(before, _)| before);
        if line_code.contains(&feature_quoted) {
            return (existing.to_owned(), true);
        }
        let new_line = rewrite_dep_with_feature(line, dep_prefix, feature);
        if new_line == line {
            // Multiline inline table — delegate to helper.
            match add_feature_to_multiline_inline_table(
                &lines,
                i,
                existing,
                feature,
                &feature_quoted,
            ) {
                None => continue,
                Some(result) => return (result, true),
            }
        }
        let mut out = String::with_capacity(existing.len() + 32);
        for (j, &l) in lines.iter().enumerate() {
            out.push_str(if j == i { &new_line } else { l });
            out.push('\n');
        }
        if !existing.ends_with('\n') {
            out.pop();
        }
        return (out, true);
    }

    // Pass 2: multiline section form `[<section>.autumn-web]`.
    let subtable_key = format!("[{section}.autumn-web]");
    for (i, &line) in lines.iter().enumerate() {
        // Strip trailing TOML line-comment before comparing the section header.
        let key_part = line.trim().split('#').next().unwrap_or("").trim();
        if key_part != subtable_key {
            continue;
        }
        return (
            add_feature_to_deps_section(&lines, i + 1, existing, feature, &feature_quoted),
            true,
        );
    }

    // Pass 2b: `[<section>.autumn_web]` table section whose body declares
    // `package = "autumn-web"` — Cargo's table-key form of a renamed dep.
    let renamed_subtable_key = format!("[{section}.autumn_web]");
    if let Some(start) = find_section_start_with_autumn_web_package(&lines, &renamed_subtable_key) {
        return (
            add_feature_to_deps_section(&lines, start, existing, feature, &feature_quoted),
            true,
        );
    }

    (existing.to_owned(), false)
}

/// Like [`ensure_autumn_web_feature_status_in_section`], but for an
/// arbitrary dependency name instead of always `autumn-web`.
///
/// Handles the literal-key inline form, the dotted-key form, and the
/// multiline `[<section>.<dep_name>]` subtable form -- but *not* a
/// renamed/aliased dependency (`x = { package = "<dep_name>", ... }`).
/// `autumn-web` is the one dependency projects realistically rename (to
/// dodge the hyphen); nothing else in a generated project's `Cargo.toml`
/// needs that, so the extra alias-detection complexity stays specific to
/// [`ensure_autumn_web_feature_status_in_section`] instead of being carried
/// here for every caller.
fn ensure_dep_feature_status_in_section(
    existing: &str,
    dep_name: &str,
    feature: &str,
    section: &str,
) -> (String, bool) {
    let feature_quoted = format!("\"{feature}\"");
    let lines: Vec<&str> = existing.lines().collect();
    let mut in_section = false;

    // Pass 1: literal-key inline or dotted-key form.
    for (i, &line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if is_section_header(trimmed, section) {
            in_section = true;
            continue;
        }
        if in_section && is_section_boundary(trimmed, section) {
            in_section = false;
            continue;
        }
        if !in_section || trimmed.starts_with('#') {
            continue;
        }
        let after_ws = line.trim_start();
        let Some(rest) = after_ws.strip_prefix(dep_name) else {
            continue;
        };
        if rest.starts_with('.') {
            return (
                patch_dotted_dep(
                    &lines,
                    i,
                    existing,
                    dep_name,
                    feature,
                    &feature_quoted,
                    section,
                ),
                true,
            );
        }
        if rest.starts_with(|c: char| c != '=' && !c.is_whitespace()) {
            // A different dependency sharing this prefix -- keep scanning.
            continue;
        }
        let line_code = line.split_once('#').map_or(line, |(before, _)| before);
        if line_code.contains(&feature_quoted) {
            return (existing.to_owned(), true);
        }
        let new_line = rewrite_dep_with_feature(line, dep_name, feature);
        if new_line == line {
            match add_feature_to_multiline_inline_table(
                &lines,
                i,
                existing,
                feature,
                &feature_quoted,
            ) {
                None => continue,
                Some(result) => return (result, true),
            }
        }
        let mut out = String::with_capacity(existing.len() + 32);
        for (j, &l) in lines.iter().enumerate() {
            out.push_str(if j == i { &new_line } else { l });
            out.push('\n');
        }
        if !existing.ends_with('\n') {
            out.pop();
        }
        return (out, true);
    }

    // Pass 2: multiline `[<section>.<dep_name>]` subtable form.
    let subtable_key = format!("[{section}.{dep_name}]");
    for (i, &line) in lines.iter().enumerate() {
        let key_part = line.trim().split('#').next().unwrap_or("").trim();
        if key_part != subtable_key {
            continue;
        }
        return (
            add_feature_to_deps_section(&lines, i + 1, existing, feature, &feature_quoted),
            true,
        );
    }

    (existing.to_owned(), false)
}

/// Ensure `dep_name`'s `[dependencies]` entry lists `feature`, adding it to the
/// existing `features = [...]` list (in any declaration shape) when missing.
///
/// If the dependency isn't declared in `[dependencies]` at all, the input is
/// returned unchanged — callers that need the dependency itself present must
/// add it separately (e.g. via `plan_cargo_deps`/`ensure_cargo_dependencies`).
/// Idempotent: a second call is a no-op once the feature is present.
#[must_use]
pub(super) fn ensure_dependency_feature(existing: &str, dep_name: &str, feature: &str) -> String {
    ensure_dep_feature_status_in_section(existing, dep_name, feature, "dependencies").0
}

/// Ensure `[dev-dependencies]` carries a `tokio` entry with the `rt` and
/// `macros` features that a generated `#[tokio::test]` smoke test needs to
/// compile.
///
/// Every `autumn new` project template already declares this (see
/// `templates/Cargo.toml.tmpl`), but a hand-rolled project -- or one where
/// the entry was edited down -- might not. `cargo test --tests` still
/// compiles `#[ignore]`d tests, so a missing (or feature-incomplete) `tokio`
/// dev-dependency leaves an otherwise-valid project unable to compile its
/// test targets at all.
///
/// If there's no existing `tokio` dev-dependency, the new entry mirrors
/// whatever `[dependencies]` declares for `tokio` (crates.io version,
/// `workspace = true`, `path`, `git`, etc.) via [`detect_dependencies_source`],
/// for the same reason [`ensure_dev_dependency_test_support`] mirrors
/// `autumn-web`'s source: Cargo requires every declaration of a dependency
/// to unify to one source across build targets, so defaulting to a
/// crates.io version unconditionally would break `cargo` entirely for any
/// project that sources `tokio` from the workspace or a local path/git
/// checkout in `[dependencies]`.
///
/// Idempotent: a second call is a no-op once both features are present.
#[must_use]
pub fn ensure_dev_dependency_tokio_test_features(existing: &str) -> String {
    let (updated, found_rt) =
        ensure_dep_feature_status_in_section(existing, "tokio", "rt", "dev-dependencies");
    let (updated, found_macros) =
        ensure_dep_feature_status_in_section(&updated, "tokio", "macros", "dev-dependencies");
    if found_rt && found_macros {
        return updated;
    }

    // No existing `tokio` dev-dependency at all -- insert one with both
    // features. (If it existed but was missing one of the features, the two
    // ensure_dep_feature_status_in_section calls above already added it.)
    let lines: Vec<&str> = existing.lines().collect();
    let source = detect_dependencies_source(existing, "tokio")
        .unwrap_or_else(|| "version = \"1\"".to_string());
    let new_dep_line = format!("tokio = {{ {source}, features = [\"rt\", \"macros\"] }}");

    let Some(header_idx) = lines
        .iter()
        .position(|l| is_section_header(l.trim(), "dev-dependencies"))
    else {
        let mut out = existing.to_owned();
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        if !out.is_empty() && !out.ends_with("\n\n") {
            out.push('\n');
        }
        out.push_str("[dev-dependencies]\n");
        let _ = writeln!(out, "{new_dep_line}");
        return out;
    };

    let mut out = String::with_capacity(existing.len() + 96);
    for (j, &l) in lines.iter().enumerate() {
        out.push_str(l);
        out.push('\n');
        if j == header_idx {
            let _ = writeln!(out, "{new_dep_line}");
        }
    }
    if !existing.ends_with('\n') {
        out.pop();
    }
    out
}

/// Ensure `[dev-dependencies]` carries an `autumn-web` entry with the
/// `test-support` feature, which enables `TestDb` (the shared Postgres
/// testcontainer that generated `TestApp`-based integration tests use).
///
/// `autumn-web` is intentionally left out of `test-support` in
/// `[dependencies]` — production builds must not pull in the
/// `testcontainers`/`testcontainers-modules` dependency tree that the feature
/// enables. Cargo unifies features across every declaration of the same
/// dependency in the build graph, so a *dev*-only entry is enough to light up
/// `TestDb` for `cargo test` while release builds stay lean.
///
/// Reuses [`ensure_autumn_web_feature_status_in_section`] for the "there's
/// already an `autumn-web` entry in `[dev-dependencies]`" case, so every
/// declaration shape that function understands for `[dependencies]` (inline,
/// dotted-key `autumn-web.workspace = true`, multiline `[dev-dependencies.autumn-web]`
/// subtable, renamed/aliased dep) is handled here too, instead of a
/// less-capable reimplementation. Only the "no `autumn-web` entry yet" case is
/// bespoke: unlike `[dependencies]` (which every `autumn new` project already
/// declares `autumn-web` in), a fresh project's `[dev-dependencies]` has no
/// `autumn-web` line at all, so this inserts one. Its source mirrors whatever
/// `[dependencies]` uses (crates.io version, `workspace = true`, `path`, or
/// `git`) via [`detect_dependencies_autumn_web_source`] -- Cargo requires
/// every declaration of a dependency to unify to one source across build
/// targets, so defaulting to a crates.io version unconditionally would break
/// `cargo` entirely for any project that inherits `autumn-web` from the
/// workspace or a local path/git checkout.
///
/// Idempotent: a second call is a no-op once the feature is present.
#[must_use]
pub fn ensure_dev_dependency_test_support(existing: &str, autumn_version: &str) -> String {
    let (updated, found) =
        ensure_autumn_web_feature_status_in_section(existing, "test-support", "dev-dependencies");
    if found {
        return updated;
    }

    let feature_quoted = "\"test-support\"";
    let source = detect_dependencies_autumn_web_source(existing)
        .unwrap_or_else(|| format!("version = \"{autumn_version}\""));
    let new_dep_line = format!("autumn-web = {{ {source}, features = [{feature_quoted}] }}");
    let lines: Vec<&str> = existing.lines().collect();

    let Some(header_idx) = lines
        .iter()
        .position(|l| is_section_header(l.trim(), "dev-dependencies"))
    else {
        // No [dev-dependencies] section yet -- append one. (Every project
        // scaffolded by `autumn new` already has one for the `tokio` test
        // dep, so this branch only guards hand-edited Cargo.toml files.)
        let mut out = existing.to_owned();
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        if !out.is_empty() && !out.ends_with("\n\n") {
            out.push('\n');
        }
        out.push_str("[dev-dependencies]\n");
        let _ = writeln!(out, "{new_dep_line}");
        return out;
    };

    // Section exists but has no `autumn-web` line yet -- insert one right
    // after the section header.
    let mut out = String::with_capacity(existing.len() + 96);
    for (j, &l) in lines.iter().enumerate() {
        out.push_str(l);
        out.push('\n');
        if j == header_idx {
            let _ = writeln!(out, "{new_dep_line}");
        }
    }
    if !existing.ends_with('\n') {
        out.pop();
    }
    out
}

/// TOML keys that determine *where* a dependency resolves from. Cargo
/// unifies every declaration of a given dependency name to a single source
/// across build targets, so if `[dependencies]` and `[dev-dependencies]`
/// disagree on any of these, `cargo` refuses to build at all ("Dependency
/// 'autumn-web' has different source paths depending on the build target").
const SOURCE_KEYS: &[&str] = &[
    "workspace",
    "path",
    "git",
    "branch",
    "tag",
    "rev",
    "registry",
];

fn is_source_key(key: &str) -> bool {
    SOURCE_KEYS.contains(&key)
}

/// Split `s` on top-level commas, ignoring commas nested inside a quoted
/// string or a `[...]`/`{...}` value (e.g. a `features = [...]` list).
fn split_top_level_commas(s: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut depth = 0i32;
    let mut in_str = false;
    for (i, c) in s.char_indices() {
        match c {
            '"' => in_str = !in_str,
            '[' | '{' if !in_str => depth += 1,
            ']' | '}' if !in_str => depth -= 1,
            ',' if !in_str && depth == 0 => {
                parts.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&s[start..]);
    parts
}

/// True iff `text` declares `package = "<target>"` -- either as one field
/// of a `{ ... }` inline table (e.g. `aw = { package = "autumn-web", ... }`)
/// or as a bare `key = value` line (the multiline subtable form, e.g. a
/// `[dependencies.aw]` body's `package = "autumn-web"` line).
///
/// Tolerant of any amount of TOML whitespace around `=` on either side --
/// TOML permits none, one, or many spaces there, so a literal substring
/// check for one specific spacing (`package = "..."` or `package="..."`)
/// silently misses forms like `package= "..."` or `package ="..."`. Also
/// tolerant of TOML's single-quoted literal-string form (`package =
/// 'autumn-web'`), which Cargo accepts identically to a double-quoted one.
fn declares_package(text: &str, target: &str) -> bool {
    let body = match (text.find('{'), text.rfind('}')) {
        (Some(open), Some(close)) if close > open => &text[open + 1..close],
        _ => text,
    };
    split_top_level_commas(body).into_iter().any(|part| {
        part.split_once('=').is_some_and(|(k, v)| {
            k.trim() == "package" && v.trim().trim_matches(['"', '\'']) == target
        })
    })
}

/// Pull the source-defining keys (see [`SOURCE_KEYS`]) out of a run of
/// `key = value` pairs, joined back into a single `key = value, ...`
/// fragment. Returns `None` only when `pairs` has no `version` and no
/// source key at all (i.e. the dependency can't be found).
///
/// `registry` is special-cased: unlike `workspace`/`path`/`git`, a registry
/// alone doesn't pin a resolvable dependency -- Cargo still requires an
/// explicit `version` alongside it (confirmed via `cargo metadata --offline`:
/// dropping `version` from a registry dep reports "was specified without
/// a path, git repository, version, or workspace dependency"), so `version`
/// is mirrored too whenever `registry` is present.
///
/// When there's no source key at all (a plain `{ version = "...", features
/// = [...] }` table), the existing `version` requirement is mirrored on its
/// own rather than returning `None` -- a caller that fell back to its own
/// version instead would produce two different requirements for the same
/// crate (e.g. an existing `autumn-web = "0.5"` pin vs. the CLI's current
/// `0.6`), which Cargo's resolver rejects outright when the ranges don't
/// overlap (confirmed via `cargo metadata`: "failed to select a version").
fn extract_source_keys<'a>(pairs: impl Iterator<Item = &'a str>) -> Option<String> {
    let items: Vec<(&str, &str)> = pairs
        .filter_map(|part| {
            let (key, value) = part.split_once('=')?;
            Some((key.trim(), value.trim()))
        })
        .collect();

    let mut found: Vec<String> = items
        .iter()
        .filter(|(key, _)| is_source_key(key))
        .map(|(key, value)| format!("{key} = {value}"))
        .collect();

    let version = items.iter().find(|(key, _)| *key == "version");

    if found.iter().any(|f| f.starts_with("registry"))
        && let Some((_, value)) = version
    {
        found.insert(0, format!("version = {value}"));
    }

    if found.is_empty() {
        return version.map(|(_, value)| format!("version = {value}"));
    }
    Some(found.join(", "))
}

/// Extract the source keys from an inline-table `autumn-web = { ... }` line.
fn extract_source_from_inline_table(line: &str) -> Option<String> {
    let open = line.find('{')?;
    let close = line.rfind('}')?;
    if close <= open {
        return None;
    }
    extract_source_keys(split_top_level_commas(&line[open + 1..close]).into_iter())
}

/// Extract the source keys from the body lines of a `[dependencies.autumn-web]`
/// multiline subtable.
fn extract_source_from_subtable_lines(lines: &[&str]) -> Option<String> {
    extract_source_keys(
        lines
            .iter()
            .map(|l| l.split_once('#').map_or(*l, |(code, _)| code).trim())
            .filter(|l| !l.is_empty()),
    )
}

/// Extract the version literal from a plain-string `<dep_name> = "x.y.z"`
/// declaration (no inline table), so it can be mirrored the same way an
/// explicit `version = "..."` key is. Recognizes both of TOML's string
/// forms -- double-quoted (`"x.y.z"`) and single-quoted literal
/// (`'x.y.z'`) -- since Cargo accepts either.
fn extract_plain_string_version(line: &str, dep_name: &str) -> Option<String> {
    let rest = line.trim().strip_prefix(dep_name)?.trim_start();
    let rest = rest.strip_prefix('=')?.trim_start();
    let rest = rest.split('#').next().unwrap_or(rest).trim();
    let is_quoted = rest.len() >= 2
        && ((rest.starts_with('"') && rest.ends_with('"'))
            || (rest.starts_with('\'') && rest.ends_with('\'')));
    is_quoted.then(|| format!("version = {rest}"))
}

/// Like [`detect_dependencies_autumn_web_source`], but for an arbitrary
/// dependency name instead of always `autumn-web`. Doesn't handle a
/// renamed/aliased dependency -- see [`ensure_dep_feature_status_in_section`]
/// for why that stays specific to `autumn-web`.
///
/// Returns `None` when `[dependencies]` has no `<dep_name>` entry at all, or
/// declares it as a plain crates.io dependency with no other source keys and
/// no explicit `version` either (i.e. nothing at all to mirror).
fn detect_dependencies_source(existing: &str, dep_name: &str) -> Option<String> {
    let lines: Vec<&str> = existing.lines().collect();
    let mut in_section = false;
    // Every `<dep_name>.<key> = <value>` dotted line, collected across the
    // whole section before filtering -- a dep can spread `version` and
    // `registry` (or any other source key) across separate dotted lines in
    // any order, and extract_source_keys needs to see all of them at once to
    // apply the "registry needs version too" rule.
    let mut dotted_pairs: Vec<String> = Vec::new();

    // Pass 1: inline or dotted-key form directly under `[dependencies]`.
    for &line in &lines {
        let trimmed = line.trim();
        if is_section_header(trimmed, "dependencies") {
            in_section = true;
            continue;
        }
        if in_section && is_section_boundary(trimmed, "dependencies") {
            in_section = false;
            continue;
        }
        if !in_section || trimmed.starts_with('#') {
            continue;
        }
        let after_ws = line.trim_start();
        let Some(rest) = after_ws.strip_prefix(dep_name) else {
            continue;
        };
        if let Some(dotted) = rest.strip_prefix('.') {
            // <dep_name>.workspace = true / <dep_name>.path = "..." / etc.
            let code = dotted.split_once('#').map_or(dotted, |(before, _)| before);
            if let Some((key, value)) = code.split_once('=') {
                dotted_pairs.push(format!("{} = {}", key.trim(), value.trim()));
            }
            continue;
        }
        if rest.starts_with(|c: char| c != '=' && !c.is_whitespace()) {
            // A different dependency sharing this prefix -- keep scanning.
            continue;
        }
        // The single `<dep_name> = ...` declaration for this section: a
        // plain string version, or an inline table that may carry
        // `workspace`/`path`/`git`/`version`.
        if let Some(version) = extract_plain_string_version(line, dep_name) {
            return Some(version);
        }
        return extract_source_from_inline_table(line);
    }

    if !dotted_pairs.is_empty() {
        return extract_source_keys(dotted_pairs.iter().map(String::as_str));
    }

    // Pass 2: multiline `[dependencies.<dep_name>]` subtable form.
    let subtable_key = format!("[dependencies.{dep_name}]");
    let section_start = lines
        .iter()
        .position(|l| l.trim().split('#').next().unwrap_or("").trim() == subtable_key)
        .map(|p| p + 1)?;
    let section_end = lines[section_start..]
        .iter()
        .position(|l| {
            let t = l.trim();
            t.starts_with('[') && !t.is_empty()
        })
        .map_or(lines.len(), |p| section_start + p);
    extract_source_from_subtable_lines(&lines[section_start..section_end])
}

/// Detect the source (`workspace = true`, `path = "..."`, `git = "..."`,
/// `version = "..."`, etc.) that `[dependencies]` declares for `autumn-web`,
/// so a freshly inserted `[dev-dependencies]` entry can mirror it instead of
/// defaulting to the CLI's own `version = ...`. See [`SOURCE_KEYS`] for why
/// mismatched sources break the build, and [`extract_source_keys`] for why
/// even a plain crates.io version needs mirroring (not just workspace/path/
/// git/registry): a stale or pinned `[dependencies]` requirement that
/// doesn't overlap the CLI's version makes Cargo's resolver fail too.
///
/// Returns `None` only when `[dependencies]` has no `autumn-web` entry at
/// all, in which case the caller should fall back to an explicit
/// `version = ...`.
fn detect_dependencies_autumn_web_source(existing: &str) -> Option<String> {
    if let Some(source) = detect_dependencies_source(existing, "autumn-web") {
        return Some(source);
    }

    // Not found under the literal key -- check for a renamed dep, e.g.
    // `aw = { package = "autumn-web", path = "../autumn" }` or its
    // dotted-key equivalent `aw.package = "autumn-web"` / `aw.path =
    // "../autumn"`. `ensure_autumn_web_feature_status_in_section` already
    // mirrors both shapes for `[dependencies]`; the source-detection path
    // needs the same coverage, else it silently drops the alias's
    // path/git/workspace source and falls back to a mismatched crates.io
    // version.
    let lines: Vec<&str> = existing.lines().collect();
    let mut in_section = false;
    // Every `autumn_web.<key> = <value>` dotted line, collected across the
    // whole section -- only trusted as the autumn-web alias once a sibling
    // `.package = "autumn-web"` line confirms it (an alias importable as
    // `autumn_web` could coincidentally exist for an unrelated crate
    // otherwise).
    let mut alias_dotted_pairs: Vec<String> = Vec::new();
    let mut alias_confirmed = false;

    for &line in &lines {
        let trimmed = line.trim();
        if is_section_header(trimmed, "dependencies") {
            in_section = true;
            continue;
        }
        if in_section && is_section_boundary(trimmed, "dependencies") {
            in_section = false;
            continue;
        }
        if !in_section || trimmed.starts_with('#') {
            continue;
        }
        let after_ws = line.trim_start();
        if after_ws.strip_prefix("autumn-web").is_some() {
            // The literal key -- already covered by detect_dependencies_source above.
            continue;
        }
        let Some((key, val)) = after_ws.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let (alias, dotted_sub) = key
            .split_once('.')
            .map_or((key, None), |(b, r)| (b, Some(r)));
        if alias.replace('-', "_") != "autumn_web" {
            continue;
        }
        let val_code = val.split('#').next().unwrap_or(val).trim();
        let Some(sub) = dotted_sub else {
            // Inline form: autumn_web = { package = "autumn-web", ... }.
            if declares_package(val_code, "autumn-web") {
                return extract_source_from_inline_table(line);
            }
            continue;
        };
        // Dotted form: autumn_web.package = "autumn-web" /
        // autumn_web.path = "../autumn" / etc. Trim both TOML quote forms
        // -- Cargo accepts `package = 'autumn-web'` identically to a
        // double-quoted value.
        if sub == "package" && val_code.trim_matches(['"', '\'']) == "autumn-web" {
            alias_confirmed = true;
        }
        alias_dotted_pairs.push(format!("{sub} = {val_code}"));
    }

    if alias_confirmed {
        return extract_source_keys(alias_dotted_pairs.iter().map(String::as_str));
    }

    // `[dependencies.autumn_web]` subtable whose body declares `package =
    // "autumn-web"` -- the table-key form of a renamed dep (mirrors
    // `ensure_autumn_web_feature_status_in_section`'s Pass 2b).
    let section_start =
        find_section_start_with_autumn_web_package(&lines, "[dependencies.autumn_web]")?;
    let section_end = lines[section_start..]
        .iter()
        .position(|l| {
            let t = l.trim();
            t.starts_with('[') && !t.is_empty()
        })
        .map_or(lines.len(), |p| section_start + p);
    extract_source_from_subtable_lines(&lines[section_start..section_end])
}

/// Scan `lines` for a section header matching `key` (after stripping inline TOML comments)
/// whose body contains a `package = "autumn-web"` key.  Returns the index of the first body
/// line when found, so the caller can pass it directly to `add_feature_to_deps_section`.
fn find_section_start_with_autumn_web_package(lines: &[&str], key: &str) -> Option<usize> {
    for (i, &line) in lines.iter().enumerate() {
        let key_part = line.trim().split('#').next().unwrap_or("").trim();
        if key_part != key {
            continue;
        }
        let section_start = i + 1;
        let section_end = lines[section_start..]
            .iter()
            .position(|l| {
                let t = l.trim();
                t.starts_with('[') && !t.is_empty()
            })
            .map_or(lines.len(), |p| section_start + p);
        let has_pkg = lines[section_start..section_end].iter().any(|l| {
            let code = l.split_once('#').map_or(*l, |(b, _)| b);
            declares_package(code, "autumn-web")
        });
        if has_pkg {
            return Some(section_start);
        }
    }
    None
}

/// Add `feature` to a `[dependencies.autumn-web]` section starting at `section_start`.
fn add_feature_to_deps_section(
    lines: &[&str],
    section_start: usize,
    existing: &str,
    feature: &str,
    feature_quoted: &str,
) -> String {
    let section_end = lines[section_start..]
        .iter()
        .position(|l| {
            let t = l.trim();
            t.starts_with('[') && !t.is_empty()
        })
        .map_or(lines.len(), |p| section_start + p);

    if lines[section_start..section_end].iter().any(|l| {
        let line_code = l.split_once('#').map_or(*l, |(before, _)| before);
        line_code.contains(feature_quoted)
    }) {
        return existing.to_owned();
    }

    for (j, &sect_line) in lines[section_start..section_end].iter().enumerate() {
        if sect_line.trim_start().starts_with("features") {
            return splice_feature_at(
                lines,
                section_start + j,
                &rewrite_features_line(sect_line, feature),
                sect_line,
                feature_quoted,
                existing.ends_with('\n'),
            );
        }
    }

    let feat_line = format!("features = [{feature_quoted}]");
    let mut out = String::with_capacity(existing.len() + feat_line.len() + 2);
    for (k, &l) in lines.iter().enumerate() {
        if k == section_end {
            out.push_str(&feat_line);
            out.push('\n');
        }
        out.push_str(l);
        out.push('\n');
    }
    if section_end == lines.len() {
        out.push_str(&feat_line);
        out.push('\n');
    }
    if !existing.ends_with('\n') && out.ends_with('\n') {
        out.pop();
    }
    out
}

/// Append `feature` to a standalone `features = [...]` TOML line.
fn rewrite_features_line(line: &str, feature: &str) -> String {
    let feature_quoted = format!("\"{feature}\"");
    if let Some(open) = line.find('[')
        && let Some(close_rel) = line[open..].find(']')
    {
        let abs_end = open + close_rel;
        let body = &line[open + 1..abs_end];
        let body_trimmed = body.trim();
        let separator = if body_trimmed.is_empty() {
            ""
        } else if body_trimmed.ends_with(',') {
            " "
        } else {
            ", "
        };
        return format!(
            "{}{}{}{}",
            &line[..abs_end],
            separator,
            feature_quoted,
            &line[abs_end..]
        );
    }
    line.to_owned()
}

/// Rewrite a single `autumn-web = …` TOML line to include `feature`.
fn rewrite_dep_with_feature(line: &str, dep_name: &str, feature: &str) -> String {
    let feature_quoted = format!("\"{feature}\"");
    let trimmed = line.trim();

    // Form 1: <dep_name> = "x.y.z"  (optional trailing TOML comment).
    // Recognizes both of TOML's string forms -- double-quoted and
    // single-quoted literal (`<dep_name> = 'x.y.z'`) -- since Cargo accepts
    // either; a version-only check here for `"` alone left a single-quoted
    // dep unrecognized, so the caller treated it as absent and inserted a
    // duplicate key.
    if let Some(rest) = trimmed.strip_prefix(dep_name) {
        let rest = rest.trim_start_matches([' ', '=', '\t']);
        if let Some(quote) = rest.chars().next().filter(|&c| c == '"' || c == '\'') {
            // Strip any trailing `# comment` before matching the closing quote.
            let value_str = rest.split('#').next().unwrap_or(rest).trim_end();
            if let Some(version) = value_str
                .strip_prefix(quote)
                .and_then(|r| r.strip_suffix(quote))
            {
                let indent_len = line.len() - line.trim_start().len();
                let indent = &line[..indent_len];
                return format!(
                    "{indent}{dep_name} = {{ version = {quote}{version}{quote}, features = [{feature_quoted}] }}"
                );
            }
        }
    }

    // Everything below only considers the code portion of the line -- a
    // trailing `# comment` containing TOML-looking text (e.g. an example
    // `# features = []`) must never be mistaken for a real key. Otherwise
    // the feature gets spliced into the comment while the actual
    // dependency value is untouched, and the caller reports success even
    // though nothing real changed (confirmed: `tokio = { version = "1" } #
    // features = []` would otherwise "succeed" without adding the feature).
    let (code, comment) = line
        .split_once('#')
        .map_or((line, String::new()), |(c, rest)| (c, format!("#{rest}")));

    // Form 2/3: <dep_name> = { ... features = [...] ... }
    if let Some(open) = code.find("features")
        && let Some(bracket_start) = code[open..].find('[')
    {
        let abs_start = open + bracket_start;
        if let Some(bracket_end_rel) = code[abs_start..].find(']') {
            let abs_end = abs_start + bracket_end_rel;
            let body = &code[abs_start + 1..abs_end];
            let body_trimmed = body.trim();
            let separator = if body_trimmed.is_empty() {
                ""
            } else if body_trimmed.ends_with(',') {
                " "
            } else {
                ", "
            };
            return format!(
                "{}{}{}{}{}",
                &code[..abs_end],
                separator,
                feature_quoted,
                &code[abs_end..],
                comment
            );
        }
    }

    // Form 2b: <dep_name> = { version = "x.y.z" } — no features key yet.
    // Insert features before the closing `}`.
    if let Some(close) = code.rfind('}') {
        let before = code[..close].trim_end();
        let after = &code[close..];
        return format!("{before}, features = [{feature_quoted}]{after}{comment}");
    }

    line.to_owned()
}

/// True iff `trimmed` is the TOML section header `[section]`, with or without
/// a trailing inline comment (e.g. `[dependencies] # shared deps`).
fn is_section_header(trimmed: &str, section: &str) -> bool {
    let header = format!("[{section}]");
    trimmed == header
        || (trimmed.starts_with(&header) && trimmed[header.len()..].trim_start().starts_with('#'))
}

/// True iff `trimmed` is a TOML table header that ends a scan of `section`'s
/// body -- i.e. a `[...]` header other than a `[<section>.subtable]`, which is
/// still part of the parent section rather than a new sibling table.
fn is_section_boundary(trimmed: &str, section: &str) -> bool {
    trimmed.starts_with('[') && !trimmed.starts_with(&format!("[{section}."))
}

/// Backend-aware `up.sql` for the full-text-search scaffold (issue #1910).
///
/// * **Postgres**: a stored generated `search_vector` `tsvector` column
///   (`setweight`/`to_tsvector` per `SEARCH_FIELDS` weight) plus a GIN index.
/// * **`SQLite`**: an **external-content FTS5 virtual table** `"<table>__fts"`
///   over the same `SEARCH_FIELDS` columns (tokenized `unicode61`, so case
///   folding covers the full Unicode range), kept in sync with `AFTER
///   INSERT`/`DELETE`/`UPDATE` triggers on the base table, and backfilled with
///   the FTS5 `'rebuild'` command. `SQLite` FTS5 has no per-language stemmer, so
///   `language` is unused on that arm (it selects the tokenizer, not a Postgres
///   text-search dictionary). The generated repository (`#[repository(...,
///   searchable)]`) queries this table with `MATCH` + `bm25()` ranking.
///
/// # Errors
/// On the `SQLite` arm, returns [`GenerateError::Config`] when a `#[searchable]`
/// field uses an FTS5-reserved column name (`rowid`/`rank`, or one colliding
/// with the generated `<table>__fts` table) — `SQLite` would otherwise reject
/// the generated `CREATE VIRTUAL TABLE … fts5(…)` only at `autumn migrate` time.
/// The Postgres arm never errors.
pub fn add_search_up_sql_for(
    backend: DatabaseBackend,
    table: &str,
    language: &str,
    fields: &[(String, char)],
) -> Result<String, GenerateError> {
    match backend {
        DatabaseBackend::Sqlite => {
            // FTS5 rejects reserved indexed-column names at migrate time; catch
            // them at generate time instead (issue #1910, epic #1614 AC #4).
            reject_reserved_sqlite_search_columns(table, fields)?;
            Ok(sqlite_add_search_up_sql(table, fields))
        }
        DatabaseBackend::Postgres => {
            let mut out = String::new();
            let _ = writeln!(
                out,
                "-- autumn-safety: potentially-blocking \n\
                 -- adding stored generated column will backfill existing rows"
            );

            let safe_lang: String = language
                .chars()
                .filter(|c| c.is_alphanumeric() || *c == '_' || *c == '.')
                .collect();
            let safe_lang = if safe_lang.is_empty() {
                "simple".to_string()
            } else {
                safe_lang
            };

            let mut expr = String::new();
            for (i, (field, weight)) in fields.iter().enumerate() {
                if i > 0 {
                    expr.push_str(" || ");
                }
                let _ = write!(
                    expr,
                    "setweight(to_tsvector('{safe_lang}'::regconfig, coalesce(\"{field}\"::text, '')), '{weight}')"
                );
            }

            let _ = writeln!(
                out,
                "ALTER TABLE {table} ADD COLUMN search_vector tsvector GENERATED ALWAYS AS ({expr}) STORED;"
            );
            let _ = writeln!(
                out,
                "CREATE INDEX idx_{table}_search_vector ON {table} USING gin(search_vector);"
            );
            Ok(out)
        }
    }
}

/// True iff `column` is a name FTS5 forbids for an indexed column of the
/// generated `<table>__fts` external-content table (issue #1910).
///
/// FTS5 reserves the column names `rowid` and `rank` (the auto rowid alias and
/// the ranking pseudo-column) and forbids a column named the same as the FTS
/// table itself (that identifier is the table's special "command" column). All
/// three are matched **case-insensitively** — `SQLite` rejects `RANK`/`RowId`
/// exactly as `rank`/`rowid`. Quoting the identifier does **not** help: these
/// are special FTS5 names, not merely SQL keywords, so `SQLite` returns a
/// `reserved fts5 column name` error (or a `vtable constructor failed` error
/// for the self-collision) at `CREATE VIRTUAL TABLE` time.
#[must_use]
fn is_fts5_reserved_search_column(column: &str, fts_table: &str) -> bool {
    column.eq_ignore_ascii_case("rowid")
        || column.eq_ignore_ascii_case("rank")
        || column.eq_ignore_ascii_case(fts_table)
}

/// Reject, at generate time, a `#[searchable]` field whose column name FTS5
/// reserves as an indexed column on the `SQLite` backend (issue #1910; epic
/// #1614 AC #4 "map or reject-at-generate").
///
/// Now that the `SQLite` search arm emits real `CREATE VIRTUAL TABLE
/// "<table>__fts" USING fts5(<cols>, …)` DDL, a model that marks a text field
/// named `rank`/`rowid` (or one colliding with the `<table>__fts` name) as
/// `#[searchable]` would generate DDL that only fails at `autumn migrate` time
/// with a `reserved fts5 column name` error. Catch it here with an actionable
/// message naming the offending field instead. The Postgres path is unaffected
/// (its `search_vector` generated column indexes the same fields with no such
/// reservation), so this guard is `SQLite`-only by construction — it is called
/// solely from the `SQLite` arm of [`add_search_up_sql_for`].
///
/// # Errors
/// Returns [`GenerateError::Config`] on the first offending field.
fn reject_reserved_sqlite_search_columns(
    table: &str,
    fields: &[(String, char)],
) -> Result<(), GenerateError> {
    let fts_table = format!("{table}__fts");
    for (field, _) in fields {
        if is_fts5_reserved_search_column(field, &fts_table) {
            return Err(GenerateError::Config(format!(
                "the #[searchable] field '{field}' on table '{table}' uses an FTS5-reserved \
                 column name: SQLite would reject the generated `CREATE VIRTUAL TABLE \
                 \"{fts_table}\" USING fts5(...)` search index because FTS5 reserves the column \
                 names `rowid` and `rank` and forbids a column named the same as the FTS table \
                 (`{fts_table}`) — matched case-insensitively, and quoting the identifier does \
                 not help. Rename the field, or drop #[searchable] from it, to generate SQLite \
                 FTS5 search. (Postgres full-text search is unaffected.)"
            )));
        }
    }
    Ok(())
}

/// Backend-aware `down.sql` companion to [`add_search_up_sql_for`].
#[must_use]
pub fn add_search_down_sql_for(backend: DatabaseBackend, table: &str) -> String {
    match backend {
        DatabaseBackend::Sqlite => sqlite_add_search_down_sql(table),
        DatabaseBackend::Postgres => {
            let mut out = String::new();
            let _ = writeln!(out, "DROP INDEX IF EXISTS idx_{table}_search_vector;");
            let _ = writeln!(
                out,
                "ALTER TABLE {table} DROP COLUMN IF EXISTS search_vector;"
            );
            out
        }
    }
}

/// `SQLite` FTS5 `up.sql`: an external-content virtual table, its maintenance
/// triggers, and a backfill rebuild (issue #1910). The FTS table is
/// `"<table>__fts"`, indexes the `SEARCH_FIELDS` columns in priority order, and
/// mirrors the base table via `content='<table>', content_rowid='id'` so the
/// base table stays the single source of truth (the generated `bm25()`-ranked
/// `MATCH` query joins the two).
fn sqlite_add_search_up_sql(table: &str, fields: &[(String, char)]) -> String {
    let fts = format!("{table}__fts");
    // Quoted, comma-separated indexed column list, shared across the DDL.
    let cols: Vec<String> = fields.iter().map(|(f, _)| format!("\"{f}\"")).collect();
    let cols_csv = cols.join(", ");
    // `new."col"` / `old."col"` lists for the trigger bodies.
    let new_vals: Vec<String> = fields.iter().map(|(f, _)| format!("new.\"{f}\"")).collect();
    let new_vals_csv = new_vals.join(", ");
    let old_vals: Vec<String> = fields.iter().map(|(f, _)| format!("old.\"{f}\"")).collect();
    let old_vals_csv = old_vals.join(", ");

    let mut out = String::new();
    let _ = writeln!(
        out,
        "-- autumn-safety: potentially-blocking \n\
         -- rebuilding the FTS5 index backfills every existing row"
    );
    // External-content FTS5 virtual table over the SEARCH_FIELDS columns.
    let _ = writeln!(
        out,
        "CREATE VIRTUAL TABLE \"{fts}\" USING fts5({cols_csv}, content='{table}', content_rowid='id', tokenize='unicode61');"
    );
    // Keep the index in sync with the base table (standard external-content
    // pattern): insert the new row, tombstone the old row on delete, and do both
    // on update.
    let _ = writeln!(
        out,
        "CREATE TRIGGER \"{fts}_ai\" AFTER INSERT ON \"{table}\" BEGIN\n  \
         INSERT INTO \"{fts}\"(rowid, {cols_csv}) VALUES (new.id, {new_vals_csv});\n\
         END;"
    );
    let _ = writeln!(
        out,
        "CREATE TRIGGER \"{fts}_ad\" AFTER DELETE ON \"{table}\" BEGIN\n  \
         INSERT INTO \"{fts}\"(\"{fts}\", rowid, {cols_csv}) VALUES('delete', old.id, {old_vals_csv});\n\
         END;"
    );
    let _ = writeln!(
        out,
        "CREATE TRIGGER \"{fts}_au\" AFTER UPDATE ON \"{table}\" BEGIN\n  \
         INSERT INTO \"{fts}\"(\"{fts}\", rowid, {cols_csv}) VALUES('delete', old.id, {old_vals_csv});\n  \
         INSERT INTO \"{fts}\"(rowid, {cols_csv}) VALUES (new.id, {new_vals_csv});\n\
         END;"
    );
    // Backfill the index for rows that already exist.
    let _ = writeln!(out, "INSERT INTO \"{fts}\"(\"{fts}\") VALUES('rebuild');");
    out
}

/// `SQLite` FTS5 `down.sql`: drop the maintenance triggers, then the FTS table.
fn sqlite_add_search_down_sql(table: &str) -> String {
    let fts = format!("{table}__fts");
    let mut out = String::new();
    let _ = writeln!(out, "DROP TRIGGER IF EXISTS \"{fts}_au\";");
    let _ = writeln!(out, "DROP TRIGGER IF EXISTS \"{fts}_ad\";");
    let _ = writeln!(out, "DROP TRIGGER IF EXISTS \"{fts}_ai\";");
    let _ = writeln!(out, "DROP TABLE IF EXISTS \"{fts}\";");
    out
}

#[allow(clippy::option_if_let_else, clippy::too_many_lines)]
pub fn singularize(s: &str) -> String {
    if s == "series" {
        return "series".to_string();
    }
    if let Some(stripped) = s.strip_suffix("people") {
        return format!("{stripped}person");
    }
    if let Some(stripped) = s.strip_suffix("children") {
        return format!("{stripped}child");
    }

    let is_false_men = s == "specimen"
        || s == "regimen"
        || s == "abdomen"
        || s == "lumen"
        || s == "omen"
        || s == "semen"
        || s == "hymen"
        || s == "acumen"
        || s == "bitumen"
        || s == "stamen"
        || s.ends_with("specimen")
        || s.ends_with("regimen")
        || s.ends_with("abdomen")
        || s.ends_with("lumen")
        || s.ends_with("omen")
        || s.ends_with("semen")
        || s.ends_with("hymen")
        || s.ends_with("acumen")
        || s.ends_with("bitumen")
        || s.ends_with("stamen");

    if s == "men" {
        return "man".to_string();
    }
    if s == "women" {
        return "woman".to_string();
    }
    if s.ends_with("men") && !is_false_men {
        let stripped = s.strip_suffix("men").unwrap();
        return format!("{stripped}man");
    }
    if s.ends_with("women") {
        let stripped = s.strip_suffix("women").unwrap();
        return format!("{stripped}woman");
    }
    if s.ends_with("ves") {
        if s.ends_with("lives") {
            return format!("{}life", s.strip_suffix("lives").unwrap());
        }
        if s.ends_with("knives") {
            return format!("{}knife", s.strip_suffix("knives").unwrap());
        }
        if s.ends_with("wives") {
            return format!("{}wife", s.strip_suffix("wives").unwrap());
        }
        if s.ends_with("ives") {
            return s.strip_suffix('s').unwrap().to_string();
        }
        let stripped = s.strip_suffix("ves").unwrap();
        return format!("{stripped}f");
    }

    if let Some(stripped) = s.strip_suffix("ies") {
        if s.ends_with("movies") || s.ends_with("cookies") || s.ends_with("zombies") {
            format!("{stripped}ie")
        } else {
            format!("{stripped}y")
        }
    } else if let Some(stripped) = s.strip_suffix("es") {
        if s.ends_with("ches")
            || s.ends_with("shes")
            || s.ends_with("xes")
            || s.ends_with("ses")
            || s.ends_with("zes")
        {
            if s.ends_with("statuses")
                || s.ends_with("aliases")
                || s.ends_with("buses")
                || s.ends_with("sses")
                || s.ends_with("lenses")
            {
                stripped.to_owned()
            } else if s.ends_with("yses") {
                format!("{stripped}is")
            } else if s == "crises" {
                "crisis".to_string()
            } else if s == "diagnoses" {
                "diagnosis".to_string()
            } else if s == "neuroses" {
                "neurosis".to_string()
            } else if s == "bases" {
                "basis".to_string()
            } else if s == "oases" {
                "oasis".to_string()
            } else if s.ends_with("ases")
                || s.ends_with("ises")
                || s.ends_with("oses")
                || s.ends_with("uses")
                || s.ends_with("ses")
            {
                format!("{stripped}e")
            } else {
                stripped.to_owned()
            }
        } else {
            format!("{stripped}e")
        }
    } else if let Some(stripped) = s.strip_suffix('s') {
        if s.ends_with("ss")
            || s == "news"
            || s == "status"
            || s == "alias"
            || s == "bus"
            || s == "lens"
            || s == "virus"
            || s == "canvas"
            || s == "analysis"
            || s == "basis"
            || s == "crisis"
        {
            s.to_owned()
        } else {
            stripped.to_owned()
        }
    } else {
        s.to_owned()
    }
}

fn strip_comments(src: &str) -> String {
    let chars: Vec<char> = src.chars().collect();
    let mut result = String::with_capacity(src.len());
    let mut i = 0;
    while i < chars.len() {
        // 1. Check for single-line comment
        if i + 1 < chars.len() && chars[i] == '/' && chars[i + 1] == '/' {
            i += 2;
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            if i < chars.len() {
                result.push('\n');
                i += 1;
            } else {
                result.push(' ');
            }
            continue;
        }

        // 2. Check for block comment
        if i + 1 < chars.len() && chars[i] == '/' && chars[i + 1] == '*' {
            i += 2;
            let mut depth = 1;
            while i + 1 < chars.len() && depth > 0 {
                if chars[i] == '/' && chars[i + 1] == '*' {
                    depth += 1;
                    i += 2;
                } else if chars[i] == '*' && chars[i + 1] == '/' {
                    depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            if depth > 0 {
                i = chars.len();
            }
            result.push(' ');
            continue;
        }

        // 3. Check for raw string literal: r"..." or r#"..."# or r##"..."##
        if chars[i] == 'r' && i + 1 < chars.len() {
            let mut hash_count = 0;
            let mut j = i + 1;
            while j < chars.len() && chars[j] == '#' {
                hash_count += 1;
                j += 1;
            }
            if j < chars.len() && chars[j] == '"' {
                result.extend(&chars[i..=j]);
                i = j + 1;

                let mut closed = false;
                while i < chars.len() {
                    if chars[i] == '"' {
                        let mut match_hashes = true;
                        for h in 0..hash_count {
                            if i + 1 + h >= chars.len() || chars[i + 1 + h] != '#' {
                                match_hashes = false;
                                break;
                            }
                        }
                        if match_hashes {
                            result.push('"');
                            for _ in 0..hash_count {
                                result.push('#');
                            }
                            i += 1 + hash_count;
                            closed = true;
                            break;
                        }
                    }
                    result.push(chars[i]);
                    i += 1;
                }
                if !closed {
                    i = chars.len();
                }
                continue;
            }
        }

        // 4. Check for standard double-quoted string
        if chars[i] == '"' {
            result.push('"');
            i += 1;
            while i < chars.len() {
                let ch = chars[i];
                result.push(ch);
                if ch == '\\' && i + 1 < chars.len() {
                    result.push(chars[i + 1]);
                    i += 2;
                    continue;
                }
                if ch == '"' {
                    i += 1;
                    break;
                }
                i += 1;
            }
            continue;
        }

        // 5. Normal character
        result.push(chars[i]);
        i += 1;
    }
    result
}

/// Scan a model file content to extract the `#[searchable]` language and field weights.
#[must_use]
#[allow(dead_code)]
pub fn parse_model_search_config(content: &str) -> Option<(String, Vec<(String, char)>)> {
    parse_model_search_config_for_table(content, "")
}

/// Scan a model file content to extract the `#[searchable]` language and field weights for a specific table.
fn is_matching_table_attr(attr_content: &str, table: &str) -> bool {
    let mut rest = attr_content;
    while let Some(pos) = rest.find("table") {
        let prev_char = if pos > 0 {
            rest.as_bytes().get(pos - 1)
        } else {
            None
        };
        let next_char = rest.as_bytes().get(pos + "table".len());
        let is_prev_boundary = prev_char.is_none_or(|&c| !c.is_ascii_alphanumeric() && c != b'_');
        let is_next_boundary = next_char.is_none_or(|&c| !c.is_ascii_alphanumeric() && c != b'_');
        if is_prev_boundary && is_next_boundary {
            let after_table = &rest[pos + "table".len()..];
            let trimmed = after_table.trim_start();
            if let Some(stripped_eq) = trimmed.strip_prefix('=') {
                let after_eq = stripped_eq.trim_start();

                // 1. Try normal string literal
                let expected_value = format!("\"{table}\"");
                if after_eq.starts_with(&expected_value) {
                    return true;
                }

                // 2. Try raw string literal (e.g. r"table" or r#"table"#)
                if let Some(after_r) = after_eq.strip_prefix('r') {
                    let mut hash_count = 0;
                    let bytes = after_r.as_bytes();
                    while hash_count < bytes.len() && bytes[hash_count] == b'#' {
                        hash_count += 1;
                    }
                    let after_hashes = &after_r[hash_count..];
                    let expected_raw = format!("\"{table}\"");
                    if after_hashes.starts_with(&expected_raw) {
                        let after_quote = &after_hashes[expected_raw.len()..];
                        let mut match_close = true;
                        for h in 0..hash_count {
                            if after_quote.as_bytes().get(h) != Some(&b'#') {
                                match_close = false;
                                break;
                            }
                        }
                        if match_close {
                            return true;
                        }
                    }
                }
            }
        }
        rest = &rest[pos + "table".len()..];
    }
    false
}

#[allow(clippy::collapsible_if)]
fn extract_diesel_column_name(attr: &str) -> Option<String> {
    let pos = attr.find("column_name")?;
    let after_col = &attr[pos + "column_name".len()..];
    let trimmed = after_col.trim_start();
    let stripped_eq = trimmed.strip_prefix('=')?;
    let after_eq = stripped_eq.trim_start();

    // 1. Try standard double quotes
    if let Some(stripped_quote) = after_eq.strip_prefix('"') {
        let quote_end = stripped_quote.find('"')?;
        return Some(stripped_quote[..quote_end].to_string());
    }

    // 2. Try raw string literal e.g. r#"headline"# or r"headline"
    if let Some(after_r) = after_eq.strip_prefix('r') {
        let mut hash_count = 0;
        let bytes = after_r.as_bytes();
        while hash_count < bytes.len() && bytes[hash_count] == b'#' {
            hash_count += 1;
        }
        let after_hashes = &after_r[hash_count..];
        if let Some(stripped_quote) = after_hashes.strip_prefix('"') {
            if let Some(quote_end) = stripped_quote.find('"') {
                return Some(stripped_quote[..quote_end].to_string());
            }
        }
    }

    // 3. Try unquoted identifier form e.g. column_name = headline or column_name = r#type
    let after_ident = after_eq.strip_prefix("r#").unwrap_or(after_eq);

    let mut id_chars = String::new();
    for c in after_ident.chars() {
        if c.is_alphanumeric() || c == '_' {
            id_chars.push(c);
        } else {
            break;
        }
    }
    if !id_chars.is_empty() {
        return Some(id_chars);
    }

    None
}

fn has_attribute_boundary(rest: &str, pos: usize, keyword: &str) -> bool {
    let after = &rest[pos + keyword.len()..];
    after
        .chars()
        .next()
        .is_none_or(|c| c == '(' || c == ']' || c.is_whitespace())
}

fn find_real_struct_keyword(src: &str, start_byte_offset: usize) -> Option<usize> {
    let mut chars = src.char_indices().peekable();
    while let Some(&(idx, _)) = chars.peek() {
        if idx < start_byte_offset {
            chars.next();
        } else {
            break;
        }
    }

    while let Some((idx, c)) = chars.next() {
        // Skip raw string literal
        if c == 'r' {
            let mut temp = chars.clone();
            let mut hash_count = 0;
            while let Some((_, '#')) = temp.peek() {
                hash_count += 1;
                temp.next();
            }
            if let Some((_, '"')) = temp.peek() {
                for _ in 0..hash_count {
                    chars.next();
                }
                chars.next(); // opening double quote '"'
                while let Some((_, rc)) = chars.next() {
                    if rc == '"' {
                        let mut match_hashes = true;
                        let mut check_chars = chars.clone();
                        for _ in 0..hash_count {
                            if check_chars.peek().is_some_and(|&(_, ch)| ch == '#') {
                                check_chars.next();
                            } else {
                                match_hashes = false;
                                break;
                            }
                        }
                        if match_hashes {
                            for _ in 0..hash_count {
                                chars.next();
                            }
                            break;
                        }
                    }
                }
                continue;
            }
        }

        // Skip standard double-quoted string
        if c == '"' {
            while let Some((_, sc)) = chars.next() {
                if sc == '\\' {
                    chars.next(); // Skip next char (escaped)
                } else if sc == '"' {
                    break;
                }
            }
            continue;
        }

        // Check if we have the word "struct"
        if c == 's' {
            let rest = &src[idx..];
            if rest.starts_with("struct") {
                let next_char = src[idx + "struct".len()..].chars().next();
                let is_followed_by_boundary =
                    next_char.is_none_or(|ch| !ch.is_ascii_alphanumeric() && ch != '_');

                let is_preceded_by_boundary = if idx == 0 {
                    true
                } else {
                    let prev_char = src[..idx].chars().next_back();
                    prev_char.is_none_or(|ch| !ch.is_ascii_alphanumeric() && ch != '_')
                };

                if is_followed_by_boundary && is_preceded_by_boundary {
                    return Some(idx);
                }
            }
        }
    }
    None
}

#[must_use]
#[allow(clippy::too_many_lines, clippy::collapsible_if)]
pub fn parse_model_search_config_for_table(
    content: &str,
    table: &str,
) -> Option<(String, Vec<(String, char)>)> {
    let clean_content = strip_comments(content);
    let mut language = "simple".to_string();
    let mut fields = Vec::new();

    // 1. Locate the model struct position anchored by #[model] or #[autumn_web::model] for the given table
    let mut model_pos = None;
    let mut struct_pos = None;

    if !table.is_empty() {
        // Try to find #[model(...table = "table"...)]
        let mut rest = clean_content.as_str();
        while let Some(pos) = rest.find("#[model") {
            let offset = clean_content.len() - rest.len() + pos;
            if has_attribute_boundary(rest, pos, "#[model") {
                if let Some(close_bracket) = rest[pos..].find(']') {
                    let attr_content = &rest[pos..pos + close_bracket];
                    if is_matching_table_attr(attr_content, table) {
                        model_pos = Some(offset);
                        break;
                    }
                }
            }
            rest = &rest[pos + "#[model".len()..];
        }

        if model_pos.is_none() {
            let mut rest = clean_content.as_str();
            while let Some(pos) = rest.find("#[autumn_web::model") {
                let offset = clean_content.len() - rest.len() + pos;
                if has_attribute_boundary(rest, pos, "#[autumn_web::model") {
                    if let Some(close_bracket) = rest[pos..].find(']') {
                        let attr_content = &rest[pos..pos + close_bracket];
                        if is_matching_table_attr(attr_content, table) {
                            model_pos = Some(offset);
                            break;
                        }
                    }
                }
                rest = &rest[pos + "#[autumn_web::model".len()..];
            }
        }

        // Try PascalCase struct name fallback
        if model_pos.is_none() {
            let singular = singularize(table);
            let struct_name = super::naming::snake_to_pascal(&singular);

            let mut current_offset = 0;
            let mut found_struct_pos = None;
            while let Some(pos) = find_real_struct_keyword(&clean_content, current_offset) {
                let after_struct = &clean_content[pos + "struct".len()..];
                if let Some(first_word) = after_struct.split_whitespace().next() {
                    let clean_name =
                        first_word.trim_end_matches(|c: char| !c.is_alphanumeric() && c != '_');
                    if clean_name == struct_name {
                        found_struct_pos = Some(pos);
                        break;
                    }
                }
                current_offset = pos + "struct".len();
            }

            if let Some(s_pos) = found_struct_pos {
                let before_struct = &clean_content[..s_pos];

                let mut m_pos_opt = None;
                let mut rest_before = before_struct;
                while let Some(p) = rest_before.rfind("#[model") {
                    if has_attribute_boundary(rest_before, p, "#[model") {
                        m_pos_opt = Some(p);
                        break;
                    }
                    rest_before = &rest_before[..p];
                }

                let mut aw_pos_opt = None;
                let mut rest_before_aw = before_struct;
                while let Some(p) = rest_before_aw.rfind("#[autumn_web::model") {
                    if has_attribute_boundary(rest_before_aw, p, "#[autumn_web::model") {
                        aw_pos_opt = Some(p);
                        break;
                    }
                    rest_before_aw = &rest_before_aw[..p];
                }

                let best_pos = match (m_pos_opt, aw_pos_opt) {
                    (Some(p1), Some(p2)) => Some(std::cmp::max(p1, p2)),
                    (Some(p), None) | (None, Some(p)) => Some(p),
                    (None, None) => None,
                };

                if let Some(pos) = best_pos {
                    let in_between = &before_struct[pos..];
                    let has_other_struct = find_real_struct_keyword(in_between, 0).is_some();
                    if !has_other_struct {
                        model_pos = Some(pos);
                        struct_pos = Some(s_pos);
                    }
                }
            }
        }
    }

    if model_pos.is_none() {
        if table.is_empty() {
            let mut rest = clean_content.as_str();
            while let Some(pos) = rest.find("#[model") {
                if has_attribute_boundary(rest, pos, "#[model") {
                    model_pos = Some(clean_content.len() - rest.len() + pos);
                    break;
                }
                rest = &rest[pos + "#[model".len()..];
            }
            if model_pos.is_none() {
                let mut rest = clean_content.as_str();
                while let Some(pos) = rest.find("#[autumn_web::model") {
                    if has_attribute_boundary(rest, pos, "#[autumn_web::model") {
                        model_pos = Some(clean_content.len() - rest.len() + pos);
                        break;
                    }
                    rest = &rest[pos + "#[autumn_web::model".len()..];
                }
            }
        } else {
            return None;
        }
    }

    let struct_pos = if let Some(s_pos) = struct_pos {
        s_pos
    } else if let Some(m_pos) = model_pos {
        find_real_struct_keyword(&clean_content, m_pos)?
    } else {
        find_real_struct_keyword(&clean_content, 0)?
    };

    // 2. Restrict FTS language search to the struct-level #[searchable] attribute (preceding our struct)
    let before_struct = &clean_content[..struct_pos];
    let mut rest_before = before_struct;
    while let Some(pos) = rest_before.rfind("#[searchable") {
        let next_char = rest_before.as_bytes().get(pos + "#[searchable".len());
        let is_boundary =
            next_char.is_none_or(|&c| c == b']' || c == b'(' || c.is_ascii_whitespace());
        if !is_boundary {
            rest_before = &rest_before[..pos];
            continue;
        }
        let attr_chunk = &rest_before[pos..];
        if let Some(close_bracket) = attr_chunk.find(']') {
            let end_of_attr = pos + close_bracket + 1;
            let in_between = &before_struct[end_of_attr..];
            if in_between.contains('}')
                || in_between.contains(';')
                || find_real_struct_keyword(in_between, 0).is_some()
            {
                // This #[searchable] belongs to a preceding model/struct, not the current one.
                break;
            }
            let attr_content = &attr_chunk[..close_bracket];
            if let Some(lang_pos) = attr_content.find("language") {
                let after_lang = &attr_content[lang_pos + "language".len()..];
                if let Some(eq_pos) = after_lang.find('=') {
                    let after_eq = &after_lang[eq_pos + 1..];
                    if let Some(quote_start) = after_eq.find('"') {
                        let after_quote = &after_eq[quote_start + 1..];
                        if let Some(quote_end) = after_quote.find('"') {
                            language = after_quote[..quote_end].to_string();
                        }
                    }
                }
            }
        }
        break;
    }

    // 3. Extract the target model's struct body definition by matching structural braces
    let mut struct_body = "";
    if let Some(open_brace_offset) = clean_content[struct_pos..].find('{') {
        let open_brace_pos = struct_pos + open_brace_offset;
        let chars: Vec<char> = clean_content[open_brace_pos + 1..].chars().collect();
        let mut brace_count = 1;
        let mut close_brace_offset = None;
        let mut i = 0;
        while i < chars.len() {
            // Check for raw string literal: r"..." or r#"..."#
            if chars[i] == 'r' && i + 1 < chars.len() {
                let mut hash_count = 0;
                let mut j = i + 1;
                while j < chars.len() && chars[j] == '#' {
                    hash_count += 1;
                    j += 1;
                }
                if j < chars.len() && chars[j] == '"' {
                    i = j + 1;
                    while i < chars.len() {
                        if chars[i] == '"' {
                            let mut match_hashes = true;
                            for h in 0..hash_count {
                                if i + 1 + h >= chars.len() || chars[i + 1 + h] != '#' {
                                    match_hashes = false;
                                    break;
                                }
                            }
                            if match_hashes {
                                i += 1 + hash_count;
                                break;
                            }
                        }
                        i += 1;
                    }
                    continue;
                }
            }

            // Check for standard double-quoted string
            if chars[i] == '"' {
                i += 1;
                while i < chars.len() {
                    let ch = chars[i];
                    if ch == '\\' && i + 1 < chars.len() {
                        i += 2;
                        continue;
                    }
                    if ch == '"' {
                        i += 1;
                        break;
                    }
                    i += 1;
                }
                continue;
            }

            // Check for structural braces
            if chars[i] == '{' {
                brace_count += 1;
            } else if chars[i] == '}' {
                brace_count -= 1;
                if brace_count == 0 {
                    close_brace_offset = Some(i);
                    break;
                }
            }
            i += 1;
        }

        if let Some(offset) = close_brace_offset {
            let consumed: String = chars[..offset].iter().collect();
            let consumed_len = consumed.len();
            struct_body = &clean_content[open_brace_pos + 1..open_brace_pos + 1 + consumed_len];
        }
    }

    // 4. Restrict FTS fields loop to scan only inside the struct body
    let mut rest = struct_body;
    while let Some(pos) = rest.find("#[searchable") {
        // Enforce word boundaries on the #[searchable] prefix check
        let next_char = rest.as_bytes().get(pos + "#[searchable".len());
        let is_boundary =
            next_char.is_none_or(|&c| c == b']' || c == b'(' || c.is_ascii_whitespace());
        if !is_boundary {
            rest = &rest[pos + "#[searchable".len()..];
            continue;
        }

        let attr_chunk = &rest[pos..];
        let mut weight = 'D';

        if let Some(close_bracket) = attr_chunk.find(']') {
            let attr_content = &attr_chunk[..close_bracket];
            // Restrict weight search purely to the current attribute block contents
            if let Some(w_pos) = attr_content.find("weight") {
                let after_weight = &attr_content[w_pos + "weight".len()..];
                if let Some(eq_pos) = after_weight.find('=') {
                    let after_eq = &after_weight[eq_pos + 1..];
                    if let Some(quote_start) = after_eq.find('"') {
                        let after_quote = &after_eq[quote_start + 1..];
                        if let Some(quote_end) = after_quote.find('"') {
                            let w_str = &after_quote[..quote_end];
                            if let Some(ch) = w_str.chars().next() {
                                let upper = ch.to_ascii_uppercase();
                                if upper == 'A' || upper == 'B' || upper == 'C' || upper == 'D' {
                                    weight = upper;
                                } else {
                                    weight = 'D';
                                }
                            }
                        }
                    }
                }
            }

            let after_attr = &attr_chunk[close_bracket + 1..];
            let mut line_to_parse = "";
            let mut field_attributes = Vec::new();

            let mut chars_iter = after_attr.char_indices().peekable();
            while let Some(&(idx, c)) = chars_iter.peek() {
                if c.is_whitespace() {
                    chars_iter.next();
                    continue;
                }

                if c == '/' {
                    let mut temp = chars_iter.clone();
                    temp.next(); // '/'
                    if let Some((_, '/')) = temp.peek() {
                        // Consume line comment until newline
                        for (_, next_c) in chars_iter.by_ref() {
                            if next_c == '\n' {
                                break;
                            }
                        }
                        continue;
                    }
                }

                if c == '#' {
                    let mut temp = chars_iter.clone();
                    temp.next(); // '#'
                    if let Some((_, '[')) = temp.peek() {
                        chars_iter.next(); // '#'
                        chars_iter.next(); // '['

                        let start_attr_idx = idx;
                        let mut bracket_depth = 1;
                        let mut end_attr_idx = None;

                        for (c_idx, next_c) in chars_iter.by_ref() {
                            if next_c == '[' {
                                bracket_depth += 1;
                            } else if next_c == ']' {
                                bracket_depth -= 1;
                                if bracket_depth == 0 {
                                    end_attr_idx = Some(c_idx + next_c.len_utf8());
                                    break;
                                }
                            }
                        }

                        if let Some(end_idx) = end_attr_idx {
                            let attr_str = &after_attr[start_attr_idx..end_idx];
                            field_attributes.push(attr_str);
                            continue;
                        }
                    }
                }

                let rest_str = &after_attr[idx..];
                if let Some(nl_idx) = rest_str.find('\n') {
                    line_to_parse = rest_str[..nl_idx].trim();
                } else {
                    line_to_parse = rest_str.trim();
                }
                break;
            }

            if !line_to_parse.is_empty() {
                let mut parts = line_to_parse;
                if let Some(stripped_pub) = parts.strip_prefix("pub") {
                    parts = stripped_pub.trim();
                    if let Some(stripped_paren) = parts.strip_prefix('(')
                        && let Some(close_paren) = stripped_paren.find(')')
                    {
                        parts = stripped_paren[close_paren + 1..].trim();
                    }
                }
                if let Some(colon) = parts.find(':') {
                    let field_name = parts[..colon].trim().to_string();
                    let mut clean_field = field_name.as_str();
                    if let Some(stripped) = clean_field.strip_prefix("r#") {
                        clean_field = stripped;
                    }
                    if !clean_field.is_empty()
                        && clean_field.chars().all(|c| c.is_alphanumeric() || c == '_')
                    {
                        let mut final_col_name = clean_field.to_string();

                        // 1. Scan attributes *before* #[searchable] using balanced separator logic
                        let before_searchable_attr = &rest[..pos];
                        let mut prev_term_pos = 0;
                        let mut paren_depth = 0;
                        let mut bracket_depth = 0;
                        let chars_vec: Vec<char> = before_searchable_attr.chars().collect();
                        let mut char_idx = chars_vec.len();

                        while char_idx > 0 {
                            char_idx -= 1;
                            let c = chars_vec[char_idx];
                            if c == ')' {
                                paren_depth += 1;
                            } else if c == '(' {
                                if paren_depth > 0 {
                                    paren_depth -= 1;
                                }
                            } else if c == ']' {
                                bracket_depth += 1;
                            } else if c == '[' {
                                if bracket_depth > 0 {
                                    bracket_depth -= 1;
                                }
                            } else if paren_depth == 0 && bracket_depth == 0 {
                                if c == ';' || c == ',' || c == '{' {
                                    let mut byte_pos = 0;
                                    for &ch in &chars_vec[..=char_idx] {
                                        byte_pos += ch.len_utf8();
                                    }
                                    prev_term_pos = byte_pos;
                                    break;
                                }
                            }
                        }
                        let field_prefix = &before_searchable_attr[prev_term_pos..];

                        // Scan for ALL #[diesel...] attributes within field_prefix
                        let mut scan_rest = field_prefix;
                        while let Some(d_pos) = scan_rest.find("#[diesel") {
                            let sub_chunk = &scan_rest[d_pos..];
                            let mut inner_bracket_depth = 0;
                            let mut closed_pos = None;
                            for (idx, c) in sub_chunk.char_indices() {
                                if c == '[' {
                                    inner_bracket_depth += 1;
                                } else if c == ']' {
                                    if inner_bracket_depth > 0 {
                                        inner_bracket_depth -= 1;
                                        if inner_bracket_depth == 0 {
                                            closed_pos = Some(idx);
                                            break;
                                        }
                                    }
                                }
                            }
                            if let Some(cb) = closed_pos {
                                let attr_content = &sub_chunk[..cb];
                                if let Some(custom_col) = extract_diesel_column_name(attr_content) {
                                    final_col_name = custom_col;
                                }
                                scan_rest = &sub_chunk[cb + 1..];
                            } else {
                                break;
                            }
                        }

                        // 2. Scan attributes *after* #[searchable] but before field declaration
                        for attr in &field_attributes {
                            if let Some(custom_col) = extract_diesel_column_name(attr) {
                                final_col_name = custom_col;
                            }
                        }

                        fields.push((final_col_name, weight));
                    }
                }
            }
        }

        rest = &rest[pos + "#[searchable".len()..];
    }

    if fields.is_empty() {
        None
    } else {
        Some((language, fields))
    }
}

#[cfg(test)]
// Test inputs like `"rank:position{scope:board_id}"` are literal DSL tokens
// passed to `parse_field`, not format strings — the `{…}` is the scaffold's
// own constraint-modifier syntax under test.
#[allow(clippy::literal_string_with_formatting_args)]
mod tests {
    use super::*;
    use crate::generate::dsl::parse_field;

    fn fields(tokens: &[&str]) -> Vec<Field> {
        tokens.iter().map(|t| parse_field(t).unwrap()).collect()
    }

    // ── #1384: `{translatable}` storage migration ───────────────────────────

    #[test]
    fn create_table_gives_a_translatable_column_the_empty_container_default() {
        let sql = create_table_sql_with_metadata_and_id(
            "posts",
            &fields(&["title:String{translatable}", "views:i64"]),
            &BTreeSet::new(),
            &BTreeMap::new(),
            IdType::BigSerial,
        );
        assert!(
            sql.contains("title TEXT NOT NULL DEFAULT '{}'"),
            "translatable column needs the empty-container default: {sql}"
        );
        // A plain column on the same table is untouched (AC7).
        assert!(sql.contains("views BIGINT NOT NULL"), "{sql}");
        assert!(!sql.contains("views BIGINT NOT NULL DEFAULT"), "{sql}");
    }

    #[test]
    fn schema_block_keeps_a_translatable_column_as_text() {
        let block = schema_table_block_with_id(
            "posts",
            &fields(&["title:String{translatable}"]),
            IdType::BigSerial,
        );
        assert!(block.contains("title -> Text,"), "{block}");
    }

    #[test]
    fn add_column_emits_the_default_and_skips_the_blocking_banner() {
        let sql = add_columns_up_sql("posts", &fields(&["title:String{translatable}"]), "");
        assert!(
            sql.contains("ALTER TABLE posts ADD COLUMN title TEXT NOT NULL DEFAULT '{}'"),
            "{sql}"
        );
        assert!(
            !sql.contains("autumn-safety: potentially-blocking"),
            "a constant default backfills in one statement — no banner: {sql}"
        );
    }

    #[test]
    fn add_column_is_accepted_on_sqlite() {
        // SQLite rejects `ADD COLUMN … NOT NULL` without a DEFAULT; the
        // container default is exactly what makes this portable.
        let sql = add_columns_up_sql_for(
            DatabaseBackend::Sqlite,
            "posts",
            &fields(&["title:String{translatable}"]),
            "",
        )
        .expect("SQLite ADD COLUMN accepted for a defaulted column");
        assert!(sql.contains("title TEXT NOT NULL DEFAULT '{}'"), "{sql}");
    }

    #[test]
    fn remove_column_rollback_restores_the_default() {
        let sql = remove_columns_down_sql_for(
            DatabaseBackend::Sqlite,
            "posts",
            &fields(&["title:String{translatable}"]),
            "",
        )
        .expect("SQLite rollback accepted for a defaulted column");
        assert!(sql.contains("title TEXT NOT NULL DEFAULT '{}'"), "{sql}");
    }

    /// AC6: `autumn migrate check` classifies the emitted migration — and
    /// classifies it as **safe**. No new unclassified operation type.
    #[test]
    fn migrate_check_classifies_the_translatable_migration_as_safe() {
        use crate::migrate::safety::{classify_sql, is_safe};

        for sql in [
            create_table_sql_with_metadata_and_id(
                "posts",
                &fields(&["title:String{translatable}"]),
                &BTreeSet::new(),
                &BTreeMap::new(),
                IdType::BigSerial,
            ),
            add_columns_up_sql("posts", &fields(&["title:String{translatable}"]), ""),
        ] {
            let findings = classify_sql(&sql);
            assert!(
                is_safe(&findings),
                "translatable storage must classify as safe, got {findings:?} for:\n{sql}"
            );
        }
    }

    /// The classification test above is only meaningful if the classifier would
    /// actually *say something* about this DDL when the default is missing —
    /// otherwise "no findings" would pass for a statement the classifier simply
    /// does not understand. Pin the discriminating case: drop the container
    /// default and the very same `ADD COLUMN` becomes a recognised
    /// `PotentiallyBlocking` finding, not an unclassified one.
    #[test]
    fn the_container_default_is_what_makes_the_add_column_safe() {
        use crate::migrate::safety::{RiskLevel, classify_sql, is_safe};

        let undefaulted = "ALTER TABLE posts ADD COLUMN title TEXT NOT NULL;";
        let findings = classify_sql(undefaulted);
        assert!(!is_safe(&findings), "control case must not be safe");
        assert!(
            findings
                .iter()
                .any(|f| f.risk == RiskLevel::PotentiallyBlocking
                    && f.operation.contains("ADD COLUMN NOT NULL")),
            "the classifier must recognise the shape, not merely stay silent: {findings:?}"
        );
        // And with the default the same statement is classified clean.
        let defaulted = add_columns_up_sql("posts", &fields(&["title:String{translatable}"]), "");
        assert!(is_safe(&classify_sql(&defaulted)));
    }

    #[test]
    fn add_mod_declaration_to_empty() {
        assert_eq!(add_mod_declaration("", "post"), "pub mod post;\n");
    }

    #[test]
    fn add_mod_declaration_idempotent() {
        let initial = "pub mod post;\n";
        assert_eq!(add_mod_declaration(initial, "post"), initial);
    }

    #[test]
    fn add_mod_declaration_appends() {
        let initial = "pub mod user;\n";
        let after = add_mod_declaration(initial, "post");
        assert!(after.contains("pub mod user;"));
        assert!(after.contains("pub mod post;"));
    }

    #[test]
    fn add_mod_recognises_private_mod() {
        let initial = "mod post;\n";
        assert_eq!(add_mod_declaration(initial, "post"), initial);
    }

    #[test]
    fn schema_table_block_minimal() {
        let block =
            schema_table_block_with_id("posts", &fields(&["title:String"]), IdType::BigSerial);
        assert!(block.contains("posts (id)"));
        assert!(block.contains("id -> Int8,"));
        assert!(block.contains("title -> Text,"));
        assert!(block.contains("created_at -> Timestamp,"));
    }

    #[test]
    fn schema_table_block_nullable() {
        let block = schema_table_block_with_id(
            "posts",
            &fields(&["body:Option<String>"]),
            IdType::BigSerial,
        );
        assert!(block.contains("body -> Nullable<Text>,"));
    }

    #[test]
    fn append_schema_table_idempotent() {
        let f = fields(&["title:String"]);
        let first = append_schema_table("", "posts", &f);
        let second = append_schema_table(&first, "posts", &f);
        assert_eq!(first, second);
    }

    #[test]
    fn append_schema_table_to_existing_keeps_old() {
        let f1 = fields(&["title:String"]);
        let f2 = fields(&["name:String"]);
        let first = append_schema_table("", "posts", &f1);
        let combined = append_schema_table(&first, "users", &f2);
        assert!(combined.contains("posts (id)"));
        assert!(combined.contains("users (id)"));
    }

    #[test]
    fn create_table_sql_minimal() {
        let sql = create_table_sql_with_metadata_and_id(
            "posts",
            &fields(&["title:String"]),
            &BTreeSet::new(),
            &BTreeMap::new(),
            IdType::BigSerial,
        );
        assert!(sql.contains("CREATE TABLE posts ("));
        assert!(sql.contains("id BIGSERIAL PRIMARY KEY"));
        assert!(sql.contains("title TEXT NOT NULL"));
        assert!(sql.contains("created_at TIMESTAMP NOT NULL DEFAULT NOW()"));
    }

    #[test]
    fn create_table_sql_no_extra_fields() {
        let sql = create_table_sql_with_metadata_and_id(
            "widgets",
            &[],
            &BTreeSet::new(),
            &BTreeMap::new(),
            IdType::BigSerial,
        );
        assert!(sql.contains("id BIGSERIAL PRIMARY KEY"));
        assert!(sql.contains("created_at"));
    }

    #[test]
    fn create_table_sql_nullable() {
        let sql = create_table_sql_with_metadata_and_id(
            "posts",
            &fields(&["body:Option<Text>"]),
            &BTreeSet::new(),
            &BTreeMap::new(),
            IdType::BigSerial,
        );
        assert!(sql.contains("body TEXT NULL"));
    }

    #[test]
    fn drop_table_sql_simple() {
        assert_eq!(drop_table_sql("posts"), "DROP TABLE posts;\n");
    }

    // ── references field: FK column + constraint + index (issue #1026) ─────

    #[test]
    fn create_table_sql_emits_fk_column_with_constraint() {
        let sql = create_table_sql_with_metadata_and_id(
            "comments",
            &fields(&["body:Text", "post:references"]),
            &BTreeSet::new(),
            &BTreeMap::new(),
            IdType::BigSerial,
        );
        assert!(
            sql.contains("post_id BIGINT NOT NULL REFERENCES posts(id)"),
            "expected FK column with constraint; got:\n{sql}"
        );
    }

    #[test]
    fn create_table_sql_emits_fk_index_automatically() {
        let sql = create_table_sql_with_metadata_and_id(
            "comments",
            &fields(&["post:references"]),
            &BTreeSet::new(),
            &BTreeMap::new(),
            IdType::BigSerial,
        );
        assert!(
            sql.contains("CREATE INDEX idx_comments_post_id ON comments (post_id);"),
            "expected an automatic FK index; got:\n{sql}"
        );
    }

    #[test]
    fn create_table_sql_nullable_reference_has_no_not_null_but_keeps_constraint_and_index() {
        let sql = create_table_sql_with_metadata_and_id(
            "comments",
            &fields(&["post:references?"]),
            &BTreeSet::new(),
            &BTreeMap::new(),
            IdType::BigSerial,
        );
        assert!(
            sql.contains("post_id BIGINT NULL REFERENCES posts(id)"),
            "nullable FK column must omit NOT NULL but keep the constraint; got:\n{sql}"
        );
        assert!(sql.contains("CREATE INDEX idx_comments_post_id ON comments (post_id);"));
    }

    #[test]
    fn create_table_sql_fk_index_not_duplicated_when_also_passed_via_index_flag() {
        let mut explicit_indexes = BTreeSet::new();
        explicit_indexes.insert("post_id".to_owned());
        let sql = create_table_sql_with_metadata_and_id(
            "comments",
            &fields(&["post:references"]),
            &explicit_indexes,
            &BTreeMap::new(),
            IdType::BigSerial,
        );
        assert_eq!(
            sql.matches("CREATE INDEX idx_comments_post_id").count(),
            1,
            "the FK index and an explicit --index on the same field must not \
             produce two CREATE INDEX statements:\n{sql}"
        );
    }

    // ── position field: NOT NULL BIGINT column + auto index (issue #1358) ──

    #[test]
    fn create_table_sql_emits_position_column_not_null_bigint() {
        let sql = create_table_sql_with_metadata_and_id(
            "tasks",
            &fields(&["title:String", "rank:position"]),
            &BTreeSet::new(),
            &BTreeMap::new(),
            IdType::BigSerial,
        );
        assert!(
            sql.contains("rank BIGINT NOT NULL"),
            "expected a NOT NULL BIGINT position column; got:\n{sql}"
        );
    }

    #[test]
    fn create_table_sql_unscoped_position_gets_single_column_index() {
        let sql = create_table_sql_with_metadata_and_id(
            "tasks",
            &fields(&["rank:position"]),
            &BTreeSet::new(),
            &BTreeMap::new(),
            IdType::BigSerial,
        );
        assert!(
            sql.contains("CREATE INDEX idx_tasks_rank ON tasks (rank);"),
            "expected a plain index on the unscoped position column; got:\n{sql}"
        );
    }

    #[test]
    fn create_table_sql_scoped_position_gets_composite_index() {
        let sql = create_table_sql_with_metadata_and_id(
            "tasks",
            &fields(&["board:references", "rank:position{scope:board_id}"]),
            &BTreeSet::new(),
            &BTreeMap::new(),
            IdType::BigSerial,
        );
        assert!(
            sql.contains("CREATE INDEX idx_tasks_board_id_rank ON tasks (board_id, rank);"),
            "expected a composite (scope, position) index; got:\n{sql}"
        );
        // The composite index replaces a plain single-column one — no
        // redundant `CREATE INDEX ... (rank)` on top of it.
        assert!(
            !sql.contains("CREATE INDEX idx_tasks_rank ON tasks (rank);"),
            "must not also emit a redundant plain index on the position column alone:\n{sql}"
        );
    }

    #[test]
    fn create_table_sql_position_scope_reference_still_gets_its_own_fk_index() {
        // The scope column is itself a `references` field, so it keeps its
        // own single-column FK index (issue #1026) in addition to the new
        // composite (scope, position) index — the two serve different query
        // shapes (join on the FK alone vs. ordered scan within a scope).
        let sql = create_table_sql_with_metadata_and_id(
            "tasks",
            &fields(&["board:references", "rank:position{scope:board_id}"]),
            &BTreeSet::new(),
            &BTreeMap::new(),
            IdType::BigSerial,
        );
        assert!(
            sql.contains("CREATE INDEX idx_tasks_board_id ON tasks (board_id);"),
            "expected the scope column's own FK index to survive; got:\n{sql}"
        );
    }

    // ── position triggers: insert-assign + delete-compact (issue #1358) ────

    #[test]
    fn position_triggers_empty_when_no_position_field() {
        let up = position_triggers_up_sql_for(
            DatabaseBackend::Postgres,
            "tasks",
            &fields(&["title:String"]),
        );
        assert_eq!(up, "");
        let down = position_triggers_down_sql_for(
            DatabaseBackend::Postgres,
            "tasks",
            &fields(&["title:String"]),
        );
        assert_eq!(down, "");
    }

    #[test]
    fn position_triggers_postgres_unscoped_assign_and_compact() {
        let up = position_triggers_up_sql_for(
            DatabaseBackend::Postgres,
            "tasks",
            &fields(&["rank:position"]),
        );
        assert!(
            up.contains("CREATE FUNCTION tasks_rank_assign() RETURNS TRIGGER"),
            "got:\n{up}"
        );
        assert!(
            up.contains(
                "NEW.\"rank\" := COALESCE((SELECT MAX(\"rank\") + 1 FROM \"tasks\" WHERE TRUE), 0);"
            ),
            "got:\n{up}"
        );
        assert!(
            up.contains("BEFORE INSERT ON \"tasks\""),
            "insert assignment must run BEFORE INSERT on Postgres so it mutates NEW directly: {up}"
        );
        assert!(
            up.contains("CREATE FUNCTION tasks_rank_compact() RETURNS TRIGGER"),
            "got:\n{up}"
        );
        assert!(
            up.contains("UPDATE \"tasks\" SET \"rank\" = \"rank\" - 1 WHERE TRUE AND \"rank\" > OLD.\"rank\";"),
            "got:\n{up}"
        );
        assert!(up.contains("AFTER DELETE ON \"tasks\""), "got:\n{up}");
        assert!(
            !up.contains("compact_soft"),
            "no deleted_at column, so no soft-delete trigger: {up}"
        );
    }

    #[test]
    fn position_triggers_postgres_assign_and_compact_share_an_advisory_lock() {
        // Regression: without a shared lock, a concurrent insert's `SELECT
        // MAX(position)` (a plain read) can compute against a snapshot
        // taken before a concurrent delete's compaction shift commits,
        // leaving a gap. Both triggers must take the SAME
        // `pg_advisory_xact_lock` key so insert and delete-compaction on the
        // same scope fully serialize.
        let up = position_triggers_up_sql_for(
            DatabaseBackend::Postgres,
            "tasks",
            &fields(&["rank:position"]),
        );
        let assign_fn = up
            .split("CREATE FUNCTION tasks_rank_assign()")
            .nth(1)
            .expect("assign function body");
        let assign_fn = &assign_fn[..assign_fn.find("$$ LANGUAGE").unwrap_or(assign_fn.len())];
        assert!(
            assign_fn.contains("pg_advisory_xact_lock(hashtext('tasks_rank_assign'), 0)"),
            "the insert-assign trigger must take the advisory lock before reading MAX: {assign_fn}"
        );
        let advisory_pos = assign_fn.find("pg_advisory_xact_lock").unwrap();
        let select_max_pos = assign_fn.find("SELECT MAX").unwrap();
        assert!(
            advisory_pos < select_max_pos,
            "the lock must be acquired BEFORE the MAX(position) read, or a concurrent \
             insert can still race in between: {assign_fn}"
        );

        let compact_fn = up
            .split("CREATE FUNCTION tasks_rank_compact()")
            .nth(1)
            .expect("compact function body");
        let compact_fn = &compact_fn[..compact_fn.find("$$ LANGUAGE").unwrap_or(compact_fn.len())];
        assert!(
            compact_fn.contains("pg_advisory_xact_lock(hashtext('tasks_rank_assign'), 0)"),
            "the delete-compact trigger must take the SAME lock key as the assign \
             trigger: {compact_fn}"
        );
        let advisory_pos = compact_fn.find("pg_advisory_xact_lock").unwrap();
        let update_pos = compact_fn.find("UPDATE \"tasks\"").unwrap();
        assert!(
            advisory_pos < update_pos,
            "the lock must be acquired BEFORE the compaction UPDATE: {compact_fn}"
        );
    }

    #[test]
    fn position_triggers_postgres_soft_delete_compact_also_takes_the_advisory_lock() {
        let up = position_triggers_up_sql_for(
            DatabaseBackend::Postgres,
            "tasks",
            &fields(&["rank:position", "deleted_at:Option<NaiveDateTime>"]),
        );
        let compact_soft_fn = up
            .split("CREATE FUNCTION tasks_rank_compact_soft()")
            .nth(1)
            .expect("compact_soft function body");
        assert!(
            compact_soft_fn.contains("pg_advisory_xact_lock(hashtext('tasks_rank_assign'), 0)"),
            "the soft-delete compaction trigger must take the same advisory lock too: \
             {compact_soft_fn}"
        );
    }

    #[test]
    fn position_triggers_postgres_scoped_advisory_lock_keys_on_scope_value() {
        let up = position_triggers_up_sql_for(
            DatabaseBackend::Postgres,
            "tasks",
            &fields(&["board:references", "rank:position{scope:board_id}"]),
        );
        assert!(
            up.contains("pg_advisory_xact_lock(hashtext('tasks_rank_assign'), hashtext(NEW.\"board_id\"::text))"),
            "the assign trigger's lock must be scoped to board_id, not a table-wide \
             constant, so unrelated boards never contend: {up}"
        );
        assert!(
            up.contains("pg_advisory_xact_lock(hashtext('tasks_rank_assign'), hashtext(OLD.\"board_id\"::text))"),
            "the compact trigger's lock must use the same scope key (from OLD, since it \
             runs after the row is gone): {up}"
        );
    }

    #[test]
    fn position_triggers_postgres_scoped_uses_scope_column() {
        let up = position_triggers_up_sql_for(
            DatabaseBackend::Postgres,
            "tasks",
            &fields(&["board:references", "rank:position{scope:board_id}"]),
        );
        assert!(
            up.contains("WHERE \"board_id\" = NEW.\"board_id\""),
            "got:\n{up}"
        );
        assert!(
            up.contains("\"board_id\" = OLD.\"board_id\" AND \"rank\" > OLD.\"rank\""),
            "got:\n{up}"
        );
    }

    #[test]
    fn position_triggers_postgres_scoped_adds_rescope_trigger() {
        // Codex review finding (issue #1358): an ordinary UPDATE reassigning
        // the scope FK (e.g. `board_id`) must compact the old scope's gap
        // and append the row to the end of the new scope, or the
        // contiguous invariant breaks on a "move card to another board"
        // operation.
        let up = position_triggers_up_sql_for(
            DatabaseBackend::Postgres,
            "tasks",
            &fields(&["board:references", "rank:position{scope:board_id}"]),
        );
        assert!(
            up.contains("CREATE FUNCTION tasks_rank_rescope() RETURNS TRIGGER"),
            "got:\n{up}"
        );
        assert!(
            up.contains(
                "CREATE TRIGGER tasks_rank_rescope_trg BEFORE UPDATE OF \"board_id\" ON \"tasks\""
            ),
            "must be BEFORE UPDATE so it can mutate NEW.rank directly: {up}"
        );
        assert!(
            up.contains("WHEN (NEW.\"board_id\" IS DISTINCT FROM OLD.\"board_id\")"),
            "must only fire when the scope actually changes: {up}"
        );
        let rescope_fn = up
            .split("CREATE FUNCTION tasks_rank_rescope()")
            .nth(1)
            .expect("rescope function body");
        let rescope_fn = &rescope_fn[..rescope_fn.find("$$ LANGUAGE").unwrap_or(rescope_fn.len())];
        assert!(
            rescope_fn.contains(
                "UPDATE \"tasks\" SET \"rank\" = \"rank\" - 1 WHERE \"board_id\" = OLD.\"board_id\" AND \"rank\" > OLD.\"rank\";"
            ),
            "must compact the old scope: {rescope_fn}"
        );
        assert!(
            rescope_fn.contains(
                "NEW.\"rank\" := COALESCE((SELECT MAX(\"rank\") + 1 FROM \"tasks\" WHERE \"board_id\" = NEW.\"board_id\"), 0);"
            ),
            "must append to the end of the new scope: {rescope_fn}"
        );
        // Both scope keys must be locked, in a fixed hash-ascending order
        // (mirroring move_to's fixed id-ascending row-lock order) so two
        // rows swapping scopes concurrently can't deadlock each other.
        assert!(
            rescope_fn
                .contains("hashtext(OLD.\"board_id\"::text) <= hashtext(NEW.\"board_id\"::text)"),
            "must lock old/new scope keys in a fixed order: {rescope_fn}"
        );
        assert!(
            rescope_fn.contains("pg_advisory_xact_lock(hashtext('tasks_rank_assign'), hashtext(OLD.\"board_id\"::text))")
                && rescope_fn.contains("pg_advisory_xact_lock(hashtext('tasks_rank_assign'), hashtext(NEW.\"board_id\"::text))"),
            "must lock BOTH the old and new scope's advisory key, same key as \
             assign/compact so they fully serialize: {rescope_fn}"
        );
    }

    #[test]
    fn position_triggers_postgres_unscoped_position_has_no_rescope_trigger() {
        // No scope column exists to reassign on an unscoped position field.
        let up = position_triggers_up_sql_for(
            DatabaseBackend::Postgres,
            "tasks",
            &fields(&["rank:position"]),
        );
        assert!(
            !up.contains("rescope"),
            "an unscoped position field must not emit a rescope trigger: {up}"
        );
    }

    #[test]
    fn position_triggers_postgres_rescope_skips_soft_deleted_rows() {
        // A soft-deleted row's scope is already excluded from both the old
        // and new scope's live sequence — compact_soft/restore own that
        // transition, not rescope.
        let up = position_triggers_up_sql_for(
            DatabaseBackend::Postgres,
            "tasks",
            &fields(&[
                "board:references",
                "rank:position{scope:board_id}",
                "deleted_at:Option<NaiveDateTime>",
            ]),
        );
        assert!(
            up.contains(
                "WHEN (NEW.\"board_id\" IS DISTINCT FROM OLD.\"board_id\" AND OLD.deleted_at IS NULL AND NEW.deleted_at IS NULL)"
            ),
            "got:\n{up}"
        );
    }

    #[test]
    fn position_triggers_postgres_soft_delete_adds_compaction_trigger() {
        let up = position_triggers_up_sql_for(
            DatabaseBackend::Postgres,
            "tasks",
            &fields(&["rank:position", "deleted_at:Option<NaiveDateTime>"]),
        );
        assert!(
            up.contains("CREATE FUNCTION tasks_rank_compact_soft() RETURNS TRIGGER"),
            "got:\n{up}"
        );
        assert!(
            up.contains("OLD.deleted_at IS NULL AND NEW.deleted_at IS NOT NULL"),
            "got:\n{up}"
        );
        assert!(
            up.contains("AFTER UPDATE OF deleted_at ON \"tasks\""),
            "got:\n{up}"
        );
    }

    #[test]
    fn position_triggers_postgres_soft_delete_adds_restore_trigger() {
        // Codex review (issue #1358): compact_soft only ever handles the
        // deletion direction; without a restore trigger a restored row
        // re-enters the live set still carrying its stale pre-delete
        // position, which some other live row may since have taken.
        let up = position_triggers_up_sql_for(
            DatabaseBackend::Postgres,
            "tasks",
            &fields(&["rank:position", "deleted_at:Option<NaiveDateTime>"]),
        );
        assert!(
            up.contains("CREATE FUNCTION tasks_rank_restore() RETURNS TRIGGER"),
            "got:\n{up}"
        );
        assert!(
            up.contains(
                "CREATE TRIGGER tasks_rank_restore_trg BEFORE UPDATE OF deleted_at ON \"tasks\""
            ),
            "must be BEFORE UPDATE so it can mutate NEW.rank directly: {up}"
        );
        assert!(
            up.contains("WHEN (OLD.deleted_at IS NOT NULL AND NEW.deleted_at IS NULL)"),
            "must only fire on the restore direction (compact_soft owns the other): {up}"
        );
        let restore_fn = up
            .split("CREATE FUNCTION tasks_rank_restore()")
            .nth(1)
            .expect("restore function body");
        let restore_fn = &restore_fn[..restore_fn.find("$$ LANGUAGE").unwrap_or(restore_fn.len())];
        assert!(
            restore_fn.contains("pg_advisory_xact_lock(hashtext('tasks_rank_assign'), 0)"),
            "must take the same advisory lock as assign/compact: {restore_fn}"
        );
        assert!(
            restore_fn.contains(
                "NEW.\"rank\" := COALESCE((SELECT MAX(\"rank\") + 1 FROM \"tasks\" WHERE TRUE AND deleted_at IS NULL), 0);"
            ),
            "must append the restored row to the end of the live sequence: {restore_fn}"
        );
    }

    #[test]
    fn position_triggers_postgres_down_drops_functions_with_cascade() {
        let down = position_triggers_down_sql_for(
            DatabaseBackend::Postgres,
            "tasks",
            &fields(&["rank:position", "deleted_at:Option<NaiveDateTime>"]),
        );
        assert!(
            down.contains("DROP FUNCTION IF EXISTS tasks_rank_assign() CASCADE;"),
            "got:\n{down}"
        );
        assert!(
            down.contains("DROP FUNCTION IF EXISTS tasks_rank_compact() CASCADE;"),
            "got:\n{down}"
        );
        assert!(
            down.contains("DROP FUNCTION IF EXISTS tasks_rank_compact_soft() CASCADE;"),
            "got:\n{down}"
        );
        assert!(
            down.contains("DROP FUNCTION IF EXISTS tasks_rank_restore() CASCADE;"),
            "got:\n{down}"
        );
        assert!(
            !down.contains("rescope"),
            "unscoped position field must not emit a rescope function to drop: {down}"
        );
    }

    #[test]
    fn position_triggers_postgres_down_drops_rescope_function_when_scoped() {
        let down = position_triggers_down_sql_for(
            DatabaseBackend::Postgres,
            "tasks",
            &fields(&["board:references", "rank:position{scope:board_id}"]),
        );
        assert!(
            down.contains("DROP FUNCTION IF EXISTS tasks_rank_rescope() CASCADE;"),
            "got:\n{down}"
        );
    }

    #[test]
    fn position_triggers_sqlite_unscoped_assign_and_compact() {
        let up = position_triggers_up_sql_for(
            DatabaseBackend::Sqlite,
            "tasks",
            &fields(&["rank:position"]),
        );
        assert!(
            up.contains("CREATE TRIGGER \"tasks_rank_assign\" AFTER INSERT ON \"tasks\""),
            "got:\n{up}"
        );
        assert!(
            up.contains(
                "UPDATE \"tasks\" SET \"rank\" = (SELECT COALESCE(MAX(\"rank\"), -1) + 1 FROM \"tasks\" WHERE 1=1 AND id != new.id) WHERE id = new.id;"
            ),
            "got:\n{up}"
        );
        assert!(
            up.contains("CREATE TRIGGER \"tasks_rank_compact\" AFTER DELETE ON \"tasks\""),
            "got:\n{up}"
        );
        assert!(
            up.contains("UPDATE \"tasks\" SET \"rank\" = \"rank\" - 1 WHERE 1=1 AND \"rank\" > old.\"rank\";"),
            "got:\n{up}"
        );
    }

    #[test]
    fn position_triggers_sqlite_scoped_uses_scope_column() {
        let up = position_triggers_up_sql_for(
            DatabaseBackend::Sqlite,
            "tasks",
            &fields(&["board:references", "rank:position{scope:board_id}"]),
        );
        assert!(
            up.contains("WHERE \"board_id\" = new.\"board_id\" AND id != new.id"),
            "got:\n{up}"
        );
        assert!(
            up.contains("WHERE \"board_id\" = old.\"board_id\" AND \"rank\" > old.\"rank\";"),
            "got:\n{up}"
        );
    }

    #[test]
    fn position_triggers_sqlite_scoped_adds_rescope_trigger() {
        // `SQLite` can't mutate NEW in a BEFORE trigger, so this must be
        // AFTER UPDATE with a follow-up corrective UPDATE, mirroring the
        // `_assign` trigger's own AFTER-INSERT correction.
        let up = position_triggers_up_sql_for(
            DatabaseBackend::Sqlite,
            "tasks",
            &fields(&["board:references", "rank:position{scope:board_id}"]),
        );
        assert!(
            up.contains(
                "CREATE TRIGGER \"tasks_rank_rescope\" AFTER UPDATE OF \"board_id\" ON \"tasks\""
            ),
            "got:\n{up}"
        );
        assert!(
            up.contains("WHEN old.\"board_id\" IS NOT new.\"board_id\""),
            "got:\n{up}"
        );
        assert!(
            up.contains(
                "UPDATE \"tasks\" SET \"rank\" = \"rank\" - 1 WHERE \"board_id\" = old.\"board_id\" AND \"rank\" > old.\"rank\";"
            ),
            "must compact the old scope: {up}"
        );
        assert!(
            up.contains(
                "UPDATE \"tasks\" SET \"rank\" = (SELECT COALESCE(MAX(\"rank\"), -1) + 1 FROM \"tasks\" WHERE \"board_id\" = new.\"board_id\" AND id != new.id) WHERE id = new.id;"
            ),
            "must append to the end of the new scope: {up}"
        );
    }

    #[test]
    fn position_triggers_sqlite_rescope_skips_soft_deleted_rows() {
        let up = position_triggers_up_sql_for(
            DatabaseBackend::Sqlite,
            "tasks",
            &fields(&[
                "board:references",
                "rank:position{scope:board_id}",
                "deleted_at:Option<NaiveDateTime>",
            ]),
        );
        assert!(
            up.contains(
                "WHEN old.\"board_id\" IS NOT new.\"board_id\" AND old.deleted_at IS NULL AND new.deleted_at IS NULL"
            ),
            "got:\n{up}"
        );
    }

    #[test]
    fn position_triggers_sqlite_soft_delete_adds_compaction_trigger() {
        let up = position_triggers_up_sql_for(
            DatabaseBackend::Sqlite,
            "tasks",
            &fields(&["rank:position", "deleted_at:Option<NaiveDateTime>"]),
        );
        assert!(
            up.contains(
                "CREATE TRIGGER \"tasks_rank_compact_soft\" AFTER UPDATE OF deleted_at ON \"tasks\""
            ),
            "got:\n{up}"
        );
        assert!(
            up.contains("WHEN old.deleted_at IS NULL AND new.deleted_at IS NOT NULL"),
            "got:\n{up}"
        );
    }

    #[test]
    fn position_triggers_sqlite_soft_delete_adds_restore_trigger() {
        let up = position_triggers_up_sql_for(
            DatabaseBackend::Sqlite,
            "tasks",
            &fields(&["rank:position", "deleted_at:Option<NaiveDateTime>"]),
        );
        assert!(
            up.contains(
                "CREATE TRIGGER \"tasks_rank_restore\" AFTER UPDATE OF deleted_at ON \"tasks\""
            ),
            "got:\n{up}"
        );
        assert!(
            up.contains("WHEN old.deleted_at IS NOT NULL AND new.deleted_at IS NULL"),
            "got:\n{up}"
        );
        assert!(
            up.contains(
                "UPDATE \"tasks\" SET \"rank\" = (SELECT COALESCE(MAX(\"rank\"), -1) + 1 FROM \"tasks\" WHERE 1=1 AND deleted_at IS NULL AND id != new.id) WHERE id = new.id;"
            ),
            "must append the restored row to the end of the live sequence: {up}"
        );
    }

    #[test]
    fn position_triggers_sqlite_down_is_a_noop() {
        // SQLite triggers are dropped automatically with their table.
        let down = position_triggers_down_sql_for(
            DatabaseBackend::Sqlite,
            "tasks",
            &fields(&["rank:position"]),
        );
        assert_eq!(down, "");
    }

    #[test]
    fn add_columns_up_sql_rejects_position_field() {
        let err = add_columns_up_sql_for(
            DatabaseBackend::Postgres,
            "tasks",
            &fields(&["rank:position"]),
            "",
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("position"), "unexpected error: {msg}");
    }

    // ── unique field marker: CREATE UNIQUE INDEX (issue #1032) ──────────────

    #[test]
    fn create_table_sql_emits_unique_index_for_dsl_marker() {
        let sql = create_table_sql_with_metadata_and_id(
            "users",
            &fields(&["email:String:unique"]),
            &BTreeSet::new(),
            &BTreeMap::new(),
            IdType::BigSerial,
        );
        assert!(
            sql.contains("CREATE UNIQUE INDEX idx_users_email_unique ON users (email);"),
            "expected a unique index for the `:unique`-marked field; got:\n{sql}"
        );
    }

    #[test]
    fn create_table_sql_unique_index_is_distinct_from_plain_index() {
        // A `--index`-flagged field emits `idx_<table>_<field>` (no `_unique`
        // suffix); a `unique`-marked field must use a distinct name so the
        // two kinds of index never collide even on the same column.
        let sql = create_table_sql_with_metadata_and_id(
            "users",
            &fields(&["email:String:unique"]),
            &BTreeSet::new(),
            &BTreeMap::new(),
            IdType::BigSerial,
        );
        assert!(
            !sql.contains("CREATE INDEX idx_users_email ON users (email);"),
            "a unique field must not also emit a plain, non-unique index; got:\n{sql}"
        );
    }

    #[test]
    fn create_table_sql_unique_field_not_duplicated_when_also_passed_via_index_flag() {
        let mut explicit_indexes = BTreeSet::new();
        explicit_indexes.insert("email".to_owned());
        let sql = create_table_sql_with_metadata_and_id(
            "users",
            &fields(&["email:String:unique"]),
            &explicit_indexes,
            &BTreeMap::new(),
            IdType::BigSerial,
        );
        assert_eq!(
            sql.matches("CREATE UNIQUE INDEX idx_users_email_unique")
                .count(),
            1,
            "got:\n{sql}"
        );
        assert!(
            !sql.contains("CREATE INDEX idx_users_email ON"),
            "the unique index already covers lookups; a redundant plain index \
             must not also be emitted:\n{sql}"
        );
    }

    #[test]
    fn create_table_sql_unique_nullable_field_keeps_null_and_unique_index() {
        let sql = create_table_sql_with_metadata_and_id(
            "users",
            &fields(&["nickname:Option<String>:unique"]),
            &BTreeSet::new(),
            &BTreeMap::new(),
            IdType::BigSerial,
        );
        assert!(sql.contains("nickname TEXT NULL"), "got:\n{sql}");
        assert!(
            sql.contains("CREATE UNIQUE INDEX idx_users_nickname_unique ON users (nickname);"),
            "got:\n{sql}"
        );
    }

    #[test]
    fn add_columns_up_sql_emits_unique_index() {
        let sql = add_columns_up_sql("users", &fields(&["email:String:unique"]), "");
        assert!(
            sql.contains("ALTER TABLE users ADD COLUMN email TEXT NOT NULL;"),
            "got:\n{sql}"
        );
        assert!(
            sql.contains("CREATE UNIQUE INDEX idx_users_email_unique ON users (email);"),
            "got:\n{sql}"
        );
    }

    #[test]
    fn remove_columns_down_sql_restores_unique_index_for_unique_field() {
        let sql = remove_columns_down_sql("users", &fields(&["email:String:unique"]), "");
        assert!(
            sql.contains("ALTER TABLE users ADD COLUMN email TEXT NOT NULL"),
            "got:\n{sql}"
        );
        assert!(
            sql.contains("CREATE UNIQUE INDEX idx_users_email_unique ON users (email);"),
            "rollback of RemoveXFromY must restore the UNIQUE index, not just the \
             bare column; got:\n{sql}"
        );
    }

    #[test]
    fn add_columns_up_sql_unique_reference_skips_redundant_plain_index() {
        // Regression guard (issue #1032 review follow-up): a `references`
        // field's own auto-index and a `unique` field's `CREATE UNIQUE
        // INDEX` were emitted unconditionally and independently here, so a
        // field that is both (`author:references:unique`) got two
        // overlapping btree indexes on the same column — the plain one is
        // fully redundant since the unique index already covers the same
        // lookup. `create_table_sql_with_metadata_and_id` already dedupes
        // this for `CREATE TABLE`; `AddXToY` must match.
        let sql = add_columns_up_sql("posts", &fields(&["author:references:unique"]), "");
        assert!(
            sql.contains("CREATE UNIQUE INDEX idx_posts_author_id_unique ON posts (author_id);"),
            "got:\n{sql}"
        );
        assert!(
            !sql.contains("CREATE INDEX idx_posts_author_id ON posts (author_id);"),
            "a references field that is also unique must not get a redundant \
             plain index alongside its unique index; got:\n{sql}"
        );
    }

    #[test]
    fn remove_columns_down_sql_unique_reference_skips_redundant_plain_index() {
        // `RemoveXFromY`'s rollback must restore the same shape `AddXToY`
        // would have created, not the redundant pre-fix pair.
        let sql = remove_columns_down_sql("posts", &fields(&["author:references:unique"]), "");
        assert!(
            sql.contains("CREATE UNIQUE INDEX idx_posts_author_id_unique ON posts (author_id);"),
            "got:\n{sql}"
        );
        assert!(
            !sql.contains("CREATE INDEX idx_posts_author_id ON posts (author_id);"),
            "got:\n{sql}"
        );
    }

    #[test]
    fn unique_index_sql_names_index_with_unique_suffix() {
        assert_eq!(
            unique_index_sql("users", "email", &[]),
            "CREATE UNIQUE INDEX idx_users_email_unique ON users (email);\n"
        );
    }

    #[test]
    fn unique_index_name_short_names_pass_through_unchanged() {
        assert_eq!(
            unique_index_name("users", "email", &[]),
            "idx_users_email_unique"
        );
    }

    #[test]
    fn unique_index_name_truncates_long_names_to_fit_postgres_limit() {
        let table = "a_very_long_table_name_that_pushes_the_identifier_over_the_limit";
        let field = "an_equally_long_field_name_for_good_measure";
        let name = unique_index_name(table, field, &[]);
        assert!(
            name.len() <= 63,
            "index name must fit Postgres's identifier limit, got {} bytes: {name}",
            name.len()
        );
        assert!(
            name.starts_with("idx_a_very_long_table_name"),
            "got: {name}"
        );
    }

    #[test]
    fn unique_index_name_disambiguates_distinct_long_names_that_share_a_prefix() {
        // Two different (table, field) pairs that truncate to the same
        // prefix must still produce distinct index names (issue #1032 review
        // follow-up) -- otherwise the runtime `unique_violation_field` match
        // would misclassify a violation on one field as the other.
        let table = "a_very_long_table_name_that_pushes_the_identifier_over_the_limit";
        let name_a = unique_index_name(table, "an_equally_long_field_name_alpha_variant", &[]);
        let name_b = unique_index_name(table, "an_equally_long_field_name_bravo_variant", &[]);
        assert_ne!(name_a, name_b);
    }

    #[test]
    fn unique_index_name_is_deterministic() {
        let table = "a_very_long_table_name_that_pushes_the_identifier_over_the_limit";
        let field = "an_equally_long_field_name_for_good_measure";
        assert_eq!(
            unique_index_name(table, field, &[]),
            unique_index_name(table, field, &[])
        );
    }

    #[test]
    fn unique_index_sql_uses_truncated_name_for_long_identifiers() {
        let table = "a_very_long_table_name_that_pushes_the_identifier_over_the_limit";
        let field = "an_equally_long_field_name_for_good_measure";
        let sql = unique_index_sql(table, field, &[]);
        let expected_name = unique_index_name(table, field, &[]);
        assert!(
            sql.contains(&format!(
                "CREATE UNIQUE INDEX {expected_name} ON {table} ({field});"
            )),
            "got:\n{sql}"
        );
    }

    #[test]
    fn unique_index_name_disambiguates_coincidental_collision_with_plain_index() {
        // Regression guard (#1032 review follow-up): a plain index always names itself
        // after its own column (`idx_<table>_<name>`), with no `_unique` suffix. If some
        // other field in the same table happens to be named literally `<field>_unique`,
        // that field's plain index collides with `field`'s unique index name even though
        // the two are unrelated: `email:unique` and `email_unique:String --index
        // email_unique` both want `idx_users_email_unique`, and the generated migration
        // would fail with "relation already exists" before the table was ever usable.
        let colliding_field = fields(&["email_unique:String"]);
        let name = unique_index_name("users", "email", &colliding_field);
        assert_ne!(
            name, "idx_users_email_unique",
            "must disambiguate away from the name a same-named plain index \
             would already claim"
        );
        assert!(name.len() <= 63, "got {} bytes: {name}", name.len());
    }

    #[test]
    fn unique_index_name_no_collision_stays_the_plain_name() {
        // The disambiguation in the test above must not fire when there's
        // nothing to collide with.
        let unrelated_fields = fields(&["age:i32"]);
        assert_eq!(
            unique_index_name("users", "email", &unrelated_fields),
            "idx_users_email_unique"
        );
    }

    #[test]
    fn create_table_sql_unique_field_avoids_name_collision_with_plain_index() {
        let fields = fields(&["email:String:unique", "email_unique:String"]);
        let indexes: BTreeSet<String> = std::iter::once("email_unique".to_owned()).collect();
        let sql = create_table_sql_with_metadata_and_id(
            "users",
            &fields,
            &indexes,
            &BTreeMap::new(),
            IdType::BigSerial,
        );
        assert!(
            sql.contains("CREATE INDEX idx_users_email_unique ON users (email_unique);"),
            "got:\n{sql}"
        );
        // The unique index must have been disambiguated away from the
        // plain index's name above, not emitted as a second, colliding
        // `CREATE UNIQUE INDEX idx_users_email_unique` (checked with the
        // exact trailing ` ON users (email);` so the plain index's own
        // line, which shares the same name as a prefix, doesn't also match).
        assert!(
            !sql.contains("CREATE UNIQUE INDEX idx_users_email_unique ON users (email);"),
            "the unique index must not collide with the plain index's exact \
             name; got:\n{sql}"
        );
        assert!(
            sql.contains("CREATE UNIQUE INDEX idx_users_email_unique_")
                && sql.contains(" ON users (email);"),
            "the unique index must still exist, under a disambiguated name; \
             got:\n{sql}"
        );
    }

    #[test]
    fn existing_schema_columns_parses_declared_column_names() {
        let schema = append_schema_table("", "users", &fields(&["email_unique:String"]));
        let columns = existing_schema_columns(&schema, "users");
        assert!(
            columns.contains(&"email_unique".to_owned()),
            "got: {columns:?}"
        );
        assert!(columns.contains(&"id".to_owned()), "got: {columns:?}");
        assert!(
            columns.contains(&"created_at".to_owned()),
            "got: {columns:?}"
        );
    }

    #[test]
    fn existing_schema_columns_empty_for_unknown_table() {
        let schema = append_schema_table("", "users", &fields(&["email:String"]));
        assert!(existing_schema_columns(&schema, "posts").is_empty());
    }

    #[test]
    fn add_columns_up_sql_avoids_name_collision_with_earlier_migrations_columns() {
        // Regression guard (#1032 review follow-up): `add_columns_up_sql` sees only the
        // columns being added in this `AddXToY` migration, not a table's other,
        // already-existing columns from an earlier, separately-run one. A field named
        // `email_unique` added when the table was first created would otherwise still
        // collide with a `unique` field named `email` added later, with no way for this
        // call alone to know `email_unique` exists. `src/schema.rs`, kept in sync by
        // every model and scaffold generator, is what lets this call see across that gap.
        let existing_schema = append_schema_table("", "users", &fields(&["email_unique:String"]));
        let sql = add_columns_up_sql("users", &fields(&["email:String:unique"]), &existing_schema);
        assert!(
            !sql.contains("CREATE UNIQUE INDEX idx_users_email_unique ON users (email);"),
            "must not collide with the pre-existing email_unique column's \
             plain index name; got:\n{sql}"
        );
        assert!(
            sql.contains("CREATE UNIQUE INDEX idx_users_email_unique_"),
            "the unique index must still exist, under a disambiguated name; \
             got:\n{sql}"
        );
    }

    #[test]
    fn remove_columns_down_sql_avoids_name_collision_with_earlier_migrations_columns() {
        // `RemoveXFromY`'s rollback must avoid the same coincidental
        // collision `add_columns_up_sql` avoids above.
        let existing_schema = append_schema_table("", "users", &fields(&["email_unique:String"]));
        let sql =
            remove_columns_down_sql("users", &fields(&["email:String:unique"]), &existing_schema);
        assert!(
            !sql.contains("CREATE UNIQUE INDEX idx_users_email_unique ON users (email);"),
            "got:\n{sql}"
        );
        assert!(
            sql.contains("CREATE UNIQUE INDEX idx_users_email_unique_"),
            "got:\n{sql}"
        );
    }

    #[test]
    fn add_columns_up_sql_emits_fk_constraint_and_index() {
        let sql = add_columns_up_sql("comments", &fields(&["post:references"]), "");
        assert!(
            sql.contains(
                "ALTER TABLE comments ADD COLUMN post_id BIGINT NOT NULL REFERENCES posts(id);"
            ),
            "got:\n{sql}"
        );
        assert!(
            sql.contains("CREATE INDEX idx_comments_post_id ON comments (post_id);"),
            "got:\n{sql}"
        );
    }

    #[test]
    fn detect_add_migration() {
        match detect_migration_shape("AddTitleToPosts") {
            MigrationShape::AddColumns { table } => assert_eq!(table, "posts"),
            other => panic!("expected AddColumns, got {other:?}"),
        }
    }

    #[test]
    fn detect_add_migration_pluralises_singular_subject() {
        match detect_migration_shape("AddBodyToPost") {
            MigrationShape::AddColumns { table } => assert_eq!(table, "posts"),
            other => panic!("expected AddColumns, got {other:?}"),
        }
    }

    #[test]
    fn detect_encrypt_migration() {
        match detect_migration_shape("EncryptApiTokenOnAccounts") {
            MigrationShape::EncryptColumns { table, columns } => {
                assert_eq!(table, "accounts");
                assert_eq!(columns, vec!["api_token".to_string()]);
            }
            other => panic!("expected EncryptColumns, got {other:?}"),
        }
    }

    #[test]
    fn encrypt_migration_documents_backfill_and_rollback() {
        let cols = vec!["api_token".to_string()];
        let up = encrypt_columns_up_sql("accounts", &cols);
        let down = encrypt_columns_down_sql("accounts", &cols);
        // up.sql documents the offline backfill + key configuration.
        assert!(up.contains("active_record_encryption"));
        assert!(up.contains("encrypt_text"));
        assert!(up.contains("autumn-safety: backfill"));
        assert!(up.contains("api_token"));
        // Bounded columns must be widened to TEXT before the envelope is stored.
        assert!(up.contains("ALTER TABLE accounts ALTER COLUMN api_token TYPE TEXT;"));
        // down.sql documents restoring plaintext from ciphertext given the keys.
        assert!(down.contains("decrypt_text"));
        assert!(down.contains("Rollback"));
        assert!(down.contains("api_token"));
    }

    #[test]
    fn detect_remove_migration() {
        match detect_migration_shape("RemoveBodyFromPosts") {
            MigrationShape::RemoveColumns { table } => assert_eq!(table, "posts"),
            other => panic!("expected RemoveColumns, got {other:?}"),
        }
    }

    #[test]
    fn detect_other_migration_is_empty() {
        assert!(matches!(
            detect_migration_shape("BackfillSomething"),
            MigrationShape::Empty
        ));
    }

    #[test]
    fn detect_does_not_match_partial_keyword() {
        // `Tooling` should not match the `To` keyword since `o` after `To` is lowercase.
        assert!(matches!(
            detect_migration_shape("AddToolingForBuilds"),
            MigrationShape::Empty
        ));
    }

    #[test]
    fn add_columns_up_sql_emits_alter_per_field() {
        let f = fields(&["title:String", "count:i32"]);
        let sql = add_columns_up_sql("posts", &f, "");
        assert!(sql.contains("ALTER TABLE posts ADD COLUMN title TEXT NOT NULL;"));
        assert!(sql.contains("ALTER TABLE posts ADD COLUMN count INTEGER NOT NULL;"));
    }

    #[test]
    fn add_columns_up_sql_includes_safety_comment_for_not_null() {
        let f = fields(&["title:String"]);
        let sql = add_columns_up_sql("posts", &f, "");
        assert!(
            sql.contains("autumn-safety: potentially-blocking"),
            "NOT NULL column must carry a safety comment; got:\n{sql}"
        );
    }

    #[test]
    fn add_columns_up_sql_no_safety_comment_for_nullable() {
        let f = fields(&["subtitle:Option<String>"]);
        let sql = add_columns_up_sql("posts", &f, "");
        assert!(
            !sql.contains("autumn-safety"),
            "nullable column must NOT carry a safety comment; got:\n{sql}"
        );
    }

    #[test]
    fn remove_columns_up_sql_includes_safety_comment() {
        let f = fields(&["body:String"]);
        let sql = remove_columns_up_sql("posts", &f);
        assert!(
            sql.contains("autumn-safety: destructive"),
            "DROP COLUMN must carry a safety comment; got:\n{sql}"
        );
        assert!(sql.contains("ALTER TABLE posts DROP COLUMN body;"));
    }

    #[test]
    fn remove_columns_down_sql_restores_fk_constraint_and_index_for_references_field() {
        // Rolling back `RemovePostFromComments post:references` must restore
        // the FK constraint and its index, not just a bare BIGINT column —
        // otherwise the relationship and its lookup index silently vanish
        // on rollback (issue #1026).
        let f = fields(&["post:references"]);
        let sql = remove_columns_down_sql("comments", &f, "");
        assert!(
            sql.contains(
                "ALTER TABLE comments ADD COLUMN post_id BIGINT NOT NULL REFERENCES posts(id);"
            ),
            "got:\n{sql}"
        );
        assert!(
            sql.contains("CREATE INDEX idx_comments_post_id ON comments (post_id);"),
            "got:\n{sql}"
        );
    }

    #[test]
    fn add_columns_down_sql_drops_in_reverse() {
        let f = fields(&["title:String", "count:i32"]);
        let sql = add_columns_down_sql("posts", &f);
        let title_pos = sql.find("DROP COLUMN title").unwrap();
        let count_pos = sql.find("DROP COLUMN count").unwrap();
        assert!(count_pos < title_pos);
    }

    /// `SQLite` refuses to `DROP COLUMN` while an index still references it, so the
    /// down path must `DROP INDEX` before `DROP COLUMN` for a nullable
    /// `references` field's auto-index (issue #1614 finding 5). The DROP INDEX
    /// name must match the CREATE INDEX name the up path generated.
    #[test]
    fn sqlite_add_columns_down_drops_reference_index_before_column() {
        let f = fields(&["author:references?"]);
        // The up path (SQLite) creates the plain auto-index for the nullable FK.
        let up = add_columns_up_sql_for(DatabaseBackend::Sqlite, "posts", &f, "").unwrap();
        assert!(
            up.contains("CREATE INDEX idx_posts_author_id ON posts (author_id);"),
            "up:\n{up}"
        );
        let down = add_columns_down_sql_for(DatabaseBackend::Sqlite, "posts", &f, "");
        let drop_idx = down
            .find("DROP INDEX idx_posts_author_id;")
            .expect("drop index");
        let drop_col = down
            .find("ALTER TABLE posts DROP COLUMN author_id;")
            .expect("drop column");
        assert!(
            drop_idx < drop_col,
            "DROP INDEX must precede DROP COLUMN:\n{down}"
        );
    }

    /// Retrofitting optimistic locking onto a shipped resource (issue #1318) is
    /// the normal way a `lock_version` column arrives, and it only works if the
    /// `ALTER TABLE ... ADD COLUMN` carries `DEFAULT 0`: the column is
    /// DB-managed, so the generated `New{Model}` never names it and a bare
    /// `NOT NULL` add would leave every later INSERT failing. The default also
    /// backfills the existing rows, so the add needs neither the blocking-safety
    /// banner nor the `SQLite` refusal a plain NOT NULL add gets.
    #[test]
    fn add_lock_version_column_carries_a_default_on_both_backends() {
        for (backend, sql_type) in [
            (DatabaseBackend::Postgres, "INTEGER"),
            (DatabaseBackend::Sqlite, "INTEGER"),
        ] {
            let f = fields(&["lock_version:i32"]);
            let up = add_columns_up_sql_for(backend, "posts", &f, "").unwrap();
            assert!(
                up.contains(&format!(
                    "ALTER TABLE posts ADD COLUMN lock_version {sql_type} NOT NULL DEFAULT 0;"
                )),
                "{backend:?} up:\n{up}"
            );
            assert!(
                !up.contains("autumn-safety: potentially-blocking"),
                "a defaulted add backfills in one statement, so it is not blocking:\n{up}"
            );
        }
        // i64 keeps its own width.
        let f = fields(&["lock_version:i64"]);
        let up = add_columns_up_sql_for(DatabaseBackend::Postgres, "posts", &f, "").unwrap();
        assert!(
            up.contains("ADD COLUMN lock_version BIGINT NOT NULL DEFAULT 0;"),
            "up:\n{up}"
        );
    }

    /// The default is scoped to the real lock column: a nullable or
    /// differently-typed `lock_version`, and any other NOT NULL column, keep the
    /// pre-#1318 behaviour exactly.
    #[test]
    fn only_the_real_lock_version_column_gets_the_implicit_default() {
        let f = fields(&["views:i32"]);
        let up = add_columns_up_sql_for(DatabaseBackend::Postgres, "posts", &f, "").unwrap();
        assert!(
            up.contains("ADD COLUMN views INTEGER NOT NULL;"),
            "an ordinary NOT NULL add keeps the pre-#1318 DDL:\n{up}"
        );
        assert!(
            up.contains("autumn-safety: potentially-blocking"),
            "up:\n{up}"
        );

        let f = fields(&["lock_version:Option<i32>"]);
        let up = add_columns_up_sql_for(DatabaseBackend::Postgres, "posts", &f, "").unwrap();
        assert!(
            up.contains("ADD COLUMN lock_version INTEGER NULL;"),
            "a nullable column is not a lock version:\n{up}"
        );
    }

    /// Same for a nullable `unique` field: its `CREATE UNIQUE INDEX` must be
    /// dropped before the column on `SQLite`, using the same derived index name.
    #[test]
    fn sqlite_add_columns_down_drops_unique_index_before_column() {
        let f = fields(&["email:Option<String>:unique"]);
        let up = add_columns_up_sql_for(DatabaseBackend::Sqlite, "users", &f, "").unwrap();
        assert!(
            up.contains("CREATE UNIQUE INDEX idx_users_email_unique ON users (email);"),
            "up:\n{up}"
        );
        let down = add_columns_down_sql_for(DatabaseBackend::Sqlite, "users", &f, "");
        let drop_idx = down
            .find("DROP INDEX idx_users_email_unique;")
            .expect("drop index");
        let drop_col = down
            .find("ALTER TABLE users DROP COLUMN email;")
            .expect("drop column");
        assert!(
            drop_idx < drop_col,
            "DROP INDEX must precede DROP COLUMN:\n{down}"
        );
    }

    /// Postgres cascades index drops with the column, so its down.sql stays
    /// byte-for-byte identical to the legacy `DROP COLUMN`-only rollback — no
    /// explicit `DROP INDEX`.
    #[test]
    fn postgres_add_columns_down_has_no_explicit_drop_index() {
        let f = fields(&["author:references?"]);
        let down = add_columns_down_sql_for(DatabaseBackend::Postgres, "posts", &f, "");
        assert_eq!(down, "ALTER TABLE posts DROP COLUMN author_id;\n");
        // And the Postgres-default test wrapper matches.
        assert_eq!(add_columns_down_sql("posts", &f), down);
    }

    /// Forward `RemoveColumns` path (issue #1614 finding 9): `SQLite` refuses to
    /// `DROP COLUMN` while an index still references it, and the generator
    /// auto-indexes a `references` field, so the `SQLite` up.sql must `DROP INDEX`
    /// before `DROP COLUMN`, using the same name the ADD path created.
    #[test]
    fn sqlite_remove_columns_up_drops_reference_index_before_column() {
        let f = fields(&["author:references?"]);
        let up = remove_columns_up_sql_for(DatabaseBackend::Sqlite, "posts", &f, "");
        let drop_idx = up
            .find("DROP INDEX IF EXISTS idx_posts_author_id;")
            .expect("drop index");
        let drop_col = up
            .find("ALTER TABLE posts DROP COLUMN author_id;")
            .expect("drop column");
        assert!(
            drop_idx < drop_col,
            "DROP INDEX must precede DROP COLUMN:\n{up}"
        );
    }

    /// Same for a `unique` field: its `CREATE UNIQUE INDEX` must be dropped
    /// before the column on the `SQLite` forward `RemoveColumns` path, using the
    /// same derived index name.
    #[test]
    fn sqlite_remove_columns_up_drops_unique_index_before_column() {
        let f = fields(&["email:Option<String>:unique"]);
        let up = remove_columns_up_sql_for(DatabaseBackend::Sqlite, "users", &f, "");
        let drop_idx = up
            .find("DROP INDEX IF EXISTS idx_users_email_unique;")
            .expect("drop index");
        let drop_col = up
            .find("ALTER TABLE users DROP COLUMN email;")
            .expect("drop column");
        assert!(
            drop_idx < drop_col,
            "DROP INDEX must precede DROP COLUMN:\n{up}"
        );
    }

    /// A column removed via a scaffold `--index <col>` field also has a
    /// generator-created `idx_<table>_<col>` index, and the DSL/schema can't tell
    /// after the fact whether the column carried a plain index. So on `SQLite`
    /// every removed column emits `DROP INDEX IF EXISTS idx_<table>_<col>;`
    /// before its `DROP COLUMN` (issue #1906 finding F10). `IF EXISTS` makes the
    /// statement a safe no-op for a column that was never indexed.
    #[test]
    fn sqlite_remove_columns_up_drops_plain_index_before_column() {
        // `RemoveTitleFromPosts`: `title` was created via scaffold `--index`.
        let f = fields(&["title:String"]);
        let up = remove_columns_up_sql_for(DatabaseBackend::Sqlite, "posts", &f, "");
        let drop_idx = up
            .find("DROP INDEX IF EXISTS idx_posts_title;")
            .expect("drop index");
        let drop_col = up
            .find("ALTER TABLE posts DROP COLUMN title;")
            .expect("drop column");
        assert!(
            drop_idx < drop_col,
            "DROP INDEX IF EXISTS must precede DROP COLUMN:\n{up}"
        );
    }

    /// Postgres cascades index drops with the column, so its forward up.sql
    /// stays byte-for-byte identical to the legacy `DROP COLUMN`-only output —
    /// no explicit `DROP INDEX`, even for an indexed `references` field.
    #[test]
    fn postgres_remove_columns_up_has_no_explicit_drop_index() {
        let f = fields(&["author:references?"]);
        let up = remove_columns_up_sql_for(DatabaseBackend::Postgres, "posts", &f, "");
        assert!(!up.contains("DROP INDEX"), "up:\n{up}");
        assert!(up.contains("ALTER TABLE posts DROP COLUMN author_id;"));
        // And the Postgres-default test wrapper matches.
        assert_eq!(remove_columns_up_sql("posts", &f), up);
    }

    // ── enum field: CHECK constraint (issue #1030) ──────────────────────────

    #[test]
    fn create_table_emits_check_constraint_for_enum() {
        let sql = create_table_sql_with_metadata_and_id(
            "posts",
            &fields(&["status:enum{draft,published,archived}"]),
            &BTreeSet::new(),
            &BTreeMap::new(),
            IdType::BigSerial,
        );
        assert!(
            sql.contains(
                "status TEXT NOT NULL CHECK (status IN ('draft', 'published', 'archived'))"
            ),
            "got:\n{sql}"
        );
    }

    #[test]
    fn create_table_check_comes_after_default() {
        let mut defaults = BTreeMap::new();
        defaults.insert("status".to_owned(), "'draft'".to_owned());
        let sql = create_table_sql_with_metadata_and_id(
            "posts",
            &fields(&["status:enum{draft,published}"]),
            &BTreeSet::new(),
            &defaults,
            IdType::BigSerial,
        );
        assert!(
            sql.contains(
                "status TEXT NOT NULL DEFAULT 'draft' CHECK (status IN ('draft', 'published'))"
            ),
            "got:\n{sql}"
        );
    }

    #[test]
    fn create_table_nullable_enum_check_allows_null() {
        let sql = create_table_sql_with_metadata_and_id(
            "posts",
            &fields(&["status:Option<enum{draft,published}>"]),
            &BTreeSet::new(),
            &BTreeMap::new(),
            IdType::BigSerial,
        );
        assert!(
            sql.contains("status TEXT NULL CHECK (status IN ('draft', 'published'))"),
            "got:\n{sql}"
        );
    }

    #[test]
    fn add_columns_emits_check_constraint_for_enum() {
        let f = fields(&["status:enum{draft,published,archived}"]);
        let sql = add_columns_up_sql("posts", &f, "");
        assert!(
            sql.contains(
                "ALTER TABLE posts ADD COLUMN status TEXT NOT NULL CHECK (status IN ('draft', 'published', 'archived'));"
            ),
            "got:\n{sql}"
        );
    }

    #[test]
    fn add_columns_down_drops_enum_column_plainly() {
        let f = fields(&["status:enum{draft,published}"]);
        let sql = add_columns_down_sql("posts", &f);
        assert_eq!(sql, "ALTER TABLE posts DROP COLUMN status;\n");
    }

    #[test]
    fn non_enum_column_has_no_check_constraint() {
        let sql = create_table_sql_with_metadata_and_id(
            "posts",
            &fields(&["title:String"]),
            &BTreeSet::new(),
            &BTreeMap::new(),
            IdType::BigSerial,
        );
        assert!(!sql.contains("CHECK"), "got:\n{sql}");
    }

    #[test]
    fn remove_columns_down_sql_restores_check_constraint_for_enum_field() {
        // Symmetric with the FK-restoration precedent above: rolling back a
        // `RemoveStatusFromPosts` migration must restore the CHECK constraint,
        // not just a bare TEXT column — otherwise the closed set silently
        // stops being enforced after a rollback.
        let f = fields(&["status:enum{draft,published,archived}"]);
        let sql = remove_columns_down_sql("posts", &f, "");
        assert!(
            sql.contains(
                "ALTER TABLE posts ADD COLUMN status TEXT NOT NULL CHECK (status IN ('draft', 'published', 'archived'));"
            ),
            "got:\n{sql}"
        );
    }

    #[test]
    fn update_main_rs_inserts_mod_and_routes() {
        let original = r#"use autumn_web::prelude::*;

#[get("/")]
async fn index() -> &'static str { "ok" }

#[autumn_web::main]
async fn main() {
    autumn_web::app()
        .routes(routes![index])
        .run()
        .await;
}
"#;
        let updated = update_main_rs(
            original,
            &["models", "routes", "schema"],
            &["routes::posts::index".to_owned()],
        );
        assert!(updated.contains("mod models;"));
        assert!(updated.contains("mod routes;"));
        assert!(updated.contains("mod schema;"));
        assert!(updated.contains("routes::posts::index"));
        assert!(updated.contains("index,")); // original entry preserved
    }

    #[test]
    fn update_main_rs_preserves_inner_attributes() {
        // Inserting `mod` items above `#![...]` would make the file reject —
        // crate-level inner attributes must precede every item.
        let original = "#![allow(clippy::needless_pass_by_value)]\n\
#![deny(unsafe_code)]\n\
\n\
use autumn_web::prelude::*;\n\
\n\
#[autumn_web::main]\n\
async fn main() {\n\
    autumn_web::app().run().await;\n\
}\n";
        let updated = update_main_rs(original, &["models"], &[]);
        let attr_pos = updated.find("#![allow").unwrap();
        let mod_pos = updated.find("mod models;").unwrap();
        assert!(
            attr_pos < mod_pos,
            "crate inner attributes must stay above mod items:\n{updated}"
        );
        assert!(updated.contains("#![deny(unsafe_code)]"));
    }

    #[test]
    fn update_main_rs_inserts_after_doc_comment_block() {
        let original = "//! Top-level docs.\n\
//! Continuation.\n\
\n\
use autumn_web::prelude::*;\n";
        let updated = update_main_rs(original, &["models"], &[]);
        let docs_pos = updated.find("//! Top-level docs.").unwrap();
        let mod_pos = updated.find("mod models;").unwrap();
        assert!(docs_pos < mod_pos);
    }

    #[test]
    fn update_main_rs_idempotent() {
        let original = "mod models;\n\
mod routes;\n\
mod schema;\n\
\n\
use autumn_web::prelude::*;\n\
\n\
#[autumn_web::main]\n\
async fn main() {\n\
    autumn_web::app()\n\
        .routes(routes![\n\
            routes::posts::index,\n\
        ])\n\
        .run()\n\
        .await;\n\
}\n";
        let once = update_main_rs(
            original,
            &["models", "routes", "schema"],
            &["routes::posts::index".to_owned()],
        );
        let twice = update_main_rs(
            &once,
            &["models", "routes", "schema"],
            &["routes::posts::index".to_owned()],
        );
        assert_eq!(once, twice);
    }

    #[test]
    fn update_main_rs_no_routes_macro_leaves_file_alone() {
        let original = "fn main() {}\n";
        let updated = update_main_rs(original, &[], &["foo".into()]);
        assert_eq!(updated, original);
    }

    // ── link_models_into_seed_bin (issue #1718) ───────────────────────────

    /// The seed binary as `autumn new --with-seed` emits it: a `//!` doc block
    /// followed by a `use` and the async `main`.
    const SEED_BIN: &str = "\
//! Database seed binary.
//!
//!   autumn seed --count 200 --model Post
use autumn_web::seed::SeedContext;

#[autumn_web::main]
async fn main() {}
";

    #[test]
    fn link_seed_bin_injects_path_qualified_schema_and_models_mods() {
        let linked = link_models_into_seed_bin(SEED_BIN);
        assert!(
            linked.contains("#[path = \"../schema.rs\"]\nmod schema;"),
            "must inject a #[path]-qualified `mod schema;`:\n{linked}"
        );
        assert!(
            linked.contains("#[path = \"../models/mod.rs\"]\nmod models;"),
            "must inject a #[path]-qualified `mod models;`:\n{linked}"
        );
    }

    #[test]
    fn link_seed_bin_inserts_after_inner_doc_block_not_before() {
        // `mod` items must follow the crate-level `//!` doc block, or the file
        // fails to parse.
        let linked = link_models_into_seed_bin(SEED_BIN);
        let doc_end = linked.find("use autumn_web::seed").unwrap();
        let mods_at = linked.find("mod schema;").unwrap();
        assert!(
            linked.find("//! Database seed binary.").unwrap() < mods_at,
            "mods must come after the doc comment: {linked}"
        );
        assert!(
            mods_at < doc_end,
            "mods must be inserted before the first ordinary item (`use`): {linked}"
        );
    }

    #[test]
    fn link_seed_bin_is_idempotent() {
        let once = link_models_into_seed_bin(SEED_BIN);
        let twice = link_models_into_seed_bin(&once);
        assert_eq!(
            once, twice,
            "re-linking an already-linked seed bin is a no-op"
        );
    }

    #[test]
    fn link_seed_bin_preserves_existing_declarations() {
        // A hand-written seed that already declares one module keeps that
        // declaration untouched and only the missing one is added.
        let existing = "\
//! seed
#[path = \"../models/mod.rs\"]
mod models;
use autumn_web::seed::SeedContext;
";
        let linked = link_models_into_seed_bin(existing);
        assert_eq!(
            linked.matches("mod models;").count(),
            1,
            "must not duplicate the pre-existing `mod models;`:\n{linked}"
        );
        assert!(
            linked.contains("mod schema;"),
            "must still add the missing `mod schema;`:\n{linked}"
        );
    }

    #[test]
    fn link_seed_bin_no_change_when_both_present() {
        let existing = "\
//! seed
#[path = \"../schema.rs\"]
mod schema;
#[path = \"../models/mod.rs\"]
mod models;
use autumn_web::seed::SeedContext;
";
        assert_eq!(link_models_into_seed_bin(existing), existing);
    }

    #[test]
    fn unlink_seed_bin_removes_the_injected_path_qualified_mods() {
        // Destroy-time inverse: a linked seed binary loses both injected
        // declarations (and their `#[path]` attributes), keeping every original
        // item, so nothing dangles at the deleted schema.rs/models/mod.rs.
        let linked = link_models_into_seed_bin(SEED_BIN);
        let unlinked = unlink_models_from_seed_bin(&linked);
        assert!(
            !unlinked.contains("mod schema;") && !unlinked.contains("mod models;"),
            "both injected `mod` declarations must be gone:\n{unlinked}"
        );
        assert!(
            !unlinked.contains("#[path = \"../schema.rs\"]")
                && !unlinked.contains("#[path = \"../models/mod.rs\"]"),
            "no dangling `#[path]` attributes may remain:\n{unlinked}"
        );
        // Original items survive.
        assert!(
            unlinked.contains("use autumn_web::seed::SeedContext;")
                && unlinked.contains("async fn main() {}")
                && unlinked.contains("//! Database seed binary."),
            "the original seed-binary items must be preserved:\n{unlinked}"
        );
    }

    #[test]
    fn unlink_seed_bin_is_idempotent_and_noop_when_absent() {
        // No injected declarations to remove from a bare seed binary.
        assert_eq!(unlink_models_from_seed_bin(SEED_BIN), SEED_BIN);
        let linked = link_models_into_seed_bin(SEED_BIN);
        let once = unlink_models_from_seed_bin(&linked);
        let twice = unlink_models_from_seed_bin(&once);
        assert_eq!(
            once, twice,
            "re-unlinking an already-unlinked seed bin is a no-op"
        );
    }

    #[test]
    fn unlink_seed_bin_leaves_hand_written_plain_mod_untouched() {
        // A plain `mod schema;` WITHOUT the injected `#[path]` attribute is the
        // author's own module, not this generator's injection — never strip it.
        let existing = "\
//! seed
mod schema;
use autumn_web::seed::SeedContext;
";
        assert_eq!(unlink_models_from_seed_bin(existing), existing);
    }

    #[test]
    fn ensure_routes_entries_handles_empty_body() {
        let original = "fn main() {\n    routes![]\n}\n";
        let updated = ensure_routes_entries(original, &["foo".into()]);
        assert!(updated.contains("foo"));
    }

    #[test]
    fn ensure_routes_entries_skips_routes_macro_in_comments() {
        // A doc comment mentioning `routes![]` must not receive the injected
        // entries — that used to break compilation by editing the comment.
        let original = "\
//! Register handlers with `.routes(routes![])` in main.
// e.g. routes![index]
fn main() {
    routes![]
}
";
        let updated = ensure_routes_entries(original, &["foo".into()]);
        assert!(
            updated.contains("//! Register handlers with `.routes(routes![])` in main."),
            "doc comment must be untouched: {updated}"
        );
        assert!(
            updated.contains("// e.g. routes![index]"),
            "line comment must be untouched: {updated}"
        );
        assert!(
            updated.contains("foo,"),
            "entry must land in the real macro: {updated}"
        );
        assert!(
            updated.rfind("foo,").unwrap() > updated.find("fn main()").unwrap(),
            "entry must be inside the macro in fn main, not in the comments: {updated}"
        );
    }

    #[test]
    fn ensure_routes_entries_only_comment_matches_is_noop() {
        let original = "//! Add handlers via routes![].\nfn main() {}\n";
        let updated = ensure_routes_entries(original, &["foo".into()]);
        assert_eq!(updated, original);
    }

    #[test]
    fn ensure_routes_entries_url_in_string_before_macro_is_code() {
        // The `//` in the URL string literal is content, not a comment
        // marker — the `routes![` on the same line is real code and must
        // receive the injected entry.
        let original =
            "fn main() {\n    let url = \"https://example.com\"; app.routes(routes![index]);\n}\n";
        let updated = ensure_routes_entries(original, &["foo".into()]);
        assert!(
            updated.contains("foo"),
            "routes![ after a URL string must be edited as code: {updated}"
        );
        assert!(
            updated.contains("\"https://example.com\""),
            "the string literal must be untouched: {updated}"
        );
    }

    #[test]
    fn ensure_routes_entries_genuinely_commented_macro_still_skipped() {
        // A real line comment before `routes![` must still be skipped, even
        // when the comment itself contains quotes.
        let original = "\
// see \"docs\": routes![index]
fn main() {
    routes![]
}
";
        let updated = ensure_routes_entries(original, &["foo".into()]);
        assert!(
            updated.contains("// see \"docs\": routes![index]"),
            "comment must be untouched: {updated}"
        );
        assert!(
            updated.rfind("foo").unwrap() > updated.find("fn main()").unwrap(),
            "entry must land in the real macro, not the comment: {updated}"
        );
    }

    #[test]
    fn ensure_routes_entries_slashes_in_string_with_macro_on_line() {
        // A string literal containing `//` followed by a real macro use on
        // the same line: the escaped quote must not flip the string state.
        let original = "fn main() {\n    let s = \"say \\\"// not a comment\\\"\"; app.routes(routes![index]);\n}\n";
        let updated = ensure_routes_entries(original, &["foo".into()]);
        assert!(
            updated.contains("foo"),
            "escaped quotes in a string must not hide the real macro: {updated}"
        );
    }

    #[test]
    fn remove_routes_entries_with_prefix_skips_commented_macro() {
        let original = "\
// routes![channels::chat::chat_page]
fn main() {
    routes![
        channels::chat::chat_page,
        index,
    ]
}
";
        let updated = remove_routes_entries_with_prefix(original, "channels::chat::");
        assert!(
            updated.contains("// routes![channels::chat::chat_page]"),
            "comment must be untouched: {updated}"
        );
        assert!(updated.contains("index,"));
        assert!(
            !updated.contains("routes![\n        channels::chat::chat_page"),
            "real entry must be removed: {updated}"
        );
    }

    // ── remove_routes_entries_with_prefix ──────────────────────────────────

    #[test]
    fn remove_routes_entries_with_prefix_drops_matching_entries() {
        let original = "fn main() {\n    routes![\n        index,\n        channels::chat::chat_page,\n        channels::chat::chat_events,\n        channels::chat::chat_publish,\n    ]\n}\n";
        let updated = remove_routes_entries_with_prefix(original, "channels::chat::");
        assert!(updated.contains("index,"));
        assert!(!updated.contains("channels::chat::chat_page"));
        assert!(!updated.contains("channels::chat::chat_events"));
        assert!(!updated.contains("channels::chat::chat_publish"));
    }

    #[test]
    fn remove_routes_entries_with_prefix_leaves_other_prefixes_untouched() {
        let original = "fn main() {\n    routes![\n        channels::chat::chat_page,\n        channels::notifications::notifications_page,\n    ]\n}\n";
        let updated = remove_routes_entries_with_prefix(original, "channels::chat::");
        assert!(!updated.contains("channels::chat::chat_page"));
        assert!(updated.contains("channels::notifications::notifications_page"));
    }

    #[test]
    fn remove_routes_entries_with_prefix_is_noop_when_nothing_matches() {
        let original = "fn main() {\n    routes![index, hello]\n}\n";
        let updated = remove_routes_entries_with_prefix(original, "channels::chat::");
        assert_eq!(updated, original);
    }

    #[test]
    fn remove_routes_entries_with_prefix_no_routes_macro_leaves_file_alone() {
        let original = "fn main() {}\n";
        let updated = remove_routes_entries_with_prefix(original, "channels::chat::");
        assert_eq!(updated, original);
    }

    #[test]
    fn remove_then_ensure_routes_entries_composes_for_transport_switch() {
        // Regression test for the `--force` transport-switch scenario: a
        // channel generated with SSE routes, then regenerated with the WS
        // route set, must not leave the stale SSE entries behind.
        let original = "fn main() {\n    routes![\n        channels::chat::chat_page,\n        channels::chat::chat_events,\n        channels::chat::chat_publish,\n    ]\n}\n";
        let stripped = remove_routes_entries_with_prefix(original, "channels::chat::");
        let updated = ensure_routes_entries(
            &stripped,
            &[
                "channels::chat::chat_ws".to_owned(),
                "channels::chat::chat_publish".to_owned(),
            ],
        );
        assert!(!updated.contains("chat_page"));
        assert!(!updated.contains("chat_events"));
        assert!(updated.contains("channels::chat::chat_ws"));
        assert!(updated.contains("channels::chat::chat_publish"));
    }

    // ── add_mail_preview_to_app ───────────────────────────────────────────

    fn app_main() -> &'static str {
        "use autumn_web::prelude::*;\n\
         \n\
         #[autumn_web::main]\n\
         async fn main() {\n\
             autumn_web::app()\n\
                 .routes(routes![index])\n\
                 .run()\n\
                 .await;\n\
         }\n"
    }

    #[test]
    fn add_mail_preview_inserts_before_run() {
        let updated = add_mail_preview_to_app(app_main(), "mailers::welcome::WelcomeMailer");
        assert!(
            updated.contains("mail_previews![mailers::welcome::WelcomeMailer]"),
            "must insert mail_previews call: {updated}"
        );
        let preview_pos = updated.find("mail_previews").unwrap();
        let run_pos = updated.find(".run()").unwrap();
        assert!(
            preview_pos < run_pos,
            "mail_previews must appear before .run(): {updated}"
        );
    }

    #[test]
    fn add_mail_preview_idempotent() {
        let first = add_mail_preview_to_app(app_main(), "mailers::welcome::WelcomeMailer");
        let second = add_mail_preview_to_app(&first, "mailers::welcome::WelcomeMailer");
        assert_eq!(first, second, "second call must be a no-op");
    }

    #[test]
    fn add_mail_preview_augments_existing_call() {
        let after_first = add_mail_preview_to_app(app_main(), "mailers::welcome::WelcomeMailer");
        let after_second = add_mail_preview_to_app(&after_first, "mailers::notify::NotifyMailer");
        assert!(after_second.contains("mailers::welcome::WelcomeMailer"));
        assert!(after_second.contains("mailers::notify::NotifyMailer"));
        assert_eq!(
            after_second.matches("mail_previews![").count(),
            1,
            "must not duplicate the mail_previews![] call: {after_second}"
        );
    }

    #[test]
    fn add_mail_preview_preserves_run_await() {
        let updated = add_mail_preview_to_app(app_main(), "mailers::welcome::WelcomeMailer");
        assert!(updated.contains(".run()"), ".run() must still be present");
        assert!(updated.contains(".await;"), ".await must still be present");
    }

    // ── add_jobs_registration_to_app ─────────────────────────────────────

    #[test]
    fn add_jobs_registration_inserts_before_run() {
        let updated = add_jobs_registration_to_app(app_main());
        assert!(
            updated.contains(".jobs(jobs::registered_jobs())"),
            "must insert .jobs call: {updated}"
        );
        let jobs_pos = updated.find(".jobs(").unwrap();
        let run_pos = updated.find(".run()").unwrap();
        assert!(
            jobs_pos < run_pos,
            ".jobs() must appear before .run(): {updated}"
        );
    }

    #[test]
    fn add_jobs_registration_idempotent() {
        let first = add_jobs_registration_to_app(app_main());
        let second = add_jobs_registration_to_app(&first);
        assert_eq!(first, second, "second call must be a no-op");
    }

    #[test]
    fn add_jobs_registration_preserves_run_await() {
        let updated = add_jobs_registration_to_app(app_main());
        assert!(updated.contains(".run()"), ".run() must still be present");
        assert!(updated.contains(".await;"), ".await must still be present");
    }

    #[test]
    fn add_jobs_registration_single_call_even_with_two_jobs() {
        let after_first = add_jobs_registration_to_app(app_main());
        // Simulates running generate job a second time — .jobs(...) is already there.
        let after_second = add_jobs_registration_to_app(&after_first);
        assert_eq!(
            after_second
                .matches(".jobs(jobs::registered_jobs())")
                .count(),
            1,
            "must not duplicate the .jobs() call"
        );
    }

    // ── augment_registered_jobs ───────────────────────────────────────────

    #[test]
    fn augment_registered_jobs_creates_fn_when_absent() {
        let mod_rs = "pub mod send_welcome_email;\n";
        let updated = augment_registered_jobs(mod_rs, "send_welcome_email::send_welcome_email");
        assert!(
            updated.contains("pub fn registered_jobs()"),
            "must create registered_jobs fn: {updated}"
        );
        assert!(
            updated.contains("jobs![send_welcome_email::send_welcome_email]"),
            "must include the new entry: {updated}"
        );
    }

    #[test]
    fn augment_registered_jobs_splices_into_existing() {
        let mod_rs = "pub mod send_welcome_email;\n\n\
            #[must_use]\n\
            pub fn registered_jobs() -> Vec<autumn_web::job::JobInfo> {\n    \
                autumn_web::jobs![send_welcome_email::send_welcome_email]\n}\n";
        let updated = augment_registered_jobs(mod_rs, "post_notification::post_notification");
        assert!(
            updated.contains("send_welcome_email::send_welcome_email"),
            "must preserve existing entry"
        );
        assert!(
            updated.contains("post_notification::post_notification"),
            "must include new entry"
        );
        assert_eq!(
            updated.matches("jobs![").count(),
            1,
            "must not duplicate jobs![]: {updated}"
        );
    }

    #[test]
    fn augment_registered_jobs_idempotent() {
        let mod_rs = "pub mod send_welcome_email;\n\n\
            #[must_use]\n\
            pub fn registered_jobs() -> Vec<autumn_web::job::JobInfo> {\n    \
                autumn_web::jobs![send_welcome_email::send_welcome_email]\n}\n";
        let second = augment_registered_jobs(mod_rs, "send_welcome_email::send_welcome_email");
        assert_eq!(mod_rs, second, "duplicate entry must be a no-op");
    }

    #[test]
    fn augment_registered_jobs_no_double_comma_with_trailing_comma() {
        // cargo fmt may produce trailing commas inside multi-line macro bodies.
        let mod_rs = "pub fn registered_jobs() -> Vec<autumn_web::job::JobInfo> {\n    \
            autumn_web::jobs![\n        send_welcome_email::send_welcome_email,\n    ]\n}\n";
        let updated = augment_registered_jobs(mod_rs, "post_notification::post_notification");
        assert!(
            !updated.contains(",,"),
            "must not produce double comma: {updated}"
        );
        assert!(
            updated.contains("post_notification::post_notification"),
            "must include new entry"
        );
    }

    #[test]
    fn augment_registered_jobs_empty_mod_rs_creates_full_fn() {
        let updated = augment_registered_jobs("", "foo::foo");
        assert!(updated.contains("pub fn registered_jobs()"));
        assert!(updated.contains("jobs![foo::foo]"));
    }

    // ── remove_job_entry / remove_jobs_registration_from_app (destroy, #1048) ──

    #[test]
    fn remove_job_entry_restores_original_when_it_was_the_only_one() {
        let base = "pub mod send_welcome_email;\n";
        let after_add = augment_registered_jobs(base, "send_welcome_email::send_welcome_email");
        assert_ne!(after_add, base);
        assert_eq!(
            remove_job_entry(&after_add, "send_welcome_email::send_welcome_email"),
            base
        );
    }

    #[test]
    fn remove_job_entry_keeps_other_entries() {
        let base = "pub mod send_welcome_email;\n";
        let with_one = augment_registered_jobs(base, "send_welcome_email::send_welcome_email");
        let with_both = augment_registered_jobs(&with_one, "post_notification::post_notification");
        let reverted = remove_job_entry(&with_both, "post_notification::post_notification");
        assert!(reverted.contains("send_welcome_email::send_welcome_email"));
        assert!(!reverted.contains("post_notification::post_notification"));
    }

    #[test]
    fn remove_job_entry_is_idempotent_when_absent() {
        let base = "pub mod send_welcome_email;\n";
        assert_eq!(remove_job_entry(base, "nonexistent::nonexistent"), base);
    }

    #[test]
    fn remove_jobs_registration_from_app_restores_original() {
        let base = "fn main() {\n    App::new()\n        .run()\n}\n";
        let after_add = add_jobs_registration_to_app(base);
        assert_ne!(after_add, base);
        assert_eq!(remove_jobs_registration_from_app(&after_add), base);
    }

    #[test]
    fn remove_jobs_registration_from_app_is_idempotent_when_absent() {
        let base = "fn main() {\n    App::new()\n        .run()\n}\n";
        assert_eq!(remove_jobs_registration_from_app(base), base);
    }

    #[test]
    fn remove_mail_preview_from_app_restores_original_when_only_entry() {
        let base = "fn main() {\n    App::new()\n        .run()\n}\n";
        let after_add = add_mail_preview_to_app(base, "WelcomeMailer");
        assert_ne!(after_add, base);
        assert_eq!(
            remove_mail_preview_from_app(&after_add, "WelcomeMailer"),
            base
        );
    }

    #[test]
    fn remove_mail_preview_from_app_keeps_other_entries() {
        let base = "fn main() {\n    App::new()\n        .run()\n}\n";
        let with_one = add_mail_preview_to_app(base, "WelcomeMailer");
        let with_both = add_mail_preview_to_app(&with_one, "ReceiptMailer");
        let reverted = remove_mail_preview_from_app(&with_both, "ReceiptMailer");
        assert!(reverted.contains("WelcomeMailer"));
        assert!(!reverted.contains("ReceiptMailer"));
    }

    #[test]
    fn remove_mail_preview_from_app_removes_a_rustfmt_wrapped_call() {
        // rustfmt commonly wraps `.mail_previews(mail_previews![...])`
        // across several lines once it has a couple of mailer names. Naive
        // line-start matching would strip only the opening line and leave
        // the rest (`WelcomeMailer,` / `])`) dangling — issue #1048 PR
        // review.
        let src = "fn main() {\n    App::new()\n        .mail_previews(mail_previews![\n            WelcomeMailer,\n        ])\n        .run()\n}\n";
        let updated = remove_mail_preview_from_app(src, "WelcomeMailer");
        assert!(
            !updated.contains("mail_previews"),
            "the whole call must be removed once its only entry is gone, got:\n{updated}"
        );
        assert_eq!(updated, "fn main() {\n    App::new()\n        .run()\n}\n");
    }

    #[test]
    fn remove_mail_preview_from_app_keeps_other_entry_in_a_rustfmt_wrapped_call() {
        let src = "fn main() {\n    App::new()\n        .mail_previews(mail_previews![\n            WelcomeMailer,\n            ReceiptMailer,\n        ])\n        .run()\n}\n";
        let updated = remove_mail_preview_from_app(src, "WelcomeMailer");
        assert!(
            updated.contains("mail_previews"),
            "the call must survive while another entry remains, got:\n{updated}"
        );
        assert!(updated.contains("ReceiptMailer"));
        assert!(!updated.contains("WelcomeMailer"));
    }

    #[test]
    fn remove_mail_preview_from_app_is_idempotent_when_absent() {
        let base = "fn main() {\n    App::new()\n        .run()\n}\n";
        assert_eq!(remove_mail_preview_from_app(base, "WelcomeMailer"), base);
    }

    // ── ensure_autumn_web_feature ─────────────────────────────────────────

    /// The `SQLite` decimal `CHECK` is SQL, so it is tested by running it —
    /// against a real in-memory `SQLite`, not by matching the string (issue
    /// #1924). Covers the digit budgets it exists for, and the malformed text
    /// that an earlier version admitted: a value that satisfies the constraint
    /// but fails `SqliteDecimal::from_sql` is a row nothing can load.
    #[test]
    fn sqlite_decimal_check_enforces_precision_scale_and_shape() {
        use diesel::connection::SimpleConnection as _;
        use diesel::prelude::*;

        let mut conn = diesel::SqliteConnection::establish(":memory:").expect("in-memory sqlite");
        // `decimal{10,2}`: at most 8 integer digits and 2 fractional.
        let check = sqlite_decimal_check("price", 10, 2);
        conn.batch_execute(&format!("CREATE TABLE t (price TEXT NULL {check})"))
            .expect("the generated CHECK must be valid SQLite SQL");

        let accepts = |conn: &mut diesel::SqliteConnection, value: &str| {
            diesel::sql_query(format!("INSERT INTO t (price) VALUES ('{value}')"))
                .execute(conn)
                .is_ok()
        };

        for value in ["0", "0.1", "19.99", "-19.99", "12345678.99", "-0.01"] {
            assert!(accepts(&mut conn, value), "`{value}` is in range");
        }
        for value in [
            // Over budget.
            "123456789.99",
            "19.999",
            "123456.789",
            // Malformed: no digit, or a stray/duplicated sign.
            "",
            "-",
            ".",
            "-.",
            "--1",
            "-1-",
            "1.2.3",
            "abc",
        ] {
            assert!(!accepts(&mut conn, value), "`{value}` must be rejected");
        }

        // Storage class, not just text shape. A BLOB whose BYTES spell a valid
        // decimal is the one case `TEXT` affinity will NOT convert, so it keeps
        // storage class blob and `FromSql<Text, Sqlite>` refuses it — an
        // unloadable row unless the CHECK rejects it up front.
        assert!(
            diesel::sql_query("INSERT INTO t (price) VALUES (x'31392e3939')")
                .execute(&mut conn)
                .is_err(),
            "a blob spelling `19.99` must be rejected: TEXT affinity does not convert it"
        );
        // Unquoted numeric literals ARE converted by TEXT affinity, so they are
        // stored as text and load fine — the CHECK must not reject them.
        for literal in ["19.99", "19", "-0.01"] {
            assert!(
                diesel::sql_query(format!("INSERT INTO t (price) VALUES ({literal})"))
                    .execute(&mut conn)
                    .is_ok(),
                "`{literal}` is converted to TEXT by affinity and must be accepted"
            );
        }

        // NULL is the column's own business, not the CHECK's.
        assert!(
            diesel::sql_query("INSERT INTO t (price) VALUES (NULL)")
                .execute(&mut conn)
                .is_ok(),
            "NULL must pass; NOT NULL decides that"
        );
    }

    #[test]
    fn ensure_feature_status_reports_not_found_when_dep_absent() {
        // No `autumn-web` dependency at all → status `false` so callers can warn.
        let cargo = "[package]\nname=\"x\"\n\n[dependencies]\nserde = \"1\"\n";
        let (out, applied) = ensure_autumn_web_feature_status(cargo, "db");
        assert!(!applied, "absent dependency must report not-applied");
        assert_eq!(
            out, cargo,
            "an unlocatable dependency leaves the toml untouched"
        );
    }

    #[test]
    fn ensure_feature_status_reports_applied_when_present() {
        let cargo = "[package]\nname=\"x\"\n\n[dependencies]\nautumn-web = \"0.6\"\n";
        let (_, added) = ensure_autumn_web_feature_status(cargo, "db");
        assert!(added, "adding the feature to a present dep reports applied");
        // Idempotent re-run still reports applied (feature already present).
        let once = ensure_autumn_web_feature(cargo, "db");
        let (_, again) = ensure_autumn_web_feature_status(&once, "db");
        assert!(again, "an already-present feature still reports applied");
    }

    #[test]
    fn ensure_feature_transforms_bare_string_dep() {
        let cargo = "[package]\nname=\"x\"\n\n[dependencies]\nautumn-web = \"0.6\"\n";
        let updated = ensure_autumn_web_feature(cargo, "mail");
        assert!(
            updated.contains("\"mail\""),
            "must add mail feature: {updated}"
        );
        assert!(
            updated.contains("version"),
            "must preserve version: {updated}"
        );
    }

    #[test]
    fn ensure_feature_idempotent_bare_string() {
        let cargo = "[package]\nname=\"x\"\n\n[dependencies]\nautumn-web = \"0.6\"\n";
        let once = ensure_autumn_web_feature(cargo, "mail");
        let twice = ensure_autumn_web_feature(&once, "mail");
        assert_eq!(once, twice, "second call must be a no-op");
    }

    #[test]
    fn ensure_feature_adds_to_existing_features_list() {
        let cargo = "[package]\nname=\"x\"\n\n[dependencies]\n\
                     autumn-web = { version = \"0.6\", features = [\"db\"] }\n";
        let updated = ensure_autumn_web_feature(cargo, "mail");
        assert!(updated.contains("\"mail\""));
        assert!(updated.contains("\"db\""), "must preserve existing feature");
    }

    #[test]
    fn ensure_feature_adds_features_key_when_absent() {
        let cargo = "[package]\nname=\"x\"\n\n[dependencies]\n\
                     autumn-web = { version = \"0.6\" }\n";
        let updated = ensure_autumn_web_feature(cargo, "mail");
        assert!(
            updated.contains("\"mail\""),
            "must add features key: {updated}"
        );
    }

    #[test]
    fn ensure_feature_idempotent_inline_table() {
        let cargo = "[package]\nname=\"x\"\n\n[dependencies]\n\
                     autumn-web = { version = \"0.6\", features = [\"mail\"] }\n";
        let updated = ensure_autumn_web_feature(cargo, "mail");
        assert_eq!(cargo, updated, "already-present feature must be a no-op");
    }

    #[test]
    fn ensure_feature_ignores_unrelated_deps() {
        let cargo = "[package]\nname=\"x\"\n\n[dependencies]\n\
                     serde = \"1\"\nautumn-web = \"0.6\"\ntracing = \"0.1\"\n";
        let updated = ensure_autumn_web_feature(cargo, "mail");
        assert!(
            updated.contains("serde = \"1\""),
            "unrelated dep must be preserved"
        );
        assert!(updated.contains("\"mail\""));
    }

    #[test]
    fn ensure_feature_returns_unchanged_when_autumn_web_absent() {
        let cargo = "[package]\nname=\"x\"\n\n[dependencies]\nserde = \"1\"\n";
        let updated = ensure_autumn_web_feature(cargo, "mail");
        assert_eq!(cargo, updated, "no autumn-web dep → must be a no-op");
    }

    #[test]
    fn ensure_feature_dep_without_closing_brace_uses_fallback() {
        // Malformed line — none of the three forms match, fallback returns unchanged.
        let cargo = "[package]\nname=\"x\"\n\n[dependencies]\nautumn-web = malformed\n";
        let updated = ensure_autumn_web_feature(cargo, "mail");
        // The function should not panic; it falls back to the existing line.
        assert!(updated.contains("autumn-web = malformed"));
    }

    #[test]
    fn ensure_feature_multiline_section_adds_to_existing_features() {
        let cargo = "[package]\nname=\"x\"\n\n[dependencies.autumn-web]\nversion = \"0.6\"\nfeatures = [\"db\"]\n";
        let updated = ensure_autumn_web_feature(cargo, "inbound-mailgun");
        assert!(
            updated.contains("\"inbound-mailgun\""),
            "must add feature to section: {updated}"
        );
        assert!(updated.contains("\"db\""), "must preserve existing feature");
    }

    #[test]
    fn ensure_feature_multiline_section_inserts_features_when_absent() {
        let cargo = "[package]\nname=\"x\"\n\n[dependencies.autumn-web]\nversion = \"0.6\"\n";
        let updated = ensure_autumn_web_feature(cargo, "inbound-mailgun");
        assert!(
            updated.contains("\"inbound-mailgun\""),
            "must insert features line: {updated}"
        );
    }

    #[test]
    fn ensure_feature_multiline_section_idempotent() {
        let cargo = "[package]\nname=\"x\"\n\n[dependencies.autumn-web]\nversion = \"0.6\"\nfeatures = [\"inbound-mailgun\"]\n";
        let updated = ensure_autumn_web_feature(cargo, "inbound-mailgun");
        assert_eq!(
            cargo, updated,
            "already-present feature in section must be a no-op"
        );
    }

    #[test]
    fn ensure_feature_trailing_comment_on_string_dep() {
        let cargo = "[package]\nname=\"x\"\n\n[dependencies]\nautumn-web = \"0.6\" # framework\n";
        let updated = ensure_autumn_web_feature(cargo, "mail");
        assert!(
            updated.contains("\"mail\""),
            "must add feature despite trailing comment: {updated}"
        );
        assert!(
            updated.contains("version"),
            "must preserve version: {updated}"
        );
    }

    #[test]
    fn ensure_feature_trailing_comma_in_features_list() {
        let cargo = "[package]\nname=\"x\"\n\n[dependencies]\nautumn-web = { version = \"0.6\", features = [\"db\",] }\n";
        let updated = ensure_autumn_web_feature(cargo, "mail");
        assert!(
            updated.contains("\"mail\""),
            "must add feature after trailing comma: {updated}"
        );
        assert!(
            !updated.contains(",,"),
            "must not produce double comma: {updated}"
        );
    }

    #[test]
    fn ensure_feature_dotted_workspace_inserts_features_line() {
        let cargo = "[package]\nname=\"x\"\n\n[dependencies]\nautumn-web.workspace = true\n";
        let updated = ensure_autumn_web_feature(cargo, "inbound-mailgun");
        assert!(
            updated.contains("\"inbound-mailgun\""),
            "must insert features line: {updated}"
        );
        assert!(
            updated.contains("autumn-web.features"),
            "must use dotted key form: {updated}"
        );
    }

    #[test]
    fn ensure_feature_dotted_workspace_existing_features_line_spliced() {
        let cargo = "[package]\nname=\"x\"\n\n[dependencies]\nautumn-web.workspace = true\nautumn-web.features = [\"db\"]\n";
        let updated = ensure_autumn_web_feature(cargo, "mail");
        assert!(
            updated.contains("\"mail\""),
            "must splice into existing features line: {updated}"
        );
        assert!(
            updated.contains("\"db\""),
            "must preserve existing feature: {updated}"
        );
    }

    #[test]
    fn ensure_feature_dotted_workspace_idempotent() {
        let cargo = "[package]\nname=\"x\"\n\n[dependencies]\nautumn-web.workspace = true\nautumn-web.features = [\"inbound-mailgun\"]\n";
        let updated = ensure_autumn_web_feature(cargo, "inbound-mailgun");
        assert_eq!(cargo, updated, "already-present feature must be a no-op");
    }

    #[test]
    fn ensure_feature_ignores_commented_features() {
        // commented out in multiline section
        let cargo_multiline = "[package]\nname=\"x\"\n\n[dependencies.autumn-web]\nversion = \"0.6\"\n# features = [\"inbound-mailgun\"]\n";
        let updated_multiline = ensure_autumn_web_feature(cargo_multiline, "inbound-mailgun");
        assert!(updated_multiline.contains("features = [\"inbound-mailgun\"]"));

        // commented out in dotted dependency section
        let cargo_dotted = "[package]\nname=\"x\"\n\n[dependencies]\nautumn-web.workspace = true\n# autumn-web.features = [\"inbound-mailgun\"]\n";
        let updated_dotted = ensure_autumn_web_feature(cargo_dotted, "inbound-mailgun");
        assert!(updated_dotted.contains("autumn-web.features = [\"inbound-mailgun\"]"));

        // commented out in multiline inline table section
        let cargo_inline = "[package]\nname=\"x\"\n\n[dependencies]\nautumn-web = {\n  version = \"0.6\",\n  # features = [\"inbound-mailgun\"]\n}\n";
        let updated_inline = ensure_autumn_web_feature(cargo_inline, "inbound-mailgun");
        assert!(updated_inline.contains("features = [\"inbound-mailgun\"]"));
    }

    #[test]
    fn ensure_feature_multiline_inline_table_inserts_features() {
        let cargo =
            "[package]\nname=\"x\"\n\n[dependencies]\nautumn-web = {\n  version = \"0.6\"\n}\n";
        let updated = ensure_autumn_web_feature(cargo, "mail");
        assert!(
            updated.contains("\"mail\""),
            "must add feature to multiline inline table: {updated}"
        );
        assert!(
            updated.contains("version = \"0.6\","),
            "must add trailing comma to preceding entry: {updated}"
        );
    }

    #[test]
    fn ensure_feature_package_alias_dep_skipped_for_non_autumn_web_alias() {
        // An alias whose name does not normalise to `autumn_web` (e.g. `aw`)
        // must be skipped — the generated code imports `autumn_web::`, so adding
        // the feature to a crate named `aw` would leave the project uncompilable.
        let cargo = "[package]\nname=\"x\"\n\n[dependencies]\naw = { package = \"autumn-web\", version = \"0.6\" }\n";
        let updated = ensure_autumn_web_feature(cargo, "mail");
        assert_eq!(
            cargo, updated,
            "non-autumn_web alias must be left unchanged"
        );
    }

    #[test]
    fn ensure_feature_package_alias_dep_autumn_web_alias() {
        // An alias explicitly named `autumn_web` (with underscore) and
        // `package = "autumn-web"` is compatible with generated imports.
        let cargo = "[package]\nname=\"x\"\n\n[dependencies]\nautumn_web = { package = \"autumn-web\", version = \"0.6\" }\n";
        let updated = ensure_autumn_web_feature(cargo, "mail");
        assert!(
            updated.contains("\"mail\""),
            "autumn_web alias must have feature added: {updated}"
        );
    }

    #[test]
    fn ensure_feature_package_alias_dep_idempotent() {
        // Same as above but the feature is already present; must be a no-op.
        let cargo = "[package]\nname=\"x\"\n\n[dependencies]\nautumn_web = { package = \"autumn-web\", version = \"0.6\", features = [\"mail\"] }\n";
        let updated = ensure_autumn_web_feature(cargo, "mail");
        assert_eq!(cargo, updated, "already-present feature must be a no-op");
    }

    #[test]
    fn ensure_feature_commented_dep_line_is_skipped() {
        // A commented-out dep like `# aw = { package = "autumn-web" }` must not
        // be treated as the actual dependency.  The real dep below must be updated.
        let cargo = "[package]\nname=\"x\"\n\n[dependencies]\n# aw = { package = \"autumn-web\", version = \"0.6\" }\nautumn-web = \"0.6\"\n";
        let updated = ensure_autumn_web_feature(cargo, "inbound-mailgun");
        // The comment line must be unchanged.
        assert!(
            updated.contains("# aw = { package"),
            "comment line must be preserved as-is: {updated}"
        );
        // The real dep must have the feature added.
        let real_dep_line = updated
            .lines()
            .find(|l| l.trim_start().starts_with("autumn-web") && !l.trim_start().starts_with('#'))
            .unwrap_or("");
        assert!(
            real_dep_line.contains("\"inbound-mailgun\""),
            "feature must be added to the real dep line: {real_dep_line}"
        );
    }

    #[test]
    fn ensure_feature_multiline_inline_table_idempotent() {
        let cargo = "[package]\nname=\"x\"\n\n[dependencies]\nautumn-web = {\n  version = \"0.6\",\n  features = [\"mail\"]\n}\n";
        let updated = ensure_autumn_web_feature(cargo, "mail");
        assert_eq!(cargo, updated, "already-present feature must be a no-op");
    }

    #[test]
    fn ensure_feature_multiline_section_trailing_comment_on_header() {
        let cargo =
            "[package]\nname=\"x\"\n\n[dependencies.autumn-web] # pinned\nversion = \"0.6\"\n";
        let updated = ensure_autumn_web_feature(cargo, "mail");
        assert!(
            updated.contains("\"mail\""),
            "must handle trailing comment on section header: {updated}"
        );
    }

    #[test]
    fn add_mail_preview_unclosed_bracket_returns_unchanged() {
        // Malformed source: `mail_previews![` with no closing `]`.
        let src = "app()\n    .mail_previews(mail_previews![Foo)\n    .run()\n    .await;\n";
        let updated = add_mail_preview_to_app(src, "Bar");
        // Must not panic; returns the original string unchanged.
        assert_eq!(src, updated);
    }

    #[test]
    fn add_mail_preview_no_run_returns_string_with_preview_appended() {
        // Source with no `.run()` call — insertion is skipped, function still returns.
        let src = "app()\n    .routes(routes![index])\n";
        let updated = add_mail_preview_to_app(src, "mailers::welcome::WelcomeMailer");
        // No `.run()` means we can't find an insertion point; original is returned.
        assert!(
            !updated.contains("mail_previews"),
            "no insertion point → no insertion"
        );
    }

    #[test]
    fn test_singularize_simple() {
        assert_eq!(singularize("posts"), "post");
        assert_eq!(singularize("categories"), "category");
        assert_eq!(singularize("wishes"), "wish");
        assert_eq!(singularize("test_search_records"), "test_search_record");
    }

    #[test]
    fn test_parse_model_search_config_simple() {
        let content = r#"
#[autumn_web::model(table = "test_search_records")]
#[searchable(language = "english")]
#[derive(PartialEq, Eq)]
pub struct SearchRecord {
    #[id]
    pub id: i64,
    #[searchable(weight = "A")]
    pub title: String,
    #[searchable(weight = "B")]
    pub body: String,
}
"#;
        let (lang, fields) = parse_model_search_config(content).unwrap();
        assert_eq!(lang, "english");
        assert_eq!(
            fields,
            vec![("title".to_string(), 'A'), ("body".to_string(), 'B'),]
        );
    }

    #[test]
    fn test_parse_model_search_config_robustness() {
        // 1. Check space-less language parsing
        let content_spaceless = r#"
#[autumn_web::model(table = "test_search_records")]
#[searchable(language="english")]
pub struct SearchRecord {
    #[id]
    pub id: i64,
    #[searchable]
    pub title: String,
}
"#;
        let (lang, fields) = parse_model_search_config(content_spaceless).unwrap();
        assert_eq!(lang, "english");
        assert_eq!(fields, vec![("title".to_string(), 'D')]);

        // 2. Check unweighted vs weighted weight inheritance leakage
        let content_leakage = r#"
#[autumn_web::model(table = "test_search_records")]
#[searchable(language = "simple")]
pub struct SearchRecord {
    #[id]
    pub id: i64,
    #[searchable]
    pub title: String,
    #[searchable(weight = "B")]
    pub body: String,
}
"#;
        let (_, fields) = parse_model_search_config(content_leakage).unwrap();
        assert_eq!(
            fields,
            vec![
                ("title".to_string(), 'D'), // title MUST NOT inherit B from body!
                ("body".to_string(), 'B'),
            ]
        );

        // 3. Check comment stripping (both block and line comments containing #[searchable])
        let content_comments = r#"
#[autumn_web::model(table = "test_search_records")]
#[searchable(language = "english")]
pub struct SearchRecord {
    #[id]
    pub id: i64,
    // #[searchable(weight = "A")]
    // pub old_title: String,
    /*
    #[searchable(weight = "C")]
    pub commented_out: String,
    */
    #[searchable(weight = "B")]
    pub body: String,
}
"#;
        let (_, fields) = parse_model_search_config(content_comments).unwrap();
        assert_eq!(fields, vec![("body".to_string(), 'B')]);

        // 4. Check prefix collisions like searchable_fields
        let content_collision = r#"
#[autumn_web::model(table = "test_search_records")]
#[searchable_fields]
pub struct SearchRecord {
    #[id]
    pub id: i64,
    #[searchable(weight = "A")]
    pub title: String,
}
"#;
        let (_, fields) = parse_model_search_config(content_collision).unwrap();
        assert_eq!(fields, vec![("title".to_string(), 'A')]);
    }

    #[test]
    fn test_singularize_ses_words() {
        assert_eq!(singularize("cases"), "case");
        assert_eq!(singularize("databases"), "database");
        assert_eq!(singularize("phases"), "phase");
        assert_eq!(singularize("uses"), "use");
        assert_eq!(singularize("statuses"), "status");
        assert_eq!(singularize("aliases"), "alias");
        assert_eq!(singularize("buses"), "bus");
    }

    #[test]
    fn test_singularize_irregular_plurals() {
        assert_eq!(singularize("people"), "person");
        assert_eq!(singularize("salespeople"), "salesperson");
        assert_eq!(singularize("children"), "child");
        assert_eq!(singularize("supermen"), "superman");
        assert_eq!(singularize("women"), "woman");
    }

    #[test]
    fn test_parse_model_search_config_helper_structs() {
        let content = r#"
pub struct HelperOne {
    pub a: i32,
}

#[autumn_web::model(table = "pages")]
#[searchable(language = "english")]
pub struct Page {
    #[id]
    pub id: i64,
    #[searchable(weight = "A")]
    pub title: String,
}

pub struct HelperTwo {
    #[searchable(weight = "B")]
    pub b: String,
}
"#;
        let (lang, fields) = parse_model_search_config(content).unwrap();
        assert_eq!(lang, "english");
        assert_eq!(fields, vec![("title".to_string(), 'A')]);
    }

    #[test]
    fn test_detect_migration_shape_to_tables() {
        // Tables starting with "To" should match FTS
        match detect_migration_shape("AddSearchToTodos") {
            MigrationShape::AddSearch { table } => assert_eq!(table, "todos"),
            other => panic!("expected AddSearch, got {other:?}"),
        }
        match detect_migration_shape("AddSearchToTopics") {
            MigrationShape::AddSearch { table } => assert_eq!(table, "topics"),
            other => panic!("expected AddSearch, got {other:?}"),
        }
        // Normal column additions starting with AddSearch should fall through to AddColumns
        match detect_migration_shape("AddSearchTokenToPosts") {
            MigrationShape::AddColumns { table } => assert_eq!(table, "posts"),
            other => panic!("expected AddColumns, got {other:?}"),
        }
    }

    #[test]
    fn test_singularize_movies_and_series() {
        assert_eq!(singularize("movies"), "movie");
        assert_eq!(singularize("series"), "series");
        assert_eq!(singularize("cookies"), "cookie");
        assert_eq!(singularize("zombies"), "zombie");
    }

    #[test]
    fn test_detect_migration_shape_internal_to() {
        match detect_migration_shape("AddSearchToTopToBottoms") {
            MigrationShape::AddSearch { table } => assert_eq!(table, "top_to_bottoms"),
            other => panic!("expected AddSearch, got {other:?}"),
        }
        match detect_migration_shape("AddSearchToToDoItems") {
            MigrationShape::AddSearch { table } => assert_eq!(table, "to_do_items"),
            other => panic!("expected AddSearch, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_model_search_config_raw_identifiers() {
        let content = r#"
#[autumn_web::model(table = "items")]
pub struct Item {
    #[id]
    pub id: i64,
    #[searchable]
    pub r#type: String,
    #[searchable(weight = "B")]
    pub r#match: String,
}
"#;
        let (lang, fields) = parse_model_search_config(content).unwrap();
        assert_eq!(lang, "simple");
        assert_eq!(
            fields,
            vec![("type".to_string(), 'D'), ("match".to_string(), 'B')]
        );
    }

    #[test]
    fn test_parse_model_search_config_reverse_lookup() {
        // Helper struct before the model has a #[searchable(language = "french")] attribute.
        // We want to make sure the model struct parses its own #[searchable(language = "english")]
        // because it is closest (reverse scanning), not the earlier one.
        let content = r#"
#[searchable(language = "french")]
pub struct HelperOne {
    pub a: i32,
}

#[autumn_web::model(table = "pages")]
#[searchable(language = "english")]
pub struct Page {
    #[id]
    pub id: i64,
    #[searchable(weight = "A")]
    pub title: String,
}
"#;
        let (lang, fields) = parse_model_search_config(content).unwrap();
        assert_eq!(lang, "english");
        assert_eq!(fields, vec![("title".to_string(), 'A')]);
    }

    #[test]
    fn test_strip_comments_in_string_literals() {
        let content = r#"
        let url = "https://example.com/api"; // this is a comment
        /* block comment */
        let regex = r"//[a-z]+";
        "#;
        let stripped = strip_comments(content);
        assert!(stripped.contains("https://example.com/api"));
        assert!(!stripped.contains("this is a comment"));
        assert!(!stripped.contains("block comment"));
    }

    #[test]
    fn test_parse_model_search_config_for_table_multi() {
        let content = r#"
#[autumn_web::model(table = "posts")]
#[searchable(language = "english")]
pub struct Post {
    #[id]
    pub id: i64,
    #[searchable(weight = "A")]
    pub title: String,
}

#[autumn_web::model(table = "comments")]
#[searchable(language = "spanish")]
pub struct Comment {
    #[id]
    pub id: i64,
    #[searchable(weight = "B")]
    pub body: String,
}
"#;
        // Verify post scanning
        let (post_lang, post_fields) =
            parse_model_search_config_for_table(content, "posts").unwrap();
        assert_eq!(post_lang, "english");
        assert_eq!(post_fields, vec![("title".to_string(), 'A')]);

        // Verify comment scanning
        let (comment_lang, comment_fields) =
            parse_model_search_config_for_table(content, "comments").unwrap();
        assert_eq!(comment_lang, "spanish");
        assert_eq!(comment_fields, vec![("body".to_string(), 'B')]);
    }

    #[test]
    fn test_strip_comments_raw_strings() {
        let content = r###"
        let r1 = r#"quote " inside raw"#;
        let r2 = r##"// not comment inside raw block"##;
        let r3 = r#"/* not block comment */"#;
        "###;
        let stripped = strip_comments(content);
        assert!(stripped.contains("r#\"quote \" inside raw\"#"));
        assert!(stripped.contains("r##\"// not comment inside raw block\"##"));
        assert!(stripped.contains("r#\"/* not block comment */\"#"));
    }

    #[test]
    fn test_parse_model_search_config_spacing_insensitivity() {
        let content_spacing = r#"
#[autumn_web::model( table  =   "posts" )]
#[searchable(language = "english")]
pub struct Post {
    #[id]
    pub id: i64,
    #[searchable(weight = "A")]
    pub title: String,
}
"#;
        let (lang, fields) = parse_model_search_config_for_table(content_spacing, "posts").unwrap();
        assert_eq!(lang, "english");
        assert_eq!(fields, vec![("title".to_string(), 'A')]);
    }

    #[test]
    fn test_parse_model_search_config_pascal_fallback() {
        let content = r#"
#[autumn_web::model]
#[searchable(language = "english")]
pub struct Post {
    #[id]
    pub id: i64,
    #[searchable(weight = "A")]
    pub title: String,
}

#[autumn_web::model]
#[searchable(language = "spanish")]
pub struct Comment {
    #[id]
    pub id: i64,
    #[searchable(weight = "B")]
    pub body: String,
}
"#;
        let (lang, fields) = parse_model_search_config_for_table(content, "comments").unwrap();
        assert_eq!(lang, "spanish");
        assert_eq!(fields, vec![("body".to_string(), 'B')]);
    }

    #[test]
    fn test_singularize_specimens_and_gentlemen() {
        assert_eq!(singularize("specimens"), "specimen");
        assert_eq!(singularize("regimens"), "regimen");
        assert_eq!(singularize("gentlemen"), "gentleman");
        assert_eq!(singularize("firemen"), "fireman");
    }

    #[test]
    fn test_singularize_ses_trailing_e() {
        assert_eq!(singularize("houses"), "house");
        assert_eq!(singularize("phrases"), "phrase");
        assert_eq!(singularize("guesses"), "guess");
        assert_eq!(singularize("lenses"), "lens");
        assert_eq!(singularize("databases"), "database");
        assert_eq!(singularize("cases"), "case");
    }

    #[test]
    fn test_parse_model_search_config_raw_string_table_attr() {
        let content_raw_table = r##"
#[autumn_web::model(table = r#"raw_posts"#)]
#[searchable(language = "english")]
pub struct Post {
    #[id]
    pub id: i64,
    #[searchable(weight = "A")]
    pub title: String,
}
"##;
        let (lang, fields) =
            parse_model_search_config_for_table(content_raw_table, "raw_posts").unwrap();
        assert_eq!(lang, "english");
        assert_eq!(fields, vec![("title".to_string(), 'A')]);
    }

    #[test]
    fn test_parse_model_search_config_braces_in_attributes() {
        let content_braces = r#"
#[autumn_web::model(table = "posts")]
#[searchable(language = "english")]
pub struct Post {
    #[id]
    pub id: i64,
    #[searchable(weight = "A")]
    #[validate(custom = "foo } bar")]
    pub title: String,
    #[searchable(weight = "B")]
    pub body: String,
}
"#;
        let (_, fields) = parse_model_search_config_for_table(content_braces, "posts").unwrap();
        assert_eq!(
            fields,
            vec![("title".to_string(), 'A'), ("body".to_string(), 'B')]
        );
    }

    #[test]
    fn test_parse_model_search_config_invalid_weight_fallback() {
        let content_invalid_weight = r#"
#[autumn_web::model(table = "posts")]
#[searchable(language = "english")]
pub struct Post {
    #[id]
    pub id: i64,
    #[searchable(weight = "Z")]
    pub title: String,
}
"#;
        let (_, fields) =
            parse_model_search_config_for_table(content_invalid_weight, "posts").unwrap();
        assert_eq!(fields, vec![("title".to_string(), 'D')]);
    }

    #[test]
    fn test_singularize_singular_table_names_ending_in_s() {
        assert_eq!(singularize("news"), "news");
        assert_eq!(singularize("status"), "status");
        assert_eq!(singularize("alias"), "alias");
        assert_eq!(singularize("bus"), "bus");
        assert_eq!(singularize("lens"), "lens");
        assert_eq!(singularize("virus"), "virus");
        assert_eq!(singularize("canvas"), "canvas");
        assert_eq!(singularize("addresses"), "address");
        assert_eq!(singularize("address"), "address");
    }

    #[test]
    fn test_parse_model_search_config_diesel_column_name() {
        let content_diesel = r##"
#[autumn_web::model(table = "posts")]
pub struct Post {
    #[id]
    pub id: i64,
    #[searchable(weight = "A")]
    #[diesel(column_name = "headline")]
    pub title: String,
    #[diesel(column_name = r#"content_body"#)]
    #[searchable(weight = "B")]
    pub body: String,
}
"##;
        let (_, fields) = parse_model_search_config_for_table(content_diesel, "posts").unwrap();
        assert_eq!(
            fields,
            vec![
                ("headline".to_string(), 'A'),
                ("content_body".to_string(), 'B')
            ]
        );
    }

    #[test]
    fn test_strip_comments_nested_block() {
        let content = "pub /* outer /* inner */ still outer */ struct Post {}";
        let stripped = strip_comments(content);
        assert_eq!(stripped.trim(), "pub   struct Post {}");
    }

    #[test]
    fn test_strip_comments_token_boundary() {
        let content = "pub/*comment*/struct Post";
        let stripped = strip_comments(content);
        assert_eq!(stripped.trim(), "pub struct Post");
    }

    #[test]
    fn test_parse_model_search_config_strict_fallback() {
        let content_multi = r#"
#[autumn_web::model(table = "posts")]
pub struct Post {
    #[id]
    pub id: i64,
    #[searchable]
    pub title: String,
}

#[autumn_web::model(table = "comments")]
pub struct Comment {
    #[id]
    pub id: i64,
    #[searchable]
    pub body: String,
}
"#;
        // Non-existent table should return None instead of falling back to the first struct (Post)
        assert!(parse_model_search_config_for_table(content_multi, "users").is_none());

        // Valid tables should still match perfectly
        let (_, posts_fields) =
            parse_model_search_config_for_table(content_multi, "posts").unwrap();
        assert_eq!(posts_fields[0].0, "title");

        let (_, comments_fields) =
            parse_model_search_config_for_table(content_multi, "comments").unwrap();
        assert_eq!(comments_fields[0].0, "body");
    }

    #[test]
    fn test_extract_diesel_column_name_identifier() {
        let content_diesel = r#"
#[autumn_web::model(table = "posts")]
pub struct Post {
    #[id]
    pub id: i64,
    #[searchable(weight = "A")]
    #[diesel(column_name = headline)]
    pub title: String,
    #[diesel(column_name = content_body)]
    #[searchable(weight = "B")]
    pub body: String,
}
"#;
        let (_, fields) = parse_model_search_config_for_table(content_diesel, "posts").unwrap();
        assert_eq!(
            fields,
            vec![
                ("headline".to_string(), 'A'),
                ("content_body".to_string(), 'B')
            ]
        );
    }

    #[test]
    fn test_singularize_greek_origin_nouns() {
        assert_eq!(singularize("analyses"), "analysis");
        assert_eq!(singularize("crises"), "crisis");
        assert_eq!(singularize("diagnoses"), "diagnosis");
        assert_eq!(singularize("neuroses"), "neurosis");
        assert_eq!(singularize("bases"), "basis");
        assert_eq!(singularize("oases"), "oasis");

        // English-origin standard plurals ending in ases/ises
        assert_eq!(singularize("databases"), "database");
        assert_eq!(singularize("phases"), "phase");
        assert_eq!(singularize("premises"), "premise");
    }

    #[test]
    fn test_extract_diesel_column_name_raw_identifier() {
        let content_diesel = r#"
#[autumn_web::model(table = "posts")]
pub struct Post {
    #[id]
    pub id: i64,
    #[searchable(weight = "A")]
    #[diesel(column_name = r#headline)]
    pub title: String,
    #[diesel(column_name = r#content_body)]
    #[searchable(weight = "B")]
    pub body: String,
}
"#;
        let (_, fields) = parse_model_search_config_for_table(content_diesel, "posts").unwrap();
        assert_eq!(
            fields,
            vec![
                ("headline".to_string(), 'A'),
                ("content_body".to_string(), 'B')
            ]
        );
    }

    #[test]
    fn test_parse_model_search_config_model_helper_collision() {
        let content_collision = r#"
#[model_helper(table = "posts")]
pub struct Helper {}

#[autumn_web::model(table = "posts")]
pub struct Post {
    #[id]
    pub id: i64,
    #[searchable]
    pub title: String,
}
"#;
        let (_, fields) = parse_model_search_config_for_table(content_collision, "posts").unwrap();
        assert_eq!(fields[0].0, "title");
    }

    #[test]
    fn test_parse_model_search_config_multiline_attributes() {
        let content_diesel = r#"
#[autumn_web::model(table = "posts")]
pub struct Post {
    #[id]
    pub id: i64,
    #[searchable(weight = "A")]
    #[diesel(
        sql_type = Text,
        column_name = "headline"
    )]
    pub title: String,
}
"#;
        let (_, fields) = parse_model_search_config_for_table(content_diesel, "posts").unwrap();
        assert_eq!(fields, vec![("headline".to_string(), 'A')]);
    }

    #[test]
    fn test_parse_model_search_config_preceding_comma_in_attribute() {
        let content_diesel = r#"
#[autumn_web::model(table = "posts")]
pub struct Post {
    #[id]
    pub id: i64,
    #[diesel(sql_type = Text, column_name = headline)]
    #[searchable(weight = "A")]
    pub title: String,
}
"#;
        let (_, fields) = parse_model_search_config_for_table(content_diesel, "posts").unwrap();
        assert_eq!(fields, vec![("headline".to_string(), 'A')]);
    }

    #[test]
    fn test_parse_model_search_config_multiple_preceding_attributes() {
        let content_diesel = r#"
#[autumn_web::model(table = "posts")]
pub struct Post {
    #[id]
    pub id: i64,
    #[diesel(sql_type = Text)]
    #[diesel(column_name = headline)]
    #[searchable(weight = "A")]
    pub title: String,
}
"#;
        let (_, fields) = parse_model_search_config_for_table(content_diesel, "posts").unwrap();
        assert_eq!(fields, vec![("headline".to_string(), 'A')]);
    }

    #[test]
    fn test_add_search_up_sql_quotes_columns() {
        let sql = add_search_up_sql_for(
            DatabaseBackend::Postgres,
            "posts",
            "english",
            &[("title".to_string(), 'A'), ("body".to_string(), 'B')],
        )
        .expect("postgres search DDL never errors");
        assert!(sql.contains("coalesce(\"title\"::text, '')"));
        assert!(sql.contains("coalesce(\"body\"::text, '')"));
    }

    #[test]
    fn test_add_search_up_sql_sqlite_emits_fts5_external_content_and_triggers() {
        // #1910: the SQLite arm emits an external-content FTS5 vtable over the
        // SEARCH_FIELDS columns, maintenance triggers, and a backfill rebuild —
        // and NO Postgres tsvector/GIN DDL.
        let up = add_search_up_sql_for(
            DatabaseBackend::Sqlite,
            "posts",
            "english",
            &[("title".to_string(), 'A'), ("body".to_string(), 'B')],
        )
        .expect("valid searchable columns generate FTS5 DDL");
        assert!(
            up.contains(
                "CREATE VIRTUAL TABLE \"posts__fts\" USING fts5(\"title\", \"body\", \
                 content='posts', content_rowid='id', tokenize='unicode61');"
            ),
            "up: {up}"
        );
        // Insert trigger writes the new row into the index.
        assert!(
            up.contains(
                "INSERT INTO \"posts__fts\"(rowid, \"title\", \"body\") \
                 VALUES (new.id, new.\"title\", new.\"body\");"
            ),
            "up: {up}"
        );
        // Delete trigger tombstones the old row via the external-content 'delete'
        // command.
        assert!(
            up.contains(
                "INSERT INTO \"posts__fts\"(\"posts__fts\", rowid, \"title\", \"body\") \
                 VALUES('delete', old.id, old.\"title\", old.\"body\");"
            ),
            "up: {up}"
        );
        // Update trigger does both (delete-then-insert).
        assert!(up.contains("CREATE TRIGGER \"posts__fts_au\""), "up: {up}");
        assert!(
            up.contains("INSERT INTO \"posts__fts\"(\"posts__fts\") VALUES('rebuild');"),
            "up: {up}"
        );
        for leak in ["tsvector", "to_tsvector", "USING gin", "search_vector"] {
            assert!(!up.contains(leak), "SQLite up leaked `{leak}`: {up}");
        }

        // down.sql drops the three triggers then the FTS table.
        let down = add_search_down_sql_for(DatabaseBackend::Sqlite, "posts");
        for trig in ["posts__fts_au", "posts__fts_ad", "posts__fts_ai"] {
            assert!(
                down.contains(&format!("DROP TRIGGER IF EXISTS \"{trig}\";")),
                "down: {down}"
            );
        }
        assert!(
            down.contains("DROP TABLE IF EXISTS \"posts__fts\";"),
            "down: {down}"
        );
    }

    #[test]
    fn test_add_search_up_sql_sqlite_rejects_fts5_reserved_column_names() {
        // #1910 / #1614 AC #4: FTS5 reserves `rowid`/`rank` (and forbids a column
        // named the same as the `<table>__fts` table). A #[searchable] field with
        // such a name must be rejected at GENERATE time with an actionable message
        // — not silently emitted to fail only at `autumn migrate` time. Reserved
        // names are matched case-insensitively, and quoting does not save them.
        for reserved in ["rowid", "rank", "RANK", "RowId", "posts__fts"] {
            let err = add_search_up_sql_for(
                DatabaseBackend::Sqlite,
                "posts",
                "english",
                &[("title".to_string(), 'A'), (reserved.to_string(), 'B')],
            )
            .expect_err(&format!("reserved column `{reserved}` must be rejected"));
            let msg = err.to_string();
            assert!(
                msg.contains(reserved) && msg.contains("FTS5-reserved"),
                "message must name the offending field `{reserved}` and the reason: {msg}"
            );
            assert!(
                msg.contains("Rename the field") && msg.contains("#[searchable]"),
                "message must be actionable (rename / drop #[searchable]): {msg}"
            );
        }

        // A normal searchable field still generates the FTS5 DDL (guard is not a
        // blanket rejection), and Postgres never sees this reservation.
        let up = add_search_up_sql_for(
            DatabaseBackend::Sqlite,
            "posts",
            "english",
            &[("rank_note".to_string(), 'A'), ("body".to_string(), 'B')],
        )
        .expect("non-reserved columns (even `rank_note`) still generate FTS5 DDL");
        assert!(
            up.contains("CREATE VIRTUAL TABLE \"posts__fts\" USING fts5(\"rank_note\", \"body\", "),
            "up: {up}"
        );
        // The Postgres arm indexes a field literally named `rank` with no error —
        // the reservation is FTS5-only and must not weaken the pg path.
        let pg = add_search_up_sql_for(
            DatabaseBackend::Postgres,
            "posts",
            "english",
            &[("rank".to_string(), 'A')],
        )
        .expect("postgres search DDL never errors on a `rank` column");
        assert!(pg.contains("coalesce(\"rank\"::text, '')"), "pg: {pg}");
    }

    #[test]
    fn test_singularize_ves_plurals() {
        assert_eq!(singularize("wolves"), "wolf");
        assert_eq!(singularize("leaves"), "leaf");
        assert_eq!(singularize("shelves"), "shelf");
        assert_eq!(singularize("lives"), "life");
        assert_eq!(singularize("hives"), "hive");
        assert_eq!(singularize("knives"), "knife");
        assert_eq!(singularize("wives"), "wife");
    }

    #[test]
    fn test_add_search_up_sql_sanitizes_language() {
        let sql = add_search_up_sql_for(
            DatabaseBackend::Postgres,
            "posts",
            "english'; DROP TABLE posts;--",
            &[("title".to_string(), 'A')],
        )
        .expect("postgres search DDL never errors");
        assert!(sql.contains("to_tsvector('englishDROPTABLEposts'::regconfig"));

        let sql_qualified = add_search_up_sql_for(
            DatabaseBackend::Postgres,
            "posts",
            "pg_catalog.english",
            &[("title".to_string(), 'A')],
        )
        .expect("postgres search DDL never errors");
        assert!(sql_qualified.contains("to_tsvector('pg_catalog.english'::regconfig"));
    }

    #[test]
    fn test_parse_model_search_config_for_table_doc_comment_struct() {
        let content = r#"
#[autumn_web::model(table = "posts")]
#[doc = "This is a struct definition inside doc comment"]
pub struct Post {
    #[id]
    pub id: i64,
    #[searchable(weight = "A")]
    pub title: String,
}
"#;
        let (lang, fields) = parse_model_search_config_for_table(content, "posts").unwrap();
        assert_eq!(lang, "simple");
        assert_eq!(fields, vec![("title".to_string(), 'A')]);
    }

    #[test]
    fn test_parse_model_search_config_for_table_prior_model_language_scoping() {
        let content = r#"
#[autumn_web::model(table = "posts")]
#[searchable(language = "english")]
pub struct Post {
    #[id]
    pub id: i64,
    #[searchable]
    pub title: String,
}

#[autumn_web::model(table = "comments")]
pub struct Comment {
    #[id]
    pub id: i64,
    #[searchable]
    pub body: String,
}
"#;
        // Comments struct does not have struct-level #[searchable(language = "english")],
        // so it should fallback to "simple" and NOT inherit "english" from Post.
        let (lang, fields) = parse_model_search_config_for_table(content, "comments").unwrap();
        assert_eq!(lang, "simple");
        assert_eq!(fields, vec![("body".to_string(), 'D')]);
    }

    #[test]
    fn ensure_feature_comment_mentions_feature_still_adds_it() {
        // A comment on the dep line that *mentions* the feature name must not fool
        // the idempotency check into skipping the actual feature insertion.
        let cargo = "[package]\nname=\"x\"\n\n[dependencies]\nautumn-web = { version = \"0.6\" } # add \"inbound-mailgun\" later\n";
        let updated = ensure_autumn_web_feature(cargo, "inbound-mailgun");
        assert!(
            updated.contains("features"),
            "feature must actually be added: {updated}"
        );
        // The feature must appear in the dep value, not just in the comment.
        let dep_line = updated
            .lines()
            .find(|l| l.contains("autumn-web"))
            .unwrap_or("");
        let code_part = dep_line.split_once('#').map_or(dep_line, |(b, _)| b);
        assert!(
            code_part.contains("\"inbound-mailgun\""),
            "feature must be present in the code portion of the dep line: {dep_line}"
        );
    }

    // ── ensure_dev_dependency_test_support (issue #1023) ───────────────────

    #[test]
    fn dev_dependency_test_support_inserts_into_existing_section() {
        let cargo = "[package]\nname=\"x\"\n\n[dependencies]\nautumn-web = \"0.6\"\n\n[dev-dependencies]\ntokio = { version = \"1\", features = [\"rt\", \"macros\"] }\n";
        let updated = ensure_dev_dependency_test_support(cargo, "0.6");
        assert!(
            updated.contains("autumn-web = { version = \"0.6\", features = [\"test-support\"] }"),
            "must add a dev-dependency autumn-web entry with test-support: {updated}"
        );
        // The original tokio dev-dependency must survive untouched.
        assert!(updated.contains("tokio = { version = \"1\""));
        // The production dependency must be untouched (no test-support there).
        let deps_section = updated.split("[dev-dependencies]").next().unwrap();
        assert!(!deps_section.contains("test-support"));
    }

    #[test]
    fn dev_dependency_test_support_is_idempotent() {
        let cargo = "[package]\nname=\"x\"\n\n[dev-dependencies]\ntokio = \"1\"\n";
        let once = ensure_dev_dependency_test_support(cargo, "0.6");
        let twice = ensure_dev_dependency_test_support(&once, "0.6");
        assert_eq!(once, twice, "a second call must be a no-op");
    }

    #[test]
    fn dev_dependency_test_support_adds_feature_to_existing_dev_entry() {
        let cargo =
            "[package]\nname=\"x\"\n\n[dev-dependencies]\nautumn-web = { version = \"0.6\" }\n";
        let updated = ensure_dev_dependency_test_support(cargo, "0.6");
        assert!(
            updated.contains("\"test-support\""),
            "must add test-support to the existing dev-dependency entry: {updated}"
        );
        // Must not duplicate the autumn-web line.
        assert_eq!(
            updated.matches("autumn-web").count(),
            1,
            "must not duplicate the autumn-web dev-dependency: {updated}"
        );
    }

    #[test]
    fn dev_dependency_test_support_rewrites_single_quoted_plain_version() {
        // Regression test (Codex review, issue #1023): same gap as the
        // tokio case -- rewrite_dep_with_feature's Form 1 only recognized
        // double-quoted plain-string versions, so a valid single-quoted
        // existing `autumn-web = '0.6'` dev-dependency was treated as
        // absent and duplicated instead of getting test-support added.
        let cargo = "[package]\nname=\"x\"\n\n[dev-dependencies]\nautumn-web = '0.6'\n";
        let updated = ensure_dev_dependency_test_support(cargo, "0.6");
        assert_eq!(
            updated.matches("autumn-web").count(),
            1,
            "must rewrite the existing single-quoted autumn-web dep in place, not duplicate it: {updated}"
        );
        assert!(
            updated.contains("\"test-support\""),
            "must add test-support to the single-quoted dev entry: {updated}"
        );
    }

    #[test]
    fn dev_dependency_test_support_creates_section_when_absent() {
        let cargo = "[package]\nname=\"x\"\n\n[dependencies]\nautumn-web = \"0.6\"\n";
        let updated = ensure_dev_dependency_test_support(cargo, "0.6");
        assert!(updated.contains("[dev-dependencies]"));
        assert!(updated.contains("\"test-support\""));
    }

    #[test]
    fn dev_dependency_test_support_handles_dotted_key_form() {
        // Regression test (code review, issue #1023): `autumn-web.workspace = true`
        // must not be silently ignored -- that used to leave the Cargo.toml
        // unmodified with no error, so the generated smoke test failed to
        // compile with no hint why. Matches `patch_dotted_dep`'s established
        // shape (see ensure_feature_dotted_workspace_inserts_features_line):
        // a separate `autumn-web.features = [...]` dotted-key line, not a
        // rewrite of the `.workspace = true` line itself.
        let cargo = "[package]\nname=\"x\"\n\n[dev-dependencies]\nautumn-web.workspace = true\n";
        let updated = ensure_dev_dependency_test_support(cargo, "0.6");
        assert!(
            updated.contains("\"test-support\""),
            "must add test-support to a dotted-key autumn-web entry: {updated}"
        );
        assert!(
            updated.contains("autumn-web.features"),
            "must use the dotted key form: {updated}"
        );
        assert!(updated.contains("autumn-web.workspace = true"));
    }

    #[test]
    fn dev_dependency_test_support_handles_subtable_form() {
        // Regression test (code review, issue #1023): a `[dev-dependencies.autumn-web]`
        // subtable used to go unrecognized, causing a second, conflicting
        // `autumn-web = {...}` line to be inserted (a duplicate-key Cargo.toml).
        let cargo = "[package]\nname=\"x\"\n\n[dev-dependencies.autumn-web]\nversion = \"0.6\"\n";
        let updated = ensure_dev_dependency_test_support(cargo, "0.6");
        assert!(
            updated.contains("test-support"),
            "must add test-support to a subtable autumn-web entry: {updated}"
        );
        assert_eq!(
            updated.matches("autumn-web").count(),
            1,
            "must not insert a duplicate autumn-web entry: {updated}"
        );
    }

    #[test]
    fn dev_dependency_test_support_reuses_shared_feature_logic() {
        // The dev-dependencies path must go through the same
        // ensure_autumn_web_feature_status_in_section as [dependencies] does,
        // so it inherits every declaration shape that function already
        // understands instead of a smaller reimplementation.
        let (via_shared, found) = ensure_autumn_web_feature_status_in_section(
            "[package]\nname=\"x\"\n\n[dev-dependencies]\nautumn-web = \"0.6\"\n",
            "test-support",
            "dev-dependencies",
        );
        assert!(found);
        let via_public = ensure_dev_dependency_test_support(
            "[package]\nname=\"x\"\n\n[dev-dependencies]\nautumn-web = \"0.6\"\n",
            "0.6",
        );
        assert_eq!(via_shared, via_public);
    }

    #[test]
    fn dev_dependency_test_support_mirrors_workspace_source() {
        // Regression test (Codex review, issue #1023): when `[dependencies]`
        // inherits `autumn-web` from the workspace and there's no existing
        // `[dev-dependencies]` entry, inserting a crates.io `version = ...`
        // entry makes Cargo refuse to build at all -- confirmed via a
        // hand-built `cargo metadata` reproduction ("Dependency 'autumn-web'
        // has different source paths depending on the build target"). The
        // new dev-dependency entry must mirror `workspace = true` instead.
        let cargo = "[package]\nname=\"x\"\n\n[dependencies]\nautumn-web.workspace = true\n\n[dev-dependencies]\ntokio = { version = \"1\" }\n";
        let updated = ensure_dev_dependency_test_support(cargo, "0.6");
        assert!(
            updated.contains("autumn-web = { workspace = true, features = [\"test-support\"] }"),
            "must mirror the workspace source instead of defaulting to a crates.io version: {updated}"
        );
        assert!(!updated.contains("version = \"0.6\""));
    }

    #[test]
    fn dev_dependency_test_support_mirrors_path_source() {
        // Same failure mode as the workspace case, but for a direct `path`
        // source (the pattern this monorepo's own `examples/*` projects use).
        let cargo =
            "[package]\nname=\"x\"\n\n[dependencies]\nautumn-web = { path = \"../autumn\" }\n";
        let updated = ensure_dev_dependency_test_support(cargo, "0.6");
        assert!(
            updated
                .contains("autumn-web = { path = \"../autumn\", features = [\"test-support\"] }"),
            "must mirror the path source instead of defaulting to a crates.io version: {updated}"
        );
        assert!(!updated.contains("version = \"0.6\""));
    }

    #[test]
    fn dev_dependency_test_support_mirrors_subtable_path_source() {
        let cargo = "[package]\nname=\"x\"\n\n[dependencies.autumn-web]\npath = \"../autumn\"\n";
        let updated = ensure_dev_dependency_test_support(cargo, "0.6");
        assert!(
            updated
                .contains("autumn-web = { path = \"../autumn\", features = [\"test-support\"] }"),
            "must mirror a subtable-declared path source: {updated}"
        );
        assert!(!updated.contains("version = \"0.6\""));
    }

    #[test]
    fn dev_dependency_test_support_falls_back_to_version_for_plain_crates_io_dep() {
        // Baseline: a plain crates.io version in `[dependencies]` must still
        // produce the pre-existing `version = ...` fallback.
        let cargo = "[package]\nname=\"x\"\n\n[dependencies]\nautumn-web = \"0.6\"\n";
        let updated = ensure_dev_dependency_test_support(cargo, "0.6");
        assert!(
            updated.contains("autumn-web = { version = \"0.6\", features = [\"test-support\"] }"),
            "must fall back to an explicit version when [dependencies] has no source keys: {updated}"
        );
    }

    #[test]
    fn dev_dependency_test_support_mirrors_registry_with_version() {
        // Regression test (Codex review, issue #1023): `registry = "..."`
        // alone doesn't pin a resolvable dependency the way `workspace`/
        // `path`/`git` do -- Cargo still requires an explicit `version`
        // alongside it (confirmed via `cargo metadata --offline`: a dep with
        // neither path/git/version/workspace fails with "specified without
        // providing a local path, Git repository, version, or workspace
        // dependency to use"). Dropping `version` when mirroring `registry`
        // would produce the same failure.
        let cargo = "[package]\nname=\"x\"\n\n[dependencies]\nautumn-web = { version = \"0.6\", registry = \"private\" }\n";
        let updated = ensure_dev_dependency_test_support(cargo, "0.6");
        assert!(
            updated.contains(
                "autumn-web = { version = \"0.6\", registry = \"private\", features = [\"test-support\"] }"
            ),
            "must mirror both version and registry together: {updated}"
        );
    }

    #[test]
    fn dev_dependency_test_support_mirrors_dotted_registry_with_version() {
        // Regression test (Codex review, issue #1023): the dotted-key form
        // can spread `version` and `registry` across separate
        // `autumn-web.<key> = <value>` lines. The scan used to return as
        // soon as it saw the first source key (`registry`), dropping the
        // sibling `autumn-web.version` line entirely -- same underlying bug
        // as the inline-table case, just in the dotted-key branch.
        let cargo = "[package]\nname=\"x\"\n\n[dependencies]\nautumn-web.version = \"0.6\"\nautumn-web.registry = \"private\"\n";
        let updated = ensure_dev_dependency_test_support(cargo, "0.6");
        let dep_line = updated
            .lines()
            .find(|l| l.starts_with("autumn-web =") && l.contains("test-support"))
            .unwrap_or_else(|| panic!("no dev-dependency autumn-web line in: {updated}"));
        assert!(
            dep_line.contains("version = \"0.6\"") && dep_line.contains("registry = \"private\""),
            "must mirror both dotted version and registry together: {dep_line}"
        );
    }

    #[test]
    fn dev_dependency_test_support_mirrors_aliased_path_source() {
        // Regression test (Codex review, #1023): a renamed dep such as `autumn_web = {
        // package = "autumn-web", path = "../autumn" }` was not recognized at all — the
        // detector matched only the literal `autumn-web` key, so it fell back to a
        // mismatched crates.io version. `cargo metadata --offline` confirms Cargo unifies
        // dependency sources by package name, here "autumn-web", not by the local alias
        // key, so an unaliased `autumn-web = { path = "../autumn", ... }` dev-dependency,
        // mirroring the source rather than the alias, resolves to the identical node as
        // the aliased `[dependencies]` entry.
        let cargo = "[package]\nname=\"x\"\n\n[dependencies]\nautumn_web = { package = \"autumn-web\", path = \"../autumn\" }\n";
        let updated = ensure_dev_dependency_test_support(cargo, "0.6");
        assert!(
            updated
                .contains("autumn-web = { path = \"../autumn\", features = [\"test-support\"] }"),
            "must mirror the aliased dep's path source: {updated}"
        );
        assert!(!updated.contains("version = \"0.6\""));
    }

    #[test]
    fn dev_dependency_test_support_mirrors_aliased_path_source_odd_spacing() {
        // Regression test (Codex review, issue #1023): the alias detector
        // matched `package = "autumn-web"` or `package="autumn-web"` as
        // literal substrings, missing other TOML-legal spacings like
        // `package= "autumn-web"` (space after `=` only) -- confirmed valid
        // via `cargo metadata --offline --no-deps`. That silently dropped
        // the alias's path/workspace/git source and fell back to a
        // mismatched crates.io version.
        let cargo = "[package]\nname=\"x\"\n\n[dependencies]\nautumn_web = { package= \"autumn-web\", path = \"../autumn\" }\n";
        let updated = ensure_dev_dependency_test_support(cargo, "0.6");
        assert!(
            updated
                .contains("autumn-web = { path = \"../autumn\", features = [\"test-support\"] }"),
            "must mirror the aliased dep's path source despite odd spacing around package=: {updated}"
        );
        assert!(!updated.contains("version = \"0.6\""));
    }

    #[test]
    fn dev_dependency_test_support_mirrors_aliased_path_source_single_quoted() {
        // Regression test (Codex review, issue #1023): TOML's single-quoted
        // literal-string form (`package = 'autumn-web'`) is accepted by
        // Cargo identically to a double-quoted one (confirmed via `cargo
        // metadata --offline --no-deps`), but the alias detector only
        // stripped double quotes from the package value, so it missed this
        // form and fell back to a mismatched crates.io version.
        let cargo = "[package]\nname=\"x\"\n\n[dependencies]\nautumn_web = { package = 'autumn-web', path = '../autumn' }\n";
        let updated = ensure_dev_dependency_test_support(cargo, "0.6");
        assert!(
            updated.contains("autumn-web = { path = '../autumn', features = [\"test-support\"] }"),
            "must mirror the aliased dep's single-quoted path source: {updated}"
        );
        assert!(!updated.contains("version = \"0.6\""));
    }

    #[test]
    fn dev_dependency_test_support_mirrors_single_quoted_version() {
        // Regression test (Codex review, issue #1023): a plain single-quoted
        // version (`autumn-web = '0.5'`) is valid Cargo.toml (confirmed via
        // `cargo metadata --offline --no-deps`), but extract_plain_string_version
        // only recognized double-quoted strings, so it fell back to the
        // CLI's own version instead of mirroring the project's pin --
        // exactly the same failure mode the double-quoted version-mirroring
        // fix addressed.
        let cargo = "[package]\nname=\"x\"\n\n[dependencies]\nautumn-web = '0.5'\n";
        let updated = ensure_dev_dependency_test_support(cargo, "0.6");
        assert!(
            updated.contains("autumn-web = { version = '0.5', features = [\"test-support\"] }"),
            "must mirror the existing single-quoted pinned version, not the CLI's: {updated}"
        );
        assert!(!updated.contains("version = \"0.6\""));
    }

    #[test]
    fn dev_dependency_test_support_mirrors_existing_pinned_version_not_cli_version() {
        // Regression test (Codex review, #1023): the fallback used to insert `version =
        // "<CLI's own CARGO_PKG_VERSION>"` unconditionally, ignoring whatever
        // `[dependencies]` actually pins. When the two differ — a project pinned to an
        // older `autumn-web = "0.5"` while the CLI is `0.6` — Cargo's resolver can reject
        // the manifest outright if the requirements do not overlap; `cargo metadata`
        // reports "failed to select a version". The dev-dependency entry must mirror the
        // existing requirement, not the CLI's.
        let cargo = "[package]\nname=\"x\"\n\n[dependencies]\nautumn-web = \"0.5\"\n";
        let updated = ensure_dev_dependency_test_support(cargo, "0.6");
        assert!(
            updated.contains("autumn-web = { version = \"0.5\", features = [\"test-support\"] }"),
            "must mirror the existing pinned version, not the CLI's: {updated}"
        );
        assert!(!updated.contains("version = \"0.6\""));
    }

    #[test]
    fn dev_dependency_test_support_mirrors_version_from_inline_table() {
        let cargo = "[package]\nname=\"x\"\n\n[dependencies]\nautumn-web = { version = \"0.5\" }\n";
        let updated = ensure_dev_dependency_test_support(cargo, "0.6");
        assert!(
            updated.contains("autumn-web = { version = \"0.5\", features = [\"test-support\"] }"),
            "must mirror the existing pinned version from an inline table: {updated}"
        );
    }

    #[test]
    fn dev_dependency_test_support_mirrors_aliased_subtable_path_source() {
        // Regression test (Codex review, issue #1023): the renamed-dep fix
        // only covered the inline-table alias form (`autumn_web = {
        // package = "autumn-web", ... }`); the multiline subtable form
        // (`[dependencies.autumn_web]` with a `package = "autumn-web"`
        // body) went undetected the same way the unaliased subtable case
        // did before that fix, silently falling back to a crates.io
        // version that conflicts with the aliased dep's real path/git/
        // workspace source.
        let cargo = "[package]\nname=\"x\"\n\n[dependencies.autumn_web]\npackage = \"autumn-web\"\npath = \"../autumn\"\n";
        let updated = ensure_dev_dependency_test_support(cargo, "0.6");
        assert!(
            updated
                .contains("autumn-web = { path = \"../autumn\", features = [\"test-support\"] }"),
            "must mirror the aliased subtable's path source: {updated}"
        );
        assert!(!updated.contains("version = \"0.6\""));
    }

    #[test]
    fn dev_dependency_test_support_mirrors_dotted_aliased_path_source() {
        // Regression test (Codex review, #1023): the renamed-dep fixes covered the
        // inline-table (`autumn_web = { package = "autumn-web", ... }`) and subtable
        // (`[dependencies.autumn_web]`) alias shapes, but not Cargo's dotted
        // renamed-dependency form — `autumn_web.package = "autumn-web"` plus
        // `autumn_web.path = "../autumn"` on separate lines. The alias-detection branch
        // split on the key's dot and compared the whole `autumn_web.package` string
        // against `autumn_web`, so it never matched and fell through to a mismatched
        // crates.io version. `cargo metadata --offline` confirms two different paths for
        // the same package name conflict, as with the other alias forms.
        let cargo = "[package]\nname=\"x\"\n\n[dependencies]\nautumn_web.package = \"autumn-web\"\nautumn_web.path = \"../autumn\"\n";
        let updated = ensure_dev_dependency_test_support(cargo, "0.6");
        assert!(
            updated
                .contains("autumn-web = { path = \"../autumn\", features = [\"test-support\"] }"),
            "must mirror the dotted-aliased dep's path source: {updated}"
        );
        assert!(!updated.contains("version = \"0.6\""));
    }

    #[test]
    fn dev_dependency_test_support_mirrors_dotted_aliased_path_source_single_quoted() {
        // Regression test (Codex review, issue #1023): the dotted-alias
        // confirmation check (`sub == "package" && val_code.trim_matches...`)
        // still trimmed only double quotes even after the inline-table and
        // subtable single-quote fixes, so `autumn_web.package =
        // 'autumn-web'` (valid Cargo.toml, same as the double-quoted form)
        // never confirmed the alias and fell through to a mismatched
        // crates.io version.
        let cargo = "[package]\nname=\"x\"\n\n[dependencies]\nautumn_web.package = 'autumn-web'\nautumn_web.path = '../autumn'\n";
        let updated = ensure_dev_dependency_test_support(cargo, "0.6");
        assert!(
            updated.contains("autumn-web = { path = '../autumn', features = [\"test-support\"] }"),
            "must mirror the single-quoted dotted-aliased dep's path source: {updated}"
        );
        assert!(!updated.contains("version = \"0.6\""));
    }

    // ── ensure_dev_dependency_tokio_test_features (issue #1023) ────────────

    #[test]
    fn tokio_test_features_inserts_when_dev_dependencies_absent() {
        // Regression test (Codex review, issue #1023): a project not
        // created from `autumn new` (or one where the tokio dev-dependency
        // was removed) has no `tokio` entry to add `rt`/`macros` to, so the
        // generated `#[tokio::test]` smoke test fails to compile. `cargo
        // test --tests` still compiles `#[ignore]`d tests, so this broke an
        // otherwise-valid project's test build entirely.
        let cargo = "[package]\nname=\"x\"\n\n[dependencies]\nautumn-web = \"0.6\"\n";
        let updated = ensure_dev_dependency_tokio_test_features(cargo);
        assert!(
            updated.contains("tokio = { version = \"1\", features = [\"rt\", \"macros\"] }"),
            "must insert a tokio dev-dependency with rt and macros: {updated}"
        );
    }

    #[test]
    fn tokio_test_features_inserts_when_dev_dependencies_section_exists_without_tokio() {
        let cargo =
            "[package]\nname=\"x\"\n\n[dev-dependencies]\nautumn-web = { version = \"0.6\" }\n";
        let updated = ensure_dev_dependency_tokio_test_features(cargo);
        assert!(
            updated.contains("tokio = { version = \"1\", features = [\"rt\", \"macros\"] }"),
            "must insert tokio into the existing dev-dependencies section: {updated}"
        );
        assert!(updated.contains("autumn-web"));
    }

    #[test]
    fn tokio_test_features_adds_missing_features_to_existing_tokio() {
        let cargo = "[package]\nname=\"x\"\n\n[dev-dependencies]\ntokio = { version = \"1\" }\n";
        let updated = ensure_dev_dependency_tokio_test_features(cargo);
        let dep_line = updated
            .lines()
            .find(|l| l.trim_start().starts_with("tokio"))
            .unwrap_or_else(|| panic!("no tokio line in: {updated}"));
        assert!(
            dep_line.contains("\"rt\"") && dep_line.contains("\"macros\""),
            "must add both missing features to the existing tokio entry: {dep_line}"
        );
        assert_eq!(
            updated.matches("tokio").count(),
            1,
            "must not duplicate the tokio dev-dependency: {updated}"
        );
    }

    #[test]
    fn tokio_test_features_adds_only_the_missing_feature() {
        let cargo = "[package]\nname=\"x\"\n\n[dev-dependencies]\ntokio = { version = \"1\", features = [\"macros\"] }\n";
        let updated = ensure_dev_dependency_tokio_test_features(cargo);
        let dep_line = updated
            .lines()
            .find(|l| l.trim_start().starts_with("tokio"))
            .unwrap_or_else(|| panic!("no tokio line in: {updated}"));
        assert!(dep_line.contains("\"rt\"") && dep_line.contains("\"macros\""));
        assert_eq!(
            dep_line.matches("\"macros\"").count(),
            1,
            "must not duplicate an already-present feature: {dep_line}"
        );
    }

    #[test]
    fn tokio_test_features_is_idempotent() {
        let cargo = "[package]\nname=\"x\"\n\n[dev-dependencies]\ntokio = { version = \"1\", features = [\"rt\", \"macros\"] }\n";
        let once = ensure_dev_dependency_tokio_test_features(cargo);
        let twice = ensure_dev_dependency_tokio_test_features(&once);
        assert_eq!(once, twice, "a second call must be a no-op");
    }

    #[test]
    fn tokio_test_features_handles_dotted_key_form() {
        let cargo = "[package]\nname=\"x\"\n\n[dev-dependencies]\ntokio.version = \"1\"\n";
        let updated = ensure_dev_dependency_tokio_test_features(cargo);
        let features_line = updated
            .lines()
            .find(|l| l.trim_start().starts_with("tokio.features"))
            .unwrap_or_else(|| panic!("no tokio.features line in: {updated}"));
        assert!(features_line.contains("\"rt\"") && features_line.contains("\"macros\""));
    }

    #[test]
    fn tokio_test_features_dotted_form_not_shadowed_by_prefixed_dep() {
        // Regression test (Codex review, issue #1023): patch_dotted_dep's
        // idempotency check used a bare `starts_with(dep_name)`, so a later
        // unrelated dependency sharing the prefix (e.g. `tokio-util`, a real
        // crate that can plausibly coexist with `tokio` in the same
        // project) whose value happened to contain `"rt"` was mistaken for
        // proof that the real `tokio` dotted dep already had the feature --
        // skipping the actual `tokio.features` splice and leaving the
        // generated `#[tokio::test]` smoke test unable to compile.
        let cargo = "[package]\nname=\"x\"\n\n[dev-dependencies]\ntokio.version = \"1\"\ntokio-util = { features = [\"rt\"] }\n";
        let updated = ensure_dev_dependency_tokio_test_features(cargo);
        let features_line = updated
            .lines()
            .find(|l| l.trim_start().starts_with("tokio.features"))
            .unwrap_or_else(|| panic!("no tokio.features line in: {updated}"));
        assert!(
            features_line.contains("\"rt\"") && features_line.contains("\"macros\""),
            "the real tokio dep must get both features despite the tokio-util decoy: {features_line}"
        );
    }

    #[test]
    fn tokio_test_features_not_spliced_into_trailing_comment() {
        // Regression test (Codex review, issue #1023): rewrite_dep_with_feature
        // searched for "features"/"[...]" in the raw line, including any
        // trailing `# comment`. A comment that happens to contain
        // TOML-looking text (e.g. a `# features = []` example) got the
        // feature spliced into the comment instead of the real dependency
        // value, while the caller still reported success -- leaving the
        // generated `#[tokio::test]` smoke test unable to compile because
        // the actual tokio entry never gained rt/macros.
        let cargo = "[package]\nname=\"x\"\n\n[dev-dependencies]\ntokio = { version = \"1\" } # features = []\n";
        let updated = ensure_dev_dependency_tokio_test_features(cargo);
        let tokio_line = updated
            .lines()
            .find(|l| l.trim_start().starts_with("tokio"))
            .unwrap_or_else(|| panic!("no tokio line in: {updated}"));
        let code = tokio_line.split_once('#').map_or(tokio_line, |(c, _)| c);
        assert!(
            code.contains("\"rt\"") && code.contains("\"macros\""),
            "features must be added to the real dependency value, not the trailing comment: {tokio_line}"
        );
    }

    #[test]
    fn tokio_test_features_rewrites_single_quoted_plain_version() {
        // Regression test (Codex review, issue #1023): rewrite_dep_with_feature's
        // Form 1 only recognized a double-quoted plain-string version
        // (`tokio = "1"`). A valid single-quoted one (`tokio = '1'`,
        // confirmed accepted by `cargo metadata --offline --no-deps`) fell
        // through unrecognized, so the caller treated the dependency as
        // absent and inserted a *second*, duplicate `tokio` key -- which
        // Cargo rejects outright, making the manifest unusable.
        let cargo = "[package]\nname=\"x\"\n\n[dev-dependencies]\ntokio = '1'\n";
        let updated = ensure_dev_dependency_tokio_test_features(cargo);
        assert_eq!(
            updated.matches("tokio").count(),
            1,
            "must rewrite the existing single-quoted tokio dep in place, not duplicate it: {updated}"
        );
        let tokio_line = updated
            .lines()
            .find(|l| l.trim_start().starts_with("tokio"))
            .unwrap_or_else(|| panic!("no tokio line in: {updated}"));
        assert!(
            tokio_line.contains("\"rt\"") && tokio_line.contains("\"macros\""),
            "must add both features to the single-quoted tokio dep: {tokio_line}"
        );
    }

    #[test]
    fn tokio_test_features_handles_subtable_form() {
        let cargo = "[package]\nname=\"x\"\n\n[dev-dependencies.tokio]\nversion = \"1\"\n";
        let updated = ensure_dev_dependency_tokio_test_features(cargo);
        assert!(
            updated.contains("\"rt\"") && updated.contains("\"macros\""),
            "must add both features to a subtable tokio entry: {updated}"
        );
        assert_eq!(
            updated.matches("tokio").count(),
            1,
            "must not insert a duplicate tokio entry: {updated}"
        );
    }

    #[test]
    fn tokio_test_features_mirrors_path_source() {
        // Regression test (Codex review, #1023): the source-mismatch bug behind the whole
        // autumn-web source-mirroring saga also applies to tokio. If `[dependencies]`
        // sources tokio from a path, workspace, or git override — an internal fork —
        // inserting a crates.io `version = "1"` dev entry makes Cargo reject the manifest
        // with "Dependency 'tokio' has different source paths depending on the build
        // target", confirmed via a hand-built `cargo metadata --offline` reproduction. The
        // new entry must mirror the existing path source instead.
        let cargo =
            "[package]\nname=\"x\"\n\n[dependencies]\ntokio = { path = \"../fake-tokio\" }\n";
        let updated = ensure_dev_dependency_tokio_test_features(cargo);
        let tokio_dev_line = updated
            .lines()
            .find(|l| l.trim_start().starts_with("tokio") && l.contains("rt"))
            .unwrap_or_else(|| panic!("no tokio dev-dependency line in: {updated}"));
        assert!(
            tokio_dev_line.contains("path = \"../fake-tokio\""),
            "must mirror the path source instead of defaulting to a crates.io version: {tokio_dev_line}"
        );
        assert!(!tokio_dev_line.contains("version"));
    }

    #[test]
    fn tokio_test_features_falls_back_to_version_for_plain_crates_io_dep() {
        let cargo = "[package]\nname=\"x\"\n\n[dependencies]\ntokio = \"1\"\n";
        let updated = ensure_dev_dependency_tokio_test_features(cargo);
        let tokio_dev_line = updated
            .lines()
            .find(|l| l.trim_start().starts_with("tokio") && l.contains("rt"))
            .unwrap_or_else(|| panic!("no tokio dev-dependency line in: {updated}"));
        assert!(
            tokio_dev_line.contains("version = \"1\""),
            "must fall back to an explicit version when [dependencies] has no source keys: {tokio_dev_line}"
        );
    }

    // ── IdType-aware variants (issue #1400) ────────────────────────────────

    #[test]
    fn schema_table_block_bigserial_is_byte_equal_to_old_default() {
        // AC4: BigSerial must produce exactly the old hardcoded schema block —
        // regression lock against accidental id-type drift.
        let fs = fields(&["title:String"]);
        let block = schema_table_block_with_id("posts", &fs, IdType::BigSerial);
        let expected = "diesel::table! {\n\
            \x20   posts (id) {\n\
            \x20       id -> Int8,\n\
            \x20       title -> Text,\n\
            \x20       created_at -> Timestamp,\n\
            \x20   }\n\
            }\n";
        assert_eq!(
            block, expected,
            "BigSerial schema block must match the old default byte-for-byte"
        );
    }

    #[test]
    fn schema_table_block_uuid_emits_uuid_type() {
        let fs = fields(&["title:String"]);
        let block = schema_table_block_with_id("posts", &fs, IdType::Uuid);
        assert!(
            block.contains("id -> Uuid,"),
            "Uuid id_type must emit 'id -> Uuid,'"
        );
        assert!(!block.contains("Int8"), "Uuid block must not contain Int8");
    }

    #[test]
    fn create_table_sql_bigserial_is_byte_equal_to_old_default() {
        // AC4 regression lock for the migration: BigSerial must emit BIGSERIAL
        // with no UUID comment prepended.
        let fs = fields(&["title:String"]);
        let sql = create_table_sql_with_metadata_and_id(
            "posts",
            &fs,
            &BTreeSet::new(),
            &BTreeMap::new(),
            IdType::BigSerial,
        );
        assert!(
            sql.starts_with("CREATE TABLE posts ("),
            "BigSerial migration must not prepend any comment: {sql}"
        );
        assert!(
            sql.contains("id BIGSERIAL PRIMARY KEY"),
            "BigSerial must emit BIGSERIAL"
        );
        assert!(
            !sql.contains("UUID"),
            "BigSerial migration must not mention UUID"
        );
    }

    #[test]
    fn create_table_sql_uuid_emits_uuid_pk() {
        let fs = fields(&["title:String"]);
        let sql = create_table_sql_with_metadata_and_id(
            "posts",
            &fs,
            &BTreeSet::new(),
            &BTreeMap::new(),
            IdType::Uuid,
        );
        assert!(
            sql.contains("id UUID PRIMARY KEY DEFAULT gen_random_uuid()"),
            "Uuid must emit UUID PK with default"
        );
        assert!(
            !sql.contains("BIGSERIAL"),
            "Uuid migration must not contain BIGSERIAL"
        );
    }

    #[test]
    fn create_table_sql_uuid_prepends_comment_with_uuidv7_path() {
        let sql = create_table_sql_with_metadata_and_id(
            "posts",
            &[],
            &BTreeSet::new(),
            &BTreeMap::new(),
            IdType::Uuid,
        );
        assert!(
            sql.contains("UUIDv7"),
            "Uuid migration must document the UUIDv7 upgrade path"
        );
    }

    #[test]
    fn append_schema_table_with_id_bigserial_byte_equal_wrapper() {
        let fs = fields(&["title:String"]);
        let via_wrapper = append_schema_table("", "posts", &fs);
        let via_explicit = append_schema_table_with_id("", "posts", &fs, IdType::BigSerial);
        assert_eq!(via_wrapper, via_explicit);
    }

    #[test]
    fn append_schema_table_with_id_uuid_contains_uuid_type() {
        let fs = fields(&["title:String"]);
        let schema = append_schema_table_with_id("", "posts", &fs, IdType::Uuid);
        assert!(schema.contains("id -> Uuid,"));
    }

    // ── `autumn destroy` inverse helpers (issue #1048) ─────────────────────
    //
    // Each inverse is tested for byte-identical round-tripping:
    // `inverse(forward(base)) == base`.

    #[test]
    fn remove_mod_declaration_restores_empty_file() {
        let after_add = add_mod_declaration("", "post");
        assert_eq!(remove_mod_declaration(&after_add, "post"), "");
    }

    #[test]
    fn remove_mod_declaration_restores_original_with_other_mods() {
        let base = "pub mod user;\n";
        let after_add = add_mod_declaration(base, "post");
        assert_eq!(remove_mod_declaration(&after_add, "post"), base);
    }

    #[test]
    fn remove_mod_declaration_is_idempotent_when_absent() {
        let base = "pub mod user;\n";
        assert_eq!(remove_mod_declaration(base, "post"), base);
    }

    #[test]
    fn remove_mod_declaration_leaves_private_mod_untouched() {
        // `add_mod_declaration` treats a bare `mod post;` as already-present and
        // never writes `pub mod post;` in that case — so destroy must not
        // remove a private `mod post;` it didn't add.
        let base = "mod post;\n";
        assert_eq!(remove_mod_declaration(base, "post"), base);
    }

    #[test]
    fn remove_schema_table_restores_empty_file() {
        let f = fields(&["title:String"]);
        let block = append_schema_table("", "posts", &f);
        let after_add = block.clone();
        assert_eq!(remove_schema_table(&after_add, "posts", &block), "");
    }

    #[test]
    fn remove_schema_table_restores_original_with_other_tables() {
        let f1 = fields(&["title:String"]);
        let f2 = fields(&["name:String"]);
        let base = append_schema_table("", "users", &f2);
        let block = append_schema_table("", "posts", &f1);
        let after_add = append_schema_table(&base, "posts", &f1);
        assert_eq!(remove_schema_table(&after_add, "posts", &block), base);
    }

    #[test]
    fn remove_schema_table_is_idempotent_when_absent() {
        let f = fields(&["name:String"]);
        let base = append_schema_table("", "users", &f);
        let block = append_schema_table("", "posts", &f);
        assert_eq!(remove_schema_table(&base, "posts", &block), base);
    }

    #[test]
    fn remove_schema_table_never_removes_a_pre_existing_table_with_different_columns() {
        // A hand-rolled `posts` table with different columns than this
        // generator invocation would produce must survive `destroy` — it
        // wasn't generate's own output, even though the name matches
        // (issue #1048 PR review).
        let hand_written = "diesel::table! {\n    posts (id) {\n        id -> BigInt,\n        \
                             body -> Text,\n    }\n}\n";
        let generated_block = append_schema_table("", "posts", &fields(&["title:String"]));
        assert_eq!(
            remove_schema_table(hand_written, "posts", &generated_block),
            hand_written,
            "pre-existing table with different columns must not be removed"
        );
    }

    #[test]
    fn remove_routes_entries_restores_single_line_template_body() {
        // Mirrors the `autumn new` template's `.routes(routes![index, hello, hello_name])`.
        let base = "fn main() {\n    App::new()\n        .routes(routes![index, hello, hello_name])\n        .run()\n}\n";
        let appended = vec![
            "routes::posts::index".to_owned(),
            "routes::posts::show".to_owned(),
        ];
        let after_add = ensure_routes_entries(base, &appended);
        assert_ne!(
            after_add, base,
            "test setup: append must actually change the body"
        );
        let reverted = remove_routes_entries(&after_add, &appended);
        assert_eq!(reverted, base);
    }

    #[test]
    fn remove_routes_entries_is_idempotent_when_absent() {
        let base =
            "fn main() {\n    App::new()\n        .routes(routes![index])\n        .run()\n}\n";
        let appended = vec!["routes::posts::index".to_owned()];
        assert_eq!(remove_routes_entries(base, &appended), base);
    }

    #[test]
    fn remove_routes_entries_removes_present_entries_even_when_one_is_already_gone() {
        // issue #1048 PR review: a user may have hand-removed one of a
        // resource's own route entries before running destroy. The rest of
        // that resource's routes are still present and about to be
        // orphaned (the underlying handler file is deleted regardless) —
        // abandoning the whole cleanup because ONE entry is already absent
        // would leave `main.rs` referencing a missing function/module.
        let appended = vec![
            "routes::posts::index".to_owned(),
            "routes::posts::show".to_owned(),
        ];
        // Simulate the user having already removed `show` by hand.
        let hand_edited = "fn main() {\n    App::new()\n        .routes(routes![index, routes::posts::index])\n        .run()\n}\n";
        let reverted = remove_routes_entries(hand_edited, &appended);
        assert_eq!(
            reverted,
            "fn main() {\n    App::new()\n        .routes(routes![index])\n        .run()\n}\n",
            "the still-present `index` entry must be removed even though `show` was \
             already gone: {reverted}"
        );
    }

    #[test]
    fn remove_routes_entries_preserves_other_resources_multiline() {
        let base =
            "fn main() {\n    App::new()\n        .routes(routes![index])\n        .run()\n}\n";
        let comments_entries = vec![
            "routes::comments::index".to_owned(),
            "routes::comments::show".to_owned(),
        ];
        let after_comments = ensure_routes_entries(base, &comments_entries);
        let posts_entries = vec![
            "routes::posts::index".to_owned(),
            "routes::posts::show".to_owned(),
        ];
        let after_posts = ensure_routes_entries(&after_comments, &posts_entries);
        let reverted = remove_routes_entries(&after_posts, &posts_entries);
        // Comments entries must survive destroying posts.
        assert!(reverted.contains("routes::comments::index"));
        assert!(reverted.contains("routes::comments::show"));
        assert!(!reverted.contains("routes::posts::index"));
        assert!(!reverted.contains("routes::posts::show"));
    }

    #[test]
    fn remove_routes_entries_not_confused_by_kept_entry_sharing_a_prefix() {
        // `routes::posts::index` is a textual PREFIX of the kept
        // `routes::posts::index_all` entry that appears earlier in the body.
        // A raw substring search for the removed entry would match inside
        // the kept one and misdetect the original single-line layout as
        // multi-line.
        let base = "fn main() {\n    App::new()\n        .routes(routes![index, routes::posts::index_all, routes::posts::index])\n        .run()\n}\n";
        let removed = vec!["routes::posts::index".to_owned()];
        let reverted = remove_routes_entries(base, &removed);
        assert_eq!(
            reverted,
            "fn main() {\n    App::new()\n        .routes(routes![index, routes::posts::index_all])\n        .run()\n}\n",
            "must restore the original single-line layout byte-identically, not collapse/reformat it"
        );
    }

    #[test]
    fn remove_autumn_web_feature_collapses_to_bare_string() {
        let base = "[dependencies]\nautumn-web = \"0.6.0\"\n";
        let after_add = ensure_autumn_web_feature(base, "maud");
        assert_ne!(after_add, base);
        assert_eq!(remove_autumn_web_feature(&after_add, "maud"), base);
    }

    #[test]
    fn remove_autumn_web_feature_keeps_other_features() {
        let base = "[dependencies]\nautumn-web = \"0.6.0\"\n";
        let with_maud = ensure_autumn_web_feature(base, "maud");
        let with_both = ensure_autumn_web_feature(&with_maud, "htmx");
        let reverted = remove_autumn_web_feature(&with_both, "htmx");
        assert_eq!(reverted, with_maud);
    }

    #[test]
    fn remove_autumn_web_feature_preserves_keys_after_the_features_array() {
        // A hand-edited (or otherwise pre-existing) dependency line can carry
        // keys after `features = [...]`, e.g. `default-features = false`.
        // Removing one feature while others remain must not silently drop
        // those trailing keys.
        let base = "[dependencies]\nautumn-web = { version = \"0.6.0\", features = [\"maud\", \"htmx\"], default-features = false }\n";
        let reverted = remove_autumn_web_feature(base, "htmx");
        assert_eq!(
            reverted,
            "[dependencies]\nautumn-web = { version = \"0.6.0\", features = [\"maud\"], default-features = false }\n",
            "trailing keys after the features array must survive: {reverted}"
        );
    }

    #[test]
    fn remove_autumn_web_feature_handles_default_features_key_before_features() {
        // issue #1048 PR review: a plain `body.find("features")` matches
        // inside `default-features` when that key comes first, truncating
        // `before_features` mid-word and corrupting the rewritten line.
        let base = "[dependencies]\nautumn-web = { version = \"0.6.0\", default-features = false, features = [\"maud\", \"htmx\"] }\n";
        let reverted = remove_autumn_web_feature(base, "htmx");
        assert_eq!(
            reverted,
            "[dependencies]\nautumn-web = { version = \"0.6.0\", default-features = false, features = [\"maud\"] }\n",
            "must remove only the target feature, keeping default-features intact: {reverted}"
        );
    }

    #[test]
    fn remove_autumn_web_feature_is_idempotent_when_absent() {
        let base = "[dependencies]\nautumn-web = \"0.6.0\"\n";
        assert_eq!(remove_autumn_web_feature(base, "maud"), base);
    }

    #[test]
    fn remove_autumn_web_feature_reverts_dotted_key_form() {
        // issue #1048 PR review: `ensure_autumn_web_feature` can add a
        // feature to a pre-existing dotted-key declaration; destroy must be
        // able to remove it again, not leave it behind forever.
        let base =
            "[dependencies]\nautumn-web.version = \"0.6.0\"\nautumn-web.features = [\"db\"]\n";
        let with_mail = ensure_autumn_web_feature(base, "mail");
        assert_ne!(with_mail, base);
        assert_eq!(remove_autumn_web_feature(&with_mail, "mail"), base);
    }

    #[test]
    fn remove_autumn_web_feature_deletes_dotted_features_key_when_emptied() {
        let base =
            "[dependencies]\nautumn-web.version = \"0.6.0\"\nautumn-web.features = [\"mail\"]\n";
        let reverted = remove_autumn_web_feature(base, "mail");
        assert_eq!(
            reverted, "[dependencies]\nautumn-web.version = \"0.6.0\"\n",
            "an emptied dotted features key must be removed outright: {reverted}"
        );
    }

    #[test]
    fn remove_autumn_web_feature_reverts_multiline_inline_table_form() {
        let base = "[dependencies]\nautumn-web = {\n    version = \"0.6.0\",\n    features = [\"db\"],\n}\n";
        let with_mail = ensure_autumn_web_feature(base, "mail");
        assert_ne!(with_mail, base);
        assert!(with_mail.contains("\"mail\""));
        let reverted = remove_autumn_web_feature(&with_mail, "mail");
        assert_eq!(
            reverted, base,
            "must remove only the added feature from the features line, restoring the \
             original multiline table: {reverted}"
        );
    }

    #[test]
    fn remove_autumn_web_feature_reverts_subtable_form() {
        // issue #1048 PR review: `[dependencies.autumn-web]` with a
        // separate `features` key is a shape `ensure_autumn_web_feature`
        // supports adding to, but the old remover only handled single-line
        // inline tables.
        let base = "[dependencies.autumn-web]\nversion = \"0.6.0\"\nfeatures = [\"db\"]\n\n[dev-dependencies]\n";
        let with_mail = ensure_autumn_web_feature(base, "mail");
        assert_ne!(with_mail, base);
        assert_eq!(remove_autumn_web_feature(&with_mail, "mail"), base);
    }

    #[test]
    fn remove_autumn_web_feature_deletes_subtable_features_key_when_emptied() {
        let base = "[dependencies.autumn-web]\nversion = \"0.6.0\"\nfeatures = [\"mail\"]\n";
        let reverted = remove_autumn_web_feature(base, "mail");
        assert_eq!(
            reverted, "[dependencies.autumn-web]\nversion = \"0.6.0\"\n",
            "an emptied subtable features key must be removed outright, leaving \
             `version` and the header untouched: {reverted}"
        );
    }

    #[test]
    fn remove_autumn_web_feature_reverts_renamed_inline_alias() {
        // issue #1048 PR review: `ensure_autumn_web_feature_status_in_section`
        // adds features to an importable rename like
        // `autumn_web = { package = "autumn-web", ... }` too, but the old
        // remover always searched for the literal `"autumn-web"` key and
        // silently left the feature behind on this shape.
        let base =
            "[dependencies]\nautumn_web = { package = \"autumn-web\", version = \"0.6.0\" }\n";
        let with_mail = ensure_autumn_web_feature(base, "mail");
        assert_ne!(with_mail, base);
        assert!(with_mail.contains("autumn_web"));
        assert_eq!(remove_autumn_web_feature(&with_mail, "mail"), base);
    }

    #[test]
    fn remove_autumn_web_feature_reverts_renamed_subtable_alias() {
        let base = "[dependencies.autumn_web]\npackage = \"autumn-web\"\nversion = \"0.6.0\"\nfeatures = [\"db\"]\n\n[dev-dependencies]\n";
        let with_mail = ensure_autumn_web_feature(base, "mail");
        assert_ne!(with_mail, base);
        assert_eq!(remove_autumn_web_feature(&with_mail, "mail"), base);
    }

    #[test]
    fn remove_autumn_web_dev_dependency_feature_deletes_freshly_inserted_line() {
        let base =
            "[dev-dependencies]\ntokio = { version = \"1\", features = [\"rt\", \"macros\"] }\n";
        let after_add = ensure_dev_dependency_test_support(base, "0.6.0");
        assert_ne!(after_add, base);
        assert_eq!(
            remove_autumn_web_dev_dependency_feature(&after_add, "test-support"),
            base
        );
    }

    #[test]
    fn remove_autumn_web_dev_dependency_feature_is_idempotent_when_absent() {
        let base = "[dev-dependencies]\ntokio = { version = \"1\" }\n";
        assert_eq!(
            remove_autumn_web_dev_dependency_feature(base, "test-support"),
            base
        );
    }

    #[test]
    fn remove_main_mod_declarations_restores_original_with_no_leading_attributes() {
        // Mirrors `autumn new`'s template main.rs, which has no leading
        // `//!`/`#![` lines at all.
        let base = "use autumn_web::prelude::*;\n\nfn main() {}\n";
        let after_add = ensure_mods(base, &["models", "repositories", "routes", "schema"]);
        assert_ne!(after_add, base);
        let reverted = remove_main_mod_declarations(
            &after_add,
            &["models", "repositories", "routes", "schema"],
        );
        assert_eq!(reverted, base);
    }

    #[test]
    fn remove_main_mod_declarations_leaves_other_shared_mods_when_only_some_missing() {
        let base = "fn main() {}\n";
        let after_add = ensure_mods(base, &["models", "jobs"]);
        let reverted = remove_main_mod_declarations(&after_add, &["jobs"]);
        assert!(reverted.contains("mod models;"));
        assert!(!reverted.contains("mod jobs;"));
    }

    #[test]
    fn remove_main_mod_declarations_is_idempotent_when_absent() {
        let base = "fn main() {}\n";
        assert_eq!(remove_main_mod_declarations(base, &["models"]), base);
    }
}
