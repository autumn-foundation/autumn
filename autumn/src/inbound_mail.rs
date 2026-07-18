//! First-class inbound email handling with provider webhooks and routing DSL.
//!
//! Three provider adapters ship in-tree, all available when the `inbound-mail`
//! Cargo feature is enabled:
//! - **Mailgun** — HTTP webhook with HMAC-SHA256 signature (also enable `inbound-mailgun`).
//! - **SES via SNS** — AWS SNS notification with subscription confirmation
//!   (also enable `inbound-ses`).
//! - **Generic** — raw RFC 5322 body for Postfix/relay setups.
//!
//! The `inbound-mailgun` and `inbound-ses` features are additive aliases that
//! imply `inbound-mail`. They exist to signal provider intent in `Cargo.toml`
//! and to allow future provider-specific optional dependencies.
//!
//! # Quick start
//!
//! ```toml
//! # Cargo.toml
//! autumn-web = { version = "...", features = ["inbound-mailgun"] }
//! ```
//!
//! ```rust,ignore
//! use autumn_web::inbound_mail::{InboundEmail, InboundMailEndpointConfig,
//!     InboundMailHandlerInfo, InboundMailRouter, ProcessingMode, RecipientPattern};
//!
//! fn handle_support(email: InboundEmail)
//!     -> std::pin::Pin<Box<dyn std::future::Future<Output = autumn_web::AutumnResult<()>>
//!        + Send + 'static>>
//! {
//!     Box::pin(async move {
//!         tracing::info!(from = %email.from, subject = %email.subject, "inbound support email");
//!         Ok(())
//!     })
//! }
//!
//! autumn_web::app()
//!     .inbound_mail_router(
//!         InboundMailRouter::new()
//!             .endpoint(InboundMailEndpointConfig::mailgun("/inbound/mailgun", "signing-key"))
//!             .handler(InboundMailHandlerInfo {
//!                 name: "support",
//!                 pattern: RecipientPattern::Exact("support@company.com".to_string()),
//!                 processing: ProcessingMode::Background,
//!                 handler: handle_support,
//!             })
//!     )
//!     .routes(routes![...])
//!     .run()
//!     .await;
//! ```

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use base64::Engine as _;
use bytes::Bytes;
use http::{HeaderMap, StatusCode};
use serde::Deserialize;

// ── SNS signature verification ────────────────────────────────────────────────

#[cfg(feature = "inbound-ses")]
mod sns_verify {
    use super::*;

    /// Validate that a `SigningCertURL` comes from the AWS SNS service.
    ///
    /// Accepted form: `https://sns.<region>.amazonaws.com/…` (or `.cn`).
    pub(super) fn is_valid_sns_cert_url(url: &str) -> bool {
        let Some(host) = url.strip_prefix("https://") else {
            return false;
        };
        let host = host.split('/').next().unwrap_or("");
        // Must be sns.<anything>.amazonaws.com or sns.<anything>.amazonaws.com.cn
        let base = host.strip_suffix(".cn").unwrap_or(host);
        base.ends_with(".amazonaws.com") && base.starts_with("sns.") && !host.contains("..")
    }

    /// Build the canonical string SNS uses for signing.
    pub(super) fn canonical_string(json: &serde_json::Value, msg_type: &str) -> Option<String> {
        let mut s = String::new();
        let fields: &[&str] = match msg_type {
            "Notification" => &[
                "Message",
                "MessageId",
                "Subject",
                "Timestamp",
                "TopicArn",
                "Type",
            ],
            "SubscriptionConfirmation" | "UnsubscribeConfirmation" => &[
                "Message",
                "MessageId",
                "SubscribeURL",
                "Timestamp",
                "Token",
                "TopicArn",
                "Type",
            ],
            _ => return None,
        };
        for field in fields {
            if let Some(v) = json.get(field).and_then(|v| v.as_str()) {
                s.push_str(field);
                s.push('\n');
                s.push_str(v);
                s.push('\n');
            }
        }
        Some(s)
    }

    // ── Minimal DER parser (X.509 SubjectPublicKeyInfo extraction) ────────────

    fn read_der_length(data: &[u8], pos: &mut usize) -> Option<usize> {
        let first = *data.get(*pos)?;
        *pos += 1;
        if first & 0x80 == 0 {
            return Some(first as usize);
        }
        let n = (first & 0x7f) as usize;
        if n == 0 || n > 4 {
            return None;
        }
        let mut len = 0usize;
        for _ in 0..n {
            len = (len << 8) | (*data.get(*pos)? as usize);
            *pos += 1;
        }
        Some(len)
    }

    fn skip_tlv(data: &[u8], pos: &mut usize) -> Option<()> {
        *pos += 1; // tag
        let len = read_der_length(data, pos)?;
        *pos = pos.checked_add(len).filter(|&e| e <= data.len())?;
        Some(())
    }

    fn enter_sequence(data: &[u8], pos: &mut usize) -> Option<(usize, usize)> {
        if *data.get(*pos)? != 0x30 {
            return None;
        }
        *pos += 1;
        let len = read_der_length(data, pos)?;
        let content_start = *pos;
        *pos = content_start
            .checked_add(len)
            .filter(|&e| e <= data.len())?;
        Some((content_start, len))
    }

    /// Extract the DER-encoded SubjectPublicKeyInfo from a DER X.509 certificate.
    pub(super) fn extract_spki_from_der(cert_der: &[u8]) -> Option<Vec<u8>> {
        let mut pos = 0;
        // Certificate SEQUENCE
        let (_, _) = enter_sequence(cert_der, &mut pos)?;
        // Restart pos at TBSCertificate
        let mut pos = 0;
        skip_tlv(cert_der, &mut pos)?; // skip outer tag+len, get to TBS content
        // Re-enter properly: outer Certificate SEQUENCE → TBSCertificate SEQUENCE
        let mut outer = 0usize;
        let (cert_start, cert_len) = enter_sequence(cert_der, &mut outer)?;
        let tbs_data = &cert_der[cert_start..cert_start + cert_len];

        let mut tbs = 0usize;
        let (tbs_start, tbs_len) = enter_sequence(tbs_data, &mut tbs)?;
        let tbs_content = &tbs_data[tbs_start..tbs_start + tbs_len];

        let mut p = 0usize;
        // Skip version [0] EXPLICIT (tag 0xa0) if present.
        if tbs_content.get(p) == Some(&0xa0) {
            skip_tlv(tbs_content, &mut p)?;
        }
        skip_tlv(tbs_content, &mut p)?; // serialNumber
        skip_tlv(tbs_content, &mut p)?; // signature AlgorithmIdentifier
        skip_tlv(tbs_content, &mut p)?; // issuer
        skip_tlv(tbs_content, &mut p)?; // validity
        skip_tlv(tbs_content, &mut p)?; // subject

        // subjectPublicKeyInfo starts here; capture the full TLV.
        let spki_start = p;
        skip_tlv(tbs_content, &mut p)?;
        Some(tbs_content[spki_start..p].to_vec())
    }

    /// Decode a PEM certificate (first CERTIFICATE block) to DER bytes.
    pub(super) fn pem_to_der(pem: &str) -> Option<Vec<u8>> {
        let start = pem.find("-----BEGIN CERTIFICATE-----")?;
        let after = &pem[start + "-----BEGIN CERTIFICATE-----".len()..];
        let end = after.find("-----END CERTIFICATE-----")?;
        let b64: String = after[..end]
            .chars()
            .filter(|c| !c.is_ascii_whitespace())
            .collect();
        base64::engine::general_purpose::STANDARD.decode(b64).ok()
    }

    /// Verify an RSA PKCS#1 v1.5 signature with SHA-1 or SHA-256.
    pub(super) fn verify_rsa_signature(
        spki_der: &[u8],
        message: &[u8],
        sig: &[u8],
        sig_version: &str,
    ) -> Result<(), ()> {
        use rsa::pkcs1v15::Pkcs1v15Sign;
        use rsa::pkcs8::DecodePublicKey as _;
        use sha2::Digest as _;

        let public_key = rsa::RsaPublicKey::from_public_key_der(spki_der).map_err(|_| ())?;
        match sig_version {
            "2" => {
                let hash = sha2::Sha256::digest(message);
                public_key
                    .verify(Pkcs1v15Sign::new::<sha2::Sha256>(), &hash, sig)
                    .map_err(|_| ())
            }
            _ => {
                // SignatureVersion 1 uses SHA-1.
                // sha1 0.10 uses const-oid 0.10.x while rsa 0.9 uses const-oid 0.9.x,
                // so Pkcs1v15Sign::new::<sha1::Sha1>() fails to compile.  Work around
                // the version split by using new_unprefixed() and prepending the
                // DigestInfo DER structure (RFC 3447 §9.2) manually.
                use sha1::Digest as _;
                let hash = sha1::Sha1::digest(message);
                // SHA-1 DigestInfo prefix: SEQUENCE { SEQUENCE { OID sha1, NULL }, OCTET STRING }
                const SHA1_DI_PREFIX: &[u8] = &[
                    0x30, 0x21, 0x30, 0x09, 0x06, 0x05, 0x2b, 0x0e, 0x03, 0x02, 0x1a, 0x05, 0x00,
                    0x04, 0x14,
                ];
                let mut digest_info = SHA1_DI_PREFIX.to_vec();
                digest_info.extend_from_slice(&hash);
                public_key
                    .verify(Pkcs1v15Sign::new_unprefixed(), &digest_info, sig)
                    .map_err(|_| ())
            }
        }
    }

    /// Verify the SNS notification signature and optional TopicArn binding.
    ///
    /// `expected_topic_arn` — when `Some`, the notification's `TopicArn` must
    /// match exactly; this prevents any other AWS account's SNS topic from
    /// posting forged (but validly-signed) messages to this endpoint.
    ///
    /// Set `AUTUMN_SES_SKIP_SNS_VERIFICATION=1` to disable in tests/local dev.
    pub(super) async fn verify(
        json: &serde_json::Value,
        http_client: &reqwest::Client,
        expected_topic_arn: Option<&str>,
    ) -> Result<(), StatusCode> {
        if super::SKIP_SNS_VERIFICATION.load(std::sync::atomic::Ordering::Relaxed)
            || std::env::var("AUTUMN_SES_SKIP_SNS_VERIFICATION")
                .ok()
                .as_deref()
                == Some("1")
        {
            return Ok(());
        }
        let msg_type = json.get("Type").and_then(|v| v.as_str()).unwrap_or("");
        let cert_url = json
            .get("SigningCertURL")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let sig_b64 = json.get("Signature").and_then(|v| v.as_str()).unwrap_or("");
        let sig_version = json
            .get("SignatureVersion")
            .and_then(|v| v.as_str())
            .unwrap_or("1");

        if !is_valid_sns_cert_url(cert_url) {
            tracing::warn!(cert_url, "inbound_mail.ses: invalid SNS SigningCertURL");
            return Err(StatusCode::UNAUTHORIZED);
        }
        let sig_bytes = base64::engine::general_purpose::STANDARD
            .decode(sig_b64)
            .map_err(|_| StatusCode::UNAUTHORIZED)?;
        let canonical = canonical_string(json, msg_type).ok_or(StatusCode::BAD_REQUEST)?;

        let cert_pem = http_client
            .get(cert_url)
            .send()
            .await
            .map_err(|e| {
                tracing::error!(error = %e, "inbound_mail.ses: failed to fetch SNS cert");
                StatusCode::BAD_GATEWAY
            })?
            .error_for_status()
            .map_err(|e| {
                tracing::error!(
                    error = %e,
                    "inbound_mail.ses: SNS cert endpoint returned an error status; \
                     treating as transient so SNS will retry"
                );
                StatusCode::SERVICE_UNAVAILABLE
            })?
            .text()
            .await
            .map_err(|_| StatusCode::BAD_GATEWAY)?;

        let cert_der = pem_to_der(&cert_pem).ok_or_else(|| {
            tracing::error!("inbound_mail.ses: could not parse SNS signing cert PEM");
            StatusCode::UNAUTHORIZED
        })?;
        let spki = extract_spki_from_der(&cert_der).ok_or_else(|| {
            tracing::error!("inbound_mail.ses: could not extract public key from SNS cert");
            StatusCode::UNAUTHORIZED
        })?;
        verify_rsa_signature(&spki, canonical.as_bytes(), &sig_bytes, sig_version).map_err(
            |_| {
                tracing::warn!("inbound_mail.ses: SNS signature verification failed");
                StatusCode::UNAUTHORIZED
            },
        )?;

        // Bind to the expected SNS topic so that signed messages from any other
        // topic are also rejected (prevents cross-account/cross-topic forgery).
        if let Some(expected) = expected_topic_arn {
            let actual = json.get("TopicArn").and_then(|v| v.as_str()).unwrap_or("");
            if actual != expected {
                tracing::warn!(
                    expected_topic_arn = expected,
                    actual_topic_arn = actual,
                    "inbound_mail.ses: TopicArn mismatch — request rejected"
                );
                return Err(StatusCode::UNAUTHORIZED);
            }
        }
        Ok(())
    }
}

// ── Test-bypass flag ─────────────────────────────────────────────────────────

/// When set to `true`, SNS signature verification is skipped entirely.
///
/// **Never set this in production.** It exists solely so integration tests can
/// call `SKIP_SNS_VERIFICATION.store(true, Ordering::Relaxed)` without needing
/// `unsafe` (which edition-2024 requires for `std::env::set_var`).
#[cfg(feature = "inbound-ses")]
pub static SKIP_SNS_VERIFICATION: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

// ── Signature helpers ─────────────────────────────────────────────────────────

