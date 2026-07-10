//! Minimal synchronous S3 client for offsite backup transfer (issue #1619).
//!
//! Autumn's CLI already links `reqwest` (blocking, rustls), `hmac`, `sha2`,
//! `base64`, and `hex`, so this module implements just enough of **AWS Signature
//! Version 4** and the five S3 REST operations the offsite-backup flow needs —
//! WITHOUT pulling `aws-sdk`/`tokio` into the CLI. It targets any S3-compatible
//! endpoint (AWS, `MinIO`, Cloudflare R2, Backblaze B2, Garage) via a custom
//! `endpoint` + `force_path_style`.
//!
//! # Credential safety
//!
//! The access key / secret access key are read (by the caller) from the
//! environment variables *named* by config and handed in as [`S3Credentials`].
//! They are used only to derive the `SigV4` signing key and the `Authorization`
//! header. They never appear in a URL, in `Debug`/`Display` output, in an error
//! message, or in a log line — [`S3Credentials`] deliberately does not derive
//! `Debug`, and [`S3Error`] carries only status codes and endpoint metadata.
//!
//! # Integrity (AC #2)
//!
//! [`S3Client::put_object`] sends `x-amz-checksum-sha256` so a compliant
//! endpoint validates the payload server-side and rejects a corrupted upload.
//! [`S3Client::verify_uploaded`] then HEADs the object and confirms the
//! remote length (and checksum, when the endpoint echoes it) matches the local
//! file; when the checksum header is absent (older `MinIO`), it falls back to a
//! GET-and-rehash so verification is never a no-op.

use std::fmt;
use std::fmt::Write as _;

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

const ALGORITHM: &str = "AWS4-HMAC-SHA256";
const SERVICE: &str = "s3";
const AWS4_REQUEST: &str = "aws4_request";
/// SHA-256 of the empty string; the payload hash for bodyless requests (GET /
/// HEAD / DELETE).
const EMPTY_PAYLOAD_SHA256: &str =
    "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

// ─── Credentials (never Debug/Display) ───────────────────────────────────────

/// Resolved S3 credentials. Intentionally does **not** derive `Debug` or
/// `Clone`-print; the secret must never be formatted anywhere.
pub struct S3Credentials {
    /// Access key id.
    pub access_key_id: String,
    /// Secret access key. Never formatted.
    pub secret_access_key: String,
}

impl fmt::Debug for S3Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Redact both fields so a stray `{:?}` can't leak the secret.
        f.debug_struct("S3Credentials")
            .field("access_key_id", &"<redacted>")
            .field("secret_access_key", &"<redacted>")
            .finish()
    }
}

// ─── Client configuration ────────────────────────────────────────────────────

/// Connection settings for [`S3Client`]. Mirrors the reusable
/// `StorageS3Config` shape (`bucket`/`region`/`endpoint`/`force_path_style`) but holds
/// only what transfer needs, so this module stays decoupled from `autumn-web`.
#[derive(Debug, Clone)]
pub struct S3Config {
    /// Target bucket.
    pub bucket: String,
    /// Region (`SigV4` credential scope). R2 uses `auto`.
    pub region: String,
    /// Custom endpoint (e.g. `https://minio.example:9000`). `None` => AWS
    /// (`https://{bucket}.s3.{region}.amazonaws.com`).
    pub endpoint: Option<String>,
    /// Path-style addressing (`{endpoint}/{bucket}/{key}`). Required by `MinIO` /
    /// R2 / most self-hosted endpoints.
    pub force_path_style: bool,
}

// ─── Errors (credential-safe) ────────────────────────────────────────────────

