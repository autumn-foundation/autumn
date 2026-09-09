//! Tools — export and import.
//!
//! WordPress's exporter emits WXR, an RSS dialect with custom namespaces that
//! nothing but WordPress reads and that cannot represent a post body containing
//! certain control characters without CDATA gymnastics. This emits JSON: same
//! content, a format every tool already parses, and a schema that is checked by
//! the round-trip test rather than by hope.

use autumn_web::AutumnResult;
use autumn_web::prelude::*;
use autumn_web::reexports::axum::response::Response;
use serde::{Deserialize, Serialize};

use crate::capabilities::Capability;
use crate::content;
use crate::models::{NewPost, NewTerm};
use crate::plugins::{Action, do_action};
use crate::repositories::{
    AttachmentRepository as _, PostRepository as _, TermRepository as _, UserRepository as _,
};
use crate::require_capability;

use super::super::site::{Csrf, Repos};
use super::layout;

/// The export envelope.
#[derive(Debug, Serialize, Deserialize)]
pub struct Export {
    /// Bumped whenever the shape changes, so an importer can refuse a file it
    /// does not understand rather than silently dropping fields.
    pub version: u32,
    pub site_title: String,
    pub exported_at: chrono::DateTime<chrono::Utc>,
    pub terms: Vec<ExportTerm>,
    pub posts: Vec<ExportPost>,
    /// Attachment metadata rows. Defaulted so a version-2 file still imports:
    /// unlike `password`, whose absence would silently *unprotect* content,
    /// an absent media list means only that there is no media to restore.
    #[serde(default)]
    pub attachments: Vec<ExportAttachment>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ExportTerm {
    pub taxonomy: String,
    pub name: String,
    pub slug: String,
    #[serde(default)]
    pub description: String,
    /// The parent term's **slug**, for hierarchical taxonomies — same
    /// reasoning as a page's `parent`. Without it a restore silently flattens
    /// the category tree.
    #[serde(default)]
    pub parent: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ExportPost {
    pub post_type: String,
    pub title: String,
    pub slug: String,
    #[serde(default)]
    pub excerpt: String,
    #[serde(default)]
    pub body: String,
    pub status: String,
    /// `open` or `closed`. Carried because forcing every restored post to
    /// `open` silently reverses an author's decision to close discussion —
    /// a restore must not change the content's policy.
    #[serde(default = "default_comment_status")]
    pub comment_status: String,
    /// The post's password, empty when the post is not protected.
    ///
    /// Carried so a restore does not silently publish content that was
    /// protected — WordPress's WXR carries `wp:post_password` for the same
    /// reason. Adding it is what took the format to version 2: a version-1
    /// file has no password field, and since `import` refuses any version it
    /// does not recognise, there is no case where this is absent and has to be
    /// guessed at.
    pub password: String,
    /// The author's **username**, not their id: ids are meaningless across
    /// installations, and a username is what an importer can actually resolve.
    pub author: String,
    /// The parent page's **slug**, for hierarchical types — same reasoning as
    /// `author`. Without it a restore flattens the tree: a page reachable at
    /// `/about/team` comes back as `/team`, so every inbound link and
    /// canonical URL to it starts 404ing after a backup restore.
    #[serde(default)]
    pub parent: Option<String>,
    #[serde(default)]
    pub published_at: Option<chrono::NaiveDateTime>,
    /// Whether the post is pinned to the top of the blog index. Carried for the
    /// same reason as `comment_status`: it is an editorial decision, and a
    /// restore that silently unpins every sticky post has changed the site.
    #[serde(default)]
    pub sticky: bool,
    /// Hand-set ordering for hierarchical content. Same reasoning again: a
    /// restore that resets every page to `0` discards the navigation order an
    /// editor arranged by hand, and nothing tells them it happened.
    #[serde(default)]
    pub menu_order: i32,
    /// Term slugs, qualified by taxonomy.
    #[serde(default)]
    pub terms: Vec<ExportTermRef>,
    /// The featured image's **slug**, for the same reason `author` and `parent`
    /// are slugs: an attachment id means nothing in another database. Without
    /// it a restore silently dropped every featured image, and the association
    /// was unrecoverable even with the blob store backed up.
    #[serde(default)]
    pub featured_media: Option<String>,
}

/// An attachment's row, including the handle that locates its bytes.
///
/// The bytes themselves are not in here and cannot be: a blob lives in the blob
/// store, which is backed up separately (it is object storage in a real
/// deployment). But the *handle* — the provider id and the stable key — must
/// be, or restoring those bytes accomplishes nothing: the database would have
/// no way to name them, `Attachment::blob()` would fail, and `/media/{slug}`
/// would answer 500 rather than serving the file that is sitting right there.
///
/// So this carries both halves: the display metadata a human needs and the
/// `file` handle the store needs. Restore the blob store's contents and the
/// media is whole; restore only the database and every image is a 500 — which
/// is why `file` is `Option` and why the importer says so when it is absent.
#[derive(Debug, Serialize, Deserialize)]
pub struct ExportAttachment {
    pub slug: String,
    pub title: String,
    #[serde(default)]
    pub mime_type: String,
    #[serde(default)]
    pub byte_size: i64,
    #[serde(default)]
    pub width: Option<i32>,
    #[serde(default)]
    pub height: Option<i32>,
    #[serde(default)]
    pub alt_text: String,
    #[serde(default)]
    pub caption: String,
    /// The stored blob handle: provider id, key, content type, size, etag.
    /// Absent only for a row whose file was already missing.
    #[serde(default)]
    pub file: Option<autumn_web::storage::Blob>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ExportTermRef {
    pub taxonomy: String,
    pub slug: String,
}

/// The ids of the terms an exported post names, for those that exist here.
///
/// Shared by the creation path and the resume path, so a retry files a post
/// under exactly what a first run would have. Terms are matched by
/// `(taxonomy, slug)` — an id from another installation means nothing — and one
/// the destination does not have is skipped rather than created, because an
/// import restores content and the taxonomy list is the site's own.
async fn resolve_import_terms(repos: &Repos, post: &ExportPost) -> AutumnResult<Vec<i64>> {
    let mut term_ids = Vec::new();
    for reference in &post.terms {
        if let Some(term) = repos
            .terms
            .find_by_slug(reference.slug.clone())
            .await?
            .into_iter()
            .find(|t| t.taxonomy == reference.taxonomy)
        {
            term_ids.push(term.id);
        }
    }
    Ok(term_ids)
}

/// `open`, matching the column default, for a file that predates the field.
fn default_comment_status() -> String {
    "open".to_owned()
}

/// The current export schema version.
///
/// Bumped to 2 when `ExportPost` gained `password` and `parent`. The importer
/// refuses a version it does not recognise rather than reading a file with
/// fields missing, which is what makes the password's presence an invariant
/// rather than something the restore path has to guess at — a version-1 file's
/// protected posts would otherwise have been restored as public.
///
/// Bumped to 3 for `attachments` and `ExportPost::featured_media`. Version 2 is
/// still *read*, because that addition is not the same kind of change: a
/// missing password would have silently unprotected content, whereas a missing
/// media list only means there is none to restore. Refusing version 2 outright
/// would strand backups for no safety gain.
pub const EXPORT_VERSION: u32 = 3;

/// The versions this site can read.
const READABLE_EXPORT_VERSIONS: &[u32] = &[2, 3];

#[get("/admin/tools")]
pub async fn show(repos: Repos, session: Session, csrf: Csrf) -> AutumnResult<Response> {
    let user = require_capability!(repos, session, csrf, Capability::ExportContent);
    let body = html! {
        div class="grid grid-cols-1 lg:grid-cols-2 gap-6 max-w-4xl" {
            section class="bg-white rounded-lg shadow p-5" {
                h2 class="font-semibold mb-2" { "Export" }
                p class="text-sm text-gray-600 mb-4" {
                    "Download every post, page and term as JSON. Media files are not included — \
                     the export references them, and the blob store is backed up separately."
                }
                a href="/admin/tools/export"
                  class="inline-block px-4 py-2 bg-indigo-600 text-white rounded \
                         hover:bg-indigo-700" {
                    "Download export"
                }
            }
            section class="bg-white rounded-lg shadow p-5" {
                h2 class="font-semibold mb-2" { "Import" }
                p class="text-sm text-gray-600 mb-4" {
                    "Paste an export file. Content is matched on (type, slug): an existing item \
                     is left alone rather than duplicated, so re-running an import is safe."
                }
                form action="/admin/tools/import" method="post" class="space-y-3" {
                    (csrf.input())
                    label for="payload" class="sr-only" { "Export JSON" }
                    textarea #payload name="payload" rows="8" required
                             placeholder="{\"version\": 3, …}"
                             class="w-full border rounded px-3 py-2 font-mono text-xs" {}
                    button type="submit"
                           class="px-4 py-2 border rounded bg-white hover:bg-gray-50" {
                        "Import"
                    }
                }
            }
        }
    };
    Ok(layout(&user, &csrf, "/admin/tools", "Tools", body).into_response())
}

#[get("/admin/tools/export")]
pub async fn export(repos: Repos, session: Session, csrf: Csrf) -> AutumnResult<Response> {
    let _user = require_capability!(repos, session, csrf, Capability::ExportContent);
    let settings = repos.settings().await?;

    let mut terms = Vec::new();
    for taxonomy in crate::content_types::all_taxonomies() {
        let in_taxonomy = repos
            .terms
            .find_by_taxonomy(taxonomy.slug.to_owned())
            .await?;
        for term in &in_taxonomy {
            // Resolve the parent to a slug now, while the ids still mean
            // something in this database.
            let parent = term.parent_id.and_then(|parent_id| {
                in_taxonomy
                    .iter()
                    .find(|candidate| candidate.id == parent_id)
                    .map(|parent| parent.slug.clone())
            });
            terms.push(ExportTerm {
                taxonomy: term.taxonomy.clone(),
                name: term.name.clone(),
                slug: term.slug.clone(),
                description: term.description.clone(),
                parent,
            });
        }
    }

    let mut posts = Vec::new();
    for post_type in crate::content_types::all_post_types() {
        for post in repos
            .posts
            .find_by_post_type(post_type.slug.to_owned())
            .await?
        {
            // Trash is deliberately excluded: an export is a backup of the
            // site's content, and restoring somebody's deleted drafts into a
            // fresh install is a surprise, not a feature.
            if post.status == "trash" {
                continue;
            }
            // Propagated, not swallowed. `.ok()` turned a database error into an
            // empty username and still produced a file that *looks* like a
            // valid backup — importing it silently reassigns the post to
            // whoever ran the import. A backup that is wrong in a way nobody
            // can see is worse than an export that fails loudly.
            let author = repos
                .users
                .find_by_id(post.author_id)
                .await?
                .map(|u| u.username)
                .unwrap_or_default();
            // The parent's slug, resolved now while the ids still mean
            // something in this database.
            // Same reasoning as `author`: a swallowed error here flattens the
            // page tree in the backup, so a restore puts `/about/team` back at
            // `/team` and every link to it starts 404ing.
            let parent_slug = match post.parent_id {
                Some(parent_id) => repos.posts.find_by_id(parent_id).await?.map(|p| p.slug),
                None => None,
            };
            let assigned = repos.post_terms(post.id).await?;
            // The featured image by slug, resolved now while the ids still mean
            // something in this database.
            // And here: swallowing drops the featured image from the backup.
            let featured_media = match post.featured_media_id {
                Some(media_id) => repos
                    .attachments
                    .find_by_id(media_id)
                    .await?
                    .map(|attachment| attachment.slug),
                None => None,
            };
            posts.push(ExportPost {
                post_type: post.post_type.clone(),
                title: post.title.clone(),
                slug: post.slug.clone(),
                excerpt: post.excerpt.clone(),
                body: post.body.clone(),
                status: post.status.clone(),
                comment_status: post.comment_status.clone(),
                password: post.password.clone(),
                author,
                parent: parent_slug,
                published_at: post.published_at,
                sticky: post.sticky,
                menu_order: post.menu_order,
                terms: assigned
                    .iter()
                    .map(|t| ExportTermRef {
                        taxonomy: t.taxonomy.clone(),
                        slug: t.slug.clone(),
                    })
                    .collect(),
                featured_media,
            });
        }
    }

    // Metadata only — the bytes live in the blob store, which is backed up
    // separately. Carrying the rows is what lets a restore resolve
    // `/media/{slug}` and re-attach featured images once those bytes are back.
    let mut attachments = Vec::new();
    for attachment in repos.attachments.find_all().await? {
        attachments.push(ExportAttachment {
            slug: attachment.slug.clone(),
            title: attachment.title.clone(),
            mime_type: attachment.mime_type.clone(),
            byte_size: attachment.byte_size,
            width: attachment.width,
            height: attachment.height,
            alt_text: attachment.alt_text.clone(),
            caption: attachment.caption.clone(),
            file: attachment.file.clone(),
        });
    }

    let payload = Export {
        version: EXPORT_VERSION,
        site_title: settings.site_title.clone(),
        exported_at: chrono::Utc::now(),
        terms,
        posts,
        attachments,
    };

    let body = serde_json::to_vec_pretty(&payload)
        .map_err(|err| AutumnError::internal_server_error_msg(err.to_string()))?;
    Ok(autumn_web::download::Download::from_bytes(body)
        .content_type("application/json")
        .filename(format!(
            "{}-export.json",
            autumn_web::slugify(&settings.site_title)
        ))
        .into_response())
}

/// One term of a taxonomy, by slug.
async fn term_by_slug(
    repos: &Repos,
    taxonomy: &str,
    slug: &str,
) -> AutumnResult<Option<crate::models::Term>> {
    Ok(repos
        .terms
        .find_by_slug(slug.to_owned())
        .await?
        .into_iter()
        .find(|candidate| candidate.taxonomy == taxonomy))
}

#[derive(Deserialize)]
pub struct ImportForm {
    pub payload: String,
}

#[post("/admin/tools/import")]
pub async fn import(
    repos: Repos,
    session: Session,
    csrf: Csrf,
    Form(form): Form<ImportForm>,
) -> AutumnResult<Response> {
    let user = require_capability!(repos, session, csrf, Capability::ImportContent);

    let payload: Export = serde_json::from_str(&form.payload)
        .map_err(|err| AutumnError::unprocessable_msg(format!("Not a valid export file: {err}")))?;
    if !READABLE_EXPORT_VERSIONS.contains(&payload.version) {
        return Err(AutumnError::unprocessable_msg(format!(
            "This export is version {}; this site reads {READABLE_EXPORT_VERSIONS:?}",
            payload.version
        )));
    }

    // Terms first: posts reference them, and creating them up front means one
    // pass over the posts rather than two.
    //
    // Which terms this run created, so the ancestry pass below can restrict
    // itself to them and leave a locally-managed hierarchy alone.
    let mut created_terms: std::collections::HashSet<i64> = std::collections::HashSet::new();
    for term in &payload.terms {
        let existing = repos
            .terms
            .find_by_slug(term.slug.clone())
            .await?
            .into_iter()
            .any(|t| t.taxonomy == term.taxonomy);
        if !existing {
            let created = repos
                .terms
                .save(&NewTerm {
                    taxonomy: term.taxonomy.clone(),
                    name: term.name.clone(),
                    slug: term.slug.clone(),
                    description: term.description.clone(),
                    // Linked in the second pass below: a child term can appear
                    // in the file before its parent.
                    parent_id: None,
                })
                .await?;
            created_terms.insert(created.id);
        }
    }

    // Re-link taxonomy ancestry. Hierarchical categories are supported, so a
    // restore that flattened them would quietly change every archive's shape.
    //
    // Only for terms *this run created*. A term the destination already had is
    // locally managed — the first pass deliberately leaves its name and
    // description alone, and moving it under the backup's parent would be the
    // same contradiction: an import that says it skips existing rows, silently
    // restructuring somebody's category tree (or closing a cycle with it). This
    // is the taxonomy-shaped twin of the post ancestry rule.
    for term in &payload.terms {
        let Some(parent_slug) = &term.parent else {
            continue;
        };
        let child = term_by_slug(&repos, &term.taxonomy, &term.slug).await?;
        let parent = term_by_slug(&repos, &term.taxonomy, parent_slug).await?;
        let (Some(child), Some(parent)) = (child, parent) else {
            continue;
        };
        if !created_terms.contains(&child.id) {
            continue;
        }
        if child.id != parent.id && child.parent_id != Some(parent.id) {
            repos
                .terms
                .update(
                    child.id,
                    &crate::models::UpdateTerm {
                        taxonomy: autumn_web::hooks::Patch::Unchanged,
                        name: autumn_web::hooks::Patch::Unchanged,
                        slug: autumn_web::hooks::Patch::Unchanged,
                        description: autumn_web::hooks::Patch::Unchanged,
                        parent_id: autumn_web::hooks::Patch::Set(Some(parent.id)),
                    },
                )
                .await?;
        }
    }

    // Attachment metadata first, so posts can reference it. Matched by slug —
    // the same "ids mean nothing across installations" rule the author and
    // parent references follow. A row already present is left alone rather than
    // overwritten: the site's own metadata is more current than the file's.
    //
    // The blob *handle* comes with the row. Restoring only the display metadata
    // was not enough: the handle is what names the bytes in the store, so
    // without it `Attachment::blob()` fails and `/media/{slug}` answers 500 —
    // restoring the separately backed-up store would not have fixed a single
    // image, because nothing in the database could point at it.
    let mut media_ids: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    for attachment in &payload.attachments {
        // The same media policy the upload path applies. An export is a file,
        // and a tampered one can label bytes already in the store as
        // `text/html`. Serving is defended separately (`may_render_inline`
        // refuses to inline anything off the allowlist), but a restore should
        // also *say* it found something it will not accept rather than quietly
        // storing a row nobody can use.
        if !crate::routes::admin::media::is_allowed_mime(&attachment.mime_type) {
            return Err(AutumnError::unprocessable_msg(format!(
                "`{}` claims the unsupported media type {:?}; this export cannot be \
                 restored as it stands",
                attachment.slug, attachment.mime_type
            )));
        }
        let existing = repos
            .attachments
            .find_by_slug(attachment.slug.clone())
            .await?
            .into_iter()
            .next();
        let id = match existing {
            Some(found) => found.id,
            None => {
                repos
                    .attachments
                    .save(&crate::models::NewAttachment {
                        title: attachment.title.clone(),
                        slug: attachment.slug.clone(),
                        file: attachment.file.clone(),
                        mime_type: attachment.mime_type.clone(),
                        byte_size: attachment.byte_size,
                        width: attachment.width,
                        height: attachment.height,
                        alt_text: attachment.alt_text.clone(),
                        caption: attachment.caption.clone(),
                        uploader_id: Some(user.id),
                    })
                    .await?
                    .id
            }
        };
        media_ids.insert(attachment.slug.clone(), id);
    }

    // Every `(post_type, source slug)` a previous run of this importer recorded.
    // Loaded once: the alternative is a lookup per post, and an import is a
    // bulk operation.
    let imported_source_slugs = repos
        .with_conn(async |conn| content::imported_source_slugs(conn).await)
        .await?;

    let mut imported = 0_usize;
    let mut skipped = 0_usize;
    let mut orphaned = 0_i64;
    // (created id, post type, the slug AS WRITTEN IN THE FILE, parent slug).
    // The written slug is the key the file's `parent` references use; the row
    // may have been given a different one to avoid colliding with content
    // already on this site.
    let mut created_ids: Vec<(i64, String, String, Option<String>)> = Vec::new();
    // Posts whose status was moved out of `draft`, so their transition action
    // can fire once the ancestry pass has finished — see the dispatch below.
    let mut transitioned_ids: Vec<i64> = Vec::new();
    for post in &payload.posts {
        // Idempotent on the slug *the file names*, not only on the slug the
        // row ended up with. Those differ whenever the allocator had to add a
        // suffix — an imported `about` landing as `about-2` because a post
        // already held the bare path — which is exactly the
        // partly-populated-site case this import is for. Checking only
        // `(post_type, about)` found nothing on a retry and created `about-3`,
        // so the advertised idempotent re-run duplicated content precisely
        // where the allocator had done its job. The source slug is recorded in
        // `post_meta` at import time and consulted here.
        // A row a previous run of *this* importer created, identified by the
        // marker rather than by the slug. That distinction is the whole point:
        // a marker says "this row is ours, finish it", while a bare slug match
        // says only "something local is already called that".
        let marker_owned =
            match imported_source_slugs.get(&(post.post_type.clone(), post.slug.clone())) {
                Some(id) => repos.posts.find_by_id(*id).await?,
                None => None,
            };
        let slug_taken = repos
            .posts
            .find_by_slug(post.slug.clone())
            .await?
            .into_iter()
            .any(|p| p.post_type == post.post_type);

        if let Some(ours) = marker_owned {
            // Ours, so a previous run may have left it unfinished: reapply the
            // terms and the status, and offer it to the ancestry pass. The
            // marker commits before that work, so a failure in between leaves
            // exactly this state, and a retry that only skipped would never
            // repair it.
            skipped += 1;
            let term_ids = resolve_import_terms(&repos, post).await?;
            let wanted_status = post.status.clone();
            let ours_id = ours.id;
            let current_status = ours.status.clone();
            let transitioned = repos
                .with_conn(async |conn| {
                    use diesel_async::AsyncConnection as _;
                    conn.transaction(async move |conn| {
                        content::set_post_terms(conn, ours_id, term_ids).await?;
                        if wanted_status != current_status {
                            content::transition_status(
                                conn,
                                ours_id,
                                &wanted_status,
                                Some(user.id),
                                None,
                            )
                            .await?;
                            return Ok::<_, AutumnError>(true);
                        }
                        Ok::<_, AutumnError>(false)
                    })
                    .await
                })
                .await?;
            if transitioned {
                transitioned_ids.push(ours.id);
            }
            created_ids.push((
                ours.id,
                post.post_type.clone(),
                post.slug.clone(),
                post.parent.clone(),
            ));
            continue;
        }

        if slug_taken {
            // Somebody else's row that merely shares the slug. Left completely
            // alone — an import that says it skips existing items must not then
            // re-parent them, which would move a local page and change its
            // canonical URL. Offering these to the ancestry pass was a
            // regression in the previous round's retry fix.
            skipped += 1;
            continue;
        }

        // An unknown author is attributed to the importing account rather than
        // dropped: content with no author is unaddressable, and inventing an
        // account would be worse.
        let author_id = repos
            .users
            .find_by_username(post.author.clone())
            .await?
            .into_iter()
            .next()
            .map_or(user.id, |u| u.id);

        // Everything lands as a draft first and is transitioned afterwards, so
        // the state machine sees every move into a published status — an import
        // cannot write a status the UI could not reach.
        // Through the shared allocator: the bare-path index is enforced by the
        // database, so importing a page whose slug an existing post already
        // holds would otherwise abort the restore part-way, after earlier rows
        // had committed.
        let created = repos
            .save_post_with_unique_slug(NewPost {
                post_type: post.post_type.clone(),
                title: post.title.clone(),
                slug: post.slug.clone(),
                excerpt: post.excerpt.clone(),
                body: post.body.clone(),
                status: "draft".to_owned(),
                author_id,
                parent_id: None,
                menu_order: post.menu_order,
                // Resolved against the media restored above, falling back to a
                // row already on this site with that slug — importing into a
                // populated site should re-attach to the image that is already
                // there rather than dropping the association.
                featured_media_id: match &post.featured_media {
                    Some(slug) => match media_ids.get(slug) {
                        Some(id) => Some(*id),
                        None => repos
                            .attachments
                            .find_by_slug(slug.clone())
                            .await?
                            .into_iter()
                            .next()
                            .map(|attachment| attachment.id),
                    },
                    None => None,
                },
                comment_status: post.comment_status.clone(),
                password: post.password.clone(),
                sticky: post.sticky,
                published_at: post.published_at,
            })
            .await?;

        let term_ids = resolve_import_terms(&repos, post).await?;
        // The terms and the transition commit together. They were separate
        // transactions after an already-committed insert, so a failure in
        // either left the post present but unfinished — and the dedupe above
        // then reported it "already present" on every retry, so the missing
        // status and taxonomy were never restored. A backup that restores
        // silently-partial content is worse than one that fails.
        //
        // The insert itself stays outside: it goes through
        // `save_post_with_unique_slug`, whose retry-on-collision needs its own
        // connection. A failure after it leaves a draft with no source marker,
        // which the next run treats as an ordinary slug collision and re-imports
        // beside — visible, rather than silently skipped.
        let wanted_status = post.status.clone();
        let source_slug = post.slug.clone();
        let transitioned = repos
            .with_conn(async |conn| {
                use diesel_async::AsyncConnection as _;
                conn.transaction(async move |conn| {
                    // The marker joins this transaction. Writing it first, on
                    // its own, meant a failure left an *unmarked* draft — which
                    // the next run reads as unrelated local content and skips
                    // forever, so its terms, status and ancestry are never
                    // restored. Marked-and-unfinished is recoverable;
                    // unmarked-and-unfinished is not.
                    content::record_import_source(conn, created.id, &source_slug).await?;
                    content::set_post_terms(conn, created.id, term_ids).await?;
                    if wanted_status != "draft" {
                        content::transition_status(
                            conn,
                            created.id,
                            &wanted_status,
                            Some(user.id),
                            None,
                        )
                        .await?;
                        return Ok::<_, AutumnError>(true);
                    }
                    Ok::<_, AutumnError>(false)
                })
                .await
            })
            .await;

        // The row is removed if any of that failed, so the file is never left
        // with an unmarked half-import that the next run cannot recognise. The
        // insert cannot join the transaction — `save_post_with_unique_slug`
        // retries on its own connection — so unwinding is what makes each
        // post's import all-or-nothing.
        let transitioned = match transitioned {
            Ok(transitioned) => transitioned,
            Err(error) => {
                if let Err(cleanup) = repos.posts.delete_by_id(created.id).await {
                    autumn_web::reexports::tracing::warn!(
                        %cleanup,
                        post_id = created.id,
                        "failed to remove a post whose import could not be completed"
                    );
                }
                return Err(error);
            }
        };
        if transitioned {
            transitioned_ids.push(created.id);
        }
        created_ids.push((
            created.id,
            post.post_type.clone(),
            post.slug.clone(),
            post.parent.clone(),
        ));
        imported += 1;
    }

    // Re-link ancestry in a second pass: a child can appear in the file before
    // its parent, so the parent's row may not exist during the first. Resolve
    // through the file's own slugs rather than by re-querying, because a row
    // may have been given a different slug on the way in.
    let by_file_slug: std::collections::HashMap<(&str, &str), i64> = created_ids
        .iter()
        .map(|(id, post_type, file_slug, _)| ((post_type.as_str(), file_slug.as_str()), *id))
        .collect();
    for (child_id, post_type, _, parent_slug) in &created_ids {
        let Some(parent_slug) = parent_slug else {
            continue;
        };
        // Prefer a row created by this run; fall back to one already on the
        // site. Importing into a partly-populated site is the common restore
        // shape, and there the parent is *skipped* as already-present — so it
        // is absent from the map, and consulting only the map would drop the
        // child to the top level and change its canonical path.
        let parent_id = match by_file_slug.get(&(post_type.as_str(), parent_slug.as_str())) {
            Some(id) => Some(*id),
            None => repos
                .posts
                .find_by_slug(parent_slug.clone())
                .await?
                .into_iter()
                .find(|candidate| candidate.post_type == *post_type)
                .map(|parent| parent.id),
        };
        if let Some(parent_id) = parent_id
            && parent_id != *child_id
            && !repos
                .with_conn(async |conn| content::set_post_parent(conn, *child_id, parent_id).await)
                .await?
        {
            // `set_post_parent` applies the editor's own parent rules — a live
            // row of the same type, no cycle, within `MAX_PAGE_DEPTH` — and
            // declines rather than raising. A resolved-by-slug parent can fail
            // any of them when importing into a partly-populated site, and
            // writing the link anyway produced a child the resolver could not
            // reach at its own canonical URL. The child lands at the top level
            // instead, and the run says how often that happened rather than
            // aborting half-restored.
            orphaned += 1;
        }
    }

    // Fired only now, after the ancestry pass. A listener that indexes or
    // caches a post's permalink needs the parent link already in place: firing
    // during the creation loop recorded `/child` for a page whose canonical URL
    // is `/parent/child`, and nothing ever told it the URL had changed.
    //
    // The same actions the admin, API and scheduler paths fire — an import is
    // how a site's content arrives after a restore or a migration, which is the
    // worst possible moment for a search index to be blind to it.
    for id in transitioned_ids {
        do_action(Action::PostTransitioned, id);
    }
    for (id, _, _, _) in &created_ids {
        do_action(Action::PostSaved, *id);
    }

    let body = html! {
        div class="bg-white rounded-lg shadow p-5 max-w-lg" {
            h2 class="font-semibold mb-2" { "Import complete" }
            p class="text-sm text-gray-700" {
                (imported) " imported, " (skipped) " already present."
            }
            @if orphaned > 0 {
                p class="text-sm text-amber-700 mt-2" {
                    (autumn_web::format::pluralize(orphaned, "item"))
                    " could not keep its parent — the named parent is missing, \
                     trashed, of another type, or already nested as deeply as \
                     pages go. They were imported at the top level."
                }
            }
            p class="mt-4" {
                a href="/admin/tools" class="text-indigo-700 hover:underline text-sm" {
                    "← Back to Tools"
                }
            }
        }
    };
    Ok(layout(&user, &csrf, "/admin/tools", "Import", body).into_response())
}
