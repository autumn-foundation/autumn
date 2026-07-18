//! Rotating "remember-me" tokens with theft detection (issue #1397).
//!
//! Implements the Barry Jaspan "improved persistent login" scheme (the same
//! model as Spring Security's `PersistentTokenBasedRememberMeServices`). A
//! remember credential is a **(series, token)** pair:
//!
//! - `series` is an opaque random id that stays **stable** across rotations for
//!   one device login-chain. It is the database lookup key.
//! - `token` is an opaque random value that **rotates on every use**.
//!
//! The server stores, per series, a *hash* of the current token (never the raw
//! token), an expiry, the owning user, and device metadata. The cookie carries
//! `series:token`. On each request that presents a remember cookie the caller
//! looks up the stored record by `series` and calls [`evaluate_remember`]:
//!
//! - token matches the stored hash (constant-time) and the record is not
//!   expired → [`RememberDecision::Rotate`]: mint a fresh token
//!   ([`generate_token`]), overwrite the stored hash, extend the expiry, set a
//!   new cookie, and authenticate the user.
//! - token does **not** match a **known** series → [`RememberDecision::Theft`]:
//!   an attacker is replaying a rotated-out cookie. The caller deletes the whole
//!   chain for this series and treats the request as unauthenticated.
//! - unknown or expired series → [`RememberDecision::Reject`]: unauthenticated;
//!   the caller clears/ignores the cookie.
//!
//! This gives theft detection: replaying an old (already-rotated-out) cookie
//! trips the mismatch and nukes the chain, so a stolen-then-rotated cookie can
//! be used at most once before the legitimate user's next visit invalidates it.
//!
//! All primitives here are **pure and deterministic** (given their inputs) so
//! the security-critical [`evaluate_remember`] decision can be unit-tested
//! against fixed timestamps without any I/O.

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use sha2::{Digest as _, Sha256};

/// Number of random bytes behind each opaque id (series/token). 24 bytes = 192
/// bits of entropy, comfortably above the 128-bit floor and URL-safe once
/// base64-encoded (32 characters, no padding).
const REMEMBER_ID_BYTES: usize = 24;

/// Default remember-cookie lifetime: 30 days.
const fn default_remember_duration_secs() -> u64 {
    2_592_000
}

/// Default remember-cookie name.
fn default_remember_cookie_name() -> String {
    "autumn.remember".to_owned()
}

const fn default_remember_enabled() -> bool {
    true
}

/// Deserializable `[auth.remember]` configuration block.
///
/// ```toml
/// [auth.remember]
/// enabled = true             # issue remember cookies on login (default true)
/// duration_secs = 2592000    # cookie lifetime in seconds (default 30 days)
/// cookie_name = "autumn.remember"
/// ```
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(default)]
pub struct RememberConfig {
    /// Whether persistent "remember-me" logins are issued (default `true`).
    pub enabled: bool,
    /// Lifetime of a remember cookie in seconds (default `2_592_000`, 30 days).
    ///
    /// Also used as the sliding expiry applied on each successful rotation.
    pub duration_secs: u64,
    /// Name of the remember cookie (default `"autumn.remember"`).
    pub cookie_name: String,
}

impl Default for RememberConfig {
    fn default() -> Self {
        Self {
            enabled: default_remember_enabled(),
            duration_secs: default_remember_duration_secs(),
            cookie_name: default_remember_cookie_name(),
        }
    }
}

/// A freshly minted credential to hand to the client and persist server-side.
///
/// The raw `token` is returned exactly once (to place in the cookie); only its
/// hash ([`hash_remember_token`]) is ever stored at rest.
#[derive(Debug, Clone)]
pub struct RememberCredential {
    /// Stable, opaque series id (the DB lookup key for this device chain).
    pub series: String,
    /// Rotating, opaque token (hashed before storage, sent raw in the cookie).
    pub token: String,
}

/// The server-side stored record for one series — i.e. what the application's
/// database row holds.
#[derive(Debug, Clone)]
pub struct RememberRecord {
    /// Stable series id.
    pub series: String,
    /// Hex SHA-256 digest of the current token ([`hash_remember_token`]).
    pub token_hash: String,
    /// Hex SHA-256 digest of the token that was **just rotated out**, if any.
    ///
    /// During the rotation grace window this previous token is still accepted
    /// (as [`RememberDecision::Accept`], without triggering another rotation or
    /// theft detection) so that benign concurrent requests — a single page load
    /// firing many parallel sub-requests that all carry the same
    /// not-yet-rotated token — do not false-fire theft detection when a sibling
    /// request has already rotated the chain.
    pub previous_token_hash: Option<String>,
    /// When the **current** token was minted (i.e. the moment of the last
    /// rotation). Combined with the grace duration this bounds how long the
    /// [`previous_token_hash`](Self::previous_token_hash) stays acceptable.
    pub rotated_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Absolute expiry of this series.
    pub expires_at: chrono::DateTime<chrono::Utc>,
}

