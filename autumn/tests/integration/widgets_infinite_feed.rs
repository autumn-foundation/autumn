//! Tests for the `infinite_feed` / `feed_page` widgets (issue #1372).
//!
//! Run with `cargo test --test integration_tests widgets_infinite_feed`.

#![cfg(feature = "maud")]

use autumn_web::pagination::CursorPage;
use autumn_web::widgets::{FeedConfig, FeedMode, feed_page, infinite_feed};
use maud::html;

fn items(rows: &[&str]) -> maud::Markup {
    html! { @for r in rows { article class="post" { (r) } } }
}

// ── page 1: container + cursored sentinel ─────────────────────────────────────

#[test]
fn page_one_emits_feed_container_and_cursored_sentinel() {
    let page = CursorPage {
        content: vec!["a", "b"],
        size: 2,
        next_cursor: Some("eyJpZCI6Mn0".into()),
        has_next: true,
    };
    let config = FeedConfig::new("/posts/feed");
    let out =
        infinite_feed(items(&page.content), page.next_cursor.as_deref(), &config).into_string();

    assert!(out.contains(r#"class="autumn-feed""#), "{out}");
    // No `role="feed"`: the ARIA feed contract (article children with
    // aria-posinset/setsize, aria-busy) isn't implemented and the sentinel is a
    // non-article child, so the role would be a false claim.
    assert!(!out.contains(r#"role="feed""#), "{out}");
    assert!(out.contains("autumn-feed__sentinel"), "{out}");
    // The single hx-get carries the cursor as a query param.
    assert!(
        out.contains(r#"hx-get="/posts/feed?cursor=eyJpZCI6Mn0""#),
        "{out}"
    );
    // Rendered items are present, once each (no duplication).
    assert_eq!(
        out.matches("article").count(),
        4,
        "open+close tags for 2 items: {out}"
    );
}

#[test]
fn reveal_mode_auto_loads_on_reveal() {
    let config = FeedConfig::new("/feed").mode(FeedMode::Reveal);
    let out = feed_page(items(&["a"]), Some("cur1"), &config).into_string();
    assert!(out.contains(r#"hx-trigger="revealed""#), "{out}");
}

#[test]
fn button_mode_has_no_reveal_trigger_but_still_loads() {
    let config = FeedConfig::new("/feed").button();
    let out = feed_page(items(&["a"]), Some("cur1"), &config).into_string();
    assert!(
        !out.contains("revealed"),
        "button mode must not auto-load: {out}"
    );
    // Still wired to fetch the next page on the default (click) trigger.
    assert!(out.contains(r#"hx-get="/feed?cursor=cur1""#), "{out}");
    assert!(out.contains("autumn-feed__more"), "{out}");
}

// ── progressive enhancement ───────────────────────────────────────────────────

#[test]
fn continuation_control_is_a_real_anchor_href() {
    let config = FeedConfig::new("/feed");
    let out = feed_page(items(&["a"]), Some("cur1"), &config).into_string();
    // A real <a href> to the next-cursor URL — reachable without htmx/JS.
    assert!(out.contains(r#"href="/feed?cursor=cur1""#), "{out}");
    assert!(out.contains("<a "), "{out}");
}

#[test]
fn custom_cursor_param_and_label_are_honored() {
    let config = FeedConfig::new("/feed")
        .cursor_param("after")
        .load_more_label("Show more posts");
    let out = feed_page(items(&["a"]), Some("cur1"), &config).into_string();
    assert!(out.contains(r#"hx-get="/feed?after=cur1""#), "{out}");
    assert!(out.contains("Show more posts"), "{out}");
}

#[test]
fn existing_query_string_uses_ampersand_separator() {
    let config = FeedConfig::new("/feed?tag=rust");
    let out = feed_page(items(&["a"]), Some("cur1"), &config).into_string();
    // The `&` separator is HTML-escaped to `&amp;` in the attribute (correct
    // HTML — the browser decodes it back to `&` when issuing the request).
    assert!(
        out.contains(r#"hx-get="/feed?tag=rust&amp;cursor=cur1""#),
        "{out}"
    );
}

#[test]
fn cursor_with_reserved_chars_is_percent_encoded() {
    let config = FeedConfig::new("/feed");
    // A cursor with reserved chars would corrupt the query / inject params if
    // spliced raw; it must be percent-encoded.
    let out = feed_page(items(&["a"]), Some("a b&x=1#z"), &config).into_string();
    assert!(out.contains("cursor=a%20b%26x%3D1%23z"), "{out}");
    // No raw reserved chars from the cursor leak into the query string.
    assert!(!out.contains("cursor=a b"), "{out}");
    assert!(!out.contains("x=1#z"), "{out}");
}

#[test]
fn custom_param_name_is_also_percent_encoded() {
    let config = FeedConfig::new("/feed").cursor_param("a b");
    let out = feed_page(items(&["a"]), Some("cur1"), &config).into_string();
    assert!(out.contains("?a%20b=cur1"), "{out}");
}

#[test]
fn fragment_url_keeps_query_before_the_fragment() {
    let config = FeedConfig::new("/feed#top");
    let out = feed_page(items(&["a"]), Some("cur1"), &config).into_string();
    // The query must precede the `#fragment`, not follow it.
    assert!(out.contains(r#"hx-get="/feed?cursor=cur1#top""#), "{out}");
}

#[test]
fn fragment_url_with_existing_query_uses_ampersand_before_fragment() {
    let config = FeedConfig::new("/feed?tag=rust#top");
    let out = feed_page(items(&["a"]), Some("cur1"), &config).into_string();
    assert!(
        out.contains(r#"hx-get="/feed?tag=rust&amp;cursor=cur1#top""#),
        "{out}"
    );
}

// ── append fragment slice ─────────────────────────────────────────────────────

#[test]
fn append_fragment_returns_the_next_slice_with_new_sentinel() {
    let config = FeedConfig::new("/feed");
    // A middle page: items plus a fresh sentinel carrying the *next* cursor.
    let out = feed_page(items(&["c", "d"]), Some("cur2"), &config).into_string();
    assert!(out.contains(">c<"), "{out}");
    assert!(out.contains(">d<"), "{out}");
    assert!(out.contains("autumn-feed__sentinel"), "{out}");
    assert!(out.contains(r#"hx-get="/feed?cursor=cur2""#), "{out}");
    // The append fragment is NOT wrapped in another feed container.
    assert!(!out.contains(r#"class="autumn-feed""#), "{out}");
}

// ── last page: no sentinel, loop terminates ───────────────────────────────────

#[test]
fn last_page_emits_no_sentinel() {
    let page = CursorPage::<&str> {
        content: vec!["z"],
        size: 2,
        next_cursor: None,
        has_next: false,
    };
    let config = FeedConfig::new("/feed");
    let out =
        infinite_feed(items(&page.content), page.next_cursor.as_deref(), &config).into_string();
    assert!(out.contains(r#"class="autumn-feed""#), "{out}");
    assert!(
        !out.contains("autumn-feed__sentinel"),
        "no sentinel on last page: {out}"
    );
    assert!(
        !out.contains("hx-get"),
        "no further request on last page: {out}"
    );
}

#[test]
fn last_page_append_fragment_emits_no_sentinel() {
    let config = FeedConfig::new("/feed");
    let out = feed_page(items(&["z"]), None, &config).into_string();
    assert!(!out.contains("autumn-feed__sentinel"), "{out}");
    assert!(!out.contains("hx-get"), "{out}");
}

// ── styling / no inline style ─────────────────────────────────────────────────

#[test]
fn feed_emits_no_inline_style() {
    let config = FeedConfig::new("/feed");
    let out = infinite_feed(items(&["a"]), Some("cur1"), &config).into_string();
    assert!(!out.contains("style="), "no inline styles: {out}");
}

// ── security / XSS ────────────────────────────────────────────────────────────

#[test]
fn url_and_cursor_are_escaped() {
    let config = FeedConfig::new("/feed\"><script>alert(1)</script>");
    let out = feed_page(items(&["a"]), Some("\"><script>x</script>"), &config).into_string();
    assert!(!out.contains("<script>alert"), "{out}");
    assert!(!out.contains("<script>x"), "{out}");
    assert!(out.contains("&lt;script&gt;"), "{out}");
}

#[test]
fn no_script_tag_emitted_by_widget_itself() {
    let config = FeedConfig::new("/feed");
    let out = infinite_feed(items(&["a"]), Some("cur1"), &config).into_string();
    assert!(!out.contains("<script"), "{out}");
    assert!(!out.contains("onclick"), "{out}");
}
