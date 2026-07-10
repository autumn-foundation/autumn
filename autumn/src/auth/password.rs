//! Password policy: length, weak/common-password rejection, context-similarity,
//! and optional Have I Been Pwned (HIBP) breach checking via k-anonymity.
//!
//! The policy is configured through the `[auth.password]` block (see
//! [`PasswordConfig`]) and evaluated at runtime by [`validate_password`], which
//! accumulates **every** applicable failure so callers can render one field
//! error per problem rather than stopping at the first.
//!
//! ## Breach checking (HIBP)
//!
//! When [`BreachCheck`] is not [`Off`](BreachCheck::Off) and an HTTP client is
//! attached to the [`PasswordPolicy`], the SHA-1 of the password is computed and
//! **only its first five hex characters** are sent to
//! `https://api.pwnedpasswords.com/range/{prefix}` (k-anonymity). The full
//! password and its full hash never leave the process and are never logged.
//!
//! The breach path only exists when the `http-client` feature is enabled (which
//! it is by default). SHA-1 is pulled in by that same feature — see the
//! `http-client` entry in `Cargo.toml`.

use std::collections::HashSet;
use std::sync::OnceLock;

/// Breach-check mode.
///
/// `Off` performs no HIBP call (the default). The two "on" variants differ only
/// in how they treat an **unreachable** HIBP endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BreachCheck {
    /// Do not consult HIBP at all.
    #[default]
    Off,
    /// Check HIBP; if the endpoint is unreachable, **allow** the password.
    ///
    /// This is the recommended setting when first enabling breach checks: a
    /// transient HIBP outage must not block legitimate sign-ups.
    FailOpen,
    /// Check HIBP; if the endpoint is unreachable, **reject** the password.
    FailClosed,
}

/// Deserializable `[auth.password]` configuration block.
///
/// ```toml
/// [auth.password]
/// min_length = 8          # minimum length in Unicode scalar values
/// reject_common = true    # reject the bundled 10k weak-password corpus
/// breach_check = "off"    # "off" | "fail_open" | "fail_closed"
/// ```
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(default)]
pub struct PasswordConfig {
    /// Minimum password length, counted in Unicode scalar values (default `8`).
    pub min_length: usize,
    /// Reject passwords found in the bundled weak-password corpus (default `true`).
    pub reject_common: bool,
    /// HIBP breach-check mode (default [`BreachCheck::Off`]).
    pub breach_check: BreachCheck,
}

impl Default for PasswordConfig {
    fn default() -> Self {
        Self {
            min_length: 8,
            reject_common: true,
            breach_check: BreachCheck::Off,
        }
    }
}

impl PasswordConfig {
    /// Build a runtime [`PasswordPolicy`] from this config (no breach client
    /// attached — call [`PasswordPolicy::with_client`] to enable the HIBP path).
    #[must_use]
    pub fn policy(&self) -> PasswordPolicy {
        PasswordPolicy::from(self)
    }
}

/// Runtime password policy.
///
/// Mirrors [`PasswordConfig`] and additionally carries an optional injected HTTP
/// client used exclusively for the HIBP breach lookup. Build one with
/// [`PasswordConfig::policy`] / [`PasswordPolicy::from`] and, when breach
/// checking is enabled, attach a client with [`PasswordPolicy::with_client`].
#[derive(Clone)]
pub struct PasswordPolicy {
    /// Minimum length in Unicode scalar values.
    pub min_length: usize,
    /// Whether to reject passwords in the bundled weak-password corpus.
    pub reject_common: bool,
    /// HIBP breach-check mode.
    pub breach_check: BreachCheck,
    /// HTTP client used for the HIBP lookup. `None` means "no client", which the
    /// breach path treats as an unreachable endpoint.
    ///
    /// Only present when the `http-client` feature is enabled; without it the
    /// breach path has nothing to call and always reports "unreachable".
    #[cfg(feature = "http-client")]
    breach_client: Option<crate::http_client::Client>,
}

