//! `autumn doctor` — first-run environment diagnostics.
//!
//! Runs a set of checks against the local environment and project configuration,
//! reports each as ✅/⚠️/❌ with a one-line remediation hint, and exits with
//! code 0 (all clear) or 1 (any failure detected).

use serde::Serialize;
use std::collections::{HashMap, HashSet};

// ── Signing secret validation constants (mirrored from autumn-web) ────────────

/// Minimum byte length for a valid production signing secret.
const SIGNING_SECRET_MIN_LEN: usize = 32;

/// Known demo / template values that must never reach production.
const SIGNING_SECRET_DEMO_VALUES: &[&str] = &[
    "changeme",
    "change_me",
    "change-me",
    "secret",
    "supersecret",
    "super-secret",
    "super_secret",
    "your-secret-here",
    "your_secret_here",
    "insert-secret-here",
    "replace-this",
    "replace_me",
    "todo",
    "fixme",
    "example",
    "placeholder",
    "dev_only",
    "dev-only",
    "test_secret",
    "test-secret",
    "test",
    "password",
];

/// Known top-level keys in a valid `autumn.toml`.
#[cfg(test)]
const KNOWN_TOML_SECTIONS: &[&str] = &[
    "server",
    "database",
    "log",
    "telemetry",
    "health",
    "actuator",
    "cors",
    "session",
    "jobs",
    "auth",
    "security",
    "i18n",
    "storage",
    "mail",
    "profile",
    "compression",
];

/// Result status for a single diagnostic check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CheckStatus {
    Pass,
    Warn,
    Fail,
}

/// Result of a single diagnostic check.
#[derive(Debug, Clone, Serialize)]
pub struct CheckResult {
    /// Short identifier for this check.
    pub name: &'static str,
    pub status: CheckStatus,
    /// Human-readable detail (what was found, or what went wrong).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// One-line remediation hint shown on warn/fail.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<&'static str>,
}

/// Aggregate counts across all checks.
#[derive(Debug, Clone, Serialize)]
pub struct Summary {
    pub passed: usize,
    pub warned: usize,
    pub failed: usize,
}

/// Options parsed from CLI flags.
#[derive(Clone, Copy)]
pub struct DoctorOptions {
    /// Emit machine-readable JSON instead of human text.
    pub json: bool,
    /// Treat warnings as failures (exit 1).
    pub strict: bool,
}

/// Extension point: implement this trait to add custom checks.
#[allow(dead_code)]
pub trait Check {
    fn run(&self) -> CheckResult;
}

/// A deprecated config key detected in the project's resolved configuration.
/// Used as input to [`check_deprecated_keys_impl`].
#[derive(Debug, Clone)]
pub struct DoctorDeprecation {
    pub path: String,
    pub replacement: Option<String>,
    pub since: String,
    pub remove_in: String,
}

/// Check for deprecated config key usage (pure, injectable for tests).
///
/// Emits one `⚠️ deprecated_keys` check with one line of `detail` per offending
/// key, consistent with how `check_toml_content` aggregates unknown-key errors.
pub fn check_deprecated_keys_impl(found: &[DoctorDeprecation]) -> CheckResult {
    if found.is_empty() {
        return CheckResult {
            name: "deprecated_keys",
            status: CheckStatus::Pass,
            detail: Some("no deprecated configuration keys in use".into()),
            hint: None,
        };
    }

    let lines: Vec<String> = found
        .iter()
        .map(|d| {
            let repl = d
                .replacement
                .as_deref()
                .unwrap_or("remove it (no replacement)");
            format!(
                "{} is deprecated since {} (use {}; removed in {})",
                d.path, d.since, repl, d.remove_in
            )
        })
        .collect();

    CheckResult {
        name: "deprecated_keys",
        status: CheckStatus::Warn,
        detail: Some(lines.join("\n")),
        hint: Some(
            "Migrate to the replacement keys before the removal version; \
             deprecated keys are still honored during the deprecation window",
        ),
    }
}

/// Check signing-secret readiness (pure, injectable for tests).
///
/// - **Dev/test** (`is_production = false`): warns when no secret is configured
///   (an ephemeral per-process key is in use) and passes when a secret is set.
/// - **Production** (`is_production = true`): fails when the secret is missing,
///   below the minimum entropy floor, or matches a known demo/template value.
pub fn check_signing_secret_impl(secret: Option<&str>, is_production: bool) -> CheckResult {
    match secret {
        None if is_production => CheckResult {
            name: "signing_secret",
            status: CheckStatus::Fail,
            detail: Some("no signing secret configured in production".into()),
            hint: Some(
                "Set AUTUMN_SECURITY__SIGNING_SECRET (generate with `openssl rand -hex 32`)",
            ),
        },
        None => CheckResult {
            name: "signing_secret",
            status: CheckStatus::Warn,
            detail: Some(
                "using an ephemeral per-process signing secret (dev/test only; \
                 sessions and signed URLs will not survive restarts or be shared across replicas)"
                    .into(),
            ),
            hint: Some("Set AUTUMN_SECURITY__SIGNING_SECRET before deploying to production"),
        },
        Some(s) if is_production => {
            // Demo-value check first: "changeme" is more informative than "too short".
            let lower = s.to_ascii_lowercase();
            if SIGNING_SECRET_DEMO_VALUES.iter().any(|&d| lower == d) {
                return CheckResult {
                    name: "signing_secret",
                    status: CheckStatus::Fail,
                    detail: Some("signing secret matches a known demo/template value".into()),
                    hint: Some("Generate a new secret: `openssl rand -hex 32`"),
                };
            }
            let byte_len = s.len();
            if byte_len < SIGNING_SECRET_MIN_LEN {
                return CheckResult {
                    name: "signing_secret",
                    status: CheckStatus::Fail,
                    detail: Some(format!(
                        "signing secret too short: {byte_len} bytes \
                         (minimum {SIGNING_SECRET_MIN_LEN})"
                    )),
                    hint: Some("Generate a longer secret: `openssl rand -hex 32`"),
                };
            }
            CheckResult {
                name: "signing_secret",
                status: CheckStatus::Pass,
                detail: Some("signing secret is present and meets entropy requirements".into()),
                hint: None,
            }
        }
        Some(_) => CheckResult {
            name: "signing_secret",
            status: CheckStatus::Pass,
            detail: Some("signing secret is configured".into()),
            hint: None,
        },
    }
}

/// Check that the rate-limit key strategy is consistent with mounted auth middleware.
///
/// When `key_strategy = "authenticated_principal"`, the rate limiter reads a
/// `RateLimitPrincipal` extension that must be set by the auth layer. If no
/// auth extractor is mounted, unauthenticated requests still fall back to IP
/// keying, but authenticated routes will never use the principal bucket.
///
/// `auth_extractor_mounted` should be `true` when the project has auth configured
/// (e.g. `[auth]` section present and enabled in `autumn.toml`).
///
/// In strict mode (`autumn doctor --strict`), a warning is surfaced as an error.
pub fn check_rate_limit_key_strategy_impl(
    key_strategy: &str,
    auth_extractor_mounted: bool,
) -> CheckResult {
    if !key_strategy.is_empty()
        && key_strategy != "ip"
        && key_strategy != "api_token"
        && key_strategy != "authenticated_principal"
    {
        return CheckResult {
            name: "rate_limit_key_strategy",
            status: CheckStatus::Fail,
            detail: Some(format!(
                "rate_limit.key_strategy = {key_strategy:?} is not a valid strategy"
            )),
            hint: Some("Expected \"ip\", \"api_token\", or \"authenticated_principal\""),
        };
    }

    if key_strategy == "authenticated_principal" && !auth_extractor_mounted {
        return CheckResult {
            name: "rate_limit_key_strategy",
            status: CheckStatus::Warn,
            detail: Some(
                "rate_limit.key_strategy = \"authenticated_principal\" is configured \
                 but no auth extractor is mounted — unauthenticated requests will \
                 always fall back to IP keying, so the per-principal budget \
                 is never applied to authenticated callers"
                    .into(),
            ),
            hint: Some(
                "Mount an auth layer (e.g. `[auth]` in autumn.toml) or change \
                 key_strategy to \"ip\" or \"api_token\"",
            ),
        };
    }

    CheckResult {
        name: "rate_limit_key_strategy",
        status: CheckStatus::Pass,
        detail: Some(format!(
            "rate_limit.key_strategy = {:?} is compatible with the current auth configuration",
            if key_strategy.is_empty() {
                "ip"
            } else {
                key_strategy
            }
        )),
        hint: None,
    }
}

/// Input data for the proxy-conflict check, resolved from `autumn.toml` and env vars.
#[derive(Default)]
pub struct ProxyConflictData {
    pub new_ranges: Vec<String>,
    pub new_trust_fwd: bool,
    pub new_hops: Option<u32>,
    pub old_ranges: Vec<String>,
    pub old_trust_fwd: bool,
}

/// Warn when both `[security.trusted_proxies]` and the deprecated
/// `[security.rate_limit]` proxy fields are configured with conflicting values.
pub fn check_proxy_conflict_impl(data: &ProxyConflictData) -> CheckResult {
    let new_set = data.new_trust_fwd || !data.new_ranges.is_empty();
    let old_set = data.old_trust_fwd || !data.old_ranges.is_empty();

    if new_set && old_set {
        let new_set: std::collections::HashSet<&str> =
            data.new_ranges.iter().map(String::as_str).collect();
        let old_set: std::collections::HashSet<&str> =
            data.old_ranges.iter().map(String::as_str).collect();
        let conflicting = new_set != old_set
            || data.new_trust_fwd != data.old_trust_fwd
            || data.new_hops.is_some();

        if conflicting {
            return CheckResult {
                name: "proxy_config_conflict",
                status: CheckStatus::Warn,
                detail: Some(
                    "[security.trusted_proxies] and [security.rate_limit] trusted proxy \
                     fields are both set with conflicting values."
                        .into(),
                ),
                hint: Some(
                    "Remove the deprecated rate_limit.trusted_proxies / \
                     rate_limit.trust_forwarded_headers fields and keep only \
                     [security.trusted_proxies]",
                ),
            };
        }
    }

    CheckResult {
        name: "proxy_config_conflict",
        status: CheckStatus::Pass,
        detail: None,
        hint: None,
    }
}

pub fn check_trusted_hosts_impl(hosts: &[String], is_production: bool) -> CheckResult {
    let normalized: Vec<String> = hosts
        .iter()
        .map(|h| h.trim().to_owned())
        .filter(|h| !h.is_empty())
        .collect();
    if normalized.is_empty() {
        return if is_production {
            CheckResult {
                name: "trusted_hosts",
                status: CheckStatus::Fail,
                detail: Some("trusted hosts list is empty in production".into()),
                hint: Some(
                    "Set [security.trusted_hosts] hosts = [\"example.com\"] or \
                     AUTUMN_SECURITY__TRUSTED_HOSTS__HOSTS",
                ),
            }
        } else {
            CheckResult {
                name: "trusted_hosts",
                status: CheckStatus::Warn,
                detail: Some("trusted hosts list is empty".into()),
                hint: Some(
                    "Set [security.trusted_hosts] hosts to prevent Host-header rebinding attacks",
                ),
            }
        };
    }

    if normalized.iter().any(|h| h == "*") {
        return CheckResult {
            name: "trusted_hosts",
            status: CheckStatus::Warn,
            detail: Some("trusted hosts wildcard '*' disables host-header allow-listing".into()),
            hint: Some("Prefer explicit hosts in production to fail closed"),
        };
    }

    CheckResult {
        name: "trusted_hosts",
        status: CheckStatus::Pass,
        detail: Some(format!(
            "trusted hosts configured ({} entries)",
            normalized.len()
        )),
        hint: None,
    }
}

/// Check a single `[auth.oauth2.<provider>]` entry for common misconfigurations.
///
/// - In production (`is_production = true`): fails when `client_secret` is empty.
/// - Outside production: warns when `client_secret` is empty.
///
/// The returned check name is `"oauth2_provider"`.
pub fn check_oauth2_provider_impl(
    provider_name: &str,
    client_id: &str,
    client_secret: &str,
    is_production: bool,
) -> CheckResult {
    // Use a static string for the name field to satisfy lifetime constraints.
    // The name is formatted in the `detail` field instead.
    let detail_prefix = format!("[auth.oauth2.{provider_name}]");

    // Check client_id first
    if client_id.trim().is_empty() {
        return if is_production {
            CheckResult {
                name: "oauth2_provider",
                status: CheckStatus::Fail,
                detail: Some(format!(
                    "{detail_prefix}: client_id is empty — OAuth2 login will fail in production"
                )),
                hint: Some(
                    "Set client_id via AUTUMN_AUTH__OAUTH2__<PROVIDER>__CLIENT_ID \
                     or in autumn.toml",
                ),
            }
        } else {
            CheckResult {
                name: "oauth2_provider",
                status: CheckStatus::Warn,
                detail: Some(format!(
                    "{detail_prefix}: client_id is empty (OK for dev if using env vars)"
                )),
                hint: Some("Set client_id before deploying to production"),
            }
        };
    }

    // Check client_secret next
    if client_secret.trim().is_empty() {
        return if is_production {
            CheckResult {
                name: "oauth2_provider",
                status: CheckStatus::Fail,
                detail: Some(format!(
                    "{detail_prefix}: client_secret is empty — OAuth2 login will fail in production"
                )),
                hint: Some(
                    "Set client_secret via AUTUMN_AUTH__OAUTH2__<PROVIDER>__CLIENT_SECRET \
                     or autumn credentials edit",
                ),
            }
        } else {
            CheckResult {
                name: "oauth2_provider",
                status: CheckStatus::Warn,
                detail: Some(format!(
                    "{detail_prefix}: client_secret is empty (OK for dev if using env vars)"
                )),
                hint: Some("Set client_secret before deploying to production"),
            }
        };
    }

    CheckResult {
        name: "oauth2_provider",
        status: CheckStatus::Pass,
        detail: Some(format!("{detail_prefix}: provider is correctly configured")),
        hint: None,
    }
}

/// Check that List-Unsubscribe is wired when any `#[mailer]` declares
/// `list_unsubscribe` (pure, injectable for tests).
///
/// - No `list_unsubscribe` usage, or the transport is disabled (no mail is
///   emitted) → pass.
/// - Usage with a base URL or mailto configured → pass.
/// - Usage with neither configured → **fail** in production (Gmail/Yahoo will
///   reject the bulk mail) and **warn** outside production so
///   `autumn doctor --strict` still surfaces it before deploy.
pub fn check_mail_unsubscribe_config_impl(
    has_list_unsubscribe_usage: bool,
    base_url: Option<&str>,
    mailto: Option<&str>,
    transport_disabled: bool,
    is_production: bool,
    token_ttl_days: i64,
) -> CheckResult {
    let base_url = base_url.map(str::trim).filter(|s| !s.is_empty());
    let mailto = mailto.map(str::trim).filter(|s| !s.is_empty());
    let any_set = base_url.is_some() || mailto.is_some();

    // The runtime `MailConfig::validate` runs the following checks at boot
    // regardless of whether any #[mailer] declares list_unsubscribe or whether
    // the transport is disabled. Mirror them *before* the usage/transport early
    // returns so `autumn doctor --strict` never greenlights a deploy that the
    // runtime would reject at startup.

    // TTL: a non-positive value is rejected in every profile (it would make every
    // unsubscribe token immediately expired).
    if token_ttl_days <= 0 {
        return CheckResult {
            name: "mail_unsubscribe",
            status: CheckStatus::Fail,
            detail: Some(format!(
                "mail.unsubscribe_token_ttl_days must be a positive number of days (got {token_ttl_days})"
            )),
            hint: Some(
                "Set mail.unsubscribe_token_ttl_days to a positive value (default 30); a non-positive value would make every unsubscribe token immediately expired",
            ),
        };
    }

    // Destination format: a non-empty but malformed base_url / mailto is rejected
    // in prod (mailbox providers require HTTPS one-click / a real mailbox).
    if is_production {
        if let Some(url) = base_url
            && !is_valid_https_base_url_doctor(url)
        {
            return CheckResult {
                name: "mail_unsubscribe",
                status: CheckStatus::Fail,
                detail: Some(format!(
                    "mail.unsubscribe_base_url is set to an invalid value ({url:?}): \
                     the runtime requires an absolute https:// URL with a real host"
                )),
                hint: Some(
                    "Use an absolute https:// URL with a host and no query/fragment, e.g. https://app.example.com",
                ),
            };
        }
        if let Some(addr) = mailto
            && !is_valid_mailto_address_doctor(addr)
        {
            return CheckResult {
                name: "mail_unsubscribe",
                status: CheckStatus::Fail,
                detail: Some(format!(
                    "mail.unsubscribe_mailto is set to an invalid value ({addr:?}): \
                     the runtime requires a bare mailbox address or mailto: URI"
                )),
                hint: Some(
                    "Use a mailbox like unsubscribe@example.com (or mailto:unsubscribe@example.com)",
                ),
            };
        }
    }

    if !has_list_unsubscribe_usage {
        return CheckResult {
            name: "mail_unsubscribe",
            status: CheckStatus::Pass,
            detail: Some("no #[mailer] declares list_unsubscribe".into()),
            hint: None,
        };
    }
    if transport_disabled {
        // Mirrors the runtime guard: a disabled transport emits no list mail, so
        // unsubscribe destinations aren't required to boot.
        return CheckResult {
            name: "mail_unsubscribe",
            status: CheckStatus::Pass,
            detail: Some("mail.transport is disabled; no list mail is emitted".into()),
            hint: None,
        };
    }

    if any_set {
        // Reached only when a #[mailer] uses list_unsubscribe and the transport is
        // active (the no-usage / disabled-transport cases returned above).
        return mail_unsubscribe_configured_result(base_url.is_some(), is_production);
    }
    let detail = Some(
        "a #[mailer] declares list_unsubscribe but neither mail.unsubscribe_base_url \
         nor mail.unsubscribe_mailto is configured"
            .into(),
    );
    let hint = Some(
        "Set mail.unsubscribe_base_url (e.g. https://app.example.com) or mail.unsubscribe_mailto so RFC 8058 List-Unsubscribe headers can be emitted",
    );
    if is_production {
        CheckResult {
            name: "mail_unsubscribe",
            status: CheckStatus::Fail,
            detail,
            hint,
        }
    } else {
        CheckResult {
            name: "mail_unsubscribe",
            status: CheckStatus::Warn,
            detail,
            hint,
        }
    }
}

/// Result for a configured unsubscribe destination (a valid `base_url` and/or
/// `mailto` is set, a list mailer is in use, and the transport is active).
///
/// A `base_url` drives the framework's one-click HTTP endpoint, which must
/// persist opt-outs. The runtime fails closed in production when a base URL is
/// set but no suppression backend (a database pool, or a `SuppressionStore`
/// registered via `AppBuilder::with_suppression_store`) is available. doctor
/// can't see a programmatically registered store, so it warns rather than
/// greenlight a config the app may reject at startup — under `--strict` this
/// surfaces as an error. A mailto-only destination needs no store, so it passes.
fn mail_unsubscribe_configured_result(base_url_set: bool, is_production: bool) -> CheckResult {
    if is_production && base_url_set {
        return CheckResult {
            name: "mail_unsubscribe",
            status: CheckStatus::Warn,
            detail: Some(
                "mail.unsubscribe_base_url is set, so the one-click endpoint must persist \
                 opt-outs; the runtime fails closed in production unless a suppression backend \
                 (a database pool, or a SuppressionStore registered via \
                 AppBuilder::with_suppression_store) is available — which doctor cannot verify \
                 statically"
                    .into(),
            ),
            hint: Some(
                "Configure a database pool (db feature) or register a SuppressionStore before deploying so one-click unsubscribes are recorded",
            ),
        };
    }
    CheckResult {
        name: "mail_unsubscribe",
        status: CheckStatus::Pass,
        detail: Some("list_unsubscribe is wired (unsubscribe URL/mailto configured)".into()),
        hint: None,
    }
}

/// Mirror of `autumn_web`'s `is_valid_https_base_url` (mail module, feature-gated
/// so not reachable from the CLI build). Kept byte-for-byte in sync so
/// `autumn doctor --strict` rejects exactly what the runtime rejects at boot.
fn is_valid_https_base_url_doctor(url: &str) -> bool {
    if url
        .chars()
        .any(|c| c.is_control() || c.is_whitespace() || matches!(c, '<' | '>'))
    {
        return false;
    }
    match url.strip_prefix("https://") {
        Some(rest) if !rest.is_empty() && !rest.starts_with('/') => {}
        _ => return false,
    }
    let Ok(parsed) = ::url::Url::parse(url) else {
        return false;
    };
    parsed.scheme() == "https"
        && parsed.host_str().is_some_and(|h| !h.is_empty())
        && parsed.username().is_empty()
        && parsed.password().is_none()
        && parsed.query().is_none()
        && parsed.fragment().is_none()
}

/// Mirror of `autumn_web`'s `is_valid_mailto_address` (see above).
fn is_valid_mailto_address_doctor(value: &str) -> bool {
    // Reject control characters and RFC 2369 delimiters (`<`/`>`/`,`) anywhere in
    // the value; mirrors autumn_web's is_valid_mailto_address.
    if value
        .chars()
        .any(|c| c.is_control() || matches!(c, '<' | '>' | ','))
    {
        return false;
    }
    let address = value
        .trim()
        .strip_prefix("mailto:")
        .unwrap_or_else(|| value.trim());
    let address = address.split('?').next().unwrap_or("");
    match address.split_once('@') {
        Some((local, domain)) => {
            !local.is_empty()
                && !domain.is_empty()
                && domain.contains('.')
                && !address.contains(char::is_whitespace)
                && !address.contains([':', '/'])
        }
        None => false,
    }
}

// ─── Pure helper functions (fully unit-testable) ──────────────────────────────

pub const fn glyph(status: &CheckStatus) -> &'static str {
    match status {
        CheckStatus::Pass => "✅",
        CheckStatus::Warn => "⚠️ ",
        CheckStatus::Fail => "❌",
    }
}

pub fn compute_summary(results: &[CheckResult]) -> Summary {
    let mut passed = 0;
    let mut warned = 0;
    let mut failed = 0;
    for r in results {
        match r.status {
            CheckStatus::Pass => passed += 1,
            CheckStatus::Warn => warned += 1,
            CheckStatus::Fail => failed += 1,
        }
    }
    Summary {
        passed,
        warned,
        failed,
    }
}

pub const fn exit_code(summary: &Summary, strict: bool) -> i32 {
    if summary.failed > 0 || (strict && summary.warned > 0) {
        1
    } else {
        0
    }
}

pub fn format_check_line(result: &CheckResult) -> String {
    use std::fmt::Write as _;
    let g = glyph(&result.status);
    let mut line = format!("{g} {}", result.name);
    if let Some(ref detail) = result.detail {
        let _ = write!(line, " — {detail}");
    }
    if let Some(hint) = result.hint
        && result.status != CheckStatus::Pass
    {
        let _ = write!(line, "\n   hint: {hint}");
    }
    line
}

pub fn format_summary_line(summary: &Summary, code: i32) -> String {
    let verdict = if code == 0 {
        "all clear"
    } else {
        "problems found"
    };
    let w_label = if summary.warned == 1 {
        "warning"
    } else {
        "warnings"
    };
    format!(
        "{} passed, {} {}, {} failed — {verdict}",
        summary.passed, summary.warned, w_label, summary.failed
    )
}

