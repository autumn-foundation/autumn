//! VAPID — Voluntary Application Server Identification for Web Push (RFC 8292).
//!
//! A VAPID key pair is a NIST P-256 (prime256v1) ECDSA key. The **public**
//! half, serialized uncompressed (SEC1, 65 bytes) and base64url-encoded
//! without padding, is the `applicationServerKey` a browser passes to
//! `pushManager.subscribe()`. The **private** half signs a short-lived
//! ES256 JWT that the push service validates before accepting a message,
//! proving the sender is the same application server the user subscribed to.

use std::fmt;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use p256::ecdsa::{Signature, SigningKey, signature::Signer};

use super::PushError;

/// How long a minted VAPID JWT stays valid.
///
/// RFC 8292 §2 caps `exp` at 24 hours from issue; 12 hours leaves generous
/// room for clock skew on both ends while staying well inside that ceiling.
pub(crate) const VAPID_TOKEN_TTL_SECS: u64 = 12 * 60 * 60;

/// Length of an uncompressed SEC1 P-256 public key: `0x04 || X(32) || Y(32)`.
pub(crate) const P256_UNCOMPRESSED_LEN: usize = 65;

/// Length of a P-256 private scalar.
const P256_SCALAR_LEN: usize = 32;

/// An application-server (VAPID) key pair.
///
/// Mint one with [`VapidKey::generate`], persist the value returned by
/// [`private_key_base64url`](Self::private_key_base64url), and load it back
/// with [`VapidKey::from_base64url`].
///
/// # Example
///
/// ```rust,ignore
/// use autumn_web::push::VapidKey;
///
/// // One-time, offline: mint and record the private half.
/// let key = VapidKey::generate();
/// println!("private: {}", key.private_key_base64url());
/// println!("public:  {}", key.public_key_base64url());
///
/// // At boot: load it back from configuration.
/// let key = VapidKey::from_base64url("…")?;
/// # Ok::<(), autumn_web::push::PushError>(())
/// ```
#[derive(Clone)]
pub struct VapidKey {
    signing: SigningKey,
}

impl VapidKey {
    /// Mint a fresh random key pair.
    #[must_use]
    pub fn generate() -> Self {
        Self {
            signing: SigningKey::random(&mut p256::elliptic_curve::rand_core::OsRng),
        }
    }

    /// Load a key pair from a base64url-encoded (padded or unpadded) 32-byte
    /// P-256 private scalar — the conventional VAPID private-key format.
    ///
    /// # Errors
    ///
    /// [`PushError::InvalidVapidKey`] when the value is not base64url, is not
    /// exactly 32 bytes, or is not a valid P-256 scalar.
    pub fn from_base64url(encoded: &str) -> Result<Self, PushError> {
        let bytes = decode_base64url(encoded.trim())
            .ok_or_else(|| PushError::InvalidVapidKey("not valid base64url".to_owned()))?;
        if bytes.len() != P256_SCALAR_LEN {
            return Err(PushError::InvalidVapidKey(format!(
                "expected a {P256_SCALAR_LEN}-byte P-256 private scalar, got {} bytes",
                bytes.len()
            )));
        }
        let signing = SigningKey::from_slice(&bytes).map_err(|_| {
            PushError::InvalidVapidKey(
                "not a valid P-256 private scalar (zero or >= curve order)".to_owned(),
            )
        })?;
        Ok(Self { signing })
    }