impl PasswordPolicy {
    /// Construct a policy directly (no breach client attached).
    #[must_use]
    pub const fn new(min_length: usize, reject_common: bool, breach_check: BreachCheck) -> Self {
        Self {
            min_length,
            reject_common,
            breach_check,
            #[cfg(feature = "http-client")]
            breach_client: None,
        }
    }

    /// Attach the HTTP client used for HIBP breach lookups.
    #[cfg(feature = "http-client")]
    #[must_use]
    pub fn with_client(mut self, client: crate::http_client::Client) -> Self {
        self.breach_client = Some(client);
        self
    }
}

// Manual `Debug`: `http_client::Client` is not `Debug`, so we summarize the
// breach-client presence instead of the client itself.
impl std::fmt::Debug for PasswordPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut s = f.debug_struct("PasswordPolicy");
        s.field("min_length", &self.min_length)
            .field("reject_common", &self.reject_common)
            .field("breach_check", &self.breach_check);
        #[cfg(feature = "http-client")]
        s.field("breach_client", &self.breach_client.is_some());
        s.finish()
    }
}

impl From<&PasswordConfig> for PasswordPolicy {
    fn from(cfg: &PasswordConfig) -> Self {
        Self::new(cfg.min_length, cfg.reject_common, cfg.breach_check)
    }
}

/// A single reason a password was rejected.
///
/// Each variant is enumerable so handlers can map failures to individual field
/// errors and render user-facing messages via [`PasswordFailure::message`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PasswordFailure {
    /// Password is shorter than the configured minimum.
    TooShort {
        /// Configured minimum length.
        min: usize,
        /// Actual length (Unicode scalar values).
        actual: usize,
    },
    /// Password appears in the bundled weak-password corpus.
    TooCommon,
    /// Password is too similar to a piece of context (email/username).
    SimilarToContext,
    /// Password appears in a known HIBP breach.
    Breached,
    /// `breach_check = fail_closed` and the HIBP endpoint was unreachable.
    BreachCheckUnavailable,
}

impl PasswordFailure {
    /// User-facing message describing this failure.
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::TooShort { min, .. } => {
                format!("Password must be at least {min} characters.")
            }
            Self::TooCommon => {
                "This password is too common — choose something less guessable.".to_owned()
            }
            Self::SimilarToContext => {
                "Password is too similar to your email or username.".to_owned()
            }
            Self::Breached => {
                "This password has appeared in a known data breach — choose a different one."
                    .to_owned()
            }
            Self::BreachCheckUnavailable => {
                "Couldn't verify this password right now — please try again.".to_owned()
            }
        }
    }
}

/// Outcome of validating a password against a [`PasswordPolicy`].
#[derive(Debug, Clone, Default)]
pub struct PasswordValidation {
    /// Every failure that applied, in evaluation order. Empty means valid.
    pub failures: Vec<PasswordFailure>,
}

impl PasswordValidation {
    /// `true` when no failures were recorded.
    #[must_use]
    pub const fn is_valid(&self) -> bool {
        self.failures.is_empty()
    }

    /// User-facing messages for all failures.
    #[must_use]
    pub fn messages(&self) -> Vec<String> {
        self.failures.iter().map(PasswordFailure::message).collect()
    }

    /// The first failure's message, if any.
    #[must_use]
    pub fn first_message(&self) -> Option<String> {
        self.failures.first().map(PasswordFailure::message)
    }
}

/// The bundled weak-password corpus (10k lowercased entries), compiled into the
/// binary — no runtime file read (single-binary invariant).
fn common_passwords() -> &'static HashSet<&'static str> {
    static SET: OnceLock<HashSet<&'static str>> = OnceLock::new();
    SET.get_or_init(|| {
        include_str!("weak_passwords.txt")
            .split('\n')
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect()
    })
}

