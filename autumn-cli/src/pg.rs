//! Native Postgres connectivity shared by `autumn flags`, `autumn config`,
//! and `autumn experiments`.
//!
//! Replaces the historical `psql` shell-out (issue #1243) with a direct
//! `tokio_postgres` connection over the same `autumn.toml` / env-var
//! resolved database URL the app itself uses. `autumn` stays a synchronous
//! binary — each command runs its queries to completion on a fresh one-shot
//! Tokio runtime rather than making `main` async.

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

/// Connect to `db_url`, printing an error and exiting the process on failure.
///
/// Uses `NoTls` — the same plaintext transport `autumn-web`'s own
/// `diesel-async`/`tokio-postgres` connection pool uses by default; TLS
/// parameters embedded in the URL itself (`sslmode=...`) are unaffected
/// either way since we pass the URL straight through.
pub async fn connect_or_die(label: &str, db_url: &str) -> Client {
    let (client, connection) = tokio_postgres::connect(db_url, NoTls)
        .await
        .unwrap_or_else(|e| {
            die(
                label,
                format_args!("failed to connect to the database: {e}"),
            )
        });

    // The connection object performs the actual IO; it must be polled
    // concurrently with the client or queries will hang forever.
    let label = label.to_owned();
    tokio::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("autumn {label}: connection error: {e}");
        }
    });

    client
}

/// Print `label`-prefixed `err` to stderr and exit the process with status 1.
pub fn die(label: &str, err: impl std::fmt::Display) -> ! {
    eprintln!("autumn {label}: {err}");
    std::process::exit(1);
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
    let mut widths: Vec<usize> = headers.iter().map(|h| h.len()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.as_deref().unwrap_or("").len());
        }
    }

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
}