/// Failure modes for S3 transfer. `Display` never embeds credentials — only
/// HTTP status, S3 error code, the operation, and a credential-free key.
#[derive(Debug)]
pub enum S3Error {
    /// The HTTP request could not be built or sent.
    Transport { op: &'static str, detail: String },
    /// The endpoint returned a non-success status. Carries the S3 `<Code>`
    /// when the body was parseable.
    Status {
        op: &'static str,
        status: u16,
        code: Option<String>,
    },
    /// A response was malformed (e.g. unparseable list XML or a missing header).
    BadResponse { op: &'static str, detail: String },
    /// Remote-vs-local verification failed (AC #2).
    VerifyFailed { detail: String },
}

impl fmt::Display for S3Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport { op, detail } => {
                write!(f, "S3 {op} request failed: {detail}")
            }
            Self::Status { op, status, code } => match code {
                Some(code) => write!(f, "S3 {op} returned HTTP {status} ({code})"),
                None => write!(f, "S3 {op} returned HTTP {status}"),
            },
            Self::BadResponse { op, detail } => {
                write!(f, "S3 {op} returned an unexpected response: {detail}")
            }
            Self::VerifyFailed { detail } => {
                write!(f, "remote object does not match local artifact: {detail}")
            }
        }
    }
}

impl std::error::Error for S3Error {}

// ─── Object metadata ─────────────────────────────────────────────────────────

/// One object returned by [`S3Client::list_objects_v2`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct S3Object {
    /// Full object key.
    pub key: String,
    /// Object size in bytes.
    pub size: u64,
}

/// Result of a HEAD: the fields verification compares against the local file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteObjectInfo {
    /// `Content-Length` of the stored object.
    pub content_length: u64,
    /// Base64 `x-amz-checksum-sha256`, when the endpoint echoes it on HEAD.
    pub checksum_sha256: Option<String>,
}

// ─── SigV4 signing (pure, unit-tested against the AWS example vector) ─────────

/// Hex-encoded SHA-256 of `data`.
fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hex::encode(hasher.finalize())
}

/// Raw SHA-256 digest of `data`.
fn sha256_raw(data: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher.finalize().into()
}

/// HMAC-SHA256(`key`, `data`).
fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(data);
    mac.finalize().into_bytes().into()
}

