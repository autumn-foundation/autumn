//! Opaque client-sealed text values.

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use thiserror::Error;

const VERSION: u8 = 1;
const ALGORITHM: &str = "A256GCM";
const NONCE_LEN: usize = 12;
const BLIND_INDEX_LEN: usize = 32;
const TAG_LEN: usize = 16;
const MAX_CIPHERTEXT_LEN: usize = 16 * 1024;
const MAX_ENVELOPE_LEN: usize = 24 * 1024;

/// A user or tenant that owns a confidential value.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConfidentialOwner {
    /// A stable user identifier.
    User(String),
    /// A stable tenant identifier.
    Tenant(String),
}

/// The owner proved by an authenticated session.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ConfidentialOwnerContext {
    /// The authenticated user owner, when present.
    pub user_id: Option<String>,
    /// The authenticated tenant owner, when present.
    pub tenant_id: Option<String>,
}

/// Public values bound to the AEAD tag by the client.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfidentialBinding<'a> {
    pub application: &'a str,
    pub model: &'a str,
    pub field: &'a str,
    pub owner: &'a ConfidentialOwner,
    pub record_id: &'a str,
}

impl ConfidentialBinding<'_> {
    /// Return the canonical AEAD associated data.
    #[must_use]
    #[allow(clippy::cast_possible_truncation)]
    pub fn associated_data(&self) -> Vec<u8> {
        let (kind, id) = match self.owner {
            ConfidentialOwner::User(id) => ("user", id.as_str()),
            ConfidentialOwner::Tenant(id) => ("tenant", id.as_str()),
        };
        let mut out = Vec::new();
        for value in [
            self.application,
            self.model,
            self.field,
            kind,
            id,
            self.record_id,
        ] {
            let bytes = value.as_bytes();
            out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
            out.extend_from_slice(bytes);
        }
        out
    }

    fn validate(&self) -> Result<(), ConfidentialError> {
        if [self.application, self.model, self.field, self.record_id]
            .iter()
            .any(|value| value.is_empty())
        {
            return Err(ConfidentialError::EmptyIdentity);
        }
        match self.owner {
            ConfidentialOwner::User(id) | ConfidentialOwner::Tenant(id) if id.is_empty() => {
                Err(ConfidentialError::EmptyIdentity)
            }
            _ => Ok(()),
        }
    }
}

/// A sealed text value. It does not expose plaintext.
#[derive(Clone, Eq, PartialEq)]
pub struct ConfidentialText {
    envelope: SealedEnvelope,
}

impl std::fmt::Debug for ConfidentialText {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ConfidentialText([SEALED])")
    }
}

#[derive(Clone, Eq, PartialEq, Serialize)]
pub struct SealedEnvelope {
    version: u8,
    algorithm: String,
    nonce: String,
    ciphertext: String,
    blind_index: String,
    key_generation: u32,
}

#[derive(Deserialize)]
struct InputEnvelope {
    version: u8,
    algorithm: String,
    nonce: String,
    ciphertext: String,
    blind_index: String,
    key_generation: u32,
}

/// Errors for confidential-value boundary checks.
#[derive(Debug, Error, Eq, PartialEq)]
pub enum ConfidentialError {
    #[error("an authenticated owner is required")]
    AbsentOwner,
    #[error("the authenticated owner is ambiguous")]
    AmbiguousOwner,
    #[error("the authenticated owner does not own this value")]
    OwnerMismatch,
    #[error("a binding identity is empty")]
    EmptyIdentity,
    #[error("the sealed envelope is too large")]
    Oversized,
    #[error("the sealed envelope is malformed")]
    Malformed,
    #[error("the sealed envelope version is not supported")]
    UnsupportedVersion,
    #[error("the sealed envelope algorithm is not supported")]
    UnsupportedAlgorithm,
}

