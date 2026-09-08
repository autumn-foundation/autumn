//! Site settings — WordPress's options table, read through one typed struct.
//!
//! WordPress reads settings with `get_option('blogname')`: a string key, an
//! untyped return, and a default supplied (or forgotten) at each of the
//! hundreds of call sites. Here the whole set is loaded once per request as a
//! [`Settings`] struct with typed fields and defaults in exactly one place, so
//! a screen cannot disagree with the settings form about what
//! `posts_per_page` means when the row is missing or unparseable.

use autumn_web::AutumnResult;
use serde::{Deserialize, Serialize};

use crate::permalinks::PermalinkStructure;
use crate::repositories::{PgSiteOptionRepository, SiteOptionRepository as _};

/// Every site-wide setting, resolved.
///
/// `Serialize`/`Deserialize` are what let the whole struct be the memoized
/// value of [`cached_settings`] — the cache stores the resolved settings, not
/// the raw rows, so a cache hit skips the parsing and defaulting too.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    /// WordPress's `blogname`.
    pub site_title: String,
    /// WordPress's `blogdescription`.
    pub tagline: String,
    /// How many posts a listing shows before paginating.
    pub posts_per_page: i64,
    /// The URL shape for a post's permalink.
    pub permalink_structure: PermalinkStructure,
    /// Whether new posts accept comments by default.
    pub default_comment_status: String,
    /// Whether a comment must be approved before it appears.
    pub comment_moderation: bool,
    /// Whether comments may be left by visitors with no account.
    pub allow_guest_comments: bool,
    /// The slug of the active theme.
    pub active_theme: String,
    /// The `strftime` format dates render in.
    pub date_format: String,
    /// A page id to show as the front page instead of the blog index.
    pub front_page_id: Option<i64>,
}

impl Default for Settings {
    /// The defaults a freshly-migrated site runs on, matching WordPress's own
    /// out-of-the-box values where it has one.
    fn default() -> Self {
        Self {
            site_title: "Autumn CMS".to_owned(),
            tagline: "Just another Autumn site".to_owned(),
            posts_per_page: 10,
            permalink_structure: PermalinkStructure::default(),
            default_comment_status: "open".to_owned(),
            comment_moderation: true,
            allow_guest_comments: true,
            active_theme: "default".to_owned(),
            date_format: "%B %-d, %Y".to_owned(),
            front_page_id: None,
        }
    }
}

/// The option names this struct is assembled from. Exposed so the settings
/// screen and the seed can round-trip the same keys rather than re-typing them.
pub mod keys {
    pub const SITE_TITLE: &str = "site_title";
    pub const TAGLINE: &str = "tagline";
    pub const POSTS_PER_PAGE: &str = "posts_per_page";
    pub const PERMALINK_STRUCTURE: &str = "permalink_structure";
    pub const DEFAULT_COMMENT_STATUS: &str = "default_comment_status";
    pub const COMMENT_MODERATION: &str = "comment_moderation";
    pub const ALLOW_GUEST_COMMENTS: &str = "allow_guest_comments";
    pub const ACTIVE_THEME: &str = "active_theme";
    pub const DATE_FORMAT: &str = "date_format";
    pub const FRONT_PAGE_ID: &str = "front_page_id";
}

/// Whether a strftime pattern can actually be rendered.
///
/// `NaiveDateTime::format` parses nothing eagerly — it hands back a
/// `DelayedFormat` whose `Display` does the work and returns `Err` on a bad
/// directive. `write!` surfaces that as a `Result`; `to_string()` panics on it.
/// The probe date carries a value for every field a pattern might name.
fn is_renderable_date_format(pattern: &str) -> bool {
    use std::fmt::Write as _;
    let probe = chrono::NaiveDate::from_ymd_opt(2026, 1, 31)
        .and_then(|date| date.and_hms_opt(13, 45, 6))
        .expect("the probe timestamp is a valid date and time");
    let mut out = String::new();
    write!(out, "{}", probe.format(pattern)).is_ok()
}

