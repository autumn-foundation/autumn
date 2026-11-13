//! Native input helpers (issue #3405): switch, range, radio group, checkbox
//! group, segmented control, and email, tel, url, time, month, color and
//! hidden inputs.
//!
//! Each helper has a happy-path test, a prefill test and an error test.
//!
//! Run with `cargo test --test integration_tests form_native_inputs`.

#![cfg(feature = "maud")]

use std::collections::HashMap;

use autumn_web::form::{
    Changeset, FieldControl, FieldOptions, FormField, FormModel, checkbox_group,
    checkbox_group_with, color_input, color_input_with, email_input, email_input_with, form_for,
    from_urlencoded, hidden_input, hidden_input_with, month_input, month_input_with, radio_group,
    radio_group_with, range_input, range_input_with, required_email_input, required_month_input,
    required_radio_group, required_segmented_control, required_switch_input, required_tel_input,
    required_time_input, required_url_input, segmented_control, segmented_control_with,
    switch_input, switch_input_with, tel_input, tel_input_with, time_input, time_input_with,
    url_input, url_input_with,
};
use proptest::prelude::*;
use serde::Serialize;

#[derive(Serialize)]
struct Profile {
    alerts: bool,
    volume: i32,
    plan: String,
    topics: Vec<String>,
    view: String,
    email: String,
    phone: String,
    site: String,
    starts: String,
    period: String,
    accent: String,
    token: String,
}

fn profile() -> Profile {
    Profile {
        alerts: true,
        volume: 40,
        plan: "pro".into(),
        topics: vec!["rust".into(), "web".into()],
        view: "grid".into(),
        email: "ada@example.com".into(),
        phone: "+1 555 0100".into(),
        site: "https://example.com".into(),
        starts: "09:30:00".into(),
        period: "2026-03".into(),
        accent: "#7C3AED".into(),
        token: "abc".into(),
    }
}

const fn blank() -> Profile {
    Profile {
        alerts: false,
        volume: 0,
        plan: String::new(),
        topics: Vec::new(),
        view: String::new(),
        email: String::new(),
        phone: String::new(),
        site: String::new(),
        starts: String::new(),
        period: String::new(),
        accent: String::new(),
        token: String::new(),
    }
}

const PLANS: &[(&str, &str)] = &[("free", "Free"), ("pro", "Pro")];
const TOPICS: &[(&str, &str)] = &[("rust", "Rust"), ("web", "Web"), ("db", "Databases")];
const VIEWS: &[(&str, &str)] = &[("list", "List"), ("grid", "Grid")];

type Render = fn(&Changeset<Profile>, &FieldOptions<'_>) -> String;

/// One case per helper: name, field, label, input type, prefill marks, render.
struct Case {
    helper: &'static str,
    field: &'static str,
    label: &'static str,
    input_type: &'static str,
    prefill: &'static [&'static str],
    grouped: bool,
    render: Render,
}