impl ConfidentialText {
    /// Validate a client envelope for an insert.
    ///
    /// # Errors
    ///
    /// Returns an error when the owner or envelope is invalid.
    pub fn for_insert(
        json: &[u8],
        binding: &ConfidentialBinding<'_>,
        context: &ConfidentialOwnerContext,
    ) -> Result<Self, ConfidentialError> {
        binding.validate()?;
        check_owner(binding.owner, context)?;
        if json.len() > MAX_ENVELOPE_LEN {
            return Err(ConfidentialError::Oversized);
        }
        let input: InputEnvelope =
            serde_json::from_slice(json).map_err(|_| ConfidentialError::Malformed)?;
        if input.version != VERSION {
            return Err(ConfidentialError::UnsupportedVersion);
        }
        if input.algorithm != ALGORITHM {
            return Err(ConfidentialError::UnsupportedAlgorithm);
        }
        let nonce = decode(&input.nonce)?;
        let ciphertext = decode(&input.ciphertext)?;
        let blind_index = decode(&input.blind_index)?;
        if nonce.len() != NONCE_LEN
            || blind_index.len() != BLIND_INDEX_LEN
            || !(TAG_LEN..=MAX_CIPHERTEXT_LEN).contains(&ciphertext.len())
            || input.key_generation == 0
        {
            return Err(ConfidentialError::Malformed);
        }
        Ok(Self {
            envelope: SealedEnvelope {
                version: input.version,
                algorithm: input.algorithm,
                nonce: input.nonce,
                ciphertext: input.ciphertext,
                blind_index: input.blind_index,
                key_generation: input.key_generation,
            },
        })
    }

    /// Return the envelope to its authenticated owner.
    ///
    /// # Errors
    ///
    /// Returns an error when the session does not prove the owner.
    pub fn for_owner(
        &self,
        owner: &ConfidentialOwner,
        context: &ConfidentialOwnerContext,
    ) -> Result<&SealedEnvelope, ConfidentialError> {
        check_owner(owner, context)?;
        Ok(&self.envelope)
    }
}

fn decode(value: &str) -> Result<Vec<u8>, ConfidentialError> {
    URL_SAFE_NO_PAD
        .decode(value)
        .map_err(|_| ConfidentialError::Malformed)
}

fn check_owner(
    owner: &ConfidentialOwner,
    context: &ConfidentialOwnerContext,
) -> Result<(), ConfidentialError> {
    let authenticated = match (&context.user_id, &context.tenant_id) {
        (None, None) => return Err(ConfidentialError::AbsentOwner),
        (Some(_), Some(_)) => return Err(ConfidentialError::AmbiguousOwner),
        (Some(id), None) => ConfidentialOwner::User(id.clone()),
        (None, Some(id)) => ConfidentialOwner::Tenant(id.clone()),
    };
    if &authenticated == owner {
        Ok(())
    } else {
        Err(ConfidentialError::OwnerMismatch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn envelope() -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "version": 1,
            "algorithm": "A256GCM",
            "nonce": URL_SAFE_NO_PAD.encode([1; 12]),
            "ciphertext": URL_SAFE_NO_PAD.encode([2; 16]),
            "blind_index": URL_SAFE_NO_PAD.encode([3; 32]),
            "key_generation": 1
        }))
        .unwrap()
    }

    #[test]
    fn insert_and_read_require_the_exact_owner() {
        let owner = ConfidentialOwner::User("u1".into());
        let binding = ConfidentialBinding {
            application: "app",
            model: "Note",
            field: "body",
            owner: &owner,
            record_id: "42",
        };
        let context = ConfidentialOwnerContext {
            user_id: Some("u1".into()),
            tenant_id: None,
        };
        let value = ConfidentialText::for_insert(&envelope(), &binding, &context).unwrap();
        assert_eq!(value.for_owner(&owner, &context).unwrap().version, 1);
        assert_eq!(format!("{value:?}"), "ConfidentialText([SEALED])");
        assert_eq!(binding.associated_data(), binding.associated_data());
    }

    #[test]
    fn rejects_bad_boundaries_before_storage() {
        let owner = ConfidentialOwner::Tenant("t1".into());
        let binding = ConfidentialBinding {
            application: "app",
            model: "Note",
            field: "body",
            owner: &owner,
            record_id: "42",
        };
        assert_eq!(
            ConfidentialText::for_insert(
                &envelope(),
                &binding,
                &ConfidentialOwnerContext::default()
            )
            .unwrap_err(),
            ConfidentialError::AbsentOwner
        );
        let ambiguous = ConfidentialOwnerContext {
            user_id: Some("u1".into()),
            tenant_id: Some("t1".into()),
        };
        assert_eq!(
            ConfidentialText::for_insert(&envelope(), &binding, &ambiguous).unwrap_err(),
            ConfidentialError::AmbiguousOwner
        );
    }
}