impl Settings {
    /// Build a settings struct from raw `(name, value)` option rows.
    ///
    /// Every field falls back to its default when the row is missing *or*
    /// unparseable. A garbage `posts_per_page` must not take the site down —
    /// it should render 10 posts and let the operator notice on the settings
    /// screen.
    #[must_use]
    pub fn from_rows(rows: &[(String, String)]) -> Self {
        let mut settings = Self::default();
        for (name, value) in rows {
            let value = value.trim();
            match name.as_str() {
                keys::SITE_TITLE if !value.is_empty() => settings.site_title = value.to_owned(),
                keys::TAGLINE => settings.tagline = value.to_owned(),
                keys::POSTS_PER_PAGE => {
                    if let Ok(parsed) = value.parse::<i64>() {
                        // A non-positive page size would loop forever; an
                        // unbounded one is a denial-of-service by settings form.
                        if (1..=100).contains(&parsed) {
                            settings.posts_per_page = parsed;
                        }
                    }
                }
                keys::PERMALINK_STRUCTURE => {
                    settings.permalink_structure = PermalinkStructure::parse(value);
                }
                keys::DEFAULT_COMMENT_STATUS if matches!(value, "open" | "closed") => {
                    settings.default_comment_status = value.to_owned();
                }
                keys::COMMENT_MODERATION => settings.comment_moderation = parse_bool(value),
                keys::ALLOW_GUEST_COMMENTS => settings.allow_guest_comments = parse_bool(value),
                keys::ACTIVE_THEME if !value.is_empty() => {
                    settings.active_theme = value.to_owned();
                }
                // Only a pattern that actually renders. An unrenderable one
                // is not a cosmetic mistake: `format()` defers everything to
                // `Display`, a malformed directive makes `Display` return an
                // error, and `to_string()` turns that into a *panic* — so a
                // single typo in the settings form (`%` is enough) takes down
                // every dated listing and every single-post page at once.
                //
                // Checked here rather than only in the form handler because
                // this is the one funnel every source goes through: the form,
                // an import, a direct write to `options`. A value that fails is
                // ignored, leaving the previous (or default) format in place —
                // a wrong-looking date is survivable, a 500 on every public
                // page is not.
                keys::DATE_FORMAT if !value.is_empty() && is_renderable_date_format(value) => {
                    settings.date_format = value.to_owned();
                }
                keys::FRONT_PAGE_ID => {
                    settings.front_page_id = value.parse::<i64>().ok().filter(|id| *id > 0);
                }
                _ => {}
            }
        }
        settings
    }

    /// Render a timestamp with the configured pattern, without the panic.
    ///
    /// `format(..).to_string()` is the obvious spelling and it panics on a
    /// malformed pattern. `from_rows` refuses to store one, so this should
    /// never see it — but the line is short enough to copy into new code, and
    /// this makes the copied version safe by construction.
    #[must_use]
    pub fn format_date(&self, when: chrono::NaiveDateTime) -> String {
        use std::fmt::Write as _;
        let mut out = String::new();
        if write!(out, "{}", when.format(&self.date_format)).is_ok() {
            return out;
        }
        // Unreachable via `from_rows`; ISO-8601 rather than an empty cell.
        when.format("%Y-%m-%d").to_string()
    }

    /// The `(name, value)` pairs that persist this struct.
    #[must_use]
    pub fn to_rows(&self) -> Vec<(&'static str, String)> {
        vec![
            (keys::SITE_TITLE, self.site_title.clone()),
            (keys::TAGLINE, self.tagline.clone()),
            (keys::POSTS_PER_PAGE, self.posts_per_page.to_string()),
            (
                keys::PERMALINK_STRUCTURE,
                self.permalink_structure.as_str().to_owned(),
            ),
            (
                keys::DEFAULT_COMMENT_STATUS,
                self.default_comment_status.clone(),
            ),
            (
                keys::COMMENT_MODERATION,
                self.comment_moderation.to_string(),
            ),
            (
                keys::ALLOW_GUEST_COMMENTS,
                self.allow_guest_comments.to_string(),
            ),
            (keys::ACTIVE_THEME, self.active_theme.clone()),
            (keys::DATE_FORMAT, self.date_format.clone()),
            (
                keys::FRONT_PAGE_ID,
                self.front_page_id
                    .map(|id| id.to_string())
                    .unwrap_or_default(),
            ),
        ]
    }
}

