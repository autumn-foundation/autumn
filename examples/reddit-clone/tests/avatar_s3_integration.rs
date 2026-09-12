//! Integration test: avatar blob operations against a real MinIO container.
//!
//! Exercises `S3BlobStore` end-to-end — `put`, `get`, `head`, `presigned_url`,
//! and `delete` — using the avatar key scheme from `src/routes/avatars.rs`.
//! Requires Docker; skipped by default.

use autumn_storage_s3::S3BlobStore;
use autumn_web::storage::{BlobStore, StorageS3Config};
use aws_sdk_s3::{
    Client,
    config::{BehaviorVersion, Credentials, Region},
};
use bytes::Bytes;
use testcontainers::ImageExt;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::minio::MinIO;

const MINIO_USER: &str = "minioadmin";
const MINIO_PASSWORD: &str = "minioadmin";
const BUCKET: &str = "test-avatars";
const KEY_ENV: &str = "__REDDIT_CLONE_MINIO_KEY__";
const SECRET_ENV: &str = "__REDDIT_CLONE_MINIO_SECRET__";

async fn make_admin_client(port: u16) -> Client {
    let endpoint = format!("http://127.0.0.1:{port}");
    let creds = Credentials::new(MINIO_USER, MINIO_PASSWORD, None, None, "test");
    let cfg = aws_sdk_s3::Config::builder()
        .behavior_version(BehaviorVersion::latest())
        .region(Region::new("us-east-1"))
        .credentials_provider(creds)
        .endpoint_url(endpoint)
        .force_path_style(true)
        .build();
    Client::from_conf(cfg)
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn avatar_blob_store_roundtrip() {
    // MinIO pulled its images from Docker Hub (`minio/minio`); MinIO Inc.
    // stopped publishing there, so `minio/minio` now 404s. It still
    // publishes the same tags on Quay -- repoint the registry, same tag.
    let container = MinIO::default()
        .with_name("quay.io/minio/minio")
        .start()
        .await
        .expect("start MinIO");
    let port = container
        .get_host_port_ipv4(9000)
        .await
        .expect("MinIO port");
    let endpoint = format!("http://127.0.0.1:{port}");

    let admin = make_admin_client(port).await;
    admin
        .create_bucket()
        .bucket(BUCKET)
        .send()
        .await
        .expect("create bucket");

    let cfg = StorageS3Config {
        bucket: Some(BUCKET.to_owned()),
        region: Some("us-east-1".to_owned()),
        endpoint: Some(endpoint),
        force_path_style: true,
        access_key_id_env: Some(KEY_ENV.to_owned()),
        secret_access_key_env: Some(SECRET_ENV.to_owned()),
        ..StorageS3Config::default()
    };

    temp_env::async_with_vars(
        [
            (KEY_ENV, Some(MINIO_USER)),
            (SECRET_ENV, Some(MINIO_PASSWORD)),
        ],
        async move {
            let store = S3BlobStore::from_config(&cfg)
                .await
                .expect("build S3BlobStore from MinIO config");

            // Avatar key matches the scheme in src/routes/avatars.rs
            let key = "avatars/42.bin";
            let data = Bytes::from_static(b"\x89PNG\r\n\x1a\n");

            // put
            let blob = store
                .put(key, "image/png", data.clone())
                .await
                .expect("put");
            assert_eq!(blob.key, key);
            assert_eq!(blob.content_type, "image/png");
            assert_eq!(blob.byte_size, data.len() as u64);

            // head — metadata matches
            let meta = store.head(key).await.expect("head").expect("blob exists");
            assert_eq!(meta.key, key);
            assert_eq!(meta.byte_size, data.len() as u64);

            // get — bytes round-trip cleanly
            let fetched = store.get(key).await.expect("get");
            assert_eq!(fetched, data);

            // presigned_url — non-empty, references the key
            let url = store
                .presigned_url(key, std::time::Duration::from_secs(300))
                .await
                .expect("presigned_url");
            assert!(!url.is_empty());

            // delete
            store.delete(key).await.expect("delete");

            // head after delete — blob is gone
            let gone = store.head(key).await.expect("head after delete");
            assert!(gone.is_none(), "blob should be gone after delete");
        },
    )
    .await;
}
