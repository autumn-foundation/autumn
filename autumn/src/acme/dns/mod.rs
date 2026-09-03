//! DNS-01 challenge support: wildcard certificates for subdomain-per-tenant
//! deployments (issue #1620).
//!
//! [#1608](https://github.com/autumn-foundation/autumn/issues/1608) shipped ACME
//! over **HTTP-01**, which no CA will accept for a wildcard identifier. This
//! module adds the other half: proving control of a zone by publishing a
//! `_acme-challenge.<domain>` TXT record, so one `*.myapp.com` certificate
//! serves every tenant subdomain — including tenants that do not exist yet.
//!
//! Everything else — the order flow, the store, the renewal loop, the hot-swap,
//! staging selection, health — is [#1608's](super) and is reused verbatim. Only
//! the challenge answer changes.
//!
//! # Shape
//!
//! - [`DnsProvider`] is the one seam: write a TXT record, delete a TXT record.
//!   Implementations live in [`cloudflare`], [`route53`], and [`exec`] (the
//!   documented escape hatch for every other provider).
//! - [`DnsCredential`] is the provider's API credential, read from the encrypted
//!   credentials store or the documented environment variables — never from
//!   `autumn.toml`. Its secret fields are [`SecretString`], whose `Debug` and
//!   `Display` are redacted so a token cannot reach a log line, an error
//!   message, or actuator output by accident.
//! - [`resolver`] answers "is the record visible yet?" against public resolvers,
//!   bounded by a configured timeout whose error names the exact record.
//!
//! # Two authorizations, one record name
//!
//! An order for `myapp.com` **and** `*.myapp.com` yields two authorizations
//! whose DNS-01 records share the name `_acme-challenge.myapp.com` but carry
//! **different** values. Both must be live simultaneously, so every provider
//! here appends a value and deletes by `(name, value)` — never "replace the
//! record set" or "delete by name".

pub mod cloudflare;
pub mod exec;
pub mod http;
pub mod resolver;
pub mod route53;

use std::sync::Arc;

use futures::future::BoxFuture;

use crate::config::{AcmeDnsConfig, AcmeDnsProvider};

/// The DNS-01 record name for `domain`, per RFC 8555 §8.4.
///
/// The identifier is always the **base** domain: an authorization for
/// `*.myapp.com` carries the identifier `myapp.com`, so both the apex and the
/// wildcard land on `_acme-challenge.myapp.com`.
#[must_use]
pub fn challenge_fqdn(domain: &str) -> String {
    format!(
        "_acme-challenge.{}",
        domain.trim().trim_end_matches('.').to_ascii_lowercase()
    )
}

/// One `_acme-challenge` TXT record to publish or remove.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxtRecord {
    /// Fully-qualified record name, e.g. `_acme-challenge.myapp.com`.
    pub fqdn: String,
    /// The challenge value — `base64url(sha256(key_authorization))`. Public by
    /// construction (it is published in DNS), so it is safe in error messages.
    pub value: String,
}

impl TxtRecord {
    /// Build a record for `domain`'s DNS-01 challenge with `value`.
    #[must_use]
    pub fn new(domain: &str, value: impl Into<String>) -> Self {
        Self {
            fqdn: challenge_fqdn(domain),
            value: value.into(),
        }
    }
}

/// Writes and removes the ephemeral `_acme-challenge` TXT records an ACME
/// DNS-01 challenge needs.
///
/// Implementations MUST be additive: [`upsert_txt`](Self::upsert_txt) adds a
/// value alongside any already present at the same name, and
/// [`delete_txt`](Self::delete_txt) removes only the `(name, value)` pair it is
/// given. An order for an apex plus its wildcard publishes two different values
/// at one name, and clobbering either fails the order.
///
/// Errors are plain strings that reach logs, `/actuator/health`, and the
/// operator-alert payload — so an implementation must never interpolate a
/// credential into one.
pub trait DnsProvider: Send + Sync {
    /// The provider's stable name, for logs and health details.
    fn name(&self) -> &'static str;

    /// Publish `record`, leaving any other value at the same name in place.
    fn upsert_txt<'a>(&'a self, record: &'a TxtRecord) -> BoxFuture<'a, Result<(), String>>;

    /// Remove exactly the `(name, value)` pair in `record`.
    fn delete_txt<'a>(&'a self, record: &'a TxtRecord) -> BoxFuture<'a, Result<(), String>>;
}

