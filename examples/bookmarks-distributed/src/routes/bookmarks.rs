use autumn_web::extract::Path;
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

/// Shared new-bookmark form body — rendered by both the plain `GET /new`
/// and `create`'s `422` re-render, from a `Changeset<NewBookmark>`, so a
/// rejected submission shows the same form with every field preserved and
/// an inline error next to the offending input. `NewBookmark` already
/// derives `validator::Validate` from the `#[validate(url)]` /
/// `#[validate(length(...))]` attributes on `Bookmark` in `models.rs` — the
/// bug was that `create` never called it, so an empty title or a
/// non-URL string in `url` was inserted straight into the table.
fn new_bookmark_form(changeset: &Changeset<NewBookmark>) -> Markup {
    layout(
        "Add Bookmark",
        html! {
            h1 class="text-2xl font-bold mb-6" { "Add Bookmark" }
            form action=(paths::create()) method="post" class="space-y-4" {
                (autumn_web::form::text_input(changeset, "url", "URL"))
                (autumn_web::form::text_input(changeset, "title", "Title"))
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
    new_bookmark_form(&Changeset::new(NewBookmark {
        url: String::new(),
        title: String::new(),
        tag: "general".to_owned(),
    }))
}

#[post("/bookmarks")]
pub async fn create(
    State(state): State<AppState>,
    Form(form): Form<NewBookmark>,
) -> AutumnResult<Response> {
    let changeset = form.into_changeset();
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
    use super::{bookmark_card, new_bookmark_form};
    use crate::models::{Bookmark, NewBookmark};
    use autumn_web::form::{Changeset, IntoChangeset};
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

    // ── create's validation (baseline was: none at all) ────────────────

    #[test]
    fn non_url_string_is_rejected_by_the_url_validator() {
        let cs = NewBookmark {
            url: "not a url".to_owned(),
            title: "Some title".to_owned(),
            tag: "general".to_owned(),
        }
        .into_changeset();
        assert!(!cs.is_valid(), "\"not a url\" must fail #[validate(url)]");
        assert!(!cs.errors_for("url").is_empty());
    }

    #[test]
    fn empty_title_is_rejected_by_the_length_validator() {
        let cs = NewBookmark {
            url: "https://example.com".to_owned(),
            title: String::new(),
            tag: "general".to_owned(),
        }
        .into_changeset();
        assert!(!cs.is_valid(), "empty title must fail length(min = 1)");
        assert!(!cs.errors_for("title").is_empty());
    }

    #[test]
    fn valid_submission_produces_a_valid_changeset() {
        let cs = NewBookmark {
            url: "https://example.com".to_owned(),
            title: "Example".to_owned(),
            tag: "general".to_owned(),
        }
        .into_changeset();
        assert!(cs.is_valid());
        assert!(cs.errors_for("url").is_empty());
        assert!(cs.errors_for("title").is_empty());
    }

    #[test]
    fn rejected_form_preserves_every_submitted_field_and_flags_only_the_bad_one() {
        let cs = NewBookmark {
            url: "not a url".to_owned(),
            title: "Kept title".to_owned(),
            tag: "kept-tag".to_owned(),
        }
        .into_changeset();
        let html = new_bookmark_form(&cs).into_string();

        // The failing field is flagged and preserved.
        assert!(html.contains(r#"aria-invalid="true""#), "{html}");
        assert!(html.contains(r#"role="alert""#), "{html}");
        assert!(html.contains(r#"value="not a url""#), "{html}");
        // The valid fields the user also typed are not dropped on the floor.
        assert!(html.contains(r#"value="Kept title""#), "{html}");
        assert!(html.contains(r#"value="kept-tag""#), "{html}");
    }

    #[test]
    fn clean_form_shows_no_errors() {
        let cs = Changeset::new(NewBookmark {
            url: String::new(),
            title: String::new(),
            tag: "general".to_owned(),
        });
        let html = new_bookmark_form(&cs).into_string();
        assert!(!html.contains(r#"role="alert""#), "{html}");
        assert!(html.contains(r#"aria-invalid="false""#), "{html}");
    }
}
