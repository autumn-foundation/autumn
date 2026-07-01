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

use super::dsl::{Field, IdType};

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

/// Build a new `diesel::table!` block for the given table, emitting the `id`
/// column with the caller-supplied `id_type`.
#[must_use]
pub fn schema_table_block_with_id(table: &str, fields: &[Field], id_type: IdType) -> String {
    let mut out = String::with_capacity(fields.len() * 40 + 128);
    out.push_str("diesel::table! {\n");
    let _ = writeln!(out, "    {table} (id) {{");
    let _ = writeln!(out, "        id -> {},", id_type.schema_type());
    for f in fields {
        let _ = writeln!(out, "        {} -> {},", f.name, f.schema_type());
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
    if has_table(existing, table) {
        return existing.to_owned();
    }
    let block = schema_table_block_with_id(table, fields, id_type);
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

/// Public predicate: whether `schema` already declares a `<table>` block.
///
/// Generators use this to detect a collision before emitting a migration that
/// would otherwise `CREATE TABLE` a name the project already owns.
#[must_use]
pub fn schema_has_table(schema: &str, table: &str) -> bool {
    has_table(schema, table)
}

/// Build the full SQL for `up.sql` of a `CREATE TABLE` migration with optional
/// defaults and non-unique indexes, honouring the caller-supplied `id_type`.
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
    let mut sql = String::with_capacity(fields.len() * 64 + indexes.len() * 96 + 256);
    if let Some(comment) = id_type.migration_comment() {
        sql.push_str(comment);
        sql.push('\n');
    }
    let _ = writeln!(sql, "CREATE TABLE {table} (");
    let _ = write!(sql, "    id {}", id_type.pk_sql());
    for f in fields {
        sql.push_str(",\n");
        let _ = write!(
            sql,
            "    {} {} {}",
            f.name,
            f.sql_type(),
            f.sql_nullability()
        );
        if let Some(default) = defaults.get(&f.name) {
            let _ = write!(sql, " DEFAULT {default}");
        }
    }
    sql.push_str(",\n    created_at TIMESTAMP NOT NULL DEFAULT NOW()\n);\n");
    for field_name in indexes {
        let _ = writeln!(
            sql,
            "CREATE INDEX idx_{table}_{field_name} ON {table} ({field_name});"
        );
    }
    sql
}

/// `down.sql` companion to [`create_table_sql_with_metadata_and_id`].
#[must_use]
pub fn drop_table_sql(table: &str) -> String {
    format!("DROP TABLE {table};\n")
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

/// SQL for adding columns to a table.
///
/// Prepends an `autumn-safety` comment for `NOT NULL` columns that have no
/// `DEFAULT` — those require a backfill or a default before the constraint can
/// be added safely on a live table.
#[must_use]
pub fn add_columns_up_sql(table: &str, fields: &[Field]) -> String {
    let mut out = String::new();
    for f in fields {
        if !f.nullable {
            let _ = writeln!(
                out,
                "-- autumn-safety: potentially-blocking \
                 -- add a DEFAULT or backfill existing rows before enforcing NOT NULL"
            );
        }
        let _ = writeln!(
            out,
            "ALTER TABLE {table} ADD COLUMN {} {} {};",
            f.name,
            f.sql_type(),
            f.sql_nullability()
        );
    }
    out
}

/// `down.sql` companion to [`add_columns_up_sql`].
#[must_use]
pub fn add_columns_down_sql(table: &str, fields: &[Field]) -> String {
    let mut out = String::new();
    for f in fields.iter().rev() {
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

/// SQL for removing columns from a table.
///
/// Prepends an `autumn-safety` comment for each `DROP COLUMN` to make the
/// rolling-deploy risk visible at a glance and machine-parseable by
/// `autumn migrate check`.
#[must_use]
pub fn remove_columns_up_sql(table: &str, fields: &[Field]) -> String {
    let mut out = String::new();
    for f in fields {
        let _ = writeln!(
            out,
            "-- autumn-safety: destructive \
             -- old replicas that reference this column will fail until restarted; \
             use expand/contract"
        );
        let _ = writeln!(out, "ALTER TABLE {table} DROP COLUMN {};", f.name);
    }
    out
}

/// `down.sql` companion to [`remove_columns_up_sql`].
#[must_use]
pub fn remove_columns_down_sql(table: &str, fields: &[Field]) -> String {
    let mut out = String::new();
    for f in fields.iter().rev() {
        let _ = writeln!(
            out,
            "ALTER TABLE {table} ADD COLUMN {} {} {};",
            f.name,
            f.sql_type(),
            f.sql_nullability()
        );
    }
    out
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

/// Insert each entry into the body of the *first* `routes![ ... ]` macro
/// invocation. Skips entries already present.
fn ensure_routes_entries(existing: &str, entries: &[String]) -> String {
    let Some(start) = existing.find("routes![") else {
        return existing.to_owned();
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
        // Unmatched bracket — leave the file untouched.
        return existing.to_owned();
    }
    let body = &existing[body_start..i];
    let new_body = augment_routes_body(body, entries);
    let mut out = String::with_capacity(existing.len() + new_body.len());
    out.push_str(&existing[..body_start]);
    out.push_str(&new_body);
    out.push_str(&existing[i..]);
    out
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

/// Insert `.mail_previews(mail_previews![mailer_type])` before `.run()`.
fn insert_mail_previews_call(existing: &str, mailer_type: &str) -> String {
    insert_before_run_call(
        existing,
        &format!(".mail_previews(mail_previews![{mailer_type}])"),
    )
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
/// silently misses forms like `package= "..."` or `package ="..."`.
fn declares_package(text: &str, target: &str) -> bool {
    let body = match (text.find('{'), text.rfind('}')) {
        (Some(open), Some(close)) if close > open => &text[open + 1..close],
        _ => text,
    };
    split_top_level_commas(body).into_iter().any(|part| {
        part.split_once('=')
            .is_some_and(|(k, v)| k.trim() == "package" && v.trim().trim_matches('"') == target)
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
/// explicit `version = "..."` key is.
fn extract_plain_string_version(line: &str, dep_name: &str) -> Option<String> {
    let rest = line.trim().strip_prefix(dep_name)?.trim_start();
    let rest = rest.strip_prefix('=')?.trim_start();
    let rest = rest.split('#').next().unwrap_or(rest).trim();
    (rest.starts_with('"') && rest.ends_with('"') && rest.len() >= 2)
        .then(|| format!("version = {rest}"))
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
        // autumn_web.path = "../autumn" / etc.
        if sub == "package" && val_code.trim_matches('"') == "autumn-web" {
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

    // Form 1: <dep_name> = "x.y.z"  (optional trailing TOML comment)
    if let Some(rest) = trimmed.strip_prefix(dep_name) {
        let rest = rest.trim_start_matches([' ', '=', '\t']);
        if rest.starts_with('"') {
            // Strip any trailing `# comment` before matching the closing quote.
            let value_str = rest.split('#').next().unwrap_or(rest).trim_end();
            if let Some(version) = value_str
                .strip_prefix('"')
                .and_then(|r| r.strip_suffix('"'))
            {
                let indent_len = line.len() - line.trim_start().len();
                let indent = &line[..indent_len];
                return format!(
                    "{indent}{dep_name} = {{ version = \"{version}\", features = [{feature_quoted}] }}"
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

/// SQL for adding a stored generated `search_vector` column and GIN index.
#[must_use]
pub fn add_search_up_sql(table: &str, language: &str, fields: &[(String, char)]) -> String {
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
    out
}

/// `down.sql` companion to [`add_search_up_sql`].
#[must_use]
pub fn add_search_down_sql(table: &str) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "DROP INDEX IF EXISTS idx_{table}_search_vector;");
    let _ = writeln!(
        out,
        "ALTER TABLE {table} DROP COLUMN IF EXISTS search_vector;"
    );
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
mod tests {
    use super::*;
    use crate::generate::dsl::parse_field;

    fn fields(tokens: &[&str]) -> Vec<Field> {
        tokens.iter().map(|t| parse_field(t).unwrap()).collect()
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
        let sql = add_columns_up_sql("posts", &f);
        assert!(sql.contains("ALTER TABLE posts ADD COLUMN title TEXT NOT NULL;"));
        assert!(sql.contains("ALTER TABLE posts ADD COLUMN count INTEGER NOT NULL;"));
    }

    #[test]
    fn add_columns_up_sql_includes_safety_comment_for_not_null() {
        let f = fields(&["title:String"]);
        let sql = add_columns_up_sql("posts", &f);
        assert!(
            sql.contains("autumn-safety: potentially-blocking"),
            "NOT NULL column must carry a safety comment; got:\n{sql}"
        );
    }

    #[test]
    fn add_columns_up_sql_no_safety_comment_for_nullable() {
        let f = fields(&["subtitle:Option<String>"]);
        let sql = add_columns_up_sql("posts", &f);
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
    fn add_columns_down_sql_drops_in_reverse() {
        let f = fields(&["title:String", "count:i32"]);
        let sql = add_columns_down_sql("posts", &f);
        let title_pos = sql.find("DROP COLUMN title").unwrap();
        let count_pos = sql.find("DROP COLUMN count").unwrap();
        assert!(count_pos < title_pos);
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

    #[test]
    fn ensure_routes_entries_handles_empty_body() {
        let original = "fn main() {\n    routes![]\n}\n";
        let updated = ensure_routes_entries(original, &["foo".into()]);
        assert!(updated.contains("foo"));
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

    // ── ensure_autumn_web_feature ─────────────────────────────────────────

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
        let sql = add_search_up_sql(
            "posts",
            "english",
            &[("title".to_string(), 'A'), ("body".to_string(), 'B')],
        );
        assert!(sql.contains("coalesce(\"title\"::text, '')"));
        assert!(sql.contains("coalesce(\"body\"::text, '')"));
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
        let sql = add_search_up_sql(
            "posts",
            "english'; DROP TABLE posts;--",
            &[("title".to_string(), 'A')],
        );
        assert!(sql.contains("to_tsvector('englishDROPTABLEposts'::regconfig"));

        let sql_qualified =
            add_search_up_sql("posts", "pg_catalog.english", &[("title".to_string(), 'A')]);
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
        // Regression test (Codex review, issue #1023): a renamed dep, e.g.
        // `autumn_web = { package = "autumn-web", path = "../autumn" }`,
        // wasn't recognized at all -- the detector only matched the literal
        // `autumn-web` key, so it fell back to a mismatched crates.io
        // version. Confirmed via `cargo metadata --offline` that Cargo
        // unifies dependency sources by *package name* (here "autumn-web"),
        // not by the local alias key, so an unaliased `autumn-web = { path
        // = "../autumn", ... }` dev-dependency (mirroring just the source,
        // not the alias) resolves to the identical node as the aliased
        // `[dependencies]` entry.
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
    fn dev_dependency_test_support_mirrors_existing_pinned_version_not_cli_version() {
        // Regression test (Codex review, issue #1023): the fallback used to
        // insert `version = "<CLI's own CARGO_PKG_VERSION>"` unconditionally,
        // ignoring whatever `[dependencies]` actually pins. When the two
        // differ (e.g. a project pinned to an older `autumn-web = "0.5"`
        // while the CLI itself is `0.6`), Cargo's resolver can reject the
        // manifest outright if the two version requirements don't overlap
        // (confirmed via `cargo metadata`: "failed to select a version").
        // The dev-dependency entry must mirror the *existing* requirement,
        // not the CLI's.
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
        // Regression test (Codex review, issue #1023): the renamed-dep fixes
        // covered the inline-table (`autumn_web = { package = "autumn-web",
        // ... }`) and subtable (`[dependencies.autumn_web]`) alias shapes,
        // but not Cargo's dotted renamed-dependency form
        // (`autumn_web.package = "autumn-web"` plus `autumn_web.path =
        // "../autumn"` on separate lines). The alias-detection branch split
        // on the key's dot and compared the whole `autumn_web.package`
        // string against `autumn_web`, so it never matched and fell through
        // to a mismatched crates.io version. Confirmed via `cargo metadata
        // --offline` that two different paths for the same package name
        // conflict, same as the other alias forms.
        let cargo = "[package]\nname=\"x\"\n\n[dependencies]\nautumn_web.package = \"autumn-web\"\nautumn_web.path = \"../autumn\"\n";
        let updated = ensure_dev_dependency_test_support(cargo, "0.6");
        assert!(
            updated
                .contains("autumn-web = { path = \"../autumn\", features = [\"test-support\"] }"),
            "must mirror the dotted-aliased dep's path source: {updated}"
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
        // Regression test (Codex review, issue #1023): the same
        // source-mismatch bug that motivated the whole autumn-web
        // source-mirroring saga also applies to tokio -- if [dependencies]
        // sources tokio from a path/workspace/git override (e.g. an
        // internal fork), inserting a crates.io `version = "1"` dev entry
        // makes Cargo reject the manifest ("Dependency 'tokio' has
        // different source paths depending on the build target"; confirmed
        // via a hand-built `cargo metadata --offline` reproduction). The
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
}
