//! The `[push]` configuration block.
//!
//! ```toml
//! [push]
//! # Mint one offline with `VapidKey::generate()`; keep it secret.
//! private_key = "…"
//! # Optional: declare the matching public half so a mismatched pair is
//! # caught at boot instead of at the first (silently rejected) send.
//! public_key  = "…"
//! # RFC 8292 `sub` claim: how a push service operator reaches you.
//! subject     = "mailto:ops@example.com"
//! # Optional: how long a push service may hold an undelivered message.
//! ttl_secs    = 2419200
//! ```
//!
//! The key is loaded **once, at boot**. A key that is present but unusable is
//! a hard error rather than a quiet fallback to "push disabled": the whole
//! failure mode this guards against is an app that starts cleanly, accepts
//! subscriptions, and never delivers anything.

use secrecy::{ExposeSecret as _, SecretString};
use serde::Deserialize;

use super::{PushError, VapidKey};

/// The default VAPID `sub` claim when none is configured.
///
/// RFC 8292 requires a `mailto:` or `https:` URL — a push service may reject
/// the message outright otherwise — so the default is a syntactically valid
/// placeholder rather than an empty string. Set `[push] subject` to something
/// an operator can actually reach you at.
pub const DEFAULT_VAPID_SUBJECT: &str = "mailto:admin@localhost";

/// Web Push settings (`[push]` section in `autumn.toml`).
///
/// Absent by default, so an app that never sends a push is unaffected.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PushConfig {
    /// The VAPID private key: a base64url-encoded 32-byte P-256 scalar.
    ///
    /// Supply it from an environment variable (`AUTUMN_PUSH__PRIVATE_KEY`) or
    /// the encrypted credentials store rather than committing it.
    ///
    /// Held as a [`SecretString`] for the same reason `cluster.secret` and
    /// `tenancy.jwt_secret` are: `AutumnConfig` derives `Debug` and is
    /// reachable from every handler, so one `tracing::debug!(?config)` or one
    /// panic message would otherwise put the signing key into the log
    /// pipeline.
    #[serde(default)]
    pub private_key: Option<SecretString>,

    /// The matching public key, base64url-encoded.
    ///
    /// Optional and purely a **safety check**: the public half is always
    /// derived from the private one, so declaring it only serves to catch a
    /// mismatched pair at boot. That is worth catching, because a mismatch
    /// otherwise surfaces as every send being rejected by the push service
    /// with no local symptom at all.
    #[serde(default)]
    pub public_key: Option<String>,

    /// The VAPID `sub` claim: a `mailto:` or `https:` URL a push service
    /// operator can use to contact you about your traffic. Defaults to
    /// [`DEFAULT_VAPID_SUBJECT`].
    #[serde(default)]
    pub subject: Option<String>,

    /// How long (seconds) a push service may hold an undelivered message.
    /// Defaults to [`DEFAULT_TTL_SECS`](super::service::DEFAULT_TTL_SECS).
    #[serde(default)]
    pub ttl_secs: Option<u32>,
}

/// Whether `subject` is a VAPID `sub` claim a push service will accept.
///
/// RFC 8292 §2.1 requires a `mailto:` or `https:` URI — it is how a push
/// service operator reaches you about your traffic, so a bare email address or
/// a name is not enough.
pub(super) fn is_valid_vapid_subject(subject: &str) -> bool {
    let subject = subject.trim();
    if let Some(rest) = subject.strip_prefix("mailto:") {
        // `mailto:` with nothing after it names nobody.
        return rest.contains('@') && !rest.starts_with('@') && !rest.ends_with('@');
    }
    url::Url::parse(subject).is_ok_and(|parsed| parsed.scheme() == "https" && parsed.has_host())
}

