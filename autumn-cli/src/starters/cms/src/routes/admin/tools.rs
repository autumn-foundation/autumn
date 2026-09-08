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
    /// The author's **username**, not their id: ids are meaningless across
    /// installations, and a username is what an importer can actually resolve.
    pub author: String,
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

/// The current export schema version.
pub const EXPORT_VERSION: u32 = 1;

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
                             placeholder="{\"version\": 1, …}"
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
        for term in repos
            .terms
            .find_by_taxonomy(taxonomy.slug.to_owned())
            .await?
        {
            terms.push(ExportTerm {
                taxonomy: term.taxonomy.clone(),
                name: term.name.clone(),
                slug: term.slug.clone(),
                description: term.description.clone(),
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
            let assigned = repos.post_terms(post.id).await?;
            posts.push(ExportPost {
                post_type: post.post_type.clone(),
                title: post.title.clone(),
                slug: post.slug.clone(),
                excerpt: post.excerpt.clone(),
                body: post.body.clone(),
                status: post.status.clone(),
                author,
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
                    parent_id: None,
                })
                .await?;
        }
    }

    let mut imported = 0_usize;
    let mut skipped = 0_usize;
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
        let created = repos
            .posts
            .save(&NewPost {
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
                comment_status: "open".to_owned(),
                password: String::new(),
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
        imported += 1;
    }

    let body = html! {
        div class="bg-white rounded-lg shadow p-5 max-w-lg" {
            h2 class="font-semibold mb-2" { "Import complete" }
            p class="text-sm text-gray-700" {
                (imported) " imported, " (skipped) " already present."
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
