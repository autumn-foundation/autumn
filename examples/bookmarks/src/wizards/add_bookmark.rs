//! `AddBookmark` wizard — demonstrates `autumn generate wizard` output after
//! filling in the generated TODOs.
//!
//! Generated skeleton via:
//!
//! ```bash
//! autumn generate wizard add_bookmark url details
//! ```
//!
//! Then edited to:
//! - Add real validation attributes to each step struct.
//! - Render actual form fields instead of the TODO placeholder comments.
//! - Call the `BookmarkRepository` in the `commit` handler.
//!
//! Mount in `src/main.rs`:
//!
//! ```rust,ignore
//! .routes(routes![
//!     wizards::add_bookmark::show_url,
//!     wizards::add_bookmark::submit_url,
//!     wizards::add_bookmark::show_details,
//!     wizards::add_bookmark::submit_details,
//!     wizards::add_bookmark::show_confirm,
//!     wizards::add_bookmark::commit,
//!     wizards::add_bookmark::cancel,
//! ])
//! ```

use autumn_web::form::ChangesetForm;
use autumn_web::prelude::*;
use autumn_web::wizard::{WizardContext, wizard_progress};
use serde::{Deserialize, Serialize};

use crate::models::bookmark::NewBookmark;
use crate::repositories::bookmark::{BookmarkRepository, PgBookmarkRepository};

// ── Wizard configuration ───────────────────────────────────────────

pub const WIZARD_NAME: &str = "add_bookmark";
pub const STEPS: &[&str] = &["url", "details"];

pub fn wizard_context(session: Session) -> WizardContext {
    WizardContext::new(session, WIZARD_NAME, STEPS.iter().map(|s| s.to_string()))
}

// ── Step structs ───────────────────────────────────────────────────

#[derive(Debug, Default, Clone, Serialize, Deserialize, Validate)]
pub struct UrlStep {
    #[validate(url)]
    pub url: String,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, Validate)]
pub struct DetailsStep {
    #[validate(length(min = 1, max = 200))]
    pub title: String,
    pub tag: String,
}

// ── Shared layout ──────────────────────────────────────────────────

fn layout(title: &str, content: Markup) -> Markup {
    html! {
        (PreEscaped("<!DOCTYPE html>"))
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (title) " - Bookmarks" }
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
                nav class="bg-indigo-600 text-white p-4" {
                    div class="max-w-3xl mx-auto flex justify-between items-center" {
                        a href="/bookmarks" class="text-xl font-bold" { "Bookmarks" }
                        a href="/bookmarks" class="text-sm opacity-75 hover:opacity-100" {
                            "← Back to list"
                        }
                    }
                }
                main id="main-content" class="max-w-3xl mx-auto p-6" { (content) }
            }
        }
    }
}

// ── Route handlers ─────────────────────────────────────────────────

/// Show the `url` step form.
///
/// Draft restore pattern (TurboTax-style persistence across sessions):
///
/// ```rust,ignore
/// // In a real app with authentication, restore a saved draft here:
/// if wizard.first_incomplete_step().await.as_deref() == Some("url") {
///     let key = wizard.draft_key(&current_user.id.to_string());
///     if let Ok(Some(json)) = db.load_wizard_draft(&key).await {
///         let draft: serde_json::Value = serde_json::from_str(&json).unwrap_or_default();
///         wizard.restore_draft(&draft).await;
///     }
/// }
/// ```
#[get("/bookmarks/add/url")]
pub async fn show_url(
    session: Session,
    csrf: Option<CsrfToken>,
    csrf_field: Option<CsrfFormField>,
) -> impl IntoResponse {
    let wizard = wizard_context(session.clone());
    if let Err(redirect_url) = wizard.guard_step("url", "/bookmarks/add").await {
        return Redirect::to(&redirect_url).into_response();
    }
    let data: UrlStep = wizard.step_data("url").await.unwrap_or_default();
    let form = ChangesetForm::blank(data, csrf.as_ref().map_or("", |t| t.token()))
        .with_csrf_field(csrf_field.map_or_else(|| "_csrf".to_string(), |f| f.0));
    layout(
        "Add Bookmark — Step 1",
        html! {
            h1 class="text-2xl font-bold mb-2" { "Add a Bookmark" }
            (wizard_progress(&wizard, "url").await)
            h2 class="text-lg font-semibold mt-6 mb-4" { "Step 1: Enter URL" }
            (form.form_tag("/bookmarks/add/url", "post", html! {
                (form.text_input("url", "URL"))
                (form.submit_button("Continue →"))
            }))
        },
    )
    .into_response()
}

