//! HTML routes for the todo application.
//!
//! These routes render Maud templates styled with Tailwind CSS and
//! use htmx attributes for interactive toggle/delete behaviour.
//!
//! # Form validation patterns
//!
//! This module demonstrates two Autumn form validation patterns:
//!
//! ## Pattern A — Custom form struct (`TodoForm`)
//!
//! Use a dedicated form struct when form validation rules differ from the
//! model, the form has extra fields (e.g. confirm password), or the form
//! needs UI-specific derives (e.g. `Clone` for re-rendering).
//!
//! ```rust,ignore
//! #[post("/todos")]
//! async fn create(db: Db, form: ChangesetForm<TodoForm>) -> impl IntoResponse {
//!     match form.into_valid() {
//!         Ok(f) => { /* insert NewTodo { title: f.title } */ }
//!         Err(form) => (StatusCode::UNPROCESSABLE_ENTITY, render_form(&form)).into_response(),
//!     }
//! }
//! ```
//!
//! ## Pattern B — `NewModel` direct (`NewTodo`)
//!
//! When the model struct already has `#[derive(Validate)]` and the form shape
//! matches the model shape exactly, use `ChangesetForm<NewTodo>` directly —
//! no separate form struct needed.
//!
//! ```rust,ignore
//! #[post("/todos/simple")]
//! async fn create_simple(db: Db, form: ChangesetForm<NewTodo>) -> impl IntoResponse {
//!     match form.into_valid() {
//!         Ok(new_todo) => { /* insert new_todo directly */ }
//!         Err(form) => (StatusCode::UNPROCESSABLE_ENTITY, render_form(&form)).into_response(),
//!     }
//! }
//! ```
//!
//! ## Inline field validation (htmx)
//!
//! A field rendered with [`text_input_htmx`] POSTs to a validation endpoint
//! when its value changes. The handler extracts [`ChangesetForm`], validates,
//! and returns just that field's wrapper partial — htmx swaps it with `outerHTML`.
//! When JavaScript is disabled, the normal `#[post("/todos")]` handler still
//! validates the whole form and returns 422 with inline errors.

use autumn_web::etag::fresh_when;
use autumn_web::extract::Path;
use autumn_web::form::{ChangesetForm, method_input};
use autumn_web::pagination::{Page, PageRequest};
use autumn_web::prelude::{HxRequest, IntoResponse, StatusCode, Validate};
use autumn_web::security::CsrfToken;
use autumn_web::{AutumnError, AutumnResult, Db, Markup, Redirect, delete, get, html, post};
use diesel::prelude::*;
use diesel_async::RunQueryDsl;
use serde::{Deserialize, Serialize};

use crate::models::{NewTodo, Todo, title_not_blank};
use crate::schema::todos;

// ── Form types ────────────────────────────────────────────────────

/// Custom form struct for the "create todo" flow.
///
/// Used when the form needs `Clone` for re-rendering or additional
/// validation rules beyond those on [`NewTodo`] itself.
#[derive(Deserialize, Serialize, Validate, Clone)]
pub struct TodoForm {
    #[validate(
        length(min = 1, max = 255, message = "Title must be 1–255 characters"),
        custom(function = "title_not_blank")
    )]
    title: String,
}

// ── Helpers ──────────────────────────────────────────────────────

/// Base HTML layout wrapping page content.
fn layout(title: &str, content: Markup) -> Markup {
    html! {
        (autumn_web::PreEscaped("<!DOCTYPE html>"))
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (title) }
                link rel="stylesheet" href="/static/css/autumn.css";
                script src="/static/js/htmx.min.js" {}
            }
            body class="bg-stone-50 min-h-screen font-sans text-stone-800 antialiased" {
                div class="max-w-xl mx-auto py-12 px-6" {
                    (content)
                }
                footer class="text-center text-xs text-stone-400 py-8" {
                    "Built with "
                    a href="https://github.com/markm/autumn"
                      class="text-amber-600 hover:text-amber-700 hover:underline" {
                        "Autumn"
                    }
                    " \u{2022} Rust + Diesel + Maud + htmx"
                }
            }
        }
    }
}

