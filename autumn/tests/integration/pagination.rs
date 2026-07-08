//! Integration tests for the pagination extractor and response wrapper.
//!
//! Verifies the full round-trip through Axum: query-string parsing,
//! clamping of out-of-range parameters, and JSON serialization of
//! [`Page`] responses.

use autumn_web::extract::Json;
use autumn_web::pagination::{
    Cursor, CursorPage, CursorRequest, DEFAULT_PAGE_SIZE, MAX_PAGE_SIZE, Page, PageRequest,
};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::get;
use serde::{Deserialize, Serialize};
use tower::ServiceExt;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct Item {
    id: i64,
    name: String,
}

fn seed(count: i64) -> Vec<Item> {
    (1..=count)
        .map(|id| Item {
            id,
            name: format!("item-{id}"),
        })
        .collect()
}

fn app() -> Router {
    async fn list(page: PageRequest) -> Json<Page<Item>> {
        // Simulate "select the current page from a 137-row table".
        let all = seed(137);
        let total = i64::try_from(all.len()).unwrap_or(i64::MAX);
        let start = usize::try_from(page.offset()).unwrap_or(0);
        let end = start.saturating_add(page.size() as usize).min(all.len());
        let items = if start < all.len() {
            all[start..end].to_vec()
        } else {
            Vec::new()
        };
        Json(Page::new(items, total, &page))
    }
    Router::new().route("/items", get(list))
}

async fn fetch_json(uri: &str) -> (StatusCode, serde_json::Value) {
    let res = app()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    (status, value)
}

#[tokio::test]
async fn first_page_reports_defaults_and_has_next() {
    let (status, body) = fetch_json("/items").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["page"], 1);
    assert_eq!(body["size"], DEFAULT_PAGE_SIZE);
    assert_eq!(body["total_elements"], 137);
    assert_eq!(body["total_pages"], 7);
    assert_eq!(body["has_previous"], false);
    assert_eq!(body["has_next"], true);
    assert_eq!(body["content"].as_array().unwrap().len(), 20);
    assert_eq!(body["content"][0]["id"], 1);
}

#[tokio::test]
async fn middle_page_has_next_and_previous() {
    let (status, body) = fetch_json("/items?page=3&size=20").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["page"], 3);
    assert_eq!(body["has_previous"], true);
    assert_eq!(body["has_next"], true);
    assert_eq!(body["content"][0]["id"], 41);
}

#[tokio::test]
async fn last_page_is_partially_full() {
    let (status, body) = fetch_json("/items?page=7&size=20").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["page"], 7);
    assert_eq!(body["has_next"], false);
    assert_eq!(body["has_previous"], true);
    // 137 rows / 20 per page → last page has 17 items
    assert_eq!(body["content"].as_array().unwrap().len(), 17);
}

#[tokio::test]
async fn past_end_yields_empty_content() {
    let (status, body) = fetch_json("/items?page=99&size=20").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["content"].as_array().unwrap().len(), 0);
    assert_eq!(body["has_next"], false);
    assert_eq!(body["has_previous"], true);
}

#[tokio::test]
async fn size_exceeding_max_is_clamped() {
    let (status, body) = fetch_json("/items?page=1&size=500").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["size"], MAX_PAGE_SIZE);
    assert_eq!(
        body["content"].as_array().unwrap().len(),
        MAX_PAGE_SIZE as usize
    );
}

#[tokio::test]
async fn page_zero_is_coerced_to_first_page() {
    let (status, body) = fetch_json("/items?page=0&size=10").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["page"], 1);
    assert_eq!(body["content"][0]["id"], 1);
}

#[tokio::test]
async fn size_zero_falls_back_to_default() {
    let (status, body) = fetch_json("/items?page=1&size=0").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["size"], DEFAULT_PAGE_SIZE);
}

#[tokio::test]
async fn malformed_page_value_does_not_400() {
    // The contract is that a bad pager never breaks the endpoint —
    // unparseable values fall back to defaults.
    let (status, body) = fetch_json("/items?page=abc&size=xyz").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["page"], 1);
    assert_eq!(body["size"], DEFAULT_PAGE_SIZE);
}

// ── Cursor pagination ──────────────────────────────────────────────

/// Sort key encoded inside cursor tokens. `created_at` is the primary
/// sort, `id` is the unique tie-breaker required for zero-duplicate
/// guarantees under concurrent inserts.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ItemCursor {
    created_at: i64,
    id: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct FeedItem {
    id: i64,
    created_at: i64,
    name: String,
}

fn seed_feed(count: i64) -> Vec<FeedItem> {
    // Newest first: created_at descends with id (older = larger id).
    (1..=count)
        .map(|id| FeedItem {
            id,
            // Stagger so the boundary case where two items share a
            // timestamp is exercised: ids 10..=11 share created_at.
            created_at: if id == 11 { 90 } else { 100 - id },
            name: format!("item-{id}"),
        })
        .collect()
}

