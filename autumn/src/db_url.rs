//! Credential redaction for configured database targets.
//!
//! Every database target Autumn is given can carry a secret, and several boot
//! paths print one: the config validator's "Invalid database URL", the pool
//! builder's backend refusals, the migration wait loop's connection errors, and
//! the startup config summary. All of them reach `tracing::error!` /
//! `tracing::info!`, so under `log.format = "json"` an unredacted target lands
//! in whatever ships the structured log stream.
//!
//! This module is the one redactor those paths share. It replaced three with
//! different behaviours (`app.rs`'s `mask_database_url`, `migrate.rs`'s
//! `redact_db_url_credentials`, `db.rs`'s `redact_pool_target`), the weakest of
//! which recognized only `postgres://` tokens and passed a
//! `mysql://user:pw@host` through untouched.
//!
//! It is deliberately NOT behind `#[cfg(feature = "db")]`: `config.rs`
//! validates database targets on every build.

use crate::config::DatabaseBackend;

/// `SQLite` URI parameters that are diagnostic rather than sensitive.
///
/// A `SQLite` target's query string is the detail the read-only and replica
/// refusals exist to report (`mode=ro`, `mode=memory`, `cache=shared`), so it
/// is kept — but by allowlist, not wholesale. `SQLite` has no authentication,
/// so no parameter outside this list has a reason to be echoed.
const SQLITE_DIAGNOSTIC_PARAMS: [&str; 5] = ["mode", "cache", "immutable", "vfs", "nolock"];

/// Keys in a libpq keyword/value string that merely identify WHICH target this
/// is, or how it connects. Everything else is dropped.
///
/// `sslmode` earns its place beside the addressing keys: it is the first thing
/// an operator looks at when a TLS posture is wrong, and it names a policy, not
/// a secret. `sslpassword` is deliberately absent.
const IDENTIFYING_KEYWORDS: [&str; 6] = ["host", "hostaddr", "port", "dbname", "user", "sslmode"];

/// Query parameters that are diagnostic on a target whose backend we
/// recognized as Postgres.
///
/// For an UNRECOGNIZED backend the whole query string goes, because the keys
/// that matter cannot be enumerated. For Postgres they can: these are
/// connection policy and identification, and the shapes that carry a secret
/// (`password`, `sslpassword`, and anything unlisted) are not among them.
/// Blanking the lot instead would cost the boot summary its most-read detail —
/// `?sslmode=verify-full&application_name=web` is exactly what an operator
/// checks first.
const PG_DIAGNOSTIC_PARAMS: [&str; 9] = [
    "sslmode",
    "sslrootcert",
    "sslcert",
    "application_name",
    "connect_timeout",
    "target_session_attrs",
    "host",
    "hostaddr",
    "port",
];

/// Whether a target is one of the two scheme-less spellings the `SQLite` pool
/// maps to `:memory:` — the bare `:memory:` token and the empty string.
///
/// [`DatabaseBackend::detect`] classifies by scheme, so it returns `None` for
/// both even though the pool has always accepted them. Kept next to the
/// redactor because that is what has to classify a target the same way on
/// either build.
pub fn is_bare_in_memory_sqlite(url: &str) -> bool {
    url.is_empty() || url == ":memory:"
}

/// Refuse a target that does not name Postgres, for a store that opens its own
/// Postgres connection from a URL.
///
/// `PgFlagStore::new(url)` and its siblings do not screen the target when they
/// are built. Without this check at connect time, a `SQLite` target fails in
/// libpq, and libpq quotes the target with its credentials in the error
/// (#2539 §5). An empty target passes: libpq reads it as "use the `PG*`
/// environment defaults".
///
/// # Errors
///
/// Returns the refusal message, with the target redacted.
pub fn require_postgres_target(url: &str, store: &str) -> Result<(), String> {
    if url.trim().is_empty() || DatabaseBackend::detect(url) == Some(DatabaseBackend::Postgres) {
        return Ok(());
    }
    Err(format!(
        "{store} needs a Postgres database target, got {:?}; on another backend, \
         use the in-memory store",
        redact_target(url)
    ))
}