/// Conservative check for whether `password` is too similar to any of the
/// supplied context strings (typically the user's email and/or username).
///
/// A context string is tokenized on non-alphanumeric boundaries
/// (`john.doe@example.com` → `[john, doe, example, com]`). The password is
/// flagged when:
/// - it equals a context string outright; or
/// - any token of length ≥ 4 is a substring of the (lowercased) password; or
/// - the password with trailing digits stripped (length ≥ 3) equals a token
///   (catches `john2024`).
fn is_similar_to_context(password: &str, context: &[&str]) -> bool {
    let pw = password.to_lowercase();
    let pw_stripped = pw.trim_end_matches(|c: char| c.is_ascii_digit());
    for raw in context {
        let ctx = raw.trim();
        if ctx.is_empty() {
            continue;
        }
        let ctx_lower = ctx.to_lowercase();
        if pw == ctx_lower {
            return true;
        }
        for token in ctx_lower.split(|c: char| !c.is_alphanumeric()) {
            if token.is_empty() {
                continue;
            }
            if token.len() >= 4 && pw.contains(token) {
                return true;
            }
            if pw_stripped.len() >= 3 && pw_stripped == token {
                return true;
            }
        }
    }
    false
}

/// Internal result of the breach lookup.
enum BreachOutcome {
    /// The password was found in a breach.
    Breached,
    /// The password was checked and not found.
    NotBreached,
    /// HIBP could not be consulted (no client, network error, or non-2xx).
    Unreachable,
}

/// Perform the HIBP k-anonymity lookup. Only the 5-char SHA-1 prefix leaves the
/// process; the full password/hash is never transmitted or logged.
#[cfg(feature = "http-client")]
async fn check_breach(password: &str, policy: &PasswordPolicy) -> BreachOutcome {
    use sha1::{Digest as _, Sha1};

    let Some(client) = policy.breach_client.as_ref() else {
        return BreachOutcome::Unreachable;
    };

    let digest = Sha1::digest(password.as_bytes());
    let hash = hex::encode_upper(digest);
    let (prefix, suffix) = hash.split_at(5);

    let url = format!("https://api.pwnedpasswords.com/range/{prefix}");
    let resp = match client.get(&url).send().await {
        Ok(resp) if resp.is_success() => resp,
        _ => return BreachOutcome::Unreachable,
    };

    // Body is `SUFFIX:COUNT` per line (uppercase hex suffixes). We look for our
    // suffix anchored to the `:` count separator so a partial hex overlap can't
    // false-positive. Matching by substring (rather than strict line parsing)
    // keeps this robust to CRLF vs LF line endings.
    let body = resp.text().to_uppercase();
    let needle = format!("{suffix}:");
    if body.contains(&needle) {
        BreachOutcome::Breached
    } else {
        BreachOutcome::NotBreached
    }
}

/// Fallback when the `http-client` feature is disabled: there is no client to
/// call, so the endpoint is always "unreachable".
#[cfg(not(feature = "http-client"))]
#[allow(clippy::unused_async)]
async fn check_breach(_password: &str, _policy: &PasswordPolicy) -> BreachOutcome {
    BreachOutcome::Unreachable
}