/// Accept the several spellings of "true" a form, a seed and a hand-edited row
/// might each produce.
fn parse_bool(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// Load the site settings, memoized for 60 seconds.
///
/// Every rendered page reads these, and they change only when an administrator
/// saves the settings form — the textbook case for a cached read, and the
/// textbook place to strand a stale value. `reads(SiteOption)` declares what
/// the value is derived from; `SiteOptionRepository`'s matching
/// `invalidates(cached_settings)` clause is what the build-time coherence gate
/// proves the pair against, and what the settings screen calls to discharge it.
/// The cache key for the one settings value this deployment has.
///
/// `#[cached]` requires `key(...)` to name at least one parameter, and the
/// repository handle cannot be it — it is a per-request extractor, not part of
/// the value's identity, so keying on it would miss on every request. A
/// single-site install therefore keys on a constant.
///
/// It is not a placeholder for tenancy. Since #2528 the macro folds the
/// ambient tenant into every generated key by itself, so an app that put this
/// CMS behind Autumn's row-level multi-tenancy would get per-tenant settings
/// without touching this constant. What it *is* for is a site discriminator
/// that is not a tenant — a multisite install serving several sites from one
/// tenant — where the parameter is the thing to replace.
pub const SITE_SCOPE: &str = "site";

#[autumn_web::cached(ttl = "60s", key(scope), reads(crate::models::SiteOption), result)]
pub async fn cached_settings(
    scope: &'static str,
    repo: &PgSiteOptionRepository,
) -> AutumnResult<Settings> {
    let rows: Vec<(String, String)> = repo
        .find_all()
        .await?
        .into_iter()
        .map(|option| (option.name, option.value))
        .collect();
    Ok(Settings::from_rows(&rows))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_rows_fall_back_to_defaults() {
        let settings = Settings::from_rows(&[]);
        assert_eq!(settings, Settings::default());
    }

    #[test]
    fn unparseable_values_do_not_take_the_site_down() {
        let rows = vec![
            ("posts_per_page".to_owned(), "not a number".to_owned()),
            ("front_page_id".to_owned(), "banana".to_owned()),
        ];
        let settings = Settings::from_rows(&rows);
        assert_eq!(settings.posts_per_page, 10);
        assert_eq!(settings.front_page_id, None);
    }

    #[test]
    fn out_of_range_page_size_is_rejected() {
        // Zero would paginate forever; 100_000 is a self-inflicted outage.
        for bad in ["0", "-5", "100000"] {
            let rows = vec![("posts_per_page".to_owned(), bad.to_owned())];
            assert_eq!(Settings::from_rows(&rows).posts_per_page, 10, "{bad}");
        }
        let rows = vec![("posts_per_page".to_owned(), "25".to_owned())];
        assert_eq!(Settings::from_rows(&rows).posts_per_page, 25);
    }

    #[test]
    fn settings_round_trip_through_rows() {
        let original = Settings {
            site_title: "My Blog".to_owned(),
            posts_per_page: 25,
            comment_moderation: false,
            front_page_id: Some(12),
            permalink_structure: PermalinkStructure::DayAndName,
            ..Settings::default()
        };

        let rows: Vec<(String, String)> = original
            .to_rows()
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v))
            .collect();
        assert_eq!(Settings::from_rows(&rows), original);
    }

    #[test]
    fn boolean_spellings_are_all_accepted() {
        for truthy in ["1", "true", "TRUE", "yes", "on"] {
            let rows = vec![("comment_moderation".to_owned(), truthy.to_owned())];
            assert!(Settings::from_rows(&rows).comment_moderation, "{truthy}");
        }
        for falsy in ["0", "false", "no", "off", ""] {
            let rows = vec![("comment_moderation".to_owned(), falsy.to_owned())];
            assert!(!Settings::from_rows(&rows).comment_moderation, "{falsy}");
        }
    }
}
