//! Field wrapper v2 (issue #3395): hint slot, typed attribute passthrough,
//! required parity, and the linked error summary.
//!
//! Run with `cargo test --test integration_tests form_field_options`.

#![cfg(feature = "maud")]

use std::collections::HashMap;

use autumn_web::form::{
    Changeset, FieldControl, FieldOptions, FormField, FormModel, RichTextLabels,
    checkbox_input_with, date_input_with, datetime_input_with, form_for, number_input_with,
    password_input, password_input_with, required_checkbox_input, required_password_input,
    required_select_input, required_text_input, required_textarea_input, rich_text_area_htmx_with,
    rich_text_area_with, select_input_with, text_input, text_input_htmx_with, text_input_with,
    textarea_input_with,
};
use autumn_web::nested_form::{
    InputsForOptions, NestedChild, decode_nested_urlencoded, inputs_for,
};
use autumn_web::widgets::{ERROR_SUMMARY_ID, error_summary};
use proptest::prelude::*;
use serde::Serialize;

#[derive(Serialize)]
struct Form {
    email: String,
    secret: String,
    bio: String,
    agree: bool,
    age: i32,
    born: String,
    starts: String,
    role: String,
}

fn form() -> Form {
    Form {
        email: "a@example.com".into(),
        secret: String::new(),
        bio: "Hi".into(),
        agree: false,
        age: 30,
        born: "2000-01-02".into(),
        starts: "2026-01-02T03:04:05".into(),
        role: "admin".into(),
    }
}

const ROLES: &[(&str, &str)] = &[("admin", "Admin"), ("user", "User")];

