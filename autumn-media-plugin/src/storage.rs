//! [`MediaStorage`] — a completed-media object store with **caller-supplied**
//! keys.
//!
//! This is a generalization of Arroyo's `VideoStorageConfig` (`src/media.rs`):
//! Arroyo hard-coded a fixed set of key namespaces (`highlights/`, `clips/`,
//! `thumbnails/`, `previews/`, `channel-avatars/`, `channel-emotes/`,
//! `avatars/`) into the storage type itself. Here the namespace lives with the
//! *caller* — every method takes the already-finished object key — so a
//! downstream app (or a later encode slice) decides its own layout while this
//! type only owns backend selection, key-prefix/URL joining, and the S3
//! upload/delete plumbing.
//!
//! It deliberately does **not** reuse `autumn-storage-s3`'s `BlobStore`: that
//! trait produces presigned/expiring URLs, validates/rejects arbitrary keys,
//! and models credentials as env-var *names*. Media objects need permanent,
//! public URLs under app-chosen keys. The aws-sdk client-builder idiom (and the
//! exact aws-* dependency pins) are copied from `autumn-storage-s3`, but the
//! surface is bespoke.
//!
//! The two backends:
//! - [`MediaStorage::Local`] keeps objects on the local filesystem and records
//!   the on-disk path as the object URI. Persist is a no-op record and delete
//!   is a no-op (the caller manages the staged file lifecycle).
//! - [`MediaStorage::S3`] uploads to any S3-compatible store (AWS S3, Fly
//!   Tigris, Cloudflare R2, `MinIO`, …) and records the `s3://{bucket}/{key}`
//!   URI.

use std::path::{Path, PathBuf};

use aws_credential_types::Credentials;
use aws_sdk_s3::Client;
use aws_sdk_s3::config::{BehaviorVersion, Region};
use aws_sdk_s3::primitives::ByteStream;
use serde::{Deserialize, Serialize};

use crate::config::MediaStorageConfig;
use crate::error::MediaError;

/// Default content type used by [`MediaStorage::persist_file`] when the caller
/// does not specify one.
pub const DEFAULT_CONTENT_TYPE: &str = "video/mp4";

/// Neutral public base used by the local backend when the config does not
/// specify one.
const DEFAULT_LOCAL_PUBLIC_BASE: &str = "/media";

/// The location a completed media object was persisted to.
///
/// The generalized form of Arroyo's `StoredVideo`: `backend` is `"local"` or
/// `"s3"`, `key` is the finished object key, and `uri` is the on-disk path
/// (local) or an `s3://{bucket}/{key}` URI (S3).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredObject {
    /// Backend label — `"local"` or `"s3"`.
    pub backend: String,
    /// The finished object key.
    pub key: String,
    /// The object URI (local path, or `s3://{bucket}/{key}`).
    pub uri: String,
}

/// Resolved S3 storage settings (concrete values, not env-var names).
///
/// `secret_access_key` is redacted by the hand-written [`std::fmt::Debug`]
/// impl, so a `MediaStorage` can be safely logged.
#[derive(Clone)]
pub struct S3MediaStorage {
    /// Bucket name.
    pub bucket: String,
    /// Endpoint URL. `None` uses the AWS SDK default endpoint.
    pub endpoint_url: Option<String>,
    /// Region, when explicitly configured (e.g. `auto` for Tigris). `None`
    /// leaves region resolution to the ambient AWS chain (`AWS_REGION`, the
    /// shared config/profile, IMDS) — the correct behaviour for a generic AWS
    /// S3 deployment on the SDK default endpoint, where forcing `auto` would
    /// break signing/resolution.
    pub region: Option<String>,
    /// Access key id. Empty together with `secret_access_key` selects the
    /// ambient AWS credential chain.
    pub access_key_id: String,
    /// Secret access key (redacted in `Debug`).
    pub secret_access_key: String,
    /// Session token for temporary/rotating credentials (Lambda/ECS), if any.
    /// `None` selects permanent static keys (e.g. Tigris) or the ambient chain.
    /// Sensitive — redacted in `Debug`.
    pub session_token: Option<String>,
    /// Public base URL that stored objects resolve under.
    pub public_base_url: String,
    /// Object key prefix (namespace) prepended to caller keys.
    pub key_prefix: String,
    /// Whether to force S3 path-style addressing.
    pub force_path_style: bool,
    /// Lazily-built, cached aws-sdk-s3 client shared across operations.
    ///
    /// Private (unlike the config fields): an internal cache, not part of the
    /// resolved configuration. The AWS SDK recommends one long-lived client, so
    /// it is built once via [`tokio::sync::OnceCell`] and reused. Wrapped in
    /// [`std::sync::Arc`] so a `#[derive(Clone)]` of `S3MediaStorage` shares the
    /// same cache. Whether the cached client's credentials can *refresh* depends
    /// on how it was built (see [`MediaStorage::build_client`]): a client built
    /// from the explicit `access_key_id` / `secret_access_key` (with or without a
    /// `session_token`) carries those static values verbatim and cannot refresh
    /// them; only the ambient-provider-chain path (used when the explicit keys
    /// are unset) refreshes credentials on its own.
    client: std::sync::Arc<tokio::sync::OnceCell<Client>>,
}

impl std::fmt::Debug for S3MediaStorage {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("S3MediaStorage")
            .field("bucket", &self.bucket)
            .field("endpoint_url", &self.endpoint_url)
            .field("region", &self.region)
            .field("access_key_id", &self.access_key_id)
            .field("secret_access_key", &"** redacted **")
            .field(
                "session_token",
                &self.session_token.as_ref().map(|_| "** redacted **"),
            )
            .field("public_base_url", &self.public_base_url)
            .field("key_prefix", &self.key_prefix)
            .field("force_path_style", &self.force_path_style)
            // `client` (the cached aws-sdk-s3 client) is intentionally omitted.
            .finish_non_exhaustive()
    }
}

/// A completed-media object store with caller-supplied keys.
#[derive(Clone, Debug)]
pub enum MediaStorage {
    /// Keep objects on the local filesystem.
    Local {
        /// Filesystem root the backend serves objects from.
        root: PathBuf,
        /// Public base URL objects resolve under.
        public_base_url: String,
    },
    /// Upload objects to an S3-compatible object store.
    S3(S3MediaStorage),
}

