//! Step-up authentication ("sudo mode") for sensitive operations.
//!
//! Provides the `#[step_up]` route attribute macro that requires fresh
//! authentication before a handler runs. Routes marked with `#[step_up]`
//! check the session for a recent [`STEP_UP_SESSION_KEY`] claim. If the
//! claim is missing or stale the user is redirected to
//! `/reauth?return_to=…` (browser clients) or receives a `401
//! problem-details` response (JSON/API clients).
//!
//! # Threat model
//!
//! `#[step_up]` protects against **session hijacking blast radius**: an
//! attacker who obtains a valid session cookie (via XSS, unlocked laptop,
//! shared browser, stolen cookie) cannot exercise destructive capabilities
//! without also knowing the user's current password. Step-up is
//! complementary to session rotation ([`Session::rotate_id`]) — rotation
//! prevents session *fixation*, step-up limits the *blast radius* of a
//! hijacked session.
//!
//! # Usage
//!
//! ```rust,ignore
//! use autumn_web::prelude::*;
//!
//! // Default max-age: 5 minutes
//! #[delete("/account")]
//! #[step_up]
//! async fn destroy_account(session: Session) -> AutumnResult<Redirect> {
//!     // ... delete account ...
//!     Ok(Redirect::to("/bye"))
//! }
//!
//! // Custom max-age
//! #[post("/auth/mfa/remove")]
//! #[step_up(max_age = "2m")]
//! async fn remove_mfa() -> AutumnResult<&'static str> {
//!     Ok("MFA removed")
//! }
//! ```

use chrono::{DateTime, Utc};

use crate::session::Session;

// ── Constants ───────────────────────────────────────────────────────────────

/// Session key that stores the Unix timestamp of the last strong (step-up)
/// authentication.
pub const STEP_UP_SESSION_KEY: &str = "last_strong_auth_at";

/// Default freshness window for step-up checks: 5 minutes (300 seconds).
pub const DEFAULT_MAX_AGE_SECS: u64 = 300;

/// Problem type URI for step-up required responses sent to API/JSON clients.
///
/// Clients receive this URI in the `"type"` field of the RFC 7807 response
/// body together with a `WWW-Authenticate: StepUp max-age=N` hint header.
pub const STEP_UP_PROBLEM_TYPE: &str = "https://autumn.rs/probs/step-up-required";

// ── Configuration ────────────────────────────────────────────────────────────

/// Global step-up defaults stored in [`AppState`](crate::AppState) extensions.
///
/// Installed automatically from `[auth.step_up]` in `autumn.toml` during
/// application startup. Individual routes can override `default_max_age_secs`
/// with the `#[step_up(max_age = "N")]` argument.
#[derive(Debug, Clone)]
pub struct StepUpGlobalConfig {
    /// Maximum age (in seconds) of the `last_strong_auth_at` session claim
    /// before the user must re-authenticate (default: `300`, i.e. 5 minutes).
    pub default_max_age_secs: u64,
}

impl Default for StepUpGlobalConfig {
    fn default() -> Self {
        Self {
            default_max_age_secs: DEFAULT_MAX_AGE_SECS,
        }
    }
}

// ── Session claim helpers ────────────────────────────────────────────────────

/// Set the `last_strong_auth_at` session claim to the current UTC timestamp.
///
/// Call this after a successful initial login **and** after a successful
/// re-authentication (reauth form submission).
pub async fn set_last_strong_auth_at(session: &Session) {
    let now = Utc::now().timestamp().to_string();
    session.insert(STEP_UP_SESSION_KEY, now).await;
}

// ── Core freshness check ─────────────────────────────────────────────────────