/// Outcome of evaluating a presented remember cookie against the stored record.
#[derive(Debug, PartialEq, Eq)]
pub enum RememberDecision {
    /// Cookie valid: caller mints a new token ([`generate_token`]), overwrites
    /// the stored hash, extends the expiry, sets a fresh cookie, and
    /// authenticates the user.
    Rotate,
    /// The presented token is the **previous** token, still within the rotation
    /// grace window — a benign concurrent request that arrived just after a
    /// sibling request rotated the chain. Caller authenticates the user but does
    /// **not** rotate again and does **not** treat this as theft.
    Accept,
    /// A replayed / rotated-out token for a **known** series → theft. Caller
    /// deletes the whole chain for this series and treats the request as
    /// unauthenticated.
    Theft,
    /// Unknown series or expired record → unauthenticated. Caller clears or
    /// ignores the cookie.
    Reject,
}

/// Generate one cryptographically-random opaque id (URL-safe, ≥128 bits).
fn generate_opaque_id() -> String {
    let mut bytes = [0u8; REMEMBER_ID_BYTES];
    getrandom::getrandom(&mut bytes).expect("getrandom must not fail on supported platforms");
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Mint a brand-new remember credential: a fresh `series` and `token`, each a
/// cryptographically-random URL-safe id of at least 128 bits.
#[must_use]
pub fn generate_remember_credential() -> RememberCredential {
    RememberCredential {
        series: generate_opaque_id(),
        token: generate_opaque_id(),
    }
}

/// Mint a fresh token for rotation, with the same entropy as the token produced
/// by [`generate_remember_credential`]. The `series` is left unchanged by the
/// caller across a rotation.
#[must_use]
pub fn generate_token() -> String {
    generate_opaque_id()
}

/// Compute the at-rest hash of a remember token: a lowercase hex SHA-256 digest.
///
/// Deterministic — the same token always hashes to the same string — so it can
/// be recomputed on the presented token and compared against the stored hash.
#[must_use]
pub fn hash_remember_token(token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());
    hex::encode(digest)
}

/// Constant-time equality over two byte slices.
///
/// Returns `false` immediately for length mismatches (length is not secret
/// here — both operands are fixed-width hex digests), then compares every byte
/// without early-exit so timing does not leak how many leading bytes matched.
#[must_use]
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    // `subtle::ConstantTimeEq` folds the whole slice into a single Choice with
    // no data-dependent branches.
    use subtle::ConstantTimeEq as _;
    if a.len() != b.len() {
        return false;
    }
    a.ct_eq(b).into()
}

/// Verify a presented token against a stored hash using a constant-time compare.
///
/// Hashes the presented token and compares the two hex digests byte-for-byte
/// without early-return, so a near-miss cannot be distinguished from a
/// far-miss by timing.
#[must_use]
pub fn verify_remember_token(token: &str, stored_hash: &str) -> bool {
    let computed = hash_remember_token(token);
    constant_time_eq(computed.as_bytes(), stored_hash.as_bytes())
}

/// Format the cookie payload for a `(series, token)` pair as `series:token`.
#[must_use]
pub fn format_remember_cookie_value(series: &str, token: &str) -> String {
    format!("{series}:{token}")
}

/// Parse a `series:token` cookie payload, splitting **once** on the first `:`.
///
/// Returns `None` if the value is empty, has no separator, or has an empty
/// series or token half.
#[must_use]
pub fn parse_remember_cookie_value(value: &str) -> Option<(String, String)> {
    let (series, token) = value.split_once(':')?;
    if series.is_empty() || token.is_empty() {
        return None;
    }
    Some((series.to_owned(), token.to_owned()))
}

