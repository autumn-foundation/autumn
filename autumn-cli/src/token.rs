//! `autumn token` -- issue and revoke API bearer tokens.
//!
//! Tokens are generated with 256 bits of OS-backed randomness, hashed with
//! SHA-256 before storage, and inserted / revoked via `psql`. The database URL
//! is resolved from `autumn.toml` or environment variables using the same
//! logic as `autumn migrate`.

use chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime};
use hex::encode as hex_encode;
use sha2::{Digest as _, Sha256};
use std::process::{Command, Stdio};

/// Issue a new API token for `principal_id` and print the raw token to stdout.
///
/// `name` is a human-readable label, `scopes` are flat permission strings
/// (e.g. `posts:read`) stored as a JSON array, and `expires_at` is an optional
/// SQL/ISO-8601 timestamp after which the token is rejected.
pub fn run_issue(principal_id: &str, name: &str, scopes: &[String], expires_at: Option<&str>) {
    let database_url = resolve_database_url();
    check_psql();

    let raw_token = generate_token();
    let token_hash = sha256_hex(&raw_token);
    let scopes_json = scopes_to_json(scopes);
    // Normalize expires_at to a UTC naive timestamp string before passing to
    // psql.  The column is TIMESTAMP (no time zone), storing UTC; we cast with
    // ::timestamp so PostgreSQL never applies a session-timezone conversion.
    // Without normalization, '2026-12-31T23:59:59'::timestamptz would be
    // interpreted in the session's TimeZone and then stripped of its zone,
    // causing tokens to expire hours early/late on non-UTC DB sessions.
    let expires_at_owned;
    let expires_at = match expires_at {
        Some(s) if !s.trim().is_empty() => {
            expires_at_owned = normalize_expires_at_utc(s);
            expires_at_owned.as_str()
        }
        _ => "",
    };

    // Use psql variable substitution (:'var') to avoid any SQL injection risk.
    // psql quotes each value as a SQL string literal, so no manual escaping is
    // needed. `:'scopes'::jsonb` casts the JSON array text into the JSONB
    // column; an empty `:'expires_at'` becomes NULL via NULLIF.  The ::timestamp
    // cast is safe here because expires_at is already UTC-normalized above.
    run_psql_silent(
        &database_url,
        "INSERT INTO api_tokens (token_hash, principal_id, name, scopes, expires_at) \
         VALUES (:'hash', :'principal', :'name', :'scopes'::jsonb, \
         NULLIF(:'expires_at', '')::timestamp);",
        &[
            ("hash", &token_hash),
            ("principal", principal_id),
            ("name", name),
            ("scopes", &scopes_json),
            ("expires_at", expires_at),
        ],
    );

    // Print raw token last so it's easy to capture with $(...)
    println!("{raw_token}");
    eprintln!("\u{2713} Token issued for principal: {principal_id}");
}

/// List non-secret metadata for a principal's tokens.
pub fn run_list(principal_id: &str) {
    let database_url = resolve_database_url();
    check_psql();

    run_psql_with_vars_or_die(
        &database_url,
        "SELECT id, name, principal_id, scopes, created_at, expires_at, last_used_at, revoked_at \
         FROM api_tokens WHERE principal_id = :'principal' ORDER BY id;",
        &[("principal", principal_id)],
    );
}

/// Rotate a token: revoke `raw_token` and issue a replacement carrying the same
/// name, scopes, and expiry. Prints the new raw token to stdout.
pub fn run_rotate(raw_token: &str) {
    let database_url = resolve_database_url();
    check_psql();

    let old_hash = sha256_hex(raw_token);
    let new_token = generate_token();
    let new_hash = sha256_hex(&new_token);

    // Revoke the old row and copy its name/scopes/expiry onto a new row in a
    // single statement so the rotation is atomic.
    // The SELECT 1/COUNT(*) forces psql to error if the old token was not found
    // (already revoked or never existed), preventing a silent no-op rotation.
    run_psql_silent(
        &database_url,
        "WITH rotated AS ( \
            UPDATE api_tokens SET revoked_at = NOW() AT TIME ZONE 'utc' \
            WHERE token_hash = :'oldhash' AND revoked_at IS NULL \
                AND (expires_at IS NULL OR expires_at > NOW() AT TIME ZONE 'utc') \
            RETURNING principal_id, name, scopes, expires_at \
         ), \
         inserted AS ( \
            INSERT INTO api_tokens (token_hash, principal_id, name, scopes, expires_at) \
            SELECT :'newhash', principal_id, name, scopes, expires_at FROM rotated \
            RETURNING 1 \
         ) \
         SELECT 1 / COUNT(*) FROM inserted;",
        &[("oldhash", &old_hash), ("newhash", &new_hash)],
    );

    println!("{new_token}");
    eprintln!("\u{2713} Token rotated.");
}

