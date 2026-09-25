use autumn_web::extract::Path;
use autumn_web::form::Changeset;
use autumn_web::prelude::*;
use autumn_web::reexports::axum::response::Response;

use crate::models::{Bookmark, NewBookmark};
use crate::repositories::BookmarkRepository;

fn layout(title: &str, content: Markup) -> Markup {
    html! {
        (PreEscaped("<!DOCTYPE html>"))
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (title) " — Bookmarks" }
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
                        a href=(paths::list()) class="text-xl font-bold" { "Bookmarks" }
                        div class="space-x-4 text-sm" {
                            a href="/actuator/health" class="opacity-75 hover:opacity-100" { "Health" }
                            a href="/actuator/info" class="opacity-75 hover:opacity-100" { "Info" }
                        }
                    }
                }
                main id="main-content" class="max-w-3xl mx-auto p-6" { (content) }
            }
        }
    }
}

fn bookmark_card(b: &Bookmark) -> Markup {
    html! {
        li id=(format!("bookmark-{}", b.id))
           class="p-4 bg-white rounded shadow flex justify-between items-center" {
            div {
                a href=(b.url) target="_blank"
                  class="text-indigo-600 font-medium hover:underline" {
                    (b.title)
                }
                span class="ml-2 text-xs bg-gray-200 rounded px-2 py-0.5" { (b.tag) }
                @if !b.alive {
                    span class="ml-2 text-xs bg-red-100 text-red-600 rounded px-2 py-0.5" {
                        "dead link"
                    }
                }
            }
            button
                hx-delete=(crate::repositories::__autumn_path_bookmark_api_delete(b.id))
                hx-target=(format!("#bookmark-{}", b.id))
                hx-swap="delete"
                hx-confirm="Delete this bookmark?"
                class="text-red-500 text-sm hover:text-red-700" {
                "Delete"
            }
        }
    }
}

#[get("/")]
pub async fn list() -> AutumnResult<Markup> {
    let repo = BookmarkRepository;
    let all = repo.find_all().await?;
    Ok(layout(
        "All",
        html! {
            div class="flex justify-between items-center mb-6" {
                h1 class="text-2xl font-bold" { "All Bookmarks" }
                a href=(paths::new_form())
                  class="bg-indigo-600 text-white px-4 py-2 rounded hover:bg-indigo-700" {
                    "+ Add"
                }
            }
            ul class="space-y-3" {
                @for b in &all {
                    (bookmark_card(b))
                }
                @if all.is_empty() {
                    li class="text-gray-400 text-center py-8" { "No bookmarks yet." }
                }
            }
        },
    ))
}

#[get("/tag/{tag}")]
pub async fn by_tag(Path(tag): Path<String>) -> AutumnResult<Markup> {
    let repo = BookmarkRepository;
    let tagged = repo.find_by_tag(tag.clone()).await?;
    Ok(layout(
        &format!("#{tag}"),
        html! {
            h1 class="text-2xl font-bold mb-6" { "Tag: " (tag) }
            ul class="space-y-3" {
                @for b in &tagged { (bookmark_card(b)) }
            }
        },
    ))
}

