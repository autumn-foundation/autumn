//! S3-compatible [`BlobStore`] plugin for autumn-web.
//!
//! # Quick start
//!
//! ```toml
//! [dependencies]
//! autumn-web     = { version = "0.3", features = ["storage", "multipart"] }
//! autumn-storage-s3 = "0.3"
//! ```
//!
//! ```rust,ignore
//! use autumn_storage_s3::S3BlobStore;
//!
//! #[tokio::main]
//! async fn main() {
//!     let config = autumn_web::config::TomlEnvConfigLoader::new()
//!         .load()
//!         .await
//!         .expect("failed to load config");
//!
//!     let store = S3BlobStore::from_config(&config.storage.s3)
//!         .await
//!         .expect("failed to build S3 store");
//!
//!     autumn_web::app()
//!         .with_blob_store(store)
//!         .run()
//!         .await;
//! }
//! ```

use std::sync::Arc;
use std::time::Duration;

use autumn_web::storage::{
    Blob, BlobFuture, BlobMeta, BlobStore, BlobStoreError, ByteStream, PresignPutResult,
    StorageS3Config, validate_key,
};
use aws_credential_types::Credentials;
use aws_sdk_s3::Client;
use aws_sdk_s3::config::{BehaviorVersion, Region};
use aws_sdk_s3::presigning::PresigningConfig;
use bytes::Bytes;
use futures::StreamExt as _;
use thiserror::Error;

/// Errors returned during [`S3BlobStore`] construction.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum S3BlobStoreError {
    /// `[storage.s3].bucket` is not set.
    #[error("storage.s3.bucket is required but not set")]
    MissingBucket,
    /// `[storage.s3].region` is not set.
    #[error("storage.s3.region is required but not set")]
    MissingRegion,
    /// Exactly one of `access_key_id_env` / `secret_access_key_env` is set.
    /// Both must be provided together, or neither (to use the AWS default
    /// credential chain).
    #[error(
        "partial credential configuration: access_key_id_env and \
         secret_access_key_env must both be set, or neither"
    )]
    PartialCredentialConfig,
    /// A credential env var listed in config is not present in the process
    /// environment.
    #[error("credential env var '{var}' is not set in the environment")]
    MissingCredentialEnvVar {
        /// Name of the missing environment variable.
        var: String,
    },
}

#[derive(Debug, Clone)]
struct S3Options {
    provider_id: String,
    bucket: String,
}

/// S3-compatible [`BlobStore`] backed by
/// `aws-sdk-s3`.
///
/// Supports AWS S3, Cloudflare R2, `MinIO`, `DigitalOcean` Spaces, and Wasabi.
/// Construct with [`S3BlobStore::from_config`].
#[derive(Debug, Clone)]
pub struct S3BlobStore {
    client: Client,
    presign_client: Client,
    options: Arc<S3Options>,
}