/// Compute the Mailgun HMAC-SHA256 webhook signature.
///
/// Mailgun signs webhooks as `HMAC-SHA256(timestamp || token, signing_key)`.
/// Returns a lowercase hex string (64 characters).
///
/// # Panics
///
/// Panics if `signing_key` cannot be used to initialize the HMAC (which the
/// underlying library guarantees never happens for any key size).
#[must_use]
pub fn compute_mailgun_signature(timestamp: &str, token: &str, signing_key: &str) -> String {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;

    type HmacSha256 = Hmac<Sha256>;
    let mut mac =
        HmacSha256::new_from_slice(signing_key.as_bytes()).expect("HMAC can accept any key size");
    mac.update(timestamp.as_bytes());
    mac.update(token.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

// ── Core email type ───────────────────────────────────────────────────────────

/// A parsed inbound email message received via webhook or relay.
#[derive(Debug, Clone)]
pub struct InboundEmail {
    /// Sender address (From header).
    pub from: String,
    /// To recipients.
    pub to: Vec<String>,
    /// CC recipients.
    pub cc: Vec<String>,
    /// Message subject.
    pub subject: String,
    /// Plain-text body, if present.
    pub text_body: Option<String>,
    /// HTML body, if present.
    pub html_body: Option<String>,
    /// All message headers keyed by lower-cased name.
    pub headers: HashMap<String, String>,
    /// Parsed attachments.
    pub attachments: Vec<Attachment>,
    /// Provider spam verdict, when reported.
    pub spam_report: Option<SpamReport>,
    /// Raw RFC 5322 bytes of the original message.
    pub raw: Bytes,
    /// Plus-address tag captured by the routing DSL (e.g. `ticket-42` from
    /// `replies+ticket-42@app.example`).
    pub plus_token: Option<String>,
    /// Set to `true` only when the provider's top-level webhook field
    /// (e.g. Mailgun's `X-Mailgun-Bounced-Address` form field) signals a
    /// bounce.  Never derived from forwarded message headers so that a sender
    /// cannot spoof bounce routing by injecting that header into their email.
    pub is_bounce: bool,
    /// The provider-reported address that hard-bounced (e.g. Mailgun's
    /// `X-Mailgun-Bounced-Address` form field). Present only on bounce webhooks;
    /// this is the *failed recipient*, distinct from `to` (which carries the
    /// app's own inbound address on a bounce). Feeds the mail suppression list
    /// (issue #1247) via
    /// [`suppression::record_inbound`](crate::mail::suppression::record_inbound).
    pub bounced_address: Option<String>,
    /// The outbound recipient who filed a spam **complaint** (a feedback-loop /
    /// FBL event), as reported by the provider. Mirrors [`bounced_address`]:
    /// present only when the parsed payload carries a genuine complainant
    /// address, and — like `bounced_address` — never derived from `to` (which
    /// on an inbound webhook is the app's own inbound address, not the
    /// complainer). `None` for the current [`InboundMailProvider`] parsers,
    /// which surface an inbound *spam verdict* (`X-Mailgun-Sflag`) rather than
    /// an outbound FBL complaint; feeds
    /// [`suppression::record_inbound`](crate::mail::suppression::record_inbound)
    /// with [`SuppressionReason::Complaint`](crate::mail::suppression::SuppressionReason::Complaint)
    /// when a future/provider-specific parser populates it.
    ///
    /// [`bounced_address`]: Self::bounced_address
    pub complained_address: Option<String>,
}

impl InboundEmail {
    /// Return the captured plus-address tag, if any.
    #[must_use]
    pub fn plus_token(&self) -> Option<&str> {
        self.plus_token.as_deref()
    }

    /// Return the primary To address, if any.
    #[must_use]
    pub fn primary_recipient(&self) -> Option<&str> {
        self.to.first().map(String::as_str)
    }
}

/// An email attachment.
#[derive(Debug, Clone)]
pub struct Attachment {
    /// Filename from Content-Disposition, if present.
    pub filename: Option<String>,
    /// MIME content type.
    pub content_type: String,
    /// Raw attachment bytes.
    pub data: Bytes,
}

/// Provider spam report.
#[derive(Debug, Clone)]
pub struct SpamReport {
    /// Numeric spam score (higher = more likely spam).
    pub score: Option<f64>,
    /// Provider verdict string (e.g. `"Yes"`, `"No"`, `"Neutral"`).
    pub verdict: Option<String>,
}

// ── Provider enum ─────────────────────────────────────────────────────────────

/// The webhook provider that sends inbound mail to this endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InboundMailProvider {
    /// Mailgun HTTP webhook with HMAC-SHA256 signature verification.
    Mailgun,
    /// AWS SES delivered via SNS notification.
    Ses,
    /// Generic RFC 5322 raw-body endpoint (no provider-specific signature).
    #[default]
    Generic,
}

// ── Processing mode ───────────────────────────────────────────────────────────

/// Controls when the webhook returns 200 relative to handler execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProcessingMode {
    /// Wait for the handler to complete before responding 200.
    Sync,
    /// Return 200 immediately; run the handler in a background Tokio task.
    #[default]
    Background,
}

// ── Endpoint configuration ────────────────────────────────────────────────────

/// Configuration for one inbound mail HTTP endpoint.
///
/// Each endpoint configuration creates one POST route (e.g.
/// `POST /inbound/mailgun`) that verifies the provider signature and
/// dispatches the parsed email to the registered handlers.
#[derive(Debug, Clone)]
pub struct InboundMailEndpointConfig {
    /// Route path (e.g. `"/inbound/mailgun"`).
    pub path: String,
    /// Which provider sends to this endpoint.
    pub provider: InboundMailProvider,
    /// Current signing key (literal value).
    pub signing_key: Option<String>,
    /// Environment variable that holds the signing key at startup.
    pub signing_key_env: Option<String>,
    /// Default processing mode for all handlers on this endpoint.
    ///
    /// Note: this field is reserved for future use. Dispatch currently
    /// honours each [`InboundMailHandlerInfo::processing`] value directly;
    /// the endpoint-level default is not yet applied automatically.
    pub processing: ProcessingMode,
    /// Expected SNS `TopicArn` for SES endpoints (recommended).
    ///
    /// When set, the SNS signature verifier rejects any notification whose
    /// `TopicArn` field does not match this value.  This prevents a validly-
    /// signed message from a *different* SNS topic (possibly owned by another
    /// AWS account) from being accepted.  Leave `None` to skip the topic check.
    pub topic_arn: Option<String>,
}

impl Default for InboundMailEndpointConfig {
    fn default() -> Self {
        Self {
            path: "/inbound/mail".to_string(),
            provider: InboundMailProvider::Generic,
            signing_key: None,
            signing_key_env: None,
            processing: ProcessingMode::Background,
            topic_arn: None,
        }
    }
}

impl InboundMailEndpointConfig {
    /// Mailgun webhook endpoint with an explicit signing key.
    #[must_use]
    pub fn mailgun(path: impl Into<String>, signing_key: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            provider: InboundMailProvider::Mailgun,
            signing_key: Some(signing_key.into()),
            signing_key_env: None,
            processing: ProcessingMode::Background,
            topic_arn: None,
        }
    }

    /// AWS SES via SNS endpoint.
    ///
    /// No signing key is configured here: SNS subscription confirmation is
    /// handled automatically, and SNS message authenticity is verified via
    /// the `X-Amz-Sns-Message-Type` header.
    ///
    /// For production use, call [`.with_topic_arn`](Self::with_topic_arn) to
    /// restrict accepted notifications to your application's SNS topic.
    #[must_use]
    pub fn ses(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            provider: InboundMailProvider::Ses,
            signing_key: None,
            signing_key_env: None,
            processing: ProcessingMode::Background,
            topic_arn: None,
        }
    }

    /// Restrict this SES endpoint to notifications from a specific SNS topic.
    ///
    /// The `TopicArn` in each notification is checked against `arn` after
    /// signature verification.  Notifications from any other topic are
    /// rejected with 401.
    #[must_use]
    pub fn with_topic_arn(mut self, arn: impl Into<String>) -> Self {
        self.topic_arn = Some(arn.into());
        self
    }

    /// Generic RFC 5322 raw-body endpoint with optional HMAC signing.
    ///
    /// When `signing_key` / `signing_key_env` is set, the handler verifies
    /// `X-Inbound-Signature: HMAC-SHA256(key, body)` before parsing.
    #[must_use]
    pub fn generic(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            provider: InboundMailProvider::Generic,
            signing_key: None,
            signing_key_env: None,
            processing: ProcessingMode::Background,
            topic_arn: None,
        }
    }

    /// Resolve the signing key: literal value first, then env var.
    #[must_use]
    pub fn resolve_signing_key(&self) -> Option<String> {
        if self.signing_key.is_some() {
            return self.signing_key.clone();
        }
        self.signing_key_env
            .as_deref()
            .and_then(|var| std::env::var(var).ok())
    }
}

// ── Routing DSL ───────────────────────────────────────────────────────────────

/// Recipient address matching rule.
///
/// Handlers are tried in registration order; the first matching pattern wins.
#[derive(Debug, Clone)]
pub enum RecipientPattern {
    /// Exact address match (case-insensitive, e.g. `"support@company.com"`).
    Exact(String),
    /// Local-part prefix match.
    ///
    /// Matches any address whose local part (before `@`) starts with the given
    /// prefix. `"ticket"` matches `"ticket@example.com"` and
    /// `"ticket+123@example.com"`.
    LocalPrefix(String),
    /// Plus-address routing: `"{local}+{token}@{domain}"`.
    ///
    /// The captured `{token}` is available via [`InboundEmail::plus_token()`].
    /// When `domain` is `None` any domain matches.
    PlusAddress {
        /// Local part before the `+` (e.g. `"replies"`).
        local: String,
        /// Optional domain restriction (e.g. `"app.example"`).
        domain: Option<String>,
    },
    /// Matches every incoming message unconditionally.
    Any,
}

impl RecipientPattern {
    /// Return `true` if this pattern matches `address`.
    #[must_use]
    pub fn matches(&self, address: &str) -> bool {
        let lower = address.to_ascii_lowercase();
        match self {
            Self::Exact(expected) => lower == expected.to_ascii_lowercase(),
            Self::LocalPrefix(prefix) => {
                local_part(&lower).starts_with(&prefix.to_ascii_lowercase() as &str)
            }
            Self::PlusAddress { local, domain } => {
                let Some((addr_local, addr_domain)) = split_address(&lower) else {
                    return false;
                };
                let prefix = format!("{}+", local.to_ascii_lowercase());
                if !addr_local.starts_with(&prefix) {
                    return false;
                }
                domain
                    .as_ref()
                    .is_none_or(|dom| addr_domain == dom.to_ascii_lowercase())
            }
            Self::Any => true,
        }
    }

    /// Extract the plus-address token from `address` if this is a
    /// [`PlusAddress`](Self::PlusAddress) pattern and the address matches.
    ///
    /// The returned token preserves its original casing.
    #[must_use]
    pub fn extract_token(&self, address: &str) -> Option<String> {
        let Self::PlusAddress { local, domain } = self else {
            return None;
        };
        let lower = address.to_ascii_lowercase();
        let (addr_local_lower, addr_domain) = split_address(&lower)?;
        if let Some(dom) = domain
            && addr_domain != dom.to_ascii_lowercase()
        {
            return None;
        }
        let prefix = format!("{}+", local.to_ascii_lowercase());
        if !addr_local_lower.starts_with(&prefix) {
            return None;
        }
        // Extract from the original address so the token's casing is preserved.
        let (orig_local, _) = split_address(address)?;
        orig_local.get(prefix.len()..).map(str::to_string)
    }
}

fn split_address(addr: &str) -> Option<(&str, &str)> {
    let at = addr.rfind('@')?;
    Some((&addr[..at], &addr[at + 1..]))
}

fn local_part(addr: &str) -> &str {
    addr.rfind('@').map_or(addr, |at| &addr[..at])
}

// ── Handler types ─────────────────────────────────────────────────────────────

/// Async inbound mail handler function.
pub type InboundMailHandlerFn =
    fn(InboundEmail) -> Pin<Box<dyn Future<Output = crate::AutumnResult<()>> + Send + 'static>>;

/// A registered inbound mail handler with its routing metadata.
pub struct InboundMailHandlerInfo {
    /// Unique handler name used in logs and diagnostics.
    pub name: &'static str,
    /// Recipient pattern this handler matches.
    pub pattern: RecipientPattern,
    /// Whether to await the handler (Sync) or spawn it (Background).
    pub processing: ProcessingMode,
    /// The async handler function.
    pub handler: InboundMailHandlerFn,
}

// ── Router ────────────────────────────────────────────────────────────────────

/// Inbound mail router that wires webhook endpoints to handler functions.
///
/// Create with [`InboundMailRouter::new()`], chain builder methods, then pass
/// to [`AppBuilder::inbound_mail_router`](crate::app::AppBuilder::inbound_mail_router).
pub struct InboundMailRouter {
    pub(crate) endpoints: Vec<InboundMailEndpointConfig>,
    pub(crate) handlers: Vec<InboundMailHandlerInfo>,
    pub(crate) fallback: Option<InboundMailHandlerFn>,
    pub(crate) bounce_handler: Option<InboundMailHandlerFn>,
    pub(crate) spam_handler: Option<InboundMailHandlerFn>,
}

impl Default for InboundMailRouter {
    fn default() -> Self {
        Self::new()
    }
}

impl InboundMailRouter {
    /// Create a new, empty router.
    #[must_use]
    pub fn new() -> Self {
        Self {
            endpoints: Vec::new(),
            handlers: Vec::new(),
            fallback: None,
            bounce_handler: None,
            spam_handler: None,
        }
    }

    /// Add a provider endpoint. One HTTP POST route is created per call.
    #[must_use]
    pub fn endpoint(mut self, config: InboundMailEndpointConfig) -> Self {
        self.endpoints.push(config);
        self
    }

    /// Register a handler. Evaluated in registration order.
    #[must_use]
    pub fn handler(mut self, info: InboundMailHandlerInfo) -> Self {
        self.handlers.push(info);
        self
    }

    /// Register a catch-all fallback for messages that no handler matches.
    #[must_use]
    pub fn fallback(mut self, f: InboundMailHandlerFn) -> Self {
        self.fallback = Some(f);
        self
    }

    /// Register a handler for bounce events.
    ///
    /// Bounce detection is provider-specific. For Mailgun, the presence of
    /// an `X-Mailgun-Bounced-Address` form field signals a bounce event.
    #[must_use]
    pub fn on_bounce(mut self, f: InboundMailHandlerFn) -> Self {
        self.bounce_handler = Some(f);
        self
    }

    /// Register a handler invoked when the provider marks the message as spam
    /// (e.g. `X-Mailgun-Sflag: Yes`). This is a provider-side spam verdict, not
    /// a user-initiated complaint/FBL event.
    #[must_use]
    pub fn on_spam(mut self, f: InboundMailHandlerFn) -> Self {
        self.spam_handler = Some(f);
        self
    }

    /// Dispatch a parsed email to the first matching handler.
    ///
    /// Evaluation order:
    /// 1. Bounce handler (when `x-mailgun-bounced-address` header present).
    /// 2. Spam handler (when provider spam verdict is `"yes"`).
    /// 3. Registered handlers, in order.
    /// 4. Fallback handler, if registered.
    /// 5. Log + drop with a `WARN` trace.
    pub(crate) async fn dispatch(&self, mut email: InboundEmail) -> crate::AutumnResult<()> {
        // Bounce detection via provider-set flag (never derived from forwarded headers).
        if email.is_bounce
            && let Some(handler) = self.bounce_handler
        {
            return handler(email).await;
        }

        // Spam dispatch: provider-side spam verdict (e.g. X-Mailgun-Sflag: Yes).
        if email
            .spam_report
            .as_ref()
            .and_then(|r| r.verdict.as_deref())
            .map_or(false, |v| v.eq_ignore_ascii_case("yes"))
            && let Some(handler) = self.spam_handler
        {
            return handler(email).await;
        }

        for info in &self.handlers {
            // Any pattern must fire even when email.to is empty (e.g. Bcc-only delivery).
            let matched = if matches!(info.pattern, RecipientPattern::Any) {
                Some(email.to.first().cloned().unwrap_or_default())
            } else {
                email.to.iter().find(|r| info.pattern.matches(r)).cloned()
            };
            if let Some(recipient) = matched {
                if let Some(token) = info.pattern.extract_token(&recipient) {
                    email.plus_token = Some(token);
                }
                match info.processing {
                    ProcessingMode::Sync => return (info.handler)(email).await,
                    ProcessingMode::Background => {
                        let fut = (info.handler)(email);
                        tokio::spawn(async move {
                            if let Err(e) = fut.await {
                                tracing::error!(error = %e, "inbound_mail: background handler error");
                            }
                        });
                        return Ok(());
                    }
                }
            }
        }

        if let Some(fallback) = self.fallback {
            return fallback(email).await;
        }

        tracing::warn!(
            from = %email.from,
            to = ?email.to,
            subject = %email.subject,
            "inbound_mail.unmatched: no handler matched; message dropped"
        );
        Ok(())
    }
}