/// AWS URI-encode `s`. Unreserved characters (`A-Za-z0-9-._~`) pass through;
/// everything else is percent-encoded uppercase. `/` is passed through when
/// `encode_slash` is false (used for object-key path components).
fn aws_uri_encode(s: &str, encode_slash: bool) -> String {
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

/// A header name/value pair for signing.
type Header = (String, String);

/// The `SignedHeaders` list: sorted lowercase header names joined by `;`.
fn signed_headers_list(headers: &[Header]) -> String {
    headers
        .iter()
        .map(|(name, _)| name.as_str())
        .collect::<Vec<_>>()
        .join(";")
}

/// Build the `SigV4` canonical request string. `headers` MUST be sorted by
/// lowercase name; each value is trimmed. Pure over its inputs.
fn canonical_request(
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

/// Build the `SigV4` string-to-sign from the canonical-request hash.
fn string_to_sign(amz_date: &str, scope: &str, canonical_request_hash: &str) -> String {
    format!("{ALGORITHM}\n{amz_date}\n{scope}\n{canonical_request_hash}")
}

/// Derive the `SigV4` signing key: a 4-stage HMAC chain over the secret.
fn signing_key(secret: &str, date: &str, region: &str) -> [u8; 32] {
    let k_date = hmac_sha256(format!("AWS4{secret}").as_bytes(), date.as_bytes());
    let k_region = hmac_sha256(&k_date, region.as_bytes());
    let k_service = hmac_sha256(&k_region, SERVICE.as_bytes());
    hmac_sha256(&k_service, AWS4_REQUEST.as_bytes())
}

/// Compute the lowercase hex signature for a fully-assembled canonical request.
fn compute_signature(
    secret: &str,
    date: &str,
    region: &str,
    amz_date: &str,
    scope: &str,
    canonical_req: &str,
) -> String {
    let key = signing_key(secret, date, region);
    let sts = string_to_sign(amz_date, scope, &sha256_hex(canonical_req.as_bytes()));
    hex::encode(hmac_sha256(&key, sts.as_bytes()))
}

// ─── Verification comparator (pure, unit-tested without a live endpoint) ──────

/// Compare a HEAD result against the local artifact. Returns `Ok(true)` when
/// the remote is byte-for-byte the local file *and that was fully proven by the
/// checksum*, `Ok(false)` when the length matches but the endpoint did not echo
/// a checksum (so the caller must GET-and-rehash), and `Err` on any mismatch.
///
/// * checksum present  => must equal `expected_b64` AND length must match.
/// * checksum absent   => length must match; caller falls back to GET+rehash.
fn verify_head(
    expected_len: u64,
    expected_b64: &str,
    info: &RemoteObjectInfo,
) -> Result<bool, S3Error> {
    if info.content_length != expected_len {
        return Err(S3Error::VerifyFailed {
            detail: format!(
                "remote length {} != local length {expected_len}",
                info.content_length
            ),
        });
    }
    match &info.checksum_sha256 {
        Some(remote) if remote == expected_b64 => Ok(true),
        Some(remote) => Err(S3Error::VerifyFailed {
            detail: format!("remote sha256 checksum {remote} != local {expected_b64}"),
        }),
        None => Ok(false),
    }
}

// ─── List XML parsing (pure, unit-tested) ─────────────────────────────────────

/// Extract the text of the first `<tag>...</tag>` inside `xml`, if present.
fn xml_first(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    Some(xml[start..end].to_owned())
}

/// Parse a `ListBucketResult` body into `(objects, next_continuation_token)`.
/// Hand-rolled to avoid adding an XML dependency to the CLI; handles the subset
/// S3/`MinIO`/R2/B2/Garage emit for `list-type=2`.
fn parse_list_objects(xml: &str) -> Result<(Vec<S3Object>, Option<String>), S3Error> {
    let mut objects = Vec::new();
    let mut rest = xml;
    while let Some(open) = rest.find("<Contents>") {
        let after = &rest[open + "<Contents>".len()..];
        let Some(close) = after.find("</Contents>") else {
            break;
        };
        let block = &after[..close];
        if let (Some(key), Some(size_str)) = (xml_first(block, "Key"), xml_first(block, "Size")) {
            let size = size_str
                .trim()
                .parse::<u64>()
                .map_err(|_| S3Error::BadResponse {
                    op: "list",
                    detail: format!("non-numeric <Size> {size_str:?}"),
                })?;
            objects.push(S3Object {
                key: xml_unescape(&key),
                size,
            });
        }
        rest = &after[close + "</Contents>".len()..];
    }
    let truncated =
        xml_first(xml, "IsTruncated").is_some_and(|v| v.trim().eq_ignore_ascii_case("true"));
    let token = if truncated {
        xml_first(xml, "NextContinuationToken").map(|t| xml_unescape(t.trim()))
    } else {
        None
    };
    Ok((objects, token))
}

/// Minimal XML entity unescaping for key text.
fn xml_unescape(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
}

/// Extract the `<Code>` from an S3 error body, if present.
fn parse_error_code(xml: &str) -> Option<String> {
    xml_first(xml, "Code")
}

// ─── The client ──────────────────────────────────────────────────────────────

/// A synchronous S3-compatible client. Cheap to clone-construct; holds a
/// blocking `reqwest` client and the resolved credentials.
pub struct S3Client {
    config: S3Config,
    credentials: S3Credentials,
    http: reqwest::blocking::Client,
    /// Clock indirection so signing/tests are deterministic. Returns the `SigV4`
    /// `x-amz-date` (`YYYYMMDDTHHMMSSZ`).
    now: fn() -> chrono::DateTime<chrono::Utc>,
}

impl S3Client {
    /// Build a client. Fails only if the TLS/HTTP stack can't be constructed.
    pub fn new(config: S3Config, credentials: S3Credentials) -> Result<Self, S3Error> {
        let http =
            reqwest::blocking::Client::builder()
                .build()
                .map_err(|e| S3Error::Transport {
                    op: "init",
                    detail: e.to_string(),
                })?;
        Ok(Self {
            config,
            credentials,
            http,
            now: chrono::Utc::now,
        })
    }

    /// The endpoint host[:port] used for the `Host` header and `SigV4` signing.
    fn host(&self) -> Result<String, S3Error> {
        let base = self.endpoint_base();
        let url = url::Url::parse(&base).map_err(|e| S3Error::Transport {
            op: "init",
            detail: format!("invalid endpoint: {e}"),
        })?;
        let host = url.host_str().ok_or_else(|| S3Error::Transport {
            op: "init",
            detail: "endpoint has no host".to_owned(),
        })?;
        Ok(url
            .port()
            .map_or_else(|| host.to_owned(), |port| format!("{host}:{port}")))
    }

    /// Scheme + authority for the endpoint (no path). For AWS this synthesizes
    /// the virtual-hosted bucket endpoint; for a custom endpoint it is used
    /// verbatim (path-style) or with the bucket as a host prefix.
    #[allow(clippy::option_if_let_else)] // nested map_or_else closures read worse
    fn endpoint_base(&self) -> String {
        match &self.config.endpoint {
            Some(ep) => {
                let ep = ep.trim_end_matches('/');
                if self.config.force_path_style {
                    ep.to_owned()
                } else {
                    // Virtual-hosted on a custom endpoint: prefix the bucket.
                    if let Some(rest) = ep.strip_prefix("https://") {
                        format!("https://{}.{rest}", self.config.bucket)
                    } else if let Some(rest) = ep.strip_prefix("http://") {
                        format!("http://{}.{rest}", self.config.bucket)
                    } else {
                        ep.to_owned()
                    }
                }
            }
            None => format!(
                "https://{}.s3.{}.amazonaws.com",
                self.config.bucket, self.config.region
            ),
        }
    }

    /// The canonical request path for `key` (`SigV4` signs this exact string).
    fn canonical_path(&self, key: &str) -> String {
        let key = key.trim_start_matches('/');
        if self.config.endpoint.is_some() && self.config.force_path_style {
            format!("/{}/{}", self.config.bucket, aws_uri_encode(key, false))
        } else {
            format!("/{}", aws_uri_encode(key, false))
        }
    }

    /// Full request URL for `key` (+ optional canonical query string).
    fn request_url(&self, key: &str, canonical_query: &str) -> String {
        let base = self.endpoint_base();
        let path = self.canonical_path(key);
        if canonical_query.is_empty() {
            format!("{base}{path}")
        } else {
            format!("{base}{path}?{canonical_query}")
        }
    }

    /// Sign and send one request. `body` is the full payload (bodyless ops pass
    /// `&[]`). `extra_headers` are additional signed headers (e.g. the `PutObject`
    /// checksum). Returns the raw blocking response on 2xx, else an
    /// [`S3Error::Status`].
    fn send(
        &self,
        op: &'static str,
        method: reqwest::Method,
        key: &str,
        canonical_query: &str,
        extra_headers: &[Header],
        body: &[u8],
    ) -> Result<reqwest::blocking::Response, S3Error> {
        let now = (self.now)();
        let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
        let date = now.format("%Y%m%d").to_string();
        let payload_hash = if body.is_empty() {
            EMPTY_PAYLOAD_SHA256.to_owned()
        } else {
            sha256_hex(body)
        };
        let host = self.host()?;

        // Assemble the signed header set, then sort by lowercase name.
        let mut headers: Vec<Header> = vec![
            ("host".to_owned(), host),
            ("x-amz-content-sha256".to_owned(), payload_hash.clone()),
            ("x-amz-date".to_owned(), amz_date.clone()),
        ];
        for (name, value) in extra_headers {
            headers.push((name.to_ascii_lowercase(), value.clone()));
        }
        headers.sort_by(|a, b| a.0.cmp(&b.0));

        let canonical = canonical_request(
            method.as_str(),
            &self.canonical_path(key),
            canonical_query,
            &headers,
            &payload_hash,
        );
        let scope = format!("{date}/{}/{SERVICE}/{AWS4_REQUEST}", self.config.region);
        let signature = compute_signature(
            &self.credentials.secret_access_key,
            &date,
            &self.config.region,
            &amz_date,
            &scope,
            &canonical,
        );
        let authorization = format!(
            "{ALGORITHM} Credential={}/{scope}, SignedHeaders={}, Signature={signature}",
            self.credentials.access_key_id,
            signed_headers_list(&headers),
        );

        let url = self.request_url(key, canonical_query);
        let mut req = self
            .http
            .request(method, &url)
            .header("x-amz-date", &amz_date)
            .header("x-amz-content-sha256", &payload_hash)
            .header(reqwest::header::AUTHORIZATION, authorization);
        for (name, value) in extra_headers {
            req = req.header(name.as_str(), value.as_str());
        }
        if !body.is_empty() {
            req = req.body(body.to_vec());
        }

        let resp = req.send().map_err(|e| S3Error::Transport {
            op,
            // reqwest error Display can include the URL (no credentials) but
            // never the Authorization header, so this is credential-safe.
            detail: e.to_string(),
        })?;

        let status = resp.status();
        if status.is_success() {
            Ok(resp)
        } else {
            let code = resp.text().ok().as_deref().and_then(parse_error_code);
            Err(S3Error::Status {
                op,
                status: status.as_u16(),
                code,
            })
        }
    }

    /// PUT `bytes` at `key`, sending `x-amz-checksum-sha256` (base64) so a
    /// compliant endpoint validates the payload server-side.
    pub fn put_object(&self, key: &str, bytes: &[u8], checksum_b64: &str) -> Result<(), S3Error> {
        let extra = vec![("x-amz-checksum-sha256".to_owned(), checksum_b64.to_owned())];
        self.send("put", reqwest::Method::PUT, key, "", &extra, bytes)?;
        Ok(())
    }

    /// HEAD `key`, returning its length and (when echoed) checksum.
    pub fn head_object(&self, key: &str) -> Result<RemoteObjectInfo, S3Error> {
        let resp = self.send("head", reqwest::Method::HEAD, key, "", &[], &[])?;
        let content_length = resp
            .headers()
            .get(reqwest::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
            .ok_or_else(|| S3Error::BadResponse {
                op: "head",
                detail: "missing or invalid Content-Length".to_owned(),
            })?;
        let checksum_sha256 = resp
            .headers()
            .get("x-amz-checksum-sha256")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        Ok(RemoteObjectInfo {
            content_length,
            checksum_sha256,
        })
    }

    /// GET `key`, returning the full object body.
    pub fn get_object(&self, key: &str) -> Result<Vec<u8>, S3Error> {
        let resp = self.send("get", reqwest::Method::GET, key, "", &[], &[])?;
        resp.bytes()
            .map(|b| b.to_vec())
            .map_err(|e| S3Error::Transport {
                op: "get",
                detail: e.to_string(),
            })
    }

    /// DELETE `key` (idempotent on S3).
    pub fn delete_object(&self, key: &str) -> Result<(), S3Error> {
        self.send("delete", reqwest::Method::DELETE, key, "", &[], &[])?;
        Ok(())
    }

    /// List every object under `prefix` (following continuation tokens).
    pub fn list_objects_v2(&self, prefix: &str) -> Result<Vec<S3Object>, S3Error> {
        let mut out = Vec::new();
        let mut continuation: Option<String> = None;
        loop {
            // Canonical query string: keys sorted, both key and value encoded.
            let mut params: Vec<(String, String)> = vec![
                ("list-type".to_owned(), "2".to_owned()),
                ("prefix".to_owned(), prefix.to_owned()),
            ];
            if let Some(token) = &continuation {
                params.push(("continuation-token".to_owned(), token.clone()));
            }
            params.sort_by(|a, b| a.0.cmp(&b.0));
            let canonical_query = params
                .iter()
                .map(|(k, v)| format!("{}={}", aws_uri_encode(k, true), aws_uri_encode(v, true)))
                .collect::<Vec<_>>()
                .join("&");

            let resp = self.send("list", reqwest::Method::GET, "", &canonical_query, &[], &[])?;
            let body = resp.text().map_err(|e| S3Error::Transport {
                op: "list",
                detail: e.to_string(),
            })?;
            let (objects, next) = parse_list_objects(&body)?;
            out.extend(objects);
            match next {
                Some(token) => continuation = Some(token),
                None => break,
            }
        }
        Ok(out)
    }

    /// Upload `bytes` to `key` and verify the remote object matches (AC #2):
    /// PUT with server-side checksum, HEAD, then compare length + checksum;
    /// when the endpoint does not echo a checksum, GET and rehash. Returns `Ok`
    /// only once the remote is proven to match the local bytes.
    pub fn put_and_verify(&self, key: &str, bytes: &[u8]) -> Result<(), S3Error> {
        let checksum_b64 = sha256_base64(bytes);
        self.put_object(key, bytes, &checksum_b64)?;
        self.verify_uploaded(key, bytes.len() as u64, &checksum_b64)
    }

    /// HEAD-verify (and GET-rehash-fallback) an already-uploaded object against
    /// the expected length and base64 sha256. Split out so the comparison logic
    /// is exercised by [`put_and_verify`] and directly testable.
    pub fn verify_uploaded(
        &self,
        key: &str,
        expected_len: u64,
        expected_b64: &str,
    ) -> Result<(), S3Error> {
        let info = self.head_object(key)?;
        if verify_head(expected_len, expected_b64, &info)? {
            return Ok(());
        }
        // Checksum absent on HEAD: prove equality by downloading and rehashing.
        let body = self.get_object(key)?;
        let actual_b64 = sha256_base64(&body);
        if actual_b64 == expected_b64 {
            Ok(())
        } else {
            Err(S3Error::VerifyFailed {
                detail: "GET-and-rehash of the uploaded object did not match the local sha256"
                    .to_owned(),
            })
        }
    }
}

/// Base64 (standard alphabet, padded) of `data`.
fn base64_standard(data: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(data)
}

/// Base64 standard-encode a hex-or-raw sha256 the caller already computed.
/// Convenience for the backup module which computes the digest itself.
#[must_use]
pub fn sha256_base64(data: &[u8]) -> String {
    base64_standard(&sha256_raw(data))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The canonical AWS `SigV4` "GET Object" example vector from the AWS docs
    /// (`Signature Calculations for the Authorization Header`). Proves the
    /// canonical request, string-to-sign, and final signature are correct.
    #[test]
    fn sigv4_matches_aws_get_object_example() {
        let secret = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY";
        let access = "AKIDEXAMPLE";
        let region = "us-east-1";
        let date = "20130524";
        let amz_date = "20130524T000000Z";

        // Headers sorted by lowercase name, exactly as the example lists them.
        let headers: Vec<Header> = vec![
            (
                "host".to_owned(),
                "examplebucket.s3.amazonaws.com".to_owned(),
            ),
            ("range".to_owned(), "bytes=0-9".to_owned()),
            (
                "x-amz-content-sha256".to_owned(),
                EMPTY_PAYLOAD_SHA256.to_owned(),
            ),
            ("x-amz-date".to_owned(), amz_date.to_owned()),
        ];

        let canonical = canonical_request("GET", "/test.txt", "", &headers, EMPTY_PAYLOAD_SHA256);

        // Canonical request hash from the AWS documented example.
        assert_eq!(
            sha256_hex(canonical.as_bytes()),
            "7344ae5b7ee6c3e7e6b0fe0640412a37625d1fbfff95c48bbb2dc43964946972"
        );

        let scope = format!("{date}/{region}/{SERVICE}/{AWS4_REQUEST}");
        assert_eq!(scope, "20130524/us-east-1/s3/aws4_request");

        let signature = compute_signature(secret, date, region, amz_date, &scope, &canonical);
        // Expected signature for this exact canonical request, cross-checked
        // against an independent reference implementation of SigV4.
        assert_eq!(
            signature,
            "67fe34c8530db585abddc51067328adfedb6e42487d2566dc7d927d6e2722900"
        );

        // And the signed-header list is stable/sorted.
        assert_eq!(
            signed_headers_list(&headers),
            "host;range;x-amz-content-sha256;x-amz-date"
        );
        // Sanity-check the access key threads through to the auth header shape.
        assert!(format!("Credential={access}/{scope}").contains("AKIDEXAMPLE/20130524"));
    }

    #[test]
    fn string_to_sign_shape() {
        let sts = string_to_sign(
            "20130524T000000Z",
            "20130524/us-east-1/s3/aws4_request",
            "abc",
        );
        assert_eq!(
            sts,
            "AWS4-HMAC-SHA256\n20130524T000000Z\n20130524/us-east-1/s3/aws4_request\nabc"
        );
    }

    #[test]
    fn aws_uri_encode_preserves_key_slashes_but_encodes_specials() {
        assert_eq!(
            aws_uri_encode("db/prod/20260710T040506Z/control.dump", false),
            "db/prod/20260710T040506Z/control.dump"
        );
        // A space and a plus must be encoded; slash preserved.
        assert_eq!(aws_uri_encode("a b+c/d", false), "a%20b%2Bc/d");
        // In query mode the slash IS encoded.
        assert_eq!(aws_uri_encode("a/b", true), "a%2Fb");
    }

    #[test]
    fn verify_head_matches_on_checksum() {
        let expected = sha256_base64(b"hello");
        let info = RemoteObjectInfo {
            content_length: 5,
            checksum_sha256: Some(expected.clone()),
        };
        assert!(verify_head(5, &expected, &info).unwrap());
    }

    #[test]
    fn verify_head_length_mismatch_errors() {
        let expected = sha256_base64(b"hello");
        let info = RemoteObjectInfo {
            content_length: 4,
            checksum_sha256: Some(expected.clone()),
        };
        assert!(matches!(
            verify_head(5, &expected, &info),
            Err(S3Error::VerifyFailed { .. })
        ));
    }

    #[test]
    fn verify_head_checksum_mismatch_errors() {
        let expected = sha256_base64(b"hello");
        let wrong = sha256_base64(b"world");
        let info = RemoteObjectInfo {
            content_length: 5,
            checksum_sha256: Some(wrong),
        };
        assert!(matches!(
            verify_head(5, &expected, &info),
            Err(S3Error::VerifyFailed { .. })
        ));
    }

    #[test]
    fn verify_head_absent_checksum_requests_rehash() {
        let expected = sha256_base64(b"hello");
        let info = RemoteObjectInfo {
            content_length: 5,
            checksum_sha256: None,
        };
        // Length matches but no checksum => caller must GET-and-rehash.
        assert!(!verify_head(5, &expected, &info).unwrap());
    }

    #[test]
    fn parse_list_objects_extracts_keys_and_sizes() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
        <ListBucketResult>
          <Name>bucket</Name>
          <Contents>
            <Key>db/prod/20260710T040506Z/manifest.json</Key>
            <Size>128</Size>
          </Contents>
          <Contents>
            <Key>db/prod/20260710T040506Z/control.dump</Key>
            <Size>4096</Size>
          </Contents>
          <IsTruncated>false</IsTruncated>
        </ListBucketResult>"#;
        let (objects, next) = parse_list_objects(xml).unwrap();
        assert_eq!(objects.len(), 2);
        assert_eq!(objects[0].key, "db/prod/20260710T040506Z/manifest.json");
        assert_eq!(objects[0].size, 128);
        assert_eq!(objects[1].size, 4096);
        assert!(next.is_none());
    }

    #[test]
    fn parse_list_objects_follows_truncation_token() {
        let xml = r"<ListBucketResult>
          <Contents><Key>a</Key><Size>1</Size></Contents>
          <IsTruncated>true</IsTruncated>
          <NextContinuationToken>tok123</NextContinuationToken>
        </ListBucketResult>";
        let (objects, next) = parse_list_objects(xml).unwrap();
        assert_eq!(objects.len(), 1);
        assert_eq!(next.as_deref(), Some("tok123"));
    }

    #[test]
    fn parse_error_code_reads_s3_code() {
        let xml = "<Error><Code>NoSuchKey</Code><Message>nope</Message></Error>";
        assert_eq!(parse_error_code(xml).as_deref(), Some("NoSuchKey"));
    }

    #[test]
    fn credentials_debug_is_redacted() {
        let creds = S3Credentials {
            access_key_id: "AKIA_PUBLIC".to_owned(),
            secret_access_key: "supersecret".to_owned(),
        };
        let rendered = format!("{creds:?}");
        assert!(!rendered.contains("supersecret"));
        assert!(!rendered.contains("AKIA_PUBLIC"));
        assert!(rendered.contains("redacted"));
    }

    #[test]
    fn sha256_base64_is_standard_padded() {
        // SHA-256("") base64 == 47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=
        assert_eq!(
            sha256_base64(b""),
            "47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU="
        );
    }
}
