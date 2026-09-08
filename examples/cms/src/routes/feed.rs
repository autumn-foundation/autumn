//! Syndication — WordPress's `/feed` and `/feed/rss`, plus per-term feeds.
//!
//! WordPress ships four feed formats (RSS2, Atom, RDF, and a comments feed) at
//! a dozen URLs. Two formats cover every reader written this century, so this
//! serves Atom (the default) and RSS 2.0, at the site level and per term.
//!
//! `Feed::conditional` is what makes a feed cheap to poll: a reader that sends
//! `If-None-Match` gets a `304` with no body, which matters because feed
//! readers poll on a timer forever.

use autumn_web::AutumnResult;
use autumn_web::feed::{Feed, FeedEntry};
use autumn_web::prelude::*;
use autumn_web::reexports::axum::response::Response;
use autumn_web::reexports::http::HeaderMap;

use crate::models::Post;
use crate::repositories::TermRepository as _;
use crate::settings::Settings;
use crate::theme;

use super::site::Repos;

/// How many items a feed carries. WordPress's `posts_per_rss` default is 10.
const FEED_LIMIT: i64 = 20;

#[get("/feed")]
pub async fn atom(
    repos: Repos,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> AutumnResult<Response> {
    let settings = repos.settings().await?;
    let base = base_url(&state);
    let feed = build(
        &repos,
        &settings,
        Feed::atom(
            settings.site_title.clone(),
            format!("{base}/"),
            format!("{base}/feed"),
        ),
        &base,
        None,
    )
    .await?;
    Ok(feed.conditional(&headers))
}

#[get("/feed/rss")]
pub async fn rss(
    repos: Repos,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> AutumnResult<Response> {
    let settings = repos.settings().await?;
    let base = base_url(&state);
    let feed = build(
        &repos,
        &settings,
        Feed::rss(
            settings.site_title.clone(),
            format!("{base}/"),
            format!("{base}/feed/rss"),
        ),
        &base,
        None,
    )
    .await?;
    Ok(feed.conditional(&headers))
}

/// A per-term feed: `/category/rust/feed`, `/tag/async/feed`.
///
/// This is the URL a reader subscribes to when they want one topic rather than
/// the whole site, and WordPress has offered it since 2004.
///
/// **Not** a route of its own: a literal `/{taxonomy_base}/{slug}/feed` and the
/// front controller's `/{*path}` are a route-shape conflict the router refuses
/// to build. `permalinks::resolve` recognises the shape instead and
/// `front::dispatch` calls this — so the URL is WordPress's without the
/// wildcard collision. `taxonomy` here is the taxonomy *slug* (`post_tag`),
/// already resolved from the URL's rewrite base (`tag`) by the resolver.
pub async fn term_feed(
    repos: &Repos,
    state: &AppState,
    headers: &HeaderMap,
    taxonomy: &str,
    slug: String,
) -> AutumnResult<Response> {
    let taxonomy = crate::content_types::find_taxonomy(taxonomy)
        .ok_or_else(|| AutumnError::not_found_msg("No such taxonomy"))?;

    let term = repos
        .terms
        .find_by_slug(slug.clone())
        .await?
        .into_iter()
        .find(|t| t.taxonomy == taxonomy.slug)
        .ok_or_else(|| AutumnError::not_found_msg("No such term"))?;

    let settings = repos.settings().await?;
    let base = base_url(state);
    let feed = build(
        repos,
        &settings,
        Feed::atom(
            format!("{} · {}", settings.site_title, term.name),
            format!("{base}{}", theme::term_url(&term)),
            format!("{base}{}/feed", theme::term_url(&term)),
        ),
        &base,
        Some(term.id),
    )
    .await?;
    Ok(feed.conditional(headers))
}

/// Fill a feed with the newest published posts, optionally scoped to a term.
async fn build(
    repos: &Repos,
    settings: &Settings,
    feed: Feed,
    base: &str,
    term_id: Option<i64>,
) -> AutumnResult<Feed> {
    let posts: Vec<Post> = match term_id {
        Some(term_id) => {
            let (posts, _) = repos
                .posts_in_term(term_id, 0, usize::try_from(FEED_LIMIT).unwrap_or(20))
                .await?;
            posts
        }
        None => repos.published_posts("post", FEED_LIMIT).await?,
    };

    let mut feed = feed.description(settings.tagline.clone());
    for post in &posts {
        // An absolute URL: a feed item's link is dereferenced far from the site
        // that served it, so a relative path is useless.
        let url = format!("{base}{}", repos.permalink(post, settings).await?);
        let mut entry =
            FeedEntry::new(url.clone(), post.title.clone(), url).summary(post.display_excerpt());

        // Password-protected posts appear in the feed by title only. Putting the
        // body in the feed would hand out exactly what the password withholds.
        if !post.is_password_protected() {
            entry = entry.content(theme::render_content(&post.body).into_string());
        }
        if let Some(published) = post.published_at {
            entry = entry
                .published(published.and_utc())
                .updated(post.updated_at.max(published).and_utc());
        }
        feed = feed.entry(entry);
    }
    Ok(feed)
}

/// The site's absolute base URL, without a trailing slash.
///
/// `[seo] base_url` in `autumn.toml` is the single source of truth for the
/// site's public origin — the same value that drives canonical URLs and
/// `sitemap.xml` — so a feed can never disagree with a canonical tag about
/// where the site lives.
pub fn base_url(state: &AppState) -> String {
    state
        .config_arc()
        .seo
        .base_url
        .clone()
        .unwrap_or_else(|| "http://localhost:3000".to_owned())
        .trim_end_matches('/')
        .to_owned()
}
