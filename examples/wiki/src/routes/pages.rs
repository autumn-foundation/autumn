use autumn_web::extract::Path;
use autumn_web::prelude::*;
use diesel::prelude::*;
use diesel_async::RunQueryDsl;
use scoped_futures::ScopedFutureExt;

use crate::models::{NewPage, NewRevision, Page, Revision};
use crate::repositories::{PageRepository, PgPageRepository};
use crate::schema::{pages, revisions};

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
                a href="#main-content"
                  class="skip-link sr-only focus:not-sr-only focus:absolute focus:top-2 focus:left-2 \
                         focus:z-50 focus:px-4 focus:py-2 focus:bg-white focus:text-gray-900 \
                         focus:border focus:border-gray-300 focus:rounded focus:shadow" {
                    "Skip to main content"
                }
                nav class="bg-emerald-700 text-white p-4" {
                    div class="max-w-4xl mx-auto flex justify-between items-center" {
                        a href=(paths::list()) class="text-xl font-bold" { "Wiki" }
                        div class="space-x-4 text-sm" {
                            a href=(paths::new_form()) class="opacity-75 hover:opacity-100" { "+ New Page" }
                            a href="/collections" class="opacity-75 hover:opacity-100" { "Collections" }
                            a href="/actuator/health" class="opacity-75 hover:opacity-100" { "Health" }
                        }
                    }
                }
                main id="main-content" class="max-w-4xl mx-auto p-6" { (content) }
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

/// Look up the message [`PageForm::validate_fields`] recorded against
/// `field`, if any.
fn field_error<'a>(errors: &'a [(&str, &str)], field: &str) -> Option<&'a str> {
    errors
        .iter()
        .find(|(f, _)| *f == field)
        .map(|(_, msg)| *msg)
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
                      aria-label="Search pages"
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

/// Render the "New Page" form. `data` holds the values to show — blank
/// defaults on the initial GET, or the author's just-rejected submission on
/// a failed POST — and `errors` is whatever [`PageForm::validate_fields`]
/// found wrong with it (empty on the GET path). Sharing this between the GET
/// route and the POST route's 422 branch is what makes a rejected submission
/// redisplay with the author's title/body/status intact and a message next
/// to the field that failed, instead of losing the draft to the generic
/// error page `PageHooks::before_create`'s `?` used to produce.
fn new_page_form(data: &PageForm, errors: &[(&str, &str)]) -> Markup {
    let title_error = field_error(errors, "title");
    let body_error = field_error(errors, "body");

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
                      value=(data.title)
                      placeholder="My Awesome Page"
                      aria-invalid=(if title_error.is_some() { "true" } else { "false" })
                      aria-describedby="title-error"
                      class="w-full border rounded p-2 mt-1";
                div id="title-error" {
                    @if let Some(msg) = title_error {
                        p class="text-red-600 text-xs mt-1" role="alert" { (msg) }
                    }
                }
            }
            div {
                label for="body" class="block text-sm font-medium" { "Body" }
                textarea id="body" name="body" rows="10"
                         placeholder="Write your content here..."
                         aria-invalid=(if body_error.is_some() { "true" } else { "false" })
                         aria-describedby="body-error"
                         class="w-full border rounded p-2 mt-1" { (data.body) }
                div id="body-error" {
                    @if let Some(msg) = body_error {
                        p class="text-red-600 text-xs mt-1" role="alert" { (msg) }
                    }
                }
            }
            div {
                label for="status" class="block text-sm font-medium" { "Status" }
                select id="status" name="status" class="border rounded p-2 mt-1" {
                    option value="draft" selected[data.status != "published"] { "Draft" }
                    option value="published" selected[data.status == "published"] { "Published" }
                }
            }
            button type="submit"
                   class="bg-emerald-600 text-white px-6 py-2 rounded hover:bg-emerald-700" {
                "Create Page"
            }
        }
    }
}

#[get("/new")]
pub async fn new_form() -> Markup {
    layout("New Page", new_page_form(&PageForm::default(), &[]))
}