/// Build a `Set-Cookie` header value that installs the remember cookie.
///
/// Mirrors the session cookie attributes: `Max-Age=<duration_secs>`,
/// `HttpOnly`, `SameSite=Lax`, `Path=/`, and `Secure` only when `secure` is
/// `true`.
#[must_use]
pub fn build_remember_cookie(
    config: &RememberConfig,
    series: &str,
    token: &str,
    secure: bool,
) -> String {
    use std::fmt::Write as _;
    let value = format_remember_cookie_value(series, token);
    let mut cookie = format!("{}={value}; Path=/", config.cookie_name);
    let _ = write!(cookie, "; Max-Age={}", config.duration_secs);
    cookie.push_str("; HttpOnly");
    if secure {
        cookie.push_str("; Secure");
    }
    cookie.push_str("; SameSite=Lax");
    cookie
}

/// Build a `Set-Cookie` header value that expires the remember cookie
/// immediately (`Max-Age=0`).
#[must_use]
pub fn build_remember_clear_cookie(config: &RememberConfig) -> String {
    format!(
        "{}=; Path=/; Max-Age=0; HttpOnly; SameSite=Lax",
        config.cookie_name
    )
}

/// Default rotation grace window, in seconds.
///
/// After a token rotates, the just-rotated-out (previous) token stays
/// acceptable for this long so that benign concurrent requests carrying the
/// old token do not false-fire theft detection. This is the single source of
/// truth for callers (e.g. the generated middleware).
pub const DEFAULT_ROTATION_GRACE_SECS: i64 = 60;

/// The default rotation grace window as a [`chrono::Duration`].
///
/// Convenience wrapper over [`DEFAULT_ROTATION_GRACE_SECS`] so callers share one
/// source of truth for the grace window.
#[must_use]
pub const fn default_rotation_grace() -> chrono::Duration {
    chrono::Duration::seconds(DEFAULT_ROTATION_GRACE_SECS)
}