/// Mask credentials in a database target before it goes into a message.
///
/// **A `SQLite` target keeps its filename and its diagnostic parameters.** It
/// is a local file URI with no authentication, and the query string is exactly
/// what makes a read-only or replica refusal actionable. Userinfo is masked
/// anyway and unrecognized parameters are dropped: `detect` classifies by
/// scheme, so `sqlite://user:hunter2@host/app.db` reaches here whole, and "this
/// kind of target cannot carry a secret" is the assumption that was already
/// wrong three times below.
///
/// **Anything else keeps only what it is safe to enumerate.** Userinfo is not
/// the only place a secret rides: `?password=`, `?sslpassword=`, `?api_key=`
/// are all real spellings, and `Url::password()` sees none of them. A
/// recognized Postgres target keeps its connection POLICY
/// ([`PG_DIAGNOSTIC_PARAMS`] — `sslmode` and friends), because that is what the
/// boot summary is read for; a target that named no backend loses its whole
/// query string, because the keys that matter there cannot be enumerated at
/// all.
///
/// **The default is to mask.** "Hand it back verbatim" has been wrong for
/// userinfo, then the query string, then libpq keyword/value strings, so what
/// reaches the end unclassified is masked. That includes a URL that PARSED but
/// whose userinfo the parser never saw: `postgres:/user:pw@host/db` (one
/// slash) and `jdbc:postgresql://user:pw@host/db` have no authority, so the
/// credentials sit in the path — any stray `@` there means we did not
/// understand the target and it goes wholesale.
///
/// The one verbatim exception is positive and narrow: a single path-shaped
/// token with no `=` (so it carries no key/value pair), no `@` (no userinfo),
/// no `?` (no query) and no whitespace (so it is one token, not a keyword/value
/// string). A bare filesystem path is exactly that, and is the case where
/// naming the target is the whole value of the message.
///
/// An OPAQUE url (`postgres:password=hunter2`) parses, but the parser reports
/// no password, query or fragment in it. It keeps its scheme only, unless the
/// rest is one path-shaped token (`mysql:app.db`).
///
/// An allowlisted query key does not make its value safe. A kept value that is
/// not a simple token is masked ([`is_simple_token`]).
pub fn redact_target(url: &str) -> String {
    let backend = DatabaseBackend::detect(url);
    if backend == Some(DatabaseBackend::Sqlite) {
        return redact_sqlite_target(url);
    }
    let is_postgres = backend == Some(DatabaseBackend::Postgres);
    if is_bare_in_memory_sqlite(url) {
        return url.to_owned();
    }
    if let Ok(mut parsed) = url::Url::parse(url) {
        // Userinfo the PARSER did not account for is userinfo we did not mask.
        // A URL with no authority keeps it in the path: `postgres:/user:pw@h/db`
        // (one slash), `jdbc:postgresql://user:pw@h/db` (opaque outer scheme),
        // `SQLITE:user:pw@h/app.db` (uppercase, so scheme detection routed it
        // here). `Url::password()` is `None` for all three.
        if parsed.path().contains('@')
            || parsed
                .fragment()
                .is_some_and(|fragment| fragment.contains('@'))
        {
            return "****".to_owned();
        }
        // A one-letter scheme is a Windows drive letter: a path. It gets the
        // bare-path test below.
        if parsed.scheme().len() == 1 {
            return if is_bare_path(url) {
                url.to_owned()
            } else {
                "****".to_owned()
            };
        }
        // JDBC-style `;k=v` material in a host or a path is not understood.
        if parsed
            .host_str()
            .is_some_and(|host| host.contains([';', '=']))
            || (!parsed.cannot_be_a_base() && parsed.path().contains([';', '=']))
        {
            return format!("{}://****", parsed.scheme());
        }
        // No authority, so the parser saw no userinfo, query or fragment: do
        // not trust the rest. Keep it only when it is one path-shaped token,
        // as for a bare path below.
        if parsed.cannot_be_a_base() {
            let opaque = url.split_once(':').map_or("", |(_, rest)| rest);
            if opaque.is_empty()
                || opaque.contains(['=', '@', '?', ':', '%', '#'])
                || opaque.contains(char::is_whitespace)
            {
                return format!("{}:****", parsed.scheme());
            }
        }
        let has_password = parsed.password().is_some();
        if has_password {
            let _ = parsed.set_password(Some("****"));
        }
        // A fragment is not part of any database target Autumn accepts, so
        // nothing in one is worth printing and there are no key names to
        // allowlist. `#password=hunter2` survived every other check here:
        // `password()` is `None`, `query()` is `None`, and the fail-closed
        // scan above only looks for `@`.
        let has_fragment = parsed.fragment().is_some();
        if has_fragment {
            parsed.set_fragment(Some("****"));
        }
        let redacted_query = parsed
            .query()
            .map(|query| filter_query(query, &PG_DIAGNOSTIC_PARAMS, is_postgres));
        let query_changed = redacted_query.as_deref().is_some_and(|redacted| {
            let changed = Some(redacted) != parsed.query();
            parsed.set_query(Some(redacted));
            changed
        });
        // Re-rendering a parsed URL normalizes it, so only hand back the
        // rewritten form when something actually had to be hidden.
        if has_password || query_changed || has_fragment {
            return parsed.to_string();
        }
        return url.to_owned();
    }
    // A libpq keyword/value connection string (`host=db user=app
    // password=hunter2`) is an explicitly supported Postgres target that
    // `Url::parse` rejects and that carries no `@` or `?` — so the bare-path
    // fallback below would hand the password straight back. Rebuild it from an
    // ALLOWLIST: a key this code has never heard of must not default to being
    // printed.
    if let Some(pairs) = crate::pg_conn_str::keyword_value_pairs(url) {
        let kept: Vec<String> = pairs
            .iter()
            .filter(|(key, _)| IDENTIFYING_KEYWORDS.contains(&key.as_str()))
            .map(|(key, value)| format!("{key}={}", identifying_value(value)))
            .collect();
        return if kept.is_empty() {
            "****".to_owned()
        } else {
            format!("{} ****", kept.join(" "))
        };
    }
    if is_bare_path(url) {
        return url.to_owned();
    }
    "****".to_owned()
}

