//! S3-compatible [`ReplicaDestination`] for continuous `SQLite` replication
//! (issue #1628).
//!
//! Signing is [`crate::sigv4`] — the same primitives `autumn db backup --upload`
//! signs with — so a replica and an offsite backup interoperate with the same
//! endpoint, the same credential conventions, and one implementation of `SigV4`
//! between them.
//!
//! # Blocking on purpose
//!
//! This uses `reqwest::blocking`, and every caller drives it from a **dedicated
//! replication thread** (see [`super::engine`]) or from the synchronous CLI —
//! never from inside the async runtime, where a blocking client must not be
//! constructed, used, or dropped. Keeping the whole replication tick on one
//! blocking thread also keeps the server's reactor free of the replicator's file
//! and network I/O entirely.
//!
//! # Credential safety
//!
//! Credentials arrive already resolved from the environment variables config
//! *names* (never inlines). They are used only to derive a `SigV4` signing key.
//! No error, log line, or `Debug` output in this module can carry them:
//! [`S3Credentials`] redacts both fields, and [`DestinationError`] holds only an
//! operation name, an HTTP status, a provider error code, and a key.
//!
//! # Object size
//!
//! Replication objects are small — a WAL segment is bounded by the checkpoint
//! threshold, and a base snapshot is a gzip'd database file. Uploads use a
//! single `PutObject`, so a snapshot larger than S3's 5 GiB single-PUT limit is
//! refused with an actionable message rather than silently failing; a database
//! that large belongs on the Postgres tier (#1614).

// autumn-panic-gate: durability-critical module — production code path must be
// panic-free. See CONTRIBUTING.md "Request-path panic gate". Justify exceptions
// with #[allow(clippy::<lint>, reason = "…")] at the narrowest scope.
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::todo,
        clippy::unimplemented,
        clippy::indexing_slicing,
        clippy::string_slice,
        clippy::arithmetic_side_effects,
    )
)]

use std::fmt;
use std::path::Path;
use std::time::Duration;

use crate::sigv4;

use super::destination::{DestinationError, ReplicaDestination, validate_key};

/// S3 service name in the `SigV4` credential scope.
const SERVICE: &str = "s3";

/// Connect-phase timeout: fail fast on an unreachable endpoint without bounding
/// a healthy transfer.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// Overall per-request timeout. Replication objects are small and a tick must
/// not wedge the replication thread, so unlike the offsite-backup client (which
/// streams multi-GB dumps) this one *does* cap total request duration.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// S3's single-`PutObject` ceiling.
const MAX_SINGLE_PUT: u64 = 5 * 1024 * 1024 * 1024;

/// Upper bound on `ListObjectsV2` pages, so a broken endpoint that never stops
/// returning a continuation token cannot loop forever.
const MAX_LIST_PAGES: usize = 1_000;

/// Resolved S3 credentials. Never formatted.
pub struct S3Credentials {
    /// Access key id.
    pub access_key_id: String,
    /// Secret access key.
    pub secret_access_key: String,
}

impl fmt::Debug for S3Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("S3Credentials")
            .field("access_key_id", &"<redacted>")
            .field("secret_access_key", &"<redacted>")
            .finish()
    }
}

/// Connection settings for [`S3Destination`].
#[derive(Debug, Clone)]
pub struct S3Settings {
    /// Target bucket.
    pub bucket: String,
    /// Region for the `SigV4` credential scope (R2 uses `auto`).
    pub region: String,
    /// Custom endpoint (`https://minio.example:9000`); `None` means AWS.
    pub endpoint: Option<String>,
    /// Path-style addressing (`{endpoint}/{bucket}/{key}`), required by most
    /// self-hosted and R2 endpoints.
    pub force_path_style: bool,
}

/// An S3-compatible replica destination.
pub struct S3Destination {
    settings: S3Settings,
    credentials: S3Credentials,
    http: reqwest::blocking::Client,
    /// Clock indirection so signing is deterministic under test.
    now: fn() -> chrono::DateTime<chrono::Utc>,
}

