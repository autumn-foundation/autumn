//! Migration safety classification — pure SQL pattern analysis.
//!
//! Inspects the contents of `up.sql` files and classifies each SQL statement
//! into a risk tier. The result drives `autumn migrate check`'s exit code and
//! human-readable safety report printed to stderr.
//!
//! Classification is per backend (issue #1906). Postgres asks how risky a
//! statement is for a rolling deploy. `SQLite` asks that too, but first asks
//! whether the statement runs at all: its `ALTER TABLE` accepts only four
//! subcommands, and it has no `CONCURRENTLY`, `TRUNCATE`, sequences, types or
//! grants. A statement the backend cannot parse is
//! [`RiskLevel::Unsupported`].
//!
//! # Known limitations
//!
//! - Statement splitting uses `;` as the delimiter with awareness of
//!   `PostgreSQL` dollar-quoted blocks (`$$…$$` and `$tag$…$tag$`), `--` line
//!   comments, and single-quoted string literals (including the standard
//!   `''`-doubled escaped quote). Semicolons inside any of these are kept
//!   intact so they do not produce spurious statement fragments -- e.g. a
//!   `DEFAULT 'hello; world'` clause splits as one statement, not two.
//! - Line comment stripping matches `--` by position on each line, checked
//!   only *outside* single-quoted literals and dollar-quoted blocks (both
//!   handled above). A `--` or `;` sequence inside a double-quoted identifier
//!   (e.g. a quoted column name) is not handled; this pattern is essentially
//!   absent from real migration files.
//! - Block comment stripping (`/* … */`) similarly does not handle `/*` or `*/`
//!   tokens that appear inside string literals.

use std::fmt;

use autumn_web::config::DatabaseBackend;

/// Risk level for a migration operation, ordered from least to most risky.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum RiskLevel {
    /// Additive, backward-compatible schema change. Safe for rolling deploys.
    Safe,
    /// May acquire a table-level lock on large datasets.
    PotentiallyBlocking,
    /// Removes data or structure; old replicas may fail until they restart.
    Destructive,
    /// Cannot be easily reversed without a multi-step expand/contract cycle.
    Irreversible,
    /// Schema change is safe but requires a separate data backfill job.
    DataBackfill,
    /// Autumn cannot auto-classify this statement. Operator review required.
    ManualReview,
    /// The target backend does not accept this statement at all — it will fail
    /// at apply time (issue #1906). `SQLite` only.
    Unsupported,
}

impl fmt::Display for RiskLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Safe => write!(f, "safe"),
            Self::PotentiallyBlocking => write!(f, "potentially-blocking"),
            Self::Destructive => write!(f, "destructive"),
            Self::Irreversible => write!(f, "irreversible"),
            Self::DataBackfill => write!(f, "data-backfill"),
            Self::ManualReview => write!(f, "manual-review"),
            Self::Unsupported => write!(f, "unsupported"),
        }
    }
}

/// A single safety finding for one SQL statement in a migration file.
#[derive(Debug, Clone)]
pub struct SafetyFinding {
    /// Short description of the risky operation (e.g. `DROP COLUMN`).
    pub operation: String,
    /// Risk classification.
    pub risk: RiskLevel,
    /// Why this is dangerous for a rolling deploy.
    pub why: &'static str,
    /// Recommended next action for the operator.
    pub next_action: &'static str,
}

/// Classify the SQL content of an `up.sql` file and return all safety findings.
///
/// Returns an empty `Vec` when the migration is fully additive and safe.
///
/// A statement annotated with `-- autumn-safety: reviewed` is skipped entirely,
/// allowing operators to acknowledge and suppress findings they have manually
/// reviewed and accepted.
///
/// Classifies against Postgres. Use [`classify_sql_for`] on a `SQLite` app.
// Retained as a Postgres-default convenience wrapper for the test suite;
// production always passes the app's detected backend.
#[cfg(test)]
pub fn classify_sql(sql: &str) -> Vec<SafetyFinding> {
    classify_sql_for(DatabaseBackend::Postgres, sql)
}

/// Classify the SQL content of an `up.sql` file against a specific database
/// `backend` (issue #1906).
///
/// The Postgres path is unchanged. The `SQLite` path applies `SQLite`'s own
/// dialect rules: statements Postgres merely finds slow are often outright
/// invalid on `SQLite` (`ALTER COLUMN`, `TRUNCATE`, `CONCURRENTLY`,
/// `ADD COLUMN NOT NULL` with no default), and Postgres-only remedies
/// (`CREATE INDEX CONCURRENTLY`) must never be recommended there. Such
/// statements are reported as [`RiskLevel::Unsupported`] — they fail at apply
/// time, so the deploy gate blocks them.
pub fn classify_sql_for(backend: DatabaseBackend, sql: &str) -> Vec<SafetyFinding> {
    // Strip block comments at the whole-SQL level before splitting so that a
    // semicolon inside a block comment (e.g. `/* note; end */`) does not produce
    // a spurious empty statement fragment.
    let without_block_comments = strip_block_comments(sql);
    let stmts = split_statements(&without_block_comments);

    // Tables created within this migration have no existing rows or live traffic,
    // so a non-concurrent index build on them is safe.  Collect the names upfront
    // so we can suppress the false-positive PotentiallyBlocking finding for those
    // tables when we encounter their CREATE INDEX statements below.
    let newly_created: Vec<String> = stmts
        .iter()
        .filter(|s| !has_review_suppression(s))
        .filter_map(|s| extract_created_table_name(&normalize_statement(s)))
        .collect();

    stmts
        .iter()
        .filter(|stmt| !has_review_suppression(stmt))
        .flat_map(|stmt| {
            let normalized = normalize_statement(stmt);
            let mut findings = classify_statement(backend, &normalized, &newly_created);
            // Drop any index-build finding whose table was created earlier in
            // this same migration file (either backend's spelling of it).
            findings.retain(|f| {
                (f.operation != CREATE_INDEX_PG_OP && f.operation != CREATE_INDEX_SQLITE_OP)
                    || extract_index_table_name(&normalized)
                        .is_none_or(|t| !newly_created.iter().any(|c| c == t))
            });
            findings
        })
        .collect()
}

/// Returns `true` if the raw (un-normalized) statement carries an operator
/// acknowledgement marker (`-- autumn-safety: reviewed`).
///
/// The check is done on the raw text before comment-stripping so the marker
/// is not accidentally erased.
fn has_review_suppression(stmt: &str) -> bool {
    stmt.lines().any(|line| {
        let trimmed = line.trim();
        trimmed.starts_with("--") && trimmed.contains("autumn-safety: reviewed")
    })
}

/// True iff all findings are at the `Safe` risk level (or there are none).
pub fn is_safe(findings: &[SafetyFinding]) -> bool {
    findings.iter().all(|f| f.risk == RiskLevel::Safe)
}

/// True iff any finding exceeds the `Safe` risk level.
pub fn has_unsafe_findings(findings: &[SafetyFinding]) -> bool {
    findings.iter().any(|f| f.risk > RiskLevel::Safe)
}

/// True iff `sql` contains at least one non-empty, non-comment SQL statement.
///
/// Used to gate `autumn migrate down`: a `down.sql` that is blank or contains
/// only comments is treated as absent — the command refuses to proceed and
/// names the offending migration.
pub fn has_executable_sql(sql: &str) -> bool {
    let without_block_comments = strip_block_comments(sql);
    split_statements(&without_block_comments)
        .iter()
        .any(|stmt| {
            stmt.lines()
                .any(|line| !line.trim().is_empty() && !line.trim().starts_with("--"))
        })
}

// ── internals ────────────────────────────────────────────────────────────────

/// Extract the table name from a normalized `CREATE TABLE name …` statement.
/// Only unconditional creates that are known to create a fresh table are matched.
fn extract_created_table_name(normalized: &str) -> Option<String> {
    let rest = normalized.strip_prefix("create table ")?;
    if rest.starts_with("if not exists ") {
        return None;
    }
    let name = rest.split([' ', '(']).next()?;
    if name.is_empty() {
        None
    } else {
        Some(name.to_owned())
    }
}

/// Extract the target table name from a normalized `CREATE [UNIQUE] INDEX … ON name …` statement.
fn extract_index_table_name(normalized: &str) -> Option<&str> {
    let after_on = normalized.find(" on ").map(|i| &normalized[i + 4..])?;
    let name = after_on.split([' ', '(']).next()?;
    if name.is_empty() { None } else { Some(name) }
}

/// Extract the table name from a normalized `ALTER TABLE name …` statement.
fn extract_altered_table_name(normalized: &str) -> Option<&str> {
    let rest = normalized.strip_prefix("alter table ")?;
    let rest = rest.strip_prefix("if exists ").unwrap_or(rest);
    let rest = rest.strip_prefix("only ").unwrap_or(rest);
    let name = rest.split([' ', '(']).next()?;
    if name.is_empty() { None } else { Some(name) }
}

/// Split `sql` into individual statements, using `;` as the delimiter. Each
/// returned statement has its terminating `;` stripped.
///
/// Dollar-quoted blocks (`$$…$$`, `$tag$…$tag$`) are kept intact so that
/// semicolons inside a function body do not produce spurious fragments.
pub fn split_statements(sql: &str) -> Vec<String> {
    let mut statements = Vec::new();
    let mut current = String::new();
    let mut i = 0;

    while i < sql.len() {
        let rest = &sql[i..];

        // Detect a dollar-quote opening: $identifier$ (identifier may be empty → $$).
        // When found, consume everything up to and including the matching closing tag
        // so that semicolons inside the body are not treated as statement separators.
        if let Some(after_dollar) = rest.strip_prefix('$')
            && let Some(close_in_rest1) = after_dollar.find('$')
        {
            let tag_body = &after_dollar[..close_in_rest1];
            if tag_body.chars().all(|c| c.is_alphanumeric() || c == '_') {
                let tag_len = 1 + close_in_rest1 + 1; // opening $ + body + closing $
                let tag = &rest[..tag_len];
                if let Some(close_pos) = rest[tag_len..].find(tag) {
                    // Push opening tag + body + closing tag as one chunk.
                    current.push_str(&rest[..tag_len + close_pos + tag_len]);
                    i += tag_len + close_pos + tag_len;
                } else {
                    // Unclosed dollar-quote: consume to end of input.
                    current.push_str(rest);
                    i = sql.len();
                }
                continue;
            }
        }

        // Single-quoted string literal: consume to the closing quote, treating
        // a doubled `''` as an escaped quote (the standard SQL convention --
        // matches how `sql_default_literal` escapes user-supplied defaults) so
        // a semicolon inside a literal (e.g. a `DEFAULT 'hello; world'`) is
        // never treated as a statement separator.
        if rest.starts_with('\'') {
            let mut j = 1;
            loop {
                if let Some(rel) = rest[j..].find('\'') {
                    let abs = j + rel;
                    if rest[abs + 1..].starts_with('\'') {
                        j = abs + 2; // escaped '' -- keep scanning
                    } else {
                        j = abs + 1; // closing quote
                        break;
                    }
                } else {
                    j = rest.len(); // unclosed literal: consume to end of input
                    break;
                }
            }
            current.push_str(&rest[..j]);
            i += j;
            continue;
        }

        // Line comment: consume to end-of-line without treating the semicolons
        // inside the comment as statement separators.  The comment text is kept
        // in `current` so that `has_review_suppression` can still see it.
        if rest.starts_with("--") {
            let end = rest.find('\n').unwrap_or(rest.len());
            current.push_str(&rest[..end]);
            i += end;
        } else if rest.starts_with(';') {
            let trimmed = current.trim().to_owned();
            if !trimmed.is_empty() {
                statements.push(trimmed);
            }
            current.clear();
            i += 1;
        } else {
            let c = rest.chars().next().unwrap();
            current.push(c);
            i += c.len_utf8();
        }
    }

    let trimmed = current.trim().to_owned();
    if !trimmed.is_empty() {
        statements.push(trimmed);
    }
    statements
}

/// Split a normalized `ALTER TABLE` statement into individual subcommand strings.
///
/// Strips the `alter table <name>` prefix and splits the remaining text on
/// commas that are not enclosed in parentheses, trimming each segment.
fn alter_table_subcommands(normalized: &str) -> Vec<&str> {
    let after_prefix = normalized.strip_prefix("alter table ").unwrap_or("");
    let subcommands_start = after_prefix.find(' ').map_or(after_prefix.len(), |i| i + 1);
    let subcommands = &after_prefix[subcommands_start..];

    let mut result = Vec::new();
    let mut depth: i32 = 0;
    let mut start = 0;
    for (i, c) in subcommands.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            ',' if depth == 0 => {
                result.push(subcommands[start..i].trim());
                start = i + 1;
            }
            _ => {}
        }
    }
    let last = subcommands[start..].trim();
    if !last.is_empty() {
        result.push(last);
    }
    result
}

/// True iff `subcommand` is an ALTER TABLE subcommand that Autumn fully classifies.
///
/// A subcommand is "known" when a specific safety rule covers all risk scenarios
/// for it (including the case where it is safe and produces no finding).
fn is_known_alter_subcommand(backend: DatabaseBackend, subcommand: &str) -> bool {
    // On SQLite every subcommand is already classified: the four the grammar
    // accepts by the rules below, and every other one as Unsupported. So none
    // is left for the manual-review fallback.
    if backend == DatabaseBackend::Sqlite {
        return true;
    }
    // `add column` is known unless it carries an inline PRIMARY KEY constraint,
    // which Autumn does not specifically classify.  UNIQUE and REFERENCES are handled
    // by dedicated rules; NOT NULL is handled by the NOT NULL rule.
    let add_col_known =
        subcommand.starts_with("add column ") && !subcommand.contains(" primary key");
    add_col_known
        || subcommand.starts_with("drop column ")
        || subcommand.starts_with("rename column ") // RENAME COLUMN
        || subcommand.starts_with("rename to ") // RENAME TABLE (ALTER TABLE … RENAME TO …)
        || (subcommand.starts_with("alter column ") && subcommand.contains(" type "))
}

/// Rewrite `SQLite`'s optional-`COLUMN` `ALTER TABLE` spellings into the
/// canonical ones, so `ADD title TEXT` classifies exactly as
/// `ADD COLUMN title TEXT`. `ADD CONSTRAINT` and `DROP CONSTRAINT` are left
/// alone: they are Postgres-only, and the grammar rule must still reject them.
fn canonical_sqlite_alter(normalized: &str) -> String {
    if !normalized.starts_with("alter table ") {
        return normalized.to_owned();
    }
    let subcommands = alter_table_subcommands(normalized);
    if subcommands.is_empty() {
        return normalized.to_owned();
    }
    let head_len = normalized.len() - subcommands.join(", ").len();
    let head = &normalized[..head_len];
    let rewritten: Vec<String> = subcommands
        .iter()
        .map(|sub| {
            for verb in ["add", "drop"] {
                let prefix = format!("{verb} ");
                if let Some(rest) = sub.strip_prefix(&prefix)
                    && !rest.starts_with("column ")
                    && !rest.starts_with("constraint ")
                {
                    return format!("{verb} column {rest}");
                }
            }
            (*sub).to_owned()
        })
        .collect();
    format!("{head}{}", rewritten.join(", "))
}