/// One renderer per helper. Each renders field `name` with `opts`.
type Render = fn(&Changeset<Form>, &FieldOptions<'_>) -> String;

fn helpers() -> Vec<(&'static str, &'static str, Render)> {
    vec![
        ("text", "email", |cs, o| {
            text_input_with(cs, "email", "Email", o).into_string()
        }),
        ("text_htmx", "email", |cs, o| {
            text_input_htmx_with(cs, "email", "Email", "/v/email", "_submit_token", o).into_string()
        }),
        ("password", "secret", |cs, o| {
            password_input_with(cs, "secret", "Secret", o).into_string()
        }),
        ("textarea", "bio", |cs, o| {
            textarea_input_with(cs, "bio", "Bio", o).into_string()
        }),
        ("checkbox", "agree", |cs, o| {
            checkbox_input_with(cs, "agree", "Agree", o).into_string()
        }),
        ("number", "age", |cs, o| {
            number_input_with(cs, "age", "Age", Some("1"), o).into_string()
        }),
        ("date", "born", |cs, o| {
            date_input_with(cs, "born", "Born", o).into_string()
        }),
        ("datetime", "starts", |cs, o| {
            datetime_input_with(cs, "starts", "Starts", o).into_string()
        }),
        ("select", "role", |cs, o| {
            select_input_with(cs, "role", "Role", ROLES, o).into_string()
        }),
        ("rich_text", "bio", |cs, o| {
            rich_text_area_with(cs, "bio", "Bio", &RichTextLabels::new(), o).into_string()
        }),
        ("rich_text_htmx", "bio", |cs, o| {
            rich_text_area_htmx_with(
                cs,
                "bio",
                "Bio",
                "/preview/bio",
                "_submit_token",
                &RichTextLabels::new(),
                o,
            )
            .into_string()
        }),
    ]
}

fn invalid(field: &str) -> Changeset<Form> {
    let mut errors = HashMap::new();
    errors.insert(field.to_owned(), vec!["is wrong".to_owned()]);
    Changeset::from_errors(form(), errors)
}

// ── Hint slot ────────────────────────────────────────────────────────────────

#[test]
fn every_helper_renders_the_hint_and_describes_the_field_with_it() {
    let opts = FieldOptions::new().hint("Help <text>");
    for (helper, field, render) in helpers() {
        let html = render(&Changeset::new(form()), &opts);
        assert!(
            html.contains(&format!(r#"id="{field}-hint""#)),
            "{helper}: {html}"
        );
        assert!(html.contains("Help &lt;text&gt;"), "{helper}: {html}");
        assert!(
            html.contains(&format!(r#"aria-describedby="{field}-hint""#)),
            "{helper}: {html}"
        );
        assert_eq!(
            html.matches(&format!(r#"id="{field}-hint""#)).count(),
            1,
            "{helper}: one hint element: {html}"
        );
    }
}

#[test]
fn hint_comes_before_the_error_in_aria_describedby() {
    let opts = FieldOptions::new().hint("Help");
    for (helper, field, render) in helpers() {
        let html = render(&invalid(field), &opts);
        assert!(
            html.contains(&format!(r#"aria-describedby="{field}-hint {field}-error""#)),
            "{helper}: {html}"
        );
    }
}

#[test]
fn no_helper_emits_an_empty_aria_describedby() {
    for (helper, _, render) in helpers() {
        let html = render(&Changeset::new(form()), &FieldOptions::new());
        assert!(!html.contains(r#"aria-describedby="""#), "{helper}: {html}");
    }
}

// ── Attribute passthrough ────────────────────────────────────────────────────

#[test]
fn data_and_hx_passthrough_renders_on_every_helper() {
    let opts = FieldOptions::new()
        .data("test-id", "x\"y")
        .hx("indicator", "#spin")
        .hx("on:click", "go()");
    for (helper, _, render) in helpers() {
        let html = render(&Changeset::new(form()), &opts);
        assert!(
            html.contains(r#"data-test-id="x&quot;y""#),
            "{helper}: {html}"
        );
        assert!(
            html.contains(r##"hx-indicator="#spin""##),
            "{helper}: {html}"
        );
        assert!(html.contains(r#"hx-on:click="go()""#), "{helper}: {html}");
    }
}

#[test]
fn disabled_renders_on_every_helper() {
    for (helper, _, render) in helpers() {
        let html = render(&Changeset::new(form()), &FieldOptions::new().disabled());
        assert!(html.contains(" disabled"), "{helper}: {html}");
    }
}

#[test]
fn typed_passthrough_renders_where_the_control_accepts_it() {
    let opts = FieldOptions::new()
        .placeholder("you@example.com")
        .autocomplete("email")
        .inputmode("email")
        .pattern(".+@.+")
        .readonly();
    let html = text_input_with(&Changeset::new(form()), "email", "Email", &opts).into_string();
    for attr in [
        r#"placeholder="you@example.com""#,
        r#"autocomplete="email""#,
        r#"inputmode="email""#,
        r#"pattern=".+@.+""#,
        " readonly",
    ] {
        assert!(html.contains(attr), "{attr}: {html}");
    }

    let opts = FieldOptions::new().min("1").max("120");
    let html = number_input_with(&Changeset::new(form()), "age", "Age", None, &opts).into_string();
    assert!(html.contains(r#"min="1""#), "{html}");
    assert!(html.contains(r#"max="120""#), "{html}");

    let opts = FieldOptions::new().min("2000-01-01").max("2030-12-31");
    let html = date_input_with(&Changeset::new(form()), "born", "Born", &opts).into_string();
    assert!(html.contains(r#"min="2000-01-01""#), "{html}");
    assert!(html.contains(r#"max="2030-12-31""#), "{html}");

    let opts = FieldOptions::new()
        .autocomplete("current-password")
        .placeholder("****");
    let html =
        password_input_with(&Changeset::new(form()), "secret", "Secret", &opts).into_string();
    assert!(
        html.contains(r#"autocomplete="current-password""#),
        "{html}"
    );
    assert!(html.contains(r#"placeholder="****""#), "{html}");

    let opts = FieldOptions::new().placeholder("About you").readonly();
    let html = textarea_input_with(&Changeset::new(form()), "bio", "Bio", &opts).into_string();
    assert!(html.contains(r#"placeholder="About you""#), "{html}");
    assert!(html.contains(" readonly"), "{html}");

    let opts = FieldOptions::new().autocomplete("organization-title");
    let html =
        select_input_with(&Changeset::new(form()), "role", "Role", ROLES, &opts).into_string();
    assert!(
        html.contains(r#"autocomplete="organization-title""#),
        "{html}"
    );
}

#[test]
fn typed_passthrough_skips_attributes_the_control_ignores() {
    // HTML does not use these attributes on these controls.
    let opts = FieldOptions::new()
        .placeholder("p")
        .readonly()
        .pattern("x")
        .min("1");
    let html = checkbox_input_with(&Changeset::new(form()), "agree", "Agree", &opts).into_string();
    for attr in ["placeholder=", " readonly", "pattern=", "min="] {
        assert!(!html.contains(attr), "{attr}: {html}");
    }
    let html =
        select_input_with(&Changeset::new(form()), "role", "Role", ROLES, &opts).into_string();
    for attr in ["placeholder=", " readonly", "pattern=", "min="] {
        assert!(!html.contains(attr), "{attr}: {html}");
    }
    let html = date_input_with(&Changeset::new(form()), "born", "Born", &opts).into_string();
    assert!(!html.contains("placeholder="), "{html}");
    assert!(!html.contains("pattern="), "{html}");
}

#[test]
fn passthrough_cannot_override_id_name_or_aria() {
    // Every extra name gets a fixed `data-` / `hx-` prefix, so it can never
    // name a reserved attribute. A name with characters outside the safe
    // alphabet is dropped, so it cannot break out of the attribute.
    let opts = FieldOptions::new()
        .data("id", "evil")
        .hx("name", "evil")
        .data("x\" id=\"evil", "1")
        .data("x aria-describedby", "1")
        .hx("y\"><script>", "1")
        .data("", "1")
        .data("UPPER", "1");
    for (helper, field, render) in helpers() {
        let html = render(&Changeset::new(form()), &opts);
        assert_eq!(
            html.matches(&format!(r#" id="{field}""#)).count(),
            1,
            "{helper}: {html}"
        );
        assert_eq!(
            html.matches(&format!(r#" name="{field}""#)).count(),
            1,
            "{helper}: {html}"
        );
        assert!(!html.contains(r#" id="evil""#), "{helper}: {html}");
        assert!(!html.contains(r#" name="evil""#), "{helper}: {html}");
        assert!(!html.contains("<script>"), "{helper}: {html}");
        assert!(!html.contains("data-UPPER"), "{helper}: {html}");
        assert!(!html.contains("data-=\""), "{helper}: {html}");
        // The safe names still render, behind their prefix.
        assert!(html.contains(r#"data-id="evil""#), "{helper}: {html}");
        assert!(html.contains(r#"hx-name="evil""#), "{helper}: {html}");
        assert_eq!(
            html.matches("aria-describedby=").count(),
            usize::from(html.contains(&format!("{field}-hint"))),
            "{helper}: {html}"
        );
    }
}

#[test]
fn htmx_helpers_keep_their_own_hx_attributes() {
    // A passthrough `hx-params` would let `_submit_token` leak into the
    // validation POST (#1360). The helper owns these names.
    let opts = FieldOptions::new()
        .hx("params", "*")
        .hx("post", "/evil")
        .hx("target", "body");
    let cs = Changeset::new(form());
    let html = text_input_htmx_with(&cs, "email", "Email", "/v/email", "_submit_token", &opts)
        .into_string();
    assert_eq!(html.matches("hx-params=").count(), 1, "{html}");
    assert!(html.contains(r#"hx-params="not _submit_token""#), "{html}");
    assert!(!html.contains("/evil"), "{html}");
    assert_eq!(html.matches("hx-target=").count(), 1, "{html}");

    let html = rich_text_area_htmx_with(
        &cs,
        "bio",
        "Bio",
        "/preview/bio",
        "_submit_token",
        &RichTextLabels::new(),
        &opts,
    )
    .into_string();
    assert_eq!(html.matches("hx-params=").count(), 1, "{html}");
    assert!(!html.contains("/evil"), "{html}");
}

proptest! {
    /// Any extra attribute either renders as exactly one `data-{name}="…"`
    /// insert or is dropped. It never changes the rest of the markup.
    #[test]
    fn passthrough_only_adds_its_own_attribute(name in ".{0,12}", value in ".{0,12}") {
        let cs = Changeset::new(form());
        let base = text_input(&cs, "email", "Email").into_string();
        let opts = FieldOptions::new().data(name.clone(), value.clone());
        let html = text_input_with(&cs, "email", "Email", &opts).into_string();
        if html != base {
            let escaped = value
                .replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;")
                .replace('"', "&quot;");
            let attr = format!(" data-{name}=\"{escaped}\"");
            prop_assert_eq!(html.matches(&attr).count(), 1, "{}", html);
            prop_assert_eq!(html.replacen(&attr, "", 1), base);
            prop_assert!(name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'));
        }
    }
}

// ── Required parity and marker ───────────────────────────────────────────────

const MARKER: &str = r#"<span class="autumn-field__required" aria-hidden="true">*</span>"#;

#[test]
fn required_variants_exist_for_password_textarea_checkbox_and_select() {
    let cs = Changeset::new(form());
    for (helper, html) in [
        (
            "password",
            required_password_input(&cs, "secret", "Secret").into_string(),
        ),
        (
            "textarea",
            required_textarea_input(&cs, "bio", "Bio").into_string(),
        ),
        (
            "checkbox",
            required_checkbox_input(&cs, "agree", "Agree").into_string(),
        ),
        (
            "select",
            required_select_input(&cs, "role", "Role", ROLES).into_string(),
        ),
        (
            "text",
            required_text_input(&cs, "email", "Email").into_string(),
        ),
    ] {
        assert!(html.contains(" required"), "{helper}: {html}");
        assert!(html.contains(r#"aria-required="true""#), "{helper}: {html}");
        assert_eq!(html.matches(MARKER).count(), 1, "{helper}: {html}");
    }
}

#[test]
fn required_option_adds_the_marker_on_every_helper() {
    let opts = FieldOptions::new().required();
    for (helper, _, render) in helpers() {
        let required = render(&Changeset::new(form()), &opts);
        assert!(required.contains(" required"), "{helper}: {required}");
        assert!(
            required.contains(r#"aria-required="true""#),
            "{helper}: {required}"
        );
        assert_eq!(required.matches(MARKER).count(), 1, "{helper}: {required}");
        // The marker is in the label, so the visible label shows it.
        let label_end = required.find("</label>").expect("label");
        assert!(
            required[..label_end].contains(MARKER),
            "{helper}: {required}"
        );

        let optional = render(&Changeset::new(form()), &FieldOptions::new());
        assert!(!optional.contains(MARKER), "{helper}: {optional}");
        assert!(!optional.contains("aria-required"), "{helper}: {optional}");
    }
}

#[test]
fn required_text_input_keeps_the_autumn_field_classes() {
    let html = required_text_input(&Changeset::new(form()), "email", "Email").into_string();
    assert!(html.contains(r#"class="autumn-field""#), "{html}");
    assert!(html.contains(r#"class="autumn-field__label""#), "{html}");
    assert!(html.contains(r#"class="autumn-field__input""#), "{html}");

    let html = required_text_input(&invalid("email"), "email", "Email").into_string();
    assert!(
        html.contains(r#"class="autumn-field__input autumn-field__input--invalid""#),
        "{html}"
    );
    assert!(html.contains(r#"class="autumn-field__errors""#), "{html}");
}

#[test]
fn password_never_renders_a_value() {
    let cs = Changeset::new(Form {
        secret: "hunter2".into(),
        ..form()
    });
    let html = password_input(&cs, "secret", "Secret").into_string();
    assert!(!html.contains("hunter2"), "{html}");
    let html = required_password_input(&cs, "secret", "Secret").into_string();
    assert!(!html.contains("hunter2"), "{html}");
}

// ── form_for: every FieldControl variant ─────────────────────────────────────

#[derive(Serialize)]
struct Post {
    title: String,
}

impl FormModel for Post {
    fn form_fields() -> Vec<FormField> {
        vec![FormField::new("title", "Title", FieldControl::Text, false)]
    }
}

fn every_control() -> Vec<FieldControl> {
    vec![
        FieldControl::Text,
        FieldControl::Textarea,
        FieldControl::Password,
        FieldControl::Number { step: None },
        FieldControl::Checkbox,
        FieldControl::Date,
        FieldControl::DateTime,
        FieldControl::Select {
            options: vec![("a".into(), "A".into())],
        },
        FieldControl::File,
    ]
}

#[test]
fn form_for_applies_field_options_to_every_control() {
    for control in every_control() {
        let cs = Changeset::new(Post {
            title: "Hello".into(),
        });
        let html = form_for(&cs, "/posts", "post")
            .override_field("title", control.clone())
            .field_options(
                "title",
                FieldOptions::new()
                    .hint(String::from("Keep it short"))
                    .required()
                    .disabled()
                    .data("role", "headline"),
            )
            .render()
            .into_string();
        assert!(html.contains(r#"id="title-hint""#), "{control:?}: {html}");
        assert!(html.contains("Keep it short"), "{control:?}: {html}");
        assert!(
            html.contains(r#"aria-describedby="title-hint""#),
            "{control:?}: {html}"
        );
        assert!(html.contains(MARKER), "{control:?}: {html}");
        assert!(html.contains(" disabled"), "{control:?}: {html}");
        assert!(
            html.contains(r#"data-role="headline""#),
            "{control:?}: {html}"
        );
    }
}

#[test]
fn form_for_field_options_last_call_wins() {
    let cs = Changeset::new(Post {
        title: "Hello".into(),
    });
    let html = form_for(&cs, "/posts", "post")
        .field_options("title", FieldOptions::new().hint("First"))
        .field_options("title", FieldOptions::new().hint("Second"))
        .render()
        .into_string();
    assert!(html.contains("Second"), "{html}");
    assert!(!html.contains("First"), "{html}");
}

#[test]
fn form_for_checkbox_ignores_model_required() {
    // A non-null `bool` is "required" in the model, but a required
    // checkbox means "must be checked". Only an explicit option sets it.
    #[derive(Serialize)]
    struct Flag {
        on: bool,
    }
    impl FormModel for Flag {
        fn form_fields() -> Vec<FormField> {
            vec![FormField::new("on", "On", FieldControl::Checkbox, true)]
        }
    }
    let cs = Changeset::new(Flag { on: false });
    let html = form_for(&cs, "/f", "post").render().into_string();
    assert!(!html.contains("aria-required"), "{html}");
    let html = form_for(&cs, "/f", "post")
        .field_options("on", FieldOptions::new().required())
        .render()
        .into_string();
    assert!(html.contains(r#"aria-required="true""#), "{html}");
}

// ── Linked error summary ─────────────────────────────────────────────────────

#[test]
fn error_summary_links_each_entry_to_its_field() {
    let mut errors = HashMap::new();
    errors.insert("email".to_owned(), vec!["is invalid".to_owned()]);
    errors.insert("items[0].name".to_owned(), vec!["is blank".to_owned()]);
    let out = error_summary(&Changeset::from_errors((), errors))
        .expect("summary")
        .into_string();
    assert!(
        out.contains(r##"<a href="#email">is invalid</a>"##),
        "{out}"
    );
    assert!(
        out.contains(r##"<a href="#items%5B0%5D.name">is blank</a>"##),
        "{out}"
    );
}

#[test]
fn error_summary_does_not_link_form_level_errors() {
    let mut errors = HashMap::new();
    errors.insert("__all__".to_owned(), vec!["dates overlap".to_owned()]);
    let out = error_summary(&Changeset::from_errors((), errors))
        .expect("summary")
        .into_string();
    assert!(out.contains("<li>dates overlap</li>"), "{out}");
    assert!(!out.contains("#__all__"), "{out}");
}

#[test]
fn error_summary_takes_focus_when_a_submit_fails() {
    let mut errors = HashMap::new();
    errors.insert("email".to_owned(), vec!["is invalid".to_owned()]);
    let out = error_summary(&Changeset::from_errors((), errors))
        .expect("summary")
        .into_string();
    assert!(
        out.contains(&format!(r#"id="{ERROR_SUMMARY_ID}""#)),
        "{out}"
    );
    assert!(out.contains(r#"tabindex="-1""#), "{out}");
    assert!(out.contains(" autofocus"), "{out}");
    assert!(out.contains(r#"role="alert""#), "{out}");
}

// ── Stylesheet ───────────────────────────────────────────────────────────────

#[test]
fn stylesheet_styles_hint_marker_and_user_invalid() {
    let css = autumn_web::ui::WIDGETS_COMPONENT_CSS;
    assert!(css.contains(".autumn-field__hint"), "hint rule");
    assert!(css.contains(".autumn-field__required"), "marker rule");
    assert!(
        css.contains(".autumn-field__input:user-invalid"),
        ":user-invalid rule"
    );
    assert!(
        !css.contains(".autumn-field__input:invalid"),
        "plain :invalid shows errors before the user acts"
    );
}

// ── Review follow-ups ────────────────────────────────────────────────────────

#[test]
fn hint_is_between_label_and_control_except_for_a_checkbox() {
    let opts = FieldOptions::new().hint("Help");
    let cs = Changeset::new(form());
    let html = text_input_with(&cs, "email", "Email", &opts).into_string();
    let label = html.find("</label>").expect("label");
    let hint = html.find(r#"id="email-hint""#).expect("hint");
    let input = html.find("<input").expect("input");
    assert!(label < hint && hint < input, "{html}");

    let html = checkbox_input_with(&cs, "agree", "Agree", &opts).into_string();
    let input = html.find("<input").expect("input");
    let label = html.find("<label").expect("label");
    let hint = html.find(r#"id="agree-hint""#).expect("hint");
    assert!(input < label && label < hint, "{html}");
}

#[test]
fn repeated_extra_name_keeps_the_last_value() {
    let opts = FieldOptions::new()
        .data("x", "1")
        .data("x", "2")
        .hx("on:click", "a()")
        .hx("on:click", "b()");
    let html = text_input_with(&Changeset::new(form()), "email", "Email", &opts).into_string();
    assert_eq!(html.matches("data-x=").count(), 1, "{html}");
    assert!(html.contains(r#"data-x="2""#), "{html}");
    assert_eq!(html.matches("hx-on:click=").count(), 1, "{html}");
    assert!(html.contains(r#"hx-on:click="b()""#), "{html}");
}

#[test]
fn passthrough_cannot_set_autumn_data_attributes() {
    // `closest [data-autumn-field-wrapper]` would match the input itself and
    // break the htmx swap.
    let opts = FieldOptions::new().data("autumn-field-wrapper", "email");
    let html = text_input_htmx_with(
        &Changeset::new(form()),
        "email",
        "Email",
        "/v/email",
        "_submit_token",
        &opts,
    )
    .into_string();
    assert_eq!(
        html.matches("data-autumn-field-wrapper=").count(),
        1,
        "{html}"
    );
}

#[test]
fn form_for_password_ignores_model_required() {
    #[derive(Serialize)]
    struct Account {
        password: String,
    }
    impl FormModel for Account {
        fn form_fields() -> Vec<FormField> {
            vec![FormField::new(
                "password",
                "Password",
                FieldControl::Password,
                true,
            )]
        }
    }
    let cs = Changeset::new(Account {
        password: String::new(),
    });
    let html = form_for(&cs, "/a", "post").render().into_string();
    assert!(!html.contains("aria-required"), "{html}");
    let html = form_for(&cs, "/a", "post")
        .field_options("password", FieldOptions::new().required())
        .render()
        .into_string();
    assert!(html.contains(r#"aria-required="true""#), "{html}");
}

#[test]
fn error_summary_does_not_link_nested_form_level_errors() {
    let mut errors = HashMap::new();
    errors.insert(
        "address.__all__".to_owned(),
        vec!["is incomplete".to_owned()],
    );
    errors.insert("a b&c".to_owned(), vec!["odd".to_owned()]);
    let out = error_summary(&Changeset::from_errors((), errors))
        .expect("summary")
        .into_string();
    assert!(out.contains("<li>is incomplete</li>"), "{out}");
    assert!(out.contains(r##"<a href="#a%20b&amp;c">odd</a>"##), "{out}");
}

#[derive(serde::Serialize, serde::Deserialize, validator::Validate)]
struct Parent {
    name: String,
}

#[derive(serde::Serialize, serde::Deserialize, validator::Validate)]
struct Item {
    #[validate(length(min = 1, message = "SKU is required"))]
    sku: String,
    note: String,
    qty: i32,
}

impl NestedChild for Item {
    const COLLECTION: &'static str = "items";
}

#[test]
fn row_scope_helpers_take_options_and_mask_required_on_the_template_row() {
    let pairs = vec![
        ("name".to_owned(), "Order".to_owned()),
        ("items[0][sku]".to_owned(), String::new()),
        ("items[0][note]".to_owned(), "n".to_owned()),
        ("items[0][qty]".to_owned(), "1".to_owned()),
    ];
    let nested = decode_nested_urlencoded::<Parent, Item>(&pairs).expect("decodes");
    let html = inputs_for(&nested, &InputsForOptions::default(), |row| {
        maud::html! {
            (row.text_input_with("sku", "SKU", &FieldOptions::new().hint("From the label").required()))
            (row.required_number_input("qty", "Qty"))
            (row.required_textarea_input("note", "Note"))
        }
    })
    .into_string();
    // The real row: hint, error, required, nested name, per-row id.
    assert!(html.contains(r#"name="items[0][sku]""#), "{html}");
    assert!(
        html.contains(r#"aria-describedby="items-0-sku-hint items-0-sku-error""#),
        "{html}"
    );
    assert!(html.contains(r#"id="items-0-qty""#), "{html}");
    // The blank template row (index 1) stays leaveable-empty.
    let template = &html[html.find(r#"id="items-1-sku""#).expect("template row")..];
    let template = &template[..template.find("</div></div>").unwrap_or(template.len())];
    assert!(!template.contains("aria-required"), "{template}");
    // Required on the real row: sku, qty, note.
    let real = &html[..html.find(r#"id="items-1-sku-field""#).expect("template")];
    assert_eq!(real.matches(r#"aria-required="true""#).count(), 3, "{real}");
    assert!(!html.contains(r#"aria-describedby="""#), "{html}");
}
