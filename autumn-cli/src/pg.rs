//! Native Postgres connectivity shared by `autumn flags`, `autumn config`,
//! and `autumn experiments`.
//!
//! Replaces the historical `psql` shell-out (issue #1243) with a direct
//! `tokio_postgres` connection over the same `autumn.toml` / env-var
//! resolved database URL the app itself uses. `autumn` stays a synchronous
//! binary — each command runs its queries to completion on a fresh one-shot
//! Tokio runtime rather than making `main` async.
//!
//! Every `run_*` command builds a single `async` block returning
//! `Result<T, CommandError>` and hands it to [`block_on_or_die`]. Using `?`
//! to bail out of that block (rather than calling [`die`] mid-flight) means
//! any `Transaction` still open at that point is dropped — and therefore
//! explicitly rolled back — *before* the process exits, instead of relying
//! on the OS closing the socket to make Postgres abort it server-side.

use crate::text_width::display_width;
use tokio_postgres::types::ToSql;
use tokio_postgres::{Client, NoTls, Row};

/// Run `fut` to completion on a fresh single-threaded Tokio runtime.
///
/// Each `autumn flags/config/experiments` invocation is a short-lived
/// process that does one round of queries, so a throwaway runtime is
/// simpler than making the whole CLI `async fn main`.
pub fn block_on<F: std::future::Future>(fut: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to start Tokio runtime")
        .block_on(fut)
}

/// An error from a `run_*` command's async body, distinguishing connection
/// failures (exit code 2, mirroring `psql`'s own convention) from every
/// other failure (exit code 1).
pub enum CommandError {
    Connect(String),
    Other(String),
}

impl CommandError {
    const fn exit_code(&self) -> i32 {
        match self {
            Self::Connect(_) => 2,
            Self::Other(_) => 1,
        }
    }

    fn message(&self) -> &str {
        match self {
            Self::Connect(msg) | Self::Other(msg) => msg,
        }
    }
}

/// Converts any `Display`-able error into a [`CommandError::Other`], so
/// `tokio_postgres`/`serde_json` results can be propagated with `.pg()?`
/// instead of repeating `.unwrap_or_else(|e| die(label, e))` at every call
/// site.
pub trait ResultExt<T> {
    fn pg(self) -> Result<T, CommandError>;
}

impl<T, E: std::fmt::Display> ResultExt<T> for Result<T, E> {
    fn pg(self) -> Result<T, CommandError> {
        self.map_err(|e| CommandError::Other(e.to_string()))
    }
}

/// Connect to `db_url`, returning a [`CommandError::Connect`] on failure so
/// callers can propagate it with `?` rather than exiting immediately.
///
/// Uses `NoTls` — the same plaintext transport `autumn-web`'s own
/// `diesel-async`/`tokio-postgres` connection pool uses by default; TLS
/// parameters embedded in the URL itself (`sslmode=...`) are unaffected
/// either way since we pass the URL straight through.
pub async fn connect(label: &str, db_url: &str) -> Result<Client, CommandError> {
    let (client, connection) = tokio_postgres::connect(db_url, NoTls)
        .await
        .map_err(|e| CommandError::Connect(format!("failed to connect to the database: {e}")))?;

    // The connection object performs the actual IO; it must be polled
    // concurrently with the client or queries will hang forever.
    let label = label.to_owned();
    tokio::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("autumn {label}: connection error: {e}");
        }
    });

    Ok(client)
}

/// Run `fut` on a fresh runtime; on `Err`, print `label`-prefixed message and
/// exit. Because `fut` has already run to completion (including dropping any
/// `Transaction` it held) by the time this function inspects the result,
/// early-return error paths still trigger a normal Rust-level rollback.
pub fn block_on_or_die<T>(
    label: &str,
    fut: impl std::future::Future<Output = Result<T, CommandError>>,
) -> T {
    match block_on(fut) {
        Ok(value) => value,
        Err(e) => {
            eprintln!("autumn {label}: {}", e.message());
            std::process::exit(e.exit_code());
        }
    }
}

/// Print `label`-prefixed `err` to stderr and exit the process with status 1.
///
/// Only for use in plain synchronous code *after* [`block_on_or_die`] has
/// already returned (e.g. an empty-result "not found" check) — never inside
/// an open transaction, where it would skip that transaction's `Drop`.
pub fn die(label: &str, err: impl std::fmt::Display) -> ! {
    eprintln!("autumn {label}: {err}");
    std::process::exit(1);
}

