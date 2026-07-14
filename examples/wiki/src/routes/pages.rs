use autumn_web::extract::Path;
use autumn_web::prelude::*;
use diesel::prelude::*;
use diesel_async::RunQueryDsl;

use crate::models::{NewPage, NewRevision, Page, Revision};
use crate::repositories::{PageRepository, PgPageRepository};
use crate::schema::revisions;

pub fn layout(title: &str, content: Markup) -> Markup {
    html! {
        (PreEscaped("<!DOCTYPE html>"))
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (title) " — Wiki" }
                link rel="stylesheet" href=(autumn_web::ui::WIDGETS_CSS_PATH);
                link rel="stylesheet" href="/static/css/autumn.css";
                script src="/static/js/htmx.min.js" {}
            }
            body class="bg-gray-50 min-h-screen" {
                nav class="bg-emerald-700 text-white p-4" {
                    div class="max-w-4xl mx-auto flex justify-between items-center" {
                        a href=(paths::list()) class="text-xl font-bold" { "Wiki" }
                        div class="space-x-4 text-sm" {
                            a href=(paths::new_form()) class="opacity-75 hover:opacity-100" { "+ New Page" }
                            a href="/actuator/health" class="opacity-75 hover:opacity-100" { "Health" }
                        }
                    }
                }
                main class="max-w-4xl mx-auto p-6" { (content) }
            }
        }
    }
}

pub fn status_badge(status: &str) -> Markup {
    let color = match status {
        "published" => "bg-green-100 text-green-700",
        "draft" => "bg-yellow-100 text-yellow-700",
        "archived" => "bg-gray-200 text-gray-600",
        _ => "bg-gray-100 text-gray-500",
    };
    html! {
        span class=(format!("text-xs rounded px-2 py-0.5 {color}")) { (status) }
    }
}

pub fn pages_list_snippet(pages: &[Page]) -> Markup {
    html! {
        ul id="search-results" class="space-y-3" {
            @for p in pages {
                li class="p-4 bg-white rounded shadow flex justify-between items-center" {
                    div {
                        a href=(paths::show(p.slug.clone()))
                          class="text-emerald-700 font-medium hover:underline" {
                            (p.title)
                          }
                        " "
                        (status_badge(&p.status))
                    }
                    div class="text-sm text-gray-400" {
                        (p.updated_at.format("%Y-%m-%d %H:%M"))
                    }
                }
            }
            @if pages.is_empty() {
                li class="text-gray-400 text-center py-8" { "No pages found. Create one!" }
            }
        }
    }
}

#[derive(serde::Deserialize)]
pub struct SearchParams {
    #[serde(default)]
    pub q: String,
}

#[get("/")]
pub async fn list(repo: PgPageRepository) -> AutumnResult<Markup> {
    let pages = repo.find_all().await?;
    Ok(layout(
        "All Pages",
        html! {
            div class="flex justify-between items-center mb-6" {
                h1 class="text-2xl font-bold" { "All Pages" }
                a href=(paths::new_form())
                  class="bg-emerald-600 text-white px-4 py-2 rounded hover:bg-emerald-700" {
                    "+ New Page"
                  }
            }
            div class="mb-6 bg-white p-4 rounded shadow flex items-center" {
                input type="search" name="q" placeholder="Search pages..."
                      hx-get="/search" hx-trigger="keyup changed delay:300ms, search"
                      hx-target="#search-results" hx-swap="outerHTML" hx-indicator="#search-indicator"
                      class="flex-grow border rounded px-3 py-2 text-sm focus:outline-none focus:ring-1 focus:ring-emerald-500";
                span id="search-indicator" class="htmx-indicator ml-3 text-sm text-gray-400" {
                    "Searching..."
                }
            }
            (pages_list_snippet(&pages))
        },
    ))
}

#[get("/search")]
pub async fn search(
    repo: PgPageRepository,
    Query(params): Query<SearchParams>,
) -> AutumnResult<Markup> {
    let term = params.q.trim();
    let pages = if term.is_empty() {
        repo.find_all().await?
    } else {
        repo.search(term).await?
    };
    Ok(pages_list_snippet(&pages))
}