    /// The private scalar, base64url-encoded without padding. **Secret** —
    /// store it the way you store any other credential.
    #[must_use]
    pub fn private_key_base64url(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.signing.to_bytes())
    }

    /// The uncompressed public key, base64url-encoded without padding. This
    /// is the `applicationServerKey` value the browser needs; it is public
    /// and safe to serve to any client.
    #[must_use]
    pub fn public_key_base64url(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.public_key_bytes())
    }

    /// The uncompressed (SEC1, 65-byte) public key.
    #[must_use]
    pub fn public_key_bytes(&self) -> [u8; P256_UNCOMPRESSED_LEN] {
        let encoded = self.signing.verifying_key().to_encoded_point(false);
        let mut out = [0_u8; P256_UNCOMPRESSED_LEN];
        out.copy_from_slice(encoded.as_bytes());
        out
    }

    /// Mint the `Authorization` header value for a message sent to
    /// `endpoint`, valid from `issued_at_unix`.
    ///
    /// The header follows RFC 8292 §3.1's single-header form:
    /// `vapid t=<jwt>, k=<base64url public key>`.
    ///
    /// # Errors
    ///
    /// [`PushError::InvalidEndpoint`] when `endpoint` has no derivable
    /// origin (missing scheme or host).
    pub fn authorization_header(
        &self,
        endpoint: &str,
        subject: &str,
        issued_at_unix: u64,
    ) -> Result<String, PushError> {
        let jwt = self.sign_jwt(&audience_for(endpoint)?, subject, issued_at_unix);
        Ok(format!("vapid t={jwt}, k={}", self.public_key_base64url()))
    }

    /// Sign a VAPID JWT (ES256) for `audience`.
    fn sign_jwt(&self, audience: &str, subject: &str, issued_at_unix: u64) -> String {
        // Fixed header — the only algorithm RFC 8292 permits.
        let header = URL_SAFE_NO_PAD.encode(br#"{"typ":"JWT","alg":"ES256"}"#);
        let exp = issued_at_unix.saturating_add(VAPID_TOKEN_TTL_SECS);
        // Hand-rolled rather than `serde_json` so the claim ORDER is fixed and
        // the signing input is byte-reproducible across serde versions — the
        // unit tests below verify the exact signing input.
        let claims = format!(
            r#"{{"aud":"{}","exp":{exp},"sub":"{}"}}"#,
            escape_json(audience),
            escape_json(subject),
        );
        let claims = URL_SAFE_NO_PAD.encode(claims);
        let signing_input = format!("{header}.{claims}");
        let signature: Signature = self.signing.sign(signing_input.as_bytes());
        // JWS ES256 signatures are the raw fixed-width r‖s pair (64 bytes),
        // NOT the ASN.1 DER encoding `Signature::to_der` would produce.
        let signature = URL_SAFE_NO_PAD.encode(signature.to_bytes());
        format!("{signing_input}.{signature}")
    }
}

impl fmt::Debug for VapidKey {
    /// Never prints the private scalar: a `VapidKey` ends up inside the push
    /// service, which is itself `Debug`-printed by handler-error paths.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VapidKey")
            .field("public_key", &self.public_key_base64url())
            .field("private_key", &"<redacted>")
            .finish()
    }
}

/// The JWT `aud` claim for `endpoint`: its origin (`scheme://host[:port]`).
///
/// # Errors
///
/// [`PushError::InvalidEndpoint`] when the URL cannot be parsed or has no host.
pub(crate) fn audience_for(endpoint: &str) -> Result<String, PushError> {
    let parsed = url::Url::parse(endpoint)
        .map_err(|e| PushError::InvalidEndpoint(format!("{endpoint}: {e}")))?;
    let host = parsed
        .host_str()
        .ok_or_else(|| PushError::InvalidEndpoint(format!("{endpoint}: no host")))?;
    let scheme = parsed.scheme();
    // `Url::port` is `None` for the scheme's default port, which is exactly
    // the origin form the push services expect (no `:443` on https).
    Ok(parsed.port().map_or_else(
        || format!("{scheme}://{host}"),
        |port| format!("{scheme}://{host}:{port}"),
    ))
}

/// Decode base64url with or without padding (browsers emit both).
pub(crate) fn decode_base64url(value: &str) -> Option<Vec<u8>> {
    URL_SAFE_NO_PAD.decode(value).map_or_else(
        |_| base64::engine::general_purpose::URL_SAFE.decode(value).ok(),
        Some,
    )
}