/// Like `autumn_web::form::text_input`, but also sets the native `type` and
/// `required` attributes on the rendered `<input>`. `text_input` itself only
/// ever renders `type="text"` with no `required` (see `autumn/src/form.rs`),
/// so a bare swap to it — as an earlier draft of this fix did — silently
/// drops the browser-native required-field check, the URL keyboard/format
/// hint, and the `aria-required` signal that the original hand-rolled `<input
/// type="url" required>` markup gave assistive tech (caught in review on
/// #2954). Otherwise identical to `text_input`: same wrapper id, same
/// per-field error rendering sourced from the changeset.
fn required_typed_input(
    changeset: &Changeset<NewBookmark>,
    field: &str,
    label: &str,
    input_type: &str,
) -> Markup {
    let errors = changeset.errors_for(field);
    let has_errors = !errors.is_empty();
    let value = changeset.field_value(field).unwrap_or_default();
    let error_id = has_errors.then(|| format!("{field}-error"));
    let wrapper_id = format!("{field}-field");

    html! {
        div id=(wrapper_id) class="autumn-field" {
            label for=(field) class="autumn-field__label" { (label) }
            input
                type=(input_type)
                id=(field)
                name=(field)
                value=(value)
                required
                class=(if has_errors { "autumn-field__input autumn-field__input--invalid" } else { "autumn-field__input" })
                aria-invalid=(if has_errors { "true" } else { "false" })
                aria-describedby=(error_id.as_deref().unwrap_or(""));
            @if has_errors {
                div id=(error_id.as_deref().unwrap_or_default()) role="alert" class="autumn-field__errors" {
                    @for error in errors {
                        p class="autumn-field__error" { (error) }
                    }
                }
            }
        }
    }
}

/// Shared new-bookmark form body: rendered both by the plain `GET /new` and
/// by `create`'s `422` re-render, from a `Changeset<NewBookmark>` — so a
/// rejected submission (invalid `url`, blank/overlong `title`) redisplays the
/// exact same form with every field preserved and an inline error next to
/// the offending input, instead of silently persisting bad data or
/// redirecting away with the user's input dropped.
fn new_bookmark_form(changeset: &Changeset<NewBookmark>) -> Markup {
    layout(
        "Add Bookmark",
        html! {
            h1 class="text-2xl font-bold mb-6" { "Add Bookmark" }
            form action=(paths::create()) method="post" class="space-y-4" {
                (required_typed_input(changeset, "url", "URL", "url"))
                (required_typed_input(changeset, "title", "Title", "text"))
                (autumn_web::form::text_input(changeset, "tag", "Tag"))
                button type="submit"
                       class="bg-indigo-600 text-white px-6 py-2 rounded hover:bg-indigo-700" {
                    "Save"
                }
            }
        },
    )
}

#[get("/new")]
pub async fn new_form() -> Markup {
    let blank = NewBookmark {
        url: String::new(),
        title: String::new(),
        tag: "general".to_owned(),
    };
    new_bookmark_form(&Changeset::new(blank))
}

#[post("/bookmarks")]
pub async fn create(
    State(state): State<AppState>,
    form: Form<NewBookmark>,
) -> AutumnResult<Response> {
    let changeset = form.0.into_changeset();
    if !changeset.is_valid() {
        return Ok((
            StatusCode::UNPROCESSABLE_ENTITY,
            new_bookmark_form(&changeset),
        )
            .into_response());
    }
    let repo = BookmarkRepository;
    repo.save(&changeset.into_inner()).await?;
    // Cluster-wide, coordination-service-free: this replica adds to its own
    // entry and the other replica sees the new total within a push interval.
    // See `src/routes/cluster.rs`.
    crate::routes::cluster::record_bookmark_created(&state);
    Ok(Redirect::to(&paths::list()).into_response())
}

autumn_web::paths![list, by_tag, new_form, create];

#[cfg(test)]
mod tests {
    use super::bookmark_card;
    use crate::models::Bookmark;
    use chrono::{DateTime, Utc};

    #[test]
    fn bookmark_delete_flow_uses_delete_swap_contract() {
        let bookmark = Bookmark {
            id: 42,
            url: "https://example.com".to_owned(),
            title: "Example".to_owned(),
            tag: "general".to_owned(),
            alive: true,
            created_at: DateTime::<Utc>::from_timestamp(0, 0)
                .expect("unix epoch should exist")
                .naive_utc(),
        };
        let markup = bookmark_card(&bookmark).into_string();

        assert!(markup.contains("hx-delete=\"/api/bookmarks/42\""));
        assert!(markup.contains("hx-swap=\"delete\""));
    }
}
