//! OTP input and password reveal / strength (issue #3412).
//!
//! Run with `cargo test --test integration_tests form_otp_password`.

#![cfg(feature = "maud")]

use std::collections::HashMap;

use autumn_web::form::{
    Changeset, FieldContext, FieldControl, FieldOptions, FieldRenderer, FormField, FormModel,
    OtpFormat, custom_input_with, form_for, otp_input, otp_input_with, password_input,
    password_input_with, required_otp_input, text_input_with, textarea_input_with,
};
use proptest::prelude::*;
use serde::Serialize;

#[derive(Serialize)]
struct Login {
    password: String,
    code: String,
}

fn login() -> Changeset<Login> {
    Changeset::new(Login {
        password: "hunter2".into(),
        code: "123456".into(),
    })
}

fn failed(field: &str, message: &str) -> Changeset<serde_json::Value> {
    let mut errors = HashMap::new();
    errors.insert(field.to_owned(), vec![message.to_owned()]);
    Changeset::from_errors(
        serde_json::json!({ "password": "hunter2", "code": "123456" }),
        errors,
    )
}

fn count(html: &str, needle: &str) -> usize {
    html.matches(needle).count()
}

// ── OtpFormat ───────────────────────────────────────────────────────────

#[test]
fn otp_format_defaults_to_six_digits_and_no_group() {
    let format = OtpFormat::default();
    assert_eq!(format.digit_count(), 6);
    assert_eq!(format.group_size(), None);
    assert_eq!(format.cells(), 6);
}

#[test]
fn otp_format_clamps_the_digit_count() {
    assert_eq!(OtpFormat::digits(0).digit_count(), 4);
    assert_eq!(OtpFormat::digits(3).digit_count(), 4);
    assert_eq!(OtpFormat::digits(8).digit_count(), 8);
    assert_eq!(OtpFormat::digits(11).digit_count(), 10);
    assert_eq!(OtpFormat::digits(u8::MAX).digit_count(), 10);
}

#[test]
fn otp_format_drops_a_group_that_does_not_split_the_code() {
    assert_eq!(OtpFormat::digits(6).group(3).group_size(), Some(3));
    assert_eq!(OtpFormat::digits(6).group(1).group_size(), None);
    assert_eq!(OtpFormat::digits(6).group(6).group_size(), None);
    assert_eq!(OtpFormat::digits(8).group(6).group_size(), None);
    assert_eq!(OtpFormat::digits(6).group(0).group_size(), None);
}

#[test]
fn otp_format_counts_one_separator_cell_between_groups() {
    assert_eq!(OtpFormat::digits(6).group(3).cells(), 7);
    assert_eq!(OtpFormat::digits(8).group(4).cells(), 9);
    assert_eq!(OtpFormat::digits(7).group(3).cells(), 9);
    assert_eq!(OtpFormat::digits(10).group(2).cells(), 14);
}

