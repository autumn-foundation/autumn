# Image Variants

On-demand image resizing, rotation, and metadata stripping for stored blobs —
with content-addressed caching so transforms run at most once.

## When you need it

Use variants whenever you store user-uploaded images and need to serve them at
different sizes or orientations without writing your own image pipeline, job
glue, or cache invalidation.

## Prerequisites

Enable both the `storage` and `variants` features on `autumn-web`:

```toml
[dependencies]
autumn-web = { version = "0.7", features = ["storage", "multipart", "variants"] }
```

The `variants` feature pulls in the `image` crate (JPEG, PNG, WebP codecs)
behind this flag so apps that don't need image processing don't pay the compile
cost.

## Quick start: avatar → 200×200 thumbnail served from S3 with caching

This walkthrough covers the full path from upload to cached thumbnail, under 100
lines of user code.

### 1. Upload the source image (unchanged from the storage guide)

```rust,ignore
use autumn_web::extract::{Multipart, State};
use autumn_web::prelude::*;
use autumn_web::storage::{Blob, BlobStoreState};

#[post("/avatar")]
async fn upload_avatar(
    State(blobs): State<BlobStoreState>,
    mut db: Db,
    session: Session,
    mut form: Multipart,
) -> AutumnResult<Redirect> {
    let store = blobs.store().clone();
    let user_id: i64 = session.get("user_id").unwrap();

    while let Some(field) = form.next_field().await? {
        if field.name() == Some("avatar") {
            let key = format!("avatars/{user_id}.jpg");
            let blob: Blob = field
                .save_to_blob_store(&*store, &key)
                .await?;

            diesel::update(users::table.find(user_id))
                .set(users::avatar.eq(Some(blob)))
                .execute(&mut *db)
                .await?;

            return Ok(Redirect::to("/profile"));
        }
    }
    Err(AutumnError::bad_request_msg("missing 'avatar' field"))
}
```

### 2. Serve the thumbnail

```rust,ignore
use std::time::Duration;
use autumn_web::prelude::*;
use autumn_web::storage::{BlobStoreState, variant::{Transform, VariantBudget, VariantError}};

#[get("/users/:id/avatar/thumb")]
async fn avatar_thumb(
    State(blobs): State<BlobStoreState>,
    mut db: Db,
    Path(user_id): Path<i64>,
) -> AutumnResult<Redirect> {
    let user: User = users::table.find(user_id).first(&mut *db).await?;
    let avatar = user
        .avatar
        .ok_or_else(|| AutumnError::not_found_msg("no avatar"))?;

    let store = blobs.store();
    let budget = VariantBudget::default(); // or load from config

    // Declare the variant: resize to fit within 200×200, strip EXIF.
    let handle = avatar.variant(
        "thumb",
        &[
            Transform::resize_to_limit(200, 200),
            Transform::strip_metadata(),
        ],
    );

    // First call: fetches source, generates variant, stores it, returns URL.
    // Subsequent calls: skips generation (cache hit), returns URL directly.
    let url = handle
        .url(&**store, &budget, Duration::from_secs(3600))
        .await
        .map_err(VariantError::into_autumn_error)?;

    Ok(Redirect::to(url))
}
```

The `url()` method:

1. Calls `store.head(variant_key)` — if the variant is already cached, goes
   directly to step 3.
2. Fetches the source bytes, decodes the image, applies transforms, encodes,
   and stores the result under a content-addressed key.
3. Calls `store.presigned_url(variant_key, expires_in)` — a route-served
   HMAC-signed URL on the Local backend; a real S3 presigned URL on the S3
   backend.

The behaviour is identical in `dev` (Local backend) and `prod` (S3 backend) —
you write no backend-specific code.

## Available transforms

```rust,ignore
use autumn_web::storage::variant::Transform;

// Resize to fit within the box, maintaining aspect ratio.
// Never upscales an image that already fits.
Transform::resize_to_limit(width, height)

// Resize and crop to exactly width × height (centred).
Transform::resize_to_fill(width, height)

// Rotate clockwise; valid values: 90, 180, 270 (others are no-ops).
Transform::rotate(degrees)

// Strip all embedded metadata (EXIF, GPS, ICC profiles).
// Re-encoding through the image crate already drops EXIF; this transform
// documents the intent explicitly.
Transform::strip_metadata()
```

Transforms are applied in order and all four compose freely:

```rust,ignore
let handle = blob.variant("card", &[
    Transform::resize_to_fill(400, 300),
    Transform::strip_metadata(),
]);
```

## Supported formats

| Source format | Decoded by | Output format |
|---|---|---|
| JPEG (`image/jpeg`) | `image` crate | JPEG |
| PNG  (`image/png`)  | `image` crate | PNG  |
| WebP (`image/webp`) | `image` crate | JPEG (transcoded) |

Non-image MIME types (PDFs, videos, binaries) are rejected with
`VariantError::UnsupportedMimeType` before any bytes are fetched from storage.