/// How much text copied from a DNS provider (an API error body, a hook's
/// `stderr`) may reach an operator-facing message.
///
/// Bounded because that message is published on `/actuator/health` and pushed to
/// the operator's alert destination; an unbounded upstream body would become an
/// unbounded health payload.
pub const UPSTREAM_EXCERPT_CHARS: usize = 400;

/// Make upstream text safe to publish in an issuance error.
///
/// Autumn's own error strings never contain a credential, but the text it copies
/// **in** from a provider is not under its control, and that text is published
/// verbatim on the unauthenticated `/actuator/health` and to the operator's
/// alert destination (Slack, PagerDuty, email). Two real shapes make that
/// dangerous:
///
/// - AWS answers a `SignatureDoesNotMatch` by echoing the canonical request it
///   expected — which contains every signed header line, including
///   `x-amz-security-token: <the STS session token>`;
/// - a shell hook written with `set -x` traces its own
///   `curl -H "Authorization: Bearer $TOKEN"` to `stderr`.
///
/// So every secret still live in this process is replaced with `<redacted>`
/// before publishing, control characters (which would let upstream text forge
/// log lines or inject ANSI escapes into `autumn doctor` output) are stripped,
/// and the result is truncated to [`UPSTREAM_EXCERPT_CHARS`].
#[must_use]
pub fn sanitize_upstream(text: &str, secrets: &[&str]) -> String {
    let mut cleaned: String = text
        .chars()
        .map(|c| {
            if c == '\t' {
                ' '
            } else if c.is_control() {
                ' '
            } else {
                c
            }
        })
        .collect();
    for secret in secrets {
        let secret = secret.trim();
        // A very short "secret" would redact half the message; the shortest
        // credential any supported provider issues is far longer than this.
        if secret.len() >= 8 {
            cleaned = cleaned.replace(secret, "<redacted>");
        }
    }
    let cleaned = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    if cleaned.chars().count() <= UPSTREAM_EXCERPT_CHARS {
        return cleaned;
    }
    let mut truncated: String = cleaned.chars().take(UPSTREAM_EXCERPT_CHARS).collect();
    truncated.push('…');
    truncated
}

/// A string that must not reach a log line, an error message, or actuator
/// output.
///
/// `Debug` and `Display` render `<redacted>`; the value is reachable only
/// through [`expose`](Self::expose), which is deliberately noisy at the call
/// site. AC: "tokens never appear in logs, error messages, or actuator output".
#[derive(Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(transparent)]
pub struct SecretString(String);

impl SecretString {
    /// Wrap `value` as a secret.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The underlying secret. Only for handing to the provider's transport.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Whether the secret is empty or whitespace-only.
    #[must_use]
    pub fn is_blank(&self) -> bool {
        self.0.trim().is_empty()
    }
}

impl std::fmt::Debug for SecretString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

impl std::fmt::Display for SecretString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

/// A DNS provider's API credential, read from the encrypted credentials store
/// (`autumn credentials edit`) or the documented environment variables.
///
/// Every field is optional because the shape differs per provider;
/// [`validate_credential`] checks the ones a given provider actually needs and
/// names the missing key. Never deserialized from `autumn.toml` — the config
/// section has no field that could hold one.
#[derive(Clone, Default, serde::Deserialize)]
#[serde(default)]
pub struct DnsCredential {
    /// Cloudflare scoped API token (`Zone:DNS:Edit` on the zone).
    pub api_token: Option<SecretString>,
    /// AWS access key id (Route 53).
    pub access_key_id: Option<String>,
    /// AWS secret access key (Route 53).
    pub secret_access_key: Option<SecretString>,
    /// AWS session token, for temporary credentials (Route 53).
    pub session_token: Option<SecretString>,
    /// Explicit Route 53 hosted zone id, skipping the zone lookup.
    pub hosted_zone_id: Option<String>,
    /// AWS region used for `SigV4` signing. Route 53 is global; defaults to
    /// `us-east-1`.
    pub region: Option<String>,
}

impl std::fmt::Debug for DnsCredential {
    /// Redacted wholesale: even the non-secret fields are omitted so a
    /// `{:?}` of a config struct can never become a credential dump.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DnsCredential(<redacted>)")
    }
}