/// Whether `subcommand` is one of the four `ALTER TABLE` forms `SQLite` parses:
/// `RENAME TO`, `RENAME [COLUMN] … TO …`, `ADD COLUMN` and `DROP COLUMN`.
/// `SQLite` also accepts `ADD`/`DROP` with the `COLUMN` keyword left out.
fn is_sqlite_alter_subcommand(subcommand: &str) -> bool {
    ["rename to ", "rename column ", "add column ", "drop column "]
        .iter()
        .any(|p| subcommand.starts_with(p))
        // Bare `RENAME old TO new`, `ADD <col> …`, `DROP <col>` — but not
        // `ADD CONSTRAINT`/`DROP CONSTRAINT`, which SQLite rejects.
        || (subcommand.starts_with("rename ") && subcommand.contains(" to "))
        || (subcommand.starts_with("add ") && !subcommand.starts_with("add constraint "))
        || (subcommand.starts_with("drop ") && !subcommand.starts_with("drop constraint "))
}

/// Operation label for a non-concurrent Postgres index build.
const CREATE_INDEX_PG_OP: &str = "CREATE INDEX (non-concurrent)";
/// Operation label for a `SQLite` index build. `SQLite` has no `CONCURRENTLY`,
/// so "non-concurrent" would be a meaningless qualifier there.
const CREATE_INDEX_SQLITE_OP: &str = "CREATE INDEX";

/// The value of an `add column` subcommand's `DEFAULT` clause, lowercased, or
/// `None` when the subcommand carries no default.
///
/// The keyword is matched at a word boundary, so `DEFAULT(0)` — valid SQL with
/// no space — is read, while a column named `defaulted` is not.
///
/// Three shapes are read whole rather than split on whitespace: a parenthesized
/// expression (`(lower(x))`), a quoted literal including one with spaces
/// (`'a b'`, with `''` as the escaped quote), and a signed number (`-1`).
/// A `DEFAULT` inside a foreign-key action (`ON UPDATE SET DEFAULT`) is not a
/// column default and is skipped.
fn add_column_default_token(subcommand: &str) -> Option<String> {
    let mut from = 0;
    let rest = loop {
        let at = subcommand[from..].find(" default")? + from;
        let after = &subcommand[at + " default".len()..];
        // `DEFAULT(0)` needs no space, but `defaulted` is a different word.
        let value = match after.strip_prefix(' ') {
            Some(v) => Some(v),
            None if after.starts_with('(') => Some(after),
            None => None,
        };
        // `SET DEFAULT` here is an FK action, not this column's default.
        if let Some(value) = value
            && !subcommand[..at].trim_end().ends_with(" set")
        {
            break value.trim_start();
        }
        from = at + " default".len();
    };
    if rest.starts_with('(') {
        let mut depth = 0usize;
        for (i, c) in rest.char_indices() {
            match c {
                '(' => depth += 1,
                // `depth` is at least 1 here: the first character is `(`.
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(rest[..=i].to_owned());
                    }
                }
                _ => {}
            }
        }
        return Some(rest.to_owned());
    }
    if rest.starts_with('\'') {
        let bytes = rest.as_bytes();
        let mut i = 1;
        while i < bytes.len() {
            if bytes[i] == b'\'' {
                if bytes.get(i + 1) == Some(&b'\'') {
                    i += 2; // `''` is one escaped quote
                    continue;
                }
                return Some(rest[..=i].to_owned());
            }
            i += 1;
        }
        return Some(rest.to_owned());
    }
    Some(
        rest.split_whitespace()
            .next()
            .unwrap_or_default()
            .trim_end_matches(',')
            .to_owned(),
    )
}

/// Whether `default` is a constant `SQLite` accepts as an `ADD COLUMN` default.
///
/// `SQLite`'s rule is that the default must reduce to a constant value. Its
/// parser keeps no node for parentheses, so `DEFAULT ('draft')` is the same as
/// `DEFAULT 'draft'` and is accepted; `DEFAULT (1+2)` and `DEFAULT (abs(-1))`
/// are not. `CURRENT_TIME`, `CURRENT_DATE` and `CURRENT_TIMESTAMP` are rejected
/// by name. An unparenthesized call such as `DEFAULT now()` is not even
/// grammatical — only a literal or signed number may follow `DEFAULT` bare — so
/// it counts as non-constant too.
fn is_sqlite_constant_default(default: &str) -> bool {
    let inner = default
        .strip_prefix('(')
        .and_then(|d| d.strip_suffix(')'))
        .unwrap_or(default)
        .trim();
    if matches!(inner, "current_time" | "current_date" | "current_timestamp") {
        return false;
    }
    // A quoted literal is constant whatever it contains.
    if inner.starts_with('\'') || inner.starts_with("x'") {
        return true;
    }
    // A number, including exponent form (`1e-3`), whose sign is part of the
    // literal rather than an operator.
    if is_numeric_literal(inner) {
        return true;
    }
    // Otherwise: a bare keyword literal. A call or an operator makes it an
    // expression.
    let unsigned = inner.trim_start_matches(['+', '-']).trim_start();
    !unsigned.contains(['(', ')', '+', '-', '*', '/', '|', '<', '>', '=', '\''])
}

/// Whether `value` is an `SQLite` numeric literal: an optionally signed integer
/// or real, in plain, exponent (`1e-3`) or hex (`0x1f`) form.
fn is_numeric_literal(value: &str) -> bool {
    let body = value.strip_prefix(['+', '-']).unwrap_or(value);
    if let Some(hex) = body.strip_prefix("0x").or_else(|| body.strip_prefix("0X")) {
        return !hex.is_empty() && hex.chars().all(|c| c.is_ascii_hexdigit());
    }
    let (mantissa, exponent) = body
        .split_once(['e', 'E'])
        .map_or((body, None), |(m, e)| (m, Some(e)));
    if !is_decimal(mantissa) {
        return false;
    }
    exponent.is_none_or(|e| {
        let digits = e.strip_prefix(['+', '-']).unwrap_or(e);
        !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit())
    })
}

/// Whether `value` is an unsigned decimal integer or real (`12`, `1.5`, `.5`).
fn is_decimal(value: &str) -> bool {
    let (int, frac) = value
        .split_once('.')
        .map_or((value, None), |(i, f)| (i, Some(f)));
    if int.is_empty() && frac.is_none_or(str::is_empty) {
        return false;
    }
    int.chars().all(|c| c.is_ascii_digit())
        && frac.is_none_or(|f| f.chars().all(|c| c.is_ascii_digit()))
}

/// A statement `SQLite` has no syntax for at all (issue #1906).
///
/// These are Postgres-only objects (sequences, types, extensions, materialized
/// views), Postgres-only statements (`TRUNCATE`, `COMMENT ON`, `GRANT`,
/// `REVOKE`) or the Postgres-only `CONCURRENTLY` index option. Each fails at
/// apply time on `SQLite`, so it is reported as [`RiskLevel::Unsupported`]
/// rather than classified for rolling-deploy risk.
fn sqlite_unsupported_statement(normalized: &str) -> Option<SafetyFinding> {
    let (operation, next_action) = if normalized.starts_with("truncate ") {
        (
            "TRUNCATE (unsupported on SQLite)",
            "SQLite has no TRUNCATE statement. Use `DELETE FROM <table>;` (add `DELETE FROM \
             sqlite_sequence WHERE name = '<table>';` to reset AUTOINCREMENT).",
        )
    } else if normalized.starts_with("create index concurrently ")
        || normalized.starts_with("create unique index concurrently ")
    {
        (
            "CREATE INDEX CONCURRENTLY (unsupported on SQLite)",
            "SQLite has no CONCURRENTLY option. Drop the keyword and build the index during a \
             low-traffic window.",
        )
    } else if normalized.starts_with("drop index concurrently ") {
        (
            "DROP INDEX CONCURRENTLY (unsupported on SQLite)",
            "SQLite has no CONCURRENTLY option. Drop the keyword — a plain DROP INDEX is a cheap \
             catalog edit on SQLite.",
        )
    } else if normalized.starts_with("create sequence")
        || normalized.starts_with("alter sequence")
        || normalized.starts_with("drop sequence")
    {
        (
            "SEQUENCE statement (unsupported on SQLite)",
            "SQLite has no sequences. Use an INTEGER PRIMARY KEY AUTOINCREMENT column instead.",
        )
    } else if normalized.starts_with("create type")
        || normalized.starts_with("alter type")
        || normalized.starts_with("drop type")
    {
        (
            "TYPE statement (unsupported on SQLite)",
            "SQLite has no user-defined types. Model an enum as a TEXT column with a CHECK \
             constraint.",
        )
    } else if normalized.starts_with("create extension") || normalized.starts_with("drop extension")
    {
        (
            "EXTENSION statement (unsupported on SQLite)",
            "SQLite has no extensions in this sense. Remove the statement, or gate the migration to \
             the Postgres backend.",
        )
    } else if normalized.starts_with("comment on") {
        (
            "COMMENT ON (unsupported on SQLite)",
            "SQLite has no COMMENT ON. Remove the statement — it carries no schema meaning.",
        )
    } else if normalized.starts_with("create materialized view")
        || normalized.starts_with("drop materialized view")
        || normalized.starts_with("refresh materialized view")
    {
        (
            "MATERIALIZED VIEW statement (unsupported on SQLite)",
            "SQLite has no materialized views. Use a plain view, or a real table refreshed by a \
             background job.",
        )
    } else if normalized.starts_with("grant ") || normalized.starts_with("revoke ") {
        (
            "GRANT/REVOKE (unsupported on SQLite)",
            "SQLite has no roles or grants — file permissions are the access control. Remove the \
             statement.",
        )
    } else {
        return sqlite_unsupported_clause(normalized);
    };
    Some(SafetyFinding {
        operation: operation.to_owned(),
        risk: RiskLevel::Unsupported,
        why: "SQLite has no syntax for this statement, so the migration fails at apply time.",
        next_action,
    })
}

/// A statement whose *shape* `SQLite` accepts but which carries a Postgres-only
/// clause, plus the DML forms `SQLite` has no grammar for (issue #1906).
///
/// Split out from [`sqlite_unsupported_statement`], which handles the
/// Postgres-only object statements.
fn sqlite_unsupported_clause(normalized: &str) -> Option<SafetyFinding> {
    let (operation, next_action) = if normalized.starts_with("merge into ") {
        (
            "MERGE (unsupported on SQLite)",
            "SQLite has no MERGE. Use INSERT … ON CONFLICT (upsert), naming the conflicting \
             columns rather than a constraint.",
        )
    } else if normalized.starts_with("drop index ")
        && normalized
            .trim_start_matches("drop index ")
            .trim_start_matches("if exists ")
            .starts_with("sqlite_autoindex")
    {
        (
            "DROP INDEX on an implicit index (unsupported on SQLite)",
            "SQLite refuses to drop the index behind a UNIQUE or PRIMARY KEY constraint, even \
             with IF EXISTS. Rebuild the table without the constraint instead.",
        )
    } else if is_create_index_statement(normalized) && has_postgres_index_clause(normalized) {
        (
            "CREATE INDEX with a Postgres-only clause (unsupported on SQLite)",
            "SQLite's CREATE INDEX takes no USING, INCLUDE, WITH, TABLESPACE or NULLS NOT \
             DISTINCT clause. Drop it — SQLite indexes are always B-trees. WHERE (partial), \
             COLLATE and DESC are supported.",
        )
    } else if normalized.starts_with("create table") && has_postgres_table_clause(normalized) {
        (
            "CREATE TABLE with a Postgres-only clause (unsupported on SQLite)",
            "SQLite has no declarative partitioning, identity columns, table inheritance or \
             tablespaces. Use INTEGER PRIMARY KEY AUTOINCREMENT for an identity column; model \
             partitioning in the application.",
        )
    } else if has_row_locking_clause(normalized) {
        (
            "SELECT … FOR UPDATE (unsupported on SQLite)",
            "SQLite has no row-level locking clause — a write transaction locks the whole \
             database. Drop the clause.",
        )
    } else if without_string_literals(normalized).contains("on conflict on constraint ") {
        (
            "ON CONFLICT ON CONSTRAINT (unsupported on SQLite)",
            "SQLite's upsert target is a column list, not a constraint name. Write \
             `ON CONFLICT (<columns>)`.",
        )
    } else if normalized.starts_with("with ") && contains_data_modifying_cte(normalized) {
        (
            "Data-modifying CTE (unsupported on SQLite)",
            "SQLite allows only SELECT inside a CTE. Run the UPDATE/DELETE/INSERT as its own \
             statement.",
        )
    } else {
        return None;
    };
    Some(SafetyFinding {
        operation: operation.to_owned(),
        risk: RiskLevel::Unsupported,
        why: "SQLite has no syntax for this statement, so the migration fails at apply time.",
        next_action,
    })
}

/// Whether the statement carries a Postgres row-locking clause.
///
/// Searches the literal-blanked form: an INSERT of the text
/// `'waiting for update'` is data, not a locking clause.
fn has_row_locking_clause(normalized: &str) -> bool {
    let bare = without_string_literals(normalized);
    bare.contains(" for update") || bare.contains(" for share")
}

/// Whether the statement is a `CREATE [UNIQUE] INDEX`.
fn is_create_index_statement(normalized: &str) -> bool {
    normalized.starts_with("create index") || normalized.starts_with("create unique index")
}

/// Whether a `CREATE INDEX` carries a clause `SQLite` has no grammar for.
/// `WHERE` (partial), `COLLATE` and `DESC` are absent from the list: `SQLite`
/// supports all three.
fn has_postgres_index_clause(normalized: &str) -> bool {
    let sql = without_string_literals(normalized);
    sql.contains(" using ")
        || sql.contains(" include (")
        || sql.contains(" with (")
        || sql.contains(" tablespace ")
        || sql.contains(" nulls not distinct")
}

/// Whether a `CREATE TABLE` carries a clause `SQLite` has no grammar for.
fn has_postgres_table_clause(normalized: &str) -> bool {
    let sql = without_string_literals(normalized);
    sql.contains(" partition by ")
        || sql.contains(" partition of ")
        || sql.contains(" as identity")
        || sql.contains(" inherits (")
        || sql.contains(" tablespace ")
}

/// Whether a `WITH` statement writes inside a CTE. `SQLite` allows only
/// `SELECT` there.
fn contains_data_modifying_cte(normalized: &str) -> bool {
    // `AS ( DELETE FROM …` is ordinary formatting; close the paren up first.
    let sql = without_string_literals(normalized).replace("( ", "(");
    ["(update ", "(delete ", "(insert into "]
        .iter()
        .any(|p| sql.contains(p))
}

/// Blank out the contents of single-quoted literals, keeping the quotes and the
/// string's length. Lets a keyword search run over SQL without matching a word
/// that only appears inside a value (`DEFAULT 'not unique yet'`).
fn without_string_literals(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut in_literal = false;
    for c in sql.chars() {
        match c {
            '\'' => {
                in_literal = !in_literal;
                out.push(c);
            }
            _ if in_literal => out.push(' '),
            _ => out.push(c),
        }
    }
    out
}

/// Returns `true` when the normalized `add column` subcommand carries an inline
/// `UNIQUE` constraint keyword.  Trailing-space / end-of-string anchoring prevents
/// matching a column or table name that contains `unique` as a substring.
fn has_inline_unique_constraint(subcommand: &str) -> bool {
    subcommand.contains(" unique ") || subcommand.ends_with(" unique")
}

/// Remove `/* ... */` block comments from `sql`.
///
/// Handles single-line and multi-line block comments. Unclosed block comments
/// are consumed to end-of-input. Block comments inside string literals are an
/// edge case not handled here (same limitation as `--` in strings).
pub fn strip_block_comments(sql: &str) -> String {
    let mut result = String::with_capacity(sql.len());
    let mut chars = sql.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '/' && chars.peek() == Some(&'*') {
            chars.next(); // consume '*'
            loop {
                match chars.next() {
                    Some('*') if chars.peek() == Some(&'/') => {
                        chars.next(); // consume '/'
                        result.push(' '); // preserve token boundary where the comment was
                        break;
                    }
                    None => break, // unclosed block comment
                    _ => {}
                }
            }
        } else {
            result.push(c);
        }
    }
    result
}

