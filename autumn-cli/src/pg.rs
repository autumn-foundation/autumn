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
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use std::sync::Arc;
use tokio_postgres::types::ToSql;
use tokio_postgres::{Client, Row};
use tokio_postgres_rustls::MakeRustlsConnect;

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

/// Accepts any server certificate without validating its chain or hostname,
/// while still cryptographically verifying the TLS handshake signatures
/// (i.e. the connection is genuinely encrypted to *some* holder of the
/// presented key — this is not "no security", only "no identity check").
///
/// This mirrors `libpq`/`psql`'s actual behavior for `sslmode=require` (and
/// the default `prefer`): those modes encrypt the connection but do
/// *not* validate the server's certificate — only `verify-ca`/`verify-full`
/// do that, and `tokio_postgres`'s own `sslmode` parser doesn't even accept
/// those two values (see `Config`'s `sslmode` match arms — only `disable`,
/// `prefer`, and `require` parse; anything else is a config error). A
/// verifier backed by the OS trust store would reject the private-CA and
/// self-signed certificates that `sslmode=require` deployments commonly use
/// (this was tried and broke against a default Debian Postgres install's
/// snakeoil cert), which `psql` never rejected under the modes this crate
/// can actually reach.
#[derive(Debug)]
struct NoServerCertVerification(CryptoProvider);

impl ServerCertVerifier for NoServerCertVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

/// Build a `rustls`-backed TLS connector.
///
/// `tokio_postgres::connect` parses `sslmode` out of `db_url` itself
/// (defaulting to `prefer`, matching `libpq`/`psql`) and decides whether to
/// negotiate TLS at all — `disable` skips it entirely, `prefer` tries TLS
/// first and falls back to plaintext if the server doesn't offer it, and
/// `require` insists on it. That decision keys off whether the `TlsConnect`
/// passed in can actually connect, so the only thing this module needs to
/// provide is a real connector: passing `NoTls` (as this module used to)
/// forces plaintext unconditionally regardless of `sslmode`, which broke
/// connections to managed Postgres instances that require TLS.
fn tls_connector() -> MakeRustlsConnect {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .expect("ring provider supports rustls's default protocol versions")
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(NoServerCertVerification((*provider).clone())))
        .with_no_client_auth();
    MakeRustlsConnect::new(config)
}

/// Connect to `db_url`, returning a [`CommandError::Connect`] on failure so
/// callers can propagate it with `?` rather than exiting immediately.
pub async fn connect(label: &str, db_url: &str) -> Result<Client, CommandError> {
    let (client, connection) = tokio_postgres::connect(db_url, tls_connector())
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

/// Build the aligned-table string (extracted from [`print_table`] for
/// testability, mirroring `routes::format_table`'s shape).
///
/// Row cells are joined by `" | "` (3 characters); the separator's `+`
/// markers must land under each `|` exactly, so each dash segment is padded
/// to just the column width and the segments are joined by `"-+-"` (also 3
/// characters, with `+` in the middle position like `|` is in `" | "`) —
/// not `w + 1` dashes joined by a bare `"+"`, which drifts out of alignment
/// by one character per column from the third column onward.
pub fn format_table(headers: &[&str], rows: &[Vec<Option<String>>]) -> String {
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

    let mut out = String::new();
    out.push_str(&format_row(headers));
    out.push('\n');
    out.push_str(
        &widths
            .iter()
            .map(|w| "-".repeat(*w))
            .collect::<Vec<_>>()
            .join("-+-"),
    );
    out.push('\n');
    for row in rows {
        let cells: Vec<&str> = row.iter().map(|c| c.as_deref().unwrap_or("")).collect();
        out.push_str(&format_row(&cells));
        out.push('\n');
    }
    out
}

/// Print a result set as a simple aligned table: a header row, a `-`
/// separator, then the data rows — no trailing row-count footer, matching
/// the `\pset footer off` output the old psql implementation used.
pub fn print_table(headers: &[&str], rows: &[Vec<Option<String>>]) {
    print!("{}", format_table(headers, rows));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn print_table_smoke_does_not_panic_on_empty_rows() {
        print_table(&["key", "value"], &[]);
    }

    #[test]
    fn tls_connector_builds_without_panicking() {
        let _ = tls_connector();
    }

    #[test]
    fn no_server_cert_verification_reports_supported_signature_schemes() {
        // A verifier with zero supported schemes would make every TLS
        // handshake fail signature checks; this pins down that the ring
        // provider's schemes actually get threaded through.
        let provider = rustls::crypto::ring::default_provider();
        let verifier = NoServerCertVerification(provider);
        assert!(!verifier.supported_verify_schemes().is_empty());
    }

    #[test]
    fn separator_plus_marks_align_with_header_pipes_for_three_plus_columns() {
        // Regression test: the separator's `+` markers must land in the
        // exact same column as each header/data row's `|`, for tables with
        // more than two columns (where a naive `w + 1` / `"+"`-joined
        // separator drifts out of alignment by one character per column).
        let table = format_table(
            &["name", "state", "variants", "winner"],
            &[vec![
                Some("checkout_flow".to_owned()),
                Some("running".to_owned()),
                Some("control=50, treatment=50".to_owned()),
                None,
            ]],
        );
        let lines: Vec<&str> = table.lines().collect();
        let header_pipes: Vec<usize> = lines[0]
            .char_indices()
            .filter(|(_, c)| *c == '|')
            .map(|(i, _)| i)
            .collect();
        let separator_plusses: Vec<usize> = lines[1]
            .char_indices()
            .filter(|(_, c)| *c == '+')
            .map(|(i, _)| i)
            .collect();
        let data_pipes: Vec<usize> = lines[2]
            .char_indices()
            .filter(|(_, c)| *c == '|')
            .map(|(i, _)| i)
            .collect();
        assert_eq!(header_pipes, separator_plusses);
        assert_eq!(header_pipes, data_pipes);
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
