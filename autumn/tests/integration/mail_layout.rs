#![cfg(feature = "mail")]

use autumn_web::mail::Mail;

// ── Phase A: composition primitive ───────────────────────────────────────────
//
// These tests drive the implementation of:
//   - `MAIL_LAYOUT_CONTENT_MARKER` constant
//   - `compose_layout(layout, body)` free function
//   - `MailBuilder::layout(html, txt)` builder method
//   - composition at `MailBuilder::build()` time

#[test]
fn layout_composes_html_and_text_into_slot() {
    let mail = Mail::builder()
        .to("user@example.com")
        .subject("Test")
        .html("<p>Hello body</p>")
        .text("Hello body")
        .layout(
            "<!DOCTYPE html><html><body>{{ content }}</body></html>",
            "Header\n{{ content }}\nFooter",
        )
        .build()
        .expect("valid mail");

    assert_eq!(
        mail.html.as_deref().unwrap(),
        "<!DOCTYPE html><html><body><p>Hello body</p></body></html>"
    );
    assert_eq!(mail.text.as_deref().unwrap(), "Header\nHello body\nFooter");
}

#[test]
fn no_layout_call_leaves_body_unchanged() {
    let raw_html = "<p>Raw body</p>";
    let raw_text = "Raw body";

    let mail = Mail::builder()
        .to("user@example.com")
        .subject("Test")
        .html(raw_html)
        .text(raw_text)
        .build()
        .expect("valid mail");

    assert_eq!(mail.html.as_deref().unwrap(), raw_html);
    assert_eq!(mail.text.as_deref().unwrap(), raw_text);
}

#[test]
fn layout_without_marker_returns_body_unchanged() {
    // A layout with no {{ content }} slot must not silently drop body —
    // fall back to body-only output.
    let mail = Mail::builder()
        .to("user@example.com")
        .subject("Test")
        .html("<p>Body</p>")
        .text("Body")
        .layout(
            "<!DOCTYPE html><html><body>no slot here</body></html>",
            "no slot here",
        )
        .build()
        .expect("valid mail");

    assert_eq!(mail.html.as_deref().unwrap(), "<p>Body</p>");
    assert_eq!(mail.text.as_deref().unwrap(), "Body");
}

#[test]
fn layout_with_html_only_body_composes_html() {
    let mail = Mail::builder()
        .to("user@example.com")
        .subject("Test")
        .html("<p>Only HTML</p>")
        .layout("<wrap>{{ content }}</wrap>", "text: {{ content }}")
        .build()
        .expect("valid mail");

    assert_eq!(
        mail.html.as_deref().unwrap(),
        "<wrap><p>Only HTML</p></wrap>"
    );
    assert!(mail.text.is_none());
}

#[test]
fn layout_with_text_only_body_composes_text() {
    let mail = Mail::builder()
        .to("user@example.com")
        .subject("Test")
        .text("only text")
        .layout("<wrap>{{ content }}</wrap>", "text: {{ content }}")
        .build()
        .expect("valid mail");

    assert_eq!(mail.text.as_deref().unwrap(), "text: only text");
    assert!(mail.html.is_none());
}

#[test]
fn compose_layout_replaces_marker_in_layout() {
    use autumn_web::mail::{MAIL_LAYOUT_CONTENT_MARKER, compose_layout};

    assert!(MAIL_LAYOUT_CONTENT_MARKER.contains("content"));

    let result = compose_layout("<header>{{ content }}</header>", "<p>Body</p>");
    assert_eq!(result, "<header><p>Body</p></header>");
}

#[test]
fn compose_layout_returns_body_when_marker_absent() {
    use autumn_web::mail::compose_layout;

    let result = compose_layout("<no-slot/>", "<p>Body</p>");
    assert_eq!(result, "<p>Body</p>");
}