impl MediaStorage {
    /// Build a [`MediaStorage`] from a resolved [`MediaStorageConfig`].
    ///
    /// Defaults applied here: a blank S3 region stays `None` (ambient AWS
    /// resolution — no generic `auto` default; the Tigris `auto` region is set
    /// by the `from_arroyo_env` shim); an unset S3 public base is derived from
    /// the bucket + key prefix via the Tigris convention
    /// ([`default_tigris_public_base`]); an unset local public base becomes
    /// `/media`. An unset S3 endpoint stays `None` (the SDK default) — no Tigris
    /// endpoint is hard-coded generically here (the `from_arroyo_env` shim is
    /// where the Tigris endpoint default lives).
    ///
    /// # Errors
    ///
    /// Returns [`MediaError::MissingBucket`] when the S3 backend is selected
    /// without a bucket, and [`MediaError::PartialS3Credentials`] when exactly
    /// one of the access-key / secret-key pair is set.
    pub fn from_config(cfg: &MediaStorageConfig) -> Result<Self, MediaError> {
        use crate::config::MediaStorageBackend;

        match cfg.backend {
            MediaStorageBackend::Local => {
                let public_base_url = non_empty(cfg.public_base_url.as_deref())
                    .unwrap_or(DEFAULT_LOCAL_PUBLIC_BASE)
                    .to_owned();
                Ok(Self::Local {
                    root: PathBuf::from(&cfg.root),
                    public_base_url,
                })
            }
            MediaStorageBackend::S3 => {
                let bucket = non_empty(cfg.bucket.as_deref())
                    .ok_or(MediaError::MissingBucket)?
                    .to_owned();

                let access_key_id = non_empty(cfg.access_key_id.as_deref());
                let secret_access_key = non_empty(cfg.secret_access_key.as_deref());
                if access_key_id.is_some() != secret_access_key.is_some() {
                    return Err(MediaError::PartialS3Credentials);
                }

                // Region is threaded through only when explicitly configured;
                // an unset region stays `None` so the ambient AWS chain resolves
                // it. No generic `auto` default here — that value is
                // Tigris/arroyo-specific and is applied by the `from_arroyo_env`
                // shim, not this generic path.
                let region = non_empty(cfg.region.as_deref()).map(ToOwned::to_owned);
                let key_prefix = cfg.key_prefix.trim_matches('/').to_owned();
                let public_base_url = non_empty(cfg.public_base_url.as_deref()).map_or_else(
                    || default_tigris_public_base(&bucket, &key_prefix),
                    ToOwned::to_owned,
                );

                Ok(Self::S3(S3MediaStorage {
                    bucket,
                    endpoint_url: non_empty(cfg.endpoint_url.as_deref()).map(ToOwned::to_owned),
                    region,
                    access_key_id: access_key_id.unwrap_or_default().to_owned(),
                    secret_access_key: secret_access_key.unwrap_or_default().to_owned(),
                    session_token: non_empty(cfg.session_token.as_deref()).map(ToOwned::to_owned),
                    public_base_url,
                    key_prefix,
                    force_path_style: cfg.force_path_style,
                    client: std::sync::Arc::new(tokio::sync::OnceCell::new()),
                }))
            }
        }
    }

