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
use crate::content;
use crate::models::{NewAttachment, UpdateAttachment};
use crate::plugins::{Action, do_action};
use crate::repositories::AttachmentRepository as _;
use crate::require_capability;

use super::super::site::{Csrf, Repos};
use super::layout;

/// How many attachments one library page shows.
const MEDIA_PER_PAGE: i64 = 48;

#[derive(Debug, Default, serde::Deserialize)]
pub struct LibraryFilter {
    #[serde(default)]
    pub page: Option<usize>,
}

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

/// Whether an attachment row may claim this type at all.
///
/// The same allowlist the upload path enforces, exposed so the importer can
/// apply it to rows arriving from a file rather than from a browser.
#[must_use]
pub fn is_allowed_mime(mime_type: &str) -> bool {
    ALLOWED_MIME.contains(&mime_type)
}

/// Whether `/media/{slug}` may serve this type inline.
///
/// Allowlist-shaped rather than denylist-shaped, deliberately. The upload path
/// enforces `ALLOWED_MIME`, but an attachment row can arrive by other routes —
/// an import of a tampered export, a hand-written `INSERT` — and a row claiming
/// `text/html` was previously served inline, because `NEVER_INLINE` named only
/// SVG and a couple of text types. That is stored script execution on the
/// site's own origin, from bytes an administrator was told they were merely
/// restoring.
///
/// Deciding it here rather than only at the door means every row is covered
/// whatever created it, and an unrecognised type downloads instead of running.
#[must_use]
pub fn may_render_inline(mime_type: &str) -> bool {
    ALLOWED_MIME.contains(&mime_type) && !NEVER_INLINE.contains(&mime_type)
}