/// Create a new page from a form submission.
///
/// On validation failure (publishing with an empty title or body) the
/// new-page form is re-rendered with the author's draft intact and a message
/// next to the field that failed (422), instead of the generic error page
/// `PageHooks::before_create`'s `bad_request_msg` used to produce via the
/// handler's `?` — which dropped the author off the form and discarded
/// whatever title/body they had typed. A draft may still be saved empty;
/// only publishing enforces [`Page::can_publish`], mirroring the hook.
#[post("/pages")]
pub async fn create(
    repo: PgPageRepository,
    mut db: Db,
    form: Form<PageForm>,
) -> AutumnResult<impl IntoResponse> {
    let submitted = form.0;
    let effective_status = if submitted.status.trim().is_empty() {
        "draft"
    } else {
        submitted.status.as_str()
    };
    let errors = submitted.validate_fields(effective_status);
    if !errors.is_empty() {
        return Ok((
            StatusCode::UNPROCESSABLE_ENTITY,
            layout("New Page", new_page_form(&submitted, &errors)),
        )
            .into_response());
    }

    let new_page = submitted.into_new();
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

    Ok(Redirect::to(&paths::show(page.slug)).into_response())
}

/// Render the "Edit Page" form, shared by the GET route and the POST route's
/// 422 branch (see [`new_page_form`] for the same pattern on create).
/// `crumb_title` drives the breadcrumb/`<title>` text: the GET route passes
/// the stored page's title, the POST route's failure branch passes back
/// whatever the author just typed. `status` is always the page's *current*
/// stored status — this form never changes it (status flows through the
/// dedicated transition route) — used both for the read-only badge and to
/// decide whether an empty title/body is a publish-guard violation.
fn edit_page_form(
    slug: &str,
    crumb_title: &str,
    status: &str,
    data: &PageForm,
    errors: &[(&str, &str)],
) -> Markup {
    let title_error = field_error(errors, "title");
    let body_error = field_error(errors, "body");

    html! {
        (breadcrumb(&[
            Crumb::link("Wiki", &paths::list()),
            Crumb::link(crumb_title, &paths::show(slug.to_string())),
            Crumb::current("Edit"),
        ]))
        h1 class="text-2xl font-bold mb-6" { "Edit: " (crumb_title) }
        form action=(paths::update(slug.to_string())) method="post"
             class="space-y-4 bg-white rounded shadow p-6" {
            div {
                label for="title" class="block text-sm font-medium" { "Title" }
                input type="text" id="title" name="title" required value=(data.title)
                      aria-invalid=(if title_error.is_some() { "true" } else { "false" })
                      aria-describedby="title-error"
                      class="w-full border rounded p-2 mt-1";
                div id="title-error" {
                    @if let Some(msg) = title_error {
                        p class="text-red-600 text-xs mt-1" role="alert" { (msg) }
                    }
                }
            }
            div {
                label for="body" class="block text-sm font-medium" { "Body" }
                textarea id="body" name="body" rows="10"
                         aria-invalid=(if body_error.is_some() { "true" } else { "false" })
                         aria-describedby="body-error"
                         class="w-full border rounded p-2 mt-1" { (data.body) }
                div id="body-error" {
                    @if let Some(msg) = body_error {
                        p class="text-red-600 text-xs mt-1" role="alert" { (msg) }
                    }
                }
            }
            p class="text-sm text-gray-500" {
                "Status: " (status_badge(status))
                " — change it from the "
                a href=(paths::show(slug.to_string())) class="text-emerald-600 hover:underline" { "page" }
                " using the status controls."
            }
            input type="hidden" name="lock_version" value=(data.lock_version);
            button type="submit"
                   class="bg-emerald-600 text-white px-6 py-2 rounded hover:bg-emerald-700" {
                "Save Changes"
            }
        }
    }
}

#[get("/pages/{slug}/edit")]
pub async fn edit_form(Path(slug): Path<String>, repo: PgPageRepository) -> AutumnResult<Markup> {
    let page = find_page_by_slug(&repo, &slug).await?;
    let data = PageForm {
        title: page.title.clone(),
        body: page.body.clone(),
        status: String::new(),
        lock_version: page.lock_version,
    };

    Ok(layout(
        &format!("Edit: {}", page.title),
        edit_page_form(&page.slug, &page.title, &page.status, &data, &[]),
    ))
}