    /// Stable backend label — `"local"` or `"s3"`.
    #[must_use]
    pub const fn backend_name(&self) -> &'static str {
        match self {
            Self::Local { .. } => "local",
            Self::S3(_) => "s3",
        }
    }

    /// Resolve a caller-relative key into the backend's fully-resolved stored
    /// object key — the **single authority** for key-prefix + slash
    /// normalization.
    ///
    /// The local backend slash-trims the key; the S3 backend prepends the
    /// configured key prefix (both ends slash-trimmed) via [`join_key_prefix`].
    /// Every method that needs the stored key ([`Self::object_key`],
    /// [`Self::storage_uri`], [`Self::persist_file_with_content_type`], and
    /// [`Self::remove_object_if_present`]) routes through this one helper, so
    /// they can never diverge on prefix or slash handling.
    fn resolved_key(&self, key: &str) -> String {
        match self {
            Self::Local { .. } => key.trim_matches('/').to_owned(),
            Self::S3(config) => join_key_prefix(&config.key_prefix, key),
        }
    }

    /// Resolve the caller-relative key into the backend's stored object key.
    ///
    /// The local backend slash-trims the key; the S3 backend prepends the
    /// configured key prefix (slash-trimmed). This is the same resolution used
    /// by [`Self::persist_file`], [`Self::storage_uri`], and
    /// [`Self::remove_object_if_present`], so the key advertised here is exactly
    /// the key those methods store, address, and delete.
    #[must_use]
    pub fn object_key(&self, key: &str) -> String {
        self.resolved_key(key)
    }

    /// The public URL a browser fetches the object at — the backend's public
    /// base joined with the caller key.
    ///
    /// The caller key is slash-trimmed first (identically to the stored key —
    /// see [`Self::resolved_key`] / [`join_key_prefix`]) so a trailing slash can
    /// never advertise a URL (`…/clips/a.mp4/`) that differs from the object
    /// actually stored (`…/clips/a.mp4`). The key's path segments are then
    /// percent-encoded (preserving `/` separators) so a caller key containing
    /// URL-reserved characters (e.g. `#`, `?`, space) addresses exactly the
    /// stored object rather than being reinterpreted as a fragment/query by the
    /// browser.
    ///
    /// The S3 public base already embeds the key prefix (see
    /// [`default_tigris_public_base`]), so this joins the base with the
    /// caller-relative key and — unlike the stored key — does **not** re-apply
    /// the prefix here.
    #[must_use]
    pub fn public_url(&self, key: &str) -> String {
        let encoded = encode_key_for_url(key.trim_matches('/'));
        match self {
            Self::Local {
                public_base_url, ..
            } => join_url(public_base_url, &encoded),
            Self::S3(config) => join_url(&config.public_base_url, &encoded),
        }
    }

    /// The stored object URI: the on-disk path for the local backend, or an
    /// `s3://{bucket}/{stored_key}` URI for the S3 backend.
    ///
    /// The S3 form embeds the fully-resolved stored key ([`Self::resolved_key`],
    /// prefix-inclusive) — the same key [`Self::persist_file`] uploads to and
    /// [`Self::parse_s3_uri_key`] round-trips back into a caller-relative key.
    #[must_use]
    pub fn storage_uri(&self, key: &str, local_path: &Path) -> String {
        match self {
            Self::Local { .. } => local_path.display().to_string(),
            Self::S3(config) => format!("s3://{}/{}", config.bucket, self.resolved_key(key)),
        }
    }

    /// Extract the **caller-relative** object key from an `s3://{bucket}/{key}`
    /// URI (e.g. one produced by [`Self::storage_uri`]).
    ///
    /// The generalized form of Arroyo's `highlight_storage_object_key`, but with
    /// a load-bearing difference: the stored key embedded in the URI is
    /// prefix-**inclusive** (`{key_prefix}/{caller_key}`), whereas this crate's
    /// public methods ([`Self::persist_file`], [`Self::remove_object_if_present`],
    /// [`Self::public_url`]) take a caller-**relative** key and apply the prefix
    /// internally. This method therefore strips the configured `key_prefix` back
    /// off, so a key round-tripped through the URI can be handed straight to
    /// [`Self::remove_object_if_present`] and delete the **same** object
    /// [`Self::persist_file`] created — never a double-prefixed
    /// `{key_prefix}/{key_prefix}/…` that would target the wrong object and leak
    /// the real one.
    ///
    /// For the S3 backend the URI's bucket must match the currently-configured
    /// bucket: a URI from a **different** bucket (a legacy record, or one written
    /// before `media.storage.bucket` was changed) returns `None`. Otherwise the
    /// caller-relative key handed back would target the wrong bucket and, fed to
    /// [`Self::remove_object_if_present`], delete an unrelated object out of the
    /// currently-configured bucket.
    ///
    /// Returns `Some(key)` only for a well-formed S3 URI in this backend's bucket
    /// whose key is non-empty after prefix stripping; a local path, a non-`s3://`
    /// URI, a degenerate URI, a foreign-bucket URI, or a key that is nothing but
    /// the prefix returns `None`, so callers never issue a delete against an empty
    /// key or the wrong bucket.
    #[must_use]
    pub fn parse_s3_uri_key(&self, uri: &str) -> Option<String> {
        let rest = uri.strip_prefix("s3://")?;
        let (bucket, stored_key) = rest.split_once('/')?;
        if stored_key.is_empty() {
            return None;
        }
        let relative = match self {
            Self::Local { .. } => stored_key.trim_matches('/').to_owned(),
            Self::S3(config) => {
                // Refuse a key from a different bucket — deleting it against the
                // configured bucket would hit an unrelated object.
                if bucket != config.bucket {
                    return None;
                }
                strip_key_prefix(&config.key_prefix, stored_key)
            }
        };
        if relative.is_empty() {
            return None;
        }
        Some(relative)
    }

    /// Persist a completed local file under `key`, defaulting the content type
    /// to [`DEFAULT_CONTENT_TYPE`] (`video/mp4`).
    ///
    /// # Errors
    ///
    /// See [`Self::persist_file_with_content_type`].
    pub async fn persist_file(
        &self,
        key: &str,
        local_path: &Path,
    ) -> Result<StoredObject, MediaError> {
        self.persist_file_with_content_type(key, local_path, DEFAULT_CONTENT_TYPE)
            .await
    }

    /// Persist a completed local file under `key` with an explicit content
    /// type.
    ///
    /// The local backend records the on-disk path without copying (the caller
    /// owns the staged file); the S3 backend streams the file to
    /// `s3://{bucket}/{stored_key}`.
    ///
    /// # Errors
    ///
    /// Returns [`MediaError::InvalidKeySegment`] when the caller key contains a
    /// dot-only (`.`/`..`) or empty path segment (rejected at this write
    /// boundary so such an object is never stored or advertised — see
    /// [`reject_dot_only_segments`]), [`MediaError::LocalRead`] when the staged
    /// file cannot be read for upload, and [`MediaError::S3Upload`] when the
    /// remote upload fails.
    pub async fn persist_file_with_content_type(
        &self,
        key: &str,
        local_path: &Path,
        content_type: &str,
    ) -> Result<StoredObject, MediaError> {
        reject_dot_only_segments(key)?;
        match self {
            Self::Local { .. } => Ok(StoredObject {
                backend: self.backend_name().to_owned(),
                key: key.to_owned(),
                uri: self.storage_uri(key, local_path),
            }),
            Self::S3(config) => {
                let stored_key = self.resolved_key(key);
                let body = ByteStream::from_path(local_path).await.map_err(|error| {
                    MediaError::LocalRead {
                        path: local_path.display().to_string(),
                        source: std::io::Error::other(error),
                    }
                })?;
                let client = self.build_client().await;
                client
                    .put_object()
                    .bucket(&config.bucket)
                    .key(&stored_key)
                    .content_type(content_type)
                    .body(body)
                    .send()
                    .await
                    .map_err(|error| MediaError::S3Upload {
                        bucket: config.bucket.clone(),
                        key: stored_key.clone(),
                        message: error.to_string(),
                    })?;
                Ok(StoredObject {
                    backend: self.backend_name().to_owned(),
                    key: key.to_owned(),
                    uri: self.storage_uri(key, local_path),
                })
            }
        }
    }

    /// Best-effort delete of a stored object by **caller-relative** key.
    ///
    /// The key is resolved the same way [`Self::persist_file`] resolves it (via
    /// [`Self::resolved_key`] — prefix applied exactly once), so passing the
    /// same caller key you persisted deletes exactly that object. A key obtained
    /// from [`Self::parse_s3_uri_key`] is already caller-relative, so it too can
    /// be passed here directly without double-prefixing.
    ///
    /// The local backend is a no-op (the caller manages the staged file). The
    /// S3 backend issues a `DeleteObject`; S3 returns success for both existing
    /// and missing keys, so this is idempotent.
    ///
    /// # Errors
    ///
    /// Returns [`MediaError::S3Delete`] when the S3 `DeleteObject` request
    /// fails.
    pub async fn remove_object_if_present(&self, key: &str) -> Result<(), MediaError> {
        match self {
            Self::Local { .. } => Ok(()),
            Self::S3(config) => {
                let stored_key = self.resolved_key(key);
                let client = self.build_client().await;
                client
                    .delete_object()
                    .bucket(&config.bucket)
                    .key(&stored_key)
                    .send()
                    .await
                    .map_err(|error| MediaError::S3Delete {
                        bucket: config.bucket.clone(),
                        key: stored_key.clone(),
                        message: error.to_string(),
                    })?;
                Ok(())
            }
        }
    }

    /// Return the S3 backend's cached aws-sdk-s3 client, building it once.
    ///
    /// The client is built lazily on first use and cached in the backend's
    /// [`tokio::sync::OnceCell`], so every operation reuses one long-lived
    /// client (the AWS SDK recommendation) instead of rebuilding per operation.
    /// The endpoint is set **only when configured** (an improvement over
    /// Arroyo's always-set endpoint, borrowed from `autumn-storage-s3`): an
    /// unset endpoint leaves the SDK default in place.
    ///
    /// **Credential source and refresh semantics.** When both explicit
    /// credentials are present they are used as **static** `SigV4` credentials
    /// (with the optional `session_token`): the config is loaded once at
    /// construction and is not hot-reloaded, so a client built this way cannot
    /// refresh its credentials — it is the right choice for permanent static
    /// keys (e.g. Tigris) or a fixed, short-lived token for a short-lived task.
    /// When the explicit `access_key_id` / `secret_access_key` are unset, the
    /// **ambient AWS credential chain** is loaded instead (env / IMDS / ECS /
    /// STS), and that provider refreshes rotating/temporary credentials
    /// automatically — so a long-running service with rotating creds should
    /// leave the explicit credential fields empty and rely on the ambient chain.
    ///
    /// # Panics
    ///
    /// Panics if called on the local backend (callers only reach this from the
    /// S3 arms of the persist/delete methods).
    async fn build_client(&self) -> Client {
        let Self::S3(config) = self else {
            unreachable!("build_client is only called for the S3 backend");
        };

        config
            .client
            .get_or_init(|| async {
                // Only an explicitly-configured region is threaded in; when
                // unset, region is resolved from the ambient AWS chain instead
                // of being forced to a Tigris-specific `auto` that would break a
                // generic AWS S3 deployment.
                let explicit_region = non_empty(config.region.as_deref())
                    .map(|region| Region::new(region.to_owned()));
                if !config.access_key_id.is_empty() && !config.secret_access_key.is_empty() {
                    let credentials = Credentials::new(
                        config.access_key_id.clone(),
                        config.secret_access_key.clone(),
                        config.session_token.clone(),
                        None,
                        "autumn-media-plugin",
                    );
                    // Static-credential path builds the S3 config directly, so
                    // there is no ambient loader to supply a region. Use the
                    // explicit region when set (e.g. Tigris `auto`), otherwise
                    // resolve it from the ambient default chain so a generic AWS
                    // deployment with static keys + `AWS_REGION`/profile works.
                    let region = match explicit_region {
                        Some(region) => Some(region),
                        None => aws_config::defaults(BehaviorVersion::latest())
                            .load()
                            .await
                            .region()
                            .cloned(),
                    };
                    let mut builder = aws_sdk_s3::Config::builder()
                        .behavior_version(BehaviorVersion::latest())
                        .credentials_provider(credentials)
                        .force_path_style(config.force_path_style);
                    if let Some(region) = region {
                        builder = builder.region(region);
                    }
                    if let Some(endpoint) = &config.endpoint_url {
                        builder = builder.endpoint_url(endpoint);
                    }
                    Client::from_conf(builder.build())
                } else {
                    // Ambient-credential path: the default loader already carries
                    // the ambient region chain, so only override it when a region
                    // is explicitly configured.
                    let mut loader = aws_config::defaults(BehaviorVersion::latest());
                    if let Some(region) = explicit_region {
                        loader = loader.region(region);
                    }
                    let shared = loader.load().await;
                    let mut builder = aws_sdk_s3::config::Builder::from(&shared)
                        .force_path_style(config.force_path_style);
                    if let Some(endpoint) = &config.endpoint_url {
                        builder = builder.endpoint_url(endpoint);
                    }
                    Client::from_conf(builder.build())
                }
            })
            .await
            .clone()
    }
}