impl PushConfig {
    /// Load and validate the configured VAPID key.
    ///
    /// Returns `Ok(None)` only when **nothing** is configured. Anything that
    /// looks like an attempt to configure push and cannot work is an error.
    ///
    /// # Errors
    ///
    /// - [`PushError::InvalidVapidKey`] when `private_key` is present but not
    ///   a valid P-256 scalar — including when it is empty, which almost
    ///   always means an environment variable failed to interpolate.
    /// - [`PushError::InvalidConfig`] when `public_key` is set without a
    ///   `private_key`, or does not match the key derived from it.
    pub fn load_vapid_key(&self) -> Result<Option<VapidKey>, PushError> {
        let private = self
            .private_key
            .as_ref()
            .map(|key| key.expose_secret().trim());
        let declared_public = self.public_key.as_deref().map(str::trim);

        let Some(private) = private else {
            if declared_public.is_some_and(|key| !key.is_empty()) {
                return Err(PushError::InvalidConfig(
                    "`[push] public_key` is set without a `private_key`. The public half is \
                     derived from the private one, so a public key alone can never send \
                     anything — set `private_key` too, or drop both."
                        .to_owned(),
                ));
            }
            return Ok(None);
        };

        // Deliberately NOT treated as absent: an empty value is a set value
        // that failed to interpolate, and silently disabling push there is the
        // exact failure this whole path exists to prevent.
        if private.is_empty() {
            return Err(PushError::InvalidVapidKey(
                "`[push] private_key` is empty. The commonest cause is \
                 `AUTUMN_PUSH__PRIVATE_KEY` being set to a secret that failed to \
                 interpolate in this environment — a blank value is refused rather than \
                 treated as \"push disabled\", because silently disabling delivery is \
                 exactly the failure this check exists to prevent."
                    .to_owned(),
            ));
        }

        let key = VapidKey::from_base64url(private)?;

        if let Some(declared) = declared_public.filter(|key| !key.is_empty()) {
            let derived = key.public_key_base64url();
            if declared != derived {
                return Err(PushError::InvalidConfig(format!(
                    "`[push] public_key` does not match the key derived from `private_key` \
                     (configured `{declared}`, derived `{derived}`). Browsers subscribe with \
                     the public key and the push service validates against the private one, \
                     so a mismatched pair means every send is rejected. Update `public_key` \
                     to the derived value, or remove it."
                )));
            }
        }

        // A `sub` the push services will reject is just as fatal as a bad key,
        // and just as invisible: the app boots, signs happily, and every
        // delivery is refused remotely. Validate it here so it fails the boot
        // alongside the key rather than in production.
        self.validated_subject()?;

        Ok(Some(key))
    }

    /// The configured `sub` claim, checked against what RFC 8292 permits.
    ///
    /// # Errors
    ///
    /// [`PushError::InvalidConfig`] when `subject` is set to something that is
    /// neither a `mailto:` nor an `https:` URI.
    pub fn validated_subject(&self) -> Result<String, PushError> {
        let subject = self.subject_or_default();
        if !is_valid_vapid_subject(&subject) {
            return Err(PushError::InvalidConfig(format!(
                "`[push] subject` must be a `mailto:` or `https:` URI (RFC 8292 §2.1), got \
                 `{subject}`. A push service may reject every message signed with anything \
                 else, so this is refused at boot rather than in production."
            )));
        }
        Ok(subject)
    }

