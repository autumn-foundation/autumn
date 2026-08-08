//! Tests for the `reaction_controls` widget (issue #1362, AC5).
//!
//! The widget renders the no-JS reaction control that `#[votable]`'s
//! `react()` / `reaction_of()` feed: a `role="group"` container holding one
//! plain `<form method="post">` per direction, upgraded in place by htmx.
//!
//! Run with `cargo test --test integration_tests widgets_reaction_controls`.

#![cfg(feature = "maud")]

use autumn_web::widgets::{ReactionControls, reaction_controls};

// ── container ────────────────────────────────────────────────────────────────

#[test]
fn renders_container_id_and_group_role() {
    let html = reaction_controls(
        &ReactionControls::votes("votes-42", "/posts/42/upvote", "/posts/42/downvote")
            .aggregate(7)
            .label("Post score"),
    )
    .into_string();

    assert!(html.contains(r#"id="votes-42""#), "{html}");
    assert!(
        html.contains(r#"class="autumn-reaction-controls""#),
        "{html}"
    );
    assert!(html.contains(r#"role="group""#), "{html}");
    assert!(html.contains(r#"aria-label="Post score""#), "{html}");
}

#[test]
fn renders_the_aggregate() {
    let html = reaction_controls(
        &ReactionControls::votes("votes-42", "/posts/42/upvote", "/posts/42/downvote").aggregate(7),
    )
    .into_string();

    assert!(html.contains(r#"class="autumn-reaction-count""#), "{html}");
    assert!(html.contains(">7<"), "the aggregate is rendered: {html}");

    // Negative aggregates render verbatim (a downvoted post is not clamped).
    let negative = reaction_controls(
        &ReactionControls::votes("votes-7", "/posts/7/upvote", "/posts/7/downvote").aggregate(-3),
    )
    .into_string();
    assert!(negative.contains(">-3<"), "{negative}");
}

// ── aria-pressed toggle semantics ────────────────────────────────────────────

#[test]
fn marks_the_current_reaction_aria_pressed() {
    let up = reaction_controls(
        &ReactionControls::votes("votes-42", "/posts/42/upvote", "/posts/42/downvote")
            .aggregate(1)
            .current(Some(1)),
    )
    .into_string();
    assert!(
        up.contains(r#"aria-pressed="true""#),
        "the current direction is a pressed toggle: {up}"
    );
    assert!(
        up.contains(r#"aria-pressed="false""#),
        "the other direction is not pressed: {up}"
    );
    assert!(
        up.contains("autumn-reaction-active"),
        "the current direction carries the active class: {up}"
    );

    // The pressed button is the *up* one: it precedes the down button in
    // source order, so the first aria-pressed occurrence must be the true one.
    let first_true = up.find(r#"aria-pressed="true""#).expect("a pressed button");
    let first_false = up
        .find(r#"aria-pressed="false""#)
        .expect("an unpressed one");
    assert!(first_true < first_false, "up is the pressed button: {up}");

    // Downvoted: the roles swap.
    let down = reaction_controls(
        &ReactionControls::votes("votes-42", "/posts/42/upvote", "/posts/42/downvote")
            .aggregate(-1)
            .current(Some(-1)),
    )
    .into_string();
    let first_true = down
        .find(r#"aria-pressed="true""#)
        .expect("a pressed button");
    let first_false = down
        .find(r#"aria-pressed="false""#)
        .expect("an unpressed one");
    assert!(
        first_false < first_true,
        "down is the pressed button: {down}"
    );
}

#[test]
fn marks_no_button_pressed_when_current_is_none() {
    let html = reaction_controls(
        &ReactionControls::votes("votes-42", "/posts/42/upvote", "/posts/42/downvote")
            .aggregate(0)
            .current(None),
    )
    .into_string();

    assert!(!html.contains(r#"aria-pressed="true""#), "{html}");
    assert_eq!(
        html.matches(r#"aria-pressed="false""#).count(),
        2,
        "both buttons render as unpressed toggles: {html}"
    );
    assert!(
        !html.contains("autumn-reaction-active"),
        "no direction is highlighted: {html}"
    );
}

// ── modes ────────────────────────────────────────────────────────────────────

#[test]
fn renders_both_directions_in_vote_mode() {
    let html = reaction_controls(
        &ReactionControls::votes("votes-42", "/posts/42/upvote", "/posts/42/downvote").aggregate(0),
    )
    .into_string();

    assert!(html.contains("autumn-reaction-up"), "{html}");
    assert!(html.contains("autumn-reaction-down"), "{html}");
    assert!(html.contains(r#"action="/posts/42/upvote""#), "{html}");
    assert!(html.contains(r#"action="/posts/42/downvote""#), "{html}");
    assert_eq!(
        html.matches("<button").count(),
        2,
        "exactly two buttons in vote mode: {html}"
    );
    assert!(!html.contains("autumn-reaction-like"), "{html}");
}

#[test]
fn renders_one_button_in_like_mode() {
    let html =
        reaction_controls(&ReactionControls::likes("likes-42", "/posts/42/like").aggregate(3))
            .into_string();

    assert!(html.contains("autumn-reaction-like"), "{html}");
    assert!(html.contains(r#"action="/posts/42/like""#), "{html}");
    assert_eq!(
        html.matches("<button").count(),
        1,
        "exactly one button in like mode: {html}"
    );
    assert!(!html.contains("autumn-reaction-up"), "{html}");
    assert!(!html.contains("autumn-reaction-down"), "{html}");
    assert!(html.contains(">3<"), "the count is rendered: {html}");
}

// ── CSRF ─────────────────────────────────────────────────────────────────────

#[test]
fn threads_the_csrf_hidden_input() {
    let html = reaction_controls(
        &ReactionControls::votes("votes-42", "/posts/42/upvote", "/posts/42/downvote")
            .aggregate(0)
            .csrf_token("secret-token"),
    )
    .into_string();

    assert_eq!(
        html.matches(r#"<input type="hidden" name="_csrf" value="secret-token">"#)
            .count(),
        2,
        "every form carries the hidden CSRF input: {html}"
    );

    // A customised form-field name is honoured (the `CsrfFormField` a handler
    // threads through, defaulting to `_csrf`).
    let custom = reaction_controls(
        &ReactionControls::votes("votes-42", "/posts/42/upvote", "/posts/42/downvote")
            .aggregate(0)
            .csrf_token("secret-token")
            .csrf_field("csrf_custom"),
    )
    .into_string();
    assert!(
        custom.contains(r#"<input type="hidden" name="csrf_custom" value="secret-token">"#),
        "{custom}"
    );
    assert!(!custom.contains(r#"name="_csrf""#), "{custom}");
}

#[test]
fn omits_the_csrf_input_when_no_token() {
    let html = reaction_controls(
        &ReactionControls::votes("votes-42", "/posts/42/upvote", "/posts/42/downvote").aggregate(0),
    )
    .into_string();

    assert!(!html.contains("_csrf"), "{html}");
    assert!(!html.contains(r#"type="hidden""#), "{html}");

    let like =
        reaction_controls(&ReactionControls::likes("likes-42", "/posts/42/like")).into_string();
    assert!(!like.contains("_csrf"), "{like}");
}

// ── accessibility ────────────────────────────────────────────────────────────

#[test]
fn buttons_have_accessible_names_and_hidden_glyphs() {
    let html = reaction_controls(
        &ReactionControls::votes("votes-42", "/posts/42/upvote", "/posts/42/downvote").aggregate(0),
    )
    .into_string();

    // The glyph is decorative; the accessible name comes from the aria-label.
    assert!(html.contains(r#"aria-label="Upvote""#), "{html}");
    assert!(html.contains(r#"aria-label="Downvote""#), "{html}");
    assert_eq!(
        html.matches(r#"aria-hidden="true""#).count(),
        2,
        "each glyph is hidden from assistive tech: {html}"
    );

    // Labels are overridable.
    let custom = reaction_controls(
        &ReactionControls::votes("votes-42", "/posts/42/upvote", "/posts/42/downvote")
            .up_label("Boost")
            .down_label("Bury"),
    )
    .into_string();
    assert!(custom.contains(r#"aria-label="Boost""#), "{custom}");
    assert!(custom.contains(r#"aria-label="Bury""#), "{custom}");

    let like =
        reaction_controls(&ReactionControls::likes("likes-42", "/posts/42/like")).into_string();
    assert!(like.contains(r#"aria-label="Like""#), "{like}");
    let relabelled = reaction_controls(
        &ReactionControls::likes("likes-42", "/posts/42/like").like_label("Star"),
    )
    .into_string();
    assert!(relabelled.contains(r#"aria-label="Star""#), "{relabelled}");
}

// ── htmx + progressive enhancement ───────────────────────────────────────────

#[test]
fn emits_htmx_post_target_and_outer_html_swap() {
    let html = reaction_controls(
        &ReactionControls::votes("votes-42", "/posts/42/upvote", "/posts/42/downvote")
            .aggregate(0)
            .hx_target("#feed-42"),
    )
    .into_string();

    assert!(html.contains(r#"hx-post="/posts/42/upvote""#), "{html}");
    assert!(html.contains(r#"hx-post="/posts/42/downvote""#), "{html}");
    assert!(html.contains(r##"hx-target="#feed-42""##), "{html}");
    assert_eq!(
        html.matches(r#"hx-swap="outerHTML""#).count(),
        2,
        "each form swaps the control in place: {html}"
    );
}

#[test]
fn hx_target_defaults_to_the_container_id() {
    let html = reaction_controls(
        &ReactionControls::votes("votes-42", "/posts/42/upvote", "/posts/42/downvote").aggregate(0),
    )
    .into_string();

    assert!(
        html.contains(r##"hx-target="#votes-42""##),
        "the control replaces itself by default: {html}"
    );
}

#[test]
fn degrades_to_a_plain_post_form() {
    let html = reaction_controls(
        &ReactionControls::votes("votes-42", "/posts/42/upvote", "/posts/42/downvote").aggregate(0),
    )
    .into_string();

    // Works with JavaScript off: real forms, real submit buttons, no inline JS.
    assert_eq!(
        html.matches(r#"<form method="post""#).count(),
        2,
        "both controls are plain POST forms: {html}"
    );
    assert_eq!(
        html.matches(r#"type="submit""#).count(),
        2,
        "both buttons submit their form: {html}"
    );
    assert!(!html.contains("onclick"), "{html}");
    assert!(!html.contains("<script"), "{html}");
    assert!(!html.contains("style="), "no inline styles: {html}");
}
