//! The content screens — WordPress's Posts, Pages and every custom type.
//!
//! One set of handlers serves every registered post type: the type is a path
//! segment, and what the editor offers (excerpt, featured image, comments,
//! a parent selector) comes from the type's registration rather than from a
//! copy of this file per type.

use autumn_web::AutumnResult;
use autumn_web::prelude::*;
use autumn_web::reexports::axum::response::Response;
use serde::Deserialize;

use crate::capabilities::{Capability, can_delete_post, can_edit_post};
use crate::content;
use crate::content_types::{self, PostType};
use crate::models::{Attachment, NewPost, Post, Term, User};
use crate::plugins::{Action, do_action};
use crate::repositories::{
    AttachmentRepository as _, PostRepository as _, TermRepository as _, UserRepository as _,
};
use crate::require_capability;

use super::super::site::{Csrf, Repos};
use super::layout;

/// The statuses the editor's dropdown offers, in workflow order.
const STATUS_CHOICES: &[(&str, &str)] = &[
    ("draft", "Draft"),
    ("pending", "Pending review"),
    ("publish", "Published"),
    ("private", "Private"),
    ("future", "Scheduled"),
];

#[derive(Debug, Default, Deserialize)]
pub struct ListFilters {
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub s: Option<String>,
}

/// What the editor submits.
#[derive(Debug, Deserialize)]
pub struct PostForm {
    pub title: String,
    #[serde(default)]
    pub slug: String,
    #[serde(default)]
    pub excerpt: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub comment_status: Option<String>,
    #[serde(default)]
    pub password: String,
    /// Absent when unchecked — browsers omit unchecked checkboxes entirely.
    #[serde(default)]
    pub sticky: Option<String>,
    #[serde(default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub menu_order: Option<String>,
    #[serde(default)]
    pub featured_media_id: Option<String>,
    /// Category ids, one per checked box.
    #[serde(default)]
    pub categories: Vec<i64>,
    /// A comma-separated tag list, as WordPress's tag box takes.
    #[serde(default)]
    pub tags: String,
    /// When scheduling: the local datetime the post goes live.
    #[serde(default)]
    pub publish_at: Option<String>,
    /// The `lock_version` the form was rendered from, for stale-edit
    /// detection. Absent on the create form, which has no row yet.
    #[serde(default)]
    pub lock_version: Option<String>,
}

/// Parse an optional numeric form field. An empty string means "not set",
/// which is different from a zero.
fn optional_id(raw: Option<&String>) -> Option<i64> {
    raw.map(|v| v.trim())
        .filter(|v| !v.is_empty())
        .and_then(|v| v.parse::<i64>().ok())
}

/// Every descendant of `root` within `all`, by id.
///
/// A breadth-first walk over the in-memory set the selector already loaded, so
/// it costs no extra queries. Bounded by the set size, so a pre-existing cycle
/// cannot spin.
fn descendant_ids(all: &[Post], root: i64) -> std::collections::HashSet<i64> {
    let mut found = std::collections::HashSet::new();
    let mut frontier = vec![root];
    while let Some(parent) = frontier.pop() {
        for post in all {
            if post.parent_id == Some(parent) && found.insert(post.id) {
                frontier.push(post.id);
            }
        }
    }
    found
}