// ── Pure helpers ─────────────────────────────────────────────────────────────

/// Return a trimmed, non-empty view of an optional string.
fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

/// The default public base URL for a Tigris-hosted bucket + key prefix
/// (`https://{bucket}.t3.tigrisfiles.io/{prefix}`).
#[must_use]
pub fn default_tigris_public_base(bucket: &str, key_prefix: &str) -> String {
    join_url(
        &format!("https://{bucket}.t3.tigrisfiles.io"),
        key_prefix.trim_matches('/'),
    )
}

/// Join an object-key prefix and a key, slash-trimming both. An empty prefix
/// yields the slash-trimmed key alone.
#[must_use]
pub fn join_key_prefix(prefix: &str, key: &str) -> String {
    let prefix = prefix.trim_matches('/');
    let key = key.trim_matches('/');
    if prefix.is_empty() {
        key.to_owned()
    } else {
        format!("{prefix}/{key}")
    }
}

/// Reject a caller key whose path carries a dot-only or empty segment.
///
/// After trimming outer slashes (the same normalization [`join_key_prefix`] /
/// [`MediaStorage::resolved_key`] apply), every `/`-split segment must be
/// non-empty and neither `.` nor `..` once trimmed. This is the boundary check
/// behind [`MediaStorage::persist_file`]: percent-encoding a dot-only segment
/// (as [`encode_key_for_url`] does defensively) is insufficient on its own
/// because WHATWG URL parsers normalize `%2E` back to `.` before the request,
/// so `clips/../other.mp4` could address a different object than the one stored.
/// Refusing the key here means such an object is never created or advertised.
///
/// # Errors
///
/// Returns [`MediaError::InvalidKeySegment`] naming the offending key (a
/// caller object key, not a secret).
pub(crate) fn reject_dot_only_segments(key: &str) -> Result<(), MediaError> {
    for segment in key.trim_matches('/').split('/') {
        let segment = segment.trim();
        if segment.is_empty() || segment == "." || segment == ".." {
            return Err(MediaError::InvalidKeySegment {
                key: key.to_owned(),
            });
        }
    }
    Ok(())
}

/// The inverse of [`join_key_prefix`]: strip a leading `{prefix}/` from a stored
/// object key, yielding the caller-relative key. Both ends are slash-trimmed.
///
/// The prefix is only stripped on a path-segment boundary — a stored key of
/// `media-extra/x` is **not** stripped by the prefix `media` — so a coincidental
/// textual prefix can never corrupt the returned key. An empty prefix, or a key
/// that does not carry the prefix, returns the slash-trimmed key unchanged.
#[must_use]
pub fn strip_key_prefix(prefix: &str, stored_key: &str) -> String {
    let prefix = prefix.trim_matches('/');
    let stored_key = stored_key.trim_matches('/');
    if prefix.is_empty() {
        return stored_key.to_owned();
    }
    if let Some(rest) = stored_key.strip_prefix(prefix) {
        // Only a boundary match counts: `rest` is empty (key == prefix) or the
        // next char is the `/` separator.
        if rest.is_empty() {
            return String::new();
        }
        if let Some(after) = rest.strip_prefix('/') {
            return after.trim_matches('/').to_owned();
        }
    }
    stored_key.to_owned()
}