/// Environment-variable names that can supply each credential field, for
/// operators who inject secrets through the environment rather than the
/// encrypted store. Documented in the TLS guide.
const ENV_API_TOKEN: &str = "AUTUMN_ACME_DNS_API_TOKEN";
const ENV_ACCESS_KEY_ID: &str = "AUTUMN_ACME_DNS_ACCESS_KEY_ID";
const ENV_SECRET_ACCESS_KEY: &str = "AUTUMN_ACME_DNS_SECRET_ACCESS_KEY";
const ENV_SESSION_TOKEN: &str = "AUTUMN_ACME_DNS_SESSION_TOKEN";
const ENV_HOSTED_ZONE_ID: &str = "AUTUMN_ACME_DNS_HOSTED_ZONE_ID";
const ENV_REGION: &str = "AUTUMN_ACME_DNS_REGION";

impl DnsCredential {
    /// Overlay any of the `AUTUMN_ACME_DNS_*` environment variables onto `self`.
    ///
    /// The environment wins over the encrypted store, so a container can inject
    /// a rotated token without rewriting the credentials file. `env` is injected
    /// rather than read from the process so this is unit-testable without
    /// mutating global state.
    #[must_use]
    pub fn with_env(mut self, env: &dyn Fn(&str) -> Option<String>) -> Self {
        fn non_blank(value: Option<String>) -> Option<String> {
            value.filter(|v| !v.trim().is_empty())
        }
        if let Some(v) = non_blank(env(ENV_API_TOKEN)) {
            self.api_token = Some(SecretString::new(v));
        }
        if let Some(v) = non_blank(env(ENV_ACCESS_KEY_ID)) {
            self.access_key_id = Some(v);
        }
        if let Some(v) = non_blank(env(ENV_SECRET_ACCESS_KEY)) {
            self.secret_access_key = Some(SecretString::new(v));
        }
        if let Some(v) = non_blank(env(ENV_SESSION_TOKEN)) {
            self.session_token = Some(SecretString::new(v));
        }
        if let Some(v) = non_blank(env(ENV_HOSTED_ZONE_ID)) {
            self.hosted_zone_id = Some(v);
        }
        if let Some(v) = non_blank(env(ENV_REGION)) {
            self.region = Some(v);
        }
        self
    }