/// Evaluate a presented remember token against the stored record for its series.
///
/// This is the pure, deterministic core of the scheme (see the module docs).
/// The caller has already looked up `record` by the `series` parsed from the
/// cookie, so a `None` record means the series is unknown.
///
/// - `record` is `None` → [`RememberDecision::Reject`].
/// - `record.expires_at <= now` → [`RememberDecision::Reject`].
/// - token matches the stored hash (constant-time) → [`RememberDecision::Rotate`].
/// - else if the token matches the **previous** hash and `now - rotated_at <=
///   grace` → [`RememberDecision::Accept`] (benign concurrent request within the
///   rotation grace window).
/// - otherwise (known, live series but wrong token) → [`RememberDecision::Theft`].
///
/// The secret-token comparisons go through [`verify_remember_token`]
/// (constant-time); the non-secret grace-window bound is checked separately.
#[must_use]
pub fn evaluate_remember(
    presented_token: &str,
    record: Option<&RememberRecord>,
    now: chrono::DateTime<chrono::Utc>,
    grace: chrono::Duration,
) -> RememberDecision {
    let Some(record) = record else {
        return RememberDecision::Reject;
    };
    if record.expires_at <= now {
        return RememberDecision::Reject;
    }
    if verify_remember_token(presented_token, &record.token_hash) {
        return RememberDecision::Rotate;
    }
    if let (Some(prev), Some(rotated_at)) = (&record.previous_token_hash, record.rotated_at)
        && verify_remember_token(presented_token, prev)
        && now - rotated_at <= grace
    {
        return RememberDecision::Accept;
    }
    RememberDecision::Theft
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};

    fn ts(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(secs, 0).expect("valid timestamp")
    }

    /// A fixed 60-second grace window for the evaluation tests.
    fn grace() -> chrono::Duration {
        chrono::Duration::seconds(60)
    }

    #[test]
    fn generate_remember_credential_is_random_and_distinct() {
        let cred = generate_remember_credential();
        assert!(!cred.series.is_empty());
        assert!(!cred.token.is_empty());
        assert_ne!(cred.series, cred.token, "series and token must differ");
        assert!(
            cred.series.len() >= 20,
            "series too short: {}",
            cred.series.len()
        );
        assert!(
            cred.token.len() >= 20,
            "token too short: {}",
            cred.token.len()
        );

        let other = generate_remember_credential();
        assert_ne!(cred.series, other.series, "two calls must differ (series)");
        assert_ne!(cred.token, other.token, "two calls must differ (token)");
    }

    #[test]
    fn generate_token_is_random_and_long() {
        let a = generate_token();
        let b = generate_token();
        assert!(a.len() >= 20);
        assert_ne!(a, b, "two token calls must differ");
    }

    #[test]
    fn hash_remember_token_is_deterministic() {
        let token = "example-token-value";
        assert_eq!(hash_remember_token(token), hash_remember_token(token));
        // 32-byte SHA-256 → 64 hex chars.
        assert_eq!(hash_remember_token(token).len(), 64);
        assert_ne!(hash_remember_token("a"), hash_remember_token("b"));
    }

    #[test]
    fn verify_remember_token_matches_correct_and_rejects_wrong() {
        let token = "the-real-token";
        let stored = hash_remember_token(token);
        assert!(verify_remember_token(token, &stored));
        assert!(!verify_remember_token("a-different-token", &stored));
    }

    #[test]
    fn constant_time_eq_correctness() {
        assert!(constant_time_eq(b"abcdef", b"abcdef"));
        assert!(!constant_time_eq(b"abcdef", b"abcdeg"));
        assert!(!constant_time_eq(b"abc", b"abcdef"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn evaluate_matching_token_not_expired_rotates() {
        let token = "live-token";
        let record = RememberRecord {
            series: "series-1".to_owned(),
            token_hash: hash_remember_token(token),
            previous_token_hash: None,
            rotated_at: None,
            expires_at: ts(2_000),
        };
        assert_eq!(
            evaluate_remember(token, Some(&record), ts(1_000), grace()),
            RememberDecision::Rotate
        );
    }

    #[test]
    fn evaluate_current_token_rotates_even_with_previous_present() {
        // With a previous token recorded, the CURRENT token must still Rotate
        // (not merely Accept).
        let current = "current-token";
        let record = RememberRecord {
            series: "series-1".to_owned(),
            token_hash: hash_remember_token(current),
            previous_token_hash: Some(hash_remember_token("old-token")),
            rotated_at: Some(ts(990)),
            expires_at: ts(2_000),
        };
        assert_eq!(
            evaluate_remember(current, Some(&record), ts(1_000), grace()),
            RememberDecision::Rotate
        );
    }

    #[test]
    fn evaluate_previous_token_within_grace_is_accept() {
        // A benign concurrent request: it presents the just-rotated-out token,
        // and we are still inside the grace window since the rotation.
        let previous = "just-rotated-out";
        let record = RememberRecord {
            series: "series-1".to_owned(),
            token_hash: hash_remember_token("brand-new-token"),
            previous_token_hash: Some(hash_remember_token(previous)),
            rotated_at: Some(ts(1_000)),
            expires_at: ts(2_000),
        };
        // 30s after rotation, grace is 60s → Accept.
        assert_eq!(
            evaluate_remember(previous, Some(&record), ts(1_030), grace()),
            RememberDecision::Accept
        );
        // Exactly at the grace boundary (60s) is still inclusive → Accept.
        assert_eq!(
            evaluate_remember(previous, Some(&record), ts(1_060), grace()),
            RememberDecision::Accept
        );
    }

    #[test]
    fn evaluate_previous_token_after_grace_is_theft() {
        // Same previous token, but the grace window has elapsed → genuine
        // replay of a rotated-out token → Theft.
        let previous = "rotated-out-long-ago";
        let record = RememberRecord {
            series: "series-1".to_owned(),
            token_hash: hash_remember_token("brand-new-token"),
            previous_token_hash: Some(hash_remember_token(previous)),
            rotated_at: Some(ts(1_000)),
            expires_at: ts(2_000),
        };
        // 61s after rotation, grace is 60s → past the window → Theft.
        assert_eq!(
            evaluate_remember(previous, Some(&record), ts(1_061), grace()),
            RememberDecision::Theft
        );
    }

    #[test]
    fn evaluate_wrong_token_known_series_is_theft() {
        let record = RememberRecord {
            series: "series-1".to_owned(),
            token_hash: hash_remember_token("the-current-token"),
            previous_token_hash: None,
            rotated_at: None,
            expires_at: ts(2_000),
        };
        assert_eq!(
            evaluate_remember("a-rotated-out-token", Some(&record), ts(1_000), grace()),
            RememberDecision::Theft
        );
    }

    #[test]
    fn evaluate_unknown_token_with_previous_present_is_theft() {
        // A token that matches neither current nor previous → Theft, even
        // within the grace window.
        let record = RememberRecord {
            series: "series-1".to_owned(),
            token_hash: hash_remember_token("current-token"),
            previous_token_hash: Some(hash_remember_token("previous-token")),
            rotated_at: Some(ts(1_000)),
            expires_at: ts(2_000),
        };
        assert_eq!(
            evaluate_remember("some-unrelated-token", Some(&record), ts(1_010), grace()),
            RememberDecision::Theft
        );
    }

    #[test]
    fn evaluate_wrong_token_no_previous_is_theft_not_accept() {
        // A record with no previous token presenting a wrong token must be
        // Theft — the Accept path requires a recorded previous hash.
        let record = RememberRecord {
            series: "series-1".to_owned(),
            token_hash: hash_remember_token("the-current-token"),
            previous_token_hash: None,
            rotated_at: Some(ts(1_000)),
            expires_at: ts(2_000),
        };
        assert_eq!(
            evaluate_remember("wrong-token", Some(&record), ts(1_010), grace()),
            RememberDecision::Theft
        );
    }

    #[test]
    fn evaluate_unknown_series_is_reject() {
        assert_eq!(
            evaluate_remember("whatever", None, ts(1_000), grace()),
            RememberDecision::Reject
        );
    }

    #[test]
    fn evaluate_expired_record_is_reject_even_with_correct_token() {
        let token = "correct-but-expired";
        let record = RememberRecord {
            series: "series-1".to_owned(),
            token_hash: hash_remember_token(token),
            previous_token_hash: None,
            rotated_at: None,
            expires_at: ts(1_000),
        };
        // now == expires_at → expired (expiry is inclusive).
        assert_eq!(
            evaluate_remember(token, Some(&record), ts(1_000), grace()),
            RememberDecision::Reject
        );
        // now strictly after expiry.
        assert_eq!(
            evaluate_remember(token, Some(&record), ts(1_001), grace()),
            RememberDecision::Reject
        );
    }

    #[test]
    fn default_rotation_grace_matches_constant() {
        assert_eq!(DEFAULT_ROTATION_GRACE_SECS, 60);
        assert_eq!(
            default_rotation_grace(),
            chrono::Duration::seconds(DEFAULT_ROTATION_GRACE_SECS)
        );
    }

    #[test]
    fn cookie_value_round_trip() {
        let series = "series-abc";
        let token = "token-xyz";
        let formatted = format_remember_cookie_value(series, token);
        assert_eq!(formatted, "series-abc:token-xyz");
        assert_eq!(
            parse_remember_cookie_value(&formatted),
            Some((series.to_owned(), token.to_owned()))
        );
    }

    #[test]
    fn parse_cookie_value_rejects_malformed() {
        assert_eq!(parse_remember_cookie_value(""), None);
        assert_eq!(parse_remember_cookie_value("noseparator"), None);
        assert_eq!(parse_remember_cookie_value(":"), None);
        assert_eq!(parse_remember_cookie_value("a:"), None);
        assert_eq!(parse_remember_cookie_value(":b"), None);
    }

    #[test]
    fn parse_cookie_value_splits_once() {
        // A token that itself contains ':' must be preserved intact.
        assert_eq!(
            parse_remember_cookie_value("series:tok:with:colons"),
            Some(("series".to_owned(), "tok:with:colons".to_owned()))
        );
    }

    #[test]
    fn build_remember_cookie_attributes() {
        let config = RememberConfig::default();
        let secure = build_remember_cookie(&config, "s", "t", true);
        assert!(secure.contains("autumn.remember=s:t"));
        assert!(secure.contains("Max-Age=2592000"));
        assert!(secure.contains("HttpOnly"));
        assert!(secure.contains("SameSite=Lax"));
        assert!(secure.contains("Path=/"));
        assert!(secure.contains("Secure"));

        let insecure = build_remember_cookie(&config, "s", "t", false);
        assert!(!insecure.contains("Secure"));
        assert!(insecure.contains("HttpOnly"));
    }

    #[test]
    fn build_remember_clear_cookie_expires() {
        let config = RememberConfig::default();
        let clear = build_remember_clear_cookie(&config);
        assert!(clear.contains("autumn.remember="));
        assert!(clear.contains("Max-Age=0"));
    }

    #[test]
    fn config_defaults() {
        let config = RememberConfig::default();
        assert!(config.enabled);
        assert_eq!(config.duration_secs, 2_592_000);
        assert_eq!(config.cookie_name, "autumn.remember");
    }

    #[test]
    fn config_deserializes_partial_toml() {
        let config: RememberConfig =
            toml::from_str("duration_secs = 3600").expect("valid partial config");
        // Overridden field takes effect...
        assert_eq!(config.duration_secs, 3_600);
        // ...while the others fall back to their defaults.
        assert!(config.enabled);
        assert_eq!(config.cookie_name, "autumn.remember");
    }
}