/// The publish date the editor's `datetime-local` field carries, if any.
fn scheduled_at(form: &PostForm) -> Option<chrono::NaiveDateTime> {
    form.publish_at
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .and_then(|value| chrono::NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M").ok())
}

fn resolve_type(slug: &str) -> AutumnResult<PostType> {
    content_types::find_post_type(slug)
        .ok_or_else(|| AutumnError::not_found_msg(format!("Unknown post type `{slug}`")))
}

// ── List ────────────────────────────────────────────────────────────────────

#[get("/admin/content/{post_type}")]
pub async fn list(
    repos: Repos,
    session: Session,
    csrf: Csrf,
    Path(post_type): Path<String>,
    Query(filters): Query<ListFilters>,
) -> AutumnResult<Response> {
    let user = require_capability!(repos, session, csrf, Capability::EditPosts);
    let registered = resolve_type(&post_type)?;

    let mut posts: Vec<Post> = match (&filters.status, &filters.s) {
        (_, Some(query)) if !query.trim().is_empty() => repos
            .posts
            .search(query.trim())
            .await?
            .into_iter()
            .filter(|p| p.post_type == post_type)
            .collect(),
        (Some(status), _) if !status.is_empty() => {
            repos
                .posts
                .find_by_post_type_and_status(post_type.clone(), status.clone())
                .await?
        }
        // The default list hides trash, exactly as WordPress's does — trashed
        // content is reachable through the "Trash" filter.
        _ => repos
            .posts
            .find_by_post_type(post_type.clone())
            .await?
            .into_iter()
            .filter(|p| p.status != "trash")
            .collect(),
    };
    posts.sort_by_key(|post| std::cmp::Reverse(post.updated_at));

    // A Contributor sees only their own content — the list must not advertise
    // what they cannot open.
    if !user.role().can(Capability::EditOthersPosts) {
        posts.retain(|p| p.author_id == user.id);
    }

    let mut rows = Vec::with_capacity(posts.len());
    for post in &posts {
        let author = repos.users.find_by_id(post.author_id).await.ok().flatten();
        rows.push((post.clone(), author));
    }

    let body = html! {
        div class="flex items-center justify-between mb-4 gap-4 flex-wrap" {
            form method="get" class="flex gap-2 items-center text-sm" {
                label for="status" class="sr-only" { "Status" }
                select #status name="status" class="border rounded px-2 py-1.5"
                       onchange="this.form.submit()" {
                    option value="" selected[filters.status.is_none()] { "All statuses" }
                    @for (value, label) in STATUS_CHOICES {
                        option value=(value)
                               selected[filters.status.as_deref() == Some(*value)] { (label) }
                    }
                    option value="trash" selected[filters.status.as_deref() == Some("trash")] {
                        "Trash"
                    }
                }
                label for="content-search" class="sr-only" { "Search content" }
                input #content-search type="search" name="s"
                      value=(filters.s.clone().unwrap_or_default())
                      placeholder="Search…" class="border rounded px-2 py-1.5";
                button type="submit" class="px-3 py-1.5 border rounded bg-white hover:bg-gray-50" {
                    "Filter"
                }
            }
            a href=(format!("/admin/content/{post_type}/new"))
              class="px-4 py-2 bg-indigo-600 text-white rounded hover:bg-indigo-700 text-sm" {
                "Add " (registered.singular)
            }
        }

        div class="bg-white rounded-lg shadow overflow-hidden" {
            table class="w-full text-sm" {
                caption class="sr-only" { (registered.plural) }
                thead class="bg-gray-50 text-left text-xs uppercase tracking-wide text-gray-500" {
                    tr {
                        th scope="col" class="px-4 py-3" { "Title" }
                        th scope="col" class="px-4 py-3" { "Author" }
                        th scope="col" class="px-4 py-3" { "Status" }
                        th scope="col" class="px-4 py-3" { "Updated" }
                    }
                }
                tbody {
                    @for (post, author) in &rows {
                        tr class="border-t border-gray-100" {
                            td class="px-4 py-3" {
                                a href=(format!("/admin/content/{post_type}/{}", post.id))
                                  class="font-medium text-indigo-700 hover:underline" {
                                    @if post.title.trim().is_empty() {
                                        "(no title)"
                                    } @else {
                                        (post.title)
                                    }
                                }
                                @if post.sticky {
                                    span class="ml-2 text-xs text-amber-700" { "· featured" }
                                }
                                @if post.is_password_protected() {
                                    span class="ml-2 text-xs text-gray-400" { "· password" }
                                }
                            }
                            td class="px-4 py-3 text-gray-500" {
                                (author.as_ref().map_or("—", |a| a.public_name()))
                            }
                            td class="px-4 py-3" { (status_badge(&post.status)) }
                            td class="px-4 py-3 text-gray-500" {
                                (post.updated_at.format("%Y-%m-%d %H:%M").to_string())
                            }
                        }
                    }
                    @if rows.is_empty() {
                        tr { td colspan="4" class="px-4 py-10 text-center text-gray-400" {
                            "Nothing here yet."
                        } }
                    }
                }
            }
        }
    };

    Ok(layout(
        &user,
        &csrf,
        &format!("/admin/content/{post_type}"),
        registered.plural,
        body,
    )
    .into_response())
}

fn status_badge(status: &str) -> Markup {
    let (classes, label) = match status {
        "publish" => ("bg-green-100 text-green-800", "Published"),
        "draft" => ("bg-gray-100 text-gray-700", "Draft"),
        "pending" => ("bg-amber-100 text-amber-800", "Pending"),
        "private" => ("bg-purple-100 text-purple-800", "Private"),
        "future" => ("bg-blue-100 text-blue-800", "Scheduled"),
        "trash" => ("bg-red-100 text-red-800", "Trash"),
        other => ("bg-gray-100 text-gray-700", other),
    };
    html! {
        span class=(format!("px-2 py-0.5 rounded text-xs {classes}")) { (label) }
    }
}

// ── Editor ──────────────────────────────────────────────────────────────────

#[get("/admin/content/{post_type}/new")]
pub async fn new_form(
    repos: Repos,
    session: Session,
    csrf: Csrf,
    Path(post_type): Path<String>,
) -> AutumnResult<Response> {
    let user = require_capability!(repos, session, csrf, Capability::EditPosts);
    let registered = resolve_type(&post_type)?;
    let context = EditorContext::load(&repos, &registered, None).await?;
    let body = editor(&registered, None, &context, &user, &csrf);
    Ok(layout(
        &user,
        &csrf,
        &format!("/admin/content/{post_type}"),
        &format!("Add {}", registered.singular),
        body,
    )
    .into_response())
}

#[get("/admin/content/{post_type}/{id}")]
pub async fn edit_form(
    repos: Repos,
    session: Session,
    csrf: Csrf,
    Path((post_type, id)): Path<(String, i64)>,
) -> AutumnResult<Response> {
    let user = require_capability!(repos, session, csrf, Capability::EditPosts);
    let registered = resolve_type(&post_type)?;
    let post = repos
        .posts
        .find_by_id(id)
        .await?
        .filter(|p| p.post_type == post_type)
        .ok_or_else(|| AutumnError::not_found_msg("No such content"))?;

    if !can_edit_post(user.role(), user.id, post.author_id, &post.status) {
        return Err(AutumnError::forbidden_msg(
            "You do not have permission to edit this content",
        ));
    }

    let context = EditorContext::load(&repos, &registered, Some(&post)).await?;
    let body = editor(&registered, Some(&post), &context, &user, &csrf);
    Ok(layout(
        &user,
        &csrf,
        &format!("/admin/content/{post_type}"),
        &format!("Edit {}", registered.singular),
        body,
    )
    .into_response())
}

/// Everything the editor form needs besides the post itself.
struct EditorContext {
    categories: Vec<Term>,
    selected_categories: Vec<i64>,
    tags: String,
    parents: Vec<Post>,
    media: Vec<Attachment>,
}

impl EditorContext {
    async fn load(repos: &Repos, registered: &PostType, post: Option<&Post>) -> AutumnResult<Self> {
        let taxonomies = content_types::taxonomies_for(registered.slug);
        let has_categories = taxonomies.iter().any(|t| t.slug == "category");
        let has_tags = taxonomies.iter().any(|t| t.slug == "post_tag");

        let categories = if has_categories {
            repos.terms.find_by_taxonomy("category".to_owned()).await?
        } else {
            Vec::new()
        };

        let (selected_categories, tags) = match post {
            Some(post) => {
                let assigned = repos.post_terms(post.id).await?;
                let selected = assigned
                    .iter()
                    .filter(|t| t.taxonomy == "category")
                    .map(|t| t.id)
                    .collect();
                let tag_names = if has_tags {
                    assigned
                        .iter()
                        .filter(|t| t.taxonomy == "post_tag")
                        .map(|t| t.name.clone())
                        .collect::<Vec<_>>()
                        .join(", ")
                } else {
                    String::new()
                };
                (selected, tag_names)
            }
            None => (Vec::new(), String::new()),
        };

        // A hierarchical type offers a parent selector. It excludes the post
        // itself *and* every descendant of it: picking a descendant closes a
        // cycle, and because page resolution walks down from a row whose
        // parent is NULL, every page in that cycle becomes unreachable at its
        // own permalink. The write path refuses it too (see `update`); this
        // just keeps the impossible option off the screen.
        let parents = if registered.hierarchical {
            let all: Vec<Post> = repos
                .posts
                .find_by_post_type(registered.slug.to_owned())
                .await?
                .into_iter()
                .filter(|p| p.status != "trash")
                .collect();
            match post {
                Some(current) => {
                    let descendants = descendant_ids(&all, current.id);
                    all.into_iter()
                        .filter(|p| p.id != current.id && !descendants.contains(&p.id))
                        .collect()
                }
                None => all,
            }
        } else {
            Vec::new()
        };

        let media = if registered.supports_thumbnail {
            repos.attachments.find_all().await.unwrap_or_default()
        } else {
            Vec::new()
        };

        Ok(Self {
            categories,
            selected_categories,
            tags,
            parents,
            media,
        })
    }
}

fn editor(
    registered: &PostType,
    post: Option<&Post>,
    context: &EditorContext,
    user: &User,
    csrf: &Csrf,
) -> Markup {
    let action = post.map_or_else(
        || format!("/admin/content/{}", registered.slug),
        |p| format!("/admin/content/{}/{}", registered.slug, p.id),
    );
    let value = |get: fn(&Post) -> &str| post.map_or("", get);
    let can_publish = user.role().can(Capability::PublishPosts);

    html! {
        form action=(action) method="post" class="grid grid-cols-1 lg:grid-cols-3 gap-6" {
            (csrf.input())
            @if let Some(post) = post {
                // Stale-edit detection: the server compares this against the
                // row it locks, so a save built on content someone else has
                // since changed is refused rather than silently overwriting.
                input type="hidden" name="lock_version" value=(post.lock_version);
            }
            div class="lg:col-span-2 space-y-4" {
                div class="bg-white rounded-lg shadow p-5 space-y-4" {
                    div {
                        label for="title" class="block text-sm font-medium mb-1" { "Title" }
                        input #title type="text" name="title" required maxlength="300"
                              value=(value(|p| &p.title))
                              class="w-full border rounded px-3 py-2 text-lg";
                    }
                    div {
                        label for="slug" class="block text-sm font-medium mb-1" {
                            "Slug "
                            span class="text-gray-400 font-normal" {
                                "(leave blank to derive from the title)"
                            }
                        }
                        input #slug type="text" name="slug" value=(value(|p| &p.slug))
                              class="w-full border rounded px-3 py-2 font-mono text-sm";
                    }
                    div {
                        label for="body" class="block text-sm font-medium mb-1" {
                            "Content "
                            span class="text-gray-400 font-normal" { "(Markdown)" }
                        }
                        textarea #body name="body" rows="18"
                                 class="w-full border rounded px-3 py-2 font-mono text-sm" {
                            (value(|p| &p.body))
                        }
                    }
                    @if registered.supports_excerpt {
                        div {
                            label for="excerpt" class="block text-sm font-medium mb-1" {
                                "Excerpt "
                                span class="text-gray-400 font-normal" {
                                    "(optional — the first 55 words are used if blank)"
                                }
                            }
                            textarea #excerpt name="excerpt" rows="3"
                                     class="w-full border rounded px-3 py-2 text-sm" {
                                (value(|p| &p.excerpt))
                            }
                        }
                    }
                }
            }

            aside class="space-y-4" {
                div class="bg-white rounded-lg shadow p-5 space-y-4" {
                    h2 class="font-semibold text-sm" { "Publish" }
                    div {
                        label for="status" class="block text-sm font-medium mb-1" { "Status" }
                        select #status name="status" class="w-full border rounded px-3 py-2" {
                            @for (value, label) in STATUS_CHOICES {
                                // A Contributor cannot publish, so the option is
                                // absent rather than present-and-rejected.
                                @if can_publish || matches!(*value, "draft" | "pending") {
                                    option value=(value)
                                           selected[post.is_some_and(|p| p.status == *value)
                                                    || (post.is_none() && *value == "draft")] {
                                        (label)
                                    }
                                }
                            }
                        }
                    }
                    div {
                        label for="publish_at" class="block text-sm font-medium mb-1" {
                            "Publish date "
                            span class="text-gray-400 font-normal" { "(for Scheduled)" }
                        }
                        input #publish_at type="datetime-local" name="publish_at"
                              value=(post.and_then(|p| p.published_at)
                                  .map(|d| d.format("%Y-%m-%dT%H:%M").to_string())
                                  .unwrap_or_default())
                              class="w-full border rounded px-3 py-2 text-sm";
                    }
                    button type="submit"
                           class="w-full px-4 py-2 bg-indigo-600 text-white rounded \
                                  hover:bg-indigo-700" {
                        @if post.is_some() { "Save changes" } @else { "Create" }
                    }
                    @if let Some(post) = post {
                        div class="flex flex-wrap gap-2 pt-2 border-t border-gray-100 text-sm" {
                            a href=(format!("/admin/content/{}/{}/revisions", registered.slug, post.id))
                              class="text-indigo-700 hover:underline" { "Revisions" }
                            @if post.status != "trash"
                                && can_delete_post(user.role(), user.id, post.author_id, &post.status) {
                                span class="text-gray-300" { "·" }
                                button type="submit" formmethod="post"
                                       formaction=(format!(
                                           "/admin/content/{}/{}/status?to=trash",
                                           registered.slug, post.id))
                                       class="text-red-700 hover:underline" {
                                           (csrf.input())
                                    "Move to trash"
                                }
                            }
                            @if post.status == "trash" {
                                span class="text-gray-300" { "·" }
                                button type="submit" formmethod="post"
                                       formaction=(format!(
                                           "/admin/content/{}/{}/status?to=draft",
                                           registered.slug, post.id))
                                       class="text-indigo-700 hover:underline" {
                                           (csrf.input())
                                    "Restore"
                                }
                            }
                        }
                    }
                }

                @if !context.categories.is_empty() {
                    fieldset class="bg-white rounded-lg shadow p-5" {
                        legend class="font-semibold text-sm px-1" { "Categories" }
                        div class="space-y-1 mt-2 max-h-56 overflow-y-auto" {
                            @for term in &context.categories {
                                label class="flex items-center gap-2 text-sm" {
                                    input type="checkbox" name="categories" value=(term.id)
                                          checked[context.selected_categories.contains(&term.id)]
                                          class="rounded border-gray-300";
                                    (term.name)
                                }
                            }
                        }
                    }
                }

                @if content_types::taxonomies_for(registered.slug).iter().any(|t| t.slug == "post_tag") {
                    div class="bg-white rounded-lg shadow p-5" {
                        label for="tags" class="block font-semibold text-sm mb-2" { "Tags" }
                        input #tags type="text" name="tags" value=(context.tags)
                              placeholder="rust, web, async"
                              class="w-full border rounded px-3 py-2 text-sm";
                        p class="text-xs text-gray-400 mt-1" {
                            "Comma separated. New tags are created automatically."
                        }
                    }
                }

                @if registered.supports_thumbnail {
                    div class="bg-white rounded-lg shadow p-5" {
                        label for="featured_media_id" class="block font-semibold text-sm mb-2" {
                            "Featured image"
                        }
                        select #featured_media_id name="featured_media_id"
                               class="w-full border rounded px-3 py-2 text-sm" {
                            option value="" { "None" }
                            @for media in &context.media {
                                option value=(media.id)
                                       selected[post.and_then(|p| p.featured_media_id)
                                           == Some(media.id)] {
                                    (media.title)
                                }
                            }
                        }
                    }
                }

                div class="bg-white rounded-lg shadow p-5 space-y-3" {
                    h2 class="font-semibold text-sm" { "Options" }
                    @if registered.supports_comments {
                        label class="flex items-center gap-2 text-sm" {
                            input type="checkbox" name="comment_status" value="open"
                                  checked[post.is_none_or(|p| p.comment_status == "open")]
                                  class="rounded border-gray-300";
                            "Allow comments"
                        }
                    }
                    @if registered.slug == "post" {
                        label class="flex items-center gap-2 text-sm" {
                            input type="checkbox" name="sticky" value="on"
                                  checked[post.is_some_and(|p| p.sticky)]
                                  class="rounded border-gray-300";
                            "Pin to the top of the blog"
                        }
                    }
                    div {
                        label for="password" class="block text-sm font-medium mb-1" {
                            "Password"
                        }
                        input #password type="text" name="password"
                              value=(value(|p| &p.password))
                              placeholder="Leave blank for public"
                              class="w-full border rounded px-3 py-2 text-sm";
                    }
                    @if registered.hierarchical {
                        div {
                            label for="parent_id" class="block text-sm font-medium mb-1" {
                                "Parent"
                            }
                            select #parent_id name="parent_id"
                                   class="w-full border rounded px-3 py-2 text-sm" {
                                option value="" { "(top level)" }
                                @for parent in &context.parents {
                                    option value=(parent.id)
                                           selected[post.and_then(|p| p.parent_id)
                                               == Some(parent.id)] {
                                        (parent.title)
                                    }
                                }
                            }
                        }
                        div {
                            label for="menu_order" class="block text-sm font-medium mb-1" {
                                "Order"
                            }
                            input #menu_order type="number" name="menu_order"
                                  value=(post.map_or(0, |p| p.menu_order))
                                  class="w-full border rounded px-3 py-2 text-sm";
                        }
                    }
                }
            }
        }
    }
}

