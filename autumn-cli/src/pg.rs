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
use std::path::{Path, PathBuf};
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
#[derive(Debug)]
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

/// Query parameters `libpq`/`psql` understand for TLS but that
/// `tokio_postgres`'s own connection-string parser rejects outright with an
/// `UnknownOption` error — custom CA pinning and client-certificate auth.
/// Since [`tls_connector`] doesn't validate certificate chains at all (see
/// [`NoServerCertVerification`]), a custom root CA wouldn't be consulted
/// even if we accepted it, so these are simply dropped rather than
/// implemented.
const UNSUPPORTED_SSL_PARAMS: &[&str] =
    &["sslrootcert", "sslcert", "sslkey", "sslpassword", "sslcrl"];

/// Drop [`UNSUPPORTED_SSL_PARAMS`] from `pairs`, and reject `sslmode=
/// verify-ca`/`verify-full` outright rather than silently downgrading them.
///
/// `tokio_postgres` only recognizes `sslmode=disable/prefer/require` — no
/// `verify-ca`/`verify-full` — and [`tls_connector`] doesn't validate
/// certificate chains at all (see [`NoServerCertVerification`]). Silently
/// remapping a `verify-ca`/`verify-full` request down to `require` would
/// make the CLI accept *any* certificate when an operator explicitly asked
/// for identity verification — a worse security posture than the request,
/// delivered silently. Failing loudly instead means an operator relying on
/// verification finds out immediately rather than being quietly exposed.
fn filter_ssl_params(
    pairs: impl Iterator<Item = (String, String)>,
) -> Result<Vec<(String, String)>, CommandError> {
    let mut out = Vec::new();
    for (key, value) in pairs {
        if UNSUPPORTED_SSL_PARAMS.contains(&key.as_str()) {
            continue;
        }
        if key == "sslmode" && matches!(value.as_str(), "verify-ca" | "verify-full") {
            return Err(CommandError::Other(format!(
                "sslmode={value} is not supported: certificate verification isn't implemented \
                 for this native connection path. Use sslmode=require to encrypt without \
                 verifying the server's certificate, or sslmode=disable to connect in plaintext."
            )));
        }
        if key == "sslmode" && value == "allow" {
            // `tokio_postgres::Config`'s own parser only recognizes
            // disable/prefer/require for `sslmode` — `allow` (try plaintext
            // first, retry with TLS only if that's refused) isn't a mode it
            // can express at all, so letting it through would surface a
            // generic "invalid value for option" parse error instead of
            // this clearer one. `prefer` is the closest supported
            // alternative, and a strictly stronger default (it prefers
            // encryption rather than only falling back to it).
            return Err(CommandError::Other(
                "sslmode=allow is not supported: this native connection path only implements \
                 disable/prefer/require semantics. Use sslmode=prefer to negotiate TLS \
                 opportunistically (preferring it, rather than allow's plaintext-first order), \
                 or sslmode=require to mandate it."
                    .to_owned(),
            ));
        }
        out.push((key, value));
    }
    Ok(out)
}