#[get("/pages/{slug}")]
pub async fn show(
    Path(slug): Path<String>,
    repo: PgPageRepository,
    mut db: Db,
) -> AutumnResult<Markup> {
    let page = find_page_by_slug(&repo, &slug).await?;

    let revs: Vec<Revision> = revisions::table
        .filter(revisions::page_id.eq(page.id))
        .order(revisions::created_at.desc())
        .limit(5)
        .select(Revision::as_select())
        .load(&mut *db)
        .await?;

    Ok(layout(
        &page.title,
        html! {
            (breadcrumb(&[
                Crumb::link("Wiki", &paths::list()),
                Crumb::current(&page.title),
            ]))
            article {
                div class="flex justify-between items-center mb-4" {
                    h1 class="text-3xl font-bold" { (page.title) }
                    div class="space-x-2 flex items-center" {
                        (status_badge(&page.status))
                        a href=(paths::edit_form(page.slug.clone()))
                          class="text-sm text-emerald-600 hover:underline" { "Edit" }
                        a href=(paths::history(page.slug.clone()))
                          class="text-sm text-gray-500 hover:underline" { "History" }
                    }
                }
                // State-machine transition buttons, generated from the same
                // `#[state_machine(...)]` declaration on `Page::status` that the
                // `transition_status` route enforces. Only edges leaving the
                // current state render; the `can_transition_status_to` guard
                // disables an edge (e.g. publishing an empty page). No CSRF token
                // is threaded here — the wiki example runs without CSRF, like its
                // other forms.
                (autumn_web::widgets::transition_controls(
                    &paths::transition_status(page.slug.clone()),
                    "status",
                    &page.status,
                    Page::__AUTUMN_SM_STATUS_TRANSITIONS,
                    |to| page.can_transition_status_to(to),
                    None,
                    None,
                ))
                div class="prose bg-white rounded shadow p-6" {
                    @for para in page.body.split("\n\n") {
                        @if !para.trim().is_empty() {
                            p { (para) }
                        }
                    }
                    @if page.body.is_empty() {
                        p class="text-gray-400 italic" { "This page has no content yet." }
                    }
                }
                @if !revs.is_empty() {
                    div class="mt-6" {
                        h2 class="text-lg font-semibold mb-2" { "Recent Revisions" }
                        ul class="space-y-1 text-sm text-gray-500" {
                            @for r in &revs {
                                li {
                                    span class="font-mono" { (r.op) }
                                    " — "
                                    @if let Some(ref summary) = r.summary {
                                        (summary)
                                        " — "
                                    }
                                    (r.created_at.format("%Y-%m-%d %H:%M"))
                                }
                            }
                        }
                        a href=(paths::history(page.slug.clone()))
                          class="text-sm text-emerald-600 hover:underline" { "View full history" }
                    }
                }
                div class="mt-4 text-sm text-gray-400" {
                    "Last updated: " (page.updated_at.format("%Y-%m-%d %H:%M"))
                }
            }
        },
    ))
}

#[get("/new")]
pub async fn new_form() -> Markup {
    layout(
        "New Page",
        html! {
            (breadcrumb(&[
                Crumb::link("Wiki", &paths::list()),
                Crumb::current("New Page"),
            ]))
            h1 class="text-2xl font-bold mb-6" { "New Page" }
            form action=(paths::create()) method="post"
                 class="space-y-4 bg-white rounded shadow p-6" {
                div {
                    label for="title" class="block text-sm font-medium" { "Title" }
                    input type="text" id="title" name="title" required
                          placeholder="My Awesome Page"
                          class="w-full border rounded p-2 mt-1";
                }
                div {
                    label for="body" class="block text-sm font-medium" { "Body" }
                    textarea id="body" name="body" rows="10"
                             placeholder="Write your content here..."
                             class="w-full border rounded p-2 mt-1" {}
                }
                div {
                    label for="status" class="block text-sm font-medium" { "Status" }
                    select id="status" name="status" class="border rounded p-2 mt-1" {
                        option value="draft" { "Draft" }
                        option value="published" { "Published" }
                    }
                }
                button type="submit"
                       class="bg-emerald-600 text-white px-6 py-2 rounded hover:bg-emerald-700" {
                    "Create Page"
                }
            }
        },
    )
}

#[post("/pages")]
pub async fn create(
    repo: PgPageRepository,
    mut db: Db,
    form: Form<PageForm>,
) -> AutumnResult<Redirect> {
    let new_page = form.0.into_new();
    let page = repo.save(&new_page).await?;

    diesel::insert_into(revisions::table)
        .values(&NewRevision {
            page_id: page.id,
            op: "create".into(),
            title: page.title.clone(),
            body: page.body.clone(),
            status: page.status.clone(),
            changed_by: None,
            summary: None,
        })
        .execute(&mut *db)
        .await?;

    Ok(Redirect::to(&paths::show(page.slug)))
}