// ── Create / update ─────────────────────────────────────────────────────────

#[post("/admin/content/{post_type}")]
pub async fn create(
    repos: Repos,
    session: Session,
    csrf: Csrf,
    mut db: autumn_web::Db,
    Path(post_type): Path<String>,
    Form(form): Form<PostForm>,
) -> AutumnResult<Response> {
    let user = require_capability!(repos, session, csrf, Capability::EditPosts);
    let registered = resolve_type(&post_type)?;

    // A Contributor may only create drafts and submissions, whatever the form
    // says — the dropdown hides the other options, and this is what enforces it.
    let status = requested_status(&form, &user);

    // A scheduled post needs the date the author picked. Without it the row
    // would carry `published_at = NULL`, and the publish sweep — which selects
    // `status = 'future' AND published_at <= now()` — would never see it
    // again: the post would sit in `future` forever.
    let scheduled_for = scheduled_at(&form);
    if status == "future" && scheduled_for.is_none() {
        return Err(AutumnError::unprocessable_msg(
            "Pick a publish date for a scheduled post",
        ));
    }

    // The same parent validation the update path runs. Creation skipped it
    // before, so repeated creates could build a hierarchy deeper than the
    // permalink builder renders — producing a canonical URL that starts
    // mid-tree and resolves to nothing.
    if let Some(parent_id) = optional_id(form.parent_id.as_ref()) {
        content::validate_parent(&mut db, None, parent_id).await?;
    }

    // Slug allocation and the retry it needs live on `Repos` so the importer
    // uses exactly the same path — `idx_posts_bare_path_slug` makes the
    // invariant the database's, so every insert path has to allocate through
    // one place.
    let created = repos
        .save_post_with_unique_slug(NewPost {
            post_type: registered.slug.to_owned(),
            title: form.title.trim().to_owned(),
            slug: form.slug.trim().to_owned(),
            excerpt: form.excerpt.trim().to_owned(),
            body: form.body.clone(),
            status: if status == "future" || status == "private" {
                // Both are only reachable by transition, so create as a draft
                // and move it immediately below — the state machine stays the
                // single authority on which statuses are reachable how.
                "draft".to_owned()
            } else {
                status.clone()
            },
            author_id: user.id,
            parent_id: optional_id(form.parent_id.as_ref()),
            featured_media_id: optional_id(form.featured_media_id.as_ref()),
            menu_order: optional_id(form.menu_order.as_ref()).unwrap_or(0) as i32,
            comment_status: form
                .comment_status
                .as_deref()
                .map_or("closed", |_| "open")
                .to_owned(),
            password: form.password.trim().to_owned(),
            sticky: form.sticky.is_some(),
            published_at: scheduled_for,
        })
        .await?;

    // The first revision records the content as created, so the history has a
    // starting point rather than beginning at the first *edit*.
    content::record_initial_revision(&mut db, &created).await?;

    apply_terms(&repos, &mut db, &created, &form).await?;
    if status == "future" || status == "private" {
        content::transition_status(&mut db, created.id, &status).await?;
    }
    do_action(Action::PostSaved, created.id);

    Ok(Redirect::to(&format!(
        "/admin/content/{}/{}",
        registered.slug, created.id
    ))
    .into_response())
}