/// Percent-encode each path segment of a storage key for safe inclusion in a
/// browser-facing URL, preserving `/` separators. Unreserved RFC 3986 chars
/// (`A-Z a-z 0-9 - . _ ~`) pass through; everything else (e.g. `#`, `?`, `%`,
/// space) is percent-encoded so the URL addresses exactly the stored object.
///
/// A whole-segment `.` or `..` is special-cased: while an ordinary filename dot
/// (`my-clip_1.mp4`) stays readable, leaving a bare `.`/`..` segment literal
/// would let a browser/proxy path-normalize `clips/../other.mp4` down to
/// `other.mp4` *before* the request — addressing a different object than the
/// stored key. Those dot-only segments therefore have their dots percent-encoded
/// (`.` → `%2E`, `..` → `%2E%2E`) so the URL survives normalization intact.
fn encode_key_for_url(key: &str) -> String {
    use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
    // NON_ALPHANUMERIC encodes everything except ASCII alphanumerics; add back
    // the RFC 3986 unreserved marks so `.mp4`, `-`, `_`, `~` stay readable.
    const SEGMENT: &AsciiSet = &NON_ALPHANUMERIC
        .remove(b'-')
        .remove(b'.')
        .remove(b'_')
        .remove(b'~');
    key.split('/')
        .map(|seg| {
            if seg == "." || seg == ".." {
                // Encode the dot(s) of a dot-only segment so path normalization
                // can't collapse the URL to a different object.
                seg.replace('.', "%2E")
            } else {
                utf8_percent_encode(seg, SEGMENT).to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// Join a base URL and a path with exactly one separating slash. An empty path
/// yields the trailing-slash-trimmed base alone.
#[must_use]
pub fn join_url(base: &str, path: &str) -> String {
    let base = base.trim_end_matches('/');
    let path = path.trim_start_matches('/');
    if path.is_empty() {
        base.to_owned()
    } else {
        format!("{base}/{path}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{MediaStorageBackend, MediaStorageConfig};

    fn s3_config(bucket: Option<&str>) -> MediaStorageConfig {
        MediaStorageConfig {
            backend: MediaStorageBackend::S3,
            bucket: bucket.map(ToOwned::to_owned),
            access_key_id: Some("AKIA".to_owned()),
            secret_access_key: Some("secret".to_owned()),
            ..MediaStorageConfig::default()
        }
    }

    // ── join_url ────────────────────────────────────────────────────────────

    #[test]
    fn join_url_joins_base_and_path() {
        assert_eq!(
            join_url("https://cdn.example.com", "a/b.mp4"),
            "https://cdn.example.com/a/b.mp4"
        );
    }

    #[test]
    fn join_url_collapses_extra_slashes() {
        assert_eq!(
            join_url("https://cdn.example.com/", "/a/b.mp4"),
            "https://cdn.example.com/a/b.mp4"
        );
    }

    #[test]
    fn join_url_empty_path_returns_trimmed_base() {
        assert_eq!(
            join_url("https://cdn.example.com/", ""),
            "https://cdn.example.com"
        );
    }

    // ── join_key_prefix ──────────────────────────────────────────────────────

    #[test]
    fn join_key_prefix_prepends_prefix() {
        assert_eq!(join_key_prefix("media", "clips/1.mp4"), "media/clips/1.mp4");
    }

    #[test]
    fn join_key_prefix_empty_prefix_trims_key() {
        assert_eq!(join_key_prefix("", "/clips/1.mp4/"), "clips/1.mp4");
    }

    #[test]
    fn join_key_prefix_trims_surrounding_slashes() {
        assert_eq!(
            join_key_prefix("/media/", "/clips/1.mp4"),
            "media/clips/1.mp4"
        );
    }

    // ── default_tigris_public_base ───────────────────────────────────────────

    #[test]
    fn tigris_public_base_uses_bucket_and_prefix() {
        assert_eq!(
            default_tigris_public_base("my-bucket", "highlights"),
            "https://my-bucket.t3.tigrisfiles.io/highlights"
        );
    }

    #[test]
    fn tigris_public_base_empty_prefix_omits_trailing_slash() {
        assert_eq!(
            default_tigris_public_base("my-bucket", ""),
            "https://my-bucket.t3.tigrisfiles.io"
        );
    }

    // ── object_key ───────────────────────────────────────────────────────────

    #[test]
    fn object_key_local_passthrough() {
        let storage = MediaStorage::Local {
            root: PathBuf::from("media"),
            public_base_url: "/media".to_owned(),
        };
        assert_eq!(storage.object_key("clips/1.mp4"), "clips/1.mp4");
    }

    #[test]
    fn object_key_s3_prefix_join() {
        let storage = MediaStorage::from_config(&s3_config(Some("b"))).unwrap();
        // default key_prefix is "media".
        assert_eq!(storage.object_key("clips/1.mp4"), "media/clips/1.mp4");
    }

    #[test]
    fn object_key_s3_empty_prefix() {
        let mut cfg = s3_config(Some("b"));
        cfg.key_prefix = String::new();
        let storage = MediaStorage::from_config(&cfg).unwrap();
        assert_eq!(storage.object_key("/clips/1.mp4/"), "clips/1.mp4");
    }

    // ── public_url ───────────────────────────────────────────────────────────

    #[test]
    fn public_url_local_joins_base() {
        let storage = MediaStorage::Local {
            root: PathBuf::from("media"),
            public_base_url: "/media".to_owned(),
        };
        assert_eq!(storage.public_url("clips/1.mp4"), "/media/clips/1.mp4");
    }

    #[test]
    fn public_url_s3_joins_public_base() {
        let mut cfg = s3_config(Some("b"));
        cfg.public_base_url = Some("https://cdn.example.com/media/".to_owned());
        let storage = MediaStorage::from_config(&cfg).unwrap();
        assert_eq!(
            storage.public_url("/clips/1.mp4"),
            "https://cdn.example.com/media/clips/1.mp4"
        );
    }

    #[test]
    fn public_url_percent_encodes_reserved_char() {
        let storage = MediaStorage::Local {
            root: PathBuf::from("media"),
            public_base_url: "/media".to_owned(),
        };
        let url = storage.public_url("clips/a#b.mp4");
        // The `#` is encoded so the browser fetches the object, not a fragment.
        assert_eq!(url, "/media/clips/a%23b.mp4");
        assert!(!url.contains("a#b"), "reserved `#` must be encoded: {url}");
        // `/` separators are preserved.
        assert!(url.contains("/clips/"), "path separators preserved: {url}");
    }

    #[test]
    fn public_url_passes_through_unreserved_chars() {
        let storage = MediaStorage::Local {
            root: PathBuf::from("media"),
            public_base_url: "/media".to_owned(),
        };
        let url = storage.public_url("highlights/my-clip_1.mp4");
        // Unreserved RFC 3986 chars (`.`, `-`, `_`) are untouched — no encoding.
        assert_eq!(url, "/media/highlights/my-clip_1.mp4");
        assert!(
            !url.contains('%'),
            "no percent-encoding for slug key: {url}"
        );
    }

    #[test]
    fn public_url_encodes_space_question_and_percent() {
        let storage = MediaStorage::Local {
            root: PathBuf::from("media"),
            public_base_url: "/media".to_owned(),
        };
        assert_eq!(
            storage.public_url("clips/a b?c%d.mp4"),
            "/media/clips/a%20b%3Fc%25d.mp4"
        );
    }

    #[test]
    fn public_url_encodes_dotdot_segment() {
        let storage = MediaStorage::Local {
            root: PathBuf::from("media"),
            public_base_url: "/media".to_owned(),
        };
        let url = storage.public_url("clips/../other.mp4");
        // The `..` segment is encoded so a browser/proxy can't normalize the URL
        // down to `/media/other.mp4` and address a different object.
        assert_eq!(url, "/media/clips/%2E%2E/other.mp4");
        assert!(
            !url.contains("/../"),
            "literal `..` traversal must not survive: {url}"
        );
    }

    #[test]
    fn public_url_encodes_single_dot_segment() {
        let storage = MediaStorage::Local {
            root: PathBuf::from("media"),
            public_base_url: "/media".to_owned(),
        };
        let url = storage.public_url("clips/./a.mp4");
        // The whole-segment `.` is encoded; the filename dot in `a.mp4` is not.
        assert_eq!(url, "/media/clips/%2E/a.mp4");
        assert!(
            !url.contains("/./"),
            "literal `.` current-dir segment must not survive: {url}"
        );
    }

    #[test]
    fn encode_key_for_url_preserves_ordinary_filename_dot() {
        // A normal filename dot is untouched — only whole-segment `.`/`..` are
        // encoded.
        assert_eq!(encode_key_for_url("clips/a.mp4"), "clips/a.mp4");
        assert!(!encode_key_for_url("clips/a.mp4").contains("%2E"));
        // And the dot-only segments still encode.
        assert_eq!(
            encode_key_for_url("clips/../other.mp4"),
            "clips/%2E%2E/other.mp4"
        );
        assert_eq!(
            encode_key_for_url("clips/./other.mp4"),
            "clips/%2E/other.mp4"
        );
    }

    // ── storage_uri ──────────────────────────────────────────────────────────

    #[test]
    fn storage_uri_local_is_path() {
        let storage = MediaStorage::Local {
            root: PathBuf::from("media"),
            public_base_url: "/media".to_owned(),
        };
        assert_eq!(
            storage.storage_uri("clips/1.mp4", Path::new("/tmp/staged/1.mp4")),
            "/tmp/staged/1.mp4"
        );
    }

    #[test]
    fn storage_uri_s3_is_s3_uri_with_prefix() {
        let storage = MediaStorage::from_config(&s3_config(Some("my-bucket"))).unwrap();
        assert_eq!(
            storage.storage_uri("clips/1.mp4", Path::new("/tmp/staged/1.mp4")),
            "s3://my-bucket/media/clips/1.mp4"
        );
    }

    // ── parse_s3_uri_key ─────────────────────────────────────────────────────

    #[test]
    fn parse_s3_uri_key_strips_prefix_to_caller_relative() {
        // The stored key embedded in the URI is prefix-inclusive; parse strips
        // the configured `key_prefix` ("media") back off so the returned key is
        // caller-relative and safe to re-feed to remove_object_if_present.
        let storage = MediaStorage::from_config(&s3_config(Some("bucket"))).unwrap();
        assert_eq!(
            storage.parse_s3_uri_key("s3://bucket/media/clips/1.mp4"),
            Some("clips/1.mp4".to_owned())
        );
    }

    #[test]
    fn parse_s3_uri_key_empty_prefix_returns_full_key() {
        let mut cfg = s3_config(Some("bucket"));
        cfg.key_prefix = String::new();
        let storage = MediaStorage::from_config(&cfg).unwrap();
        assert_eq!(
            storage.parse_s3_uri_key("s3://bucket/a/b/c.jpg"),
            Some("a/b/c.jpg".to_owned())
        );
    }

    #[test]
    fn parse_s3_uri_key_foreign_prefix_left_intact() {
        // A stored key that does not carry the configured prefix (nor a
        // coincidental textual match) is returned unchanged.
        let storage = MediaStorage::from_config(&s3_config(Some("bucket"))).unwrap();
        assert_eq!(
            storage.parse_s3_uri_key("s3://bucket/media-extra/x.jpg"),
            Some("media-extra/x.jpg".to_owned())
        );
    }

    #[test]
    fn parse_s3_uri_key_foreign_bucket_is_none() {
        // A URI naming a DIFFERENT bucket must not yield a key: handing it to
        // remove_object_if_present would delete an unrelated object out of the
        // currently-configured bucket.
        let storage = MediaStorage::from_config(&s3_config(Some("bucket"))).unwrap();
        assert_eq!(
            storage.parse_s3_uri_key("s3://other-bucket/media/clips/1.mp4"),
            None
        );
        // The matching-bucket URI still round-trips to the caller-relative key.
        assert_eq!(
            storage.parse_s3_uri_key("s3://bucket/media/clips/1.mp4"),
            Some("clips/1.mp4".to_owned())
        );
    }

    #[test]
    fn parse_s3_uri_key_prefix_only_is_none() {
        // A URI whose whole key is exactly the prefix has no caller-relative
        // key, so it is None (callers never delete an empty key).
        let storage = MediaStorage::from_config(&s3_config(Some("bucket"))).unwrap();
        assert_eq!(storage.parse_s3_uri_key("s3://bucket/media"), None);
    }

    #[test]
    fn parse_s3_uri_key_empty_key_is_none() {
        let storage = MediaStorage::from_config(&s3_config(Some("bucket"))).unwrap();
        assert_eq!(storage.parse_s3_uri_key("s3://bucket/"), None);
    }

    #[test]
    fn parse_s3_uri_key_non_s3_is_none() {
        let storage = MediaStorage::from_config(&s3_config(Some("bucket"))).unwrap();
        assert_eq!(storage.parse_s3_uri_key("/local/path/1.mp4"), None);
        assert_eq!(storage.parse_s3_uri_key("https://x/y"), None);
    }

    // ── contract round-trip: persist ⇄ storage_uri ⇄ parse ⇄ remove ──────────

    #[test]
    fn key_resolution_is_consistent_across_object_key_storage_uri_and_parse() {
        // With key_prefix = "media", a caller key `clips/a.mp4` must resolve to
        // `media/clips/a.mp4` EVERYWHERE, and the parse→remove path must target
        // that same key — never a double-prefixed `media/media/clips/a.mp4`.
        let storage = MediaStorage::from_config(&s3_config(Some("bucket"))).unwrap();
        let caller_key = "clips/a.mp4";

        // object_key and the s3:// URI embed the same resolved key.
        assert_eq!(storage.object_key(caller_key), "media/clips/a.mp4");
        let uri = storage.storage_uri(caller_key, Path::new("/tmp/staged/a.mp4"));
        assert_eq!(uri, "s3://bucket/media/clips/a.mp4");

        // Parsing the URI yields the ORIGINAL caller-relative key back.
        let parsed = storage.parse_s3_uri_key(&uri).unwrap();
        assert_eq!(parsed, caller_key);

        // Re-resolving the parsed key targets the SAME object (no double prefix).
        assert_eq!(storage.object_key(&parsed), "media/clips/a.mp4");
        assert_ne!(storage.object_key(&parsed), "media/media/clips/a.mp4");
    }

    #[test]
    fn key_resolution_round_trips_with_empty_prefix() {
        let mut cfg = s3_config(Some("bucket"));
        cfg.key_prefix = String::new();
        let storage = MediaStorage::from_config(&cfg).unwrap();
        let caller_key = "clips/a.mp4";
        let uri = storage.storage_uri(caller_key, Path::new("/tmp/a.mp4"));
        assert_eq!(uri, "s3://bucket/clips/a.mp4");
        assert_eq!(storage.parse_s3_uri_key(&uri).as_deref(), Some(caller_key));
    }

    #[test]
    fn public_url_and_stored_key_agree_on_trailing_slash() {
        // A caller key with a trailing slash must be advertised at the SAME URL
        // the stored key resolves to — never `…/clips/a.mp4/` (a 404) while the
        // object is stored at `…/clips/a.mp4`.
        let mut cfg = s3_config(Some("bucket"));
        cfg.public_base_url = Some("https://cdn.example.com/media/".to_owned());
        let storage = MediaStorage::from_config(&cfg).unwrap();
        // Stored key trims the trailing slash.
        assert_eq!(storage.object_key("clips/a.mp4/"), "media/clips/a.mp4");
        // Public URL trims it identically — no advertised trailing slash.
        assert_eq!(
            storage.public_url("clips/a.mp4/"),
            "https://cdn.example.com/media/clips/a.mp4"
        );
        // And a leading slash is handled the same way.
        assert_eq!(
            storage.public_url("/clips/a.mp4/"),
            "https://cdn.example.com/media/clips/a.mp4"
        );
    }

    #[test]
    fn public_url_local_trailing_slash_matches_object_key() {
        let storage = MediaStorage::Local {
            root: PathBuf::from("media"),
            public_base_url: "/media".to_owned(),
        };
        assert_eq!(storage.object_key("clips/a.mp4/"), "clips/a.mp4");
        assert_eq!(storage.public_url("clips/a.mp4/"), "/media/clips/a.mp4");
    }

    // ── strip_key_prefix ─────────────────────────────────────────────────────

    #[test]
    fn strip_key_prefix_removes_boundary_prefix() {
        assert_eq!(
            strip_key_prefix("media", "media/clips/1.mp4"),
            "clips/1.mp4"
        );
        assert_eq!(
            strip_key_prefix("/media/", "media/clips/1.mp4"),
            "clips/1.mp4"
        );
    }

    #[test]
    fn strip_key_prefix_leaves_non_boundary_and_foreign_keys() {
        // Coincidental textual prefix is not a path boundary.
        assert_eq!(strip_key_prefix("media", "media-extra/x"), "media-extra/x");
        // Foreign key without the prefix is unchanged.
        assert_eq!(strip_key_prefix("media", "other/x"), "other/x");
    }

    #[test]
    fn strip_key_prefix_empty_prefix_trims_only() {
        assert_eq!(strip_key_prefix("", "/clips/1.mp4/"), "clips/1.mp4");
    }

    #[test]
    fn strip_key_prefix_key_equal_to_prefix_is_empty() {
        assert_eq!(strip_key_prefix("media", "media"), "");
    }

    // ── reject_dot_only_segments ─────────────────────────────────────────────

    #[test]
    fn reject_dot_only_segments_rejects_dotdot() {
        assert!(matches!(
            reject_dot_only_segments("clips/../other.mp4"),
            Err(MediaError::InvalidKeySegment { .. })
        ));
    }

    #[test]
    fn reject_dot_only_segments_rejects_single_dot() {
        assert!(matches!(
            reject_dot_only_segments("clips/./a.mp4"),
            Err(MediaError::InvalidKeySegment { .. })
        ));
    }

    #[test]
    fn reject_dot_only_segments_accepts_filename_with_dots() {
        // Dots *inside* a segment (an ordinary filename) are fine.
        assert!(reject_dot_only_segments("clips/my.file.mp4").is_ok());
        // A normal nested key round-trips fine.
        assert!(reject_dot_only_segments("highlights/chan/a-b_c.mp4").is_ok());
    }

    #[test]
    fn reject_dot_only_segments_trims_outer_slashes_but_rejects_inner_empties() {
        // Leading/trailing slashes are trimmed (as resolved_key would), so a
        // key with only outer slashes is accepted.
        assert!(reject_dot_only_segments("/clips/a.mp4/").is_ok());
        // An internal empty segment (`//`) is rejected.
        assert!(reject_dot_only_segments("clips//a.mp4").is_err());
        // A whitespace-padded dot-only segment is rejected.
        assert!(reject_dot_only_segments("clips/ .. /a.mp4").is_err());
        // An empty key has no legitimate object to name.
        assert!(reject_dot_only_segments("").is_err());
    }

    #[tokio::test]
    async fn persist_file_rejects_dot_only_segment_key() {
        let storage = MediaStorage::Local {
            root: PathBuf::from("media"),
            public_base_url: "/media".to_owned(),
        };
        let err = storage
            .persist_file("clips/../secret.mp4", Path::new("/tmp/staged/x.mp4"))
            .await
            .expect_err("dot-only-segment key must be rejected at the boundary");
        assert!(matches!(err, MediaError::InvalidKeySegment { .. }));
    }

    #[tokio::test]
    async fn persist_file_accepts_ordinary_dotted_filename() {
        // The local backend records the path without I/O, so this exercises the
        // validation boundary while accepting an ordinary dotted filename.
        let storage = MediaStorage::Local {
            root: PathBuf::from("media"),
            public_base_url: "/media".to_owned(),
        };
        let stored = storage
            .persist_file("clips/my.file.mp4", Path::new("/tmp/staged/my.file.mp4"))
            .await
            .expect("ordinary dotted filename must be accepted");
        assert_eq!(stored.key, "clips/my.file.mp4");
    }

    // ── from_config ──────────────────────────────────────────────────────────

    #[test]
    fn from_config_local_default() {
        let storage = MediaStorage::from_config(&MediaStorageConfig::default()).unwrap();
        match storage {
            MediaStorage::Local {
                root,
                public_base_url,
            } => {
                assert_eq!(root, PathBuf::from("media"));
                assert_eq!(public_base_url, "/media");
            }
            MediaStorage::S3(_) => panic!("expected local backend"),
        }
        assert_eq!(
            MediaStorage::from_config(&MediaStorageConfig::default())
                .unwrap()
                .backend_name(),
            "local"
        );
    }

    #[test]
    fn from_config_s3_with_bucket_no_region_and_tigris_base() {
        let storage = MediaStorage::from_config(&s3_config(Some("my-bucket"))).unwrap();
        match storage {
            MediaStorage::S3(config) => {
                assert_eq!(config.bucket, "my-bucket");
                // blank region stays None on the generic path (ambient-resolved);
                // no `auto` default here — that is Tigris/arroyo-only.
                assert_eq!(config.region, None);
                // unset endpoint stays None (SDK default)
                assert_eq!(config.endpoint_url, None);
                // unset public base → tigris fallback using the "media" prefix
                assert_eq!(
                    config.public_base_url,
                    "https://my-bucket.t3.tigrisfiles.io/media"
                );
            }
            MediaStorage::Local { .. } => panic!("expected s3 backend"),
        }
    }

    #[test]
    fn from_config_s3_respects_explicit_region_endpoint_public_base() {
        let mut cfg = s3_config(Some("b"));
        cfg.region = Some("us-east-1".to_owned());
        cfg.endpoint_url = Some("https://example.r2.cloudflarestorage.com".to_owned());
        cfg.public_base_url = Some("https://cdn.example.com".to_owned());
        let MediaStorage::S3(config) = MediaStorage::from_config(&cfg).unwrap() else {
            panic!("expected s3 backend");
        };
        assert_eq!(config.region.as_deref(), Some("us-east-1"));
        assert_eq!(
            config.endpoint_url.as_deref(),
            Some("https://example.r2.cloudflarestorage.com")
        );
        assert_eq!(config.public_base_url, "https://cdn.example.com");
    }

    #[test]
    fn from_config_s3_no_region_yields_none() {
        // The generic S3 path leaves region unset (`None`) so the ambient AWS
        // chain (AWS_REGION / profile / IMDS) resolves it — a generic AWS
        // deployment on the SDK default endpoint is not signed for `auto`.
        let MediaStorage::S3(config) = MediaStorage::from_config(&s3_config(Some("b"))).unwrap()
        else {
            panic!("expected s3 backend");
        };
        assert_eq!(config.region, None);
    }

    #[test]
    fn from_config_s3_threads_explicit_auto_region() {
        // The arroyo/Tigris shim resolves region to `auto` in the config; a
        // `Some("auto")` region must be threaded through verbatim (only the
        // no-region generic path yields `None`).
        let mut cfg = s3_config(Some("b"));
        cfg.region = Some("auto".to_owned());
        let MediaStorage::S3(config) = MediaStorage::from_config(&cfg).unwrap() else {
            panic!("expected s3 backend");
        };
        assert_eq!(config.region.as_deref(), Some("auto"));
    }

    #[test]
    fn from_config_s3_missing_bucket_errors() {
        assert!(matches!(
            MediaStorage::from_config(&s3_config(None)),
            Err(MediaError::MissingBucket)
        ));
        // blank bucket is also missing.
        assert!(matches!(
            MediaStorage::from_config(&s3_config(Some("   "))),
            Err(MediaError::MissingBucket)
        ));
    }

    #[test]
    fn from_config_s3_partial_credentials_errors() {
        let mut cfg = s3_config(Some("b"));
        cfg.secret_access_key = None;
        assert!(matches!(
            MediaStorage::from_config(&cfg),
            Err(MediaError::PartialS3Credentials)
        ));

        let mut cfg = s3_config(Some("b"));
        cfg.access_key_id = None;
        assert!(matches!(
            MediaStorage::from_config(&cfg),
            Err(MediaError::PartialS3Credentials)
        ));
    }

    #[test]
    fn from_config_s3_without_credentials_is_ok() {
        let mut cfg = s3_config(Some("b"));
        cfg.access_key_id = None;
        cfg.secret_access_key = None;
        let MediaStorage::S3(config) = MediaStorage::from_config(&cfg).unwrap() else {
            panic!("expected s3 backend");
        };
        assert!(config.access_key_id.is_empty());
        assert!(config.secret_access_key.is_empty());
    }

    #[test]
    fn secret_access_key_is_redacted_in_debug() {
        let mut cfg = s3_config(Some("b"));
        cfg.secret_access_key = Some("s3kr3t-value-xyz".to_owned());
        cfg.session_token = Some("session-token-value-abc".to_owned());
        let storage = MediaStorage::from_config(&cfg).unwrap();
        let rendered = format!("{storage:?}");
        assert!(rendered.contains("** redacted **"));
        // The literal secret value must never appear (the field *name*
        // `secret_access_key` legitimately contains the word "secret").
        assert!(
            !rendered.contains("s3kr3t-value-xyz"),
            "secret value must not appear: {rendered}"
        );
        // The session token is equally sensitive: its value must never appear,
        // and its `Some(_)` presence must render as `** redacted **`.
        assert!(
            !rendered.contains("session-token-value-abc"),
            "session token value must not appear: {rendered}"
        );
        assert!(
            rendered.contains("session_token: Some(\"** redacted **\")"),
            "session token must render redacted when present: {rendered}"
        );
    }

    #[test]
    fn session_token_is_none_in_debug_when_absent() {
        // An absent session token renders as `None`, never a redaction marker.
        let storage = MediaStorage::from_config(&s3_config(Some("b"))).unwrap();
        let rendered = format!("{storage:?}");
        assert!(
            rendered.contains("session_token: None"),
            "absent session token must render None: {rendered}"
        );
    }

    #[test]
    fn from_config_threads_session_token() {
        let mut cfg = s3_config(Some("b"));
        cfg.session_token = Some("temp-creds-token".to_owned());
        let MediaStorage::S3(config) = MediaStorage::from_config(&cfg).unwrap() else {
            panic!("expected s3 backend");
        };
        assert_eq!(config.session_token.as_deref(), Some("temp-creds-token"));
    }

    #[test]
    fn from_config_blank_session_token_is_none() {
        // A blank token is normalized to `None`, matching the access-key /
        // secret-key non-empty handling.
        let mut cfg = s3_config(Some("b"));
        cfg.session_token = Some("   ".to_owned());
        let MediaStorage::S3(config) = MediaStorage::from_config(&cfg).unwrap() else {
            panic!("expected s3 backend");
        };
        assert_eq!(config.session_token, None);
    }

    #[test]
    fn from_config_absent_session_token_is_none() {
        let MediaStorage::S3(config) = MediaStorage::from_config(&s3_config(Some("b"))).unwrap()
        else {
            panic!("expected s3 backend");
        };
        assert_eq!(config.session_token, None);
    }

    // ── client-builder smoke test (no network) ───────────────────────────────

    #[tokio::test]
    async fn build_client_constructs_against_fake_endpoint_without_network() {
        // Mirrors autumn-storage-s3's smoke test: constructing an S3 client
        // against an unreachable fake endpoint must succeed with no network
        // round-trip (no request is sent until an operation runs).
        let mut cfg = s3_config(Some("test-bucket"));
        cfg.region = Some("us-east-1".to_owned());
        cfg.endpoint_url = Some("http://127.0.0.1:9".to_owned());
        cfg.force_path_style = true;
        let storage = MediaStorage::from_config(&cfg).unwrap();
        // Should return a constructed client; nothing here reaches the network.
        let _client = storage.build_client().await;
    }

    #[tokio::test]
    async fn build_client_threads_explicit_region_into_client() {
        // An explicitly-configured region must be threaded into the built S3
        // client's config (e.g. Tigris `auto`, or a real AWS region).
        let mut cfg = s3_config(Some("test-bucket"));
        cfg.region = Some("eu-west-2".to_owned());
        cfg.endpoint_url = Some("http://127.0.0.1:9".to_owned());
        cfg.force_path_style = true;
        let storage = MediaStorage::from_config(&cfg).unwrap();
        let client = storage.build_client().await;
        assert_eq!(
            client.config().region().map(Region::as_ref),
            Some("eu-west-2")
        );
    }
}
