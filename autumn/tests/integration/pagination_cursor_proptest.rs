//! Property-based invariants for opaque and signed cursor tokens.
//!
//! Covers [`Cursor::encode`]/[`Cursor::decode`] and the HMAC-signed
//! [`Cursor::encode_signed`]/[`Cursor::decode_signed`]. These back pagination
//! cursors that may carry tenant/scope boundaries, so round-trip fidelity,
//! forgery rejection, and never-panic decoding of hostile input all matter.

use autumn_web::pagination::Cursor;
use proptest::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Key {
    id: i64,
    ts: u64,
    tenant: String,
}

prop_compose! {
    fn arb_key()(id in any::<i64>(), ts in any::<u64>(), tenant in ".*") -> Key {
        Key { id, ts, tenant }
    }
}

/// The identity of an HMAC key, for "are these two keys actually different?".
///
/// HMAC-SHA256 zero-pads any key shorter than its 64-byte block size, so `[]`,
/// `[0]` and `[0, 0]` are all the *same* key and mint byte-identical
/// signatures. Raw slice inequality therefore does not mean two keys differ as
/// HMAC keys, and a forgery test that assumes it does is asking for a token
/// signed with key A to be rejected under a key B that is, in fact, key A.
/// Stripping trailing zero bytes collapses each key to the padded form HMAC
/// actually uses, so `!=` on the result means what the test needs it to mean.
///
/// Scoped to keys at or under the block size, which is all this file draws
/// (0..48 bytes); a longer key is hashed rather than padded, so it would need
/// the hash, not this.
fn hmac_key_identity(key: &[u8]) -> &[u8] {
    let significant = key.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
    &key[..significant]
}

/// Pins the padding property the assumption above encodes, so the reason that
/// assumption is not simply `key_a != key_b` cannot be lost: two keys that
/// differ only in trailing zero bytes verify each other's tokens, because they
/// are one key as far as HMAC is concerned.
#[test]
fn keys_differing_only_in_zero_padding_are_one_hmac_key() {
    let key = Key {
        id: 7,
        ts: 11,
        tenant: "acme".to_owned(),
    };
    let token = Cursor::encode_signed(&key, &[0]).expect("encode_signed never fails");

    let decoded: Option<Key> = Cursor::decode_signed(&token, &[]);

    assert_eq!(
        decoded,
        Some(key),
        "`[0]` and `[]` are the same HMAC key once zero-padded to the block \
         size, so a token signed with one must verify under the other"
    );
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Unsigned round-trip: decode(encode(v)) == Some(v) for any representable
    /// value.
    #[test]
    fn encode_decode_round_trips(key in arb_key()) {
        let token = Cursor::encode(&key).expect("encode never fails for this type");
        let decoded: Option<Key> = Cursor::decode(&token);
        prop_assert_eq!(decoded, Some(key));
    }

    /// `decode` never panics on ARBITRARY input — a tampered or stale cursor
    /// must degrade to `None`, not crash the handler.
    #[test]
    fn decode_never_panics(token in ".*") {
        let _decoded: Option<Key> = Cursor::decode(&token);
    }

    /// Signed round-trip: a token minted and verified with the same key decodes
    /// back to the original value.
    #[test]
    fn encode_signed_round_trips(key in arb_key(), secret in prop::collection::vec(any::<u8>(), 0..48)) {
        let token = Cursor::encode_signed(&key, &secret).expect("encode_signed never fails for this type");
        let decoded: Option<Key> = Cursor::decode_signed(&token, &secret);
        prop_assert_eq!(decoded, Some(key));
    }

    /// Forgery rejection: a token signed with key A must NOT verify under a
    /// key B that is genuinely a different key — see `hmac_key_identity` for
    /// why "genuinely" is doing work there.
    #[test]
    fn signed_token_rejects_wrong_key(
        key in arb_key(),
        key_a in prop::collection::vec(any::<u8>(), 0..48),
        key_b in prop::collection::vec(any::<u8>(), 0..48),
    ) {
        // Not `key_a != key_b`: keys differing only in trailing zero bytes are
        // the same HMAC key (see `hmac_key_identity`), so that weaker
        // assumption lets through pairs where rejection is impossible.
        prop_assume!(hmac_key_identity(&key_a) != hmac_key_identity(&key_b));
        let token = Cursor::encode_signed(&key, &key_a).expect("encode_signed never fails");
        let decoded: Option<Key> = Cursor::decode_signed(&token, &key_b);
        prop_assert_eq!(decoded, None);
    }

    /// Payload tampering is always detected: flipping any byte of the *payload*
    /// portion of a signed token makes verification fail (`None`). This is the
    /// security-relevant invariant — an attacker must not be able to change the
    /// signed value (e.g. a tenant/scope boundary) without invalidating the
    /// signature.
    ///
    /// Note: the invariant is deliberately scoped to the payload, not the whole
    /// token. The signature portion is base64url of the 32-byte HMAC (43 chars,
    /// 258 bits with 2 slack low-bits), so its final character has several
    /// encodings that decode to the *same* signature bytes; flipping into an
    /// equivalent encoding still verifies. That is signature-encoding
    /// non-canonicality, not a payload forgery — the signed value is unchanged.
    #[test]
    fn signed_token_detects_payload_tampering(
        key in arb_key(),
        secret in prop::collection::vec(any::<u8>(), 1..48),
        idx in any::<prop::sample::Index>(),
        flip in 1u8..=255,
    ) {
        let token = Cursor::encode_signed(&key, &secret).expect("encode_signed never fails");
        let dot = token.find('.').expect("signed token has a `.` separator");
        // The payload base64url portion is non-empty (Key serializes to a
        // non-empty JSON object).
        prop_assume!(dot > 0);
        let mut bytes = token.into_bytes();
        let i = idx.index(dot); // strictly within the payload portion
        bytes[i] ^= flip;
        // Base64url payload bytes are ASCII, so a flip keeps it valid UTF-8, but
        // guard anyway.
        if let Ok(mutated) = String::from_utf8(bytes) {
            let decoded: Option<Key> = Cursor::decode_signed(&mutated, &secret);
            prop_assert_eq!(decoded, None);
        }
    }

    /// `decode_signed` never panics on ARBITRARY input.
    #[test]
    fn decode_signed_never_panics(token in ".*", secret in prop::collection::vec(any::<u8>(), 0..32)) {
        let _decoded: Option<Key> = Cursor::decode_signed(&token, &secret);
    }
}