// ── Provider parsing ──────────────────────────────────────────────────────────

fn subtle_eq(a: &[u8], b: &[u8]) -> bool {
    use subtle::ConstantTimeEq as _;
    a.ct_eq(b).into()
}

/// Strip a display-name from an RFC 5322 address, returning only the addr-spec.
///
/// `"Support <support@company.com>"` → `"support@company.com"`
/// `"<support@company.com>"` → `"support@company.com"`
/// `"support@company.com"` → `"support@company.com"` (unchanged)
fn extract_addr_spec(addr: &str) -> String {
    if let Some(start) = addr.find('<')
        && let Some(rel_end) = addr[start + 1..].find('>')
    {
        return addr[start + 1..start + 1 + rel_end].trim().to_string();
    }
    addr.trim().to_string()
}

fn parse_address_list(s: &str) -> Vec<String> {
    if s.is_empty() {
        return Vec::new();
    }
    s.split(',')
        .map(str::trim)
        .filter(|a| !a.is_empty())
        .map(extract_addr_spec)
        .collect()
}

/// Parse and authenticate a Mailgun webhook form POST.
///
/// Returns `Ok(InboundEmail)` on valid signature, `Err(401)` on failure.
pub(crate) fn parse_mailgun(
    form: &HashMap<String, String>,
    signing_key: &str,
    file_parts: Vec<Attachment>,
) -> Result<InboundEmail, StatusCode> {
    // Refuse all requests when no signing key is configured to prevent trivial forgery.
    if signing_key.is_empty() {
        tracing::error!("inbound_mail.mailgun: signing key is empty; rejecting request");
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    }

    let timestamp = form.get("timestamp").map_or("", String::as_str);
    let token = form.get("token").map_or("", String::as_str);
    let signature = form.get("signature").map_or("", String::as_str);

    // Reject stale or future timestamps (5-minute window) to block replay attacks.
    let ts: i64 = timestamp.parse().map_err(|_| {
        tracing::warn!("inbound_mail.mailgun: missing or non-numeric timestamp");
        StatusCode::UNAUTHORIZED
    })?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
        .cast_signed();
    // Use abs_diff to avoid signed overflow when `ts` is an extreme value
    // (e.g. i64::MIN), which would panic in debug builds before the rejection runs.
    if now.abs_diff(ts) > 300 {
        tracing::warn!("inbound_mail.mailgun: timestamp outside 5-minute window");
        return Err(StatusCode::UNAUTHORIZED);
    }

    let expected = compute_mailgun_signature(timestamp, token, signing_key);
    if !subtle_eq(expected.as_bytes(), signature.as_bytes()) {
        tracing::warn!("inbound_mail.mailgun: invalid signature — request rejected");
        return Err(StatusCode::UNAUTHORIZED);
    }

    let from = form.get("from").cloned().unwrap_or_default();
    // Collect all recipients: RFC `To` header + Mailgun's envelope `recipient` field.
    let mut to = parse_address_list(form.get("to").map_or("", String::as_str));
    for addr in parse_address_list(form.get("recipient").map_or("", String::as_str)) {
        if !to.contains(&addr) {
            to.push(addr);
        }
    }
    let cc = parse_address_list(
        form.get("Cc")
            .or_else(|| form.get("cc"))
            .map_or("", String::as_str),
    );
    let subject = form.get("subject").cloned().unwrap_or_default();
    let text_body = form.get("body-plain").cloned().filter(|s| !s.is_empty());
    let html_body = form.get("body-html").cloned().filter(|s| !s.is_empty());

    let headers = parse_mailgun_headers(form.get("message-headers").map_or("", String::as_str));

    // Bounce detection: only trust the provider's top-level webhook field, never
    // forwarded message headers (which a sender could forge).
    let bounced_address = form
        .get("X-Mailgun-Bounced-Address")
        .or_else(|| form.get("x-mailgun-bounced-address"))
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty());
    let is_bounce = bounced_address.is_some();
    let final_headers = headers;

    let spam_score = form
        .get("X-Mailgun-Spam-Score")
        .or_else(|| form.get("x-mailgun-spam-score"))
        .and_then(|s| s.parse::<f64>().ok());
    let spam_verdict = form
        .get("X-Mailgun-Sflag")
        .or_else(|| form.get("x-mailgun-sflag"))
        .cloned();
    let spam_report = if spam_score.is_some() || spam_verdict.is_some() {
        Some(SpamReport {
            score: spam_score,
            verdict: spam_verdict,
        })
    } else {
        None
    };

    // Use actual file parts when available (multipart/form-data delivery); fall back
    // to metadata-only attachment stubs for URL-encoded deliveries where Mailgun
    // includes only attachment-count / attachment-{n} fields.
    let attachments = if !file_parts.is_empty() {
        file_parts
    } else {
        let attachment_count: usize = form
            .get("attachment-count")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0)
            .min(100);
        let mut metadata = Vec::new();
        for i in 1..=attachment_count {
            if let Some(name) = form.get(&format!("attachment-{i}")) {
                metadata.push(Attachment {
                    filename: Some(name.clone()),
                    content_type: "application/octet-stream".to_string(),
                    data: Bytes::new(),
                });
            }
        }
        metadata
    };

    Ok(InboundEmail {
        from,
        to,
        cc,
        subject,
        text_body,
        html_body,
        headers: final_headers,
        attachments,
        spam_report,
        raw: form
            .get("body-mime")
            .map(|s| Bytes::from(s.as_bytes().to_vec()))
            .unwrap_or_default(),
        plus_token: None,
        is_bounce,
        bounced_address,
        // Mailgun's inbound/spam-verdict payload carries no genuine outbound
        // complainant address (`X-Mailgun-Sflag` is an inbound spam score, not
        // an FBL complaint), so this stays `None`. A provider-specific
        // complaint parser would populate it.
        complained_address: None,
    })
}

fn parse_mailgun_headers(json: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    if json.is_empty() {
        return map;
    }
    if let Ok(serde_json::Value::Array(arr)) = serde_json::from_str(json) {
        for item in arr {
            if let serde_json::Value::Array(pair) = item
                && pair.len() == 2
                && let (Some(name), Some(value)) = (pair[0].as_str(), pair[1].as_str())
            {
                map.insert(name.to_ascii_lowercase(), value.to_string());
            }
        }
    }
    map
}

/// Parse and authenticate an SES-via-SNS notification.
///
/// Handles `SubscriptionConfirmation` and `Notification` (extracts the raw
/// email from the `Message` field). The `Type` field is read from the verified
/// JSON body rather than the unverified HTTP header.
pub(crate) fn parse_ses(body: &Bytes) -> Result<SnsParseResult, StatusCode> {
    let json: serde_json::Value =
        serde_json::from_slice(body).map_err(|_| StatusCode::BAD_REQUEST)?;
    let msg_type = json.get("Type").and_then(|t| t.as_str()).unwrap_or("");

    match msg_type {
        "SubscriptionConfirmation" => {
            let url = json
                .get("SubscribeURL")
                .and_then(|u| u.as_str())
                .unwrap_or("");
            tracing::info!(
                subscribe_url = %url,
                "inbound_mail.ses: SNS SubscriptionConfirmation received — confirming automatically"
            );
            Ok(SnsParseResult::SubscriptionConfirmation {
                url: url.to_string(),
            })
        }
        "Notification" => {
            let message = json.get("Message").and_then(|m| m.as_str()).unwrap_or("");
            // The SNS Message may be (a) a plain base64-encoded RFC 5322 email,
            // (b) a raw RFC 5322 string, or (c) a JSON object with a "content"
            // field containing the base64-encoded email (SES default action format).
            let (raw, envelope_to) = match serde_json::from_str::<serde_json::Value>(message) {
                Err(_) => {
                    let bytes = base64::engine::general_purpose::STANDARD
                        .decode(message)
                        .unwrap_or_else(|_| message.as_bytes().to_vec());
                    (bytes, Vec::new())
                }
                Ok(msg_json) => {
                    let content = msg_json
                        .get("content")
                        .and_then(|c| c.as_str())
                        .ok_or(StatusCode::UNPROCESSABLE_ENTITY)?;
                    let bytes = base64::engine::general_purpose::STANDARD
                        .decode(content)
                        .unwrap_or_else(|_| content.as_bytes().to_vec());
                    // Preserve SES envelope recipients (mail.destination) for Bcc/alias routing.
                    let envelope_to: Vec<String> = msg_json
                        .get("mail")
                        .and_then(|m| m.get("destination"))
                        .and_then(|d| d.as_array())
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|v| v.as_str().map(str::to_owned))
                                .collect()
                        })
                        .unwrap_or_default();
                    (bytes, envelope_to)
                }
            };
            let mut email = parse_rfc5322(Bytes::from(raw));
            // Merge envelope recipients not already in the RFC 5322 To header.
            // Compare case-insensitively to avoid duplicates while preserving original
            // casing (needed for plus-address token extraction).
            for addr in envelope_to {
                let addr_lower = addr.to_ascii_lowercase();
                if !email
                    .to
                    .iter()
                    .any(|t| t.to_ascii_lowercase() == addr_lower)
                {
                    email.to.push(addr);
                }
            }
            Ok(SnsParseResult::Email(Box::new(email)))
        }
        _ => {
            tracing::warn!(msg_type, "inbound_mail.ses: unknown SNS message type");
            Err(StatusCode::BAD_REQUEST)
        }
    }
}

/// Possible outcomes of parsing an SNS notification body.
#[derive(Debug)]
pub(crate) enum SnsParseResult {
    SubscriptionConfirmation { url: String },
    Email(Box<InboundEmail>),
}

/// Parse and optionally authenticate a generic RFC 5322 raw-body POST.
///
/// When `signing_key` is `Some`, the handler verifies
/// `X-Inbound-Signature: HMAC-SHA256(key, body)` before parsing.
pub(crate) fn parse_generic(
    raw_body: Bytes,
    signing_key: Option<&str>,
    headers: &HeaderMap,
) -> Result<InboundEmail, StatusCode> {
    if let Some(key) = signing_key {
        if key.is_empty() {
            tracing::error!("inbound_mail.generic: signing key is empty; rejecting request");
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
        let provided = headers
            .get("x-inbound-signature")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        let expected = crate::security::config::hmac_sha256_hex(key.as_bytes(), &raw_body);
        if !subtle_eq(expected.as_bytes(), provided.as_bytes()) {
            tracing::warn!("inbound_mail.generic: invalid signature — request rejected");
            return Err(StatusCode::UNAUTHORIZED);
        }
    }
    Ok(parse_rfc5322(raw_body))
}

// ── Fuzzing seams ───────────────────────────────────────────────────────────
// `#[cfg(fuzzing)]` wrappers exposing the inbound-mail parsing surface to the
// `fuzz/` crate: SNS/SES JSON parsing, the RFC 5322 / MIME body parser (via the
// unsigned `parse_generic` path), and address-list parsing. Compiled out of all
// normal builds. See `fuzz/fuzz_targets/body.rs`.

/// Fuzzing seam: parse an SES-via-SNS notification body.
#[cfg(fuzzing)]
pub fn __fuzz_parse_ses(body: &[u8]) {
    let _ = parse_ses(&Bytes::copy_from_slice(body));
}

/// Fuzzing seam: parse a generic RFC 5322 / MIME raw body (no signature check),
/// exercising `parse_rfc5322` including its multi-part MIME handling.
#[cfg(fuzzing)]
pub fn __fuzz_parse_generic(body: &[u8]) {
    let _ = parse_generic(Bytes::copy_from_slice(body), None, &HeaderMap::new());
}

/// Fuzzing seam: parse an RFC 5322 address list.
#[cfg(fuzzing)]
pub fn __fuzz_parse_address_list(value: &str) {
    let _ = parse_address_list(value);
}

/// Minimal RFC 5322 parser.
///
/// Handles folded headers, plain-text and HTML bodies (single-part only).
/// Multi-part MIME bodies are accepted but only the first part is extracted.
fn parse_rfc5322(raw: Bytes) -> InboundEmail {
    // Split at the byte level so body bytes are preserved exactly for binary attachments.
    let (header_bytes, body_bytes): (&[u8], &[u8]) = find_subslice(&raw, b"\r\n\r\n")
        .map(|p| (&raw[..p], &raw[p + 4..]))
        .or_else(|| find_subslice(&raw, b"\n\n").map(|p| (&raw[..p], &raw[p + 2..])))
        .unwrap_or((raw.as_ref(), &[]));
    let header_block = String::from_utf8_lossy(header_bytes);

    let mut from = String::new();
    let mut to = Vec::new();
    let mut cc = Vec::new();
    let mut subject = String::new();
    let mut parsed_headers: HashMap<String, String> = HashMap::new();

    let mut cur_name = String::new();
    let mut cur_value = String::new();

    for line in header_block.lines() {
        if line.starts_with(' ') || line.starts_with('\t') {
            cur_value.push(' ');
            cur_value.push_str(line.trim());
        } else if let Some(colon) = line.find(':') {
            if !cur_name.is_empty() {
                apply_header(
                    &mut parsed_headers,
                    &mut from,
                    &mut to,
                    &mut cc,
                    &mut subject,
                    &cur_name,
                    &cur_value,
                );
            }
            cur_name = line[..colon].trim().to_ascii_lowercase();
            cur_value = line[colon + 1..].trim().to_string();
        }
    }
    if !cur_name.is_empty() {
        apply_header(
            &mut parsed_headers,
            &mut from,
            &mut to,
            &mut cc,
            &mut subject,
            &cur_name,
            &cur_value,
        );
    }

    let content_type = parsed_headers
        .get("content-type")
        .cloned()
        .unwrap_or_default();
    let cte = parsed_headers
        .get("content-transfer-encoding")
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_default();
    // Normalise the MIME type/subtype for matching (RFC 2045 §5 — case-insensitive),
    // but pass the original `content_type` value to boundary extraction so that
    // case-sensitive boundary parameter values are preserved.
    let ct_lower = content_type.to_ascii_lowercase();
    let (text_body, html_body, attachments) = if body_bytes.iter().all(|b| b.is_ascii_whitespace())
    {
        (None, None, Vec::new())
    } else if ct_lower.starts_with("multipart/") {
        // Pass raw bytes so binary attachment parts are not corrupted.
        extract_multipart_bodies(body_bytes, &content_type)
    } else {
        let disposition = parsed_headers
            .get("content-disposition")
            .map(|s| s.to_ascii_lowercase())
            .unwrap_or_default();
        let is_attachment = disposition.starts_with("attachment")
            || (!ct_lower.is_empty()
                && !ct_lower.starts_with("text/")
                && !ct_lower.starts_with("message/"));
        if is_attachment {
            let filename = parsed_headers
                .get("content-disposition")
                .and_then(|d| mime_param(d, "filename"));
            let ct_only = ct_lower
                .split(';')
                .next()
                .map_or("application/octet-stream", str::trim)
                .to_string();
            let data = if cte == "base64" {
                let stripped: String = String::from_utf8_lossy(body_bytes)
                    .chars()
                    .filter(|c| !c.is_ascii_whitespace())
                    .collect();
                base64::engine::general_purpose::STANDARD
                    .decode(stripped.as_bytes())
                    .map_or_else(|_| Bytes::copy_from_slice(body_bytes), Bytes::from)
            } else if cte == "quoted-printable" {
                Bytes::from(decode_quoted_printable_bytes(body_bytes))
            } else {
                Bytes::copy_from_slice(body_bytes)
            };
            (
                None,
                None,
                vec![Attachment {
                    filename,
                    content_type: ct_only,
                    data,
                }],
            )
        } else {
            let body_str = String::from_utf8_lossy(body_bytes).into_owned();
            if ct_lower.contains("text/html") {
                (
                    None,
                    Some(decode_transfer_encoding(&body_str, &cte)),
                    Vec::new(),
                )
            } else {
                (
                    Some(decode_transfer_encoding(&body_str, &cte)),
                    None,
                    Vec::new(),
                )
            }
        }
    };

    InboundEmail {
        from,
        to,
        cc,
        subject,
        text_body,
        html_body,
        headers: parsed_headers,
        attachments,
        spam_report: None,
        raw,
        plus_token: None,
        is_bounce: false,
        bounced_address: None,
        complained_address: None,
    }
}

/// Decode a MIME part body according to its `Content-Transfer-Encoding` header.
///
/// Handles `base64` and `quoted-printable`; passes everything else through unchanged.
fn decode_transfer_encoding(body: &str, cte: &str) -> String {
    match cte.trim() {
        "base64" => {
            let stripped: String = body.chars().filter(|c| !c.is_ascii_whitespace()).collect();
            base64::engine::general_purpose::STANDARD
                .decode(stripped.as_bytes())
                .ok()
                .and_then(|b| String::from_utf8(b).ok())
                .unwrap_or_else(|| body.to_string())
        }
        "quoted-printable" => decode_quoted_printable(body),
        _ => body.to_string(),
    }
}

/// Decode quoted-printable bytes per RFC 2045, returning the raw decoded bytes.
///
/// This is the canonical implementation used by both text and binary paths.
/// For text bodies the caller can convert to `String`; for binary attachment
/// bodies the `Vec<u8>` is used directly so no UTF-8 lossy replacement occurs.
fn decode_quoted_printable_bytes(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        if input[i] == b'=' {
            if i + 1 < input.len() && (input[i + 1] == b'\n' || input[i + 1] == b'\r') {
                // Soft line break: `=\n` or `=\r\n`
                i += 1;
                if i + 1 < input.len() && input[i] == b'\r' && input[i + 1] == b'\n' {
                    i += 2;
                } else {
                    i += 1;
                }
            } else if i + 2 < input.len()
                && input[i + 1].is_ascii_hexdigit()
                && input[i + 2].is_ascii_hexdigit()
            {
                let hi = (input[i + 1] as char).to_digit(16).unwrap_or(0) as u8;
                let lo = (input[i + 2] as char).to_digit(16).unwrap_or(0) as u8;
                out.push((hi << 4) | lo);
                i += 3;
            } else {
                out.push(b'=');
                i += 1;
            }
        } else {
            out.push(input[i]);
            i += 1;
        }
    }
    out
}

