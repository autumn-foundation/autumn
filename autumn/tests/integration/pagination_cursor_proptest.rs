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
    /// different key B (when the keys actually differ).
    #[test]
    fn signed_token_rejects_wrong_key(
        key in arb_key(),
        key_a in prop::collection::vec(any::<u8>(), 0..48),
        key_b in prop::collection::vec(any::<u8>(), 0..48),
    ) {
        prop_assume!(key_a != key_b);
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
