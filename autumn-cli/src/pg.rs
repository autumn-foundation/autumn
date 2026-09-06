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
const UNSUPPORTED_SSL_PARAMS: &[&str] = &[
    "sslrootcert",
    "sslcert",
    "sslkey",
    "sslpassword",
    "sslcrl",
    "sslcrldir",
];

/// Drop [`UNSUPPORTED_SSL_PARAMS`] from `pairs`, and reject `sslmode=
/// verify-ca`/`verify-full` (and `sslmode=require` combined with
/// `sslrootcert`) outright rather than silently downgrading them.
///
/// `tokio_postgres` only recognizes `sslmode=disable/prefer/require` — no
/// `verify-ca`/`verify-full` — and [`tls_connector`] doesn't validate
/// certificate chains at all (see [`NoServerCertVerification`]). Silently
/// remapping a `verify-ca`/`verify-full` request down to `require` would
/// make the CLI accept *any* certificate when an operator explicitly asked
/// for identity verification — a worse security posture than the request,
/// delivered silently. Failing loudly instead means an operator relying on
/// verification finds out immediately rather than being quietly exposed.
///
/// `sslmode=require` combined with `sslrootcert` gets the same treatment for
/// the same reason: `libpq` documents that `require` auto-upgrades to
/// certificate verification when a root CA file is available (a
/// backwards-compatibility quirk specific to `require` — `prefer`/`allow`
/// never auto-upgrade even with a CA present), so silently dropping
/// `sslrootcert` here — as [`UNSUPPORTED_SSL_PARAMS`] otherwise
/// unconditionally does — would connect to any certificate exactly where an
/// operator relying on that documented behavior expects verification.
fn filter_ssl_params(
    pairs: impl Iterator<Item = (String, String)>,
) -> Result<Vec<(String, String)>, CommandError> {
    let pairs: Vec<(String, String)> = pairs.collect();
    if find_last_pair(&pairs, "sslmode") == Some("require")
        && find_pair(&pairs, "sslrootcert").is_some()
    {
        return Err(CommandError::Other(
            "sslmode=require with sslrootcert is not supported: PostgreSQL treats this \
             combination as requiring certificate verification against that CA file, but this \
             native connection path doesn't implement CA-based verification. Use sslmode=require \
             without sslrootcert to encrypt without verifying the server's certificate, or \
             sslmode=disable to connect in plaintext."
                .to_owned(),
        ));
    }
    if find_pair(&pairs, "sslcert").is_some() || find_pair(&pairs, "sslkey").is_some() {
        // `tls_connector` builds its `rustls::ClientConfig` with
        // `with_no_client_auth()` — silently dropping `sslcert`/`sslkey`
        // (and `sslpassword`, for an encrypted key) would leave a
        // `cert`/`clientcert=verify-ca` deployment that presented this
        // certificate under `psql` instead failing opaquely at the server's
        // auth layer, with nothing in this CLI's own error pointing at the
        // real cause.
        return Err(CommandError::Other(
            "sslcert/sslkey (client-certificate authentication) is not supported: this native \
             connection path doesn't load a client certificate for the TLS handshake. Configure \
             password-based authentication instead, or omit sslcert/sslkey/sslpassword."
                .to_owned(),
        ));
    }

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

/// Percent-decode `s` (e.g. `p%40ss` -> `p@ss`) — `url::Url`'s own
/// `username()`/`password()`/`path()` getters return these components
/// exactly as they appear on the wire, never decoded.
fn percent_decode(s: &str) -> String {
    percent_encoding::percent_decode_str(s)
        .decode_utf8_lossy()
        .into_owned()
}

/// Translate a JDBC-compatibility `ssl=true` query parameter into
/// `sslmode=require`, matching the compatibility note in `PostgreSQL`'s own
/// URI-format docs ("libpq-connect.html", "Connection URIs"): "for
/// compatibility with JDBC connection URIs, instances of ssl=true are
/// translated into sslmode=require." `tokio_postgres` has no `ssl` keyword
/// of its own (only `sslmode`), so an untranslated `ssl=true` would
/// otherwise be rejected outright with an `UnknownOption` error before ever
/// attempting TLS, breaking any `DATABASE_URL` carried over from a
/// JDBC-style deployment that `psql` accepted. Only the literal `true`
/// value is documented as translated — any other `ssl=` value isn't a
/// recognized `libpq` keyword either, so it's left alone to surface
/// `tokio_postgres`'s own error, same as before this translation existed.
///
/// Rewrites each `ssl` pair to `sslmode` *in place* (rather than removing it
/// and appending a new `sslmode` pair at the end) so its position in
/// `pairs` — and therefore which of it and any other explicit `sslmode=`
/// ends up as the *effective* one once `find_last_pair` and
/// `tokio_postgres`'s own last-write-wins `sslmode` semantics apply — stays
/// exactly where the operator wrote it in the original connection string.
fn translate_jdbc_ssl_param(pairs: &mut [(String, String)]) {
    for (key, value) in pairs.iter_mut() {
        if key == "ssl" && value == "true" {
            "sslmode".clone_into(key);
            "require".clone_into(value);
        }
    }
}

/// Resolve an inline `requiressl` connection parameter into `sslmode`,
/// matching `libpq`'s own `PQconninfoParse`/`fe-connect.c`: `requiressl`
/// only ever takes effect when `sslmode` *isn't set from any other source*
/// (inline, service group, or otherwise) — `requiressl=1` then becomes
/// `sslmode=require`, and `requiressl=0` (the default) or any other value
/// becomes `sslmode=prefer`. This is a real `libpq` keyword valid in both
/// connection-string forms, unlike `ssl=true` (a URI-only JDBC
/// compatibility text substitution — see [`translate_jdbc_ssl_param`],
/// which is a purely positional in-place rewrite rather than this
/// "does something else already win?" check). `tokio_postgres` has no
/// `requiressl` keyword at all, so an untranslated occurrence would
/// otherwise be rejected outright with an `UnknownOption` error before ever
/// attempting TLS — `requiressl` is therefore always removed from `pairs`
/// here, whether or not it ends up mattering.
///
/// Callers must run this *after* any service-group merge has contributed
/// its own pairs (a service-supplied `requiressl` needs resolving too) but
/// *before* [`fill_in_libpq_ssl_env_defaults`] (which checks whether
/// `sslmode` is already set to decide whether to consult
/// `PGSSLMODE`/`PGREQUIRESSL`).
fn translate_requiressl_param(pairs: &mut Vec<(String, String)>) {
    let requiressl = take_pair(pairs, "requiressl");
    if find_pair(pairs, "sslmode").is_none()
        && let Some(value) = requiressl
    {
        let sslmode = if value == "1" { "require" } else { "prefer" };
        pairs.push(("sslmode".to_owned(), sslmode.to_owned()));
    }
}

/// Parse `query` (a URL's raw, still-percent-encoded query string, e.g. from
/// [`url::Url::query`]) into `(key, value)` pairs using pure
/// percent-decoding, splitting on `&` then the first `=` in each segment —
/// exactly the grammar `tokio_postgres`'s own `UrlParser::parse_params`
/// uses. This is deliberately *not* `url::Url::query_pairs()`: that method
/// applies `application/x-www-form-urlencoded` decoding, which also turns a
/// literal `+` into a space, but `tokio_postgres`'s decoder
/// (`percent_encoding::percent_decode`, see `UrlParser::decode`) never does
/// that — so reading a query string through `query_pairs()` would silently
/// turn `application_name=ops+cli` into `ops cli` before this sanitizer
/// even got a chance to look at it.
fn parse_raw_query_pairs(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|segment| !segment.is_empty())
        .map(|segment| match segment.split_once('=') {
            Some((key, value)) => (percent_decode(key), percent_decode(value)),
            None => (percent_decode(segment), String::new()),
        })
        .collect()
}

/// Percent-encode `s` for use as a URL query key or value being handed to
/// `tokio_postgres` — not `url::Url`'s own `query_pairs_mut()`, whose
/// `form_urlencoded` serialization encodes a space as `+`, which
/// `tokio_postgres`'s own query-string decoder (pure percent-decoding, see
/// `UrlParser::decode`) would then read back as a literal `+` rather than a
/// space. `NON_ALPHANUMERIC` is broader than strictly necessary (it also
/// escapes `-`/`_`/`.`/`~`, which are already query-safe), but over-encoding
/// is harmless to a decoder that only ever percent-decodes.
fn query_value_token(s: &str) -> String {
    percent_encoding::utf8_percent_encode(s, percent_encoding::NON_ALPHANUMERIC).to_string()
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

/// Remove every `(key, value)` pair matching `key`, returning the *last*
/// one's value — matching `libpq`'s own `PQconninfoParse`, which keeps the
/// final value when a key like `service`/`passfile` repeats. Removing only
/// the first occurrence (as this used to) would leave a later duplicate
/// still in `pairs` to be re-emitted, and `tokio_postgres` understands
/// neither key at all, so that leftover would be rejected outright with an
/// `UnknownOption` error instead of `libpq`'s documented effective value
/// ever being used.
fn take_pair(pairs: &mut Vec<(String, String)>, key: &str) -> Option<String> {
    let mut result = None;
    pairs.retain(|(k, v)| {
        if k == key {
            result = Some(v.clone());
            false
        } else {
            true
        }
    });
    result
}

/// Which fields of a connection string are already set explicitly, from the
/// caller's own connection-string shape (URL structure and/or query pairs
/// for `sanitize_url_form`; plain pairs for `sanitize_keyword_form`) — used
/// by [`fill_in_libpq_env_defaults`] to know which `PG*` environment
/// variables it may still fill in without overriding anything the caller
/// already decided.
#[allow(clippy::struct_excessive_bools)] // five independent flags, not a state machine
struct ExplicitFields {
    host: bool,
    hostaddr: bool,
    port: bool,
    user: bool,
    dbname: bool,
}

/// Build an [`ExplicitFields`] by asking `is_set` about each field's key —
/// `is_set` is the caller's own "is this key already present" check
/// (`is_explicit` for `sanitize_url_form`, a plain `find_pair` lookup for
/// `sanitize_keyword_form`).
fn explicit_fields(is_set: impl Fn(&str) -> bool) -> ExplicitFields {
    ExplicitFields {
        host: is_set("host"),
        hostaddr: is_set("hostaddr"),
        port: is_set("port"),
        user: is_set("user"),
        dbname: is_set("dbname"),
    }
}

/// Fill in `host`/`hostaddr`/`port`/`user`/`dbname` from `PGHOST`/
/// `PGHOSTADDR`/`PGPORT`/`PGUSER`/`PGDATABASE` for whichever of those
/// `explicit` says aren't already set — `tokio_postgres` never consults any
/// of these environment variables on its own (unlike `psql`, which honors
/// them as documented `libpq` environment variables), so a connection
/// string that omits one of these fields and relies on the environment the
/// way `psql` did would otherwise get a hard-coded default instead (and for
/// `host`/`hostaddr` specifically, `tokio_postgres::connect` refuses to
/// connect at all if neither ends up set). (`libpq`'s further fallback to a
/// platform-default Unix socket directory when even `PGHOST` is unset isn't
/// replicated here — that default is a compile-time detail of the actual
/// `libpq` build in use, not something derivable from this crate alone.)
///
/// Returns whether it injected a `port` — the one field here with the same
/// authority-baked-default duplication risk `needs_keyword_form` already
/// guards against for a service-supplied `host`/`port` (see that comment):
/// if the URL's own authority already names a host, `tokio_postgres` has
/// already baked a default `port=5432` into the config by the time this
/// runs, so adding `PGPORT`'s value as a query parameter would sit
/// alongside that default instead of replacing it. `host`/`hostaddr`
/// injected here don't have the same risk — they only fire when the
/// authority had no host at all, so no default was ever baked in.
fn fill_in_libpq_env_defaults(
    pairs: &mut Vec<(String, String)>,
    explicit: &ExplicitFields,
) -> bool {
    // `host` and `hostaddr` are independent per-parameter fallbacks, not a
    // pair that's only filled in together — libpq lets either one be
    // explicit while the other still falls back to its own env var (e.g. an
    // explicit `host=db-name` with only `PGHOSTADDR` set connects via that
    // address while retaining `db-name` as the name used for other purposes,
    // such as certificate/auth checks).
    if !explicit.host
        && let Ok(pghost) = std::env::var("PGHOST")
    {
        pairs.push(("host".to_owned(), pghost));
    }
    if !explicit.hostaddr
        && let Ok(pghostaddr) = std::env::var("PGHOSTADDR")
    {
        pairs.push(("hostaddr".to_owned(), pghostaddr));
    }
    let injected_port = if !explicit.port
        && let Ok(pgport) = std::env::var("PGPORT")
    {
        pairs.push(("port".to_owned(), pgport));
        true
    } else {
        false
    };
    if !explicit.user
        && let Ok(pguser) = std::env::var("PGUSER")
    {
        pairs.push(("user".to_owned(), pguser));
    }
    if !explicit.dbname
        && let Ok(pgdatabase) = std::env::var("PGDATABASE")
    {
        pairs.push(("dbname".to_owned(), pgdatabase));
    }
    fill_in_simple_libpq_env_fallbacks(pairs);
    injected_port
}

/// `(connection parameter, libpq environment variable)` pairs with no
/// special parsing semantics of their own — each is just a single
/// query/keyword pair `tokio_postgres` accepts as-is, with no
/// URL-authority equivalent (unlike `host`/`port`) and no accumulation or
/// validation behavior (unlike `sslmode`/`sslrootcert`, which need
/// [`fill_in_libpq_ssl_env_defaults`]'s extra care around
/// [`filter_ssl_params`]). New libpq environment variables of this same
/// simple "one parameter, one env var" shape can be added here directly.
const SIMPLE_LIBPQ_ENV_FALLBACKS: &[(&str, &str)] = &[
    ("connect_timeout", "PGCONNECT_TIMEOUT"),
    ("target_session_attrs", "PGTARGETSESSIONATTRS"),
    ("channel_binding", "PGCHANNELBINDING"),
    ("options", "PGOPTIONS"),
    ("application_name", "PGAPPNAME"),
    ("load_balance_hosts", "PGLOADBALANCEHOSTS"),
    ("sslnegotiation", "PGSSLNEGOTIATION"),
];

/// Fill in each of [`SIMPLE_LIBPQ_ENV_FALLBACKS`] from its environment
/// variable when the connection parameter isn't already set — `pairs`
/// itself is the source of truth for "already set" here (rather than a
/// separately captured boolean) since none of these parameters have a
/// URL-authority form that could go stale the way `host`/`port` can.
fn fill_in_simple_libpq_env_fallbacks(pairs: &mut Vec<(String, String)>) {
    for (param, env_var) in SIMPLE_LIBPQ_ENV_FALLBACKS {
        if find_pair(pairs, param).is_none()
            && let Ok(value) = std::env::var(env_var)
        {
            pairs.push(((*param).to_owned(), value));
        }
    }
}

/// Fill in `sslmode`/`sslrootcert` from `PGSSLMODE`/`PGSSLROOTCERT` for
/// whichever of those isn't already set, mirroring the same
/// environment-variable fallback [`fill_in_libpq_env_defaults`] already
/// provides for `host`/`port`/`user`/`dbname` — `tokio_postgres` never reads
/// either of these on its own, so a connection string that omits them and
/// relies on the environment the way `psql` did would otherwise silently
/// connect under the default `sslmode=prefer` (no verification requested at
/// all) instead.
///
/// Must run before [`filter_ssl_params`] has had any chance to strip
/// `sslrootcert` from `pairs` (see [`UNSUPPORTED_SSL_PARAMS`]) — callers
/// call this while `pairs` still holds every source's raw, unfiltered
/// values (this connection string's own, plus any merged-in service group),
/// so `find_pair` here reliably distinguishes "no source set this" from
/// "set, but not going to survive filtering." Callers must run
/// [`filter_ssl_params`] over the fully-assembled `pairs` afterward — the
/// values injected here still need its validation (the
/// `verify-ca`/`verify-full`/`allow` rejection, and the
/// `require`+`sslrootcert` rejection) just as much as any other source's.
fn fill_in_libpq_ssl_env_defaults(pairs: &mut Vec<(String, String)>) {
    if find_pair(pairs, "sslmode").is_none()
        && let Ok(pgsslmode) = std::env::var("PGSSLMODE")
    {
        pairs.push(("sslmode".to_owned(), pgsslmode));
    }
    // `PGREQUIRESSL` is the deprecated predecessor to `PGSSLMODE`. libpq's docs: "This
    // environment variable is deprecated in favor of the PGSSLMODE variable; setting
    // both variables suppresses the effect of this one." This fires only when `sslmode`
    // is still unset after the `PGSSLMODE` check above, so an explicit `PGSSLMODE` or
    // inline `sslmode=` always wins, matching that suppression rule. `requiressl=1` is
    // documented as equivalent to `sslmode require`; `requiressl=0` is the default,
    // already `prefer`-equivalent, so it needs no translation.
    if find_pair(pairs, "sslmode").is_none() && std::env::var("PGREQUIRESSL").as_deref() == Ok("1")
    {
        pairs.push(("sslmode".to_owned(), "require".to_owned()));
    }
    if find_pair(pairs, "sslrootcert").is_none()
        && let Ok(pgsslrootcert) = std::env::var("PGSSLROOTCERT")
    {
        pairs.push(("sslrootcert".to_owned(), pgsslrootcert));
    }
    // `sslcert`/`sslkey` themselves aren't implemented (see
    // `filter_ssl_params`), but PostgreSQL documents `PGSSLCERT`/`PGSSLKEY`
    // as behaving exactly like those inline parameters — an operator relying
    // on client-certificate auth configured only through the environment
    // deserves the same clear rejection as one who wrote `sslcert=` inline,
    // not a silent connection attempt under `with_no_client_auth()`. Pulling
    // these into `pairs` here (rather than a separate check) means the one
    // rejection in `filter_ssl_params` covers every source uniformly.
    if find_pair(pairs, "sslcert").is_none()
        && let Ok(pgsslcert) = std::env::var("PGSSLCERT")
    {
        pairs.push(("sslcert".to_owned(), pgsslcert));
    }
    if find_pair(pairs, "sslkey").is_none()
        && let Ok(pgsslkey) = std::env::var("PGSSLKEY")
    {
        pairs.push(("sslkey".to_owned(), pgsslkey));
    }
}

/// Return the value of the first `(key, value)` pair matching `key`, if any.
fn find_pair<'a>(pairs: &'a [(String, String)], key: &str) -> Option<&'a str> {
    pairs
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