/// Form struct for page edits. We cannot use `Form<UpdatePage>` directly
/// because `UpdatePage` wraps every field in `Patch<T>` (for partial updates
/// via JSON). HTML forms always submit all fields, so we deserialize into
/// plain values here and convert to the `UpdatePage` type.
#[derive(Default, serde::Deserialize)]
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
    /// A draft page may be saved with an empty title or body —
    /// [`PageHooks::before_create`]/[`PageHooks::before_update`] only enforce
    /// [`Page::can_publish`] once a page is (or becomes) published — so this
    /// mirrors that same rule for the HTML forms. `effective_status` is the
    /// status the saved row would have: the submitted `status` on create, or
    /// the page's current stored status on edit (this form never changes
    /// it). A submission that fails here would otherwise reach the hook's
    /// `bad_request_msg` and bounce to the generic error page instead of
    /// back to the form with the author's draft intact.
    fn validate_fields(&self, effective_status: &str) -> Vec<(&'static str, &'static str)> {
        let mut errors = Vec::new();
        if effective_status == "published" {
            if self.title.trim().is_empty() {
                errors.push(("title", "A published page needs a title"));
            }
            if self.body.trim().is_empty() {
                errors.push(("body", "A published page needs body content"));
            }
        }
        errors
    }

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

/// Update a page from a form submission.
///
/// On validation failure the edit page is re-rendered the same way
/// [`create`] does — see that handler's doc comment; the same anti-pattern
/// applied here via `PageHooks::before_update`'s `?` on the update path too.
/// An already-published page's title/body can't be edited down to empty
/// (`PageHooks::before_update` enforces the same guard the create path
/// does), so `effective_status` is the page's current stored status.
#[post("/pages/{slug}")]
pub async fn update(
    Path(slug): Path<String>,
    repo: PgPageRepository,
    mut db: Db,
    form: Form<PageForm>,
) -> AutumnResult<impl IntoResponse> {
    let page = find_page_by_slug(&repo, &slug).await?;
    let submitted = form.0;
    let errors = submitted.validate_fields(&page.status);
    if !errors.is_empty() {
        return Ok((
            StatusCode::UNPROCESSABLE_ENTITY,
            layout(
                &format!("Edit: {}", submitted.title),
                edit_page_form(
                    &page.slug,
                    &submitted.title,
                    &page.status,
                    &submitted,
                    &errors,
                ),
            ),
        )
            .into_response());
    }

    let update_page = submitted.into_update();
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

    Ok(Redirect::to(&paths::show(updated.slug)).into_response())
}