pub fn to_json_output(results: &[CheckResult], summary: &Summary) -> String {
    #[derive(Serialize)]
    struct Output<'a> {
        checks: &'a [CheckResult],
        summary: &'a Summary,
    }
    serde_json::to_string_pretty(&Output {
        checks: results,
        summary,
    })
    .unwrap_or_else(|_| "{}".to_string())
}

// ─── Check implementations ────────────────────────────────────────────────────

/// Recursively validates TOML content against the derived schema.
/// Returns a list of errors: (`dotted_path`, `option_suggestion`)
pub fn validate_toml_content(
    content: &str,
    schema: &HashMap<String, HashSet<String>>,
) -> Vec<(String, Option<String>)> {
    autumn_web::config::AutumnConfig::validate_toml(content, schema)
}

fn find_all_profile_files() -> Vec<(String, std::path::PathBuf)> {
    let mut files = Vec::new();
    if let Ok(entries) = std::fs::read_dir(".") {
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if let Some(filename) = path.file_name().and_then(|n| n.to_str()) {
                let is_profile = path.is_file()
                    && filename.starts_with("autumn-")
                    && std::path::Path::new(filename)
                        .extension()
                        .is_some_and(|ext| ext.eq_ignore_ascii_case("toml"));
                if is_profile {
                    let profile =
                        filename["autumn-".len()..filename.len() - ".toml".len()].to_string();
                    files.push((profile, path));
                }
            }
        }
    }
    files
}

/// Check that `autumn.toml` content parses cleanly (pure, injectable for tests).
pub fn check_toml_content(content: &str) -> CheckResult {
    if let Err(e) = toml::from_str::<toml::Table>(content) {
        CheckResult {
            name: "autumn_toml",
            status: CheckStatus::Fail,
            detail: Some(e.to_string()),
            hint: Some("Fix the syntax error in autumn.toml"),
        }
    } else {
        let schema = autumn_web::config::AutumnConfig::get_schema_keys();
        let mut errors = Vec::new();

        // 1. Validate base autumn.toml
        let base_errors = validate_toml_content(content, &schema);
        for (path, sug) in base_errors {
            errors.push(format!(
                "autumn.toml: unknown key \"{path}\"{}",
                sug.map(|s| format!(" — did you mean \"{s}\"?"))
                    .unwrap_or_default()
            ));
        }

        // 2. Validate any autumn-*.toml profile files
        let profile_files = find_all_profile_files();
        for (_, path) in profile_files {
            if let Ok(file_content) = std::fs::read_to_string(&path) {
                let profile_errors = validate_toml_content(&file_content, &schema);
                let filename = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("profile config");
                for (path, sug) in profile_errors {
                    errors.push(format!(
                        "{filename}: unknown key \"{path}\"{}",
                        sug.map(|s| format!(" — did you mean \"{s}\"?"))
                            .unwrap_or_default()
                    ));
                }
            }
        }

        if errors.is_empty() {
            CheckResult {
                name: "autumn_toml",
                status: CheckStatus::Pass,
                detail: Some("autumn.toml and profile configurations are valid".into()),
                hint: None,
            }
        } else {
            CheckResult {
                name: "autumn_toml",
                status: CheckStatus::Warn,
                detail: Some(errors.join(", ")),
                hint: Some("Remove or rename unrecognised keys in configuration files"),
            }
        }
    }
}

/// Compare CLI version against the project's `autumn-web` version (pure, injectable for tests).
///
/// For semver < 1.0 (`0.MINOR.PATCH`), a minor-version mismatch is treated as
/// a breaking incompatibility (Fail); a patch-only mismatch is a warning.
pub fn check_version_compat(cli_version: &str, web_version: &str) -> CheckResult {
    let parse = |v: &str| -> Option<(u64, u64, u64)> {
        // Strip leading `=`, `^`, `~` requirement operators if present.
        let v = v.trim().trim_start_matches(['=', '^', '~', ' ']);
        let parts: Vec<&str> = v.split('.').collect();
        if parts.len() < 2 {
            return None;
        }
        let major: u64 = parts[0].parse().ok()?;
        let minor: u64 = parts[1].parse().ok()?;
        let patch: u64 = if parts.len() >= 3 {
            parts[2]
                .split(|c: char| !c.is_ascii_digit())
                .next()
                .unwrap_or("0")
                .parse()
                .unwrap_or(0)
        } else {
            0
        };
        Some((major, minor, patch))
    };

    let Some(cli) = parse(cli_version) else {
        return CheckResult {
            name: "version_compat",
            status: CheckStatus::Warn,
            detail: Some(format!("cannot parse CLI version: {cli_version}")),
            hint: Some("Reinstall autumn-cli"),
        };
    };
    let Some(web) = parse(web_version) else {
        return CheckResult {
            name: "version_compat",
            status: CheckStatus::Warn,
            detail: Some(format!("cannot parse autumn-web version: {web_version}")),
            hint: Some("Check autumn-web version in Cargo.toml"),
        };
    };

    if cli.0 != web.0 || cli.1 != web.1 {
        CheckResult {
            name: "version_compat",
            status: CheckStatus::Fail,
            detail: Some(format!(
                "autumn-cli {cli_version} is incompatible with autumn-web {web_version}"
            )),
            hint: Some(
                "Run `cargo install --path autumn-cli` to match your project's autumn-web version",
            ),
        }
    } else if cli.2 != web.2 {
        CheckResult {
            name: "version_compat",
            status: CheckStatus::Warn,
            detail: Some(format!(
                "autumn-cli {cli_version} vs autumn-web {web_version} (patch skew)"
            )),
            hint: Some("Consider updating either the CLI or your project's autumn-web dependency"),
        }
    } else {
        CheckResult {
            name: "version_compat",
            status: CheckStatus::Pass,
            detail: Some(format!(
                "autumn-cli {cli_version} matches autumn-web {web_version}"
            )),
            hint: None,
        }
    }
}

/// Check whether a port is bindable using an injectable binding function.
pub fn check_port_bindable_impl(port: u16, try_bind: impl Fn(u16) -> bool) -> CheckResult {
    if try_bind(port) {
        CheckResult {
            name: "port_bindable",
            status: CheckStatus::Pass,
            detail: Some(format!("port {port} is available")),
            hint: None,
        }
    } else {
        CheckResult {
            name: "port_bindable",
            status: CheckStatus::Fail,
            detail: Some(format!("port {port} is already in use")),
            hint: Some("Kill the process using that port, or change server.port in autumn.toml"),
        }
    }
}

/// Compare version strings for the Rust toolchain check (pure, injectable for tests).
pub fn check_rust_toolchain_impl(current_output: &str, required: &str) -> CheckResult {
    let parse_ver = |s: &str| -> Option<(u64, u64, u64)> {
        let s = s.trim();
        let s = s
            .strip_prefix("rustc ")
            .map_or(s, |rest| rest.split_whitespace().next().unwrap_or(rest));
        let parts: Vec<&str> = s.split('.').collect();
        if parts.len() < 3 {
            return None;
        }
        let major: u64 = parts[0].parse().ok()?;
        let minor: u64 = parts[1].parse().ok()?;
        let patch: u64 = parts[2]
            .split(|c: char| !c.is_ascii_digit())
            .next()
            .unwrap_or("0")
            .parse()
            .unwrap_or(0);
        Some((major, minor, patch))
    };

    let Some(cur) = parse_ver(current_output) else {
        return CheckResult {
            name: "rust_toolchain",
            status: CheckStatus::Warn,
            detail: Some(format!("cannot parse rustc version: {current_output}")),
            hint: Some("Run `rustup update` to ensure a known Rust version"),
        };
    };
    let Some(req) = parse_ver(required) else {
        return CheckResult {
            name: "rust_toolchain",
            status: CheckStatus::Warn,
            detail: Some(format!("cannot parse MSRV: {required}")),
            hint: None,
        };
    };

    if cur >= req {
        CheckResult {
            name: "rust_toolchain",
            status: CheckStatus::Pass,
            detail: Some(format!(
                "rustc {}.{}.{} ≥ MSRV {}.{}.{}",
                cur.0, cur.1, cur.2, req.0, req.1, req.2
            )),
            hint: None,
        }
    } else {
        CheckResult {
            name: "rust_toolchain",
            status: CheckStatus::Fail,
            detail: Some(format!(
                "rustc {}.{}.{} < MSRV {}.{}.{}",
                cur.0, cur.1, cur.2, req.0, req.1, req.2
            )),
            hint: Some("Run `rustup update stable` to upgrade your Rust toolchain"),
        }
    }
}

// ─── IO-dependent checks ──────────────────────────────────────────────────────

fn check_rust_toolchain(msrv: &str) -> CheckResult {
    match std::process::Command::new("rustc")
        .arg("--version")
        .output()
    {
        Ok(out) if out.status.success() => {
            let ver = String::from_utf8_lossy(&out.stdout).into_owned();
            check_rust_toolchain_impl(ver.trim(), msrv)
        }
        _ => CheckResult {
            name: "rust_toolchain",
            status: CheckStatus::Fail,
            detail: Some("`rustc --version` failed".into()),
            hint: Some("Install Rust via https://rustup.rs/"),
        },
    }
}

fn check_port_bindable(port: u16) -> CheckResult {
    check_port_bindable_impl(port, |p| {
        std::net::TcpListener::bind(("127.0.0.1", p)).is_ok()
    })
}

/// Check whether the Tailwind binary is present and executable without launching it.
pub fn check_tailwind_binary_at(path: &std::path::Path) -> CheckResult {
    check_tailwind_binary_at_with_executable_probe(path, tailwind_file_is_executable)
}

fn check_tailwind_binary_at_with_executable_probe(
    path: &std::path::Path,
    executable_probe: impl Fn(&std::path::Path, &std::fs::Metadata) -> bool,
) -> CheckResult {
    let symlink_metadata = match path.symlink_metadata() {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return CheckResult {
                name: "tailwind_binary",
                status: CheckStatus::Fail,
                detail: Some(format!("{} not found", path.display())),
                hint: Some("Run `autumn setup` to download the Tailwind CSS binary"),
            };
        }
        Err(e) => {
            return CheckResult {
                name: "tailwind_binary",
                status: CheckStatus::Fail,
                detail: Some(format!("cannot inspect {}: {e}", path.display())),
                hint: Some("Run `autumn setup --force` to re-download the Tailwind CSS binary"),
            };
        }
    };

    let metadata = match path.metadata() {
        Ok(metadata) => metadata,
        Err(_) if symlink_metadata.file_type().is_symlink() => {
            return CheckResult {
                name: "tailwind_binary",
                status: CheckStatus::Fail,
                detail: Some(format!("{} is a broken symlink", path.display())),
                hint: Some("Run `autumn setup --force` to re-download the Tailwind CSS binary"),
            };
        }
        Err(e) => {
            return CheckResult {
                name: "tailwind_binary",
                status: CheckStatus::Fail,
                detail: Some(format!("cannot inspect {}: {e}", path.display())),
                hint: Some("Run `autumn setup --force` to re-download the Tailwind CSS binary"),
            };
        }
    };

    if !metadata.is_file() {
        let kind = if metadata.is_dir() {
            "directory"
        } else {
            "non-file filesystem entry"
        };
        return CheckResult {
            name: "tailwind_binary",
            status: CheckStatus::Fail,
            detail: Some(format!("{} is a {kind}, not a file", path.display())),
            hint: Some("Run `autumn setup --force` to re-download the Tailwind CSS binary"),
        };
    }

    if !executable_probe(path, &metadata) {
        return CheckResult {
            name: "tailwind_binary",
            status: CheckStatus::Fail,
            detail: Some(format!("{} exists but is not executable", path.display())),
            hint: Some("Run `autumn setup --force` to re-download the Tailwind CSS binary"),
        };
    }

    CheckResult {
        name: "tailwind_binary",
        status: CheckStatus::Pass,
        detail: Some(format!("{} is present and executable", path.display())),
        hint: None,
    }
}

#[cfg(unix)]
fn tailwind_file_is_executable(path: &std::path::Path, _metadata: &std::fs::Metadata) -> bool {
    nix::unistd::access(path, nix::unistd::AccessFlags::X_OK).is_ok()
}

#[cfg(windows)]
fn tailwind_file_is_executable(path: &std::path::Path, _metadata: &std::fs::Metadata) -> bool {
    use std::io::{Read as _, Seek as _};

    if !path
        .extension()
        .and_then(std::ffi::OsStr::to_str)
        .is_some_and(|extension| extension.eq_ignore_ascii_case("exe"))
    {
        return false;
    }

    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let mut dos_header = [0_u8; 64];
    if file.read_exact(&mut dos_header).is_err() || &dos_header[0..2] != b"MZ" {
        return false;
    }

    let pe_offset = u32::from_le_bytes([
        dos_header[0x3c],
        dos_header[0x3d],
        dos_header[0x3e],
        dos_header[0x3f],
    ]);
    if file
        .seek(std::io::SeekFrom::Start(u64::from(pe_offset)))
        .is_err()
    {
        return false;
    }

    let mut pe_signature = [0_u8; 4];
    file.read_exact(&mut pe_signature).is_ok() && pe_signature == *b"PE\0\0"
}

#[cfg(not(any(unix, windows)))]
fn tailwind_file_is_executable(_path: &std::path::Path, _metadata: &std::fs::Metadata) -> bool {
    true
}

fn check_tailwind_binary() -> CheckResult {
    let path = if cfg!(windows) {
        std::path::PathBuf::from("target/autumn/tailwindcss.exe")
    } else {
        std::path::PathBuf::from("target/autumn/tailwindcss")
    };

    check_tailwind_binary_at(&path)
}

fn check_stale_artifacts() -> CheckResult {
    let cargo_lock = std::path::Path::new("Cargo.lock");
    let dist = std::path::Path::new("dist");
    let target = std::path::Path::new("target");

    let lock_mtime = cargo_lock.metadata().and_then(|m| m.modified()).ok();

    let dir_older_than_lock = |dir: &std::path::Path| -> bool {
        let Some(lock_t) = lock_mtime else {
            return false;
        };
        dir.metadata()
            .and_then(|m| m.modified())
            .is_ok_and(|dir_t| dir_t < lock_t)
    };

    let dist_stale = dist.exists() && dir_older_than_lock(dist);
    let target_stale = target.exists() && dir_older_than_lock(target);

    if dist_stale || target_stale {
        let which: Vec<&str> = [
            dist_stale.then_some("dist/"),
            target_stale.then_some("target/"),
        ]
        .into_iter()
        .flatten()
        .collect();
        CheckResult {
            name: "stale_artifacts",
            status: CheckStatus::Warn,
            detail: Some(format!(
                "{} may be stale relative to Cargo.lock",
                which.join(", ")
            )),
            hint: Some("Run `cargo build` or `autumn build` to refresh artifacts"),
        }
    } else {
        CheckResult {
            name: "stale_artifacts",
            status: CheckStatus::Pass,
            detail: Some("artifacts look fresh".into()),
            hint: None,
        }
    }
}

/// Read the `rust-version` MSRV from the nearest workspace/package `Cargo.toml`.
fn read_msrv() -> Option<String> {
    let content = std::fs::read_to_string("Cargo.toml").ok()?;
    let table: toml::Table = toml::from_str(&content).ok()?;

    // Workspace: [workspace.package] rust-version
    if let Some(ver) = table
        .get("workspace")
        .and_then(|w| w.get("package"))
        .and_then(|p| p.get("rust-version"))
        .and_then(|v| v.as_str())
    {
        return Some(ver.to_owned());
    }

    // Plain package: [package] rust-version
    table
        .get("package")
        .and_then(|p| p.get("rust-version"))
        .and_then(|v| v.as_str())
        .map(std::borrow::ToOwned::to_owned)
}

/// Read the `autumn-web` version requirement from the project's `Cargo.toml`.
fn read_autumn_web_version() -> Option<String> {
    let content = std::fs::read_to_string("Cargo.toml").ok()?;
    let table: toml::Table = toml::from_str(&content).ok()?;

    let find_in_deps = |deps: &toml::Value| -> Option<String> {
        let entry = deps.get("autumn-web")?;
        match entry {
            toml::Value::String(v) => Some(v.clone()),
            toml::Value::Table(t) => t
                .get("version")?
                .as_str()
                .map(std::borrow::ToOwned::to_owned),
            _ => None,
        }
    };

    // [dependencies] then [workspace.dependencies]
    table
        .get("dependencies")
        .and_then(find_in_deps)
        .or_else(|| {
            table
                .get("workspace")
                .and_then(|w| w.get("dependencies"))
                .and_then(find_in_deps)
        })
}