/// Check whether the session has a sufficiently fresh `last_strong_auth_at`
/// claim.
///
/// Returns `Ok(())` when the claim exists and its age is ≤ `max_age_secs`.
/// Returns `Err(AutumnError::unauthorized)` when the claim is missing, stale,
/// or unparseable.
///
/// # Errors
///
/// Returns [`crate::AutumnError`] with status `401 Unauthorized` when the
/// session does not satisfy the step-up requirement.
pub async fn check_step_up(session: &Session, max_age_secs: u64) -> crate::AutumnResult<()> {
    let stored = session.get(STEP_UP_SESSION_KEY).await;

    let Some(ts_str) = stored else {
        return Err(crate::AutumnError::unauthorized_msg(
            "step-up authentication required",
        ));
    };

    let ts: i64 = ts_str
        .parse()
        .map_err(|_| crate::AutumnError::unauthorized_msg("step-up authentication required"))?;

    let last_auth = DateTime::from_timestamp(ts, 0)
        .ok_or_else(|| crate::AutumnError::unauthorized_msg("step-up authentication required"))?;

    let age_secs = (Utc::now() - last_auth).num_seconds();
    // Negative means the timestamp is in the future (clock skew) — treat as expired.
    if u64::try_from(age_secs).map_or(true, |age| age > max_age_secs) {
        return Err(crate::AutumnError::unauthorized_msg(
            "step-up authentication required",
        ));
    }

    Ok(())
}

// ── return_to validation ────────────────────────────────────────────────────

/// Validate that a `return_to` URL is safe (same-origin, absolute path only).
///
/// Accepts:
/// - Empty string (treated as no redirect).
/// - Absolute path starting with `/` (e.g. `/dashboard`, `/account/edit`).
///
/// Rejects:
/// - Protocol-relative URLs (`//evil.com/…`).
/// - Absolute URLs with a scheme (`http://`, `https://`, `javascript:`, etc.).
///
/// # Errors
///
/// Returns `Err(&'static str)` with a human-readable reason when the URL is
/// not safe.
pub fn validate_return_to(url: &str) -> Result<(), &'static str> {
    if url.is_empty() {
        return Ok(());
    }
    // Must begin with exactly one '/'
    if !url.starts_with('/') {
        return Err("return_to must be an absolute path starting with /");
    }
    // Reject protocol-relative URLs (//host/path or /\host/path).
    // Some browsers normalise /\ to // so we must reject both.
    if url.starts_with("//") || url.starts_with("/\\") {
        return Err("return_to must not be a protocol-relative URL");
    }
    Ok(())
}

// ── Duration string parser ───────────────────────────────────────────────────

/// Parse a human-readable duration string into seconds.
///
/// Supported suffixes: `m` (minutes), `h` (hours), `s` (seconds).
/// A bare number is treated as seconds.
///
/// # Errors
///
/// Returns `Err(String)` when the string is not a valid duration.
pub fn parse_max_age_str(s: &str) -> Result<u64, String> {
    if let Some(mins) = s.strip_suffix('m') {
        return mins
            .parse::<u64>()
            .map(|m| m * 60)
            .map_err(|_| format!("invalid max_age: '{s}' (expected e.g. \"5m\")"));
    }
    if let Some(hours) = s.strip_suffix('h') {
        return hours
            .parse::<u64>()
            .map(|h| h * 3600)
            .map_err(|_| format!("invalid max_age: '{s}' (expected e.g. \"1h\")"));
    }
    if let Some(secs) = s.strip_suffix('s') {
        return secs
            .parse::<u64>()
            .map_err(|_| format!("invalid max_age: '{s}' (expected e.g. \"30s\")"));
    }
    s.parse::<u64>()
        .map_err(|_| format!("invalid max_age: '{s}' (expected seconds or e.g. \"5m\")"))
}

// ── URL encoding ─────────────────────────────────────────────────────────────

/// Extract the path (and optional query) from a `Referer` header value.
///
/// Returns `None` when the Referer is malformed or fails the same-origin path
/// validation performed by [`validate_return_to`].
///
/// # Example
///
/// ```
/// use autumn_web::step_up::referer_path;
/// assert_eq!(
///     referer_path("https://example.com/account/settings?x=1"),
///     Some("/account/settings?x=1".to_owned()),
/// );
/// assert_eq!(referer_path("//evil.com/path"), None);
/// assert_eq!(referer_path("not-a-url"), None);
/// ```
#[must_use]
pub fn referer_path(referer: &str) -> Option<String> {
    let path = referer
        .split_once("://")
        .and_then(|(_, rest)| rest.find('/').map(|i| &rest[i..]))?;
    validate_return_to(path)
        .ok()
        .filter(|()| !path.is_empty())?;
    Some(path.to_owned())
}