/// Render a single todo item as a list element with htmx controls.
fn todo_item(todo: &Todo) -> Markup {
    let (check_classes, check_icon) = if todo.completed {
        (
            "w-5 h-5 shrink-0 rounded border-2 border-amber-500 bg-amber-500 \
             text-white flex items-center justify-center \
             cursor-pointer hover:bg-amber-600 hover:border-amber-600 transition-colors",
            "\u{2713}",
        )
    } else {
        (
            "w-5 h-5 shrink-0 rounded border-2 border-stone-300 \
             flex items-center justify-center \
             cursor-pointer hover:border-amber-400 transition-colors",
            "",
        )
    };

    let title_classes = if todo.completed {
        "flex-1 line-through text-stone-400 hover:text-stone-500 transition-colors"
    } else {
        "flex-1 text-stone-700 hover:text-amber-700 transition-colors"
    };

    html! {
        li id=(format!("todo-{}", todo.id))
           class="group flex items-center gap-3 px-4 py-3 bg-white rounded-lg \
                  border border-stone-200 hover:border-stone-300 \
                  shadow-sm transition-colors" {
            button hx-post=(paths::toggle(todo.id))
                   hx-target=(format!("#todo-{}", todo.id))
                   hx-swap="outerHTML"
                   class=(check_classes) {
                @if todo.completed {
                    span class="text-xs font-bold" { (check_icon) }
                }
            }
            a href=(paths::detail(todo.id))
               class=(title_classes) {
                (todo.title)
            }
            button hx-delete=(paths::delete_todo(todo.id))
                   hx-target=(format!("#todo-{}", todo.id))
                   hx-swap="outerHTML"
                   hx-confirm="Delete this todo?"
                   class="opacity-0 group-hover:opacity-100 text-stone-400 \
                          hover:text-red-500 transition-all cursor-pointer p-1" {
                // Trash icon (SVG)
                (autumn_web::PreEscaped(r#"<svg xmlns="http://www.w3.org/2000/svg" class="w-4 h-4" viewBox="0 0 20 20" fill="currentColor"><path fill-rule="evenodd" d="M9 2a1 1 0 00-.894.553L7.382 4H4a1 1 0 000 2v10a2 2 0 002 2h8a2 2 0 002-2V6a1 1 0 100-2h-3.382l-.724-1.447A1 1 0 0011 2H9zM7 8a1 1 0 012 0v6a1 1 0 11-2 0V8zm5-1a1 1 0 00-1 1v6a1 1 0 102 0V8a1 1 0 00-1-1z" clip-rule="evenodd" /></svg>"#))
            }
        }
    }
}

/// Render the title input field wrapper with htmx inline-validation attributes.
///
/// Returns a `<div id="title-field">` partial so that:
/// - The initial form render includes the field with htmx wired up.
/// - The `POST /todos/validate/title` handler returns this same partial,
///   which htmx swaps in place with `outerHTML`.
/// - No-JavaScript fallback: when htmx is absent the full form POST to
///   `POST /todos` re-renders this partial inside the full form response.
fn title_field_partial(form: &ChangesetForm<TodoForm>) -> Markup {
    let errors = form.errors_for("title");
    let value = form.field_value("title").unwrap_or_default();
    html! {
        div id="title-field" data-autumn-field-wrapper="title" class="flex-1 flex flex-col gap-1" {
            input type="text" name="title"
                  value=(value)
                  placeholder="What needs to be done?"
                  autocomplete="off"
                  aria-invalid=(if errors.is_empty() { "false" } else { "true" })
                  hx-post=(paths::validate_title())
                  hx-trigger="change"
                  hx-target="closest [data-autumn-field-wrapper]"
                  hx-swap="outerHTML"
                  hx-include="closest form"
                  class="w-full px-4 py-2.5 bg-white border border-stone-300 rounded-lg \
                         text-sm placeholder-stone-400 \
                         focus:outline-none focus:ring-2 focus:ring-amber-400/50 \
                         focus:border-amber-400 transition-colors";
            @for msg in errors {
                p class="text-red-600 text-xs px-1" role="alert" { (msg) }
            }
        }
    }
}

/// Render the new-todo form, re-populating the title and showing errors on failure.
fn new_todo_form(pending: &ChangesetForm<TodoForm>) -> Markup {
    let form = pending.form_tag(
        &paths::create(),
        "post",
        html! {
            div class="flex gap-2 items-start" {
                (title_field_partial(pending))
                button type="submit"
                       class="shrink-0 px-5 py-2.5 bg-amber-600 text-white text-sm font-medium rounded-lg \
                              shadow-sm hover:bg-amber-700 active:bg-amber-800 \
                              transition-colors" {
                    "Add"
                }
            }
        },
    );
    html! { div class="flex flex-col gap-2 mb-8" { (form) } }
}

/// Render Previous / page-indicator / Next pagination controls.
fn pagination_controls(page: &Page<Todo>, base_url: &str) -> Markup {
    if page.total_pages <= 1 {
        return html! {};
    }
    html! {
        nav class="flex items-center justify-between mt-6 text-sm" aria-label="Pagination" {
            @if page.has_previous {
                a href=(format!("{}?page={}&size={}", base_url, page.page - 1, page.size))
                   hx-get=(format!("{}?page={}&size={}", base_url, page.page - 1, page.size))
                   hx-target="body"
                   class="px-3 py-1.5 bg-white border border-stone-300 rounded-lg \
                          text-stone-600 hover:border-amber-400 hover:text-amber-700 transition-colors" {
                    "← Previous"
                }
            } @else {
                span class="px-3 py-1.5 text-stone-300 select-none" { "← Previous" }
            }
            span class="text-xs text-stone-400" {
                "Page " (page.page) " of " (page.total_pages)
                " \u{2022} " (page.total_elements) " total"
            }
            @if page.has_next {
                a href=(format!("{}?page={}&size={}", base_url, page.page + 1, page.size))
                   hx-get=(format!("{}?page={}&size={}", base_url, page.page + 1, page.size))
                   hx-target="body"
                   class="px-3 py-1.5 bg-white border border-stone-300 rounded-lg \
                          text-stone-600 hover:border-amber-400 hover:text-amber-700 transition-colors" {
                    "Next →"
                }
            } @else {
                span class="px-3 py-1.5 text-stone-300 select-none" { "Next →" }
            }
        }
    }
}

fn todo_count_badge(total: i64, done: i64) -> Markup {
    html! {
        div id="todo-count" class="flex items-center justify-between mb-3 px-1" {
            @if total > 0 {
                p class="text-xs text-stone-400" {
                    (total) " item"
                    @if total != 1 { "s" }
                    @if done > 0 {
                        " \u{2022} " (done) " done this page"
                    }
                }
            }
            button
                hx-post=(paths::start_export())
                hx-target="#export-status"
                hx-swap="outerHTML"
                class="text-xs text-amber-700 hover:text-amber-900 underline" {
                "Export CSV"
            }
        }
        div id="export-status" {}
    }
}

/// Render the full list page given a paginated todo result and an optional pending form.
async fn list_page(
    mut db: Db,
    page_req: &PageRequest,
    pending: &ChangesetForm<TodoForm>,
) -> AutumnResult<Markup> {
    let page_data = Todo::page(page_req, &mut db).await?;
    let done_count = page_data.content.iter().filter(|t| t.completed).count() as i64;

    Ok(layout(
        "Autumn Todo App",
        html! {
            header class="mb-10" {
                h1 class="text-2xl font-semibold tracking-tight text-stone-900" {
                    "\u{1F342} Autumn Todos"
                }
                p class="text-stone-500 text-sm mt-1" {
                    "A full-stack example with htmx + Tailwind"
                }
            }

            (new_todo_form(pending))

            (todo_count_badge(page_data.total_elements as i64, done_count))

            @if page_data.total_elements == 0 {
                div class="text-center py-16" {
                    p class="text-stone-400 text-sm" {
                        "No todos yet. Add one above!"
                    }
                }
            } @else {
                ul id="todo-list" class="space-y-2" {
                    @for todo in &page_data.content {
                        (todo_item(todo))
                    }
                }
                (pagination_controls(&page_data, &paths::list()))
            }
        },
    ))
}

// ── Route handlers ───────────────────────────────────────────────

/// Redirect the root path to `/todos`.
#[get("/")]
pub async fn index() -> Redirect {
    Redirect::to(&paths::list())
}

/// List todos (paginated).
///
/// Accepts `?page=N&size=M` query parameters via the [`PageRequest`] extractor.
/// Defaults to 20 items per page ordered newest-first.
#[get("/todos")]
pub async fn list(page: PageRequest, db: Db) -> AutumnResult<Markup> {
    let blank = ChangesetForm::without_csrf(TodoForm {
        title: String::new(),
    });
    list_page(db, &page, &blank).await
}

/// Render the detail page body for a todo.
///
/// Extracted so the rendering — including the no-JavaScript delete form —
/// can be unit-tested without spinning up a database. The `csrf_token`
/// parameter mirrors what `#[get("/todos/{id}")]` receives in real
/// requests: `None` when CSRF protection is disabled (e.g. dev profile).
fn detail_view(todo: &Todo, csrf_token: Option<&str>) -> Markup {
    layout(
        &format!("Todo: {}", todo.title),
        html! {
            a href=(paths::list())
               class="inline-flex items-center gap-1 text-sm text-stone-500 \
                      hover:text-amber-600 transition-colors mb-6" {
                "\u{2190} Back to list"
            }

            div class="bg-white rounded-lg border border-stone-200 shadow-sm p-6" {
                h2 class="text-lg font-semibold text-stone-900 mb-3" { (todo.title) }
                div class="flex items-center gap-4 text-sm" {
                    @if todo.completed {
                        span class="inline-flex items-center gap-1.5 px-2.5 py-1 \
                                    bg-green-50 text-green-700 rounded-full text-xs font-medium" {
                            "\u{2713} Completed"
                        }
                    } @else {
                        span class="inline-flex items-center gap-1.5 px-2.5 py-1 \
                                    bg-amber-50 text-amber-700 rounded-full text-xs font-medium" {
                            "\u{25CB} Pending"
                        }
                    }
                    span class="text-stone-400 text-xs" {
                        "Created " (todo.created_at.format("%b %d, %Y at %H:%M"))
                    }
                }

                form method="post"
                     action=(paths::delete_todo(todo.id))
                     class="mt-6 flex items-center gap-3 border-t border-stone-100 pt-4" {
                    (method_input("DELETE"))
                    @if let Some(token) = csrf_token {
                        input type="hidden" name="_csrf" value=(token);
                    }
                    button type="submit"
                           class="text-sm text-red-600 hover:text-red-700 \
                                  underline-offset-2 hover:underline cursor-pointer" {
                        "Delete this todo"
                    }
                    span class="text-xs text-stone-400" {
                        "Works without JavaScript"
                    }
                }
            }
        },
    )
}

/// Show a single todo by ID.
///
/// Uses [`fresh_when`] for conditional-GET support: if the client already
/// holds a matching ETag the handler returns `304 Not Modified` with an empty
/// body, saving a full render + transport round-trip on every repeat visit or
/// htmx poll.
///
/// The ETag is derived from the todo's `created_at` timestamp and its
/// `completed` flag, so it changes whenever the todo is toggled.
///
/// The page also renders a plain `<form method="post">` carrying a hidden
/// `_method=DELETE` field as a no-JavaScript fallback alongside the
/// htmx-driven delete button on the list view. Both paths hit the same
/// `#[delete("/todos/{id}")]` handler — Autumn's method-override middleware
/// rewrites the transport `POST` to `DELETE` before route matching.
#[get("/todos/{id}")]
pub async fn detail(
    id: Path<i64>,
    headers: http::HeaderMap,
    mut db: Db,
    csrf: Option<CsrfToken>,
) -> AutumnResult<impl IntoResponse> {
    let todo = Todo::find(*id, &mut db).await?;

    // ETag: hash of (created_at_unix, completed flag, csrf_token).
    // Including the CSRF token ensures that CSRF rotation invalidates the
    // cached response, so clients never replay a stale hidden token.
    let csrf_tok = csrf.as_ref().map(CsrfToken::token).unwrap_or_default();
    let etag_input = format!(
        "{}-{}-{}",
        todo.created_at.and_utc().timestamp(),
        todo.completed,
        csrf_tok
    );
    let fw = fresh_when(&headers, etag_input.as_str());

    Ok(fw.or(detail_view(&todo, csrf.as_ref().map(CsrfToken::token))))
}

/// Inline field validation for the todo title (htmx endpoint).
///
/// Called by htmx when the title input changes. Extracts and validates the
/// submitted form, then returns just the `<div id="title-field">` partial so
/// htmx can swap it with `outerHTML`.
///
/// No-JavaScript fallback: when htmx is absent this endpoint is never called;
/// the full `#[post("/todos")]` handler validates the whole form instead.
#[post("/todos/validate/title")]
pub async fn validate_title(form: ChangesetForm<TodoForm>) -> Markup {
    title_field_partial(&form)
}

/// Create a new todo from a form submission.
///
/// On validation failure the list page is re-rendered with inline errors (422).
/// On success a new row is inserted and the browser redirects to the list.
#[post("/todos")]
pub async fn create(db: Db, form: ChangesetForm<TodoForm>) -> AutumnResult<impl IntoResponse> {
    match form.into_valid() {
        Ok(f) => {
            let mut db = db;
            diesel::insert_into(todos::table)
                .values(NewTodo {
                    title: f.title.trim().to_owned(),
                })
                .execute(&mut *db)
                .await?;
            Ok(Redirect::to(&paths::list()).into_response())
        }
        Err(form) => {
            let page_req = PageRequest::default();
            let markup = list_page(db, &page_req, &form).await?;
            Ok((StatusCode::UNPROCESSABLE_ENTITY, markup).into_response())
        }
    }
}

/// Kick off an async CSV export (issue #1373) and hand back an htmx fragment
/// that polls the built-in tracked-job status route for progress and the
/// eventual download link. The request itself returns immediately — the
/// export runs as a background job instead of blocking this thread.
#[post("/todos/export")]
pub async fn start_export() -> AutumnResult<Markup> {
    let handle =
        crate::jobs::ExportTodosCsvJob::enqueue_tracked(crate::jobs::ExportTodosArgs {}).await?;
    Ok(export_status_fragment(&handle.status_path()))
}

/// The self-polling fragment embedded by [`start_export`] and re-rendered by
/// the built-in `GET /_autumn/jobs/{token}` route on every 2s poll.
fn export_status_fragment(poll_path: &str) -> Markup {
    html! {
        div id="export-status"
            hx-get=(poll_path)
            hx-trigger="load"
            hx-swap="outerHTML"
            class="text-xs text-stone-400 mt-1" {
            "Preparing export…"
        }
    }
}

fn page_request_from_hx(hx: &HxRequest) -> PageRequest {
    if let Some(parsed_url) = hx
        .current_url
        .as_deref()
        .and_then(|url| url::Url::parse(url).ok())
    {
        let mut page = None;
        let mut size = None;
        for (key, val) in parsed_url.query_pairs() {
            if key == "page" {
                page = val.parse::<u32>().ok();
            } else if key == "size" {
                size = val.parse::<u32>().ok();
            }
        }
        if page.is_some() || size.is_some() {
            return PageRequest::new(page.unwrap_or(1), size.unwrap_or(20));
        }
    }
    PageRequest::default()
}

/// Toggle the completion status of a todo (htmx endpoint).
///
/// Uses a single `UPDATE ... SET completed = NOT completed RETURNING *`
/// query — one round-trip instead of three.
#[post("/todos/{id}/toggle")]
pub async fn toggle(id: Path<i64>, mut db: Db, hx: HxRequest) -> AutumnResult<impl IntoResponse> {
    let updated: Todo = diesel::update(todos::table.find(*id))
        .set(todos::completed.eq(diesel::dsl::not(todos::completed)))
        .returning(Todo::as_returning())
        .get_result(&mut *db)
        .await
        .map_err(AutumnError::not_found)?;

    if hx.is_htmx {
        let page_req = page_request_from_hx(&hx);
        let page_data = Todo::page(&page_req, &mut db).await?;
        let done_count = page_data.content.iter().filter(|t| t.completed).count() as i64;
        let count_markup = todo_count_badge(page_data.total_elements as i64, done_count);

        Ok(autumn_web::htmx::HtmxFragments::new(todo_item(&updated))
            .oob("todo-count", count_markup)
            .into_response())
    } else {
        Ok(todo_item(&updated).into_response())
    }
}

/// Delete a todo by ID.
///
/// Serves two callers off the same `#[delete]` route:
/// - htmx button on the list view: returns an empty body so htmx removes
///   the element from the DOM.
/// - Plain HTML form (no JavaScript): a `<form method="post">` carrying
///   `_method=DELETE` is rewritten to `DELETE` by Autumn's method-override
///   middleware before route matching. The handler redirects back to the
///   list, which is the no-JS-friendly response.
#[delete("/todos/{id}")]
pub async fn delete_todo(
    id: Path<i64>,
    mut db: Db,
    hx: HxRequest,
) -> AutumnResult<impl IntoResponse> {
    let deleted = diesel::delete(todos::table.find(*id))
        .execute(&mut *db)
        .await?;

    if deleted != 1 {
        return Err(AutumnError::not_found_msg(format!(
            "Todo with id {} not found",
            *id
        )));
    }

    if hx.is_htmx {
        let page_req = page_request_from_hx(&hx);
        let page_data = Todo::page(&page_req, &mut db).await?;
        let done_count = page_data.content.iter().filter(|t| t.completed).count() as i64;
        let count_markup = todo_count_badge(page_data.total_elements as i64, done_count);

        Ok(autumn_web::htmx::HtmxFragments::oob_only()
            .oob("todo-count", count_markup)
            .into_response())
    } else {
        Ok(Redirect::to(&paths::list()).into_response())
    }
}

autumn_web::paths![
    index,
    list,
    detail,
    create,
    validate_title,
    toggle,
    delete_todo,
    start_export
];

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use super::*;
    use autumn_web::form::IntoChangeset;

    #[test]
    fn test_path_helpers_correct() {
        assert_eq!(paths::list(), "/todos");
        assert_eq!(paths::detail(42), "/todos/42");
        assert_eq!(paths::toggle(7), "/todos/7/toggle");
        assert_eq!(paths::delete_todo(3), "/todos/3");
        assert_eq!(paths::create(), "/todos");
        assert_eq!(paths::start_export(), "/todos/export");
    }

    #[test]
    fn export_status_fragment_polls_the_tracked_job_status_route() {
        let html = export_status_fragment("/_autumn/jobs/abc123").into_string();
        assert!(html.contains(r#"hx-get="/_autumn/jobs/abc123""#), "{html}");
        assert!(html.contains(r#"hx-trigger="load""#), "{html}");
        assert!(html.contains(r#"hx-swap="outerHTML""#), "{html}");
    }

    #[test]
    fn test_layout_generates_html() {
        let markup = layout("Test Title", html! { p { "Test Content" } });
        let html = markup.into_string();
        assert!(html.contains("Test Title"));
        assert!(html.contains("Test Content"));
        assert!(html.contains("htmx.min.js"));
    }

    #[test]
    fn test_todo_item_completed() {
        let todo = Todo {
            id: 1,
            title: "Test Todo".into(),
            completed: true,
            created_at: chrono::DateTime::from_timestamp(0, 0).unwrap().naive_utc(),
        };
        let markup = todo_item(&todo);
        let html = markup.into_string();
        assert!(html.contains("Test Todo"));
        assert!(html.contains("line-through"));
    }

    #[test]
    fn test_todo_item_pending() {
        let todo = Todo {
            id: 1,
            title: "Test Todo".into(),
            completed: false,
            created_at: chrono::DateTime::from_timestamp(0, 0).unwrap().naive_utc(),
        };
        let markup = todo_item(&todo);
        let html = markup.into_string();
        assert!(html.contains("Test Todo"));
        assert!(!html.contains("line-through"));
    }

    #[test]
    fn test_todo_item_uses_path_helpers() {
        let todo = Todo {
            id: 42,
            title: "Check paths".into(),
            completed: false,
            created_at: chrono::DateTime::from_timestamp(0, 0).unwrap().naive_utc(),
        };
        let markup = todo_item(&todo);
        let html = markup.into_string();
        assert!(html.contains("/todos/42/toggle"));
        assert!(html.contains("href=\"/todos/42\""));
        assert!(html.contains("hx-delete=\"/todos/42\""));
    }

    #[test]
    fn new_todo_form_shows_validation_errors() {
        let cs = TodoForm {
            title: String::new(),
        }
        .into_changeset();
        let form = ChangesetForm::from_changeset(cs);
        let html = new_todo_form(&form).into_string();
        assert!(html.contains("Title must be 1"));
        assert!(html.contains(r#"aria-invalid="true""#));
    }

    #[test]
    fn new_todo_form_clean_when_valid() {
        let form = ChangesetForm::without_csrf(TodoForm {
            title: "Buy milk".into(),
        });
        let html = new_todo_form(&form).into_string();
        assert!(html.contains(r#"aria-invalid="false""#));
        assert!(html.contains(r#"value="Buy milk""#));
    }

    /// Detail-page renders a plain `<form method="post">` that targets
    /// the declared `#[delete]` route via the `_method` override field.
    /// This is the no-JavaScript delete path alongside the htmx-driven
    /// delete on the list view.
    #[test]
    fn detail_view_renders_no_js_delete_form() {
        let todo = Todo {
            id: 42,
            title: "Write docs".into(),
            completed: false,
            created_at: chrono::DateTime::from_timestamp(0, 0).unwrap().naive_utc(),
        };
        let html = detail_view(&todo, Some("csrf-tok-xyz")).into_string();
        assert!(html.contains(r#"method="post""#), "{html}");
        assert!(html.contains(r#"action="/todos/42""#), "{html}");
        assert!(html.contains(r#"name="_method""#), "{html}");
        assert!(html.contains(r#"value="DELETE""#), "{html}");
        assert!(html.contains(r#"value="csrf-tok-xyz""#), "{html}");
        assert!(html.contains("Delete this todo"), "{html}");
    }

    #[test]
    fn detail_view_omits_csrf_input_when_token_absent() {
        let todo = Todo {
            id: 7,
            title: "No csrf".into(),
            completed: true,
            created_at: chrono::DateTime::from_timestamp(0, 0).unwrap().naive_utc(),
        };
        let html = detail_view(&todo, None).into_string();
        assert!(html.contains(r#"name="_method""#), "{html}");
        assert!(!html.contains(r#"name="_csrf""#), "{html}");
    }

    #[test]
    fn pagination_controls_shows_next_on_first_page() {
        use autumn_web::pagination::Page;
        let req = PageRequest::new(1, 5);
        let items: Vec<Todo> = (1..=5)
            .map(|i| Todo {
                id: i,
                title: format!("Todo {i}"),
                completed: false,
                created_at: chrono::DateTime::from_timestamp(0, 0).unwrap().naive_utc(),
            })
            .collect();
        let page: Page<Todo> = Page::new(items, 12, &req);
        let html = pagination_controls(&page, "/todos").into_string();
        // Next must be a link on the first page
        assert!(html.contains("Next"), "first page must show Next: {html}");
        assert!(
            html.contains("href=\"/todos?page=2"),
            "first page must link to page 2: {html}"
        );
        // Previous must NOT be a link on the first page (rendered as a disabled span)
        assert!(
            !html.contains("href=\"/todos?page=0"),
            "first page must not link to page 0: {html}"
        );
        assert!(
            html.contains("Page 1 of 3"),
            "must show current page of total: {html}"
        );
    }

    #[test]
    fn pagination_controls_hidden_for_single_page() {
        use autumn_web::pagination::Page;
        let req = PageRequest::new(1, 20);
        let items: Vec<Todo> = (1..=5)
            .map(|i| Todo {
                id: i,
                title: format!("Todo {i}"),
                completed: false,
                created_at: chrono::DateTime::from_timestamp(0, 0).unwrap().naive_utc(),
            })
            .collect();
        let page: Page<Todo> = Page::new(items, 5, &req);
        let html = pagination_controls(&page, "/todos").into_string();
        assert!(
            html.is_empty(),
            "single page must render no pagination controls: {html}"
        );
    }
}

/// Tests covering the htmx inline validation pattern (AC10, AC11).
#[cfg(test)]
mod inline_validation_tests {
    use super::*;
    use autumn_web::form::IntoChangeset;

    // ── title_field_partial ──────────────────────────────────────

    #[test]
    fn title_field_partial_has_stable_wrapper_id() {
        let form = ChangesetForm::without_csrf(TodoForm {
            title: "Buy milk".into(),
        });
        let html = title_field_partial(&form).into_string();
        assert!(html.contains(r#"id="title-field""#), "{html}");
        assert!(
            html.contains(r#"data-autumn-field-wrapper="title""#),
            "{html}"
        );
    }

    #[test]
    fn title_field_partial_valid_no_errors() {
        let form = ChangesetForm::without_csrf(TodoForm {
            title: "Buy milk".into(),
        });
        let html = title_field_partial(&form).into_string();
        assert!(html.contains(r#"aria-invalid="false""#), "{html}");
        assert!(!html.contains(r#"role="alert""#), "{html}");
        assert!(html.contains(r#"value="Buy milk""#), "{html}");
    }

    #[test]
    fn title_field_partial_invalid_shows_errors_and_preserves_value() {
        let cs = TodoForm {
            title: String::new(),
        }
        .into_changeset();
        let form = ChangesetForm::from_changeset(cs);
        let html = title_field_partial(&form).into_string();
        assert!(html.contains(r#"aria-invalid="true""#), "{html}");
        assert!(html.contains(r#"role="alert""#), "{html}");
        assert!(html.contains("Title must be 1"), "{html}");
        assert!(html.contains(r#"value="""#), "{html}");
    }

    #[test]
    fn title_field_partial_has_htmx_validation_attributes() {
        let form = ChangesetForm::without_csrf(TodoForm {
            title: String::new(),
        });
        let html = title_field_partial(&form).into_string();
        assert!(
            html.contains(&format!(r#"hx-post="{}""#, paths::validate_title())),
            "{html}"
        );
        assert!(html.contains(r#"hx-trigger="change""#), "{html}");
        assert!(
            html.contains(r#"hx-target="closest [data-autumn-field-wrapper]""#),
            "{html}"
        );
        assert!(html.contains(r#"hx-swap="outerHTML""#), "{html}");
        assert!(html.contains(r#"hx-include="closest form""#), "{html}");
    }

    #[test]
    fn title_field_partial_does_not_include_submit_button() {
        let form = ChangesetForm::without_csrf(TodoForm {
            title: String::new(),
        });
        let html = title_field_partial(&form).into_string();
        assert!(!html.contains(r#"type="submit""#), "{html}");
        assert!(!html.contains("Add"), "{html}");
    }

    #[test]
    fn new_todo_form_includes_htmx_validation() {
        let form = ChangesetForm::without_csrf(TodoForm {
            title: String::new(),
        });
        let html = new_todo_form(&form).into_string();
        assert!(
            html.contains(r#"hx-post="/todos/validate/title""#),
            "{html}"
        );
        assert!(html.contains(r#"hx-trigger="change""#), "{html}");
        assert!(html.contains(r#"type="submit""#), "{html}");
        assert!(html.contains("Add"), "{html}");
    }

    // ── No-JavaScript fallback ─────────────────────────────────

    #[test]
    fn new_todo_form_has_standard_form_action_for_no_js_fallback() {
        let form = ChangesetForm::without_csrf(TodoForm {
            title: String::new(),
        });
        let html = new_todo_form(&form).into_string();
        assert!(html.contains(r#"action="/todos""#), "{html}");
        assert!(html.contains(r#"method="post""#), "{html}");
    }

    // ── AC7: NewModel validation reuse ─────────────────────────

    #[test]
    fn new_todo_with_validate_can_use_changeset_form_directly() {
        let cs = NewTodo {
            title: String::new(),
        }
        .into_changeset();
        assert!(!cs.is_valid(), "empty title should be invalid");
        assert!(!cs.errors_for("title").is_empty());
    }

    #[test]
    fn new_todo_valid_title_produces_valid_changeset() {
        let cs = NewTodo {
            title: "Buy milk".into(),
        }
        .into_changeset();
        assert!(cs.is_valid());
        assert!(cs.errors_for("title").is_empty());
    }

    #[test]
    fn new_todo_blank_title_fails_custom_validator() {
        let cs = NewTodo {
            title: "   ".into(),
        }
        .into_changeset();
        assert!(!cs.is_valid());
        let errs = cs.errors_for("title");
        assert!(
            errs.iter().any(|e| e.contains("blank")),
            "expected blank error: {errs:?}"
        );
    }

    #[test]
    fn new_todo_changeset_preserves_submitted_value() {
        let cs = NewTodo { title: "ab".into() }.into_changeset();
        assert_eq!(cs.field_value("title"), Some("ab".to_string()));
    }
}

#[cfg(test)]
mod mutant_tests {
    #[allow(unused_imports)]
    use super::*;

    #[test]
    fn test_delete_todo_behavior() {
        // Can we test the exact endpoint behavior easily without DB integration?
        // No, we need TestDb. Since we can't easily mock it, we'll write a note on why
        // we can't write a direct unit test here and what it might look like.
    }

    #[test]
    fn test_page_request_from_hx_parsing() {
        // Default when current_url is None
        let hx_none = HxRequest {
            current_url: None,
            ..Default::default()
        };
        let req = page_request_from_hx(&hx_none);
        assert_eq!(req.page(), 1);
        assert_eq!(req.size(), 20);

        // Default when current_url is invalid
        let hx_invalid = HxRequest {
            current_url: Some("invalid-url".to_string()),
            ..Default::default()
        };
        let req = page_request_from_hx(&hx_invalid);
        assert_eq!(req.page(), 1);
        assert_eq!(req.size(), 20);

        // Page parsed correctly
        let hx_page = HxRequest {
            current_url: Some("http://localhost:3000/todos?page=2".to_string()),
            ..Default::default()
        };
        let req = page_request_from_hx(&hx_page);
        assert_eq!(req.page(), 2);
        assert_eq!(req.size(), 20);

        // Size parsed correctly
        let hx_size = HxRequest {
            current_url: Some("http://localhost:3000/todos?size=10".to_string()),
            ..Default::default()
        };
        let req = page_request_from_hx(&hx_size);
        assert_eq!(req.page(), 1);
        assert_eq!(req.size(), 10);

        // Page and size parsed correctly
        let hx_both = HxRequest {
            current_url: Some("http://localhost:3000/todos?page=5&size=15".to_string()),
            ..Default::default()
        };
        let req = page_request_from_hx(&hx_both);
        assert_eq!(req.page(), 5);
        assert_eq!(req.size(), 15);
    }
}