proptest! {
    /// The same rules as `verification/otp_format.rs`.
    #[test]
    fn otp_format_stays_in_bounds(digits in any::<u8>(), group in any::<u8>()) {
        let format = OtpFormat::digits(digits).group(group);
        let n = format.digit_count();
        prop_assert!((4..=10).contains(&n));
        if let Some(g) = format.group_size() {
            prop_assert!((2..=5).contains(&g) && g < n);
            prop_assert_eq!(format.cells(), n + (n - 1) / g);
        } else {
            prop_assert_eq!(format.cells(), n);
        }
        prop_assert!(format.cells() <= 14);
    }

    /// The markup has one box per digit and one separator per group break.
    #[test]
    fn otp_markup_matches_the_format(digits in 0u8..16, group in 0u8..8) {
        let format = OtpFormat::digits(digits).group(group);
        let html = otp_input_with(&login(), "code", "Code", format, &FieldOptions::new())
            .into_string();
        let n = usize::from(format.digit_count());
        let seps = usize::from(format.cells() - format.digit_count());
        prop_assert_eq!(count(&html, r#"<span class="autumn-otp__cell"></span>"#), n);
        prop_assert_eq!(count(&html, r#"<span class="autumn-otp__sep"></span>"#), seps);
        let pattern = format!(r#"pattern="[0-9]{{{n}}}""#);
        prop_assert!(html.contains(&pattern), "{}", html);
    }
}

// ── otp_input ───────────────────────────────────────────────────────────

#[test]
fn otp_input_is_one_numeric_one_time_code_input() {
    let html = otp_input(&login(), "code", "Code").into_string();
    assert_eq!(count(&html, "<input"), 1, "{html}");
    assert!(
        html.contains(
            r#"<input type="text" id="code" name="code" class="autumn-field__input autumn-otp__input" aria-invalid="false" autocomplete="one-time-code" inputmode="numeric" pattern="[0-9]{6}" maxlength="6" data-autumn-otp="6">"#
        ),
        "{html}"
    );
    assert!(
        html.starts_with(r#"<div id="code-field" class="autumn-field autumn-field--otp"><label for="code" class="autumn-field__label">Code</label><div class="autumn-otp">"#),
        "{html}"
    );
    assert_eq!(
        count(&html, r#"<span class="autumn-otp__cell"></span>"#),
        6,
        "{html}"
    );
    assert!(
        html.contains(r#"<span class="autumn-otp__cells" aria-hidden="true">"#),
        "{html}"
    );
}

#[test]
fn otp_input_never_renders_the_value() {
    let html = otp_input(&login(), "code", "Code").into_string();
    assert!(!html.contains("123456"), "{html}");
    assert!(!html.contains("value="), "{html}");
}

#[test]
fn otp_input_has_a_maxlength_for_a_page_with_no_js() {
    // The runtime removes it, so it does not cut a paste of `123-456`.
    let html = otp_input(&login(), "code", "Code").into_string();
    assert!(html.contains(r#" maxlength="6""#), "{html}");
    let grouped = otp_input_with(
        &login(),
        "code",
        "Code",
        OtpFormat::digits(8).group(4),
        &FieldOptions::new(),
    )
    .into_string();
    assert!(grouped.contains(r#" maxlength="8""#), "{grouped}");
}

#[test]
fn required_otp_input_marks_the_field() {
    let html = required_otp_input(&login(), "code", "Code").into_string();
    assert!(html.contains(r#" required aria-required="true""#), "{html}");
    assert!(html.contains("autumn-field__required"), "{html}");
}

#[test]
fn otp_input_with_group_sends_the_group_and_separators() {
    let html = otp_input_with(
        &login(),
        "code",
        "Code",
        OtpFormat::digits(6).group(3),
        &FieldOptions::new(),
    )
    .into_string();
    assert!(html.contains(r#"data-autumn-otp-group="3""#), "{html}");
    // The separator sits between the third and the fourth box.
    assert!(
        html.contains(concat!(
            r#"<span class="autumn-otp__cell"></span><span class="autumn-otp__cell"></span>"#,
            r#"<span class="autumn-otp__cell"></span><span class="autumn-otp__sep"></span>"#,
            r#"<span class="autumn-otp__cell"></span>"#
        )),
        "{html}"
    );
    // The server pattern is for the digits. JS changes it when it groups.
    assert!(html.contains(r#"pattern="[0-9]{6}""#), "{html}");
}

#[test]
fn otp_input_lets_an_option_replace_a_default() {
    // The app owns its choice. Each attribute shows one time only.
    let html = otp_input_with(
        &login(),
        "code",
        "Code",
        OtpFormat::default(),
        &FieldOptions::new()
            .autocomplete("off")
            .inputmode("text")
            .pattern("[a-z]+")
            .placeholder("000000")
            .minlength(2)
            .maxlength(3),
    )
    .into_string();
    for (name, value) in [
        ("autocomplete", "off"),
        ("inputmode", "text"),
        ("pattern", "[a-z]+"),
        ("placeholder", "000000"),
        ("minlength", "2"),
        ("maxlength", "3"),
    ] {
        assert_eq!(count(&html, &format!(" {name}=")), 1, "{name}: {html}");
        assert!(
            html.contains(&format!(r#" {name}="{value}""#)),
            "{name}: {html}"
        );
    }
}

#[test]
fn otp_input_keeps_hint_disabled_and_passthrough() {
    let html = otp_input_with(
        &login(),
        "code",
        "Code",
        OtpFormat::default(),
        &FieldOptions::new()
            .hint("Open your app.")
            .disabled()
            .data("test-id", "otp"),
    )
    .into_string();
    assert!(html.contains(r#"aria-describedby="code-hint""#), "{html}");
    assert!(html.contains(r#"<p id="code-hint""#), "{html}");
    assert!(html.contains(" disabled"), "{html}");
    assert!(html.contains(r#"data-test-id="otp""#), "{html}");
}

#[test]
fn otp_input_shows_errors_after_the_boxes() {
    let cs = failed("code", "Wrong code");
    let html = otp_input(&cs, "code", "Code").into_string();
    assert!(html.contains("autumn-field__input--invalid"), "{html}");
    assert!(html.contains(r#"aria-invalid="true""#), "{html}");
    assert!(html.contains(r#"aria-describedby="code-error""#), "{html}");
    let boxes = html.find("autumn-otp__cells").expect("boxes");
    let error = html.find(r#"id="code-error""#).expect("error");
    assert!(boxes < error, "{html}");
}

#[test]
fn form_for_renders_an_otp_control() {
    #[derive(Serialize)]
    struct Verify {
        code: String,
    }
    impl FormModel for Verify {
        fn form_fields() -> Vec<FormField> {
            vec![FormField::new(
                "code",
                "Code",
                FieldControl::Otp(OtpFormat::digits(8).group(4)),
                true,
            )]
        }
    }
    let cs = Changeset::new(Verify {
        code: String::new(),
    });
    let html = form_for(&cs, "/verify", "post").render().into_string();
    assert!(html.contains(r#"data-autumn-otp="8""#), "{html}");
    assert!(html.contains(r#"data-autumn-otp-group="4""#), "{html}");
    assert!(html.contains(r#" required aria-required="true""#), "{html}");
}

// ── password_input ──────────────────────────────────────────────────────

#[test]
fn password_input_with_no_option_is_unchanged() {
    let html = password_input(&login(), "password", "Password").into_string();
    assert_eq!(
        html,
        concat!(
            r#"<div id="password-field" class="autumn-field">"#,
            r#"<label for="password" class="autumn-field__label">Password</label>"#,
            r#"<input type="password" id="password" name="password" class="autumn-field__input" aria-invalid="false">"#,
            "</div>"
        )
    );
}

#[test]
fn password_autocomplete_options_set_the_token() {
    let current = password_input_with(
        &login(),
        "password",
        "Password",
        &FieldOptions::new().current_password(),
    )
    .into_string();
    assert!(
        current.contains(r#"autocomplete="current-password""#),
        "{current}"
    );
    let new = password_input_with(
        &login(),
        "password",
        "Password",
        &FieldOptions::new().new_password(),
    )
    .into_string();
    assert!(new.contains(r#"autocomplete="new-password""#), "{new}");
    assert_eq!(count(&new, "autocomplete="), 1, "{new}");
}

#[test]
fn password_reveal_adds_a_hidden_toggle_button() {
    let html = password_input_with(
        &login(),
        "password",
        "Password",
        &FieldOptions::new().reveal(),
    )
    .into_string();
    assert!(
        html.contains(concat!(
            r#"<div class="autumn-password">"#,
            r#"<input type="password" id="password" name="password" class="autumn-field__input" aria-invalid="false" spellcheck="false" autocapitalize="off">"#,
            r#"<button type="button" class="autumn-password__reveal" aria-controls="password" aria-pressed="false" data-autumn-reveal hidden>Show password</button>"#,
            "</div>"
        )),
        "{html}"
    );
    // The value is never rendered.
    assert!(!html.contains("hunter2"), "{html}");
}

#[test]
fn password_reveal_label_sets_the_button_text() {
    let html = password_input_with(
        &login(),
        "password",
        "Mot de passe",
        &FieldOptions::new().reveal_label("Afficher <le> mot de passe"),
    )
    .into_string();
    assert!(
        html.contains("data-autumn-reveal hidden>Afficher &lt;le&gt; mot de passe</button>"),
        "{html}"
    );
}

#[test]
fn password_reveal_button_is_disabled_with_the_field() {
    let html = password_input_with(
        &login(),
        "password",
        "Password",
        &FieldOptions::new().reveal().disabled(),
    )
    .into_string();
    assert!(
        html.contains("data-autumn-reveal hidden disabled>"),
        "{html}"
    );
}

#[test]
fn password_strength_hint_is_a_live_region_in_describedby() {
    let html = password_input_with(
        &login(),
        "password",
        "Password",
        &FieldOptions::new()
            .new_password()
            .hint("Use 12 or more characters.")
            .strength_hint(),
    )
    .into_string();
    assert!(
        html.contains(r#"aria-describedby="password-hint password-strength""#),
        "{html}"
    );
    assert!(
        html.contains(concat!(
            r#"<p id="password-strength" class="autumn-field__strength" aria-live="polite" "#,
            r#"data-autumn-strength="[&quot;Strength: weak&quot;,&quot;Strength: fair&quot;,"#,
            r#"&quot;Strength: good&quot;,&quot;Strength: strong&quot;]"></p>"#
        )),
        "{html}"
    );
}

#[test]
fn password_strength_comes_before_the_error_in_describedby() {
    let cs = failed("password", "Too short");
    let html = password_input_with(
        &cs,
        "password",
        "Password",
        &FieldOptions::new().strength_hint(),
    )
    .into_string();
    assert!(
        html.contains(r#"aria-describedby="password-strength password-error""#),
        "{html}"
    );
    let strength = html.find(r#"id="password-strength""#).expect("strength");
    let error = html.find(r#"id="password-error""#).expect("error");
    assert!(strength < error, "{html}");
}

#[test]
fn password_strength_labels_are_escaped_json() {
    let html = password_input_with(
        &login(),
        "password",
        "Password",
        &FieldOptions::new().strength_labels(["Faible", "Moyen \"ok\"", "Bon", "Fort"]),
    )
    .into_string();
    assert!(
        html.contains(
            r#"data-autumn-strength="[&quot;Faible&quot;,&quot;Moyen \&quot;ok\&quot;&quot;,&quot;Bon&quot;,&quot;Fort&quot;]""#
        ),
        "{html}"
    );
}

/// The options the auth generator sends for signup and reset.
#[test]
fn password_signup_options_render_in_order() {
    let html = password_input_with(
        &login(),
        "password",
        "Password",
        &FieldOptions::new()
            .required()
            .new_password()
            .minlength(12)
            .reveal()
            .strength_hint(),
    )
    .into_string();
    assert!(
        html.contains(concat!(
            r#"<div class="autumn-password"><input type="password" id="password" name="password" required aria-required="true" class="autumn-field__input" aria-invalid="false" aria-describedby="password-strength" spellcheck="false" autocapitalize="off" autocomplete="new-password" minlength="12">"#,
            r#"<button type="button" class="autumn-password__reveal" aria-controls="password" aria-pressed="false" data-autumn-reveal hidden>Show password</button></div>"#,
            r#"<p id="password-strength" class="autumn-field__strength""#
        )),
        "{html}"
    );
    assert!(html.ends_with("></p></div>"), "{html}");
}

#[test]
fn password_without_reveal_has_no_spellcheck_attribute() {
    // A `type="password"` input is never spellchecked, so the markup stays.
    let html = password_input_with(
        &login(),
        "password",
        "Password",
        &FieldOptions::new().strength_hint(),
    )
    .into_string();
    assert!(!html.contains("spellcheck"), "{html}");
}

#[test]
fn password_options_are_ignored_on_other_controls() {
    let opts = FieldOptions::new().reveal().strength_hint();
    let html = text_input_with(&login(), "code", "Code", &opts).into_string();
    assert!(!html.contains("autumn-password"), "{html}");
    assert!(!html.contains("strength"), "{html}");
    assert!(!html.contains("aria-describedby"), "{html}");
}

#[test]
fn form_for_passes_reveal_to_a_password_field() {
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
    let html = form_for(&cs, "/account", "post")
        .field_options("password", FieldOptions::new().reveal().new_password())
        .render()
        .into_string();
    assert!(html.contains("data-autumn-reveal"), "{html}");
    assert!(html.contains(r#"autocomplete="new-password""#), "{html}");
}

// ── minlength / maxlength ───────────────────────────────────────────────

#[test]
fn length_options_render_on_text_entry_controls() {
    let opts = FieldOptions::new().minlength(8).maxlength(64);
    for html in [
        password_input_with(&login(), "password", "Password", &opts).into_string(),
        text_input_with(&login(), "code", "Code", &opts).into_string(),
        textarea_input_with(&login(), "code", "Code", &opts).into_string(),
    ] {
        assert!(html.contains(r#" minlength="8""#), "{html}");
        assert!(html.contains(r#" maxlength="64""#), "{html}");
    }
}

/// A custom control that writes its own `maxlength`.
struct Short;

impl FieldRenderer for Short {
    fn name(&self) -> &'static str {
        "short"
    }

    fn render(&self, field: &FieldContext<'_>) -> maud::Markup {
        field.control(
            "input",
            &[("type", "text"), ("minlength", "1"), ("maxlength", "20")],
            maud::html! {},
        )
    }
}

#[test]
fn length_options_replace_a_custom_control_attribute() {
    let html = custom_input_with(
        &login(),
        "code",
        "Code",
        &Short,
        &FieldOptions::new().minlength(2).maxlength(8),
    )
    .into_string();
    assert_eq!(count(&html, " minlength="), 1, "{html}");
    assert_eq!(count(&html, " maxlength="), 1, "{html}");
    assert!(html.contains(r#" maxlength="8""#), "{html}");
}
