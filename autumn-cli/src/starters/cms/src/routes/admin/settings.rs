//! Settings — WordPress's General, Reading, Discussion and Permalinks screens,
//! on one page because there are not enough of them to warrant four.

use autumn_web::AutumnResult;
use autumn_web::prelude::*;
use autumn_web::reexports::axum::response::Response;
use serde::Deserialize;

use crate::capabilities::Capability;
use crate::models::Post;
use crate::permalinks::PermalinkStructure;
use crate::repositories::{PgSiteOptionRepository, PostRepository as _};
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
    pub timezone: String,
    #[serde(default)]
    pub front_page_id: String,
}

/// What the form shows: the stored settings, or the rejected submission.
///
/// Strings stay raw so a refused value is redisplayed as typed.
struct SettingsView {
    site_title: String,
    tagline: String,
    posts_per_page: String,
    permalink_structure: PermalinkStructure,
    default_comment_status: String,
    comment_moderation: bool,
    allow_guest_comments: bool,
    active_theme: String,
    date_format: String,
    timezone: String,
    front_page_id: Option<i64>,
}

impl From<&Settings> for SettingsView {
    fn from(settings: &Settings) -> Self {
        Self {
            site_title: settings.site_title.clone(),
            tagline: settings.tagline.clone(),
            posts_per_page: settings.posts_per_page.to_string(),
            permalink_structure: settings.permalink_structure,
            default_comment_status: settings.default_comment_status.clone(),
            comment_moderation: settings.comment_moderation,
            allow_guest_comments: settings.allow_guest_comments,
            active_theme: settings.active_theme.clone(),
            date_format: settings.date_format.clone(),
            timezone: settings.timezone.clone(),
            front_page_id: settings.front_page_id,
        }
    }
}

impl From<&SettingsForm> for SettingsView {
    fn from(form: &SettingsForm) -> Self {
        Self {
            site_title: form.site_title.clone(),
            tagline: form.tagline.clone(),
            posts_per_page: form.posts_per_page.clone(),
            permalink_structure: PermalinkStructure::parse(&form.permalink_structure),
            default_comment_status: form.default_comment_status.clone(),
            comment_moderation: form.comment_moderation.is_some(),
            allow_guest_comments: form.allow_guest_comments.is_some(),
            active_theme: form.active_theme.clone(),
            date_format: form.date_format.clone(),
            timezone: form.timezone.trim().to_owned(),
            front_page_id: form.front_page_id.trim().parse().ok().filter(|id| *id > 0),
        }
    }
}

/// Reasons a submission is refused, one per free-text field.
#[derive(Default)]
struct FieldErrors {
    date_format: Option<String>,
    timezone: Option<String>,
}

impl FieldErrors {
    /// Checks the two fields `Settings::from_rows` would silently reset.
    fn check(view: &SettingsView) -> Self {
        let timezone = view.timezone.trim();
        Self {
            timezone: (!crate::settings::is_known_timezone(timezone)).then(|| {
                if timezone.is_empty() {
                    "Enter a timezone — use an IANA name such as Europe/London".to_owned()
                } else {
                    format!(
                        "`{timezone}` is not a timezone this site knows — use an IANA name \
                         such as Europe/London"
                    )
                }
            }),
            date_format: (!crate::settings::is_renderable_date_format(view.date_format.trim()))
                .then(|| {
                    "That date format cannot be rendered — check the strftime directives".to_owned()
                }),
        }
    }

    const fn is_empty(&self) -> bool {
        self.date_format.is_none() && self.timezone.is_none()
    }

    /// Focus goes to the first field in page order that has an error.
    const fn focus_date_format(&self) -> bool {
        self.date_format.is_some()
    }

    const fn focus_timezone(&self) -> bool {
        self.timezone.is_some() && self.date_format.is_none()
    }
}

fn field_error(id: &str, message: Option<&str>) -> Markup {
    html! {
        @if let Some(message) = message {
            p #(id) class="text-sm text-red-700 mt-1" role="alert" { (message) }
        }
    }
}

