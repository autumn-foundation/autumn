//! Automatic ACME (Let's Encrypt) certificate provisioning + renewal
//! (issue #1608).
//!
//! This module lets an Autumn app obtain and auto-renew its own TLS
//! certificate over the ACME **HTTP-01** challenge, without an operator-provided
//! certificate on disk or a reverse proxy. It builds directly on the #1603 TLS
//! listener: the issued certificate hot-swaps into the SAME
//! [`ReloadableCertResolver`](crate::tls::ReloadableCertResolver) the listener
//! already serves, so no second resolver or listener is introduced.
//!
//! The moving parts, each in its own submodule:
//!
//! - [`challenge`] — the `:80` listener that answers the ACME HTTP-01 challenge
//!   (`/.well-known/acme-challenge/{token}`) and redirects every other request
//!   to HTTPS.
//! - [`store`] — the [`AcmeStore`](store::AcmeStore) trait and its filesystem
//!   implementation ([`FsAcmeStore`](store::FsAcmeStore)) that persists the ACME
//!   account key and issued certificates (all `0600`).
//! - [`renewal`] — issuance, the renew-before-expiry decision
//!   ([`needs_renewal`](renewal::needs_renewal)), the background renewal loop,
//!   the self-signed placeholder used to bind `:443` before the first issuance,
//!   and the [`AcmeStatus`](renewal::AcmeStatus) /
//!   [`AcmeHealthIndicator`](renewal::AcmeHealthIndicator) observability seam.
//!
//! # Deployment scope: single-host in this slice
//!
//! HTTP-01 ACME as shipped here is **single-host**. The renewal loop
//! leader-elects correctly across a fleet (so only one replica *orders* per
//! certificate), but the HTTP-01 token map ([`challenge::Http01Tokens`]) is
//! per-process in-memory and the only [`store::AcmeStore`] is the local-disk
//! [`store::FsAcmeStore`]. Behind a load balancer, the CA's `:80` validation
//! request can therefore land on a replica that never published the token
//! (→ 404), and non-leader replicas cannot adopt an issued certificate from a
//! non-shared store. Single-replica deployments are fully correct.
//!
//! Multi-replica HTTP-01 needs a shared token store (or DNS-01), tracked in
//! #1620. When ACME is configured alongside a distributed scheduler backend
//! (i.e. a multi-replica deployment) the app emits a startup `warn` to this
//! effect.
//!
//! The crypto backend is `ring` throughout (`instant-acme` and `rcgen` are both
//! pinned `default-features = false` + `ring`), the SAME backend the rest of
//! autumn-web uses — the workspace forbids a second crypto backend.

pub mod challenge;
pub mod dns;
pub mod renewal;
pub mod store;

use crate::config::AcmeDirectory;

/// Resolve an [`AcmeDirectory`] to its ACME directory URL.
///
/// Staging and production map to the Let's Encrypt endpoints; `Custom` returns
/// its configured URL verbatim (a private CA or a pebble test server).
#[must_use]
pub fn directory_url(directory: &AcmeDirectory) -> String {
    match directory {
        AcmeDirectory::Staging => instant_acme::LetsEncrypt::Staging.url().to_owned(),
        AcmeDirectory::Production => instant_acme::LetsEncrypt::Production.url().to_owned(),
        AcmeDirectory::Custom { url } => url.clone(),
    }
}

/// A short, stable label for an [`AcmeDirectory`].
///
/// Used to key the ACME account file per directory so switching directories
/// (staging vs production) cleanly re-registers a fresh account rather than
/// reusing an incompatible one.
#[must_use]
pub fn directory_label(directory: &AcmeDirectory) -> String {
    match directory {
        AcmeDirectory::Staging => "staging".to_owned(),
        AcmeDirectory::Production => "production".to_owned(),
        // A custom URL is hashed to a short, filesystem-safe token so two
        // distinct custom directories get distinct account files.
        AcmeDirectory::Custom { url } => format!("custom-{}", short_hash(url)),
    }
}

/// A short hex digest (first 8 bytes of SHA-256) of `input`, for
/// filesystem-safe identifiers.
#[must_use]
pub fn short_hash(input: &str) -> String {
    use sha2::{Digest as _, Sha256};
    let digest = Sha256::digest(input.as_bytes());
    let mut out = String::with_capacity(16);
    for byte in &digest[..8] {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directory_url_maps_known_directories() {
        assert!(directory_url(&AcmeDirectory::Staging).contains("staging"));
        assert!(!directory_url(&AcmeDirectory::Production).contains("staging"));
        assert_eq!(
            directory_url(&AcmeDirectory::Custom {
                url: "https://pebble.test/dir".to_owned()
            }),
            "https://pebble.test/dir"
        );
    }

    #[test]
    fn directory_label_is_stable_and_distinct() {
        assert_eq!(directory_label(&AcmeDirectory::Staging), "staging");
        assert_eq!(directory_label(&AcmeDirectory::Production), "production");
        let a = directory_label(&AcmeDirectory::Custom {
            url: "https://a.test/dir".to_owned(),
        });
        let b = directory_label(&AcmeDirectory::Custom {
            url: "https://b.test/dir".to_owned(),
        });
        assert_ne!(a, b);
        assert!(a.starts_with("custom-"));
    }

    #[test]
    fn short_hash_is_deterministic() {
        assert_eq!(short_hash("app.example.com"), short_hash("app.example.com"));
        assert_ne!(short_hash("a.example.com"), short_hash("b.example.com"));
        assert_eq!(short_hash("x").len(), 16);
    }
}