/// Try to TCP-connect to a host:port within a short timeout.
fn tcp_reachable(host: &str, port: u16) -> bool {
    use std::net::ToSocketAddrs;
    use std::time::Duration;
    let addrs: Vec<_> = match format!("{host}:{port}").to_socket_addrs() {
        Ok(a) => a.collect(),
        Err(_) => return false,
    };
    addrs
        .iter()
        .any(|addr| std::net::TcpStream::connect_timeout(addr, Duration::from_secs(1)).is_ok())
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DoctorReplicaFallback {
    #[default]
    FailReadiness,
    Primary,
}

impl DoctorReplicaFallback {
    fn from_config_value(value: Option<&str>) -> Self {
        match value.unwrap_or("fail_readiness") {
            "primary" | "fallback_to_primary" | "fallback-to-primary" => Self::Primary,
            _ => Self::FailReadiness,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct DoctorDatabaseTopology {
    pub primary_url: Option<String>,
    pub replica_url: Option<String>,
    pub auto_migrate_in_production: bool,
    pub replica_fallback: DoctorReplicaFallback,
}

fn check_database_topology_contract(
    topology: &DoctorDatabaseTopology,
    is_production: bool,
) -> CheckResult {
    if topology.replica_url.is_some() && topology.primary_url.is_none() {
        return CheckResult {
            name: "database_topology",
            status: CheckStatus::Fail,
            detail: Some("replica role configured without a primary/write role".into()),
            hint: Some("Set database.primary_url or remove database.replica_url"),
        };
    }

    if is_production && topology.replica_url.is_some() && topology.auto_migrate_in_production {
        return CheckResult {
            name: "database_topology",
            status: CheckStatus::Fail,
            detail: Some(
                "unsafe migration ownership: production primary/replica topology cannot run migrations from every web replica"
                    .into(),
            ),
            hint: Some("Run `autumn migrate` as one primary-role job before starting web replicas"),
        };
    }

    if topology.primary_url.is_some() && topology.replica_url.is_some() {
        CheckResult {
            name: "database_topology",
            status: CheckStatus::Pass,
            detail: Some("primary and replica database roles configured".into()),
            hint: None,
        }
    } else if topology.primary_url.is_some() {
        CheckResult {
            name: "database_topology",
            status: CheckStatus::Pass,
            detail: Some("single primary database role configured".into()),
            hint: None,
        }
    } else {
        CheckResult {
            name: "database_topology",
            status: CheckStatus::Pass,
            detail: Some("database not configured".into()),
            hint: None,
        }
    }
}

fn check_replica_migration_versions(
    primary_latest: Option<&str>,
    replica_latest: Option<&str>,
    fallback: DoctorReplicaFallback,
) -> CheckResult {
    match (primary_latest, replica_latest) {
        (Some(primary), Some(replica)) if primary == replica => CheckResult {
            name: "replica_migrations",
            status: CheckStatus::Pass,
            detail: Some(format!("replica replayed latest migration {primary}")),
            hint: None,
        },
        (Some(primary), Some(replica)) => {
            let detail = format!(
                "replica migration version {replica} is behind primary migration version {primary}"
            );
            match fallback {
                DoctorReplicaFallback::FailReadiness => CheckResult {
                    name: "replica_migrations",
                    status: CheckStatus::Fail,
                    detail: Some(detail),
                    hint: Some(
                        "Wait for the replica to replay the latest migration before admitting traffic",
                    ),
                },
                DoctorReplicaFallback::Primary => CheckResult {
                    name: "replica_migrations",
                    status: CheckStatus::Warn,
                    detail: Some(format!("{detail}; reads may fall back to primary")),
                    hint: Some(
                        "Restore replica replay or set replica_fallback = \"fail_readiness\" for stricter rollout gates",
                    ),
                },
            }
        }
        _ => CheckResult {
            name: "replica_migrations",
            status: CheckStatus::Warn,
            detail: Some("could not determine primary and replica migration versions".into()),
            hint: Some("Ensure both roles expose __diesel_schema_migrations"),
        },
    }
}

fn check_db_role_connectivity(
    role: &'static str,
    database_url: &str,
    reachable: impl Fn(&str, u16) -> bool,
) -> CheckResult {
    match parse_db_host_port(database_url) {
        None => CheckResult {
            name: "db_connectivity",
            status: CheckStatus::Warn,
            detail: Some(format!(
                "cannot parse {role} database URL to extract host:port"
            )),
            hint: Some("Ensure database URLs in autumn.toml use postgres:// or postgresql://"),
        },
        Some((host, port)) if reachable(&host, port) => CheckResult {
            name: "db_connectivity",
            status: CheckStatus::Pass,
            detail: Some(format!("{role} database reachable at {host}:{port}")),
            hint: None,
        },
        Some((host, port)) => CheckResult {
            name: "db_connectivity",
            status: CheckStatus::Fail,
            detail: Some(format!(
                "cannot connect to {role} database at {host}:{port}"
            )),
            hint: Some("Start Postgres and verify the configured database role URL"),
        },
    }
}

fn check_db_connectivity(database_url: &str) -> CheckResult {
    check_db_role_connectivity("primary", database_url, tcp_reachable)
}

fn check_pending_migrations(database_url: &str) -> CheckResult {
    match std::process::Command::new("diesel")
        .args(["migration", "pending"])
        .env("DATABASE_URL", database_url)
        .output()
    {
        Err(_) => CheckResult {
            name: "pending_migrations",
            status: CheckStatus::Warn,
            detail: Some("diesel CLI not found; cannot check pending migrations".into()),
            hint: Some(
                "Install diesel_cli: `cargo install diesel_cli --no-default-features --features postgres`",
            ),
        },
        Ok(out) if out.status.success() => {
            let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
            let pending = stdout.lines().filter(|l| !l.trim().is_empty()).count();
            if pending == 0 {
                CheckResult {
                    name: "pending_migrations",
                    status: CheckStatus::Pass,
                    detail: Some("no pending migrations".into()),
                    hint: None,
                }
            } else {
                CheckResult {
                    name: "pending_migrations",
                    status: CheckStatus::Warn,
                    detail: Some(format!("{pending} pending migration(s)")),
                    hint: Some("Run `autumn migrate` to apply pending migrations"),
                }
            }
        }
        Ok(_) => CheckResult {
            name: "pending_migrations",
            status: CheckStatus::Warn,
            detail: Some("diesel migration pending returned non-zero".into()),
            hint: Some("Run `autumn migrate` to apply pending migrations"),
        },
    }
}

fn check_replica_migrations(
    primary_url: &str,
    replica_url: &str,
    fallback: DoctorReplicaFallback,
) -> CheckResult {
    let primary_latest = latest_applied_migration_version(primary_url);
    let replica_latest = latest_applied_migration_version(replica_url);
    check_replica_migration_versions(
        primary_latest.as_deref(),
        replica_latest.as_deref(),
        fallback,
    )
}

fn latest_applied_migration_version(database_url: &str) -> Option<String> {
    let output = std::process::Command::new("diesel")
        .args(["migration", "list"])
        .env("DATABASE_URL", database_url)
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    parse_latest_applied_migration_version(&String::from_utf8_lossy(&output.stdout))
}

fn parse_latest_applied_migration_version(output: &str) -> Option<String> {
    output
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            let version = trimmed
                .strip_prefix("[X]")
                .or_else(|| trimmed.strip_prefix("[x]"))?
                .split_whitespace()
                .next()?;
            Some(version.to_owned())
        })
        .max()
}

/// Parse (host, port) from a Postgres connection URL.
pub fn parse_db_host_port(url: &str) -> Option<(String, u16)> {
    // Expect: postgres://[user:pass@]host[:port]/db
    let without_scheme = url
        .strip_prefix("postgres://")
        .or_else(|| url.strip_prefix("postgresql://"))?;

    // Drop everything after the first `/` (the database name).
    let authority = without_scheme.split('/').next()?;

    // Drop user:pass@ prefix if present.
    let host_port = authority
        .rfind('@')
        .map_or(authority, |at| &authority[at + 1..]);

    if let Some(rest) = host_port.strip_prefix('[') {
        let close = rest.find(']')?;
        let host = &rest[..close];
        let after_bracket = &rest[close + 1..];
        let port = if let Some(port) = after_bracket.strip_prefix(':') {
            port.parse().ok()?
        } else if after_bracket.is_empty() {
            5432
        } else {
            return None;
        };
        return Some((host.to_owned(), port));
    }

    match host_port.matches(':').count() {
        1 => {
            let (host, port) = host_port.split_once(':')?;
            Some((host.to_owned(), port.parse().ok()?))
        }
        _ => Some((host_port.to_owned(), 5432)),
    }
}

/// Resolve the configured HTTP port from `autumn.toml` (fallback: 3000).
fn resolve_server_port() -> u16 {
    std::fs::read_to_string("autumn.toml")
        .ok()
        .and_then(|c| toml::from_str::<toml::Table>(&c).ok())
        .and_then(|t| {
            t.get("server")
                .and_then(|s| s.get("port"))
                .and_then(toml::Value::as_integer)
                .and_then(|p| u16::try_from(p).ok())
        })
        .unwrap_or(3000)
}

fn read_autumn_toml_table() -> Option<toml::Table> {
    std::fs::read_to_string("autumn.toml")
        .ok()
        .and_then(|contents| toml::from_str::<toml::Table>(&contents).ok())
}

fn deep_merge(target: &mut toml::Table, source: &toml::Table) {
    for (k, v) in source {
        if let (Some(source_tbl), Some(target_tbl)) = (
            v.as_table(),
            target.get_mut(k).and_then(|t| t.as_table_mut()),
        ) {
            deep_merge(target_tbl, source_tbl);
            continue;
        }
        target.insert(k.clone(), v.clone());
    }
}

fn get_merged_toml_table(profile: &str) -> toml::Table {
    get_merged_toml_table_profiles(&[profile])
}

/// Merges config from all given profile names in order (last wins).
///
/// Each profile name contributes `[profile.{name}]` from `autumn.toml` and
/// `autumn-{name}.toml`. The base `autumn.toml` top-level is applied once
/// before the per-profile layers.
fn get_merged_toml_table_profiles(profiles: &[&str]) -> toml::Table {
    let mut merged = toml::Table::new();

    let base_toml = std::fs::read_to_string("autumn.toml")
        .ok()
        .and_then(|c| toml::from_str::<toml::Table>(&c).ok());

    // 1. Base autumn.toml top-level (applied once)
    if let Some(ref table) = base_toml {
        deep_merge(&mut merged, table);
    }

    for &profile in profiles {
        // 2. Base autumn.toml [profile.{name}]
        if let Some(prof) = base_toml
            .as_ref()
            .and_then(|t| t.get("profile"))
            .and_then(toml::Value::as_table)
            .and_then(|p| p.get(profile))
            .and_then(toml::Value::as_table)
        {
            deep_merge(&mut merged, prof);
        }

        // 3. autumn-{name}.toml
        if let Some(table) = std::fs::read_to_string(format!("autumn-{profile}.toml"))
            .ok()
            .and_then(|c| toml::from_str::<toml::Table>(&c).ok())
        {
            deep_merge(&mut merged, &table);
        }
    }

    merged
}

/// Inline `[profile.{name}]` lookup order, mirroring the runtime config loader's
/// `profile_lookup_names`: the canonical spelling is applied **last** so it wins
/// (`production` then `prod`; `development` then `dev`).
fn inline_profile_lookup_names(profile: &str) -> Vec<&str> {
    match profile {
        "prod" => vec!["production", "prod"],
        "dev" => vec!["development", "dev"],
        other => vec![other],
    }
}

/// Ordered `autumn-{name}.toml` override-file lookup, mirroring the runtime's
/// `profile_override_file_lookup_names`: only the **first existing** file is
/// loaded, preferring the spelling the operator actually selected.
fn override_file_lookup_names(profile: &str, selected_input: &str) -> Vec<String> {
    match profile {
        "prod" if selected_input.eq_ignore_ascii_case("production") => {
            vec!["production".to_owned(), "prod".to_owned()]
        }
        "prod" => vec!["prod".to_owned(), "production".to_owned()],
        "dev" if selected_input.eq_ignore_ascii_case("development") => {
            vec!["development".to_owned(), "dev".to_owned()]
        }
        "dev" => vec!["dev".to_owned(), "development".to_owned()],
        other => vec![other.to_owned()],
    }
}

/// Build the merged TOML the same way the runtime config loader layers profile
/// sources, so doctor evaluates exactly what the app will boot with. Mirrors
/// `autumn_web::config`: inline `[profile.{name}]` sections are applied in alias
/// order (canonical spelling wins), and exactly one `autumn-{name}.toml` file is
/// loaded — the first that exists, preferring the selected spelling.
fn get_merged_toml_table_runtime(normalized_profile: &str, selected_input: &str) -> toml::Table {
    let mut merged = toml::Table::new();

    let base_toml = std::fs::read_to_string("autumn.toml")
        .ok()
        .and_then(|c| toml::from_str::<toml::Table>(&c).ok());

    // 1. Base autumn.toml top-level.
    if let Some(ref table) = base_toml {
        deep_merge(&mut merged, table);
    }

    // 2. Inline [profile.{name}] sections, canonical spelling applied last (wins).
    for profile_name in inline_profile_lookup_names(normalized_profile) {
        if let Some(prof) = base_toml
            .as_ref()
            .and_then(|t| t.get("profile"))
            .and_then(toml::Value::as_table)
            .and_then(|p| p.get(profile_name))
            .and_then(toml::Value::as_table)
        {
            deep_merge(&mut merged, prof);
        }
    }

    // 3. Exactly one autumn-{name}.toml override file (first existing).
    for profile_name in override_file_lookup_names(normalized_profile, selected_input) {
        if let Some(table) = std::fs::read_to_string(format!("autumn-{profile_name}.toml"))
            .ok()
            .and_then(|c| toml::from_str::<toml::Table>(&c).ok())
        {
            deep_merge(&mut merged, &table);
            break;
        }
    }

    merged
}

pub struct DoctorOAuth2Provider {
    pub name: String,
    pub client_id: String,
    pub client_secret: String,
}

fn resolve_oauth2_providers() -> Vec<DoctorOAuth2Provider> {
    let raw_profile = std::env::var("AUTUMN_ENV")
        .or_else(|_| std::env::var("AUTUMN_PROFILE"))
        .unwrap_or_else(|_| "dev".to_owned());
    let profile = match raw_profile.trim().to_lowercase().as_str() {
        "production" => "prod".to_owned(),
        "development" => "dev".to_owned(),
        other => other.to_owned(),
    };
    let merged_toml = get_merged_toml_table(&profile);
    resolve_oauth2_providers_from_sources(
        |key| std::env::var(key).ok().filter(|v| !v.is_empty()),
        Some(&merged_toml),
        &profile,
    )
}

fn resolve_oauth2_providers_from_sources<F>(
    env_var: F,
    table: Option<&toml::Table>,
    profile: &str,
) -> Vec<DoctorOAuth2Provider>
where
    F: Fn(&str) -> Option<String>,
{
    // Load credentials if available
    let credentials =
        autumn_web::credentials::load_credentials(profile, std::path::Path::new(".")).ok();

    table
        .and_then(|t| t.get("auth"))
        .and_then(toml::Value::as_table)
        .and_then(|auth| auth.get("oauth2"))
        .and_then(toml::Value::as_table)
        .map(|providers| {
            providers
                .iter()
                .map(|(name, val)| {
                    let mut client_id = val
                        .as_table()
                        .and_then(|t| t.get("client_id"))
                        .and_then(toml::Value::as_str)
                        .unwrap_or("")
                        .to_owned();
                    let mut client_secret = val
                        .as_table()
                        .and_then(|t| t.get("client_secret"))
                        .and_then(toml::Value::as_str)
                        .unwrap_or("")
                        .to_owned();

                    let normalized_name = name
                        .chars()
                        .map(|c| if c.is_alphanumeric() { c } else { '_' })
                        .collect::<String>()
                        .to_lowercase();
                    let upper = normalized_name.to_uppercase();

                    // Get values from env vars
                    let env_id_key = format!("AUTUMN_AUTH__OAUTH2__{upper}__CLIENT_ID");
                    if let Some(id) = env_var(&env_id_key) {
                        client_id = id;
                    }
                    let env_secret_key = format!("AUTUMN_AUTH__OAUTH2__{upper}__CLIENT_SECRET");
                    if let Some(sec) = env_var(&env_secret_key) {
                        client_secret = sec;
                    }

                    // Get values from credentials
                    if let Some(ref creds) = credentials {
                        let id_key = format!("oauth2_{normalized_name}_client_id");
                        if client_id.is_empty() {
                            if let Some(id) = creds.get::<String>(&id_key) {
                                client_id = id;
                            } else if let Some(id) =
                                creds.get::<String>(&format!("oauth2_{name}_client_id"))
                            {
                                client_id = id;
                            }
                        }

                        let secret_key = format!("oauth2_{normalized_name}_client_secret");
                        if client_secret.is_empty() {
                            if let Some(sec) = creds.get::<String>(&secret_key) {
                                client_secret = sec;
                            } else if let Some(sec) =
                                creds.get::<String>(&format!("oauth2_{name}_client_secret"))
                            {
                                client_secret = sec;
                            }
                        }
                    }

                    DoctorOAuth2Provider {
                        name: name.clone(),
                        client_id,
                        client_secret,
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Whether a Rust source file declares the `#[mailer(list_unsubscribe = ...)]`
/// attribute (whitespace-insensitive), as opposed to merely calling the
/// `.list_unsubscribe(...)` builder method or mentioning it in a comment.
pub fn file_declares_list_unsubscribe(content: &str) -> bool {
    // Strip line and block comments so a commented-out attribute (`//` or
    // `/* ... */`) does not trip a false positive.
    let without_comments = strip_rust_comments(content);
    let collapsed: String = without_comments
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    collapsed.contains("mailer(list_unsubscribe")
}

/// Best-effort removal of `//` line and `/* ... */` block comments. Intended for
/// heuristic source scanning, not as a Rust lexer; it does not special-case
/// comment markers inside string or char literals.
fn strip_rust_comments(content: &str) -> String {
    let mut out = String::with_capacity(content.len());
    let mut chars = content.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '/' && chars.peek() == Some(&'/') {
            // Line comment: skip to (and keep) the newline.
            for n in chars.by_ref() {
                if n == '\n' {
                    out.push('\n');
                    break;
                }
            }
        } else if c == '/' && chars.peek() == Some(&'*') {
            // Block comment: skip until the closing `*/`.
            chars.next();
            let mut prev = '\0';
            for n in chars.by_ref() {
                if prev == '*' && n == '/' {
                    break;
                }
                prev = n;
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Detect whether any Rust source in the project (including workspace member
/// crates) declares `#[mailer(list_unsubscribe = "...")]`.
///
/// Walks from the project root, skipping build/VCS/vendor directories, so a
/// mailer in e.g. `crates/marketing/src` is found. Matches the attribute form
/// specifically (whitespace-insensitive) so builder calls or comments do not
/// trip a false positive.
fn detect_list_unsubscribe_usage() -> bool {
    scan_dir_for_list_unsubscribe(std::path::Path::new("."))
}

/// Recursively scan `root` for a source file declaring
/// `#[mailer(list_unsubscribe = "...")]`.
///
/// Skips build/VCS/vendor directories so a `cargo vendor` copy of the framework
/// (whose own tests declare a real list mailer) can't trip a false positive for
/// an application binary that registers none. Also skips `tests/` and `examples/`
/// trees: the runtime inventory only registers mailers compiled into the
/// application binary, so an integration-test or example fixture must not make
/// `autumn doctor --strict` require unsubscribe config for mail that can't be sent.
fn scan_dir_for_list_unsubscribe(root: &std::path::Path) -> bool {
    /// Directories never worth scanning for source.
    const SKIP_DIRS: &[&str] = &[
        "target",
        ".git",
        "node_modules",
        "dist",
        ".github",
        "vendor",
        "tests",
        "examples",
    ];
    let Ok(entries) = std::fs::read_dir(root) else {
        return false;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.is_dir() {
            let skip = path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| SKIP_DIRS.contains(&n));
            if !skip && scan_dir_for_list_unsubscribe(&path) {
                return true;
            }
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs")
            && std::fs::read_to_string(&path)
                .is_ok_and(|content| file_declares_list_unsubscribe(&content))
        {
            return true;
        }
    }
    false
}

/// Resolve `[mail]` unsubscribe destinations, preferring env overrides over the
/// merged toml table (mirroring the runtime's `AUTUMN_MAIL__*` precedence).
fn resolve_mail_unsubscribe(table: Option<&toml::Table>) -> (Option<String>, Option<String>) {
    let mail = table
        .and_then(|t| t.get("mail"))
        .and_then(toml::Value::as_table);
    // Match the runtime's precedence: a *present* env var wins even when blank
    // (it clears the TOML value); only an *absent* env var falls back to TOML.
    let read = |env_key: &str, toml_key: &str| {
        std::env::var(env_key).map_or_else(
            |_| {
                mail.and_then(|m| m.get(toml_key))
                    .and_then(toml::Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(std::borrow::ToOwned::to_owned)
            },
            |value| {
                let trimmed = value.trim();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.to_owned())
                }
            },
        )
    };
    (
        read("AUTUMN_MAIL__UNSUBSCRIBE_BASE_URL", "unsubscribe_base_url"),
        read("AUTUMN_MAIL__UNSUBSCRIBE_MAILTO", "unsubscribe_mailto"),
    )
}

/// Resolve the effective `unsubscribe_token_ttl_days`, mirroring the runtime
/// precedence: `[mail].unsubscribe_token_ttl_days` (or the default of 30) is the
/// base, and a present, *parseable* `AUTUMN_MAIL__UNSUBSCRIBE_TOKEN_TTL_DAYS`
/// overrides it. A present-but-invalid env value (blank or non-integer) is
/// ignored exactly as `apply_mail_env_overrides_with_env` does — it warns and
/// leaves the TOML/default in place — so doctor must not treat it as `0` and
/// block a deploy the app would boot with the effective positive TTL.
fn resolve_unsubscribe_token_ttl_days(table: Option<&toml::Table>) -> i64 {
    resolve_unsubscribe_token_ttl_days_from_sources(
        std::env::var("AUTUMN_MAIL__UNSUBSCRIBE_TOKEN_TTL_DAYS").ok(),
        table,
    )
}

/// True when `[mail].unsubscribe_token_ttl_days` is present in the merged TOML
/// but is not an integer (e.g. a quoted string or a float).
///
/// The runtime deserializes this field as `i64` from the TOML *before* any
/// `AUTUMN_MAIL__UNSUBSCRIBE_TOKEN_TTL_DAYS` override is applied (the override
/// mutates the already-typed value), so a non-integer TOML value fails config
/// loading at boot regardless of the env var. doctor reads the merged config as
/// an untyped table, so it must check the type explicitly instead of silently
/// defaulting (which would greenlight a deploy the app can't start).
fn unsubscribe_ttl_toml_type_invalid(table: Option<&toml::Table>) -> bool {
    table
        .and_then(|t| t.get("mail"))
        .and_then(toml::Value::as_table)
        .and_then(|m| m.get("unsubscribe_token_ttl_days"))
        .is_some_and(|v| v.as_integer().is_none())
}

/// True when `[mail].unsubscribe_base_url` or `[mail].unsubscribe_mailto` is
/// present in the merged TOML but is not a string (e.g. an integer or array).
///
/// The runtime deserializes these as `Option<String>`, so a present non-string
/// value fails config loading before boot. `resolve_mail_unsubscribe` only reads
/// `as_str()` and would otherwise treat such a value as absent, letting doctor
/// report `Pass` for a config the app can't start with — so flag it explicitly,
/// like the TTL type guard.
fn unsubscribe_dest_toml_type_invalid(table: Option<&toml::Table>) -> bool {
    let mail = table
        .and_then(|t| t.get("mail"))
        .and_then(toml::Value::as_table);
    ["unsubscribe_base_url", "unsubscribe_mailto"]
        .iter()
        .any(|key| {
            mail.and_then(|m| m.get(*key))
                .is_some_and(|v| v.as_str().is_none())
        })
}

fn resolve_unsubscribe_token_ttl_days_from_sources(
    env_value: Option<String>,
    table: Option<&toml::Table>,
) -> i64 {
    const DEFAULT_TTL_DAYS: i64 = 30;
    let toml_or_default = table
        .and_then(|t| t.get("mail"))
        .and_then(toml::Value::as_table)
        .and_then(|m| m.get("unsubscribe_token_ttl_days"))
        .and_then(toml::Value::as_integer)
        .unwrap_or(DEFAULT_TTL_DAYS);
    // Match the runtime's `val.parse::<i64>()` (no trim): an unparseable override
    // is ignored, falling back to the TOML/default value.
    if let Some(raw) = env_value
        && let Ok(days) = raw.parse::<i64>()
    {
        return days;
    }
    toml_or_default
}

/// Mirror of `Transport::from_env_value` (mail module, feature-gated so not
/// reachable from the CLI build): trim + lowercase and accept only a known
/// transport, returning its canonical spelling, else `None`.
fn parse_mail_transport(value: &str) -> Option<&'static str> {
    match value.trim().to_ascii_lowercase().as_str() {
        "log" => Some("log"),
        "file" => Some("file"),
        "smtp" => Some("smtp"),
        "disabled" => Some("disabled"),
        _ => None,
    }
}

/// Resolve the effective mail transport the way the runtime does: a *valid* env
/// override wins, then a valid `[mail].transport`, then the profile smart-default
/// (`dev` → `log`, otherwise `disabled`). Invalid/whitespace values are ignored
/// just as `Transport::from_env_value` ignores them, so doctor does not treat a
/// malformed override as an active transport.
fn resolve_effective_mail_transport(
    env_value: Option<String>,
    toml_transport: Option<&str>,
    normalized_profile: &str,
) -> String {
    if let Some(raw) = env_value
        && let Some(parsed) = parse_mail_transport(&raw)
    {
        return parsed.to_owned();
    }
    if let Some(parsed) = toml_transport.and_then(parse_mail_transport) {
        return parsed.to_owned();
    }
    if normalized_profile == "dev" {
        "log".to_owned()
    } else {
        "disabled".to_owned()
    }
}

fn resolve_database_topology(table: Option<&toml::Table>) -> DoctorDatabaseTopology {
    resolve_database_topology_from_sources(
        |key| std::env::var(key).ok().filter(|value| !value.is_empty()),
        table,
    )
}

fn resolve_database_topology_from_sources<F>(
    env_var: F,
    table: Option<&toml::Table>,
) -> DoctorDatabaseTopology
where
    F: Fn(&str) -> Option<String>,
{
    let database = table
        .and_then(|t| t.get("database"))
        .and_then(toml::Value::as_table);

    let primary_url = first_env(
        &env_var,
        &[
            "AUTUMN_DATABASE__PRIMARY_URL",
            "AUTUMN_DATABASE__URL",
            "DATABASE_URL",
        ],
    )
    .or_else(|| first_toml_string(database, &["primary_url", "url"]));

    let replica_url = first_env(&env_var, &["AUTUMN_DATABASE__REPLICA_URL"])
        .or_else(|| first_toml_string(database, &["replica_url"]));

    let auto_migrate_in_production =
        first_env(&env_var, &["AUTUMN_DATABASE__AUTO_MIGRATE_IN_PRODUCTION"])
            .as_deref()
            .and_then(parse_config_bool)
            .or_else(|| database?.get("auto_migrate_in_production")?.as_bool())
            .unwrap_or(false);

    let replica_fallback = DoctorReplicaFallback::from_config_value(
        first_env(&env_var, &["AUTUMN_DATABASE__REPLICA_FALLBACK"])
            .or_else(|| first_toml_string(database, &["replica_fallback"]))
            .as_deref(),
    );

    DoctorDatabaseTopology {
        primary_url,
        replica_url,
        auto_migrate_in_production,
        replica_fallback,
    }
}

fn first_env<F>(env_var: &F, keys: &[&str]) -> Option<String>
where
    F: Fn(&str) -> Option<String>,
{
    keys.iter().find_map(|key| {
        env_var(key)
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
    })
}

fn first_toml_string(database: Option<&toml::Table>, keys: &[&str]) -> Option<String> {
    let database = database?;
    keys.iter().find_map(|key| {
        database
            .get(*key)
            .and_then(toml::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(std::borrow::ToOwned::to_owned)
    })
}

fn parse_config_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// Resolves the active profile names the runtime would load.
///
/// Returns `(canonical, selected_input, profiles)`:
/// - `canonical` — normalized name (`"prod"` / `"dev"` / custom)
/// - `selected_input` — the raw env-var value (used by `override_file_lookup_names`
///   to pick the right `autumn-{name}.toml` spelling when both exist)
/// - `profiles` — `[alias, canonical]` list for inline `[profile.{name}]` merging
fn resolve_active_profiles() -> (String, String, Vec<String>) {
    let raw_profile = std::env::var("AUTUMN_ENV")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| std::env::var("AUTUMN_PROFILE").ok())
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_default();
    let raw_trimmed = raw_profile.trim().to_owned();
    // Mirror normalize_profile_name from config.rs: built-in aliases are
    // matched case-insensitively; custom profile names preserve the original
    // user-specified case (so AUTUMN_ENV=QA loads [profile.QA], not [profile.qa]).
    let canonical = if raw_trimmed.eq_ignore_ascii_case("production")
        || raw_trimmed.eq_ignore_ascii_case("prod")
    {
        "prod".to_owned()
    } else if raw_trimmed.eq_ignore_ascii_case("development")
        || raw_trimmed.eq_ignore_ascii_case("dev")
        || raw_trimmed.is_empty()
    {
        "dev".to_owned()
    } else {
        raw_trimmed.clone()
    };
    // Mirror profile_lookup_names in config.rs: the runtime always loads the
    // legacy long-form alias first (e.g. "production") then the canonical short
    // form ("prod"), regardless of which spelling was used in the env var.
    let alias = match canonical.as_str() {
        "prod" => Some("production".to_owned()),
        "dev" => Some("development".to_owned()),
        _ => None,
    };
    let profiles: Vec<String> = alias
        .into_iter()
        .chain(std::iter::once(canonical.clone()))
        .collect();
    (canonical, raw_trimmed, profiles)
}

fn resolve_proxy_conflict_data() -> ProxyConflictData {
    // Mirror the runtime: load only the first existing override file so a
    // stale autumn-production.toml doesn't shadow autumn-prod.toml at startup.
    let (canonical, selected, _profiles) = resolve_active_profiles();
    let table = get_merged_toml_table_runtime(&canonical, &selected);

    let parse_csv_env = |var: &str| -> Option<Vec<String>> {
        std::env::var(var).ok().map(|v| {
            v.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(std::borrow::ToOwned::to_owned)
                .collect()
        })
    };
    let parse_bool_env = |var: &str| -> Option<bool> {
        std::env::var(var)
            .ok()
            .map(|v| matches!(v.trim(), "true" | "1"))
    };

    let security = table.get("security");
    let tp_section = security.and_then(|s| s.get("trusted_proxies"));
    let rl_section = security.and_then(|s| s.get("rate_limit"));

    let toml_csv = |section: Option<&toml::Value>, key: &str| -> Vec<String> {
        section
            .and_then(|s| s.get(key))
            .and_then(toml::Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(toml::Value::as_str)
                    .map(std::borrow::ToOwned::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    };
    let toml_bool = |section: Option<&toml::Value>, key: &str| -> bool {
        section
            .and_then(|s| s.get(key))
            .and_then(toml::Value::as_bool)
            .unwrap_or(false)
    };

    ProxyConflictData {
        new_ranges: parse_csv_env("AUTUMN_SECURITY__TRUSTED_PROXIES__RANGES")
            .unwrap_or_else(|| toml_csv(tp_section, "ranges")),
        new_trust_fwd: parse_bool_env("AUTUMN_SECURITY__TRUSTED_PROXIES__TRUST_FORWARDED_HEADERS")
            .unwrap_or_else(|| toml_bool(tp_section, "trust_forwarded_headers")),
        new_hops: std::env::var("AUTUMN_SECURITY__TRUSTED_PROXIES__TRUSTED_HOPS")
            .ok()
            .and_then(|v| v.trim().parse::<u32>().ok())
            .or_else(|| {
                tp_section
                    .and_then(|s| s.get("trusted_hops"))
                    .and_then(toml::Value::as_integer)
                    .and_then(|v| u32::try_from(v).ok())
            }),
        old_ranges: parse_csv_env("AUTUMN_SECURITY__RATE_LIMIT__TRUSTED_PROXIES")
            .unwrap_or_else(|| toml_csv(rl_section, "trusted_proxies")),
        old_trust_fwd: parse_bool_env("AUTUMN_SECURITY__RATE_LIMIT__TRUST_FORWARDED_HEADERS")
            .unwrap_or_else(|| toml_bool(rl_section, "trust_forwarded_headers")),
    }
}

/// Resolve which deprecated config keys are currently set in this project.
///
/// Uses [`autumn_web::config::detect_deprecated_keys_for`], which seeds the same
/// profile-default base layer the runtime loader applies before merging the
/// file table — so doctor evaluates the *same* layered config the runtime would
/// load (a key set only in a profile default is still detected).
fn resolve_deprecations() -> Vec<DoctorDeprecation> {
    use autumn_web::config::{OsEnv, deprecated_config_keys, detect_deprecated_keys_for};

    let (canonical, selected, _profiles) = resolve_active_profiles();
    // Use the same single-file lookup path the runtime uses: only the first
    // existing autumn-{profile}.toml is loaded, never both alias spellings.
    let file_table = get_merged_toml_table_runtime(&canonical, &selected);

    detect_deprecated_keys_for(&canonical, &file_table, &OsEnv, deprecated_config_keys())
        .into_iter()
        .map(|f| DoctorDeprecation {
            path: f.path,
            replacement: f.replacement,
            since: f.since,
            remove_in: f.remove_in,
        })
        .collect()
}

/// Resolve the rate-limit key strategy from config/env.
///
/// Priority:
/// 1. `AUTUMN_SECURITY__RATE_LIMIT__KEY_STRATEGY` env var
/// 2. `[security.rate_limit] key_strategy` in `autumn.toml`
fn resolve_rate_limit_key_strategy() -> String {
    if let Ok(val) = std::env::var("AUTUMN_SECURITY__RATE_LIMIT__KEY_STRATEGY")
        && !val.is_empty()
    {
        return val;
    }
    std::fs::read_to_string("autumn.toml")
        .ok()
        .and_then(|c| toml::from_str::<toml::Table>(&c).ok())
        .and_then(|t| {
            t.get("security")
                .and_then(|s| s.get("rate_limit"))
                .and_then(|rl| rl.get("key_strategy"))
                .and_then(toml::Value::as_str)
                .filter(|v| !v.is_empty())
                .map(std::borrow::ToOwned::to_owned)
        })
        .unwrap_or_default()
}

/// Detect whether an auth extractor is mounted (i.e. `[auth]` section exists
/// and is not explicitly disabled).
fn resolve_auth_extractor_mounted() -> bool {
    if let Ok(val) = std::env::var("AUTUMN_AUTH__ENABLED") {
        return !val.trim().eq_ignore_ascii_case("false");
    }
    std::fs::read_to_string("autumn.toml")
        .ok()
        .and_then(|c| toml::from_str::<toml::Table>(&c).ok())
        .is_some_and(|t| {
            // [auth] section present and not explicitly disabled.
            t.get("auth").is_some_and(|auth| {
                auth.get("enabled")
                    .and_then(toml::Value::as_bool)
                    .unwrap_or(true) // enabled by default when section exists
            })
        })
}

fn resolve_optional_signing_secret() -> Option<String> {
    if let Ok(val) = std::env::var("AUTUMN_SECURITY__SIGNING_SECRET")
        && !val.is_empty()
    {
        return Some(val);
    }
    std::fs::read_to_string("autumn.toml")
        .ok()
        .and_then(|c| toml::from_str::<toml::Table>(&c).ok())
        .and_then(|t| {
            t.get("security")
                .and_then(|s| s.get("signing_secret"))
                .and_then(|ss| ss.get("secret"))
                .and_then(toml::Value::as_str)
                .filter(|v| !v.is_empty())
                .map(std::borrow::ToOwned::to_owned)
        })
}

fn resolve_trusted_hosts() -> Vec<String> {
    if let Ok(val) = std::env::var("AUTUMN_SECURITY__TRUSTED_HOSTS__HOSTS") {
        return val
            .split(',')
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(std::borrow::ToOwned::to_owned)
            .collect();
    }
    let table = read_autumn_toml_table().unwrap_or_default();
    let profile = std::env::var("AUTUMN_ENV")
        .ok()
        .or_else(|| std::env::var("AUTUMN_PROFILE").ok())
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();

    let parse_hosts = |root: &toml::Table| {
        root.get("security")
            .and_then(|s| s.get("trusted_hosts"))
            .and_then(|th| th.get("hosts"))
            .and_then(toml::Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(toml::Value::as_str)
                    .map(str::trim)
                    .filter(|v| !v.is_empty())
                    .map(std::borrow::ToOwned::to_owned)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    };

    if !profile.is_empty()
        && let Some(profile_table) = table
            .get("profile")
            .and_then(|v| v.get(&profile))
            .and_then(toml::Value::as_table)
    {
        let profile_hosts = parse_hosts(profile_table);
        if !profile_hosts.is_empty() {
            return profile_hosts;
        }
    }

    parse_hosts(&table)
}

/// Resolve whether the active profile is production from the environment or
/// `autumn.toml`.
fn resolve_is_production() -> bool {
    for var in ["AUTUMN_ENV", "AUTUMN_PROFILE"] {
        if let Ok(val) = std::env::var(var) {
            let v = val.trim().to_ascii_lowercase();
            if v == "prod" || v == "production" {
                return true;
            }
            if !v.is_empty() {
                return false;
            }
        }
    }
    // Fall back to build-mode detection: release binary implies prod
    false
}

/// Check whether Tailwind is enabled in `autumn.toml`.
///
/// Falls back to a heuristic: if `build.rs` exists the project is assumed to
/// use the Tailwind build pipeline.
fn tailwind_enabled() -> bool {
    std::fs::read_to_string("autumn.toml")
        .ok()
        .and_then(|c| toml::from_str::<toml::Table>(&c).ok())
        .and_then(|t| t.get("tailwind").and_then(toml::Value::as_bool))
        .unwrap_or_else(|| std::path::Path::new("build.rs").exists())
}

fn resolve_compression_enabled() -> bool {
    // 1. Env var takes highest precedence. Use the same accepted values as the
    //    runtime's parse_env_bool ("true"/"1"/"false"/"0") so doctor never
    //    reports a different state than the app would observe.
    if let Ok(val) = std::env::var("AUTUMN_COMPRESSION__ENABLED") {
        match val.as_str() {
            "true" | "1" => return true,
            "false" | "0" => return false,
            _ => {}
        }
    }

    // 2. Read TOML, applying profile-specific override when a profile is active
    //    (mirrors the five-layer config system so `[profile.prod] compression.enabled`
    //    doesn't cause a spurious doctor warning in production).
    let table = read_autumn_toml_table().unwrap_or_default();
    let profile = std::env::var("AUTUMN_ENV")
        .ok()
        .or_else(|| std::env::var("AUTUMN_PROFILE").ok())
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();

    let parse_enabled = |root: &toml::Table| {
        root.get("compression")
            .and_then(|v| v.get("enabled"))
            .and_then(toml::Value::as_bool)
    };

    // Profile-specific section takes precedence over base config.
    if !profile.is_empty()
        && let Some(enabled) = table
            .get("profile")
            .and_then(|v| v.get(&profile))
            .and_then(toml::Value::as_table)
            .and_then(&parse_enabled)
    {
        return enabled;
    }

    parse_enabled(&table).unwrap_or(false)
}

// ─── Main entry point ─────────────────────────────────────────────────────────

/// Run all doctor checks and report results.
///
/// Checks are organised in two phases:
/// 1. **Config phase** (serial, fast) — file/env reads to decide which checks
///    to run and what data to pass them.
/// 2. **Check phase** (parallel) — every applicable check is spawned on its own
///    thread so that slow operations (TCP connect, subprocess calls) overlap.
///    Results are joined back in display order.
#[allow(clippy::too_many_lines)]
pub fn run(opts: DoctorOptions) {
    use std::thread;
    type Task = Box<dyn FnOnce() -> CheckResult + Send>;

    let cli_version = env!("CARGO_PKG_VERSION");

    if !opts.json {
        println!("\u{1F342} autumn doctor\n");
    }

    // ── Phase 1: config reads (serial, cheap) ────────────────────────────────
    let msrv = read_msrv().unwrap_or_else(|| "1.88.0".to_owned());
    let web_ver = read_autumn_web_version();
    let toml_result = std::fs::read_to_string("autumn.toml");
    let toml_table = toml_result
        .as_deref()
        .ok()
        .and_then(|content| toml::from_str::<toml::Table>(content).ok())
        .or_else(read_autumn_toml_table);
    let db_topology = resolve_database_topology(toml_table.as_ref());
    let port = resolve_server_port();
    let tailwind = tailwind_enabled();
    let signing_secret = resolve_optional_signing_secret();
    let trusted_hosts = resolve_trusted_hosts();
    let is_production = resolve_is_production();
    let rate_limit_key_strategy = resolve_rate_limit_key_strategy();
    let auth_extractor_mounted = resolve_auth_extractor_mounted();
    let compression_enabled = resolve_compression_enabled();
    let proxy_conflict_data = resolve_proxy_conflict_data();
    let (dotenv_has_example, dotenv_has_env, dotenv_uncovered) = resolve_dotenv_state();

    // ── Phase 2: build tasks in display order ────────────────────────────────
    let mut tasks: Vec<Task> = Vec::new();

    // 1. Rust toolchain
    tasks.push(Box::new(move || check_rust_toolchain(&msrv)));

    // 2. Version skew (only when autumn-web appears in the project's Cargo.toml)
    if let Some(web) = web_ver {
        tasks.push(Box::new(move || check_version_compat(cli_version, &web)));
    }

    // 3. autumn.toml
    match toml_result {
        Ok(content) => tasks.push(Box::new(move || check_toml_content(&content))),
        Err(_) => tasks.push(Box::new(|| CheckResult {
            name: "autumn_toml",
            status: CheckStatus::Warn,
            detail: Some("autumn.toml not found in current directory".into()),
            hint: Some("Run `autumn doctor` from your project root (where autumn.toml lives)"),
        })),
    }

    // 4+. Database topology and role-specific checks.
    let topology_for_check = db_topology.clone();
    tasks.push(Box::new(move || {
        check_database_topology_contract(&topology_for_check, is_production)
    }));

    if let Some(url) = db_topology.primary_url.clone() {
        let primary_for_pending = url.clone();
        tasks.push(Box::new(move || check_db_connectivity(&url)));
        tasks.push(Box::new(move || {
            check_pending_migrations(&primary_for_pending)
        }));
    }

    if let Some(url) = db_topology.replica_url.clone() {
        tasks.push(Box::new(move || {
            check_db_role_connectivity("replica", &url, tcp_reachable)
        }));
    }

    if let (Some(primary_url), Some(replica_url)) = (
        db_topology.primary_url.clone(),
        db_topology.replica_url.clone(),
    ) {
        let fallback = db_topology.replica_fallback;
        tasks.push(Box::new(move || {
            check_replica_migrations(&primary_url, &replica_url, fallback)
        }));
    }

    // 6. Port bindable
    tasks.push(Box::new(move || check_port_bindable(port)));

    // 7. Tailwind binary (only when build pipeline is present)
    if tailwind {
        tasks.push(Box::new(check_tailwind_binary));
    }

    // 8. Signing-secret readiness (warn in dev, fail in prod when missing/weak)
    tasks.push(Box::new(move || {
        check_signing_secret_impl(signing_secret.as_deref(), is_production)
    }));
    tasks.push(Box::new(move || {
        check_trusted_hosts_impl(&trusted_hosts, is_production)
    }));

    // 8b. Rate-limit key-strategy misconfiguration
    tasks.push(Box::new(move || {
        check_rate_limit_key_strategy_impl(&rate_limit_key_strategy, auth_extractor_mounted)
    }));

    // 8c. Conflicting trusted-proxy configuration (new vs. deprecated fields)
    tasks.push(Box::new(move || {
        check_proxy_conflict_impl(&proxy_conflict_data)
    }));

    // 8d. Deprecated config key usage
    let deprecated_keys = resolve_deprecations();
    tasks.push(Box::new(move || {
        check_deprecated_keys_impl(&deprecated_keys)
    }));

    // 9. OAuth2 provider checks (client_id and client_secret validation)
    let oauth2_providers = resolve_oauth2_providers();
    for p in oauth2_providers {
        tasks.push(Box::new(move || {
            check_oauth2_provider_impl(&p.name, &p.client_id, &p.client_secret, is_production)
        }));
    }

    // 9b. List-Unsubscribe wiring: fail closed in prod when a #[mailer] declares
    // list_unsubscribe but no unsubscribe destination is configured. Layer the
    // profile sources exactly as the runtime config loader does (alias-aware
    // inline precedence with the canonical spelling winning, single override
    // file preferring the selected spelling), so doctor evaluates what the app
    // will actually boot with rather than a stale legacy spelling.
    //
    // Profile selection mirrors `resolve_profile_input`: a blank/whitespace
    // AUTUMN_ENV is ignored before falling back to AUTUMN_PROFILE, so a blank
    // preferred var does not silently downgrade a prod selection to dev.
    let raw_mail_profile = std::env::var("AUTUMN_ENV")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| {
            std::env::var("AUTUMN_PROFILE")
                .ok()
                .filter(|v| !v.trim().is_empty())
        })
        .unwrap_or_else(|| "dev".to_owned());
    let selected_input = raw_mail_profile.trim().to_owned();
    let normalized_profile = match selected_input.to_lowercase().as_str() {
        "prod" | "production" => "prod".to_owned(),
        "dev" | "development" | "" => "dev".to_owned(),
        other => other.to_owned(),
    };
    let merged_mail_toml = get_merged_toml_table_runtime(&normalized_profile, &selected_input);
    let (unsub_base_url, unsub_mailto) = resolve_mail_unsubscribe(Some(&merged_mail_toml));
    // Resolve the effective transport the same way the runtime does: a *valid* env
    // override (trimmed, case-insensitive) wins, then explicit `[mail].transport`,
    // then the profile smart-default (only `dev` defaults to `log`; every other
    // profile defaults to `disabled`). An invalid/whitespace env value is ignored,
    // matching `Transport::from_env_value`.
    let effective_transport = resolve_effective_mail_transport(
        std::env::var("AUTUMN_MAIL__TRANSPORT").ok(),
        merged_mail_toml
            .get("mail")
            .and_then(toml::Value::as_table)
            .and_then(|m| m.get("transport"))
            .and_then(toml::Value::as_str),
        &normalized_profile,
    );
    let mail_transport_disabled = effective_transport == "disabled";
    let unsub_token_ttl_days = resolve_unsubscribe_token_ttl_days(Some(&merged_mail_toml));
    let unsub_ttl_toml_type_invalid = unsubscribe_ttl_toml_type_invalid(Some(&merged_mail_toml));
    let unsub_dest_toml_type_invalid = unsubscribe_dest_toml_type_invalid(Some(&merged_mail_toml));
    let has_list_unsubscribe_usage = detect_list_unsubscribe_usage();
    tasks.push(Box::new(move || {
        // A present-but-non-string destination fails the runtime's typed config
        // load (Option<String>) before boot; doctor reads an untyped table, so a
        // non-string value would otherwise look absent. Flag it like the TTL guard.
        if unsub_dest_toml_type_invalid {
            return CheckResult {
                name: "mail_unsubscribe",
                status: CheckStatus::Fail,
                detail: Some(
                    "mail.unsubscribe_base_url and mail.unsubscribe_mailto must be strings; a \
                     non-string TOML value (e.g. an integer or array) fails configuration loading \
                     before the app can boot"
                        .into(),
                ),
                hint: Some(
                    "Set mail.unsubscribe_base_url / mail.unsubscribe_mailto to quoted string values",
                ),
            };
        }
        // A present-but-non-integer TOML TTL fails the runtime's typed config load
        // before boot; doctor reads an untyped table, so flag it here rather than
        // letting it silently default in `resolve_unsubscribe_token_ttl_days`.
        if unsub_ttl_toml_type_invalid {
            return CheckResult {
                name: "mail_unsubscribe",
                status: CheckStatus::Fail,
                detail: Some(
                    "mail.unsubscribe_token_ttl_days must be an integer number of days; a \
                     non-integer TOML value (e.g. a quoted string or a float) fails configuration \
                     loading before the app can boot"
                        .into(),
                ),
                hint: Some(
                    "Set mail.unsubscribe_token_ttl_days to a bare integer, e.g. unsubscribe_token_ttl_days = 30",
                ),
            };
        }
        check_mail_unsubscribe_config_impl(
            has_list_unsubscribe_usage,
            unsub_base_url.as_deref(),
            unsub_mailto.as_deref(),
            mail_transport_disabled,
            is_production,
            unsub_token_ttl_days,
        )
    }));

    // 10. Stale artifacts (warn only, never fail)
    tasks.push(Box::new(check_stale_artifacts));

    // 11. Maintenance mode state
    tasks.push(Box::new(check_maintenance_mode));

    // 12. Compression (warn in production when disabled)
    tasks.push(Box::new(move || {
        check_compression_impl(compression_enabled, is_production)
    }));

    // 12b. Local-dev `.env` handling (warn on `.env.example` without `.env`,
    //      or a present-but-ungitignored `.env`).
    tasks.push(Box::new(move || {
        check_dotenv_impl(dotenv_has_example, dotenv_has_env, &dotenv_uncovered)
    }));

    // 13. System-test browser (warn if missing; not all projects use system tests)
    tasks.push(Box::new(check_system_test_browser));

    // 14. GDPR export/erasure registration (warn when auth starter is present
    //     but #[repository] models are not registered in GdprRegistry).
    //     File-system work runs inside the task so it overlaps with other checks.
    tasks.push(Box::new(|| {
        let has_auth_starter = std::path::Path::new("src/routes/auth.rs").exists();
        let unregistered = resolve_gdpr_unregistered_tables();
        check_gdpr_export_registration_impl(
            has_auth_starter,
            &unregistered.iter().map(String::as_str).collect::<Vec<_>>(),
        )
    }));

    // 15. Model column privacy (issue #1374): warn when a `#[model]` has a
    //     sensitively-named column (password/token/secret/*_hash) that is not
    //     marked `#[private]`, so it would leak into JSON responses.
    tasks.push(Box::new(|| {
        let found = resolve_unprivate_sensitive_columns();
        check_model_private_columns_impl(&found)
    }));

    // ── Phase 3: spawn all tasks concurrently ────────────────────────────────
    #[allow(clippy::needless_collect)]
    let handles: Vec<thread::JoinHandle<CheckResult>> =
        tasks.into_iter().map(thread::spawn).collect();

    // ── Phase 4: join in order (preserves display ordering) ──────────────────
    let results: Vec<CheckResult> = handles
        .into_iter()
        .map(|h| {
            h.join().unwrap_or_else(|_| CheckResult {
                name: "internal_error",
                status: CheckStatus::Fail,
                detail: Some("a check panicked unexpectedly".into()),
                hint: None,
            })
        })
        .collect();

    let summary = compute_summary(&results);
    let code = exit_code(&summary, opts.strict);

    if opts.json {
        println!("{}", to_json_output(&results, &summary));
    } else {
        for r in &results {
            println!("{}", format_check_line(r));
        }
        println!();
        println!("{}", format_summary_line(&summary, code));
    }

    std::process::exit(code);
}

/// Check whether maintenance mode is currently active.
///
/// Warns (not fails) so `autumn doctor` stays green during planned windows.
/// Check whether response compression is configured for a production profile.
///
/// Compression is off by default (for BREACH/CRIME safety). When running in
/// production without compression this check emits a `Warn` so operators know
/// they may want to enable it (or document the deliberate choice to rely on a
/// CDN / reverse-proxy for compression instead).
pub fn check_compression_impl(compression_enabled: bool, is_production: bool) -> CheckResult {
    if is_production && !compression_enabled {
        return CheckResult {
            name: "compression",
            status: CheckStatus::Warn,
            detail: Some(
                "response compression is disabled in production; \
                 text payloads (HTML/JSON/CSS) are served uncompressed"
                    .into(),
            ),
            hint: Some(
                "Set [compression] enabled = true in autumn.toml (or AUTUMN_COMPRESSION__ENABLED=true) \
                 if you are not using a CDN or reverse-proxy that compresses for you. \
                 Read the BREACH/CRIME tradeoff in docs/guide/compression.md before enabling.",
            ),
        };
    }
    CheckResult {
        name: "compression",
        status: CheckStatus::Pass,
        detail: Some(if compression_enabled {
            "response compression is enabled".into()
        } else {
            "response compression is disabled (off by default; use CDN or enable explicitly)".into()
        }),
        hint: None,
    }
}

/// Resolve `.env` filesystem state for [`check_dotenv_impl`].
///
/// Returns `(has_env_example, has_env, uncovered_secret_files)`, where the last
/// element lists every always-secret dotenv file present in the project root
/// (`.env`, `.env.local`, and any `.env.<profile>.local`) that the project's
/// `.gitignore` does NOT cover. Committable `.env.<profile>` (non-local) files
/// are intentionally excluded from the secret set — they follow the Vite/Next
/// convention of being safe to commit — so doctor never flags them.
fn resolve_dotenv_state() -> (bool, bool, Vec<String>) {
    let has_env_example = std::path::Path::new(".env.example").exists();
    let has_env = std::path::Path::new(".env").exists();
    let gitignore = std::fs::read_to_string(".gitignore").unwrap_or_default();
    let uncovered = present_secret_dotenv_files(std::path::Path::new("."))
        .into_iter()
        .filter(|name| !gitignore_covers(&gitignore, name))
        .collect();
    (has_env_example, has_env, uncovered)
}

/// Enumerate the always-secret dotenv files present in `dir`: the root `.env`,
/// `.env.local`, and any `.env.<profile>.local`. Returned sorted for stable
/// output. Committable `.env.<profile>` (non-local) files are excluded — see
/// [`is_secret_dotenv_filename`].
fn present_secret_dotenv_files(dir: &std::path::Path) -> Vec<String> {
    let mut files = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.filter_map(Result::ok) {
            if !entry.path().is_file() {
                continue;
            }
            if let Some(name) = entry.file_name().to_str()
                && is_secret_dotenv_filename(name)
            {
                files.push(name.to_owned());
            }
        }
    }
    files.sort();
    files
}

/// Whether `name` is an always-secret dotenv file the loader can auto-load:
/// `.env`, `.env.local`, or a `.env.<profile>.local` variant.
///
/// A non-local `.env.<profile>` (e.g. `.env.dev`) is committable by convention
/// and returns `false`, as does the `.env.example` template.
fn is_secret_dotenv_filename(name: &str) -> bool {
    // Everything after the mandatory `.env` prefix.
    let Some(rest) = name.strip_prefix(".env") else {
        return false;
    };
    match rest {
        // Bare `.env` and the root `.env.local` are always secret.
        "" | ".local" => true,
        // `.env.<profile>.local` — a per-profile local override. `rest` here is
        // `.<profile>.local`; require a leading `.`, a `local` final segment,
        // and at least two `.` separators so `<profile>` is a real segment
        // (excludes committable non-local `.env.<profile>` files).
        _ => {
            rest.starts_with('.')
                && rest.rsplit('.').next() == Some("local")
                && rest.matches('.').count() >= 2
        }
    }
}

/// Whether a `.gitignore` body ignores a project-root file named `filename`.
///
/// Git-ignore semantics, restricted to the cases needed for root-level dotenv
/// files: comment (`#…`) and blank lines are skipped; every other line is a
/// pattern matched against the file's basename, where `*` matches any run of
/// non-`/` characters (including empty). A leading `!` marks a negation that
/// **re-includes** a previously excluded file. An optional leading `**/` (match
/// in any directory) then an optional leading `/` (anchor to the repository
/// root) are accepted and stripped before matching. A pattern that still
/// contains a `/` after stripping those prefixes — or ends in `/`
/// (directory-only) — is a path/dir pattern that cannot match a bare root-level
/// file and is skipped.
///
/// Per gitignore precedence, the **last** matching pattern decides: patterns
/// are evaluated top-to-bottom while tracking a running ignored state, so
/// `.env*` followed by `!.env` correctly reports `.env` as NOT covered, and
/// `!.env` followed by `.env` reports it as covered.
fn gitignore_covers(contents: &str, filename: &str) -> bool {
    let mut ignored = false;
    for line in contents.lines() {
        let entry = line.trim();
        if entry.is_empty() || entry.starts_with('#') {
            continue;
        }
        // A leading `!` re-includes (un-ignores) a matching file.
        let (negation, body) = entry
            .strip_prefix('!')
            .map_or((false, entry), |rest| (true, rest));
        let pattern = body.strip_prefix("**/").unwrap_or(body);
        let pattern = pattern.strip_prefix('/').unwrap_or(pattern);
        if pattern.ends_with('/') || pattern.contains('/') {
            continue;
        }
        if glob_matches_basename(pattern, filename) {
            // Last match wins: a normal pattern ignores, a `!` negation
            // re-includes.
            ignored = !negation;
        }
    }
    ignored
}

/// Match a basename glob against `text`, anchored at both ends. The only special
/// character is `*`, which matches any run of characters (including empty).
fn glob_matches_basename(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    // Iterative wildcard match with backtracking on the most recent `*`.
    let (mut pi, mut ti) = (0_usize, 0_usize);
    let mut star: Option<usize> = None;
    let mut star_ti = 0_usize;
    while ti < t.len() {
        if pi < p.len() && p[pi] == t[ti] {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            star_ti = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            star_ti += 1;
            ti = star_ti;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// Check that project-root `.env` handling is set up safely.
///
/// - Any always-secret dotenv file (`.env`, `.env.local`, `.env.<profile>.local`)
///   present but NOT gitignore-covered → Warn, naming the offending file(s).
///   This security finding takes priority: it is surfaced even when
///   `.env.example` exists without a root `.env` (so the copy-the-template hint
///   can never hide an ungitignored, committable secret file).
/// - `.env.example` present but no `.env` → Warn (developer likely needs to
///   copy the template).
/// - otherwise → Pass.
///
/// Warn-only by design (never Fail), consistent with the skill doc.
pub fn check_dotenv_impl(
    has_env_example: bool,
    has_env: bool,
    uncovered_secret_files: &[String],
) -> CheckResult {
    // Security first: an ungitignored always-secret dotenv file risks committing
    // secrets and must never be hidden behind the copy-the-template hint, so
    // check it before the `.env.example`-without-`.env` case. When both apply,
    // lead with the security issue and still mention copying the template.
    if !uncovered_secret_files.is_empty() {
        let mut detail = format!(
            "{} present but not gitignored — you risk committing secrets",
            uncovered_secret_files.join(", ")
        );
        if has_env_example && !has_env {
            detail.push_str("; also copy `.env.example` to `.env` to get started");
        }
        return CheckResult {
            name: "dotenv",
            status: CheckStatus::Warn,
            detail: Some(detail),
            hint: Some(
                "Add your local .env files to .gitignore (e.g. `.env`, `.env.local`, `.env.*.local`)",
            ),
        };
    }
    if has_env_example && !has_env {
        return CheckResult {
            name: "dotenv",
            status: CheckStatus::Warn,
            detail: Some("`.env.example` is present but no `.env` exists".into()),
            hint: Some("Copy `.env.example` to `.env` and fill in local values"),
        };
    }
    CheckResult {
        name: "dotenv",
        status: CheckStatus::Pass,
        detail: Some(if has_env {
            "`.env` handling looks correct".into()
        } else {
            ".env not configured".into()
        }),
        hint: None,
    }
}

pub fn check_maintenance_mode() -> CheckResult {
    use autumn_web::maintenance::{MAINTENANCE_FLAG_FILE, MaintenanceState};
    let path = std::path::Path::new(MAINTENANCE_FLAG_FILE);
    match MaintenanceState::load_from_file(path) {
        Ok(Some(config)) => {
            let detail = config.message.as_ref().map_or_else(
                || "maintenance mode is ON".to_owned(),
                |msg| format!("maintenance mode is ON — \"{msg}\""),
            );
            CheckResult {
                name: "maintenance_mode",
                status: CheckStatus::Warn,
                detail: Some(detail),
                hint: Some("Run `autumn maintenance off` to re-enable normal traffic"),
            }
        }
        Ok(None) | Err(_) => CheckResult {
            name: "maintenance_mode",
            status: CheckStatus::Pass,
            detail: Some("maintenance mode is off".into()),
            hint: None,
        },
    }
}

// ── System-test browser check ─────────────────────────────────────────────

/// Candidate paths probed for a Chromium binary, in resolution order.
pub fn browser_candidate_paths() -> Vec<std::path::PathBuf> {
    let mut candidates: Vec<std::path::PathBuf> = Vec::new();

    if let Ok(p) = std::env::var("AUTUMN_CHROMIUM") {
        candidates.push(std::path::PathBuf::from(p));
    }

    if let Ok(base) = std::env::var("PLAYWRIGHT_BROWSERS_PATH") {
        let base = std::path::PathBuf::from(base);
        if let Ok(entries) = std::fs::read_dir(&base) {
            let mut pw_paths: Vec<_> = entries
                .flatten()
                .filter(|e| e.file_name().to_string_lossy().starts_with("chromium-"))
                .map(|e| {
                    if cfg!(target_os = "macos") {
                        e.path()
                            .join("chrome-mac")
                            .join("Chromium.app")
                            .join("Contents")
                            .join("MacOS")
                            .join("Chromium")
                    } else if cfg!(target_os = "windows") {
                        e.path().join("chrome-win").join("chrome.exe")
                    } else {
                        e.path().join("chrome-linux").join("chrome")
                    }
                })
                .collect();
            pw_paths.sort();
            pw_paths.reverse();
            candidates.extend(pw_paths);
        }
    }

    candidates.extend(
        [
            "/usr/bin/chromium-browser",
            "/usr/bin/chromium",
            "/usr/bin/google-chrome",
            "/usr/bin/google-chrome-stable",
            "/snap/bin/chromium",
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
            "/Applications/Chromium.app/Contents/MacOS/Chromium",
        ]
        .map(std::path::PathBuf::from),
    );

    candidates
}

/// Run `<path> --version` and return the trimmed output on success.
fn probe_browser_version(path: &std::path::Path) -> Option<String> {
    std::process::Command::new(path)
        .arg("--version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
}

/// Check whether a Chromium binary is available for system tests.
///
/// Returns `true` if the `[features]` table in `cargo_toml` declares `key`.
///
/// Scans only the `[features]` section (not dev-dependencies that may also
/// reference the feature) so a line like `autumn-web = { features = ["system-tests"] }`
/// does not produce a false positive.
fn cargo_toml_features_has_key(cargo_toml: &str, key: &str) -> bool {
    let quoted_key = format!("\"{key}\"");
    let mut in_features = false;
    for line in cargo_toml.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('#') {
            continue;
        }
        if trimmed == "[features]"
            || (trimmed.starts_with("[features]")
                && trimmed["[features]".len()..].trim_start().starts_with('#'))
        {
            in_features = true;
            continue;
        }
        if in_features {
            if trimmed.starts_with('[') {
                break;
            }
            let bare = trimmed
                .strip_prefix(key)
                .is_some_and(|r| r.trim_start().starts_with('='));
            let quoted = trimmed
                .strip_prefix(quoted_key.as_str())
                .is_some_and(|r| r.trim_start().starts_with('='));
            if bare || quoted {
                return true;
            }
        }
    }
    false
}

/// Reports `Warn` (not `Fail`) so that projects that don't use system tests
/// are not penalized. If you _do_ run system tests you'll get a clear
/// actionable message in `autumn doctor` output.
pub fn check_system_test_browser() -> CheckResult {
    let candidates = browser_candidate_paths();
    for path in &candidates {
        if path.is_file()
            && let Some(version) = probe_browser_version(path)
        {
            return CheckResult {
                name: "system_test_browser",
                status: CheckStatus::Pass,
                detail: Some(format!(
                    "Chromium for system tests: {version} ({})",
                    path.display()
                )),
                hint: None,
            };
        }
    }

    // Only warn when the project has opted into system tests; otherwise a
    // missing browser is irrelevant and must not fail `autumn doctor --strict`.
    let project_uses_system_tests = std::env::current_dir()
        .ok()
        .and_then(|d| std::fs::read_to_string(d.join("Cargo.toml")).ok())
        .is_some_and(|s| cargo_toml_features_has_key(&s, "system-tests"));

    if project_uses_system_tests {
        CheckResult {
            name: "system_test_browser",
            status: CheckStatus::Warn,
            detail: Some("no Chromium binary found — system tests will be skipped".into()),
            hint: Some(
                "Install: apt-get install chromium-browser  \
                 or set AUTUMN_CHROMIUM=/path/to/chrome",
            ),
        }
    } else {
        CheckResult {
            name: "system_test_browser",
            status: CheckStatus::Pass,
            detail: Some("no Chromium binary found (project does not use system-tests)".into()),
            hint: None,
        }
    }
}

/// Resolve the list of `#[repository]`-annotated table names that are not yet
/// registered in a `GdprRegistry` in the project source.
///
/// Currently uses a lightweight heuristic: scans `src/schema.rs` for `diesel::table!`
/// declarations and returns those that have no matching `GdprRegistry::register` call
/// in any `*.rs` source file. Returns an empty list when the project has no schema or
/// when all tables appear to be registered.
fn resolve_gdpr_unregistered_tables() -> Vec<String> {
    let schema_path = std::path::Path::new("src/schema.rs");
    let Ok(schema) = std::fs::read_to_string(schema_path) else {
        return Vec::new();
    };

    // Collect table names declared as `diesel::table! { <name> (id) { ... } }`.
    let mut declared_tables: Vec<String> = Vec::new();
    for line in schema.lines() {
        let trimmed = line.trim();
        // Match lines like `diesel::table! {` followed by the table name on the next line,
        // or inline `pub table_name (id) {` patterns.  Use a simple prefix scan.
        if let Some(rest) = trimmed
            .strip_prefix("diesel::table!")
            .map(str::trim)
            .filter(|r| r.starts_with('{'))
        {
            // table name is on the same line or will appear shortly; skip for now
            let _ = rest;
        }
        // Diesel schema emits bare identifiers like `    table_name (pk) {`
        // where `pk` can be any column name (id, uuid, code, or composite).
        if let Some(open_paren) = trimmed.find('(')
            && (trimmed.ends_with('{') || trimmed.ends_with("{ "))
        {
            let name = trimmed[..open_paren].trim();
            if !name.is_empty() && !name.starts_with("//") && !name.starts_with("diesel") {
                declared_tables.push(name.to_owned());
            }
        }
    }

    if declared_tables.is_empty() {
        return Vec::new();
    }

    // Check which tables appear to be registered via GdprRegistry in the project.
    let registered_tables = collect_gdpr_registered_tables_from_source();

    declared_tables
        .into_iter()
        .filter(|t| !registered_tables.contains(t))
        .collect()
}

/// Scan all `*.rs` files under `src/` for `GdprRegistry` register calls and
/// return the set of table names found. Handles both single-line and
/// rustfmt-formatted multi-line `ModelRegistration::` calls.
fn collect_gdpr_registered_tables_from_source() -> std::collections::HashSet<String> {
    let mut found = std::collections::HashSet::new();
    for path in glob_rs_files(std::path::Path::new("src")) {
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        scan_source_for_gdpr_registrations(&content, &mut found);
    }
    found
}

/// Extract `ModelRegistration::` table names from source text, handling both
/// single-line calls and multi-line rustfmt-formatted calls.
fn scan_source_for_gdpr_registrations(
    content: &str,
    found: &mut std::collections::HashSet<String>,
) {
    let lines: Vec<&str> = content.lines().collect();
    for (i, line) in lines.iter().enumerate() {
        if let Some(idx) = line.find("ModelRegistration::") {
            let rest = &line[idx..];
            // Try the same line first, then the next 3 lines for multi-line calls.
            if extract_quoted_table_name(rest, found) {
                continue;
            }
            for j in 1..=3 {
                if let Some(next) = lines.get(i + j)
                    && extract_quoted_table_name(next.trim(), found)
                {
                    break;
                }
            }
        }
    }
}

/// Extract the first double-quoted string from `s` as a table name.
/// Returns `true` if a non-empty, non-whitespace name was found and inserted.
fn extract_quoted_table_name(s: &str, found: &mut std::collections::HashSet<String>) -> bool {
    if let Some(start) = s.find('"')
        && let Some(end) = s[start + 1..].find('"')
    {
        let name = &s[start + 1..start + 1 + end];
        if !name.is_empty() && !name.contains(' ') {
            found.insert(name.to_owned());
            return true;
        }
    }
    false
}

/// Recursively collect all `*.rs` file paths under `dir`.
fn glob_rs_files(dir: impl AsRef<std::path::Path>) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.append(&mut glob_rs_files(&path));
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(path);
        }
    }
    out
}

/// Warn when the auth starter is present but one or more `#[repository]`-annotated
/// tables have not been registered in the GDPR export/erasure registry.
///
/// This check implements GDPR issue #820 AC #8:
/// > `autumn doctor` warns when repositories exist whose models are not registered
/// > for export/erasure once the auth starter is present.
///
/// `has_auth_starter` should be `true` when the project's `src/routes/auth.rs` exists
/// (indicating `autumn generate auth` was run).
/// `unregistered_tables` is the list of `#[repository]`-annotated table names that
/// have not been registered via `GdprRegistry`.
pub fn check_gdpr_export_registration_impl(
    has_auth_starter: bool,
    unregistered_tables: &[&str],
) -> CheckResult {
    if !has_auth_starter {
        return CheckResult {
            name: "gdpr_export_registration",
            status: CheckStatus::Pass,
            detail: Some("auth starter not present; GDPR registration not required".into()),
            hint: None,
        };
    }

    if unregistered_tables.is_empty() {
        return CheckResult {
            name: "gdpr_export_registration",
            status: CheckStatus::Pass,
            detail: Some("all repositories are registered for GDPR export/erasure".into()),
            hint: None,
        };
    }

    let tables = unregistered_tables.join(", ");
    CheckResult {
        name: "gdpr_export_registration",
        status: CheckStatus::Warn,
        detail: Some(format!(
            "{} repository model(s) not registered for GDPR export/erasure: {tables}",
            unregistered_tables.len()
        )),
        hint: Some(
            "Register each model via GdprRegistry in your application state. \
             See docs/guide/gdpr-compliance.md for the export/erasure API.",
        ),
    }
}

// ─── Model column privacy (#1374) ───────────────────────────────────────────

/// A `#[model]` column whose name looks sensitive but is not marked
/// `#[private]`. Input to [`check_model_private_columns_impl`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnprivateSensitiveColumn {
    pub model: String,
    pub column: String,
}

/// The sensitive-name heuristic shared with the `#[model]` macro: matches
/// `password`, `token`, `secret` anywhere, or a `hash` / `_hash` suffix.
pub fn column_name_is_sensitive(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.contains("password")
        || lower.contains("token")
        || lower.contains("secret")
        || lower.ends_with("_hash")
        || lower == "hash"
}

/// Warn when a `#[model]` column matches the sensitive-name heuristic but is
/// not marked `#[private]` (issue #1374 AC).
///
/// Pure and injectable for tests; a warning (never a hard failure) so it can't
/// break an existing build.
pub fn check_model_private_columns_impl(found: &[UnprivateSensitiveColumn]) -> CheckResult {
    if found.is_empty() {
        return CheckResult {
            name: "model_private_columns",
            status: CheckStatus::Pass,
            detail: Some("no sensitively-named model columns are missing `#[private]`".into()),
            hint: None,
        };
    }
    let lines: Vec<String> = found
        .iter()
        .map(|c| {
            format!(
                "{}.{} looks sensitive but is not marked `#[private]` (it will serialize into JSON responses)",
                c.model, c.column
            )
        })
        .collect();
    CheckResult {
        name: "model_private_columns",
        status: CheckStatus::Warn,
        detail: Some(lines.join("\n")),
        hint: Some(
            "Mark sensitive columns `#[private]` so they never leak into `Json`/`--api` \
             responses; the field stays writable and queryable.",
        ),
    }
}

/// Scan the project's `src/` for `#[model]` structs and return the sensitively
/// named columns that are not marked `#[private]`. Returns an empty list when
/// there is no `src/` directory or nothing is flagged.
fn resolve_unprivate_sensitive_columns() -> Vec<UnprivateSensitiveColumn> {
    let mut found = Vec::new();
    for path in glob_rs_files(std::path::Path::new("src")) {
        if let Ok(content) = std::fs::read_to_string(&path) {
            scan_source_for_unprivate_sensitive_columns(&content, &mut found);
        }
    }
    found
}

/// Line-based scanner: for each `#[model]`-annotated struct, flag `pub <name>:`
/// fields whose name is sensitive and that are not immediately preceded by a
/// `#[private]` (or `#[encrypted]`, which is private-in-JSON by default)
/// attribute. Deliberately lightweight (no full parse) — consistent with the
/// other `autumn doctor` source heuristics.
/// Whether an attribute line applies the `#[model]` macro, including the
/// path-qualified forms `#[autumn_web::model(...)]` and `#[macros::model]`.
/// Matches when the attribute path's last `::`-segment is `model`.
fn attr_line_is_model(line: &str) -> bool {
    let Some(rest) = line.trim_start().strip_prefix("#[") else {
        return false;
    };
    // The attribute path runs up to the first argument list, close bracket,
    // whitespace, or comma (e.g. `autumn_web::model` in `#[autumn_web::model(...)]`).
    let path = rest
        .split(|c: char| c == '(' || c == ']' || c == ',' || c.is_whitespace())
        .next()
        .unwrap_or("");
    !path.is_empty() && path.rsplit("::").next() == Some("model")
}

fn scan_source_for_unprivate_sensitive_columns(
    content: &str,
    found: &mut Vec<UnprivateSensitiveColumn>,
) {
    let lines: Vec<&str> = content.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        if !attr_line_is_model(lines[i]) {
            i += 1;
            continue;
        }
        // Find the struct name (the `struct X` line) and the opening brace.
        let mut j = i + 1;
        let mut model_name: Option<String> = None;
        while j < lines.len() {
            let t = lines[j].trim_start();
            if let Some(rest) = t
                .strip_prefix("pub struct ")
                .or_else(|| t.strip_prefix("struct "))
            {
                model_name = rest
                    .split(|c: char| c == '{' || c == '(' || c.is_whitespace())
                    .next()
                    .filter(|name| !name.is_empty())
                    .map(str::to_owned);
                break;
            }
            j += 1;
        }
        let Some(model_name) = model_name else {
            i += 1;
            continue;
        };
        // Walk the struct body until the matching closing brace at column 0-ish.
        let mut k = j + 1;
        let mut field_is_private = false;
        while k < lines.len() {
            let t = lines[k].trim();
            if t.starts_with('}') {
                // Closing brace terminates the struct even when followed by a
                // trailing comment (`} // User`) or a semicolon (`};`).
                break;
            }
            if t.starts_with("#[private") || t.starts_with("#[encrypted") {
                field_is_private = true;
            } else if let Some(field) = parse_pub_field_name(t) {
                if column_name_is_sensitive(&field) && !field_is_private {
                    found.push(UnprivateSensitiveColumn {
                        model: model_name.clone(),
                        column: field,
                    });
                }
                // Reset the attribute flag after consuming a field line.
                field_is_private = false;
            }
            k += 1;
        }
        i = k + 1;
    }
}

/// Extract the field name from a `pub <name>: <ty>,` struct-field line, or
/// `None` if the line is not a field declaration.
fn parse_pub_field_name(line: &str) -> Option<String> {
    let rest = line.strip_prefix("pub ")?;
    let colon = rest.find(':')?;
    let name = rest[..colon].trim();
    if name.is_empty() || !name.chars().all(|c| c.is_alphanumeric() || c == '_') {
        return None;
    }
    Some(name.to_owned())
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_private_columns_pass_when_none_flagged() {
        let r = check_model_private_columns_impl(&[]);
        assert_eq!(r.status, CheckStatus::Pass);
    }

    #[test]
    fn model_private_columns_warn_lists_offenders() {
        let found = vec![
            UnprivateSensitiveColumn {
                model: "User".into(),
                column: "password_hash".into(),
            },
            UnprivateSensitiveColumn {
                model: "ApiKey".into(),
                column: "secret".into(),
            },
        ];
        let r = check_model_private_columns_impl(&found);
        assert_eq!(r.status, CheckStatus::Warn);
        let detail = r.detail.unwrap();
        assert!(detail.contains("User.password_hash"));
        assert!(detail.contains("ApiKey.secret"));
    }

    #[test]
    fn sensitive_name_heuristic_matches_expected() {
        assert!(column_name_is_sensitive("password_hash"));
        assert!(column_name_is_sensitive("reset_token"));
        assert!(column_name_is_sensitive("api_secret"));
        assert!(column_name_is_sensitive("password"));
        assert!(!column_name_is_sensitive("email"));
        assert!(!column_name_is_sensitive("display_name"));
    }

    #[test]
    fn scan_flags_unprivate_sensitive_columns_only() {
        let src = r"
#[model]
pub struct User {
    #[id]
    pub id: i64,
    pub email: String,
    pub password_hash: String,
    #[private]
    pub reset_token: String,
}
";
        let mut found = Vec::new();
        scan_source_for_unprivate_sensitive_columns(src, &mut found);
        assert_eq!(
            found.len(),
            1,
            "only password_hash should be flagged: {found:?}"
        );
        assert_eq!(found[0].model, "User");
        assert_eq!(found[0].column, "password_hash");
    }

    #[test]
    fn scan_terminates_struct_on_brace_with_trailing_tokens() {
        // The closing brace may be followed by a trailing comment (`} // User`)
        // or a semicolon (`};`); the scan must still stop at the struct end and
        // not swallow following declarations.
        for closing in ["} // User", "};", "}  // trailing"] {
            let src = format!(
                "#[model]\npub struct User {{\n    #[id]\n    pub id: i64,\n    pub password_hash: String,\n{closing}\npub struct Other {{\n    pub api_secret: String,\n}}\n"
            );
            let mut found = Vec::new();
            scan_source_for_unprivate_sensitive_columns(&src, &mut found);
            assert_eq!(
                found.len(),
                1,
                "closing line `{closing}` must terminate the struct: {found:?}"
            );
            assert_eq!(found[0].model, "User");
            assert_eq!(found[0].column, "password_hash");
        }
    }

    #[test]
    fn scan_treats_encrypted_as_private_in_json() {
        let src = r"
#[model]
pub struct Vault {
    #[id]
    pub id: i64,
    #[encrypted]
    pub api_secret: String,
}
";
        let mut found = Vec::new();
        scan_source_for_unprivate_sensitive_columns(src, &mut found);
        assert!(
            found.is_empty(),
            "encrypted columns are private-in-JSON: {found:?}"
        );
    }

    #[test]
    fn scan_matches_path_qualified_model_attributes() {
        // Path-qualified forms (`#[autumn_web::model]`, `#[macros::model]`) must
        // be scanned like the bare `#[model]` — otherwise the privacy heuristic
        // silently does nothing for files that use them.
        for attr in [
            "#[autumn_web::model]",
            "#[macros::model]",
            "#[model(table = \"users\")]",
        ] {
            let src = format!(
                "{attr}\npub struct User {{\n    #[id]\n    pub id: i64,\n    pub password_hash: String,\n}}\n"
            );
            let mut found = Vec::new();
            scan_source_for_unprivate_sensitive_columns(&src, &mut found);
            assert_eq!(
                found.len(),
                1,
                "path-qualified `{attr}` must be scanned and flag password_hash: {found:?}"
            );
            assert_eq!(found[0].column, "password_hash");
        }

        // A helper self-check: only attributes whose last segment is `model`.
        assert!(attr_line_is_model("#[autumn_web::model(table = \"x\")]"));
        assert!(attr_line_is_model("  #[macros::model]"));
        assert!(attr_line_is_model("#[model]"));
        assert!(!attr_line_is_model("#[models]"));
        assert!(!attr_line_is_model("#[model_config]"));
        assert!(!attr_line_is_model("pub struct User {"));
    }

    #[test]
    fn test_recursive_toml_validation() {
        let valid_toml = r#"
            [server]
            port = 4000
            host = "0.0.0.0"

            [database]
            primary_url = "postgres://localhost/db"
        "#;
        let r = check_toml_content(valid_toml);
        assert_eq!(r.status, CheckStatus::Pass);

        let typo_toml = r#"
            [database]
            primry_url = "postgres://localhost/db"
        "#;
        let r = check_toml_content(typo_toml);
        assert_eq!(r.status, CheckStatus::Warn);
        let detail = r.detail.as_ref().unwrap();
        assert!(detail.contains("database.primry_url"));
        assert!(detail.contains("did you mean \"database.primary_url\"?"));

        let unknown_toml = r"
            [server]
            xyz_completely_unknown_key = 123
        ";
        let r = check_toml_content(unknown_toml);
        assert_eq!(r.status, CheckStatus::Warn);
        let detail = r.detail.as_ref().unwrap();
        assert!(detail.contains("server.xyz_completely_unknown_key"));
        assert!(!detail.contains("did you mean"));
    }

    // ── mail_unsubscribe check ─────────────────────────────────────────────────

    #[test]
    fn mail_unsubscribe_no_usage_passes() {
        let r = check_mail_unsubscribe_config_impl(false, None, None, false, true, 30);
        assert_eq!(r.status, CheckStatus::Pass);
    }

    #[test]
    fn mail_unsubscribe_usage_without_config_fails_in_prod() {
        let r = check_mail_unsubscribe_config_impl(true, None, None, false, true, 30);
        assert_eq!(r.status, CheckStatus::Fail);
        assert!(r.detail.unwrap().contains("list_unsubscribe"));
    }

    #[test]
    fn mail_unsubscribe_usage_without_config_warns_outside_prod() {
        let r = check_mail_unsubscribe_config_impl(true, None, Some("   "), false, false, 30);
        assert_eq!(r.status, CheckStatus::Warn);
    }

    #[test]
    fn mail_unsubscribe_disabled_transport_passes() {
        // Disabled transport emits no list mail, so config isn't required.
        let r = check_mail_unsubscribe_config_impl(true, None, None, true, true, 30);
        assert_eq!(r.status, CheckStatus::Pass);
    }

    #[test]
    fn mail_unsubscribe_base_url_warns_in_prod_but_passes_outside() {
        // In production a base_url additionally requires a suppression backend the
        // runtime fails closed without; doctor can't verify it, so it warns.
        let r = check_mail_unsubscribe_config_impl(
            true,
            Some("https://app.example.com"),
            None,
            false,
            true,
            30,
        );
        assert_eq!(r.status, CheckStatus::Warn);
        assert!(r.detail.unwrap().contains("suppression backend"));
        // Outside production the runtime does not gate on the backend, so it passes.
        let r = check_mail_unsubscribe_config_impl(
            true,
            Some("https://app.example.com"),
            None,
            false,
            false,
            30,
        );
        assert_eq!(r.status, CheckStatus::Pass);
    }

    #[test]
    fn mail_unsubscribe_usage_with_mailto_passes() {
        let r = check_mail_unsubscribe_config_impl(
            true,
            None,
            Some("unsub@example.com"),
            false,
            true,
            30,
        );
        assert_eq!(r.status, CheckStatus::Pass);
    }

    #[test]
    fn detects_mailer_attribute_but_not_builder_or_comment() {
        assert!(file_declares_list_unsubscribe(
            "#[mailer(list_unsubscribe = \"weekly_digest\")]"
        ));
        // whitespace / newlines inside the attribute still match
        assert!(file_declares_list_unsubscribe(
            "#[mailer(\n    list_unsubscribe = \"x\"\n)]"
        ));
        // builder call must NOT match
        assert!(!file_declares_list_unsubscribe(
            "Mail::builder().list_unsubscribe(\"x\").build()"
        ));
        // line comment must NOT match
        assert!(!file_declares_list_unsubscribe(
            "// remember to set list_unsubscribe later"
        ));
        // commented-out attribute (line and mid-line) must NOT match
        assert!(!file_declares_list_unsubscribe(
            "// #[mailer(list_unsubscribe = \"x\")]"
        ));
        assert!(!file_declares_list_unsubscribe(
            "let x = 1; // #[mailer(list_unsubscribe = \"x\")]"
        ));
        // block-commented attribute must NOT match
        assert!(!file_declares_list_unsubscribe(
            "/* #[mailer(list_unsubscribe = \"weekly\")] */"
        ));
        assert!(!file_declares_list_unsubscribe(
            "before /*\n#[mailer(list_unsubscribe = \"x\")]\n*/ after"
        ));
    }

    #[test]
    fn scan_finds_workspace_mailer_but_skips_vendored_copy() {
        let root = tempfile::tempdir().expect("temp dir");
        // A vendored dependency copy (e.g. `cargo vendor`) carries the framework's
        // own list mailer in its tests — this must NOT count as app usage.
        let vendored = root.path().join("vendor/autumn-web/tests");
        std::fs::create_dir_all(&vendored).expect("create vendor dir");
        std::fs::write(
            vendored.join("mail_unsubscribe.rs"),
            "#[mailer(list_unsubscribe = \"weekly_digest\")]\nfn x() {}",
        )
        .expect("write vendored source");
        assert!(
            !scan_dir_for_list_unsubscribe(root.path()),
            "vendored dependency sources must be skipped"
        );

        // Integration-test and example fixtures are not compiled into the app
        // binary, so the runtime inventory never registers them — they must be
        // skipped too.
        for tree in ["tests", "examples"] {
            let dir = root.path().join(tree);
            std::fs::create_dir_all(&dir).expect("create test/example dir");
            std::fs::write(
                dir.join("fixture.rs"),
                "#[mailer(list_unsubscribe = \"weekly_digest\")]\nfn f() {}",
            )
            .expect("write fixture source");
        }
        assert!(
            !scan_dir_for_list_unsubscribe(root.path()),
            "tests/ and examples/ fixtures must be skipped"
        );

        // A real workspace member that declares one is still detected.
        let app = root.path().join("crates/marketing/src");
        std::fs::create_dir_all(&app).expect("create app dir");
        std::fs::write(
            app.join("mailers.rs"),
            "#[mailer(list_unsubscribe = \"weekly_digest\")]\nfn y() {}",
        )
        .expect("write app source");
        assert!(
            scan_dir_for_list_unsubscribe(root.path()),
            "workspace member mailer must still be detected"
        );
    }

    #[test]
    fn inline_profile_lookup_order_matches_runtime() {
        // Canonical spelling applied last so it wins over the legacy alias —
        // matching autumn_web::config::profile_lookup_names.
        assert_eq!(
            inline_profile_lookup_names("prod"),
            vec!["production", "prod"]
        );
        assert_eq!(
            inline_profile_lookup_names("dev"),
            vec!["development", "dev"]
        );
        assert_eq!(inline_profile_lookup_names("staging"), vec!["staging"]);
    }

    #[test]
    fn override_file_lookup_order_prefers_selected_spelling() {
        // Default canonical selection prefers the canonical file.
        assert_eq!(
            override_file_lookup_names("prod", "prod"),
            vec!["prod".to_owned(), "production".to_owned()]
        );
        // When the operator selected the legacy spelling, that file is preferred.
        assert_eq!(
            override_file_lookup_names("prod", "production"),
            vec!["production".to_owned(), "prod".to_owned()]
        );
        assert_eq!(
            override_file_lookup_names("dev", "dev"),
            vec!["dev".to_owned(), "development".to_owned()]
        );
        assert_eq!(
            override_file_lookup_names("dev", "development"),
            vec!["development".to_owned(), "dev".to_owned()]
        );
        assert_eq!(
            override_file_lookup_names("staging", "staging"),
            vec!["staging".to_owned()]
        );
    }

    #[test]
    fn mail_unsubscribe_invalid_base_url_fails_in_prod() {
        // Non-empty but malformed: runtime validate() would reject this, so doctor
        // must not return Pass on it.
        let r = check_mail_unsubscribe_config_impl(
            true,
            Some("http://app.example.com"),
            None,
            false,
            true,
            30,
        );
        assert_eq!(r.status, CheckStatus::Fail);
        let r = check_mail_unsubscribe_config_impl(
            true,
            Some("https://app.example.com:abc"),
            None,
            false,
            true,
            30,
        );
        assert_eq!(r.status, CheckStatus::Fail);
    }

    #[test]
    fn mail_unsubscribe_invalid_mailto_fails_in_prod() {
        let r = check_mail_unsubscribe_config_impl(
            true,
            None,
            Some("unsubscribe example.com"),
            false,
            true,
            30,
        );
        assert_eq!(r.status, CheckStatus::Fail);
    }

    #[test]
    fn mail_unsubscribe_invalid_value_is_lenient_outside_prod() {
        // Dev mirrors the runtime: malformed values are not a hard gate.
        let r = check_mail_unsubscribe_config_impl(
            true,
            Some("http://app.example.com"),
            None,
            false,
            false,
            30,
        );
        assert_eq!(r.status, CheckStatus::Pass);
    }

    #[test]
    fn doctor_url_validation_matches_runtime_cases() {
        assert!(is_valid_https_base_url_doctor("https://app.example.com"));
        assert!(is_valid_https_base_url_doctor(
            "https://app.example.com:8443"
        ));
        assert!(is_valid_https_base_url_doctor(
            "https://app.example.com/base"
        ));
        assert!(!is_valid_https_base_url_doctor("http://app.example.com"));
        assert!(!is_valid_https_base_url_doctor(
            "https://app.example.com:abc"
        ));
        assert!(!is_valid_https_base_url_doctor("https://@/base"));
        assert!(!is_valid_https_base_url_doctor("https:///path"));
        assert!(!is_valid_https_base_url_doctor("https:/app.example.com"));
        assert!(!is_valid_https_base_url_doctor("https:app.example.com"));
        assert!(!is_valid_https_base_url_doctor(
            "https://user@app.example.com"
        ));
        assert!(!is_valid_https_base_url_doctor(
            "https://app.example.com?q=1"
        ));

        assert!(is_valid_mailto_address_doctor("unsub@example.com"));
        assert!(is_valid_mailto_address_doctor("mailto:unsub@example.com"));
        assert!(!is_valid_mailto_address_doctor("not-an-email"));
        assert!(!is_valid_mailto_address_doctor("unsubscribe example.com"));
        assert!(!is_valid_mailto_address_doctor("https://unsub@example.com"));
        assert!(!is_valid_mailto_address_doctor("unsub@https://example.com"));
    }

    #[test]
    fn mail_unsubscribe_usage_with_both_url_and_mailto_warns_in_prod() {
        // base_url presence drives the suppression-backend warning even when a
        // mailto fallback is also configured.
        let r = check_mail_unsubscribe_config_impl(
            true,
            Some("https://app.example.com"),
            Some("unsub@example.com"),
            false,
            true,
            30,
        );
        assert_eq!(r.status, CheckStatus::Warn);
    }

    #[test]
    fn transport_resolver_trims_and_ignores_invalid_env() {
        // Valid env override wins (trimmed + case-insensitive).
        assert_eq!(
            resolve_effective_mail_transport(Some(" SMTP ".to_owned()), Some("disabled"), "prod"),
            "smtp"
        );
        // Whitespace-only "disabled" must resolve to disabled, not be treated raw.
        assert_eq!(
            resolve_effective_mail_transport(Some(" disabled ".to_owned()), None, "prod"),
            "disabled"
        );
        // Invalid env override is ignored → falls back to TOML.
        assert_eq!(
            resolve_effective_mail_transport(Some("bogus".to_owned()), Some("disabled"), "prod"),
            "disabled"
        );
        // No env, no TOML → profile smart-default.
        assert_eq!(resolve_effective_mail_transport(None, None, "dev"), "log");
        assert_eq!(
            resolve_effective_mail_transport(None, None, "prod"),
            "disabled"
        );
        // TOML value is parsed too (case-insensitive).
        assert_eq!(
            resolve_effective_mail_transport(None, Some("LOG"), "prod"),
            "log"
        );
    }

    #[test]
    fn ttl_resolver_ignores_invalid_env_like_runtime() {
        let table: toml::Table =
            toml::from_str("[mail]\nunsubscribe_token_ttl_days = 14\n").unwrap();
        // Absent env → TOML value.
        assert_eq!(
            resolve_unsubscribe_token_ttl_days_from_sources(None, Some(&table)),
            14
        );
        // No TOML, no env → default 30.
        assert_eq!(
            resolve_unsubscribe_token_ttl_days_from_sources(None, None),
            30
        );
        // Valid env overrides TOML.
        assert_eq!(
            resolve_unsubscribe_token_ttl_days_from_sources(Some("7".to_owned()), Some(&table)),
            7
        );
        // Blank / non-integer env is ignored (runtime warns + keeps TOML/default),
        // so it must NOT resolve to 0 and falsely fail the positive-days check.
        assert_eq!(
            resolve_unsubscribe_token_ttl_days_from_sources(Some(String::new()), Some(&table)),
            14
        );
        assert_eq!(
            resolve_unsubscribe_token_ttl_days_from_sources(Some("abc".to_owned()), Some(&table)),
            14
        );
        assert_eq!(
            resolve_unsubscribe_token_ttl_days_from_sources(Some("  ".to_owned()), None),
            30
        );
    }

    #[test]
    fn ttl_toml_type_invalid_flags_non_integer_values() {
        // Absent key → not invalid (the default applies).
        assert!(!unsubscribe_ttl_toml_type_invalid(None));
        let empty: toml::Table = toml::from_str("[mail]\n").unwrap();
        assert!(!unsubscribe_ttl_toml_type_invalid(Some(&empty)));
        // A bare integer is valid.
        let int: toml::Table = toml::from_str("[mail]\nunsubscribe_token_ttl_days = 30\n").unwrap();
        assert!(!unsubscribe_ttl_toml_type_invalid(Some(&int)));
        // A quoted string or a float is the wrong type — the runtime's typed i64
        // deserialize rejects it before boot, so doctor must flag it.
        let string: toml::Table =
            toml::from_str("[mail]\nunsubscribe_token_ttl_days = \"30\"\n").unwrap();
        assert!(unsubscribe_ttl_toml_type_invalid(Some(&string)));
        let float: toml::Table =
            toml::from_str("[mail]\nunsubscribe_token_ttl_days = 30.0\n").unwrap();
        assert!(unsubscribe_ttl_toml_type_invalid(Some(&float)));
    }

    #[test]
    fn dest_toml_type_invalid_flags_non_string_values() {
        assert!(!unsubscribe_dest_toml_type_invalid(None));
        let empty: toml::Table = toml::from_str("[mail]\n").unwrap();
        assert!(!unsubscribe_dest_toml_type_invalid(Some(&empty)));
        // String values are valid.
        let strings: toml::Table = toml::from_str(
            "[mail]\nunsubscribe_base_url = \"https://x\"\nunsubscribe_mailto = \"u@x.com\"\n",
        )
        .unwrap();
        assert!(!unsubscribe_dest_toml_type_invalid(Some(&strings)));
        // A present non-string (integer/array) is the wrong type — the runtime's
        // Option<String> deserialize rejects it before boot, so doctor must flag it.
        let int_url: toml::Table = toml::from_str("[mail]\nunsubscribe_base_url = 42\n").unwrap();
        assert!(unsubscribe_dest_toml_type_invalid(Some(&int_url)));
        let arr_mailto: toml::Table =
            toml::from_str("[mail]\nunsubscribe_mailto = [\"u@x.com\"]\n").unwrap();
        assert!(unsubscribe_dest_toml_type_invalid(Some(&arr_mailto)));
    }

    #[test]
    fn mail_unsubscribe_non_positive_ttl_fails_in_any_profile() {
        // Runtime validate() rejects a non-positive TTL in every profile, even
        // with no list usage or a disabled transport, so the app won't boot.
        for is_prod in [true, false] {
            let r = check_mail_unsubscribe_config_impl(false, None, None, true, is_prod, 0);
            assert_eq!(r.status, CheckStatus::Fail, "ttl=0 prod={is_prod}");
            let r = check_mail_unsubscribe_config_impl(false, None, None, true, is_prod, -5);
            assert_eq!(r.status, CheckStatus::Fail, "ttl=-5 prod={is_prod}");
        }
    }

    #[test]
    fn mail_unsubscribe_invalid_destination_fails_even_without_usage() {
        // Runtime validate() rejects a malformed destination in prod regardless of
        // whether any #[mailer] declares list_unsubscribe, so doctor must not Pass
        // before validating it.
        let r = check_mail_unsubscribe_config_impl(
            false,
            Some("http://app.example.com"),
            None,
            true,
            true,
            30,
        );
        assert_eq!(r.status, CheckStatus::Fail);
        let r = check_mail_unsubscribe_config_impl(
            false,
            None,
            Some("unsubscribe example.com"),
            true,
            true,
            30,
        );
        assert_eq!(r.status, CheckStatus::Fail);
    }

    #[test]
    fn strip_rust_comments_handles_unterminated_block_comment() {
        // Should not panic; the remaining unclosed block is simply stripped.
        let result = strip_rust_comments("code /* unclosed comment");
        assert!(result.contains("code"));
        assert!(!result.contains("unclosed"));
    }

    #[test]
    fn strip_rust_comments_removes_line_comments_and_preserves_code() {
        // Code before a comment is retained; everything after `//` is dropped.
        let result = strip_rust_comments("let x = 1; // this comment should disappear");
        assert!(
            result.contains("let x = 1;"),
            "code before comment preserved: {result}"
        );
        assert!(
            !result.contains("disappear"),
            "line comment stripped: {result}"
        );
    }

    // ── glyph ────────────────────────────────────────────────────────────────

    #[test]
    fn glyph_pass() {
        assert_eq!(glyph(&CheckStatus::Pass), "✅");
    }

    #[test]
    fn glyph_warn() {
        assert_eq!(glyph(&CheckStatus::Warn), "⚠️ ");
    }

    #[test]
    fn glyph_fail() {
        assert_eq!(glyph(&CheckStatus::Fail), "❌");
    }

    #[test]
    fn trusted_hosts_fail_in_production_when_empty() {
        let result = check_trusted_hosts_impl(&[], true);
        assert_eq!(result.name, "trusted_hosts");
        assert!(matches!(result.status, CheckStatus::Fail));
    }

    #[test]
    fn trusted_hosts_warn_on_wildcard() {
        let result = check_trusted_hosts_impl(&["*".to_owned()], true);
        assert_eq!(result.name, "trusted_hosts");
        assert!(matches!(result.status, CheckStatus::Warn));
    }

    #[test]
    fn trusted_hosts_warn_on_wildcard_when_mixed_with_other_entries() {
        let result = check_trusted_hosts_impl(&["example.com".to_owned(), "*".to_owned()], true);
        assert_eq!(result.name, "trusted_hosts");
        assert!(matches!(result.status, CheckStatus::Warn));
    }

    // ── compute_summary ──────────────────────────────────────────────────────

    #[test]
    fn compute_summary_all_pass() {
        let results = vec![
            CheckResult {
                name: "a",
                status: CheckStatus::Pass,
                detail: None,
                hint: None,
            },
            CheckResult {
                name: "b",
                status: CheckStatus::Pass,
                detail: None,
                hint: None,
            },
        ];
        let s = compute_summary(&results);
        assert_eq!(s.passed, 2);
        assert_eq!(s.warned, 0);
        assert_eq!(s.failed, 0);
    }

    #[test]
    fn compute_summary_mixed() {
        let results = vec![
            CheckResult {
                name: "a",
                status: CheckStatus::Pass,
                detail: None,
                hint: None,
            },
            CheckResult {
                name: "b",
                status: CheckStatus::Warn,
                detail: None,
                hint: None,
            },
            CheckResult {
                name: "c",
                status: CheckStatus::Fail,
                detail: None,
                hint: None,
            },
        ];
        let s = compute_summary(&results);
        assert_eq!(s.passed, 1);
        assert_eq!(s.warned, 1);
        assert_eq!(s.failed, 1);
    }

    #[test]
    fn compute_summary_empty() {
        let s = compute_summary(&[]);
        assert_eq!(s.passed, 0);
        assert_eq!(s.warned, 0);
        assert_eq!(s.failed, 0);
    }

    // ── exit_code ────────────────────────────────────────────────────────────

    #[test]
    fn exit_code_no_failures() {
        let s = Summary {
            passed: 3,
            warned: 0,
            failed: 0,
        };
        assert_eq!(exit_code(&s, false), 0);
    }

    #[test]
    fn exit_code_with_failure() {
        let s = Summary {
            passed: 2,
            warned: 0,
            failed: 1,
        };
        assert_eq!(exit_code(&s, false), 1);
    }

    #[test]
    fn exit_code_warn_non_strict() {
        let s = Summary {
            passed: 2,
            warned: 1,
            failed: 0,
        };
        assert_eq!(exit_code(&s, false), 0);
    }

    #[test]
    fn exit_code_warn_strict() {
        let s = Summary {
            passed: 2,
            warned: 1,
            failed: 0,
        };
        assert_eq!(exit_code(&s, true), 1);
    }

    #[test]
    fn exit_code_zero_when_all_pass_strict() {
        let s = Summary {
            passed: 5,
            warned: 0,
            failed: 0,
        };
        assert_eq!(exit_code(&s, true), 0);
    }

    // ── format_check_line ────────────────────────────────────────────────────

    #[test]
    fn format_check_line_pass_contains_glyph_and_name() {
        let r = CheckResult {
            name: "rust_toolchain",
            status: CheckStatus::Pass,
            detail: Some("1.88.0".into()),
            hint: None,
        };
        let line = format_check_line(&r);
        assert!(line.contains("✅"));
        assert!(line.contains("rust_toolchain"));
    }

    #[test]
    fn format_check_line_fail_includes_hint() {
        let r = CheckResult {
            name: "port_bindable",
            status: CheckStatus::Fail,
            detail: Some("port 3000 in use".into()),
            hint: Some("Kill the process using port 3000 or change server.port in autumn.toml"),
        };
        let line = format_check_line(&r);
        assert!(line.contains("❌"));
        assert!(line.contains("port_bindable"));
        assert!(line.contains("Kill the process"));
    }

    #[test]
    fn format_check_line_pass_omits_hint() {
        let r = CheckResult {
            name: "rust_toolchain",
            status: CheckStatus::Pass,
            detail: None,
            hint: Some("some hint that should not appear on pass"),
        };
        let line = format_check_line(&r);
        assert!(!line.contains("some hint"));
    }

    #[test]
    fn format_check_line_warn_includes_hint() {
        let r = CheckResult {
            name: "version_compat",
            status: CheckStatus::Warn,
            detail: Some("patch skew".into()),
            hint: Some("Update your dependency"),
        };
        let line = format_check_line(&r);
        assert!(line.contains("⚠️"));
        assert!(line.contains("Update your dependency"));
    }

    // ── format_summary_line ──────────────────────────────────────────────────

    #[test]
    fn format_summary_all_pass() {
        let s = Summary {
            passed: 7,
            warned: 0,
            failed: 0,
        };
        let line = format_summary_line(&s, 0);
        assert!(line.contains("7 passed"));
        assert!(line.contains("0 warnings"));
        assert!(line.contains("0 failed"));
        assert!(line.contains("all clear"));
    }

    #[test]
    fn format_summary_with_failure() {
        let s = Summary {
            passed: 5,
            warned: 1,
            failed: 1,
        };
        let line = format_summary_line(&s, 1);
        assert!(line.contains("5 passed"));
        assert!(line.contains("1 warning"));
        assert!(line.contains("1 failed"));
        assert!(line.contains("problems found"));
    }

    #[test]
    fn format_summary_singular_warning_label() {
        let s = Summary {
            passed: 3,
            warned: 1,
            failed: 0,
        };
        let line = format_summary_line(&s, 0);
        assert!(line.contains("1 warning,"));
    }

    #[test]
    fn format_summary_plural_warning_label() {
        let s = Summary {
            passed: 1,
            warned: 2,
            failed: 0,
        };
        let line = format_summary_line(&s, 0);
        assert!(line.contains("2 warnings,"));
    }

    // ── check_toml_content ───────────────────────────────────────────────────

    #[test]
    fn check_toml_content_valid() {
        let content = r#"
[server]
port = 3000

[database]
url = "postgres://localhost/mydb"
"#;
        let r = check_toml_content(content);
        assert_eq!(r.status, CheckStatus::Pass);
    }

    #[test]
    fn check_toml_content_syntax_error() {
        let content = "[[[[invalid toml";
        let r = check_toml_content(content);
        assert_eq!(r.status, CheckStatus::Fail);
        assert!(r.hint.is_some());
    }

    #[test]
    fn check_toml_content_unknown_key_warns() {
        let content = r#"
[server]
port = 3000

[totally_unknown_section]
foo = "bar"
"#;
        let r = check_toml_content(content);
        assert_eq!(r.status, CheckStatus::Warn);
        assert!(
            r.detail
                .as_deref()
                .unwrap_or("")
                .contains("totally_unknown_section"),
            "detail should mention the unknown key"
        );
    }

    #[test]
    fn check_toml_content_empty_is_pass() {
        let r = check_toml_content("");
        assert_eq!(r.status, CheckStatus::Pass);
    }

    #[test]
    fn check_toml_content_all_known_sections_pass() {
        let content: String = KNOWN_TOML_SECTIONS
            .iter()
            .flat_map(|&s| ["[", s, "]\n"])
            .collect();
        let r = check_toml_content(&content);
        assert_eq!(
            r.status,
            CheckStatus::Pass,
            "Test failed with result: {r:?}"
        );
    }

    // ── check_version_compat ─────────────────────────────────────────────────

    #[test]
    fn check_version_compat_matching() {
        let r = check_version_compat("0.3.0", "0.3.0");
        assert_eq!(r.status, CheckStatus::Pass);
    }

    #[test]
    fn check_version_compat_minor_skew_fails() {
        let r = check_version_compat("0.3.0", "0.4.0");
        assert_eq!(r.status, CheckStatus::Fail);
        assert!(r.hint.is_some());
    }

    #[test]
    fn check_version_compat_patch_skew_warns() {
        let r = check_version_compat("0.3.0", "0.3.1");
        assert_eq!(r.status, CheckStatus::Warn);
    }

    #[test]
    fn check_version_compat_caret_requirement_stripped() {
        let r = check_version_compat("0.3.0", "^0.3");
        assert_eq!(r.status, CheckStatus::Pass);
    }

    #[test]
    fn check_version_compat_tilde_requirement_stripped() {
        let r = check_version_compat("0.3.0", "~0.3.0");
        assert_eq!(r.status, CheckStatus::Pass);
    }

    #[test]
    fn check_version_compat_exact_requirement_stripped() {
        let r = check_version_compat("0.3.0", "=0.3.0");
        assert_eq!(r.status, CheckStatus::Pass);
    }

    // ── check_port_bindable_impl ─────────────────────────────────────────────

    #[test]
    fn check_port_bindable_impl_success() {
        let r = check_port_bindable_impl(3000, |_port| true);
        assert_eq!(r.status, CheckStatus::Pass);
        assert!(r.detail.as_deref().unwrap_or("").contains("available"));
    }

    #[test]
    fn check_port_bindable_impl_failure() {
        let r = check_port_bindable_impl(3000, |_port| false);
        assert_eq!(r.status, CheckStatus::Fail);
        assert!(r.hint.is_some());
        assert!(r.detail.as_deref().unwrap_or("").contains("in use"));
    }

    #[test]
    fn check_port_bindable_impl_reports_correct_port() {
        let r = check_port_bindable_impl(8080, |_| true);
        assert!(r.detail.as_deref().unwrap_or("").contains("8080"));
    }

    // ── check_rust_toolchain_impl ────────────────────────────────────────────

    #[test]
    fn check_rust_toolchain_impl_pass() {
        let r = check_rust_toolchain_impl("rustc 1.88.0 (abc123 2026-04-01)", "1.88.0");
        assert_eq!(r.status, CheckStatus::Pass);
    }

    #[test]
    fn check_rust_toolchain_impl_fail() {
        let r = check_rust_toolchain_impl("rustc 1.80.0 (abc123 2025-01-01)", "1.88.0");
        assert_eq!(r.status, CheckStatus::Fail);
        assert!(r.hint.is_some());
    }

    #[test]
    fn check_rust_toolchain_impl_newer_passes() {
        let r = check_rust_toolchain_impl("rustc 1.90.0 (abc 2026-01-01)", "1.88.0");
        assert_eq!(r.status, CheckStatus::Pass);
    }

    #[test]
    fn check_rust_toolchain_impl_exact_msrv_passes() {
        let r = check_rust_toolchain_impl("1.88.0", "1.88.0");
        assert_eq!(r.status, CheckStatus::Pass);
    }

    #[test]
    fn check_rust_toolchain_impl_unparseable_warns() {
        let r = check_rust_toolchain_impl("not-a-version", "1.88.0");
        assert_eq!(r.status, CheckStatus::Warn);
    }

    // ── parse_db_host_port ───────────────────────────────────────────────────

    #[test]
    fn parse_db_host_port_full_url() {
        let (host, port) = parse_db_host_port("postgres://user:pass@localhost:5432/mydb").unwrap();
        assert_eq!(host, "localhost");
        assert_eq!(port, 5432);
    }

    #[test]
    fn parse_db_host_port_no_credentials() {
        let (host, port) = parse_db_host_port("postgres://localhost/mydb").unwrap();
        assert_eq!(host, "localhost");
        assert_eq!(port, 5432);
    }

    #[test]
    fn parse_db_host_port_default_port() {
        let (host, port) = parse_db_host_port("postgres://user:pass@db.example.com/mydb").unwrap();
        assert_eq!(host, "db.example.com");
        assert_eq!(port, 5432);
    }

    #[test]
    fn parse_db_host_port_custom_port() {
        let (host, port) = parse_db_host_port("postgres://localhost:6543/test").unwrap();
        assert_eq!(host, "localhost");
        assert_eq!(port, 6543);
    }

    #[test]
    fn parse_db_host_port_unbracketed_ipv6_defaults_port() {
        let (host, port) = parse_db_host_port("postgres://::1/db").unwrap();
        assert_eq!(host, "::1");
        assert_eq!(port, 5432);
    }

    #[test]
    fn parse_db_host_port_bracketed_ipv6_custom_port() {
        let (host, port) = parse_db_host_port("postgres://[::1]:6543/db").unwrap();
        assert_eq!(host, "::1");
        assert_eq!(port, 6543);
    }

    #[test]
    fn parse_db_host_port_postgresql_scheme() {
        let (host, port) = parse_db_host_port("postgresql://localhost:5432/db").unwrap();
        assert_eq!(host, "localhost");
        assert_eq!(port, 5432);
    }

    #[test]
    fn parse_db_host_port_invalid_scheme() {
        assert!(parse_db_host_port("mysql://localhost/db").is_none());
    }

    // ── check_tailwind_binary_at ─────────────────────────────────────────────

    #[test]
    fn database_topology_doctor_fails_replica_without_primary() {
        let topology = DoctorDatabaseTopology {
            primary_url: None,
            replica_url: Some("postgres://user:secret@replica:5432/app".to_owned()),
            auto_migrate_in_production: false,
            replica_fallback: DoctorReplicaFallback::FailReadiness,
        };

        let result = check_database_topology_contract(&topology, true);

        assert_eq!(result.status, CheckStatus::Fail);
        assert!(result.detail.unwrap_or_default().contains("replica"));
        assert!(result.hint.unwrap_or_default().contains("primary"));
    }

    #[test]
    fn database_topology_doctor_fails_prod_replica_with_startup_migrations() {
        let topology = DoctorDatabaseTopology {
            primary_url: Some("postgres://user:secret@primary:5432/app".to_owned()),
            replica_url: Some("postgres://user:secret@replica:5432/app".to_owned()),
            auto_migrate_in_production: true,
            replica_fallback: DoctorReplicaFallback::FailReadiness,
        };

        let result = check_database_topology_contract(&topology, true);

        assert_eq!(result.status, CheckStatus::Fail);
        let detail = result.detail.unwrap_or_default();
        assert!(detail.contains("migration"));
        assert!(!detail.contains("secret"));
    }

    #[test]
    fn database_topology_doctor_fails_stale_replica_when_readiness_must_fail() {
        let result = check_replica_migration_versions(
            Some("20260510000000"),
            Some("20260509000000"),
            DoctorReplicaFallback::FailReadiness,
        );

        assert_eq!(result.status, CheckStatus::Fail);
        assert!(result.detail.unwrap_or_default().contains("replica"));
        assert!(result.hint.unwrap_or_default().contains("replay"));
    }

    #[test]
    fn database_topology_doctor_warns_stale_replica_when_falling_back_to_primary() {
        let result = check_replica_migration_versions(
            Some("20260510000000"),
            Some("20260509000000"),
            DoctorReplicaFallback::Primary,
        );

        assert_eq!(result.status, CheckStatus::Warn);
        assert!(result.detail.unwrap_or_default().contains("primary"));
    }

    #[test]
    fn database_topology_parses_latest_applied_migration_version() {
        let output = r"
            [X] 20260509000000_create_widgets
            [ ] 20260510000000_add_widget_index
            [X] 20260508000000_create_users
        ";

        assert_eq!(
            parse_latest_applied_migration_version(output).as_deref(),
            Some("20260509000000_create_widgets")
        );
    }

    #[test]
    fn database_topology_connectivity_names_role_without_credentials() {
        let result = check_db_role_connectivity(
            "primary",
            "postgres://user:secret@db:5432/app",
            |host, port| {
                assert_eq!(host, "db");
                assert_eq!(port, 5432);
                false
            },
        );

        assert_eq!(result.status, CheckStatus::Fail);
        let detail = result.detail.unwrap_or_default();
        assert!(detail.contains("primary"));
        assert!(detail.contains("db:5432"));
        assert!(!detail.contains("user"));
        assert!(!detail.contains("secret"));
    }

    fn temp_tailwind_path(temp: &tempfile::TempDir) -> std::path::PathBuf {
        let tailwind_name = if cfg!(windows) {
            "tailwindcss.exe"
        } else {
            "tailwindcss"
        };
        temp.path()
            .join("target")
            .join("autumn")
            .join(tailwind_name)
    }

    fn create_executable_tailwind_fixture(path: &std::path::Path) {
        let parent = path.parent().expect("tailwind path has parent");
        std::fs::create_dir_all(parent).expect("create tailwind parent");
        std::fs::copy(std::env::current_exe().expect("current test exe"), path)
            .expect("copy executable fixture");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mut permissions = std::fs::metadata(path)
                .expect("tailwind fixture metadata")
                .permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(path, permissions).expect("make tailwind fixture executable");
        }
    }

    #[test]
    fn check_tailwind_expected_path_directory_fails() {
        let temp = tempfile::tempdir().expect("temp dir");
        let tailwind_path = temp_tailwind_path(&temp);
        std::fs::create_dir_all(&tailwind_path).expect("create directory at tailwind path");

        let r = check_tailwind_binary_at(&tailwind_path);
        assert_eq!(r.status, CheckStatus::Fail);
        assert!(
            r.detail.as_deref().unwrap_or("").contains("directory"),
            "detail should identify directory path, got {r:?}"
        );
    }

    #[test]
    fn check_tailwind_not_found() {
        let temp = tempfile::tempdir().expect("temp dir");
        let r = check_tailwind_binary_at(&temp_tailwind_path(&temp));
        assert_eq!(r.status, CheckStatus::Fail);
        assert!(r.detail.as_deref().unwrap_or("").contains("not found"));
        assert!(r.hint.unwrap_or("").contains("autumn setup"));
    }

    #[test]
    fn check_tailwind_regular_file_without_execute_permission_fails() {
        let temp = tempfile::tempdir().expect("temp dir");
        let tailwind_path = temp_tailwind_path(&temp);
        std::fs::create_dir_all(tailwind_path.parent().expect("tailwind path has parent"))
            .expect("create tailwind parent");
        std::fs::write(&tailwind_path, "not an executable").expect("write non-executable file");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mut permissions = std::fs::metadata(&tailwind_path)
                .expect("tailwind file metadata")
                .permissions();
            permissions.set_mode(0o644);
            std::fs::set_permissions(&tailwind_path, permissions)
                .expect("clear executable permission");
        }

        let r = check_tailwind_binary_at(&tailwind_path);
        assert_eq!(r.status, CheckStatus::Fail);
        assert!(r.detail.as_deref().unwrap_or("").contains("not executable"));
        assert!(r.hint.unwrap_or("").contains("--force"));
    }

    #[test]
    fn check_tailwind_existing_file_fails_when_current_user_cannot_execute() {
        let temp = tempfile::tempdir().expect("temp dir");
        let tailwind_path = temp_tailwind_path(&temp);
        create_executable_tailwind_fixture(&tailwind_path);

        let r = check_tailwind_binary_at_with_executable_probe(&tailwind_path, |_, _| false);
        assert_eq!(r.status, CheckStatus::Fail);
        assert!(r.detail.as_deref().unwrap_or("").contains("not executable"));
    }

    #[test]
    fn check_tailwind_executable_file_passes() {
        let temp = tempfile::tempdir().expect("temp dir");
        let tailwind_path = temp_tailwind_path(&temp);
        create_executable_tailwind_fixture(&tailwind_path);

        let r = check_tailwind_binary_at(&tailwind_path);
        assert_eq!(r.status, CheckStatus::Pass);
        assert!(r.detail.as_deref().unwrap_or("").contains("executable"));
    }

    #[cfg(unix)]
    #[test]
    fn check_tailwind_broken_symlink_fails() {
        let temp = tempfile::tempdir().expect("temp dir");
        let tailwind_path = temp_tailwind_path(&temp);
        std::fs::create_dir_all(tailwind_path.parent().expect("tailwind path has parent"))
            .expect("create tailwind parent");
        std::os::unix::fs::symlink(temp.path().join("missing-tailwind"), &tailwind_path)
            .expect("create broken symlink");

        let r = check_tailwind_binary_at(&tailwind_path);
        assert_eq!(r.status, CheckStatus::Fail);
        assert!(r.detail.as_deref().unwrap_or("").contains("broken symlink"));
    }

    // ── check_signing_secret_impl (RED phase) ────────────────────────────────

    #[test]
    fn check_signing_secret_impl_name_is_signing_secret() {
        let r = check_signing_secret_impl(None, false);
        assert_eq!(r.name, "signing_secret");
    }

    #[test]
    fn check_signing_secret_impl_dev_no_secret_warns() {
        let r = check_signing_secret_impl(None, false);
        assert_eq!(r.status, CheckStatus::Warn);
        assert!(r.detail.as_deref().unwrap_or("").contains("ephemeral"));
    }

    #[test]
    fn check_signing_secret_impl_dev_with_valid_secret_passes() {
        let r = check_signing_secret_impl(Some("a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4"), false);
        assert_eq!(r.status, CheckStatus::Pass);
    }

    #[test]
    fn check_signing_secret_impl_prod_missing_fails() {
        let r = check_signing_secret_impl(None, true);
        assert_eq!(r.status, CheckStatus::Fail);
        assert!(r.hint.is_some());
    }

    #[test]
    fn check_signing_secret_impl_prod_too_short_fails() {
        let r = check_signing_secret_impl(Some("short"), true);
        assert_eq!(r.status, CheckStatus::Fail);
        assert!(r.detail.as_deref().unwrap_or("").contains("short"));
    }

    #[test]
    fn check_signing_secret_impl_prod_demo_value_fails() {
        let r = check_signing_secret_impl(Some("changeme"), true);
        assert_eq!(r.status, CheckStatus::Fail);
        assert!(r.hint.is_some());
    }

    #[test]
    fn check_signing_secret_impl_prod_valid_secret_passes() {
        let secret = "a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4";
        let r = check_signing_secret_impl(Some(secret), true);
        assert_eq!(r.status, CheckStatus::Pass);
    }

    #[test]
    fn check_signing_secret_impl_prod_hint_mentions_openssl() {
        let r = check_signing_secret_impl(None, true);
        let hint = r.hint.unwrap_or("");
        assert!(hint.contains("openssl") || hint.contains("rand"));
    }

    #[test]
    fn check_signing_secret_impl_dev_warn_has_hint() {
        let r = check_signing_secret_impl(None, false);
        assert!(r.hint.is_some());
    }

    // ── to_json_output ───────────────────────────────────────────────────────

    #[test]
    fn json_output_contains_checks_and_summary() {
        let results = vec![
            CheckResult {
                name: "rust_toolchain",
                status: CheckStatus::Pass,
                detail: Some("1.88.0".into()),
                hint: None,
            },
            CheckResult {
                name: "port_bindable",
                status: CheckStatus::Fail,
                detail: None,
                hint: Some("hint text"),
            },
        ];
        let summary = compute_summary(&results);
        let json = to_json_output(&results, &summary);

        assert!(json.contains("rust_toolchain"));
        assert!(json.contains("port_bindable"));
        assert!(json.contains("\"passed\": 1"));
        assert!(json.contains("\"failed\": 1"));
    }

    #[test]
    fn json_output_valid_json() {
        let results = vec![CheckResult {
            name: "test",
            status: CheckStatus::Pass,
            detail: None,
            hint: None,
        }];
        let summary = compute_summary(&results);
        let json = to_json_output(&results, &summary);
        // Should parse as valid JSON
        assert!(serde_json::from_str::<serde_json::Value>(&json).is_ok());
    }

    // ── check_rate_limit_key_strategy ────────────────────────────────────────

    #[test]
    fn rate_limit_key_strategy_ip_always_passes() {
        let r = check_rate_limit_key_strategy_impl("ip", false);
        assert_eq!(r.status, CheckStatus::Pass);
    }

    #[test]
    fn rate_limit_key_strategy_api_token_always_passes() {
        let r = check_rate_limit_key_strategy_impl("api_token", false);
        assert_eq!(r.status, CheckStatus::Pass);
    }

    #[test]
    fn rate_limit_key_strategy_authenticated_principal_without_auth_warns_in_strict() {
        let r = check_rate_limit_key_strategy_impl("authenticated_principal", false);
        assert_eq!(r.status, CheckStatus::Warn);
        assert!(r.hint.is_some());
        assert!(
            r.detail.as_deref().unwrap_or("").contains("auth extractor"),
            "detail should mention auth extractor"
        );
    }

    #[test]
    fn rate_limit_key_strategy_authenticated_principal_with_auth_passes() {
        let r = check_rate_limit_key_strategy_impl("authenticated_principal", true);
        assert_eq!(r.status, CheckStatus::Pass);
    }

    #[test]
    fn rate_limit_key_strategy_name_is_stable() {
        let r = check_rate_limit_key_strategy_impl("authenticated_principal", false);
        assert_eq!(r.name, "rate_limit_key_strategy");
    }

    #[test]
    fn rate_limit_key_strategy_empty_strategy_passes() {
        // Unconfigured / empty strategy is treated as default (ip).
        let r = check_rate_limit_key_strategy_impl("", false);
        assert_eq!(r.status, CheckStatus::Pass);
    }

    #[test]
    fn rate_limit_key_strategy_invalid_value_fails() {
        let r = check_rate_limit_key_strategy_impl("authenticated_principals", false);
        assert_eq!(r.status, CheckStatus::Fail);
        assert!(
            r.detail
                .as_deref()
                .unwrap_or("")
                .contains("not a valid strategy")
        );
    }

    // ── check_maintenance_mode ────────────────────────────────────────────────

    #[test]
    fn check_maintenance_mode_passes_when_off() {
        // No flag file in the test dir → maintenance is off → Pass
        let result = check_maintenance_mode();
        // In CI there should be no flag file; if there is, accept Warn too.
        assert!(
            result.status == CheckStatus::Pass || result.status == CheckStatus::Warn,
            "unexpected status: {:?}",
            result.status
        );
    }

    // ── check_oauth2 (RED phase) ──────────────────────────────────────────────

    #[test]
    fn check_oauth2_empty_client_id_in_production_fails() {
        let result = check_oauth2_provider_impl("github", "", "real-secret-value", true);
        assert_eq!(
            result.status,
            CheckStatus::Fail,
            "empty client_id in production must fail: {result:?}",
        );
        assert!(
            result.detail.as_deref().unwrap_or("").contains("client_id"),
            "detail must mention client_id: {:?}",
            result.detail
        );
    }

    #[test]
    fn check_oauth2_empty_client_secret_in_production_fails() {
        let result = check_oauth2_provider_impl("github", "cid", "", true);
        assert_eq!(
            result.status,
            CheckStatus::Fail,
            "empty client_secret in production must fail: {result:?}",
        );
        assert!(
            result
                .detail
                .as_deref()
                .unwrap_or("")
                .contains("client_secret"),
            "detail must mention client_secret: {:?}",
            result.detail
        );
    }

    #[test]
    fn check_oauth2_empty_client_secret_in_dev_warns() {
        let result = check_oauth2_provider_impl("github", "cid", "", false);
        assert_eq!(
            result.status,
            CheckStatus::Warn,
            "empty client_secret outside production must warn: {result:?}",
        );
    }

    #[test]
    fn check_oauth2_non_empty_client_secret_passes() {
        let result = check_oauth2_provider_impl("github", "cid", "real-secret-value", true);
        assert_eq!(
            result.status,
            CheckStatus::Pass,
            "non-empty client_secret must pass: {result:?}",
        );
    }

    #[test]
    fn check_oauth2_provider_name_appears_in_check_name() {
        let result = check_oauth2_provider_impl("google", "cid", "", true);
        assert!(
            result.name.contains("oauth2") || result.name.contains("google"),
            "check name must identify the provider: {}",
            result.name
        );
    }

    #[test]
    fn resolve_oauth2_providers_prefers_env_var_over_empty_toml_secret() {
        let toml: toml::Table = toml::from_str(
            r#"
[auth.oauth2.github]
client_id = "cid"
client_secret = ""
authorize_url = "https://github.com/login/oauth/authorize"
token_url = "https://github.com/login/oauth/access_token"
redirect_uri = "http://localhost/callback"
"#,
        )
        .unwrap();

        let providers = resolve_oauth2_providers_from_sources(
            |key| {
                if key == "AUTUMN_AUTH__OAUTH2__GITHUB__CLIENT_SECRET" {
                    Some("ghp_test_secret".to_owned())
                } else {
                    None
                }
            },
            Some(&toml),
            "dev",
        );

        let p = providers.into_iter().find(|p| p.name == "github").unwrap();
        assert_eq!(
            p.client_secret, "ghp_test_secret",
            "env var must override empty TOML client_secret"
        );
    }

    // ── check_compression ────────────────────────────────────────────────────

    #[test]
    fn compression_warns_in_production_when_disabled() {
        let result = check_compression_impl(false, true);
        assert_eq!(result.name, "compression");
        assert!(
            matches!(result.status, CheckStatus::Warn),
            "expected Warn, got {:?}",
            result.status
        );
    }

    #[test]
    fn compression_passes_in_production_when_enabled() {
        let result = check_compression_impl(true, true);
        assert_eq!(result.name, "compression");
        assert!(
            matches!(result.status, CheckStatus::Pass),
            "expected Pass, got {:?}",
            result.status
        );
    }

    #[test]
    fn compression_passes_in_dev_when_disabled() {
        let result = check_compression_impl(false, false);
        assert_eq!(result.name, "compression");
        assert!(
            matches!(result.status, CheckStatus::Pass),
            "expected Pass in dev profile, got {:?}",
            result.status
        );
    }

    // ── check_dotenv ─────────────────────────────────────────────────────────

    #[test]
    fn dotenv_warns_when_example_present_without_env() {
        let result = check_dotenv_impl(true, false, &[]);
        assert_eq!(result.name, "dotenv");
        assert!(
            matches!(result.status, CheckStatus::Warn),
            "expected Warn, got {:?}",
            result.status
        );
        assert!(result.detail.as_deref().unwrap().contains(".env.example"));
    }

    #[test]
    fn dotenv_warns_when_env_present_but_not_gitignored() {
        let result = check_dotenv_impl(true, true, &[".env".to_owned()]);
        assert_eq!(result.name, "dotenv");
        assert!(
            matches!(result.status, CheckStatus::Warn),
            "expected Warn, got {:?}",
            result.status
        );
        let detail = result.detail.as_deref().unwrap();
        assert!(detail.contains("not gitignored"), "got: {detail:?}");
        assert!(detail.contains(".env"), "got: {detail:?}");
    }

    #[test]
    fn dotenv_warns_when_env_local_present_but_not_gitignored() {
        // A `.env.local` with no root `.env` must still warn: the loader reads
        // it and it is always secret.
        let result = check_dotenv_impl(false, false, &[".env.local".to_owned()]);
        assert_eq!(result.name, "dotenv");
        assert!(
            matches!(result.status, CheckStatus::Warn),
            "expected Warn, got {:?}",
            result.status
        );
        assert!(
            result.detail.as_deref().unwrap().contains(".env.local"),
            "detail should name the offending file: {:?}",
            result.detail
        );
    }

    #[test]
    fn dotenv_uncovered_secret_takes_priority_over_missing_env() {
        // Combined case: `.env.example` present, no root `.env`, and a present
        // `.env.local` that is NOT gitignored. The ungitignored-secret warning
        // must win (naming the file) rather than being hidden behind the
        // copy-the-template hint.
        let result = check_dotenv_impl(true, false, &[".env.local".to_owned()]);
        assert_eq!(result.name, "dotenv");
        assert!(
            matches!(result.status, CheckStatus::Warn),
            "expected Warn, got {:?}",
            result.status
        );
        let detail = result.detail.as_deref().unwrap();
        assert!(
            detail.contains(".env.local"),
            "detail must name the uncovered secret file: {detail:?}"
        );
        assert!(
            detail.contains("not gitignored"),
            "detail must lead with the security issue: {detail:?}"
        );
        // The gitignore hint is the actionable one for the security issue.
        assert!(
            result.hint.unwrap().contains(".gitignore"),
            "hint should point at .gitignore: {:?}",
            result.hint
        );
    }

    #[test]
    fn dotenv_warns_when_profile_local_present_but_not_gitignored() {
        let result = check_dotenv_impl(false, false, &[".env.dev.local".to_owned()]);
        assert!(
            matches!(result.status, CheckStatus::Warn),
            "expected Warn, got {:?}",
            result.status
        );
        assert!(
            result.detail.as_deref().unwrap().contains(".env.dev.local"),
            "got: {:?}",
            result.detail
        );
    }

    #[test]
    fn dotenv_passes_when_all_present_secret_files_gitignored() {
        // No uncovered secret files → Pass, even with a root `.env` present.
        let result = check_dotenv_impl(true, true, &[]);
        assert_eq!(result.name, "dotenv");
        assert!(
            matches!(result.status, CheckStatus::Pass),
            "expected Pass, got {:?}",
            result.status
        );
    }

    #[test]
    fn dotenv_passes_when_nothing_configured() {
        let result = check_dotenv_impl(false, false, &[]);
        assert_eq!(result.name, "dotenv");
        assert!(
            matches!(result.status, CheckStatus::Pass),
            "expected Pass, got {:?}",
            result.status
        );
    }

    #[test]
    fn is_secret_dotenv_filename_classifies_local_vs_committable() {
        assert!(is_secret_dotenv_filename(".env"));
        assert!(is_secret_dotenv_filename(".env.local"));
        assert!(is_secret_dotenv_filename(".env.dev.local"));
        assert!(is_secret_dotenv_filename(".env.production.local"));
        // Committable per-profile (non-local) files are NOT secret.
        assert!(!is_secret_dotenv_filename(".env.dev"));
        assert!(!is_secret_dotenv_filename(".env.production"));
        // The template is not itself a secret.
        assert!(!is_secret_dotenv_filename(".env.example"));
        assert!(!is_secret_dotenv_filename("env.local"));
    }

    #[test]
    fn gitignore_covers_truth_table() {
        // `.env` covers `.env` but NOT `.env.local`.
        assert!(gitignore_covers(".env\n", ".env"));
        assert!(!gitignore_covers(".env\n", ".env.local"));
        // `.env.*` covers `.env.local` but NOT `.env`.
        assert!(gitignore_covers(".env.*\n", ".env.local"));
        assert!(!gitignore_covers(".env.*\n", ".env"));
        // `.env*` covers both.
        assert!(gitignore_covers(".env*\n", ".env"));
        assert!(gitignore_covers(".env*\n", ".env.local"));
        // `*.local` and `.env.*.local` cover `.env.dev.local`.
        assert!(gitignore_covers("*.local\n", ".env.dev.local"));
        assert!(gitignore_covers(".env.*.local\n", ".env.dev.local"));
        // `.env.example` covers neither `.env` nor `.env.local`.
        assert!(!gitignore_covers(".env.example\n", ".env"));
        assert!(!gitignore_covers(".env.example\n", ".env.local"));
        // `**/.env` covers `.env`.
        assert!(gitignore_covers("**/.env\n", ".env"));
        // Leading `/` anchor and surrounding whitespace.
        assert!(gitignore_covers("/target\n.env\n", ".env"));
        assert!(gitignore_covers("  .env  \n", ".env"));
        // Comment, blank, and negation lines never count as coverage.
        assert!(!gitignore_covers("# .env\n\n", ".env"));
        assert!(!gitignore_covers("!.env\n", ".env"));
        assert!(!gitignore_covers("", ".env"));
        // A path pattern in a subdirectory does not cover a root-level file.
        assert!(!gitignore_covers("config/.env\n", ".env"));
        // A directory-only pattern does not cover a regular file.
        assert!(!gitignore_covers(".env/\n", ".env"));
        // Last-match-wins with `!` negations (gitignore precedence): a `!.env`
        // re-include after a broader ignore means `.env` is NOT covered.
        assert!(!gitignore_covers(".env*\n!.env\n", ".env"));
        assert!(!gitignore_covers(".env\n!.env\n", ".env"));
        // `.env*` still covers a sibling the negation does not re-include.
        assert!(gitignore_covers(".env*\n!.env\n", ".env.local"));
        // Order matters: a later ignore re-covers a file an earlier `!` freed.
        assert!(gitignore_covers("!.env\n.env\n", ".env"));
    }

    #[test]
    fn parse_config_bool_handles_false_values() {
        assert_eq!(parse_config_bool("false"), Some(false));
        assert_eq!(parse_config_bool("0"), Some(false));
        assert_eq!(parse_config_bool("no"), Some(false));
        assert_eq!(parse_config_bool("off"), Some(false));
        assert_eq!(parse_config_bool("true"), Some(true));
        assert_eq!(parse_config_bool("1"), Some(true));
        assert_eq!(parse_config_bool("yes"), Some(true));
        assert_eq!(parse_config_bool("on"), Some(true));
        assert_eq!(parse_config_bool("garbage"), None);
    }

    // ── check_proxy_conflict ─────────────────────────────────────────────────

    #[test]
    fn proxy_conflict_passes_when_only_new_fields_set() {
        let data = ProxyConflictData {
            new_ranges: vec!["10.0.0.0/8".into()],
            new_trust_fwd: true,
            new_hops: None,
            old_ranges: vec![],
            old_trust_fwd: false,
        };
        let r = check_proxy_conflict_impl(&data);
        assert_eq!(r.status, CheckStatus::Pass);
    }

    #[test]
    fn proxy_conflict_passes_when_only_old_fields_set() {
        let data = ProxyConflictData {
            new_ranges: vec![],
            new_trust_fwd: false,
            new_hops: None,
            old_ranges: vec!["10.0.0.0/8".into()],
            old_trust_fwd: true,
        };
        let r = check_proxy_conflict_impl(&data);
        assert_eq!(r.status, CheckStatus::Pass);
    }

    #[test]
    fn proxy_conflict_warns_when_both_set_with_different_ranges() {
        let data = ProxyConflictData {
            new_ranges: vec!["10.0.0.0/8".into()],
            new_trust_fwd: true,
            new_hops: None,
            old_ranges: vec!["192.168.0.0/16".into()],
            old_trust_fwd: true,
        };
        let r = check_proxy_conflict_impl(&data);
        assert_eq!(r.status, CheckStatus::Warn);
        assert!(r.hint.is_some());
    }

    #[test]
    fn proxy_conflict_warns_when_trusted_hops_set_alongside_old_fields() {
        let data = ProxyConflictData {
            new_ranges: vec![],
            new_trust_fwd: true,
            new_hops: Some(1),
            old_ranges: vec![],
            old_trust_fwd: true,
        };
        let r = check_proxy_conflict_impl(&data);
        assert_eq!(r.status, CheckStatus::Warn);
    }

    #[test]
    fn proxy_conflict_passes_when_matching_values_no_hops() {
        let data = ProxyConflictData {
            new_ranges: vec!["10.0.0.0/8".into()],
            new_trust_fwd: true,
            new_hops: None,
            old_ranges: vec!["10.0.0.0/8".into()],
            old_trust_fwd: true,
        };
        let r = check_proxy_conflict_impl(&data);
        assert_eq!(r.status, CheckStatus::Pass);
    }

    // ── system_test_browser ───────────────────────────────────────────────────

    #[test]
    fn browser_check_not_found_is_warn_not_fail() {
        // Simulate: no browser in an empty candidate list.  The check must
        // return Warn so projects that don't use system tests aren't penalized.
        let result = check_system_test_browser();
        // Accept Pass (if Chrome is on the host) or Warn (if not).
        assert!(
            result.status == CheckStatus::Pass || result.status == CheckStatus::Warn,
            "browser check must be Pass or Warn, got {:?}",
            result.status
        );
    }

    #[test]
    fn browser_candidate_paths_includes_common_locations() {
        let paths = browser_candidate_paths();
        let as_strs: Vec<_> = paths
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        assert!(
            as_strs
                .iter()
                .any(|s| s.contains("chromium") || s.contains("chrome")),
            "candidate list must include common Chrome paths; got {as_strs:?}"
        );
    }

    #[test]
    fn browser_check_hint_mentions_apt_get() {
        let result = check_system_test_browser();
        if result.status == CheckStatus::Warn {
            let hint = result.hint.unwrap_or("");
            assert!(
                hint.contains("apt-get") || hint.contains("AUTUMN_CHROMIUM"),
                "hint must mention install command; got: {hint}"
            );
        }
    }

    // ── cargo_toml_features_has_key ───────────────────────────────────────────

    #[test]
    fn features_has_key_detects_bare_key() {
        let toml = "[features]\nsystem-tests = [\"autumn-web/system-tests\"]\n";
        assert!(cargo_toml_features_has_key(toml, "system-tests"));
    }

    #[test]
    fn features_has_key_detects_quoted_key() {
        let toml = "[features]\n\"system-tests\" = [\"autumn-web/system-tests\"]\n";
        assert!(cargo_toml_features_has_key(toml, "system-tests"));
    }

    #[test]
    fn features_has_key_ignores_dev_dependency_mention() {
        // The key appears in [dev-dependencies] but NOT in [features].
        let toml = "[dev-dependencies]\nautumn-web = { features = [\"system-tests\"] }\n";
        assert!(!cargo_toml_features_has_key(toml, "system-tests"));
    }

    #[test]
    fn features_has_key_no_features_section() {
        let toml = "[package]\nname = \"x\"\n";
        assert!(!cargo_toml_features_has_key(toml, "system-tests"));
    }

    #[test]
    fn features_has_key_commented_header() {
        let toml = "[features] # project features\nsystem-tests = []\n";
        assert!(cargo_toml_features_has_key(toml, "system-tests"));
    }

    // ── check_gdpr_export_registration ───────────────────────────────────────

    #[test]
    fn gdpr_check_passes_when_no_auth_starter() {
        let r = check_gdpr_export_registration_impl(false, &[]);
        assert_eq!(
            r.status,
            CheckStatus::Pass,
            "no auth starter → should always pass: {r:?}"
        );
    }

    #[test]
    fn gdpr_check_passes_when_no_auth_starter_even_with_unregistered_tables() {
        let r = check_gdpr_export_registration_impl(false, &["posts", "comments"]);
        assert_eq!(
            r.status,
            CheckStatus::Pass,
            "no auth starter → must not warn about unregistered tables: {r:?}"
        );
    }

    #[test]
    fn gdpr_check_passes_when_all_tables_registered() {
        let r = check_gdpr_export_registration_impl(true, &[]);
        assert_eq!(
            r.status,
            CheckStatus::Pass,
            "auth starter present + all tables registered → should pass: {r:?}"
        );
    }

    #[test]
    fn gdpr_check_warns_when_unregistered_tables_present() {
        let r = check_gdpr_export_registration_impl(true, &["posts", "comments"]);
        assert_eq!(
            r.status,
            CheckStatus::Warn,
            "unregistered tables with auth starter must warn: {r:?}"
        );
    }

    #[test]
    fn gdpr_check_name_is_stable() {
        let r = check_gdpr_export_registration_impl(false, &[]);
        assert_eq!(r.name, "gdpr_export_registration");
    }

    #[test]
    fn gdpr_check_detail_lists_unregistered_table_names() {
        let r = check_gdpr_export_registration_impl(true, &["posts", "comments"]);
        let detail = r.detail.as_deref().unwrap_or("");
        assert!(
            detail.contains("posts"),
            "detail must mention unregistered table 'posts': {detail}"
        );
        assert!(
            detail.contains("comments"),
            "detail must mention unregistered table 'comments': {detail}"
        );
    }

    #[test]
    fn gdpr_check_hint_references_registry_api() {
        let r = check_gdpr_export_registration_impl(true, &["orders"]);
        let hint = r.hint.unwrap_or("");
        assert!(
            hint.contains("GdprRegistry") || hint.contains("gdpr"),
            "hint must reference the GdprRegistry API: {hint}"
        );
    }

    #[test]
    fn gdpr_check_detail_mentions_count_of_unregistered_tables() {
        let r = check_gdpr_export_registration_impl(true, &["t1", "t2", "t3"]);
        let detail = r.detail.as_deref().unwrap_or("");
        assert!(
            detail.contains('3') || detail.contains("3 "),
            "detail must mention the count of unregistered tables: {detail}"
        );
    }

    // ── scan_source_for_gdpr_registrations / extract_quoted_table_name ─────────

    #[test]
    fn scanner_detects_single_line_hard_delete() {
        let src = r#"registry.register(ModelRegistration::hard_delete("posts"));"#;
        let mut found = std::collections::HashSet::new();
        scan_source_for_gdpr_registrations(src, &mut found);
        assert!(
            found.contains("posts"),
            "should detect single-line hard_delete: {found:?}"
        );
    }

    #[test]
    fn scanner_detects_multiline_retain_call() {
        let src = "registry.register(ModelRegistration::retain(\n    \"invoices\",\n    \"7-year financial hold\",\n));";
        let mut found = std::collections::HashSet::new();
        scan_source_for_gdpr_registrations(src, &mut found);
        assert!(
            found.contains("invoices"),
            "should detect multi-line retain registration: {found:?}"
        );
    }

    #[test]
    fn scanner_detects_multiline_anonymize_call() {
        let src = "registry.register(ModelRegistration::anonymize(\n    \"comments\",\n));";
        let mut found = std::collections::HashSet::new();
        scan_source_for_gdpr_registrations(src, &mut found);
        assert!(
            found.contains("comments"),
            "should detect multi-line anonymize registration: {found:?}"
        );
    }

    #[test]
    fn scanner_ignores_string_before_token() {
        let src = r#"let _x = "not_a_table"; ModelRegistration::hard_delete("real_table");"#;
        let mut found = std::collections::HashSet::new();
        scan_source_for_gdpr_registrations(src, &mut found);
        assert!(
            found.contains("real_table"),
            "should contain real_table: {found:?}"
        );
        assert!(
            !found.contains("not_a_table"),
            "should not contain string before token: {found:?}"
        );
    }

    // ── Deprecated-keys check tests ───────────────────────────────────────────

    #[test]
    fn red_check_deprecated_keys_empty_is_pass() {
        let result = check_deprecated_keys_impl(&[]);
        assert_eq!(result.status, CheckStatus::Pass);
        assert_eq!(result.name, "deprecated_keys");
    }

    #[test]
    fn red_check_deprecated_keys_warns_one_line_per_key() {
        let found = vec![
            DoctorDeprecation {
                path: "security.rate_limit.trusted_proxies".into(),
                replacement: Some("security.trusted_proxies.ranges".into()),
                since: "0.5.0".into(),
                remove_in: "1.0.0".into(),
            },
            DoctorDeprecation {
                path: "security.rate_limit.trust_forwarded_headers".into(),
                replacement: Some("security.trusted_proxies.trust_forwarded_headers".into()),
                since: "0.5.0".into(),
                remove_in: "1.0.0".into(),
            },
        ];
        let result = check_deprecated_keys_impl(&found);
        assert_eq!(result.status, CheckStatus::Warn);
        assert_eq!(result.name, "deprecated_keys");
        let detail = result.detail.unwrap();
        let lines: Vec<&str> = detail.lines().collect();
        assert_eq!(lines.len(), 2, "one line per key: {detail:?}");
        assert!(lines[0].contains("security.rate_limit.trusted_proxies"));
        assert!(lines[0].contains("security.trusted_proxies.ranges"));
        assert!(
            lines[0].contains("0.5.0"),
            "detail must include since version"
        );
        assert!(lines[0].contains("1.0.0"));
        assert!(lines[1].contains("security.rate_limit.trust_forwarded_headers"));
    }

    #[test]
    fn red_check_deprecated_keys_json_includes_detail() {
        let found = vec![DoctorDeprecation {
            path: "security.rate_limit.trusted_proxies".into(),
            replacement: Some("security.trusted_proxies.ranges".into()),
            since: "0.5.0".into(),
            remove_in: "1.0.0".into(),
        }];
        let results = vec![check_deprecated_keys_impl(&found)];
        let summary = compute_summary(&results);
        let json = to_json_output(&results, &summary);
        assert!(
            json.contains("\"deprecated_keys\""),
            "JSON must name the check"
        );
        assert!(json.contains("\"warn\""), "JSON must include warn status");
        assert!(
            json.contains("security.rate_limit.trusted_proxies"),
            "JSON must contain the deprecated key path"
        );
    }
}
