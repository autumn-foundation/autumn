use autumn_web::feed::{Feed, FeedEntry, FeedFormat};
use axum::body::to_bytes;
use axum::response::IntoResponse;
use chrono::{TimeZone, Utc};
use http::header::{CONTENT_TYPE, ETAG, IF_NONE_MATCH, LAST_MODIFIED};
use http::{HeaderMap, StatusCode};

fn sample_feed(format: FeedFormat) -> Feed {
    let base = match format {
        FeedFormat::Atom => Feed::atom(
            "Autumn Blog",
            "https://example.com/",
            "https://example.com/feed.xml",
        ),
        FeedFormat::Rss => Feed::rss(
            "Autumn Blog",
            "https://example.com/",
            "https://example.com/feed.xml",
        ),
    };
    let entries = (0u32..10).map(|i| {
        let ts = Utc.with_ymd_and_hms(2026, 5, 1 + i, 12, 0, 0).unwrap();
        FeedEntry::new(
            format!("https://example.com/posts/post-{i}"),
            format!("Post {i}"),
            format!("https://example.com/posts/post-{i}"),
        )
        .summary(format!("Summary of post {i}"))
        .published(ts)
        .updated(ts)
    });
    base.author("Autumn Team").entries(entries)
}

#[test]
fn atom_feed_has_valid_shape() {
    let xml = sample_feed(FeedFormat::Atom).render();
    assert!(xml.starts_with("<?xml version=\"1.0\" encoding=\"utf-8\"?>"));
    assert!(xml.contains("<feed xmlns=\"http://www.w3.org/2005/Atom\">"));
    assert!(xml.contains("<title>Autumn Blog</title>"));
    assert!(xml.contains("<id>https://example.com/feed.xml</id>"));
    assert!(xml.contains(
        "<link href=\"https://example.com/feed.xml\" rel=\"self\" type=\"application/atom+xml\"/>"
    ));
    assert!(xml.contains("<updated>"));
    assert!(xml.contains("<name>Autumn Team</name>"));
    assert_eq!(xml.matches("<entry>").count(), 10);
    assert!(xml.contains("<title>Post 0</title>"));
    assert!(xml.contains("<published>2026-05-01T12:00:00Z</published>"));
    assert!(xml.trim_end().ends_with("</feed>"));
}

#[test]
fn rss_feed_has_valid_shape() {
    let xml = sample_feed(FeedFormat::Rss).render();
    assert!(xml.contains("<rss version=\"2.0\""));
    assert!(xml.contains("<channel>"));
    assert!(xml.contains("<title>Autumn Blog</title>"));
    assert!(xml.contains("<link>https://example.com/</link>"));
    assert!(xml.contains("<description>"));
    assert!(xml.contains("rel=\"self\""));
    assert_eq!(xml.matches("<item>").count(), 10);
    assert!(xml.contains("<guid isPermaLink=\"false\">https://example.com/posts/post-0</guid>"));
    assert!(xml.contains("<pubDate>"));
    assert!(xml.trim_end().ends_with("</rss>"));
}

#[test]
fn escapes_special_characters_no_injection() {
    let feed = Feed::atom(
        "A & B <feed>",
        "https://example.com/",
        "https://example.com/feed.xml",
    )
    .entry(
        FeedEntry::new(
            "https://example.com/x",
            "<script>alert('x')</script> & \"quotes\"",
            "https://example.com/x",
        )
        .summary("payload ]]> break-out & <b>bold</b>")
        .published(Utc.with_ymd_and_hms(2026, 5, 1, 12, 0, 0).unwrap()),
    );
    let xml = feed.render();
    // No raw markup from user input survives.
    assert!(!xml.contains("<script>"));
    assert!(!xml.contains("]]>"));
    assert!(!xml.contains("<b>bold</b>"));
    // Escaped forms are present.
    assert!(xml.contains("&lt;script&gt;"));
    assert!(xml.contains("&amp;"));
    assert!(xml.contains("]]&gt;"));
    assert!(xml.contains("&quot;quotes&quot;"));
}

#[tokio::test]
async fn atom_into_response_sets_content_type() {
    let resp = sample_feed(FeedFormat::Atom).into_response();
    let ct = resp.headers().get(CONTENT_TYPE).unwrap().to_str().unwrap();
    assert!(ct.contains("application/atom+xml"), "got {ct}");
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    assert!(!body.is_empty());
}

#[tokio::test]
async fn rss_into_response_sets_content_type() {
    let resp = sample_feed(FeedFormat::Rss).into_response();
    let ct = resp.headers().get(CONTENT_TYPE).unwrap().to_str().unwrap();
    assert!(ct.contains("application/rss+xml"), "got {ct}");
}

#[tokio::test]
async fn conditional_get_304_when_etag_matches() {
    let feed = sample_feed(FeedFormat::Atom);
    let etag = feed.etag();
    let mut headers = HeaderMap::new();
    headers.insert(IF_NONE_MATCH, etag.header_value());
    let resp = feed.conditional(&headers);
    assert_eq!(resp.status(), StatusCode::NOT_MODIFIED);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    assert!(body.is_empty(), "304 must have no body");
}

#[tokio::test]
async fn conditional_get_200_carries_etag_and_last_modified() {
    let feed = sample_feed(FeedFormat::Atom);
    let resp = feed.conditional(&HeaderMap::new());
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(resp.headers().get(ETAG).is_some(), "200 must carry ETag");
    assert!(
        resp.headers().get(LAST_MODIFIED).is_some(),
        "200 must carry Last-Modified"
    );
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    assert!(!body.is_empty());
}

#[tokio::test]
async fn conditional_get_200_when_etag_differs() {
    let feed = sample_feed(FeedFormat::Atom);
    let mut headers = HeaderMap::new();
    headers.insert(IF_NONE_MATCH, "\"stale-etag\"".parse().unwrap());
    let resp = feed.conditional(&headers);
    assert_eq!(resp.status(), StatusCode::OK);
}

#[test]
fn etag_changes_when_content_changes_within_same_second() {
    let ts = Utc.with_ymd_and_hms(2026, 5, 1, 12, 0, 0).unwrap();
    let mk = |title: &str| {
        Feed::atom(
            "Blog",
            "https://example.com/",
            "https://example.com/feed.xml",
        )
        .entry(
            FeedEntry::new("https://example.com/x", title, "https://example.com/x")
                .summary("body")
                .published(ts)
                .updated(ts),
        )
    };
    let a = mk("Original title");
    let b = mk("Edited title");
    assert_eq!(a.last_updated(), b.last_updated()); // identical whole-second timestamp
    assert_ne!(
        a.etag().header_value(),
        b.etag().header_value(),
        "a content edit within the same second must change the ETag"
    );
}

#[test]
fn empty_feed_renders_deterministically() {
    let mk = || {
        Feed::atom(
            "Blog",
            "https://example.com/",
            "https://example.com/feed.xml",
        )
    };
    assert_eq!(
        mk().render(),
        mk().render(),
        "empty feed body must be deterministic"
    );
    assert_eq!(mk().etag().header_value(), mk().etag().header_value());
}
