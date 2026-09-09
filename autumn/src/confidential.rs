//! Client-sealed confidential values.
//!
//! Unlike [`crate::encryption`], confidential values are encrypted by the
//! authenticated client's key and are never decrypted by the application.
//! The application stores [`ConfidentialEnvelope`] and uses the separately
//! supplied, owner-scoped [`BlindIndex`] for equality lookup.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine as _;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use subtle::ConstantTimeEq as _;
use thiserror::Error;

const MAGIC: &[u8; 4] = b"ACF\x01";
const NONCE_LEN: usize = 12;
const TAG_LEN: usize = 16;
const MIN_ENVELOPE_LEN: usize = MAGIC.len() + NONCE_LEN + TAG_LEN;
const INDEX_DOMAIN: &[u8] = b"autumn:confidential:index:v1\0";
const AAD_DOMAIN: &[u8] = b"autumn:confidential:owner:v1\0";

type HmacSha256 = Hmac<Sha256>;

/// A client-held key. It must not be sent to, logged by, or persisted by the
/// application; only sealed values and blind-index tokens cross the boundary.
#[derive(Clone)]
pub struct ClientKey([u8; 32]);

impl std::fmt::Debug for ClientKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ClientKey([REDACTED])")
    }
}

impl ClientKey {
    /// Generate a key using the operating system CSPRNG.
    pub fn generate() -> Result<Self, ConfidentialError> {
        let mut key = [0; 32];
        getrandom::getrandom(&mut key).map_err(|_| ConfidentialError::Entropy)?;
        Ok(Self(key))
    }

    /// Construct a key from client-owned key material.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Seal plaintext for `owner` and create its owner-scoped equality token.
    pub fn seal(&self, owner: &str, plaintext: &[u8]) -> Result<SealedValue, ConfidentialError> {
        validate_owner(owner)?;
        let mut nonce = [0; NONCE_LEN];
        getrandom::getrandom(&mut nonce).map_err(|_| ConfidentialError::Entropy)?;
        let cipher = Aes256Gcm::new_from_slice(&self.0).expect("AES-256 key has fixed length");
        let ciphertext = cipher
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: plaintext,
                    aad: &owner_aad(owner),
                },
            )
            .map_err(|_| ConfidentialError::Authentication)?;
        let mut bytes = Vec::with_capacity(MAGIC.len() + NONCE_LEN + ciphertext.len());
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&nonce);
        bytes.extend_from_slice(&ciphertext);
        Ok(SealedValue {
            envelope: ConfidentialEnvelope(
                base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes),
            ),
            blind_index: self.blind_index(owner, plaintext)?,
        })
    }

    /// Produce the only value that should be used for an equality predicate.
    pub fn blind_index(
        &self,
        owner: &str,
        plaintext: &[u8],
    ) -> Result<BlindIndex, ConfidentialError> {
        validate_owner(owner)?;
        let mut mac = <HmacSha256 as Mac>::new_from_slice(&self.0).expect("HMAC accepts fixed key");
        mac.update(INDEX_DOMAIN);
        mac.update(&(owner.len() as u64).to_be_bytes());
        mac.update(owner.as_bytes());
        mac.update(plaintext);
        Ok(BlindIndex(mac.finalize().into_bytes().into()))
    }

    /// Open an envelope. Owner identity is authenticated as AEAD associated
    /// data, so moving ciphertext between owner scopes fails closed.
    pub fn open(
        &self,
        owner: &str,
        envelope: &ConfidentialEnvelope,
    ) -> Result<Vec<u8>, ConfidentialError> {
        validate_owner(owner)?;
        let bytes = envelope.decode()?;
        let cipher = Aes256Gcm::new_from_slice(&self.0).expect("AES-256 key has fixed length");
        cipher
            .decrypt(
                Nonce::from_slice(&bytes[MAGIC.len()..MAGIC.len() + NONCE_LEN]),
                Payload {
                    msg: &bytes[MAGIC.len() + NONCE_LEN..],
                    aad: &owner_aad(owner),
                },
            )
            .map_err(|_| ConfidentialError::Authentication)
    }
}

/// Opaque, validated ciphertext stored by the application.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ConfidentialEnvelope(String);

impl std::fmt::Debug for ConfidentialEnvelope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ConfidentialEnvelope(<ciphertext>)")
    }
}

impl ConfidentialEnvelope {
    /// Parse and validate the structural envelope without decrypting it.
    pub fn parse(encoded: impl Into<String>) -> Result<Self, ConfidentialError> {
        let value = Self(encoded.into());
        value.decode()?;
        Ok(value)
    }