/// Decode a quoted-printable encoded string per RFC 2045.
///
/// For text bodies only — the result is a `String` so non-UTF-8 bytes in the
/// decoded stream are replaced with `U+FFFD`.  Binary attachment paths should
/// use [`decode_quoted_printable_bytes`] to avoid that replacement.
fn decode_quoted_printable(input: &str) -> String {
    let out = decode_quoted_printable_bytes(input.as_bytes());
    String::from_utf8(out).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

/// Extract a named parameter from a semicolon-separated MIME header value,
/// correctly handling quoted strings so that a semicolon inside a quoted
/// filename (e.g. `filename="Q1;final.pdf"`) is not treated as a separator.
fn mime_param(header_val: &str, name: &str) -> Option<String> {
    let bytes = header_val.as_bytes();
    let len = bytes.len();
    let mut i = 0;
    while i < len {
        // Skip whitespace / leading semicolon between parameters.
        while i < len && (bytes[i] == b';' || bytes[i] == b' ' || bytes[i] == b'\t') {
            i += 1;
        }
        if i >= len {
            break;
        }
        // Read the key up to '=' or ';'.
        let key_start = i;
        while i < len && bytes[i] != b'=' && bytes[i] != b';' {
            i += 1;
        }
        let key = header_val[key_start..i].trim();
        if i >= len || bytes[i] == b';' {
            continue;
        }
        i += 1; // skip '='
        // Skip whitespace before the value.
        while i < len && (bytes[i] == b' ' || bytes[i] == b'\t') {
            i += 1;
        }
        if i >= len {
            break;
        }
        // Read the value, respecting double-quoted strings.
        let val = if bytes[i] == b'"' {
            i += 1; // skip opening '"'
            let mut val = String::new();
            while i < len && bytes[i] != b'"' {
                if bytes[i] == b'\\' && i + 1 < len {
                    i += 1; // skip backslash escape
                }
                val.push(bytes[i] as char);
                i += 1;
            }
            if i < len {
                i += 1;
            } // skip closing '"'
            val
        } else {
            let val_start = i;
            while i < len && bytes[i] != b';' {
                i += 1;
            }
            header_val[val_start..i].trim().to_string()
        };
        if key.eq_ignore_ascii_case(name) {
            return Some(val);
        }
    }
    None
}

/// Extract the MIME boundary parameter from a `Content-Type` header value.
fn extract_boundary(content_type: &str) -> Option<String> {
    content_type.split(';').skip(1).find_map(|part| {
        let part = part.trim();
        let (key, val) = part.split_once('=')?;
        if key.trim().eq_ignore_ascii_case("boundary") {
            Some(val.trim().trim_matches('"').to_string())
        } else {
            None
        }
    })
}

/// Split a MIME multipart body and return `(text/plain, text/html, attachments)`.
///
/// Accepts raw bytes so non-base64 attachment parts are stored without UTF-8
/// round-trip corruption. Recurses into nested `multipart/*` parts.
fn extract_multipart_bodies(
    body: &[u8],
    content_type: &str,
) -> (Option<String>, Option<String>, Vec<Attachment>) {
    let Some(boundary) = extract_boundary(content_type) else {
        return (
            Some(String::from_utf8_lossy(body).into_owned()),
            None,
            Vec::new(),
        );
    };
    let delimiter = format!("--{boundary}");
    let delim = delimiter.as_bytes();
    let mut text_body: Option<String> = None;
    let mut html_body: Option<String> = None;
    let mut attachments: Vec<Attachment> = Vec::new();

    // Split body into parts by finding each boundary delimiter.
    // RFC 2046 §5.1.1: boundaries must appear at the start of a line and be followed by a
    // valid terminator (CRLF, `-`, SP, HT, or EOF) — not an arbitrary character like a digit.
    //
    // `seg_start` tracks the beginning of the current segment independently of `pos` (the
    // search cursor), so that advancing past a false-positive match does not discard content.
    let mut raw_parts: Vec<&[u8]> = Vec::new();
    let mut pos = 0;
    let mut seg_start = 0;
    loop {
        match find_subslice(&body[pos..], delim) {
            None => {
                raw_parts.push(&body[seg_start..]);
                break;
            }
            Some(rel) => {
                let abs = pos + rel;
                if abs > 0 && body.get(abs - 1) != Some(&b'\n') {
                    // Not at a line start — skip this false match.
                    pos += rel + 1;
                    continue;
                }
                // Boundary must be followed by a valid MIME terminator.
                let after = abs + delim.len();
                if !matches!(
                    body.get(after),
                    None | Some(b'\r') | Some(b'\n') | Some(b'-') | Some(b' ') | Some(b'\t')
                ) {
                    pos += rel + 1;
                    continue;
                }
                raw_parts.push(&body[seg_start..abs]);
                seg_start = abs + delim.len();
                pos = seg_start;
            }
        }
    }

    for part in raw_parts.into_iter().skip(1) {
        // End-boundary remainder starts with `--`; blank/whitespace-only parts are noise.
        if part.starts_with(b"--") || part.iter().all(|b| b.is_ascii_whitespace() || *b == b'-') {
            continue;
        }
        // Strip leading CRLF/LF (the newline after the boundary marker line).
        let part = part
            .strip_prefix(b"\r\n")
            .or_else(|| part.strip_prefix(b"\n"))
            .unwrap_or(part);
        // Strip trailing CRLF/LF (the line ending before the next boundary).
        let part = part
            .strip_suffix(b"\r\n")
            .or_else(|| part.strip_suffix(b"\n"))
            .unwrap_or(part);

        // Split part into headers and body at the blank-line separator.
        // If no blank-line separator exists, the part has no headers — treat the
        // entire content as the body with empty headers (RFC 2045 §5.2 defaults apply).
        let (part_header_bytes, part_body_bytes) = find_subslice(part, b"\r\n\r\n")
            .map(|p| (&part[..p], &part[p + 4..]))
            .or_else(|| find_subslice(part, b"\n\n").map(|p| (&part[..p], &part[p + 2..])))
            .unwrap_or((&part[..0], part));

        let part_headers = unfold_mime_headers(&String::from_utf8_lossy(part_header_bytes));
        // Per RFC 2045 §5.2, a missing Content-Type defaults to text/plain.
        let part_ct_lower = part_headers
            .lines()
            .find(|l| l.to_ascii_lowercase().starts_with("content-type:"))
            .map_or_else(
                || "text/plain".to_string(),
                |l| l[13..].trim().to_ascii_lowercase(),
            );
        let part_ct_orig = part_headers
            .lines()
            .find(|l| l.to_ascii_lowercase().starts_with("content-type:"))
            .map_or_else(|| "text/plain".to_string(), |l| l[13..].trim().to_string());
        let part_cte = part_headers
            .lines()
            .find(|l| {
                l.to_ascii_lowercase()
                    .starts_with("content-transfer-encoding:")
            })
            .map(|l| l[26..].trim().to_ascii_lowercase())
            .unwrap_or_default();
        // Keep original case for value extraction; only compare names case-insensitively.
        let disposition = part_headers
            .lines()
            .find(|l| l.to_ascii_lowercase().starts_with("content-disposition:"))
            .map(|l| l[l.find(':').map_or(0, |p| p + 1)..].trim().to_string())
            .unwrap_or_default();
        let disposition_lower = disposition.to_ascii_lowercase();
        let is_attachment = disposition_lower.starts_with("attachment")
            || mime_param(&disposition, "filename").is_some();

        if part_ct_lower.starts_with("multipart/") {
            // Recurse into nested multipart parts (e.g. multipart/alternative).
            let (nested_text, nested_html, nested_atts) =
                extract_multipart_bodies(part_body_bytes, &part_ct_orig);
            if text_body.is_none() {
                text_body = nested_text;
            }
            if html_body.is_none() {
                html_body = nested_html;
            }
            attachments.extend(nested_atts);
        } else if !is_attachment && part_ct_lower.starts_with("text/plain") && text_body.is_none() {
            let s = String::from_utf8_lossy(part_body_bytes).into_owned();
            text_body = Some(decode_transfer_encoding(&s, &part_cte));
        } else if !is_attachment && part_ct_lower.starts_with("text/html") && html_body.is_none() {
            let s = String::from_utf8_lossy(part_body_bytes).into_owned();
            html_body = Some(decode_transfer_encoding(&s, &part_cte));
        } else if is_attachment || !part_ct_lower.starts_with("text/") {
            // Collect attachment parts; use raw bytes to avoid UTF-8 corruption.
            let filename = mime_param(&disposition, "filename");
            let ct_only = part_ct_lower
                .split(';')
                .next()
                .map_or("application/octet-stream", str::trim)
                .to_string();
            let data = if part_cte == "base64" {
                let stripped: String = String::from_utf8_lossy(part_body_bytes)
                    .chars()
                    .filter(|c| !c.is_ascii_whitespace())
                    .collect();
                base64::engine::general_purpose::STANDARD
                    .decode(stripped.as_bytes())
                    .map_or_else(|_| Bytes::copy_from_slice(part_body_bytes), Bytes::from)
            } else if part_cte == "quoted-printable" {
                // Use the byte-returning decoder to avoid UTF-8 replacement for
                // non-text attachments whose decoded bytes may not be valid UTF-8.
                Bytes::from(decode_quoted_printable_bytes(part_body_bytes))
            } else {
                Bytes::copy_from_slice(part_body_bytes)
            };
            attachments.push(Attachment {
                filename,
                content_type: ct_only,
                data,
            });
        }
    }
    (text_body, html_body, attachments)
}

/// Decode RFC 2047 encoded words (`=?charset?B/Q?text?=`) in a header value.
///
/// Adjacent encoded words separated only by whitespace are joined without the whitespace
/// per RFC 2047 §6.2.
fn decode_rfc2047(value: &str) -> String {
    if !value.contains("=?") {
        return value.to_string();
    }
    let mut result = String::with_capacity(value.len());
    let mut rest = value;
    let mut last_was_encoded = false;
    while let Some(start) = rest.find("=?") {
        let before = &rest[..start];
        // If the gap between encoded words is only whitespace, skip it.
        if last_was_encoded && before.chars().all(|c| c == ' ' || c == '\t') {
            // swallow inter-word whitespace
        } else {
            result.push_str(before);
        }
        rest = &rest[start + 2..];
        let Some(charset_end) = rest.find('?') else {
            result.push_str("=?");
            result.push_str(rest);
            return result;
        };
        let charset = &rest[..charset_end];
        rest = &rest[charset_end + 1..];
        let Some(enc_end) = rest.find('?') else {
            result.push_str("=?");
            result.push_str(charset);
            result.push('?');
            result.push_str(rest);
            return result;
        };
        let encoding = &rest[..enc_end];
        rest = &rest[enc_end + 1..];
        let Some(text_end) = rest.find("?=") else {
            result.push_str("=?");
            result.push_str(charset);
            result.push('?');
            result.push_str(encoding);
            result.push('?');
            result.push_str(rest);
            return result;
        };
        let encoded_text = &rest[..text_end];
        rest = &rest[text_end + 2..];
        let decoded_bytes: Option<Vec<u8>> = match encoding.to_ascii_uppercase().as_str() {
            "B" => {
                let stripped: String = encoded_text
                    .chars()
                    .filter(|c| !c.is_ascii_whitespace())
                    .collect();
                base64::engine::general_purpose::STANDARD
                    .decode(stripped.as_bytes())
                    .ok()
            }
            "Q" => Some(decode_rfc2047_q(encoded_text.as_bytes())),
            _ => None,
        };
        let decoded = decoded_bytes
            .and_then(|b| rfc2047_bytes_to_string(b, charset))
            .unwrap_or_else(|| encoded_text.to_string());
        result.push_str(&decoded);
        last_was_encoded = true;
    }
    result.push_str(rest);
    result
}

/// Decode RFC 2047 Q-encoding (header variant): `_` → space, `=XX` → byte.
fn decode_rfc2047_q(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        if input[i] == b'_' {
            out.push(b' ');
            i += 1;
        } else if input[i] == b'='
            && i + 2 < input.len()
            && input[i + 1].is_ascii_hexdigit()
            && input[i + 2].is_ascii_hexdigit()
        {
            let hi = (input[i + 1] as char).to_digit(16).unwrap_or(0) as u8;
            let lo = (input[i + 2] as char).to_digit(16).unwrap_or(0) as u8;
            out.push((hi << 4) | lo);
            i += 3;
        } else {
            out.push(input[i]);
            i += 1;
        }
    }
    out
}