#[post("/admin/content/{post_type}/{id}")]
pub async fn update(
    repos: Repos,
    session: Session,
    csrf: Csrf,
    mut db: autumn_web::Db,
    Path((post_type, id)): Path<(String, i64)>,
    Form(form): Form<PostForm>,
) -> AutumnResult<Response> {
    let user = require_capability!(repos, session, csrf, Capability::EditPosts);
    let registered = resolve_type(&post_type)?;
    let existing = repos
        .posts
        .find_by_id(id)
        .await?
        .filter(|p| p.post_type == post_type)
        .ok_or_else(|| AutumnError::not_found_msg("No such content"))?;

    if !can_edit_post(user.role(), user.id, existing.author_id, &existing.status) {
        return Err(AutumnError::forbidden_msg(
            "You do not have permission to edit this content",
        ));
    }

    let status = requested_status(&form, &user);
    let scheduled_for = scheduled_at(&form);
    if status == "future" && scheduled_for.is_none() && existing.published_at.is_none() {
        return Err(AutumnError::unprocessable_msg(
            "Pick a publish date for a scheduled post",
        ));
    }

    // Validate the status change BEFORE anything is written. The content edit
    // and the term assignment each commit in their own transaction, so a
    // transition rejected afterwards would leave the title, body, publish date
    // and taxonomy already persisted while the response reported a failure.
    // The state machine's validator is pure, so it can run up front: build the
    // proposed row (new content, OLD status) so the `can_publish` guard
    // evaluates what is about to be saved rather than what is there now.
    if status != existing.status {
        let mut proposed = existing.clone();
        proposed.title = form.title.trim().to_owned();
        proposed.status.clone_from(&existing.status);
        proposed.transition_status_to(&status)?;
    }

    // A page cannot be parented to itself or to one of its own descendants:
    // that closes a cycle, and page resolution walks down from a NULL parent,
    // so every page in the cycle becomes unreachable at its own permalink.
    if let Some(parent_id) = optional_id(form.parent_id.as_ref()) {
        content::validate_parent(&mut db, Some(id), parent_id).await?;
    }

    let slug = {
        let mut conn = repos.conn().await?;
        let desired = crate::hooks::normalize_slug(&form.slug, &form.title);
        content::ensure_unique_slug(&mut conn, &post_type, &desired, Some(id)).await?
    };

    let form_snapshot = (
        form.title.trim().to_owned(),
        slug,
        form.excerpt.trim().to_owned(),
        form.body.clone(),
        form.password.trim().to_owned(),
        form.sticky.is_some(),
        form.comment_status.is_some(),
        optional_id(form.parent_id.as_ref()),
        optional_id(form.featured_media_id.as_ref()),
        optional_id(form.menu_order.as_ref()).unwrap_or(0) as i32,
    );

    // The `lock_version` the editor's form was rendered from. The server
    // compares it against the row it locks, so a save built on content
    // somebody else has since changed is refused rather than overwriting it.
    let expected_lock_version = form
        .lock_version
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .and_then(|value| value.parse::<i32>().ok());

    // The content edit and its revision commit together.
    let updated =
        content::update_post_with_revision(
            &mut db,
            id,
            user.id,
            "Edited",
            expected_lock_version,
            move |post| {
                let (
                    title,
                    slug,
                    excerpt,
                    body,
                    password,
                    sticky,
                    comments_open,
                    parent,
                    media,
                    order,
                ) = form_snapshot;
                post.title = title;
                post.slug = slug;
                post.excerpt = excerpt;
                post.body = body;
                post.password = password;
                post.sticky = sticky;
                post.comment_status = if comments_open { "open" } else { "closed" }.to_owned();
                post.parent_id = parent;
                post.featured_media_id = media;
                post.menu_order = order;
                if let Some(when) = scheduled_for {
                    post.published_at = Some(when);
                }
            },
        )
        .await?;

    apply_terms(&repos, &mut db, &updated, &form).await?;

    // A status change goes through the state machine, never through the plain
    // field write above — so an illegal edge is refused rather than persisted.
    if status != updated.status {
        content::transition_status(&mut db, id, &status).await?;
        do_action(Action::PostTransitioned, id);
    }
    do_action(Action::PostSaved, id);

    Ok(Redirect::to(&format!("/admin/content/{}/{id}", registered.slug)).into_response())
}

