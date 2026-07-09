//! Postgres connection-string shape helpers, shared between the pool's TLS
//! module (`db::tls`, feature `db`) and config validation (`config`, always
//! compiled).
//!
//! tokio-postgres accepts two syntaxes: `postgres://`/`postgresql://` URLs,
//! and libpq-style keyword/value strings (`host=db user=app sslmode=require`).
//! Both consumers must agree on how those are recognized — config validation
//! rejecting a string the TLS parser supports would make the keyword form
//! unreachable (issue #1585 review).

/// Whether tokio-postgres would parse this connection string as a URL.
/// Its `Config::from_str` only treats `postgres://`/`postgresql://`
/// prefixes as URLs; everything else is keyword/value syntax. A substring
/// check like `contains("://")` would send keyword strings whose values
/// embed URL-like tokens (`password=https://…`) down the URL path.
pub fn is_url(database_url: &str) -> bool {
    database_url.starts_with("postgres://") || database_url.starts_with("postgresql://")
}

/// Parse a libpq-style `key = value` connection string into its pairs,
/// mirroring tokio-postgres's (private) connection-string parser:
/// whitespace is allowed around `=`, values may be single-quoted, and
/// `\` escapes the next character both inside and outside quotes
/// (`host=db sslmode = require sslrootcert='/pki/my ca.pem'`). Naive
/// whitespace splitting would miss `sslmode` in such strings and
/// silently downgrade the TLS posture.
///
/// Returns `None` for strings tokio-postgres itself would reject
/// (missing `=`, empty unquoted value, unterminated quote), so callers
/// keep the default path and surface tokio-postgres's own parse error
/// at connect time. Like tokio-postgres, parsing stops silently at an
/// empty keyword (e.g. a stray leading `=`).
pub fn keyword_value_pairs(s: &str) -> Option<Vec<(String, String)>> {
    let mut pairs = Vec::new();
    let mut it = s.chars().peekable();
    loop {
        while it.next_if(|c| c.is_whitespace()).is_some() {}
        let mut key = String::new();
        while let Some(&c) = it.peek() {
            if c.is_whitespace() || c == '=' {
                break;
            }
            key.push(c);
            it.next();
        }
        if key.is_empty() {
            return Some(pairs);
        }
        while it.next_if(|c| c.is_whitespace()).is_some() {}
        if it.next() != Some('=') {
            return None;
        }
        while it.next_if(|c| c.is_whitespace()).is_some() {}
        let mut value = String::new();
        if it.next_if_eq(&'\'').is_some() {
            loop {
                // `?`: EOF inside quotes (also right after `\`) is an
                // unterminated value.
                match it.next()? {
                    '\'' => break,
                    '\\' => value.push(it.next()?),
                    c => value.push(c),
                }
            }
        } else {
            while let Some(&c) = it.peek() {
                if c.is_whitespace() {
                    break;
                }
                it.next();
                if c == '\\' {
                    match it.next() {
                        Some(c2) => value.push(c2),
                        None => break,
                    }
                } else {
                    value.push(c);
                }
            }
            if value.is_empty() {
                // `key=` followed by whitespace/EOF: tokio-postgres
                // rejects the empty unquoted value (use `key=''`).
                return None;
            }
        }
        pairs.push((key, value));
    }
}

/// Whether `s` is a plausible libpq-style keyword/value connection string —
/// the shape check config validation uses to accept the keyword form.
///
/// Stricter than [`keyword_value_pairs`] alone: the string must contain at
/// least one pair, and every keyword must look like a libpq parameter name
/// (`[A-Za-z][A-Za-z0-9_]*` — `host`, `dbname`, `sslmode`, …). The key-shape
/// check keeps mistyped URL schemes (`mysql://db/app?a=b`, whose whole
/// prefix would otherwise parse as one weird keyword) rejected with the
/// clear config error instead of failing later at connect time.
pub fn is_keyword_value(s: &str) -> bool {
    keyword_value_pairs(s).is_some_and(|pairs| {
        !pairs.is_empty()
            && pairs.iter().all(|(key, _)| {
                let mut chars = key.chars();
                chars.next().is_some_and(|c| c.is_ascii_alphabetic())
                    && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_prefixes_are_urls() {
        assert!(is_url("postgres://u@h/db"));
        assert!(is_url("postgresql://u@h/db"));
        assert!(!is_url("host=db user=u"));
        assert!(!is_url("mysql://u@h/db"));
    }

    #[test]
    fn keyword_form_is_accepted() {
        for s in [
            "host=db user=app dbname=app",
            "host=db sslmode=require",
            "host=db sslmode = require",
            "host=db password='p w' sslmode='verify-full'",
            "host=db password=https://looks-like-a-url sslmode=require",
        ] {
            assert!(is_keyword_value(s), "must accept keyword form: {s}");
        }
    }

    #[test]
    fn garbage_is_rejected() {
        for s in [
            "",
            "   ",
            "not a connection string",
            "localhost",
            "mysql://localhost/db",
            "mysql://localhost/db?a=b",
            "host=db port",       // trailing junk without `=`
            "host=",              // empty unquoted value
            "host='unterminated", // unterminated quote
        ] {
            assert!(!is_keyword_value(s), "must reject: {s:?}");
        }
    }
}