impl fmt::Debug for S3Destination {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("S3Destination")
            .field("settings", &self.settings)
            .finish_non_exhaustive()
    }
}

impl S3Destination {
    /// Build a destination.
    ///
    /// Must be called from a blocking thread (never inside the async runtime) —
    /// see the module docs.
    ///
    /// # Errors
    ///
    /// Returns [`DestinationError::Rejected`] when the endpoint carries a path
    /// prefix (which `SigV4` path canonicalization cannot express, so every
    /// request would 403) or when the HTTP stack cannot be built.
    pub fn new(settings: S3Settings, credentials: S3Credentials) -> Result<Self, DestinationError> {
        if settings.bucket.trim().is_empty() {
            return Err(DestinationError::Rejected {
                detail: "replication.s3.bucket is unset".to_owned(),
            });
        }
        if let Some(endpoint) = &settings.endpoint {
            validate_endpoint(endpoint)?;
        }
        let http = reqwest::blocking::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|e| DestinationError::Rejected {
                detail: format!("could not build the replication HTTP client: {e}"),
            })?;
        Ok(Self {
            settings,
            credentials,
            http,
            now: chrono::Utc::now,
        })
    }

    /// Scheme + authority for the endpoint (no path). The bucket lands in the
    /// host for virtual-hosted addressing and in the path for path-style.
    fn endpoint_base(&self) -> String {
        match &self.settings.endpoint {
            Some(endpoint) => {
                let endpoint = endpoint.trim_end_matches('/');
                if self.settings.force_path_style {
                    endpoint.to_owned()
                } else if let Some(rest) = endpoint.strip_prefix("https://") {
                    format!("https://{}.{rest}", self.settings.bucket)
                } else if let Some(rest) = endpoint.strip_prefix("http://") {
                    format!("http://{}.{rest}", self.settings.bucket)
                } else {
                    endpoint.to_owned()
                }
            }
            None if self.settings.force_path_style => {
                format!("https://s3.{}.amazonaws.com", self.settings.region)
            }
            None => format!(
                "https://{}.s3.{}.amazonaws.com",
                self.settings.bucket, self.settings.region
            ),
        }
    }

    /// The `host[:port]` used for the `Host` header and for signing.
    fn host(&self) -> Result<String, DestinationError> {
        let base = self.endpoint_base();
        let url = url::Url::parse(&base).map_err(|e| DestinationError::Rejected {
            detail: format!("invalid replication endpoint: {e}"),
        })?;
        let host = url.host_str().ok_or_else(|| DestinationError::Rejected {
            detail: "replication endpoint has no host".to_owned(),
        })?;
        Ok(url
            .port()
            .map_or_else(|| host.to_owned(), |port| format!("{host}:{port}")))
    }

    /// The canonical request path `SigV4` signs for `key`.
    fn canonical_path(&self, key: &str) -> String {
        let key = key.trim_start_matches('/');
        if self.settings.force_path_style {
            format!(
                "/{}/{}",
                self.settings.bucket,
                sigv4::uri_encode(key, false)
            )
        } else {
            format!("/{}", sigv4::uri_encode(key, false))
        }
    }

    /// Sign and send one request, returning the response on 2xx.
    fn send(
        &self,
        op: &'static str,
        method: reqwest::Method,
        key: &str,
        query: &str,
        body: Option<Vec<u8>>,
    ) -> Result<reqwest::blocking::Response, DestinationError> {
        let now = (self.now)();
        let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
        let date = now.format("%Y%m%d").to_string();
        let payload_hash = body.as_ref().map_or_else(
            || sigv4::EMPTY_PAYLOAD_SHA256.to_owned(),
            |bytes| sigv4::sha256_hex(bytes),
        );
        let host = self.host()?;

        let mut headers: Vec<sigv4::Header> = vec![
            ("host".to_owned(), host),
            ("x-amz-content-sha256".to_owned(), payload_hash.clone()),
            ("x-amz-date".to_owned(), amz_date.clone()),
        ];
        headers.sort_by(|a, b| a.0.cmp(&b.0));

        let canonical = sigv4::canonical_request(
            method.as_str(),
            &self.canonical_path(key),
            query,
            &headers,
            &payload_hash,
        );
        let scope = sigv4::credential_scope(&date, &self.settings.region, SERVICE);
        let signature = sigv4::signature(
            &self.credentials.secret_access_key,
            &date,
            &self.settings.region,
            SERVICE,
            &amz_date,
            &scope,
            &canonical,
        );
        let authorization = sigv4::authorization_header(
            &self.credentials.access_key_id,
            &scope,
            &headers,
            &signature,
        );

        let base = self.endpoint_base();
        let path = self.canonical_path(key);
        let url = if query.is_empty() {
            format!("{base}{path}")
        } else {
            format!("{base}{path}?{query}")
        };

        let mut request = self
            .http
            .request(method, &url)
            .header("x-amz-date", &amz_date)
            .header("x-amz-content-sha256", &payload_hash)
            .header(reqwest::header::AUTHORIZATION, authorization);
        if let Some(bytes) = body {
            request = request.body(bytes);
        }

        let response = request.send().map_err(|e| DestinationError::Io {
            op,
            detail: e.to_string(),
        })?;
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(DestinationError::NotFound {
                key: key.to_owned(),
            });
        }
        let code = response.text().ok().as_deref().and_then(parse_error_code);
        Err(DestinationError::Remote {
            op,
            status: status.as_u16(),
            code,
        })
    }
}

