//! Length-prefixed hash field framing.
//!
//! Concatenating labelled fields directly before hashing is ambiguous —
//! `"ab"` + `"c"` hashes the same as `"a"` + `"bc"` — and that ambiguity is a
//! real vulnerability class in this codebase (see
//! `docs/plans/2026-06-05-feedback-bugfixes.md`, Task 19, and
//! `docs/security/2026-09-12-legacy-upload-signature-malleability/README.md`).
//! [`push_length_prefixed_field`] closes it by feeding each field's own
//! length ahead of its bytes, so no two distinct field splits can ever hash
//! the same.
//!
//! [`crate::idempotency`], [`crate::repository_commit_hooks`],
//! [`crate::money::ledger`] and [`crate::derivation`] each held their own
//! byte-identical copy of this function.

use sha2::Digest as _;

/// Length-prefix one labelled field into the hash.
///
/// The length prefix stops two different field splits from hashing the same.
pub fn push_length_prefixed_field(hasher: &mut sha2::Sha256, label: &str, value: &[u8]) {
    hasher.update(label.as_bytes());
    hasher.update(b":");
    hasher.update((value.len() as u64).to_be_bytes());
    hasher.update(b":");
    hasher.update(value);
    hasher.update(b";");
}
