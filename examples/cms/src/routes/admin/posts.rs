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

/// Re-exported so the three call sites below read as they did.
///
/// The definition moved to `content` when `transition_status` started applying
/// it to the locked row: the check that matters is the one inside the
/// transaction, and two copies of a rule like this is how they drift.
use content::require_future_publish_date;

/// The statuses the editor's dropdown offers, in workflow order.
const STATUS_CHOICES: &[(&str, &str)] = &[
    ("draft", "Draft"),
    ("pending", "Pending review"),
    ("publish", "Published"),
    ("private", "Private"),
    ("future", "Scheduled"),
];

/// Whether the editor should offer `target` for a post currently at `current`.
///
/// Read from the state machine's own generated transition table rather than
/// from a second list kept in step by hand — a hand-maintained copy is exactly
/// how the dropdown came to offer `publish -> pending` and `publish -> future`,
/// which the graph does not declare, so choosing either was rejected *after*
/// the UI had explicitly offered it.
///
/// Guards are deliberately not evaluated. `draft -> publish` is guarded on
/// `can_publish`, which reads the *stored* title; filtering on it would hide
/// "Published" from an untitled draft even when the same submission supplies a
/// title. The guard still runs on the write path, where it can see what is
/// actually being saved.
fn status_is_offerable(current: Option<&str>, target: &str) -> bool {
    // A new post has no current state; the create path accepts any status the
    // author is allowed to choose.
    let Some(current) = current else {
        return true;
    };
    // Staying put is always an option — it is what "save without changing the
    // status" looks like in a single dropdown.
    current == target
        || crate::models::Post::__AUTUMN_SM_STATUS_TRANSITIONS
            .iter()
            .any(|(from, to, _guard)| *from == current && *to == target)
}

#[derive(Debug, Default, Deserialize)]
pub struct ListFilters {
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub s: Option<String>,
    #[serde(default)]
    pub page: Option<usize>,
}

/// How many rows one page of the content list shows.
const POSTS_PER_PAGE: i64 = 50;

/// Percent-encode one query-string value.
///
/// The pager carries the reader's status and search on every link, and a search
/// for `a&b` or `100%` would otherwise split the query string or decode as an
/// escape. Written out rather than pulled in: the workspace has no
/// URL-encoding dependency, and this is the only place the starter needs one.
fn query_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char);
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// What the editor submits.
///
/// Decoded by [`PostForm::from_body`] rather than the `Form` extractor.
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
    /// Checked term ids, keyed by taxonomy slug: `taxonomies[category]=3`.
    ///
    /// Keyed rather than a fixed `categories` field so the editor is driven by
    /// the registry: a plugin registering a taxonomy for this post type gets
    /// controls without the form growing a field. Special-casing `category` and
    /// `post_tag` is what left custom taxonomies creatable but unattachable.
    ///
    /// Decoded by `PostForm::from_body` rather than the `Form` extractor: a
    /// checkbox group posts the same key repeatedly, and `Form<T>` decodes
    /// bodies through `serde_urlencoded`, which has neither a
    /// repeated-key-to-sequence rule nor the bracketed-key one this needs.
    #[serde(default)]
    pub taxonomies: std::collections::HashMap<String, Vec<i64>>,
    /// Comma-separated term names for flat taxonomies, keyed by slug:
    /// `taxonomy_names[post_tag]=rust, web`. Names rather than ids because a
    /// flat taxonomy's box creates what it does not find, as WordPress's tag
    /// box does.
    #[serde(default)]
    pub taxonomy_names: std::collections::HashMap<String, String>,
    /// When scheduling: the local datetime the post goes live.
    #[serde(default)]
    pub publish_at: Option<String>,
    /// The `lock_version` the form was rendered from, for stale-edit
    /// detection. Absent on the create form, which has no row yet.
    #[serde(default)]
    pub lock_version: Option<String>,
}

impl PostForm {
    /// Decode a submitted editor form.
    ///
    /// `autumn_web::query_string::from_query_str` is the framework's superset
    /// parser: a flat body of unique scalar keys decodes exactly as
    /// `serde_urlencoded` would, and on top of that a repeated key becomes a
    /// sequence — which is precisely what an HTML checkbox group posts.
    ///
    /// The framework applies that parser to query strings only; `Form<T>` is
    /// documented as decoding bodies through `serde_urlencoded`, which has no
    /// repeated-key rule. So the category checkboxes made every save fail with
    /// "invalid type: string, expected a sequence" — checking a single box was
    /// enough. The feature did not work at all, and no test noticed because
    /// none of them ever ticked one.
    fn from_body(body: &str) -> AutumnResult<Self> {
        autumn_web::query_string::from_query_str(body).map_err(|err| {
            AutumnError::unprocessable_msg(format!("Failed to deserialize form body: {err}"))
        })
    }
}

/// Parse an optional numeric form field. An empty string means "not set",
/// which is different from a zero.
fn optional_id(raw: Option<&String>) -> Option<i64> {
    raw.map(|v| v.trim())
        .filter(|v| !v.is_empty())
        .and_then(|v| v.parse::<i64>().ok())
}

