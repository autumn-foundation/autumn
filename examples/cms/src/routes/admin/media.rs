//! The media library — WordPress's uploader and attachment list.
//!
//! Uploaded bytes go to the configured `BlobStore` (local disk in development,
//! S3 in production); the database keeps only the handle and the metadata, so
//! the same code serves both backends and Postgres never carries an image.

use autumn_web::AutumnResult;
use autumn_web::download::Download;
use autumn_web::prelude::*;
use autumn_web::reexports::axum::response::Response;
use autumn_web::storage::BlobStoreState;

use crate::capabilities::Capability;
use crate::models::{NewAttachment, UpdateAttachment};
use crate::plugins::{Action, do_action};
use crate::repositories::AttachmentRepository as _;
use crate::require_capability;

use super::super::site::{Csrf, Repos};
use super::layout;

/// The upload cap. WordPress defers to PHP's `upload_max_filesize`, which is
/// how a 2 MB default surprises people; stating it here means the form text and
/// the enforced limit come from the same constant.
const MAX_UPLOAD_BYTES: usize = 16 * 1024 * 1024;

/// The MIME types the library accepts.
///
/// An allowlist, not a blocklist, and enforced **server-side**: the form's
/// `accept` attribute is a hint the browser may ignore. Without this check a
/// crafted client could upload HTML, which `/media/{slug}` would later serve
/// from the site's own origin with the attacker's `Content-Type` — stored XSS.
const ALLOWED_MIME: &[&str] = &[
    "image/png",
    "image/jpeg",
    "image/gif",
    "image/webp",
    "image/svg+xml",
    "application/pdf",
    "text/plain",
    "text/csv",
];

// SVG is on the allowlist because a CMS genuinely needs logos, but an SVG is a
// document that can carry script — so it is served as an attachment rather than
// inline (see `serve` below), which is what stops it executing on this origin.
const NEVER_INLINE: &[&str] = &["image/svg+xml", "text/plain", "text/csv"];

#[get("/admin/media")]
pub async fn list(repos: Repos, session: Session, csrf: Csrf) -> AutumnResult<Response> {
    let user = require_capability!(repos, session, csrf, Capability::UploadFiles);
    let mut items = repos.attachments.find_all().await?;
    items.sort_by_key(|item| std::cmp::Reverse(item.created_at));

    let body = html! {
        form action="/admin/media" method="post" enctype="multipart/form-data"
             class="bg-white rounded-lg shadow p-5 mb-6 space-y-3" {
                 (csrf.input())
            h2 class="font-semibold text-sm" { "Upload" }
            div class="grid grid-cols-1 sm:grid-cols-2 gap-3" {
                div {
                    label for="file" class="block text-sm font-medium mb-1" { "File" }
                    input #file type="file" name="file" required
                          accept=(ALLOWED_MIME.join(","))
                          class="w-full border rounded px-3 py-2 text-sm";
                }
                div {
                    label for="alt_text" class="block text-sm font-medium mb-1" {
                        "Alt text "
                        span class="text-gray-400 font-normal" {
                            "(leave blank if decorative)"
                        }
                    }
                    input #alt_text type="text" name="alt_text" maxlength="300"
                          class="w-full border rounded px-3 py-2 text-sm";
                }
            }
            p class="text-xs text-gray-400" {
                "Up to " (MAX_UPLOAD_BYTES / (1024 * 1024)) " MB. Accepted: "
                (ALLOWED_MIME.join(", ")) "."
            }
            button type="submit"
                   class="px-4 py-2 bg-indigo-600 text-white rounded hover:bg-indigo-700" {
                "Upload"
            }
        }

        div class="grid grid-cols-2 sm:grid-cols-3 lg:grid-cols-4 gap-4" {
            @for item in &items {
                figure class="bg-white rounded-lg shadow overflow-hidden" {
                    @if item.is_image() && item.mime_type != "image/svg+xml" {
                        img src=(format!("/media/{}", item.slug)) alt=(item.alt_text)
                            loading="lazy" class="w-full h-32 object-cover bg-gray-100";
                    } @else {
                        div class="w-full h-32 bg-gray-100 flex items-center justify-center \
                                   text-gray-400 text-xs font-mono" {
                            (item.mime_type)
                        }
                    }
                    figcaption class="p-3 text-xs" {
                        p class="font-medium truncate" { (item.title) }
                        p class="text-gray-400" { (item.byte_size / 1024) " KB" }
                        @if item.is_image() && item.alt_text.trim().is_empty() {
                            p class="text-amber-700 mt-1" { "No alt text" }
                        }
                        div class="flex gap-2 mt-2" {
                            a href=(format!("/media/{}", item.slug))
                              class="text-indigo-700 hover:underline" { "View" }
                            form method="post"
                                 action=(format!("/admin/media/{}/delete", item.id)) {
                                     (csrf.input())
                                button type="submit" class="text-red-700 hover:underline" {
                                    "Delete"
                                }
                            }
                        }
                    }
                }
            }
            @if items.is_empty() {
                p class="col-span-full text-center text-gray-400 py-10" {
                    "Nothing in the library yet."
                }
            }
        }
    };

    Ok(layout(&user, &csrf, "/admin/media", "Media", body).into_response())
}