/// Clamp the submitted status to what the account may actually set.
fn requested_status(form: &PostForm, user: &User) -> String {
    let requested = if form.status.trim().is_empty() {
        "draft"
    } else {
        form.status.trim()
    };
    if user.role().can(Capability::PublishPosts) {
        requested.to_owned()
    } else {
        // Without `publish_posts`, the only reachable statuses are `draft` and
        // `pending` — submitting for review is the whole Contributor workflow.
        match requested {
            "pending" => "pending".to_owned(),
            _ => "draft".to_owned(),
        }
    }
}

/// Save the post's categories and tags, creating any tag that does not exist.
async fn apply_terms(
    repos: &Repos,
    db: &mut autumn_web::Db,
    post: &Post,
    form: &PostForm,
) -> AutumnResult<()> {
    let taxonomies = content_types::taxonomies_for(&post.post_type);
    if taxonomies.is_empty() {
        return Ok(());
    }

    let mut term_ids: Vec<i64> = Vec::new();
    if taxonomies.iter().any(|t| t.slug == "category") {
        term_ids.extend(form.categories.iter().copied());
    }

    if taxonomies.iter().any(|t| t.slug == "post_tag") {
        for raw in form.tags.split(',') {
            let name = raw.trim();
            if name.is_empty() {
                continue;
            }
            let slug = autumn_web::slugify(name);
            if slug.is_empty() {
                continue;
            }
            // Find-or-create, matching WordPress's tag box: typing a new tag
            // creates it.
            let existing = repos
                .terms
                .find_by_slug(slug.clone())
                .await?
                .into_iter()
                .find(|t| t.taxonomy == "post_tag");
            let term = match existing {
                Some(term) => term,
                None => {
                    repos
                        .terms
                        .save(&crate::models::NewTerm {
                            taxonomy: "post_tag".to_owned(),
                            name: name.to_owned(),
                            slug,
                            description: String::new(),
                            parent_id: None,
                        })
                        .await?
                }
            };
            term_ids.push(term.id);
        }
    }

    content::set_post_terms(db, post.id, term_ids).await
}