/// Validate `password` against `policy`, accumulating **all** applicable
/// failures.
///
/// `context` carries strings the password should not resemble — typically the
/// user's email and username. Pass an empty slice to skip the similarity check.
///
/// The breach check runs only when `policy.breach_check` is not
/// [`BreachCheck::Off`]; see [`BreachCheck`] for fail-open vs fail-closed
/// semantics when HIBP is unreachable.
pub async fn validate_password(
    password: &str,
    policy: &PasswordPolicy,
    context: &[&str],
) -> PasswordValidation {
    let mut failures = Vec::new();

    // 1. Length (Unicode-safe).
    let actual = password.chars().count();
    if actual < policy.min_length {
        failures.push(PasswordFailure::TooShort {
            min: policy.min_length,
            actual,
        });
    }

    // 2. Common/weak corpus.
    if policy.reject_common {
        let lower = password.to_lowercase();
        if common_passwords().contains(lower.as_str()) {
            failures.push(PasswordFailure::TooCommon);
        }
    }

    // 3. Context similarity.
    if is_similar_to_context(password, context) {
        failures.push(PasswordFailure::SimilarToContext);
    }

    // 4. Breach check.
    if policy.breach_check != BreachCheck::Off {
        match check_breach(password, policy).await {
            BreachOutcome::Breached => failures.push(PasswordFailure::Breached),
            BreachOutcome::NotBreached => {}
            BreachOutcome::Unreachable => {
                if policy.breach_check == BreachCheck::FailClosed {
                    failures.push(PasswordFailure::BreachCheckUnavailable);
                }
            }
        }
    }

    PasswordValidation { failures }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(min_length: usize, reject_common: bool, breach_check: BreachCheck) -> PasswordPolicy {
        PasswordPolicy::new(min_length, reject_common, breach_check)
    }

    // ── Config / defaults ────────────────────────────────────────────────────

    #[test]
    fn config_defaults_are_sane() {
        let cfg = PasswordConfig::default();
        assert_eq!(cfg.min_length, 8);
        assert!(cfg.reject_common);
        assert_eq!(cfg.breach_check, BreachCheck::Off);
    }

    #[test]
    fn config_deserialises_breach_modes() {
        #[derive(serde::Deserialize)]
        struct Wrap {
            password: PasswordConfig,
        }
        let toml = r#"
            [password]
            min_length = 12
            reject_common = false
            breach_check = "fail_closed"
        "#;
        let w: Wrap = toml::from_str(toml).unwrap();
        assert_eq!(w.password.min_length, 12);
        assert!(!w.password.reject_common);
        assert_eq!(w.password.breach_check, BreachCheck::FailClosed);
    }

    #[test]
    fn config_to_policy_roundtrips_fields() {
        let cfg = PasswordConfig {
            min_length: 10,
            reject_common: true,
            breach_check: BreachCheck::FailOpen,
        };
        let p = cfg.policy();
        assert_eq!(p.min_length, 10);
        assert!(p.reject_common);
        assert_eq!(p.breach_check, BreachCheck::FailOpen);
    }

    // ── Length ───────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn too_short_is_flagged() {
        let v = validate_password("ab", &policy(8, false, BreachCheck::Off), &[]).await;
        assert!(
            v.failures
                .contains(&PasswordFailure::TooShort { min: 8, actual: 2 })
        );
        assert!(!v.is_valid());
    }

    #[tokio::test]
    async fn exactly_min_length_passes_length() {
        // 8 non-common chars, exactly at the boundary.
        let v = validate_password("aZ3!xQ7v", &policy(8, true, BreachCheck::Off), &[]).await;
        assert!(
            !v.failures
                .iter()
                .any(|f| matches!(f, PasswordFailure::TooShort { .. }))
        );
    }

    #[tokio::test]
    async fn length_counts_unicode_scalar_values() {
        // 4 multibyte chars — byte length would be 8+ but char count is 4.
        let v = validate_password("héllo", &policy(8, false, BreachCheck::Off), &[]).await;
        // "héllo" is 5 chars < 8.
        assert!(
            v.failures
                .iter()
                .any(|f| matches!(f, PasswordFailure::TooShort { actual: 5, .. }))
        );
    }

    // ── Common / weak corpus ─────────────────────────────────────────────────

    #[tokio::test]
    async fn common_passwords_are_rejected() {
        for pw in [
            "password",
            "12345678",
            "qwerty",
            "letmein",
            "password123",
            "iloveyou",
        ] {
            let v = validate_password(pw, &policy(1, true, BreachCheck::Off), &[]).await;
            assert!(
                v.failures.contains(&PasswordFailure::TooCommon),
                "expected {pw:?} to be flagged TooCommon"
            );
        }
    }

    #[tokio::test]
    async fn strong_passphrase_is_not_common() {
        let v = validate_password(
            "Tr0ub4dour&3-Staple-Horse",
            &policy(8, true, BreachCheck::Off),
            &[],
        )
        .await;
        assert!(!v.failures.contains(&PasswordFailure::TooCommon));
    }

    #[tokio::test]
    async fn reject_common_false_disables_common_check() {
        let v = validate_password("password", &policy(1, false, BreachCheck::Off), &[]).await;
        assert!(!v.failures.contains(&PasswordFailure::TooCommon));
        assert!(v.is_valid());
    }

    // ── Context similarity ───────────────────────────────────────────────────

    #[test]
    fn similarity_helper_flags_full_email_and_local_part() {
        assert!(is_similar_to_context(
            "john@example.com",
            &["john@example.com"]
        ));
        assert!(is_similar_to_context("john", &["john@example.com"]));
        assert!(is_similar_to_context("john2024", &["john@example.com"]));
        assert!(is_similar_to_context("password", &["password"]));
    }

    #[test]
    fn similarity_helper_ignores_unrelated_and_empty() {
        assert!(!is_similar_to_context(
            "Tr0ub4dour&3-Staple-Horse",
            &["john@example.com"]
        ));
        assert!(!is_similar_to_context("Tr0ub4dour&3", &[]));
        assert!(!is_similar_to_context("Tr0ub4dour&3", &["  "]));
        // ".com" (len 3) must not trigger a false positive.
        assert!(!is_similar_to_context("Tr0ub4dour&3.com", &["a@b.com"]));
    }

    #[tokio::test]
    async fn context_similarity_is_flagged() {
        let v = validate_password(
            "john@example.com",
            &policy(1, false, BreachCheck::Off),
            &["john@example.com"],
        )
        .await;
        assert!(v.failures.contains(&PasswordFailure::SimilarToContext));

        let v2 = validate_password(
            "john",
            &policy(1, false, BreachCheck::Off),
            &["john@example.com"],
        )
        .await;
        assert!(v2.failures.contains(&PasswordFailure::SimilarToContext));

        let v3 = validate_password(
            "Tr0ub4dour&3-Staple-Horse",
            &policy(8, false, BreachCheck::Off),
            &["john@example.com"],
        )
        .await;
        assert!(!v3.failures.contains(&PasswordFailure::SimilarToContext));
    }

    // ── Fully valid ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn fully_valid_strong_password() {
        let v = validate_password(
            "Tr0ub4dour&3-Staple-Horse",
            &policy(8, true, BreachCheck::Off),
            &["alice@example.com", "alice"],
        )
        .await;
        assert!(v.is_valid(), "unexpected failures: {:?}", v.failures);
        assert!(v.failures.is_empty());
        assert!(v.first_message().is_none());
    }

    // ── Accumulation ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn multiple_failures_accumulate() {
        // "qwerty" is 6 chars (< 8) AND in the common corpus.
        let v = validate_password("qwerty", &policy(8, true, BreachCheck::Off), &[]).await;
        assert!(
            v.failures
                .iter()
                .any(|f| matches!(f, PasswordFailure::TooShort { .. }))
        );
        assert!(v.failures.contains(&PasswordFailure::TooCommon));
        assert_eq!(v.messages().len(), 2);
    }

    // ── Failure messages ─────────────────────────────────────────────────────

    #[test]
    fn failure_messages_render() {
        assert_eq!(
            PasswordFailure::TooShort { min: 8, actual: 2 }.message(),
            "Password must be at least 8 characters."
        );
        assert!(PasswordFailure::TooCommon.message().contains("too common"));
        assert!(
            PasswordFailure::SimilarToContext
                .message()
                .contains("too similar")
        );
        assert!(PasswordFailure::Breached.message().contains("data breach"));
        assert!(
            PasswordFailure::BreachCheckUnavailable
                .message()
                .contains("try again")
        );
    }

    // ── Breach: no-client fallbacks ──────────────────────────────────────────

    #[tokio::test]
    async fn breach_fail_open_without_client_allows() {
        let v = validate_password(
            "Tr0ub4dour&3-Staple-Horse",
            &policy(8, false, BreachCheck::FailOpen),
            &[],
        )
        .await;
        assert!(v.is_valid());
        assert!(!v.failures.contains(&PasswordFailure::Breached));
        assert!(
            !v.failures
                .contains(&PasswordFailure::BreachCheckUnavailable)
        );
    }

    #[tokio::test]
    async fn breach_fail_closed_without_client_is_unavailable() {
        let v = validate_password(
            "Tr0ub4dour&3-Staple-Horse",
            &policy(8, false, BreachCheck::FailClosed),
            &[],
        )
        .await;
        assert!(
            v.failures
                .contains(&PasswordFailure::BreachCheckUnavailable)
        );
    }

    // ── Breach: mocked HIBP endpoint ─────────────────────────────────────────

    #[cfg(feature = "http-client")]
    mod breach_mock {
        use super::*;
        use crate::http_client::{Client, MockEntry, MockRegistry};
        use reqwest::Method;
        use std::sync::Arc;
        use std::sync::atomic::AtomicUsize;

        /// Compute the HIBP (prefix, suffix) pair the way production does.
        fn hibp_split(password: &str) -> (String, String) {
            use sha1::{Digest as _, Sha1};
            let hash = hex::encode_upper(Sha1::digest(password.as_bytes()));
            (hash[..5].to_owned(), hash[5..].to_owned())
        }

        fn mock_client(prefix: &str, body: &str) -> Client {
            let registry = Arc::new(MockRegistry::new());
            registry.register(MockEntry {
                method: Some(Method::GET),
                path: format!("/range/{prefix}"),
                alias: None,
                status: 200,
                body: Some(serde_json::Value::String(body.to_owned())),
                call_count: Arc::new(AtomicUsize::new(0)),
            });
            Client::new().with_mock(registry)
        }

        #[tokio::test]
        async fn breached_when_suffix_present_in_body() {
            let pw = "hunter2";
            let (prefix, suffix) = hibp_split(pw);
            // Our suffix sits on a clean middle line so JSON-string quoting on
            // the first/last line can't corrupt the match.
            let body = format!("00000AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA:1\n{suffix}:42\nFFFFF:3");
            let client = mock_client(&prefix, &body);

            let v = validate_password(
                pw,
                &policy(1, false, BreachCheck::FailClosed).with_client(client),
                &[],
            )
            .await;
            assert!(v.failures.contains(&PasswordFailure::Breached));
        }

        #[tokio::test]
        async fn not_breached_when_suffix_absent() {
            let pw = "hunter2";
            let (prefix, _suffix) = hibp_split(pw);
            let body =
                "00000AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA:1\nBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB:2";
            let client = mock_client(&prefix, body);

            let v = validate_password(
                pw,
                &policy(1, false, BreachCheck::FailClosed).with_client(client),
                &[],
            )
            .await;
            assert!(!v.failures.contains(&PasswordFailure::Breached));
            assert!(
                !v.failures
                    .contains(&PasswordFailure::BreachCheckUnavailable)
            );
            assert!(v.is_valid());
        }

        #[tokio::test]
        async fn unreachable_endpoint_fail_open_allows() {
            // Register a mock for a DIFFERENT prefix so the real lookup misses
            // and the client returns a NoMock error → unreachable.
            let pw = "hunter2";
            let client = mock_client("ZZZZZ", "irrelevant:1");
            let v = validate_password(
                pw,
                &policy(1, false, BreachCheck::FailOpen).with_client(client),
                &[],
            )
            .await;
            assert!(v.is_valid());
        }

        #[tokio::test]
        async fn unreachable_endpoint_fail_closed_unavailable() {
            let pw = "hunter2";
            let client = mock_client("ZZZZZ", "irrelevant:1");
            let v = validate_password(
                pw,
                &policy(1, false, BreachCheck::FailClosed).with_client(client),
                &[],
            )
            .await;
            assert!(
                v.failures
                    .contains(&PasswordFailure::BreachCheckUnavailable)
            );
        }
    }
}