/// Tokenize a `libpq` keyword/value connection string (e.g.
/// `host=localhost dbname=mydb sslmode=require`) using the same grammar
/// `tokio_postgres`'s own `Parser` does: whitespace-separated `key=value`
/// pairs, where a value is either a run of non-whitespace characters or a
/// `'...'`-quoted string, both supporting `\`-escapes. Returns `None` if
/// `s` doesn't parse as this grammar at all — callers fall back to passing
/// `s` through unchanged so `tokio_postgres` can produce its own error.
fn parse_keyword_value_pairs(s: &str) -> Option<Vec<(String, String)>> {
    let mut pairs = Vec::new();
    let mut chars = s.char_indices().peekable();

    loop {
        while matches!(chars.peek(), Some((_, c)) if c.is_whitespace()) {
            chars.next();
        }
        let Some(&(key_start, _)) = chars.peek() else {
            break;
        };
        while matches!(chars.peek(), Some((_, c)) if !c.is_whitespace() && *c != '=') {
            chars.next();
        }
        let key_end = chars.peek().map_or(s.len(), |&(i, _)| i);
        if key_end == key_start {
            return None;
        }
        let key = &s[key_start..key_end];

        while matches!(chars.peek(), Some((_, c)) if c.is_whitespace()) {
            chars.next();
        }
        if chars.next().map(|(_, c)| c) != Some('=') {
            return None;
        }
        while matches!(chars.peek(), Some((_, c)) if c.is_whitespace()) {
            chars.next();
        }

        let mut value = String::new();
        if matches!(chars.peek(), Some((_, '\''))) {
            chars.next();
            let mut terminated = false;
            while let Some((_, c)) = chars.next() {
                if c == '\'' {
                    terminated = true;
                    break;
                }
                if c == '\\' {
                    if let Some((_, c2)) = chars.next() {
                        value.push(c2);
                    }
                } else {
                    value.push(c);
                }
            }
            if !terminated {
                return None;
            }
        } else {
            while matches!(chars.peek(), Some((_, c)) if !c.is_whitespace()) {
                let (_, c) = chars.next().unwrap();
                if c == '\\' {
                    if let Some((_, c2)) = chars.next() {
                        value.push(c2);
                    }
                } else {
                    value.push(c);
                }
            }
            if value.is_empty() {
                return None;
            }
        }

        pairs.push((key.to_owned(), value));
    }

    if pairs.is_empty() { None } else { Some(pairs) }
}

/// Quote `value` in `libpq` keyword/value form if needed (empty, contains
/// whitespace, or contains a quote/backslash), escaping `\` and `'`.
fn keyword_value_token(key: &str, value: &str) -> String {
    if value.is_empty() || value.contains(|c: char| c.is_whitespace() || c == '\'' || c == '\\') {
        let escaped = value.replace('\\', "\\\\").replace('\'', "\\'");
        format!("{key}='{escaped}'")
    } else {
        format!("{key}={value}")
    }
}

/// Remove and return the first `(key, value)` pair matching `key`, if any.
fn take_pair(pairs: &mut Vec<(String, String)>, key: &str) -> Option<String> {
    let index = pairs.iter().position(|(k, _)| k == key)?;
    Some(pairs.remove(index).1)
}

/// Return the value of the first `(key, value)` pair matching `key`, if any.
fn find_pair<'a>(pairs: &'a [(String, String)], key: &str) -> Option<&'a str> {
    pairs
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

/// The OS login name `tokio_postgres` itself falls back to when a connection
/// string sets no `user` (see `connect_raw.rs`'s use of `whoami::username()`)
/// — used here only to resolve the same effective username for `.pgpass`
/// matching, not to set it explicitly (that stays `tokio_postgres`'s job).
fn current_os_user() -> String {
    whoami::username().unwrap_or_else(|_| "postgres".to_owned())
}

/// Parse one `.pgpass` line into its 5 colon-delimited fields
/// (`host:port:dbname:user:password`), honoring `\`-escapes for literal `:`
/// and `\` within a field (the format `libpq` itself uses). Returns `None`
/// if the line doesn't have exactly 5 fields.
fn parse_pgpass_line(line: &str) -> Option<[String; 5]> {
    let mut fields = Vec::with_capacity(5);
    let mut current = String::new();
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' && matches!(chars.peek(), Some(':' | '\\')) {
            current.push(chars.next().unwrap());
        } else if c == ':' {
            fields.push(std::mem::take(&mut current));
        } else {
            current.push(c);
        }
    }
    fields.push(current);
    fields.try_into().ok()
}

/// Find the password for `(host, port, dbname, user)` in a `.pgpass` file's
/// `contents`, using the first line whose fields all match (a field of `*`
/// matches anything) — mirrors `libpq`'s own `.pgpass` lookup rule.
fn pgpass_lookup(
    contents: &str,
    host: &str,
    port: &str,
    dbname: &str,
    user: &str,
) -> Option<String> {
    let field_matches = |field: &str, value: &str| field == "*" || field == value;
    contents.lines().find_map(|line| {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            return None;
        }
        let [f_host, f_port, f_dbname, f_user, f_password] = parse_pgpass_line(trimmed)?;
        (field_matches(&f_host, host)
            && field_matches(&f_port, port)
            && field_matches(&f_dbname, dbname)
            && field_matches(&f_user, user))
        .then_some(f_password)
    })
}

