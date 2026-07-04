//! Tests for the `tabs` widget (issue #1316).
//!
//! Run with `cargo test --test widgets_tabs`.

#![allow(clippy::must_use_candidate)]

#[cfg(feature = "maud")]
mod tabs_tests {
    use autumn_web::widgets::tabs;
    use maud::html;

    // ── structure ───────────────────────────────────────────────────────

    #[test]
    fn renders_root_container_with_id_and_class() {
        let panels = [("overview", "Overview", html! { p { "a" } })];
        let html = tabs("post-tabs", &panels).into_string();
        assert!(html.contains(r#"id="post-tabs""#), "{html}");
        assert!(html.contains(r#"class="autumn-tabs""#), "{html}");
    }

    #[test]
    fn renders_tablist_role() {
        let panels = [("overview", "Overview", html! { p { "a" } })];
        let html = tabs("post-tabs", &panels).into_string();
        assert!(html.contains(r#"role="tablist""#), "{html}");
        assert!(html.contains("autumn-tabs__list"), "{html}");
    }

    #[test]
    fn renders_n_panels_and_n_tabs() {
        let panels = [
            ("overview", "Overview", html! { p { "Overview body" } }),
            ("comments", "Comments", html! { p { "Comments body" } }),
            ("activity", "Activity", html! { p { "Activity body" } }),
        ];
        let html = tabs("post-tabs", &panels).into_string();
        assert_eq!(html.matches(r#"role="tab""#).count(), 3, "{html}");
        assert_eq!(html.matches(r#"role="tabpanel""#).count(), 3, "{html}");
        assert!(html.contains("Overview"), "{html}");
        assert!(html.contains("Comments"), "{html}");
        assert!(html.contains("Activity"), "{html}");
        assert!(html.contains("Overview body"), "{html}");
        assert!(html.contains("Comments body"), "{html}");
        assert!(html.contains("Activity body"), "{html}");
    }

    #[test]
    fn tabs_use_semantic_class_names() {
        let panels = [("overview", "Overview", html! { p { "a" } })];
        let html = tabs("post-tabs", &panels).into_string();
        assert!(html.contains("autumn-tabs__tab"), "{html}");
        assert!(html.contains("autumn-tabs__panel"), "{html}");
    }

    // ── active state ────────────────────────────────────────────────────

    #[test]
    fn first_tab_is_selected_by_default() {
        let panels = [
            ("overview", "Overview", html! { p { "a" } }),
            ("comments", "Comments", html! { p { "b" } }),
        ];
        let html = tabs("post-tabs", &panels).into_string();
        assert_eq!(html.matches("aria-selected=\"true\"").count(), 1, "{html}");
        assert_eq!(html.matches("aria-selected=\"false\"").count(), 1, "{html}");
    }

    #[test]
    fn exactly_one_active_tab_and_one_active_panel() {
        let panels = [
            ("overview", "Overview", html! { p { "a" } }),
            ("comments", "Comments", html! { p { "b" } }),
            ("activity", "Activity", html! { p { "c" } }),
        ];
        let html = tabs("post-tabs", &panels).into_string();
        assert_eq!(
            html.matches("autumn-tabs__tab--active").count(),
            1,
            "{html}"
        );
        assert_eq!(
            html.matches("autumn-tabs__panel--active").count(),
            1,
            "{html}"
        );
    }

    #[test]
    fn first_panel_and_tab_carry_active_class() {
        let panels = [
            ("overview", "Overview", html! { p { "a" } }),
            ("comments", "Comments", html! { p { "b" } }),
        ];
        let html = tabs("post-tabs", &panels).into_string();
        // First tab's markup (contains "Overview") should be the one with --active.
        let first_tab_start = html.find("Overview").unwrap();
        let preceding = &html[..first_tab_start];
        let last_tag_start = preceding.rfind('<').unwrap();
        assert!(
            preceding[last_tag_start..].contains("autumn-tabs__tab--active"),
            "{html}"
        );
    }

    // ── ARIA wiring ─────────────────────────────────────────────────────

    #[test]
    fn tab_aria_controls_matches_panel_id() {
        let panels = [("security", "Security", html! { p { "a" } })];
        let html = tabs("settings-tabs", &panels).into_string();
        assert!(html.contains(r#"aria-controls="security""#), "{html}");
        assert!(html.contains(r#"id="security""#), "{html}");
    }

    #[test]
    fn panel_aria_labelledby_matches_tab_id() {
        let panels = [("security", "Security", html! { p { "a" } })];
        let html = tabs("settings-tabs", &panels).into_string();
        assert!(
            html.contains(r#"id="settings-tabs-tab-security""#),
            "{html}"
        );
        assert!(
            html.contains(r#"aria-labelledby="settings-tabs-tab-security""#),
            "{html}"
        );
    }

    #[test]
    fn panels_have_tabindex_zero() {
        let panels = [("security", "Security", html! { p { "a" } })];
        let html = tabs("settings-tabs", &panels).into_string();
        assert!(html.contains(r#"tabindex="0""#), "{html}");
    }

    #[test]
    fn panels_have_role_tabpanel_and_tabs_have_role_tab() {
        let panels = [("security", "Security", html! { p { "a" } })];
        let html = tabs("settings-tabs", &panels).into_string();
        assert!(html.contains(r#"role="tab""#), "{html}");
        assert!(html.contains(r#"role="tabpanel""#), "{html}");
    }

    // ── deep-linking ────────────────────────────────────────────────────

    #[test]
    fn tab_href_points_at_raw_panel_id_fragment() {
        let panels = [("billing", "Billing", html! { p { "a" } })];
        let html = tabs("settings-tabs", &panels).into_string();
        assert!(html.contains(r##"href="#billing""##), "{html}");
    }

    #[test]
    fn panel_id_is_raw_tuple_id_for_fragment_targeting() {
        let panels = [("billing", "Billing", html! { p { "a" } })];
        let html = tabs("settings-tabs", &panels).into_string();
        // The panel element itself must carry id="billing" (not namespaced)
        // so `#billing` in the URL can :target it directly.
        assert!(html.contains(r#"id="billing""#), "{html}");
    }

    // ── no-JS ───────────────────────────────────────────────────────────

    #[test]
    fn no_script_tag_emitted() {
        let panels = [("overview", "Overview", html! { p { "a" } })];
        let html = tabs("post-tabs", &panels).into_string();
        assert!(!html.contains("<script"), "{html}");
    }

    #[test]
    fn no_inline_event_handlers_or_htmx_attrs() {
        let panels = [("overview", "Overview", html! { p { "a" } })];
        let html = tabs("post-tabs", &panels).into_string();
        assert!(!html.contains("onclick"), "{html}");
        assert!(!html.contains("hx-"), "{html}");
    }

    // ── security / XSS ──────────────────────────────────────────────────

    #[test]
    fn label_html_is_escaped() {
        let panels = [("overview", "<script>alert(1)</script>", html! { p { "a" } })];
        let html = tabs("post-tabs", &panels).into_string();
        assert!(html.contains("&lt;script&gt;"), "{html}");
        assert!(!html.contains("<script>alert"), "{html}");
    }

    #[test]
    fn id_containing_quote_does_not_break_out_of_attribute() {
        let panels = [("a\"b", "Label", html! { p { "x" } })];
        let html = tabs("post-tabs", &panels).into_string();
        assert!(!html.contains(r#"id="a"b""#), "{html}");
        assert!(
            html.contains("a&quot;b") || html.contains("a%22b"),
            "{html}"
        );
    }

    // ── empty list ──────────────────────────────────────────────────────

    #[test]
    fn empty_panel_list_renders_container_without_panic() {
        let panels: [(&str, &str, maud::Markup); 0] = [];
        let html = tabs("post-tabs", &panels).into_string();
        assert!(html.contains(r#"class="autumn-tabs""#), "{html}");
        assert!(!html.contains(r#"role="tab""#), "{html}");
        assert!(!html.contains(r#"role="tabpanel""#), "{html}");
    }

    // ── duplicate ids ───────────────────────────────────────────────────

    #[test]
    fn duplicate_panel_ids_render_both_without_panic() {
        let panels = [
            ("dup", "First", html! { p { "first body" } }),
            ("dup", "Second", html! { p { "second body" } }),
        ];
        let html = tabs("post-tabs", &panels).into_string();
        assert_eq!(html.matches(r#"role="tabpanel""#).count(), 2, "{html}");
        assert!(html.contains("first body"), "{html}");
        assert!(html.contains("second body"), "{html}");
    }
}

// ── TestClient integration: markup fully present in the response body ─────

#[cfg(feature = "maud")]
mod test_client_integration {
    use autumn_web::prelude::*;
    use autumn_web::test::TestApp;
    use autumn_web::widgets::tabs;
    use maud::html;

    #[get("/posts/{id}")]
    async fn show_post(Path(id): Path<i64>) -> Markup {
        let panels = [
            (
                "overview",
                "Overview",
                html! { p { "Post " (id) " overview" } },
            ),
            ("comments", "Comments", html! { p { "Comments for " (id) } }),
            ("activity", "Activity", html! { p { "Activity for " (id) } }),
        ];
        html! {
            h1 { "Post " (id) }
            (tabs("post-tabs", &panels))
        }
    }

    #[tokio::test]
    async fn test_client_can_assert_tabs_structure() {
        let client = TestApp::new().routes(routes![show_post]).build();

        let resp = client.get("/posts/42").send().await;
        resp.assert_ok()
            .assert_selector("div.autumn-tabs")
            .assert_selector(r#"[role="tablist"]"#)
            .assert_selector(r#"[role="tab"]"#)
            .assert_selector(r#"[role="tabpanel"]"#)
            .assert_attr(r##"a[href="#comments"]"##, "aria-controls", "comments");
    }
}
