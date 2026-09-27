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
//! environment variables *named* by config and handed in as `S3Credentials`.
//! They are used only to derive the `SigV4` signing key and the `Authorization`
//! header. They never appear in a URL, in `Debug`/`Display` output, in an error
//! message, or in a log line — `S3Credentials` deliberately does not derive
//! `Debug`, and `S3Error` carries only status codes and endpoint metadata.
//!
//! # Integrity (AC #2)
//!
//! `S3Client::put_file_and_verify` streams the artifact as the PUT body and
//! sends `x-amz-checksum-sha256` so a compliant endpoint validates the payload
//! server-side and rejects a corrupted upload. It then HEADs the object and
//! confirms the remote length (and checksum, when the endpoint echoes it)
//! matches the local file; when the checksum header is absent (older `MinIO`),
//! it falls back to a **streaming** GET-and-rehash so verification is never a
//! no-op and never buffers a multi-GB object in memory.

use std::fmt;
use std::fmt::Write as _;
use std::path::Path;
use std::time::Duration;

use sha2::{Digest, Sha256};

use autumn_web::sigv4;

/// `SigV4` signing lives in `autumn_web::sigv4` so this client and the in-process
/// `SQLite` replicator (#1628) sign identically, from one implementation. The
/// aliases below keep this module's call sites and its published test names
/// unchanged while the arithmetic itself has a single home.
const ALGORITHM: &str = sigv4::ALGORITHM;
const SERVICE: &str = "s3";
const AWS4_REQUEST: &str = sigv4::AWS4_REQUEST;
/// SHA-256 of the empty string; the payload hash for bodyless requests (GET /
/// HEAD / DELETE).
const EMPTY_PAYLOAD_SHA256: &str = sigv4::EMPTY_PAYLOAD_SHA256;
/// Upper bound on `list_objects_v2` pagination. At 1000 objects/page (the S3
/// maximum) this covers a million objects — far beyond any backup prefix — while
/// bounding a broken endpoint that never stops returning a continuation token.
const MAX_LIST_PAGES: usize = 1000;
/// Artifacts larger than this upload via multipart. A single `PutObject` is
/// capped at 5 GiB by S3, so a large production dump must be sent in parts.
const MULTIPART_THRESHOLD: u64 = 64 * 1024 * 1024; // 64 MiB
/// Default target part size for multipart uploads. With 64 MiB parts and S3's
/// 10 000-part limit the largest object is ≈ 640 GiB; larger files transparently
/// grow the part size (see [`effective_part_size`]).
const MULTIPART_PART_SIZE: u64 = 64 * 1024 * 1024; // 64 MiB
/// S3's minimum part size (every part except the last).
const S3_MIN_PART_SIZE: u64 = 5 * 1024 * 1024; // 5 MiB
/// S3's maximum number of parts in one multipart upload.
const S3_MAX_PARTS: u64 = 10_000;
/// Connect-phase timeout for the transfer client: fail fast on a dead host
/// without bounding the total transfer duration.
const TRANSFER_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// TCP-level inactivity guard (`TCP_USER_TIMEOUT`) for the transfer client:
/// drop a stalled connection whose data stays unacknowledged this long, WITHOUT
/// capping a healthy long-running multi-GB stream.
///
/// Linux-only: `TCP_USER_TIMEOUT` (and reqwest's `tcp_user_timeout` builder
/// method) exist only on Linux. On macOS/Windows the transfer client relies on
/// `connect_timeout` + the disabled overall timeout instead.
#[cfg(target_os = "linux")]
const TRANSFER_STALL_TIMEOUT: Duration = Duration::from_secs(300);

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
    sigv4::sha256_hex(data)
}

/// Raw SHA-256 digest of `data`. (Test helper; the streaming upload/verify paths
/// hash incrementally via [`sha256_file`] / [`Sha256Writer`].)
#[cfg(test)]
fn sha256_raw(data: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher.finalize().into()
}

/// AWS URI-encode `s`. Unreserved characters (`A-Za-z0-9-._~`) pass through;
/// everything else is percent-encoded uppercase. `/` is passed through when
/// `encode_slash` is false (used for object-key path components).
fn aws_uri_encode(s: &str, encode_slash: bool) -> String {
    sigv4::uri_encode(s, encode_slash)
}

/// A header name/value pair for signing.
type Header = (String, String);

/// The extra signed header a HEAD carries so the endpoint echoes the object's
/// checksum on the response (AWS requires opting in with `checksum-mode`).
fn head_extra_headers() -> Vec<Header> {
    vec![("x-amz-checksum-mode".to_owned(), "ENABLED".to_owned())]
}

/// The `SignedHeaders` list: sorted lowercase header names joined by `;`.
fn signed_headers_list(headers: &[Header]) -> String {
    sigv4::signed_headers_list(headers)
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
    sigv4::canonical_request(
        method,
        canonical_uri,
        canonical_query,
        headers,
        payload_hash,
    )
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
    sigv4::signature(
        secret,
        date,
        region,
        SERVICE,
        amz_date,
        scope,
        canonical_req,
    )
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
        // A COMPOSITE checksum (`<hash>-<n>`, echoed by some endpoints for a
        // multipart object) is a hash-of-part-hashes, NOT the whole-object
        // sha256, so it must not be compared to `expected_b64` — that would be a
        // spurious mismatch. Ignore it and fall through to the GET-and-rehash path.
        Some(remote) if is_composite_checksum(remote) => Ok(false),
        Some(remote) if remote == expected_b64 => Ok(true),
        Some(remote) => Err(S3Error::VerifyFailed {
            detail: format!("remote sha256 checksum {remote} != local {expected_b64}"),
        }),
        None => Ok(false),
    }
}

/// Whether an `x-amz-checksum-sha256` value is a COMPOSITE checksum — a
/// base64 digest followed by a `-<partcount>` suffix (e.g. `AbCd...=-3`), which
/// endpoints echo for multipart objects. Such a value is a hash of part hashes,
/// not the object's sha256, so it can never equal the whole-file digest.
fn is_composite_checksum(value: &str) -> bool {
    value.rsplit_once('-').is_some_and(|(head, n)| {
        !head.is_empty() && !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())
    })
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

// ─── Multipart helpers (pure, unit-tested) ────────────────────────────────────

/// Build a `SigV4` canonical query string from `(key, value)` pairs: each key and
/// value AWS-URI-encoded (slash encoded), pairs sorted by encoded key. A
/// value-less flag (e.g. `uploads`) encodes as `key=`.
fn canonical_query(params: &[(&str, &str)]) -> String {
    sigv4::canonical_query(params)
}