fn cursor_app() -> Router {
    async fn feed(req: CursorRequest) -> Json<CursorPage<FeedItem>> {
        // Sort newest-first with id as tie-breaker.
        let mut sorted = seed_feed(25);
        sorted.sort_by_key(|i| std::cmp::Reverse((i.created_at, i.id)));

        // Apply keyset filter (this is what the SQL query does in
        // production: `WHERE (created_at, id) < (?, ?)`).
        let cursor = req.decode::<ItemCursor>();
        let after_cursor = sorted.into_iter().filter(move |i| {
            cursor.as_ref().is_none_or(|c| {
                i.created_at < c.created_at || (i.created_at == c.created_at && i.id < c.id)
            })
        });

        // Overfetch by 1 so CursorPage can detect has_next.
        let fetch_limit = usize::try_from(req.fetch_limit()).unwrap_or(0);
        let fetched: Vec<FeedItem> = after_cursor.take(fetch_limit).collect();

        Json(CursorPage::from_overfetched(fetched, &req, |i| {
            ItemCursor {
                created_at: i.created_at,
                id: i.id,
            }
        }))
    }
    Router::new().route("/feed", get(feed))
}

async fn fetch_cursor_json(uri: &str) -> (StatusCode, serde_json::Value) {
    let res = cursor_app()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    (status, value)
}

#[tokio::test]
async fn cursor_first_page_returns_size_items_and_a_next_cursor() {
    let (status, body) = fetch_cursor_json("/feed?size=5").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["size"], 5);
    assert_eq!(body["has_next"], true);
    assert!(body["next_cursor"].is_string());
    assert_eq!(body["content"].as_array().unwrap().len(), 5);
}

#[tokio::test]
async fn cursor_last_page_has_no_next_cursor() {
    // 25 items, size=20 → first page has 20, second page has 5 with no next.
    let (_, page1) = fetch_cursor_json("/feed?size=20").await;
    let cursor = page1["next_cursor"].as_str().unwrap();

    let (status, page2) = fetch_cursor_json(&format!("/feed?size=20&cursor={cursor}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page2["has_next"], false);
    assert!(page2["next_cursor"].is_null());
    assert_eq!(page2["content"].as_array().unwrap().len(), 5);
}

#[tokio::test]
async fn cursor_pages_cover_every_item_with_no_duplicates() {
    let mut seen_ids: Vec<i64> = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..10 {
        // safety bound — 25/3 = 9 pages max
        let uri = cursor.as_deref().map_or_else(
            || "/feed?size=3".to_string(),
            |c| format!("/feed?size=3&cursor={c}"),
        );
        let (status, body) = fetch_cursor_json(&uri).await;
        assert_eq!(status, StatusCode::OK);
        for item in body["content"].as_array().unwrap() {
            seen_ids.push(item["id"].as_i64().unwrap());
        }
        if body["has_next"] == false {
            break;
        }
        cursor = Some(body["next_cursor"].as_str().unwrap().to_string());
    }
    let mut sorted = seen_ids.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), seen_ids.len(), "no duplicates");
    assert_eq!(seen_ids.len(), 25, "every item visited exactly once");
}

#[tokio::test]
async fn cursor_size_is_clamped_to_max() {
    let (status, body) = fetch_cursor_json("/feed?size=9999").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["size"], MAX_PAGE_SIZE);
}

#[tokio::test]
async fn cursor_size_zero_falls_back_to_default() {
    let (status, body) = fetch_cursor_json("/feed?size=0").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["size"], DEFAULT_PAGE_SIZE);
}

#[tokio::test]
async fn cursor_malformed_token_falls_back_to_first_page() {
    // Tampered/garbage token: should not 400; should be treated as no cursor.
    let (status, body) = fetch_cursor_json("/feed?cursor=NOT-A-VALID-TOKEN&size=5").await;
    assert_eq!(status, StatusCode::OK);
    // First page → newest item (id=1) at the head.
    assert_eq!(body["content"][0]["id"], 1);
}

#[tokio::test]
async fn cursor_tie_breaker_handles_duplicate_timestamps() {
    // Items 10 and 11 share created_at=90 in the seed. Walking the
    // pages must still yield each row exactly once even when the
    // primary sort column is non-unique.
    let mut seen = Vec::<i64>::new();
    let mut cursor: Option<String> = None;
    loop {
        let uri = cursor.as_deref().map_or_else(
            || "/feed?size=2".to_string(),
            |c| format!("/feed?size=2&cursor={c}"),
        );
        let (_, body) = fetch_cursor_json(&uri).await;
        for it in body["content"].as_array().unwrap() {
            seen.push(it["id"].as_i64().unwrap());
        }
        if body["has_next"] == false {
            break;
        }
        cursor = Some(body["next_cursor"].as_str().unwrap().to_string());
    }
    // Both id=10 and id=11 must appear, exactly once.
    assert!(seen.contains(&10));
    assert!(seen.contains(&11));
    let mut deduped = seen.clone();
    deduped.sort_unstable();
    deduped.dedup();
    assert_eq!(deduped.len(), seen.len());
}

#[tokio::test]
async fn cursor_token_is_url_safe() {
    // The `next_cursor` must round-trip through the URL without any
    // percent-encoding gymnastics from the client.
    let (_, body) = fetch_cursor_json("/feed?size=3").await;
    let token = body["next_cursor"].as_str().unwrap();
    assert!(
        token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "cursor must be URL-safe, got {token}"
    );

    // And the framework's own decoder reads it back.
    let key: ItemCursor = Cursor::decode(token).unwrap();
    // Page size 3 → boundary item is id=3.
    assert_eq!(key.id, 3);
}