#[get("/admin/media")]
pub async fn list(
    repos: Repos,
    session: Session,
    csrf: Csrf,
    Query(filter): Query<LibraryFilter>,
) -> AutumnResult<Response> {
    let user = require_capability!(repos, session, csrf, Capability::UploadFiles);

    // Ordered and bounded in SQL. `find_all` loaded every attachment row and
    // this screen then sorted the whole collection in memory and rendered all
    // of it into one response — so the library grew itself out of usability,
    // one legitimate upload at a time.
    let page = i64::try_from(filter.page.unwrap_or(1).clamp(1, 100_000)).unwrap_or(1);
    let (items, total) = {
        let mut conn = repos.conn().await?;
        let rows =
            content::attachments_page(&mut conn, (page - 1) * MEDIA_PER_PAGE, MEDIA_PER_PAGE)
                .await?;
        let total = content::attachment_count(&mut conn).await?;
        (rows, total)
    };
    let last_page = ((total + MEDIA_PER_PAGE - 1) / MEDIA_PER_PAGE).max(1);

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
                    @if page > 1 { "Nothing on this page." } @else { "Nothing in the library yet." }
                }
            }
        }

        @if last_page > 1 {
            nav aria-label="Media pages" class="flex items-center justify-between mt-6 text-sm" {
                @if page > 1 {
                    a href=(format!("/admin/media?page={}", page - 1))
                      class="text-indigo-700 hover:underline" { "← Newer" }
                } @else {
                    span {}
                }
                span class="text-gray-500" { "Page " (page) " of " (last_page) }
                @if page < last_page {
                    a href=(format!("/admin/media?page={}", page + 1))
                      class="text-indigo-700 hover:underline" { "Older →" }
                } @else {
                    span {}
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
    // Armed for the whole handler: anything that returns after a blob is stored
    // and before the attachment row exists takes the bytes with it.
    let mut orphan = OrphanedBlobGuard::new(std::sync::Arc::clone(blobs.store()));

    while let Some(field) = form.next_field().await? {
        match field.name() {
            Some("alt_text") => {
                // `bytes_limited` rather than an unbounded read: a text field
                // is still attacker-controlled length.
                let raw = field.with_max_bytes(1024).bytes_limited().await?;
                alt_text = String::from_utf8_lossy(&raw).trim().to_owned();
            }
            Some("file") => {
                // Refused *before* the body is read, so a second file part
                // costs nothing. Overwriting `stored` instead left the earlier
                // blob in the store with no attachment row pointing at it:
                // unreachable from the media library, undeletable through the
                // UI, and repeatable — an Author could fill local disk or an S3
                // bucket a few hundred megabytes at a time.
                if stored.is_some() {
                    return Err(AutumnError::unprocessable_msg("Upload one file at a time"));
                }
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
                // From here the bytes exist, so the guard owns them until the
                // attachment row does.
                orphan.watch(blob.key.clone());
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

    // The row exists, so the bytes are its responsibility now rather than the
    // guard's.
    orphan.keep();

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

    // A row whose file is absent is a *missing file*, not a broken server:
    // that is what a version-2 import (which carried no handles) leaves behind,
    // and what a hand-written row looks like. 404 says so; the 500
    // `Attachment::blob()` raises is for callers that have no better answer.
    let blob = attachment
        .file
        .as_ref()
        .ok_or_else(|| AutumnError::not_found_msg("No such file"))?;
    let mut download = Download::from_blob(blobs.store(), blob.key.clone())
        .await?
        .content_type(attachment.mime_type.clone())
        .filename(download_filename(&attachment));
    if may_render_inline(&attachment.mime_type) {
        download = download.inline();
    }
    Ok(download.into_response())
}

/// A URL-safe, collision-resistant key for an upload.
fn unique_slug(filename: &str, rng: &autumn_web::entropy::Rng) -> String {
    let stem = filename.rsplit('/').next().unwrap_or(filename);
    // ASCII alphanumeric, not merely short. `slugify` never returns an empty
    // string — it falls back to a hash token derived from its input (see its
    // docs, and #2424) — so `photo.画像` and `logo.` were stored as
    // `photo-<suffix>.n3f2a9c…`, a manufactured extension that is worse than
    // none: it names a type nothing has, and it is what the reader sees when
    // they save the file. Nothing here is a filesystem hazard, so the answer is
    // to decide whether the suffix *is* an extension rather than to normalize
    // whatever follows the dot.
    let (name, ext) = match stem.rsplit_once('.') {
        Some((name, ext))
            if !ext.is_empty()
                && ext.len() <= 8
                && ext.chars().all(|c| c.is_ascii_alphanumeric()) =>
        {
            (name, Some(ext))
        }
        _ => (stem, None),
    };
    let base = autumn_web::slugify(name);
    let mut base = if base.is_empty() {
        "file".to_owned()
    } else {
        base
    };
    // Bounded, because this becomes both the public URL segment and the blob
    // store key. A 500-character filename produced a key the local backend
    // could not write at all — `File name too long (os error 63)`, a 500 on an
    // upload that is otherwise perfectly valid, raised before the attachment
    // row was even attempted. Most filesystems cap a path component at 255
    // bytes; 100 leaves ample room for the random suffix and the extension.
    const MAX_SLUG_BASE: usize = 100;
    if base.chars().count() > MAX_SLUG_BASE {
        base = base.chars().take(MAX_SLUG_BASE).collect();
    }
    // A random suffix rather than a counter: a counter needs a read-then-write
    // against the table and still races.
    // The whole UUID, not a prefix of it. Twelve hex characters is 48 bits, and
    // the consequence of a birthday collision here is not a retry: the blob is
    // written *before* the attachment row, so a colliding key first overwrites
    // the existing object's bytes, and then the row's unique violation drops
    // `OrphanedBlobGuard`, which deletes that now-shared key — taking the
    // pre-existing attachment's file with it. A cheaper key is not worth a
    // failure mode that destroys somebody else's upload.
    let suffix = rng.uuid_v4().simple().to_string();
    // Already ASCII alphanumeric by the match above, so lowercasing is the whole
    // normalization — `slugify` would only reintroduce the fallback token.
    match ext {
        Some(ext) => format!("{base}-{suffix}.{}", ext.to_ascii_lowercase()),
        None => format!("{base}-{suffix}"),
    }
}

/// The name a download is saved under.
///
/// `display_title` strips the extension before it becomes `attachment.title` —
/// deliberately, because the title is what the editor reads — so using the title
/// alone saved `report.csv` as `report`, a file the operating system no longer
/// knows what to open. The extension comes back from the slug, which is where
/// the normalized one lives.
///
/// The title, not the slug, remains the base: the slug is machine-shaped
/// (`my-report-8f21a0c4d5e6.csv`) and the file the reader saves should be named
/// the way the library names it.
fn download_filename(attachment: &crate::models::Attachment) -> String {
    let title = attachment.title.trim();
    if title.is_empty() {
        return attachment.slug.clone();
    }
    let Some((_, ext)) = attachment.slug.rsplit_once('.') else {
        return title.to_owned();
    };
    if ext.is_empty() {
        return title.to_owned();
    }
    // An editor who renamed the file to include the extension already gets it
    // right; appending a second one would produce `report.csv.csv`.
    let suffix = format!(".{ext}");
    if title.to_lowercase().ends_with(&suffix.to_lowercase()) {
        return title.to_owned();
    }
    format!("{title}{suffix}")
}

/// Deletes an uploaded blob unless the upload completes.
///
/// Cleanup has to survive *every* way the handler can leave after the bytes are
/// stored, and enumerating those by hand is how the last two attempts at this
/// went wrong: the first missed the failed `INSERT`, the second missed the
/// duplicate-part rejection — which leaks a whole 16 MB object, on an endpoint
/// an Author can call in a loop. A guard makes the property structural instead:
/// the blob is orphaned unless something explicitly says it is not, so an error
/// path added later is covered by construction.
///
/// The delete is spawned because `Drop` cannot await. It is best-effort by
/// nature — a cleanup failure is logged and never replaces the error that
/// caused it, which is what the caller actually needs to hear.
struct OrphanedBlobGuard {
    store: autumn_web::storage::SharedBlobStore,
    key: Option<String>,
}

impl OrphanedBlobGuard {
    fn new(store: autumn_web::storage::SharedBlobStore) -> Self {
        Self { store, key: None }
    }

    /// Take responsibility for `key` until [`Self::keep`] says otherwise.
    fn watch(&mut self, key: String) {
        self.key = Some(key);
    }

    /// The attachment row exists; the bytes belong to it now.
    fn keep(&mut self) {
        self.key = None;
    }
}

impl Drop for OrphanedBlobGuard {
    fn drop(&mut self) {
        let Some(key) = self.key.take() else {
            return;
        };
        let store = std::sync::Arc::clone(&self.store);
        autumn_web::reexports::tokio::spawn(async move {
            if let Err(error) = store.delete(&key).await {
                autumn_web::reexports::tracing::warn!(
                    %error,
                    key = %key,
                    "failed to remove the blob for an upload that did not complete"
                );
            }
        });
    }
}

/// A readable title from an uploaded filename.
fn display_title(filename: &str) -> String {
    let stem = filename.rsplit('/').next().unwrap_or(filename);
    let name = stem.rsplit_once('.').map_or(stem, |(name, _)| name);
    let name = name.trim();
    if name.is_empty() {
        return "Untitled".to_owned();
    }
    // Truncated to the model's own limit. A filename long enough to exceed it
    // is a perfectly valid upload, and letting the derived title fail
    // validation meant the bytes were already in the store when the row was
    // refused — an object with no attachment row, unreachable from the media
    // library and undeletable through it. Truncating at a char boundary; the
    // filename is not the title's only source of truth, and the editor can
    // rename.
    const MAX_TITLE: usize = 300;
    if name.chars().count() <= MAX_TITLE {
        return name.to_owned();
    }
    name.chars().take(MAX_TITLE).collect()
}

#[allow(dead_code)]
fn _type_uses(_: UpdateAttachment) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn attachment(title: &str, slug: &str) -> crate::models::Attachment {
        crate::models::Attachment {
            id: 1,
            title: title.to_owned(),
            slug: slug.to_owned(),
            file: None,
            mime_type: "text/csv".to_owned(),
            byte_size: 1,
            width: None,
            height: None,
            alt_text: String::new(),
            caption: String::new(),
            uploader_id: None,
            created_at: chrono::NaiveDateTime::default(),
            updated_at: chrono::NaiveDateTime::default(),
        }
    }

    /// A key carries a real extension or none — never a manufactured one.
    ///
    /// `slugify` never returns an empty string: it falls back to a hash token
    /// derived from its input. So slugifying whatever follows the last dot
    /// turned `logo.` and `photo.画像` into `….n3f2a9c…`, an extension naming a
    /// type nothing has, which is then what the reader sees when they save it.
    #[test]
    fn a_key_carries_a_real_extension_or_none() {
        let rng =
            autumn_web::entropy::Rng::from_source(autumn_web::entropy::SeededEntropy::shared(7));
        for filename in [
            "logo.",
            "photo.画像",
            "notes",
            "a.b.画像",
            "x.toolongextension",
        ] {
            let slug = unique_slug(filename, &rng);
            assert!(!slug.is_empty(), "{filename} produced an empty key");
            if let Some((_, ext)) = slug.rsplit_once('.') {
                panic!("{filename} manufactured the extension {ext:?} in {slug}");
            }
        }
        // A real one survives, lowercased.
        assert!(unique_slug("report.csv", &rng).ends_with(".csv"));
        assert!(unique_slug("archive.TAR", &rng).ends_with(".tar"));
        assert!(unique_slug("shot.PNG", &rng).ends_with(".png"));
    }

    /// The key keeps the whole UUID.
    ///
    /// A truncated one is not just a smaller namespace: the blob is written
    /// before the attachment row, so a colliding key overwrites the existing
    /// object's bytes, and the row's unique violation then drops
    /// `OrphanedBlobGuard`, which deletes the shared key — destroying a
    /// pre-existing upload rather than failing the new one.
    #[test]
    fn a_key_keeps_the_whole_uuid() {
        let rng =
            autumn_web::entropy::Rng::from_source(autumn_web::entropy::SeededEntropy::shared(11));
        let slug = unique_slug("photo.png", &rng);
        let suffix = slug
            .strip_prefix("photo-")
            .and_then(|rest| rest.strip_suffix(".png"))
            .expect("the shape is base-suffix.ext");
        assert_eq!(
            suffix.len(),
            32,
            "a UUID is 32 hex characters; anything shorter trades a collision \
             that destroys somebody else's file for a shorter URL: {slug}"
        );
        assert!(suffix.chars().all(|c| c.is_ascii_hexdigit()));
    }

    /// `display_title` strips the extension, so the title alone saved
    /// `report.csv` as `report` — a file the operating system no longer knows
    /// what to open.
    #[test]
    fn a_download_keeps_the_uploaded_extension() {
        assert_eq!(
            download_filename(&attachment("Report", "report-8f21a0c4d5e6.csv")),
            "Report.csv"
        );
        // No extension on the slug means none to restore.
        assert_eq!(
            download_filename(&attachment("Notes", "notes-8f21a0c4d5e6")),
            "Notes"
        );
        // An editor who already typed it does not get it twice, whatever case.
        assert_eq!(
            download_filename(&attachment("Report.csv", "report-8f21a0c4d5e6.csv")),
            "Report.csv"
        );
        assert_eq!(
            download_filename(&attachment("Report.CSV", "report-8f21a0c4d5e6.csv")),
            "Report.CSV"
        );
        // A dot in the title is not an extension.
        assert_eq!(
            download_filename(&attachment("Report v1.2", "report-8f21a0c4d5e6.csv")),
            "Report v1.2.csv"
        );
    }
}