/// Path to the `.pgpass` file: `PGPASSFILE` if set, else `~/.pgpass`
/// (matching `libpq`'s own precedence).
fn pgpass_file_path() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("PGPASSFILE") {
        return Some(PathBuf::from(path));
    }
    Some(directories::BaseDirs::new()?.home_dir().join(".pgpass"))
}

/// `libpq` refuses to use a `.pgpass` file that's readable by anyone but its
/// owner, to avoid trusting a password another local user could have
/// planted; mirror that here rather than silently trusting a
/// world/group-readable credentials file.
#[cfg(unix)]
fn pgpass_permissions_ok(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|meta| meta.permissions().mode().trailing_zeros() >= 6)
}

#[cfg(not(unix))]
fn pgpass_permissions_ok(_path: &Path) -> bool {
    true
}

/// Look up a password for `(host, port, dbname, user)` in the `.pgpass` file,
/// if one exists and has safe permissions.
fn pgpass_password(host: &str, port: &str, dbname: &str, user: &str) -> Option<String> {
    let path = pgpass_file_path()?;
    if !path.is_file() {
        return None;
    }
    if !pgpass_permissions_ok(&path) {
        eprintln!(
            "autumn: ignoring {} — it has group or world access; permissions should be u=rw (0600) or less",
            path.display()
        );
        return None;
    }
    let contents = std::fs::read_to_string(&path).ok()?;
    pgpass_lookup(&contents, host, port, dbname, user)
}

/// Resolve a password for `(host, port, dbname, user)` the same way `libpq`
/// does when none is given explicitly: the `PGPASSWORD` environment
/// variable first, then the `.pgpass` file.
fn resolve_password(host: &str, port: &str, dbname: &str, user: &str) -> Option<String> {
    std::env::var("PGPASSWORD")
        .ok()
        .or_else(|| pgpass_password(host, port, dbname, user))
}

/// Parse a `pg_service.conf`-style file, returning the `key=value` pairs of
/// the `[service_name]` section, or `None` if that section isn't present.
/// Lines starting with `#` or `;` are comments, matching `libpq`'s own
/// service-file format.
fn parse_service_file(contents: &str, service_name: &str) -> Option<Vec<(String, String)>> {
    let mut pairs = Vec::new();
    let mut in_section = false;
    let mut found = false;
    for line in contents.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with(';') {
            continue;
        }
        if let Some(name) = trimmed.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            in_section = name == service_name;
            found |= in_section;
            continue;
        }
        if in_section && let Some((key, value)) = trimmed.split_once('=') {
            pairs.push((key.trim().to_owned(), value.trim().to_owned()));
        }
    }
    found.then_some(pairs)
}

/// Path to the service file: `PGSERVICEFILE` if set, else `~/.pg_service.conf`
/// (matching `libpq`'s own precedence; the system-wide service file `libpq`
/// also consults isn't supported here).
fn service_file_path() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("PGSERVICEFILE") {
        return Some(PathBuf::from(path));
    }
    Some(
        directories::BaseDirs::new()?
            .home_dir()
            .join(".pg_service.conf"),
    )
}

/// Load the `[service_name]` group's parameters from the service file,
/// erroring clearly if no service file can be found or the named group
/// isn't in it — silently ignoring an explicitly requested `service=` would
/// leave a connection missing parameters the operator expected to be set.
fn load_service(service_name: &str) -> Result<Vec<(String, String)>, CommandError> {
    let path = service_file_path().ok_or_else(|| {
        CommandError::Other(format!(
            "service={service_name} was requested but no service file could be located \
             (set PGSERVICEFILE or create ~/.pg_service.conf)"
        ))
    })?;
    let contents = std::fs::read_to_string(&path).map_err(|e| {
        CommandError::Other(format!(
            "service={service_name} was requested but its service file {} could not be read: {e}",
            path.display()
        ))
    })?;
    parse_service_file(&contents, service_name).ok_or_else(|| {
        CommandError::Other(format!(
            "service \"{service_name}\" was not found in service file {}",
            path.display()
        ))
    })
}

