//! Settings — WordPress's General, Reading, Discussion and Permalinks screens,
//! on one page because there are not enough of them to warrant four.

use autumn_web::AutumnResult;
use autumn_web::prelude::*;
use autumn_web::reexports::axum::response::Response;
use serde::Deserialize;

use crate::capabilities::Capability;
use crate::models::{NewSiteOption, UpdateSiteOption};
use crate::permalinks::PermalinkStructure;
use crate::repositories::{PgSiteOptionRepository, SiteOptionRepository as _};
use crate::require_capability;
use crate::settings::Settings;
use crate::theme;

use super::super::site::{Csrf, Repos};
use super::layout;

#[derive(Deserialize)]
pub struct SettingsForm {
    pub site_title: String,
    #[serde(default)]
    pub tagline: String,
    pub posts_per_page: String,
    pub permalink_structure: String,
    pub default_comment_status: String,
    #[serde(default)]
    pub comment_moderation: Option<String>,
    #[serde(default)]
    pub allow_guest_comments: Option<String>,
    pub active_theme: String,
    pub date_format: String,
    #[serde(default)]
    pub front_page_id: String,
}

#[get("/admin/settings")]
pub async fn show(repos: Repos, session: Session, csrf: Csrf) -> AutumnResult<Response> {
    let user = require_capability!(repos, session, csrf, Capability::ManageOptions);
    let settings = repos.settings().await?;
    let pages = repos.published_posts("page", 200).await?;

    let body = html! {
        form action="/admin/settings" method="post" class="max-w-2xl space-y-6" {
            (csrf.input())
            section class="bg-white rounded-lg shadow p-5 space-y-4" {
                h2 class="font-semibold" { "General" }
                div {
                    label for="site_title" class="block text-sm font-medium mb-1" {
                        "Site title"
                    }
                    input #site_title type="text" name="site_title" required
                          value=(settings.site_title) class="w-full border rounded px-3 py-2";
                }
                div {
                    label for="tagline" class="block text-sm font-medium mb-1" { "Tagline" }
                    input #tagline type="text" name="tagline" value=(settings.tagline)
                          class="w-full border rounded px-3 py-2";
                }
                div {
                    label for="date_format" class="block text-sm font-medium mb-1" {
                        "Date format "
                        span class="text-gray-400 font-normal" { "(strftime)" }
                    }
                    input #date_format type="text" name="date_format" required
                          value=(settings.date_format)
                          class="w-full border rounded px-3 py-2 font-mono text-sm";
                }
            }

            section class="bg-white rounded-lg shadow p-5 space-y-4" {
                h2 class="font-semibold" { "Reading" }
                div {
                    label for="posts_per_page" class="block text-sm font-medium mb-1" {
                        "Posts per page"
                    }
                    input #posts_per_page type="number" name="posts_per_page" min="1" max="100"
                          value=(settings.posts_per_page)
                          class="w-full border rounded px-3 py-2";
                }
                div {
                    label for="front_page_id" class="block text-sm font-medium mb-1" {
                        "Front page"
                    }
                    select #front_page_id name="front_page_id"
                           class="w-full border rounded px-3 py-2" {
                        option value="" selected[settings.front_page_id.is_none()] {
                            "Your latest posts"
                        }
                        @for page in &pages {
                            option value=(page.id)
                                   selected[settings.front_page_id == Some(page.id)] {
                                (page.title)
                            }
                        }
                    }
                }
            }

            section class="bg-white rounded-lg shadow p-5 space-y-4" {
                h2 class="font-semibold" { "Discussion" }
                div {
                    label for="default_comment_status" class="block text-sm font-medium mb-1" {
                        "New posts"
                    }
                    select #default_comment_status name="default_comment_status"
                           class="w-full border rounded px-3 py-2" {
                        option value="open" selected[settings.default_comment_status == "open"] {
                            "Allow comments"
                        }
                        option value="closed"
                               selected[settings.default_comment_status == "closed"] {
                            "Do not allow comments"
                        }
                    }
                }
                label class="flex items-center gap-2 text-sm" {
                    input type="checkbox" name="comment_moderation" value="on"
                          checked[settings.comment_moderation] class="rounded border-gray-300";
                    "Hold guest comments for moderation"
                }
                label class="flex items-center gap-2 text-sm" {
                    input type="checkbox" name="allow_guest_comments" value="on"
                          checked[settings.allow_guest_comments] class="rounded border-gray-300";
                    "Allow comments from visitors without an account"
                }
            }

            section class="bg-white rounded-lg shadow p-5 space-y-4" {
                h2 class="font-semibold" { "Permalinks" }
                p class="text-xs text-gray-500" {
                    "Changing this changes the URLs new links are built from. Existing URLs keep \
                     working: the router resolves every structure's shape, not just the \
                     configured one."
                }
                @for structure in PermalinkStructure::all() {
                    label class="flex items-center gap-2 text-sm" {
                        input type="radio" name="permalink_structure" value=(structure.as_str())
                              checked[settings.permalink_structure == *structure]
                              class="border-gray-300";
                        (structure.label())
                    }
                }
            }

            section class="bg-white rounded-lg shadow p-5 space-y-4" {
                h2 class="font-semibold" { "Theme" }
                div {
                    label for="active_theme" class="block text-sm font-medium mb-1" {
                        "Active theme"
                    }
                    select #active_theme name="active_theme"
                           class="w-full border rounded px-3 py-2" {
                        @for registered in theme::registered_themes() {
                            option value=(registered.slug())
                                   selected[settings.active_theme == registered.slug()] {
                                (registered.name())
                            }
                        }
                    }
                }
            }

            button type="submit"
                   class="px-5 py-2 bg-indigo-600 text-white rounded hover:bg-indigo-700" {
                "Save settings"
            }
        }
    };

    Ok(layout(&user, &csrf, "/admin/settings", "Settings", body).into_response())
}