/// Handle `url` step submission.
#[post("/bookmarks/add/url")]
pub async fn submit_url(session: Session, form: ChangesetForm<UrlStep>) -> impl IntoResponse {
    let wizard = wizard_context(session.clone());
    if let Err(redirect_url) = wizard.guard_step("url", "/bookmarks/add").await {
        return Redirect::to(&redirect_url).into_response();
    }
    match form.into_valid() {
        Ok(data) => {
            if wizard.save_step("url", &data).await.is_err() {
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
            Redirect::to("/bookmarks/add/details").into_response()
        }
        Err(form) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            layout(
                "Add Bookmark — Step 1",
                html! {
                    h1 class="text-2xl font-bold mb-2" { "Add a Bookmark" }
                    (wizard_progress(&wizard, "url").await)
                    h2 class="text-lg font-semibold mt-6 mb-4" { "Step 1: Enter URL" }
                    (form.form_tag("/bookmarks/add/url", "post", html! {
                        (form.text_input("url", "URL"))
                        (form.submit_button("Continue →"))
                    }))
                },
            ),
        )
            .into_response(),
    }
}

/// Show the `details` step form.
#[get("/bookmarks/add/details")]
pub async fn show_details(
    session: Session,
    csrf: Option<CsrfToken>,
    csrf_field: Option<CsrfFormField>,
) -> impl IntoResponse {
    let wizard = wizard_context(session.clone());
    if let Err(redirect_url) = wizard.guard_step("details", "/bookmarks/add").await {
        return Redirect::to(&redirect_url).into_response();
    }
    let data: DetailsStep = wizard.step_data("details").await.unwrap_or_default();
    let form = ChangesetForm::blank(data, csrf.as_ref().map_or("", |t| t.token()))
        .with_csrf_field(csrf_field.map_or_else(|| "_csrf".to_string(), |f| f.0));
    layout(
        "Add Bookmark — Step 2",
        html! {
            h1 class="text-2xl font-bold mb-2" { "Add a Bookmark" }
            (wizard_progress(&wizard, "details").await)
            h2 class="text-lg font-semibold mt-6 mb-4" { "Step 2: Add Details" }
            (form.form_tag("/bookmarks/add/details", "post", html! {
                (form.text_input("title", "Title"))
                (form.text_input("tag", "Tag"))
                div class="flex gap-3 mt-4" {
                    a href="/bookmarks/add/url"
                      class="px-4 py-2 border border-gray-300 rounded text-sm" {
                        "← Back"
                    }
                    (form.submit_button("Review →"))
                }
            }))
        },
    )
    .into_response()
}

/// Handle `details` step submission.
#[post("/bookmarks/add/details")]
pub async fn submit_details(
    session: Session,
    form: ChangesetForm<DetailsStep>,
) -> impl IntoResponse {
    let wizard = wizard_context(session.clone());
    if let Err(redirect_url) = wizard.guard_step("details", "/bookmarks/add").await {
        return Redirect::to(&redirect_url).into_response();
    }
    match form.into_valid() {
        Ok(data) => {
            if wizard.save_step("details", &data).await.is_err() {
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
            Redirect::to("/bookmarks/add/confirm").into_response()
        }
        Err(form) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            layout(
                "Add Bookmark — Step 2",
                html! {
                    h1 class="text-2xl font-bold mb-2" { "Add a Bookmark" }
                    (wizard_progress(&wizard, "details").await)
                    h2 class="text-lg font-semibold mt-6 mb-4" { "Step 2: Add Details" }
                    (form.form_tag("/bookmarks/add/details", "post", html! {
                        (form.text_input("title", "Title"))
                        (form.text_input("tag", "Tag"))
                        div class="flex gap-3 mt-4" {
                            a href="/bookmarks/add/url"
                              class="px-4 py-2 border border-gray-300 rounded text-sm" {
                                "← Back"
                            }
                            (form.submit_button("Review →"))
                        }
                    }))
                },
            ),
        )
            .into_response(),
    }
}