## Content addressing

The variant's storage key is `SHA-256(source_key || NUL || JSON(transforms))`
encoded as `_variants/{h[0..2]}/{h[2..4]}/{h}`:

- The same source and same transforms always produce the same key.
- The human-readable `name` label (e.g. `"thumb"`) is stored on the handle but
  does not affect the cache key — it's a hint for your own code.
- Different specs (different dimensions, different transform order) produce
  different keys without any coordination.

## ETag and cache headers

Variants are stored as ordinary blobs and served through the same presigned-URL
path as any other blob.  The Local backend's serving route adds `ETag` from the
blob's SHA-256 sidecar; S3 returns the S3 ETag.  Because the key is content-
addressed, the URL never changes for the same (source, spec) pair — you can
safely set `Cache-Control: immutable` on the serving response, or let a CDN
cache the presigned redirect.

## Resource limits

Variant generation is bounded to prevent a pathologically large source image
from exhausting worker memory.  Set limits in `autumn.toml`:

```toml
[storage.variants]
max_source_bytes  = 20971520  # 20 MiB (default)
max_source_width  = 10000     # pixels (default)
max_source_height = 10000     # pixels (default)
```

Requests that exceed the byte limit are rejected before the source is fetched.
Requests that exceed the pixel limit are rejected after decode (but before
transform).  Both produce a `VariantError` that maps to HTTP 413.

Build a `VariantBudget` from your config:

```rust,ignore
use autumn_web::storage::variant::VariantBudget;

let budget = VariantBudget {
    max_source_bytes:  config.storage.variants.max_source_bytes,
    max_source_width:  config.storage.variants.max_source_width,
    max_source_height: config.storage.variants.max_source_height,
};
```

Or use the defaults:

```rust,ignore
let budget = VariantBudget::default(); // 20 MiB, 10 000 × 10 000 px
```

## Background generation

For routes where you can't afford the decode-and-encode latency on the request
path, generate the variant in a background job and redirect to a placeholder
while it completes:

```rust,ignore
use autumn_web::storage::variant::{Transform, VariantBudget};

#[job]
async fn generate_variant_job(state: AppState, payload: serde_json::Value) {
    let blob: Blob = serde_json::from_value(payload["blob"].clone()).unwrap();
    let transforms: Vec<Transform> = serde_json::from_value(payload["transforms"].clone()).unwrap();
    let blobs = state.extension::<BlobStoreState>().unwrap();
    let store = blobs.store();
    let budget = VariantBudget::default();
    blob.variant("thumb", &transforms)
        .ensure_generated(&**store, &budget)
        .await
        .ok();
}

#[get("/images/:id/thumb")]
async fn serve_thumb(
    State(state): State<AppState>,
    Path(image_id): Path<i64>,
    mut db: Db,
) -> AutumnResult<impl IntoResponse> {
    let img: Image = images::table.find(image_id).first(&mut *db).await?;
    let blob = img.file;
    let store = state.extension::<BlobStoreState>().unwrap().store().clone();
    let budget = VariantBudget::default();
    let handle = blob.variant("thumb", &[Transform::resize_to_limit(200, 200)]);

    // Try cache first; if cold, enqueue the job and return a placeholder redirect.
    if store.head(handle.key()).await.ok().flatten().is_some() {
        let url = store.presigned_url(handle.key(), Duration::from_secs(3600)).await?;
        return Ok(Redirect::to(url).into_response());
    }

    // Enqueue generation job.
    state.jobs().enqueue("generate_variant_job", serde_json::json!({
        "blob": blob,
        "transforms": [{"ResizeToLimit": {"width": 200, "height": 200}}],
    })).await?;

    // Return placeholder while job runs.
    Ok(Redirect::to("/static/placeholder-thumb.png").into_response())
}
```

## Error handling

```rust,ignore
use autumn_web::storage::variant::VariantError;

match handle.ensure_generated(&**store, &budget).await {
    Ok(blob) => { /* use blob.key for presigned_url */ }
    Err(VariantError::UnsupportedMimeType(mime)) => {
        // 400: caller uploaded a PDF, video, etc.
    }
    Err(VariantError::SourceTooLarge { byte_size, max_bytes }) => {
        // 413: source exceeds budget.max_source_bytes
    }
    Err(VariantError::SourceDimensionsTooLarge { width, height, .. }) => {
        // 413: decoded image too large
    }
    Err(VariantError::DecodeError(msg)) => {
        // 422: corrupted or unsupported image data
    }
    Err(VariantError::Storage(err)) => {
        // propagate the BlobStoreError
    }
}
```

## Works with both backends

The variant layer is fully backend-agnostic: variants are stored via
`BlobStore::put`, checked via `BlobStore::head`, and served via
`BlobStore::presigned_url`.  Those three methods work identically on the Local
backend and the `autumn-storage-s3` backend — no backend-specific user code is
required.