/// The publish date the editor's `datetime-local` field carries, as UTC.
///
/// `datetime-local` submits a wall-clock value with **no offset**. Storing it
/// as-is made it a UTC timestamp by accident, and the scheduler compares
/// against `Utc::now()` — so an editor in UTC-7 who scheduled a post for 09:00
/// had it publish at 02:00 their time. The site timezone is what turns the
/// wall-clock reading into the instant it names.
fn scheduled_at(
    form: &PostForm,
    settings: &crate::settings::Settings,
) -> AutumnResult<Option<chrono::NaiveDateTime>> {
    let Some(local) = form
        .publish_at
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .and_then(|value| chrono::NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M").ok())
    else {
        return Ok(None);
    };
    // `None` only for a local time that does not exist — the hour a
    // daylight-saving change skips. Saying so is better than silently
    // scheduling an hour the editor did not choose.
    settings.from_local(local).map(Some).ok_or_else(|| {
        AutumnError::unprocessable_msg(
            "That time does not exist in this site's timezone — daylight saving skips it",
        )
    })
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

    let settings = repos.settings().await?;

    // Type, status, search, the Contributor restriction, the order and the
    // bound are all asked of Postgres. Every branch here used to load the
    // complete matching rows — bodies included — sort them in Rust, render all
    // of them, and look the author up once per row; Authors and Contributors
    // grow this table continuously, so the screen used to manage content was
    // the one that stopped working first.
    let status = filters
        .status
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let search = filters
        .s
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    // A Contributor sees only their own content — the list must not advertise
    // what they cannot open.
    let author_id = (!user.role().can(Capability::EditOthersPosts)).then_some(user.id);
    let page = i64::try_from(filters.page.unwrap_or(1).clamp(1, 100_000)).unwrap_or(1);

    let (rows, total) = {
        let mut conn = repos.conn().await?;
        let query = content::AdminPostQuery {
            post_type: &post_type,
            status,
            search,
            author_id,
        };
        let (posts, total) = content::admin_posts_page(
            &mut conn,
            &query,
            (page - 1) * POSTS_PER_PAGE,
            POSTS_PER_PAGE,
        )
        .await?;
        let authors = content::authors_for_posts(&mut conn, &posts).await?;
        let rows: Vec<(Post, Option<User>)> = posts
            .into_iter()
            .map(|post| {
                let author = authors.get(&post.author_id).cloned();
                (post, author)
            })
            .collect();
        (rows, total)
    };
    let last_page = ((total + POSTS_PER_PAGE - 1) / POSTS_PER_PAGE).max(1);

    // Carried on every pager link so paging does not silently drop the filter
    // the reader is looking at.
    let carried = {
        let mut parts = Vec::new();
        if let Some(status) = status {
            parts.push(format!("status={}", query_escape(status)));
        }
        if let Some(search) = search {
            parts.push(format!("s={}", query_escape(search)));
        }
        parts.join("&")
    };
    let page_href = |target: i64| {
        if carried.is_empty() {
            format!("/admin/content/{post_type}?page={target}")
        } else {
            format!("/admin/content/{post_type}?{carried}&page={target}")
        }
    };

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
                                (settings.format_datetime(post.updated_at))
                            }
                        }
                    }
                    @if rows.is_empty() {
                        tr { td colspan="4" class="px-4 py-10 text-center text-gray-400" {
                            @if page > 1 { "Nothing on this page." } @else { "Nothing here yet." }
                        } }
                    }
                }
            }
        }

        @if last_page > 1 {
            nav aria-label="Content pages"
                class="flex items-center justify-between mt-6 text-sm" {
                @if page > 1 {
                    a href=(page_href(page - 1)) class="text-indigo-700 hover:underline" {
                        "← Newer"
                    }
                } @else {
                    span {}
                }
                span class="text-gray-500" { "Page " (page) " of " (last_page) }
                @if page < last_page {
                    a href=(page_href(page + 1)) class="text-indigo-700 hover:underline" {
                        "Older →"
                    }
                } @else {
                    span {}
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

/// One editor control for one registered taxonomy.
///
/// Built from the registry rather than from two hard-coded slugs, so a plugin
/// registering a taxonomy for this post type gets a working control — the
/// previous shape let an administrator create custom terms through the generic
/// term screens and then gave them no way to attach one to anything.
struct TaxonomyField {
    slug: &'static str,
    label: &'static str,
    hierarchical: bool,
    /// The terms offered in a hierarchical taxonomy's checkbox list — a
    /// bounded window, plus whatever this post already carries.
    terms: Vec<Term>,
    /// Whether the taxonomy holds more terms than the window is showing.
    truncated: bool,
    /// Which of them this post carries.
    selected: Vec<i64>,
    /// The comma-separated names, for a flat taxonomy's box.
    names: String,
}

/// Everything the editor form needs besides the post itself.
struct EditorContext {
    /// Carried so the editor can render and collect times in the site's zone
    /// rather than in UTC, without changing every `editor` caller.
    settings: crate::settings::Settings,
    taxonomies: Vec<TaxonomyField>,
    parents: Vec<Post>,
    /// Whether the type holds more rows than the parent picker is showing.
    parents_truncated: bool,
    media: Vec<Attachment>,
    /// Whether the library holds more than the picker is showing, so the
    /// editor can say so rather than appear to be the whole library.
    media_truncated: bool,
}

/// How many attachments the featured-image picker offers.
///
/// Paginating `/admin/media` did nothing for this control: opening any
/// thumbnail-capable editor still materialized the entire library and rendered
/// every row as an `<option>`, so the core authoring workflow was the one left
/// unprotected. The most recent uploads are what an author is choosing between;
/// the current selection is added to them explicitly below, whatever its age.
const MEDIA_PICKER_LIMIT: i64 = 100;

/// How many rows the hierarchical parent picker offers.
///
/// Same argument as `MEDIA_PICKER_LIMIT`: paginating the content list left the
/// editor itself loading every page of the type — bodies and all — and
/// rendering nearly all of them as `<option>`s, so the screen an author uses
/// most was the one still growing without bound. The most recently edited pages
/// are what a parent is chosen from; the current parent is added to them
/// explicitly below, whatever its age.
const PARENT_PICKER_LIMIT: i64 = 100;

/// How many terms a hierarchical taxonomy's checkbox list offers.
///
/// Terms accumulate through ordinary category creation *and* this editor's own
/// find-or-create box, so paginating the taxonomy screen left the authoring
/// form loading the whole taxonomy — descriptions included — and rendering a
/// checkbox per row. The post's own terms are added back explicitly below,
/// which is not optional: saving *replaces* a post's filings, so a selected
/// term missing from the form would be silently unfiled.
const TERM_PICKER_LIMIT: i64 = 100;

/// The most terms one save may apply, per taxonomy.
///
/// Both halves of `resolve_term_ids` need it, and for the same reason: the
/// submission is a form body, not a rendering of the bounded controls the
/// editor drew. A flat taxonomy's box is free text that could carry millions of
/// comma-separated names; a hierarchical taxonomy's checkbox list is a set of
/// ids a crafted request can enumerate over the whole taxonomy. Either way the
/// cost is per entry and the result is permanent — and enough filings on one
/// post would make that post's editor unbounded again, since the picker adds
/// every selected term back. An editor filing a post picks a handful.
const MAX_TERMS_PER_SAVE: usize = 50;

impl EditorContext {
    async fn load(repos: &Repos, registered: &PostType, post: Option<&Post>) -> AutumnResult<Self> {
        // One control per taxonomy this type registers, whatever they are.
        let assigned = match post {
            Some(post) => repos.post_terms(post.id).await?,
            None => Vec::new(),
        };
        let mut taxonomies = Vec::new();
        for taxonomy in content_types::taxonomies_for(registered.slug) {
            let mine: Vec<&Term> = assigned
                .iter()
                .filter(|term| term.taxonomy == taxonomy.slug)
                .collect();
            let selected: Vec<i64> = mine.iter().map(|term| term.id).collect();
            // A hierarchical taxonomy lists terms as checkboxes; a flat one
            // takes names, so it needs no term list.
            let (terms, truncated) = if taxonomy.hierarchical {
                let slug = taxonomy.slug;
                let selected = selected.clone();
                repos
                    .with_conn(async move |conn| {
                        let (mut rows, total) =
                            content::terms_page_with_total(conn, slug, 0, TERM_PICKER_LIMIT)
                                .await?;
                        // Whatever this post carries that the window missed,
                        // put first. Without it, saving an unchanged form
                        // would unfile the post from a term it is in.
                        let present: std::collections::HashSet<i64> =
                            rows.iter().map(|term| term.id).collect();
                        let missing: Vec<i64> = selected
                            .into_iter()
                            .filter(|id| !present.contains(id))
                            .collect();
                        if !missing.is_empty() {
                            let mut extra: Vec<Term> = content::terms_by_ids(conn, &missing)
                                .await?
                                .into_values()
                                .collect();
                            extra.sort_by(|a, b| a.name.cmp(&b.name));
                            rows.splice(0..0, extra);
                        }
                        Ok((rows, total > TERM_PICKER_LIMIT))
                    })
                    .await?
            } else {
                (Vec::new(), false)
            };
            taxonomies.push(TaxonomyField {
                slug: taxonomy.slug,
                label: taxonomy.plural,
                hierarchical: taxonomy.hierarchical,
                terms,
                truncated,
                selected,
                names: mine
                    .iter()
                    .map(|term| term.name.clone())
                    .collect::<Vec<_>>()
                    .join(", "),
            });
        }

        // A hierarchical type offers a parent selector. It excludes the post
        // itself *and* every descendant of it: picking a descendant closes a
        // cycle, and because page resolution walks down from a row whose
        // parent is NULL, every page in that cycle becomes unreachable at its
        // own permalink. The write path refuses it too (see `update`); this
        // just keeps the impossible option off the screen.
        let (parents, parents_truncated) = if registered.hierarchical {
            let current_id = post.map(|p| p.id);
            let current_parent = post.and_then(|p| p.parent_id);
            let slug = registered.slug.to_owned();
            repos
                .with_conn(async move |conn| {
                    let (rows, total) =
                        content::parent_candidates(conn, &slug, PARENT_PICKER_LIMIT).await?;
                    // Resolved in SQL rather than from the loaded set: the set
                    // is a window now, so a descendant outside it would have
                    // been offered as its own ancestor's parent.
                    let descendants = match current_id {
                        Some(id) => content::descendant_ids(conn, id).await?,
                        None => std::collections::HashSet::new(),
                    };
                    let mut rows: Vec<Post> = rows
                        .into_iter()
                        .filter(|p| Some(p.id) != current_id && !descendants.contains(&p.id))
                        .collect();
                    // The current parent, whatever its age. Without this a page
                    // whose parent has not been edited recently would be shown
                    // a select missing its own value, and saving the form
                    // unchanged would move the page to the top level — silently
                    // changing its canonical URL.
                    if let Some(parent_id) = current_parent
                        && !rows.iter().any(|p| p.id == parent_id)
                        && let Some(parent) = content::post_by_id(conn, parent_id).await?
                    {
                        rows.insert(0, parent);
                    }
                    Ok((rows, total > PARENT_PICKER_LIMIT))
                })
                .await?
        } else {
            (Vec::new(), false)
        };

        // Propagated, not swallowed. An empty library renders a form whose
        // featured-image select has only "(none)" selected — so saving that
        // otherwise-valid form silently clears the post's existing image, and
        // the error that caused it never surfaces. Failing the page is the
        // honest outcome: the editor cannot be saved from a state it was not
        // shown correctly.
        //
        // Bounded for the same reason, and the bound is what makes the
        // re-adding below necessary: a post whose featured image has since
        // scrolled past `MEDIA_PICKER_LIMIT` would otherwise be shown a select
        // that does not contain its own current value, and saving the form
        // unchanged would clear it — the exact failure the paragraph above is
        // about, reintroduced by the fix for the size.
        let (media, media_truncated) = if registered.supports_thumbnail {
            let (rows, total) = repos
                .with_conn(async |conn| {
                    let rows = content::attachments_page(conn, 0, MEDIA_PICKER_LIMIT).await?;
                    let total = content::attachment_count(conn).await?;
                    Ok((rows, total))
                })
                .await?;
            let mut rows = rows;
            if let Some(selected) = post.and_then(|p| p.featured_media_id)
                && !rows.iter().any(|item| item.id == selected)
                && let Some(current) = repos.attachments.find_by_id(selected).await?
            {
                rows.insert(0, current);
            }
            (rows, total > MEDIA_PICKER_LIMIT)
        } else {
            (Vec::new(), false)
        };

        Ok(Self {
            settings: repos.settings().await?,
            taxonomies,
            parents,
            parents_truncated,
            media,
            media_truncated,
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
                                // Two filters, for two different reasons: the
                                // capability one hides what this author may
                                // never choose, and the state-machine one hides
                                // what this *post* cannot reach from where it
                                // is. Both exist so the dropdown never offers
                                // something the write path will reject.
                                @if (can_publish || matches!(*value, "draft" | "pending"))
                                    && status_is_offerable(post.map(|p| p.status.as_str()), value) {
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
                            span class="text-gray-400 font-normal" {
                                "(for Scheduled, " (context.settings.timezone) ")"
                            }
                        }
                        // Rendered *and* read back in the site'"'"'s zone. The control
                        // submits a wall clock with no offset, so naming the zone
                        // beside it is part of the fix rather than decoration:
                        // whichever zone the browser is in, this field means the
                        // site'"'"'s.
                        input #publish_at type="datetime-local" name="publish_at"
                              value=(post.and_then(|p| p.published_at)
                                  .map(|d| context.settings.format_datetime_local(d))
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
                            @if registered.supports_revisions {
                                a href=(format!("/admin/content/{}/{}/revisions", registered.slug, post.id))
                                  class="text-indigo-700 hover:underline" { "Revisions" }
                            }
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

                // One control per registered taxonomy, hierarchical ones as a
                // checkbox list and flat ones as a comma-separated box. Driven
                // by the registry, so a plugin's taxonomy is editable here
                // without this markup knowing its name.
                @for field in &context.taxonomies {
                    @if field.hierarchical {
                        @if !field.terms.is_empty() {
                            fieldset class="bg-white rounded-lg shadow p-5" {
                                legend class="font-semibold text-sm px-1" { (field.label) }
                                div class="space-y-1 mt-2 max-h-56 overflow-y-auto" {
                                    @for term in &field.terms {
                                        label class="flex items-center gap-2 text-sm" {
                                            input type="checkbox"
                                                  name=(format!("taxonomies[{}]", field.slug))
                                                  value=(term.id)
                                                  checked[field.selected.contains(&term.id)]
                                                  class="rounded border-gray-300";
                                            (term.name)
                                        }
                                    }
                                }
                                @if field.truncated {
                                    p class="text-xs text-gray-400 mt-2" {
                                        "Showing the first " (TERM_PICKER_LIMIT) " by name. "
                                        a href=(format!("/admin/terms/{}", field.slug))
                                          class="text-indigo-700 hover:underline" {
                                            "Manage " (field.label.to_lowercase())
                                        }
                                        "."
                                    }
                                }
                            }
                        }
                    } @else {
                        div class="bg-white rounded-lg shadow p-5" {
                            label for=(format!("taxonomy-{}", field.slug))
                                  class="block font-semibold text-sm mb-2" { (field.label) }
                            input id=(format!("taxonomy-{}", field.slug)) type="text"
                                  name=(format!("taxonomy_names[{}]", field.slug))
                                  value=(field.names)
                                  placeholder="rust, web, async"
                                  class="w-full border rounded px-3 py-2 text-sm";
                            p class="text-xs text-gray-400 mt-1" {
                                "Comma separated. New entries are created automatically."
                            }
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
                        @if context.media_truncated {
                            p class="text-xs text-gray-400 mt-2" {
                                "Showing the " (MEDIA_PICKER_LIMIT) " most recent uploads. "
                                a href="/admin/media" class="text-indigo-700 hover:underline" {
                                    "Browse the library"
                                }
                                " to find an older one."
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
                            @if context.parents_truncated {
                                p class="text-xs text-gray-400 mt-2" {
                                    "Showing the " (PARENT_PICKER_LIMIT)
                                    " most recently edited."
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
    Path(post_type): Path<String>,
    body: String,
) -> AutumnResult<Response> {
    let user = require_capability!(repos, session, csrf, Capability::EditPosts);
    let form = PostForm::from_body(&body)?;
    let registered = resolve_type(&post_type)?;

    // A Contributor may only create drafts and submissions, whatever the form
    // says — the dropdown hides the other options, and this is what enforces it.
    let status = requested_status(&form, &user);

    // A scheduled post needs the date the author picked. Without it the row
    // would carry `published_at = NULL`, and the publish sweep — which selects
    // `status = 'future' AND published_at <= now()` — would never see it
    // again: the post would sit in `future` forever.
    let scheduled_for = scheduled_at(&form, &repos.settings().await?)?;
    require_future_publish_date(&status, scheduled_for)?;

    // Asked before anything is written. `private` and `future` are reached by
    // transitioning the draft this creates, and that edge carries the
    // `can_publish` guard — so a rejection after the insert left the draft, its
    // initial revision and its term assignments committed, with each retry
    // consuming another suffixed slug.
    content::guard_deferred_transition(&status, &form.title)?;

    // The same parent validation the update path runs. Creation skipped it
    // before, so repeated creates could build a hierarchy deeper than the
    // permalink builder renders — producing a canonical URL that starts
    // mid-tree and resolves to nothing.
    if let Some(parent_id) = optional_id(form.parent_id.as_ref()) {
        repos
            .with_conn(async |conn| {
                content::validate_parent(conn, None, registered.slug, parent_id).await
            })
            .await?;
    }

    // Resolved before the row exists, so the insert and the filings can share
    // one transaction. A post being created carries nothing forward, so this
    // needs only the type and the form.
    let term_ids = resolve_term_ids(&repos, registered.slug, None, &form).await?;

    let draft = NewPost {
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
    };

    // The insert and every write that completes it are one transaction.
    //
    // They were not: the insert went through the pool-backed allocator, so the
    // row committed and then the revision, the filings and any deferred
    // transition ran as later statements with an unwind behind them. An unwind
    // covers a failed statement; it does not cover a cancelled request or a
    // process that stops existing, and either left an immediately-public post
    // with no taxonomy or revision history, or a requested private/scheduled
    // post stuck as a draft, with no marker a retry could resume from.
    // `content::insert_post_with_unique_slug` allocates on this connection with
    // a savepoint per attempt, which is what makes one transaction possible —
    // the same change the importer took two rounds ago.
    let (created, transitioned) = repos
        .with_conn(async |conn| {
            use diesel_async::AsyncConnection as _;
            conn.transaction(async move |conn| {
                // Under the hierarchy lock, taken before the insert now that
                // the insert is in here: the pre-flight check above runs on a
                // released connection, so another editor can deepen the chosen
                // parent in between and leave this child past
                // `MAX_PAGE_DEPTH`, whose path `page_ancestry` then truncates.
                if optional_id(form.parent_id.as_ref()).is_some() {
                    content::lock_page_hierarchy(conn).await?;
                }
                let created = content::insert_post_with_unique_slug(conn, draft).await?;
                if let Some(parent_id) = created.parent_id {
                    content::validate_parent(conn, Some(created.id), registered.slug, parent_id)
                        .await?;
                }

                if registered.supports_revisions {
                    content::record_initial_revision(conn, &created).await?;
                }

                // Unconditionally, even for an empty set: `set_post_terms`
                // *replaces* the filings, so skipping it when the selection is
                // empty means "remove every category" quietly does nothing and
                // the post stays in archives it was taken out of. (A guard here
                // was a regression I introduced when this split out of
                // `apply_terms`.)
                content::set_post_terms(conn, created.id, term_ids).await?;

                if status == "future" || status == "private" {
                    let moved = content::transition_status(
                        conn,
                        created.id,
                        &status,
                        Some(user.id),
                        Some(&user),
                    )
                    .await?;
                    return Ok::<_, AutumnError>((moved, true));
                }
                Ok::<_, AutumnError>((created, false))
            })
            .await
        })
        .await?;

    // Actions fire only once the post is actually complete.
    if transitioned {
        // The same action the update, explicit-transition, API and scheduler
        // paths fire. Without it a plugin indexing or invalidating on this hook
        // missed exactly the admin-created private and scheduled posts.
        do_action(Action::PostTransitioned, created.id);
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
    Path((post_type, id)): Path<(String, i64)>,
    body: String,
) -> AutumnResult<Response> {
    let user = require_capability!(repos, session, csrf, Capability::EditPosts);
    let form = PostForm::from_body(&body)?;
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
    let scheduled_for = scheduled_at(&form, &repos.settings().await?)?;
    // The submitted date, not the stored one. Falling back to
    // `existing.published_at` is what let a past timestamp through.
    require_future_publish_date(&status, scheduled_for)?;

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
        repos
            .with_conn(async |conn| {
                content::validate_parent(conn, Some(id), &post_type, parent_id).await
            })
            .await?;
    }

    let desired = crate::hooks::normalize_slug(&form.slug, &form.title);
    let form_snapshot = (
        form.title.trim().to_owned(),
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
    //
    // Required here, not optional. `update_post_with_revision` takes an
    // `Option` because it also serves callers that legitimately have no form
    // behind them — the importer and the API — but for *this* handler a
    // missing, empty or unparseable value is not "no form", it is a form whose
    // guard has been removed. Falling through to `None` silently disabled the
    // stale-edit check, so a crafted save could overwrite an edit committed
    // after the form was loaded: the one thing optimistic locking exists to
    // prevent, defeated by omitting a field.
    let Some(expected_lock_version) = form
        .lock_version
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .and_then(|value| value.parse::<i32>().ok())
    else {
        return Err(AutumnError::unprocessable_msg(
            "This form is missing its version stamp — reload the editor and try again",
        ));
    };
    let expected_lock_version = Some(expected_lock_version);

    // The edit, its revision, the term replacement and any status transition
    // all commit together.
    //
    // They were three independent transactions, so another request trashing the
    // post in between made the last one reject an undeclared `trash -> private`
    // edge *after* the title, body, revision and taxonomy changes had already
    // committed — an error response for a save that had largely happened. The
    // three calls nest as savepoints inside this one, so the whole save is
    // atomic and the row lock `update_post_with_revision` takes is held for all
    // of it.
    //
    // The tag find-or-create stays outside: it is repository work on its own
    // connection, and a tag that survives a failed save is harmless — an
    // orphaned tag is visible and removable, unlike a half-applied post.
    let term_ids = resolve_term_ids(&repos, &post_type, Some(existing.id), &form).await?;

    // Retried on a slug collision, like the insert allocators.
    //
    // The slug is allocated on a connection released before this transaction
    // opens, so two edits racing for the same slug under the same parent can
    // both see it free — and the loser hit `idx_pages_parent_slug` (or either
    // of the others) with no retry, returning a raw constraint error where a
    // create would have got the next suffix. Re-allocating inside each attempt
    // is what makes the second pass find the slug taken.
    let parent_id = optional_id(form.parent_id.as_ref());
    let mut attempts = 0;
    let updated = loop {
        attempts += 1;
        let term_ids = term_ids.clone();
        let form_snapshot = form_snapshot.clone();
        let post_type = post_type.clone();
        let desired = desired.clone();
        let status = status.clone();
        let user = user.clone();
        let outcome = repos
            .with_conn(async |conn| {
                use diesel_async::AsyncConnection as _;
                conn.transaction(async move |conn| {
                    // Allocated *inside* the transaction, against the parent
                    // this save is moving the page to rather than the one it
                    // has — a page being re-filed competes with its new
                    // siblings, and allocating against the old ones would let
                    // it land on a slug already taken where it is going.
                    let slug = content::ensure_unique_slug(
                        conn,
                        &post_type,
                        &desired,
                        parent_id,
                        Some(id),
                    )
                    .await?;
                    let updated = content::update_post_with_revision(
                        conn,
                        id,
                        content::EditContext {
                            editor_id: user.id,
                            summary: "Edited".to_owned(),
                            expected_lock_version,
                            record_revision: registered.supports_revisions,
                            // Declared for the whole transaction, not for this
                            // call alone: a hierarchical type may be re-parented
                            // here, and a trash below reaches for the same lock.
                            // Either one taking it after this row lock puts the
                            // transaction on the opposite order from every
                            // create and re-parent — see `transition_status`.
                            may_touch_hierarchy: registered.hierarchical || status == "trash",
                            // Re-authorized against the row as locked. The
                            // check above this transaction ran on a released
                            // connection, and `lock_version` cannot stand in
                            // for it — it is form data.
                            actor: Some(user.clone()),
                        },
                        move |post| {
                            let (
                                title,
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
                            // The slug allocated by this attempt, not one
                            // captured before the transaction opened.
                            post.slug = slug;
                            post.excerpt = excerpt;
                            post.body = body;
                            post.password = password;
                            post.sticky = sticky;
                            post.comment_status =
                                if comments_open { "open" } else { "closed" }.to_owned();
                            post.parent_id = parent;
                            post.featured_media_id = media;
                            post.menu_order = order;
                            match scheduled_for {
                                Some(when) => post.published_at = Some(when),
                                // Blanking the date field on something that has
                                // never actually been live clears the timestamp.
                                // Leaving it made "unschedule this" keep the old
                                // due time, so publishing the draft later dated and
                                // ordered it at a moment that never happened — and
                                // with a future date, gave it a dated permalink and
                                // archive in the future. `validate_post_update`
                                // stamps `now` when a post with no date goes live,
                                // which is what the editor meant.
                                //
                                // Only for a row that is not live: for a published
                                // or private post `published_at` is a publication
                                // record, and an edit must never reorder the blog
                                // index.
                                None if !matches!(post.status.as_str(), "publish" | "private") => {
                                    post.published_at = None;
                                }
                                None => {}
                            }
                        },
                    )
                    .await?;

                    // Unconditionally — see `create`. An empty selection is a
                    // deliberate "file this under nothing", not "leave it alone".
                    content::set_post_terms(conn, id, term_ids).await?;

                    // A status change goes through the state machine, never through the
                    // plain field write above — so an illegal edge is refused rather
                    // than persisted, and now it is refused before anything commits.
                    let transitioned = status != updated.status;
                    if transitioned {
                        content::transition_status(conn, id, &status, Some(user.id), Some(&user))
                            .await?;
                    }
                    Ok::<_, AutumnError>((updated, transitioned))
                })
                .await
            })
            .await;
        match outcome {
            Ok(value) => break value,
            Err(error)
                if attempts < 5
                    && autumn_web::error::unique_violation_field(
                        &error,
                        content::SLUG_COLLISION_INDEXES,
                    )
                    .is_some() =>
            {
                // Re-allocate against the state the winner left behind.
                continue;
            }
            Err(error) => return Err(error),
        }
    };
    let (_updated, transitioned) = updated;

    // Actions fire only after the transaction has committed: a listener that
    // reads the post back must not see a state that is about to roll back.
    if transitioned {
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

/// The term ids this submission asks for, across every taxonomy the post type
/// registers, creating terms for a flat taxonomy's names as WordPress does.
///
/// Driven by the registry rather than by the two built-in slugs, so a plugin's
/// taxonomy is attachable through the same code that handles categories and
/// tags. Special-casing `category` and `post_tag` is what left a custom
/// taxonomy creatable through the term screens and unattachable from anywhere.
/// Takes the type and an *optional* id rather than a `Post`, so the creation
/// path can resolve the selection before the row exists — which is what lets
/// the insert and the filings share one transaction. A post being created has
/// no filings to carry forward, so `None` is the honest input rather than a
/// stand-in row.
async fn resolve_term_ids(
    repos: &Repos,
    post_type: &str,
    post_id: Option<i64>,
    form: &PostForm,
) -> AutumnResult<Vec<i64>> {
    let taxonomies = content_types::taxonomies_for(post_type);
    if taxonomies.is_empty() {
        return Ok(Vec::new());
    }

    let mut term_ids: Vec<i64> = Vec::new();

    // Filings in taxonomies this editor does not render are carried forward.
    // `set_post_terms` replaces the whole set, so anything the form did not
    // reconstruct would be deleted — and the form is not evidence about a
    // taxonomy it never showed. Now that the controls are registry-driven, that
    // set is exactly "taxonomies this post type does not register", which is
    // what an import or a direct write can leave behind.
    let rendered: std::collections::HashSet<&str> =
        taxonomies.iter().map(|taxonomy| taxonomy.slug).collect();
    if let Some(post_id) = post_id {
        for term in repos.post_terms(post_id).await? {
            if !rendered.contains(term.taxonomy.as_str()) {
                term_ids.push(term.id);
            }
        }
    }

    for taxonomy in &taxonomies {
        if taxonomy.hierarchical {
            // Ids, resolved and filtered rather than trusted: they come from a
            // form, and `set_post_terms` checks neither the term's taxonomy nor
            // whether that taxonomy applies to this post type. A crafted
            // submission could otherwise file a post under a taxonomy
            // registered for something else, after which that taxonomy's public
            // archive listed it.
            let Some(submitted) = form.taxonomies.get(taxonomy.slug) else {
                continue;
            };
            // Deduplicated first, so a form repeating one id cannot spend the
            // budget, and bounded before any database work.
            let mut seen = std::collections::HashSet::new();
            let requested: Vec<i64> = submitted
                .iter()
                .copied()
                .filter(|id| seen.insert(*id))
                .collect();
            if requested.len() > MAX_TERMS_PER_SAVE {
                return Err(AutumnError::unprocessable_msg(format!(
                    "At most {MAX_TERMS_PER_SAVE} {} can be applied in one save",
                    taxonomy.plural.to_lowercase()
                )));
            }
            // One query for the whole set rather than one per id. The filter is
            // still the point — `set_post_terms` checks neither the term's
            // taxonomy nor whether that taxonomy applies to this post type, so
            // a crafted submission could otherwise file a post under a taxonomy
            // registered for something else, after which that taxonomy's public
            // archive listed it.
            let slug = taxonomy.slug;
            let lookup = requested.clone();
            let accepted = repos
                .with_conn(async move |conn| {
                    content::term_ids_in_taxonomy(conn, slug, &lookup).await
                })
                .await?;
            // From the deduplicated set, not the raw submission: a form
            // repeating an id would otherwise file the post under it twice.
            term_ids.extend(requested.into_iter().filter(|id| accepted.contains(id)));
        } else {
            // Names, find-or-create — a flat taxonomy's box creates what it
            // does not find, matching WordPress's tag box.
            let Some(submitted) = form.taxonomy_names.get(taxonomy.slug) else {
                continue;
            };
            // Bounded *before* any database work: find-or-create means one
            // lookup and possibly one insert per name. See
            // `MAX_TERMS_PER_SAVE`.
            if submitted
                .split(',')
                .filter(|n| !n.trim().is_empty())
                .count()
                > MAX_TERMS_PER_SAVE
            {
                return Err(AutumnError::unprocessable_msg(format!(
                    "At most {MAX_TERMS_PER_SAVE} {} can be applied in one save",
                    taxonomy.plural.to_lowercase()
                )));
            }
            for raw in submitted.split(',') {
                let name = raw.trim();
                if name.is_empty() {
                    continue;
                }
                // The model's own limit, checked here so an over-long name is a
                // clear message rather than a validation failure after the
                // lookup has already run.
                // The shared constant, so the editor's box and the importer's
                // direct insert cannot drift — they had.
                use crate::hooks::MAX_TERM_NAME;
                if name.chars().count() > MAX_TERM_NAME {
                    return Err(AutumnError::unprocessable_msg(format!(
                        "`{}…` is too long; {} names are limited to {MAX_TERM_NAME} characters",
                        name.chars().take(30).collect::<String>(),
                        taxonomy.singular.to_lowercase()
                    )));
                }
                let slug = autumn_web::slugify(name);
                if slug.is_empty() {
                    continue;
                }
                let existing = repos
                    .terms
                    .find_by_slug(slug.clone())
                    .await?
                    .into_iter()
                    .find(|t| t.taxonomy == taxonomy.slug);
                let term = match existing {
                    Some(term) => term,
                    None => {
                        // The tag box is a *third* term-creation path, after
                        // the term screen and the importer — and the one an
                        // author reaches without meaning to create anything.
                        // A configured probe at `/tag/status` claims that
                        // archive whichever door the term came through.
                        content::guard_term_path(taxonomy.slug, &slug)?;
                        match repos
                            .terms
                            .save(&crate::models::NewTerm {
                                taxonomy: taxonomy.slug.to_owned(),
                                name: name.to_owned(),
                                slug: slug.clone(),
                                description: String::new(),
                                parent_id: None,
                            })
                            .await
                        {
                            Ok(term) => term,
                            // Somebody else created the same new term between
                            // the lookup and the insert — `idx_terms_taxonomy_slug`
                            // says so. Find-or-create means the *outcome* is
                            // what matters, and the outcome is now satisfied,
                            // so adopting their row is right where failing the
                            // whole save would be absurd. Two editors tagging
                            // posts `rust` at the same time is ordinary.
                            Err(error) => repos
                                .terms
                                .find_by_slug(slug)
                                .await?
                                .into_iter()
                                .find(|t| t.taxonomy == taxonomy.slug)
                                .ok_or(error)?,
                        }
                    }
                };
                term_ids.push(term.id);
            }
        }
    }

    term_ids.sort_unstable();
    term_ids.dedup();

    // Only the ids. The *write* belongs to whatever transaction the caller is
    // running, so it commits or rolls back with the rest of the save — see the
    // update handler.
    Ok(term_ids)
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

    // This endpoint carries no date, so it can only move a post to `future`
    // when the row already holds a future one. Otherwise it produces a
    // scheduled post that either never publishes (`published_at IS NULL`, which
    // the sweep's `published_at <= now` never matches) or publishes on the very
    // next sweep (a retained past date). The editor asks for a date; this is
    // the one-click control, and the honest answer here is to send the user
    // there.
    require_future_publish_date(&query.to, post.published_at).map_err(|_| {
        AutumnError::unprocessable_msg(
            "Scheduling needs a publish date — open the editor and pick one",
        )
    })?;

    repos
        .with_conn(async |conn| {
            content::transition_status(conn, id, &query.to, Some(user.id), Some(&user)).await
        })
        .await?;
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

    // A type registered `supports_revisions: false` has no history to show, and
    // the editor hides the link — but a hidden link is not a closed route. This
    // is the same 404 an unknown type gets.
    if !content::type_supports_revisions(&post.post_type) {
        return Err(AutumnError::not_found_msg(
            "This content type does not keep revisions",
        ));
    }

    let settings = repos.settings().await?;
    let history = repos
        .with_conn(async |conn| content::revisions_for(conn, id).await)
        .await?;

    // Who made each edit. The column was always there and always stored the
    // post's owner, so on a collaborative site it was quietly wrong — and
    // nothing rendered it, which is why nobody could notice. It records the
    // acting editor now, and the history says so.
    let mut names: std::collections::HashMap<i64, String> = std::collections::HashMap::new();
    for author_id in history.iter().filter_map(|revision| revision.author_id) {
        if let std::collections::hash_map::Entry::Vacant(slot) = names.entry(author_id)
            && let Some(account) = repos.users.find_by_id(author_id).await?
        {
            slot.insert(account.public_name().to_owned());
        }
    }

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
                            (settings.format_datetime(revision.created_at))
                            " · " (revision.summary)
                            " · " (revision.status)
                            @if let Some(name) = revision.author_id.and_then(|a| names.get(&a)) {
                                " · by " (name)
                            }
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

    if !content::type_supports_revisions(&post.post_type) {
        return Err(AutumnError::not_found_msg(
            "This content type does not keep revisions",
        ));
    }

    repos
        .with_conn(async |conn| {
            content::restore_revision(conn, id, revision_id, Some(user.id), Some(&user)).await
        })
        .await?;
    do_action(Action::PostSaved, id);
    Ok(Redirect::to(&format!("/admin/content/{post_type}/{id}")).into_response())
}
