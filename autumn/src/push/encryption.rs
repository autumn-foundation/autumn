//! RFC 8291 "Message Encryption for Web Push" over the RFC 8188 `aes128gcm`
//! content coding.
//!
//! A Web Push payload is encrypted *for the browser*, not for the push
//! service: the push service is an untrusted relay that only ever sees
//! ciphertext. The receiving user agent publishes two values in its
//! `PushSubscription` — a P-256 public key (`p256dh`) and a 16-byte
//! authentication secret (`auth`) — and RFC 8291 combines them with a
//! per-message ephemeral key pair to derive the content encryption key.
//!
//! Per message:
//!
//! 1. Mint a fresh ephemeral P-256 key pair (the "application server" key).
//! 2. ECDH it against the subscription's `p256dh` to get a shared secret.
//! 3. `IKM = HKDF(salt = auth, ikm = ecdh, info = "WebPush: info\0" ‖ ua ‖ as, 32)`
//! 4. Mint a fresh random 16-byte `salt`, then derive from `HKDF(salt, IKM, …)`
//!    the 16-byte content encryption key (`"Content-Encoding: aes128gcm\0"`)
//!    and the 12-byte nonce (`"Content-Encoding: nonce\0"`).
//! 5. AES-128-GCM the plaintext with a trailing `0x02` record delimiter.
//! 6. Frame it: `salt(16) ‖ rs(4, BE) ‖ idlen(1) ‖ as_public(65) ‖ ciphertext`.
//!
//! Both the salt and the ephemeral key MUST be fresh per message — reuse
//! would repeat the AES-GCM (key, nonce) pair across messages, which destroys
//! confidentiality. [`encrypt`] is the only entry point application code
//! reaches; [`encrypt_with`] exists so the RFC's published test vector can be
//! reproduced exactly.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes128Gcm, Key, Nonce};
use hmac::{Hmac, Mac};
use p256::elliptic_curve::sec1::ToEncodedPoint;
use sha2::Sha256;

use super::PushError;
use super::vapid::P256_UNCOMPRESSED_LEN;

/// Length of the `auth` secret a browser publishes (RFC 8291 §3.2).
const AUTH_SECRET_LEN: usize = 16;
/// Length of the per-message content encryption key (AES-128).
const CEK_LEN: usize = 16;
/// Length of the AES-GCM nonce.
const NONCE_LEN: usize = 12;
/// Length of the AES-GCM authentication tag.
const TAG_LEN: usize = 16;
/// Bytes of framing before the ciphertext: salt ‖ rs ‖ idlen ‖ key.
const HEADER_LEN: usize = 16 + 4 + 1 + P256_UNCOMPRESSED_LEN;
/// RFC 8188 delimiter marking the final record.
const LAST_RECORD_DELIMITER: u8 = 0x02;

/// The `rs` (record size) written into the header.
///
/// Everything Autumn sends is a single record, so this only needs to exceed
/// the record's own length; 4096 is the value RFC 8291's example uses and the
/// one every push service is required to accept.
pub(crate) const RECORD_SIZE: u32 = 4096;

/// The largest plaintext that still fits an encrypted body of [`RECORD_SIZE`].
///
/// `4096 - header(86) - delimiter(1) - GCM tag(16)`. Push services are only
/// required to accept 4096 octets, so exceeding this is refused up front with
/// [`PushError::PayloadTooLarge`] rather than dispatched and rejected
/// remotely.
pub(crate) const MAX_PLAINTEXT_LEN: usize =
    RECORD_SIZE as usize - HEADER_LEN - 1 - TAG_LEN;

/// Encrypt `plaintext` for a subscription, minting a fresh salt and ephemeral
/// key pair.
///
/// # Errors
///
/// [`PushError::InvalidSubscriptionKey`] for a malformed `p256dh`/`auth`,
/// [`PushError::PayloadTooLarge`] past [`MAX_PLAINTEXT_LEN`], and
/// [`PushError::Encryption`] if AES-GCM itself fails.
pub(crate) fn encrypt(
    plaintext: &[u8],
    ua_public: &[u8],
    auth_secret: &[u8],
) -> Result<Vec<u8>, PushError> {
    let mut salt = [0_u8; 16];
    // `OsRng` is the operating system CSPRNG — the same source `SigningKey::
    // random` draws from. A non-cryptographic RNG here would be a real break,
    // not a style choice.
    rand::TryRngCore::try_fill_bytes(&mut rand::rngs::OsRng, &mut salt)
        .map_err(|e| PushError::Encryption(format!("could not draw a random salt: {e}")))?;
    let ephemeral = p256::SecretKey::random(&mut p256::elliptic_curve::rand_core::OsRng);
    encrypt_with(plaintext, ua_public, auth_secret, salt, ephemeral)
}

