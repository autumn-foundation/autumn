# autumn-storage-s3

S3-compatible blob storage plugin for [`autumn-web`](https://crates.io/crates/autumn-web) applications.

This crate provides `S3BlobStore`, an implementation of `autumn_web::storage::BlobStore` backed
by any S3-compatible object storage service (AWS S3, MinIO, Tigris, Cloudflare R2, etc.).

## Installation

```bash
autumn plugin add autumn-storage-s3
```

One command adds the dependency at a version compatible with your app's
`autumn-web`, wires the S3 blob store into your `autumn_web::app()` builder chain
with `.with_blob_store(...)`, and
prints any configuration still needed. It is safe to re-run, and it refuses —
before touching any file — to install into an app on an incompatible
`autumn-web` version. See [docs/plugins.md](https://github.com/autumn-foundation/autumn/blob/main/docs/plugins.md#installing-a-plugin).

### Manual install

If you would rather wire it yourself (or `autumn plugin add` could not find your
builder chain and printed these lines for you):

```toml
[dependencies]
autumn-web        = { version = "0.7", features = ["storage"] }
autumn-storage-s3 = "0.7"
```

## Quick Start

```rust,ignore
use autumn_storage_s3::S3BlobStore;

#[autumn_web::main]
async fn main() {
    let store = S3BlobStore::from_config(&config.storage.s3).await
        .expect("S3 configuration is valid");

    autumn_web::app()
        .with_blob_store(store)
        .run()
        .await;
}
```

## Configuration

Configure S3 credentials and bucket via `config/default.toml`:

```toml
[storage.s3]
bucket   = "my-bucket"
region   = "us-east-1"
endpoint = "https://s3.amazonaws.com"   # optional — omit for AWS
```

Credentials are read from the standard AWS credential chain (env vars, instance profile, etc.).

## Status

This crate is the first-party S3 storage plugin for `autumn-web`. The API follows the
`BlobStore` trait defined in `autumn-web`; breaking changes to that trait are a breaking
change here too.