/// One path-shaped token: no `=` (no key/value pair), no `@` (no userinfo), no
/// `?` (no query) and no whitespace (not a keyword/value string).
fn is_bare_path(url: &str) -> bool {
    !url.is_empty() && !url.contains(['=', '@', '?']) && !url.contains(char::is_whitespace)
}

/// Mask userinfo and drop non-diagnostic parameters in a `SQLite` target.
///
/// String surgery rather than a `Url` round-trip: the accepted spellings
/// (`sqlite::memory:`, `file::memory:?cache=shared`, `sqlite:app.db`) do not
/// all survive `Url`'s normalization intact, and a target that comes back
/// respelled is a worse diagnostic than the one the operator wrote.
///
/// Userinfo is masked FIRST, on the whole string and at the LAST `@`. Splitting
/// the query off first would miss a `?` inside the userinfo
/// (`sqlite://admin:pa?ss@db/app.db`), and looking only inside a `//` authority
/// would miss the documented scheme-only spellings (`sqlite:user:pw@h/app.db`).
fn redact_sqlite_target(url: &str) -> String {
    // Same reasoning as the URL arm: a `SQLite` target has no fragment, so
    // whatever rides in one is masked rather than echoed.
    let (url, fragment) = match url.split_once('#') {
        Some((target, _)) => (target, "#****"),
        None => (url, ""),
    };
    let (scheme, rest) = split_sqlite_scheme(url);
    let (userinfo, rest) = match rest.rsplit_once('@') {
        Some((_, after)) => ("****@", after),
        None => ("", rest),
    };
    let (target, query) = match rest.split_once('?') {
        Some((target, query)) => (target, Some(query)),
        None => (rest, None),
    };
    let head = format!("{scheme}{userinfo}{target}");
    query.map_or_else(
        || format!("{head}{fragment}"),
        |query| {
            format!(
                "{head}?{}{fragment}",
                filter_query(query, &SQLITE_DIAGNOSTIC_PARAMS, true)
            )
        },
    )
}

/// Split a `SQLite` target into its scheme prefix and the rest.
///
/// The prefixes are exactly the ones
/// [`DatabaseBackend::detect`] recognizes, longest first so `sqlite://` wins
/// over `sqlite:`.
fn split_sqlite_scheme(url: &str) -> (&str, &str) {
    for scheme in ["sqlite://", "sqlite:", "file:"] {
        if let Some(rest) = url.strip_prefix(scheme) {
            return (scheme, rest);
        }
    }
    ("", url)
}

/// Keep the `allowed` parameters of a query string and replace the rest with a
/// single `****`.
///
/// `enumerable` says whether the caller could enumerate the keys at all: for a
/// target whose backend we did not recognize, the answer is no, and the whole
/// query goes — a key this code has never heard of must not default to being
/// printed.
fn filter_query(query: &str, allowed: &[&str], enumerable: bool) -> String {
    if !enumerable {
        return "****".to_owned();
    }
    let mut kept: Vec<String> = Vec::new();
    let mut dropped = false;
    // BOTH separators. Splitting on `&` alone let a `;`-joined tail ride
    // through on the back of an allowed key —
    // `?sslmode=require;password=hunter2` was one pair whose key was
    // `sslmode`. `;` is what an operator pastes in from a JDBC-style string.
    for pair in query.split(['&', ';']) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        if !allowed.iter().any(|a| key.trim().eq_ignore_ascii_case(a)) {
            dropped = true;
        } else if is_simple_token(value) || (is_host_key(key) && is_host_token(value)) {
            kept.push(pair.to_owned());
        } else {
            kept.push(format!("{key}=****"));
        }
    }
    if dropped {
        kept.push("****".to_owned());
    }
    kept.join("&")
}