/// Revoke the token identified by `raw_token`.
pub fn run_revoke(raw_token: &str) {
    let database_url = resolve_database_url();
    check_psql();

    let token_hash = sha256_hex(raw_token);

    run_psql_with_vars_or_die(
        &database_url,
        "UPDATE api_tokens SET revoked_at = NOW() AT TIME ZONE 'utc' WHERE token_hash = :'hash' AND revoked_at IS NULL;",
        &[("hash", &token_hash)],
    );
    eprintln!("\u{2713} Token revoked.");
}

// ── internals ─────────────────────────────────────────────────────────────────────────────

fn sha256_hex(input: &str) -> String {
    hex_encode(Sha256::digest(input.as_bytes()))
}

/// Normalize an `expires_at` string to a UTC naive datetime suitable for a
/// `TIMESTAMP` (no time zone) column that stores UTC values.
///
/// Accepts RFC 3339 (with offset) and bare ISO 8601-ish forms (no offset,
/// treated as UTC).  Returns the input unchanged if none of the known formats
/// match, letting psql produce a clear error.
fn normalize_expires_at_utc(s: &str) -> String {
    let s = s.trim();
    // Try RFC 3339 / ISO 8601 with explicit offset — convert to UTC.
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return dt.naive_utc().format("%Y-%m-%dT%H:%M:%S").to_string();
    }
    // Minute-precision offset forms (e.g. `date -Iminutes` → "2026-12-31T23:59-05:00").
    // RFC 3339 requires seconds; pad ":00" after the minutes field so the
    // full RFC 3339 parser can still apply the offset and produce UTC.
    if s.len() >= 17 {
        let b = s.as_bytes();
        if b[10] == b'T' && b[13] == b':' && matches!(b[16], b'Z' | b'+' | b'-') {
            let padded = format!("{}:00{}", &s[..16], &s[16..]);
            if let Ok(dt) = DateTime::parse_from_rfc3339(&padded) {
                return dt.naive_utc().format("%Y-%m-%dT%H:%M:%S").to_string();
            }
        }
    }
    // Hour-only offset forms (ISO 8601 allows ±HH with no minutes, e.g.
    // "2026-12-31T23:59:59-05" or "2026-12-31 23:59:59-05"). Expand the
    // offset to ±HH:00 and re-run so the existing parsers handle the rest.
    // Guard: require a datetime separator (T or space) at position 10 to
    // avoid treating the day-separator in date-only strings ("2026-12-31")
    // as an offset sign (the `-` before `31` would otherwise be mistaken
    // for a `-HH` hour-offset, causing `2026-12-31:00` → passthrough).
    // Termination: the expanded string ends with ±HH:MM; `b[n-3]` = `:`
    // so this branch is never re-entered.
    {
        let b = s.as_bytes();
        let n = b.len();
        if n >= 14
            && matches!(b[10], b'T' | b' ')
            && matches!(b[n - 3], b'+' | b'-')
            && b[n - 2].is_ascii_digit()
            && b[n - 1].is_ascii_digit()
        {
            return normalize_expires_at_utc(&format!("{s}:00"));
        }
    }
    // Offset-aware formats not accepted by `parse_from_rfc3339`:
    //   - compact offset (e.g. "2026-12-31T23:59:59-0500" from `date +%Y-%m-%dT%H:%M:%S%z`)
    //   - space-separated SQL form with extended or compact offset
    //     (e.g. "2026-12-31 23:59:59-05:00" from psql or ORMs)
    // Must come before the naive formats so offsets are applied, not silently stripped.
    for fmt in [
        "%Y-%m-%dT%H:%M:%S%z",  // T, compact offset, seconds
        "%Y-%m-%dT%H:%M%z",     // T, compact offset, minute-precision
        "%Y-%m-%d %H:%M:%S%:z", // space, extended offset, seconds
        "%Y-%m-%d %H:%M:%S%z",  // space, compact offset, seconds
        "%Y-%m-%d %H:%M%:z",    // space, extended offset, minute-precision
        "%Y-%m-%d %H:%M%z",     // space, compact offset, minute-precision
    ] {
        if let Ok(dt) = DateTime::parse_from_str(s, fmt) {
            return dt.naive_utc().format("%Y-%m-%dT%H:%M:%S").to_string();
        }
    }
    // Datetime without offset — treat as UTC directly.
    for fmt in ["%Y-%m-%dT%H:%M:%S", "%Y-%m-%dT%H:%M", "%Y-%m-%d %H:%M:%S"] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(s, fmt) {
            return naive.format("%Y-%m-%dT%H:%M:%S").to_string();
        }
    }
    // Date only — midnight UTC.
    if let Ok(date) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return NaiveDateTime::new(date, NaiveTime::from_hms_opt(0, 0, 0).unwrap())
            .format("%Y-%m-%dT%H:%M:%S")
            .to_string();
    }
    s.to_owned()
}