#[get("/pages/{slug}/edit")]
pub async fn edit_form(Path(slug): Path<String>, repo: PgPageRepository) -> AutumnResult<Markup> {
    let page = find_page_by_slug(&repo, &slug).await?;

    Ok(layout(
        &format!("Edit: {}", page.title),
        html! {
            (breadcrumb(&[
                Crumb::link("Wiki", &paths::list()),
                Crumb::link(&page.title, &paths::show(page.slug.clone())),
                Crumb::current("Edit"),
            ]))
            h1 class="text-2xl font-bold mb-6" { "Edit: " (page.title) }
            form action=(paths::update(page.slug.clone())) method="post"
                 class="space-y-4 bg-white rounded shadow p-6" {
                div {
                    label for="title" class="block text-sm font-medium" { "Title" }
                    input type="text" id="title" name="title" required value=(page.title)
                          class="w-full border rounded p-2 mt-1";
                }
                div {
                    label for="body" class="block text-sm font-medium" { "Body" }
                    textarea id="body" name="body" rows="10"
                             class="w-full border rounded p-2 mt-1" { (page.body) }
                }
                p class="text-sm text-gray-500" {
                    "Status: " (status_badge(&page.status))
                    " — change it from the "
                    a href=(paths::show(page.slug.clone())) class="text-emerald-600 hover:underline" { "page" }
                    " using the status controls."
                }
                input type="hidden" name="lock_version" value=(page.lock_version);
                button type="submit"
                       class="bg-emerald-600 text-white px-6 py-2 rounded hover:bg-emerald-700" {
                    "Save Changes"
                }
            }
        },
    ))
}

/// Form struct for page edits. We cannot use `Form<UpdatePage>` directly
/// because `UpdatePage` wraps every field in `Patch<T>` (for partial updates
/// via JSON). HTML forms always submit all fields, so we deserialize into
/// plain values here and convert to the `UpdatePage` type.
#[derive(serde::Deserialize)]
pub struct PageForm {
    pub title: String,
    pub body: String,
    // Only the create form submits an initial status; the edit form edits
    // title/body and leaves status untouched (status changes flow through the
    // dedicated transitions route), so this defaults when absent.
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub lock_version: i32,
}

impl PageForm {
    fn into_new(self) -> NewPage {
        NewPage {
            title: self.title,
            slug: String::new(), // auto-generated by before_create hook
            body: self.body,
            status: self.status,
        }
    }

    pub fn into_update(self) -> crate::models::UpdatePage {
        crate::models::UpdatePage {
            title: Patch::Set(self.title),
            slug: Patch::Unchanged,
            body: Patch::Set(self.body),
            // Status transitions go through `transition_status`, never the edit
            // form, so an edit leaves the stored status unchanged.
            status: Patch::Unchanged,
            lock_version: self.lock_version,
        }
    }
}

/// Form body posted by the `transition_controls` widget on the show page: the
/// target state under the field name (`status`).
#[derive(serde::Deserialize)]
pub struct TransitionForm {
    pub status: String,
}

pub(crate) fn generate_update_summary(
    old_status: &str,
    new_status: &str,
    old_title: &str,
    new_title: &str,
) -> Option<String> {
    if new_status != old_status {
        Some(format!("Status changed: {} → {}", old_status, new_status))
    } else if new_title != old_title {
        Some(format!("Title changed: {} → {}", old_title, new_title))
    } else {
        None
    }
}

#[post("/pages/{slug}")]
pub async fn update(
    Path(slug): Path<String>,
    repo: PgPageRepository,
    mut db: Db,
    form: Form<PageForm>,
) -> AutumnResult<Redirect> {
    let page = find_page_by_slug(&repo, &slug).await?;
    let update_page = form.0.into_update();
    let updated = repo.update(page.id, &update_page).await?;

    let summary =
        generate_update_summary(&page.status, &updated.status, &page.title, &updated.title);

    diesel::insert_into(revisions::table)
        .values(&NewRevision {
            page_id: updated.id,
            op: "update".into(),
            title: updated.title.clone(),
            body: updated.body.clone(),
            status: updated.status.clone(),
            changed_by: None,
            summary,
        })
        .execute(&mut *db)
        .await?;

    Ok(Redirect::to(&paths::show(updated.slug)))
}