/// [`encrypt`] with the salt and ephemeral key supplied by the caller.
///
/// Deterministic, so RFC 8291 §5's published vector can be reproduced exactly.
/// Application code always goes through [`encrypt`]: passing a salt or key
/// that has been used before repeats an AES-GCM (key, nonce) pair.
///
/// # Errors
///
/// See [`encrypt`].
pub(crate) fn encrypt_with(
    plaintext: &[u8],
    ua_public: &[u8],
    auth_secret: &[u8],
    salt: [u8; 16],
    ephemeral: p256::SecretKey,
) -> Result<Vec<u8>, PushError> {
    if plaintext.len() > MAX_PLAINTEXT_LEN {
        return Err(PushError::PayloadTooLarge {
            len: plaintext.len(),
            max: MAX_PLAINTEXT_LEN,
        });
    }
    if auth_secret.len() != AUTH_SECRET_LEN {
        return Err(PushError::InvalidSubscriptionKey(format!(
            "`auth` must be exactly {AUTH_SECRET_LEN} bytes, got {}",
            auth_secret.len()
        )));
    }
    // `from_sec1_bytes` rejects both a wrong length and a point that is not on
    // the curve, so a hostile `p256dh` cannot steer the ECDH onto a weak curve.
    let ua_key = p256::PublicKey::from_sec1_bytes(ua_public).map_err(|_| {
        PushError::InvalidSubscriptionKey(
            "`p256dh` is not an uncompressed P-256 public key on the curve".to_owned(),
        )
    })?;

    let as_public_point = ephemeral.public_key().to_encoded_point(false);
    let as_public = as_public_point.as_bytes();
    debug_assert_eq!(as_public.len(), P256_UNCOMPRESSED_LEN);

    // ── Step 1: ECDH ────────────────────────────────────────────────────────
    let shared = p256::ecdh::diffie_hellman(ephemeral.to_nonzero_scalar(), ua_key.as_affine());

    // ── Step 2: the RFC 8291 §3.3 combined IKM ──────────────────────────────
    // info = "WebPush: info" ‖ 0x00 ‖ ua_public ‖ as_public
    let mut key_info = Vec::with_capacity(14 + P256_UNCOMPRESSED_LEN * 2);
    key_info.extend_from_slice(b"WebPush: info\0");
    key_info.extend_from_slice(ua_key.to_encoded_point(false).as_bytes());
    key_info.extend_from_slice(as_public);
    let ikm = hkdf_sha256(auth_secret, shared.raw_secret_bytes(), &key_info, 32);

    // ── Step 3: the RFC 8188 content key and nonce ──────────────────────────
    let cek = hkdf_sha256(&salt, &ikm, b"Content-Encoding: aes128gcm\0", CEK_LEN);
    let nonce = hkdf_sha256(&salt, &ikm, b"Content-Encoding: nonce\0", NONCE_LEN);

    // ── Step 4: AES-128-GCM over `plaintext ‖ 0x02` ─────────────────────────
    let mut record = Vec::with_capacity(plaintext.len() + 1);
    record.extend_from_slice(plaintext);
    record.push(LAST_RECORD_DELIMITER);
    let cipher = Aes128Gcm::new(Key::<Aes128Gcm>::from_slice(&cek));
    let ciphertext = cipher
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: &record,
                // `aes128gcm` binds the header through the key derivation, not
                // through AEAD associated data (RFC 8188 §2).
                aad: b"",
            },
        )
        .map_err(|e| PushError::Encryption(format!("AES-128-GCM: {e}")))?;

    // ── Step 5: RFC 8188 framing ────────────────────────────────────────────
    let mut body = Vec::with_capacity(HEADER_LEN + ciphertext.len());
    body.extend_from_slice(&salt);
    body.extend_from_slice(&RECORD_SIZE.to_be_bytes());
    // The key id length is a single octet; a P-256 point is always 65 bytes.
    body.push(
        u8::try_from(P256_UNCOMPRESSED_LEN)
            .expect("65 fits in a u8"),
    );
    body.extend_from_slice(as_public);
    body.extend_from_slice(&ciphertext);
    Ok(body)
}