/// Show a summary for the user to review before saving.
#[get("/bookmarks/add/confirm")]
pub async fn show_confirm(
    session: Session,
    csrf: Option<CsrfToken>,
    csrf_field: Option<CsrfFormField>,
) -> impl IntoResponse {
    let wizard = wizard_context(session.clone());

    if let Some(incomplete) = wizard.first_incomplete_step().await {
        return Redirect::to(&format!("/bookmarks/add/{incomplete}")).into_response();
    }

    let url_data: Option<UrlStep> = wizard.step_data("url").await;
    let details_data: Option<DetailsStep> = wizard.step_data("details").await;

    let csrf_field_name = csrf_field.as_ref().map_or("_csrf", |f| f.0.as_str());
    let csrf_token_value = csrf.as_ref().map_or("", |t| t.token());

    layout(
        "Add Bookmark — Confirm",
        html! {
            h1 class="text-2xl font-bold mb-2" { "Add a Bookmark" }
            (wizard_progress(&wizard, "confirm").await)
            h2 class="text-lg font-semibold mt-6 mb-4" { "Review your bookmark" }
            div class="bg-white rounded shadow p-6 mb-6 space-y-3" {
                @if let Some(ref u) = url_data {
                    div {
                        span class="text-sm text-gray-500" { "URL" }
                        p class="font-medium" { (u.url) }
                    }
                }
                @if let Some(ref d) = details_data {
                    div {
                        span class="text-sm text-gray-500" { "Title" }
                        p class="font-medium" { (d.title) }
                    }
                    div {
                        span class="text-sm text-gray-500" { "Tag" }
                        p { span class="bg-gray-200 rounded px-2 py-0.5 text-sm" { (d.tag) } }
                    }
                }
            }
            div class="flex gap-3" {
                a href="/bookmarks/add/details"
                  class="px-4 py-2 border border-gray-300 rounded text-sm" {
                    "← Edit"
                }
                form action="/bookmarks/add/commit" method="post" class="inline" {
                    input type="hidden" name=(csrf_field_name) value=(csrf_token_value) {}
                    button type="submit"
                           class="bg-indigo-600 text-white px-4 py-2 rounded hover:bg-indigo-700 text-sm" {
                        "Save Bookmark"
                    }
                }
                form action="/bookmarks/add/cancel" method="post" class="inline" {
                    input type="hidden" name=(csrf_field_name) value=(csrf_token_value) {}
                    button type="submit"
                           class="text-gray-500 hover:text-gray-700 px-4 py-2 text-sm" {
                        "Cancel"
                    }
                }
            }
        },
    )
    .into_response()
}

/// Commit the wizard — assembles step data and saves the bookmark.
#[post("/bookmarks/add/commit")]
pub async fn commit(session: Session, repo: PgBookmarkRepository) -> impl IntoResponse {
    let wizard = wizard_context(session.clone());

    if let Some(incomplete) = wizard.first_incomplete_step().await {
        return Redirect::to(&format!("/bookmarks/add/{incomplete}")).into_response();
    }

    let url_data: Option<UrlStep> = wizard.step_data("url").await;
    let details_data: Option<DetailsStep> = wizard.step_data("details").await;

    match (url_data, details_data) {
        (Some(u), Some(d)) => {
            let new_bookmark = NewBookmark {
                url: u.url,
                title: d.title,
                tag: d.tag,
            };
            if repo.save(&new_bookmark).await.is_err() {
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
            wizard.clear().await;
            Redirect::to("/bookmarks").into_response()
        }
        _ => Redirect::to("/bookmarks/add/url").into_response(),
    }
}

/// Cancel the wizard — clears session state and returns to the list.
#[post("/bookmarks/add/cancel")]
pub async fn cancel(session: Session) -> impl IntoResponse {
    let wizard = wizard_context(session.clone());
    wizard.clear().await;
    Redirect::to("/bookmarks").into_response()
}
