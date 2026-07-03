//! Tests for the `modal` widget and `confirm_action` helper (issue #1233).
//!
//! Run with `cargo test --test widgets_modal`.

#![allow(clippy::must_use_candidate)]

#[cfg(feature = "maud")]
mod modal_tests {
    use autumn_web::widgets::{HeadingLevel, ModalConfig, modal};
    use maud::html;

    // ── modal: structure ───────────────────────────────────────────────

    #[test]
    fn modal_renders_native_dialog_element() {
        let body = html! { p { "Body content" } };
        let html = modal("my-modal", "My Title", &body, &ModalConfig::new()).into_string();
        assert!(html.contains("<dialog"), "{html}");
        assert!(html.contains(r#"id="my-modal""#), "{html}");
    }

    #[test]
    fn modal_has_autumn_modal_class() {
        let body = html! { p { "x" } };
        let html = modal("m", "Title", &body, &ModalConfig::new()).into_string();
        assert!(html.contains(r#"class="autumn-modal""#), "{html}");
    }

    #[test]
    fn modal_extra_class_merged() {
        let body = html! { p { "x" } };
        let config = ModalConfig::new().class("wide");
        let html = modal("m", "Title", &body, &config).into_string();
        assert!(html.contains(r#"class="autumn-modal wide""#), "{html}");
    }

    #[test]
    fn modal_body_content_present() {
        let body = html! { p { "Unique body marker" } };
        let html = modal("m", "Title", &body, &ModalConfig::new()).into_string();
        assert!(html.contains("Unique body marker"), "{html}");
        assert!(html.contains(r#"class="autumn-modal__body""#), "{html}");
    }

    #[test]
    fn modal_title_escaped_and_rendered() {
        let body = html! { p { "x" } };
        let html =
            modal("m", "<script>alert(1)</script>", &body, &ModalConfig::new()).into_string();
        assert!(html.contains("&lt;script&gt;"), "{html}");
        assert!(!html.contains("<script>alert"), "{html}");
    }

    // ── modal: accessibility ───────────────────────────────────────────

    #[test]
    fn modal_has_aria_modal_true() {
        let body = html! { p { "x" } };
        let html = modal("m", "Title", &body, &ModalConfig::new()).into_string();
        assert!(html.contains(r#"aria-modal="true""#), "{html}");
    }

    #[test]
    fn modal_has_role_dialog() {
        let body = html! { p { "x" } };
        let html = modal("m", "Title", &body, &ModalConfig::new()).into_string();
        assert!(html.contains(r#"role="dialog""#), "{html}");
    }

    #[test]
    fn modal_aria_labelledby_points_at_title_id() {
        let body = html! { p { "x" } };
        let html = modal(
            "checkout-modal",
            "Confirm order",
            &body,
            &ModalConfig::new(),
        )
        .into_string();
        assert!(
            html.contains(r#"aria-labelledby="checkout-modal-title""#),
            "{html}"
        );
        assert!(html.contains(r#"id="checkout-modal-title""#), "{html}");
    }

    #[test]
    fn modal_title_defaults_to_h2() {
        let body = html! { p { "x" } };
        let html = modal("m", "Title", &body, &ModalConfig::new()).into_string();
        assert!(html.contains("<h2"), "{html}");
        assert!(html.contains(r#"class="autumn-modal__title""#), "{html}");
    }

    #[test]
    fn modal_configurable_heading_level() {
        let body = html! { p { "x" } };
        let config = ModalConfig::new().level(HeadingLevel::H1);
        let html = modal("m", "Title", &body, &config).into_string();
        assert!(html.contains("<h1"), "{html}");
        assert!(!html.contains("<h2"), "{html}");
    }

    // ── modal: footer ───────────────────────────────────────────────────

    #[test]
    fn modal_no_footer_by_default() {
        let body = html! { p { "x" } };
        let html = modal("m", "Title", &body, &ModalConfig::new()).into_string();
        assert!(!html.contains("autumn-modal__footer"), "{html}");
    }

    #[test]
    fn modal_renders_footer_when_configured() {
        let body = html! { p { "x" } };
        let footer = html! { button { "Close" } };
        let config = ModalConfig::new().footer(footer);
        let html = modal("m", "Title", &body, &config).into_string();
        assert!(html.contains(r#"class="autumn-modal__footer""#), "{html}");
        assert!(html.contains("Close"), "{html}");
    }

    // ── modal: light dismiss ────────────────────────────────────────────

    #[test]
    fn modal_no_closedby_attribute_by_default() {
        let body = html! { p { "x" } };
        let html = modal("m", "Title", &body, &ModalConfig::new()).into_string();
        assert!(!html.contains("closedby"), "{html}");
    }

    #[test]
    fn modal_light_dismiss_emits_closedby_any() {
        let body = html! { p { "x" } };
        let config = ModalConfig::new().light_dismiss(true);
        let html = modal("m", "Title", &body, &config).into_string();
        assert!(html.contains(r#"closedby="any""#), "{html}");
    }
}

#[cfg(feature = "maud")]
mod modal_trigger_tests {
    use autumn_web::widgets::{modal_close_button, modal_trigger};

    #[test]
    fn trigger_is_type_button_not_submit() {
        let html = modal_trigger("Open", "my-dialog", None).into_string();
        assert!(html.contains(r#"type="button""#), "{html}");
    }

    #[test]
    fn trigger_uses_native_invoker_commands() {
        let html = modal_trigger("Open", "my-dialog", None).into_string();
        assert!(html.contains(r#"command="show-modal""#), "{html}");
        assert!(html.contains(r#"commandfor="my-dialog""#), "{html}");
    }

    #[test]
    fn trigger_has_fallback_data_attribute() {
        let html = modal_trigger("Open", "my-dialog", None).into_string();
        assert!(html.contains(r#"data-modal-open="my-dialog""#), "{html}");
    }

    #[test]
    fn trigger_class_applied_when_given() {
        let html = modal_trigger("Open", "my-dialog", Some("btn btn-primary")).into_string();
        assert!(html.contains(r#"class="btn btn-primary""#), "{html}");
    }

    #[test]
    fn trigger_no_class_attribute_when_none() {
        let html = modal_trigger("Open", "my-dialog", None).into_string();
        assert!(!html.contains("class="), "{html}");
    }

    #[test]
    fn close_button_uses_native_invoker_close_command() {
        let html = modal_close_button("Cancel", "my-dialog", None).into_string();
        assert!(html.contains(r#"command="close""#), "{html}");
        assert!(html.contains(r#"commandfor="my-dialog""#), "{html}");
    }

    #[test]
    fn close_button_has_fallback_data_attribute() {
        let html = modal_close_button("Cancel", "my-dialog", None).into_string();
        assert!(html.contains(r#"data-modal-close="my-dialog""#), "{html}");
    }

    #[test]
    fn close_button_is_type_button() {
        let html = modal_close_button("Cancel", "my-dialog", None).into_string();
        assert!(html.contains(r#"type="button""#), "{html}");
    }
}

#[cfg(feature = "maud")]
mod confirm_action_tests {
    use autumn_web::widgets::{ConfirmActionConfig, confirm_action};
    use http::Method;

    #[test]
    fn renders_trigger_button_and_dialog() {
        let html = confirm_action(
            "delete-post-1",
            "Delete",
            "/posts/1",
            Method::DELETE,
            "tok123",
            &ConfirmActionConfig::new(),
        )
        .into_string();
        assert!(html.contains("<dialog"), "{html}");
        assert!(html.contains(r#"id="delete-post-1""#), "{html}");
        assert!(html.contains(r#"command="show-modal""#), "{html}");
    }

    #[test]
    fn default_title_is_are_you_sure() {
        let html = confirm_action(
            "d1",
            "Delete",
            "/posts/1",
            Method::DELETE,
            "tok",
            &ConfirmActionConfig::new(),
        )
        .into_string();
        assert!(html.contains("Are you sure?"), "{html}");
    }

    #[test]
    fn custom_title_and_message() {
        let config = ConfirmActionConfig::new()
            .title("Delete this post?")
            .message(maud::html! { p { "This cannot be undone." } });
        let html = confirm_action("d1", "Delete", "/posts/1", Method::DELETE, "tok", &config)
            .into_string();
        assert!(html.contains("Delete this post?"), "{html}");
        assert!(html.contains("This cannot be undone."), "{html}");
    }

    #[test]
    fn confirm_form_action_and_method() {
        let html = confirm_action(
            "d1",
            "Delete",
            "/posts/42",
            Method::DELETE,
            "tok123",
            &ConfirmActionConfig::new(),
        )
        .into_string();
        assert!(
            html.contains(r#"<form action="/posts/42" method="post""#),
            "{html}"
        );
        assert!(html.contains(r#"name="_method" value="DELETE""#), "{html}");
    }

    #[test]
    fn confirm_form_carries_csrf_token() {
        let html = confirm_action(
            "d1",
            "Delete",
            "/posts/42",
            Method::DELETE,
            "secret-tok",
            &ConfirmActionConfig::new(),
        )
        .into_string();
        assert!(
            html.contains(r#"name="_csrf" value="secret-tok""#),
            "{html}"
        );
    }

    #[test]
    fn confirm_form_custom_csrf_field() {
        let config = ConfirmActionConfig::new().csrf_field("csrf_tok");
        let html = confirm_action(
            "d1",
            "Delete",
            "/posts/42",
            Method::DELETE,
            "secret-tok",
            &config,
        )
        .into_string();
        assert!(
            html.contains(r#"name="csrf_tok" value="secret-tok""#),
            "{html}"
        );
        assert!(!html.contains(r#"name="_csrf""#), "{html}");
    }

    #[test]
    fn post_method_emits_no_method_override_field() {
        let html = confirm_action(
            "d1",
            "Archive",
            "/posts/42/archive",
            Method::POST,
            "tok",
            &ConfirmActionConfig::new(),
        )
        .into_string();
        assert!(!html.contains("_method"), "{html}");
    }

    #[test]
    fn confirm_button_has_danger_class_by_default() {
        let html = confirm_action(
            "d1",
            "Delete",
            "/posts/42",
            Method::DELETE,
            "tok",
            &ConfirmActionConfig::new(),
        )
        .into_string();
        assert!(html.contains("autumn-modal__confirm--danger"), "{html}");
    }

    #[test]
    fn confirm_button_danger_class_can_be_disabled() {
        let config = ConfirmActionConfig::new().danger(false);
        let html = confirm_action(
            "a1",
            "Archive",
            "/posts/1/archive",
            Method::POST,
            "tok",
            &config,
        )
        .into_string();
        assert!(!html.contains("autumn-modal__confirm--danger"), "{html}");
    }

    #[test]
    fn confirm_label_defaults_to_trigger_label() {
        let html = confirm_action(
            "d1",
            "Delete post",
            "/posts/1",
            Method::DELETE,
            "tok",
            &ConfirmActionConfig::new(),
        )
        .into_string();
        // Both the trigger button and the confirm submit button read "Delete post".
        assert_eq!(html.matches("Delete post").count(), 2, "{html}");
    }

    #[test]
    fn confirm_label_overridable() {
        let config = ConfirmActionConfig::new().confirm_label("Yes, delete it");
        let html = confirm_action("d1", "Delete", "/posts/1", Method::DELETE, "tok", &config)
            .into_string();
        assert!(html.contains("Yes, delete it"), "{html}");
    }

    #[test]
    fn cancel_button_present_and_closes_dialog() {
        let html = confirm_action(
            "d1",
            "Delete",
            "/posts/1",
            Method::DELETE,
            "tok",
            &ConfirmActionConfig::new(),
        )
        .into_string();
        assert!(html.contains("Cancel"), "{html}");
        assert!(html.contains(r#"command="close""#), "{html}");
        assert!(html.contains(&format!(r#"commandfor="d1""#)), "{html}");
    }

    #[test]
    fn cancel_label_overridable() {
        let config = ConfirmActionConfig::new().cancel_label("Never mind");
        let html = confirm_action("d1", "Delete", "/posts/1", Method::DELETE, "tok", &config)
            .into_string();
        assert!(html.contains("Never mind"), "{html}");
    }

    #[test]
    fn trigger_and_confirm_class_configurable() {
        let config = ConfirmActionConfig::new()
            .trigger_class("btn btn-danger")
            .confirm_class("btn btn-danger");
        let html = confirm_action("d1", "Delete", "/posts/1", Method::DELETE, "tok", &config)
            .into_string();
        // trigger_class on the opener button, confirm_class merged onto the submit button
        assert!(html.contains(r#"class="btn btn-danger""#), "{html}");
        assert!(html.contains("autumn-modal__confirm"), "{html}");
    }

    #[test]
    fn extra_attrs_passthrough_to_confirm_button() {
        let config = ConfirmActionConfig::new().attrs(&[("data-testid", "confirm-delete")]);
        let html = confirm_action("d1", "Delete", "/posts/1", Method::DELETE, "tok", &config)
            .into_string();
        assert!(html.contains(r#"data-testid="confirm-delete""#), "{html}");
    }

    #[test]
    fn no_hx_confirm_or_window_confirm_in_output() {
        // The whole point of this widget: no native window.confirm()/hx-confirm
        // fallback is needed — the dialog markup itself is the confirm UI.
        let html = confirm_action(
            "d1",
            "Delete",
            "/posts/1",
            Method::DELETE,
            "tok",
            &ConfirmActionConfig::new(),
        )
        .into_string();
        assert!(!html.contains("hx-confirm"), "{html}");
        assert!(!html.contains("window.confirm"), "{html}");
    }
}

// ── TestClient integration: markup fully present in the response body ─────

#[cfg(feature = "maud")]
mod test_client_integration {
    use autumn_web::prelude::*;
    use autumn_web::test::TestApp;
    use autumn_web::widgets::{ConfirmActionConfig, confirm_action};
    use http::Method;

    #[get("/posts/{id}")]
    async fn show_post(Path(id): Path<i64>) -> Markup {
        let action = format!("/posts/{id}");
        let config = ConfirmActionConfig::new()
            .title("Delete this post?")
            .trigger_class("btn btn-danger")
            .confirm_class("btn btn-danger");
        html! {
            h1 { "Post " (id) }
            (confirm_action(
                "delete-post",
                "Delete",
                &action,
                Method::DELETE,
                "test-csrf-token",
                &config,
            ))
        }
    }

    #[tokio::test]
    async fn test_client_can_assert_confirm_dialog_title_and_form() {
        let client = TestApp::new().routes(routes![show_post]).build();

        let resp = client.get("/posts/42").send().await;
        resp.assert_ok()
            .assert_selector("dialog#delete-post")
            .assert_attr("dialog#delete-post", "aria-modal", "true")
            .assert_attr("dialog#delete-post", "role", "dialog")
            .assert_text("#delete-post-title", "Delete this post?")
            .assert_attr("form.autumn-modal__confirm-form", "action", "/posts/42")
            .assert_attr("form.autumn-modal__confirm-form", "method", "post")
            .assert_attr(
                r#"form.autumn-modal__confirm-form input[name="_method"]"#,
                "value",
                "DELETE",
            )
            .assert_attr(
                r#"form.autumn-modal__confirm-form input[name="_csrf"]"#,
                "value",
                "test-csrf-token",
            );
    }
}