/// Adapt `db_url` for `tokio_postgres` before connecting — see
/// [`filter_ssl_params`] for what's dropped/rejected and why. Handles both
/// connection-string shapes `tokio_postgres` itself accepts: URL form
/// (`postgres://...?sslmode=...`) and `libpq`'s keyword/value form
/// (`host=... sslmode=...`). A string matching neither shape is passed
/// through unchanged so `tokio_postgres` can produce its own error.
///
/// Also resolves two credential sources `tokio_postgres` itself never
/// consults, but `libpq`/`psql` always did — leaving them out would silently
/// break authentication for any deployment relying on them instead of
/// embedding the password directly in the connection string:
/// - a `service=name` parameter, expanded from the `pg_service.conf` group of
///   that name (params already set explicitly in `db_url` win over the
///   service group's, matching `libpq`'s own precedence);
/// - a still-missing password, resolved from `PGPASSWORD` or `.pgpass` (see
///   [`resolve_password`]) using the (possibly service-expanded) host/port
///   /dbname/user.
fn sanitize_db_url(db_url: &str) -> Result<String, CommandError> {
    if let Ok(url) = url::Url::parse(db_url) {
        return sanitize_url_form(url);
    }

    if let Some(pairs) = parse_keyword_value_pairs(db_url) {
        return sanitize_keyword_form(pairs);
    }

    Ok(db_url.to_owned())
}

/// The URL-form half of [`sanitize_db_url`].
fn sanitize_url_form(mut url: url::Url) -> Result<String, CommandError> {
    let mut pairs = filter_ssl_params(
        url.query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned())),
    )?;

    // Capture what the URL's own structure (as opposed to its query
    // string) already says explicitly, once, up front: `host`/`port`
    // need this snapshot below to detect whether a later merge is safe
    // to fold back into the query string at all (see the comment by
    // `needs_keyword_form`).
    let explicit_user = Some(url.username())
        .filter(|u| !u.is_empty())
        .map(str::to_owned)
        .or_else(|| find_pair(&pairs, "user").map(str::to_owned));
    let explicit_password = url
        .password()
        .map(str::to_owned)
        .or_else(|| find_pair(&pairs, "password").map(str::to_owned));
    let explicit_host = url
        .host_str()
        .filter(|h| !h.is_empty())
        .map(str::to_owned)
        .or_else(|| find_pair(&pairs, "host").map(str::to_owned));
    let explicit_port = url
        .port()
        .map(|p| p.to_string())
        .or_else(|| find_pair(&pairs, "port").map(str::to_owned));
    let explicit_dbname = Some(url.path())
        .filter(|p| p.len() > 1)
        .map(|p| p.trim_start_matches('/').to_owned())
        .or_else(|| find_pair(&pairs, "dbname").map(str::to_owned));

    let is_explicit = |pairs: &[(String, String)], key: &str| match key {
        "user" => explicit_user.is_some(),
        "password" => explicit_password.is_some(),
        "host" => explicit_host.is_some(),
        "port" => explicit_port.is_some(),
        "dbname" => explicit_dbname.is_some(),
        _ => find_pair(pairs, key).is_some(),
    };

    // Whether the service merge below ends up filling in a `host` or
    // `port` that wasn't already explicit — see the comment where this
    // is used for why that specifically forces a keyword-form rebuild.
    let mut needs_keyword_form = false;

    if let Some(service_name) = take_pair(&mut pairs, "service") {
        for (key, value) in load_service(&service_name)? {
            if !is_explicit(&pairs, &key) {
                needs_keyword_form |= key == "host" || key == "port";
                pairs.push((key, value));
            }
        }
    }

    let host = explicit_host
        .clone()
        .or_else(|| find_pair(&pairs, "host").map(str::to_owned))
        .unwrap_or_else(|| "localhost".to_owned());
    let port = explicit_port
        .clone()
        .or_else(|| find_pair(&pairs, "port").map(str::to_owned))
        .unwrap_or_else(|| "5432".to_owned());
    let user = explicit_user
        .clone()
        .or_else(|| find_pair(&pairs, "user").map(str::to_owned))
        .unwrap_or_else(current_os_user);
    let dbname = explicit_dbname
        .clone()
        .or_else(|| find_pair(&pairs, "dbname").map(str::to_owned))
        .unwrap_or_else(|| user.clone());

    if !is_explicit(&pairs, "password")
        && let Some(password) = resolve_password(&host, &port, &dbname, &user)
    {
        pairs.push(("password".to_owned(), password));
    }

    if needs_keyword_form {
        // `tokio_postgres`'s URL-form parser bakes a default `port=5432`
        // into the config as soon as the authority names *any* host —
        // even with no port given — and both `host` and `port` are
        // appended to a list rather than overwritten. So folding a
        // service-provided host/port back in as a query parameter here
        // wouldn't override that implicit default, it would sit
        // alongside it as a second entry. Keyword form has no such
        // authority-parsing special case — every key here is one we
        // added ourselves — so falling back to it for exactly this case
        // sidesteps the duplication entirely.
        for (key, value) in [
            ("user", explicit_user),
            ("password", explicit_password),
            ("host", explicit_host),
            ("port", explicit_port),
            ("dbname", explicit_dbname),
        ] {
            if let Some(value) = value
                && find_pair(&pairs, key).is_none()
            {
                pairs.push((key.to_owned(), value));
            }
        }
        return Ok(pairs
            .iter()
            .map(|(key, value)| keyword_value_token(key, value))
            .collect::<Vec<_>>()
            .join(" "));
    }

    if pairs.is_empty() {
        url.set_query(None);
    } else {
        let mut query_pairs = url.query_pairs_mut();
        query_pairs.clear();
        for (key, value) in &pairs {
            query_pairs.append_pair(key, value);
        }
        drop(query_pairs);
    }
    Ok(url.into())
}