/// Run `mutation_sql` then `audit_sql` inside one transaction and commit.
///
/// Every `flags`/`experiments` mutation command must record an audit-log row
/// alongside its data change — the audit tables' `NOTIFY`-firing DB triggers
/// depend on it, and it's the record a future `history`/`status` command
/// reads back. Routing every such mutation through this one function makes
/// that pairing structurally mandatory (there's no way to call it and skip
/// the audit insert) instead of a convention each `run_*` function has to
/// re-implement and could forget.
///
/// `not_found_message`, if given, turns "`mutation_sql` affected zero rows"
/// into that error *instead of* writing the audit row and committing — this
/// lets a caller fold a validation guard into `mutation_sql`'s own `WHERE`
/// clause (e.g. "and not already concluded") and have it enforced
/// atomically with the write, without a separate check-then-mutate round
/// trip that would leave a race window between the check and the write.
/// Pass `None` when the mutation is an upsert that always affects a row.
pub async fn execute_with_audit(
    client: &mut Client,
    mutation_sql: &str,
    mutation_params: &[&(dyn ToSql + Sync)],
    not_found_message: Option<&str>,
    audit_sql: &str,
    audit_params: &[&(dyn ToSql + Sync)],
) -> Result<(), CommandError> {
    execute_many_with_audit(
        client,
        &[(mutation_sql, mutation_params)],
        not_found_message,
        audit_sql,
        audit_params,
    )
    .await
}

/// As [`execute_with_audit`], but for mutations that need more than one
/// statement before the audit insert (e.g. `flags allow`, which upserts the
/// flag row and then updates its allowlist). `not_found_message` is checked
/// against the *last* mutation statement's affected-row count.
pub async fn execute_many_with_audit(
    client: &mut Client,
    mutation_statements: &[(&str, &[&(dyn ToSql + Sync)])],
    not_found_message: Option<&str>,
    audit_sql: &str,
    audit_params: &[&(dyn ToSql + Sync)],
) -> Result<(), CommandError> {
    let txn = client.transaction().await.pg()?;
    let mut affected = 0;
    for (sql, params) in mutation_statements {
        affected = txn.execute(*sql, params).await.pg()?;
    }
    if affected == 0
        && let Some(msg) = not_found_message
    {
        // Early return drops `txn` here, sending an explicit ROLLBACK —
        // the audit row (below) and the commit never happen.
        return Err(CommandError::Other(msg.to_owned()));
    }
    txn.execute(audit_sql, audit_params).await.pg()?;
    txn.commit().await.pg()
}

/// Read every column of `row` as `Option<String>`.
///
/// All SQL in `flags`/`config`/`experiments` casts non-text columns to
/// `text` explicitly, so every result column round-trips through this one
/// conversion without needing extra `tokio-postgres` type features enabled.
pub fn row_to_strings(row: &Row) -> Vec<Option<String>> {
    (0..row.len())
        .map(|i| row.get::<_, Option<String>>(i))
        .collect()
}

/// Print a result set as a simple aligned table: a header row, a `-`
/// separator, then the data rows — no trailing row-count footer, matching
/// the `\pset footer off` output the old psql implementation used.
pub fn print_table(headers: &[&str], rows: &[Vec<Option<String>>]) {
    let mut widths: Vec<usize> = headers.iter().map(|h| display_width(h)).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(display_width(cell.as_deref().unwrap_or("")));
        }
    }

    // `format!`'s `{:width$}` spec for `&str` pads by Unicode scalar count
    // (`chars().count()`), the same unit `display_width` computes above —
    // unlike the byte length `str::len()` would give, which mismatched here
    // and over-padded any cell containing multi-byte characters.
    let format_row = |cells: &[&str]| -> String {
        cells
            .iter()
            .enumerate()
            .map(|(i, cell)| format!("{cell:width$}", width = widths[i]))
            .collect::<Vec<_>>()
            .join(" | ")
            .trim_end()
            .to_owned()
    };

    println!("{}", format_row(headers));
    println!(
        "{}",
        widths
            .iter()
            .map(|w| "-".repeat(w + 1))
            .collect::<Vec<_>>()
            .join("+")
    );
    for row in rows {
        let cells: Vec<&str> = row.iter().map(|c| c.as_deref().unwrap_or("")).collect();
        println!("{}", format_row(&cells));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn print_table_smoke_does_not_panic_on_empty_rows() {
        print_table(&["key", "value"], &[]);
    }

    #[test]
    fn block_on_returns_future_output() {
        let result = block_on(async { 1 + 1 });
        assert_eq!(result, 2);
    }

    #[test]
    fn connect_error_exits_with_code_2() {
        assert_eq!(CommandError::Connect("x".into()).exit_code(), 2);
    }

    #[test]
    fn other_error_exits_with_code_1() {
        assert_eq!(CommandError::Other("x".into()).exit_code(), 1);
    }

    #[test]
    fn pg_ext_wraps_display_error_as_other() {
        let result: Result<(), String> = Err("boom".to_owned());
        match result.pg() {
            Err(CommandError::Other(msg)) => assert_eq!(msg, "boom"),
            _ => panic!("expected CommandError::Other"),
        }
    }
}