/// Reject an endpoint that carries a path prefix: `SigV4` signs `/{bucket}/{key}`
/// or `/{key}` and knows nothing about an endpoint path segment, so the signed
/// path and the sent path would diverge and every request would 403.
fn validate_endpoint(endpoint: &str) -> Result<(), DestinationError> {
    let url = url::Url::parse(endpoint).map_err(|e| DestinationError::Rejected {
        detail: format!("invalid endpoint {endpoint:?}: {e}"),
    })?;
    let path = url.path();
    if !path.is_empty() && path != "/" {
        return Err(DestinationError::Rejected {
            detail: format!(
                "endpoint {endpoint:?} must be scheme + host[:port] only (no path); \
                 a path prefix like {path:?} is not supported"
            ),
        });
    }
    Ok(())
}

impl ReplicaDestination for S3Destination {
    fn describe(&self) -> String {
        self.settings.endpoint.as_ref().map_or_else(
            || {
                format!(
                    "s3://{}  (AWS {})",
                    self.settings.bucket, self.settings.region
                )
            },
            |endpoint| format!("s3://{}  ({endpoint})", self.settings.bucket),
        )
    }

    fn put(&self, key: &str, body: &[u8]) -> Result<(), DestinationError> {
        validate_key(key)?;
        let len = body.len() as u64;
        if len > MAX_SINGLE_PUT {
            return Err(DestinationError::Rejected {
                detail: format!(
                    "replication object {key:?} is {len} bytes, above S3's single-upload \
                     limit of {MAX_SINGLE_PUT}"
                ),
            });
        }
        self.send("upload", reqwest::Method::PUT, key, "", Some(body.to_vec()))
            .map(|_| ())
    }

    fn put_file(&self, key: &str, path: &Path) -> Result<(), DestinationError> {
        let bytes = std::fs::read(path).map_err(DestinationError::io("read upload source"))?;
        self.put(key, &bytes)
    }

    fn get(&self, key: &str) -> Result<Vec<u8>, DestinationError> {
        validate_key(key)?;
        let response = self.send("download", reqwest::Method::GET, key, "", None)?;
        response
            .bytes()
            .map(|b| b.to_vec())
            .map_err(|e| DestinationError::Io {
                op: "download",
                detail: e.to_string(),
            })
    }