/// Strip line comments, collapse whitespace, and lowercase a single statement.
pub fn normalize_statement(stmt: &str) -> String {
    let without_block_comments = strip_block_comments(stmt);
    without_block_comments
        .lines()
        .map(|line| line.find("--").map_or(line, |i| &line[..i]))
        .flat_map(str::split_whitespace)
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Returns `true` when `sql` contains an executable concurrent index operation
/// (`CREATE [UNIQUE] INDEX CONCURRENTLY` or `DROP INDEX CONCURRENTLY`).
///
/// Uses the same comment-stripping and whitespace-normalization pipeline as
/// [`classify_sql_for`] so that concurrent index keywords mentioned only inside a
/// SQL comment (e.g. `-- CREATE INDEX CONCURRENTLY ...`) are not counted.
pub fn contains_concurrent_index(sql: &str) -> bool {
    split_statements(sql).iter().any(|stmt| {
        let normalized = normalize_statement(stmt);
        normalized.contains("create index concurrently ")
            || normalized.contains("create unique index concurrently ")
            || normalized.starts_with("drop index concurrently ")
    })
}

/// `PostgreSQL` STABLE/IMMUTABLE function prefixes that are safe as NOT NULL column
/// defaults: Postgres evaluates them once at statement time and stores the result as
/// a constant, so the PG 11+ fast-default path applies — no table rewrite needed.
const STABLE_FN_PREFIXES: &[&str] = &["now(", "current_timestamp(", "localtimestamp("];

/// Returns `true` when `default_expr` is a volatile function call that `PostgreSQL`
/// cannot optimise via the PG 11+ fast-constant-default path.
///
/// Only VOLATILE function calls are flagged.  Grouped constant expressions such as
/// `(0)` or `('draft')` have `(` as the first non-space character (no identifier
/// before the parenthesis) and are treated as constants — they use the fast path.
/// STABLE/IMMUTABLE functions (e.g. `now()`) are also exempt via `STABLE_FN_PREFIXES`.
fn is_volatile_function_default(default_expr: &str) -> bool {
    let Some(paren_pos) = default_expr.find('(') else {
        return false; // no parentheses — constant literal
    };
    // A function call has an identifier character immediately before `(`.
    // A grouped constant like `(0)` or `('x')` has nothing or whitespace before it.
    let is_fn_call = default_expr[..paren_pos]
        .chars()
        .next_back()
        .is_some_and(|c| c.is_alphanumeric() || c == '_');
    if !is_fn_call {
        return false; // parenthesized constant — uses the fast-default path
    }
    !STABLE_FN_PREFIXES
        .iter()
        .any(|p| default_expr.starts_with(p))
}

/// Apply all pattern checks to a single normalized (lowercase, single-spaced) statement.
#[allow(clippy::too_many_lines)]
fn classify_statement(
    backend: DatabaseBackend,
    normalized: &str,
    newly_created: &[String],
) -> Vec<SafetyFinding> {
    if normalized.is_empty() {
        return vec![];
    }
    let sqlite = backend == DatabaseBackend::Sqlite;
    // SQLite lets `ALTER TABLE … ADD <col>` and `DROP <col>` omit the `COLUMN`
    // keyword. Every rule below matches the `COLUMN` spelling, so canonicalize
    // first rather than duplicating each match.
    let canonical;
    let normalized = if sqlite {
        canonical = canonical_sqlite_alter(normalized);
        canonical.as_str()
    } else {
        normalized
    };

    // Statements SQLite has no syntax for at all. They fail at apply time, so
    // there is nothing further to classify about them.
    if sqlite && let Some(f) = sqlite_unsupported_statement(normalized) {
        return vec![f];
    }

    let mut findings = Vec::new();
    // Some SQLite ADD COLUMN restrictions apply only to a table that already has
    // rows: NOT NULL without a usable default, a non-constant default, and a
    // STORED generated column. A table this same migration creates has none, so
    // those statements apply cleanly — the suppression `classify_sql_for`
    // already makes for index builds. UNIQUE and PRIMARY KEY are NOT in that
    // set: SQLite rejects them on an empty table too.
    let fresh_table = sqlite
        && extract_altered_table_name(normalized)
            .is_some_and(|t| newly_created.iter().any(|c| *c == t));

    // DROP TABLE — check first; it subsumes DROP COLUMN detection
    if normalized.starts_with("drop table") {
        findings.push(SafetyFinding {
            operation: "DROP TABLE".to_owned(),
            risk: RiskLevel::Destructive,
            why: "Drops the entire table and all its data. Old replicas that reference this \
                  table will error immediately.",
            next_action: "Use expand/contract: first deploy code that stops using the table, \
                          then drop it in a subsequent release.",
        });
        return findings;
    }

    // DROP VIEW
    if normalized.starts_with("drop view") {
        findings.push(SafetyFinding {
            operation: "DROP VIEW".to_owned(),
            risk: RiskLevel::Destructive,
            why: "Drops the view. Old replicas that query this view will error immediately \
                  during a rolling deploy.",
            next_action: "Use expand/contract: first deploy code that no longer references the \
                          view, then drop it in a subsequent release.",
        });
        return findings;
    }

    // DROP SEQUENCE
    if normalized.starts_with("drop sequence") {
        findings.push(SafetyFinding {
            operation: "DROP SEQUENCE".to_owned(),
            risk: RiskLevel::Destructive,
            why: "Dropping a sequence breaks any column that uses it as a default \
                  (`nextval(seq)`) and any application code that calls `nextval` directly. \
                  Old replicas will error immediately on INSERT.",
            next_action: "Use expand/contract: first deploy code that no longer relies on this \
                          sequence, then drop it in a subsequent release.",
        });
        return findings;
    }

    // TRUNCATE TABLE
    if normalized.starts_with("truncate ") {
        findings.push(SafetyFinding {
            operation: "TRUNCATE".to_owned(),
            risk: RiskLevel::Destructive,
            why: "Truncating a table deletes all data and acquires an AccessExclusiveLock, \
                  blocking all concurrent reads and writes.",
            next_action: "If you need to empty the table, delete rows in small batches, or perform \
                          the truncate during a coordinated maintenance window.",
        });
        return findings;
    }

    // DROP COLUMN
    if normalized.contains(" drop column ") {
        findings.push(SafetyFinding {
            operation: "DROP COLUMN".to_owned(),
            risk: RiskLevel::Destructive,
            why: if sqlite {
                "Removes a column and its data. SQLite also refuses the statement outright in \
                 several cases. The column must not be a primary key, UNIQUE, or named by any \
                 index — a partial index's WHERE clause included. It must not appear in a CHECK \
                 constraint, a generated column, a foreign key, a view or a trigger."
            } else {
                "Removes a column and its data. Old replicas that SELECT or INSERT this column \
                 will error until they restart."
            },
            next_action: if sqlite {
                "DROP INDEX every index that names this column earlier in the same migration. \
                 For a primary-key, UNIQUE, CHECK or generated-column reference, rebuild the \
                 table instead (`autumn schema diff --write-migration` emits the rebuild)."
            } else {
                "Use expand/contract: first deploy code that no longer reads or writes \
                 this column, then drop it in the next release."
            },
        });
    }

    // RENAME COLUMN
    if normalized.contains(" rename column ") {
        findings.push(SafetyFinding {
            operation: "RENAME COLUMN".to_owned(),
            risk: RiskLevel::Irreversible,
            why: "Renaming a column breaks queries from old replicas that still reference the \
                  old name, causing errors during a rolling deploy.",
            next_action: "Use expand/contract: add the new column, dual-write, backfill existing \
                          rows, update all code, then drop the old column.",
        });
    }

    // RENAME TABLE
    if normalized.contains("alter table")
        && normalized.contains(" rename to ")
        && !normalized.contains(" rename column ")
    {
        findings.push(SafetyFinding {
            operation: "RENAME TABLE".to_owned(),
            risk: RiskLevel::Irreversible,
            why: "Renaming a table breaks all queries from old replicas that reference the \
                  original name.",
            next_action: "Create a view under the old name while the new name rolls out, or \
                          coordinate a maintenance window.",
        });
    }

    // SQLite's ALTER TABLE grammar is exactly RENAME TO, RENAME [COLUMN],
    // ADD COLUMN and DROP COLUMN. Every other subcommand — ALTER COLUMN in any
    // spelling, ADD/DROP CONSTRAINT, SET SCHEMA, OWNER TO — is a parse error, so
    // one rule covers them all rather than a list that can fall behind Postgres.
    if sqlite && normalized.starts_with("alter table") {
        let subcommands = alter_table_subcommands(normalized);
        // SQLite takes exactly one action per ALTER TABLE; Postgres allows a
        // comma-separated list.
        if subcommands.len() > 1 {
            findings.push(SafetyFinding {
                operation: "Multi-action ALTER TABLE (unsupported on SQLite)".to_owned(),
                risk: RiskLevel::Unsupported,
                why: "SQLite takes one action per ALTER TABLE. A comma-separated list is a parse \
                      error, so this statement fails at apply time.",
                next_action: "Split the statement: one ALTER TABLE per action.",
            });
        }
        for subcommand in subcommands {
            if is_sqlite_alter_subcommand(subcommand) {
                continue;
            }
            let is_alter_column = subcommand.starts_with("alter column ");
            findings.push(SafetyFinding {
                operation: if is_alter_column {
                    "ALTER COLUMN (unsupported on SQLite)".to_owned()
                } else {
                    "ALTER TABLE subcommand (unsupported on SQLite)".to_owned()
                },
                risk: RiskLevel::Unsupported,
                why: "SQLite's ALTER TABLE supports only RENAME, ADD COLUMN and DROP COLUMN. \
                      Any other subcommand is a parse error, so this statement fails at apply \
                      time.",
                next_action: "Rebuild the table: create the new-shape table, copy the rows, drop \
                              the old table, rename the new one. `autumn schema diff \
                              --write-migration` emits that rebuild for you.",
            });
            break; // one finding per statement is enough
        }
    }

    // ALTER COLUMN TYPE (Postgres)
    if !sqlite
        && let Some(i) = normalized.find("alter column")
        && normalized[i..].contains(" type ")
    {
        findings.push(SafetyFinding {
            operation: "ALTER COLUMN TYPE".to_owned(),
            risk: RiskLevel::Destructive,
            why: "Changing a column's type rewrites the column data and may be incompatible \
                  with values read by old replicas or application code.",
            next_action: "Add a new column with the target type, migrate data, update code to \
                          use the new column, then drop the old one.",
        });
    }

    // ADD COLUMN NOT NULL — checked per subcommand so that a DEFAULT on one column
    // in a multi-column ALTER TABLE does not suppress the check for other columns.
    //
    // Two unsafe cases:
    //  1. No DEFAULT at all — Postgres must validate every existing row under lock.
    //  2. Volatile DEFAULT (contains a function call) — Postgres must evaluate the
    //     function per-row and cannot use the fast constant-default path (PG 11+),
    //     so the table is still rewritten under the exclusive lock.
    if normalized.starts_with("alter table") {
        for subcommand in alter_table_subcommands(normalized) {
            if subcommand.starts_with("add column ") && subcommand.contains("not null") {
                // SQLite drops a literal `DEFAULT NULL` before applying the
                // NOT NULL check, so it is not a default there.
                let default_token = add_column_default_token(subcommand);
                let has_default = if sqlite {
                    default_token.as_deref().is_some_and(|d| d != "null")
                } else {
                    subcommand.contains(" default ")
                };
                // A DEFAULT is "volatile" when it is a VOLATILE function call —
                // i.e. one that Postgres must evaluate per-row, preventing the PG11+
                // fast-constant-default path.  STABLE functions (e.g. `now()`) are
                // evaluated once at statement time and do not require a table rewrite.
                let has_volatile_default = has_default
                    && subcommand.find(" default ").is_some_and(|pos| {
                        let default_expr = subcommand[pos + " default ".len()..].trim();
                        is_volatile_function_default(default_expr)
                    });

                if !has_default {
                    findings.push(SafetyFinding {
                        operation: "ADD COLUMN NOT NULL (no default)".to_owned(),
                        // SQLite does not merely slow down here: it rejects the
                        // statement ("Cannot add a NOT NULL column with default
                        // value NULL"), so this can never apply.
                        risk: if sqlite && !fresh_table {
                            RiskLevel::Unsupported
                        } else if sqlite {
                            // A table created in this same migration has no rows,
                            // so SQLite accepts the statement; Postgres still
                            // validates nothing on an empty table either.
                            RiskLevel::Safe
                        } else {
                            RiskLevel::PotentiallyBlocking
                        },
                        why: if sqlite {
                            "SQLite rejects ALTER TABLE … ADD COLUMN … NOT NULL without a \
                             DEFAULT outright — the statement fails at apply time."
                        } else {
                            "Adding a NOT NULL column without a DEFAULT forces Postgres to \
                             validate every existing row under an exclusive lock. On a large \
                             table this may time out."
                        },
                        next_action: if sqlite {
                            "Give the column a constant DEFAULT, or add it as nullable and \
                             backfill. SQLite cannot add the NOT NULL constraint afterwards \
                             (no ALTER COLUMN) — that needs a table rebuild."
                        } else {
                            "Provide a constant DEFAULT value, or add the column as \
                             nullable first, backfill existing rows, then add the NOT \
                             NULL constraint in a later migration."
                        },
                    });
                    break; // one finding per statement is sufficient
                } else if !sqlite && has_volatile_default {
                    findings.push(SafetyFinding {
                        operation: "ADD COLUMN NOT NULL (volatile default)".to_owned(),
                        risk: RiskLevel::PotentiallyBlocking,
                        why: "A volatile function-call DEFAULT (e.g. random(), gen_random_uuid()) \
                              is evaluated per-row: Postgres cannot use the PG11+ fast-constant \
                              path and must rewrite the entire table under an exclusive lock.",
                        next_action: "Use a constant literal DEFAULT instead (e.g. DEFAULT 0, \
                                      DEFAULT ''), or add the column nullable, backfill, then \
                                      add the NOT NULL constraint in a later migration.",
                    });
                    break; // one finding per statement is sufficient
                }
            }
        }
    }

    // ADD COLUMN with inline UNIQUE — implicitly builds a non-concurrent unique index
    // ADD COLUMN with inline REFERENCES — scans existing rows to validate the FK
    if normalized.starts_with("alter table") {
        for subcommand in alter_table_subcommands(normalized) {
            if !subcommand.starts_with("add column ") {
                continue;
            }
            if has_inline_unique_constraint(&without_string_literals(subcommand)) {
                findings.push(if sqlite {
                    // SQLite: "Cannot add a UNIQUE column".
                    SafetyFinding {
                        operation: "ADD COLUMN UNIQUE (unsupported on SQLite)".to_owned(),
                        risk: RiskLevel::Unsupported,
                        why: "SQLite rejects ALTER TABLE … ADD COLUMN with an inline UNIQUE \
                              constraint — the statement fails at apply time.",
                        next_action: "Add the column without UNIQUE, then CREATE UNIQUE INDEX on \
                                      it in the same migration.",
                    }
                } else {
                    SafetyFinding {
                        operation: "ADD COLUMN UNIQUE (inline constraint)".to_owned(),
                        risk: RiskLevel::PotentiallyBlocking,
                        why: "An inline UNIQUE constraint implicitly builds a non-concurrent \
                              unique index under an exclusive table lock, blocking all reads and \
                              writes during the build.",
                        next_action: "Add the column without UNIQUE first, then create the unique \
                                      index in a separate migration using \
                                      `CREATE UNIQUE INDEX CONCURRENTLY`.",
                    }
                });
            }
            if sqlite && subcommand.contains(" primary key") {
                findings.push(SafetyFinding {
                    operation: "ADD COLUMN PRIMARY KEY (unsupported on SQLite)".to_owned(),
                    risk: RiskLevel::Unsupported,
                    why: "SQLite rejects ALTER TABLE … ADD COLUMN with an inline PRIMARY KEY \
                          constraint — the statement fails at apply time.",
                    next_action: "Rebuild the table with the new primary key \
                                  (`autumn schema diff --write-migration` emits the rebuild).",
                });
            }
            // SQLite accepts an added REFERENCES column but requires its default
            // to be NULL, and never scans existing rows to validate the key — so
            // the Postgres row-validation finding does not apply there.
            if sqlite {
                // A generated column is the one ADD COLUMN form left; SQLite
                // accepts VIRTUAL and rejects STORED.
                if !fresh_table && subcommand.contains(" stored") && subcommand.contains(" as (") {
                    findings.push(SafetyFinding {
                        operation: "ADD COLUMN GENERATED … STORED (unsupported on SQLite)"
                            .to_owned(),
                        risk: RiskLevel::Unsupported,
                        why: "SQLite cannot add a STORED generated column to an existing table \
                              — the statement fails at apply time. VIRTUAL is accepted.",
                        next_action: "Use a VIRTUAL generated column, or rebuild the table with \
                                      the STORED column in its CREATE TABLE.",
                    });
                }
                // SQLite's ADD COLUMN default must reduce to a constant.
                // NOTE: no rule for `REFERENCES` with a non-NULL default. SQLite
                // enforces that only when `PRAGMA foreign_keys` is ON, and
                // Autumn's migration connection deliberately leaves it OFF (see
                // `establish_sqlite_migration_connection`), so the statement does
                // apply on the path this classifier grades.
                if !fresh_table
                    && let Some(default) = add_column_default_token(subcommand)
                    && !is_sqlite_constant_default(&default)
                {
                    findings.push(SafetyFinding {
                        operation: "ADD COLUMN (non-constant default)".to_owned(),
                        risk: RiskLevel::Unsupported,
                        why: "SQLite's ADD COLUMN default must reduce to a constant: \
                              CURRENT_TIME/CURRENT_DATE/CURRENT_TIMESTAMP, a call, and any other \
                              expression are rejected at apply time.",
                        next_action: "Use a literal DEFAULT, or add the column nullable and \
                                      backfill it with an UPDATE.",
                    });
                }
            } else if subcommand.contains(" references ") {
                findings.push(SafetyFinding {
                    operation: "ADD COLUMN REFERENCES (inline FK)".to_owned(),
                    risk: RiskLevel::PotentiallyBlocking,
                    why: "An inline REFERENCES constraint scans all existing rows to validate the \
                          foreign key, acquiring locks that can block writes on the referenced \
                          table.",
                    next_action: "Add the column without the constraint first, then add the FK \
                                  using `ADD CONSTRAINT ... FOREIGN KEY ... NOT VALID` and \
                                  validate separately with `VALIDATE CONSTRAINT`.",
                });
            }
        }
    }

    // Unclassified ALTER TABLE subcommand — fires when any subcommand in the statement
    // is not covered by the specific rules above. Checking all subcommands individually
    // prevents a known-safe subcommand (e.g. ADD COLUMN) from hiding an unknown one
    // (e.g. DROP CONSTRAINT) in the same multi-action ALTER TABLE.
    if normalized.starts_with("alter table") {
        let subcommands = alter_table_subcommands(normalized);
        let all_known = subcommands
            .iter()
            .all(|s| is_known_alter_subcommand(backend, s));
        if !all_known {
            findings.push(SafetyFinding {
                operation: "Unclassified ALTER TABLE".to_owned(),
                risk: RiskLevel::ManualReview,
                why: "Autumn cannot automatically assess the safety of this ALTER TABLE \
                      subcommand for a rolling deploy. Some operations (e.g. DROP CONSTRAINT, \
                      ALTER COLUMN SET NOT NULL, ADD CONSTRAINT) acquire table locks or validate \
                      existing rows.",
                next_action: "Review the statement manually. If it is safe, you may suppress \
                              this finding by adding `-- autumn-safety: reviewed` above the \
                              statement.",
            });
        }
    }

    // CREATE INDEX / CREATE UNIQUE INDEX without CONCURRENTLY
    let is_create_index =
        normalized.starts_with("create index") || normalized.starts_with("create unique index");
    // The trailing space matters: `concurrently` must be the SQL option, not the
    // start of the index's name (`CREATE INDEX concurrently_idx ON …`).
    let is_concurrent = normalized.starts_with("create index concurrently ")
        || normalized.starts_with("create unique index concurrently ");
    if is_create_index && !is_concurrent {
        findings.push(SafetyFinding {
            operation: if sqlite {
                CREATE_INDEX_SQLITE_OP
            } else {
                CREATE_INDEX_PG_OP
            }
            .to_owned(),
            risk: RiskLevel::PotentiallyBlocking,
            why: if sqlite {
                "Building an index takes SQLite's single write lock for the whole build. \
                 Writers block until it finishes; under WAL, readers do not."
            } else {
                "Non-concurrent index creation holds an exclusive table lock for the entire \
                 build, blocking all reads and writes."
            },
            next_action: if sqlite {
                "SQLite has no online index build. Build the index during a low-traffic window, \
                 and keep the migration's transaction short."
            } else {
                "Use CREATE INDEX CONCURRENTLY instead. Note: concurrent index \
                 creation cannot run inside a transaction block."
            },
        });
    }

    // Data backfill — bulk DML inside a migration requires a separate job
    if normalized.starts_with("update ")
        || normalized.starts_with("insert into ")
        || normalized.starts_with("delete from ")
        || normalized.starts_with("merge into ")
    {
        findings.push(SafetyFinding {
            operation: "Bulk DML (data backfill)".to_owned(),
            risk: RiskLevel::DataBackfill,
            why: "Running bulk UPDATE or INSERT inside a migration locks rows for the duration \
                  of the transaction. On large tables this can time out or block application \
                  traffic for seconds to minutes.",
            next_action: "Run the data backfill as a separate idempotent background job or \
                          one-off task (`autumn task`) after the schema migration has deployed. \
                          Add a NOT VALID constraint first if you need the constraint enforced \
                          before the backfill completes.",
        });
    }

    // CTE-prefixed bulk DML — WITH … UPDATE / DELETE / INSERT
    // A CTE starts with `with` so the plain DML checks above don't fire.
    // Check both the outer statement (`) update/delete/insert`) and CTE bodies
    // (`(update/delete/insert`) to catch data-modifying CTEs followed by SELECT.
    if normalized.starts_with("with ")
        && (normalized.contains(") update ")
            || normalized.contains(") delete ")
            || normalized.contains(") insert into ")
            || normalized.contains("(update ")
            || normalized.contains("(delete ")
            || normalized.contains("(insert into "))
    {
        findings.push(SafetyFinding {
            operation: "Bulk DML (data backfill via CTE)".to_owned(),
            risk: RiskLevel::DataBackfill,
            why: "A CTE that writes (UPDATE, DELETE, INSERT) locks rows for the duration of the \
                  transaction. On large tables this can time out or block application traffic.",
            next_action: "Run the data backfill as a separate idempotent background job or \
                          one-off task (`autumn task`) after the schema migration has deployed.",
        });
    }

    // DROP INDEX (non-concurrent) — holds an exclusive table lock
    // Use token-aware check: `concurrently` must be the SQL option immediately after
    // `drop index`, not a substring of the index name (e.g. idx_concurrently).
    // Postgres only: on SQLite a DROP INDEX is a cheap catalog edit, and the
    // generated SQLite `DROP COLUMN` path is *required* to emit one first —
    // flagging it would fail the deploy gate on every such migration.
    if !sqlite
        && normalized.starts_with("drop index")
        && !normalized.starts_with("drop index concurrently ")
    {
        findings.push(SafetyFinding {
            operation: "DROP INDEX (non-concurrent)".to_owned(),
            risk: RiskLevel::PotentiallyBlocking,
            why: "Non-concurrent DROP INDEX acquires an AccessExclusiveLock on the table, \
                  blocking all reads and writes for the duration of the drop.",
            next_action: "Use DROP INDEX CONCURRENTLY to avoid the exclusive table lock. \
                          Add `run_in_transaction = false` to the migration's `metadata.toml`.",
        });
    }

    // ALTER TYPE RENAME VALUE — renaming an enum label breaks old replicas that
    // still INSERT or compare against the old label during a rolling deploy.
    if !sqlite && normalized.starts_with("alter type") && normalized.contains(" rename value ") {
        findings.push(SafetyFinding {
            operation: "ALTER TYPE RENAME VALUE".to_owned(),
            risk: RiskLevel::Irreversible,
            why: "Renaming an enum label breaks old replicas that still insert, compare, or \
                  decode the old label. Errors will appear immediately during a rolling deploy.",
            next_action: "Use expand/contract: add a new enum value, migrate all writes to use \
                          it, then remove the old value in a subsequent release.",
        });
        return findings;
    }

    // ALTER TYPE RENAME TO — renaming the type itself breaks references in old replicas.
    if !sqlite && normalized.starts_with("alter type") && normalized.contains(" rename to ") {
        findings.push(SafetyFinding {
            operation: "ALTER TYPE RENAME".to_owned(),
            risk: RiskLevel::Irreversible,
            why: "Renaming a type breaks all references to its old name in old replicas, \
                  causing errors during a rolling deploy.",
            next_action: "Coordinate a maintenance window or use expand/contract by creating \
                          the new type, migrating columns/code, then dropping the old one.",
        });
        return findings;
    }

    // Generic catch-all — DDL/DML not matched by any rule above
    let is_known_start = normalized.starts_with("drop table")
        || normalized.starts_with("drop index")
        || normalized.starts_with("alter table") // unclassified subcommands handled above
        || normalized.starts_with("create table")
        || normalized.starts_with("create index")
        || normalized.starts_with("create unique index")
        || normalized.starts_with("update ")
        || normalized.starts_with("insert into ")
        || normalized.starts_with("delete from ")
        || normalized.starts_with("merge into ")
        || normalized.starts_with("truncate ")
        || normalized.starts_with("comment on")
        || normalized.starts_with("create sequence")
        || normalized.starts_with("alter sequence")
        || normalized.starts_with("drop sequence")
        || normalized.starts_with("create type")
        // `alter type` is intentionally absent — unclassified forms fall through to ManualReview
        // `drop type` is intentionally absent — falls through to ManualReview
        || normalized.starts_with("create extension")
        || normalized.starts_with("create view")
        || normalized.starts_with("drop view")
        || normalized.starts_with("select ");

    let starts_with_ddl_keyword = normalized.starts_with("create ")
        || normalized.starts_with("drop ")
        || normalized.starts_with("alter ")
        || normalized.starts_with("truncate ")
        || normalized.starts_with("grant ")
        || normalized.starts_with("revoke ");

    if starts_with_ddl_keyword && !is_known_start {
        findings.push(SafetyFinding {
            operation: "Unclassified DDL".to_owned(),
            risk: RiskLevel::ManualReview,
            why: "Autumn cannot automatically assess the safety of this statement for a rolling \
                  deploy. Operator review is required before applying this migration in \
                  production.",
            next_action: "Review the statement manually. If it is safe, you may suppress this \
                          finding by adding `-- autumn-safety: reviewed` above the statement.",
        });
    }

    findings
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── RiskLevel ordering ────────────────────────────────────────────────────

    #[test]
    fn risk_level_ordering() {
        assert!(RiskLevel::Safe < RiskLevel::PotentiallyBlocking);
        assert!(RiskLevel::PotentiallyBlocking < RiskLevel::Destructive);
        assert!(RiskLevel::Destructive < RiskLevel::Irreversible);
        assert!(RiskLevel::Irreversible < RiskLevel::DataBackfill);
        assert!(RiskLevel::DataBackfill < RiskLevel::ManualReview);
    }

    #[test]
    fn risk_level_display() {
        assert_eq!(RiskLevel::Safe.to_string(), "safe");
        assert_eq!(
            RiskLevel::PotentiallyBlocking.to_string(),
            "potentially-blocking"
        );
        assert_eq!(RiskLevel::Destructive.to_string(), "destructive");
        assert_eq!(RiskLevel::Irreversible.to_string(), "irreversible");
        assert_eq!(RiskLevel::DataBackfill.to_string(), "data-backfill");
        assert_eq!(RiskLevel::ManualReview.to_string(), "manual-review");
    }

    // ── safe migrations ───────────────────────────────────────────────────────

    #[test]
    fn empty_sql_has_no_findings() {
        assert!(classify_sql("").is_empty());
    }

    #[test]
    fn create_table_is_safe() {
        let sql = "CREATE TABLE posts (\n id BIGSERIAL PRIMARY KEY,\n title TEXT NOT NULL\n);";
        let findings = classify_sql(sql);
        assert!(
            findings.is_empty(),
            "CREATE TABLE should be safe: {findings:?}"
        );
    }

    #[test]
    fn add_nullable_column_is_safe() {
        let sql = "ALTER TABLE posts ADD COLUMN subtitle TEXT NULL;";
        let findings = classify_sql(sql);
        assert!(
            findings.is_empty(),
            "ADD COLUMN NULL should be safe: {findings:?}"
        );
    }

    #[test]
    fn add_not_null_column_with_default_is_safe() {
        let sql = "ALTER TABLE posts ADD COLUMN status TEXT NOT NULL DEFAULT 'draft';";
        let findings = classify_sql(sql);
        assert!(
            findings.is_empty(),
            "ADD COLUMN NOT NULL DEFAULT should be safe: {findings:?}"
        );
    }

    #[test]
    fn add_not_null_column_name_containing_default_is_blocking() {
        // Column named `defaulted_at` must not be mistaken for having a DEFAULT clause.
        let sql = "ALTER TABLE posts ADD COLUMN defaulted_at TIMESTAMP NOT NULL;";
        let findings = classify_sql(sql);
        assert_eq!(
            findings.len(),
            1,
            "column name containing 'default' must not suppress finding"
        );
        assert_eq!(findings[0].risk, RiskLevel::PotentiallyBlocking);
    }

    #[test]
    fn create_concurrent_index_is_safe() {
        let sql = "CREATE INDEX CONCURRENTLY idx_posts_title ON posts (title);";
        let findings = classify_sql(sql);
        assert!(
            findings.is_empty(),
            "CREATE INDEX CONCURRENTLY should be safe: {findings:?}"
        );
    }

    #[test]
    fn create_unique_index_concurrently_is_safe() {
        let sql = "CREATE UNIQUE INDEX CONCURRENTLY idx_posts_slug ON posts (slug);";
        let findings = classify_sql(sql);
        assert!(
            findings.is_empty(),
            "CREATE UNIQUE INDEX CONCURRENTLY should be safe: {findings:?}"
        );
    }

    // ── destructive patterns ──────────────────────────────────────────────────

    #[test]
    fn drop_view_is_destructive() {
        let findings = classify_sql("DROP VIEW active_posts;");
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].risk, RiskLevel::Destructive);
        assert_eq!(findings[0].operation, "DROP VIEW");
    }

    #[test]
    fn drop_view_cascade_is_destructive() {
        let findings = classify_sql("DROP VIEW active_posts CASCADE;");
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].risk, RiskLevel::Destructive);
    }

    #[test]
    fn drop_table_is_destructive() {
        let findings = classify_sql("DROP TABLE posts;");
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].risk, RiskLevel::Destructive);
        assert_eq!(findings[0].operation, "DROP TABLE");
    }

    #[test]
    fn drop_sequence_is_destructive() {
        let findings = classify_sql("DROP SEQUENCE posts_id_seq;");
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].risk, RiskLevel::Destructive);
        assert_eq!(findings[0].operation, "DROP SEQUENCE");
    }

    #[test]
    fn drop_sequence_cascade_is_destructive() {
        let findings = classify_sql("DROP SEQUENCE posts_id_seq CASCADE;");
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].risk, RiskLevel::Destructive);
    }

    #[test]
    fn drop_column_is_destructive() {
        let findings = classify_sql("ALTER TABLE posts DROP COLUMN title;");
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].risk, RiskLevel::Destructive);
        assert_eq!(findings[0].operation, "DROP COLUMN");
    }

    #[test]
    fn drop_column_case_insensitive() {
        let findings = classify_sql("alter table posts drop column title;");
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].risk, RiskLevel::Destructive);
    }

    #[test]
    fn alter_column_type_is_destructive() {
        let findings = classify_sql("ALTER TABLE posts ALTER COLUMN score TYPE BIGINT;");
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].risk, RiskLevel::Destructive);
        assert_eq!(findings[0].operation, "ALTER COLUMN TYPE");
    }

    // ── irreversible patterns ─────────────────────────────────────────────────

    #[test]
    fn rename_column_is_irreversible() {
        let findings = classify_sql("ALTER TABLE posts RENAME COLUMN body TO content;");
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].risk, RiskLevel::Irreversible);
        assert_eq!(findings[0].operation, "RENAME COLUMN");
    }

    #[test]
    fn rename_table_is_irreversible() {
        let findings = classify_sql("ALTER TABLE posts RENAME TO articles;");
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].risk, RiskLevel::Irreversible);
        assert_eq!(findings[0].operation, "RENAME TABLE");
    }

    #[test]
    fn alter_type_rename_value_is_irreversible() {
        let findings = classify_sql("ALTER TYPE status RENAME VALUE 'draft' TO 'pending';");
        assert_eq!(
            findings.len(),
            1,
            "ALTER TYPE RENAME VALUE must be flagged: {findings:?}"
        );
        assert_eq!(findings[0].risk, RiskLevel::Irreversible);
        assert_eq!(findings[0].operation, "ALTER TYPE RENAME VALUE");
    }

    #[test]
    fn alter_type_rename_to_is_irreversible() {
        let findings = classify_sql("ALTER TYPE order_status RENAME TO status;");
        assert_eq!(
            findings.len(),
            1,
            "ALTER TYPE RENAME TO must be flagged: {findings:?}"
        );
        assert_eq!(findings[0].risk, RiskLevel::Irreversible);
        assert_eq!(findings[0].operation, "ALTER TYPE RENAME");
    }

    #[test]
    fn alter_type_add_value_requires_manual_review() {
        // ADD VALUE is not specifically classified — operator must review.
        let findings = classify_sql("ALTER TYPE status ADD VALUE 'archived';");
        assert_eq!(
            findings.len(),
            1,
            "unclassified ALTER TYPE must require manual review: {findings:?}"
        );
        assert_eq!(findings[0].risk, RiskLevel::ManualReview);
    }

    #[test]
    fn rename_constraint_requires_manual_review() {
        // RENAME CONSTRAINT is a schema change that Autumn cannot auto-classify —
        // it must not silently pass as safe.
        let findings = classify_sql("ALTER TABLE users RENAME CONSTRAINT old_name TO new_name;");
        assert_eq!(
            findings.len(),
            1,
            "RENAME CONSTRAINT must not silently pass: {findings:?}"
        );
        assert_eq!(findings[0].risk, RiskLevel::ManualReview);
        assert_eq!(findings[0].operation, "Unclassified ALTER TABLE");
    }

    // ── potentially blocking patterns ─────────────────────────────────────────

    #[test]
    fn multi_column_add_only_flags_clause_without_default() {
        // ADD COLUMN score has DEFAULT — ADD COLUMN slug does NOT. Only slug should be flagged.
        let sql = "ALTER TABLE posts \
                   ADD COLUMN score INT NOT NULL DEFAULT 0, \
                   ADD COLUMN slug TEXT NOT NULL;";
        let findings = classify_sql(sql);
        assert_eq!(
            findings.len(),
            1,
            "only the column without a DEFAULT should be flagged"
        );
        assert_eq!(findings[0].risk, RiskLevel::PotentiallyBlocking);
    }

    #[test]
    fn mixed_known_and_unknown_alter_table_subcommands_flagged() {
        // ADD COLUMN is safe, but DROP CONSTRAINT is unclassified — should get ManualReview.
        let sql = "ALTER TABLE posts ADD COLUMN subtitle TEXT, DROP CONSTRAINT posts_title_key;";
        let findings = classify_sql(sql);
        assert!(
            findings.iter().any(|f| f.risk == RiskLevel::ManualReview),
            "unknown subcommand must produce ManualReview: {findings:?}"
        );
    }

    #[test]
    fn add_not_null_column_without_default_is_blocking() {
        let findings = classify_sql("ALTER TABLE posts ADD COLUMN score INTEGER NOT NULL;");
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].risk, RiskLevel::PotentiallyBlocking);
        assert_eq!(findings[0].operation, "ADD COLUMN NOT NULL (no default)");
    }

    #[test]
    fn add_not_null_column_with_volatile_default_is_blocking() {
        let findings = classify_sql(
            "ALTER TABLE posts ADD COLUMN token UUID NOT NULL DEFAULT gen_random_uuid();",
        );
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].risk, RiskLevel::PotentiallyBlocking);
        assert_eq!(
            findings[0].operation,
            "ADD COLUMN NOT NULL (volatile default)"
        );
    }

    #[test]
    fn add_not_null_column_with_now_default_is_safe() {
        // now() is STABLE: Postgres evaluates it once at statement time and stores the
        // constant, so the PG11+ fast-default path applies — no table rewrite needed.
        let findings = classify_sql(
            "ALTER TABLE posts ADD COLUMN created_at TIMESTAMP NOT NULL DEFAULT now();",
        );
        assert!(
            findings.is_empty(),
            "DEFAULT now() is stable and must not be flagged as volatile: {findings:?}"
        );
    }

    #[test]
    fn add_not_null_column_with_parenthesized_constant_default_is_safe() {
        // `DEFAULT (0)` and `DEFAULT ('draft')` are parenthesized constants, not function
        // calls.  They use the PG11+ fast-default path and must not be flagged as volatile.
        let findings_int =
            classify_sql("ALTER TABLE posts ADD COLUMN score INT NOT NULL DEFAULT (0);");
        assert!(
            findings_int.is_empty(),
            "DEFAULT (0) must be safe: {findings_int:?}"
        );
        let findings_str =
            classify_sql("ALTER TABLE posts ADD COLUMN status TEXT NOT NULL DEFAULT ('draft');");
        assert!(
            findings_str.is_empty(),
            "DEFAULT ('draft') must be safe: {findings_str:?}"
        );
    }

    #[test]
    fn add_not_null_column_with_constant_default_is_safe() {
        // Constant literals use the PG11+ fast path — no table rewrite.
        let findings =
            classify_sql("ALTER TABLE posts ADD COLUMN active BOOLEAN NOT NULL DEFAULT false;");
        assert!(
            findings.is_empty(),
            "constant DEFAULT false must be safe: {findings:?}"
        );
    }

    #[test]
    fn add_column_with_inline_unique_is_potentially_blocking() {
        let findings = classify_sql("ALTER TABLE posts ADD COLUMN slug TEXT UNIQUE;");
        assert_eq!(
            findings.len(),
            1,
            "inline UNIQUE must be flagged: {findings:?}"
        );
        assert_eq!(findings[0].risk, RiskLevel::PotentiallyBlocking);
        assert_eq!(
            findings[0].operation,
            "ADD COLUMN UNIQUE (inline constraint)"
        );
    }

    #[test]
    fn add_column_with_inline_references_is_potentially_blocking() {
        let findings =
            classify_sql("ALTER TABLE posts ADD COLUMN user_id INT REFERENCES users(id);");
        assert_eq!(
            findings.len(),
            1,
            "inline REFERENCES must be flagged: {findings:?}"
        );
        assert_eq!(findings[0].risk, RiskLevel::PotentiallyBlocking);
        assert_eq!(findings[0].operation, "ADD COLUMN REFERENCES (inline FK)");
    }

    #[test]
    fn add_column_with_primary_key_requires_manual_review() {
        // PRIMARY KEY inline on ADD COLUMN is not specifically classified.
        let findings = classify_sql("ALTER TABLE posts ADD COLUMN id BIGSERIAL PRIMARY KEY;");
        assert!(
            findings.iter().any(|f| f.risk == RiskLevel::ManualReview),
            "inline PRIMARY KEY must require manual review: {findings:?}"
        );
    }

    #[test]
    fn add_column_without_constraints_does_not_trigger_manual_review() {
        let findings = classify_sql("ALTER TABLE posts ADD COLUMN subtitle TEXT;");
        assert!(
            findings.iter().all(|f| f.risk != RiskLevel::ManualReview),
            "simple ADD COLUMN must not trigger ManualReview: {findings:?}"
        );
    }

    #[test]
    fn create_non_concurrent_index_is_blocking() {
        let findings = classify_sql("CREATE INDEX idx_posts_title ON posts (title);");
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].risk, RiskLevel::PotentiallyBlocking);
        assert_eq!(findings[0].operation, "CREATE INDEX (non-concurrent)");
    }

    #[test]
    fn create_unique_index_without_concurrently_is_blocking() {
        let findings = classify_sql("CREATE UNIQUE INDEX idx_posts_slug ON posts (slug);");
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].risk, RiskLevel::PotentiallyBlocking);
        assert_eq!(findings[0].operation, "CREATE INDEX (non-concurrent)");
    }

    #[test]
    fn non_concurrent_index_on_newly_created_table_is_safe() {
        // The table is created in the same migration — no existing rows to lock.
        // This is the shape emitted by `autumn generate ... --index`.
        let sql = "CREATE TABLE posts (id BIGSERIAL PRIMARY KEY, title TEXT NOT NULL);\n\
                   CREATE INDEX idx_posts_title ON posts (title);";
        let findings = classify_sql(sql);
        assert!(
            findings.is_empty(),
            "non-concurrent index on a table created in the same migration must be safe: \
             {findings:?}"
        );
    }

    #[test]
    fn unique_non_concurrent_index_on_newly_created_table_is_safe() {
        let sql = "CREATE TABLE posts (id BIGSERIAL PRIMARY KEY, slug TEXT NOT NULL);\n\
                   CREATE UNIQUE INDEX idx_posts_slug ON posts (slug);";
        let findings = classify_sql(sql);
        assert!(
            findings.is_empty(),
            "non-concurrent unique index on a newly created table must be safe: {findings:?}"
        );
    }

    #[test]
    fn non_concurrent_index_on_different_table_is_still_blocking() {
        // CREATE TABLE `posts` does not exempt an index on a different table `comments`.
        let sql = "CREATE TABLE posts (id BIGSERIAL PRIMARY KEY);\n\
                   CREATE INDEX idx_comments_post_id ON comments (post_id);";
        let findings = classify_sql(sql);
        assert_eq!(
            findings.len(),
            1,
            "non-concurrent index on a pre-existing table must still be flagged: {findings:?}"
        );
        assert_eq!(findings[0].risk, RiskLevel::PotentiallyBlocking);
    }

    #[test]
    fn if_not_exists_table_not_treated_as_newly_created() {
        let sql = "CREATE TABLE IF NOT EXISTS posts (id BIGSERIAL PRIMARY KEY, title TEXT NOT NULL);\n\
                   CREATE INDEX idx_posts_title ON posts (title);";
        let findings = classify_sql(sql);
        assert_eq!(
            findings.len(),
            1,
            "non-concurrent index on IF NOT EXISTS table must still be flagged"
        );
        assert_eq!(findings[0].risk, RiskLevel::PotentiallyBlocking);
    }

    #[test]
    fn truncate_table_is_destructive() {
        let findings = classify_sql("TRUNCATE TABLE posts;");
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].risk, RiskLevel::Destructive);
        assert_eq!(findings[0].operation, "TRUNCATE");
    }

    // ── multi-statement SQL ───────────────────────────────────────────────────

    #[test]
    fn multiple_safe_statements_produce_no_findings() {
        let sql = "\
            ALTER TABLE posts ADD COLUMN subtitle TEXT NULL;\n\
            CREATE INDEX CONCURRENTLY idx_posts_subtitle ON posts (subtitle);";
        let findings = classify_sql(sql);
        assert!(findings.is_empty());
    }

    #[test]
    fn mixed_safe_and_unsafe_statements_produces_findings_for_unsafe() {
        let sql = "\
            ALTER TABLE posts ADD COLUMN subtitle TEXT NULL;\n\
            ALTER TABLE posts DROP COLUMN body;";
        let findings = classify_sql(sql);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].risk, RiskLevel::Destructive);
    }

    #[test]
    fn multiple_unsafe_statements_produce_multiple_findings() {
        let sql = "\
            ALTER TABLE posts DROP COLUMN body;\n\
            CREATE INDEX idx_posts_title ON posts (title);";
        let findings = classify_sql(sql);
        assert_eq!(findings.len(), 2);
        assert!(findings.iter().any(|f| f.risk == RiskLevel::Destructive));
        assert!(
            findings
                .iter()
                .any(|f| f.risk == RiskLevel::PotentiallyBlocking)
        );
    }

    // ── line comments are ignored ─────────────────────────────────────────────

    #[test]
    fn sql_with_line_comments_is_classified_correctly() {
        let sql = "-- Removing old column\nALTER TABLE posts DROP COLUMN body;";
        let findings = classify_sql(sql);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].risk, RiskLevel::Destructive);
    }

    #[test]
    fn line_comment_with_semicolon_does_not_hide_following_statement() {
        // A `;` inside a `--` comment must not be treated as a statement separator.
        // Before the fix, `-- rollout complete; safe\nDROP TABLE posts` would be split
        // and the fragment `safe\nDROP TABLE posts` no longer starts with `drop table`.
        let sql = "-- rollout complete; safe to proceed\nDROP TABLE posts;";
        let findings = classify_sql(sql);
        assert_eq!(
            findings.len(),
            1,
            "DROP TABLE must be found after a line comment containing ';': {findings:?}"
        );
        assert_eq!(findings[0].risk, RiskLevel::Destructive);
    }

    #[test]
    fn autumn_safety_comment_does_not_double_classify() {
        // Autumn-generated SQL includes a leading safety comment; ensure the
        // comment text itself doesn't trigger a second finding.
        let sql = "-- autumn-safety: destructive\nALTER TABLE posts DROP COLUMN body;";
        let findings = classify_sql(sql);
        assert_eq!(findings.len(), 1);
    }

    #[test]
    fn autumn_safety_reviewed_suppresses_manual_review_finding() {
        // Operator acknowledges a CREATE FUNCTION is safe for their deploy.
        let sql = "-- autumn-safety: reviewed\nCREATE FUNCTION noop() RETURNS void LANGUAGE sql AS $$SELECT 1$$;";
        let findings = classify_sql(sql);
        assert!(
            findings.is_empty(),
            "reviewed marker must suppress ManualReview finding: {findings:?}"
        );
    }

    #[test]
    fn autumn_safety_reviewed_suppresses_unclassified_alter_table() {
        let sql = "-- autumn-safety: reviewed\nALTER TABLE users DROP CONSTRAINT users_email_key;";
        let findings = classify_sql(sql);
        assert!(
            findings.is_empty(),
            "reviewed marker must suppress finding: {findings:?}"
        );
    }

    #[test]
    fn autumn_safety_destructive_does_not_suppress() {
        // Only the `reviewed` marker suppresses; other autumn-safety values are informational.
        let sql = "-- autumn-safety: destructive\nALTER TABLE posts DROP COLUMN body;";
        let findings = classify_sql(sql);
        assert_eq!(
            findings.len(),
            1,
            "non-reviewed marker must not suppress findings"
        );
    }

    // ── helper predicates ─────────────────────────────────────────────────────

    #[test]
    fn is_safe_returns_true_for_empty() {
        assert!(is_safe(&[]));
    }

    #[test]
    fn is_safe_returns_false_for_unsafe_findings() {
        let f = SafetyFinding {
            operation: "DROP COLUMN".to_owned(),
            risk: RiskLevel::Destructive,
            why: "",
            next_action: "",
        };
        assert!(!is_safe(&[f]));
    }

    #[test]
    fn has_unsafe_findings_returns_false_for_empty() {
        assert!(!has_unsafe_findings(&[]));
    }

    #[test]
    fn has_unsafe_findings_returns_true_for_blocking() {
        let f = SafetyFinding {
            operation: "CREATE INDEX (non-concurrent)".to_owned(),
            risk: RiskLevel::PotentiallyBlocking,
            why: "",
            next_action: "",
        };
        assert!(has_unsafe_findings(&[f]));
    }

    // ── finding fields carry useful guidance ─────────────────────────────────

    #[test]
    fn drop_column_finding_names_the_risk_and_next_action() {
        let findings = classify_sql("ALTER TABLE posts DROP COLUMN body;");
        let f = &findings[0];
        assert!(
            !f.why.is_empty(),
            "why must explain the rolling-deploy risk"
        );
        assert!(
            !f.next_action.is_empty(),
            "next_action must tell the operator what to do"
        );
    }

    #[test]
    fn non_concurrent_index_finding_mentions_concurrently() {
        let findings = classify_sql("CREATE INDEX idx ON posts (title);");
        let f = &findings[0];
        assert!(
            f.next_action.to_lowercase().contains("concurrently"),
            "next_action should recommend CONCURRENTLY: {}",
            f.next_action
        );
    }

    // ── data backfill patterns ────────────────────────────────────────────────

    #[test]
    fn merge_into_is_data_backfill() {
        let sql = "MERGE INTO posts AS target \
                   USING staging AS src ON target.id = src.id \
                   WHEN MATCHED THEN UPDATE SET title = src.title \
                   WHEN NOT MATCHED THEN INSERT (id, title) VALUES (src.id, src.title);";
        let findings = classify_sql(sql);
        assert_eq!(
            findings.len(),
            1,
            "MERGE INTO must be classified as a data backfill: {findings:?}"
        );
        assert_eq!(findings[0].risk, RiskLevel::DataBackfill);
        assert_eq!(findings[0].operation, "Bulk DML (data backfill)");
    }

    #[test]
    fn bulk_update_is_data_backfill() {
        let findings = classify_sql("UPDATE posts SET status = 'draft' WHERE status IS NULL;");
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].risk, RiskLevel::DataBackfill);
        assert_eq!(findings[0].operation, "Bulk DML (data backfill)");
    }

    #[test]
    fn insert_select_is_data_backfill() {
        let findings =
            classify_sql("INSERT INTO post_tags (post_id, tag) SELECT id, 'untagged' FROM posts;");
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].risk, RiskLevel::DataBackfill);
    }

    #[test]
    fn bulk_delete_is_data_backfill() {
        let findings = classify_sql("DELETE FROM posts WHERE created_at < '2020-01-01';");
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].risk, RiskLevel::DataBackfill);
        assert_eq!(findings[0].operation, "Bulk DML (data backfill)");
    }

    #[test]
    fn data_backfill_finding_recommends_separate_job() {
        let findings = classify_sql("UPDATE posts SET slug = LOWER(title);");
        let f = &findings[0];
        assert!(!f.why.is_empty());
        assert!(
            f.next_action.to_lowercase().contains("background job")
                || f.next_action.to_lowercase().contains("task"),
            "next_action should recommend a separate job or task: {}",
            f.next_action
        );
    }

    #[test]
    fn cte_update_is_data_backfill() {
        let sql = "WITH batch AS (SELECT id FROM posts WHERE status IS NULL LIMIT 1000) \
                   UPDATE posts SET status = 'draft' FROM batch WHERE posts.id = batch.id;";
        let findings = classify_sql(sql);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].risk, RiskLevel::DataBackfill);
        assert_eq!(findings[0].operation, "Bulk DML (data backfill via CTE)");
    }

    #[test]
    fn cte_delete_is_data_backfill() {
        let sql = "WITH doomed AS (SELECT id FROM posts WHERE archived = true) DELETE FROM posts USING doomed WHERE posts.id = doomed.id;";
        let findings = classify_sql(sql);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].risk, RiskLevel::DataBackfill);
    }

    #[test]
    fn cte_select_only_is_safe() {
        // A read-only CTE should not produce a DataBackfill finding.
        let sql = "WITH recent AS (SELECT id FROM posts ORDER BY created_at DESC LIMIT 10) \
                   SELECT * FROM recent;";
        let findings = classify_sql(sql);
        assert!(
            findings.iter().all(|f| f.risk != RiskLevel::DataBackfill),
            "read-only CTE must not produce DataBackfill finding: {findings:?}"
        );
    }

    #[test]
    fn cte_body_write_with_outer_select_is_data_backfill() {
        // data-modifying CTE where the outer statement is SELECT, not UPDATE/DELETE.
        let sql = "WITH touched AS \
                   (UPDATE posts SET migrated = true WHERE migrated IS NULL RETURNING id) \
                   SELECT count(*) FROM touched;";
        let findings = classify_sql(sql);
        assert!(
            findings.iter().any(|f| f.risk == RiskLevel::DataBackfill),
            "data-modifying CTE with outer SELECT must still be flagged: {findings:?}"
        );
    }

    // ── manual review patterns ────────────────────────────────────────────────

    #[test]
    fn create_function_requires_manual_review() {
        let sql = "CREATE FUNCTION update_modified() RETURNS trigger AS $$ BEGIN NEW.updated_at = now(); RETURN NEW; END; $$ LANGUAGE plpgsql;";
        let findings = classify_sql(sql);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].risk, RiskLevel::ManualReview);
        assert_eq!(findings[0].operation, "Unclassified DDL");
    }

    #[test]
    fn truncate_is_destructive() {
        let findings = classify_sql("TRUNCATE TABLE staging_data;");
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].risk, RiskLevel::Destructive);
    }

    #[test]
    fn drop_constraint_requires_manual_review() {
        let findings = classify_sql("ALTER TABLE users DROP CONSTRAINT users_email_key;");
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].risk, RiskLevel::ManualReview);
        assert_eq!(findings[0].operation, "Unclassified ALTER TABLE");
    }

    #[test]
    fn alter_column_set_not_null_requires_manual_review() {
        let findings = classify_sql("ALTER TABLE users ALTER COLUMN email SET NOT NULL;");
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].risk, RiskLevel::ManualReview);
        assert_eq!(findings[0].operation, "Unclassified ALTER TABLE");
    }

    #[test]
    fn known_ddl_does_not_trigger_manual_review() {
        // CREATE TABLE is safe — must not get a ManualReview finding on top
        let findings = classify_sql("CREATE TABLE comments (id BIGSERIAL PRIMARY KEY);");
        assert!(
            findings.iter().all(|f| f.risk != RiskLevel::ManualReview),
            "known DDL should not also produce ManualReview: {findings:?}"
        );
    }

    #[test]
    fn add_column_does_not_trigger_unclassified_alter_table() {
        // A safe ADD COLUMN must not also generate a ManualReview finding.
        let findings = classify_sql("ALTER TABLE posts ADD COLUMN subtitle TEXT NULL;");
        assert!(
            findings.iter().all(|f| f.risk != RiskLevel::ManualReview),
            "safe ADD COLUMN should not produce ManualReview: {findings:?}"
        );
    }

    // ── contains_concurrent_index ─────────────────────────────────────────────

    #[test]
    fn contains_concurrent_index_true_for_executable_statement() {
        assert!(contains_concurrent_index(
            "CREATE INDEX CONCURRENTLY idx ON posts (title);"
        ));
        assert!(contains_concurrent_index(
            "CREATE UNIQUE INDEX CONCURRENTLY idx ON posts (slug);"
        ));
    }

    #[test]
    fn contains_concurrent_index_false_for_non_concurrent() {
        assert!(!contains_concurrent_index(
            "CREATE INDEX idx ON posts (title);"
        ));
    }

    #[test]
    fn contains_concurrent_index_false_for_comment_only_mention() {
        let sql = "-- TODO: use CREATE INDEX CONCURRENTLY later\n\
                   CREATE INDEX idx ON posts (title);";
        assert!(
            !contains_concurrent_index(sql),
            "a CONCURRENTLY reference only in a comment must return false"
        );
    }

    #[test]
    fn contains_concurrent_index_true_for_multiline_statement() {
        let sql = "CREATE INDEX\n  CONCURRENTLY idx_posts_title ON posts (title);";
        assert!(
            contains_concurrent_index(sql),
            "multi-line CONCURRENTLY statement must be detected"
        );
    }

    #[test]
    fn contains_concurrent_index_true_for_drop_index_concurrently() {
        assert!(
            contains_concurrent_index("DROP INDEX CONCURRENTLY idx_posts_title;"),
            "DROP INDEX CONCURRENTLY must be detected"
        );
    }

    // ── block comment stripping ───────────────────────────────────────────────

    #[test]
    fn block_comment_before_drop_table_is_still_classified() {
        let sql = "/* cleanup old table */ DROP TABLE posts;";
        let findings = classify_sql(sql);
        assert_eq!(
            findings.len(),
            1,
            "DROP TABLE must be found after block comment"
        );
        assert_eq!(findings[0].risk, RiskLevel::Destructive);
    }

    #[test]
    fn block_comment_before_create_index_is_still_classified() {
        let sql = "/* needs index */ CREATE INDEX idx ON posts (title);";
        let findings = classify_sql(sql);
        assert_eq!(
            findings.len(),
            1,
            "CREATE INDEX must be found after block comment"
        );
        assert_eq!(findings[0].risk, RiskLevel::PotentiallyBlocking);
    }

    #[test]
    fn multiline_block_comment_is_stripped() {
        let sql = "/*\n * Remove legacy column\n */\nALTER TABLE posts DROP COLUMN body;";
        let findings = classify_sql(sql);
        assert_eq!(
            findings.len(),
            1,
            "DROP COLUMN must be found after multi-line block comment"
        );
        assert_eq!(findings[0].risk, RiskLevel::Destructive);
    }

    #[test]
    fn block_comment_only_mention_of_keyword_is_not_classified() {
        // Only mentions DROP TABLE inside a block comment; actual statement is safe.
        let sql = "/* was: DROP TABLE posts; */ ALTER TABLE posts ADD COLUMN active BOOL;";
        let findings = classify_sql(sql);
        assert!(
            findings.iter().all(|f| f.risk != RiskLevel::Destructive),
            "Destructive keyword inside block comment must not produce a Destructive finding"
        );
    }

    #[test]
    fn block_comment_with_semicolon_does_not_hide_following_statement() {
        // The semicolon inside the block comment must not split the statement early.
        let sql = "/* cleanup; safe to drop */ DROP TABLE posts;";
        let findings = classify_sql(sql);
        assert_eq!(
            findings.len(),
            1,
            "DROP TABLE after a block comment containing ';' must still be classified"
        );
        assert_eq!(findings[0].risk, RiskLevel::Destructive);
    }

    #[test]
    fn block_comment_between_keywords_preserves_token_boundary() {
        // `DROP/* note */TABLE posts` must not concatenate to `DROPTABLE posts`,
        // which would miss both the `drop table` rule and the `drop ` catch-all.
        let findings = classify_sql("DROP/* note */TABLE posts;");
        assert_eq!(
            findings.len(),
            1,
            "block comment between keywords must not merge them: {findings:?}"
        );
        assert_eq!(findings[0].risk, RiskLevel::Destructive);
    }

    // ── dollar-quoted function bodies ─────────────────────────────────────────

    #[test]
    fn dollar_quoted_function_with_semicolons_is_one_statement() {
        // The semicolons inside $$ ... $$ must not produce extra statement fragments.
        let sql = "CREATE FUNCTION bump() RETURNS void AS $$ BEGIN \
                   UPDATE posts SET hits = hits + 1; RETURN; END; $$ LANGUAGE plpgsql;";
        let findings = classify_sql(sql);
        assert_eq!(
            findings.len(),
            1,
            "dollar-quoted body with semicolons must produce exactly one finding: {findings:?}"
        );
        assert_eq!(findings[0].risk, RiskLevel::ManualReview);
    }

    #[test]
    fn autumn_safety_reviewed_suppresses_function_with_dml_in_body() {
        // Without dollar-quote-aware splitting the DML fragment would escape suppression.
        let sql = "-- autumn-safety: reviewed\n\
                   CREATE FUNCTION migrate_posts() RETURNS void AS $$\n\
                   BEGIN\n  UPDATE posts SET migrated = true;\n  RETURN;\nEND;\n\
                   $$ LANGUAGE plpgsql;";
        let findings = classify_sql(sql);
        assert!(
            findings.is_empty(),
            "reviewed marker must suppress a dollar-quoted function containing DML: {findings:?}"
        );
    }

    #[test]
    fn tagged_dollar_quote_with_semicolons_is_one_statement() {
        let sql = "CREATE FUNCTION foo() RETURNS void AS $func$ \
                   BEGIN UPDATE posts SET x = 1; END; $func$ LANGUAGE plpgsql;";
        let findings = classify_sql(sql);
        assert_eq!(
            findings.len(),
            1,
            "tagged dollar-quote body with semicolons must not split: {findings:?}"
        );
    }

    #[test]
    fn split_statements_keeps_semicolon_in_string_literal_intact() {
        // Regression test (PR review, issue #1023): a `DEFAULT 'hello; world'`
        // clause -- exactly what `sql_default_literal` produces for a
        // `--default` value containing a semicolon -- must not be split at
        // the semicolon inside the quotes.
        let sql = "CREATE TABLE posts (\n    \
             id BIGSERIAL PRIMARY KEY,\n    \
             title TEXT NOT NULL DEFAULT 'hello; world',\n    \
             created_at TIMESTAMP NOT NULL DEFAULT NOW()\n\
             );";
        let statements = split_statements(sql);
        assert_eq!(
            statements.len(),
            1,
            "a semicolon inside a single-quoted literal must not split the statement: {statements:?}"
        );
        assert!(statements[0].contains("'hello; world'"));
    }

    #[test]
    fn split_statements_handles_doubled_escaped_quote_in_literal() {
        // `''` is the standard SQL-escaped single quote; a semicolon after one
        // must still be treated as inside the (still-open) literal.
        let sql = "INSERT INTO posts (title) VALUES ('it''s; still one literal');";
        let statements = split_statements(sql);
        assert_eq!(statements.len(), 1, "got: {statements:?}");
    }

    #[test]
    fn split_statements_multiple_string_literals_with_semicolons() {
        let sql = "CREATE TABLE posts (a TEXT DEFAULT 'x;y', b TEXT DEFAULT 'p;q');";
        let statements = split_statements(sql);
        assert_eq!(statements.len(), 1, "got: {statements:?}");
    }

    // ── DROP INDEX ────────────────────────────────────────────────────────────

    #[test]
    fn drop_index_non_concurrent_is_potentially_blocking() {
        let findings = classify_sql("DROP INDEX idx_posts_title;");
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].risk, RiskLevel::PotentiallyBlocking);
        assert_eq!(findings[0].operation, "DROP INDEX (non-concurrent)");
    }

    #[test]
    fn drop_index_with_concurrently_in_name_is_still_blocking() {
        // "concurrently" appears in the index name, not as the SQL token.
        let findings = classify_sql("DROP INDEX idx_concurrently;");
        assert_eq!(
            findings.len(),
            1,
            "index named idx_concurrently must still be flagged as non-concurrent: {findings:?}"
        );
        assert_eq!(findings[0].operation, "DROP INDEX (non-concurrent)");
    }

    #[test]
    fn drop_index_concurrently_is_safe_from_classifier() {
        // CONCURRENTLY avoids the table lock; the opt-out check in migrate.rs handles
        // the metadata.toml requirement separately.
        let findings = classify_sql("DROP INDEX CONCURRENTLY idx_posts_title;");
        assert!(
            findings
                .iter()
                .all(|f| f.risk != RiskLevel::PotentiallyBlocking
                    || f.operation.contains("CONCURRENTLY")),
            "DROP INDEX CONCURRENTLY must not produce a non-concurrent finding"
        );
    }

    // ── DROP TYPE ─────────────────────────────────────────────────────────────

    #[test]
    fn drop_type_requires_manual_review() {
        let findings = classify_sql("DROP TYPE status;");
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].risk, RiskLevel::ManualReview);
    }

    #[test]
    fn drop_type_cascade_requires_manual_review() {
        let findings = classify_sql("DROP TYPE status CASCADE;");
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].risk, RiskLevel::ManualReview);
    }

    // ── has_executable_sql ────────────────────────────────────────────────────

    #[test]
    fn has_executable_sql_empty_string_is_false() {
        assert!(!has_executable_sql(""));
    }

    #[test]
    fn has_executable_sql_whitespace_only_is_false() {
        assert!(!has_executable_sql("   \n\t\n  "));
    }

    #[test]
    fn has_executable_sql_line_comment_only_is_false() {
        assert!(!has_executable_sql("-- nothing here\n-- just comments"));
    }

    #[test]
    fn has_executable_sql_block_comment_only_is_false() {
        assert!(!has_executable_sql("/* block comment only */"));
    }

    #[test]
    fn has_executable_sql_real_sql_is_true() {
        assert!(has_executable_sql("DROP TABLE posts;"));
    }

    #[test]
    fn has_executable_sql_comment_plus_sql_is_true() {
        assert!(has_executable_sql(
            "-- undo the migration\nDROP TABLE posts;"
        ));
    }

    #[test]
    fn has_executable_sql_no_trailing_semicolon_is_true() {
        assert!(has_executable_sql("DROP TABLE posts"));
    }

    // ── SQLite dialect (#1906) ────────────────────────────────────────────────

    /// One finding for `operation`, or `None`.
    fn sqlite_finding(sql: &str, operation: &str) -> Option<SafetyFinding> {
        classify_sql_for(DatabaseBackend::Sqlite, sql)
            .into_iter()
            .find(|f| f.operation == operation)
    }

    #[test]
    fn sqlite_drop_index_is_safe() {
        // SQLite has no `CONCURRENTLY`, and DROP INDEX is a cheap catalog edit.
        // The generated SQLite `DROP COLUMN` path must emit DROP INDEX first, so
        // flagging it would make every such migration fail the deploy gate.
        assert!(
            classify_sql_for(DatabaseBackend::Sqlite, "DROP INDEX idx_posts_title;").is_empty(),
            "SQLite DROP INDEX must not be flagged"
        );
        // Postgres keeps its existing finding.
        assert!(!classify_sql("DROP INDEX idx_posts_title;").is_empty());
    }

    #[test]
    fn sqlite_create_index_advice_never_names_concurrently() {
        let f = sqlite_finding(
            "CREATE INDEX idx_posts_title ON posts (title);",
            "CREATE INDEX",
        )
        .expect("SQLite still reports a blocking index build");
        assert_eq!(f.risk, RiskLevel::PotentiallyBlocking);
        assert!(
            !f.next_action.to_lowercase().contains("concurrently"),
            "SQLite has no CREATE INDEX CONCURRENTLY: {}",
            f.next_action
        );
        assert!(
            !f.why.to_lowercase().contains("postgres"),
            "why must not cite Postgres on SQLite: {}",
            f.why
        );
    }

    #[test]
    fn sqlite_create_index_concurrently_is_unsupported() {
        let f = sqlite_finding(
            "CREATE INDEX CONCURRENTLY idx_posts_title ON posts (title);",
            "CREATE INDEX CONCURRENTLY (unsupported on SQLite)",
        )
        .expect("CONCURRENTLY is not SQLite syntax");
        assert_eq!(f.risk, RiskLevel::Unsupported);
    }

    #[test]
    fn sqlite_add_column_not_null_without_default_is_unsupported() {
        // SQLite rejects the statement outright — it is not merely blocking.
        let f = sqlite_finding(
            "ALTER TABLE posts ADD COLUMN title TEXT NOT NULL;",
            "ADD COLUMN NOT NULL (no default)",
        )
        .expect("finding expected");
        assert_eq!(f.risk, RiskLevel::Unsupported);
        // Postgres keeps the potentially-blocking classification.
        assert_eq!(
            classify_sql("ALTER TABLE posts ADD COLUMN title TEXT NOT NULL;")[0].risk,
            RiskLevel::PotentiallyBlocking
        );
    }

    #[test]
    fn sqlite_add_column_not_null_with_constant_default_is_safe() {
        assert!(
            classify_sql_for(
                DatabaseBackend::Sqlite,
                "ALTER TABLE posts ADD COLUMN views INTEGER NOT NULL DEFAULT 0;"
            )
            .is_empty(),
            "a constant default is the supported SQLite form"
        );
    }

    #[test]
    fn sqlite_add_column_with_expression_default_is_unsupported() {
        // SQLite forbids CURRENT_TIMESTAMP and parenthesized expressions as an
        // ADD COLUMN default.
        for sql in [
            "ALTER TABLE posts ADD COLUMN seen_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP;",
            "ALTER TABLE posts ADD COLUMN slug TEXT NOT NULL DEFAULT (lower(title));",
        ] {
            let f = sqlite_finding(sql, "ADD COLUMN (non-constant default)")
                .unwrap_or_else(|| panic!("finding expected for {sql}"));
            assert_eq!(f.risk, RiskLevel::Unsupported, "{sql}");
        }
    }

    #[test]
    fn sqlite_alter_column_is_unsupported() {
        for sql in [
            "ALTER TABLE posts ALTER COLUMN title TYPE TEXT;",
            "ALTER TABLE posts ALTER COLUMN title SET NOT NULL;",
        ] {
            let f = sqlite_finding(sql, "ALTER COLUMN (unsupported on SQLite)")
                .unwrap_or_else(|| panic!("finding expected for {sql}"));
            assert_eq!(f.risk, RiskLevel::Unsupported, "{sql}");
            assert!(
                f.next_action.contains("rebuild"),
                "must point at the table-rebuild procedure: {}",
                f.next_action
            );
        }
    }

    #[test]
    fn sqlite_add_column_inline_unique_is_unsupported() {
        let f = sqlite_finding(
            "ALTER TABLE posts ADD COLUMN slug TEXT UNIQUE;",
            "ADD COLUMN UNIQUE (unsupported on SQLite)",
        )
        .expect("SQLite rejects ADD COLUMN with UNIQUE");
        assert_eq!(f.risk, RiskLevel::Unsupported);
    }

    #[test]
    fn sqlite_add_column_nullable_references_is_safe() {
        // SQLite does not scan existing rows for a new FK, so the Postgres
        // "validates every row" finding is wrong here.
        assert!(
            classify_sql_for(
                DatabaseBackend::Sqlite,
                "ALTER TABLE comments ADD COLUMN post_id INTEGER REFERENCES posts(id);"
            )
            .is_empty(),
            "a nullable FK column is the supported SQLite form"
        );
    }

    #[test]
    fn sqlite_truncate_is_unsupported() {
        let f = sqlite_finding("TRUNCATE TABLE posts;", "TRUNCATE (unsupported on SQLite)")
            .expect("SQLite has no TRUNCATE statement");
        assert_eq!(f.risk, RiskLevel::Unsupported);
        assert!(f.next_action.contains("DELETE FROM"), "{}", f.next_action);
    }

    #[test]
    fn sqlite_postgres_only_objects_are_unsupported() {
        for sql in [
            "CREATE SEQUENCE post_ids;",
            "ALTER SEQUENCE post_ids RESTART WITH 1;",
            "DROP SEQUENCE post_ids;",
            "CREATE TYPE mood AS ENUM ('ok');",
            "ALTER TYPE mood RENAME VALUE 'ok' TO 'fine';",
            "DROP TYPE mood;",
            "CREATE EXTENSION IF NOT EXISTS pgcrypto;",
            "COMMENT ON TABLE posts IS 'hi';",
            "CREATE MATERIALIZED VIEW mv AS SELECT 1;",
        ] {
            let findings = classify_sql_for(DatabaseBackend::Sqlite, sql);
            assert!(
                findings.iter().any(|f| f.risk == RiskLevel::Unsupported),
                "expected an Unsupported finding for {sql}, got {findings:?}"
            );
        }
    }

    #[test]
    fn sqlite_drop_column_stays_destructive_with_sqlite_guidance() {
        let f = sqlite_finding("ALTER TABLE posts DROP COLUMN title;", "DROP COLUMN")
            .expect("finding expected");
        assert_eq!(f.risk, RiskLevel::Destructive);
        assert!(
            f.next_action.contains("DROP INDEX"),
            "SQLite refuses to drop an indexed column: {}",
            f.next_action
        );
    }

    #[test]
    fn sqlite_unsupported_blocks_the_deploy_gate() {
        let findings = classify_sql_for(
            DatabaseBackend::Sqlite,
            "ALTER TABLE posts ALTER COLUMN a TYPE TEXT;",
        );
        assert!(has_unsafe_findings(&findings));
        assert!(!is_safe(&findings));
    }

    #[test]
    fn unsupported_is_the_highest_risk_level() {
        assert!(RiskLevel::ManualReview < RiskLevel::Unsupported);
    }

    #[test]
    fn classify_sql_still_classifies_as_postgres() {
        // The bare entry point must stay byte-identical to the Postgres path.
        let sql = "ALTER TABLE posts ADD COLUMN title TEXT NOT NULL;\nDROP INDEX idx_posts_a;\n\
                   CREATE INDEX idx_posts_b ON posts (b);";
        let bare = classify_sql(sql);
        let explicit = classify_sql_for(DatabaseBackend::Postgres, sql);
        assert_eq!(bare.len(), explicit.len());
        for (a, b) in bare.iter().zip(explicit.iter()) {
            assert_eq!(a.operation, b.operation);
            assert_eq!(a.risk, b.risk);
            assert_eq!(a.why, b.why);
            assert_eq!(a.next_action, b.next_action);
        }
    }

    // ── review regressions: SQLite dialect (#1906) ────────────────────────

    /// Every operation reported for `sql` on `SQLite`.
    fn sqlite_ops(sql: &str) -> Vec<String> {
        classify_sql_for(DatabaseBackend::Sqlite, sql)
            .into_iter()
            .map(|f| f.operation)
            .collect()
    }

    #[test]
    fn sqlite_add_column_not_null_default_null_is_unsupported() {
        // SQLite drops a literal DEFAULT NULL before the NOT NULL check, so it
        // is not a default there.
        let f = sqlite_finding(
            "ALTER TABLE posts ADD COLUMN title TEXT NOT NULL DEFAULT NULL;",
            "ADD COLUMN NOT NULL (no default)",
        )
        .expect("finding expected");
        assert_eq!(f.risk, RiskLevel::Unsupported);
    }

    #[test]
    fn sqlite_add_column_bare_function_default_is_unsupported() {
        // Only a literal or signed number may follow a bare DEFAULT.
        for sql in [
            "ALTER TABLE posts ADD COLUMN a TEXT NOT NULL DEFAULT now();",
            "ALTER TABLE posts ADD COLUMN b TEXT NOT NULL DEFAULT gen_random_uuid();",
        ] {
            let f = sqlite_finding(sql, "ADD COLUMN (non-constant default)")
                .unwrap_or_else(|| panic!("finding expected for {sql}"));
            assert_eq!(f.risk, RiskLevel::Unsupported, "{sql}");
        }
    }

    #[test]
    fn sqlite_add_column_parenthesized_literal_default_is_accepted() {
        // SQLite's parser keeps no node for parentheses, so `DEFAULT ('draft')`
        // is the same constant as `DEFAULT 'draft'`.
        for sql in [
            "ALTER TABLE posts ADD COLUMN a TEXT NOT NULL DEFAULT ('draft');",
            "ALTER TABLE posts ADD COLUMN b INTEGER NOT NULL DEFAULT (0);",
            "ALTER TABLE posts ADD COLUMN c INTEGER NOT NULL DEFAULT -1;",
            "ALTER TABLE posts ADD COLUMN d TEXT NOT NULL DEFAULT 'a b';",
        ] {
            assert!(
                classify_sql_for(DatabaseBackend::Sqlite, sql).is_empty(),
                "{sql} must be accepted, got {:?}",
                sqlite_ops(sql)
            );
        }
    }

    #[test]
    fn sqlite_reads_a_default_with_no_space_before_the_value() {
        // `DEFAULT(0)` is valid SQL. Missing it made the NOT NULL rule fire and
        // block a statement SQLite applies cleanly.
        assert!(
            classify_sql_for(
                DatabaseBackend::Sqlite,
                "ALTER TABLE posts ADD COLUMN views INTEGER NOT NULL DEFAULT(0);"
            )
            .is_empty(),
            "got {:?}",
            sqlite_ops("ALTER TABLE posts ADD COLUMN views INTEGER NOT NULL DEFAULT(0);")
        );
        // A column whose name merely starts with `default` is not a keyword.
        let f = sqlite_finding(
            "ALTER TABLE posts ADD COLUMN defaulted TEXT NOT NULL;",
            "ADD COLUMN NOT NULL (no default)",
        )
        .expect("finding expected");
        assert_eq!(f.risk, RiskLevel::Unsupported);
    }

    #[test]
    fn sqlite_numeric_defaults_including_exponent_form_are_accepted() {
        for sql in [
            "ALTER TABLE s ADD COLUMN a REAL DEFAULT 1e-3;",
            "ALTER TABLE s ADD COLUMN b REAL DEFAULT -1.5E+10;",
            "ALTER TABLE s ADD COLUMN c REAL DEFAULT .5;",
            "ALTER TABLE s ADD COLUMN d INTEGER DEFAULT 0x1f;",
            "ALTER TABLE s ADD COLUMN e REAL DEFAULT (1e-3);",
        ] {
            assert!(
                classify_sql_for(DatabaseBackend::Sqlite, sql).is_empty(),
                "{sql} is a numeric literal, got {:?}",
                sqlite_ops(sql)
            );
        }
        // Still an expression, not a literal.
        let expr = "ALTER TABLE s ADD COLUMN f REAL DEFAULT 1e-3+1;";
        assert!(
            classify_sql_for(DatabaseBackend::Sqlite, expr)
                .iter()
                .any(|f| f.risk == RiskLevel::Unsupported),
            "{expr} -> {:?}",
            sqlite_ops(expr)
        );
    }

    #[test]
    fn sqlite_add_column_parenthesized_expression_default_is_unsupported() {
        for sql in [
            "ALTER TABLE posts ADD COLUMN a TEXT NOT NULL DEFAULT (1+2);",
            "ALTER TABLE posts ADD COLUMN b TEXT NOT NULL DEFAULT (abs(-1));",
            "ALTER TABLE posts ADD COLUMN c TEXT NOT NULL DEFAULT (datetime('now'));",
        ] {
            let f = sqlite_finding(sql, "ADD COLUMN (non-constant default)")
                .unwrap_or_else(|| panic!("finding expected for {sql}"));
            assert_eq!(f.risk, RiskLevel::Unsupported, "{sql}");
        }
    }

    #[test]
    fn sqlite_add_column_references_applies_without_a_foreign_keys_pragma() {
        // Autumn's migration connection leaves `PRAGMA foreign_keys` OFF, so
        // SQLite accepts a non-NULL default on an added REFERENCES column.
        let sql = "ALTER TABLE comments ADD COLUMN post_id INTEGER NOT NULL DEFAULT 0 \
                   REFERENCES posts(id);";
        assert!(
            classify_sql_for(DatabaseBackend::Sqlite, sql).is_empty(),
            "got {:?}",
            sqlite_ops(sql)
        );
    }

    #[test]
    fn sqlite_fk_action_set_default_is_not_a_column_default() {
        let sql = "ALTER TABLE t ADD COLUMN c INTEGER REFERENCES u(id) \
                   ON UPDATE SET DEFAULT ON DELETE CASCADE;";
        assert!(
            classify_sql_for(DatabaseBackend::Sqlite, sql).is_empty(),
            "got {:?}",
            sqlite_ops(sql)
        );
    }

    #[test]
    fn sqlite_unique_inside_a_string_literal_is_not_a_constraint() {
        let sql = "ALTER TABLE t ADD COLUMN d TEXT DEFAULT 'not unique yet';";
        assert!(
            classify_sql_for(DatabaseBackend::Sqlite, sql).is_empty(),
            "got {:?}",
            sqlite_ops(sql)
        );
    }

    #[test]
    fn sqlite_add_column_generated_stored_is_unsupported() {
        let f = sqlite_finding(
            "ALTER TABLE posts ADD COLUMN n INT GENERATED ALWAYS AS (id + 1) STORED;",
            "ADD COLUMN GENERATED … STORED (unsupported on SQLite)",
        )
        .expect("finding expected");
        assert_eq!(f.risk, RiskLevel::Unsupported);
        // VIRTUAL is accepted.
        assert!(
            classify_sql_for(
                DatabaseBackend::Sqlite,
                "ALTER TABLE posts ADD COLUMN n INT GENERATED ALWAYS AS (id + 1) VIRTUAL;"
            )
            .is_empty()
        );
    }

    #[test]
    fn sqlite_row_dependent_add_column_rules_skip_a_table_this_migration_creates() {
        // Verified against SQLite 3.45: on an EMPTY table these three apply
        // cleanly. UNIQUE and PRIMARY KEY do not — see the test below.
        let sql = "CREATE TABLE posts (id INTEGER PRIMARY KEY);\n\
                   ALTER TABLE posts ADD COLUMN a INT NOT NULL;\n\
                   ALTER TABLE posts ADD COLUMN b TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP;\n\
                   ALTER TABLE posts ADD COLUMN c INT GENERATED ALWAYS AS (id + 1) STORED;";
        assert!(
            !classify_sql_for(DatabaseBackend::Sqlite, sql)
                .iter()
                .any(|f| f.risk == RiskLevel::Unsupported),
            "got {:?}",
            sqlite_ops(sql)
        );
        // On a table the migration does not create, all three are unsupported.
        let existing = "ALTER TABLE posts ADD COLUMN a INT NOT NULL;\n\
                        ALTER TABLE posts ADD COLUMN b TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP;\n\
                        ALTER TABLE posts ADD COLUMN c INT GENERATED ALWAYS AS (id + 1) STORED;";
        assert_eq!(
            classify_sql_for(DatabaseBackend::Sqlite, existing)
                .iter()
                .filter(|f| f.risk == RiskLevel::Unsupported)
                .count(),
            3,
            "got {:?}",
            sqlite_ops(existing)
        );
    }

    #[test]
    fn sqlite_concurrently_must_be_the_keyword_not_a_name_prefix() {
        for sql in [
            "DROP INDEX concurrently_old_idx;",
            "CREATE INDEX concurrently_idx ON posts (x);",
        ] {
            assert!(
                !classify_sql_for(DatabaseBackend::Sqlite, sql)
                    .iter()
                    .any(|f| f.risk == RiskLevel::Unsupported),
                "{sql} names an index, it does not use CONCURRENTLY: {:?}",
                sqlite_ops(sql)
            );
        }
    }

    #[test]
    fn sqlite_rejects_every_alter_table_subcommand_outside_its_grammar() {
        for sql in [
            "ALTER TABLE t ADD CONSTRAINT ck CHECK (id > 0);",
            "ALTER TABLE t DROP CONSTRAINT ck;",
            "ALTER TABLE t SET SCHEMA other;",
            "ALTER TABLE t OWNER TO app;",
        ] {
            let findings = classify_sql_for(DatabaseBackend::Sqlite, sql);
            assert!(
                findings.iter().any(|f| f.risk == RiskLevel::Unsupported),
                "{sql} -> {:?}",
                sqlite_ops(sql)
            );
        }
        // The four forms SQLite does parse stay classified as before.
        for sql in [
            "ALTER TABLE t ADD COLUMN c TEXT;",
            "ALTER TABLE t DROP COLUMN c;",
            "ALTER TABLE t RENAME COLUMN a TO b;",
            "ALTER TABLE t RENAME TO u;",
        ] {
            assert!(
                !classify_sql_for(DatabaseBackend::Sqlite, sql)
                    .iter()
                    .any(|f| f.risk == RiskLevel::Unsupported),
                "{sql} -> {:?}",
                sqlite_ops(sql)
            );
        }
    }

    #[test]
    fn sqlite_unique_and_primary_key_are_rejected_even_on_a_fresh_table() {
        // Verified against SQLite 3.45: "Cannot add a UNIQUE column" and "Cannot
        // add a PRIMARY KEY column" fire on an empty table too, unlike the
        // NOT NULL / non-constant-default / STORED restrictions.
        let sql = "CREATE TABLE posts (id INTEGER PRIMARY KEY);\n\
                   ALTER TABLE posts ADD COLUMN a TEXT UNIQUE;\n\
                   ALTER TABLE posts ADD COLUMN b INTEGER PRIMARY KEY;";
        let ops = sqlite_ops(sql);
        assert!(
            ops.contains(&"ADD COLUMN UNIQUE (unsupported on SQLite)".to_owned()),
            "got {ops:?}"
        );
        assert!(
            ops.contains(&"ADD COLUMN PRIMARY KEY (unsupported on SQLite)".to_owned()),
            "got {ops:?}"
        );
    }

    #[test]
    fn sqlite_bare_add_and_drop_forms_get_the_same_rules() {
        // SQLite lets the COLUMN keyword be omitted.
        let f = sqlite_finding("ALTER TABLE posts DROP title;", "DROP COLUMN")
            .expect("a bare DROP is still destructive");
        assert_eq!(f.risk, RiskLevel::Destructive);

        let f = sqlite_finding(
            "ALTER TABLE posts ADD title TEXT NOT NULL;",
            "ADD COLUMN NOT NULL (no default)",
        )
        .expect("a bare ADD still hits the NOT NULL rule");
        assert_eq!(f.risk, RiskLevel::Unsupported);

        // The canonicalization must not swallow the Postgres-only forms.
        for sql in [
            "ALTER TABLE t ADD CONSTRAINT ck CHECK (id > 0);",
            "ALTER TABLE t DROP CONSTRAINT ck;",
        ] {
            assert!(
                classify_sql_for(DatabaseBackend::Sqlite, sql)
                    .iter()
                    .any(|f| f.risk == RiskLevel::Unsupported),
                "{sql} -> {:?}",
                sqlite_ops(sql)
            );
        }
    }

    #[test]
    fn sqlite_writing_cte_is_caught_through_formatting_whitespace() {
        let sql = "WITH removed AS ( DELETE FROM posts RETURNING id) SELECT * FROM removed;";
        assert!(
            classify_sql_for(DatabaseBackend::Sqlite, sql)
                .iter()
                .any(|f| f.risk == RiskLevel::Unsupported),
            "got {:?}",
            sqlite_ops(sql)
        );
    }

    #[test]
    fn sqlite_rejects_a_multi_action_alter_table() {
        // SQLite takes one action per ALTER TABLE; both actions are individually
        // valid, so only the statement-level rule catches this.
        let f = sqlite_finding(
            "ALTER TABLE posts ADD COLUMN a TEXT, ADD COLUMN b TEXT;",
            "Multi-action ALTER TABLE (unsupported on SQLite)",
        )
        .expect("finding expected");
        assert_eq!(f.risk, RiskLevel::Unsupported);
        // One action is fine.
        assert!(
            classify_sql_for(
                DatabaseBackend::Sqlite,
                "ALTER TABLE posts ADD COLUMN a TEXT;"
            )
            .is_empty()
        );
        // Postgres allows the list.
        assert!(classify_sql("ALTER TABLE posts ADD COLUMN a TEXT, ADD COLUMN b TEXT;").is_empty());
    }

    #[test]
    fn sqlite_alter_table_never_falls_through_to_manual_review() {
        // Every subcommand is classified, so the Postgres catch-all must not fire.
        for sql in [
            "ALTER TABLE t ALTER COLUMN c SET NOT NULL;",
            "ALTER TABLE t ADD COLUMN c INTEGER PRIMARY KEY;",
            "ALTER TABLE t VALIDATE CONSTRAINT ck;",
        ] {
            assert!(
                !sqlite_ops(sql).contains(&"Unclassified ALTER TABLE".to_owned()),
                "{sql} -> {:?}",
                sqlite_ops(sql)
            );
        }
    }

    #[test]
    fn sqlite_rejects_postgres_only_index_and_table_clauses() {
        for sql in [
            "CREATE INDEX i ON posts USING btree (a);",
            "CREATE UNIQUE INDEX i ON posts (a) INCLUDE (b);",
            "CREATE INDEX i ON posts (a) WITH (fillfactor=70);",
            "CREATE TABLE p (id int) PARTITION BY RANGE (id);",
            "CREATE TABLE r (id int GENERATED BY DEFAULT AS IDENTITY);",
        ] {
            assert!(
                classify_sql_for(DatabaseBackend::Sqlite, sql)
                    .iter()
                    .any(|f| f.risk == RiskLevel::Unsupported),
                "{sql} -> {:?}",
                sqlite_ops(sql)
            );
        }
        // A partial / COLLATE / DESC index is valid SQLite.
        let ok = "CREATE INDEX i ON posts (title COLLATE NOCASE DESC) WHERE deleted_at IS NULL;";
        assert!(
            !classify_sql_for(DatabaseBackend::Sqlite, ok)
                .iter()
                .any(|f| f.risk == RiskLevel::Unsupported),
            "{ok} -> {:?}",
            sqlite_ops(ok)
        );
    }

    #[test]
    fn sqlite_unsupported_clauses_ignore_string_literal_contents() {
        for sql in [
            "INSERT INTO audit_log(message) VALUES ('waiting for update');",
            "INSERT INTO audit_log(message) VALUES ('on conflict on constraint x');",
        ] {
            assert!(
                !classify_sql_for(DatabaseBackend::Sqlite, sql)
                    .iter()
                    .any(|f| f.risk == RiskLevel::Unsupported),
                "{sql} is ordinary data, got {:?}",
                sqlite_ops(sql)
            );
        }
    }

    #[test]
    fn sqlite_rejects_postgres_only_dml_forms() {
        for sql in [
            "MERGE INTO t USING u ON t.id = u.id WHEN MATCHED THEN UPDATE SET a = 1;",
            "SELECT id FROM t FOR UPDATE;",
            "INSERT INTO t (a) VALUES (1) ON CONFLICT ON CONSTRAINT t_pkey DO NOTHING;",
            "WITH d AS (DELETE FROM t RETURNING id) SELECT * FROM d;",
        ] {
            assert!(
                classify_sql_for(DatabaseBackend::Sqlite, sql)
                    .iter()
                    .any(|f| f.risk == RiskLevel::Unsupported),
                "{sql} -> {:?}",
                sqlite_ops(sql)
            );
        }
    }

    #[test]
    fn sqlite_cannot_drop_an_implicit_index() {
        for sql in [
            "DROP INDEX sqlite_autoindex_posts_1;",
            "DROP INDEX IF EXISTS sqlite_autoindex_posts_1;",
        ] {
            let findings = classify_sql_for(DatabaseBackend::Sqlite, sql);
            assert!(
                findings.iter().any(|f| f.risk == RiskLevel::Unsupported),
                "{sql} -> {:?}",
                sqlite_ops(sql)
            );
        }
    }

    #[test]
    fn sqlite_covers_every_postgres_only_object_statement() {
        for sql in [
            "GRANT SELECT ON posts TO app;",
            "REVOKE SELECT ON posts FROM app;",
            "DROP EXTENSION pgcrypto;",
            "DROP MATERIALIZED VIEW mv;",
            "REFRESH MATERIALIZED VIEW mv;",
            "CREATE UNIQUE INDEX CONCURRENTLY i ON posts (a);",
            "DROP INDEX CONCURRENTLY i;",
            "ALTER SEQUENCE s RESTART;",
        ] {
            assert!(
                classify_sql_for(DatabaseBackend::Sqlite, sql)
                    .iter()
                    .any(|f| f.risk == RiskLevel::Unsupported),
                "{sql} -> {:?}",
                sqlite_ops(sql)
            );
        }
    }

    #[test]
    fn finding_prose_has_no_stray_whitespace_runs() {
        // A Rust `\` string continuation eats the newline and the indentation;
        // losing one leaves a space run in text printed straight to stderr.
        let samples = [
            "ALTER TABLE t ALTER COLUMN c TYPE TEXT;",
            "ALTER TABLE t ADD COLUMN c TEXT NOT NULL;",
            "ALTER TABLE t ADD COLUMN c TEXT UNIQUE;",
            "ALTER TABLE t DROP COLUMN c;",
            "TRUNCATE TABLE t;",
            "CREATE INDEX i ON t (a);",
            "CREATE SEQUENCE s;",
            "CREATE TYPE m AS ENUM ('a');",
            "CREATE EXTENSION pgcrypto;",
            "COMMENT ON TABLE t IS 'x';",
            "CREATE MATERIALIZED VIEW mv AS SELECT 1;",
            "GRANT SELECT ON t TO app;",
            "MERGE INTO t USING u ON t.id = u.id WHEN MATCHED THEN UPDATE SET a = 1;",
            "CREATE INDEX i ON t USING btree (a);",
            "CREATE TABLE p (id int) PARTITION BY RANGE (id);",
            "SELECT id FROM t FOR UPDATE;",
            "WITH d AS (DELETE FROM t RETURNING id) SELECT * FROM d;",
            "DROP INDEX sqlite_autoindex_t_1;",
            "ALTER TABLE t ADD COLUMN n INT GENERATED ALWAYS AS (id + 1) STORED;",
        ];
        for backend in [DatabaseBackend::Postgres, DatabaseBackend::Sqlite] {
            for sql in samples {
                for f in classify_sql_for(backend, sql) {
                    assert!(!f.why.contains("  "), "{sql}: why = {:?}", f.why);
                    assert!(
                        !f.next_action.contains("  "),
                        "{sql}: next_action = {:?}",
                        f.next_action
                    );
                }
            }
        }
    }
}