fn is_host_key(key: &str) -> bool {
    let key = key.trim();
    key.eq_ignore_ascii_case("host") || key.eq_ignore_ascii_case("hostaddr")
}

/// A host value may also hold an IPv6 address: `::1`, `[::1]`.
fn is_host_token(value: &str) -> bool {
    value.chars().all(|c| {
        c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | ',' | '/' | ':' | '[' | ']')
    })
}

/// Whether a query value is a simple token that is safe to echo.
///
/// The default is to mask. A value with `@`, `:`, `%`, whitespace or a quote
/// can hold a nested target or an encoded one, so it does not pass. Paths
/// (`/var/run/postgresql`) and host lists (`db1,db2`) pass.
fn is_simple_token(value: &str) -> bool {
    value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | '~' | ',' | '/' | '+'))
}

/// Re-render one allowlisted keyword/value pair's value.
///
/// The allowlist is on KEY names, and libpq lets an identifying value be a
/// whole target of its own (`dbname=postgres://app:pw@h/x`), so a value that
/// looks like one is masked rather than echoed. A value containing whitespace
/// is re-quoted, so what comes back still reads as what the operator wrote.
fn identifying_value(value: &str) -> String {
    if value.contains("://") || value.contains('@') {
        return "****".to_owned();
    }
    if value.contains(char::is_whitespace) {
        return format!("'{value}'");
    }
    value.to_owned()
}