    /// Ciphertext representation suitable for a text database column.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn decode(&self) -> Result<Vec<u8>, ConfidentialError> {
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(&self.0)
            .map_err(|_| ConfidentialError::Malformed("invalid base64url"))?;
        if bytes.len() < MIN_ENVELOPE_LEN {
            return Err(ConfidentialError::Malformed("truncated envelope"));
        }
        if bytes[..MAGIC.len()].ct_eq(MAGIC).unwrap_u8() != 1 {
            return Err(ConfidentialError::Malformed("bad magic or version"));
        }
        Ok(bytes)
    }
}

impl TryFrom<String> for ConfidentialEnvelope {
    type Error = ConfidentialError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl From<ConfidentialEnvelope> for String {
    fn from(value: ConfidentialEnvelope) -> Self {
        value.0
    }
}

/// Fixed-size, owner-scoped deterministic equality token.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct BlindIndex([u8; 32]);

impl std::fmt::Debug for BlindIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("BlindIndex(<token>)")
    }
}

impl BlindIndex {
    /// Encode for a text database column or equality bind.
    #[must_use]
    pub fn encode(&self) -> String {
        hex::encode(self.0)
    }
}

impl TryFrom<String> for BlindIndex {
    type Error = ConfidentialError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        let bytes =
            hex::decode(value).map_err(|_| ConfidentialError::Malformed("invalid blind index"))?;
        let bytes: [u8; 32] = bytes
            .try_into()
            .map_err(|_| ConfidentialError::Malformed("invalid blind-index length"))?;
        Ok(Self(bytes))
    }
}

impl From<BlindIndex> for String {
    fn from(value: BlindIndex) -> Self {
        value.encode()
    }
}

/// Wire payload sent by a client and stored without application decryption.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SealedValue {
    pub envelope: ConfidentialEnvelope,
    pub blind_index: BlindIndex,
}

/// Confidential-value validation/decryption failures. Diagnostics never embed
/// input bytes or key material.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ConfidentialError {
    #[error("confidential owner scope must not be empty")]
    EmptyOwner,
    #[error("malformed confidential envelope: {0}")]
    Malformed(&'static str),
    #[error("confidential envelope authentication failed")]
    Authentication,
    #[error("operating-system entropy is unavailable")]
    Entropy,
}

fn validate_owner(owner: &str) -> Result<(), ConfidentialError> {
    if owner.is_empty() {
        Err(ConfidentialError::EmptyOwner)
    } else {
        Ok(())
    }
}

fn owner_aad(owner: &str) -> Vec<u8> {
    let mut aad = Vec::with_capacity(AAD_DOMAIN.len() + 8 + owner.len());
    aad.extend_from_slice(AAD_DOMAIN);
    aad.extend_from_slice(&(owner.len() as u64).to_be_bytes());
    aad.extend_from_slice(owner.as_bytes());
    aad
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_serialization_and_owner_authentication_never_expose_plaintext() {
        let key = ClientKey::from_bytes([7; 32]);
        let marker = b"marker-that-must-never-leak";
        let sealed = key.seal("owner-a", marker).unwrap();
        let json = serde_json::to_vec(&sealed).unwrap();
        assert!(!json.windows(marker.len()).any(|w| w == marker));
        assert!(!format!("{key:?} {sealed:?}").contains("marker-that-must-never-leak"));
        assert_eq!(key.open("owner-a", &sealed.envelope).unwrap(), marker);
        assert_eq!(
            key.open("owner-b", &sealed.envelope),
            Err(ConfidentialError::Authentication)
        );
        assert_eq!(
            ClientKey::from_bytes([8; 32]).open("owner-a", &sealed.envelope),
            Err(ConfidentialError::Authentication)
        );
        assert_ne!(
            key.blind_index("owner-a", marker).unwrap(),
            key.blind_index("owner-b", marker).unwrap()
        );
    }

    #[test]
    fn malformed_inputs_and_validation_errors_are_value_free() {
        for value in ["", "%%%%", "QUZGAQ"] {
            let err = ConfidentialEnvelope::parse(value).unwrap_err();
            if !value.is_empty() {
                assert!(!err.to_string().contains(value), "diagnostic echoed input");
            }
        }
        assert_eq!(
            ClientKey::from_bytes([1; 32]).seal("", b"secret"),
            Err(ConfidentialError::EmptyOwner)
        );
        assert!(
            serde_json::from_str::<SealedValue>(
                r#"{"envelope":"x","blind_index":"x","plaintext":"secret"}"#
            )
            .is_err()
        );
    }
}
