//! AWS Signature Version 4 request signing.
//!
//! A small, dependency-free (beyond `hmac`/`sha2`/`hex`) implementation of the
//! `SigV4` primitives, shared by every part of Autumn that talks to an
//! S3-compatible endpoint:
//!
//! * `autumn_web::replication::s3` — the in-process `SQLite` replicator (#1628);
//! * `autumn db backup --upload` / `autumn db restore --offsite` — the CLI's
//!   offsite artifact transfer (#1619).
//!
//! Both sign identically because they sign here. The functions are **pure over
//! their inputs** — no clock, no environment, no I/O — so the canonical request,
//! the string-to-sign and the derived key are all directly unit-testable against
//! AWS's published test vectors.
//!
//! # Credential safety
//!
//! The secret access key is used only to derive the signing key. Nothing in this
//! module formats, logs, or returns it, and no type here holds it.

use std::fmt::Write as _;

use hmac::{Hmac, Mac};
use sha2::{Digest as _, Sha256};

type HmacSha256 = Hmac<Sha256>;

/// The `SigV4` algorithm identifier.
pub const ALGORITHM: &str = "AWS4-HMAC-SHA256";

/// The terminator of a `SigV4` credential scope.
pub const AWS4_REQUEST: &str = "aws4_request";

/// SHA-256 of the empty string: the payload hash for a bodyless request
/// (`GET` / `HEAD` / `DELETE`).
pub const EMPTY_PAYLOAD_SHA256: &str =
    "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// A header name/value pair for signing. Names must be lowercase.
pub type Header = (String, String);

/// Lowercase hex SHA-256 of `data`.
#[must_use]
pub fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hex::encode(hasher.finalize())
}

/// `HMAC-SHA256(key, data)`.
///
/// # Panics
///
/// Never in practice: HMAC accepts a key of any length.
#[must_use]
pub fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(data);
    mac.finalize().into_bytes().into()
}

/// AWS URI-encode `s`.
///
/// Unreserved characters (`A-Za-z0-9-._~`) pass through; everything else is
/// percent-encoded uppercase. `/` passes through when `encode_slash` is false,
/// which is what an object-key path component wants.
#[must_use]
pub fn uri_encode(s: &str, encode_slash: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char);
            }
            b'/' if !encode_slash => out.push('/'),
            _ => {
                let _ = write!(out, "%{b:02X}");
            }
        }
    }
    out
}

/// The `SignedHeaders` list: header names joined by `;`. `headers` must already
/// be sorted by lowercase name.
#[must_use]
pub fn signed_headers_list(headers: &[Header]) -> String {
    headers
        .iter()
        .map(|(name, _)| name.as_str())
        .collect::<Vec<_>>()
        .join(";")
}

/// Build the canonical query string: `key=value` pairs, each side URI-encoded
/// (slashes included), sorted by encoded key.
#[must_use]
pub fn canonical_query(params: &[(&str, &str)]) -> String {
    let mut encoded: Vec<(String, String)> = params
        .iter()
        .map(|(k, v)| (uri_encode(k, true), uri_encode(v, true)))
        .collect();
    encoded.sort();
    encoded
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&")
}

/// Build the `SigV4` canonical request string.
///
/// `headers` MUST be sorted by lowercase name; each value is trimmed.
#[must_use]
pub fn canonical_request(
    method: &str,
    canonical_uri: &str,
    canonical_query: &str,
    headers: &[Header],
    payload_hash: &str,
) -> String {
    let canonical_headers: String = headers
        .iter()
        .fold(String::new(), |mut acc, (name, value)| {
            let _ = writeln!(acc, "{name}:{}", value.trim());
            acc
        });
    let signed = signed_headers_list(headers);
    format!(
        "{method}\n{canonical_uri}\n{canonical_query}\n{canonical_headers}\n{signed}\n{payload_hash}"
    )
}

/// The credential scope: `{date}/{region}/{service}/aws4_request`.
#[must_use]
pub fn credential_scope(date: &str, region: &str, service: &str) -> String {
    format!("{date}/{region}/{service}/{AWS4_REQUEST}")
}

/// Build the string-to-sign from the canonical request's hash.
#[must_use]
pub fn string_to_sign(amz_date: &str, scope: &str, canonical_request_hash: &str) -> String {
    format!("{ALGORITHM}\n{amz_date}\n{scope}\n{canonical_request_hash}")
}