/// Number of parts a `len`-byte object splits into at `part_size` bytes/part.
const fn multipart_part_count(len: u64, part_size: u64) -> u64 {
    if part_size == 0 {
        return 0;
    }
    len.div_ceil(part_size)
}

/// Resolve the part size actually used for a `len`-byte file from the `configured`
/// target. Floors at S3's 5 MiB minimum, and grows the part size when `len` at the
/// configured size would exceed S3's 10 000-part limit so the plan always fits.
fn effective_part_size(len: u64, configured: u64) -> u64 {
    // Smallest part size that keeps the part count within S3_MAX_PARTS.
    let min_for_parts = len.div_ceil(S3_MAX_PARTS).max(1);
    configured.max(min_for_parts).max(S3_MIN_PART_SIZE)
}

/// Build the `CompleteMultipartUpload` request body from `(part_number, etag)`
/// pairs (already in ascending part order).
fn build_complete_multipart_xml(parts: &[(u32, String)]) -> String {
    let mut xml = String::from("<CompleteMultipartUpload>");
    for (number, etag) in parts {
        let _ = write!(
            xml,
            "<Part><PartNumber>{number}</PartNumber><ETag>{}</ETag></Part>",
            xml_escape(etag)
        );
    }
    xml.push_str("</CompleteMultipartUpload>");
    xml
}

/// Classify a `CompleteMultipartUpload` response body. S3 returns HTTP 200 even
/// when assembly fails, embedding an `<Error>` in the body — so success requires
/// a `CompleteMultipartUploadResult` element AND the absence of an `<Error>`
/// element (an error body could conceivably embed the result tag as a
/// substring). Returns `Err(code)` (the S3 `<Code>`, or a placeholder) on a
/// 200-with-error body.
fn classify_complete_response(body: &str) -> Result<(), String> {
    if body.contains("<CompleteMultipartUploadResult") && !body.contains("<Error") {
        return Ok(());
    }
    Err(parse_error_code(body).unwrap_or_else(|| "UnrecognizedCompleteResponse".to_owned()))
}

/// Minimal XML entity escaping for text placed inside an element (`ETag` values).
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Read up to `buf.len()` bytes, looping over short reads so a full part is
/// assembled even when the underlying reader returns data in small pieces.
/// Returns the number of bytes read (0 only at EOF).
fn read_up_to(reader: &mut impl std::io::Read, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        let n = reader.read(&mut buf[filled..])?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    Ok(filled)
}

// ─── The client ──────────────────────────────────────────────────────────────

/// A synchronous S3-compatible client. Cheap to clone-construct; holds a
/// blocking `reqwest` client and the resolved credentials.
pub struct S3Client {
    config: S3Config,
    credentials: S3Credentials,
    http: reqwest::blocking::Client,
    /// Files larger than this upload via multipart (default
    /// [`MULTIPART_THRESHOLD`]); overridable for tests via
    /// [`with_part_size`](Self::with_part_size).
    multipart_threshold: u64,
    /// Target part size for multipart uploads (default [`MULTIPART_PART_SIZE`]);
    /// the effective size is floored/grown per file by [`effective_part_size`].
    multipart_part_size: u64,
    /// Clock indirection so signing/tests are deterministic. Returns the `SigV4`
    /// `x-amz-date` (`YYYYMMDDTHHMMSSZ`).
    now: fn() -> chrono::DateTime<chrono::Utc>,
}

/// The body of a request. Bodyless ops use [`RequestBody::Empty`]; the artifact
/// upload uses [`RequestBody::File`], which streams the file straight from disk
/// as the request body (bounded memory) with a payload hash the caller computed
/// in a single streaming pass — a multi-GB dump is never read into a `Vec`.
enum RequestBody<'a> {
    /// No body (GET/HEAD/DELETE).
    Empty,
    /// Stream this file as the body; `sha256_hex` is its precomputed payload hash.
    File { path: &'a Path, sha256_hex: &'a str },
    /// An owned in-memory body (a multipart part chunk, or a small XML request
    /// body like `CompleteMultipartUpload`). The buffer is MOVED into the request
    /// body — reqwest takes ownership without copying — so a part is held once,
    /// not duplicated. The payload hash is computed inline.
    Owned(Vec<u8>),
}

/// Reject a custom `endpoint` that carries a path prefix. `canonical_path` signs
/// `/{bucket}/{key}` (path-style) or `/{key}` (virtual-hosted) — it does NOT
/// know about an endpoint path segment (e.g. `https://gw.example/s3`), so the
/// signed path and the actually-sent path would diverge and every request would
/// 403. Fail loud at construction with clear guidance instead of at first use.
fn validate_endpoint(endpoint: &str) -> Result<(), S3Error> {
    let url = url::Url::parse(endpoint).map_err(|e| S3Error::Transport {
        op: "init",
        detail: format!("invalid endpoint {endpoint:?}: {e}"),
    })?;
    let path = url.path();
    if !path.is_empty() && path != "/" {
        return Err(S3Error::Transport {
            op: "init",
            detail: format!(
                "endpoint {endpoint:?} must be scheme + host[:port] only (no path); \
                 a path prefix like {path:?} is not supported"
            ),
        });
    }
    Ok(())
}

impl S3Client {
    /// Build a client. Fails if the TLS/HTTP stack can't be constructed or if a
    /// custom `endpoint` carries a path prefix (see [`validate_endpoint`]).
    pub fn new(config: S3Config, credentials: S3Credentials) -> Result<Self, S3Error> {
        if let Some(endpoint) = &config.endpoint {
            validate_endpoint(endpoint)?;
        }
        let builder = reqwest::blocking::Client::builder()
            // reqwest's blocking default is a 30s TOTAL request timeout, which
            // would abort a correctly-streaming multi-GB PUT/GET on a slow
            // offsite link before it finishes. Disable the overall cap so long
            // transfers aren't bounded by wall-clock…
            .timeout(None)
            // …but still fail fast on a dead/unreachable host.
            .connect_timeout(TRANSFER_CONNECT_TIMEOUT);
        // Catch a stalled connection at the TCP layer WITHOUT capping total
        // duration: TCP_USER_TIMEOUT drops the connection when sent data stays
        // unacknowledged this long. reqwest 0.13's blocking builder has no
        // per-read `read_timeout`, so this is the closest inactivity guard — but
        // the option (and reqwest's `tcp_user_timeout` method) are Linux-only, so
        // it's gated to Linux; other platforms rely on connect_timeout + the
        // disabled overall timeout. Non-configurable sane constants (a
        // configurable transfer timeout is a possible follow-up).
        #[cfg(target_os = "linux")]
        let builder = builder.tcp_user_timeout(TRANSFER_STALL_TIMEOUT);
        let http = builder.build().map_err(|e| S3Error::Transport {
            op: "init",
            detail: e.to_string(),
        })?;
        Ok(Self {
            config,
            credentials,
            http,
            multipart_threshold: MULTIPART_THRESHOLD,
            multipart_part_size: MULTIPART_PART_SIZE,
            now: chrono::Utc::now,
        })
    }