/// The keyword/value-form half of [`sanitize_db_url`].
fn sanitize_keyword_form(pairs: Vec<(String, String)>) -> Result<String, CommandError> {
    let mut pairs = filter_ssl_params(pairs.into_iter())?;

    if let Some(service_name) = take_pair(&mut pairs, "service") {
        for (key, value) in load_service(&service_name)? {
            if find_pair(&pairs, &key).is_none() {
                pairs.push((key, value));
            }
        }
    }

    if find_pair(&pairs, "password").is_none() {
        let host = find_pair(&pairs, "host").map_or_else(|| "localhost".to_owned(), str::to_owned);
        let port = find_pair(&pairs, "port").map_or_else(|| "5432".to_owned(), str::to_owned);
        let user = find_pair(&pairs, "user").map_or_else(current_os_user, str::to_owned);
        let dbname = find_pair(&pairs, "dbname").map_or_else(|| user.clone(), str::to_owned);
        if let Some(password) = resolve_password(&host, &port, &dbname, &user) {
            pairs.push(("password".to_owned(), password));
        }
    }

    Ok(pairs
        .iter()
        .map(|(key, value)| keyword_value_token(key, value))
        .collect::<Vec<_>>()
        .join(" "))
}

/// Connect to `db_url`, returning a [`CommandError::Connect`] on failure so
/// callers can propagate it with `?` rather than exiting immediately.
pub async fn connect(label: &str, db_url: &str) -> Result<Client, CommandError> {
    let db_url = sanitize_db_url(db_url)?;
    let (client, connection) = tokio_postgres::connect(&db_url, tls_connector())
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

    /// Set `path`'s permissions to satisfy [`pgpass_permissions_ok`] on
    /// whichever platform the test is running on (a no-op on non-unix,
    /// where that check always passes).
    #[cfg(unix)]
    fn set_pgpass_permissions_safe(path: &std::path::Path) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    #[cfg(not(unix))]
    fn set_pgpass_permissions_safe(_path: &std::path::Path) {}

    #[test]
    fn print_table_smoke_does_not_panic_on_empty_rows() {
        print_table(&["key", "value"], &[]);
    }

    #[test]
    fn tls_connector_builds_without_panicking() {
        let _ = tls_connector();
    }

    #[test]
    fn sanitize_drops_unsupported_ssl_params_from_url() {
        let sanitized = sanitize_db_url(
            "postgres://user:pw@host/db?sslmode=require&sslrootcert=/etc/ca.pem&connect_timeout=5",
        )
        .unwrap();
        let parsed = url::Url::parse(&sanitized).unwrap();
        let pairs: std::collections::HashMap<_, _> = parsed.query_pairs().into_owned().collect();
        assert_eq!(pairs.get("sslmode").map(String::as_str), Some("require"));
        assert!(!pairs.contains_key("sslrootcert"));
        assert_eq!(pairs.get("connect_timeout").map(String::as_str), Some("5"));
    }

    #[test]
    fn sanitize_rejects_verify_full_in_url_form_instead_of_downgrading() {
        // A silent downgrade to `require` would make the CLI accept any
        // certificate when an operator explicitly asked for verification —
        // this must fail loudly instead.
        let err = sanitize_db_url("postgres://host/db?sslmode=verify-full").unwrap_err();
        assert!(err.message().contains("verify-full"));
    }

    #[test]
    fn sanitize_rejects_verify_ca_in_url_form_instead_of_downgrading() {
        let err = sanitize_db_url("postgres://host/db?sslmode=verify-ca").unwrap_err();
        assert!(err.message().contains("verify-ca"));
    }

    #[test]
    fn sanitize_rejects_sslmode_allow_with_a_clear_message() {
        // `tokio_postgres::Config`'s own parser only accepts
        // disable/prefer/require for `sslmode` (verified against the real
        // parser below, not just our own logic) — `allow` is a valid libpq
        // mode this native path can't honor, so it must fail with a message
        // pointing at what *is* supported rather than surfacing
        // tokio_postgres's generic "invalid value for option" parse error.
        let err = sanitize_db_url("postgres://host/db?sslmode=allow").unwrap_err();
        assert!(err.message().contains("sslmode=allow"));
        assert!(err.message().contains("prefer"));

        let err = sanitize_db_url("host=localhost sslmode=allow").unwrap_err();
        assert!(err.message().contains("sslmode=allow"));
    }

    #[test]
    fn sanitize_leaves_ordinary_url_unchanged_in_content() {
        let sanitized = sanitize_db_url("postgres://user:pw@host:5432/db?sslmode=require").unwrap();
        let parsed = url::Url::parse(&sanitized).unwrap();
        let pairs: std::collections::HashMap<_, _> = parsed.query_pairs().into_owned().collect();
        assert_eq!(pairs.get("sslmode").map(String::as_str), Some("require"));
    }

    #[test]
    fn sanitize_drops_all_ssl_cert_params_from_url() {
        // No password is given anywhere in this URL, which would otherwise
        // make `sanitize_db_url` consult `PGPASSWORD`/`.pgpass` — scope both
        // away so this test (which isn't about credential resolution at all)
        // can't flake against whatever's ambient in the environment, or race
        // another test's `temp_env`-scoped value for the same variable.
        temp_env::with_vars(
            [
                ("PGPASSWORD", None::<&str>),
                ("PGPASSFILE", Some("/nonexistent-pgpass-file-for-test")),
            ],
            || {
                let sanitized = sanitize_db_url(
                    "postgres://host/db?sslcert=/c.pem&sslkey=/k.pem&sslpassword=x&sslcrl=/crl.pem",
                )
                .unwrap();
                let parsed = url::Url::parse(&sanitized).unwrap();
                assert_eq!(parsed.query_pairs().count(), 0);
            },
        );
    }

    #[test]
    fn sanitize_drops_unsupported_ssl_params_from_keyword_form() {
        // See the comment on `sanitize_drops_all_ssl_cert_params_from_url`
        // for why this scopes away PGPASSWORD/PGPASSFILE.
        temp_env::with_vars(
            [
                ("PGPASSWORD", None::<&str>),
                ("PGPASSFILE", Some("/nonexistent-pgpass-file-for-test")),
            ],
            || {
                let sanitized = sanitize_db_url(
                    "host=localhost dbname=mydb sslmode=require sslrootcert=/etc/ca.pem",
                )
                .unwrap();
                // Verify against tokio_postgres's *actual* parser, not just our
                // own tokenizer's self-consistency.
                let config: tokio_postgres::Config = sanitized.parse().unwrap();
                assert_eq!(config.get_dbname(), Some("mydb"));
                assert_eq!(
                    config.get_ssl_mode(),
                    tokio_postgres::config::SslMode::Require
                );
            },
        );
    }

    #[test]
    fn sanitize_rejects_verify_full_in_keyword_form_instead_of_downgrading() {
        let err = sanitize_db_url("host=localhost dbname=mydb sslmode=verify-full").unwrap_err();
        assert!(err.message().contains("verify-full"));
    }

    #[test]
    fn sanitize_handles_quoted_values_in_keyword_form() {
        // See the comment on `sanitize_drops_all_ssl_cert_params_from_url`
        // for why this scopes away PGPASSWORD/PGPASSFILE.
        temp_env::with_vars(
            [
                ("PGPASSWORD", None::<&str>),
                ("PGPASSFILE", Some("/nonexistent-pgpass-file-for-test")),
            ],
            || {
                let sanitized = sanitize_db_url(
                    "host=localhost dbname='my db' sslmode=require sslrootcert=/etc/ca.pem",
                )
                .unwrap();
                let config: tokio_postgres::Config = sanitized.parse().unwrap();
                assert_eq!(config.get_dbname(), Some("my db"));
            },
        );
    }

    #[test]
    fn sanitize_passes_through_strings_matching_neither_shape() {
        let input = "not a valid connection string at all";
        assert_eq!(sanitize_db_url(input).unwrap(), input);
    }

    #[test]
    fn sanitize_leaves_password_unset_when_no_source_is_configured() {
        temp_env::with_vars(
            [
                ("PGPASSWORD", None::<&str>),
                ("PGPASSFILE", Some("/nonexistent-pgpass-file-for-test")),
            ],
            || {
                let sanitized = sanitize_db_url("postgres://user@host:5432/db").unwrap();
                let config: tokio_postgres::Config = sanitized.parse().unwrap();
                assert_eq!(config.get_password(), None);
            },
        );
    }

    #[test]
    fn sanitize_fills_missing_password_from_pgpassword_env() {
        temp_env::with_vars(
            [
                ("PGPASSWORD", Some("from-env")),
                ("PGPASSFILE", Some("/nonexistent-pgpass-file-for-test")),
            ],
            || {
                let sanitized = sanitize_db_url("postgres://user@host:5432/db").unwrap();
                let config: tokio_postgres::Config = sanitized.parse().unwrap();
                assert_eq!(config.get_password(), Some(b"from-env".as_slice()));
            },
        );
    }

    #[test]
    fn sanitize_does_not_override_an_explicit_password_with_pgpassword_env() {
        temp_env::with_var("PGPASSWORD", Some("from-env"), || {
            let sanitized = sanitize_db_url("postgres://user:explicit@host:5432/db").unwrap();
            let config: tokio_postgres::Config = sanitized.parse().unwrap();
            assert_eq!(config.get_password(), Some(b"explicit".as_slice()));
        });
    }

    #[test]
    fn sanitize_fills_missing_password_from_pgpass_file() {
        let dir = tempfile::tempdir().unwrap();
        let pgpass_path = dir.path().join("pgpass");
        std::fs::write(&pgpass_path, "host:5432:db:user:from-pgpass\n").unwrap();
        set_pgpass_permissions_safe(&pgpass_path);
        temp_env::with_vars(
            [
                ("PGPASSWORD", None::<&str>),
                ("PGPASSFILE", Some(pgpass_path.to_str().unwrap())),
            ],
            || {
                let sanitized = sanitize_db_url("postgres://user@host:5432/db").unwrap();
                let config: tokio_postgres::Config = sanitized.parse().unwrap();
                assert_eq!(config.get_password(), Some(b"from-pgpass".as_slice()));
            },
        );
    }

    #[test]
    #[cfg(unix)]
    fn pgpass_file_with_insecure_permissions_is_ignored() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let pgpass_path = dir.path().join("pgpass");
        std::fs::write(&pgpass_path, "*:*:*:*:insecure-password\n").unwrap();
        std::fs::set_permissions(&pgpass_path, std::fs::Permissions::from_mode(0o644)).unwrap();
        temp_env::with_vars(
            [
                ("PGPASSWORD", None::<&str>),
                ("PGPASSFILE", Some(pgpass_path.to_str().unwrap())),
            ],
            || {
                let sanitized = sanitize_db_url("postgres://user@host:5432/db").unwrap();
                let config: tokio_postgres::Config = sanitized.parse().unwrap();
                assert_eq!(config.get_password(), None);
            },
        );
    }

    #[test]
    fn pgpass_lookup_matches_wildcards_and_skips_non_matching_lines() {
        let contents = "otherhost:5432:otherdb:otheruser:wrong\n*:*:*:user:right\n";
        assert_eq!(
            pgpass_lookup(contents, "host", "5432", "db", "user"),
            Some("right".to_owned())
        );
    }

    #[test]
    fn pgpass_lookup_handles_escaped_colons_and_backslashes() {
        let contents = "host:5432:db:user:pa\\:ss\\\\word\n";
        assert_eq!(
            pgpass_lookup(contents, "host", "5432", "db", "user"),
            Some("pa:ss\\word".to_owned())
        );
    }

    #[test]
    fn parse_service_file_extracts_named_section_only() {
        let contents = "# comment\n[dev]\nhost=devhost\n\n[prod]\nhost=prodhost\nport=6543\n";
        let pairs = parse_service_file(contents, "prod").unwrap();
        assert!(pairs.contains(&("host".to_owned(), "prodhost".to_owned())));
        assert!(pairs.contains(&("port".to_owned(), "6543".to_owned())));
        assert_eq!(parse_service_file(contents, "missing"), None);
    }

    #[test]
    fn sanitize_merges_service_file_params_without_overriding_explicit_ones() {
        let dir = tempfile::tempdir().unwrap();
        let service_path = dir.path().join("pg_service.conf");
        std::fs::write(
            &service_path,
            "[prod]\nhost=svchost\nport=6543\ndbname=svcdb\nuser=svcuser\npassword=svcpass\n",
        )
        .unwrap();
        temp_env::with_vars(
            [
                ("PGSERVICEFILE", Some(service_path.to_str().unwrap())),
                ("PGPASSWORD", None::<&str>),
                ("PGPASSFILE", Some("/nonexistent-pgpass-for-test")),
            ],
            || {
                // The explicit host (in the URL authority) and dbname (the URL
                // path) must win over the service file's `svchost`/`svcdb`; the
                // port, user, and password are unset explicitly, so those come
                // from the service group.
                let sanitized = sanitize_db_url("postgres://host/db?service=prod").unwrap();
                let config: tokio_postgres::Config = sanitized.parse().unwrap();
                assert_eq!(config.get_dbname(), Some("db"));
                assert_eq!(config.get_user(), Some("svcuser"));
                assert_eq!(config.get_password(), Some(b"svcpass".as_slice()));
                assert_eq!(config.get_ports(), &[6543]);
            },
        );
    }

    #[test]
    fn sanitize_merges_service_file_params_in_keyword_form_too() {
        let dir = tempfile::tempdir().unwrap();
        let service_path = dir.path().join("pg_service.conf");
        std::fs::write(&service_path, "[prod]\ndbname=svcdb\n").unwrap();
        temp_env::with_vars(
            [
                ("PGSERVICEFILE", Some(service_path.to_str().unwrap())),
                ("PGPASSWORD", None),
                ("PGPASSFILE", Some("/nonexistent-pgpass-for-test")),
            ],
            || {
                let sanitized = sanitize_db_url("host=localhost service=prod").unwrap();
                let config: tokio_postgres::Config = sanitized.parse().unwrap();
                assert_eq!(config.get_dbname(), Some("svcdb"));
            },
        );
    }

    #[test]
    fn sanitize_errors_when_named_service_is_missing_from_service_file() {
        let dir = tempfile::tempdir().unwrap();
        let service_path = dir.path().join("pg_service.conf");
        std::fs::write(&service_path, "[other]\nhost=x\n").unwrap();
        temp_env::with_var(
            "PGSERVICEFILE",
            Some(service_path.to_str().unwrap()),
            || {
                let err = sanitize_db_url("postgres://host/db?service=missing").unwrap_err();
                assert!(err.message().contains("missing"));
            },
        );
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