/// Derive the signing key: a four-stage HMAC chain over the secret.
#[must_use]
pub fn signing_key(secret: &str, date: &str, region: &str, service: &str) -> [u8; 32] {
    let k_date = hmac_sha256(format!("AWS4{secret}").as_bytes(), date.as_bytes());
    let k_region = hmac_sha256(&k_date, region.as_bytes());
    let k_service = hmac_sha256(&k_region, service.as_bytes());
    hmac_sha256(&k_service, AWS4_REQUEST.as_bytes())
}

/// Compute the lowercase-hex signature for a fully assembled canonical request.
#[must_use]
pub fn signature(
    secret: &str,
    date: &str,
    region: &str,
    service: &str,
    amz_date: &str,
    scope: &str,
    canonical_req: &str,
) -> String {
    let key = signing_key(secret, date, region, service);
    let sts = string_to_sign(amz_date, scope, &sha256_hex(canonical_req.as_bytes()));
    hex::encode(hmac_sha256(&key, sts.as_bytes()))
}

/// Assemble the `Authorization` header value.
#[must_use]
pub fn authorization_header(
    access_key_id: &str,
    scope: &str,
    headers: &[Header],
    signature: &str,
) -> String {
    format!(
        "{ALGORITHM} Credential={access_key_id}/{scope}, SignedHeaders={}, Signature={signature}",
        signed_headers_list(headers),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_payload_hash_is_the_sha256_of_the_empty_string() {
        assert_eq!(sha256_hex(b""), EMPTY_PAYLOAD_SHA256);
    }

    #[test]
    fn uri_encode_follows_the_aws_unreserved_set() {
        assert_eq!(uri_encode("abcXYZ019-._~", true), "abcXYZ019-._~");
        assert_eq!(uri_encode("a/b", false), "a/b");
        assert_eq!(uri_encode("a/b", true), "a%2Fb");
        assert_eq!(uri_encode("a b+c", true), "a%20b%2Bc");
        assert_eq!(uri_encode("ü", true), "%C3%BC");
    }

    #[test]
    fn canonical_query_sorts_and_encodes() {
        assert_eq!(
            canonical_query(&[("prefix", "a/b"), ("list-type", "2")]),
            "list-type=2&prefix=a%2Fb"
        );
        assert_eq!(canonical_query(&[]), "");
    }

    #[test]
    fn canonical_request_has_the_documented_shape() {
        let headers = vec![
            ("host".to_owned(), "example.com".to_owned()),
            ("x-amz-date".to_owned(), "  20260902T000000Z  ".to_owned()),
        ];
        assert_eq!(
            canonical_request("PUT", "/bucket/key", "", &headers, "PAYLOAD"),
            "PUT\n/bucket/key\n\nhost:example.com\nx-amz-date:20260902T000000Z\n\n\
             host;x-amz-date\nPAYLOAD"
        );
    }

    /// AWS's published `SigV4` test vector (`get-vanilla` from the
    /// `aws-sig-v4-test-suite`), which pins the whole chain: canonical request →
    /// string-to-sign → derived key → signature.
    #[test]
    fn matches_the_published_aws_test_vector() {
        let headers = vec![
            ("host".to_owned(), "example.amazonaws.com".to_owned()),
            ("x-amz-date".to_owned(), "20150830T123600Z".to_owned()),
        ];
        let canonical = canonical_request("GET", "/", "", &headers, EMPTY_PAYLOAD_SHA256);
        assert_eq!(
            canonical,
            "GET\n/\n\nhost:example.amazonaws.com\nx-amz-date:20150830T123600Z\n\n\
             host;x-amz-date\ne3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        let scope = credential_scope("20150830", "us-east-1", "service");
        assert_eq!(scope, "20150830/us-east-1/service/aws4_request");
        let sig = signature(
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            "20150830",
            "us-east-1",
            "service",
            "20150830T123600Z",
            &scope,
            &canonical,
        );
        assert_eq!(
            sig,
            "5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"
        );
        assert_eq!(
            authorization_header("AKIDEXAMPLE", &scope, &headers, &sig),
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, \
             SignedHeaders=host;x-amz-date, \
             Signature=5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"
        );
    }

    #[test]
    fn signing_key_is_deterministic_and_scope_separated() {
        let a = signing_key("secret", "20260902", "us-east-1", "s3");
        let b = signing_key("secret", "20260902", "us-east-1", "s3");
        let c = signing_key("secret", "20260902", "eu-west-1", "s3");
        assert_eq!(a, b);
        assert_ne!(a, c);
    }
}
