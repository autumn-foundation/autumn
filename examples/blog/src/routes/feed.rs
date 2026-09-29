//! Atom feed route for the blog application.
//!
//! Builds an Atom 1.0 feed of published posts and returns it directly from a
//! `#[get]` handler. [`Feed::conditional`] reuses Autumn's `etag` layer so
//! feed pollers get a `304 Not Modified` when the feed hasn't changed.

use autumn_web::feed::{Feed, FeedEntry};
use autumn_web::reexports::axum::response::Response;
use autumn_web::reexports::http::HeaderMap;
use autumn_web::{AutumnResult, Db, get};

use crate::models::Post;

/// Base URL used to build absolute links in the feed.
const SITE_URL: &str = "https://autumn-demo.example.com";

/// Atom feed of published posts with conditional-GET (304) support.
///
/// Returns `application/atom+xml`. On a repeat request whose `If-None-Match`
/// header matches the feed's ETag, [`Feed::conditional`] short-circuits with a
/// `304 Not Modified` instead of re-rendering the XML.
#[get("/feed.xml")]
pub async fn feed_xml(mut db: Db, headers: HeaderMap) -> AutumnResult<Response> {
    let posts = Post::published(&mut db).await?;

    let feed = Feed::atom("Autumn Blog", SITE_URL, format!("{SITE_URL}/feed.xml"))
        .author("Autumn Blog")
        .entries(posts.iter().map(|p| {
            let url = format!("{SITE_URL}/posts/{}", p.slug);
            FeedEntry::new(url.clone(), p.title.as_str(), url)
                .summary(p.body.as_str())
                .published(p.created_at.and_utc())
                .updated(p.updated_at.and_utc())
        }));

    Ok(feed.conditional(&headers))
}
