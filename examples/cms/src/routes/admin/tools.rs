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
use crate::models::NewPost;
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
    /// A page's **full path** (`about/team`), which is what identifies it.
    ///
    /// A slug alone stopped being an identity when nested page slugs became
    /// unique per parent rather than globally: `/about/team` and
    /// `/company/team` are both legitimate and both carry the slug `team`, so
    /// an importer keyed on `(post_type, slug)` restored the first and skipped
    /// the second as already present — losing a page out of the site's own
    /// backup. Added in version 4; absent from a version-2 or -3 file, where
    /// the slug was the identity because the schema made it one.
    #[serde(default)]
    pub path: Option<String>,
    /// The parent page's **slug**, for hierarchical types — same reasoning as
    /// `author`. Without it a restore flattens the tree: a page reachable at
    /// `/about/team` comes back as `/team`, so every inbound link and
    /// canonical URL to it starts 404ing after a backup restore.
    ///
    /// Retained alongside `path` for version-2 and -3 files, which have no
    /// `path`; for a version-4 page the parent is the path's own prefix, which
    /// is what `parent_identity` reads.
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
    /// The post's retained revision history, oldest first.
    ///
    /// Carried in version 5 with the comments and the custom fields. Revisions
    /// are a supported feature of this CMS, and a restore that drops them takes
    /// away the ability to roll content back — silently, since nothing on the
    /// restored post says its history used to exist.
    #[serde(default)]
    pub revisions: Vec<ExportRevision>,
    /// Custom fields, as `key` → `value` pairs.
    ///
    /// Carried in version 5 alongside the comments. A plugin storing per-post
    /// data through the `PostMeta` repository had it silently dropped by a
    /// backup-and-restore, with nothing in the file or the report to say so.
    /// The importer's own private keys are excluded on the way out and refused
    /// on the way in — see `content::INTERNAL_META_KEYS`.
    #[serde(default)]
    pub meta: Vec<ExportMeta>,
    /// The post's discussion, nested as it is rendered.
    ///
    /// Carried in version 5. Without it a backup restored a site with every
    /// thread gone and every comment count at zero — approved discussion,
    /// the moderation queue, and the spam decisions a moderator had already
    /// made, none of them recoverable from the file. WordPress's WXR carries
    /// `wp:comment` for exactly this reason.
    #[serde(default)]
    pub comments: Vec<ExportComment>,
}

/// One retained revision in an export file.
#[derive(Debug, Serialize, Deserialize)]
pub struct ExportRevision {
    /// The editor's **username**, absent when the snapshot recorded no author
    /// (the scheduler and the seeder write some).
    #[serde(default)]
    pub author: Option<String>,
    pub title: String,
    #[serde(default)]
    pub excerpt: String,
    #[serde(default)]
    pub body: String,
    pub status: String,
    #[serde(default)]
    pub summary: String,
    pub created_at: chrono::NaiveDateTime,
}

/// One custom field in an export file.
#[derive(Debug, Serialize, Deserialize)]
pub struct ExportMeta {
    pub key: String,
    #[serde(default)]
    pub value: String,
}