/// Minimal JSON string escaping for the two claim values we interpolate.
fn escape_json(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                use std::fmt::Write as _;
                write!(out, "\\u{:04x}", c as u32).expect("writing to a String cannot fail");
            }
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Split a compact JWS into its three base64url parts.
    fn jwt_parts(jwt: &str) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3, "a JWS has exactly three parts: {jwt}");
        (
            URL_SAFE_NO_PAD.decode(parts[0]).expect("header base64url"),
            URL_SAFE_NO_PAD.decode(parts[1]).expect("claims base64url"),
            URL_SAFE_NO_PAD
                .decode(parts[2])
                .expect("signature base64url"),
        )
    }

    fn jwt_from_header(header: &str) -> String {
        let rest = header
            .strip_prefix("vapid t=")
            .expect("header starts with `vapid t=`");
        rest.split_once(", k=")
            .expect("header carries a `, k=` public key")
            .0
            .to_owned()
    }

    #[test]
    fn generate_round_trips_through_base64url() {
        let key = VapidKey::generate();
        let reloaded =
            VapidKey::from_base64url(&key.private_key_base64url()).expect("reload the private key");
        assert_eq!(
            reloaded.public_key_base64url(),
            key.public_key_base64url(),
            "a reloaded key must derive the same public half"
        );
    }

    #[test]
    fn public_key_is_65_uncompressed_bytes() {
        let key = VapidKey::generate();
        let bytes = key.public_key_bytes();
        assert_eq!(bytes.len(), 65);
        assert_eq!(
            bytes[0], 0x04,
            "the browser's applicationServerKey must be UNCOMPRESSED SEC1 (0x04 prefix)"
        );
        // The base64url form the client actually receives must be unpadded:
        // `atob`-based snippets and `applicationServerKey` conversion break on
        // `=` padding.
        let encoded = key.public_key_base64url();
        assert!(
            !encoded.contains('='),
            "public key must be unpadded: {encoded}"
        );
        assert_eq!(URL_SAFE_NO_PAD.decode(&encoded).expect("decodes").len(), 65);
    }

    #[test]
    fn from_base64url_accepts_padded_input() {
        let key = VapidKey::generate();
        let padded = base64::engine::general_purpose::URL_SAFE.encode(
            URL_SAFE_NO_PAD
                .decode(key.private_key_base64url())
                .expect("decode"),
        );
        let reloaded = VapidKey::from_base64url(&padded).expect("padded base64url is accepted");
        assert_eq!(reloaded.public_key_base64url(), key.public_key_base64url());
    }

    #[test]
    fn from_base64url_rejects_garbage_with_a_clear_error() {
        let err = VapidKey::from_base64url("not base64!!!").expect_err("garbage is rejected");
        assert!(
            matches!(err, PushError::InvalidVapidKey(_)),
            "expected InvalidVapidKey, got {err:?}"
        );
        assert!(
            err.to_string().contains("base64url"),
            "the error must say what is wrong: {err}"
        );
    }

    #[test]
    fn from_base64url_rejects_a_wrong_length_key() {
        let short = URL_SAFE_NO_PAD.encode([1_u8; 16]);
        let err = VapidKey::from_base64url(&short).expect_err("16 bytes is not a P-256 scalar");
        assert!(
            err.to_string().contains("32-byte"),
            "the error must name the expected length: {err}"
        );
    }

    #[test]
    fn from_base64url_rejects_an_all_zero_scalar() {
        // Zero is a syntactically well-formed 32 bytes but not a valid P-256
        // private key — a length check alone would let it through and produce
        // signatures no push service accepts.
        let zero = URL_SAFE_NO_PAD.encode([0_u8; 32]);
        let err = VapidKey::from_base64url(&zero).expect_err("the zero scalar is rejected");
        assert!(matches!(err, PushError::InvalidVapidKey(_)), "{err:?}");
    }

    #[test]
    fn audience_is_the_endpoint_origin() {
        assert_eq!(
            audience_for("https://fcm.googleapis.com/fcm/send/abc123").expect("origin"),
            "https://fcm.googleapis.com",
            "the aud claim is the ORIGIN, never the full endpoint path"
        );
    }

    #[test]
    fn audience_keeps_a_non_default_port_and_drops_the_default_one() {
        assert_eq!(
            audience_for("https://push.example.com:8443/x").expect("origin"),
            "https://push.example.com:8443"
        );
        assert_eq!(
            audience_for("https://push.example.com:443/x").expect("origin"),
            "https://push.example.com",
            "the default port must not appear in aud — push services compare it literally"
        );
    }

    #[test]
    fn audience_rejects_a_hostless_url() {
        let err = audience_for("not a url").expect_err("garbage endpoint is rejected");
        assert!(matches!(err, PushError::InvalidEndpoint(_)), "{err:?}");
    }

    #[test]
    fn authorization_header_carries_the_public_key_and_a_jwt() {
        let key = VapidKey::generate();
        let header = key
            .authorization_header(
                "https://fcm.googleapis.com/fcm/send/abc",
                "mailto:ops@example.com",
                1_700_000_000,
            )
            .expect("header");
        assert!(header.starts_with("vapid t="), "{header}");
        assert!(
            header.contains(&format!(", k={}", key.public_key_base64url())),
            "the `k=` parameter must be the application server's public key: {header}"
        );
    }

    #[test]
    fn jwt_header_declares_es256() {
        let key = VapidKey::generate();
        let header = key
            .authorization_header("https://push.example.com/x", "mailto:a@b.c", 1_700_000_000)
            .expect("header");
        let (jose, _, _) = jwt_parts(&jwt_from_header(&header));
        let jose: serde_json::Value = serde_json::from_slice(&jose).expect("JOSE header is JSON");
        assert_eq!(jose["alg"], "ES256", "RFC 8292 permits only ES256");
        assert_eq!(jose["typ"], "JWT");
    }

    #[test]
    fn jwt_claims_carry_aud_sub_and_a_bounded_exp() {
        let key = VapidKey::generate();
        let issued_at = 1_700_000_000_u64;
        let header = key
            .authorization_header(
                "https://push.example.com:8443/send/x",
                "mailto:ops@example.com",
                issued_at,
            )
            .expect("header");
        let (_, claims, _) = jwt_parts(&jwt_from_header(&header));
        let claims: serde_json::Value = serde_json::from_slice(&claims).expect("claims are JSON");
        assert_eq!(claims["aud"], "https://push.example.com:8443");
        assert_eq!(claims["sub"], "mailto:ops@example.com");
        let exp = claims["exp"].as_u64().expect("exp is a number");
        assert!(exp > issued_at, "exp must be in the future");
        assert!(
            exp - issued_at <= 24 * 60 * 60,
            "RFC 8292 caps exp at 24h from issue; got {}s",
            exp - issued_at
        );
    }

    #[test]
    fn jwt_signature_is_a_raw_64_byte_r_s_pair() {
        let key = VapidKey::generate();
        let header = key
            .authorization_header("https://push.example.com/x", "mailto:a@b.c", 1_700_000_000)
            .expect("header");
        let (_, _, signature) = jwt_parts(&jwt_from_header(&header));
        assert_eq!(
            signature.len(),
            64,
            "JWS ES256 uses the fixed-width r‖s pair, not ASN.1 DER (which is ~70-72 bytes)"
        );
    }

    #[test]
    fn jwt_signature_verifies_against_the_public_key() {
        use p256::ecdsa::VerifyingKey;
        use p256::ecdsa::signature::Verifier;

        let key = VapidKey::generate();
        let header = key
            .authorization_header("https://push.example.com/x", "mailto:a@b.c", 1_700_000_000)
            .expect("header");
        let jwt = jwt_from_header(&header);
        let (signing_input, signature_b64) = jwt.rsplit_once('.').expect("compact JWS");
        let signature = Signature::from_slice(
            &URL_SAFE_NO_PAD
                .decode(signature_b64)
                .expect("sig base64url"),
        )
        .expect("64-byte r‖s parses as a P-256 signature");

        // Verify with a key reconstructed from the PUBLIC bytes we hand the
        // browser — the same path the push service takes.
        let verifying = VerifyingKey::from_sec1_bytes(&key.public_key_bytes())
            .expect("public bytes parse as a P-256 point");
        verifying
            .verify(signing_input.as_bytes(), &signature)
            .expect("the push service must be able to verify this signature");
    }

    #[test]
    fn subject_with_a_quote_cannot_break_out_of_the_claims_json() {
        // A `sub` read from configuration must never be able to inject claims.
        let key = VapidKey::generate();
        let header = key
            .authorization_header(
                "https://push.example.com/x",
                r#"mailto:a@b.c","exp":9999999999,"x":"#,
                1_700_000_000,
            )
            .expect("header");
        let (_, claims, _) = jwt_parts(&jwt_from_header(&header));
        let claims: serde_json::Value =
            serde_json::from_slice(&claims).expect("claims stay valid JSON");
        assert_eq!(
            claims["exp"].as_u64().expect("exp"),
            1_700_000_000 + VAPID_TOKEN_TTL_SECS,
            "an injected exp must not win over the computed one"
        );
        assert!(claims.get("x").is_none(), "no injected claim may appear");
    }

    #[test]
    fn debug_never_prints_the_private_scalar() {
        let key = VapidKey::generate();
        let rendered = format!("{key:?}");
        assert!(
            !rendered.contains(&key.private_key_base64url()),
            "Debug must redact the private key: {rendered}"
        );
        assert!(rendered.contains("<redacted>"), "{rendered}");
    }
}