/// The settings form for `view`, with `errors` shown beside their fields.
fn settings_body(csrf: &Csrf, view: &SettingsView, pages: &[Post], errors: &FieldErrors) -> Markup {
    html! {
        form action="/admin/settings" method="post" class="max-w-2xl space-y-6" {
            (csrf.input())
            section class="bg-white rounded-lg shadow p-5 space-y-4" {
                h2 class="font-semibold" { "General" }
                div {
                    label for="site_title" class="block text-sm font-medium mb-1" {
                        "Site title"
                    }
                    input #site_title type="text" name="site_title" required
                          value=(view.site_title) class="w-full border rounded px-3 py-2";
                }
                div {
                    label for="tagline" class="block text-sm font-medium mb-1" { "Tagline" }
                    input #tagline type="text" name="tagline" value=(view.tagline)
                          class="w-full border rounded px-3 py-2";
                }
                div {
                    label for="date_format" class="block text-sm font-medium mb-1" {
                        "Date format "
                        span class="text-gray-400 font-normal" { "(strftime)" }
                    }
                    input #date_format type="text" name="date_format" required
                          value=(view.date_format)
                          autofocus[errors.focus_date_format()]
                          aria-invalid=[errors.date_format.as_ref().map(|_| "true")]
                          aria-describedby=[errors.date_format.as_ref().map(|_| "date_format-error")]
                          class="w-full border rounded px-3 py-2 font-mono text-sm";
                    (field_error("date_format-error", errors.date_format.as_deref()))
                }
                div {
                    label for="timezone" class="block text-sm font-medium mb-1" {
                        "Timezone "
                        span class="text-gray-400 font-normal" { "(IANA name)" }
                    }
                    // A text field rather than a `<select>` of every zone the
                    // database knows: that list is some six hundred entries, and
                    // rendering all of them into every settings page is the same
                    // unbounded-control shape the media and parent pickers were
                    // just fixed for.
                    input #timezone type="text" name="timezone" required
                          value=(view.timezone) placeholder="Europe/London"
                          autofocus[errors.focus_timezone()]
                          aria-invalid=[errors.timezone.as_ref().map(|_| "true")]
                          aria-describedby=[errors.timezone.as_ref().map(|_| "timezone-error")]
                          class="w-full border rounded px-3 py-2 font-mono text-sm";
                    (field_error("timezone-error", errors.timezone.as_deref()))
                    p class="text-xs text-gray-400 mt-1" {
                        "Times are stored in UTC and shown in this zone — including \
                         the publish date the editor collects for a scheduled post."
                    }
                }
            }

            section class="bg-white rounded-lg shadow p-5 space-y-4" {
                h2 class="font-semibold" { "Reading" }
                div {
                    label for="posts_per_page" class="block text-sm font-medium mb-1" {
                        "Posts per page"
                    }
                    input #posts_per_page type="number" name="posts_per_page" min="1" max="100"
                          value=(view.posts_per_page)
                          class="w-full border rounded px-3 py-2";
                }
                div {
                    label for="front_page_id" class="block text-sm font-medium mb-1" {
                        "Front page"
                    }
                    select #front_page_id name="front_page_id"
                           class="w-full border rounded px-3 py-2" {
                        option value="" selected[view.front_page_id.is_none()] {
                            "Your latest posts"
                        }
                        @for page in pages {
                            option value=(page.id)
                                   selected[view.front_page_id == Some(page.id)] {
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
                        option value="open" selected[view.default_comment_status == "open"] {
                            "Allow comments"
                        }
                        option value="closed"
                               selected[view.default_comment_status == "closed"] {
                            "Do not allow comments"
                        }
                    }
                }
                label class="flex items-center gap-2 text-sm" {
                    input type="checkbox" name="comment_moderation" value="on"
                          checked[view.comment_moderation] class="rounded border-gray-300";
                    "Hold guest comments for moderation"
                }
                label class="flex items-center gap-2 text-sm" {
                    input type="checkbox" name="allow_guest_comments" value="on"
                          checked[view.allow_guest_comments] class="rounded border-gray-300";
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
                              checked[view.permalink_structure == *structure]
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
                                   selected[view.active_theme == registered.slug()] {
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
    }
}

/// The newest 200 pages, plus whichever one is configured. Without the
/// second half, a front page older than the newest 200 is simply absent from
/// the `<select>` — the browser then selects the empty option, and saving
/// any unrelated setting submits an empty `front_page_id` and silently
/// switches the site back to the posts index.
async fn front_page_choices(repos: &Repos, configured: Option<i64>) -> AutumnResult<Vec<Post>> {
    let mut pages = repos.published_posts("page", 200).await?;
    if let Some(configured) = configured
        && !pages.iter().any(|page| page.id == configured)
        && let Some(page) = repos.posts.find_by_id(configured).await?
    {
        pages.insert(0, page);
    }
    Ok(pages)
}

#[get("/admin/settings")]
pub async fn show(repos: Repos, session: Session, csrf: Csrf) -> AutumnResult<Response> {
    let user = require_capability!(repos, session, csrf, Capability::ManageOptions);
    let view = SettingsView::from(&repos.settings().await?);
    let pages = front_page_choices(&repos, view.front_page_id).await?;
    let body = settings_body(&csrf, &view, &pages, &FieldErrors::default());
    Ok(layout(&user, &csrf, "/admin/settings", "Settings", body).into_response())
}

#[post("/admin/settings")]
pub async fn save(
    repos: Repos,
    session: Session,
    csrf: Csrf,
    Form(form): Form<SettingsForm>,
) -> AutumnResult<Response> {
    let user = require_capability!(repos, session, csrf, Capability::ManageOptions);

    // Two fields are checked *before* the round trip below. `from_rows` keeps
    // the default for a value it cannot use. That is right for reading the
    // options table, but wrong for a form: a typo in `timezone` would
    // silently move a non-UTC site to UTC, and a typo in `date_format` would
    // reset the date style. The current value is worth more than a guess.
    // A refusal redisplays the form, so the other fields are not lost.
    let view = SettingsView::from(&form);
    let errors = FieldErrors::check(&view);
    if !errors.is_empty() {
        let pages = front_page_choices(&repos, view.front_page_id).await?;
        let body = settings_body(&csrf, &view, &pages, &errors);
        return Ok((
            StatusCode::UNPROCESSABLE_ENTITY,
            layout(&user, &csrf, "/admin/settings", "Settings", body),
        )
            .into_response());
    }
    let timezone = view.timezone.as_str();

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
        ("timezone".to_owned(), timezone.to_owned()),
        ("front_page_id".to_owned(), form.front_page_id.clone()),
    ]);

    // One transaction for the whole form. Committing option by option let two
    // administrators saving at once interleave into a configuration neither
    // submitted, and a failure part-way through left the form half-applied
    // while reporting an error.
    repos
        .with_conn(async |conn| crate::content::save_settings(conn, submitted.to_rows()).await)
        .await?;

    // Discharge the invalidation `SiteOptionRepository` declares. This write
    // does not use a generated repository method, so it does not invalidate
    // by itself. Invalidate *after* the commit, so a reader cannot cache the
    // pre-commit values after the invalidation. Without it the site serves the
    // old title, theme and permalink structure for up to the 60-second TTL. A
    // failure is already logged and counted.
    let _ = PgSiteOptionRepository::invalidate_declared_caches_async().await;

    Ok(Redirect::to("/admin/settings").into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view() -> SettingsView {
        SettingsView::from(&Settings::default())
    }

    #[test]
    fn a_valid_view_has_no_field_errors() {
        assert!(FieldErrors::check(&view()).is_empty());
    }

    #[test]
    fn an_unknown_timezone_is_a_timezone_error_only() {
        let mut v = view();
        v.timezone = "Mars/Olympus".to_owned();
        let errors = FieldErrors::check(&v);
        assert!(errors.timezone.as_deref().unwrap().contains("Mars/Olympus"));
        assert!(errors.date_format.is_none());
    }

    #[test]
    fn a_bad_date_format_is_a_date_format_error_only() {
        let mut v = view();
        v.date_format = "%".to_owned();
        let errors = FieldErrors::check(&v);
        assert!(errors.date_format.is_some());
        assert!(errors.timezone.is_none());
    }

    #[test]
    fn both_errors_are_reported_together() {
        let mut v = view();
        v.timezone = "nope".to_owned();
        v.date_format = "%".to_owned();
        let errors = FieldErrors::check(&v);
        assert!(errors.timezone.is_some() && errors.date_format.is_some());
    }

    #[test]
    fn a_blank_timezone_gets_its_own_message() {
        let mut v = view();
        v.timezone = String::new();
        let message = FieldErrors::check(&v).timezone.unwrap();
        assert!(message.starts_with("Enter a timezone"));
    }

    #[test]
    fn only_one_field_takes_focus() {
        let mut v = view();
        v.timezone = "nope".to_owned();
        let errors = FieldErrors::check(&v);
        assert!(errors.focus_timezone() && !errors.focus_date_format());
        v.date_format = "%".to_owned();
        let errors = FieldErrors::check(&v);
        assert!(errors.focus_date_format() && !errors.focus_timezone());
    }

    #[test]
    fn a_form_view_keeps_the_raw_input() {
        let form = SettingsForm {
            site_title: "T".to_owned(),
            tagline: "G".to_owned(),
            posts_per_page: "abc".to_owned(),
            permalink_structure: "plain".to_owned(),
            default_comment_status: "closed".to_owned(),
            comment_moderation: None,
            allow_guest_comments: Some("on".to_owned()),
            active_theme: "default".to_owned(),
            date_format: "%".to_owned(),
            timezone: " Mars/X ".to_owned(),
            front_page_id: "7".to_owned(),
        };
        let v = SettingsView::from(&form);
        assert_eq!(v.posts_per_page, "abc");
        assert_eq!(v.timezone, "Mars/X");
        assert_eq!(v.date_format, "%");
        assert_eq!(v.front_page_id, Some(7));
        assert!(!v.comment_moderation && v.allow_guest_comments);
    }
}