/// One comment in an export file.
///
/// Nesting is a tree rather than a parent id, because an id means nothing in
/// another database — the same reasoning that makes `author` a username and
/// `parent` a slug elsewhere in this format.
#[derive(Debug, Serialize, Deserialize)]
pub struct ExportComment {
    /// The registered account's **username**, absent for a guest.
    #[serde(default)]
    pub author: Option<String>,
    /// The name and email as stored on the comment. A registered commenter
    /// renders under their current account name, but these are what the row
    /// carries and what a guest comment is identified by.
    #[serde(default)]
    pub author_name: String,
    #[serde(default)]
    pub author_email: String,
    #[serde(default)]
    pub author_url: String,
    pub body: String,
    /// `approved`, `pending`, `spam` or `trash` — the moderation decision,
    /// which is work a restore must not discard.
    pub status: String,
    pub created_at: chrono::NaiveDateTime,
    #[serde(default)]
    pub replies: Vec<ExportComment>,
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
    /// The uploader's **username**, for the reason a post's author is one.
    ///
    /// Media deletion lets an Author remove only files whose `uploader_id` is
    /// theirs, so a restore that dropped this handed every file to whoever ran
    /// the import — the original uploader losing control of their own uploads,
    /// silently, in a workflow that is supposed to put the site back.
    #[serde(default)]
    pub uploader: Option<String>,
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
pub const EXPORT_VERSION: u32 = 5;

/// The versions this site can read.
const READABLE_EXPORT_VERSIONS: &[u32] = &[2, 3, 4, 5];

/// How much of the request budget multipart framing and the CSRF field may use.
///
/// The part cap is the configured request limit minus this, so an operator has
/// exactly one knob — `security.upload.max_request_size_bytes` — and raising it
/// raises what the importer accepts. A second compile-time constant here would
/// have made that knob ineffective, which is the whole complaint about a
/// hard-coded cap: the exporter is unbounded, so a fixed number is a size of
/// backup the CMS can create and cannot restore, with no way out.
const IMPORT_FRAMING_HEADROOM: usize = 64 * 1024;

/// The largest export this deployment can accept, from its own configuration.
fn max_import_bytes(state: &AppState) -> usize {
    state
        .config()
        .security
        .upload
        .max_request_size_bytes
        .saturating_sub(IMPORT_FRAMING_HEADROOM)
}

#[get("/admin/tools")]
pub async fn show(
    repos: Repos,
    session: Session,
    csrf: Csrf,
    State(state): State<AppState>,
) -> AutumnResult<Response> {
    let user = require_capability!(repos, session, csrf, Capability::ExportContent);
    // Read from configuration rather than a constant, so the number the screen
    // states is the number the handler enforces.
    let max_import_bytes = max_import_bytes(&state);
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
                    "Upload an export file. Content is matched on (type, slug): an existing item \
                     is left alone rather than duplicated, so re-running an import is safe."
                }
                form action="/admin/tools/import" method="post"
                     enctype="multipart/form-data" class="space-y-3" {
                    (csrf.input())
                    label for="payload" class="sr-only" { "Export file" }
                    input #payload type="file" name="payload" required accept="application/json"
                          class="w-full border rounded px-3 py-2 text-sm";
                    p class="text-xs text-gray-400" {
                        "Up to " (max_import_bytes / (1024 * 1024)) " MB, from \
                         `security.upload.max_request_size_bytes` in autumn.toml — raise that \
                         and the importer follows."
                    }
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

    // Every read on one repeatable-read snapshot. They were separate
    // repository calls, each on its own pooled connection and therefore its own
    // snapshot: renaming a page while the export ran could write that page into
    // the file under its old slug and, a few rows later, name it as a child's
    // ancestor under the new one. The restore then cannot resolve the parent
    // and files the child at the top level — a backup that is wrong in a way
    // nobody can see, which is the failure mode a backup exists to prevent.
    let rows = {
        let mut conn = repos.conn().await?;
        crate::content::export_snapshot(&mut conn).await?
    };

    let mut terms = Vec::new();
    for (_, in_taxonomy) in &rows.terms_by_taxonomy {
        for term in in_taxonomy {
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
    for post in &rows.posts {
        // Propagated, not swallowed. `.ok()` turned a database error into an
        // empty username and still produced a file that *looks* like a valid
        // backup — importing it silently reassigns the post to whoever ran the
        // import. An account deleted between the read and now cannot happen at
        // all any more: the snapshot answers both questions at one instant.
        let author = rows
            .usernames
            .get(&post.author_id)
            .cloned()
            .unwrap_or_default();
        // The parent's slug, resolved from the same snapshot. Swallowing a
        // failure here flattens the page tree in the backup, so a restore puts
        // `/about/team` back at `/team` and every link to it starts 404ing.
        let parent_slug = post
            .parent_id
            .and_then(|parent_id| rows.posts_by_id.get(&parent_id))
            .map(|parent| parent.slug.clone());
        // The full path, which is what identifies a page now that a nested slug
        // is only unique among its siblings.
        let path = if post.post_type == "page" {
            let ancestry = rows.ancestry(post);
            Some(if ancestry.is_empty() {
                post.slug.clone()
            } else {
                format!("{}/{}", ancestry.join("/"), post.slug)
            })
        } else {
            None
        };
        let assigned = rows.terms_by_post.get(&post.id);
        // The featured image by slug, resolved from the same snapshot: a
        // dropped one is a missing image on every restore of this file.
        let featured_media = post
            .featured_media_id
            .and_then(|media_id| rows.attachments_by_id.get(&media_id))
            .map(|attachment| attachment.slug.clone());
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
            path,
            parent: parent_slug,
            published_at: post.published_at,
            sticky: post.sticky,
            menu_order: post.menu_order,
            terms: assigned
                .into_iter()
                .flatten()
                .map(|t| ExportTermRef {
                    taxonomy: t.taxonomy.clone(),
                    slug: t.slug.clone(),
                })
                .collect(),
            featured_media,
            revisions: rows
                .revisions_by_post
                .get(&post.id)
                .into_iter()
                .flatten()
                .map(|revision| ExportRevision {
                    author: revision
                        .author_id
                        .and_then(|id| rows.usernames.get(&id).cloned()),
                    title: revision.title.clone(),
                    excerpt: revision.excerpt.clone(),
                    body: revision.body.clone(),
                    status: revision.status.clone(),
                    summary: revision.summary.clone(),
                    created_at: revision.created_at,
                })
                .collect(),
            meta: rows
                .meta_by_post
                .get(&post.id)
                .into_iter()
                .flatten()
                .map(|(key, value)| ExportMeta {
                    key: key.clone(),
                    value: value.clone(),
                })
                .collect(),
            // Nested from the flat rows, which are already in creation order.
            comments: export_comments(rows.comments_by_post.get(&post.id), &rows.usernames),
        });
    }

    // Metadata only — the bytes live in the blob store, which is backed up
    // separately. Carrying the rows is what lets a restore resolve
    // `/media/{slug}` and re-attach featured images once those bytes are back.
    let mut attachments = Vec::new();
    for attachment in &rows.attachments {
        attachments.push(ExportAttachment {
            uploader: attachment
                .uploader_id
                .and_then(|id| rows.usernames.get(&id).cloned()),
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

/// The file's revisions, in the shape `content::import_revisions` restores.
fn imported_revisions(revisions: &[ExportRevision]) -> Vec<content::ImportedRevision> {
    revisions
        .iter()
        .map(|revision| content::ImportedRevision {
            author_username: revision.author.clone(),
            title: revision.title.clone(),
            excerpt: revision.excerpt.clone(),
            body: revision.body.clone(),
            status: revision.status.clone(),
            summary: revision.summary.clone(),
            created_at: revision.created_at,
        })
        .collect()
}

/// The file's custom fields, in the shape `content::import_post_meta` restores.
fn imported_meta(meta: &[ExportMeta]) -> Vec<(String, String)> {
    meta.iter()
        .map(|field| (field.key.clone(), field.value.clone()))
        .collect()
}

/// The file's comment tree, in the shape `content::import_comments` restores.
fn imported_comments(comments: &[ExportComment]) -> Vec<content::ImportedComment> {
    comments
        .iter()
        .map(|comment| content::ImportedComment {
            author_username: comment.author.clone(),
            author_name: comment.author_name.clone(),
            author_email: comment.author_email.clone(),
            author_url: comment.author_url.clone(),
            body: comment.body.clone(),
            status: comment.status.clone(),
            created_at: comment.created_at,
            replies: imported_comments(&comment.replies),
        })
        .collect()
}

/// Nest a post's flat comment rows into the tree the file carries.
///
/// Built from the roots down, so a row whose parent is missing — a comment
/// whose parent was deleted by a direct write — is dropped rather than promoted
/// to a root it never was.
///
/// Indexed by parent once rather than filtered per node. Filtering the whole
/// slice at every call is quadratic, and it is quadratic in the thing an
/// unauthenticated visitor grows: a thread of ten thousand roots is a hundred
/// million comparisons, so producing a backup could monopolise a core or time
/// the administrator out. The tree is bounded by `MAX_COMMENT_DEPTH` but the
/// *breadth* is not, which is exactly the shape that has bitten the render path
/// twice in this review.
fn export_comments(
    rows: Option<&Vec<crate::models::Comment>>,
    usernames: &std::collections::HashMap<i64, String>,
) -> Vec<ExportComment> {
    let Some(rows) = rows else {
        return Vec::new();
    };
    let mut by_parent: std::collections::HashMap<Option<i64>, Vec<&crate::models::Comment>> =
        std::collections::HashMap::new();
    for row in rows {
        by_parent.entry(row.parent_id).or_default().push(row);
    }

    fn children(
        by_parent: &std::collections::HashMap<Option<i64>, Vec<&crate::models::Comment>>,
        parent: Option<i64>,
        depth: usize,
        usernames: &std::collections::HashMap<i64, String>,
    ) -> Vec<ExportComment> {
        // The same bound the write path enforces, so a cycle from a direct
        // write cannot make the export recurse forever.
        if depth > crate::content::MAX_COMMENT_DEPTH + 1 {
            return Vec::new();
        }
        by_parent
            .get(&parent)
            .into_iter()
            .flatten()
            .map(|row| ExportComment {
                author: row.author_id.and_then(|id| usernames.get(&id).cloned()),
                author_name: row.author_name.clone(),
                author_email: row.author_email.clone(),
                author_url: row.author_url.clone(),
                body: row.body.clone(),
                status: row.status.clone(),
                created_at: row.created_at,
                replies: children(by_parent, Some(row.id), depth + 1, usernames),
            })
            .collect()
    }
    children(&by_parent, None, 0, usernames)
}

/// What identifies a post inside an export file.
///
/// A page's full path, when the file carries one; its slug otherwise. The slug
/// alone stopped being an identity when nested page slugs became unique per
/// parent — `/about/team` and `/company/team` both carry `team`.
fn identity(post: &ExportPost) -> String {
    post.path.clone().unwrap_or_else(|| post.slug.clone())
}

/// The identity of a post's parent, as the file describes it.
///
/// For a version-4 page this is the path's own prefix, which is unambiguous.
/// For an older file it is the bare parent slug, which is the best that file
/// can say — and was unambiguous under the schema that wrote it.
fn parent_identity(post: &ExportPost) -> Option<String> {
    if let Some(path) = &post.path {
        let mut segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        segments.pop();
        return (!segments.is_empty()).then(|| segments.join("/"));
    }
    post.parent.clone()
}

/// The full path a stored post is addressed at, for comparing against a file's
/// identity.
async fn local_identity(repos: &Repos, post: &crate::models::Post) -> AutumnResult<String> {
    if post.post_type != "page" {
        return Ok(post.slug.clone());
    }
    let ancestry = repos.page_ancestry(post).await?;
    Ok(if ancestry.is_empty() {
        post.slug.clone()
    } else {
        format!("{}/{}", ancestry.join("/"), post.slug)
    })
}

/// A stored post of `post_type` whose own identity matches, if there is one.
async fn find_local(
    repos: &Repos,
    post_type: &str,
    identity: &str,
) -> AutumnResult<Option<crate::models::Post>> {
    // The last segment is the slug, which is what the index can find.
    let slug = identity.rsplit('/').next().unwrap_or(identity).to_owned();
    for candidate in repos
        .posts
        .find_by_slug(slug)
        .await?
        .into_iter()
        .filter(|candidate| candidate.post_type == post_type)
    {
        if local_identity(repos, &candidate).await? == identity {
            return Ok(Some(candidate));
        }
    }
    Ok(None)
}

/// The status an imported post should land in.
///
/// A backup restored after downtime routinely carries `future` posts whose time
/// has already passed. `transition_status` refuses that edge — correctly, since
/// a schedule in the past either never fires or fires on the next sweep — so
/// the import aborted the post and unwound it, and an otherwise valid backup
/// could not be restored without hand-editing its JSON. The scheduler would
/// have published these within a minute of their due time, so publishing them
/// now is what the file actually asked for.
fn import_status(status: &str, published_at: Option<chrono::NaiveDateTime>) -> &str {
    // The exporter excludes trash, so a file carrying it was hand-edited or
    // came from another tool. Restoring straight into the trash is meaningless
    // — a backup restores content, not deletions — and it is the one status
    // whose transition reaches for the page-hierarchy lock, which on this path
    // would be taken behind a post row lock `set_post_terms` already holds.
    // Landing it as a draft keeps the content and puts it somewhere visible.
    if status == "trash" {
        return "draft";
    }
    if status == "future" && published_at.is_none_or(|when| when <= chrono::Utc::now().naive_utc())
    {
        return "publish";
    }
    status
}

#[post("/admin/tools/import")]
pub async fn import(
    repos: Repos,
    session: Session,
    csrf: Csrf,
    State(state): State<AppState>,
    mut form: autumn_web::extract::Multipart,
) -> AutumnResult<Response> {
    let user = require_capability!(repos, session, csrf, Capability::ImportContent);
    let max_import_bytes = max_import_bytes(&state);

    // A file upload rather than a textarea, which is what the export already
    // hands the operator. The old form posted the JSON as a URL-encoded field:
    // every quote and brace became a three-byte escape, so a backup roughly a
    // third of the request limit already exceeded it — and the CMS could not
    // restore its own export under the shipped configuration. Multipart carries
    // the bytes as they are.
    let mut raw: Option<Vec<u8>> = None;
    while let Some(field) = form.next_field().await? {
        if field.name() == Some("payload") {
            if raw.is_some() {
                return Err(AutumnError::unprocessable_msg("Upload one file at a time"));
            }
            // Bounded read: the part is attacker-controlled length, and the cap
            // is what turns "too big" into a sentence rather than a truncated
            // parse.
            raw = Some(
                field
                    .with_max_bytes(max_import_bytes)
                    .bytes_limited()
                    .await?,
            );
        }
    }
    let Some(raw) = raw else {
        return Err(AutumnError::unprocessable_msg(
            "Choose an export file to import",
        ));
    };

    let payload: Export = serde_json::from_slice(&raw)
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
    // Creations and ancestry go together, in one transaction owned by
    // `content::import_terms`. Split across statements, a failure during the
    // linking half left the creations committed, and a retry then read every
    // one of those rows as pre-existing local content it must not restructure
    // — so the unfinished links were skipped permanently and the hierarchy
    // stayed flat while the retry reported the terms as already present.
    repos
        .with_conn(async |conn| {
            content::import_terms(
                conn,
                &payload
                    .terms
                    .iter()
                    .map(|term| content::ImportedTerm {
                        taxonomy: term.taxonomy.clone(),
                        name: term.name.clone(),
                        slug: term.slug.clone(),
                        description: term.description.clone(),
                        parent: term.parent.clone(),
                    })
                    .collect::<Vec<_>>(),
            )
            .await
        })
        .await?;

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
                        // The file's own uploader when this site has that
                        // account, falling back to the importer only when it
                        // does not. Attributing every restored file to whoever
                        // ran the import takes each Author's uploads out of
                        // their control — `delete_attachment` lets an Author
                        // remove only files whose `uploader_id` is theirs.
                        uploader_id: Some(match attachment.uploader.as_ref() {
                            Some(username) => repos
                                .users
                                .find_by_username(username.clone())
                                .await?
                                .into_iter()
                                .next()
                                .map_or(user.id, |owner| owner.id),
                            None => user.id,
                        }),
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
    // Which of those a previous run *finished*. The source marker is written
    // before the row's terms, status and ancestry are, so on its own it cannot
    // distinguish "ours, unfinished, repair it" from "ours, done, leave it".
    let completed_imports = repos
        .with_conn(async |conn| content::completed_import_ids(conn).await)
        .await?;

    let mut restored = 0_usize;
    let mut skipped = 0_usize;
    let mut comments_restored = 0_usize;
    let mut orphaned = 0_i64;
    // (created id, post type, the identity AS WRITTEN IN THE FILE, parent
    // identity). The written identity is the key the file's parent references
    // use; the row may have been given a different slug to avoid colliding with
    // content already on this site.
    let mut created_ids: Vec<(i64, String, String, Option<String>)> = Vec::new();
    // Posts whose status was moved out of `draft`, so their transition action
    // can fire once the ancestry pass has finished — see the dispatch below.
    let mut transitioned_ids: Vec<i64> = Vec::new();

    // Shallowest first, so a parent is created before its children.
    //
    // The order is not cosmetic. Every page used to be inserted at the top
    // level and re-parented in the pass below, which meant a nested page
    // transiently occupied the *bare-path* namespace — so restoring both
    // `/about/team` and `/company/team` gave the second the slug `team-2`, and
    // it stayed that way after re-parenting. Creating the parent first lets the
    // child be inserted where it belongs, where its slug only has to be unique
    // among its siblings.
    let mut ordered: Vec<&ExportPost> = payload.posts.iter().collect();
    ordered.sort_by_key(|post| identity(post).matches('/').count());

    for post in ordered {
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
        let file_identity = identity(post);
        // The parent, if this run has already created it or the site already
        // had it. `None` leaves the row at the top level for the pass below to
        // re-link — which is still needed for a parent the file names but does
        // not contain.
        let parent_now = match parent_identity(post) {
            Some(parent) => match created_ids
                .iter()
                .find(|(_, post_type, id, _)| post_type == &post.post_type && id == &parent)
            {
                Some((id, _, _, _)) => Some(*id),
                None => find_local(&repos, &post.post_type, &parent)
                    .await?
                    .map(|found| found.id),
            },
            None => None,
        };
        let marker_owned =
            match imported_source_slugs.get(&(post.post_type.clone(), file_identity.clone())) {
                Some(id) => repos.posts.find_by_id(*id).await?,
                None => None,
            };
        // Matched on the *path*, not the bare slug: a local `/about/team` does
        // not make the file's `/company/team` already present, and treating it
        // as such dropped a page out of the site's own backup.
        let slug_taken = find_local(&repos, &post.post_type, &file_identity)
            .await?
            .is_some();

        if let Some(ours) = marker_owned {
            skipped += 1;
            // A row an earlier run finished. Left completely alone, exactly
            // like somebody else's row below: re-applying the file's terms and
            // status here would silently undo an editor who has since re-filed
            // the post or moved it back to draft — on a screen whose whole
            // promise is that existing items are left alone. Reconciliation is
            // for *unfinished* work, not for every re-import of the same
            // backup.
            if completed_imports.contains(&ours.id) {
                continue;
            }
            // Ours and unfinished: reapply the terms and the status, and offer
            // it to the ancestry pass. The marker commits before that work, so
            // a failure in between leaves exactly this state, and a retry that
            // only skipped would never repair it.
            let term_ids = resolve_import_terms(&repos, post).await?;
            // Same mapping the creation path uses: a retry of a backup whose
            // schedules have since elapsed must not be refused either.
            let wanted_status = import_status(&post.status, post.published_at).to_owned();
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
            // The discussion too. Skipped when the post already carries one, so
            // finishing a half-done import does not append a second copy — see
            // `content::import_comments`.
            let incoming = imported_comments(&post.comments);
            comments_restored += repos
                .with_conn(async |conn| content::import_comments(conn, ours.id, &incoming).await)
                .await?;
            let fields = imported_meta(&post.meta);
            repos
                .with_conn(async |conn| content::import_post_meta(conn, ours.id, &fields).await)
                .await?;
            // After the status transition above, which records revisions of its
            // own: the file's history is the true one, and the rows the restore
            // made along the way are an artefact of restoring.
            let history = imported_revisions(&post.revisions);
            repos
                .with_conn(async |conn| content::import_revisions(conn, ours.id, &history).await)
                .await?;
            created_ids.push((
                ours.id,
                post.post_type.clone(),
                file_identity.clone(),
                parent_identity(post),
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
        //
        // Built here and inserted below, inside the transaction: the row and
        // its source marker have to commit together. Through the shared
        // allocator either way, because the bare-path index is enforced by the
        // database and importing a page whose slug an existing post already
        // holds would otherwise abort the restore part-way.
        let draft = NewPost {
            post_type: post.post_type.clone(),
            title: post.title.clone(),
            slug: post.slug.clone(),
            excerpt: post.excerpt.clone(),
            body: post.body.clone(),
            status: "draft".to_owned(),
            author_id,
            parent_id: parent_now,
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
        };

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
        // Not the file's status verbatim: an elapsed schedule becomes a
        // publication. See `import_status`.
        let wanted_status = import_status(&post.status, post.published_at).to_owned();
        let source_slug = file_identity.clone();
        // The insert, the marker, the terms and the status are one transaction.
        //
        // The insert used to sit outside it, because the `Repos` allocator
        // takes a connection of its own — which left a window a retry could not
        // recover from. A process killed between the insert and the marker
        // leaves an *unmarked* post; the next run sees an ordinary local row
        // holding that slug, classifies it as somebody else's content and skips
        // it forever, so its terms, status and ancestry are never restored. The
        // unwind below covers a failed statement, but nothing covers a process
        // that stops existing. `content::insert_post_with_unique_slug` does the
        // same allocation on this connection, each attempt in a savepoint, so
        // there is no window at all.
        let outcome = repos
            .with_conn(async |conn| {
                use diesel_async::AsyncConnection as _;
                conn.transaction(async move |conn| {
                    // The parent resolved above is only *used* if it is one the
                    // editor would accept — a live row of the same type, no
                    // cycle, within `MAX_PAGE_DEPTH`. Pre-setting it without
                    // asking let an import file a page under a trashed parent,
                    // whose canonical URL then resolves nowhere; declining here
                    // leaves the row at the top level and hands it to the
                    // ancestry pass, which declines too and reports the count.
                    let mut draft = draft;
                    if let Some(parent_id) = draft.parent_id {
                        // Under the hierarchy lock, held through the insert, as
                        // the editor's create and `set_post_parent` both do.
                        // Without it this validation and a concurrent
                        // re-parenting each see the old tree and both commit,
                        // leaving the imported child past `MAX_PAGE_DEPTH` —
                        // whose path `page_ancestry` then truncates, so the
                        // page is unreachable at the URL it advertises.
                        content::lock_page_hierarchy(conn).await?;
                        if content::validate_parent(conn, None, &draft.post_type, parent_id)
                            .await
                            .is_err()
                        {
                            draft.parent_id = None;
                        }
                    }
                    let created = content::insert_post_with_unique_slug(conn, draft).await?;
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
                        return Ok::<_, AutumnError>((created.id, true));
                    }
                    Ok::<_, AutumnError>((created.id, false))
                })
                .await
            })
            .await?;
        let (created_id, transitioned) = outcome;

        // No unwind: the transaction above is the unwind. A failure anywhere in
        // it rolls the insert back with everything else, so there is no row to
        // remove and no half-import for the next run to misread.
        if transitioned {
            transitioned_ids.push(created_id);
        }
        let incoming = imported_comments(&post.comments);
        comments_restored += repos
            .with_conn(async |conn| content::import_comments(conn, created_id, &incoming).await)
            .await?;
        let fields = imported_meta(&post.meta);
        repos
            .with_conn(async |conn| content::import_post_meta(conn, created_id, &fields).await)
            .await?;
        // After the status transition above, which records revisions of its
        // own: the file's history is the true one, and the rows the restore
        // made along the way are an artefact of restoring.
        let history = imported_revisions(&post.revisions);
        repos
            .with_conn(async |conn| content::import_revisions(conn, created_id, &history).await)
            .await?;
        created_ids.push((
            created_id,
            post.post_type.clone(),
            file_identity.clone(),
            parent_identity(post),
        ));
        restored += 1;
    }

    // Re-link ancestry in a second pass: a child can appear in the file before
    // its parent, so the parent's row may not exist during the first. Resolve
    // through the file's own identities rather than by re-querying, because a
    // row may have been given a different slug on the way in.
    let by_file_identity: std::collections::HashMap<(&str, &str), i64> = created_ids
        .iter()
        .map(|(id, post_type, identity, _)| ((post_type.as_str(), identity.as_str()), *id))
        .collect();
    for (child_id, post_type, _, parent_identity) in &created_ids {
        let Some(parent_identity) = parent_identity else {
            continue;
        };
        // Prefer a row created by this run; fall back to one already on the
        // site. Importing into a partly-populated site is the common restore
        // shape, and there the parent is *skipped* as already-present — so it
        // is absent from the map, and consulting only the map would drop the
        // child to the top level and change its canonical path.
        let parent_id = match by_file_identity.get(&(post_type.as_str(), parent_identity.as_str()))
        {
            Some(id) => Some(*id),
            None => find_local(&repos, post_type, parent_identity)
                .await?
                .map(|parent| parent.id),
        };
        // Already correct when the creation pass could resolve the parent; the
        // second pass exists for the ones it could not.
        let already_linked = repos
            .posts
            .find_by_id(*child_id)
            .await?
            .and_then(|child| child.parent_id)
            == parent_id;
        if let Some(parent_id) = parent_id
            && !already_linked
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

    // Recorded only now, after the ancestry pass — that is what makes the
    // marker mean "finished" rather than "created". A failure anywhere above
    // leaves these rows unmarked, so the next run reconciles them instead of
    // skipping them.
    let finished: Vec<i64> = created_ids.iter().map(|(id, _, _, _)| *id).collect();
    repos
        .with_conn(async |conn| content::mark_imports_complete(conn, &finished).await)
        .await?;

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
                (restored) " imported, " (skipped) " already present."
            }
            @if comments_restored > 0 {
                p class="text-sm text-gray-700 mt-1" {
                    (autumn_web::format::pluralize(
                        i64::try_from(comments_restored).unwrap_or(i64::MAX), "comment"))
                    " restored, moderation states and all."
                }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn comment(id: i64, parent: Option<i64>, body: &str) -> crate::models::Comment {
        crate::models::Comment {
            id,
            post_id: 1,
            parent_id: parent,
            author_id: None,
            author_name: "Guest".to_owned(),
            author_email: String::new(),
            author_url: String::new(),
            author_ip: String::new(),
            body: body.to_owned(),
            status: "approved".to_owned(),
            created_at: chrono::NaiveDateTime::default(),
        }
    }

    /// The tree the file carries is the tree the rows describe.
    ///
    /// Guarding the rewrite that made this linear: it used to filter the whole
    /// slice at every node, so indexing by parent was a change to *how* the
    /// nesting is found, and this is what says the nesting itself did not move.
    #[test]
    fn the_export_tree_matches_the_rows() {
        let names = std::collections::HashMap::new();
        let rows = vec![
            comment(1, None, "root one"),
            comment(2, Some(1), "reply to one"),
            comment(3, None, "root two"),
            comment(4, Some(2), "reply to the reply"),
            // A row whose parent is gone — only reachable by a direct write.
            comment(5, Some(99), "orphan"),
        ];
        let tree = export_comments(Some(&rows), &names);

        let roots: Vec<&str> = tree.iter().map(|c| c.body.as_str()).collect();
        assert_eq!(
            roots,
            vec!["root one", "root two"],
            "an orphan is dropped rather than promoted to a root it never was"
        );
        assert_eq!(tree[0].replies.len(), 1);
        assert_eq!(tree[0].replies[0].body, "reply to one");
        assert_eq!(tree[0].replies[0].replies[0].body, "reply to the reply");
        assert!(tree[1].replies.is_empty());
    }

    /// A cycle from a direct write terminates the walk rather than spinning.
    #[test]
    fn the_export_tree_is_bounded() {
        let names = std::collections::HashMap::new();
        // 1 → 2 → 3 → … each the child of the last, deeper than the cap.
        let mut rows = vec![comment(1, None, "root")];
        for id in 2..20 {
            rows.push(comment(id, Some(id - 1), "deeper"));
        }
        let tree = export_comments(Some(&rows), &names);

        let mut depth = 0;
        let mut node = &tree[0];
        while let Some(next) = node.replies.first() {
            depth += 1;
            node = next;
        }
        assert!(
            depth <= crate::content::MAX_COMMENT_DEPTH + 1,
            "the walk stops at the cap the write path enforces, got {depth}"
        );
    }
}