#[post("/admin/media")]
pub async fn upload(
    repos: Repos,
    session: Session,
    csrf: Csrf,
    State(state): State<AppState>,
    // The framework's injectable entropy source, rather than a thread RNG, so a
    // simulation test replays the same object keys for the same seed.
    rng: autumn_web::entropy::Rng,
    mut form: autumn_web::extract::Multipart,
) -> AutumnResult<Response> {
    let user = require_capability!(repos, session, csrf, Capability::UploadFiles);
    let blobs = state
        .extension::<BlobStoreState>()
        .ok_or_else(|| AutumnError::internal_server_error_msg("storage is not configured"))?;

    let mut filename = String::new();
    let mut mime_type = String::new();
    let mut alt_text = String::new();
    let mut stored: Option<(autumn_web::storage::Blob, String)> = None;

    while let Some(field) = form.next_field().await? {
        match field.name() {
            Some("alt_text") => {
                // `bytes_limited` rather than an unbounded read: a text field
                // is still attacker-controlled length.
                let raw = field.with_max_bytes(1024).bytes_limited().await?;
                alt_text = String::from_utf8_lossy(&raw).trim().to_owned();
            }
            Some("file") => {
                filename = field.file_name().unwrap_or("upload").to_owned();
                mime_type = field.content_type().unwrap_or("").to_owned();
                if !ALLOWED_MIME.contains(&mime_type.as_str()) {
                    return Err(AutumnError::unprocessable_msg(format!(
                        "Unsupported file type {mime_type:?}. Accepted: {}",
                        ALLOWED_MIME.join(", ")
                    )));
                }

                // A content-addressed key under a stable prefix. Two uploads of
                // the same name never collide, and the key carries no
                // user-controlled path segment.
                let slug = unique_slug(&filename, &rng);
                let key = format!("media/{slug}");
                let blob = field
                    .with_max_bytes(MAX_UPLOAD_BYTES)
                    .save_to_blob_store(&*blobs.store().clone(), &key)
                    .await?;
                stored = Some((blob, slug));
            }
            _ => {}
        }
    }

    let (blob, slug) =
        stored.ok_or_else(|| AutumnError::bad_request_msg("No file was included in the upload"))?;
    let byte_size = i64::try_from(blob.byte_size).unwrap_or(0);

    let created = repos
        .attachments
        .save(&NewAttachment {
            title: display_title(&filename),
            slug,
            file: Some(blob),
            mime_type,
            byte_size,
            width: None,
            height: None,
            alt_text,
            caption: String::new(),
            uploader_id: Some(user.id),
        })
        .await?;

    do_action(Action::AttachmentUploaded, created.id);
    Ok(Redirect::to("/admin/media").into_response())
}