/// Convert a decoded byte sequence to a String given its RFC 2047 charset label.
fn rfc2047_bytes_to_string(bytes: Vec<u8>, charset: &str) -> Option<String> {
    match charset.to_ascii_lowercase().as_str() {
        "utf-8" | "utf8" | "us-ascii" | "ascii" => String::from_utf8(bytes).ok(),
        "iso-8859-1" | "latin-1" | "iso8859-1" => Some(bytes.iter().map(|&b| b as char).collect()),
        _ => String::from_utf8(bytes).ok(),
    }
}

fn apply_header(
    headers: &mut HashMap<String, String>,
    from: &mut String,
    to: &mut Vec<String>,
    cc: &mut Vec<String>,
    subject: &mut String,
    name: &str,
    value: &str,
) {
    headers.insert(name.to_string(), value.to_string());
    match name {
        "from" => *from = value.to_string(),
        "to" => *to = parse_address_list(value),
        "cc" => *cc = parse_address_list(value),
        "subject" => *subject = decode_rfc2047(value),
        _ => {}
    }
}

// ── Axum route construction ───────────────────────────────────────────────────

/// URL-decode a `application/x-www-form-urlencoded` body.
fn url_decode_form(body: &[u8]) -> HashMap<String, String> {
    let s = String::from_utf8_lossy(body);
    serde_urlencoded::from_str(&s).unwrap_or_default()
}

/// Find the first occurrence of `needle` in `haystack`, returning the start offset.
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Unfold RFC 2822 §2.2.3 header folding.
///
/// A CRLF (or bare LF) immediately before a whitespace character is a fold
/// point; the line ending is dropped so the header reads as one logical line.
fn unfold_mime_headers(s: &str) -> String {
    s.replace("\r\n ", " ")
        .replace("\r\n\t", "\t")
        .replace("\n ", " ")
        .replace("\n\t", "\t")
}

/// Find the first part-end delimiter (`crlf_delim` preferred over `lf_delim`)
/// whose immediately following byte is a valid MIME terminator.
///
/// Per RFC 2046 §5.1.1, a boundary delimiter must be followed by `\r\n`, `\n`,
/// `--`, SP, HT, or end-of-input — never by an arbitrary character such as a
/// digit. This prevents a boundary-prefix false match (e.g. `\r\n--abc` inside
/// `\r\n--abc123`) from prematurely ending a MIME part.
fn find_part_end(body: &[u8], crlf_delim: &[u8], lf_delim: &[u8]) -> Option<usize> {
    let mut pos = 0;
    while let Some(rel) = find_subslice(&body[pos..], crlf_delim) {
        let abs = pos + rel;
        let after = abs + crlf_delim.len();
        if matches!(
            body.get(after),
            None | Some(b'\r') | Some(b'\n') | Some(b'-') | Some(b' ') | Some(b'\t')
        ) {
            return Some(abs);
        }
        pos = abs + 1;
    }
    pos = 0;
    while let Some(rel) = find_subslice(&body[pos..], lf_delim) {
        let abs = pos + rel;
        let after = abs + lf_delim.len();
        if matches!(
            body.get(after),
            None | Some(b'\r') | Some(b'\n') | Some(b'-') | Some(b' ') | Some(b'\t')
        ) {
            return Some(abs);
        }
        pos = abs + 1;
    }
    None
}

/// Parse a `multipart/form-data` Mailgun webhook body into a field map and attachment list.
///
/// Operates at the byte level so binary file parts are not corrupted by lossy UTF-8 conversion.
fn parse_mailgun_form_data(
    body: &[u8],
    content_type: &str,
) -> (HashMap<String, String>, Vec<Attachment>) {
    let Some(boundary) = extract_boundary(content_type) else {
        return (HashMap::new(), Vec::new());
    };
    let delim = format!("--{boundary}");
    let delim_bytes = delim.as_bytes();
    let crlf_delim = format!("\r\n--{boundary}");
    let lf_delim = format!("\n--{boundary}");
    let crlf_delim_bytes = crlf_delim.as_bytes();
    let lf_delim_bytes = lf_delim.as_bytes();

    let mut map = HashMap::new();
    let mut file_parts: Vec<Attachment> = Vec::new();
    let mut search_from = 0_usize;

    loop {
        // Locate the next "--{boundary}" in the raw byte buffer.
        let Some(rel) = find_subslice(&body[search_from..], delim_bytes) else {
            break;
        };
        let abs = search_from + rel;
        let after_delim = abs + delim_bytes.len();

        // RFC 2046 §5.1.1: boundary must start at the beginning of a line.
        if abs > 0 && body.get(abs - 1) != Some(&b'\n') {
            search_from = abs + 1;
            continue;
        }

        // RFC 2046: boundary must be followed by a valid terminator byte.
        if !matches!(
            body.get(after_delim),
            None | Some(b'\r') | Some(b'\n') | Some(b'-') | Some(b' ') | Some(b'\t')
        ) {
            search_from = abs + 1;
            continue;
        }

        // "--{boundary}--" is the final delimiter — stop.
        if body[after_delim..].starts_with(b"--") {
            break;
        }

        // Skip the CRLF/LF that follows the opening boundary line.
        let part_start = if body[after_delim..].starts_with(b"\r\n") {
            after_delim + 2
        } else if body[after_delim..].starts_with(b"\n") {
            after_delim + 1
        } else {
            after_delim
        };

        // The part body ends just before the next "\r\n--{boundary}" or "\n--{boundary}".
        // `find_part_end` validates the terminator byte to avoid false matches on
        // boundary-prefix strings (e.g. "\r\n--abc" inside "\r\n--abc123").
        let part_end = find_part_end(&body[part_start..], crlf_delim_bytes, lf_delim_bytes)
            .map_or(body.len(), |p| part_start + p);

        search_from = part_end;
        let part = &body[part_start..part_end];

        // Split part headers from body on the blank line.
        let (headers_bytes, body_bytes) = if let Some(sep) = find_subslice(part, b"\r\n\r\n") {
            (&part[..sep], &part[sep + 4..])
        } else if let Some(sep) = find_subslice(part, b"\n\n") {
            (&part[..sep], &part[sep + 2..])
        } else {
            continue;
        };

        // Headers are ASCII; lossy conversion is safe here.  Unfold before
        // parsing so a folded Content-Disposition reads as one logical line.
        let headers_str = unfold_mime_headers(&String::from_utf8_lossy(headers_bytes));

        // Preserve the original disposition value so that filename casing is not
        // corrupted — parameter values are case-sensitive (RFC 2183 §2).
        // Key matching (name=, filename=) is done case-insensitively.
        let disposition = headers_str
            .lines()
            .find(|l| l.to_ascii_lowercase().starts_with("content-disposition:"))
            .map(|l| l[l.find(':').map_or(0, |p| p + 1)..].trim().to_string())
            .unwrap_or_default();

        // Use the quote-aware parser so that filenames containing semicolons,
        // e.g. filename="Q1;final.pdf", are not truncated at the semicolon.
        let name = mime_param(&disposition, "name");
        let Some(name) = name else { continue };

        let filename = mime_param(&disposition, "filename");

        if let Some(filename) = filename {
            // File part: use raw bytes from the original buffer to avoid lossy UTF-8 corruption.
            let part_ct = headers_str
                .lines()
                .find(|l| l.to_ascii_lowercase().starts_with("content-type:"))
                .map_or_else(
                    || "application/octet-stream".to_string(),
                    |l| {
                        l[13..]
                            .trim()
                            .split(';')
                            .next()
                            .map_or("application/octet-stream", str::trim)
                            .to_ascii_lowercase()
                    },
                );
            let part_cte = headers_str
                .lines()
                .find(|l| {
                    l.to_ascii_lowercase()
                        .starts_with("content-transfer-encoding:")
                })
                .map(|l| l[26..].trim().to_ascii_lowercase())
                .unwrap_or_default();
            let data: Bytes = if part_cte == "base64" {
                // base64 is ASCII; string round-trip is safe.
                let stripped: String = String::from_utf8_lossy(body_bytes)
                    .chars()
                    .filter(|c| !c.is_ascii_whitespace())
                    .collect();
                base64::engine::general_purpose::STANDARD
                    .decode(stripped.as_bytes())
                    .map_or_else(|_| Bytes::copy_from_slice(body_bytes), Bytes::from)
            } else {
                // Binary (8-bit): copy raw bytes without any string conversion.
                Bytes::copy_from_slice(body_bytes)
            };
            file_parts.push(Attachment {
                filename: Some(filename),
                content_type: part_ct,
                data,
            });
        } else {
            // Text field: lossy conversion is acceptable.
            // Do not trim trailing newlines — the boundary separator is already excluded
            // by the line-anchored boundary split, so trailing content is intentional.
            map.insert(name, String::from_utf8_lossy(body_bytes).into_owned());
        }
    }
    (map, file_parts)
}

/// Decode a Mailgun webhook body, supporting both `application/x-www-form-urlencoded`
/// and `multipart/form-data` content types.
fn decode_mailgun_body(
    body: &[u8],
    content_type: &str,
) -> (HashMap<String, String>, Vec<Attachment>) {
    if content_type
        .to_ascii_lowercase()
        .starts_with("multipart/form-data")
    {
        parse_mailgun_form_data(body, content_type)
    } else {
        (url_decode_form(body), Vec::new())
    }
}

/// Build all Axum routes for the router's configured endpoints.
///
/// Called by `AppBuilder::run()` and `TestApp::build()`.
pub(crate) fn build_routes(
    router: &Arc<InboundMailRouter>,
) -> Vec<(String, axum::Router<crate::state::AppState>)> {
    router
        .endpoints
        .iter()
        .map(|endpoint| {
            let path = endpoint.path.clone();
            let provider = endpoint.provider;
            let signing_key = endpoint.resolve_signing_key();
            let router_arc = Arc::clone(router);

            // Whether a signing key was *configured* (even if the env var is absent).
            let key_configured =
                endpoint.signing_key.is_some() || endpoint.signing_key_env.is_some();
            let axum_router = match provider {
                InboundMailProvider::Mailgun => build_mailgun_route(&path, signing_key, router_arc),
                InboundMailProvider::Ses => {
                    build_ses_route(&path, endpoint.topic_arn.clone(), router_arc)
                }
                InboundMailProvider::Generic => {
                    build_generic_route(&path, signing_key, key_configured, router_arc)
                }
            };
            (path, axum_router)
        })
        .collect()
}