    fn get_to_file(&self, key: &str, path: &Path) -> Result<(), DestinationError> {
        let bytes = self.get(key)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(DestinationError::io("create download directory"))?;
        }
        std::fs::write(path, &bytes).map_err(DestinationError::io("write download"))
    }

    fn list(&self, prefix: &str) -> Result<Vec<String>, DestinationError> {
        let mut keys = Vec::new();
        let mut token: Option<String> = None;
        for _ in 0..MAX_LIST_PAGES {
            let mut params: Vec<(&str, &str)> = vec![("list-type", "2"), ("prefix", prefix)];
            if let Some(t) = &token {
                params.push(("continuation-token", t.as_str()));
            }
            let query = sigv4::canonical_query(&params);
            let response = self.send("list", reqwest::Method::GET, "", &query, None)?;
            let body = response.text().map_err(|e| DestinationError::Io {
                op: "list",
                detail: e.to_string(),
            })?;
            let (page, next) = parse_list_objects(&body)?;
            keys.extend(page);
            let Some(next) = next else {
                keys.sort();
                return Ok(keys);
            };
            token = Some(next);
        }
        Err(DestinationError::Rejected {
            detail: format!("listing {prefix:?} did not terminate within {MAX_LIST_PAGES} pages"),
        })
    }

    fn delete(&self, key: &str) -> Result<(), DestinationError> {
        validate_key(key)?;
        match self.send("delete", reqwest::Method::DELETE, key, "", None) {
            Ok(_) => Ok(()),
            Err(e) if e.is_not_found() => Ok(()),
            Err(e) => Err(e),
        }
    }
}

/// Extract the first `<tag>…</tag>` body from an XML document.
fn xml_first(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)?.checked_add(open.len())?;
    let rest = xml.get(start..)?;
    let end = rest.find(&close)?;
    Some(xml_unescape(rest.get(..end)?))
}

/// Minimal XML entity unescaping for the five predefined entities.
fn xml_unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// Parse a `ListObjectsV2` response into its keys plus the continuation token
/// when the listing is truncated.
fn parse_list_objects(xml: &str) -> Result<(Vec<String>, Option<String>), DestinationError> {
    let mut keys = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find("<Contents>") {
        let after = rest.get(start..).unwrap_or("");
        let Some(end) = after.find("</Contents>") else {
            break;
        };
        let entry = after.get(..end).unwrap_or("");
        if let Some(key) = xml_first(entry, "Key") {
            keys.push(key);
        }
        rest = after.get(end..).unwrap_or("");
        // Guard against a pathological document that never advances.
        if rest.is_empty() {
            break;
        }
    }
    let truncated = xml_first(xml, "IsTruncated").is_some_and(|v| v.trim() == "true");
    let token = if truncated {
        xml_first(xml, "NextContinuationToken")
    } else {
        None
    };
    if truncated && token.is_none() {
        return Err(DestinationError::Rejected {
            detail: "listing is truncated but carries no continuation token".to_owned(),
        });
    }
    Ok((keys, token))
}