/// Redact a driver error about the target `url`.
///
/// libpq can quote ONE decoded part of the target, not the whole target
/// (`invalid percent-encoded token: "p%ss"`), and a scheme scan finds nothing
/// in that. This redactor knows the target, so it does three things:
///
/// 1. It masks a quoted span that is not part of the [`redact_target`] form of
///    `url`, or that holds a password from `url`.
/// 2. It runs [`redact_targets_in_message`].
/// 3. It masks each password from `url` that is still in the text.
pub fn redact_driver_error(msg: &str, url: &str) -> String {
    let safe = redact_target(url);
    let secrets = secrets_in(url);
    let holds_secret = |text: &str| secrets.iter().any(|secret| text.contains(secret.as_str()));
    let mut out = String::with_capacity(msg.len());
    let mut rest = msg;
    while let Some(open) = rest.find('"') {
        let after = &rest[open + 1..];
        let Some(close) = after.find('"') else {
            break;
        };
        let span = &after[..close];
        out.push_str(&rest[..=open]);
        if span.is_empty() || (safe.contains(span) && !holds_secret(span)) {
            out.push_str(span);
        } else {
            out.push_str("****");
        }
        out.push('"');
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    let mut out = redact_targets_in_message(&out);
    for secret in &secrets {
        out = out.replace(secret.as_str(), "****");
    }
    out
}

/// The passwords in a target, raw and percent-decoded: the userinfo password
/// of a URL, and each `password` / `sslpassword` value of a query or of a
/// keyword/value string.
///
/// Read by string surgery, not by `Url::parse`: a target that libpq rejects
/// can be one that `Url::parse` rejects too.
fn secrets_in(url: &str) -> Vec<String> {
    let mut raw: Vec<String> = Vec::new();
    if let Some((_, rest)) = url.split_once("://") {
        if let Some((userinfo, _)) = rest.rsplit_once('@')
            && let Some((_, password)) = userinfo.split_once(':')
        {
            raw.push(password.to_owned());
        }
        if let Some((_, query)) = rest.split_once('?') {
            for pair in query.split(['&', ';']) {
                if let Some((key, value)) = pair.split_once('=')
                    && is_password_key(key)
                {
                    raw.push(value.to_owned());
                }
            }
        }
    } else if let Some(pairs) = crate::pg_conn_str::keyword_value_pairs(url) {
        raw.extend(
            pairs
                .into_iter()
                .filter(|(key, _)| is_password_key(key))
                .map(|(_, value)| value),
        );
    }
    let mut secrets = Vec::new();
    for secret in raw.into_iter().filter(|secret| !secret.is_empty()) {
        let decoded = percent_encoding::percent_decode_str(&secret)
            .decode_utf8_lossy()
            .into_owned();
        if decoded != secret && !decoded.is_empty() {
            secrets.push(decoded);
        }
        secrets.push(secret);
    }
    secrets
}

fn is_password_key(key: &str) -> bool {
    let key = key.trim();
    key.eq_ignore_ascii_case("password") || key.eq_ignore_ascii_case("sslpassword")
}

/// Redact every database target embedded in a free-text message.
///
/// Used where the text is a driver error rather than a target Autumn holds —
/// the migration paths quote libpq's own message, which contains the URI it was
/// handed. Each whitespace-delimited chunk is searched for a target and the
/// match, plus everything after it in that chunk, is routed through
/// [`redact_target`]; everything else is left alone.
pub fn redact_targets_in_message(msg: &str) -> String {
    msg.split_inclusive(char::is_whitespace)
        .map(|chunk| {
            let trimmed = chunk.trim_end();
            let trailing = &chunk[trimmed.len()..];
            target_start(trimmed).map_or_else(
                || chunk.to_owned(),
                |at| {
                    // A closing delimiter is not part of the target. libpq
                    // quotes the URI it rejects, so leaving the `"` attached
                    // fed it to `Url::parse`, which re-emitted it as `%22`
                    // inside the path — a mangled message for no gain. Split
                    // it off and put it back after.
                    let (target, closer) = split_closing_delimiters(&trimmed[at..]);
                    format!(
                        "{}{}{closer}{trailing}",
                        &trimmed[..at],
                        redact_target(target)
                    )
                },
            )
        })
        .collect()
}

/// Split a trailing run of closing delimiters off a target.
///
/// Only the three a target is actually wrapped in — `"` (libpq's own quoting),
/// `'` and `)`. Not `]`, which ends an IPv6 host.
fn split_closing_delimiters(token: &str) -> (&str, &str) {
    let end = token.trim_end_matches(['"', '\'', ')']).len();
    (&token[..end], &token[end..])
}

/// Byte offset where a database target starts inside `token`, if any.
///
/// Searched ANYWHERE in the token, not just at its start: libpq wraps the URI
/// it rejects in quotes or parentheses (`… in URI: "postgres://app:pw@[::1/db"`)
/// and operators write `url=postgres://…`. Requiring the token to begin with a
/// scheme would hand every one of those back whole — the coverage the previous
/// substring-matching redactor had.
///
/// Any `scheme://`, not just the two Postgres spellings: a
/// `mysql://user:pw@host` in a driver message is just as much a credential.
fn target_start(token: &str) -> Option<usize> {
    if let Some(sep) = token.find("://") {
        return Some(scheme_start(token, sep));
    }
    // The scheme-only SQLite spellings, which have no `//`.
    ["sqlite:", "file:"].iter().find_map(|scheme| {
        let at = token.find(scheme)?;
        // Must start the token or follow a delimiter, so `usesqlite:x` is prose.
        let starts_cleanly = at == 0
            || token[..at]
                .chars()
                .next_back()
                .is_some_and(|c| !is_scheme_char(c));
        starts_cleanly.then_some(at)
    })
}

/// Walk left from a `://` at `sep` to the first byte of its scheme.
fn scheme_start(token: &str, sep: usize) -> usize {
    let mut start = sep;
    for (idx, ch) in token[..sep].char_indices().rev() {
        if is_scheme_char(ch) {
            start = idx;
        } else {
            break;
        }
    }
    start
}

/// The characters a URL scheme may contain (RFC 3986).
const fn is_scheme_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.'
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_credentials_but_keeps_plain_targets_legible() {
        // Userinfo, on any scheme.
        assert_eq!(
            redact_target("mysql://user:secret@host/db"),
            "mysql://user:****@host/db"
        );
        assert_eq!(
            redact_target("postgres://user:secret@localhost:5432/app"),
            "postgres://user:****@localhost:5432/app"
        );

        // An unrecognized backend's query string goes wholesale — its key
        // names cannot be enumerated, and `Url::password()` sees none of them.
        assert_eq!(
            redact_target("mysql://host/db?password=hunter2"),
            "mysql://host/db?****"
        );
        // A Postgres target's CAN be enumerated, so the policy parameters an
        // operator reads the boot summary for survive and only the rest goes.
        assert_eq!(
            redact_target("postgres://host/app?sslpassword=hunter2&sslmode=require"),
            "postgres://host/app?sslmode=require&****"
        );
        assert_eq!(
            redact_target(
                "postgres://app@db/app?sslmode=verify-full&application_name=web&connect_timeout=5"
            ),
            "postgres://app@db/app?sslmode=verify-full&application_name=web&connect_timeout=5"
        );
        assert_eq!(
            redact_target("mysql://user:secret@host/db?api_key=hunter2"),
            "mysql://user:****@host/db?****"
        );

        // A bare path names the target and hides nothing.
        assert_eq!(redact_target("/var/lib/app.db"), "/var/lib/app.db");

        // SQLite targets stay legible — this is what the read-only and
        // replica refusals report on.
        assert_eq!(
            redact_target("sqlite:///var/lib/app.db"),
            "sqlite:///var/lib/app.db"
        );
        assert_eq!(redact_target("sqlite::memory:"), "sqlite::memory:");
        assert_eq!(
            redact_target("sqlite://file:app.db?mode=ro"),
            "sqlite://file:app.db?mode=ro"
        );
        assert_eq!(
            redact_target("file::memory:?cache=shared"),
            "file::memory:?cache=shared"
        );
        assert_eq!(redact_target(":memory:"), ":memory:");
    }

    // `detect` classifies by SCHEME, so anything at all can ride in the rest of
    // a `sqlite://` target. SQLite has no authentication, so userinfo there is
    // never deliberate — but it is still printed by the default build's
    // "requires --features sqlite" refusal and by the replica refusal.
    #[test]
    fn sqlite_targets_do_not_carry_credentials_through() {
        assert_eq!(
            redact_target("sqlite://user:hunter2@host/app.db"),
            "sqlite://****@host/app.db"
        );
        assert_eq!(
            redact_target("sqlite:///var/lib/app.db?password=hunter2"),
            "sqlite:///var/lib/app.db?****"
        );
        // A diagnostic parameter survives alongside a dropped one.
        assert_eq!(
            redact_target("file:app.db?mode=ro&password=hunter2"),
            "file:app.db?mode=ro&****"
        );
    }

    #[test]
    fn keyword_value_targets_keep_only_identifying_keys() {
        assert_eq!(
            redact_target("host=db user=app password=hunter2"),
            "host=db user=app ****"
        );
        assert_eq!(
            redact_target("host=db port=5432 dbname=app sslmode=require password=hunter2"),
            "host=db port=5432 dbname=app sslmode=require ****"
        );
        assert_eq!(redact_target("password=hunter2"), "****");
    }

    // FAIL CLOSED: what could not be classified at all is masked, including a
    // malformed keyword/value string that carries neither `@` nor `?`.
    #[test]
    fn unclassifiable_targets_are_masked() {
        assert_eq!(redact_target("host=db user=app password='hunter2"), "****");
        assert_eq!(redact_target("://user:secret@host/db"), "****");
        assert_eq!(redact_target("not-a-url?password=hunter2"), "****");
        assert_eq!(redact_target("garbage password=hunter2 more"), "****");
    }

    // A URL that PARSES but whose userinfo the parser never saw. All three of
    // these were handed back whole: `Url::password()` is `None` when there is
    // no authority, so the credential rides in the path.
    #[test]
    fn userinfo_outside_an_authority_fails_closed() {
        // One slash instead of three.
        assert_eq!(
            redact_target("postgres:/user:hunter2@db.internal/app"),
            "****"
        );
        // A JDBC-style outer scheme — a routine copy-paste from a Java config.
        assert_eq!(
            redact_target("jdbc:postgresql://user:hunter2@db/app"),
            "****"
        );
        // Uppercase dodges the (case-sensitive) SQLite scheme detection, so it
        // arrives here rather than at the SQLite arm.
        assert_eq!(redact_target("SQLITE:user:hunter2@host/app.db"), "****");
        // A username with no password is not a secret and stays legible.
        assert_eq!(
            redact_target("postgres://app@db.internal/app"),
            "postgres://app@db.internal/app"
        );
    }

    // A fragment is not part of any database target Autumn accepts, and it
    // survived every other check: `Url::password()` is `None`, `query()` is
    // `None`, and the fail-closed scan looks only for `@`. The whole string
    // then reached `DatabaseConfig::validate`'s boot error.
    #[test]
    fn a_url_fragment_cannot_carry_a_secret_out() {
        assert_eq!(
            redact_target("mysql://host/db#password=hunter2"),
            "mysql://host/db#****"
        );
        assert_eq!(
            redact_target("postgres://host/db#password=hunter2"),
            "postgres://host/db#****"
        );
        assert_eq!(
            redact_target("sqlite:///var/lib/app.db#password=hunter2"),
            "sqlite:///var/lib/app.db#****"
        );
        // The diagnostic parts still survive alongside it.
        assert_eq!(
            redact_target("sqlite://file:app.db?mode=ro#password=hunter2"),
            "sqlite://file:app.db?mode=ro#****"
        );
    }

    // `;` is a query separator an operator pastes in from a JDBC-style string.
    // Split on `&` alone, `sslmode=require;password=hunter2` was ONE pair whose
    // key was `sslmode` — allowlisted, so the tail rode through on its back.
    #[test]
    fn a_semicolon_joined_tail_does_not_ride_through() {
        assert_eq!(
            redact_target("postgres://db.internal/app?sslmode=require;password=hunter2"),
            "postgres://db.internal/app?sslmode=require&****"
        );
        assert_eq!(
            redact_target("file:app.db?mode=ro;password=hunter2"),
            "file:app.db?mode=ro&****"
        );
    }

    // The allowlist is on KEY names, and libpq lets an identifying value be a
    // whole target of its own.
    #[test]
    fn an_identifying_keyword_value_that_is_itself_a_target_is_masked() {
        assert_eq!(
            redact_target("host=db dbname=postgres://app:hunter2@h/x"),
            "host=db dbname=**** ****"
        );
        // Quoting is round-tripped, so the result reads as what was written.
        assert_eq!(
            redact_target("host=db user='app x' password=hunter2"),
            "host=db user='app x' ****"
        );
    }

    // An opaque URL parses, but the parser reports no password, query or
    // fragment. Key/value material in it is masked (#2571 item 1).
    #[test]
    fn an_opaque_url_keeps_only_its_scheme() {
        assert_eq!(redact_target("postgres:password=hunter2"), "postgres:****");
        assert_eq!(
            redact_target("postgresql:host=db password=hunter2"),
            "postgresql:****"
        );
        assert_eq!(redact_target("postgres:app:hunter2"), "postgres:****");
        assert_eq!(redact_target("mysql:pass%77ord"), "mysql:****");
        // One path-shaped token stays, as a bare path does.
        assert_eq!(redact_target("mysql:app.db"), "mysql:app.db");
        // A Windows drive letter parses as a one-letter scheme. It is a path.
        assert_eq!(redact_target(r"C:\data\app.db"), r"C:\data\app.db");
    }

    // An allowlisted key does not make its value safe. A value that is not a
    // simple token is masked (#2571 item 4).
    #[test]
    fn an_allowlisted_query_value_must_be_a_simple_token() {
        assert_eq!(
            redact_target("postgres://db/app?application_name=postgres://app:hunter2@other/db"),
            "postgres://db/app?application_name=****"
        );
        assert_eq!(
            redact_target("postgres://db/app?sslmode=require&application_name=app:hunter2@other"),
            "postgres://db/app?sslmode=require&application_name=****"
        );
        // Simple tokens stay legible: paths, lists, numbers.
        assert_eq!(
            redact_target(
                "postgres://db/app?host=/var/run/postgresql&sslrootcert=/etc/ssl/root.crt"
            ),
            "postgres://db/app?host=/var/run/postgresql&sslrootcert=/etc/ssl/root.crt"
        );
        // The SQLite arm: no credential comes out, and a plain value stays.
        let out = redact_target("sqlite://file:app.db?mode=ro&vfs=postgres://app:hunter2@other/db");
        assert!(!out.contains("hunter2"), "leaked: {out}");
        assert_eq!(
            redact_target("file:app.db?mode=ro&vfs=unix dotfile"),
            "file:app.db?mode=ro&vfs=****"
        );
    }

    // libpq can quote ONE decoded part of a target, not the whole target,
    // so a scheme scan finds nothing. The redactor that knows the target
    // masks it (#2539 review).
    #[test]
    fn a_driver_error_does_not_echo_a_part_of_the_target() {
        let url = "postgres://app:p%ss@localhost:1/db";
        let out = redact_driver_error(r#"invalid percent-encoded token: "p%ss""#, url);
        assert_eq!(out, r#"invalid percent-encoded token: "****""#);

        let url = "postgres://app:hun ter2@[::1/db";
        let out = redact_driver_error(
            r#"unexpected spaces found in "hun ter2", use percent-encoded spaces (%20) instead"#,
            url,
        );
        assert!(!out.contains("ter2"), "{out}");
        // libpq 16 quotes the whole target, which holds a space.
        let out = redact_driver_error(r#"invalid URI: "postgres://app:hun ter2@[::1/db""#, url);
        assert!(!out.contains("ter2"), "{out}");

        // A keyword/value password is masked wherever it shows.
        let out = redact_driver_error("failed near hunter2", "host=db password=hunter2");
        assert!(!out.contains("hunter2"), "{out}");

        // A quoted part of the redacted target stays legible.
        assert_eq!(
            redact_driver_error(
                r#"could not translate host name "db" to address"#,
                "postgres://app:pw@db/app"
            ),
            r#"could not translate host name "db" to address"#
        );
    }

    // JDBC-style `;k=v` material in a host or a path is not understood.
    #[test]
    fn key_value_material_in_a_host_or_path_is_masked() {
        assert_eq!(
            redact_target("postgres://db;password=hunter2/app"),
            "postgres://****"
        );
        assert_eq!(
            redact_target("postgres://host/db;password=hunter2"),
            "postgres://****"
        );
        let refusal = require_postgres_target("mysql://host/db;password=hunter2", "Store")
            .expect_err("not Postgres");
        assert!(!refusal.contains("hunter2"), "{refusal}");
    }

    // A one-letter scheme is a drive letter. It gets the bare-path test.
    #[test]
    fn a_one_letter_scheme_gets_the_bare_path_test() {
        assert_eq!(redact_target("x:password=hunter2"), "****");
        assert_eq!(redact_target(r"C:\data\pw=hunter2.db"), "****");
        assert_eq!(redact_target(r"C:\data\app.db"), r"C:\data\app.db");
    }

    #[test]
    fn an_ipv6_host_value_stays_legible() {
        assert_eq!(
            redact_target("postgres://db/app?host=[::1]&hostaddr=::1"),
            "postgres://db/app?host=[::1]&hostaddr=::1"
        );
        // Only on the host keys.
        assert_eq!(
            redact_target("postgres://db/app?application_name=a:b"),
            "postgres://db/app?application_name=****"
        );
    }

    // An empty conninfo means "libpq defaults from the PG* environment".
    #[test]
    fn an_empty_conninfo_is_a_postgres_target_for_the_stores() {
        assert!(require_postgres_target("", "Store").is_ok());
        assert!(require_postgres_target("  ", "Store").is_ok());
        assert!(require_postgres_target("sqlite::memory:", "Store").is_err());
    }

    #[test]
    fn message_redaction_covers_every_scheme_and_keeps_the_prose() {
        assert_eq!(
            redact_targets_in_message("failed: postgres://user:secret@host:5432/db"),
            "failed: postgres://user:****@host:5432/db"
        );
        // The old message redactor recognized `postgres://` tokens only.
        assert_eq!(
            redact_targets_in_message("failed: mysql://user:secret@host/db"),
            "failed: mysql://user:****@host/db"
        );
        assert_eq!(
            redact_targets_in_message("connection refused at postgres://host:5432/db"),
            "connection refused at postgres://host:5432/db"
        );
        // Prose that merely mentions a colon is left alone.
        assert_eq!(
            redact_targets_in_message("note: retrying in 500ms"),
            "note: retrying in 500ms"
        );
        // libpq QUOTES the URI it rejects, and an operator writes `url=…`.
        // Requiring the token to START with a scheme handed all of these back
        // whole — the coverage the substring-matching predecessor had.
        //
        // Asserted by EXACT equality, not just "the password is gone": these
        // are the inputs where the scanner has to guess a token boundary
        // inside a delimiter, so over-redacting them is the likelier failure,
        // and a redactor that returned a bare `****` for all five would pass a
        // leak-only check.
        assert_eq!(
            redact_targets_in_message(
                r#"invalid connection string: "postgres://user:hunter2@host/db""#
            ),
            r#"invalid connection string: "postgres://user:****@host/db""#
        );
        assert_eq!(
            redact_targets_in_message("connect failed (postgres://user:hunter2@host/db)"),
            "connect failed (postgres://user:****@host/db)"
        );
        assert_eq!(
            redact_targets_in_message("url=postgres://user:hunter2@host/db"),
            "url=postgres://user:****@host/db"
        );
        assert_eq!(
            redact_targets_in_message("'postgres://user:hunter2@host/db'"),
            "'postgres://user:****@host/db'"
        );
        // Unparseable (libpq's own diagnostic): the target goes wholesale, but
        // the prose and its delimiters still frame it.
        assert_eq!(
            redact_targets_in_message(
                r#"end of string reached when looking for matching "]" in URI: "postgres://app:hunter2@[::1/db""#
            ),
            r#"end of string reached when looking for matching "]" in URI: "****""#
        );
    }
}