/// `POST /pages/{slug}/transitions/status` — apply a state-machine transition
/// with its **transition effect** (docs/guide/transition-effects.md).
///
/// The `transition_controls` widget on the show page posts the target state
/// under the field name (`status`). Rather than the pure `transition_status_to`
/// validator plus a separate revision insert, this drives the effectful
/// `transition_status_to_on_conn` inside one transaction: it validates the edge
/// and `can_publish` guard, fires the edge's synchronous `on` effect (which
/// writes the audit `Revision`), and we then persist the returned status on the
/// same connection — so the state change and its audit row commit or roll back
/// atomically. An illegal edge / rejected guard / effect `Err` becomes a 400 (or
/// rolls the transaction back), surfaced by `?` per the wiki's convention.
///
/// `Db::tx_with` hands the effect the `AsyncPgConnection` its `on` methods
/// expect (`Db::tx` yields a different pooled connection type).
#[post("/pages/{slug}/transitions/status")]
pub async fn transition_status(
    Path(slug): Path<String>,
    repo: PgPageRepository,
    mut db: Db,
    form: Form<TransitionForm>,
) -> AutumnResult<Redirect> {
    let page = find_page_by_slug(&repo, &slug).await?;
    let target = form.0.status;
    let page_id = page.id;
    let redirect_slug = page.slug.clone();

    db.tx_with(TxOptions::default(), move |conn| {
        let page = page.clone();
        let target = target.clone();
        async move {
            // Validates the edge + guard and runs the `on` effect (the audit
            // Revision write) on `conn`.
            let new_status = page.transition_status_to_on_conn(conn, &target).await?;
            // Persist the new state on the same connection so it commits with
            // the effect's audit row.
            diesel::update(pages::table.find(page_id))
                .set((
                    pages::status.eq(&new_status),
                    pages::lock_version.eq(pages::lock_version + 1),
                    pages::updated_at.eq(diesel::dsl::now),
                ))
                .execute(conn)
                .await?;
            Ok::<(), AutumnError>(())
        }
        .scope_boxed()
    })
    .await?;

    Ok(Redirect::to(&paths::show(redirect_slug)))
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

/// Error-path coverage for `create`/`update`'s redisplay-on-failure fix
/// (baseline: both handlers sent a publish-guard rejection through
/// `PageHooks::before_create`/`before_update`'s `bad_request_msg` via `?`,
/// landing on the generic error page and losing the author's title/body —
/// 0 of 2 failure modes were adjacent to cause, persisted in place, said how
/// to recover, or preserved the draft).
#[cfg(test)]
mod page_form_tests {
    use super::*;

    fn blank() -> PageForm {
        PageForm::default()
    }

    #[test]
    fn a_draft_may_have_an_empty_title_and_body() {
        let form = blank();
        assert!(form.validate_fields("draft").is_empty());
    }

    #[test]
    fn publishing_with_a_blank_title_is_rejected_with_a_field_message() {
        let form = PageForm {
            title: "   ".into(),
            body: "Some body text".into(),
            ..blank()
        };
        let errors = form.validate_fields("published");
        assert_eq!(
            field_error(&errors, "title"),
            Some("A published page needs a title")
        );
        assert_eq!(field_error(&errors, "body"), None);
    }

    #[test]
    fn publishing_with_a_blank_body_is_rejected_with_a_field_message() {
        let form = PageForm {
            title: "A real title".into(),
            body: "  \n ".into(),
            ..blank()
        };
        let errors = form.validate_fields("published");
        assert_eq!(field_error(&errors, "title"), None);
        assert_eq!(
            field_error(&errors, "body"),
            Some("A published page needs body content")
        );
    }

    #[test]
    fn a_fully_populated_published_page_has_no_errors() {
        let form = PageForm {
            title: "A real title".into(),
            body: "Some body text".into(),
            status: "published".into(),
            ..blank()
        };
        assert!(form.validate_fields("published").is_empty());
    }

    /// The rejected new-page submission keeps the author's draft and wires
    /// each error to its field (adjacent to cause, `aria-invalid`, preserved
    /// entered data) instead of dropping them onto a generic error page.
    #[test]
    fn a_rejected_new_page_submission_keeps_the_authors_input_and_wires_its_error() {
        let submitted = PageForm {
            title: String::new(),
            body: String::new(),
            status: "published".into(),
            lock_version: 0,
        };
        let errors = submitted.validate_fields("published");
        let html = new_page_form(&submitted, &errors).into_string();

        assert!(
            html.contains(r#"option value="published" selected"#),
            "{html}"
        );
        assert!(html.contains(r#"aria-invalid="true""#), "{html}");
        assert!(html.contains("A published page needs a title"), "{html}");
        assert!(
            html.contains("A published page needs body content"),
            "{html}"
        );
    }

    /// Same coverage as above for the edit form's 422 branch: the author's
    /// just-typed title/body survive the round trip, adjacent to the field
    /// that failed.
    #[test]
    fn a_rejected_edit_submission_keeps_the_authors_input_and_wires_its_error() {
        let submitted = PageForm {
            title: "Kept Title".into(),
            body: String::new(),
            status: String::new(),
            lock_version: 3,
        };
        let errors = submitted.validate_fields("published");
        let html =
            edit_page_form("my-page", "Kept Title", "published", &submitted, &errors).into_string();

        assert!(html.contains(r#"value="Kept Title""#), "{html}");
        assert!(html.contains(r#"value="3""#), "{html}");
        assert!(
            html.contains("A published page needs body content"),
            "{html}"
        );
        assert!(field_error(&errors, "title").is_none());
    }
}