/// HKDF-SHA256 (RFC 5869): extract with `salt`, then expand `info` to `len`
/// bytes.
///
/// Hand-rolled over the `hmac` crate already in the dependency graph rather
/// than adding the `hkdf` crate for ~15 lines. Every output Web Push needs is
/// at most 32 bytes, i.e. a single expansion block, but the loop is written
/// generally so the RFC 5869 test vectors (which go to 42 bytes) exercise it.
pub(crate) fn hkdf_sha256(salt: &[u8], ikm: &[u8], info: &[u8], len: usize) -> Vec<u8> {
    type HmacSha256 = Hmac<Sha256>;

    // Extract: PRK = HMAC(salt, IKM). `new_from_slice` accepts any key length.
    let mut extract = <HmacSha256 as Mac>::new_from_slice(salt).expect("HMAC accepts any key length");
    extract.update(ikm);
    let prk = extract.finalize().into_bytes();

    // Expand: T(n) = HMAC(PRK, T(n-1) ‖ info ‖ n).
    let mut okm = Vec::with_capacity(len);
    let mut previous: Vec<u8> = Vec::new();
    let mut counter: u8 = 1;
    while okm.len() < len {
        let mut expand = <HmacSha256 as Mac>::new_from_slice(&prk).expect("HMAC accepts any key length");
        expand.update(&previous);
        expand.update(info);
        expand.update(&[counter]);
        previous = expand.finalize().into_bytes().to_vec();
        okm.extend_from_slice(&previous);
        counter += 1;
    }
    okm.truncate(len);
    okm
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::push::vapid::decode_base64url;
    use base64::Engine as _;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;

    // ── RFC 8291 §5 published test vector ───────────────────────────────────
    //
    // Every value below is copied verbatim from RFC 8291 Section 5 ("Push
    // Message Encryption Example"). Pinning the RFC's own vector — not a
    // round-trip against our own code — is what proves this implementation
    // interoperates with real push services and real browsers.

    const RFC_PLAINTEXT: &str = "When I grow up, I want to be a watermelon";
    /// The user agent's public key (`p256dh`).
    const RFC_UA_PUBLIC: &str =
        "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4";
    /// The user agent's authentication secret (`auth`).
    const RFC_AUTH_SECRET: &str = "BTBZMqHH6r4Tts7J_aSIgg";
    /// The application server's ephemeral private key for this message.
    const RFC_AS_PRIVATE: &str = "yfWPiYE-n46HLnH0KqZOF1fJJU3MYrct3AELtAQ-oRw";
    /// The application server's ephemeral public key for this message.
    const RFC_AS_PUBLIC: &str =
        "BP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A8";
    const RFC_SALT: &str = "DGv6ra1nlYgDCS1FRnbzlw";
    /// The complete `aes128gcm` body the RFC's inputs must produce.
    const RFC_BODY: &str = "DGv6ra1nlYgDCS1FRnbzlwAAEABBBP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27ml\
         mlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A_yl95bQpu6cVPT\
         pK4Mqgkf1CXztLVBSt2Ks3oZwbuwXPXLWyouBWLVWGNWQexSgSxsj_Qulcy4a-fN";

    fn b64(value: &str) -> Vec<u8> {
        decode_base64url(value).expect("test vector is base64url")
    }

    fn rfc_salt() -> [u8; 16] {
        let mut salt = [0_u8; 16];
        salt.copy_from_slice(&b64(RFC_SALT));
        salt
    }

    fn rfc_ephemeral() -> p256::SecretKey {
        p256::SecretKey::from_slice(&b64(RFC_AS_PRIVATE)).expect("RFC private key parses")
    }

    #[test]
    fn matches_the_rfc_8291_published_vector() {
        let body = encrypt_with(
            RFC_PLAINTEXT.as_bytes(),
            &b64(RFC_UA_PUBLIC),
            &b64(RFC_AUTH_SECRET),
            rfc_salt(),
            rfc_ephemeral(),
        )
        .expect("encrypting the RFC vector succeeds");

        assert_eq!(
            URL_SAFE_NO_PAD.encode(&body),
            RFC_BODY.replace([' ', '\n'], ""),
            "the aes128gcm body must match RFC 8291 §5 byte-for-byte — anything \
             else will not decrypt in a real browser"
        );
    }

    #[test]
    fn body_is_framed_as_rfc_8188_requires() {
        let body = encrypt_with(
            RFC_PLAINTEXT.as_bytes(),
            &b64(RFC_UA_PUBLIC),
            &b64(RFC_AUTH_SECRET),
            rfc_salt(),
            rfc_ephemeral(),
        )
        .expect("encrypt");

        assert_eq!(&body[..16], &rfc_salt()[..], "bytes 0..16 are the salt");
        assert_eq!(
            u32::from_be_bytes([body[16], body[17], body[18], body[19]]),
            RECORD_SIZE,
            "bytes 16..20 are the big-endian record size"
        );
        assert_eq!(body[20], 65, "byte 20 is the key id length (65 for P-256)");
        assert_eq!(
            URL_SAFE_NO_PAD.encode(&body[21..86]),
            RFC_AS_PUBLIC,
            "bytes 21..86 are the application server's ephemeral public key"
        );
        assert_eq!(
            body.len(),
            21 + 65 + RFC_PLAINTEXT.len() + 1 + 16,
            "header + key + (plaintext ‖ 0x02 delimiter) + GCM tag"
        );
    }

    #[test]
    fn hkdf_matches_rfc_5869_test_case_1() {
        // RFC 5869 Appendix A.1 — an independent check that the HKDF-SHA256
        // used for every derived key here is correct, separate from the
        // Web Push framing.
        let ikm = [0x0b_u8; 22];
        let salt: Vec<u8> = (0x00..=0x0c).collect();
        let info: Vec<u8> = (0xf0..=0xf9).collect();
        let okm = hkdf_sha256(&salt, &ikm, &info, 42);
        assert_eq!(
            hex::encode(okm),
            "3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf34007208d5b887185865"
        );
    }

    #[test]
    fn each_message_uses_a_fresh_salt_and_ephemeral_key() {
        // Reusing either across messages would leak plaintext relationships;
        // the random entry point must never produce the same body twice.
        let first = encrypt(
            RFC_PLAINTEXT.as_bytes(),
            &b64(RFC_UA_PUBLIC),
            &b64(RFC_AUTH_SECRET),
        )
        .expect("encrypt");
        let second = encrypt(
            RFC_PLAINTEXT.as_bytes(),
            &b64(RFC_UA_PUBLIC),
            &b64(RFC_AUTH_SECRET),
        )
        .expect("encrypt");
        assert_ne!(&first[..16], &second[..16], "salt must be freshly random");
        assert_ne!(
            &first[21..86],
            &second[21..86],
            "the ephemeral key must be freshly random"
        );
    }

    #[test]
    fn rejects_a_malformed_p256dh_key() {
        let err = encrypt(b"hi", &[0x04, 0x01, 0x02], &b64(RFC_AUTH_SECRET))
            .expect_err("a 3-byte p256dh is not a P-256 point");
        assert!(
            matches!(err, PushError::InvalidSubscriptionKey(_)),
            "{err:?}"
        );
    }

    #[test]
    fn rejects_a_p256dh_that_is_not_on_the_curve() {
        // Right length and right prefix, but not a curve point: accepting it
        // would be an invalid-curve attack surface.
        let mut bogus = b64(RFC_UA_PUBLIC);
        bogus[64] ^= 0x01;
        let err = encrypt(b"hi", &bogus, &b64(RFC_AUTH_SECRET))
            .expect_err("an off-curve point is rejected");
        assert!(
            matches!(err, PushError::InvalidSubscriptionKey(_)),
            "{err:?}"
        );
    }

    #[test]
    fn rejects_a_wrong_length_auth_secret() {
        let err = encrypt(b"hi", &b64(RFC_UA_PUBLIC), &[0_u8; 8])
            .expect_err("auth is exactly 16 bytes");
        assert!(
            matches!(err, PushError::InvalidSubscriptionKey(_)),
            "{err:?}"
        );
        assert!(err.to_string().contains("16"), "{err}");
    }

    #[test]
    fn rejects_a_payload_that_would_not_fit_a_single_record() {
        let too_big = vec![b'x'; MAX_PLAINTEXT_LEN + 1];
        let err = encrypt(&too_big, &b64(RFC_UA_PUBLIC), &b64(RFC_AUTH_SECRET))
            .expect_err("oversize payloads are refused, not silently truncated");
        assert!(
            matches!(err, PushError::PayloadTooLarge { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn accepts_a_payload_exactly_at_the_limit() {
        let at_limit = vec![b'x'; MAX_PLAINTEXT_LEN];
        let body = encrypt(&at_limit, &b64(RFC_UA_PUBLIC), &b64(RFC_AUTH_SECRET))
            .expect("the documented maximum must actually be sendable");
        assert!(
            body.len() <= 4096,
            "the encrypted body must fit the 4096-byte floor every push service \
             is required to accept, got {}",
            body.len()
        );
    }

    #[test]
    fn empty_payload_still_produces_a_valid_record() {
        let body = encrypt(b"", &b64(RFC_UA_PUBLIC), &b64(RFC_AUTH_SECRET)).expect("encrypt");
        assert_eq!(body.len(), 21 + 65 + 1 + 16, "delimiter + tag only");
    }
}