#[post("/admin/settings")]
pub async fn save(
    repos: Repos,
    session: Session,
    csrf: Csrf,
    Form(form): Form<SettingsForm>,
) -> AutumnResult<Response> {
    let _user = require_capability!(repos, session, csrf, Capability::ManageOptions);

    // Round-trip through `Settings` rather than writing the form fields
    // straight to the options table: `from_rows` is where every value is
    // validated and clamped, so a hand-crafted POST cannot store a
    // `posts_per_page` of 0 that would paginate forever.
    let submitted = Settings::from_rows(&[
        ("site_title".to_owned(), form.site_title.trim().to_owned()),
        ("tagline".to_owned(), form.tagline.trim().to_owned()),
        ("posts_per_page".to_owned(), form.posts_per_page.clone()),
        (
            "permalink_structure".to_owned(),
            form.permalink_structure.clone(),
        ),
        (
            "default_comment_status".to_owned(),
            form.default_comment_status.clone(),
        ),
        (
            "comment_moderation".to_owned(),
            form.comment_moderation.is_some().to_string(),
        ),
        (
            "allow_guest_comments".to_owned(),
            form.allow_guest_comments.is_some().to_string(),
        ),
        ("active_theme".to_owned(), form.active_theme.clone()),
        ("date_format".to_owned(), form.date_format.clone()),
        ("front_page_id".to_owned(), form.front_page_id.clone()),
    ]);

    for (name, value) in submitted.to_rows() {
        upsert_option(&repos, name, &value).await?;
    }

    // Discharge the invalidation `SiteOptionRepository` declares. Without it the
    // site would keep serving the old title, theme and permalink structure for
    // up to the 60-second TTL — the settings form would look broken.
    if !PgSiteOptionRepository::invalidate_declared_caches() {
        autumn_web::reexports::tracing::warn!(
            "cache backend cannot invalidate by namespace; settings may be stale until the TTL \
             expires"
        );
    }

    Ok(Redirect::to("/admin/settings").into_response())
}

/// Write one option, inserting it if it does not exist yet.
async fn upsert_option(repos: &Repos, name: &str, value: &str) -> AutumnResult<()> {
    match repos
        .options
        .find_by_name(name.to_owned())
        .await?
        .into_iter()
        .next()
    {
        Some(existing) => {
            repos
                .options
                .update(
                    existing.id,
                    &UpdateSiteOption {
                        name: autumn_web::hooks::Patch::Unchanged,
                        value: autumn_web::hooks::Patch::Set(value.to_owned()),
                        autoload: autumn_web::hooks::Patch::Unchanged,
                    },
                )
                .await?;
        }
        None => {
            repos
                .options
                .save(&NewSiteOption {
                    name: name.to_owned(),
                    value: value.to_owned(),
                    autoload: true,
                })
                .await?;
        }
    }
    Ok(())
}