// ── Status transitions ──────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct TransitionQuery {
    pub to: String,
}

#[post("/admin/content/{post_type}/{id}/status")]
pub async fn transition(
    repos: Repos,
    session: Session,
    csrf: Csrf,
    mut db: autumn_web::Db,
    Path((post_type, id)): Path<(String, i64)>,
    Query(query): Query<TransitionQuery>,
) -> AutumnResult<Response> {
    let user = require_capability!(repos, session, csrf, Capability::EditPosts);
    let post = repos
        .posts
        .find_by_id(id)
        .await?
        .filter(|p| p.post_type == post_type)
        .ok_or_else(|| AutumnError::not_found_msg("No such content"))?;

    // Trashing is a delete capability; every other move is an edit.
    let allowed = if query.to == "trash" {
        can_delete_post(user.role(), user.id, post.author_id, &post.status)
    } else if matches!(query.to.as_str(), "publish" | "private" | "future") {
        user.role().can(Capability::PublishPosts)
            && can_edit_post(user.role(), user.id, post.author_id, &post.status)
    } else {
        can_edit_post(user.role(), user.id, post.author_id, &post.status)
    };
    if !allowed {
        return Err(AutumnError::forbidden_msg(
            "You do not have permission to change this content's status",
        ));
    }

    content::transition_status(&mut db, id, &query.to).await?;
    do_action(Action::PostTransitioned, id);

    let destination = if query.to == "trash" {
        format!("/admin/content/{post_type}")
    } else {
        format!("/admin/content/{post_type}/{id}")
    };
    Ok(Redirect::to(&destination).into_response())
}