/// Like [`find_pair`], but returns the *last* matching pair instead of the
/// first — for a key like `sslmode` that `tokio_postgres::Config::param`
/// simply overwrites on every occurrence (unlike `host`/`port`, which
/// accumulate), the last occurrence in `pairs` is the one that actually
/// takes effect once `tokio_postgres` reparses the reassembled connection
/// string, so callers reasoning about the *effective* value of such a key
/// must use this instead of `find_pair`.
fn find_last_pair<'a>(pairs: &'a [(String, String)], key: &str) -> Option<&'a str> {
    pairs
        .iter()
        .rev()
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

/// Path to the `.pgpass` file: `passfile_override` (the connection string's
/// own `passfile=` parameter, if any) if set, else `PGPASSFILE`, else the
/// platform default `libpq` itself uses — `~/.pgpass` on Unix, or
/// `%APPDATA%\postgresql\pgpass.conf` on Windows (`PostgreSQL` docs, "The
/// Password File"; `BaseDirs::config_dir()` resolves to the roaming
/// `%APPDATA%` known folder on Windows).
#[cfg(not(windows))]
fn pgpass_file_path(passfile_override: Option<&str>) -> Option<PathBuf> {
    if let Some(path) = passfile_override {
        return Some(PathBuf::from(path));
    }
    if let Ok(path) = std::env::var("PGPASSFILE") {
        return Some(PathBuf::from(path));
    }
    Some(directories::BaseDirs::new()?.home_dir().join(".pgpass"))
}