fn build_mailgun_route(
    path: &str,
    signing_key: Option<String>,
    router: Arc<InboundMailRouter>,
) -> axum::Router<crate::state::AppState> {
    use axum::extract::DefaultBodyLimit;
    use axum::routing::post;

    let handler = move |headers: HeaderMap, body: Bytes| {
        let router = Arc::clone(&router);
        let key = signing_key.clone();
        async move {
            let content_type = headers
                .get(http::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            let (form, file_parts) = decode_mailgun_body(&body, content_type);
            let effective_key = key.as_deref().unwrap_or("");
            match parse_mailgun(&form, effective_key, file_parts) {
                Ok(email) => match router.dispatch(email).await {
                    Ok(()) => StatusCode::OK,
                    Err(e) => {
                        tracing::error!(error = %e, "inbound_mail.mailgun: sync dispatch error");
                        StatusCode::INTERNAL_SERVER_ERROR
                    }
                },
                Err(status) => status,
            }
        }
    };

    axum::Router::new()
        .route(path, post(handler))
        .layer(DefaultBodyLimit::max(50 * 1024 * 1024))
}

fn build_ses_route(
    path: &str,
    topic_arn: Option<String>,
    router: Arc<InboundMailRouter>,
) -> axum::Router<crate::state::AppState> {
    use axum::extract::DefaultBodyLimit;
    use axum::routing::post;

    // Fail closed: without a topic ARN every SNS-signed notification from any
    // AWS account passes signature verification.  A third party could subscribe
    // this endpoint to their own topic and deliver arbitrary payloads.  Emit an
    // error at startup so the misconfiguration is visible immediately.
    if topic_arn.is_none() {
        tracing::error!(
            path = %path,
            "inbound_mail.ses: no topic_arn configured — all requests to this SES \
             endpoint will be rejected until one is set via .with_topic_arn(\"arn:aws:sns:…\")"
        );
    }

    // One shared client per route for SNS cert fetching.
    #[cfg(feature = "inbound-ses")]
    let http_client = reqwest::Client::new();

    let handler = move |_headers: HeaderMap, body: Bytes| {
        let router = Arc::clone(&router);
        #[cfg(feature = "inbound-ses")]
        let http_client = http_client.clone();
        let topic_arn = topic_arn.clone();
        async move {
            // Fail closed: reject every request if no topic ARN was configured.
            if topic_arn.is_none() {
                return StatusCode::SERVICE_UNAVAILABLE;
            }

            // Reject SES notifications when the `inbound-ses` feature is off: the
            // signature verifier is not compiled in, so accepting requests would
            // bypass SNS authentication entirely.
            #[cfg(not(feature = "inbound-ses"))]
            {
                tracing::error!(
                    "inbound_mail.ses: SES endpoint configured but the `inbound-ses` \
                     Cargo feature is not enabled; all requests are rejected. \
                     Add `inbound-ses` to your feature list."
                );
                return StatusCode::SERVICE_UNAVAILABLE;
            }

            // Verify SNS signature (and optional TopicArn binding) before parsing.
            #[cfg(feature = "inbound-ses")]
            if let Ok(json) = serde_json::from_slice::<serde_json::Value>(&body) {
                if let Err(status) =
                    sns_verify::verify(&json, &http_client, topic_arn.as_deref()).await
                {
                    return status;
                }
            }

            match parse_ses(&body) {
                #[cfg(feature = "inbound-ses")]
                Ok(SnsParseResult::SubscriptionConfirmation { url }) => {
                    match http_client
                        .get(&url)
                        .send()
                        .await
                        .and_then(|r| r.error_for_status())
                    {
                        Ok(_) => StatusCode::OK,
                        Err(e) => {
                            // Return 5xx so SNS retries the SubscriptionConfirmation delivery,
                            // giving us another chance to GET the SubscribeURL.
                            tracing::error!(
                                error = %e,
                                "inbound_mail.ses: SNS subscription confirmation fetch failed"
                            );
                            StatusCode::INTERNAL_SERVER_ERROR
                        }
                    }
                }
                #[cfg(not(feature = "inbound-ses"))]
                Ok(SnsParseResult::SubscriptionConfirmation { .. }) => StatusCode::OK,
                Ok(SnsParseResult::Email(email)) => match router.dispatch(*email).await {
                    Ok(()) => StatusCode::OK,
                    Err(e) => {
                        tracing::error!(error = %e, "inbound_mail.ses: sync dispatch error");
                        StatusCode::INTERNAL_SERVER_ERROR
                    }
                },
                Err(status) => status,
            }
        }
    };

    axum::Router::new()
        .route(path, post(handler))
        .layer(DefaultBodyLimit::max(50 * 1024 * 1024))
}

fn build_generic_route(
    path: &str,
    signing_key: Option<String>,
    key_configured: bool,
    router: Arc<InboundMailRouter>,
) -> axum::Router<crate::state::AppState> {
    use axum::extract::DefaultBodyLimit;
    use axum::routing::post;

    let handler = move |headers: HeaderMap, body: Bytes| {
        let router = Arc::clone(&router);
        let key = signing_key.clone();
        async move {
            // Fail closed: if a signing key was configured but could not be resolved
            // (e.g. missing env var), reject instead of silently skipping verification.
            if key_configured && key.is_none() {
                tracing::error!(
                    "inbound_mail.generic: signing key env var not resolved; rejecting request"
                );
                return StatusCode::INTERNAL_SERVER_ERROR;
            }
            match parse_generic(body, key.as_deref(), &headers) {
                Ok(email) => match router.dispatch(email).await {
                    Ok(()) => StatusCode::OK,
                    Err(e) => {
                        tracing::error!(error = %e, "inbound_mail.generic: sync dispatch error");
                        StatusCode::INTERNAL_SERVER_ERROR
                    }
                },
                Err(status) => status,
            }
        }
    };

    axum::Router::new()
        .route(path, post(handler))
        .layer(DefaultBodyLimit::max(50 * 1024 * 1024))
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mailgun_signature_is_64_hex_chars() {
        let sig = compute_mailgun_signature("1234", "token", "key");
        assert_eq!(sig.len(), 64);
        assert!(sig.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn mailgun_signature_is_deterministic() {
        assert_eq!(
            compute_mailgun_signature("ts", "tok", "key"),
            compute_mailgun_signature("ts", "tok", "key"),
        );
    }

    #[test]
    fn mailgun_signature_changes_with_input() {
        let a = compute_mailgun_signature("ts1", "tok", "key");
        let b = compute_mailgun_signature("ts2", "tok", "key");
        assert_ne!(a, b);
    }

    #[test]
    fn exact_pattern_case_insensitive() {
        let p = RecipientPattern::Exact("Support@Company.COM".to_string());
        assert!(p.matches("support@company.com"));
        assert!(p.matches("SUPPORT@COMPANY.COM"));
        assert!(!p.matches("other@company.com"));
    }

    #[test]
    fn local_prefix_matches_with_and_without_plus() {
        let p = RecipientPattern::LocalPrefix("ticket".to_string());
        assert!(p.matches("ticket@example.com"));
        assert!(p.matches("ticket+123@example.com"));
        assert!(!p.matches("my-ticket@example.com"));
    }

    #[test]
    fn plus_address_extracts_token() {
        let p = RecipientPattern::PlusAddress {
            local: "replies".to_string(),
            domain: Some("app.example".to_string()),
        };
        assert!(p.matches("replies+abc@app.example"));
        assert!(!p.matches("other+abc@app.example"));
        assert!(!p.matches("replies@app.example"));
        assert_eq!(
            p.extract_token("replies+xyz@app.example"),
            Some("xyz".to_string())
        );
    }

    #[test]
    fn plus_address_without_domain_restriction() {
        let p = RecipientPattern::PlusAddress {
            local: "mail".to_string(),
            domain: None,
        };
        assert!(p.matches("mail+token@any.org"));
        assert_eq!(
            p.extract_token("mail+mytoken@anything.com"),
            Some("mytoken".to_string())
        );
    }

    #[test]
    fn any_pattern_matches_everything() {
        let p = RecipientPattern::Any;
        assert!(p.matches("anyone@example.com"));
        assert!(p.matches(""));
        assert!(p.extract_token("irrelevant@example.com").is_none());
    }

    #[test]
    fn rfc5322_parses_basic_email() {
        let raw = "From: alice@example.com\r\n\
                   To: bob@example.com\r\n\
                   Subject: Hello\r\n\
                   \r\n\
                   Body text here.";
        let email = parse_rfc5322(Bytes::from_static(raw.as_bytes()));
        assert_eq!(email.from, "alice@example.com");
        assert_eq!(email.to, vec!["bob@example.com"]);
        assert_eq!(email.subject, "Hello");
        assert_eq!(email.text_body.as_deref(), Some("Body text here."));
    }

    #[test]
    fn rfc5322_parses_multiple_to_addresses() {
        let raw = "From: alice@example.com\r\n\
                   To: bob@example.com, carol@example.com\r\n\
                   Subject: Multi\r\n\
                   \r\n";
        let email = parse_rfc5322(Bytes::from_static(raw.as_bytes()));
        assert_eq!(email.to.len(), 2);
        assert!(email.to.contains(&"bob@example.com".to_string()));
        assert!(email.to.contains(&"carol@example.com".to_string()));
    }

    #[test]
    fn generic_config_no_signing_key() {
        let c = InboundMailEndpointConfig::generic("/inbound");
        assert!(c.signing_key.is_none());
        assert_eq!(c.provider, InboundMailProvider::Generic);
    }

    #[test]
    fn config_resolve_signing_key_prefers_literal() {
        let c = InboundMailEndpointConfig {
            signing_key: Some("literal".to_string()),
            signing_key_env: Some("SOME_VAR".to_string()),
            ..InboundMailEndpointConfig::generic("/p")
        };
        assert_eq!(c.resolve_signing_key().as_deref(), Some("literal"));
    }

    #[test]
    fn config_resolve_signing_key_falls_back_to_env() {
        temp_env::with_var("TEST_AUTUMN_INBOUND_KEY", Some("from-env"), || {
            let c = InboundMailEndpointConfig {
                signing_key: None,
                signing_key_env: Some("TEST_AUTUMN_INBOUND_KEY".to_string()),
                ..InboundMailEndpointConfig::generic("/p")
            };
            assert_eq!(c.resolve_signing_key().as_deref(), Some("from-env"));
        });
    }

    fn now_ts() -> String {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs())
            .to_string()
    }

    #[test]
    fn mailgun_parse_extreme_timestamp_returns_401_not_panic() {
        // i64::MIN as a timestamp would overflow `(now - ts).abs()` in debug
        // builds before the stale-window check rejects it.  Ensure we get 401.
        for ts_str in &[
            i64::MIN.to_string(),
            i64::MAX.to_string(),
            "9999999999999999999".to_string(),
        ] {
            let form: HashMap<String, String> = [
                ("from".to_string(), "u@example.com".to_string()),
                ("to".to_string(), "s@example.com".to_string()),
                ("timestamp".to_string(), ts_str.clone()),
                ("token".to_string(), "tok".to_string()),
                ("signature".to_string(), "sig".to_string()),
            ]
            .into_iter()
            .collect();
            let result = parse_mailgun(&form, "key", Vec::new());
            // Either a parse error (non-i64) or a 401 — must never panic.
            assert!(
                result.is_err(),
                "extreme timestamp {ts_str} must be rejected, not panic"
            );
        }
    }

    #[test]
    fn mailgun_parse_rejects_invalid_signature() {
        let ts = now_ts();
        let form: HashMap<String, String> = [
            ("from".to_string(), "user@example.com".to_string()),
            ("to".to_string(), "support@company.com".to_string()),
            ("subject".to_string(), "Test".to_string()),
            ("timestamp".to_string(), ts),
            ("token".to_string(), "some-token".to_string()),
            ("signature".to_string(), "deadbeef".repeat(8)),
        ]
        .into_iter()
        .collect();

        let result = parse_mailgun(&form, "correct-key", Vec::new());
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn mailgun_parse_accepts_valid_signature() {
        let ts = now_ts();
        let tok = "mytoken";
        let key = "my-signing-key";
        let sig = compute_mailgun_signature(&ts, tok, key);

        let form: HashMap<String, String> = [
            ("from".to_string(), "user@example.com".to_string()),
            ("to".to_string(), "support@company.com".to_string()),
            ("subject".to_string(), "Hello".to_string()),
            ("body-plain".to_string(), "Test body".to_string()),
            ("timestamp".to_string(), ts),
            ("token".to_string(), tok.to_string()),
            ("signature".to_string(), sig),
        ]
        .into_iter()
        .collect();

        let result = parse_mailgun(&form, key, Vec::new());
        assert!(result.is_ok());
        let email = result.unwrap();
        assert_eq!(email.from, "user@example.com");
        assert_eq!(email.to, vec!["support@company.com"]);
        assert_eq!(email.subject, "Hello");
        assert_eq!(email.text_body.as_deref(), Some("Test body"));
    }

    #[test]
    fn mailgun_parse_detects_bounce_header() {
        let ts = now_ts();
        let tok = "bouncetoken";
        let key = "bounce-key";
        let sig = compute_mailgun_signature(&ts, tok, key);

        let form: HashMap<String, String> = [
            ("from".to_string(), "MAILER-DAEMON@mailgun.net".to_string()),
            ("to".to_string(), "bounced@company.com".to_string()),
            ("subject".to_string(), "Delivery failed".to_string()),
            ("timestamp".to_string(), ts),
            ("token".to_string(), tok.to_string()),
            ("signature".to_string(), sig),
            (
                "X-Mailgun-Bounced-Address".to_string(),
                "user@bad-domain.com".to_string(),
            ),
        ]
        .into_iter()
        .collect();

        let email = parse_mailgun(&form, key, Vec::new()).unwrap();
        assert!(email.is_bounce, "top-level bounce field must set is_bounce");
        // The injected header must NOT appear in email.headers — bounce state is
        // tracked only via is_bounce to prevent spoofing via message-headers.
        assert!(
            !email.headers.contains_key("x-mailgun-bounced-address"),
            "bounce header must not bleed into forwarded headers map"
        );
    }

    #[test]
    fn mailgun_bounce_not_triggered_by_injected_message_header() {
        // A sender that embeds X-Mailgun-Bounced-Address in their own email headers
        // must not have those headers treated as a provider bounce signal.
        let ts = now_ts();
        let tok = "regtoken";
        let key = "spoof-key";
        let sig = compute_mailgun_signature(&ts, tok, key);
        // Simulate Mailgun including the injected header in message-headers JSON.
        let injected_headers = r#"[["X-Mailgun-Bounced-Address", "victim@example.com"]]"#;
        let form: HashMap<String, String> = [
            ("from".to_string(), "attacker@evil.example".to_string()),
            ("to".to_string(), "support@company.com".to_string()),
            ("subject".to_string(), "Normal email".to_string()),
            ("timestamp".to_string(), ts),
            ("token".to_string(), tok.to_string()),
            ("signature".to_string(), sig),
            ("message-headers".to_string(), injected_headers.to_string()),
        ]
        .into_iter()
        .collect();

        let email = parse_mailgun(&form, key, Vec::new()).unwrap();
        assert!(
            !email.is_bounce,
            "injected message-header must not set is_bounce"
        );
    }

    #[test]
    fn mailgun_empty_key_returns_500() {
        let result = parse_mailgun(&HashMap::new(), "", Vec::new());
        assert_eq!(result.unwrap_err(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn mailgun_merges_recipient_into_to() {
        let ts = now_ts();
        let tok = "tok";
        let key = "k";
        let sig = compute_mailgun_signature(&ts, tok, key);
        let form: HashMap<String, String> = [
            ("from".to_string(), "u@example.com".to_string()),
            ("to".to_string(), "a@example.com".to_string()),
            ("recipient".to_string(), "b@example.com".to_string()),
            ("timestamp".to_string(), ts),
            ("token".to_string(), tok.to_string()),
            ("signature".to_string(), sig),
        ]
        .into_iter()
        .collect();
        let email = parse_mailgun(&form, key, Vec::new()).unwrap();
        assert!(email.to.contains(&"a@example.com".to_string()));
        assert!(email.to.contains(&"b@example.com".to_string()));
    }

    #[test]
    fn mailgun_display_name_stripped_from_to() {
        let ts = now_ts();
        let tok = "tok2";
        let key = "k2";
        let sig = compute_mailgun_signature(&ts, tok, key);
        let form: HashMap<String, String> = [
            ("from".to_string(), "u@example.com".to_string()),
            (
                "to".to_string(),
                "Support <support@company.com>".to_string(),
            ),
            ("timestamp".to_string(), ts),
            ("token".to_string(), tok.to_string()),
            ("signature".to_string(), sig),
        ]
        .into_iter()
        .collect();
        let email = parse_mailgun(&form, key, Vec::new()).unwrap();
        assert!(email.to.contains(&"support@company.com".to_string()));
    }

    #[test]
    fn mailgun_spam_report_populated() {
        let ts = now_ts();
        let tok = "spam-tok";
        let key = "spam-key";
        let sig = compute_mailgun_signature(&ts, tok, key);
        let form: HashMap<String, String> = [
            ("from".to_string(), "u@example.com".to_string()),
            ("to".to_string(), "r@example.com".to_string()),
            ("timestamp".to_string(), ts),
            ("token".to_string(), tok.to_string()),
            ("signature".to_string(), sig),
            ("X-Mailgun-Spam-Score".to_string(), "5.7".to_string()),
            ("X-Mailgun-Sflag".to_string(), "Yes".to_string()),
        ]
        .into_iter()
        .collect();
        let email = parse_mailgun(&form, key, Vec::new()).unwrap();
        let report = email.spam_report.unwrap();
        assert!((report.score.unwrap() - 5.7_f64).abs() < 0.01);
        assert_eq!(report.verdict.as_deref(), Some("Yes"));
    }

    #[test]
    fn generic_empty_key_returns_500() {
        let headers = HeaderMap::new();
        let result = parse_generic(Bytes::from("test"), Some(""), &headers);
        assert_eq!(result.unwrap_err(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn generic_invalid_signature_returns_401() {
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::HeaderName::from_static("x-inbound-signature"),
            "badhex".parse().unwrap(),
        );
        let result = parse_generic(Bytes::from("body"), Some("key"), &headers);
        assert_eq!(result.unwrap_err(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn generic_no_key_passes_through() {
        let headers = HeaderMap::new();
        let raw = "From: a@b.com\r\nTo: c@d.com\r\nSubject: S\r\n\r\nB";
        let result = parse_generic(Bytes::from(raw), None, &headers);
        assert!(result.is_ok());
        assert_eq!(result.unwrap().subject, "S");
    }

    #[test]
    fn parse_ses_subscription_confirmation() {
        let payload = serde_json::json!({
            "Type": "SubscriptionConfirmation",
            "SubscribeURL": "https://sns.example.com/confirm?token=abc"
        });
        let body = Bytes::from(payload.to_string());
        let result = parse_ses(&body);
        assert!(
            matches!(result, Ok(SnsParseResult::SubscriptionConfirmation { .. })),
            "expected SubscriptionConfirmation"
        );
    }

    #[test]
    fn parse_ses_notification_base64_email() {
        use base64::Engine as _;
        let raw =
            "From: sender@example.com\r\nTo: recv@example.com\r\nSubject: SES-Base64\r\n\r\nHi";
        let encoded = base64::engine::general_purpose::STANDARD.encode(raw);
        let sns = serde_json::json!({ "Type": "Notification", "Message": encoded });
        let result = parse_ses(&Bytes::from(sns.to_string()));
        let Ok(SnsParseResult::Email(email)) = result else {
            panic!("expected Email result, got: {result:?}");
        };
        assert_eq!(email.subject, "SES-Base64");
    }

    #[test]
    fn parse_ses_notification_nested_content_field() {
        use base64::Engine as _;
        let raw = "From: sender@example.com\r\nTo: dest@example.com\r\nSubject: Nested\r\n\r\nBody";
        let encoded = base64::engine::general_purpose::STANDARD.encode(raw);
        let msg_json = serde_json::json!({ "content": encoded });
        let sns = serde_json::json!({
            "Type": "Notification",
            "Message": msg_json.to_string()
        });
        let result = parse_ses(&Bytes::from(sns.to_string()));
        let Ok(SnsParseResult::Email(email)) = result else {
            panic!("expected Email, got: {result:?}");
        };
        assert_eq!(email.subject, "Nested");
    }

    #[test]
    fn parse_ses_unknown_type_returns_400() {
        let result = parse_ses(&Bytes::from("{}"));
        assert_eq!(result.unwrap_err(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn decode_rfc2047_plain_subject_unchanged() {
        assert_eq!(decode_rfc2047("Hello world"), "Hello world");
    }

    #[test]
    fn decode_rfc2047_base64_utf8() {
        use base64::Engine as _;
        let text = "Héllo";
        let b64 = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
        let encoded = format!("=?UTF-8?B?{b64}?=");
        assert_eq!(decode_rfc2047(&encoded), text);
    }

    #[test]
    fn decode_rfc2047_q_encoding() {
        // =?UTF-8?Q?Hello_World?= → "Hello World"
        let encoded = "=?UTF-8?Q?Hello_World?=";
        assert_eq!(decode_rfc2047(encoded), "Hello World");
    }

    #[test]
    fn decode_rfc2047_q_hex_sequence() {
        // =?UTF-8?Q?caf=C3=A9?= → "café"
        let encoded = "=?UTF-8?Q?caf=C3=A9?=";
        assert_eq!(decode_rfc2047(encoded), "café");
    }

    #[test]
    fn decode_rfc2047_adjacent_words_no_space() {
        // Two adjacent encoded words should be joined without whitespace.
        use base64::Engine as _;
        let w1 = base64::engine::general_purpose::STANDARD.encode("Hello");
        let w2 = base64::engine::general_purpose::STANDARD.encode(" World");
        let encoded = format!("=?UTF-8?B?{w1}?= =?UTF-8?B?{w2}?=");
        assert_eq!(decode_rfc2047(&encoded), "Hello World");
    }

    #[test]
    fn decode_rfc2047_mixed_literal_and_encoded() {
        use base64::Engine as _;
        let b64 = base64::engine::general_purpose::STANDARD.encode("World");
        let encoded = format!("Hello =?UTF-8?B?{b64}?=");
        assert_eq!(decode_rfc2047(&encoded), "Hello World");
    }

    #[test]
    fn decode_transfer_encoding_passthrough_for_7bit() {
        assert_eq!(
            decode_transfer_encoding("Hello world", "7bit"),
            "Hello world"
        );
    }

    #[test]
    fn decode_transfer_encoding_base64_decodes() {
        use base64::Engine as _;
        let plain = "Hello, MIME world!";
        let encoded = base64::engine::general_purpose::STANDARD.encode(plain);
        assert_eq!(decode_transfer_encoding(&encoded, "base64"), plain);
    }

    #[test]
    fn decode_transfer_encoding_base64_with_whitespace() {
        // Base64 encoded across lines (as email clients produce).
        use base64::Engine as _;
        let plain = "Hello";
        let encoded = base64::engine::general_purpose::STANDARD.encode(plain);
        let with_ws = format!("{encoded}\r\n");
        assert_eq!(decode_transfer_encoding(&with_ws, "base64"), plain);
    }

    #[test]
    fn decode_transfer_encoding_quoted_printable() {
        let qp = "Subject=3A Hello=20World";
        assert_eq!(
            decode_transfer_encoding(qp, "quoted-printable"),
            "Subject: Hello World"
        );
    }

    #[test]
    fn decode_quoted_printable_soft_line_break() {
        let qp = "long line=\r\ncontinued";
        assert_eq!(decode_quoted_printable(qp), "long linecontinued");
    }

    #[test]
    fn decode_quoted_printable_literal_equals() {
        let qp = "price=3D10";
        assert_eq!(decode_quoted_printable(qp), "price=10");
    }

    #[test]
    fn extract_multipart_bodies_base64_decoded() {
        use base64::Engine as _;
        let b = "boundary99";
        let plain = "Decoded body text";
        let encoded = base64::engine::general_purpose::STANDARD.encode(plain);
        let body = format!(
            "--{b}\r\nContent-Type: text/plain\r\nContent-Transfer-Encoding: base64\r\n\r\n{encoded}\r\n--{b}--\r\n"
        );
        let ct = format!("multipart/mixed; boundary={b}");
        let (text, _html, _) = extract_multipart_bodies(body.as_bytes(), &ct);
        assert_eq!(text.unwrap(), plain);
    }

    #[test]
    fn extract_multipart_bodies_quoted_printable_decoded() {
        let b = "bnd";
        let body = format!(
            "--{b}\r\nContent-Type: text/plain\r\nContent-Transfer-Encoding: quoted-printable\r\n\r\nHello=20World\r\n--{b}--\r\n"
        );
        let ct = format!("multipart/mixed; boundary={b}");
        let (text, _html, _) = extract_multipart_bodies(body.as_bytes(), &ct);
        assert_eq!(text.unwrap(), "Hello World");
    }

    #[test]
    fn extract_boundary_parses_unquoted() {
        assert_eq!(
            extract_boundary("multipart/mixed; boundary=abc123"),
            Some("abc123".to_string())
        );
    }

    #[test]
    fn extract_boundary_parses_quoted() {
        assert_eq!(
            extract_boundary("multipart/mixed; boundary=\"abc=123\""),
            Some("abc=123".to_string())
        );
    }

    #[test]
    fn extract_boundary_returns_none_for_plain_content_type() {
        assert!(extract_boundary("text/plain").is_none());
    }

    #[test]
    fn extract_multipart_bodies_text_and_html() {
        let b = "boundary42";
        let body = format!(
            "--{b}\r\nContent-Type: text/plain\r\n\r\nHello text\r\n\
             --{b}\r\nContent-Type: text/html\r\n\r\n<b>Hello</b>\r\n\
             --{b}--\r\n"
        );
        let ct = format!("multipart/alternative; boundary={b}");
        let (text, html, _) = extract_multipart_bodies(body.as_bytes(), &ct);
        assert_eq!(text.as_deref(), Some("Hello text"));
        assert_eq!(html.as_deref(), Some("<b>Hello</b>"));
    }

    #[test]
    fn extract_multipart_bodies_no_boundary_returns_body_as_text() {
        let (text, html, _) = extract_multipart_bodies(b"plain text", "text/plain");
        assert_eq!(text.as_deref(), Some("plain text"));
        assert!(html.is_none());
    }

    #[test]
    fn rfc5322_parses_multipart_body() {
        let b = "TESTBOUNDARY";
        let raw = format!(
            "From: a@b.com\r\nTo: c@d.com\r\nSubject: Multi\r\n\
             Content-Type: multipart/alternative; boundary={b}\r\n\r\n\
             --{b}\r\nContent-Type: text/plain\r\n\r\nText part\r\n\
             --{b}\r\nContent-Type: text/html\r\n\r\n<p>HTML</p>\r\n\
             --{b}--\r\n"
        );
        let email = parse_rfc5322(Bytes::from(raw));
        assert_eq!(email.text_body.as_deref(), Some("Text part"));
        assert_eq!(email.html_body.as_deref(), Some("<p>HTML</p>"));
    }

    #[test]
    fn rfc5322_html_only_body() {
        let raw = "From: a@b.com\r\nTo: c@d.com\r\nContent-Type: text/html\r\n\r\n<p>Hello</p>";
        let email = parse_rfc5322(Bytes::from_static(raw.as_bytes()));
        assert!(email.text_body.is_none());
        assert_eq!(email.html_body.as_deref(), Some("<p>Hello</p>"));
    }

    #[test]
    fn primary_recipient_returns_first_to() {
        let email = InboundEmail {
            from: "a@b.com".to_string(),
            to: vec!["first@x.com".to_string(), "second@x.com".to_string()],
            cc: vec![],
            subject: String::new(),
            text_body: None,
            html_body: None,
            headers: HashMap::new(),
            attachments: vec![],
            spam_report: None,
            raw: Bytes::new(),
            plus_token: None,
            is_bounce: false,
            bounced_address: None,
            complained_address: None,
        };
        assert_eq!(email.primary_recipient(), Some("first@x.com"));
    }

    #[test]
    fn find_subslice_basic_cases() {
        assert_eq!(find_subslice(b"hello world", b"world"), Some(6));
        assert_eq!(find_subslice(b"hello world", b"xyz"), None);
        assert_eq!(find_subslice(b"", b"abc"), None);
        assert_eq!(find_subslice(b"abc", b"abc"), Some(0));
        assert_eq!(find_subslice(b"aabb", b""), Some(0));
    }

    #[test]
    fn parse_mailgun_form_data_binary_file_preserved() {
        // Verify that non-UTF-8 binary bytes are not corrupted by the parser.
        let b = "formbnd";
        let binary_payload: &[u8] = &[0x80u8, 0xFFu8, 0x00u8, 0x42u8];
        let mut body: Vec<u8> = Vec::new();
        body.extend_from_slice(
            format!("--{b}\r\nContent-Disposition: form-data; name=\"field1\"\r\n\r\nvalue1\r\n")
                .as_bytes(),
        );
        body.extend_from_slice(
            format!(
                "--{b}\r\nContent-Disposition: form-data; name=\"attachment-1\"; \
                 filename=\"test.bin\"\r\nContent-Type: application/octet-stream\r\n\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(binary_payload);
        body.extend_from_slice(format!("\r\n--{b}--\r\n").as_bytes());

        let ct = format!("multipart/form-data; boundary={b}");
        let (fields, files) = parse_mailgun_form_data(&body, &ct);

        assert_eq!(fields.get("field1").map(String::as_str), Some("value1"));
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].filename.as_deref(), Some("test.bin"));
        assert_eq!(files[0].data.as_ref(), binary_payload);
    }

    #[test]
    fn extract_multipart_bodies_collects_attachment() {
        let b = "bndatt";
        let body = format!(
            "--{b}\r\nContent-Type: text/plain\r\n\r\nText body\r\n\
             --{b}\r\nContent-Type: application/pdf\r\n\
             Content-Disposition: attachment; filename=\"doc.pdf\"\r\n\r\nPDFdata\r\n\
             --{b}--\r\n"
        );
        let ct = format!("multipart/mixed; boundary={b}");
        let (text, html, atts) = extract_multipart_bodies(body.as_bytes(), &ct);
        assert_eq!(text.as_deref(), Some("Text body"));
        assert!(html.is_none());
        assert_eq!(atts.len(), 1);
        assert_eq!(atts[0].filename.as_deref(), Some("doc.pdf"));
        assert_eq!(atts[0].content_type, "application/pdf");
        assert_eq!(atts[0].data.as_ref(), b"PDFdata");
    }

    #[test]
    fn extract_multipart_bodies_nested_multipart_alternative() {
        let inner = "inner42";
        let outer = "outer42";
        let inner_body = format!(
            "--{inner}\r\nContent-Type: text/plain\r\n\r\nPlain text\r\n\
             --{inner}\r\nContent-Type: text/html\r\n\r\n<b>HTML</b>\r\n\
             --{inner}--\r\n"
        );
        let body = format!(
            "--{outer}\r\nContent-Type: multipart/alternative; boundary={inner}\r\n\r\n\
             {inner_body}\
             --{outer}--\r\n"
        );
        let ct = format!("multipart/mixed; boundary={outer}");
        let (text, html, _) = extract_multipart_bodies(body.as_bytes(), &ct);
        assert_eq!(text.as_deref(), Some("Plain text"));
        assert_eq!(html.as_deref(), Some("<b>HTML</b>"));
    }

    #[test]
    fn parse_ses_notification_json_message_no_content_returns_422() {
        // Message is valid JSON but missing the required "content" field.
        let msg_json = serde_json::json!({ "other_field": "value" });
        let sns = serde_json::json!({
            "Type": "Notification",
            "Message": msg_json.to_string()
        });
        let result = parse_ses(&Bytes::from(sns.to_string()));
        assert_eq!(result.unwrap_err(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[cfg(feature = "inbound-ses")]
    #[test]
    fn is_valid_sns_cert_url_accepts_aws_https() {
        assert!(sns_verify::is_valid_sns_cert_url(
            "https://sns.us-east-1.amazonaws.com/SimpleNotificationService-abc.pem"
        ));
        assert!(sns_verify::is_valid_sns_cert_url(
            "https://sns.ap-southeast-2.amazonaws.com/cert.pem"
        ));
        assert!(sns_verify::is_valid_sns_cert_url(
            "https://sns.cn-north-1.amazonaws.com.cn/cert.pem"
        ));
    }

    #[cfg(feature = "inbound-ses")]
    #[test]
    fn is_valid_sns_cert_url_rejects_invalid() {
        // http (not https)
        assert!(!sns_verify::is_valid_sns_cert_url(
            "http://sns.us-east-1.amazonaws.com/cert.pem"
        ));
        // non-AWS domain
        assert!(!sns_verify::is_valid_sns_cert_url(
            "https://evil.com/cert.pem"
        ));
        // not starting with sns.
        assert!(!sns_verify::is_valid_sns_cert_url(
            "https://s3.amazonaws.com/cert.pem"
        ));
        // double-dot (path traversal attempt)
        assert!(!sns_verify::is_valid_sns_cert_url(
            "https://sns..amazonaws.com/cert.pem"
        ));
    }

    #[cfg(feature = "inbound-ses")]
    #[test]
    fn canonical_string_notification_includes_required_fields() {
        let json = serde_json::json!({
            "Type": "Notification",
            "MessageId": "abc-123",
            "Message": "hello world",
            "Timestamp": "2024-01-01T00:00:00.000Z",
            "TopicArn": "arn:aws:sns:us-east-1:123:MyTopic"
        });
        let result = sns_verify::canonical_string(&json, "Notification").unwrap();
        // Each present field should appear as "FieldName\nvalue\n".
        assert!(result.contains("Message\nhello world\n"));
        assert!(result.contains("MessageId\nabc-123\n"));
        assert!(result.contains("Type\nNotification\n"));
    }

    #[cfg(feature = "inbound-ses")]
    #[test]
    fn canonical_string_unknown_type_returns_none() {
        let json = serde_json::json!({ "Type": "UnknownType" });
        assert!(sns_verify::canonical_string(&json, "UnknownType").is_none());
    }

    #[test]
    fn primary_recipient_returns_none_for_empty_to() {
        let email = InboundEmail {
            from: "a@b.com".to_string(),
            to: vec![],
            cc: vec![],
            subject: String::new(),
            text_body: None,
            html_body: None,
            headers: HashMap::new(),
            attachments: vec![],
            spam_report: None,
            raw: Bytes::new(),
            plus_token: None,
            is_bounce: false,
            bounced_address: None,
            complained_address: None,
        };
        assert!(email.primary_recipient().is_none());
    }

    #[test]
    fn extract_multipart_bodies_headerless_part_treated_as_text_plain() {
        // A part with no headers at all (no blank-line separator after boundary)
        // must still yield a text_body, defaulting to text/plain per RFC 2045 §5.2.
        let b = "bnd";
        let body = format!("--{b}\r\nHello headerless\r\n--{b}--\r\n");
        let ct = format!("multipart/mixed; boundary={b}");
        let (text, html, atts) = extract_multipart_bodies(body.as_bytes(), &ct);
        assert_eq!(
            text.as_deref(),
            Some("Hello headerless"),
            "headerless part must become text_body"
        );
        assert!(html.is_none());
        assert!(atts.is_empty());
    }

    #[test]
    fn extract_multipart_bodies_boundary_inside_content_not_split() {
        // A boundary token that appears in the middle of a line (not at line-start)
        // must NOT be treated as a MIME boundary delimiter.
        let b = "abc";
        let body = format!(
            "--{b}\r\nContent-Type: text/plain\r\n\r\nsee --{b} here, not a split\r\n--{b}--\r\n"
        );
        let ct = format!("multipart/mixed; boundary={b}");
        let (text, _html, _atts) = extract_multipart_bodies(body.as_bytes(), &ct);
        assert_eq!(
            text.as_deref(),
            Some("see --abc here, not a split"),
            "mid-line boundary must be treated as content"
        );
    }

    #[test]
    fn extract_multipart_bodies_boundary_prefix_not_split() {
        // If the boundary is "abc" but the line starts with "--abc123" (extended token),
        // the RFC 2046 terminator check must reject it as a non-boundary.
        let b = "abc";
        let body = format!(
            "--{b}\r\nContent-Type: text/plain\r\n\r\nhello\r\n--{b}123\r\nmore content\r\n--{b}--\r\n"
        );
        let ct = format!("multipart/mixed; boundary={b}");
        let (text, _html, _atts) = extract_multipart_bodies(body.as_bytes(), &ct);
        assert_eq!(
            text.as_deref(),
            Some("hello\r\n--abc123\r\nmore content"),
            "boundary prefix followed by non-terminator must not split: {text:?}"
        );
    }

    #[test]
    fn extract_multipart_bodies_attachment_quoted_printable_decoded() {
        let b = "bndqp";
        // QP-encoded attachment: "Hello=20World" decodes to "Hello World"
        let body = format!(
            "--{b}\r\n\
             Content-Type: application/octet-stream\r\n\
             Content-Disposition: attachment; filename=\"out.txt\"\r\n\
             Content-Transfer-Encoding: quoted-printable\r\n\
             \r\n\
             Hello=20World\r\n\
             --{b}--\r\n"
        );
        let ct = format!("multipart/mixed; boundary={b}");
        let (_text, _html, atts) = extract_multipart_bodies(body.as_bytes(), &ct);
        assert_eq!(atts.len(), 1);
        // The CRLF before the boundary delimiter is stripped by MIME before QP decode.
        assert_eq!(atts[0].data.as_ref(), b"Hello World");
    }

    #[test]
    fn unfold_mime_headers_drops_crlf_fold() {
        let folded = "Content-Type: text/plain;\r\n charset=utf-8";
        assert_eq!(
            unfold_mime_headers(folded),
            "Content-Type: text/plain; charset=utf-8"
        );
    }

    #[test]
    fn unfold_mime_headers_drops_lf_fold() {
        let folded = "Content-Disposition: form-data;\n name=\"field1\"";
        assert_eq!(
            unfold_mime_headers(folded),
            "Content-Disposition: form-data; name=\"field1\""
        );
    }

    #[test]
    fn extract_multipart_bodies_folded_content_type() {
        let b = "bnd";
        let body = format!(
            "--{b}\r\n\
             Content-Type: text/plain;\r\n charset=utf-8\r\n\
             \r\n\
             Hello\r\n\
             --{b}--\r\n"
        );
        let ct = format!("multipart/mixed; boundary={b}");
        let (text, _, _) = extract_multipart_bodies(body.as_bytes(), &ct);
        assert_eq!(
            text.as_deref(),
            Some("Hello"),
            "folded Content-Type must be unfolded before parsing"
        );
    }

    #[test]
    fn parse_mailgun_form_data_folded_content_disposition() {
        let b = "bnd";
        let body = format!(
            "--{b}\r\n\
             Content-Disposition: form-data;\r\n name=\"field1\"\r\n\
             \r\n\
             hello\r\n\
             --{b}--\r\n"
        );
        let ct = format!("multipart/form-data; boundary={b}");
        let (fields, _) = parse_mailgun_form_data(body.as_bytes(), &ct);
        assert_eq!(
            fields.get("field1").map(String::as_str),
            Some("hello"),
            "folded Content-Disposition must be unfolded before name extraction"
        );
    }

    #[test]
    fn parse_mailgun_form_data_boundary_prefix_not_split() {
        // A line that starts with "--{boundary}" followed by non-terminator bytes
        // (e.g. "--abc123") must NOT be treated as a valid MIME part boundary.
        let b = "abc";
        let body = format!(
            "--{b}\r\nContent-Disposition: form-data; name=\"field1\"\r\n\r\nvalue1\r\n\
             --{b}123\r\nextra content\r\n\
             --{b}--\r\n"
        );
        let ct = format!("multipart/form-data; boundary={b}");
        let (fields, _) = parse_mailgun_form_data(body.as_bytes(), &ct);
        assert_eq!(
            fields.get("field1").map(String::as_str),
            Some("value1\r\n--abc123\r\nextra content"),
            "boundary prefix followed by non-terminator must not split the part"
        );
    }

    #[test]
    fn parse_mailgun_form_data_boundary_must_start_at_line() {
        // A boundary string that appears mid-line (not at position 0 or immediately
        // after a newline) must NOT be treated as a valid MIME part boundary.
        let b = "sep";
        // Embed the delimiter string mid-line inside the value of field1.
        let body = format!(
            "--{b}\r\nContent-Disposition: form-data; name=\"field1\"\r\n\r\nvalue with --{b} inside\r\n\
             --{b}\r\nContent-Disposition: form-data; name=\"field2\"\r\n\r\nvalue2\r\n\
             --{b}--\r\n"
        );
        let ct = format!("multipart/form-data; boundary={b}");
        let (fields, _) = parse_mailgun_form_data(body.as_bytes(), &ct);
        assert_eq!(
            fields.get("field1").map(String::as_str),
            Some("value with --sep inside"),
            "boundary embedded mid-line must not split the part"
        );
        assert_eq!(fields.get("field2").map(String::as_str), Some("value2"),);
    }

    #[test]
    fn rfc5322_single_part_pdf_collected_as_attachment() {
        // A non-multipart message whose Content-Type is application/pdf must have
        // its body collected as an Attachment rather than placed in text_body.
        let pdf_b64 = "JVBERi0xLjA="; // minimal %PDF-1.0 stub, base64-encoded
        let raw = format!(
            "From: sender@example.com\r\nTo: inbox@example.com\r\n\
             Content-Type: application/pdf\r\n\
             Content-Transfer-Encoding: base64\r\n\
             Content-Disposition: attachment; filename=\"doc.pdf\"\r\n\
             \r\n\
             {pdf_b64}\r\n"
        );
        let email = parse_rfc5322(Bytes::from(raw.into_bytes()));
        assert!(
            email.text_body.is_none(),
            "text_body should be empty for attachment"
        );
        assert!(
            email.html_body.is_none(),
            "html_body should be empty for attachment"
        );
        assert_eq!(
            email.attachments.len(),
            1,
            "should have exactly one attachment"
        );
        let att = &email.attachments[0];
        assert_eq!(att.content_type, "application/pdf");
        assert_eq!(att.filename.as_deref(), Some("doc.pdf"));
        // The decoded bytes should match the original PDF stub.
        assert_eq!(att.data.as_ref(), b"%PDF-1.0");
    }

    #[test]
    fn rfc5322_attachment_filename_with_semicolon() {
        // A filename that contains a semicolon must not be truncated when the
        // Content-Disposition parameter is parsed.
        let b64 = base64::engine::general_purpose::STANDARD.encode(b"data");
        let raw = format!(
            "From: s@example.com\r\nTo: r@example.com\r\n\
             Content-Type: application/pdf\r\n\
             Content-Transfer-Encoding: base64\r\n\
             Content-Disposition: attachment; filename=\"Q1;final.pdf\"\r\n\
             \r\n\
             {b64}\r\n"
        );
        let email = parse_rfc5322(Bytes::from(raw.into_bytes()));
        assert_eq!(email.attachments.len(), 1);
        assert_eq!(
            email.attachments[0].filename.as_deref(),
            Some("Q1;final.pdf"),
            "semicolon inside quoted filename must not be treated as a parameter separator"
        );
    }

    #[test]
    fn rfc5322_attachment_qp_binary_not_corrupted() {
        // A QP-encoded attachment with non-UTF-8 bytes (=FF) must not have those
        // bytes replaced by U+FFFD when decoded.
        let raw = "From: s@example.com\r\nTo: r@example.com\r\n\
                   Content-Type: application/octet-stream\r\n\
                   Content-Transfer-Encoding: quoted-printable\r\n\
                   Content-Disposition: attachment; filename=\"bin.dat\"\r\n\
                   \r\n\
                   =FF=80\r\n";
        let email = parse_rfc5322(Bytes::from_static(raw.as_bytes()));
        assert_eq!(email.attachments.len(), 1);
        let data = &email.attachments[0].data;
        assert_eq!(
            data.as_ref(),
            b"\xFF\x80\r\n",
            "non-UTF-8 QP bytes must not be corrupted by UTF-8 replacement"
        );
    }

    #[test]
    fn rfc5322_single_part_attachment_qp_decoded() {
        // A single-part attachment with Content-Transfer-Encoding: quoted-printable
        // must have its body decoded; handlers must not receive raw QP bytes.
        let raw = "From: s@example.com\r\nTo: r@example.com\r\n\
                   Content-Type: application/octet-stream\r\n\
                   Content-Transfer-Encoding: quoted-printable\r\n\
                   Content-Disposition: attachment; filename=\"hello.txt\"\r\n\
                   \r\n\
                   Hello=20World\r\n";
        let email = parse_rfc5322(Bytes::from_static(raw.as_bytes()));
        assert!(email.text_body.is_none());
        assert!(email.html_body.is_none());
        assert_eq!(email.attachments.len(), 1);
        let att = &email.attachments[0];
        assert_eq!(att.filename.as_deref(), Some("hello.txt"));
        // The QP-decoded body retains the trailing CRLF from the message line.
        let decoded = std::str::from_utf8(att.data.as_ref()).unwrap_or("<non-utf8>");
        assert_eq!(
            decoded.trim_end_matches(|c| c == '\r' || c == '\n'),
            "Hello World",
            "QP-encoded attachment must be decoded: got {decoded:?}"
        );
    }

    #[test]
    fn parse_mailgun_form_data_preserves_filename_casing() {
        // RFC 2183 §2: parameter values are case-sensitive.
        // A filename like "Q1-Report.PDF" must not be lowercased to "q1-report.pdf".
        let b = "bound";
        let body = format!(
            "--{b}\r\n\
             Content-Disposition: form-data; name=\"upload\"; filename=\"Q1-Report.PDF\"\r\n\
             Content-Type: application/pdf\r\n\
             \r\n\
             %PDF\r\n\
             --{b}--\r\n"
        );
        let ct = format!("multipart/form-data; boundary={b}");
        let (_, files) = parse_mailgun_form_data(body.as_bytes(), &ct);
        assert_eq!(files.len(), 1);
        assert_eq!(
            files[0].filename.as_deref(),
            Some("Q1-Report.PDF"),
            "original filename casing must be preserved"
        );
    }

    #[test]
    fn parse_ses_notification_preserves_envelope_recipient_casing() {
        // Bcc recipients in mail.destination must retain original case so that
        // extract_token can recover the exact token from a plus-address.
        let msg_json = serde_json::json!({
            "content": "From: sender@example.com\r\nTo: support@example.com\r\nSubject: hi\r\n\r\nbody",
            "mail": {
                "destination": ["Replies+ABC@app.example"]
            }
        });
        let sns = serde_json::json!({
            "Type": "Notification",
            "Message": msg_json.to_string()
        });
        let result = parse_ses(&Bytes::from(sns.to_string()));
        let email = match result.unwrap() {
            SnsParseResult::Email(e) => *e,
            other => panic!("expected Email, got {other:?}"),
        };
        assert!(
            email.to.iter().any(|a| a == "Replies+ABC@app.example"),
            "original casing must be preserved; got: {:?}",
            email.to
        );
    }
}