impl S3BlobStore {
    /// Build an `S3BlobStore` from the `[storage.s3]` config section.
    ///
    /// Credentials resolve from `access_key_id_env` /
    /// `secret_access_key_env` when **both** are set; if neither is set the
    /// AWS default credential chain (environment variables, instance metadata,
    /// IAM roles) is used. Providing exactly one is a configuration error —
    /// [`S3BlobStoreError::PartialCredentialConfig`] is returned immediately.
    ///
    /// The [`provider_id`](BlobStore::provider_id) recorded on every produced
    /// [`Blob`] defaults to `"s3"`. Override it with
    /// [`with_provider_id`](Self::with_provider_id) if you want blobs to carry
    /// the same [`StorageConfig::default_provider`](autumn_web::storage::StorageConfig)
    /// label used by other backends in your deployment.
    ///
    /// # Errors
    ///
    /// Returns [`S3BlobStoreError`] when required config fields are absent,
    /// credential env vars are partially configured, or a listed credential
    /// env var is missing from the environment.
    pub async fn from_config(cfg: &StorageS3Config) -> Result<Self, S3BlobStoreError> {
        let bucket = cfg
            .bucket
            .as_deref()
            .filter(|b| !b.trim().is_empty())
            .ok_or(S3BlobStoreError::MissingBucket)?
            .to_owned();
        let region = cfg
            .region
            .as_deref()
            .filter(|r| !r.trim().is_empty())
            .ok_or(S3BlobStoreError::MissingRegion)?
            .to_owned();

        if cfg.access_key_id_env.is_some() != cfg.secret_access_key_env.is_some() {
            return Err(S3BlobStoreError::PartialCredentialConfig);
        }

        let presign_endpoint = cfg.public_base_url.as_deref().or(cfg.endpoint.as_deref());
        let (client, presign_client) = if let (Some(key_env), Some(secret_env)) =
            (&cfg.access_key_id_env, &cfg.secret_access_key_env)
        {
            let key =
                std::env::var(key_env).map_err(|_| S3BlobStoreError::MissingCredentialEnvVar {
                    var: key_env.clone(),
                })?;
            let secret = std::env::var(secret_env).map_err(|_| {
                S3BlobStoreError::MissingCredentialEnvVar {
                    var: secret_env.clone(),
                }
            })?;
            let creds = Credentials::new(key, secret, None, None, "autumn-storage-s3");
            let mut builder = aws_sdk_s3::Config::builder()
                .behavior_version(BehaviorVersion::latest())
                .region(Region::new(region.clone()))
                .credentials_provider(creds.clone())
                .force_path_style(cfg.force_path_style);
            if let Some(endpoint) = &cfg.endpoint {
                builder = builder.endpoint_url(endpoint);
            }
            let client = Client::from_conf(builder.build());

            let mut presign_builder = aws_sdk_s3::Config::builder()
                .behavior_version(BehaviorVersion::latest())
                .region(Region::new(region))
                .credentials_provider(creds)
                .force_path_style(cfg.force_path_style);
            if let Some(endpoint) = presign_endpoint {
                presign_builder = presign_builder.endpoint_url(endpoint);
            }
            (client, Client::from_conf(presign_builder.build()))
        } else {
            let shared = aws_config::defaults(BehaviorVersion::latest())
                .region(Region::new(region))
                .load()
                .await;
            let mut builder =
                aws_sdk_s3::config::Builder::from(&shared).force_path_style(cfg.force_path_style);
            if let Some(endpoint) = &cfg.endpoint {
                builder = builder.endpoint_url(endpoint);
            }
            let client = Client::from_conf(builder.build());

            let mut presign_builder =
                aws_sdk_s3::config::Builder::from(&shared).force_path_style(cfg.force_path_style);
            if let Some(endpoint) = presign_endpoint {
                presign_builder = presign_builder.endpoint_url(endpoint);
            }
            (client, Client::from_conf(presign_builder.build()))
        };

        Ok(Self {
            client,
            presign_client,
            options: Arc::new(S3Options {
                provider_id: "s3".to_owned(),
                bucket,
            }),
        })
    }

    /// Override the `provider_id` recorded on every [`Blob`] produced by
    /// this store.
    ///
    /// Defaults to `"s3"`. Call this if you want blobs to carry the same
    /// [`StorageConfig::default_provider`](autumn_web::storage::StorageConfig)
    /// label used by other backends, so backend migrations appear as
    /// provider mismatches in existing blob records.
    ///
    /// ```rust,ignore
    /// let store = S3BlobStore::from_config(&config.storage.s3)
    ///     .await?
    ///     .with_provider_id(&config.storage.default_provider);
    /// ```
    #[must_use]
    pub fn with_provider_id(mut self, provider_id: impl Into<String>) -> Self {
        Arc::make_mut(&mut self.options).provider_id = provider_id.into();
        self
    }
}

/// Minimum part size for S3 multipart uploads (5 MiB — the S3 minimum).
///
/// Streams smaller than this threshold are uploaded as a single `PutObject`
/// call; larger streams are chunked into multipart uploads so the payload is
/// never fully buffered in memory.
const MULTIPART_PART_SIZE: usize = 5 * 1024 * 1024;