#[cfg(windows)]
fn pgpass_file_path(passfile_override: Option<&str>) -> Option<PathBuf> {
    if let Some(path) = passfile_override {
        return Some(PathBuf::from(path));
    }
    if let Ok(path) = std::env::var("PGPASSFILE") {
        return Some(PathBuf::from(path));
    }
    Some(
        directories::BaseDirs::new()?
            .config_dir()
            .join("postgresql")
            .join("pgpass.conf"),
    )
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
const fn pgpass_permissions_ok(_path: &Path) -> bool {
    true
}

/// Look up a password for `(host, port, dbname, user)` in the `.pgpass` file
/// (or `passfile_override`, if given), if one exists and has safe
/// permissions.
fn pgpass_password(
    host: &str,
    port: &str,
    dbname: &str,
    user: &str,
    passfile_override: Option<&str>,
) -> Option<String> {
    let path = pgpass_file_path(passfile_override)?;
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
/// variable first, then the `.pgpass` file (or `passfile_override`, `libpq`'s
/// own `passfile=` connection parameter, if the caller has one).
fn resolve_password(
    host: &str,
    port: &str,
    dbname: &str,
    user: &str,
    passfile_override: Option<&str>,
) -> Option<String> {
    std::env::var("PGPASSWORD")
        .ok()
        .or_else(|| pgpass_password(host, port, dbname, user, passfile_override))
}

/// If no password is already explicit, resolve one via [`resolve_password`]
/// (using whichever of `host`/`port`/`user`/`dbname` is already known,
/// falling back to `pairs` and then the same defaults `tokio_postgres`
/// itself would use) and push it into `pairs`.
///
/// `explicit_host`/`port`/`user`/`dbname` are `None` for
/// [`sanitize_keyword_form`], which has no URL-structure concept of
/// "explicit" separate from `pairs` itself; [`sanitize_url_form`] passes its
/// own snapshots so a value already decided from the URL's structure (not
/// just its query string) is preferred over a same-named `pairs` entry.
fn fill_in_missing_password(
    pairs: &mut Vec<(String, String)>,
    is_explicit_password: bool,
    explicit_host: Option<&str>,
    explicit_port: Option<&str>,
    explicit_user: Option<&str>,
    explicit_dbname: Option<&str>,
    passfile: Option<&str>,
) {
    // The `find_last_pair` fallbacks here (rather than `find_pair`) matter
    // whenever a caller passes `None` for the corresponding `explicit_*`
    // (as `sanitize_keyword_form` always does) — `tokio_postgres::Config`'s
    // own parser overwrites each of these scalar fields on every
    // occurrence, so a connection string repeating one of these keys (e.g.
    // `user=alice user=bob`) actually connects as the *last* occurrence,
    // not the first `find_pair` would return.
    let host = explicit_host
        .map(str::to_owned)
        .or_else(|| find_last_pair(pairs, "host").map(str::to_owned))
        .or_else(|| find_last_pair(pairs, "hostaddr").map(str::to_owned))
        .unwrap_or_else(|| "localhost".to_owned());
    let port = explicit_port
        .map(str::to_owned)
        .or_else(|| find_last_pair(pairs, "port").map(str::to_owned))
        .unwrap_or_else(|| "5432".to_owned());
    let user = explicit_user
        .map(str::to_owned)
        .or_else(|| find_last_pair(pairs, "user").map(str::to_owned))
        .unwrap_or_else(current_os_user);
    let dbname = explicit_dbname
        .map(str::to_owned)
        .or_else(|| find_last_pair(pairs, "dbname").map(str::to_owned))
        .unwrap_or_else(|| user.clone());

    if !is_explicit_password
        && let Some(password) = resolve_password(&host, &port, &dbname, &user, passfile)
    {
        pairs.push(("password".to_owned(), password));
    }
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

/// Path to the service file: `PGSERVICEFILE` if set, else the platform
/// default `libpq` itself uses — `~/.pg_service.conf` on Unix, or
/// `%APPDATA%\postgresql\.pg_service.conf` on Windows (`PostgreSQL` docs, "The
/// Connection Service File" — unlike `.pgpass`, which drops its leading dot
/// to become `pgpass.conf` on Windows, the service file keeps its dot on both
/// platforms; see [`pgpass_file_path`] for why `BaseDirs::config_dir()` is
/// the right call on Windows). See [`system_service_file_path`] for the
/// system-wide service file `libpq` also consults.
#[cfg(not(windows))]
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

#[cfg(windows)]
fn service_file_path() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("PGSERVICEFILE") {
        return Some(PathBuf::from(path));
    }
    Some(
        directories::BaseDirs::new()?
            .config_dir()
            .join("postgresql")
            .join(".pg_service.conf"),
    )
}

/// Path to the system-wide service file, if `PGSYSCONFDIR` is set —
/// `libpq` also falls back to a compiled-in default sysconfdir when this
/// isn't set (see `pg_config --sysconfdir`), but that default is a
/// build-time detail of the actual `libpq`/`postgres` install in use, not
/// something derivable from this crate alone (same precedent as
/// [`fill_in_libpq_env_defaults`] not replicating `libpq`'s compiled-in
/// default Unix socket directory).
fn system_service_file_path() -> Option<PathBuf> {
    std::env::var("PGSYSCONFDIR")
        .ok()
        .map(|dir| PathBuf::from(dir).join("pg_service.conf"))
}

/// Load the `[service_name]` group's parameters, checking the per-user
/// service file first and falling back to the system-wide one (via
/// [`system_service_file_path`]) if the per-user file doesn't define this
/// service — matching `libpq`'s documented precedence ("If the same service
/// name exists in both the user and the system file, the user file takes
/// precedence", <https://www.postgresql.org/docs/current/libpq-pgservice.html>).
/// Errors clearly if neither file defines it — silently ignoring an
/// explicitly requested `service=` would leave a connection missing
/// parameters the operator expected to be set.
fn load_service(service_name: &str) -> Result<Vec<(String, String)>, CommandError> {
    let mut checked_paths = Vec::new();

    if let Some(path) = service_file_path() {
        if let Ok(contents) = std::fs::read_to_string(&path)
            && let Some(pairs) = parse_service_file(&contents, service_name)
        {
            return Ok(pairs);
        }
        checked_paths.push(path);
    }

    if let Some(path) = system_service_file_path() {
        if let Ok(contents) = std::fs::read_to_string(&path)
            && let Some(pairs) = parse_service_file(&contents, service_name)
        {
            return Ok(pairs);
        }
        checked_paths.push(path);
    }

    if checked_paths.is_empty() {
        return Err(CommandError::Other(format!(
            "service={service_name} was requested but no service file could be located \
             (set PGSERVICEFILE or PGSYSCONFDIR, or create ~/.pg_service.conf)"
        )));
    }

    Err(CommandError::Other(format!(
        "service \"{service_name}\" was not found in {}",
        checked_paths
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(" or ")
    )))
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
    // [`filter_ssl_params`] isn't applied yet — see the comment where it
    // finally runs, near the end of this function, for why: validating each
    // source (this URL's own query string, then a merged-in service group)
    // separately, as this used to do, can't see a `require`+`sslrootcert`
    // combination split across two sources.
    let mut pairs: Vec<(String, String)> = parse_raw_query_pairs(url.query().unwrap_or(""));
    translate_jdbc_ssl_param(&mut pairs);

    // Capture, once and up front, what the URL's own structure says explicitly, as
    // opposed to its query string. `host` and `port` need this snapshot below to detect
    // whether a later merge can be folded back into the query string at all (see the
    // comment by `needs_keyword_form`). `username()`, `password()`, and `path()` are
    // percent-encoded as `url::Url` stores them, and so is `host_str()` for the opaque
    // host case `tokio_postgres`'s Unix-socket-path URL form uses
    // (`postgresql://%2Fvar%2Frun%2Fpostgresql/db`). That is fine when the whole URL
    // goes back to `tokio_postgres`, which percent-decodes itself, but
    // [`keyword_value_token`] has no percent-encoding concept, so these must be decoded
    // before any of them can become a keyword-form value.
    //
    // `user`, `password`, and `dbname` are each a single field `tokio_postgres`'s URL
    // parser overwrites — unlike `host` and `port`, which accumulate — so when a URL
    // names one via userinfo or path and separately as a query parameter, the query
    // parameter takes effect: it is parsed later, and last write wins. The query pair is
    // therefore checked first, not as a fallback. Getting this backwards would not break
    // the connection, since the unmodified URL still reparses, but it would make the
    // `.pgpass` lookup below match the wrong identity. `find_last_pair`, not `find_pair`,
    // for the same reason: a query string repeating one of these keys
    // (`?user=alice&user=bob`) also has its last occurrence take effect.
    let explicit_user = find_last_pair(&pairs, "user")
        .map(str::to_owned)
        .or_else(|| {
            Some(url.username())
                .filter(|u| !u.is_empty())
                .map(percent_decode)
        });
    let explicit_password = find_last_pair(&pairs, "password")
        .map(str::to_owned)
        .or_else(|| url.password().map(percent_decode));
    // `host` and `port` get the same query-pair-first precedence as `user`, `password`,
    // and `dbname` above, for the same reason but by a different mechanism. They are
    // `Vec`s that accumulate rather than overwrite in `tokio_postgres::Config`, so a
    // query `host=`/`port=` alongside a non-empty authority host does not override it
    // the way query `user=` overrides userinfo. That is the bug `needs_keyword_form`
    // below rebuilds the connection string to work around, so that the only host and
    // port surviving into the final keyword-form output are the query ones, matching
    // libpq's own `PQconninfoParse` semantics. The `.pgpass` lookup must use that
    // effective value, not the authority snapshot, or it matches an entry keyed to a
    // host and port the connection does not use.
    let explicit_host = find_last_pair(&pairs, "host")
        .map(str::to_owned)
        .or_else(|| url.host_str().filter(|h| !h.is_empty()).map(percent_decode));
    let explicit_port = find_last_pair(&pairs, "port")
        .map(str::to_owned)
        .or_else(|| url.port().map(|p| p.to_string()));
    let explicit_dbname = find_last_pair(&pairs, "dbname")
        .map(str::to_owned)
        .or_else(|| {
            Some(url.path())
                .filter(|p| p.len() > 1)
                .map(|p| percent_decode(p.trim_start_matches('/')))
        });

    // Checks the *current* `pairs` (not just the `explicit_*` snapshot
    // taken above) for every key, not only the catch-all arm below — a
    // service group merged in earlier in this function's own control flow
    // (see the loop right after this) lands in `pairs`, and a later
    // re-check (e.g. before consulting PGPASSWORD/.pgpass) must see that
    // merge too, or an ambient fallback would look past an already-decided
    // value and silently override it.
    let is_explicit = |pairs: &[(String, String)], key: &str| match key {
        "user" => explicit_user.is_some() || find_pair(pairs, "user").is_some(),
        "password" => explicit_password.is_some() || find_pair(pairs, "password").is_some(),
        "host" => explicit_host.is_some() || find_pair(pairs, "host").is_some(),
        "port" => explicit_port.is_some() || find_pair(pairs, "port").is_some(),
        "dbname" => explicit_dbname.is_some() || find_pair(pairs, "dbname").is_some(),
        _ => find_pair(pairs, key).is_some(),
    };

    // Whether the service merge below fills in a `host` or `port` that was not already
    // explicit — see the comment where this is used for why that forces a keyword-form
    // rebuild.
    //
    // A `port=` given as a query parameter, rather than as part of `host:port`, needs
    // the same treatment as soon as the authority names any host: `tokio_postgres`'s URL
    // parser bakes a port into the config for every authority host — its own explicit
    // port, or a default of 5432 — and separately appends whatever the query string's
    // `port=` says, producing two port entries `tokio_postgres::connect` then rejects
    // with "invalid number of ports", even for an ordinary URL `psql` always accepted.
    //
    // A query-string `host=` alongside a non-empty authority host needs the same
    // treatment for a related but distinct reason: libpq's `PQconninfoParse` makes the
    // named `host=` the sole effective host and discards the authority host entirely,
    // while `tokio_postgres`'s URL parser parses the authority host first and then
    // appends the query host as an extra entry, producing a two-host list with the
    // authority tried first. Rebuilding as keyword form sidesteps this: its `pairs`-only
    // output never carries the authority host, only what ended up in `pairs`.
    let mut needs_keyword_form = url.host_str().is_some_and(|h| !h.is_empty())
        && (find_pair(&pairs, "port").is_some() || find_pair(&pairs, "host").is_some());

    let service_name = take_pair(&mut pairs, "service").or_else(|| std::env::var("PGSERVICE").ok());
    if let Some(service_name) = service_name {
        for (key, value) in load_service(&service_name)? {
            if !is_explicit(&pairs, &key) {
                needs_keyword_form |= key == "host" || key == "port";
                pairs.push((key, value));
            }
        }
    }

    // Must run after the service merge above (a service-supplied
    // `requiressl` needs resolving too) but before
    // `fill_in_libpq_ssl_env_defaults` below (which checks whether
    // `sslmode` is already set).
    translate_requiressl_param(&mut pairs);

    // `tokio_postgres` has no `passfile` key of its own — it must always be
    // consumed here and stripped, regardless of whether it ends up mattering
    // (i.e. even if a password is already explicit), or `tokio_postgres`
    // would reject the connection string outright with an UnknownOption
    // error over a parameter it was never going to need anyway.
    let passfile = take_pair(&mut pairs, "passfile");

    let explicit_fields = explicit_fields(|key| is_explicit(&pairs, key));
    needs_keyword_form |= fill_in_libpq_env_defaults(&mut pairs, &explicit_fields);
    fill_in_libpq_ssl_env_defaults(&mut pairs);

    // `tokio_postgres`'s URL parser special-cases a query-string `host=`
    // through a single `Config::host()` call rather than splitting it on
    // commas the way keyword form (and a multi-host *authority* like
    // `host1:5432,host2:5432`) do — so a comma-separated `host` ending up
    // in the query string, from any source (inline, a merged-in service
    // group, or an injected `PGHOST`), needs the same keyword-form rebuild
    // rather than being handed back as-is.
    needs_keyword_form |= find_pair(&pairs, "host").is_some_and(|h| h.contains(','));

    let is_explicit_password = is_explicit(&pairs, "password");
    fill_in_missing_password(
        &mut pairs,
        is_explicit_password,
        explicit_host.as_deref(),
        explicit_port.as_deref(),
        explicit_user.as_deref(),
        explicit_dbname.as_deref(),
        passfile.as_deref(),
    );

    // The single point where every source (this URL's own query string, a
    // merged-in service group, and the `PGSSLMODE`/`PGSSLROOTCERT` env
    // fallback above) has now contributed its `sslmode`/`sslrootcert`, so a
    // `require`+`sslrootcert` combination split across sources is visible
    // here even though it wasn't at any single earlier point.
    pairs = filter_ssl_params(pairs.into_iter())?;

    if needs_keyword_form {
        // `tokio_postgres`'s URL-form parser bakes a default `port=5432` into the
        // config as soon as the authority names any host, even with no port given, and
        // both `host` and `port` are appended to a list rather than overwritten. So
        // folding a service-provided host or port back in as a query parameter here
        // would not override that implicit default; it would sit alongside it as a
        // second entry. Keyword form has no such authority-parsing special case — every
        // key there is one we added ourselves — so falling back to it for this case
        // sidesteps the duplication.
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
        // Not `query_pairs_mut()` — its `append_pair` serializes with
        // `form_urlencoded`, which encodes a space as `+`. `tokio_postgres`'s
        // own query-string decoder only ever percent-decodes (see
        // `UrlParser::decode`) and never treats `+` as a space, so a `+`
        // produced here would come back out as a literal `+` instead of the
        // space it was meant to represent (this affects any value with a
        // space, e.g. `options=-c search_path=...` from `PGOPTIONS`).
        let query = pairs
            .iter()
            .map(|(key, value)| format!("{}={}", query_value_token(key), query_value_token(value)))
            .collect::<Vec<_>>()
            .join("&");
        url.set_query(Some(&query));
    }
    Ok(url.into())
}

/// The keyword/value-form half of [`sanitize_db_url`].
fn sanitize_keyword_form(mut pairs: Vec<(String, String)>) -> Result<String, CommandError> {
    // See the identical comment in `sanitize_url_form` — [`filter_ssl_params`]
    // isn't applied until every source has contributed, near the end of
    // this function.
    let service_name = take_pair(&mut pairs, "service").or_else(|| std::env::var("PGSERVICE").ok());
    if let Some(service_name) = service_name {
        for (key, value) in load_service(&service_name)? {
            if find_pair(&pairs, &key).is_none() {
                pairs.push((key, value));
            }
        }
    }

    // See the identical comment in `sanitize_url_form` on why this runs
    // after the service merge but before `fill_in_libpq_ssl_env_defaults`.
    translate_requiressl_param(&mut pairs);

    // See the identical comment in `sanitize_url_form` — `passfile` must
    // always be consumed and stripped, not just when it turns out to matter.
    let passfile = take_pair(&mut pairs, "passfile");

    let explicit_fields = explicit_fields(|key| find_pair(&pairs, key).is_some());
    fill_in_libpq_env_defaults(&mut pairs, &explicit_fields);
    fill_in_libpq_ssl_env_defaults(&mut pairs);
    let mut pairs = filter_ssl_params(pairs.into_iter())?;

    let is_explicit_password = find_pair(&pairs, "password").is_some();
    fill_in_missing_password(
        &mut pairs,
        is_explicit_password,
        None,
        None,
        None,
        None,
        passfile.as_deref(),
    );

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
        // `sslmode=prefer` (not `require`) so this stays a plain "unsupported
        // param dropped" case rather than tripping the separate
        // require+sslrootcert rejection tested below. Scopes away
        // PGSSLCERT/PGSSLKEY (in addition to the connection string's own
        // lack of sslcert/sslkey) so an ambient/racing value set by another
        // test's `temp_env` call can't trip the sslcert/sslkey rejection
        // instead of the plain-drop path this test is actually about.
        temp_env::with_vars(
            [("PGSSLCERT", None::<&str>), ("PGSSLKEY", None::<&str>)],
            || {
                let sanitized = sanitize_db_url(
                    "postgres://user:pw@host/db?sslmode=prefer&sslrootcert=/etc/ca.pem&connect_timeout=5",
                )
                .unwrap();
                let parsed = url::Url::parse(&sanitized).unwrap();
                let pairs: std::collections::HashMap<_, _> =
                    parsed.query_pairs().into_owned().collect();
                assert_eq!(pairs.get("sslmode").map(String::as_str), Some("prefer"));
                assert!(!pairs.contains_key("sslrootcert"));
                assert_eq!(pairs.get("connect_timeout").map(String::as_str), Some("5"));
            },
        );
    }

    #[test]
    fn sanitize_rejects_require_with_sslrootcert_in_url_form() {
        // Real libpq/psql treats `sslmode=require` combined with a root CA
        // file as requiring certificate verification against that CA (a
        // documented backwards-compatibility quirk specific to `require`).
        // This native path doesn't implement CA verification, so silently
        // dropping `sslrootcert` and connecting via `NoServerCertVerification`
        // would be a real security downgrade from what an operator relying on
        // that combination expects — it must fail loudly instead.
        let err = sanitize_db_url("postgres://host/db?sslmode=require&sslrootcert=/etc/ca.pem")
            .unwrap_err();
        assert!(err.message().contains("sslrootcert"));
        assert!(err.message().contains("require"));
    }

    #[test]
    fn sanitize_rejects_require_with_sslrootcert_in_url_form_regardless_of_param_order() {
        let err = sanitize_db_url("postgres://host/db?sslrootcert=/etc/ca.pem&sslmode=require")
            .unwrap_err();
        assert!(err.message().contains("sslrootcert"));
    }

    #[test]
    fn sanitize_rejects_require_with_sslrootcert_when_an_earlier_sslmode_shadows_it() {
        // `sslmode` is an overwrite-semantics field in `tokio_postgres::Config`:
        // `Config::param`'s "sslmode" arm calls `self.ssl_mode(mode)` unconditionally
        // each time, so a later `sslmode=` in the same connection string wins, exactly
        // like `user`, `password`, and `dbname`. `find_pair`'s first-match semantics
        // would instead see the first `sslmode=prefer` here, miss the
        // require-plus-sslrootcert combination, and let `sslrootcert` be dropped by the
        // unconditional `UNSUPPORTED_SSL_PARAMS` loop while both `sslmode` values
        // survive. The final string `tokio_postgres` reparses would then connect under
        // the effective `sslmode=require` with no certificate verification at all —
        // exactly the silent downgrade this check exists to prevent.
        let err = sanitize_db_url(
            "postgres://host/db?sslmode=prefer&sslrootcert=/etc/ca.pem&sslmode=require",
        )
        .unwrap_err();
        assert!(err.message().contains("sslrootcert"));
        assert!(err.message().contains("require"));
    }

    #[test]
    fn sanitize_allows_prefer_with_sslrootcert_in_url_form() {
        // The require+sslrootcert auto-upgrade quirk is specific to
        // `require` — `prefer` (and `allow`) never auto-upgrade even with a
        // CA file present, so this combination must still just drop the
        // unsupported `sslrootcert` param rather than erroring.
        //
        // Scopes away PGSSLCERT/PGSSLKEY so an ambient/racing value can't trip
        // the sslcert/sslkey rejection — see the comment on
        // `sanitize_rejects_require_with_sslrootcert_in_url_form`. Without it
        // `sanitize_rejects_pgsslcert_env_var_in_url_form` and
        // `sanitize_does_not_override_explicit_sslcert_rejection_message_source`,
        // which set PGSSLCERT to a value, make this `unwrap()` panic with
        // "sslcert/sslkey (client-certificate authentication) is not
        // supported" whenever they overlap it.
        temp_env::with_vars(
            [("PGSSLCERT", None::<&str>), ("PGSSLKEY", None::<&str>)],
            || {
                let sanitized =
                    sanitize_db_url("postgres://host/db?sslmode=prefer&sslrootcert=/etc/ca.pem")
                        .unwrap();
                let config: tokio_postgres::Config = sanitized.parse().unwrap();
                assert_eq!(
                    config.get_ssl_mode(),
                    tokio_postgres::config::SslMode::Prefer
                );
            },
        );
    }

    #[test]
    fn sanitize_uses_pgsslmode_env_var_when_url_has_no_inline_sslmode() {
        // `PGSSLMODE` is documented to behave the same as an inline
        // `sslmode=` parameter — a bare connection string relying on it (the
        // way `psql` did) must not silently connect under the default
        // `prefer` instead.
        temp_env::with_var("PGSSLMODE", Some("require"), || {
            let sanitized = sanitize_db_url("postgres://host/db").unwrap();
            let config: tokio_postgres::Config = sanitized.parse().unwrap();
            assert_eq!(
                config.get_ssl_mode(),
                tokio_postgres::config::SslMode::Require
            );
        });
    }

    #[test]
    fn sanitize_uses_pgsslmode_env_var_in_keyword_form_too() {
        temp_env::with_var("PGSSLMODE", Some("require"), || {
            let sanitized = sanitize_db_url("host=host dbname=db").unwrap();
            let config: tokio_postgres::Config = sanitized.parse().unwrap();
            assert_eq!(
                config.get_ssl_mode(),
                tokio_postgres::config::SslMode::Require
            );
        });
    }

    #[test]
    fn sanitize_prefers_inline_sslmode_over_pgsslmode_env_var() {
        temp_env::with_var("PGSSLMODE", Some("require"), || {
            let sanitized = sanitize_db_url("postgres://host/db?sslmode=disable").unwrap();
            let config: tokio_postgres::Config = sanitized.parse().unwrap();
            assert_eq!(
                config.get_ssl_mode(),
                tokio_postgres::config::SslMode::Disable
            );
        });
    }

    #[test]
    fn sanitize_rejects_verify_full_from_pgsslmode_env_var() {
        // Scopes away PGSSLCERT/PGSSLKEY — see the comment on
        // `sanitize_rejects_verify_ca_in_url_form_instead_of_downgrading`.
        temp_env::with_vars(
            [
                ("PGSSLMODE", Some("verify-full")),
                ("PGSSLCERT", None::<&str>),
                ("PGSSLKEY", None::<&str>),
            ],
            || {
                let err = sanitize_db_url("postgres://host/db").unwrap_err();
                assert!(err.message().contains("verify-full"));
            },
        );
    }

    #[test]
    fn sanitize_uses_pgrequiressl_env_var_when_url_has_no_inline_sslmode() {
        // `PGREQUIRESSL` is a documented (deprecated in favor of
        // `PGSSLMODE`) libpq environment variable: "PGREQUIRESSL behaves
        // the same as the requiressl connection parameter" — and
        // `requiressl=1` is documented as "equivalent to sslmode require".
        // A bare connection string relying on the legacy variable (the way
        // `psql` did) must not silently connect under the default `prefer`
        // instead.
        //
        // Scopes away PGSSLROOTCERT/PGSSLCERT/PGSSLKEY so an ambient/racing
        // value can't trip the require+sslrootcert or sslcert/sslkey
        // rejections instead of exercising the translation this test is
        // actually about.
        temp_env::with_vars(
            [
                ("PGSSLMODE", None::<&str>),
                ("PGREQUIRESSL", Some("1")),
                ("PGSSLROOTCERT", None::<&str>),
                ("PGSSLCERT", None::<&str>),
                ("PGSSLKEY", None::<&str>),
            ],
            || {
                let sanitized = sanitize_db_url("postgres://host/db").unwrap();
                let config: tokio_postgres::Config = sanitized.parse().unwrap();
                assert_eq!(
                    config.get_ssl_mode(),
                    tokio_postgres::config::SslMode::Require
                );
            },
        );
    }

    #[test]
    fn sanitize_ignores_pgrequiressl_zero() {
        // `requiressl=0` is documented as the default, negotiated
        // (`sslmode=prefer`-equivalent) behavior — not a value that needs
        // translating into an explicit `sslmode=`. Scopes away
        // PGSSLCERT/PGSSLKEY — see the comment on
        // `sanitize_uses_pgrequiressl_env_var_when_url_has_no_inline_sslmode`.
        temp_env::with_vars(
            [
                ("PGSSLMODE", None::<&str>),
                ("PGREQUIRESSL", Some("0")),
                ("PGSSLCERT", None::<&str>),
                ("PGSSLKEY", None::<&str>),
            ],
            || {
                let sanitized = sanitize_db_url("postgres://host/db").unwrap();
                let config: tokio_postgres::Config = sanitized.parse().unwrap();
                assert_eq!(
                    config.get_ssl_mode(),
                    tokio_postgres::config::SslMode::Prefer
                );
            },
        );
    }

    #[test]
    fn sanitize_suppresses_pgrequiressl_when_pgsslmode_is_also_set() {
        // libpq docs: "setting both variables suppresses the effect of
        // this one" -- PGSSLMODE must win outright, not merely take
        // precedence when both would otherwise conflict. Scopes away
        // PGSSLCERT/PGSSLKEY — see the comment on
        // `sanitize_uses_pgrequiressl_env_var_when_url_has_no_inline_sslmode`.
        temp_env::with_vars(
            [
                ("PGSSLMODE", Some("disable")),
                ("PGREQUIRESSL", Some("1")),
                ("PGSSLCERT", None::<&str>),
                ("PGSSLKEY", None::<&str>),
            ],
            || {
                let sanitized = sanitize_db_url("postgres://host/db").unwrap();
                let config: tokio_postgres::Config = sanitized.parse().unwrap();
                assert_eq!(
                    config.get_ssl_mode(),
                    tokio_postgres::config::SslMode::Disable
                );
            },
        );
    }

    #[test]
    fn sanitize_prefers_inline_sslmode_over_pgrequiressl_env_var() {
        // Scopes away PGSSLCERT/PGSSLKEY — see the comment on
        // `sanitize_uses_pgrequiressl_env_var_when_url_has_no_inline_sslmode`.
        temp_env::with_vars(
            [
                ("PGSSLMODE", None::<&str>),
                ("PGREQUIRESSL", Some("1")),
                ("PGSSLCERT", None::<&str>),
                ("PGSSLKEY", None::<&str>),
            ],
            || {
                let sanitized = sanitize_db_url("postgres://host/db?sslmode=disable").unwrap();
                let config: tokio_postgres::Config = sanitized.parse().unwrap();
                assert_eq!(
                    config.get_ssl_mode(),
                    tokio_postgres::config::SslMode::Disable
                );
            },
        );
    }

    #[test]
    fn sanitize_rejects_require_pgsslmode_combined_with_pgsslrootcert_env_var() {
        // The literal scenario libpq documents: an operator relying entirely
        // on environment variables (the way `psql` did) for TLS policy,
        // with no inline `sslmode=`/`sslrootcert=` at all. This must get the
        // same loud rejection as the inline-string case, not silently
        // connect under the default `prefer` with no verification.
        temp_env::with_vars(
            [
                ("PGSSLMODE", Some("require")),
                ("PGSSLROOTCERT", Some("/etc/ca.pem")),
            ],
            || {
                let err = sanitize_db_url("postgres://host/db").unwrap_err();
                assert!(err.message().contains("sslrootcert"));
            },
        );
    }

    #[test]
    fn sanitize_rejects_require_pgsslmode_combined_with_pgsslrootcert_env_var_in_keyword_form() {
        temp_env::with_vars(
            [
                ("PGSSLMODE", Some("require")),
                ("PGSSLROOTCERT", Some("/etc/ca.pem")),
            ],
            || {
                let err = sanitize_db_url("host=host dbname=db").unwrap_err();
                assert!(err.message().contains("sslrootcert"));
            },
        );
    }

    #[test]
    fn sanitize_ignores_pgsslrootcert_env_var_when_effective_sslmode_is_not_require() {
        // The require+sslrootcert quirk is specific to `require` — a
        // `PGSSLROOTCERT` env var with no `require` in play anywhere (inline
        // or via `PGSSLMODE`) must just be dropped like any other unsupported
        // cert param, not rejected.
        temp_env::with_vars(
            [
                ("PGSSLMODE", None::<&str>),
                ("PGSSLROOTCERT", Some("/etc/ca.pem")),
            ],
            || {
                let sanitized = sanitize_db_url("postgres://host/db?sslmode=prefer").unwrap();
                let config: tokio_postgres::Config = sanitized.parse().unwrap();
                assert_eq!(
                    config.get_ssl_mode(),
                    tokio_postgres::config::SslMode::Prefer
                );
            },
        );
    }

    #[test]
    fn sanitize_does_not_override_explicit_inline_sslrootcert_with_pgsslrootcert_env_var() {
        // An inline `sslrootcert=` (even one that ends up dropped because
        // `sslmode` isn't `require`) still represents an explicit choice —
        // `PGSSLROOTCERT` must not be injected on top of it.
        temp_env::with_var("PGSSLROOTCERT", Some("/etc/env-ca.pem"), || {
            let sanitized =
                sanitize_db_url("postgres://host/db?sslmode=prefer&sslrootcert=/etc/inline-ca.pem")
                    .unwrap();
            let config: tokio_postgres::Config = sanitized.parse().unwrap();
            assert_eq!(
                config.get_ssl_mode(),
                tokio_postgres::config::SslMode::Prefer
            );
        });
    }

    #[test]
    fn sanitize_rejects_verify_full_in_url_form_instead_of_downgrading() {
        // A silent downgrade to `require` would make the CLI accept any
        // certificate when an operator explicitly asked for verification —
        // this must fail loudly instead. Scopes away PGSSLCERT/PGSSLKEY —
        // see the comment on
        // `sanitize_rejects_verify_ca_in_url_form_instead_of_downgrading`.
        temp_env::with_vars(
            [("PGSSLCERT", None::<&str>), ("PGSSLKEY", None::<&str>)],
            || {
                let err = sanitize_db_url("postgres://host/db?sslmode=verify-full").unwrap_err();
                assert!(err.message().contains("verify-full"));
            },
        );
    }

    #[test]
    fn sanitize_rejects_verify_ca_in_url_form_instead_of_downgrading() {
        // Scopes away PGSSLCERT/PGSSLKEY so an ambient/racing value can't
        // trip the sslcert/sslkey rejection first (checked before the
        // sslmode one) and produce a message that doesn't mention
        // "verify-ca" at all.
        temp_env::with_vars(
            [("PGSSLCERT", None::<&str>), ("PGSSLKEY", None::<&str>)],
            || {
                let err = sanitize_db_url("postgres://host/db?sslmode=verify-ca").unwrap_err();
                assert!(err.message().contains("verify-ca"));
            },
        );
    }

    #[test]
    fn sanitize_translates_jdbc_ssl_true_to_sslmode_require() {
        // PostgreSQL's own URI-format docs (libpq-connect.html, "Connection
        // URIs"): "for compatibility with JDBC connection URIs, instances
        // of ssl=true are translated into sslmode=require." `tokio_postgres`
        // has no `ssl` keyword of its own (only `sslmode`), so an
        // untranslated `ssl=true` would otherwise be rejected outright by
        // its parser with an UnknownOption error before ever attempting
        // TLS, breaking any `DATABASE_URL` carried over from a JDBC-style
        // deployment that `psql` accepted.
        //
        // Scopes away PGSSLROOTCERT/PGSSLCERT/PGSSLKEY so an ambient/racing
        // value set by another test's `temp_env` call can't trip the
        // require+sslrootcert or sslcert/sslkey rejections instead of
        // exercising the translation this test is actually about.
        temp_env::with_vars(
            [
                ("PGSSLROOTCERT", None::<&str>),
                ("PGSSLCERT", None::<&str>),
                ("PGSSLKEY", None::<&str>),
            ],
            || {
                let sanitized = sanitize_db_url("postgres://host/db?ssl=true").unwrap();
                let config: tokio_postgres::Config = sanitized.parse().unwrap();
                assert_eq!(
                    config.get_ssl_mode(),
                    tokio_postgres::config::SslMode::Require
                );
            },
        );
    }

    #[test]
    fn sanitize_does_not_let_jdbc_ssl_true_override_an_explicit_sslmode() {
        // `sslmode` is itself an overwrite-semantics field (see
        // `find_last_pair`) -- translating `ssl=true` in place (rather than
        // just appending an `sslmode=require` at the end regardless of
        // position) preserves whichever of the two actually comes last, the
        // same "last one wins" rule that already governs a connection
        // string with two explicit `sslmode=` occurrences.
        let sanitized = sanitize_db_url("postgres://host/db?ssl=true&sslmode=disable").unwrap();
        let config: tokio_postgres::Config = sanitized.parse().unwrap();
        assert_eq!(
            config.get_ssl_mode(),
            tokio_postgres::config::SslMode::Disable
        );
    }

    #[test]
    fn sanitize_leaves_non_true_ssl_value_alone() {
        // Only the literal `ssl=true` is a documented JDBC compatibility
        // translation -- `sanitize_db_url` doesn't connect or otherwise
        // validate parameters it doesn't specifically recognize, so any
        // other `ssl=` value passes through unchanged here and only fails
        // once `tokio_postgres` itself tries to parse it (it has no `ssl`
        // keyword at all), the same as before this translation existed.
        let sanitized = sanitize_db_url("postgres://host/db?ssl=false").unwrap();
        let parsed: Result<tokio_postgres::Config, _> = sanitized.parse();
        assert!(parsed.is_err());
    }

    #[test]
    fn sanitize_translates_inline_requiressl_one_to_sslmode_require_in_url_form() {
        // `libpq`'s own `PQconninfoParse` accepts `requiressl` as a real
        // inline connection parameter (not just the `PGREQUIRESSL` env var
        // fixed separately): `requiressl=1` becomes `sslmode=require`.
        // `tokio_postgres` has no `requiressl` keyword at all, so an
        // untranslated occurrence would otherwise be rejected outright with
        // an `UnknownOption` error before ever attempting TLS.
        //
        // Scopes away PGSSLROOTCERT/PGSSLCERT/PGSSLKEY — see the comment on
        // `sanitize_translates_jdbc_ssl_true_to_sslmode_require`.
        temp_env::with_vars(
            [
                ("PGSSLROOTCERT", None::<&str>),
                ("PGSSLCERT", None::<&str>),
                ("PGSSLKEY", None::<&str>),
            ],
            || {
                let sanitized = sanitize_db_url("postgres://host/db?requiressl=1").unwrap();
                let config: tokio_postgres::Config = sanitized.parse().unwrap();
                assert_eq!(
                    config.get_ssl_mode(),
                    tokio_postgres::config::SslMode::Require
                );
            },
        );
    }

    #[test]
    fn sanitize_translates_inline_requiressl_zero_to_sslmode_prefer_in_keyword_form() {
        // `requiressl=0` is documented as `libpq`'s default, equivalent to
        // `sslmode=prefer` — confirming the translation doesn't just handle
        // `1` as a special case and leave every other value (including the
        // explicitly-documented `0`) as an unrecognized `requiressl` key.
        //
        // Scopes away PGSSLCERT/PGSSLKEY — see the comment on
        // `sanitize_rejects_sslmode_allow_with_a_clear_message`.
        temp_env::with_vars(
            [("PGSSLCERT", None::<&str>), ("PGSSLKEY", None::<&str>)],
            || {
                let sanitized = sanitize_db_url("host=host dbname=db requiressl=0").unwrap();
                let config: tokio_postgres::Config = sanitized.parse().unwrap();
                assert_eq!(
                    config.get_ssl_mode(),
                    tokio_postgres::config::SslMode::Prefer
                );
            },
        );
    }

    #[test]
    fn sanitize_does_not_let_requiressl_override_an_explicit_sslmode() {
        // Unlike `ssl=true` (a positional text substitution — see
        // `sanitize_does_not_let_jdbc_ssl_true_override_an_explicit_sslmode`),
        // `requiressl` only ever takes effect when `sslmode` isn't set from
        // *any* other source at all, matching `libpq`'s own
        // `fe-connect.c` semantics — so this holds regardless of which of
        // the two comes first in the connection string.
        //
        // Scopes away PGSSLCERT/PGSSLKEY — see the comment on
        // `sanitize_rejects_sslmode_allow_with_a_clear_message`.
        temp_env::with_vars(
            [("PGSSLCERT", None::<&str>), ("PGSSLKEY", None::<&str>)],
            || {
                let sanitized =
                    sanitize_db_url("postgres://host/db?requiressl=1&sslmode=disable").unwrap();
                let config: tokio_postgres::Config = sanitized.parse().unwrap();
                assert_eq!(
                    config.get_ssl_mode(),
                    tokio_postgres::config::SslMode::Disable
                );
            },
        );
    }

    #[test]
    fn sanitize_translates_requiressl_contributed_by_a_service_group() {
        // `requiressl` merged in from a `pg_service.conf` group must be
        // translated too, not just an inline occurrence — the merge
        // happens mid-function, so the translation has to run after it,
        // not only once at the very start.
        let dir = tempfile::tempdir().unwrap();
        let service_path = dir.path().join("pg_service.conf");
        std::fs::write(&service_path, "[prod]\nrequiressl=1\n").unwrap();
        temp_env::with_vars(
            [
                ("PGSERVICEFILE", Some(service_path.to_str().unwrap())),
                ("PGPASSWORD", None::<&str>),
                ("PGPASSFILE", Some("/nonexistent-pgpass-for-test")),
                ("PGSSLROOTCERT", None::<&str>),
                ("PGSSLCERT", None::<&str>),
                ("PGSSLKEY", None::<&str>),
            ],
            || {
                let sanitized = sanitize_db_url("postgres://host/db?service=prod").unwrap();
                let config: tokio_postgres::Config = sanitized.parse().unwrap();
                assert_eq!(
                    config.get_ssl_mode(),
                    tokio_postgres::config::SslMode::Require
                );
            },
        );
    }

    #[test]
    fn sanitize_rejects_sslmode_allow_with_a_clear_message() {
        // `tokio_postgres::Config`'s own parser only accepts
        // disable/prefer/require for `sslmode` (verified against the real
        // parser below, not just our own logic) — `allow` is a valid libpq
        // mode this native path can't honor, so it must fail with a message
        // pointing at what *is* supported rather than surfacing
        // tokio_postgres's generic "invalid value for option" parse error.
        //
        // Scopes away PGSSLCERT/PGSSLKEY so an ambient/racing value set by
        // another test's `temp_env` call can't trip the sslcert/sslkey
        // rejection first (it's checked before the sslmode=allow one) and
        // produce a message that doesn't mention "sslmode=allow" at all.
        temp_env::with_vars(
            [("PGSSLCERT", None::<&str>), ("PGSSLKEY", None::<&str>)],
            || {
                let err = sanitize_db_url("postgres://host/db?sslmode=allow").unwrap_err();
                assert!(err.message().contains("sslmode=allow"));
                assert!(err.message().contains("prefer"));

                let err = sanitize_db_url("host=localhost sslmode=allow").unwrap_err();
                assert!(err.message().contains("sslmode=allow"));
            },
        );
    }

    #[test]
    fn sanitize_leaves_ordinary_url_unchanged_in_content() {
        // Scope away PGSSLROOTCERT/PGSSLMODE so this can't race against
        // another test's `temp_env`-scoped value for either — with
        // `sslmode=require` already explicit here, an ambient
        // `PGSSLROOTCERT` would trip the require+sslrootcert rejection this
        // test isn't about at all.
        temp_env::with_vars(
            [("PGSSLROOTCERT", None::<&str>), ("PGSSLMODE", None::<&str>)],
            || {
                let sanitized =
                    sanitize_db_url("postgres://user:pw@host:5432/db?sslmode=require").unwrap();
                let parsed = url::Url::parse(&sanitized).unwrap();
                let pairs: std::collections::HashMap<_, _> =
                    parsed.query_pairs().into_owned().collect();
                assert_eq!(pairs.get("sslmode").map(String::as_str), Some("require"));
            },
        );
    }

    #[test]
    fn sanitize_rebuilds_as_keyword_form_when_port_is_a_query_parameter() {
        // `tokio_postgres`'s URL-form parser bakes a port into the config
        // for every host in the authority (the authority's own explicit
        // port if given, else a default of 5432) *and separately* appends
        // whatever the query string's own `port=` says — so a URL like
        // `postgres://db/app?port=6543` (port given as a query parameter,
        // not as part of `host:port`) ends up with two port entries and
        // `tokio_postgres::connect` rejects it with "invalid number of
        // ports", even though this is a perfectly ordinary URL `psql` always
        // accepted. This needs the exact same keyword-form-rebuild
        // treatment already used for a service/PGPORT-supplied port.
        temp_env::with_vars(
            [
                ("PGPASSWORD", None::<&str>),
                ("PGPASSFILE", Some("/nonexistent-pgpass-for-test")),
            ],
            || {
                let sanitized = sanitize_db_url("postgres://db.internal/app?port=6543").unwrap();
                let config: tokio_postgres::Config = sanitized.parse().unwrap();
                assert_eq!(config.get_ports(), &[6543]);
                assert_eq!(config.get_dbname(), Some("app"));
            },
        );
    }

    #[test]
    fn sanitize_rebuilds_as_keyword_form_when_port_is_a_query_parameter_alongside_an_authority_port()
     {
        // Same duplication risk even when the authority *also* names an
        // (overridden) port — `parse_host` bakes in the authority's own
        // port unconditionally, so the query-string `port=` still ends up
        // as a second entry rather than replacing it.
        temp_env::with_vars(
            [
                ("PGPASSWORD", None::<&str>),
                ("PGPASSFILE", Some("/nonexistent-pgpass-for-test")),
            ],
            || {
                let sanitized =
                    sanitize_db_url("postgres://db.internal:5432/app?port=6543").unwrap();
                let config: tokio_postgres::Config = sanitized.parse().unwrap();
                assert_eq!(config.get_ports(), &[6543]);
            },
        );
    }

    #[test]
    fn sanitize_rebuilds_as_keyword_form_when_query_host_is_comma_separated() {
        // `tokio_postgres`'s URL parser special-cases a query `host=`
        // through a single `Config::host()` call rather than splitting on
        // comma (unlike keyword form, which does split, and unlike a
        // multi-host *authority* like `host1:5432,host2:5432`, which is
        // split chunk-by-chunk before parsing) — so `?host=host1,host2`
        // ends up as one literal (and wrong) hostname `"host1,host2"`
        // instead of a two-host failover list.
        //
        // Scopes away PGSSLMODE/PGSSLROOTCERT/PGSSLCERT/PGSSLKEY so an
        // ambient/racing value set by another test's `temp_env` call can't
        // trip one of the SSL-param rejections `filter_ssl_params` always
        // runs (regardless of what this test is actually about) and turn
        // the `unwrap()` below into a flake.
        temp_env::with_vars(
            [
                ("PGSSLMODE", None::<&str>),
                ("PGSSLROOTCERT", None::<&str>),
                ("PGSSLCERT", None::<&str>),
                ("PGSSLKEY", None::<&str>),
            ],
            || {
                let sanitized = sanitize_db_url("postgres:///db?host=host1,host2").unwrap();
                let config: tokio_postgres::Config = sanitized.parse().unwrap();
                assert_eq!(
                    config.get_hosts(),
                    &[
                        tokio_postgres::config::Host::Tcp("host1".to_owned()),
                        tokio_postgres::config::Host::Tcp("host2".to_owned()),
                    ]
                );
            },
        );
    }

    #[test]
    fn sanitize_rebuilds_as_keyword_form_when_query_host_overrides_a_non_empty_authority_host() {
        // `libpq`'s own `PQconninfoParse` makes a named `host=` query
        // parameter the *sole* effective host, discarding the authority
        // host entirely (confirmed against `tokio_postgres::Config`'s own
        // parser — see the comment on `needs_keyword_form` below). But
        // `tokio_postgres`'s URL parser runs `parse_host` (which reads the
        // authority) *before* `parse_params` (which reads the query
        // string), and `host` accumulates rather than overwrites — so a URL
        // like `postgres://authorityhost/db?host=queryhost` ends up with
        // *two* hosts, `[authorityhost, queryhost]`, tried in that order,
        // instead of `queryhost` alone. This is the same accumulation risk
        // `sanitize_rebuilds_as_keyword_form_when_query_host_is_comma_separated`
        // guards against, just without a comma — an authority host is
        // enough on its own to trigger the same bug.
        //
        // See the comment on
        // `sanitize_rebuilds_as_keyword_form_when_query_host_is_comma_separated`
        // for why PGSSLMODE/PGSSLROOTCERT/PGSSLCERT/PGSSLKEY are scoped away.
        temp_env::with_vars(
            [
                ("PGSSLMODE", None::<&str>),
                ("PGSSLROOTCERT", None::<&str>),
                ("PGSSLCERT", None::<&str>),
                ("PGSSLKEY", None::<&str>),
            ],
            || {
                let sanitized =
                    sanitize_db_url("postgres://authorityhost/db?host=queryhost").unwrap();
                let config: tokio_postgres::Config = sanitized.parse().unwrap();
                assert_eq!(
                    config.get_hosts(),
                    &[tokio_postgres::config::Host::Tcp("queryhost".to_owned())]
                );
            },
        );
    }

    #[test]
    fn sanitize_rebuilds_as_keyword_form_when_pghost_env_var_is_comma_separated() {
        // Same bug, reached via the `PGHOST` fallback instead of an inline
        // query parameter — a hostless URL relying on
        // `PGHOST=host1,host2` the way `psql` did must not end up with that
        // whole string pushed into the query string as one literal host.
        temp_env::with_var("PGHOST", Some("host1,host2"), || {
            let sanitized = sanitize_db_url("postgres:///db").unwrap();
            let config: tokio_postgres::Config = sanitized.parse().unwrap();
            assert_eq!(
                config.get_hosts(),
                &[
                    tokio_postgres::config::Host::Tcp("host1".to_owned()),
                    tokio_postgres::config::Host::Tcp("host2".to_owned()),
                ]
            );
        });
    }

    #[test]
    fn sanitize_prefers_query_user_password_dbname_over_url_structure() {
        // `tokio_postgres`'s URL parser applies the query string *after*
        // parsing userinfo/path, and `user`/`password`/`dbname` are single
        // overwritten fields (unlike `host`/`port`, which accumulate) — so a
        // URL like `postgres://alice:secret1@db/app1?user=bob&password=secret2&dbname=app2`
        // actually authenticates as `bob`/`secret2` against `app2`, not
        // `alice`/`secret1`/`app1`. `explicit_user`/`password`/`dbname` must
        // reflect that same effective precedence, or `.pgpass` lookup (which
        // uses these values to pick a row) matches against the wrong
        // identity entirely.
        //
        // Scopes away PGSERVICE so an ambient/racing value set by another
        // test's `temp_env` call (this URL has no `service=` of its own)
        // can't send this connection string through service-file
        // resolution at all and turn the `unwrap()` below into a flake.
        temp_env::with_var("PGSERVICE", None::<&str>, || {
            let sanitized = sanitize_db_url(
                "postgres://alice:secret1@db.internal/app1?user=bob&password=secret2&dbname=app2",
            )
            .unwrap();
            let config: tokio_postgres::Config = sanitized.parse().unwrap();
            assert_eq!(config.get_user(), Some("bob"));
            assert_eq!(config.get_password(), Some(b"secret2".as_slice()));
            assert_eq!(config.get_dbname(), Some("app2"));
        });
    }

    #[test]
    fn sanitize_uses_query_user_for_pgpass_lookup_not_url_userinfo() {
        // The concrete failure mode: a `.pgpass` entry keyed to the
        // *effectively* connecting user (`bob`) must be the one that gets
        // matched, not one keyed to the URL's userinfo (`alice`) that
        // `tokio_postgres` ends up overriding anyway.
        let dir = tempfile::tempdir().unwrap();
        let pgpass_path = dir.path().join(".pgpass");
        std::fs::write(
            &pgpass_path,
            "db.internal:5432:app:alice:wrong_password\n\
             db.internal:5432:app:bob:right_password\n",
        )
        .unwrap();
        set_pgpass_permissions_safe(&pgpass_path);
        // Scopes away PGSERVICE too — none of these connection strings set
        // an inline `service=`, so an ambient/racing value would send them
        // through service-file resolution instead of the pgpass lookup
        // path this test is actually about.
        temp_env::with_vars(
            [
                ("PGPASSWORD", None::<&str>),
                ("PGPASSFILE", Some(pgpass_path.to_str().unwrap())),
                ("PGSERVICE", None::<&str>),
            ],
            || {
                let sanitized =
                    sanitize_db_url("postgres://alice@db.internal/app?user=bob").unwrap();
                let config: tokio_postgres::Config = sanitized.parse().unwrap();
                assert_eq!(config.get_user(), Some("bob"));
                assert_eq!(config.get_password(), Some(b"right_password".as_slice()));
            },
        );
    }

    #[test]
    fn sanitize_uses_query_host_for_pgpass_lookup_not_url_authority() {
        // Sibling of `sanitize_uses_query_user_for_pgpass_lookup_not_url_userinfo`,
        // but for `host` instead of `user`: once `needs_keyword_form` forces
        // a rebuild for a query `host=`
        // that overrides a non-empty authority host, the final connection
        // actually uses the query host (`actual-db`), not the authority
        // placeholder — so `.pgpass` lookup must key off that same
        // effective host, or a password entry for the real target gets
        // missed.
        let dir = tempfile::tempdir().unwrap();
        let pgpass_path = dir.path().join(".pgpass");
        std::fs::write(
            &pgpass_path,
            "placeholder:5432:app:postgres:wrong_password\n\
             actual-db:5432:app:postgres:right_password\n",
        )
        .unwrap();
        set_pgpass_permissions_safe(&pgpass_path);
        // Scopes away PGSERVICE too — none of these connection strings set
        // an inline `service=`, so an ambient/racing value would send them
        // through service-file resolution instead of the pgpass lookup
        // path this test is actually about.
        temp_env::with_vars(
            [
                ("PGPASSWORD", None::<&str>),
                ("PGPASSFILE", Some(pgpass_path.to_str().unwrap())),
                ("PGSERVICE", None::<&str>),
            ],
            || {
                let sanitized =
                    sanitize_db_url("postgres://postgres@placeholder/app?host=actual-db").unwrap();
                let config: tokio_postgres::Config = sanitized.parse().unwrap();
                assert_eq!(
                    config.get_hosts(),
                    &[tokio_postgres::config::Host::Tcp("actual-db".to_owned())]
                );
                assert_eq!(config.get_password(), Some(b"right_password".as_slice()));
            },
        );
    }

    #[test]
    fn sanitize_uses_query_port_for_pgpass_lookup_not_url_authority() {
        // See `sanitize_uses_query_host_for_pgpass_lookup_not_url_authority`
        // for the same issue, but for `port=` overriding an authority port.
        let dir = tempfile::tempdir().unwrap();
        let pgpass_path = dir.path().join(".pgpass");
        std::fs::write(
            &pgpass_path,
            "db.internal:5432:app:postgres:wrong_password\n\
             db.internal:6543:app:postgres:right_password\n",
        )
        .unwrap();
        set_pgpass_permissions_safe(&pgpass_path);
        // Scopes away PGSERVICE too — none of these connection strings set
        // an inline `service=`, so an ambient/racing value would send them
        // through service-file resolution instead of the pgpass lookup
        // path this test is actually about.
        temp_env::with_vars(
            [
                ("PGPASSWORD", None::<&str>),
                ("PGPASSFILE", Some(pgpass_path.to_str().unwrap())),
                ("PGSERVICE", None::<&str>),
            ],
            || {
                let sanitized =
                    sanitize_db_url("postgres://postgres@db.internal:5432/app?port=6543").unwrap();
                let config: tokio_postgres::Config = sanitized.parse().unwrap();
                assert_eq!(config.get_ports(), &[6543]);
                assert_eq!(config.get_password(), Some(b"right_password".as_slice()));
            },
        );
    }

    #[test]
    fn sanitize_uses_last_occurrence_of_repeated_user_for_pgpass_lookup_in_keyword_form() {
        // `tokio_postgres::Config::param`'s "user" arm overwrites the field
        // on every occurrence, so `user=alice user=bob` connects as `bob`
        // (last write wins) — but `fill_in_missing_password`'s internal
        // `find_pair(pairs, "user")` fallback (used here since
        // `sanitize_keyword_form` passes `None` for every `explicit_*`
        // snapshot) returned the *first* match, `alice`, so `.pgpass` would
        // look up and inject Alice's password while the connection actually
        // authenticates as Bob.
        let dir = tempfile::tempdir().unwrap();
        let pgpass_path = dir.path().join(".pgpass");
        std::fs::write(
            &pgpass_path,
            "db.internal:5432:app:alice:wrong_password\n\
             db.internal:5432:app:bob:right_password\n",
        )
        .unwrap();
        set_pgpass_permissions_safe(&pgpass_path);
        // Scopes away PGSERVICE too — none of these connection strings set
        // an inline `service=`, so an ambient/racing value would send them
        // through service-file resolution instead of the pgpass lookup
        // path this test is actually about.
        temp_env::with_vars(
            [
                ("PGPASSWORD", None::<&str>),
                ("PGPASSFILE", Some(pgpass_path.to_str().unwrap())),
                ("PGSERVICE", None::<&str>),
            ],
            || {
                let sanitized =
                    sanitize_db_url("host=db.internal dbname=app user=alice user=bob").unwrap();
                let config: tokio_postgres::Config = sanitized.parse().unwrap();
                assert_eq!(config.get_user(), Some("bob"));
                assert_eq!(config.get_password(), Some(b"right_password".as_slice()));
            },
        );
    }

    #[test]
    fn sanitize_uses_last_occurrence_of_repeated_dbname_for_pgpass_lookup_in_url_form() {
        // Same bug, but for `dbname` via a repeated query parameter in URL
        // form (`explicit_dbname`'s own `find_pair` fallback has the same
        // first-match problem as `fill_in_missing_password`'s internal one).
        let dir = tempfile::tempdir().unwrap();
        let pgpass_path = dir.path().join(".pgpass");
        std::fs::write(
            &pgpass_path,
            "db.internal:5432:old:postgres:wrong_password\n\
             db.internal:5432:new:postgres:right_password\n",
        )
        .unwrap();
        set_pgpass_permissions_safe(&pgpass_path);
        // Scopes away PGSERVICE too — none of these connection strings set
        // an inline `service=`, so an ambient/racing value would send them
        // through service-file resolution instead of the pgpass lookup
        // path this test is actually about.
        temp_env::with_vars(
            [
                ("PGPASSWORD", None::<&str>),
                ("PGPASSFILE", Some(pgpass_path.to_str().unwrap())),
                ("PGSERVICE", None::<&str>),
            ],
            || {
                let sanitized =
                    sanitize_db_url("postgres://postgres@db.internal/?dbname=old&dbname=new")
                        .unwrap();
                let config: tokio_postgres::Config = sanitized.parse().unwrap();
                assert_eq!(config.get_dbname(), Some("new"));
                assert_eq!(config.get_password(), Some(b"right_password".as_slice()));
            },
        );
    }

    #[test]
    fn sanitize_drops_ca_and_crl_ssl_params_from_url() {
        // This test asserts an *exact* pair count of zero, so unlike most
        // other tests here (which only need to scope away the handful of
        // env vars actually relevant to what they're testing) this one
        // needs *every* `PG*` env var `sanitize_db_url` ever falls back to
        // scoped away — an ambient/racing value for *any* of them, set by
        // another test's `temp_env::with_vars` call on a different thread,
        // would inject one more surviving pair and break the count. Keep
        // this list in sync with every `PG*` var this module reads (see
        // `fill_in_libpq_env_defaults`, `fill_in_simple_libpq_env_fallbacks`,
        // `fill_in_libpq_ssl_env_defaults`, `service_file_path`,
        // `system_service_file_path`, `pgpass_file_path`) as new ones are
        // added.
        //
        // `sslcert`/`sslkey`/`sslpassword` (client-certificate auth) aren't
        // silently dropped like `sslcrl`/`sslcrldir` — they're rejected
        // outright, tested separately below — but `PGSSLCERT`/`PGSSLKEY`
        // still need to be scoped away here since an ambient value would
        // turn this test's `.unwrap()` into a panic rather than a wrong
        // count.
        temp_env::with_vars(
            [
                ("PGHOST", None::<&str>),
                ("PGHOSTADDR", None::<&str>),
                ("PGPORT", None::<&str>),
                ("PGUSER", None::<&str>),
                ("PGDATABASE", None::<&str>),
                ("PGPASSWORD", None::<&str>),
                ("PGPASSFILE", Some("/nonexistent-pgpass-file-for-test")),
                ("PGSERVICEFILE", None::<&str>),
                ("PGSERVICE", None::<&str>),
                ("PGSYSCONFDIR", None::<&str>),
                ("PGSSLMODE", None::<&str>),
                ("PGSSLROOTCERT", None::<&str>),
                ("PGSSLCERT", None::<&str>),
                ("PGSSLKEY", None::<&str>),
                ("PGCONNECT_TIMEOUT", None::<&str>),
                ("PGTARGETSESSIONATTRS", None::<&str>),
                ("PGCHANNELBINDING", None::<&str>),
                ("PGOPTIONS", None::<&str>),
                ("PGAPPNAME", None::<&str>),
                ("PGLOADBALANCEHOSTS", None::<&str>),
            ],
            || {
                let sanitized =
                    sanitize_db_url("postgres://host/db?sslcrl=/crl.pem&sslcrldir=/crls").unwrap();
                let parsed = url::Url::parse(&sanitized).unwrap();
                assert_eq!(parsed.query_pairs().count(), 0);
            },
        );
    }

    #[test]
    fn sanitize_rejects_sslcert_in_url_form_instead_of_silently_dropping() {
        // Client-certificate authentication (`sslcert`/`sslkey`, optionally
        // `sslpassword` for an encrypted key) isn't implemented by this
        // native connection path — `tls_connector` builds its `rustls`
        // config with `with_no_client_auth()`. Silently dropping these
        // params would leave a `cert`/`clientcert=verify-ca` deployment that
        // `psql` authenticated to failing opaquely at the server's auth
        // layer, with nothing in the CLI's own output pointing at the real
        // cause — reject clearly instead.
        //
        // Scopes away PGSSLMODE/PGSSLROOTCERT so an ambient/racing
        // `PGSSLMODE=require` plus `PGSSLROOTCERT` can't instead trip the
        // require+sslrootcert rejection (checked first) and produce a
        // message that doesn't mention `sslcert` at all.
        temp_env::with_vars(
            [("PGSSLMODE", None::<&str>), ("PGSSLROOTCERT", None::<&str>)],
            || {
                let err =
                    sanitize_db_url("postgres://host/db?sslcert=/c.pem&sslkey=/k.pem").unwrap_err();
                assert!(err.message().contains("sslcert"));
            },
        );
    }

    #[test]
    fn sanitize_rejects_sslkey_alone_in_url_form() {
        // See the comment on `sanitize_rejects_sslcert_in_url_form_instead_of_silently_dropping`
        // for why this scopes away PGSSLMODE/PGSSLROOTCERT.
        temp_env::with_vars(
            [("PGSSLMODE", None::<&str>), ("PGSSLROOTCERT", None::<&str>)],
            || {
                let err = sanitize_db_url("postgres://host/db?sslkey=/k.pem").unwrap_err();
                assert!(err.message().contains("sslkey"));
            },
        );
    }

    #[test]
    fn sanitize_rejects_sslcert_in_keyword_form() {
        // See the comment on `sanitize_rejects_sslcert_in_url_form_instead_of_silently_dropping`
        // for why this scopes away PGSSLMODE/PGSSLROOTCERT.
        temp_env::with_vars(
            [("PGSSLMODE", None::<&str>), ("PGSSLROOTCERT", None::<&str>)],
            || {
                let err = sanitize_db_url("host=host dbname=db sslcert=/c.pem sslkey=/k.pem")
                    .unwrap_err();
                assert!(err.message().contains("sslcert"));
            },
        );
    }

    #[test]
    fn sanitize_rejects_sslcert_from_a_service_group() {
        let dir = tempfile::tempdir().unwrap();
        let service_path = dir.path().join("pg_service.conf");
        std::fs::write(&service_path, "[prod]\nsslcert=/c.pem\nsslkey=/k.pem\n").unwrap();
        temp_env::with_vars(
            [
                ("PGSERVICEFILE", Some(service_path.to_str().unwrap())),
                ("PGPASSWORD", None::<&str>),
                ("PGPASSFILE", Some("/nonexistent-pgpass-for-test")),
                ("PGSSLMODE", None::<&str>),
                ("PGSSLROOTCERT", None::<&str>),
            ],
            || {
                let err = sanitize_db_url("postgres://host/db?service=prod").unwrap_err();
                assert!(err.message().contains("sslcert"));
            },
        );
    }

    #[test]
    fn sanitize_rejects_pgsslcert_env_var_in_url_form() {
        // PostgreSQL documents PGSSLCERT/PGSSLKEY as behaving like
        // sslcert/sslkey when supplied via the environment rather than
        // inline or in a service file — they need the same rejection as
        // `sanitize_rejects_sslcert_in_url_form_instead_of_silently_dropping`,
        // not a silent no-op that leaves `tls_connector`'s
        // `with_no_client_auth()` config in place.
        temp_env::with_vars(
            [
                ("PGSSLMODE", None::<&str>),
                ("PGSSLROOTCERT", None::<&str>),
                ("PGSSLCERT", Some("/c.pem")),
                ("PGSSLKEY", Some("/k.pem")),
            ],
            || {
                let err = sanitize_db_url("postgres://host/db").unwrap_err();
                assert!(err.message().contains("sslcert"));
            },
        );
    }

    #[test]
    fn sanitize_rejects_pgsslkey_env_var_in_keyword_form() {
        temp_env::with_vars(
            [
                ("PGSSLMODE", None::<&str>),
                ("PGSSLROOTCERT", None::<&str>),
                ("PGSSLCERT", None::<&str>),
                ("PGSSLKEY", Some("/k.pem")),
            ],
            || {
                let err = sanitize_db_url("host=host dbname=db").unwrap_err();
                assert!(err.message().contains("sslkey"));
            },
        );
    }

    #[test]
    fn sanitize_does_not_override_explicit_sslcert_rejection_message_source() {
        // An inline sslcert= still triggers the same rejection when
        // PGSSLCERT is *also* set — this just confirms the env fallback
        // doesn't need to fire (and doesn't crash / double-push) when the
        // parameter is already explicit.
        temp_env::with_vars(
            [
                ("PGSSLMODE", None::<&str>),
                ("PGSSLROOTCERT", None::<&str>),
                ("PGSSLCERT", Some("/from-env.pem")),
                ("PGSSLKEY", None::<&str>),
            ],
            || {
                let err = sanitize_db_url("postgres://host/db?sslcert=/inline.pem").unwrap_err();
                assert!(err.message().contains("sslcert"));
            },
        );
    }

    #[test]
    fn sanitize_drops_unsupported_ssl_params_from_keyword_form() {
        // See the comment on `sanitize_drops_all_ssl_cert_params_from_url`
        // for why this scopes away PGPASSWORD/PGPASSFILE. `sslmode=prefer`
        // (not `require`) so this stays a plain "unsupported param dropped"
        // case rather than tripping the require+sslrootcert rejection.
        temp_env::with_vars(
            [
                ("PGPASSWORD", None::<&str>),
                ("PGPASSFILE", Some("/nonexistent-pgpass-file-for-test")),
            ],
            || {
                let sanitized = sanitize_db_url(
                    "host=localhost dbname=mydb sslmode=prefer sslrootcert=/etc/ca.pem",
                )
                .unwrap();
                // Verify against tokio_postgres's *actual* parser, not just our
                // own tokenizer's self-consistency.
                let config: tokio_postgres::Config = sanitized.parse().unwrap();
                assert_eq!(config.get_dbname(), Some("mydb"));
                assert_eq!(
                    config.get_ssl_mode(),
                    tokio_postgres::config::SslMode::Prefer
                );
            },
        );
    }

    #[test]
    fn sanitize_rejects_verify_full_in_keyword_form_instead_of_downgrading() {
        // Scopes away PGSSLCERT/PGSSLKEY — see the comment on
        // `sanitize_rejects_verify_ca_in_url_form_instead_of_downgrading`.
        temp_env::with_vars(
            [("PGSSLCERT", None::<&str>), ("PGSSLKEY", None::<&str>)],
            || {
                let err =
                    sanitize_db_url("host=localhost dbname=mydb sslmode=verify-full").unwrap_err();
                assert!(err.message().contains("verify-full"));
            },
        );
    }

    #[test]
    fn sanitize_rejects_require_with_sslrootcert_in_keyword_form() {
        let err =
            sanitize_db_url("host=localhost dbname=mydb sslmode=require sslrootcert=/etc/ca.pem")
                .unwrap_err();
        assert!(err.message().contains("sslrootcert"));
        assert!(err.message().contains("require"));
    }

    #[test]
    fn sanitize_handles_quoted_values_in_keyword_form() {
        // See the comment on `sanitize_drops_all_ssl_cert_params_from_url`
        // for why this scopes away PGPASSWORD/PGPASSFILE. `sslmode=prefer`
        // (not `require`) for the same reason as
        // `sanitize_drops_unsupported_ssl_params_from_keyword_form` above.
        temp_env::with_vars(
            [
                ("PGPASSWORD", None::<&str>),
                ("PGPASSFILE", Some("/nonexistent-pgpass-file-for-test")),
            ],
            || {
                let sanitized = sanitize_db_url(
                    "host=localhost dbname='my db' sslmode=prefer sslrootcert=/etc/ca.pem",
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
    fn sanitize_fills_in_pghost_when_url_has_neither_host_nor_hostaddr() {
        // `tokio_postgres::connect` errors "both host and hostaddr are
        // missing" if neither is set on the final `Config` — a bare
        // `postgres:///app` (a common local-dev Unix-socket-style
        // connection string, or one relying on `PGHOST` the way `psql`
        // itself would) must not be handed to it as-is.
        temp_env::with_var("PGHOST", Some("db.internal"), || {
            let sanitized = sanitize_db_url("postgres:///app").unwrap();
            let config: tokio_postgres::Config = sanitized.parse().unwrap();
            assert_eq!(
                config.get_hosts(),
                &[tokio_postgres::config::Host::Tcp("db.internal".to_owned())]
            );
        });
    }

    #[test]
    fn sanitize_fills_in_pghost_when_keyword_form_has_neither_host_nor_hostaddr() {
        temp_env::with_var("PGHOST", Some("db.internal"), || {
            let sanitized = sanitize_db_url("dbname=app").unwrap();
            let config: tokio_postgres::Config = sanitized.parse().unwrap();
            assert_eq!(
                config.get_hosts(),
                &[tokio_postgres::config::Host::Tcp("db.internal".to_owned())]
            );
        });
    }

    #[test]
    fn sanitize_does_not_override_an_explicit_host_with_pghost() {
        temp_env::with_var("PGHOST", Some("wrong-host"), || {
            let sanitized = sanitize_db_url("postgres://right-host/app").unwrap();
            let config: tokio_postgres::Config = sanitized.parse().unwrap();
            assert_eq!(
                config.get_hosts(),
                &[tokio_postgres::config::Host::Tcp("right-host".to_owned())]
            );
        });
    }

    #[test]
    fn sanitize_fills_in_both_pghost_and_pghostaddr_when_url_has_neither() {
        // libpq allows both `PGHOST` and `PGHOSTADDR` to be set together —
        // it connects to `PGHOSTADDR` while retaining `PGHOST` as the name
        // used for e.g. certificate/auth checks (a common setup when DNS
        // isn't available or `PGHOST` is only meaningful as a name, not an
        // address). `tokio_postgres::Config` supports the same pairing
        // (`connect.rs` requires `host`/`hostaddr` to have equal lengths
        // when both are set, then uses `hostaddr` to open the socket).
        temp_env::with_vars(
            [
                ("PGHOST", Some("db.internal")),
                ("PGHOSTADDR", Some("10.0.0.5")),
            ],
            || {
                let sanitized = sanitize_db_url("postgres:///app").unwrap();
                let config: tokio_postgres::Config = sanitized.parse().unwrap();
                assert_eq!(
                    config.get_hosts(),
                    &[tokio_postgres::config::Host::Tcp("db.internal".to_owned())]
                );
                assert_eq!(
                    config.get_hostaddrs(),
                    &["10.0.0.5".parse::<std::net::IpAddr>().unwrap()]
                );
            },
        );
    }

    #[test]
    fn sanitize_fills_in_both_pghost_and_pghostaddr_in_keyword_form_too() {
        temp_env::with_vars(
            [
                ("PGHOST", Some("db.internal")),
                ("PGHOSTADDR", Some("10.0.0.5")),
            ],
            || {
                let sanitized = sanitize_db_url("dbname=app").unwrap();
                let config: tokio_postgres::Config = sanitized.parse().unwrap();
                assert_eq!(
                    config.get_hosts(),
                    &[tokio_postgres::config::Host::Tcp("db.internal".to_owned())]
                );
                assert_eq!(
                    config.get_hostaddrs(),
                    &["10.0.0.5".parse::<std::net::IpAddr>().unwrap()]
                );
            },
        );
    }

    #[test]
    fn sanitize_fills_in_pghostaddr_even_when_host_is_already_explicit() {
        // `host`/`hostaddr` are independent per-parameter fallbacks, not a
        // pair that only falls back together — an explicit `host` in the URL
        // must not block `PGHOSTADDR` from still filling in the address, the
        // way it would for a bare hostless connection string.
        temp_env::with_var("PGHOSTADDR", Some("10.0.0.5"), || {
            let sanitized = sanitize_db_url("postgres://db-name/app").unwrap();
            let config: tokio_postgres::Config = sanitized.parse().unwrap();
            assert_eq!(
                config.get_hosts(),
                &[tokio_postgres::config::Host::Tcp("db-name".to_owned())]
            );
            assert_eq!(
                config.get_hostaddrs(),
                &["10.0.0.5".parse::<std::net::IpAddr>().unwrap()]
            );
        });
    }

    #[test]
    fn sanitize_fills_in_pghost_even_when_hostaddr_is_already_explicit() {
        temp_env::with_var("PGHOST", Some("db-name"), || {
            let sanitized = sanitize_db_url("postgres:///app?hostaddr=10.0.0.9").unwrap();
            let config: tokio_postgres::Config = sanitized.parse().unwrap();
            assert_eq!(
                config.get_hosts(),
                &[tokio_postgres::config::Host::Tcp("db-name".to_owned())]
            );
            assert_eq!(
                config.get_hostaddrs(),
                &["10.0.0.9".parse::<std::net::IpAddr>().unwrap()]
            );
        });
    }

    #[test]
    fn sanitize_fills_in_pghostaddr_even_when_host_is_already_explicit_in_keyword_form() {
        temp_env::with_var("PGHOSTADDR", Some("10.0.0.5"), || {
            let sanitized = sanitize_db_url("host=db-name dbname=app").unwrap();
            let config: tokio_postgres::Config = sanitized.parse().unwrap();
            assert_eq!(
                config.get_hosts(),
                &[tokio_postgres::config::Host::Tcp("db-name".to_owned())]
            );
            assert_eq!(
                config.get_hostaddrs(),
                &["10.0.0.5".parse::<std::net::IpAddr>().unwrap()]
            );
        });
    }

    #[test]
    fn sanitize_fills_in_pgport_pguser_pgdatabase_when_url_omits_them() {
        // `tokio_postgres` never reads these on its own (unlike `psql`, which
        // honors them as documented libpq environment variables), so a URL
        // that omits port/user/dbname and relies on the environment the way
        // `psql` did would otherwise connect as the OS login on the default
        // port to a database named after that login instead.
        temp_env::with_vars(
            [
                ("PGPORT", Some("6543")),
                ("PGUSER", Some("app_user")),
                ("PGDATABASE", Some("app_db")),
                ("PGPASSWORD", None),
                ("PGPASSFILE", Some("/nonexistent-pgpass-for-test")),
            ],
            || {
                let sanitized = sanitize_db_url("postgres://db.internal").unwrap();
                let config: tokio_postgres::Config = sanitized.parse().unwrap();
                assert_eq!(config.get_ports(), &[6543]);
                assert_eq!(config.get_user(), Some("app_user"));
                assert_eq!(config.get_dbname(), Some("app_db"));
            },
        );
    }

    #[test]
    fn sanitize_fills_in_pgport_pguser_pgdatabase_in_keyword_form_too() {
        temp_env::with_vars(
            [
                ("PGPORT", Some("6543")),
                ("PGUSER", Some("app_user")),
                ("PGDATABASE", Some("app_db")),
                ("PGPASSWORD", None),
                ("PGPASSFILE", Some("/nonexistent-pgpass-for-test")),
            ],
            || {
                let sanitized = sanitize_db_url("host=db.internal").unwrap();
                let config: tokio_postgres::Config = sanitized.parse().unwrap();
                assert_eq!(config.get_ports(), &[6543]);
                assert_eq!(config.get_user(), Some("app_user"));
                assert_eq!(config.get_dbname(), Some("app_db"));
            },
        );
    }

    #[test]
    fn sanitize_does_not_override_explicit_port_user_dbname_with_env_vars() {
        temp_env::with_vars(
            [
                ("PGPORT", Some("9999")),
                ("PGUSER", Some("wrong_user")),
                ("PGDATABASE", Some("wrong_db")),
                ("PGPASSWORD", None),
                ("PGPASSFILE", Some("/nonexistent-pgpass-for-test")),
            ],
            || {
                let sanitized =
                    sanitize_db_url("postgres://right_user@db.internal:6543/right_db").unwrap();
                let config: tokio_postgres::Config = sanitized.parse().unwrap();
                assert_eq!(config.get_ports(), &[6543]);
                assert_eq!(config.get_user(), Some("right_user"));
                assert_eq!(config.get_dbname(), Some("right_db"));
            },
        );
    }

    #[test]
    fn sanitize_uses_pgconnect_timeout_env_var_when_url_omits_it() {
        // `tokio_postgres` supports `connect_timeout` as a connection
        // parameter but never reads `PGCONNECT_TIMEOUT` itself (unlike
        // `psql`, which honors it as a documented libpq environment
        // variable) — a URL relying on it the way `psql` did would
        // otherwise wait on the OS-level TCP timeout instead of the
        // configured bound.
        temp_env::with_var("PGCONNECT_TIMEOUT", Some("10"), || {
            let sanitized = sanitize_db_url("postgres://db.internal").unwrap();
            let config: tokio_postgres::Config = sanitized.parse().unwrap();
            assert_eq!(
                config.get_connect_timeout(),
                Some(&std::time::Duration::from_secs(10))
            );
        });
    }

    #[test]
    fn sanitize_uses_pgconnect_timeout_env_var_in_keyword_form_too() {
        temp_env::with_var("PGCONNECT_TIMEOUT", Some("10"), || {
            let sanitized = sanitize_db_url("host=db.internal").unwrap();
            let config: tokio_postgres::Config = sanitized.parse().unwrap();
            assert_eq!(
                config.get_connect_timeout(),
                Some(&std::time::Duration::from_secs(10))
            );
        });
    }

    #[test]
    fn sanitize_does_not_override_explicit_connect_timeout_with_pgconnect_timeout_env_var() {
        temp_env::with_var("PGCONNECT_TIMEOUT", Some("999"), || {
            let sanitized = sanitize_db_url("postgres://db.internal?connect_timeout=10").unwrap();
            let config: tokio_postgres::Config = sanitized.parse().unwrap();
            assert_eq!(
                config.get_connect_timeout(),
                Some(&std::time::Duration::from_secs(10))
            );
        });
    }

    #[test]
    fn sanitize_preserves_literal_plus_in_url_query_values() {
        // `url::Url::query_pairs()` applies `application/x-www-form-urlencoded`
        // decoding, which turns a literal `+` into a space — but
        // `tokio_postgres`'s own query-string decoder (`UrlParser::decode`,
        // via `percent_encoding::percent_decode`) does pure percent-decoding
        // and never treats `+` as space. A URL like
        // `...?application_name=ops+cli` therefore means the literal string
        // "ops+cli" to `tokio_postgres`, not "ops cli" — reading the query
        // string through `query_pairs()` before this sanitizer even runs
        // would already have corrupted it.
        // Scopes away PGSSLMODE/PGSSLROOTCERT so an ambient/racing
        // `PGSSLMODE=require` plus `PGSSLROOTCERT` (set by another test via
        // `temp_env` on a different thread) can't trip the
        // require+sslrootcert rejection and turn this `unwrap()` into a
        // flake, per the established test-isolation precedent in this file.
        temp_env::with_vars(
            [("PGSSLMODE", None::<&str>), ("PGSSLROOTCERT", None::<&str>)],
            || {
                let sanitized =
                    sanitize_db_url("postgres://db.internal/mydb?application_name=ops+cli")
                        .unwrap();
                let config: tokio_postgres::Config = sanitized.parse().unwrap();
                assert_eq!(config.get_application_name(), Some("ops+cli"));
            },
        );
    }

    #[test]
    fn sanitize_still_percent_decodes_url_query_values_with_plus_present() {
        // Companion to `sanitize_preserves_literal_plus_in_url_query_values`
        // — confirms the fix doesn't regress plain percent-decoding (`%20`
        // for a space) just because it stopped treating `+` as space. See
        // that test for why PGSSLMODE/PGSSLROOTCERT are scoped away.
        temp_env::with_vars(
            [("PGSSLMODE", None::<&str>), ("PGSSLROOTCERT", None::<&str>)],
            || {
                let sanitized = sanitize_db_url(
                    "postgres://db.internal/mydb?options=-c%20search_path%3Dapp+public",
                )
                .unwrap();
                let config: tokio_postgres::Config = sanitized.parse().unwrap();
                assert_eq!(config.get_options(), Some("-c search_path=app+public"));
            },
        );
    }

    #[test]
    fn sanitize_uses_pgsslnegotiation_env_var_when_url_omits_it() {
        // `tokio_postgres` supports `sslnegotiation` as a connection
        // parameter (postgres/direct) but never reads `PGSSLNEGOTIATION`
        // itself (unlike `psql`, which honors it as a documented libpq
        // environment variable, per libpq-envars.html: "PGSSLNEGOTIATION
        // behaves the same as the sslnegotiation connection parameter") —
        // a connection string relying on it for direct TLS negotiation
        // would otherwise silently keep the default `postgres` negotiation,
        // which can fail against endpoints/proxies expecting direct mode.
        temp_env::with_var("PGSSLNEGOTIATION", Some("direct"), || {
            let sanitized = sanitize_db_url("postgres://db.internal").unwrap();
            let config: tokio_postgres::Config = sanitized.parse().unwrap();
            assert_eq!(
                config.get_ssl_negotiation(),
                tokio_postgres::config::SslNegotiation::Direct
            );
        });
    }

    #[test]
    fn sanitize_uses_pgloadbalancehosts_env_var_when_url_omits_it() {
        // `tokio_postgres` supports `load_balance_hosts` as a connection
        // parameter (disable/random) but never reads `PGLOADBALANCEHOSTS`
        // itself (unlike `psql`, which honors it as a documented libpq
        // environment variable) — a multi-host connection string relying on
        // it the way `psql` did would otherwise silently fall back to
        // ordered/default host selection instead of randomizing.
        temp_env::with_var("PGLOADBALANCEHOSTS", Some("random"), || {
            let sanitized = sanitize_db_url("postgres://db.internal").unwrap();
            let config: tokio_postgres::Config = sanitized.parse().unwrap();
            assert_eq!(
                config.get_load_balance_hosts(),
                tokio_postgres::config::LoadBalanceHosts::Random
            );
        });
    }

    #[test]
    fn sanitize_uses_pgtargetsessionattrs_env_var_when_url_omits_it() {
        // `tokio_postgres` supports `target_session_attrs` as a connection
        // parameter but never reads `PGTARGETSESSIONATTRS` itself (unlike
        // `psql`, which honors it as a documented libpq environment
        // variable) — a multi-host connection string relying on it the way
        // `psql` did would otherwise default to `any` and could connect to
        // a read-only standby instead of skipping to the primary.
        temp_env::with_var("PGTARGETSESSIONATTRS", Some("read-write"), || {
            let sanitized = sanitize_db_url("postgres://db.internal").unwrap();
            let config: tokio_postgres::Config = sanitized.parse().unwrap();
            assert_eq!(
                config.get_target_session_attrs(),
                tokio_postgres::config::TargetSessionAttrs::ReadWrite
            );
        });
    }

    #[test]
    fn sanitize_uses_pgtargetsessionattrs_env_var_in_keyword_form_too() {
        temp_env::with_var("PGTARGETSESSIONATTRS", Some("read-write"), || {
            let sanitized = sanitize_db_url("host=db.internal").unwrap();
            let config: tokio_postgres::Config = sanitized.parse().unwrap();
            assert_eq!(
                config.get_target_session_attrs(),
                tokio_postgres::config::TargetSessionAttrs::ReadWrite
            );
        });
    }

    #[test]
    fn sanitize_does_not_override_explicit_target_session_attrs_with_env_var() {
        temp_env::with_var("PGTARGETSESSIONATTRS", Some("read-write"), || {
            let sanitized =
                sanitize_db_url("postgres://db.internal?target_session_attrs=any").unwrap();
            let config: tokio_postgres::Config = sanitized.parse().unwrap();
            assert_eq!(
                config.get_target_session_attrs(),
                tokio_postgres::config::TargetSessionAttrs::Any
            );
        });
    }

    #[test]
    fn sanitize_uses_pgchannelbinding_env_var_when_url_omits_it() {
        // `tokio_postgres` defaults `channel_binding` to `Prefer` and never
        // reads `PGCHANNELBINDING` itself (unlike `psql`, which honors it
        // as a documented libpq environment variable) — relying on it the
        // way `psql` did would otherwise silently weaken an operator's
        // SCRAM channel-binding policy.
        temp_env::with_var("PGCHANNELBINDING", Some("require"), || {
            let sanitized = sanitize_db_url("postgres://db.internal").unwrap();
            let config: tokio_postgres::Config = sanitized.parse().unwrap();
            assert_eq!(
                config.get_channel_binding(),
                tokio_postgres::config::ChannelBinding::Require
            );
        });
    }

    #[test]
    fn sanitize_uses_pgoptions_env_var_when_url_omits_it() {
        temp_env::with_var("PGOPTIONS", Some("-c search_path=app,public"), || {
            let sanitized = sanitize_db_url("postgres://db.internal").unwrap();
            let config: tokio_postgres::Config = sanitized.parse().unwrap();
            assert_eq!(config.get_options(), Some("-c search_path=app,public"));
        });
    }

    #[test]
    fn sanitize_uses_pgappname_env_var_when_url_omits_it() {
        temp_env::with_var("PGAPPNAME", Some("my-app"), || {
            let sanitized = sanitize_db_url("postgres://db.internal").unwrap();
            let config: tokio_postgres::Config = sanitized.parse().unwrap();
            assert_eq!(config.get_application_name(), Some("my-app"));
        });
    }

    #[test]
    fn sanitize_does_not_override_explicit_channel_binding_options_appname_with_env_vars() {
        temp_env::with_vars(
            [
                ("PGCHANNELBINDING", Some("require")),
                ("PGOPTIONS", Some("-c wrong=1")),
                ("PGAPPNAME", Some("wrong-app")),
            ],
            || {
                let sanitized = sanitize_db_url(
                    "postgres://db.internal?channel_binding=disable&options=-c%20right%3D1&application_name=right-app",
                )
                .unwrap();
                let config: tokio_postgres::Config = sanitized.parse().unwrap();
                assert_eq!(
                    config.get_channel_binding(),
                    tokio_postgres::config::ChannelBinding::Disable
                );
                assert_eq!(config.get_options(), Some("-c right=1"));
                assert_eq!(config.get_application_name(), Some("right-app"));
            },
        );
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
        // Scopes away PGSERVICE too — none of these connection strings set
        // an inline `service=`, so an ambient/racing value would send them
        // through service-file resolution instead of the pgpass lookup
        // path this test is actually about.
        temp_env::with_vars(
            [
                ("PGPASSWORD", None::<&str>),
                ("PGPASSFILE", Some(pgpass_path.to_str().unwrap())),
                ("PGSERVICE", None::<&str>),
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
        // Scopes away PGSERVICE too — none of these connection strings set
        // an inline `service=`, so an ambient/racing value would send them
        // through service-file resolution instead of the pgpass lookup
        // path this test is actually about.
        temp_env::with_vars(
            [
                ("PGPASSWORD", None::<&str>),
                ("PGPASSFILE", Some(pgpass_path.to_str().unwrap())),
                ("PGSERVICE", None::<&str>),
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
    fn sanitize_prefers_service_file_password_over_an_ambient_pgpassword() {
        // Regression test: `is_explicit`'s "password" arm must reflect the
        // *current* state of `pairs` (after the service merge has already
        // run), not a stale pre-merge snapshot — otherwise a service group
        // that supplies its own password still looks "unset" to the
        // PGPASSWORD/.pgpass fallback below it, which then appends a
        // second `password` pair that `tokio_postgres` treats as
        // overriding the service's (last query-string occurrence wins).
        let dir = tempfile::tempdir().unwrap();
        let service_path = dir.path().join("pg_service.conf");
        std::fs::write(&service_path, "[prod]\npassword=svcpass\n").unwrap();
        temp_env::with_vars(
            [
                ("PGSERVICEFILE", Some(service_path.to_str().unwrap())),
                ("PGPASSWORD", Some("ambient-password")),
            ],
            || {
                let sanitized = sanitize_db_url("postgres://host/db?service=prod").unwrap();
                let config: tokio_postgres::Config = sanitized.parse().unwrap();
                assert_eq!(config.get_password(), Some(b"svcpass".as_slice()));
            },
        );
    }

    #[test]
    fn sanitize_falls_back_to_hostaddr_for_pgpass_lookup_when_host_is_absent() {
        // libpq matches a `.pgpass` entry's host field against `hostaddr`
        // when no `host` is given at all — defaulting straight to
        // "localhost" (as if `hostaddr` weren't there) misses that entry.
        let dir = tempfile::tempdir().unwrap();
        let pgpass_path = dir.path().join("pgpass");
        std::fs::write(&pgpass_path, "10.0.0.5:5432:db:user:from-hostaddr-pgpass\n").unwrap();
        set_pgpass_permissions_safe(&pgpass_path);
        // Scopes away PGSERVICE too — none of these connection strings set
        // an inline `service=`, so an ambient/racing value would send them
        // through service-file resolution instead of the pgpass lookup
        // path this test is actually about.
        temp_env::with_vars(
            [
                ("PGPASSWORD", None::<&str>),
                ("PGPASSFILE", Some(pgpass_path.to_str().unwrap())),
                ("PGSERVICE", None::<&str>),
            ],
            || {
                let sanitized = sanitize_db_url("hostaddr=10.0.0.5 dbname=db user=user").unwrap();
                let config: tokio_postgres::Config = sanitized.parse().unwrap();
                assert_eq!(
                    config.get_password(),
                    Some(b"from-hostaddr-pgpass".as_slice())
                );
            },
        );
    }

    #[test]
    fn sanitize_falls_back_to_hostaddr_for_pgpass_lookup_in_url_form_too() {
        let dir = tempfile::tempdir().unwrap();
        let pgpass_path = dir.path().join("pgpass");
        std::fs::write(&pgpass_path, "10.0.0.5:5432:db:user:from-hostaddr-pgpass\n").unwrap();
        set_pgpass_permissions_safe(&pgpass_path);
        // Scopes away PGSERVICE too — none of these connection strings set
        // an inline `service=`, so an ambient/racing value would send them
        // through service-file resolution instead of the pgpass lookup
        // path this test is actually about.
        temp_env::with_vars(
            [
                ("PGPASSWORD", None::<&str>),
                ("PGPASSFILE", Some(pgpass_path.to_str().unwrap())),
                ("PGSERVICE", None::<&str>),
            ],
            || {
                let sanitized =
                    sanitize_db_url("postgres:///db?hostaddr=10.0.0.5&user=user").unwrap();
                let config: tokio_postgres::Config = sanitized.parse().unwrap();
                assert_eq!(
                    config.get_password(),
                    Some(b"from-hostaddr-pgpass".as_slice())
                );
            },
        );
    }

    #[test]
    fn sanitize_uses_passfile_parameter_for_password_lookup_in_url_form() {
        let dir = tempfile::tempdir().unwrap();
        let passfile_path = dir.path().join("custom_pgpass");
        std::fs::write(&passfile_path, "host:5432:db:user:from-passfile\n").unwrap();
        set_pgpass_permissions_safe(&passfile_path);
        temp_env::with_vars(
            [
                ("PGPASSWORD", None::<&str>),
                ("PGPASSFILE", Some("/nonexistent-pgpassfile-for-test")),
            ],
            || {
                let sanitized = sanitize_db_url(&format!(
                    "postgres://user@host/db?passfile={}",
                    passfile_path.to_str().unwrap()
                ))
                .unwrap();
                // Parsing successfully at all proves "passfile" was
                // stripped — tokio_postgres has no such key and would
                // otherwise reject it with an UnknownOption error.
                let config: tokio_postgres::Config = sanitized.parse().unwrap();
                assert_eq!(config.get_password(), Some(b"from-passfile".as_slice()));
            },
        );
    }

    #[test]
    fn sanitize_uses_passfile_parameter_for_password_lookup_in_keyword_form() {
        let dir = tempfile::tempdir().unwrap();
        let passfile_path = dir.path().join("custom_pgpass");
        std::fs::write(&passfile_path, "host:5432:db:user:from-passfile\n").unwrap();
        set_pgpass_permissions_safe(&passfile_path);
        temp_env::with_vars(
            [
                ("PGPASSWORD", None::<&str>),
                ("PGPASSFILE", Some("/nonexistent-pgpassfile-for-test")),
            ],
            || {
                // `keyword_value_token` (rather than raw interpolation)
                // properly quotes/escapes the path — load-bearing on
                // Windows, where paths contain backslashes that this
                // format's own escaping syntax would otherwise misparse.
                let sanitized = sanitize_db_url(&format!(
                    "host=host dbname=db user=user {}",
                    keyword_value_token("passfile", passfile_path.to_str().unwrap())
                ))
                .unwrap();
                let config: tokio_postgres::Config = sanitized.parse().unwrap();
                assert_eq!(config.get_password(), Some(b"from-passfile".as_slice()));
            },
        );
    }

    #[test]
    fn sanitize_uses_last_occurrence_of_repeated_passfile_in_url_form() {
        // `take_pair` used to remove only the *first* matching pair,
        // leaving a repeated `passfile=` still in `pairs` to be re-emitted
        // — `tokio_postgres` has no `passfile` keyword at all, so that
        // leftover duplicate would be rejected outright with an
        // `UnknownOption` error. `libpq`'s own `PQconninfoParse` keeps the
        // *last* value for a repeated key, so `take_pair` must consume
        // every occurrence and return the last one, not just the first.
        let dir = tempfile::tempdir().unwrap();
        let wrong_path = dir.path().join("wrong_pgpass");
        std::fs::write(&wrong_path, "host:5432:db:user:from-wrong-passfile\n").unwrap();
        set_pgpass_permissions_safe(&wrong_path);
        let right_path = dir.path().join("right_pgpass");
        std::fs::write(&right_path, "host:5432:db:user:from-right-passfile\n").unwrap();
        set_pgpass_permissions_safe(&right_path);
        // Scopes away PGSSLCERT/PGSSLKEY too — see the comment on
        // `sanitize_rejects_sslmode_allow_with_a_clear_message`.
        temp_env::with_vars(
            [
                ("PGPASSWORD", None::<&str>),
                ("PGPASSFILE", Some("/nonexistent-pgpassfile-for-test")),
                ("PGSSLCERT", None::<&str>),
                ("PGSSLKEY", None::<&str>),
            ],
            || {
                let sanitized = sanitize_db_url(&format!(
                    "postgres://user@host/db?passfile={}&passfile={}",
                    wrong_path.to_str().unwrap(),
                    right_path.to_str().unwrap()
                ))
                .unwrap();
                let config: tokio_postgres::Config = sanitized.parse().unwrap();
                assert_eq!(
                    config.get_password(),
                    Some(b"from-right-passfile".as_slice())
                );
            },
        );
    }

    #[test]
    fn sanitize_uses_last_occurrence_of_repeated_service_in_keyword_form() {
        // See `sanitize_uses_last_occurrence_of_repeated_passfile_in_url_form`
        // for the same bug, but for a repeated `service=` in keyword form.
        let dir = tempfile::tempdir().unwrap();
        let service_path = dir.path().join("pg_service.conf");
        std::fs::write(
            &service_path,
            "[old]\ndbname=old_db\n[prod]\ndbname=prod_db\n",
        )
        .unwrap();
        // Scopes away PGSSLCERT/PGSSLKEY too — see the comment on
        // `sanitize_rejects_sslmode_allow_with_a_clear_message`.
        temp_env::with_vars(
            [
                ("PGSERVICEFILE", Some(service_path.to_str().unwrap())),
                ("PGPASSWORD", None::<&str>),
                ("PGPASSFILE", Some("/nonexistent-pgpass-for-test")),
                ("PGSSLCERT", None::<&str>),
                ("PGSSLKEY", None::<&str>),
            ],
            || {
                let sanitized = sanitize_db_url("host=host service=old service=prod").unwrap();
                let config: tokio_postgres::Config = sanitized.parse().unwrap();
                assert_eq!(config.get_dbname(), Some("prod_db"));
            },
        );
    }

    #[test]
    fn sanitize_percent_decodes_explicit_password_when_forced_into_keyword_form() {
        // Explicit credentials in a URL are percent-encoded on the wire
        // (`url::Url::password()` returns the raw "p%40ss", never decoded
        // by the `url` crate itself) — normally that's fine, since we hand
        // the URL straight back to tokio_postgres and it decodes when
        // reparsing. But when a service group supplies a port the URL
        // authority didn't have, needing_keyword_form kicks in and
        // re-emits the credentials as literal keyword-form values, which
        // have no percent-encoding concept at all — so the raw
        // "p%40ss" must be decoded to "p@ss" before that happens.
        let dir = tempfile::tempdir().unwrap();
        let service_path = dir.path().join("pg_service.conf");
        std::fs::write(&service_path, "[prod]\nport=6543\n").unwrap();
        temp_env::with_vars(
            [
                ("PGSERVICEFILE", Some(service_path.to_str().unwrap())),
                ("PGPASSWORD", None::<&str>),
                ("PGPASSFILE", Some("/nonexistent-pgpass-for-test")),
            ],
            || {
                let sanitized =
                    sanitize_db_url("postgres://user:p%40ss@host/db?service=prod").unwrap();
                let config: tokio_postgres::Config = sanitized.parse().unwrap();
                assert_eq!(config.get_user(), Some("user"));
                assert_eq!(config.get_password(), Some(b"p@ss".as_slice()));
                assert_eq!(config.get_ports(), &[6543]);
            },
        );
    }

    #[test]
    #[cfg(unix)]
    fn sanitize_percent_decodes_explicit_unix_socket_host_when_forced_into_keyword_form() {
        // `tokio_postgres`'s Unix-socket-path URL form uses the host
        // component as a percent-encoded path (e.g.
        // `postgresql://%2Fvar%2Frun%2Fpostgresql/db`) — `url::Url::host_str`
        // returns that raw, never decoded. Same fix as the password case
        // above: a service-supplied port that forces `needs_keyword_form`
        // must not re-emit `host=%2Fvar%2Frun%2Fpostgresql` literally, or
        // `tokio_postgres` tries to resolve that as a hostname instead of
        // using the socket directory.
        let dir = tempfile::tempdir().unwrap();
        let service_path = dir.path().join("pg_service.conf");
        std::fs::write(&service_path, "[prod]\nport=6543\n").unwrap();
        temp_env::with_vars(
            [
                ("PGSERVICEFILE", Some(service_path.to_str().unwrap())),
                ("PGPASSWORD", None::<&str>),
                ("PGPASSFILE", Some("/nonexistent-pgpass-for-test")),
            ],
            || {
                let sanitized =
                    sanitize_db_url("postgresql://%2Fvar%2Frun%2Fpostgresql/db?service=prod")
                        .unwrap();
                let config: tokio_postgres::Config = sanitized.parse().unwrap();
                assert_eq!(
                    config.get_hosts(),
                    &[tokio_postgres::config::Host::Unix(
                        std::path::PathBuf::from("/var/run/postgresql")
                    )]
                );
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
    fn sanitize_drops_unsupported_ssl_params_from_a_service_group_in_url_form() {
        // A service group can carry the exact same libpq-only TLS params an
        // inline connection string can — they must go through the same
        // filter/rejection, not reach `tokio_postgres` unfiltered.
        // `sslmode=prefer` (not `require`) so this stays a plain "unsupported
        // param dropped" case rather than tripping the require+sslrootcert
        // rejection tested separately below.
        let dir = tempfile::tempdir().unwrap();
        let service_path = dir.path().join("pg_service.conf");
        std::fs::write(
            &service_path,
            "[prod]\ndbname=svcdb\nsslmode=prefer\nsslrootcert=/ca.pem\n",
        )
        .unwrap();
        temp_env::with_vars(
            [
                ("PGSERVICEFILE", Some(service_path.to_str().unwrap())),
                ("PGPASSWORD", None::<&str>),
                ("PGPASSFILE", Some("/nonexistent-pgpass-for-test")),
            ],
            || {
                let sanitized = sanitize_db_url("postgres://host/db?service=prod").unwrap();
                // Must parse against tokio_postgres's *real* parser — it
                // rejects `sslrootcert` outright, so this proves the
                // service group's params were actually filtered rather
                // than just checking our own string content.
                let config: tokio_postgres::Config = sanitized.parse().unwrap();
                assert_eq!(config.get_dbname(), Some("db"));
                assert_eq!(
                    config.get_ssl_mode(),
                    tokio_postgres::config::SslMode::Prefer
                );
            },
        );
    }

    #[test]
    fn sanitize_rejects_require_with_sslrootcert_from_a_service_group() {
        let dir = tempfile::tempdir().unwrap();
        let service_path = dir.path().join("pg_service.conf");
        std::fs::write(
            &service_path,
            "[prod]\ndbname=svcdb\nsslmode=require\nsslrootcert=/ca.pem\n",
        )
        .unwrap();
        temp_env::with_vars(
            [
                ("PGSERVICEFILE", Some(service_path.to_str().unwrap())),
                ("PGPASSWORD", None::<&str>),
                ("PGPASSFILE", Some("/nonexistent-pgpass-for-test")),
            ],
            || {
                let err = sanitize_db_url("postgres://host/db?service=prod").unwrap_err();
                assert!(err.message().contains("sslrootcert"));
            },
        );
    }

    #[test]
    fn sanitize_rejects_require_from_url_combined_with_sslrootcert_from_service_group() {
        // The require+sslrootcert combination must be caught even when
        // split across sources — `sslmode=require` inline with the service
        // group supplying only `sslrootcert` (no `sslmode` of its own) is
        // just as much a "verify against this CA" request as having both in
        // the same place, and must not silently drop the CA and connect
        // unverified just because neither single source had both keys.
        let dir = tempfile::tempdir().unwrap();
        let service_path = dir.path().join("pg_service.conf");
        std::fs::write(&service_path, "[prod]\nsslrootcert=/ca.pem\n").unwrap();
        temp_env::with_vars(
            [
                ("PGSERVICEFILE", Some(service_path.to_str().unwrap())),
                ("PGPASSWORD", None::<&str>),
                ("PGPASSFILE", Some("/nonexistent-pgpass-for-test")),
            ],
            || {
                let err =
                    sanitize_db_url("postgres://host/db?sslmode=require&service=prod").unwrap_err();
                assert!(err.message().contains("sslrootcert"));
            },
        );
    }

    #[test]
    fn sanitize_rejects_sslrootcert_from_url_combined_with_require_from_service_group() {
        // The reverse split: the URL supplies `sslrootcert` (silently
        // dropped by itself, since nothing there says `require`) while the
        // service group is what actually asks for `require`.
        let dir = tempfile::tempdir().unwrap();
        let service_path = dir.path().join("pg_service.conf");
        std::fs::write(&service_path, "[prod]\nsslmode=require\n").unwrap();
        temp_env::with_vars(
            [
                ("PGSERVICEFILE", Some(service_path.to_str().unwrap())),
                ("PGPASSWORD", None::<&str>),
                ("PGPASSFILE", Some("/nonexistent-pgpass-for-test")),
            ],
            || {
                let err = sanitize_db_url("postgres://host/db?sslrootcert=/ca.pem&service=prod")
                    .unwrap_err();
                assert!(err.message().contains("sslrootcert"));
            },
        );
    }

    #[test]
    fn sanitize_rejects_require_from_keyword_form_combined_with_sslrootcert_from_service_group() {
        let dir = tempfile::tempdir().unwrap();
        let service_path = dir.path().join("pg_service.conf");
        std::fs::write(&service_path, "[prod]\nsslrootcert=/ca.pem\n").unwrap();
        temp_env::with_vars(
            [
                ("PGSERVICEFILE", Some(service_path.to_str().unwrap())),
                ("PGPASSWORD", None::<&str>),
                ("PGPASSFILE", Some("/nonexistent-pgpass-for-test")),
            ],
            || {
                let err = sanitize_db_url("host=host dbname=db sslmode=require service=prod")
                    .unwrap_err();
                assert!(err.message().contains("sslrootcert"));
            },
        );
    }

    #[test]
    fn sanitize_drops_unsupported_ssl_params_from_a_service_group_in_keyword_form() {
        let dir = tempfile::tempdir().unwrap();
        let service_path = dir.path().join("pg_service.conf");
        std::fs::write(&service_path, "[prod]\nsslrootcert=/ca.pem\n").unwrap();
        temp_env::with_vars(
            [
                ("PGSERVICEFILE", Some(service_path.to_str().unwrap())),
                ("PGPASSWORD", None::<&str>),
                ("PGPASSFILE", Some("/nonexistent-pgpass-for-test")),
            ],
            || {
                let sanitized = sanitize_db_url("host=localhost dbname=db service=prod").unwrap();
                let config: tokio_postgres::Config = sanitized.parse().unwrap();
                assert_eq!(config.get_dbname(), Some("db"));
            },
        );
    }

    #[test]
    fn sanitize_rejects_verify_full_from_a_service_group_instead_of_a_raw_parse_error() {
        let dir = tempfile::tempdir().unwrap();
        let service_path = dir.path().join("pg_service.conf");
        std::fs::write(&service_path, "[prod]\nsslmode=verify-full\n").unwrap();
        // Scopes away PGSSLCERT/PGSSLKEY — see the comment on
        // `sanitize_rejects_verify_ca_in_url_form_instead_of_downgrading`.
        temp_env::with_vars(
            [
                ("PGSERVICEFILE", Some(service_path.to_str().unwrap())),
                ("PGSSLCERT", None::<&str>),
                ("PGSSLKEY", None::<&str>),
            ],
            || {
                let err = sanitize_db_url("postgres://host/db?service=prod").unwrap_err();
                assert!(err.message().contains("verify-full"));
            },
        );
    }

    #[test]
    fn sanitize_errors_when_named_service_is_missing_from_service_file() {
        let dir = tempfile::tempdir().unwrap();
        let service_path = dir.path().join("pg_service.conf");
        std::fs::write(&service_path, "[other]\nhost=x\n").unwrap();
        // Scopes away PGSYSCONFDIR so an ambient/racing value pointing at a
        // real system-wide service file that happens to define "missing"
        // can't turn this into a flake now that `load_service` also checks
        // it as a fallback.
        temp_env::with_vars(
            [
                ("PGSERVICEFILE", Some(service_path.to_str().unwrap())),
                ("PGSYSCONFDIR", None::<&str>),
            ],
            || {
                let err = sanitize_db_url("postgres://host/db?service=missing").unwrap_err();
                assert!(err.message().contains("missing"));
            },
        );
    }

    #[test]
    fn sanitize_falls_back_to_system_wide_service_file_via_pgsysconfdir() {
        // PostgreSQL documents (libpq-pgservice.html) that a service name
        // may be defined in either the per-user service file or a
        // system-wide `pg_service.conf` located via `PGSYSCONFDIR` -- the
        // compiled-in sysconfdir default isn't derivable from this crate
        // (same precedent as not replicating libpq's compiled-in default
        // Unix socket directory), so only the `PGSYSCONFDIR` override is
        // supported. `PGSERVICEFILE` points at a file that doesn't define
        // the service at all, so this must fall through to the system file.
        let user_dir = tempfile::tempdir().unwrap();
        let user_service_path = user_dir.path().join("pg_service.conf");
        std::fs::write(&user_service_path, "[other]\nhost=x\n").unwrap();

        let sys_dir = tempfile::tempdir().unwrap();
        std::fs::write(
            sys_dir.path().join("pg_service.conf"),
            "[prod]\nhost=syswide\nport=6543\n",
        )
        .unwrap();

        temp_env::with_vars(
            [
                ("PGSERVICEFILE", Some(user_service_path.to_str().unwrap())),
                ("PGSYSCONFDIR", Some(sys_dir.path().to_str().unwrap())),
                ("PGPASSWORD", None::<&str>),
                ("PGPASSFILE", Some("/nonexistent-pgpass-for-test")),
            ],
            || {
                let sanitized = sanitize_db_url("postgres://host/db?service=prod").unwrap();
                let config: tokio_postgres::Config = sanitized.parse().unwrap();
                assert_eq!(config.get_ports(), &[6543]);
            },
        );
    }

    #[test]
    fn sanitize_prefers_user_service_file_over_system_wide_one() {
        // libpq docs: "If the same service name exists in both the user and
        // the system file, the user file takes precedence."
        let user_dir = tempfile::tempdir().unwrap();
        let user_service_path = user_dir.path().join("pg_service.conf");
        std::fs::write(&user_service_path, "[prod]\nhost=userhost\nport=1111\n").unwrap();

        let sys_dir = tempfile::tempdir().unwrap();
        std::fs::write(
            sys_dir.path().join("pg_service.conf"),
            "[prod]\nhost=syshost\nport=2222\n",
        )
        .unwrap();

        temp_env::with_vars(
            [
                ("PGSERVICEFILE", Some(user_service_path.to_str().unwrap())),
                ("PGSYSCONFDIR", Some(sys_dir.path().to_str().unwrap())),
                ("PGPASSWORD", None::<&str>),
                ("PGPASSFILE", Some("/nonexistent-pgpass-for-test")),
            ],
            || {
                let sanitized = sanitize_db_url("postgres://host/db?service=prod").unwrap();
                let config: tokio_postgres::Config = sanitized.parse().unwrap();
                assert_eq!(config.get_ports(), &[1111]);
            },
        );
    }

    #[test]
    fn sanitize_errors_when_service_is_missing_from_both_user_and_system_files() {
        let user_dir = tempfile::tempdir().unwrap();
        let user_service_path = user_dir.path().join("pg_service.conf");
        std::fs::write(&user_service_path, "[other]\nhost=x\n").unwrap();

        let sys_dir = tempfile::tempdir().unwrap();
        std::fs::write(sys_dir.path().join("pg_service.conf"), "[other]\nhost=y\n").unwrap();

        temp_env::with_vars(
            [
                ("PGSERVICEFILE", Some(user_service_path.to_str().unwrap())),
                ("PGSYSCONFDIR", Some(sys_dir.path().to_str().unwrap())),
            ],
            || {
                let err = sanitize_db_url("postgres://host/db?service=missing").unwrap_err();
                assert!(err.message().contains("missing"));
            },
        );
    }

    #[test]
    fn sanitize_uses_pgservice_env_var_when_no_inline_service_param() {
        // `PGSERVICE` behaves like an inline `service=` parameter per
        // `libpq`'s own docs — a bare connection string relying on it (the
        // way `psql` did) must not silently skip service-file resolution
        // just because `service=` wasn't spelled out in the string itself.
        let dir = tempfile::tempdir().unwrap();
        let service_path = dir.path().join("pg_service.conf");
        std::fs::write(&service_path, "[prod]\ndbname=svcdb\n").unwrap();
        temp_env::with_vars(
            [
                ("PGSERVICEFILE", Some(service_path.to_str().unwrap())),
                ("PGSERVICE", Some("prod")),
                ("PGPASSWORD", None::<&str>),
                ("PGPASSFILE", Some("/nonexistent-pgpass-for-test")),
            ],
            || {
                let sanitized = sanitize_db_url("postgres://host").unwrap();
                let config: tokio_postgres::Config = sanitized.parse().unwrap();
                assert_eq!(config.get_dbname(), Some("svcdb"));
            },
        );
    }

    #[test]
    fn sanitize_uses_pgservice_env_var_in_keyword_form_too() {
        let dir = tempfile::tempdir().unwrap();
        let service_path = dir.path().join("pg_service.conf");
        std::fs::write(&service_path, "[prod]\ndbname=svcdb\n").unwrap();
        temp_env::with_vars(
            [
                ("PGSERVICEFILE", Some(service_path.to_str().unwrap())),
                ("PGSERVICE", Some("prod")),
                ("PGPASSWORD", None::<&str>),
                ("PGPASSFILE", Some("/nonexistent-pgpass-for-test")),
            ],
            || {
                let sanitized = sanitize_db_url("host=host").unwrap();
                let config: tokio_postgres::Config = sanitized.parse().unwrap();
                assert_eq!(config.get_dbname(), Some("svcdb"));
            },
        );
    }

    #[test]
    fn sanitize_prefers_inline_service_param_over_pgservice_env_var() {
        let dir = tempfile::tempdir().unwrap();
        let service_path = dir.path().join("pg_service.conf");
        std::fs::write(
            &service_path,
            "[inline_wins]\ndbname=inline_db\n[wrong]\ndbname=wrong_db\n",
        )
        .unwrap();
        temp_env::with_vars(
            [
                ("PGSERVICEFILE", Some(service_path.to_str().unwrap())),
                ("PGSERVICE", Some("wrong")),
                ("PGPASSWORD", None::<&str>),
                ("PGPASSFILE", Some("/nonexistent-pgpass-for-test")),
            ],
            || {
                let sanitized = sanitize_db_url("postgres://host?service=inline_wins").unwrap();
                let config: tokio_postgres::Config = sanitized.parse().unwrap();
                assert_eq!(config.get_dbname(), Some("inline_db"));
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
