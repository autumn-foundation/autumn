//! Theme preference and density words for framework UI (issue #3396).
//!
//! The design tokens follow the OS color scheme. A user can override it:
//! the layout reads [`Theme`] from the `autumn.theme` cookie and renders
//! `data-theme` on `<html>`, so the first paint has the correct theme.
//!
//! [`crate::widgets::theme_toggle`] posts to [`THEME_PATH`]. Mount the
//! handler with [`router`]. `autumn-widgets.js` applies the theme in place and
//! writes the same cookie, so with JS no request is sent.
//!
//! # Example
//!
//! ```rust
//! use autumn_web::ui::theme::{Density, Theme};
//!
//! // In a layout: `html data-theme=[theme.data_theme()] data-density=(density.as_str())`.
//! let theme = Theme::parse("dark").unwrap_or_default();
//! assert_eq!(theme.data_theme(), Some("dark"));
//! assert_eq!(Theme::System.data_theme(), None);
//! assert_eq!(Density::Compact.as_str(), "compact");
//! ```
//!
//! A shared page cache must vary on `Cookie` when the layout reads the theme.

use std::convert::Infallible;

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};

/// Name of the theme preference cookie.
pub const THEME_COOKIE: &str = "autumn.theme";

/// Path of the route that [`router`] mounts. The default form action of
/// [`crate::widgets::theme_toggle`].
pub const THEME_PATH: &str = "/_autumn/theme";

/// Lifetime of the theme cookie: one year.
pub const THEME_COOKIE_MAX_AGE_SECS: u64 = 365 * 24 * 60 * 60;

/// A user's color scheme preference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Theme {
    /// Follow the OS setting. No `data-theme` attribute.
    #[default]
    System,
    /// Always light.
    Light,
    /// Always dark.
    Dark,
}

impl Theme {
    /// All values, in toggle order.
    pub const ALL: [Self; 3] = [Self::System, Self::Light, Self::Dark];

    /// The cookie and form word: `"system"`, `"light"` or `"dark"`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Light => "light",
            Self::Dark => "dark",
        }
    }

    /// Parse a word from [`Theme::as_str`]. Any other text gives `None`.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|theme| theme.as_str() == value)
    }

    /// The value for the `data-theme` attribute, or `None` for
    /// [`Theme::System`] (omit the attribute).
    #[must_use]
    pub const fn data_theme(self) -> Option<&'static str> {
        match self {
            Self::System => None,
            Self::Light | Self::Dark => Some(self.as_str()),
        }
    }

    /// Read the preference from the `Cookie` header.
    ///
    /// A missing, unknown or duplicate cookie gives [`Theme::System`].
    #[must_use]
    pub fn from_headers(headers: &HeaderMap) -> Self {
        crate::consent::find_cookie(headers, THEME_COOKIE)
            .and_then(|value| Self::parse(&value))
            .unwrap_or_default()
    }

    /// A `Set-Cookie` value that stores this preference.
    ///
    /// [`Theme::System`] expires the cookie. The cookie is not `HttpOnly`,
    /// because `autumn-widgets.js` writes it too. It holds no personal data.
    #[must_use]
    pub fn set_cookie(self) -> String {
        match self {
            Self::System => format!("{THEME_COOKIE}=; Path=/; Max-Age=0; Secure; SameSite=Lax"),
            Self::Light | Self::Dark => format!(
                "{THEME_COOKIE}={}; Path=/; Max-Age={THEME_COOKIE_MAX_AGE_SECS}; Secure; SameSite=Lax",
                self.as_str()
            ),
        }
    }
}

impl<S> FromRequestParts<S> for Theme
where
    S: Send + Sync,
{
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(Self::from_headers(&parts.headers))
    }
}

/// Spacing density. Set it with `data-density` on any element.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Density {
    /// Space tokens at 0.75×.
    Compact,
    /// Space tokens at 1× (the default).
    #[default]
    Comfortable,
    /// Space tokens at 1.25×.
    Spacious,
}

impl Density {
    /// All values, from small to large.
    pub const ALL: [Self; 3] = [Self::Compact, Self::Comfortable, Self::Spacious];

