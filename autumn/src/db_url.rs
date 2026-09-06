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
/// is. Everything else is dropped.
const IDENTIFYING_KEYWORDS: [&str; 5] = ["host", "hostaddr", "port", "dbname", "user"];

/// Whether a target is one of the two scheme-less spellings the `SQLite` pool
/// maps to `:memory:` — the bare `:memory:` token and the empty string.
///
/// [`DatabaseBackend::detect`] classifies by scheme, so it returns `None` for
/// both even though the pool has always accepted them. Kept next to the
/// redactor because that is what has to classify a target the same way on
/// either build.
pub(crate) fn is_bare_in_memory_sqlite(url: &str) -> bool {
    url.is_empty() || url == ":memory:"
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
/// **Anything else keeps only what identifies WHICH target was misconfigured.**
/// Userinfo is not the only place a secret rides: `?password=`,
/// `?sslpassword=`, `?api_key=` are all real spellings, and `Url::password()`
/// sees none of them. For a target that named no backend we cannot even
/// enumerate the keys that matter, so rather than guess at key names the whole
/// query string is replaced — scheme, host and path are enough to tell an
/// operator which URL to go fix.
///
/// **The default is to mask.** "Hand it back verbatim" has been wrong for
/// userinfo, then the query string, then libpq keyword/value strings, so what
/// reaches the end unclassified is masked. The one exception is positive and
/// narrow: a single path-shaped token with no `=` (so it carries no key/value
/// pair), no `@` (no userinfo), no `?` (no query) and no whitespace (so it is
/// one token, not a keyword/value string). A bare filesystem path is exactly
/// that, and is the case where naming the target is the whole value of the
/// message.
pub(crate) fn redact_target(url: &str) -> String {
    if DatabaseBackend::detect(url) == Some(DatabaseBackend::Sqlite) {
        return redact_sqlite_target(url);
    }
    if is_bare_in_memory_sqlite(url) {
        return url.to_owned();
    }
    if let Ok(mut parsed) = url::Url::parse(url) {
        let has_password = parsed.password().is_some();
        if has_password {
            let _ = parsed.set_password(Some("****"));
        }
        let has_query = parsed.query().is_some();
        if has_query {
            parsed.set_query(Some("****"));
        }
        // Re-rendering a parsed URL normalizes it, so only hand back the
        // rewritten form when something actually had to be hidden.
        if has_password || has_query {
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
            .map(|(key, value)| format!("{key}={value}"))
            .collect();
        return if kept.is_empty() {
            "****".to_owned()
        } else {
            format!("{} ****", kept.join(" "))
        };
    }
    if !url.is_empty() && !url.contains(['=', '@', '?']) && !url.contains(char::is_whitespace) {
        return url.to_owned();
    }
    "****".to_owned()
}

/// Mask userinfo and drop non-diagnostic parameters in a `SQLite` target.
///
/// String surgery rather than a `Url` round-trip: the accepted spellings
/// (`sqlite::memory:`, `file::memory:?cache=shared`, `sqlite:app.db`) do not
/// all survive `Url`'s normalization intact, and a target that comes back
/// respelled is a worse diagnostic than the one the operator wrote.
fn redact_sqlite_target(url: &str) -> String {
    let (target, query) = match url.split_once('?') {
        Some((target, query)) => (target, Some(query)),
        None => (url, None),
    };

    // Userinfo only exists inside an authority, i.e. after `//`.
    let mut redacted = target.to_owned();
    if let Some(authority_start) = target.find("//") {
        let authority_start = authority_start + 2;
        let authority_end = target[authority_start..]
            .find('/')
            .map_or(target.len(), |offset| authority_start + offset);
        let authority = &target[authority_start..authority_end];
        if let Some((_, host)) = authority.rsplit_once('@') {
            redacted = format!(
                "{}****@{host}{}",
                &target[..authority_start],
                &target[authority_end..]
            );
        }
    }

    let Some(query) = query else {
        return redacted;
    };
    let mut kept: Vec<&str> = Vec::new();
    let mut dropped = false;
    for pair in query.split('&') {
        let key = pair.split_once('=').map_or(pair, |(key, _)| key).trim();
        if SQLITE_DIAGNOSTIC_PARAMS
            .iter()
            .any(|allowed| key.eq_ignore_ascii_case(allowed))
        {
            kept.push(pair);
        } else {
            dropped = true;
        }
    }
    if dropped {
        kept.push("****");
    }
    if kept.is_empty() {
        redacted
    } else {
        format!("{redacted}?{}", kept.join("&"))
    }
}

/// Redact every database target embedded in a free-text message.
///
/// Used where the text is a driver error rather than a target Autumn holds —
/// the migration wait loop quotes libpq's own message, which contains the URL
/// it was handed. Each whitespace-delimited token that looks like a database
/// target is routed through [`redact_target`]; everything else is left alone.
pub(crate) fn redact_targets_in_message(msg: &str) -> String {
    msg.split_inclusive(char::is_whitespace)
        .map(|chunk| {
            let trimmed = chunk.trim_end();
            let trailing = &chunk[trimmed.len()..];
            if looks_like_target(trimmed) {
                format!("{}{trailing}", redact_target(trimmed))
            } else {
                chunk.to_owned()
            }
        })
        .collect()
}

/// Whether a token is worth handing to [`redact_target`].
///
/// Any `scheme://` token, not just the two Postgres spellings: the previous
/// redactor recognized `postgres://` / `postgresql://` only, so a
/// `mysql://user:pw@host` in a driver message went out whole.
fn looks_like_target(token: &str) -> bool {
    let Some((scheme, rest)) = token.split_once(':') else {
        return false;
    };
    if scheme.is_empty()
        || !scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.')
    {
        return false;
    }
    rest.starts_with("//")
        || scheme.eq_ignore_ascii_case("sqlite")
        || scheme.eq_ignore_ascii_case("file")
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

        // The query string goes wholesale — `Url::password()` never sees these.
        assert_eq!(
            redact_target("mysql://host/db?password=hunter2"),
            "mysql://host/db?****"
        );
        assert_eq!(
            redact_target("postgres://host/app?sslpassword=hunter2&sslmode=require"),
            "postgres://host/app?****"
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
            "host=db port=5432 dbname=app ****"
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
    }
}
