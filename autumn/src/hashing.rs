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

#[cfg(test)]
mod tests {
    use sha2::Digest as _;

    use super::push_length_prefixed_field;

    fn digest(fields: &[(&str, &[u8])]) -> [u8; 32] {
        let mut hasher = sha2::Sha256::new();
        for (label, value) in fields {
            push_length_prefixed_field(&mut hasher, label, value);
        }
        hasher.finalize().into()
    }

    #[test]
    fn different_field_splits_do_not_collide() {
        // `column = "ab"` + `transform = "c"` must not hash the same as
        // `column = "a"` + `transform = "bc"` — the ambiguity this function
        // exists to close.
        let split_a = digest(&[("column", b"ab"), ("transform", b"c")]);
        let split_b = digest(&[("column", b"a"), ("transform", b"bc")]);
        assert_ne!(split_a, split_b);
    }

    #[test]
    fn same_fields_hash_the_same() {
        let first = digest(&[("method", b"GET"), ("target", b"/")]);
        let second = digest(&[("method", b"GET"), ("target", b"/")]);
        assert_eq!(first, second);
    }
}