/// Render flat scope strings as a JSON array for the `scopes` JSONB column.
fn scopes_to_json(scopes: &[String]) -> String {
    let items: Vec<serde_json::Value> = scopes
        .iter()
        .map(|s| serde_json::Value::String(s.clone()))
        .collect();
    serde_json::Value::Array(items).to_string()
}

/// Produce 32 random bytes from the OS and return them as a 64-char hex string.
fn generate_token() -> String {
    hex_encode(os_random_bytes())
}

/// Read 32 cryptographically-random bytes from the OS.
fn os_random_bytes() -> [u8; 32] {
    let mut buf = [0u8; 32];
    getrandom::fill(&mut buf).expect("OS random source unavailable");
    buf
}

fn resolve_database_url() -> String {
    crate::config::resolve_database_url()
}

fn check_psql() {
    match Command::new("psql").arg("--version").output() {
        Ok(out) if out.status.success() => {
            let v = String::from_utf8_lossy(&out.stdout);
            eprintln!("  Using {}", v.trim());
        }
        _ => {
            eprintln!("\u{2717} psql not found on PATH.");
            eprintln!("  Install PostgreSQL client tools (e.g. `apt install postgresql-client`).");
            std::process::exit(1);
        }
    }
}

/// Run a SQL statement using psql variable substitution.
///
/// Each `(name, value)` in `vars` is passed as `-v name=value`, and the SQL
/// may reference them as `:'name'` (psql quotes the value as a SQL string
/// literal, preventing SQL injection).
fn run_psql_with_vars_or_die(database_url: &str, sql: &str, vars: &[(&str, &str)]) {
    run_psql_impl(database_url, sql, vars, false);
}

/// Like [`run_psql_with_vars_or_die`] but discards psql stdout so that
/// validation-only SELECT results don't pollute the caller's stdout capture
/// (e.g. `TOKEN=$(autumn token rotate …)`).
fn run_psql_silent(database_url: &str, sql: &str, vars: &[(&str, &str)]) {
    run_psql_impl(database_url, sql, vars, true);
}