/// `POST /pages/{slug}/transitions/status` — apply a state-machine transition.
///
/// The `transition_controls` widget on the show page posts the target state
/// under the field name (`status`). Enforcement is the macro-generated
/// `Page::transition_status_to`, which returns a 400 for an undefined edge or a
/// rejected `can_publish` guard — surfaced here by `?`, matching the wiki's
/// error-propagation convention (no Flash layer is configured).
#[post("/pages/{slug}/transitions/status")]
pub async fn transition_status(
    Path(slug): Path<String>,
    repo: PgPageRepository,
    mut db: Db,
    form: Form<TransitionForm>,
) -> AutumnResult<Redirect> {
    let page = find_page_by_slug(&repo, &slug).await?;

    // The state machine is the single source of truth: illegal edges and
    // guard rejections become a 400 here.
    let new_status = page.transition_status_to(&form.0.status)?;

    let update = crate::models::UpdatePage {
        title: Patch::Unchanged,
        slug: Patch::Unchanged,
        body: Patch::Unchanged,
        status: Patch::Set(new_status),
        lock_version: page.lock_version,
    };
    let updated = repo.update(page.id, &update).await?;

    let summary =
        generate_update_summary(&page.status, &updated.status, &page.title, &updated.title);

    diesel::insert_into(revisions::table)
        .values(&NewRevision {
            page_id: updated.id,
            op: "update".into(),
            title: updated.title.clone(),
            body: updated.body.clone(),
            status: updated.status.clone(),
            changed_by: None,
            summary,
        })
        .execute(&mut *db)
        .await?;

    Ok(Redirect::to(&paths::show(updated.slug)))
}

#[get("/pages/{slug}/history")]
pub async fn history(
    Path(slug): Path<String>,
    repo: PgPageRepository,
    mut db: Db,
) -> AutumnResult<Markup> {
    let page = find_page_by_slug(&repo, &slug).await?;

    let revs: Vec<Revision> = revisions::table
        .filter(revisions::page_id.eq(page.id))
        .order(revisions::created_at.desc())
        .select(Revision::as_select())
        .load(&mut *db)
        .await?;

    Ok(layout(
        &format!("History: {}", page.title),
        html! {
            (breadcrumb(&[
                Crumb::link("Wiki", &paths::list()),
                Crumb::link(&page.title, &paths::show(page.slug.clone())),
                Crumb::current("History"),
            ]))
            div class="flex justify-between items-center mb-6" {
                h1 class="text-2xl font-bold" { "History: " (page.title) }
                a href=(paths::show(page.slug.clone()))
                  class="text-sm text-emerald-600 hover:underline" { "Back to page" }
            }
            @if revs.is_empty() {
                p class="text-gray-400 text-center py-8" { "No revisions recorded." }
            } @else {
                div class="space-y-4" {
                    @for r in &revs {
                        div class="p-4 bg-white rounded shadow" {
                            div class="flex justify-between items-center mb-2" {
                                div class="flex items-center space-x-2" {
                                    span class="font-mono text-sm bg-gray-100 rounded px-2 py-0.5" {
                                        (r.op)
                                    }
                                    (status_badge(&r.status))
                                }
                                span class="text-sm text-gray-400" {
                                    (r.created_at.format("%Y-%m-%d %H:%M:%S"))
                                }
                            }
                            @if let Some(ref summary) = r.summary {
                                p class="text-sm text-gray-600" { (summary) }
                            }
                            details class="mt-2" {
                                summary class="text-sm text-gray-500 cursor-pointer hover:text-gray-700" {
                                    "Show snapshot"
                                }
                                div class="mt-2 p-3 bg-gray-50 rounded text-sm" {
                                    h3 class="font-medium" { (r.title) }
                                    @for para in r.body.split("\n\n") {
                                        @if !para.trim().is_empty() {
                                            p class="mt-1 text-gray-600" { (para) }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        },
    ))
}

autumn_web::paths![
    list,
    show,
    new_form,
    create,
    edit_form,
    update,
    transition_status,
    history,
    search
];

/// Look up a page by slug, returning 404 if not found.
async fn find_page_by_slug(repo: &PgPageRepository, slug: &str) -> AutumnResult<Page> {
    let pages = repo.find_by_slug(slug.to_owned()).await?;
    pages
        .into_iter()
        .next()
        .ok_or_else(|| AutumnError::not_found_msg(format!("Page '{slug}' not found")))
}
