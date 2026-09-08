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
use crate::repositories::{PostRepository as _, TermRepository as _, UserRepository as _};
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
    /// Term slugs, qualified by taxonomy.
    #[serde(default)]
    pub terms: Vec<ExportTermRef>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ExportTermRef {
    pub taxonomy: String,
    pub slug: String,
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
pub const EXPORT_VERSION: u32 = 2;

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
                             placeholder="{\"version\": 2, …}"
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
            let author = repos
                .users
                .find_by_id(post.author_id)
                .await
                .ok()
                .flatten()
                .map(|u| u.username)
                .unwrap_or_default();
            // The parent's slug, resolved now while the ids still mean
            // something in this database.
            let parent_slug = match post.parent_id {
                Some(parent_id) => repos
                    .posts
                    .find_by_id(parent_id)
                    .await
                    .ok()
                    .flatten()
                    .map(|parent| parent.slug),
                None => None,
            };
            let assigned = repos.post_terms(post.id).await?;
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
                terms: assigned
                    .iter()
                    .map(|t| ExportTermRef {
                        taxonomy: t.taxonomy.clone(),
                        slug: t.slug.clone(),
                    })
                    .collect(),
            });
        }
    }

    let payload = Export {
        version: EXPORT_VERSION,
        site_title: settings.site_title.clone(),
        exported_at: chrono::Utc::now(),
        terms,
        posts,
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
    mut db: autumn_web::Db,
    Form(form): Form<ImportForm>,
) -> AutumnResult<Response> {
    let user = require_capability!(repos, session, csrf, Capability::ImportContent);

    let payload: Export = serde_json::from_str(&form.payload)
        .map_err(|err| AutumnError::unprocessable_msg(format!("Not a valid export file: {err}")))?;
    if payload.version != EXPORT_VERSION {
        return Err(AutumnError::unprocessable_msg(format!(
            "This export is version {}; this site reads version {EXPORT_VERSION}",
            payload.version
        )));
    }

    // Terms first: posts reference them, and creating them up front means one
    // pass over the posts rather than two.
    for term in &payload.terms {
        let existing = repos
            .terms
            .find_by_slug(term.slug.clone())
            .await?
            .into_iter()
            .any(|t| t.taxonomy == term.taxonomy);
        if !existing {
            repos
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
        }
    }

    // Re-link taxonomy ancestry. Hierarchical categories are supported, so a
    // restore that flattened them would quietly change every archive's shape.
    for term in &payload.terms {
        let Some(parent_slug) = &term.parent else {
            continue;
        };
        let child = term_by_slug(&repos, &term.taxonomy, &term.slug).await?;
        let parent = term_by_slug(&repos, &term.taxonomy, parent_slug).await?;
        let (Some(child), Some(parent)) = (child, parent) else {
            continue;
        };
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

    let mut imported = 0_usize;
    let mut skipped = 0_usize;
    let mut orphaned = 0_i64;
    // (created id, post type, the slug AS WRITTEN IN THE FILE, parent slug).
    // The written slug is the key the file's `parent` references use; the row
    // may have been given a different one to avoid colliding with content
    // already on this site.
    let mut created_ids: Vec<(i64, String, String, Option<String>)> = Vec::new();
    for post in &payload.posts {
        // Idempotent on `(post_type, slug)` — the same pair the database's
        // unique index enforces — so re-running an import updates nothing and
        // duplicates nothing.
        let exists = repos
            .posts
            .find_by_slug(post.slug.clone())
            .await?
            .into_iter()
            .any(|p| p.post_type == post.post_type);
        if exists {
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
                featured_media_id: None,
                menu_order: 0,
                comment_status: post.comment_status.clone(),
                password: post.password.clone(),
                sticky: false,
                published_at: post.published_at,
            })
            .await?;

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
        if !term_ids.is_empty() {
            content::set_post_terms(&mut db, created.id, term_ids).await?;
        }

        if post.status != "draft" {
            content::transition_status(&mut db, created.id, &post.status).await?;
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
            && !content::set_post_parent(&mut db, *child_id, parent_id).await?
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