/// Pull the `<Code>` out of an S3 error document, when present.
fn parse_error_code(xml: &str) -> Option<String> {
    xml_first(xml, "Code")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn destination(endpoint: Option<&str>, force_path_style: bool) -> S3Destination {
        S3Destination::new(
            S3Settings {
                bucket: "replicas".to_owned(),
                region: "us-east-1".to_owned(),
                endpoint: endpoint.map(str::to_owned),
                force_path_style,
            },
            S3Credentials {
                access_key_id: "AKIDEXAMPLE".to_owned(),
                secret_access_key: "secret".to_owned(),
            },
        )
        .expect("destination")
    }

    #[test]
    fn credentials_never_format_their_secret() {
        let creds = S3Credentials {
            access_key_id: "AKIA".to_owned(),
            secret_access_key: "super-secret".to_owned(),
        };
        let rendered = format!("{creds:?}");
        assert!(!rendered.contains("super-secret"), "{rendered}");
        assert!(!rendered.contains("AKIA"), "{rendered}");
    }

    #[test]
    fn describe_and_debug_never_leak_credentials() {
        let dest = destination(Some("https://minio.test:9000"), true);
        assert_eq!(dest.describe(), "s3://replicas  (https://minio.test:9000)");
        assert!(!format!("{dest:?}").contains("secret"));
    }

    #[test]
    fn addressing_follows_path_style_and_virtual_hosted_rules() {
        let path_style = destination(Some("https://minio.test:9000"), true);
        assert_eq!(path_style.endpoint_base(), "https://minio.test:9000");
        assert_eq!(path_style.host().expect("host"), "minio.test:9000");
        assert_eq!(path_style.canonical_path("a/b.seg"), "/replicas/a/b.seg");

        let virtual_hosted = destination(Some("https://minio.test"), false);
        assert_eq!(
            virtual_hosted.endpoint_base(),
            "https://replicas.minio.test"
        );
        assert_eq!(virtual_hosted.canonical_path("a/b.seg"), "/a/b.seg");

        let aws = destination(None, false);
        assert_eq!(
            aws.endpoint_base(),
            "https://replicas.s3.us-east-1.amazonaws.com"
        );
        let aws_path = destination(None, true);
        assert_eq!(
            aws_path.endpoint_base(),
            "https://s3.us-east-1.amazonaws.com"
        );
        assert_eq!(aws_path.canonical_path("k"), "/replicas/k");
    }

    #[test]
    fn construction_refuses_an_endpoint_with_a_path_prefix() {
        let err = S3Destination::new(
            S3Settings {
                bucket: "b".to_owned(),
                region: "auto".to_owned(),
                endpoint: Some("https://gw.example/s3".to_owned()),
                force_path_style: true,
            },
            S3Credentials {
                access_key_id: "a".to_owned(),
                secret_access_key: "s".to_owned(),
            },
        )
        .expect_err("must refuse");
        assert!(matches!(err, DestinationError::Rejected { .. }), "{err}");
    }

    #[test]
    fn construction_refuses_an_empty_bucket() {
        let err = S3Destination::new(
            S3Settings {
                bucket: "  ".to_owned(),
                region: "auto".to_owned(),
                endpoint: None,
                force_path_style: false,
            },
            S3Credentials {
                access_key_id: "a".to_owned(),
                secret_access_key: "s".to_owned(),
            },
        )
        .expect_err("must refuse");
        assert!(matches!(err, DestinationError::Rejected { .. }), "{err}");
    }

    #[test]
    fn list_response_parses_keys_and_pagination() {
        let xml = "<?xml version=\"1.0\"?><ListBucketResult>\
            <IsTruncated>true</IsTruncated>\
            <Contents><Key>prod/generations/g/segments/0000000000-1.seg</Key><Size>4</Size></Contents>\
            <Contents><Key>prod/generations/g/snapshot.json</Key></Contents>\
            <NextContinuationToken>tok&amp;en</NextContinuationToken>\
            </ListBucketResult>";
        let (keys, token) = parse_list_objects(xml).expect("parse");
        assert_eq!(
            keys,
            vec![
                "prod/generations/g/segments/0000000000-1.seg".to_owned(),
                "prod/generations/g/snapshot.json".to_owned(),
            ]
        );
        assert_eq!(token.as_deref(), Some("tok&en"));

        let (keys, token) = parse_list_objects(
            "<ListBucketResult><IsTruncated>false</IsTruncated></ListBucketResult>",
        )
        .expect("parse");
        assert!(keys.is_empty());
        assert!(token.is_none());
    }

    #[test]
    fn a_truncated_listing_without_a_token_is_refused() {
        let err = parse_list_objects(
            "<ListBucketResult><IsTruncated>true</IsTruncated></ListBucketResult>",
        )
        .expect_err("must refuse");
        assert!(matches!(err, DestinationError::Rejected { .. }), "{err}");
    }

    #[test]
    fn error_documents_yield_their_code() {
        assert_eq!(
            parse_error_code("<Error><Code>NoSuchBucket</Code></Error>").as_deref(),
            Some("NoSuchBucket")
        );
        assert_eq!(parse_error_code("not xml"), None);
    }
}