    /// The configured `sub` claim, or [`DEFAULT_VAPID_SUBJECT`].
    #[must_use]
    pub fn subject_or_default(&self) -> String {
        self.subject
            .as_deref()
            .map(str::trim)
            .filter(|subject| !subject.is_empty())
            .unwrap_or(DEFAULT_VAPID_SUBJECT)
            .to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(toml: &str) -> PushConfig {
        toml::from_str(toml).expect("config parses")
    }

    #[test]
    fn an_absent_block_configures_no_key() {
        let key = config("")
            .load_vapid_key()
            .expect("an absent [push] is fine");
        assert!(
            key.is_none(),
            "an app that never configured push must still boot"
        );
    }

    #[test]
    fn a_valid_private_key_is_loaded() {
        let generated = VapidKey::generate();
        let loaded = config(&format!(
            "private_key = \"{}\"",
            generated.private_key_base64url()
        ))
        .load_vapid_key()
        .expect("valid key loads")
        .expect("a key is present");
        assert_eq!(
            loaded.public_key_base64url(),
            generated.public_key_base64url()
        );
    }

    #[test]
    fn an_invalid_private_key_fails_fast_rather_than_silently_disabling_push() {
        // The whole point: a typo'd key must stop the boot, not leave the app
        // running with push quietly dead.
        let err = config("private_key = \"obviously-not-a-key\"")
            .load_vapid_key()
            .expect_err("an invalid key is a hard error");
        assert!(matches!(err, PushError::InvalidVapidKey(_)), "{err:?}");
    }

    #[test]
    fn an_empty_private_key_is_rejected_not_treated_as_absent() {
        // `private_key = ""` reads as "I meant to set this" — most often an
        // env var that failed to interpolate. Treating it as absent would
        // silently disable push in exactly the deployment that needed it.
        let err = config("private_key = \"\"")
            .load_vapid_key()
            .expect_err("an empty key is an error, not an absent one");
        assert!(matches!(err, PushError::InvalidVapidKey(_)), "{err:?}");
    }

    #[test]
    fn a_declared_public_key_must_match_the_private_one() {
        // A mismatched pair is the nastiest possible failure: the browser
        // subscribes under one key, the server signs with another, and every
        // send is rejected by the push service with no clue why.
        let generated = VapidKey::generate();
        let other = VapidKey::generate();
        let err = config(&format!(
            "private_key = \"{}\"\npublic_key = \"{}\"",
            generated.private_key_base64url(),
            other.public_key_base64url()
        ))
        .load_vapid_key()
        .expect_err("a mismatched pair is refused");
        assert!(matches!(err, PushError::InvalidConfig(_)), "{err:?}");
        assert!(
            err.to_string().contains("public_key"),
            "the error must name the offending key: {err}"
        );
    }

    #[test]
    fn a_matching_public_key_is_accepted() {
        let generated = VapidKey::generate();
        config(&format!(
            "private_key = \"{}\"\npublic_key = \"{}\"",
            generated.private_key_base64url(),
            generated.public_key_base64url()
        ))
        .load_vapid_key()
        .expect("a matching declared pair is fine");
    }

    #[test]
    fn a_public_key_without_a_private_one_is_rejected() {
        // Public-key-only would mean the browser can subscribe but nothing can
        // ever be sent — a configuration that cannot work.
        let err = config(&format!(
            "public_key = \"{}\"",
            VapidKey::generate().public_key_base64url()
        ))
        .load_vapid_key()
        .expect_err("public-only is refused");
        assert!(matches!(err, PushError::InvalidConfig(_)), "{err:?}");
    }

    #[test]
    fn surrounding_whitespace_in_a_key_is_tolerated() {
        // Keys arrive via env vars and shell here-docs; a trailing newline is
        // not a configuration error.
        let generated = VapidKey::generate();
        config(&format!(
            "private_key = \"  {}\\n\"",
            generated.private_key_base64url()
        ))
        .load_vapid_key()
        .expect("whitespace is trimmed")
        .expect("key present");
    }

    #[test]
    fn subject_defaults_to_a_valid_vapid_sub_claim() {
        // RFC 8292 requires `sub` to be a mailto: or https: URL; a push
        // service may reject anything else.
        let subject = PushConfig::default().subject_or_default();
        assert!(
            subject.starts_with("mailto:") || subject.starts_with("https://"),
            "the default sub must be a valid VAPID subject, got {subject}"
        );
    }

    #[test]
    fn a_configured_subject_wins() {
        assert_eq!(
            config("subject = \"mailto:ops@example.com\"").subject_or_default(),
            "mailto:ops@example.com"
        );
    }
    // ── Subject validation (RFC 8292 §2.1) ──────────────────────────────────

    #[test]
    fn a_bare_email_address_is_not_a_valid_subject() {
        // The commonest mistake, and the most invisible: the app boots, signs
        // happily, and every push service refuses the delivery.
        let err = config("subject = \"ops@example.com\"")
            .validated_subject()
            .expect_err("a bare address is not a `mailto:` URI");
        assert!(matches!(err, PushError::InvalidConfig(_)), "{err:?}");
        assert!(
            err.to_string().contains("mailto:"),
            "the error must say what a valid subject looks like: {err}"
        );
    }

    #[test]
    fn mailto_and_https_subjects_are_accepted() {
        for subject in [
            "mailto:ops@example.com",
            "https://example.com/contact",
            "  mailto:ops@example.com  ",
        ] {
            config(&format!("subject = \"{}\"", subject.replace('"', "")))
                .validated_subject()
                .unwrap_or_else(|e| panic!("{subject} must be accepted, got {e}"));
        }
    }

    #[test]
    fn a_subject_with_a_wrong_scheme_is_rejected() {
        // `http://` in particular: the point of the `sub` claim is that a push
        // service operator can reach the sender, and RFC 8292 names only
        // `mailto:` and `https:`.
        for subject in ["http://example.com", "tel:+15550100", "example.com"] {
            let err = config(&format!("subject = \"{subject}\""))
                .validated_subject()
                .expect_err("this subject must be refused");
            assert!(
                matches!(err, PushError::InvalidConfig(_)),
                "{subject}: {err:?}"
            );
        }
    }

    #[test]
    fn a_malformed_mailto_is_rejected() {
        for subject in ["mailto:", "mailto:@example.com", "mailto:ops@"] {
            assert!(
                config(&format!("subject = \"{subject}\""))
                    .validated_subject()
                    .is_err(),
                "{subject} must be refused"
            );
        }
    }

    #[test]
    fn the_default_subject_passes_its_own_validation() {
        PushConfig::default()
            .validated_subject()
            .expect("the shipped default must itself be a valid VAPID subject");
    }

    #[test]
    fn an_invalid_subject_fails_the_boot_alongside_the_key() {
        // `load_vapid_key` is what `AutumnConfig::validate_push` calls, so the
        // subject has to be checked there or a bad one boots cleanly.
        let key = VapidKey::generate();
        let err = config(&format!(
            "private_key = \"{}\"\nsubject = \"nonsense\"",
            key.private_key_base64url()
        ))
        .load_vapid_key()
        .expect_err("an invalid subject is a boot failure");
        assert!(matches!(err, PushError::InvalidConfig(_)), "{err:?}");
    }
}