/// Percent-encode a path for safe embedding in a `return_to` query parameter.
///
/// Characters that are valid unencoded in query parameter values are left
/// intact; everything else is percent-encoded.
#[must_use]
pub fn encode_return_to(path: &str) -> String {
    let mut encoded = String::with_capacity(path.len() + 16);
    for byte in path.bytes() {
        match byte {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'~'
            | b'/'
            | b':'
            | b'@'
            | b'!'
            | b'$'
            | b'\''
            | b'('
            | b')'
            | b'*'
            | b','
            | b';'
            | b'=' => {
                encoded.push(byte as char);
            }
            _ => {
                const HEX: &[u8; 16] = b"0123456789ABCDEF";
                encoded.push('%');
                encoded.push(HEX[(byte >> 4) as usize] as char);
                encoded.push(HEX[(byte & 0xF) as usize] as char);
            }
        }
    }
    encoded
}

// ── Runtime functions used by the #[step_up] macro ───────────────────────────

/// Resolve the effective max-age for a step-up check.
///
/// Used by the `#[step_up]` macro so it can advertise the correct value in
/// `WWW-Authenticate` headers — which would otherwise show `DEFAULT_MAX_AGE_SECS`
/// even when a non-default global config is in effect.
///
/// **Not intended for direct use** — generated by the `#[step_up]` macro.
#[doc(hidden)]
#[must_use]
pub fn __resolve_step_up_max_age(state: &crate::AppState, route_max_age_secs: Option<u64>) -> u64 {
    route_max_age_secs.unwrap_or_else(|| {
        state
            .extension::<StepUpGlobalConfig>()
            .map_or(DEFAULT_MAX_AGE_SECS, |c| c.default_max_age_secs)
    })
}

/// Runtime freshness check used by the `#[step_up]` proc macro.
///
/// When `route_max_age_secs` is `Some`, that value is used directly.
/// When `None`, the global default from [`StepUpGlobalConfig`] stored in
/// `state` extensions is used, falling back to [`DEFAULT_MAX_AGE_SECS`].
///
/// Also emits `auth.step_up.success` / `auth.step_up.failure` audit events
/// through any [`crate::audit::AuditLogger`] registered in `state`.
///
/// **Not intended for direct use** — use `#[step_up]` instead.
#[doc(hidden)]
pub async fn __check_step_up_with_config(
    session: &Session,
    state: &crate::AppState,
    route_max_age_secs: Option<u64>,
) -> crate::AutumnResult<()> {
    let max_age = route_max_age_secs.unwrap_or_else(|| {
        state
            .extension::<StepUpGlobalConfig>()
            .map_or(DEFAULT_MAX_AGE_SECS, |c| c.default_max_age_secs)
    });

    // Attribute through the same rule the three `Current::set_actor` seams use:
    // while the session is impersonating (#1394), the operator — not the user
    // the request resolves as — is who performed the sensitive action, and a
    // step-up event naming the target would be exactly the misattribution
    // impersonation exists to prevent.
    let actor_id = match session.get(state.auth_session_key()).await {
        Some(effective) => crate::auth::impersonation::audit_actor_id(session, &effective).await,
        None => "anonymous".to_owned(),
    };

    match check_step_up(session, max_age).await {
        Ok(()) => {
            let event = crate::audit::AuditEvent::new(
                &actor_id,
                "auth.step_up.success",
                "session",
                None,
                crate::audit::AuditStatus::Success,
            );
            let _ = crate::audit::write_from_state(state, event).await;
            Ok(())
        }
        Err(err) => {
            let event = crate::audit::AuditEvent::new(
                &actor_id,
                "auth.step_up.failure",
                "session",
                None,
                crate::audit::AuditStatus::Failure,
            );
            let _ = crate::audit::write_from_state(state, event).await;
            Err(err)
        }
    }
}