fn run_psql_impl(database_url: &str, sql: &str, vars: &[(&str, &str)], suppress_stdout: bool) {
    let mut cmd = Command::new("psql");
    cmd.arg(database_url);
    for (name, value) in vars {
        cmd.args(["-v", &format!("{name}={value}")]);
    }
    cmd.args(["-c", sql]);
    if suppress_stdout {
        cmd.stdout(Stdio::null());
    }
    match cmd.status() {
        Ok(s) if s.success() => {}
        Ok(_) => {
            eprintln!("\u{2717} psql command failed. Check the output above.");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("\u{2717} Failed to run psql: {e}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_hex_is_64_chars() {
        let h = sha256_hex("test_token");
        assert_eq!(h.len(), 64);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn sha256_hex_is_deterministic() {
        assert_eq!(sha256_hex("abc"), sha256_hex("abc"));
    }

    #[test]
    fn sha256_hex_differs_on_different_input() {
        assert_ne!(sha256_hex("abc"), sha256_hex("xyz"));
    }

    #[test]
    fn scopes_to_json_renders_array() {
        assert_eq!(scopes_to_json(&[]), "[]");
        assert_eq!(
            scopes_to_json(&["posts:read".to_owned(), "posts:write".to_owned()]),
            r#"["posts:read","posts:write"]"#
        );
    }

    #[test]
    fn generate_token_produces_64_hex_chars() {
        let t = generate_token();
        assert_eq!(t.len(), 64);
        assert!(t.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn generate_token_is_not_all_zeros() {
        let t = generate_token();
        assert_ne!(t, "0".repeat(64));
    }

    #[test]
    fn resolve_prefers_autumn_database_url_over_database_url() {
        let url = temp_env::with_vars(
            [
                ("AUTUMN_DATABASE__PRIMARY_URL", None::<&str>),
                ("AUTUMN_DATABASE__URL", Some("postgres://autumn-primary")),
                ("DATABASE_URL", Some("postgres://fallback")),
            ],
            resolve_database_url,
        );
        assert_eq!(url, "postgres://autumn-primary");
    }

    #[test]
    fn resolve_falls_back_to_database_url_when_autumn_unset() {
        let url = temp_env::with_vars(
            [
                ("AUTUMN_DATABASE__PRIMARY_URL", None::<&str>),
                ("AUTUMN_DATABASE__URL", None::<&str>),
                ("DATABASE_URL", Some("postgres://fallback")),
            ],
            resolve_database_url,
        );
        assert_eq!(url, "postgres://fallback");
    }

    #[test]
    fn check_psql_does_not_panic_when_available() {
        // Run only when psql is on PATH; skip otherwise to avoid process::exit.
        if std::process::Command::new("psql")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
        {
            check_psql();
        }
    }

    #[test]
    fn normalize_utc_strips_offset_and_converts_to_utc() {
        // +05:30 → subtract 5h30m to get UTC
        assert_eq!(
            normalize_expires_at_utc("2026-12-31T23:59:59+05:30"),
            "2026-12-31T18:29:59"
        );
    }

    #[test]
    fn normalize_utc_z_suffix_passthrough() {
        assert_eq!(
            normalize_expires_at_utc("2026-12-31T23:59:59Z"),
            "2026-12-31T23:59:59"
        );
    }

    #[test]
    fn normalize_utc_no_offset_treated_as_utc() {
        assert_eq!(
            normalize_expires_at_utc("2026-12-31T23:59:59"),
            "2026-12-31T23:59:59"
        );
    }

    #[test]
    fn normalize_utc_date_only_pads_time() {
        assert_eq!(
            normalize_expires_at_utc("2026-12-31"),
            "2026-12-31T00:00:00"
        );
    }

    #[test]
    fn normalize_utc_unknown_format_passthrough() {
        assert_eq!(normalize_expires_at_utc("not-a-date"), "not-a-date");
    }

    #[test]
    fn normalize_utc_minute_precision_negative_offset() {
        // `date -Iminutes` → "2026-12-31T23:59-05:00"; 23:59 EST = 04:59 UTC next day
        assert_eq!(
            normalize_expires_at_utc("2026-12-31T23:59-05:00"),
            "2027-01-01T04:59:00"
        );
    }

    #[test]
    fn normalize_utc_minute_precision_positive_offset() {
        // +05:30 → 23:59 - 5h30m = 18:29 UTC
        assert_eq!(
            normalize_expires_at_utc("2026-12-31T23:59+05:30"),
            "2026-12-31T18:29:00"
        );
    }

    #[test]
    fn normalize_utc_minute_precision_z_suffix() {
        assert_eq!(
            normalize_expires_at_utc("2026-12-31T23:59Z"),
            "2026-12-31T23:59:00"
        );
    }

    #[test]
    fn normalize_utc_compact_offset_with_seconds() {
        // `date +%Y-%m-%dT%H:%M:%S%z` → "2026-12-31T23:59:59-0500"; 23:59:59 EST = 04:59:59 UTC
        assert_eq!(
            normalize_expires_at_utc("2026-12-31T23:59:59-0500"),
            "2027-01-01T04:59:59"
        );
    }

    #[test]
    fn normalize_utc_compact_offset_minute_precision() {
        // Minute-precision compact offset: "2026-12-31T23:59-0500" → 04:59:00 UTC
        assert_eq!(
            normalize_expires_at_utc("2026-12-31T23:59-0500"),
            "2027-01-01T04:59:00"
        );
    }

    #[test]
    fn normalize_utc_space_separator_extended_offset() {
        // SQL/psql form: "2026-12-31 23:59:59-05:00"; 23:59:59 EST = 04:59:59 UTC next day
        assert_eq!(
            normalize_expires_at_utc("2026-12-31 23:59:59-05:00"),
            "2027-01-01T04:59:59"
        );
    }

    #[test]
    fn normalize_utc_space_separator_compact_offset() {
        // Space separator + compact offset: "2026-12-31 23:59:59-0500"
        assert_eq!(
            normalize_expires_at_utc("2026-12-31 23:59:59-0500"),
            "2027-01-01T04:59:59"
        );
    }

    #[test]
    fn normalize_utc_hour_only_offset_t_separator() {
        // ISO 8601 hour-only offset: "2026-12-31T23:59:59-05"; 23:59:59 UTC-5 = 04:59:59 UTC
        assert_eq!(
            normalize_expires_at_utc("2026-12-31T23:59:59-05"),
            "2027-01-01T04:59:59"
        );
    }

    #[test]
    fn normalize_utc_hour_only_offset_space_separator() {
        // SQL form with hour-only offset: "2026-12-31 23:59:59-05"
        assert_eq!(
            normalize_expires_at_utc("2026-12-31 23:59:59-05"),
            "2027-01-01T04:59:59"
        );
    }
}