#[allow(clippy::too_many_lines, reason = "one table row for each helper")]
fn cases() -> Vec<Case> {
    vec![
        Case {
            helper: "switch",
            field: "alerts",
            label: "Alerts",
            input_type: "checkbox",
            prefill: &[" checked"],
            grouped: false,
            render: |cs, o| switch_input_with(cs, "alerts", "Alerts", o).into_string(),
        },
        Case {
            helper: "range",
            field: "volume",
            label: "Volume",
            input_type: "range",
            prefill: &[r#"value="40""#],
            grouped: false,
            render: |cs, o| range_input_with(cs, "volume", "Volume", Some("5"), o).into_string(),
        },
        Case {
            helper: "radio_group",
            field: "plan",
            label: "Plan",
            input_type: "radio",
            prefill: &[r#"value="pro" checked"#],
            grouped: true,
            render: |cs, o| radio_group_with(cs, "plan", "Plan", PLANS, o).into_string(),
        },
        Case {
            helper: "checkbox_group",
            field: "topics",
            label: "Topics",
            input_type: "checkbox",
            prefill: &[r#"value="rust" checked"#, r#"value="web" checked"#],
            grouped: true,
            render: |cs, o| checkbox_group_with(cs, "topics", "Topics", TOPICS, o).into_string(),
        },
        Case {
            helper: "segmented_control",
            field: "view",
            label: "View",
            input_type: "radio",
            prefill: &[r#"value="grid" checked"#],
            grouped: true,
            render: |cs, o| segmented_control_with(cs, "view", "View", VIEWS, o).into_string(),
        },
        Case {
            helper: "email",
            field: "email",
            label: "Email",
            input_type: "email",
            prefill: &[r#"value="ada@example.com""#],
            grouped: false,
            render: |cs, o| email_input_with(cs, "email", "Email", o).into_string(),
        },
        Case {
            helper: "tel",
            field: "phone",
            label: "Phone",
            input_type: "tel",
            prefill: &[r#"value="+1 555 0100""#],
            grouped: false,
            render: |cs, o| tel_input_with(cs, "phone", "Phone", o).into_string(),
        },
        Case {
            helper: "url",
            field: "site",
            label: "Site",
            input_type: "url",
            prefill: &[r#"value="https://example.com""#],
            grouped: false,
            render: |cs, o| url_input_with(cs, "site", "Site", o).into_string(),
        },
        Case {
            helper: "time",
            field: "starts",
            label: "Starts",
            input_type: "time",
            prefill: &[r#"value="09:30""#],
            grouped: false,
            render: |cs, o| time_input_with(cs, "starts", "Starts", o).into_string(),
        },
        Case {
            helper: "month",
            field: "period",
            label: "Period",
            input_type: "month",
            prefill: &[r#"value="2026-03""#],
            grouped: false,
            render: |cs, o| month_input_with(cs, "period", "Period", o).into_string(),
        },
        Case {
            helper: "color",
            field: "accent",
            label: "Accent",
            input_type: "color",
            prefill: &[r##"value="#7c3aed""##],
            grouped: false,
            render: |cs, o| color_input_with(cs, "accent", "Accent", o).into_string(),
        },
    ]
}

fn invalid(field: &str) -> Changeset<Profile> {
    let mut errors = HashMap::new();
    errors.insert(field.to_owned(), vec!["is <bad>".to_owned()]);
    Changeset::from_errors(profile(), errors)
}

const MARKER: &str = r#"<span class="autumn-field__required" aria-hidden="true">*</span>"#;

// ── Happy path ───────────────────────────────────────────────────────────────

#[test]
fn every_helper_renders_a_labelled_control_of_its_type() {
    for case in cases() {
        let html = (case.render)(&Changeset::new(blank()), &FieldOptions::new());
        let field = case.field;
        assert!(
            html.contains(&format!(r#"id="{field}-field""#)),
            "{}: {html}",
            case.helper
        );
        assert!(
            html.contains(&format!(r#"type="{}""#, case.input_type)),
            "{}: {html}",
            case.helper
        );
        assert!(
            html.contains(&format!(r#"name="{field}""#)),
            "{}: {html}",
            case.helper
        );
        let label_tag = if case.grouped {
            format!(
                r#"<legend class="autumn-field__label">{}</legend>"#,
                case.label
            )
        } else {
            format!(
                r#"<label for="{field}" class="autumn-field__label">{}</label>"#,
                case.label
            )
        };
        assert!(html.contains(&label_tag), "{}: {html}", case.helper);
        assert!(
            !html.contains("aria-describedby"),
            "{}: {html}",
            case.helper
        );
        assert!(!html.contains("checked"), "{}: {html}", case.helper);
    }
}

#[test]
fn switch_is_a_checkbox_with_the_switch_role() {
    let html = switch_input(&Changeset::new(blank()), "alerts", "Alerts").into_string();
    assert_eq!(
        html,
        concat!(
            r#"<div id="alerts-field" class="autumn-field autumn-field--checkbox autumn-field--switch">"#,
            r#"<input type="checkbox" id="alerts" name="alerts" value="true" role="switch" class="autumn-field__input" aria-invalid="false">"#,
            r#"<label for="alerts" class="autumn-field__label">Alerts</label>"#,
            "</div>"
        )
    );
}

#[test]
fn radio_group_renders_a_fieldset_with_a_legend() {
    let html = radio_group(&Changeset::new(profile()), "plan", "Plan", PLANS).into_string();
    assert_eq!(
        html,
        concat!(
            r#"<fieldset id="plan-field" class="autumn-field autumn-field--group">"#,
            r#"<legend class="autumn-field__label">Plan</legend>"#,
            r#"<div class="autumn-choices">"#,
            r#"<div class="autumn-choice">"#,
            r#"<input type="radio" id="plan" name="plan" value="free" class="autumn-choice__input">"#,
            r#"<label for="plan" class="autumn-choice__label">Free</label>"#,
            "</div>",
            r#"<div class="autumn-choice">"#,
            r#"<input type="radio" id="plan-1" name="plan" value="pro" checked class="autumn-choice__input">"#,
            r#"<label for="plan-1" class="autumn-choice__label">Pro</label>"#,
            "</div>",
            "</div>",
            "</fieldset>"
        )
    );
}

#[test]
fn checkbox_group_renders_a_fieldset_with_a_legend() {
    let html = checkbox_group(&Changeset::new(blank()), "topics", "Topics", TOPICS).into_string();
    assert!(
        html.starts_with(
            r#"<fieldset id="topics-field" class="autumn-field autumn-field--group">"#
        ),
        "{html}"
    );
    assert!(
        html.contains(r#"<legend class="autumn-field__label">Topics</legend>"#),
        "{html}"
    );
    assert!(html.ends_with("</fieldset>"), "{html}");
    for (index, (value, label)) in TOPICS.iter().enumerate() {
        // The first box has the field id, so `#topics` finds the group.
        let id = if index == 0 {
            "topics".to_owned()
        } else {
            format!("topics-{index}")
        };
        assert!(
            html.contains(&format!(
                r#"<input type="checkbox" id="{id}" name="topics" value="{value}" class="autumn-choice__input">"#
            )),
            "{html}"
        );
        assert!(
            html.contains(&format!(
                r#"<label for="{id}" class="autumn-choice__label">{label}</label>"#
            )),
            "{html}"
        );
    }
}

#[test]
fn segmented_control_is_radios_with_segment_classes() {
    let html = segmented_control(&Changeset::new(profile()), "view", "View", VIEWS).into_string();
    assert!(
        html.starts_with(r#"<fieldset id="view-field" class="autumn-field autumn-field--group">"#),
        "{html}"
    );
    assert!(html.contains(r#"<div class="autumn-segmented">"#), "{html}");
    assert!(
        html.contains(concat!(
            r#"<input type="radio" id="view-1" name="view" value="grid" checked class="autumn-segmented__input">"#,
            r#"<label for="view-1" class="autumn-segmented__option">Grid</label>"#
        )),
        "{html}"
    );
    // The radio and its label are siblings, so CSS can style the label from
    // the radio state.
    assert!(!html.contains("autumn-choice"), "{html}");
}

#[test]
fn range_input_writes_step_and_bounds() {
    let html = range_input_with(
        &Changeset::new(profile()),
        "volume",
        "Volume",
        Some("5"),
        &FieldOptions::new().min("0").max("100"),
    )
    .into_string();
    assert!(
        html.contains(r#"<div id="volume-field" class="autumn-field autumn-field--range">"#),
        "{html}"
    );
    for attr in [
        r#"type="range""#,
        r#"value="40""#,
        r#"step="5""#,
        r#"min="0""#,
        r#"max="100""#,
    ] {
        assert!(html.contains(attr), "{attr}: {html}");
    }
    let html = range_input(&Changeset::new(profile()), "volume", "Volume", None).into_string();
    assert!(!html.contains("step="), "{html}");
}

#[test]
fn color_input_has_its_own_wrapper_class() {
    let html = color_input(&Changeset::new(profile()), "accent", "Accent").into_string();
    assert!(
        html.contains(r#"<div id="accent-field" class="autumn-field autumn-field--color">"#),
        "{html}"
    );
}

#[test]
fn simple_inputs_render_their_type() {
    let cs = Changeset::new(profile());
    for (html, input_type) in [
        (email_input(&cs, "email", "Email").into_string(), "email"),
        (tel_input(&cs, "phone", "Phone").into_string(), "tel"),
        (url_input(&cs, "site", "Site").into_string(), "url"),
        (time_input(&cs, "starts", "Starts").into_string(), "time"),
        (month_input(&cs, "period", "Period").into_string(), "month"),
    ] {
        assert!(
            html.contains(&format!(r#"<input type="{input_type}" id=""#)),
            "{html}"
        );
        assert!(
            html.contains(r#"class="autumn-field""#),
            "{input_type}: {html}"
        );
    }
}

#[test]
fn hidden_input_has_no_label_and_no_wrapper() {
    let html = hidden_input(&Changeset::new(profile()), "token").into_string();
    assert_eq!(
        html,
        r#"<input type="hidden" id="token" name="token" value="abc">"#
    );
}

// ── Prefill from the changeset ───────────────────────────────────────────────

#[test]
fn every_helper_prefills_from_the_changeset() {
    for case in cases() {
        let html = (case.render)(&Changeset::new(profile()), &FieldOptions::new());
        for mark in case.prefill {
            assert!(html.contains(mark), "{}: {mark}: {html}", case.helper);
        }
    }
}

#[test]
fn hidden_input_prefills_from_the_changeset() {
    let cs = Changeset::new(Profile {
        token: "a\"b".into(),
        ..profile()
    });
    let html = hidden_input(&cs, "token").into_string();
    assert!(html.contains(r#"value="a&quot;b""#), "{html}");
}

#[test]
fn checkbox_group_checks_only_the_submitted_values() {
    let html = checkbox_group(&Changeset::new(profile()), "topics", "Topics", TOPICS).into_string();
    assert_eq!(html.matches(" checked").count(), 2, "{html}");
    assert!(!html.contains(r#"value="db" checked"#), "{html}");
}

#[test]
fn radio_group_checks_one_value() {
    let html = radio_group(&Changeset::new(profile()), "plan", "Plan", PLANS).into_string();
    assert_eq!(html.matches(" checked").count(), 1, "{html}");
    let html = radio_group(&Changeset::new(blank()), "plan", "Plan", PLANS).into_string();
    assert_eq!(html.matches(" checked").count(), 0, "{html}");
}

#[test]
fn switch_is_unchecked_for_false() {
    let html = switch_input(&Changeset::new(blank()), "alerts", "Alerts").into_string();
    assert!(!html.contains("checked"), "{html}");
}

#[test]
fn time_value_is_normalized_for_the_browser() {
    for (stored, value, step) in [
        ("09:30:00", "09:30", None),
        ("09:30", "09:30", None),
        ("09:30:15", "09:30:15", Some("1")),
        ("09:30:15.000", "09:30:15", Some("1")),
        ("09:30:15.123456789", "09:30:15.123", Some("0.001")),
        ("09:30:15.5", "09:30:15.5", Some("0.001")),
        ("", "", None),
    ] {
        let cs = Changeset::new(Profile {
            starts: stored.into(),
            ..profile()
        });
        let html = time_input(&cs, "starts", "Starts").into_string();
        assert!(
            html.contains(&format!(r#"value="{value}""#)),
            "{stored}: {html}"
        );
        match step {
            Some(step) => assert!(html.contains(&format!(r#"step="{step}""#)), "{html}"),
            None => assert!(!html.contains("step="), "{stored}: {html}"),
        }
    }
}

#[test]
fn month_value_is_normalized_for_the_browser() {
    for (stored, value) in [
        ("2026-03", "2026-03"),
        ("2026-03-15", "2026-03"),
        ("2026-03-15T10:00:00", "2026-03"),
        ("", ""),
    ] {
        let cs = Changeset::new(Profile {
            period: stored.into(),
            ..profile()
        });
        let html = month_input(&cs, "period", "Period").into_string();
        assert!(
            html.contains(&format!(r#"value="{value}""#)),
            "{stored}: {html}"
        );
    }
}

#[test]
fn color_value_is_a_lowercase_simple_color_or_nothing() {
    for (stored, value) in [
        ("#7C3AED", Some("#7c3aed")),
        ("#00ff00", Some("#00ff00")),
        ("red", None),
        ("#fff", Some("#ffffff")),
        ("#ABC", Some("#aabbcc")),
        ("#ffff", None),
        ("#12345g", None),
        ("", None),
    ] {
        let cs = Changeset::new(Profile {
            accent: stored.into(),
            ..profile()
        });
        let html = color_input(&cs, "accent", "Accent").into_string();
        match value {
            Some(value) => assert!(html.contains(&format!(r#"value="{value}""#)), "{html}"),
            None => assert!(!html.contains("value="), "{stored}: {html}"),
        }
    }
}

#[test]
fn a_time_input_value_decodes_into_naive_time() {
    #[derive(serde::Deserialize)]
    struct Slot {
        starts: chrono::NaiveTime,
    }
    for (body, want) in [
        (&b"starts=09%3A30"[..], "09:30:00"),
        (&b"starts=09%3A30%3A15"[..], "09:30:15"),
    ] {
        let slot: Slot = from_urlencoded(body).expect("decodes");
        assert_eq!(slot.starts.to_string(), want);
    }
}

// ── Error wiring ─────────────────────────────────────────────────────────────

#[test]
fn every_helper_wires_its_errors() {
    for case in cases() {
        let field = case.field;
        let html = (case.render)(&invalid(field), &FieldOptions::new());
        assert!(
            html.contains(&format!(
                r#"<div id="{field}-error" role="alert" class="autumn-field__errors"><p class="autumn-field__error">is &lt;bad&gt;</p></div>"#
            )),
            "{}: {html}",
            case.helper
        );
        assert!(
            html.contains(r#"aria-invalid="true""#),
            "{}: {html}",
            case.helper
        );
        assert_eq!(
            html.matches(&format!(r#"aria-describedby="{field}-error""#))
                .count(),
            1,
            "{}: one describedby: {html}",
            case.helper
        );
        if !case.grouped {
            assert!(
                html.contains("autumn-field__input autumn-field__input--invalid"),
                "{}: {html}",
                case.helper
            );
        }
    }
}

#[test]
fn a_group_puts_the_error_links_on_the_fieldset() {
    for (field, choices, html) in [
        (
            "plan",
            PLANS,
            radio_group(&invalid("plan"), "plan", "Plan", PLANS).into_string(),
        ),
        (
            "topics",
            TOPICS,
            checkbox_group(&invalid("topics"), "topics", "Topics", TOPICS).into_string(),
        ),
        (
            "view",
            VIEWS,
            segmented_control(&invalid("view"), "view", "View", VIEWS).into_string(),
        ),
    ] {
        let fieldset_end = html.find('>').expect("fieldset tag");
        let fieldset = &html[..fieldset_end];
        assert!(
            fieldset.contains(r#"aria-invalid="true""#),
            "{field}: {html}"
        );
        assert!(
            fieldset.contains(&format!(r#"aria-describedby="{field}-error""#)),
            "{field}: {html}"
        );
        // The fieldset and each item say they are invalid.
        assert_eq!(
            html.matches(r#"aria-invalid="true""#).count(),
            choices.len() + 1,
            "{field}: {html}"
        );
    }
}

#[test]
fn hidden_input_renders_no_error_block() {
    // A hidden input is not visible. Show its errors with `error_summary`.
    let html = hidden_input(&invalid("token"), "token").into_string();
    assert!(!html.contains("token-error"), "{html}");
    assert!(!html.contains("aria-"), "{html}");
}

// ── Wrapper v2: hint, required and passthrough ───────────────────────────────

#[test]
fn every_helper_renders_the_hint() {
    let opts = FieldOptions::new().hint("Help <me>");
    for case in cases() {
        let field = case.field;
        let html = (case.render)(&Changeset::new(blank()), &opts);
        assert!(
            html.contains(&format!(
                r#"<p id="{field}-hint" class="autumn-field__hint">Help &lt;me&gt;</p>"#
            )),
            "{}: {html}",
            case.helper
        );
        assert!(
            html.contains(&format!(r#"aria-describedby="{field}-hint""#)),
            "{}: {html}",
            case.helper
        );
        let html = (case.render)(&invalid(field), &opts);
        assert!(
            html.contains(&format!(r#"aria-describedby="{field}-hint {field}-error""#)),
            "{}: {html}",
            case.helper
        );
    }
}

#[test]
fn every_helper_renders_the_passthrough() {
    let opts = FieldOptions::new()
        .data("test-id", "x")
        .hx("trigger", "change")
        .disabled();
    for case in cases() {
        let html = (case.render)(&Changeset::new(blank()), &opts);
        assert!(
            html.contains(r#"data-test-id="x""#),
            "{}: {html}",
            case.helper
        );
        assert!(
            html.contains(r#"hx-trigger="change""#),
            "{}: {html}",
            case.helper
        );
        assert!(html.contains(" disabled"), "{}: {html}", case.helper);
    }
    let html = hidden_input_with(&Changeset::new(blank()), "token", &opts).into_string();
    assert!(html.contains(r#"data-test-id="x""#), "{html}");
    assert!(html.contains(r#"hx-trigger="change""#), "{html}");
    assert!(html.contains(" disabled"), "{html}");
}

#[test]
fn a_group_writes_the_passthrough_on_each_input() {
    let opts = FieldOptions::new().data("test-id", "x");
    let html =
        radio_group_with(&Changeset::new(blank()), "plan", "Plan", PLANS, &opts).into_string();
    assert_eq!(
        html.matches(r#"data-test-id="x""#).count(),
        PLANS.len(),
        "{html}"
    );
}

#[test]
fn hidden_input_ignores_the_hint_and_the_required_state() {
    let opts = FieldOptions::new().hint("Help").required();
    let html = hidden_input_with(&Changeset::new(blank()), "token", &opts).into_string();
    assert!(!html.contains("Help"), "{html}");
    assert!(!html.contains("required"), "{html}");
}

#[test]
fn required_variants_mark_the_field_required() {
    let cs = Changeset::new(blank());
    for (helper, html) in [
        (
            "switch",
            required_switch_input(&cs, "alerts", "Alerts").into_string(),
        ),
        (
            "email",
            required_email_input(&cs, "email", "Email").into_string(),
        ),
        (
            "tel",
            required_tel_input(&cs, "phone", "Phone").into_string(),
        ),
        ("url", required_url_input(&cs, "site", "Site").into_string()),
        (
            "time",
            required_time_input(&cs, "starts", "Starts").into_string(),
        ),
        (
            "month",
            required_month_input(&cs, "period", "Period").into_string(),
        ),
    ] {
        assert!(html.contains(" required"), "{helper}: {html}");
        assert!(html.contains(r#"aria-required="true""#), "{helper}: {html}");
        assert_eq!(html.matches(MARKER).count(), 1, "{helper}: {html}");
    }
}

#[test]
fn a_required_radio_group_marks_each_radio_and_the_legend() {
    let cs = Changeset::new(blank());
    for (helper, html) in [
        (
            "radio_group",
            required_radio_group(&cs, "plan", "Plan", PLANS).into_string(),
        ),
        (
            "segmented_control",
            required_segmented_control(&cs, "view", "View", VIEWS).into_string(),
        ),
    ] {
        assert_eq!(html.matches(" required").count(), 2, "{helper}: {html}");
        // `aria-required` is not allowed on a radio or on a group.
        assert!(!html.contains("aria-required"), "{helper}: {html}");
        let legend_end = html.find("</legend>").expect("legend");
        assert!(html[..legend_end].contains(MARKER), "{helper}: {html}");
    }
}

#[test]
fn a_required_checkbox_group_does_not_require_each_box() {
    // `required` on a box means "check this box". A group means "check one
    // or more", which HTML cannot say. Show the marker, and validate on the
    // server.
    let opts = FieldOptions::new().required();
    let html = checkbox_group_with(&Changeset::new(blank()), "topics", "Topics", TOPICS, &opts)
        .into_string();
    assert!(!html.contains(" required"), "{html}");
    assert!(!html.contains("aria-required"), "{html}");
    assert_eq!(html.matches(MARKER).count(), 1, "{html}");
}

#[test]
fn range_and_color_drop_the_required_state() {
    // HTML does not apply `required` to these types: they always have a value.
    let opts = FieldOptions::new().required();
    let cs = Changeset::new(blank());
    for html in [
        range_input_with(&cs, "volume", "Volume", None, &opts).into_string(),
        color_input_with(&cs, "accent", "Accent", &opts).into_string(),
    ] {
        assert!(!html.contains("required"), "{html}");
        assert!(!html.contains(MARKER), "{html}");
    }
}

#[test]
fn typed_options_render_only_where_the_type_uses_them() {
    let opts = FieldOptions::new()
        .placeholder("p")
        .pattern("x")
        .inputmode("tel")
        .autocomplete("tel")
        .min("1")
        .max("9")
        .readonly();
    let cs = Changeset::new(blank());
    let has = |html: &str, attrs: &[&str], missing: &[&str]| {
        for attr in attrs {
            assert!(html.contains(attr), "{attr}: {html}");
        }
        for attr in missing {
            assert!(!html.contains(attr), "{attr}: {html}");
        }
    };
    let text_attrs = [
        r#"placeholder="p""#,
        r#"pattern="x""#,
        r#"inputmode="tel""#,
        r#"autocomplete="tel""#,
        " readonly",
    ];
    for html in [
        email_input_with(&cs, "email", "Email", &opts).into_string(),
        tel_input_with(&cs, "phone", "Phone", &opts).into_string(),
        url_input_with(&cs, "site", "Site", &opts).into_string(),
    ] {
        has(&html, &text_attrs, &["min=", "max="]);
    }
    for html in [
        time_input_with(&cs, "starts", "Starts", &opts).into_string(),
        month_input_with(&cs, "period", "Period", &opts).into_string(),
    ] {
        has(
            &html,
            &[
                r#"min="1""#,
                r#"max="9""#,
                " readonly",
                r#"autocomplete="tel""#,
            ],
            &["placeholder=", "pattern=", "inputmode="],
        );
    }
    let html = range_input_with(&cs, "volume", "Volume", None, &opts).into_string();
    has(
        &html,
        &[r#"min="1""#, r#"max="9""#],
        &["placeholder=", "pattern=", "readonly"],
    );
    let html = color_input_with(&cs, "accent", "Accent", &opts).into_string();
    has(
        &html,
        &[r#"autocomplete="tel""#],
        &["placeholder=", "pattern=", "readonly", "min=", "max="],
    );
    let html = switch_input_with(&cs, "alerts", "Alerts", &opts).into_string();
    has(
        &html,
        &[],
        &[
            "placeholder=",
            "pattern=",
            "readonly",
            "min=",
            "autocomplete=",
        ],
    );
    let html = radio_group_with(&cs, "plan", "Plan", PLANS, &opts).into_string();
    has(
        &html,
        &[],
        &[
            "placeholder=",
            "pattern=",
            "readonly",
            "min=",
            "autocomplete=",
        ],
    );
}

#[test]
fn choice_values_and_labels_are_escaped() {
    let choices = &[("a\"b", "<b>A</b>")];
    let html = radio_group(&Changeset::new(blank()), "plan", "Plan", choices).into_string();
    assert!(html.contains(r#"value="a&quot;b""#), "{html}");
    assert!(html.contains("&lt;b&gt;A&lt;/b&gt;"), "{html}");
    assert!(!html.contains("<b>"), "{html}");
}

#[test]
fn a_blank_value_checks_the_blank_choice_like_a_select() {
    let choices = &[("", "None"), ("pro", "Pro")];
    let html = radio_group(&Changeset::new(blank()), "plan", "Plan", choices).into_string();
    assert!(html.contains(r#"value="" checked"#), "{html}");
    assert_eq!(html.matches(" checked").count(), 1, "{html}");
}

#[test]
fn the_error_summary_link_lands_on_the_first_item_of_a_group() {
    // `error_summary` links to `#{field}`. The first input has that id.
    let cs = invalid("plan");
    let summary = autumn_web::widgets::error_summary(&cs)
        .expect("summary")
        .into_string();
    assert!(summary.contains(r##"href="#plan""##), "{summary}");
    for (input_type, html) in [
        (
            "radio",
            radio_group(&cs, "plan", "Plan", PLANS).into_string(),
        ),
        (
            "radio",
            segmented_control(&cs, "plan", "Plan", PLANS).into_string(),
        ),
        (
            "checkbox",
            checkbox_group(&cs, "plan", "Plan", PLANS).into_string(),
        ),
    ] {
        assert_eq!(html.matches(r#" id="plan""#).count(), 1, "{html}");
        assert!(
            html.contains(&format!(
                r#"<input type="{input_type}" id="plan" name="plan""#
            )),
            "{html}"
        );
    }
}

#[test]
fn an_empty_choice_list_renders_an_empty_group() {
    let html = radio_group(&Changeset::new(blank()), "plan", "Plan", &[]).into_string();
    assert!(
        html.contains(r#"<div class="autumn-choices"></div>"#),
        "{html}"
    );
}

/// Every `id="…"` value in `html`.
fn ids(html: &str) -> Vec<&str> {
    html.split(r#" id=""#)
        .skip(1)
        .filter_map(|rest| rest.split('"').next())
        .collect()
}

proptest! {
    /// Mirror of `verification/choice_item_id.rs`, on the rendered markup:
    /// groups on one page, each with a hint and an error. No id occurs two
    /// times, and each `<label for>` names an id on the page.
    #[test]
    fn choice_item_ids_never_collide(
        fields in proptest::collection::btree_set("[a-z_][a-z0-9_]{0,6}", 1..4),
        count in 1usize..13,
    ) {
        let choices: Vec<(String, String)> =
            (0..count).map(|i| (format!("v{i}"), format!("L{i}"))).collect();
        let borrowed: Vec<(&str, &str)> =
            choices.iter().map(|(v, l)| (v.as_str(), l.as_str())).collect();
        let opts = FieldOptions::new().hint("h");
        let mut page = String::new();
        for (n, field) in fields.iter().enumerate() {
            let mut errors = HashMap::new();
            errors.insert(field.clone(), vec!["bad".to_owned()]);
            let cs = Changeset::from_errors(serde_json::json!({}), errors);
            let html = match n % 3 {
                0 => radio_group_with(&cs, field, "L", &borrowed, &opts),
                1 => checkbox_group_with(&cs, field, "L", &borrowed, &opts),
                _ => segmented_control_with(&cs, field, "L", &borrowed, &opts),
            };
            page.push_str(&html.into_string());
        }
        let all = ids(&page);
        let unique: std::collections::HashSet<&str> = all.iter().copied().collect();
        prop_assert_eq!(unique.len(), all.len(), "duplicate id in {}", page);
        // Per group: the wrapper, the hint, the error and one id per item.
        prop_assert_eq!(all.len(), fields.len() * (count + 3), "{}", page);
        for target in page.split(r#" for=""#).skip(1).filter_map(|r| r.split('"').next()) {
            prop_assert!(unique.contains(target), "label for {} has no id", target);
        }
    }
}

// ── form_for ─────────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct Post {
    title: String,
}

impl FormModel for Post {
    fn form_fields() -> Vec<FormField> {
        vec![FormField::new("title", "Title", FieldControl::Text, false)]
    }
}

fn post_form(control: FieldControl, options: FieldOptions<'_>) -> String {
    let cs = Changeset::new(Post {
        title: "pro".into(),
    });
    form_for(&cs, "/posts", "post")
        .override_field("title", control)
        .field_options("title", options)
        .render()
        .into_string()
}

fn choice_options() -> Vec<(String, String)> {
    vec![("free".into(), "Free".into()), ("pro".into(), "Pro".into())]
}

#[test]
fn form_for_renders_every_new_control() {
    for (control, mark) in [
        (FieldControl::Switch, r#"role="switch""#),
        (
            FieldControl::Range {
                step: Some("2".into()),
            },
            r#"step="2""#,
        ),
        (
            FieldControl::Radio {
                options: choice_options(),
            },
            r#"value="pro" checked"#,
        ),
        (
            FieldControl::Segmented {
                options: choice_options(),
            },
            r#"class="autumn-segmented""#,
        ),
        (
            FieldControl::CheckboxGroup {
                options: choice_options(),
            },
            r#"type="checkbox" id="title-1" name="title" value="pro" checked"#,
        ),
        (FieldControl::Email, r#"type="email""#),
        (FieldControl::Tel, r#"type="tel""#),
        (FieldControl::Url, r#"type="url""#),
        (FieldControl::Time, r#"type="time""#),
        (FieldControl::Month, r#"type="month""#),
        (FieldControl::Color, r#"type="color""#),
        (
            FieldControl::Hidden,
            r#"<input type="hidden" id="title" name="title" value="pro">"#,
        ),
    ] {
        let html = post_form(control.clone(), FieldOptions::new());
        assert!(html.contains(mark), "{control:?}: {html}");
    }
}

#[test]
fn form_for_passes_options_to_range_color_and_hidden() {
    let opts = || {
        FieldOptions::new()
            .hint("Help")
            .data("role", "x")
            .disabled()
    };
    for control in [
        FieldControl::Range { step: None },
        FieldControl::Color,
        FieldControl::Hidden,
    ] {
        let html = post_form(control.clone(), opts());
        assert!(html.contains(r#"data-role="x""#), "{control:?}: {html}");
        assert!(html.contains(" disabled"), "{control:?}: {html}");
        // A hidden input has no hint. The others have one.
        let hinted = !matches!(control, FieldControl::Hidden);
        assert_eq!(
            html.contains(r#"aria-describedby="title-hint""#),
            hinted,
            "{control:?}: {html}"
        );
    }
}

#[test]
fn form_for_hidden_has_no_label() {
    let html = post_form(FieldControl::Hidden, FieldOptions::new());
    assert!(!html.contains("<label"), "{html}");
    assert!(!html.contains("title-field"), "{html}");
}

#[test]
fn form_for_model_required_skips_controls_that_always_have_a_value() {
    // A non-null column is "required" in the model. A switch, a slider, a
    // color, a hidden input and a checkbox group get no `required` from it.
    #[derive(Serialize)]
    struct Settings {
        on: bool,
        level: i32,
        tint: String,
        secret: String,
        tags: Vec<String>,
        plan: String,
        when: String,
    }
    impl FormModel for Settings {
        fn form_fields() -> Vec<FormField> {
            vec![
                FormField::new("on", "On", FieldControl::Switch, true),
                FormField::new("level", "Level", FieldControl::Range { step: None }, true),
                FormField::new("tint", "Tint", FieldControl::Color, true),
                FormField::new("secret", "Secret", FieldControl::Hidden, true),
                FormField::new(
                    "tags",
                    "Tags",
                    FieldControl::CheckboxGroup {
                        options: vec![("a".into(), "A".into())],
                    },
                    true,
                ),
                FormField::new(
                    "plan",
                    "Plan",
                    FieldControl::Radio {
                        options: vec![("a".into(), "A".into())],
                    },
                    true,
                ),
                FormField::new("when", "When", FieldControl::Time, true),
            ]
        }
    }
    let cs = Changeset::new(Settings {
        on: false,
        level: 1,
        tint: "#000000".into(),
        secret: "s".into(),
        tags: vec!["a".into()],
        plan: String::new(),
        when: String::new(),
    });
    let html = form_for(&cs, "/s", "post").render().into_string();
    // Only the radio and the time input are required.
    assert_eq!(html.matches(" required").count(), 2, "{html}");
    assert_eq!(html.matches(MARKER).count(), 2, "{html}");
    assert!(
        html.contains(r#"<input type="time" id="when" name="when" value="" required"#),
        "{html}"
    );
    assert!(html.contains(r#"value="a" required"#), "{html}");
}

#[test]
fn form_for_checkbox_group_checks_every_stored_value() {
    #[derive(Serialize)]
    struct Tagged {
        tags: Vec<String>,
    }
    impl FormModel for Tagged {
        fn form_fields() -> Vec<FormField> {
            vec![FormField::new(
                "tags",
                "Tags",
                FieldControl::CheckboxGroup {
                    options: vec![
                        ("a".into(), "A".into()),
                        ("b".into(), "B".into()),
                        ("c".into(), "C".into()),
                    ],
                },
                false,
            )]
        }
    }
    let cs = Changeset::new(Tagged {
        tags: vec!["a".into(), "c".into()],
    });
    let html = form_for(&cs, "/t", "post").render().into_string();
    assert!(html.contains(r#"value="a" checked"#), "{html}");
    assert!(!html.contains(r#"value="b" checked"#), "{html}");
    assert!(html.contains(r#"value="c" checked"#), "{html}");
}

// ── Stylesheet ───────────────────────────────────────────────────────────────

#[test]
fn stylesheet_styles_the_new_controls() {
    let css = autumn_web::ui::WIDGETS_COMPONENT_CSS;
    for selector in [
        ".autumn-field--switch .autumn-field__input",
        ".autumn-field--switch .autumn-field__input:checked",
        ".autumn-field--range .autumn-field__input",
        ".autumn-field--color .autumn-field__input",
        ".autumn-choices",
        ".autumn-choice__input",
        ".autumn-segmented__input:checked + .autumn-segmented__option",
        ".autumn-segmented__input:focus-visible + .autumn-segmented__option",
    ] {
        assert!(css.contains(selector), "missing `{selector}`");
    }
    // A forced-colors block draws the switch thumb with system colors.
    assert!(
        css.split("@media (forced-colors: active)")
            .skip(1)
            .any(|block| block.contains(".autumn-field--switch")),
        "no forced-colors rule for the switch"
    );
    // The segmented radio stays in the a11y tree and takes focus.
    let start = css
        .find(".autumn-segmented__input {")
        .expect("segment input rule");
    let rule = &css[start..start + css[start..].find('}').expect("rule end")];
    assert!(!rule.contains("display: none"), "{rule}");
    assert!(rule.contains("clip-path"), "{rule}");
}