/// Best-effort abort of a multipart upload; errors are silently ignored
/// because we're already on the failure path.
async fn abort_multipart(client: &Client, bucket: &str, key: &str, upload_id: &str) {
    let _ = client
        .abort_multipart_upload()
        .bucket(bucket)
        .key(key)
        .upload_id(upload_id)
        .send()
        .await;
}

impl BlobStore for S3BlobStore {
    fn provider_id(&self) -> &str {
        &self.options.provider_id
    }

    fn put<'a>(
        &'a self,
        key: &'a str,
        content_type: &'a str,
        bytes: Bytes,
    ) -> BlobFuture<'a, Blob> {
        let byte_size = bytes.len() as u64;
        Box::pin(async move {
            validate_key(key)?;
            let result = self
                .client
                .put_object()
                .bucket(&self.options.bucket)
                .key(key)
                .content_type(content_type)
                .body(bytes.into())
                .send()
                .await
                .map_err(|e| BlobStoreError::backend(e.to_string()))?;
            let etag = result.e_tag().map(str::to_owned);
            let mut blob = Blob::new(&self.options.provider_id, key, content_type, byte_size);
            if let Some(e) = etag {
                blob = blob.with_etag(e);
            }
            Ok(blob)
        })
    }

    #[allow(clippy::too_many_lines)]
    fn put_stream<'a>(
        &'a self,
        key: &'a str,
        content_type: &'a str,
        data: ByteStream<'a>,
    ) -> BlobFuture<'a, Blob> {
        Box::pin(async move {
            validate_key(key)?;
            let mut stream = data;
            let mut current_part: Vec<u8> = Vec::with_capacity(MULTIPART_PART_SIZE);

            // Buffer until we have one full part or the stream ends.
            loop {
                match stream.next().await {
                    Some(Ok(chunk)) => {
                        current_part.extend_from_slice(&chunk);
                        if current_part.len() >= MULTIPART_PART_SIZE {
                            break;
                        }
                    }
                    Some(Err(e)) => return Err(e),
                    None => {
                        // Stream ended before reaching the multipart threshold:
                        // use a single-object PUT instead.
                        return self.put(key, content_type, Bytes::from(current_part)).await;
                    }
                }
            }

            // The stream exceeded MULTIPART_PART_SIZE — use S3 multipart upload
            // so the payload is never fully buffered in memory.
            let create = self
                .client
                .create_multipart_upload()
                .bucket(&self.options.bucket)
                .key(key)
                .content_type(content_type)
                .send()
                .await
                .map_err(|e| BlobStoreError::backend(e.to_string()))?;

            let upload_id = create
                .upload_id()
                .ok_or_else(|| {
                    BlobStoreError::backend("CreateMultipartUpload returned no upload_id")
                })?
                .to_owned();

            let mut completed_parts: Vec<aws_sdk_s3::types::CompletedPart> = Vec::new();
            let mut part_number = 1_i32;
            let mut total_bytes: u64 = 0;

            // Upload loop. `current_part` is always non-empty at the top.
            loop {
                if part_number > 10_000 {
                    abort_multipart(&self.client, &self.options.bucket, key, &upload_id).await;
                    return Err(BlobStoreError::PayloadTooLarge(
                        "S3 multipart upload part limit (10,000) exceeded; \
                         object is too large to upload in 5 MiB chunks"
                            .into(),
                    ));
                }

                let part_bytes = Bytes::from(std::mem::take(&mut current_part));
                total_bytes += part_bytes.len() as u64;

                let upload_result = self
                    .client
                    .upload_part()
                    .bucket(&self.options.bucket)
                    .key(key)
                    .upload_id(&upload_id)
                    .part_number(part_number)
                    .body(part_bytes.into())
                    .send()
                    .await;

                let upload_resp = match upload_result {
                    Ok(r) => r,
                    Err(e) => {
                        abort_multipart(&self.client, &self.options.bucket, key, &upload_id).await;
                        return Err(BlobStoreError::backend(e.to_string()));
                    }
                };

                completed_parts.push(
                    aws_sdk_s3::types::CompletedPart::builder()
                        .part_number(part_number)
                        .set_e_tag(upload_resp.e_tag().map(str::to_owned))
                        .build(),
                );
                part_number += 1;

                // Refill the part buffer from the remaining stream.
                loop {
                    match stream.next().await {
                        Some(Ok(chunk)) => {
                            current_part.extend_from_slice(&chunk);
                            if current_part.len() >= MULTIPART_PART_SIZE {
                                break;
                            }
                        }
                        Some(Err(e)) => {
                            abort_multipart(&self.client, &self.options.bucket, key, &upload_id)
                                .await;
                            return Err(e);
                        }
                        None => break,
                    }
                }

                if current_part.is_empty() {
                    break;
                }
            }

            let complete_result = self
                .client
                .complete_multipart_upload()
                .bucket(&self.options.bucket)
                .key(key)
                .upload_id(&upload_id)
                .multipart_upload(
                    aws_sdk_s3::types::CompletedMultipartUpload::builder()
                        .set_parts(Some(completed_parts))
                        .build(),
                )
                .send()
                .await;

            match complete_result {
                Ok(resp) => {
                    let mut blob =
                        Blob::new(&self.options.provider_id, key, content_type, total_bytes);
                    if let Some(etag) = resp.e_tag() {
                        blob = blob.with_etag(etag.to_owned());
                    }
                    Ok(blob)
                }
                Err(e) => {
                    abort_multipart(&self.client, &self.options.bucket, key, &upload_id).await;
                    Err(BlobStoreError::backend(e.to_string()))
                }
            }
        })
    }

    fn get<'a>(&'a self, key: &'a str) -> BlobFuture<'a, Bytes> {
        Box::pin(async move {
            validate_key(key)?;
            let result = self
                .client
                .get_object()
                .bucket(&self.options.bucket)
                .key(key)
                .send()
                .await
                .map_err(|e| {
                    if let aws_sdk_s3::error::SdkError::ServiceError(ref svc) = e
                        && (svc.err().is_no_such_key() || svc.raw().status().as_u16() == 404)
                    {
                        return BlobStoreError::NotFound(key.to_owned());
                    }
                    BlobStoreError::backend(e.to_string())
                })?;
            let body = result
                .body
                .collect()
                .await
                .map_err(|e| BlobStoreError::io(e.to_string()))?;
            Ok(body.into_bytes())
        })
    }

    fn delete<'a>(&'a self, key: &'a str) -> BlobFuture<'a, ()> {
        Box::pin(async move {
            validate_key(key)?;
            self.client
                .delete_object()
                .bucket(&self.options.bucket)
                .key(key)
                .send()
                .await
                .map_err(|e| BlobStoreError::backend(e.to_string()))?;
            Ok(())
        })
    }

    fn head<'a>(&'a self, key: &'a str) -> BlobFuture<'a, Option<BlobMeta>> {
        Box::pin(async move {
            validate_key(key)?;
            let result = self
                .client
                .head_object()
                .bucket(&self.options.bucket)
                .key(key)
                .send()
                .await;
            match result {
                Ok(resp) => {
                    let content_type = resp
                        .content_type()
                        .unwrap_or("application/octet-stream")
                        .to_owned();
                    let byte_size = resp
                        .content_length()
                        .and_then(|n| u64::try_from(n).ok())
                        .unwrap_or(0);
                    let etag = resp.e_tag().map(str::to_owned);
                    Ok(Some(BlobMeta {
                        key: key.to_owned(),
                        content_type,
                        byte_size,
                        etag,
                    }))
                }
                Err(e) => {
                    if let aws_sdk_s3::error::SdkError::ServiceError(ref svc) = e
                        && (svc.err().is_not_found() || svc.raw().status().as_u16() == 404)
                    {
                        return Ok(None);
                    }
                    Err(BlobStoreError::backend(e.to_string()))
                }
            }
        })
    }

    fn presigned_url<'a>(&'a self, key: &'a str, expires_in: Duration) -> BlobFuture<'a, String> {
        Box::pin(async move {
            validate_key(key)?;
            let presigning = PresigningConfig::expires_in(expires_in)
                .map_err(|e| BlobStoreError::backend(e.to_string()))?;
            let req = self
                .presign_client
                .get_object()
                .bucket(&self.options.bucket)
                .key(key)
                .presigned(presigning)
                .await
                .map_err(|e| BlobStoreError::backend(e.to_string()))?;
            Ok(req.uri().to_string())
        })
    }

    fn presign_put<'a>(
        &'a self,
        key: &'a str,
        content_type: &'a str,
        expires_in: Duration,
    ) -> BlobFuture<'a, PresignPutResult> {
        Box::pin(async move {
            validate_key(key)?;
            let presigning = PresigningConfig::expires_in(expires_in)
                .map_err(|e| BlobStoreError::backend(e.to_string()))?;
            let req = self
                .presign_client
                .put_object()
                .bucket(&self.options.bucket)
                .key(key)
                .content_type(content_type)
                .presigned(presigning)
                .await
                .map_err(|e| BlobStoreError::backend(e.to_string()))?;

            let headers: std::collections::HashMap<String, String> = req
                .headers()
                .map(|(k, v)| (k.to_owned(), v.to_owned()))
                .collect();

            Ok(PresignPutResult {
                url: req.uri().to_string(),
                method: "PUT".to_owned(),
                headers,
                expires_in,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;

    fn s3_config(bucket: Option<&str>, region: Option<&str>) -> StorageS3Config {
        StorageS3Config {
            bucket: bucket.map(str::to_owned),
            region: region.map(str::to_owned),
            ..StorageS3Config::default()
        }
    }

    fn query_param<'a>(url: &'a str, name: &str) -> Option<&'a str> {
        let query = url.split_once('?')?.1;
        query.split('&').find_map(|part| {
            let (key, value) = part.split_once('=')?;
            (key == name).then_some(value)
        })
    }

    fn authority(url: &str) -> Option<&str> {
        let rest = url.split_once("://")?.1;
        Some(rest.split('/').next().unwrap_or(rest))
    }

    fn parse_amz_date(value: &str) -> SystemTime {
        assert_eq!(value.len(), 16, "unexpected X-Amz-Date format");
        assert_eq!(&value[8..9], "T", "unexpected X-Amz-Date format");
        assert_eq!(&value[15..16], "Z", "unexpected X-Amz-Date format");
        let year = value[0..4].parse::<i32>().expect("year");
        let month = value[4..6].parse::<u32>().expect("month");
        let day = value[6..8].parse::<u32>().expect("day");
        let hour = value[9..11].parse::<u64>().expect("hour");
        let minute = value[11..13].parse::<u64>().expect("minute");
        let second = value[13..15].parse::<u64>().expect("second");

        let days = days_since_unix_epoch(year, month, day);
        assert!(days >= 0, "X-Amz-Date before unix epoch");
        let days = u64::try_from(days).expect("nonnegative day count");
        SystemTime::UNIX_EPOCH
            + Duration::from_secs(days * 86_400 + hour * 3_600 + minute * 60 + second)
    }

    fn days_since_unix_epoch(year: i32, month: u32, day: u32) -> i64 {
        let year = year - i32::from(month <= 2);
        let era = if year >= 0 { year } else { year - 399 } / 400;
        let year_of_era = year - era * 400;
        let month = i32::try_from(month).expect("month fits i32");
        let day = i32::try_from(day).expect("day fits i32");
        let month_prime = month + if month > 2 { -3 } else { 9 };
        let day_of_year = (153 * month_prime + 2) / 5 + day - 1;
        let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
        i64::from(era) * 146_097 + i64::from(day_of_era) - 719_468
    }

    fn test_client(endpoint: &str) -> Client {
        Client::from_conf(
            aws_sdk_s3::Config::builder()
                .behavior_version(BehaviorVersion::latest())
                .region(Region::new("us-east-1"))
                .credentials_provider(Credentials::new(
                    "AKIAIOSFODNN7EXAMPLE",
                    "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
                    None,
                    None,
                    "test",
                ))
                .force_path_style(true)
                .endpoint_url(endpoint)
                .build(),
        )
    }

    fn test_store() -> S3BlobStore {
        let client = test_client("http://127.0.0.1:9");
        S3BlobStore {
            client: client.clone(),
            presign_client: client,
            options: Arc::new(S3Options {
                provider_id: "s3".to_owned(),
                bucket: "test-bucket".to_owned(),
            }),
        }
    }

    fn assert_invalid_input<T: std::fmt::Debug>(result: Result<T, BlobStoreError>) {
        match result {
            Err(BlobStoreError::InvalidInput(_)) => {}
            other => panic!("expected InvalidInput, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn from_config_requires_bucket() {
        let cfg = s3_config(None, Some("us-east-1"));
        let err = S3BlobStore::from_config(&cfg).await.unwrap_err();
        assert!(matches!(err, S3BlobStoreError::MissingBucket));
    }

    #[tokio::test]
    async fn from_config_requires_region() {
        let cfg = s3_config(Some("my-bucket"), None);
        let err = S3BlobStore::from_config(&cfg).await.unwrap_err();
        assert!(matches!(err, S3BlobStoreError::MissingRegion));
    }

    #[tokio::test]
    async fn from_config_empty_bucket_is_rejected() {
        let cfg = s3_config(Some("   "), Some("us-east-1"));
        let err = S3BlobStore::from_config(&cfg).await.unwrap_err();
        assert!(matches!(err, S3BlobStoreError::MissingBucket));
    }

    #[tokio::test]
    async fn from_config_empty_region_is_rejected() {
        let cfg = s3_config(Some("my-bucket"), Some(""));
        let err = S3BlobStore::from_config(&cfg).await.unwrap_err();
        assert!(matches!(err, S3BlobStoreError::MissingRegion));
    }

    #[tokio::test]
    async fn from_config_missing_credential_env_var() {
        let cfg = StorageS3Config {
            bucket: Some("b".into()),
            region: Some("us-east-1".into()),
            access_key_id_env: Some("__AUTUMN_TEST_KEY_ID_DEFINITELY_NOT_SET__".into()),
            secret_access_key_env: Some("__AUTUMN_TEST_SECRET_DEFINITELY_NOT_SET__".into()),
            ..StorageS3Config::default()
        };
        let err = S3BlobStore::from_config(&cfg).await.unwrap_err();
        assert!(matches!(
            err,
            S3BlobStoreError::MissingCredentialEnvVar { .. }
        ));
    }

    #[tokio::test]
    async fn from_config_with_static_creds_sets_provider_id() {
        let cfg = StorageS3Config {
            bucket: Some("test-bucket".into()),
            region: Some("us-east-1".into()),
            access_key_id_env: Some("__AUTUMN_S3_TEST_KEY_ID__".into()),
            secret_access_key_env: Some("__AUTUMN_S3_TEST_SECRET__".into()),
            ..StorageS3Config::default()
        };
        Box::pin(temp_env::async_with_vars(
            [
                ("__AUTUMN_S3_TEST_KEY_ID__", Some("AKIAIOSFODNN7EXAMPLE")),
                (
                    "__AUTUMN_S3_TEST_SECRET__",
                    Some("wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"),
                ),
            ],
            async {
                let store = S3BlobStore::from_config(&cfg)
                    .await
                    .expect("should build with static creds");
                assert_eq!(store.provider_id(), "s3");
            },
        ))
        .await;
    }

    #[tokio::test]
    async fn from_config_with_custom_endpoint() {
        let cfg = StorageS3Config {
            bucket: Some("my-bucket".into()),
            region: Some("auto".into()),
            endpoint: Some("https://example.r2.cloudflarestorage.com".into()),
            force_path_style: true,
            access_key_id_env: Some("__AUTUMN_S3_TEST_KEY_ID2__".into()),
            secret_access_key_env: Some("__AUTUMN_S3_TEST_SECRET2__".into()),
            ..StorageS3Config::default()
        };
        Box::pin(temp_env::async_with_vars(
            [
                ("__AUTUMN_S3_TEST_KEY_ID2__", Some("key")),
                ("__AUTUMN_S3_TEST_SECRET2__", Some("secret")),
            ],
            async {
                let store = S3BlobStore::from_config(&cfg).await.expect("should build");
                assert_eq!(store.provider_id(), "s3");
            },
        ))
        .await;
    }

    #[tokio::test]
    async fn from_config_partial_key_env_is_rejected() {
        let cfg = StorageS3Config {
            bucket: Some("b".into()),
            region: Some("us-east-1".into()),
            access_key_id_env: Some("SOME_KEY_ENV".into()),
            secret_access_key_env: None,
            ..StorageS3Config::default()
        };
        let err = S3BlobStore::from_config(&cfg).await.unwrap_err();
        assert!(matches!(err, S3BlobStoreError::PartialCredentialConfig));
    }

    #[tokio::test]
    async fn from_config_partial_secret_env_is_rejected() {
        let cfg = StorageS3Config {
            bucket: Some("b".into()),
            region: Some("us-east-1".into()),
            access_key_id_env: None,
            secret_access_key_env: Some("SOME_SECRET_ENV".into()),
            ..StorageS3Config::default()
        };
        let err = S3BlobStore::from_config(&cfg).await.unwrap_err();
        assert!(matches!(err, S3BlobStoreError::PartialCredentialConfig));
    }

    #[tokio::test]
    async fn with_provider_id_overrides_default() {
        let cfg = StorageS3Config {
            bucket: Some("b".into()),
            region: Some("us-east-1".into()),
            access_key_id_env: Some("__AUTUMN_S3_PROVID_KEY__".into()),
            secret_access_key_env: Some("__AUTUMN_S3_PROVID_SECRET__".into()),
            ..StorageS3Config::default()
        };
        Box::pin(temp_env::async_with_vars(
            [
                ("__AUTUMN_S3_PROVID_KEY__", Some("key")),
                ("__AUTUMN_S3_PROVID_SECRET__", Some("secret")),
            ],
            async {
                let store = S3BlobStore::from_config(&cfg)
                    .await
                    .expect("build store")
                    .with_provider_id("myapp-prod");
                assert_eq!(store.provider_id(), "myapp-prod");
            },
        ))
        .await;
    }

    #[tokio::test]
    async fn public_base_url_is_used_for_presigning_host() {
        let cfg = StorageS3Config {
            bucket: Some("test-bucket".into()),
            region: Some("us-east-1".into()),
            endpoint: Some("https://internal.example.local".into()),
            public_base_url: Some("https://public.example.com".into()),
            force_path_style: true,
            access_key_id_env: Some("__AUTUMN_S3_PRESIGN_KEY__".into()),
            secret_access_key_env: Some("__AUTUMN_S3_PRESIGN_SECRET__".into()),
            ..StorageS3Config::default()
        };

        Box::pin(temp_env::async_with_vars(
            [
                ("__AUTUMN_S3_PRESIGN_KEY__", Some("AKIAIOSFODNN7EXAMPLE")),
                (
                    "__AUTUMN_S3_PRESIGN_SECRET__",
                    Some("wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"),
                ),
            ],
            async {
                let store = S3BlobStore::from_config(&cfg).await.expect("build store");
                let actual = store
                    .presigned_url("avatars/42.bin", Duration::from_secs(900))
                    .await
                    .expect("presign URL");

                let amz_date = query_param(&actual, "X-Amz-Date").expect("X-Amz-Date");
                let expected_presigning = PresigningConfig::builder()
                    .start_time(parse_amz_date(amz_date))
                    .expires_in(Duration::from_secs(900))
                    .build()
                    .expect("fixed presigning config");
                let expected = test_client("https://public.example.com")
                    .get_object()
                    .bucket("test-bucket")
                    .key("avatars/42.bin")
                    .presigned(expected_presigning)
                    .await
                    .expect("expected presigned URL")
                    .uri()
                    .to_string();

                assert_eq!(authority(&actual), Some("public.example.com"));
                assert_eq!(
                    query_param(&actual, "X-Amz-Signature"),
                    query_param(&expected, "X-Amz-Signature")
                );
            },
        ))
        .await;
    }

    #[tokio::test]
    async fn operations_reject_invalid_keys_before_calling_s3() {
        let store = test_store();

        assert_invalid_input(store.put("", "text/plain", Bytes::from_static(b"x")).await);

        let stream: ByteStream<'_> = Box::pin(futures::stream::once(async {
            Ok(Bytes::from_static(b"x"))
        }));
        assert_invalid_input(store.put_stream("../x", "text/plain", stream).await);

        assert_invalid_input(store.get("foo.meta").await);
        assert_invalid_input(store.delete("con/file.txt").await);
        assert_invalid_input(store.head("avatars/User.png").await);
        assert_invalid_input(
            store
                .presigned_url("avatars//user.png", Duration::from_secs(60))
                .await,
        );
    }

    #[test]
    fn implements_blob_store_trait() {
        fn assert_impl<T: BlobStore>() {}
        assert_impl::<S3BlobStore>();
    }

    #[test]
    fn multipart_part_size_is_exactly_5_mib() {
        assert_eq!(
            MULTIPART_PART_SIZE, 5242880,
            "MULTIPART_PART_SIZE must be exactly 5 MiB to satisfy S3 minimums"
        );
    }

    #[test]
    fn implements_send_sync_clone() {
        fn assert_impl<T: Send + Sync + Clone>() {}
        assert_impl::<S3BlobStore>();
    }

    // ── RED: presign_put for S3 ─────────────────────────────────────────────

    #[tokio::test]
    async fn presign_put_returns_put_method() {
        let store = test_store();
        let result = store
            .presign_put(
                "uploads/report.pdf",
                "application/pdf",
                Duration::from_secs(300),
            )
            .await
            .unwrap();
        assert_eq!(result.method, "PUT");
    }

    #[tokio::test]
    async fn presign_put_url_uses_expected_bucket_and_key() {
        let store = test_store();
        let result = store
            .presign_put("docs/file.txt", "text/plain", Duration::from_secs(60))
            .await
            .unwrap();
        assert!(
            result.url.contains("docs/file.txt"),
            "URL must contain the key: {}",
            result.url
        );
        assert!(
            result.url.contains("test-bucket"),
            "URL must contain the bucket: {}",
            result.url
        );
    }

    #[tokio::test]
    async fn presign_put_url_contains_sigv4_parameters() {
        let store = test_store();
        let result = store
            .presign_put("img/photo.jpg", "image/jpeg", Duration::from_secs(900))
            .await
            .unwrap();
        assert!(
            result.url.contains("X-Amz-Signature") || result.url.contains("x-amz-signature"),
            "S3 presigned PUT URL must contain SigV4 signature: {}",
            result.url
        );
    }

    #[tokio::test]
    async fn presign_put_rejects_invalid_key() {
        let store = test_store();
        let err = store
            .presign_put("../escape", "text/plain", Duration::from_secs(60))
            .await
            .unwrap_err();
        assert!(matches!(err, BlobStoreError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn presign_put_and_presigned_url_produce_different_tokens() {
        // A presigned GET URL and a presigned PUT URL for the same key must
        // produce structurally different tokens — they cannot be substituted
        // for each other.
        let store = test_store();
        let get_url = store
            .presigned_url("shared/key.txt", Duration::from_secs(60))
            .await
            .unwrap();
        let put_result = store
            .presign_put("shared/key.txt", "text/plain", Duration::from_secs(60))
            .await
            .unwrap();
        // Different operation, different signature.
        assert_ne!(
            get_url, put_result.url,
            "GET and PUT presigned URLs for the same key must differ"
        );
    }
}