    /// Override ONLY the target part size, leaving the multipart threshold at its
    /// default. Shrinking the part size therefore never pushes a sub-threshold
    /// file into a multipart upload — it only changes how an already-multipart
    /// transfer is chunked. Floored to S3's 5 MiB minimum at upload time.
    #[must_use]
    pub const fn with_part_size(mut self, part_size: u64) -> Self {
        self.multipart_part_size = part_size;
        self
    }

    /// The endpoint `host[:port]` used for the `Host` header and `SigV4` signing.
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

    /// Scheme + authority for the endpoint (no path). The bucket lands in the
    /// HOST for virtual-hosted addressing and in the PATH (via
    /// [`canonical_path`](Self::canonical_path)) for path-style. `force_path_style`
    /// is honored for BOTH a custom endpoint and AWS-default: AWS path-style uses
    /// the regional host `s3.<region>.amazonaws.com` (required for dotted bucket
    /// names, which can't present a valid virtual-hosted TLS certificate).
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
            // AWS path-style: regional host, bucket goes in the path.
            None if self.config.force_path_style => {
                format!("https://s3.{}.amazonaws.com", self.config.region)
            }
            // AWS virtual-hosted (default): bucket is a host prefix.
            None => format!(
                "https://{}.s3.{}.amazonaws.com",
                self.config.bucket, self.config.region
            ),
        }
    }

    /// The canonical request path for `key` (`SigV4` signs this exact string).
    /// Path-style (`force_path_style`) puts the bucket in the path — for BOTH a
    /// custom endpoint and AWS-default — so it stays consistent with the
    /// bucketless host [`endpoint_base`](Self::endpoint_base) builds in that mode.
    fn canonical_path(&self, key: &str) -> String {
        let key = key.trim_start_matches('/');
        if self.config.force_path_style {
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

    /// Sign and send one request. `extra_headers` are additional signed headers
    /// (e.g. the `PutObject` checksum). A [`RequestBody::File`] streams from disk
    /// so a large upload never buffers in memory. Returns the raw blocking
    /// response on 2xx, else an [`S3Error::Status`].
    fn send(
        &self,
        op: &'static str,
        method: reqwest::Method,
        key: &str,
        canonical_query: &str,
        extra_headers: &[Header],
        body: RequestBody<'_>,
    ) -> Result<reqwest::blocking::Response, S3Error> {
        let now = (self.now)();
        let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
        let date = now.format("%Y%m%d").to_string();
        let payload_hash = match &body {
            RequestBody::Empty => EMPTY_PAYLOAD_SHA256.to_owned(),
            RequestBody::File { sha256_hex, .. } => (*sha256_hex).to_owned(),
            RequestBody::Owned(bytes) => sha256_hex(bytes),
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
        match body {
            RequestBody::Empty => {}
            RequestBody::File { path, .. } => {
                // Stream the file straight from disk. reqwest's blocking `Body`
                // derives Content-Length from the file's metadata, so the object
                // is sent with constant memory regardless of size.
                let file = std::fs::File::open(path).map_err(|e| S3Error::Transport {
                    op,
                    detail: format!("opening {} for upload: {e}", path.display()),
                })?;
                req = req.body(file);
            }
            RequestBody::Owned(bytes) => {
                // Move the buffer into the request body — no extra copy.
                req = req.body(bytes);
            }
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

    /// Stream the file at `path` to `key` as a PUT, sending `x-amz-checksum-sha256`
    /// (base64) so a compliant endpoint validates the payload server-side.
    /// `content_sha256_hex` is the file's hex payload hash (the caller computed
    /// it in a single streaming pass) and the body streams from disk — the file
    /// is never buffered in memory.
    fn put_file(
        &self,
        key: &str,
        path: &Path,
        checksum_b64: &str,
        content_sha256_hex: &str,
    ) -> Result<(), S3Error> {
        let extra = vec![("x-amz-checksum-sha256".to_owned(), checksum_b64.to_owned())];
        self.send(
            "put",
            reqwest::Method::PUT,
            key,
            "",
            &extra,
            RequestBody::File {
                path,
                sha256_hex: content_sha256_hex,
            },
        )?;
        Ok(())
    }

    /// HEAD `key`, returning its length and (when echoed) checksum.
    ///
    /// Sends a signed `x-amz-checksum-mode: ENABLED` so AWS (and compliant
    /// endpoints) echo `x-amz-checksum-sha256` on the HEAD response, letting
    /// [`verify_uploaded`](Self::verify_uploaded) take the cheap HEAD-only path
    /// instead of downloading and rehashing the whole (possibly multi-GB) object.
    pub fn head_object(&self, key: &str) -> Result<RemoteObjectInfo, S3Error> {
        let resp = self.send(
            "head",
            reqwest::Method::HEAD,
            key,
            "",
            &head_extra_headers(),
            RequestBody::Empty,
        )?;
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

    /// GET `key`, streaming the response body straight into `writer` with
    /// constant memory — used for restore downloads and the rehash-verify
    /// fallback so a multi-GB object is never buffered in a `Vec`.
    pub fn download_object(
        &self,
        key: &str,
        mut writer: impl std::io::Write,
    ) -> Result<(), S3Error> {
        let mut resp = self.send(
            "get",
            reqwest::Method::GET,
            key,
            "",
            &[],
            RequestBody::Empty,
        )?;
        std::io::copy(&mut resp, &mut writer).map_err(|e| S3Error::Transport {
            op: "get",
            detail: e.to_string(),
        })?;
        Ok(())
    }

    /// DELETE `key` (idempotent on S3).
    pub fn delete_object(&self, key: &str) -> Result<(), S3Error> {
        self.send(
            "delete",
            reqwest::Method::DELETE,
            key,
            "",
            &[],
            RequestBody::Empty,
        )?;
        Ok(())
    }

    /// List every object under `prefix` (following continuation tokens).
    ///
    /// Bounded by [`MAX_LIST_PAGES`]: a broken endpoint that returns a repeating
    /// continuation token can't loop forever — past the cap this errors rather
    /// than hanging.
    pub fn list_objects_v2(&self, prefix: &str) -> Result<Vec<S3Object>, S3Error> {
        let mut out = Vec::new();
        let mut continuation: Option<String> = None;
        for _page in 0..MAX_LIST_PAGES {
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

            let resp = self.send(
                "list",
                reqwest::Method::GET,
                "",
                &canonical_query,
                &[],
                RequestBody::Empty,
            )?;
            let body = resp.text().map_err(|e| S3Error::Transport {
                op: "list",
                detail: e.to_string(),
            })?;
            let (objects, next) = parse_list_objects(&body)?;
            out.extend(objects);
            match next {
                Some(token) => continuation = Some(token),
                None => return Ok(out),
            }
        }
        Err(S3Error::BadResponse {
            op: "list",
            detail: format!(
                "exceeded {MAX_LIST_PAGES} list pages (endpoint returned an unending \
                 continuation token?)"
            ),
        })
    }

    /// Verify the just-written object matches, deleting the remote object on a
    /// verification failure (#1760). `put_file*` WRITES the object BEFORE
    /// verifying it, so a mismatch (or a transient read failure during verify)
    /// can leave a corrupt/partial object behind. On any verify error we
    /// best-effort `DeleteObject` the key so `put_file_and_verify` is
    /// self-cleaning for every caller and leaves nothing behind even
    /// transiently. The delete is cleanup only: its own failure must NEVER mask
    /// the original verify error, which stays loud and non-zero.
    fn verify_or_delete(&self, key: &str, len: u64, checksum_b64: &str) -> Result<(), S3Error> {
        if let Err(e) = self.verify_uploaded(key, len, checksum_b64) {
            if let Err(del_err) = self.delete_object(key) {
                eprintln!(
                    "  \u{26A0} failed to delete {key} after post-upload verification \
                     failed: {del_err}"
                );
            }
            return Err(e);
        }
        Ok(())
    }

    /// Stream the file at `path` to `key` and verify the remote object matches
    /// (AC #2): a single streaming hash pre-pass computes the sha256 + length,
    /// the file is streamed as the PUT body with a server-side checksum, then
    /// HEAD confirms length + checksum (falling back to a streaming
    /// GET-and-rehash when the endpoint omits the checksum). Memory stays bounded
    /// regardless of file size. Returns `Ok` only once the remote is proven to
    /// match the local file.
    pub fn put_file_and_verify(&self, key: &str, path: &Path) -> Result<(), S3Error> {
        let len = std::fs::metadata(path)
            .map_err(|e| S3Error::Transport {
                op: "put",
                detail: format!("stat {} for upload: {e}", path.display()),
            })?
            .len();
        if len > self.multipart_threshold {
            return self.put_file_multipart(key, path, len);
        }
        let (digest, hashed_len) = sha256_file(path).map_err(|e| S3Error::Transport {
            op: "put",
            detail: format!("hashing {} for upload: {e}", path.display()),
        })?;
        let checksum_b64 = base64_standard(&digest);
        let content_hex = hex::encode(digest);
        self.put_file(key, path, &checksum_b64, &content_hex)?;
        self.verify_or_delete(key, hashed_len, &checksum_b64)
    }

    /// Upload `path` to `key` as an S3 multipart upload: `CreateMultipartUpload`,
    /// then one `UploadPart` per [`effective_part_size`] chunk (streamed a part at
    /// a time — peak memory ≈ one `part_size`, which for multi-TB files grows
    /// above the configured floor), then `CompleteMultipartUpload`.
    /// On ANY failure before completion the in-progress upload is aborted
    /// (`AbortMultipartUpload`) so no orphaned parts accrue storage cost.
    ///
    /// Integrity: a multipart object's `ETag` is a hash-of-part-hashes, NOT the
    /// object's sha256, so verification can't compare `ETag`s. Instead the whole
    /// file is hashed in the same streaming pass that uploads it, then the remote
    /// object is verified via HEAD `Content-Length` + (checksum or GET-and-rehash)
    /// exactly like the single-PUT path — never size alone.
    fn put_file_multipart(&self, key: &str, path: &Path, len: u64) -> Result<(), S3Error> {
        let part_size = effective_part_size(len, self.multipart_part_size);
        // Defensive: effective_part_size grows the part size to keep the plan
        // within S3's 10 000-part cap, so this should never trip — but fail loud
        // before starting an upload that can't complete rather than mid-stream.
        let part_count = multipart_part_count(len, part_size);
        if part_count > S3_MAX_PARTS {
            return Err(S3Error::BadResponse {
                op: "create-multipart",
                detail: format!(
                    "artifact needs {part_count} parts at {part_size} bytes/part, \
                     over S3's {S3_MAX_PARTS}-part limit"
                ),
            });
        }
        let upload_id = self.create_multipart_upload(key)?;
        match self.upload_parts_and_complete(key, path, len, part_size, &upload_id) {
            Ok(whole_b64) => {
                // Completion succeeded: the object now exists. Do NOT abort.
                // Verify the assembled object matches the streamed-hash of the
                // local file (HEAD length + checksum, else GET-and-rehash). On a
                // verify failure the completed object is deleted (#1760) so a
                // corrupt object is never left behind.
                self.verify_or_delete(key, len, &whole_b64)
            }
            Err(err) => {
                // Best-effort abort; surface the ORIGINAL upload error regardless
                // of whether the abort itself succeeds.
                let _ = self.abort_multipart_upload(key, &upload_id);
                Err(err)
            }
        }
    }

    /// Stream `path` in `part_size` chunks, uploading each as a numbered part and
    /// feeding every byte into a whole-file SHA-256. Returns the base64 sha256 of
    /// the entire file once `CompleteMultipartUpload` succeeds. Peak memory is
    /// ≈ one part (`part_size`, which grows above the configured floor for
    /// multi-TB files); each chunk is read into an owned buffer and MOVED into the
    /// request body, so a part is never duplicated.
    fn upload_parts_and_complete(
        &self,
        key: &str,
        path: &Path,
        len: u64,
        part_size: u64,
        upload_id: &str,
    ) -> Result<String, S3Error> {
        let mut file = std::fs::File::open(path).map_err(|e| S3Error::Transport {
            op: "upload-part",
            detail: format!("opening {} for upload: {e}", path.display()),
        })?;
        let cap = usize::try_from(part_size).unwrap_or(usize::MAX);
        let mut whole = Sha256Writer::new();
        let mut parts: Vec<(u32, String)> = Vec::new();
        let mut part_number: u32 = 1;
        let mut uploaded: u64 = 0;
        loop {
            // Read into a fresh owned buffer so it can be moved straight into the
            // request body without a second copy (≈ one part in memory at a time).
            let mut chunk = vec![0u8; cap];
            let n = read_up_to(&mut file, &mut chunk).map_err(|e| S3Error::Transport {
                op: "upload-part",
                detail: format!("reading {} for upload: {e}", path.display()),
            })?;
            if n == 0 {
                break;
            }
            chunk.truncate(n);
            {
                use std::io::Write as _;
                whole
                    .write_all(&chunk)
                    .expect("Sha256Writer::write is infallible");
            }
            let etag = self.upload_part(key, upload_id, part_number, chunk)?;
            parts.push((part_number, etag));
            uploaded += u64::try_from(n).unwrap_or(0);
            part_number = part_number
                .checked_add(1)
                .ok_or_else(|| S3Error::BadResponse {
                    op: "upload-part",
                    detail: "part count overflow".to_owned(),
                })?;
        }
        if uploaded != len {
            return Err(S3Error::Transport {
                op: "upload-part",
                detail: format!("read {uploaded} bytes but file length is {len}"),
            });
        }
        self.complete_multipart_upload(key, upload_id, &parts)?;
        Ok(base64_standard(&whole.finalize()))
    }

    /// `CreateMultipartUpload` (`POST /{key}?uploads=`). Returns the `UploadId`.
    fn create_multipart_upload(&self, key: &str) -> Result<String, S3Error> {
        let query = canonical_query(&[("uploads", "")]);
        let resp = self.send(
            "create-multipart",
            reqwest::Method::POST,
            key,
            &query,
            &[],
            RequestBody::Empty,
        )?;
        let body = resp.text().map_err(|e| S3Error::Transport {
            op: "create-multipart",
            detail: e.to_string(),
        })?;
        xml_first(&body, "UploadId").ok_or_else(|| S3Error::BadResponse {
            op: "create-multipart",
            detail: "CreateMultipartUpload response had no <UploadId>".to_owned(),
        })
    }

    /// `UploadPart` (`PUT /{key}?partNumber=n&uploadId=...`). Takes ownership of
    /// the chunk and MOVES it into the request body (no extra copy); returns the
    /// part's `ETag` (needed to complete).
    fn upload_part(
        &self,
        key: &str,
        upload_id: &str,
        part_number: u32,
        chunk: Vec<u8>,
    ) -> Result<String, S3Error> {
        let pn = part_number.to_string();
        let query = canonical_query(&[("partNumber", &pn), ("uploadId", upload_id)]);
        let resp = self.send(
            "upload-part",
            reqwest::Method::PUT,
            key,
            &query,
            &[],
            RequestBody::Owned(chunk),
        )?;
        resp.headers()
            .get(reqwest::header::ETAG)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
            .ok_or_else(|| S3Error::BadResponse {
                op: "upload-part",
                detail: format!("UploadPart {part_number} response had no ETag"),
            })
    }

    /// `CompleteMultipartUpload` (`POST /{key}?uploadId=...`). S3 can return HTTP
    /// 200 with an *error* body (the connection stays open while parts assemble),
    /// so success is confirmed by inspecting the body, not just the status.
    fn complete_multipart_upload(
        &self,
        key: &str,
        upload_id: &str,
        parts: &[(u32, String)],
    ) -> Result<(), S3Error> {
        let query = canonical_query(&[("uploadId", upload_id)]);
        let xml = build_complete_multipart_xml(parts);
        let resp = self.send(
            "complete-multipart",
            reqwest::Method::POST,
            key,
            &query,
            &[],
            RequestBody::Owned(xml.into_bytes()),
        )?;
        let body = resp.text().map_err(|e| S3Error::Transport {
            op: "complete-multipart",
            detail: e.to_string(),
        })?;
        classify_complete_response(&body).map_err(|code| S3Error::Status {
            op: "complete-multipart",
            status: 200,
            code: Some(code),
        })
    }

    /// `AbortMultipartUpload` (`DELETE /{key}?uploadId=...`). Best-effort cleanup
    /// of a failed upload's parts.
    fn abort_multipart_upload(&self, key: &str, upload_id: &str) -> Result<(), S3Error> {
        let query = canonical_query(&[("uploadId", upload_id)]);
        self.send(
            "abort-multipart",
            reqwest::Method::DELETE,
            key,
            &query,
            &[],
            RequestBody::Empty,
        )?;
        Ok(())
    }

    /// HEAD-verify (and streaming GET-rehash fallback) an already-uploaded object
    /// against the expected length and base64 sha256. Split out so the
    /// comparison logic is exercised by [`put_file_and_verify`](Self::put_file_and_verify)
    /// and directly testable.
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
        // Checksum absent on HEAD: prove equality by streaming the object through
        // a sha256 hasher (constant memory, never buffering the whole object).
        let mut hasher = Sha256Writer::new();
        self.download_object(key, &mut hasher)?;
        let actual_b64 = base64_standard(&hasher.finalize());
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

/// A `std::io::Write` sink that feeds everything written into a running SHA-256
/// digest, so an object can be hashed while streaming without buffering it.
struct Sha256Writer {
    hasher: Sha256,
}

impl Sha256Writer {
    fn new() -> Self {
        Self {
            hasher: Sha256::new(),
        }
    }

    fn finalize(self) -> [u8; 32] {
        self.hasher.finalize().into()
    }
}

impl std::io::Write for Sha256Writer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.hasher.update(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Stream a file through SHA-256, returning its `(digest, byte length)` without
/// loading it into memory. Reads in 64 KiB chunks.
fn sha256_file(path: &Path) -> std::io::Result<([u8; 32], u64)> {
    use std::io::Read as _;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut total: u64 = 0;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        total += u64::try_from(n).unwrap_or(u64::MAX);
    }
    Ok((hasher.finalize().into(), total))
}

/// Base64 (standard alphabet, padded) of `data`.
fn base64_standard(data: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(data)
}

/// Base64 of SHA-256(`data`). Test helper for the verification unit tests; the
/// streaming paths hash incrementally and base64 the digest directly.
#[cfg(test)]
fn sha256_base64(data: &[u8]) -> String {
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
        let sts = sigv4::string_to_sign(
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

    #[test]
    fn head_request_signs_checksum_mode_header() {
        // HEAD must ask the endpoint to echo the checksum so verification can
        // take the cheap HEAD-only path (no full GET+rehash).
        let extra = head_extra_headers();
        assert_eq!(
            extra,
            vec![("x-amz-checksum-mode".to_owned(), "ENABLED".to_owned())]
        );
        // The header must land in the SIGNED set: assemble the base HEAD headers
        // exactly as `send` does, sort, and confirm it's part of SignedHeaders in
        // its correct sorted position.
        let mut headers: Vec<Header> = vec![
            ("host".to_owned(), "s3.example.test".to_owned()),
            (
                "x-amz-content-sha256".to_owned(),
                EMPTY_PAYLOAD_SHA256.to_owned(),
            ),
            ("x-amz-date".to_owned(), "20260710T000000Z".to_owned()),
        ];
        headers.extend(extra);
        headers.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            signed_headers_list(&headers),
            "host;x-amz-checksum-mode;x-amz-content-sha256;x-amz-date"
        );
    }

    #[test]
    fn validate_endpoint_accepts_scheme_authority_only() {
        assert!(validate_endpoint("https://minio.example:9000").is_ok());
        assert!(validate_endpoint("https://s3.example.test/").is_ok());
        assert!(validate_endpoint("http://127.0.0.1:9000").is_ok());
    }

    #[test]
    fn validate_endpoint_rejects_a_path_prefix() {
        // A gateway path prefix would be signed differently than it's sent → 403.
        let err = validate_endpoint("https://gw.example/s3").unwrap_err();
        assert!(matches!(err, S3Error::Transport { .. }));
        assert!(err.to_string().contains("no path"));
    }

    #[test]
    fn new_rejects_endpoint_with_path() {
        let config = S3Config {
            bucket: "b".to_owned(),
            region: "us-east-1".to_owned(),
            endpoint: Some("https://gw.example/s3".to_owned()),
            force_path_style: true,
        };
        let creds = S3Credentials {
            access_key_id: "k".to_owned(),
            secret_access_key: "s".to_owned(),
        };
        assert!(S3Client::new(config, creds).is_err());
    }

    #[test]
    fn client_builds_with_uncapped_transfer_timeouts() {
        // The transfer client must construct with the overall timeout disabled
        // (so multi-GB streams aren't capped) plus the connect/stall guards.
        let config = S3Config {
            bucket: "b".to_owned(),
            region: "us-east-1".to_owned(),
            endpoint: Some("https://minio.example:9000".to_owned()),
            force_path_style: true,
        };
        let creds = S3Credentials {
            access_key_id: "k".to_owned(),
            secret_access_key: "s".to_owned(),
        };
        assert!(S3Client::new(config, creds).is_ok());
        // Sanity-check the guard constants stay sane (fast connect, long stall).
        // The stall guard is Linux-only (TCP_USER_TIMEOUT).
        #[cfg(target_os = "linux")]
        assert!(TRANSFER_CONNECT_TIMEOUT < TRANSFER_STALL_TIMEOUT);
    }

    #[test]
    fn sha256_writer_matches_one_shot_hash() {
        use std::io::Write as _;
        // Streaming the object through the Write sink (multiple chunks) yields the
        // same digest as hashing the whole buffer — this is the rehash-verify path.
        let mut w = Sha256Writer::new();
        w.write_all(b"hello ").unwrap();
        w.write_all(b"streamed ").unwrap();
        w.write_all(b"world").unwrap();
        assert_eq!(w.finalize(), sha256_raw(b"hello streamed world"));
    }

    #[test]
    fn sha256_file_streams_length_and_digest() {
        // A payload larger than the 64 KiB read buffer spans multiple chunks, so
        // this exercises the streaming loop (constant memory) used before upload.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("blob.bin");
        let data: Vec<u8> = (0..100_000u32)
            .map(|i| u8::try_from(i % 256).unwrap())
            .collect();
        std::fs::write(&path, &data).unwrap();
        let (digest, len) = sha256_file(&path).unwrap();
        assert_eq!(len, u64::try_from(data.len()).unwrap());
        assert_eq!(digest, sha256_raw(&data));
    }

    #[test]
    fn download_object_streams_response_body_into_writer() {
        use std::io::{Read as _, Write as _};
        // A one-shot local HTTP server returns a canned body; `download_object`
        // must stream it straight into the caller's writer.
        let body = b"streamed-object-body-contents".to_vec();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let body_for_server = body.clone();
        let server = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            // Consume the request headers so the client finishes sending.
            let mut acc = Vec::new();
            let mut buf = [0u8; 512];
            loop {
                let n = sock.read(&mut buf).unwrap();
                if n == 0 {
                    break;
                }
                acc.extend_from_slice(&buf[..n]);
                if acc.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
                body_for_server.len()
            );
            sock.write_all(head.as_bytes()).unwrap();
            sock.write_all(&body_for_server).unwrap();
            sock.flush().unwrap();
        });

        let client = S3Client::new(
            S3Config {
                bucket: "b".to_owned(),
                region: "us-east-1".to_owned(),
                endpoint: Some(format!("http://127.0.0.1:{port}")),
                force_path_style: true,
            },
            S3Credentials {
                access_key_id: "k".to_owned(),
                secret_access_key: "s".to_owned(),
            },
        )
        .unwrap();

        let mut sink: Vec<u8> = Vec::new();
        client
            .download_object("prefix/obj.dump", &mut sink)
            .unwrap();
        server.join().unwrap();
        assert_eq!(sink, body);
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn put_file_and_verify_deletes_object_on_verify_failure() {
        // #1760: `put_file_and_verify` WRITES the object BEFORE verifying it, so a
        // post-upload verify failure must best-effort DELETE the just-written key
        // — leaving no corrupt/partial object behind. A one-connection local S3
        // stub accepts the PUT, then answers the verify HEAD with a WRONG
        // Content-Length (forcing a length-mismatch `VerifyFailed`), and records
        // that a DELETE for the same key follows. The multipart path funnels
        // through the same `verify_or_delete` helper, so proving the single-PUT
        // path proves the shared cleanup.
        use std::io::{Read as _, Write as _};
        use std::time::Duration;

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("artifact.dump");
        let payload = b"verify-failure-cleanup-body";
        std::fs::write(&path, payload).unwrap();

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let payload_len = payload.len();

        let server = std::thread::spawn(move || {
            let mut methods: Vec<String> = Vec::new();
            // reqwest's blocking client reuses one keep-alive connection for the
            // PUT/HEAD/DELETE sequence; the outer loop tolerates a fresh
            // connection per request too. Read timeouts stop a stray idle
            // connection from hanging the test.
            'accept: for _conn in 0..8 {
                let Ok((mut sock, _)) = listener.accept() else {
                    break;
                };
                sock.set_read_timeout(Some(Duration::from_secs(5))).ok();
                loop {
                    // Read the request head (up to CRLFCRLF).
                    let mut acc: Vec<u8> = Vec::new();
                    let mut buf = [0u8; 1024];
                    let header_end = loop {
                        if let Some(p) = acc.windows(4).position(|w| w == b"\r\n\r\n") {
                            break Some(p + 4);
                        }
                        match sock.read(&mut buf) {
                            Ok(0) | Err(_) => break None,
                            Ok(n) => acc.extend_from_slice(&buf[..n]),
                        }
                    };
                    let Some(header_end) = header_end else {
                        break; // connection closed / idle — try the next one
                    };
                    let head = String::from_utf8_lossy(&acc[..header_end]).into_owned();
                    let method = head
                        .lines()
                        .next()
                        .and_then(|l| l.split_whitespace().next())
                        .unwrap_or_default()
                        .to_owned();
                    let content_length: usize = head
                        .lines()
                        .find_map(|l| {
                            let (k, v) = l.split_once(':')?;
                            k.trim()
                                .eq_ignore_ascii_case("content-length")
                                .then(|| v.trim().parse().ok())
                                .flatten()
                        })
                        .unwrap_or(0);
                    // Consume any request body (the PUT streams the file).
                    let mut remaining = content_length.saturating_sub(acc.len() - header_end);
                    while remaining > 0 {
                        match sock.read(&mut buf) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => remaining = remaining.saturating_sub(n),
                        }
                    }
                    methods.push(method.clone());
                    match method.as_str() {
                        "HEAD" => {
                            // A wrong length forces verify_head's length-mismatch
                            // VerifyFailed BEFORE any checksum/GET fallback.
                            let resp = format!(
                                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
                                payload_len + 999
                            );
                            sock.write_all(resp.as_bytes()).ok();
                        }
                        "DELETE" => {
                            sock.write_all(b"HTTP/1.1 204 No Content\r\n\r\n").ok();
                            sock.flush().ok();
                            break 'accept;
                        }
                        _ => {
                            sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                                .ok();
                        }
                    }
                    sock.flush().ok();
                }
            }
            methods
        });

        let client = S3Client::new(
            S3Config {
                bucket: "b".to_owned(),
                region: "us-east-1".to_owned(),
                endpoint: Some(format!("http://127.0.0.1:{port}")),
                force_path_style: true,
            },
            S3Credentials {
                access_key_id: "k".to_owned(),
                secret_access_key: "s".to_owned(),
            },
        )
        .unwrap();

        let err = client
            .put_file_and_verify("prefix/artifact.dump", &path)
            .expect_err("verify must fail on the mismatched HEAD length");
        assert!(
            matches!(err, S3Error::VerifyFailed { .. }),
            "expected VerifyFailed, got {err:?}"
        );

        let methods = server.join().unwrap();
        assert_eq!(
            methods,
            vec!["PUT".to_owned(), "HEAD".to_owned(), "DELETE".to_owned()],
            "on a verify failure the object must be PUT, verified via HEAD, then DELETEd",
        );
    }

    #[test]
    fn multipart_part_count_divides_ceiling() {
        assert_eq!(multipart_part_count(0, 5), 0);
        assert_eq!(multipart_part_count(5, 5), 1);
        assert_eq!(multipart_part_count(6, 5), 2);
        // ~12 MiB at a 5 MiB part size → 3 parts (the Docker round-trip's shape).
        let twelve_mib = 12 * 1024 * 1024;
        let five_mib = 5 * 1024 * 1024;
        assert_eq!(multipart_part_count(twelve_mib, five_mib), 3);
    }

    #[test]
    fn effective_part_size_floors_at_s3_minimum() {
        // A tiny configured part size is floored to S3's 5 MiB minimum.
        assert_eq!(
            effective_part_size(100 * 1024 * 1024, 1024),
            S3_MIN_PART_SIZE
        );
    }

    #[test]
    fn effective_part_size_keeps_a_valid_configured_size() {
        let part = 8 * 1024 * 1024;
        assert_eq!(effective_part_size(64 * 1024 * 1024, part), part);
    }

    #[test]
    fn effective_part_size_grows_to_fit_part_cap() {
        // A file so large that the configured part size would exceed 10 000 parts
        // must transparently grow the part size to stay within the cap.
        let configured = S3_MIN_PART_SIZE;
        // 10 001 parts' worth of bytes at the minimum size.
        let len = (S3_MAX_PARTS + 1) * S3_MIN_PART_SIZE;
        let eff = effective_part_size(len, configured);
        assert!(
            eff > configured,
            "part size should grow past the configured floor"
        );
        assert!(
            multipart_part_count(len, eff) <= S3_MAX_PARTS,
            "grown part size must keep the plan within S3's 10 000-part cap"
        );
    }

    #[test]
    fn build_complete_multipart_xml_lists_parts_in_order() {
        let parts = vec![
            (1u32, "\"etag-one\"".to_owned()),
            (2u32, "\"etag-two\"".to_owned()),
        ];
        let xml = build_complete_multipart_xml(&parts);
        assert_eq!(
            xml,
            "<CompleteMultipartUpload>\
             <Part><PartNumber>1</PartNumber><ETag>\"etag-one\"</ETag></Part>\
             <Part><PartNumber>2</PartNumber><ETag>\"etag-two\"</ETag></Part>\
             </CompleteMultipartUpload>"
        );
    }

    #[test]
    fn build_complete_multipart_xml_escapes_etag() {
        let parts = vec![(1u32, "a&b<c".to_owned())];
        let xml = build_complete_multipart_xml(&parts);
        assert!(xml.contains("<ETag>a&amp;b&lt;c</ETag>"));
    }

    #[test]
    fn canonical_query_encodes_and_sorts() {
        // Flag-only param encodes as `key=`; pairs sort by encoded key.
        assert_eq!(canonical_query(&[("uploads", "")]), "uploads=");
        assert_eq!(
            canonical_query(&[("partNumber", "2"), ("uploadId", "abc/def")]),
            "partNumber=2&uploadId=abc%2Fdef"
        );
    }

    #[test]
    fn classify_complete_response_accepts_result_body() {
        let ok = r#"<?xml version="1.0"?><CompleteMultipartUploadResult>\
            <Location>http://x</Location><ETag>"abc-3"</ETag>\
            </CompleteMultipartUploadResult>"#;
        assert!(classify_complete_response(ok).is_ok());
    }

    #[test]
    fn classify_complete_response_detects_200_with_error_body() {
        // S3 returns 200 then an <Error> body when assembly fails; we must treat
        // that as a failure and surface the <Code>.
        let err = "<Error><Code>InternalError</Code><Message>oops</Message></Error>";
        assert_eq!(
            classify_complete_response(err).unwrap_err(),
            "InternalError"
        );
    }

    #[test]
    fn classify_complete_response_rejects_unrecognized_body() {
        assert_eq!(
            classify_complete_response("garbage").unwrap_err(),
            "UnrecognizedCompleteResponse"
        );
    }

    #[test]
    fn classify_complete_response_rejects_error_even_with_result_substring() {
        // A 200 body that carries BOTH an <Error> and (defensively) the result tag
        // as a substring must still be treated as a failure — the <Error> wins.
        let tricky = "<Error><Code>SlowDown</Code></Error><CompleteMultipartUploadResult/>";
        assert_eq!(classify_complete_response(tricky).unwrap_err(), "SlowDown");
    }

    #[test]
    fn is_composite_checksum_detects_partcount_suffix() {
        assert!(is_composite_checksum("AbCdEf0123456789=-3"));
        assert!(is_composite_checksum("x-10000"));
        // A whole-object base64 sha256 (no `-<n>` suffix) is NOT composite.
        assert!(!is_composite_checksum(
            "47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU="
        ));
        // A trailing dash with no digits, or no head, is not a composite marker.
        assert!(!is_composite_checksum("abc-"));
        assert!(!is_composite_checksum("-3"));
    }

    #[test]
    fn verify_head_ignores_composite_checksum_and_requests_rehash() {
        // An endpoint that echoes a COMPOSITE checksum for a multipart object must
        // NOT trip a spurious mismatch — length matches, so fall through to rehash.
        let expected = sha256_base64(b"hello");
        let info = RemoteObjectInfo {
            content_length: 5,
            checksum_sha256: Some(format!("{expected}-3")),
        };
        assert!(!verify_head(5, &expected, &info).unwrap());
    }

    #[test]
    fn parse_upload_id_from_create_response() {
        let body = r#"<?xml version="1.0" encoding="UTF-8"?>
        <InitiateMultipartUploadResult>
          <Bucket>b</Bucket>
          <Key>db/prod/x.dump</Key>
          <UploadId>VXBsb2FkSUQtMTIz</UploadId>
        </InitiateMultipartUploadResult>"#;
        assert_eq!(
            xml_first(body, "UploadId").as_deref(),
            Some("VXBsb2FkSUQtMTIz")
        );
    }

    #[test]
    fn read_up_to_fills_across_short_reads() {
        // A reader that hands back one byte at a time must still fill the buffer.
        struct DribbleReader<'a> {
            data: &'a [u8],
            pos: usize,
        }
        impl std::io::Read for DribbleReader<'_> {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                if self.pos >= self.data.len() || buf.is_empty() {
                    return Ok(0);
                }
                buf[0] = self.data[self.pos];
                self.pos += 1;
                Ok(1)
            }
        }
        let mut r = DribbleReader {
            data: b"abcdef",
            pos: 0,
        };
        let mut buf = [0u8; 4];
        assert_eq!(read_up_to(&mut r, &mut buf).unwrap(), 4);
        assert_eq!(&buf, b"abcd");
        let mut rest = [0u8; 4];
        assert_eq!(read_up_to(&mut r, &mut rest).unwrap(), 2);
        assert_eq!(&rest[..2], b"ef");
    }

    #[test]
    fn with_part_size_overrides_only_part_size_not_threshold() {
        // Low 3: shrinking the part size must NOT lower the multipart threshold,
        // so a sub-threshold file is never forced into a multipart upload.
        let client = S3Client::new(
            S3Config {
                bucket: "b".to_owned(),
                region: "us-east-1".to_owned(),
                endpoint: Some("https://minio.example:9000".to_owned()),
                force_path_style: true,
            },
            S3Credentials {
                access_key_id: "k".to_owned(),
                secret_access_key: "s".to_owned(),
            },
        )
        .unwrap()
        .with_part_size(5 * 1024 * 1024);
        assert_eq!(client.multipart_part_size, 5 * 1024 * 1024);
        // Threshold stays at the default — unchanged by a part-size override.
        assert_eq!(client.multipart_threshold, MULTIPART_THRESHOLD);
    }

    /// Build a client for addressing-mode tests (no network).
    fn client_for(bucket: &str, endpoint: Option<&str>, force_path_style: bool) -> S3Client {
        S3Client::new(
            S3Config {
                bucket: bucket.to_owned(),
                region: "us-west-2".to_owned(),
                endpoint: endpoint.map(str::to_owned),
                force_path_style,
            },
            S3Credentials {
                access_key_id: "k".to_owned(),
                secret_access_key: "s".to_owned(),
            },
        )
        .unwrap()
    }

    #[test]
    fn aws_virtual_hosted_addressing_is_the_default() {
        // AWS + force_path_style=false → bucket in the HOST, key-only path.
        let c = client_for("mybucket", None, false);
        assert_eq!(
            c.endpoint_base(),
            "https://mybucket.s3.us-west-2.amazonaws.com"
        );
        assert_eq!(c.host().unwrap(), "mybucket.s3.us-west-2.amazonaws.com");
        assert_eq!(c.canonical_path("db/x.dump"), "/db/x.dump");
    }

    #[test]
    fn aws_force_path_style_uses_regional_host_and_bucket_in_path() {
        // P2 #14: AWS + force_path_style=true must NOT be virtual-hosted — the
        // regional host is bucketless and the bucket goes in the path.
        let c = client_for("mybucket", None, true);
        assert_eq!(c.endpoint_base(), "https://s3.us-west-2.amazonaws.com");
        assert_eq!(c.host().unwrap(), "s3.us-west-2.amazonaws.com");
        assert_eq!(c.canonical_path("db/x.dump"), "/mybucket/db/x.dump");
        // The signed canonical URI must be the tail of the actually-sent URL
        // (SigV4 consistency): request_url == endpoint_base + canonical_path.
        assert_eq!(
            c.request_url("db/x.dump", ""),
            "https://s3.us-west-2.amazonaws.com/mybucket/db/x.dump"
        );
    }

    #[test]
    fn aws_dotted_bucket_with_force_path_style_is_path_style() {
        // A dotted bucket can't use virtual-hosted TLS; path-style keeps it off
        // the host entirely.
        let c = client_for("my.dotted.bucket", None, true);
        assert_eq!(c.host().unwrap(), "s3.us-west-2.amazonaws.com");
        assert!(!c.host().unwrap().contains("my.dotted.bucket"));
        assert_eq!(
            c.canonical_path("control.dump"),
            "/my.dotted.bucket/control.dump"
        );
    }

    #[test]
    fn custom_endpoint_addressing_is_unchanged_by_the_aws_fix() {
        // Custom endpoint + path-style: bucket in path, host verbatim.
        let path = client_for("b", Some("https://minio.example:9000"), true);
        assert_eq!(path.host().unwrap(), "minio.example:9000");
        assert_eq!(path.canonical_path("k"), "/b/k");
        // Custom endpoint + virtual-hosted: bucket prefixes the custom host.
        let virt = client_for("b", Some("https://minio.example:9000"), false);
        assert_eq!(virt.host().unwrap(), "b.minio.example:9000");
        assert_eq!(virt.canonical_path("k"), "/k");
    }
}