    /// The `data-density` attribute value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Compact => "compact",
            Self::Comfortable => "comfortable",
            Self::Spacious => "spacious",
        }
    }
}

/// Form body of a theme change.
#[derive(Debug, serde::Deserialize)]
struct ThemeForm {
    theme: Option<String>,
    return_to: Option<String>,
}

/// A router with `POST` [`THEME_PATH`]. Merge it into the app:
///
/// ```rust,no_run
/// # async fn run() {
/// autumn_web::app()
///     .merge(autumn_web::ui::theme::router())
///     .run()
///     .await;
/// # }
/// ```
///
/// The handler stores the cookie and redirects (`303`) to the form's
/// `return_to` field, else to the `Referer` path, else to `/`. Off-site
/// targets go to `/`. An unknown theme gives `400`. The route is a normal
/// `POST`, so the CSRF layer applies: give the widget the token with
/// [`crate::widgets::ThemeToggleConfig::csrf`].
pub fn router<S>() -> axum::Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    axum::Router::new().route(THEME_PATH, axum::routing::post(set_theme))
}

async fn set_theme(headers: HeaderMap, axum::Form(form): axum::Form<ThemeForm>) -> Response {
    let Some(theme) = form.theme.as_deref().and_then(Theme::parse) else {
        return (StatusCode::BAD_REQUEST, "unknown theme").into_response();
    };
    let target = form.return_to.as_deref().map_or_else(
        || {
            crate::consent::redirect_target_from_referer(
                headers.get(header::REFERER).and_then(|v| v.to_str().ok()),
            )
        },
        |path| crate::consent::safe_redirect_target(path).to_owned(),
    );
    let location =
        HeaderValue::from_str(&encode_non_ascii(&target)).unwrap_or(HeaderValue::from_static("/"));
    let mut response = StatusCode::SEE_OTHER.into_response();
    let out = response.headers_mut();
    out.insert(header::LOCATION, location);
    // Built from fixed words only, so the value is always valid.
    if let Ok(cookie) = HeaderValue::from_str(&theme.set_cookie()) {
        out.insert(header::SET_COOKIE, cookie);
    }
    out.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// Percent-encode each non-ASCII byte, so the path is plain ASCII header text.
fn encode_non_ascii(path: &str) -> String {
    use std::fmt::Write as _;
    path.bytes()
        .fold(String::with_capacity(path.len()), |mut out, byte| {
            if byte.is_ascii() {
                out.push(char::from(byte));
            } else {
                let _ = write!(out, "%{byte:02X}");
            }
            out
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{HeaderMap, HeaderValue, Request, StatusCode, header};
    use tower::ServiceExt as _;

    fn cookie_headers(raw: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(header::COOKIE, HeaderValue::from_str(raw).unwrap());
        headers
    }

    #[test]
    fn parse_accepts_the_three_words_only() {
        assert_eq!(Theme::parse("system"), Some(Theme::System));
        assert_eq!(Theme::parse("light"), Some(Theme::Light));
        assert_eq!(Theme::parse("dark"), Some(Theme::Dark));
        for bad in ["", "Dark", " dark", "blue", "dark\"><script>"] {
            assert_eq!(Theme::parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn as_str_round_trips_and_system_is_the_default() {
        assert_eq!(Theme::default(), Theme::System);
        assert_eq!(Theme::ALL, [Theme::System, Theme::Light, Theme::Dark]);
        for theme in Theme::ALL {
            assert_eq!(Theme::parse(theme.as_str()), Some(theme));
        }
    }

    #[test]
    fn data_theme_is_none_for_system() {
        assert_eq!(Theme::System.data_theme(), None);
        assert_eq!(Theme::Light.data_theme(), Some("light"));
        assert_eq!(Theme::Dark.data_theme(), Some("dark"));
    }

    #[test]
    fn from_headers_reads_the_cookie() {
        assert_eq!(Theme::from_headers(&HeaderMap::new()), Theme::System);
        assert_eq!(
            Theme::from_headers(&cookie_headers("a=1; autumn.theme=dark")),
            Theme::Dark
        );
        assert_eq!(
            Theme::from_headers(&cookie_headers("autumn.theme=purple")),
            Theme::System
        );
    }

    /// Two cookies with one name can be cookie tossing. Trust neither.
    #[test]
    fn from_headers_ignores_a_duplicate_cookie() {
        let headers = cookie_headers("autumn.theme=dark; autumn.theme=light");
        assert_eq!(Theme::from_headers(&headers), Theme::System);
    }

    #[test]
    fn set_cookie_stores_light_and_dark_for_a_year() {
        let cookie = Theme::Dark.set_cookie();
        assert_eq!(
            cookie,
            "autumn.theme=dark; Path=/; Max-Age=31536000; Secure; SameSite=Lax"
        );
        // The widget script writes this cookie, so it must not be HttpOnly.
        assert!(!cookie.contains("HttpOnly"));
    }

    #[test]
    fn set_cookie_for_system_expires_the_cookie() {
        assert_eq!(
            Theme::System.set_cookie(),
            "autumn.theme=; Path=/; Max-Age=0; Secure; SameSite=Lax"
        );
    }

    #[test]
    fn density_words_match_the_stylesheet() {
        assert_eq!(Density::default(), Density::Comfortable);
        let words: Vec<_> = Density::ALL.iter().map(|d| d.as_str()).collect();
        assert_eq!(words, ["compact", "comfortable", "spacious"]);
        for word in words {
            let rule = format!("[data-density=\"{word}\"]");
            assert!(crate::ui::tokens::TOKENS_CSS.contains(&rule), "{rule}");
        }
    }

    #[tokio::test]
    async fn extractor_reads_the_cookie() {
        let (mut parts, ()) = Request::builder()
            .header(header::COOKIE, "autumn.theme=light")
            .body(())
            .unwrap()
            .into_parts();
        let theme =
            <Theme as axum::extract::FromRequestParts<()>>::from_request_parts(&mut parts, &())
                .await
                .unwrap();
        assert_eq!(theme, Theme::Light);
    }

    async fn post(body: &str, referer: Option<&str>) -> axum::response::Response {
        let mut request = Request::post(THEME_PATH)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
        if let Some(referer) = referer {
            request = request.header(header::REFERER, referer);
        }
        router::<()>()
            .oneshot(request.body(Body::from(body.to_owned())).unwrap())
            .await
            .unwrap()
    }

    fn location(response: &axum::response::Response) -> &str {
        response.headers()[header::LOCATION].to_str().unwrap()
    }

    #[tokio::test]
    async fn post_sets_the_cookie_and_returns_to_the_referer() {
        let response = post("theme=dark", Some("https://app.example/posts?page=2")).await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(location(&response), "/posts?page=2");
        assert_eq!(
            response.headers()[header::SET_COOKIE],
            Theme::Dark.set_cookie().as_str()
        );
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    }

    #[tokio::test]
    async fn post_prefers_return_to_over_the_referer() {
        let response = post("theme=light&return_to=%2Fsettings", Some("https://a/b")).await;
        assert_eq!(location(&response), "/settings");
    }

    #[tokio::test]
    async fn post_refuses_an_off_site_return_to() {
        for target in [
            "%2F%2Fevil.example",
            "https%3A%2F%2Fevil.example",
            "%2F%5Cevil",
        ] {
            let response = post(&format!("theme=dark&return_to={target}"), None).await;
            assert_eq!(location(&response), "/", "{target}");
        }
    }

    #[tokio::test]
    async fn post_percent_encodes_a_non_ascii_return_to() {
        let response = post("theme=dark&return_to=%2Fcaf%C3%A9", None).await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(location(&response), "/caf%C3%A9");
    }

    #[tokio::test]
    async fn post_without_a_referer_returns_home() {
        let response = post("theme=system", None).await;
        assert_eq!(location(&response), "/");
        assert_eq!(
            response.headers()[header::SET_COOKIE],
            Theme::System.set_cookie().as_str()
        );
    }

    #[tokio::test]
    async fn post_with_an_unknown_theme_is_a_bad_request() {
        for body in ["theme=blue", "", "return_to=%2F"] {
            let response = post(body, None).await;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{body}");
            assert!(response.headers().get(header::SET_COOKIE).is_none());
        }
    }
}
