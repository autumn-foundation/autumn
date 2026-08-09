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
fn marks_the_down_button_active_when_current_is_negative() {
    let html = reaction_controls(
        &ReactionControls::votes("votes-42", "/posts/42/upvote", "/posts/42/downvote")
            .aggregate(-1)
            .current(Some(-1)),
    )
    .into_string();

    // The active *class* (not just the aria-pressed ordering) must land on the
    // down button: styling keys off it, and a swapped branch would still put
    // the aria attributes in the right order while highlighting the wrong side.
    let down_form = html
        .split(r#"class="autumn-reaction autumn-reaction-down""#)
        .nth(1)
        .expect("a down form");
    assert!(
        down_form.contains("autumn-reaction-button autumn-reaction-active"),
        "the down button carries the active class: {html}"
    );
    assert_eq!(
        html.matches("autumn-reaction-active").count(),
        1,
        "exactly one direction is highlighted: {html}"
    );
    // The up button is the plain, unpressed one.
    let up_form = html
        .split(r#"class="autumn-reaction autumn-reaction-up""#)
        .nth(1)
        .expect("an up form");
    let up_button = up_form.split("</form>").next().expect("the up button");
    assert!(
        !up_button.contains("autumn-reaction-active"),
        "the up button is not highlighted: {html}"
    );
}

#[test]
fn marks_the_like_button_pressed_and_active_in_like_mode() {
    let html = reaction_controls(
        &ReactionControls::likes("likes-42", "/posts/42/like")
            .aggregate(3)
            .current(Some(1)),
    )
    .into_string();

    assert!(
        html.contains(r#"aria-pressed="true""#),
        "the viewer's like is a pressed toggle: {html}"
    );
    assert!(
        html.contains("autumn-reaction-button autumn-reaction-active"),
        "the like button carries the active class: {html}"
    );
    assert!(!html.contains(r#"aria-pressed="false""#), "{html}");
}

#[test]
fn like_mode_ignores_a_negative_current() {
    // Documented behavior: `current` is `{None, Some(1), Some(-1)}`, and count
    // mode has no down direction — a `Some(-1)` (which `reaction_of()` never
    // returns in count mode) presses nothing rather than pressing the like.
    let html = reaction_controls(
        &ReactionControls::likes("likes-42", "/posts/42/like")
            .aggregate(3)
            .current(Some(-1)),
    )
    .into_string();

    assert!(!html.contains(r#"aria-pressed="true""#), "{html}");
    assert_eq!(
        html.matches(r#"aria-pressed="false""#).count(),
        1,
        "the single like button renders as an unpressed toggle: {html}"
    );
    assert!(
        !html.contains("autumn-reaction-active"),
        "nothing is highlighted: {html}"
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
    // Both forms share one `replace`-strategy sync scope, so a second click
    // (up then down before the first response returns) aborts the in-flight
    // request and only the LAST click's response repaints the control — an
    // older response can never land second and press the stale direction
    // (PR #2177 review).
    assert_eq!(
        html.matches(r##"hx-sync="#votes-42:replace""##).count(),
        2,
        "both forms must serialize through the control's sync scope: {html}"
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

    // The markup half of the no-JS path: real forms, real submit buttons, no
    // inline JS or styles. This does not on its own prove the POST *succeeds*
    // with JavaScript off — in a CSRF-protected app the plain form also needs
    // the hidden token (`threads_the_csrf_hidden_input`); without it only the
    // htmx path works, via the header shim.
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

#[test]
fn buttons_carry_stable_ids_derived_from_dom_id() {
    // The ids are what `hx-preserve` matches on (see
    // `preserve_pressed_state_marks_the_buttons`), so they must be stable and
    // derived from the caller's `dom_id`, per direction and per mode.
    let votes = reaction_controls(
        &ReactionControls::votes("votes-42", "/posts/42/upvote", "/posts/42/downvote").aggregate(0),
    )
    .into_string();
    assert!(votes.contains(r#"id="votes-42-up""#), "{votes}");
    assert!(votes.contains(r#"id="votes-42-down""#), "{votes}");

    let likes =
        reaction_controls(&ReactionControls::likes("likes-7", "/bookmarks/7/like").aggregate(0))
            .into_string();
    assert!(likes.contains(r#"id="likes-7-like""#), "{likes}");
    assert!(!likes.contains(r#"id="likes-7-up""#), "{likes}");
}

#[test]
fn preserve_pressed_state_marks_the_buttons() {
    // Broadcast fragments (SSE fan-out, `current` necessarily `None`) opt in
    // to `hx-preserve` so a shared card swap keeps each viewer's own live
    // button elements — pressed state included — while still refreshing the
    // aggregate and the rest of the card.
    let broadcast = reaction_controls(
        &ReactionControls::votes("votes-42", "/posts/42/upvote", "/posts/42/downvote")
            .aggregate(7)
            .preserve_pressed_state(true),
    )
    .into_string();
    assert_eq!(
        broadcast.matches(r#"hx-preserve="true""#).count(),
        2,
        "both buttons must be preserved on a broadcast fragment: {broadcast}"
    );

    // The direct vote response must NOT preserve: it exists to repaint the
    // pressed state it just computed, and `hx-preserve` would resurrect the
    // stale buttons instead.
    let direct = reaction_controls(
        &ReactionControls::votes("votes-42", "/posts/42/upvote", "/posts/42/downvote")
            .aggregate(7)
            .current(Some(1)),
    )
    .into_string();
    assert!(
        !direct.contains("hx-preserve"),
        "the default (direct-response) rendering must not preserve: {direct}"
    );
}