// ── Revisions ───────────────────────────────────────────────────────────────

#[get("/admin/content/{post_type}/{id}/revisions")]
pub async fn revisions(
    repos: Repos,
    session: Session,
    csrf: Csrf,
    mut db: autumn_web::Db,
    Path((post_type, id)): Path<(String, i64)>,
) -> AutumnResult<Response> {
    let user = require_capability!(repos, session, csrf, Capability::EditPosts);
    let post = repos
        .posts
        .find_by_id(id)
        .await?
        .filter(|p| p.post_type == post_type)
        .ok_or_else(|| AutumnError::not_found_msg("No such content"))?;

    if !can_edit_post(user.role(), user.id, post.author_id, &post.status) {
        return Err(AutumnError::forbidden_msg(
            "You do not have permission to view this content's history",
        ));
    }

    let history = content::revisions_for(&mut db, id).await?;
    let body = html! {
        p class="text-sm text-gray-500 mb-4" {
            "Every edit is snapshotted before it is applied, so restoring a revision returns \
             the content to how it was at that moment. Restoring is itself an edit — it appends \
             to the history rather than rewinding it."
        }
        div class="bg-white rounded-lg shadow divide-y divide-gray-100" {
            @for revision in &history {
                div class="p-4 flex items-start justify-between gap-4" {
                    div class="min-w-0" {
                        p class="font-medium" { (revision.title) }
                        p class="text-xs text-gray-500" {
                            (revision.created_at.format("%Y-%m-%d %H:%M").to_string())
                            " · " (revision.summary)
                            " · " (revision.status)
                        }
                        p class="text-sm text-gray-600 mt-2 line-clamp-3" {
                            (autumn_web::format::truncate(&revision.body, 240))
                        }
                    }
                    form method="post"
                         action=(format!(
                             "/admin/content/{post_type}/{id}/revisions/{}/restore",
                             revision.id)) {
                        (csrf.input())
                        button type="submit"
                               class="px-3 py-1.5 border rounded text-sm bg-white \
                                      hover:bg-gray-50 whitespace-nowrap" {
                            "Restore"
                        }
                    }
                }
            }
            @if history.is_empty() {
                p class="p-8 text-center text-gray-400" { "No revisions yet." }
            }
        }
        p class="mt-4" {
            a href=(format!("/admin/content/{post_type}/{id}"))
              class="text-indigo-700 hover:underline text-sm" { "← Back to the editor" }
        }
    };

    Ok(layout(
        &user,
        &csrf,
        &format!("/admin/content/{post_type}"),
        &format!("Revisions: {}", post.title),
        body,
    )
    .into_response())
}

#[post("/admin/content/{post_type}/{id}/revisions/{revision_id}/restore")]
pub async fn restore(
    repos: Repos,
    session: Session,
    csrf: Csrf,
    mut db: autumn_web::Db,
    Path((post_type, id, revision_id)): Path<(String, i64, i64)>,
) -> AutumnResult<Response> {
    let user = require_capability!(repos, session, csrf, Capability::EditPosts);
    let post = repos
        .posts
        .find_by_id(id)
        .await?
        .filter(|p| p.post_type == post_type)
        .ok_or_else(|| AutumnError::not_found_msg("No such content"))?;

    if !can_edit_post(user.role(), user.id, post.author_id, &post.status) {
        return Err(AutumnError::forbidden_msg(
            "You do not have permission to edit this content",
        ));
    }

    content::restore_revision(&mut db, id, revision_id).await?;
    do_action(Action::PostSaved, id);
    Ok(Redirect::to(&format!("/admin/content/{post_type}/{id}")).into_response())
}
