//! Client-computed blind indexes for equality lookup over encrypted columns.
//!
//! A blind index is an equality/frequency-leaking search aid, not encryption and
//! not proof that matching plaintexts are equal. Applications must fetch every
//! row whose token matches and decrypt and compare the candidate plaintext.

use hmac::{Hmac, Mac};
use sha2::Sha256;
use subtle::ConstantTimeEq as _;
use thiserror::Error;
use unicode_normalization::UnicodeNormalization as _;

/// The only normalization algorithm supported by this release.
pub const NORMALIZATION_V1: u16 = 1;
/// PRF/domain format version.
pub const PRF_VERSION: u8 = 1;

/// An explicitly versioned token key. Keep old keys while rows using their
/// version exist; the version is stored beside the token in the database.
#[derive(Clone, Copy)]
pub struct TokenKey<'a> {
    /// Application-assigned key generation identifier.
    pub generation: u32,
    /// Secret PRF key (at least 32 random bytes is recommended).
    pub secret: &'a [u8],
}

/// Domain separation inputs. None may be omitted or inferred by the server.
#[derive(Clone, Copy)]
pub struct Domain<'a> {
    pub application: &'a str,
    pub model: &'a str,
    pub field: &'a str,
    /// Tenant/user identifier, or an explicit stable global-scope identifier.
    pub owner_scope: &'a str,
    pub normalization_version: u16,
}

/// A database-ready blind-index token and its key generation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Token {
    pub key_generation: u32,
    pub bytes: [u8; 32],
}

/// Errors are closed: clients cannot silently choose their own normalization.
#[derive(Debug, Error, Eq, PartialEq)]
pub enum BlindIndexError {
    #[error("unsupported blind-index normalization version {0}; supported version is 1")]
    UnsupportedNormalization(u16),
    #[error("blind-index domain component `{0}` must not be empty")]
    EmptyDomain(&'static str),
}

/// Canonical v1 normalization: Unicode NFKC, Unicode lowercase, trim leading
/// and trailing whitespace. The exact order is part of the persisted format.
pub fn normalize(value: &str, version: u16) -> Result<String, BlindIndexError> {
    if version != NORMALIZATION_V1 {
        return Err(BlindIndexError::UnsupportedNormalization(version));
    }
    Ok(value
        .nfkc()
        .flat_map(char::to_lowercase)
        .collect::<String>()
        .trim()
        .to_owned())
}

fn component(mac: &mut Hmac<Sha256>, value: &[u8]) {
    mac.update(&(value.len() as u64).to_be_bytes());
    mac.update(value);
}

/// Compute `HMAC-SHA256(key, version || length-prefixed domain || plaintext)`.
/// This function is intended to run in the trusted client/application process;
/// never expose it as an unauthenticated chosen-input token oracle.
pub fn compute(
    key: TokenKey<'_>,
    domain: Domain<'_>,
    plaintext: &str,
) -> Result<Token, BlindIndexError> {
    for (name, value) in [
        ("application", domain.application),
        ("model", domain.model),
        ("field", domain.field),
        ("owner_scope", domain.owner_scope),
    ] {
        if value.is_empty() {
            return Err(BlindIndexError::EmptyDomain(name));
        }
    }
    let normalized = normalize(plaintext, domain.normalization_version)?;
    let mut mac =
        <Hmac<Sha256> as Mac>::new_from_slice(key.secret).expect("HMAC accepts any key length");
    mac.update(b"autumn:blind-index\0");
    mac.update(&[PRF_VERSION]);
    component(&mut mac, domain.application.as_bytes());
    component(&mut mac, domain.model.as_bytes());
    component(&mut mac, domain.field.as_bytes());
    component(&mut mac, domain.owner_scope.as_bytes());
    component(&mut mac, &domain.normalization_version.to_be_bytes());
    component(&mut mac, &key.generation.to_be_bytes());
    component(&mut mac, normalized.as_bytes());
    Ok(Token {
        key_generation: key.generation,
        bytes: mac.finalize().into_bytes().into(),
    })
}

/// Constant-time Rust token equality. PostgreSQL queries should use ordinary
/// equality only against the dedicated token column (and key-version column).
#[must_use]
pub fn tokens_equal(left: &Token, right: &Token) -> bool {
    let generations = left
        .key_generation
        .to_be_bytes()
        .ct_eq(&right.key_generation.to_be_bytes());
    bool::from(generations & left.bytes.ct_eq(&right.bytes))
}

/// Resolve a token collision after decrypting a candidate row. A token match
/// only selects candidates; this canonical plaintext comparison decides equality.
pub fn verify_candidate(
    expected_plaintext: &str,
    decrypted_candidate: &str,
    normalization_version: u16,
) -> Result<bool, BlindIndexError> {
    let expected = normalize(expected_plaintext, normalization_version)?;
    let candidate = normalize(decrypted_candidate, normalization_version)?;
    Ok(bool::from(expected.as_bytes().ct_eq(candidate.as_bytes())))
}

/// Metadata emitted by `#[model]` for migration tooling.
#[derive(Debug)]
pub struct BlindIndexColumnDescriptor {
    pub model: &'static str,
    pub table: &'static str,
    pub ciphertext_column: &'static str,
    pub token_column: &'static str,
    pub key_version_column: &'static str,
    pub migration_name: &'static str,
    pub index_name: &'static str,
}
inventory::collect!(BlindIndexColumnDescriptor);

#[cfg(test)]
mod tests {
    use super::*;
    const KEY: &[u8] = b"0123456789abcdef0123456789abcdef";
    fn token(app: &str, model: &str, field: &str, owner: &str, text: &str) -> Token {
        compute(
            TokenKey {
                generation: 7,
                secret: KEY,
            },
            Domain {
                application: app,
                model,
                field,
                owner_scope: owner,
                normalization_version: 1,
            },
            text,
        )
        .unwrap()
    }
    #[test]
    fn deterministic_vector_and_all_domains_are_separated() {
        let base = token(
            "billing",
            "User",
            "email",
            "tenant:1",
            " Alice@example.com ",
        );
        assert_eq!(
            hex::encode(base.bytes),
            "7c2e15f656a547c346119876948e99b0d12217d980fa30b4580104872083e84f"
        );
        assert_ne!(
            base,
            token("support", "User", "email", "tenant:1", "alice@example.com")
        );
        assert_ne!(
            base,
            token(
                "billing",
                "Contact",
                "email",
                "tenant:1",
                "alice@example.com"
            )
        );
        assert_ne!(
            base,
            token("billing", "User", "login", "tenant:1", "alice@example.com")
        );
        assert_ne!(
            base,
            token("billing", "User", "email", "tenant:2", "alice@example.com")
        );
    }
    #[test]
    fn scope_properties_and_no_plaintext_marker() {
        for marker in ["MARKER-plaintext-937", "another secret", "💾 Secret"] {
            let a = token("app", "Model", "field", "owner:a", marker);
            let b = token("app", "Model", "field", "owner:b", marker);
            assert_ne!(a, b);
            assert!(!hex::encode(a.bytes).contains(marker));
        }
    }
    #[test]
    fn normalization_is_canonical_and_unknown_versions_fail() {
        assert_eq!(normalize("  Ａlice  ", 1).unwrap(), "alice");
        assert_eq!(
            normalize("x", 2),
            Err(BlindIndexError::UnsupportedNormalization(2))
        );
    }
    #[test]
    fn collision_candidates_require_plaintext_verification() {
        assert!(verify_candidate(" Alice ", "alice", 1).unwrap());
        assert!(!verify_candidate("alice", "mallory", 1).unwrap());
    }
}