    /// Read the credential for `dns_cfg` from `store`, then overlay the
    /// environment.
    #[must_use]
    pub fn resolve(
        dns_cfg: &AcmeDnsConfig,
        store: &crate::credentials::CredentialsStore,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Self {
        store
            .get::<Self>(dns_cfg.credential.trim())
            .unwrap_or_default()
            .with_env(env)
    }
}

/// Read an environment variable from the real process environment.
///
/// The default `env` argument for [`DnsCredential::resolve`] in production.
#[must_use]
pub fn process_env(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

/// Check that `credential` carries the fields `provider` needs, naming the
/// missing key and where to put it.
///
/// Pure, so `autumn doctor` grades the same rule the runtime enforces
/// (issue #1620 AC: "`autumn doctor` diagnoses … missing or invalid provider
/// credential").
///
/// # Errors
///
/// Returns an operator-actionable message describing the first missing or blank
/// field. Never echoes a credential value.
pub fn validate_credential(
    provider: AcmeDnsProvider,
    credential_key: &str,
    credential: &DnsCredential,
) -> Result<(), String> {
    let where_to_put = |field: &str, env: &str| {
        format!(
            "add it under `[{credential_key}]` in the encrypted credentials store \
             (`autumn credentials edit`) as `{field} = \"...\"`, or set the {env} environment \
             variable"
        )
    };
    match provider {
        AcmeDnsProvider::Cloudflare => match &credential.api_token {
            Some(token) if !token.is_blank() => Ok(()),
            Some(_) => Err(format!(
                "the Cloudflare DNS credential `{credential_key}` has a blank `api_token`: {}",
                where_to_put("api_token", ENV_API_TOKEN)
            )),
            None => Err(format!(
                "no Cloudflare API token found for [server.tls.acme.dns] credential \
                     `{credential_key}`: {}",
                where_to_put("api_token", ENV_API_TOKEN)
            )),
        },
        AcmeDnsProvider::Route53 => {
            let id_ok = credential
                .access_key_id
                .as_ref()
                .is_some_and(|v| !v.trim().is_empty());
            if !id_ok {
                return Err(format!(
                    "no AWS access key id found for [server.tls.acme.dns] credential \
                     `{credential_key}`: {}",
                    where_to_put("access_key_id", ENV_ACCESS_KEY_ID)
                ));
            }
            let secret_ok = credential
                .secret_access_key
                .as_ref()
                .is_some_and(|v| !v.is_blank());
            if !secret_ok {
                return Err(format!(
                    "no AWS secret access key found for [server.tls.acme.dns] credential \
                     `{credential_key}`: {}",
                    where_to_put("secret_access_key", ENV_SECRET_ACCESS_KEY)
                ));
            }
            Ok(())
        }
        // The hook program authenticates itself (an `nsupdate` TSIG file, a
        // provider CLI's own profile); autumn holds no credential for it.
        AcmeDnsProvider::Exec => Ok(()),
    }
}

/// Build the [`DnsProvider`] for `dns_cfg`, using `credential` and `transport`.
///
/// # Errors
///
/// Returns a message when the credential does not carry what the provider needs.
pub fn build_provider(
    dns_cfg: &AcmeDnsConfig,
    credential: &DnsCredential,
    transport: Arc<dyn http::HttpTransport>,
) -> Result<Arc<dyn DnsProvider>, String> {
    validate_credential(dns_cfg.provider, dns_cfg.credential.trim(), credential)?;
    Ok(match dns_cfg.provider {
        AcmeDnsProvider::Cloudflare => Arc::new(cloudflare::CloudflareProvider::new(
            credential
                .api_token
                .clone()
                .unwrap_or_else(|| SecretString::new(String::new())),
            transport,
        )),
        AcmeDnsProvider::Route53 => Arc::new(route53::Route53Provider::new(
            route53::Route53Credentials {
                access_key_id: credential.access_key_id.clone().unwrap_or_default(),
                secret_access_key: credential
                    .secret_access_key
                    .clone()
                    .unwrap_or_else(|| SecretString::new(String::new())),
                session_token: credential.session_token.clone(),
                region: credential
                    .region
                    .clone()
                    .unwrap_or_else(|| "us-east-1".to_owned()),
                hosted_zone_id: credential.hosted_zone_id.clone(),
            },
            transport,
        )),
        AcmeDnsProvider::Exec => Arc::new(exec::ExecProvider::new(dns_cfg.command.clone())),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn challenge_fqdn_is_the_base_domain_prefixed() {
        assert_eq!(challenge_fqdn("myapp.com"), "_acme-challenge.myapp.com");
        // A trailing dot and mixed case normalize to the same record name.
        assert_eq!(challenge_fqdn("MyApp.com."), "_acme-challenge.myapp.com");
        // An authorization for `*.myapp.com` carries the identifier `myapp.com`,
        // so apex and wildcard share one record name — see the module docs.
        assert_eq!(
            challenge_fqdn("myapp.com"),
            TxtRecord::new("myapp.com", "v").fqdn
        );
    }

    // AWS answers a SigV4 mismatch by echoing the canonical request, which
    // contains every signed header — including the STS session token. That reply
    // is published on the unauthenticated `/actuator/health` and pushed to the
    // operator's alert destination, so it must be scrubbed on the way in.
    #[test]
    fn upstream_text_is_scrubbed_of_live_secrets() {
        let token = "FwoGZXIvYXdzEHwaDNOT-A-REAL-SESSION-TOKEN";
        let aws_reply = format!(
            "The request signature we calculated does not match. The Canonical String for this \
             request should have been 'POST /2013-04-01/hostedzone/Z1/rrset/ \
             host:route53.amazonaws.com x-amz-security-token:{token} '"
        );
        let safe = sanitize_upstream(&aws_reply, &[token]);
        assert!(!safe.contains(token), "leaked: {safe}");
        assert!(safe.contains("<redacted>"), "got: {safe}");
        // …and the operator-actionable part survives.
        assert!(
            safe.contains("signature we calculated does not match"),
            "got: {safe}"
        );
    }

    #[test]
    fn upstream_text_is_bounded_and_stripped_of_control_characters() {
        let long = "x".repeat(UPSTREAM_EXCERPT_CHARS * 3);
        let bounded = sanitize_upstream(&long, &[]);
        assert!(bounded.chars().count() <= UPSTREAM_EXCERPT_CHARS + 1);
        assert!(bounded.ends_with('…'));

        // A hook that writes newlines or ANSI escapes must not be able to forge
        // log lines or repaint `autumn doctor`'s terminal output.
        let hostile = "line one\n2026-01-01 ERROR forged\r\n\u{1b}[31mred\u{1b}[0m";
        let safe = sanitize_upstream(hostile, &[]);
        assert!(!safe.contains('\n'), "got: {safe:?}");
        assert!(!safe.contains('\r'), "got: {safe:?}");
        assert!(!safe.contains('\u{1b}'), "got: {safe:?}");
        assert!(safe.contains("forged"), "the text itself is kept: {safe:?}");
    }

    // A short string is not treated as a secret: redacting on a 3-character
    // "secret" would blank out most of any message.
    #[test]
    fn a_too_short_secret_is_not_used_as_a_redaction_pattern() {
        assert_eq!(
            sanitize_upstream("the zone was not found", &["e"]),
            "the zone was not found"
        );
        assert_eq!(sanitize_upstream("a b c", &[""]), "a b c");
    }

    #[test]
    fn secret_string_never_renders_its_value() {
        let secret = SecretString::new("cf-super-secret-token");
        assert_eq!(format!("{secret:?}"), "<redacted>");
        assert_eq!(format!("{secret}"), "<redacted>");
        assert!(!format!("{secret:?} {secret}").contains("super-secret"));
        assert_eq!(secret.expose(), "cf-super-secret-token");
    }

    #[test]
    fn dns_credential_debug_is_redacted_wholesale() {
        let credential = DnsCredential {
            api_token: Some(SecretString::new("cf-super-secret-token")),
            access_key_id: Some("AKIAEXAMPLE".to_owned()),
            secret_access_key: Some(SecretString::new("aws-super-secret")),
            ..DnsCredential::default()
        };
        let rendered = format!("{credential:?}");
        assert!(!rendered.contains("super-secret"), "leaked: {rendered}");
        assert!(!rendered.contains("AKIAEXAMPLE"), "leaked: {rendered}");
    }

    #[test]
    fn env_overlays_the_stored_credential() {
        let credential = DnsCredential {
            api_token: Some(SecretString::new("from-store")),
            ..DnsCredential::default()
        };
        let env = |name: &str| match name {
            ENV_API_TOKEN => Some("from-env".to_owned()),
            ENV_REGION => Some("eu-west-1".to_owned()),
            // A blank environment variable must not shadow a real stored value.
            ENV_ACCESS_KEY_ID => Some("   ".to_owned()),
            _ => None,
        };
        let resolved = credential.with_env(&env);
        assert_eq!(resolved.api_token.as_ref().unwrap().expose(), "from-env");
        assert_eq!(resolved.region.as_deref(), Some("eu-west-1"));
        assert!(resolved.access_key_id.is_none());
    }

    #[test]
    fn validate_credential_names_the_missing_field_and_where_to_put_it() {
        let empty = DnsCredential::default();

        let err = validate_credential(AcmeDnsProvider::Cloudflare, "acme_dns", &empty)
            .expect_err("cloudflare needs an api_token");
        assert!(err.contains("api_token"), "got: {err}");
        assert!(err.contains("autumn credentials edit"), "got: {err}");
        assert!(err.contains("AUTUMN_ACME_DNS_API_TOKEN"), "got: {err}");
        assert!(err.contains("acme_dns"), "must name the key: {err}");

        let err = validate_credential(AcmeDnsProvider::Route53, "acme_dns", &empty)
            .expect_err("route53 needs an access key");
        assert!(err.contains("access_key_id"), "got: {err}");

        let partial = DnsCredential {
            access_key_id: Some("AKIAEXAMPLE".to_owned()),
            ..DnsCredential::default()
        };
        let err = validate_credential(AcmeDnsProvider::Route53, "acme_dns", &partial)
            .expect_err("route53 needs a secret too");
        assert!(err.contains("secret_access_key"), "got: {err}");

        // A blank token is as missing as an absent one.
        let blank = DnsCredential {
            api_token: Some(SecretString::new("   ")),
            ..DnsCredential::default()
        };
        assert!(validate_credential(AcmeDnsProvider::Cloudflare, "acme_dns", &blank).is_err());

        // The exec hook authenticates itself.
        assert!(validate_credential(AcmeDnsProvider::Exec, "acme_dns", &empty).is_ok());
    }

    // A credential error is surfaced to operators verbatim; it must describe the
    // problem without ever echoing the value it read.
    #[test]
    fn validate_credential_errors_never_echo_a_secret() {
        let credential = DnsCredential {
            api_token: Some(SecretString::new("   ")),
            access_key_id: Some("AKIAEXAMPLE".to_owned()),
            ..DnsCredential::default()
        };
        let err = validate_credential(AcmeDnsProvider::Route53, "acme_dns", &credential)
            .expect_err("missing secret_access_key");
        assert!(!err.contains("AKIAEXAMPLE"), "leaked: {err}");
    }
}