#[post("/admin/media/{id}/delete")]
pub async fn delete(
    repos: Repos,
    session: Session,
    csrf: Csrf,
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> AutumnResult<Response> {
    let user = require_capability!(repos, session, csrf, Capability::UploadFiles);
    let attachment = repos
        .attachments
        .find_by_id(id)
        .await?
        .ok_or_else(|| AutumnError::not_found_msg("No such attachment"))?;

    // `upload_files` is permission to *add* a file, not to remove anyone's.
    // Without this an Author could delete a colleague's upload — the library
    // lists every attachment — taking its blob with it and detaching it from
    // every post using it as a featured image. Deleting someone else's upload
    // needs the same capability as deleting their content.
    let owns_it = attachment.uploader_id == Some(user.id);
    if !owns_it && !user.role().can(Capability::DeleteOthersPosts) {
        return Err(AutumnError::forbidden_msg(
            "You can only delete files you uploaded",
        ));
    }

    // Remove the row first: posts referencing it as a featured image are
    // detached by `ON DELETE SET NULL`, so nothing is left pointing at bytes
    // that are about to vanish. If the blob delete then fails, the result is an
    // orphaned object in the store — wasted space, not a broken page. The
    // reverse order would leave live pages pointing at a missing file.
    repos.attachments.delete_by_id(id).await?;
    if let (Some(blob), Some(blobs)) = (
        attachment.file.as_ref(),
        state.extension::<BlobStoreState>(),
    ) {
        // Best effort: a store that has already lost the object must not turn
        // a successful delete into a 500.
        let _ = blobs.store().delete(&blob.key).await;
    }

    Ok(Redirect::to("/admin/media").into_response())
}

/// Serve an uploaded file.
///
/// Public, because media referenced from published content has to be. The
/// `Content-Type` is the one recorded at upload — from the allowlist, never
/// from the request — and anything that can carry script is served as an
/// attachment rather than inline.
#[get("/media/{slug}")]
pub async fn serve(
    repos: Repos,
    State(state): State<AppState>,
    Path(slug): Path<String>,
) -> AutumnResult<Response> {
    let attachment = repos
        .attachments
        .find_by_slug(slug)
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| AutumnError::not_found_msg("No such file"))?;
    let blobs = state
        .extension::<BlobStoreState>()
        .ok_or_else(|| AutumnError::internal_server_error_msg("storage is not configured"))?;

    let blob = attachment.blob()?;
    let mut download = Download::from_blob(blobs.store(), blob.key.clone())
        .await?
        .content_type(attachment.mime_type.clone())
        .filename(attachment.title.clone());
    if !NEVER_INLINE.contains(&attachment.mime_type.as_str()) {
        download = download.inline();
    }
    Ok(download.into_response())
}

/// A URL-safe, collision-resistant key for an upload.
fn unique_slug(filename: &str, rng: &autumn_web::entropy::Rng) -> String {
    let stem = filename.rsplit('/').next().unwrap_or(filename);
    let (name, ext) = match stem.rsplit_once('.') {
        Some((name, ext)) if ext.len() <= 8 => (name, Some(ext)),
        _ => (stem, None),
    };
    let base = autumn_web::slugify(name);
    let base = if base.is_empty() {
        "file".to_owned()
    } else {
        base
    };
    // A random suffix rather than a counter: a counter needs a read-then-write
    // against the table and still races.
    let uuid = rng.uuid_v4().simple().to_string();
    let suffix = &uuid[..12];
    match ext {
        Some(ext) => format!("{base}-{suffix}.{}", autumn_web::slugify(ext)),
        None => format!("{base}-{suffix}"),
    }
}

/// A readable title from an uploaded filename.
fn display_title(filename: &str) -> String {
    let stem = filename.rsplit('/').next().unwrap_or(filename);
    let name = stem.rsplit_once('.').map_or(stem, |(name, _)| name);
    if name.trim().is_empty() {
        "Untitled".to_owned()
    } else {
        name.trim().to_owned()
    }
}

#[allow(dead_code)]
fn _type_uses(_: UpdateAttachment) {}