/// Build the 401 JSON response body and response for API clients that require
/// step-up authentication.
///
/// The response includes:
/// - `Content-Type: application/problem+json`
/// - `WWW-Authenticate: StepUp max-age=N`
/// - RFC 7807 problem-details body with
///   `"type": "https://autumn.rs/probs/step-up-required"`
///
/// **Not intended for direct use** — generated by the `#[step_up]` macro.
#[doc(hidden)]
#[must_use]
pub fn __step_up_json_response(max_age_secs: u64) -> axum::response::Response {
    use axum::http::{HeaderValue, StatusCode, header};
    use axum::response::IntoResponse;

    let body = format!(
        r#"{{"type":"{STEP_UP_PROBLEM_TYPE}","title":"Step-Up Authentication Required","status":401,"detail":"This operation requires recent authentication. Please re-authenticate and retry.","code":"step_up_required"}}"#
    );

    let www_auth_value = format!("StepUp max-age={max_age_secs}");
    let www_auth_header = HeaderValue::from_str(&www_auth_value)
        .unwrap_or_else(|_| HeaderValue::from_static("StepUp"));

    (
        StatusCode::UNAUTHORIZED,
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/problem+json"),
            ),
            (header::WWW_AUTHENTICATE, www_auth_header),
        ],
        body,
    )
        .into_response()
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::session::Session;

    // ── check_step_up ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn check_step_up_fails_when_no_session_claim() {
        let session = Session::new_for_test("test-id".into(), HashMap::new());
        let result = check_step_up(&session, 300).await;
        assert!(result.is_err(), "missing claim should fail step-up");
        assert_eq!(
            result.unwrap_err().status(),
            http::StatusCode::UNAUTHORIZED,
            "should return 401"
        );
    }

    #[tokio::test]
    async fn check_step_up_fails_when_claim_is_stale() {
        let mut data = HashMap::new();
        let stale_ts = (Utc::now() - chrono::Duration::seconds(600))
            .timestamp()
            .to_string();
        data.insert(STEP_UP_SESSION_KEY.to_string(), stale_ts);
        let session = Session::new_for_test("test-id".into(), data);
        let result = check_step_up(&session, 300).await;
        assert!(
            result.is_err(),
            "stale claim (10 min old) should fail 5-min check"
        );
    }

    #[tokio::test]
    async fn check_step_up_succeeds_when_fresh() {
        let mut data = HashMap::new();
        let fresh_ts = Utc::now().timestamp().to_string();
        data.insert(STEP_UP_SESSION_KEY.to_string(), fresh_ts);
        let session = Session::new_for_test("test-id".into(), data);
        let result = check_step_up(&session, 300).await;
        assert!(result.is_ok(), "fresh claim should pass: {result:?}");
    }

    #[tokio::test]
    async fn check_step_up_succeeds_at_exactly_max_age() {
        let mut data = HashMap::new();
        // Exactly 300 seconds ago — should still pass (age == max_age).
        let ts = (Utc::now() - chrono::Duration::seconds(300))
            .timestamp()
            .to_string();
        data.insert(STEP_UP_SESSION_KEY.to_string(), ts);
        let session = Session::new_for_test("test-id".into(), data);
        let result = check_step_up(&session, 300).await;
        assert!(result.is_ok(), "claim at exactly max_age should pass");
    }

    #[tokio::test]
    async fn check_step_up_fails_one_second_past_max_age() {
        let mut data = HashMap::new();
        let ts = (Utc::now() - chrono::Duration::seconds(301))
            .timestamp()
            .to_string();
        data.insert(STEP_UP_SESSION_KEY.to_string(), ts);
        let session = Session::new_for_test("test-id".into(), data);
        let result = check_step_up(&session, 300).await;
        assert!(result.is_err(), "claim one second past max_age should fail");
    }

    #[tokio::test]
    async fn check_step_up_fails_with_invalid_timestamp() {
        let mut data = HashMap::new();
        data.insert(
            STEP_UP_SESSION_KEY.to_string(),
            "not-a-timestamp".to_string(),
        );
        let session = Session::new_for_test("test-id".into(), data);
        let result = check_step_up(&session, 300).await;
        assert!(
            result.is_err(),
            "invalid timestamp should fail step-up check"
        );
    }

    // ── set_last_strong_auth_at ───────────────────────────────────────────────

    #[tokio::test]
    async fn set_last_strong_auth_at_stores_current_timestamp() {
        let session = Session::new_for_test("test-id".into(), HashMap::new());
        set_last_strong_auth_at(&session).await;
        let stored = session.get(STEP_UP_SESSION_KEY).await;
        assert!(stored.is_some(), "should store a timestamp");
        let ts: i64 = stored
            .unwrap()
            .parse()
            .expect("timestamp must be a valid i64");
        let now = Utc::now().timestamp();
        assert!(
            (now - ts).abs() < 5,
            "stored timestamp should be within 5 seconds of now"
        );
    }

    #[tokio::test]
    async fn set_then_check_passes_immediately() {
        let session = Session::new_for_test("test-id".into(), HashMap::new());
        set_last_strong_auth_at(&session).await;
        let result = check_step_up(&session, 300).await;
        assert!(
            result.is_ok(),
            "freshly set claim should pass step-up check"
        );
    }

    // ── validate_return_to ────────────────────────────────────────────────────

    #[test]
    fn validate_return_to_allows_same_origin_paths() {
        assert!(validate_return_to("/dashboard").is_ok());
        assert!(validate_return_to("/account/settings").is_ok());
        assert!(validate_return_to("/admin/users/1").is_ok());
        assert!(validate_return_to("/").is_ok());
        assert!(
            validate_return_to("").is_ok(),
            "empty string should be allowed"
        );
    }

    #[test]
    fn validate_return_to_rejects_external_https() {
        assert!(
            validate_return_to("https://evil.com/steal").is_err(),
            "https URL should be rejected"
        );
    }

    #[test]
    fn validate_return_to_rejects_external_http() {
        assert!(
            validate_return_to("http://attacker.net").is_err(),
            "http URL should be rejected"
        );
    }

    #[test]
    fn validate_return_to_rejects_protocol_relative() {
        assert!(
            validate_return_to("//evil.com/path").is_err(),
            "protocol-relative URL should be rejected"
        );
    }

    #[test]
    fn validate_return_to_rejects_javascript_scheme() {
        assert!(
            validate_return_to("javascript:alert(1)").is_err(),
            "javascript: URL should be rejected"
        );
    }

    #[test]
    fn validate_return_to_rejects_data_scheme() {
        assert!(
            validate_return_to("data:text/html,<h1>phish</h1>").is_err(),
            "data: URL should be rejected"
        );
    }

    #[test]
    fn validate_return_to_rejects_ftp_scheme() {
        assert!(
            validate_return_to("ftp://files.example.com").is_err(),
            "ftp: URL should be rejected"
        );
    }

    // ── parse_max_age_str ─────────────────────────────────────────────────────

    #[test]
    fn parse_max_age_handles_minutes() {
        assert_eq!(parse_max_age_str("5m"), Ok(300));
        assert_eq!(parse_max_age_str("10m"), Ok(600));
        assert_eq!(parse_max_age_str("1m"), Ok(60));
    }

    #[test]
    fn parse_max_age_handles_hours() {
        assert_eq!(parse_max_age_str("1h"), Ok(3600));
        assert_eq!(parse_max_age_str("2h"), Ok(7200));
    }

    #[test]
    fn parse_max_age_handles_seconds_suffix() {
        assert_eq!(parse_max_age_str("30s"), Ok(30));
        assert_eq!(parse_max_age_str("60s"), Ok(60));
    }

    #[test]
    fn parse_max_age_handles_bare_number() {
        assert_eq!(parse_max_age_str("300"), Ok(300));
        assert_eq!(parse_max_age_str("0"), Ok(0));
    }

    #[test]
    fn parse_max_age_rejects_invalid() {
        assert!(parse_max_age_str("invalid").is_err());
        assert!(parse_max_age_str("5x").is_err());
        assert!(parse_max_age_str("").is_err());
    }

    // ── encode_return_to ──────────────────────────────────────────────────────

    #[test]
    fn encode_return_to_leaves_plain_paths_unchanged() {
        assert_eq!(encode_return_to("/dashboard"), "/dashboard");
        assert_eq!(encode_return_to("/account/settings"), "/account/settings");
        assert_eq!(encode_return_to("/"), "/");
    }

    #[test]
    fn encode_return_to_encodes_query_delimiters() {
        let encoded = encode_return_to("/account?tab=security");
        // '?' must be encoded so the return_to param doesn't break the outer query
        assert!(encoded.contains("%3F"), "should encode '?': {encoded}");
    }

    #[test]
    fn encode_return_to_encodes_plus_sign() {
        // '+' is decoded as a space by application/x-www-form-urlencoded parsers.
        // It must be percent-encoded so a URL like /reports?q=a+b round-trips
        // correctly through the return_to query parameter.
        let encoded = encode_return_to("/reports?q=a+b");
        assert!(
            encoded.contains("%2B"),
            "'+' must be encoded as %2B: {encoded}"
        );
        assert!(
            !encoded.contains("a+b"),
            "literal '+' must not survive encoding: {encoded}"
        );
    }

    // ── __resolve_step_up_max_age ─────────────────────────────────────────────

    #[test]
    fn resolve_max_age_uses_route_override_when_set() {
        let state = crate::AppState::for_test();
        state.insert_extension(StepUpGlobalConfig {
            default_max_age_secs: 600,
        });
        assert_eq!(
            __resolve_step_up_max_age(&state, Some(120)),
            120,
            "explicit route override should take precedence over global config"
        );
    }

    #[test]
    fn resolve_max_age_reads_global_config_when_no_route_override() {
        let state = crate::AppState::for_test();
        state.insert_extension(StepUpGlobalConfig {
            default_max_age_secs: 600,
        });
        assert_eq!(
            __resolve_step_up_max_age(&state, None),
            600,
            "None route override should use global config (600s), not DEFAULT_MAX_AGE_SECS"
        );
    }

    #[test]
    fn resolve_max_age_falls_back_to_default_when_no_config() {
        let state = crate::AppState::for_test();
        assert_eq!(
            __resolve_step_up_max_age(&state, None),
            DEFAULT_MAX_AGE_SECS,
            "no config → should fall back to DEFAULT_MAX_AGE_SECS"
        );
    }

    // ── referer_path ─────────────────────────────────────────────────────────

    #[test]
    fn referer_path_extracts_path_from_full_url() {
        assert_eq!(
            referer_path("https://example.com/account/settings"),
            Some("/account/settings".to_owned()),
        );
    }

    #[test]
    fn referer_path_preserves_query_string() {
        assert_eq!(
            referer_path("https://example.com/account/settings?tab=security"),
            Some("/account/settings?tab=security".to_owned()),
        );
    }

    #[test]
    fn referer_path_rejects_protocol_relative() {
        assert_eq!(referer_path("//evil.com/path"), None);
    }

    #[test]
    fn referer_path_rejects_non_url() {
        assert_eq!(referer_path("not-a-url"), None);
        assert_eq!(referer_path(""), None);
    }

    // ── __check_step_up_with_config ───────────────────────────────────────────

    #[tokio::test]
    async fn check_step_up_with_config_fails_when_no_claim() {
        let state = crate::AppState::for_test();
        let session = Session::new_for_test("test-id".into(), HashMap::new());
        let result = __check_step_up_with_config(&session, &state, None).await;
        assert!(
            result.is_err(),
            "missing claim should fail with state check"
        );
    }

    #[tokio::test]
    async fn check_step_up_with_config_uses_global_config() {
        let state = crate::AppState::for_test();
        state.insert_extension(StepUpGlobalConfig {
            default_max_age_secs: 60,
        });
        let mut data = HashMap::new();
        // 2 minutes ago — fails against 60s global config
        let old_ts = (Utc::now() - chrono::Duration::seconds(120))
            .timestamp()
            .to_string();
        data.insert(STEP_UP_SESSION_KEY.to_string(), old_ts);
        let session = Session::new_for_test("test-id".into(), data);
        let result = __check_step_up_with_config(&session, &state, None).await;
        assert!(
            result.is_err(),
            "2-min old claim should fail against 60s global config"
        );
    }

    #[tokio::test]
    async fn check_step_up_with_config_route_overrides_global() {
        let state = crate::AppState::for_test();
        state.insert_extension(StepUpGlobalConfig {
            default_max_age_secs: 60,
        });
        let mut data = HashMap::new();
        // 2 minutes ago — passes against 600s route override
        let ts = (Utc::now() - chrono::Duration::seconds(120))
            .timestamp()
            .to_string();
        data.insert(STEP_UP_SESSION_KEY.to_string(), ts);
        let session = Session::new_for_test("test-id".into(), data);
        let result = __check_step_up_with_config(&session, &state, Some(600)).await;
        assert!(
            result.is_ok(),
            "route override of 600s should pass for 2-min old claim"
        );
    }

    #[tokio::test]
    async fn check_step_up_with_config_emits_audit_on_success() {
        use std::future::Future;
        use std::pin::Pin;
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };

        use crate::audit::{AuditError, AuditEvent, AuditLogger, AuditSink, AuditStatus};

        struct CountingSink(Arc<AtomicUsize>);
        impl AuditSink for CountingSink {
            fn write(
                &self,
                event: AuditEvent,
            ) -> Pin<Box<dyn Future<Output = Result<(), AuditError>> + Send + '_>> {
                assert_eq!(event.action, "auth.step_up.success");
                assert_eq!(event.status, AuditStatus::Success);
                let counter = self.0.clone();
                Box::pin(async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })
            }
        }

        let writes = Arc::new(AtomicUsize::new(0));
        let logger = AuditLogger::new().with_sink(Arc::new(CountingSink(writes.clone())));
        let state = crate::AppState::for_test();
        state.insert_extension(logger);

        let mut data = HashMap::new();
        data.insert(
            STEP_UP_SESSION_KEY.to_string(),
            Utc::now().timestamp().to_string(),
        );
        let session = Session::new_for_test("test-id".into(), data);

        __check_step_up_with_config(&session, &state, Some(300))
            .await
            .unwrap();
        assert_eq!(
            writes.load(Ordering::SeqCst),
            1,
            "should emit one success audit event"
        );
    }

    #[tokio::test]
    async fn check_step_up_with_config_emits_audit_on_failure() {
        use std::future::Future;
        use std::pin::Pin;
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };

        use crate::audit::{AuditError, AuditEvent, AuditLogger, AuditSink, AuditStatus};

        struct FailCountingSink(Arc<AtomicUsize>);
        impl AuditSink for FailCountingSink {
            fn write(
                &self,
                event: AuditEvent,
            ) -> Pin<Box<dyn Future<Output = Result<(), AuditError>> + Send + '_>> {
                assert_eq!(event.action, "auth.step_up.failure");
                assert_eq!(event.status, AuditStatus::Failure);
                let counter = self.0.clone();
                Box::pin(async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })
            }
        }

        let writes = Arc::new(AtomicUsize::new(0));
        let logger = AuditLogger::new().with_sink(Arc::new(FailCountingSink(writes.clone())));
        let state = crate::AppState::for_test();
        state.insert_extension(logger);

        let session = Session::new_for_test("test-id".into(), HashMap::new());

        let result = __check_step_up_with_config(&session, &state, Some(300)).await;
        assert!(result.is_err(), "should fail without claim");
        assert_eq!(
            writes.load(Ordering::SeqCst),
            1,
            "should emit one failure audit event"
        );
    }
}
