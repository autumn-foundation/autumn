//!
//! Integration tests for the typed accessible UI primitives (issue #1706).
//!
//! These assert the *rendered* markup carries the accessible-name attributes
//! each primitive is responsible for. The compile-time obligation (that an
//! inaccessible primitive cannot be constructed or rendered at all) is proven
//! separately by the trybuild fixtures in `tests/compile-fail/a11y_*.rs`.
//!
#![cfg(feature = "maud")]

use autumn_web::a11y::{
    Button, Checkbox, FileField, Img, Link, MenuItem, Select, SelectOption, TextArea, TextField,
};
use autumn_web::html;
use maud::Render;

#[test]
fn img_renders_required_alt() {
    let markup = Img::new("/logo.png", "Company logo").render().into_string();
    assert!(markup.contains("src=\"/logo.png\""), "{markup}");
    assert!(markup.contains("alt=\"Company logo\""), "{markup}");
}

#[test]
fn img_decorative_has_empty_alt() {
    let markup = Img::decorative("/divider.png").render().into_string();
    assert!(markup.contains("alt=\"\""), "{markup}");
}

#[test]
fn button_text_label_has_no_aria_label() {
    let markup = Button::new("Save").submit().render().into_string();
    assert!(markup.contains("type=\"submit\""), "{markup}");
    assert!(markup.contains(">Save<"), "{markup}");
    assert!(!markup.contains("aria-label"), "{markup}");
}

#[test]
fn icon_button_uses_aria_label() {
    let icon = html! { span aria-hidden="true" { "x" } };
    let markup = Button::icon(icon, "Close dialog").render().into_string();
    assert!(markup.contains("aria-label=\"Close dialog\""), "{markup}");
}

#[test]
fn labeled_text_field_associates_label_and_input() {
    let markup = TextField::new("email")
        .input_type("email")
        .required()
        .label("Email address")
        .render()
        .into_string();
    assert!(
        markup.contains("<label for=\"email\">Email address</label>"),
        "{markup}"
    );
    assert!(markup.contains("id=\"email\""), "{markup}");
    assert!(markup.contains("name=\"email\""), "{markup}");
    assert!(markup.contains("type=\"email\""), "{markup}");
    assert!(markup.contains("required"), "{markup}");
}

#[test]
fn labeled_text_field_carries_scaffold_attributes() {
    // The attributes raw scaffold form-fields need: presentational `class`,
    // `aria-invalid`/`aria-describedby` error wiring, and HTML5 validation
    // constraints — all expressible through `TextField` while it still requires
    // a label to render.
    let markup = TextField::new("title")
        .input_type("text")
        .class("autumn-field__input autumn-field__input--invalid")
        .aria_invalid(true)
        .described_by("title-error")
        .required()
        .aria_required()
        .minlength(3)
        .maxlength(120)
        .label("Title")
        .render()
        .into_string();
    assert!(
        markup.contains("<label for=\"title\">Title</label>"),
        "{markup}"
    );
    assert!(
        markup.contains("class=\"autumn-field__input autumn-field__input--invalid\""),
        "{markup}"
    );
    assert!(markup.contains("aria-invalid=\"true\""), "{markup}");
    assert!(
        markup.contains("aria-describedby=\"title-error\""),
        "{markup}"
    );
    assert!(markup.contains("aria-required=\"true\""), "{markup}");
    assert!(markup.contains("minlength=\"3\""), "{markup}");
    assert!(markup.contains("maxlength=\"120\""), "{markup}");
    assert!(markup.contains("required"), "{markup}");
}

#[test]
fn labeled_text_field_carries_numeric_constraints() {
    let markup = TextField::new("ratio")
        .input_type("number")
        .min("0")
        .max("1")
        .step("any")
        .aria_label("Ratio")
        .render()
        .into_string();
    assert!(markup.contains("type=\"number\""), "{markup}");
    assert!(markup.contains("min=\"0\""), "{markup}");
    assert!(markup.contains("max=\"1\""), "{markup}");
    assert!(markup.contains("step=\"any\""), "{markup}");
    assert!(markup.contains("aria-label=\"Ratio\""), "{markup}");
}

#[test]
fn text_field_aria_label_and_labelled_by_variants() {
    let aria = TextField::new("q")
        .aria_label("Search")
        .render()
        .into_string();
    assert!(aria.contains("aria-label=\"Search\""), "{aria}");

    let by = TextField::new("q")
        .labelled_by("search-heading")
        .render()
        .into_string();
    assert!(by.contains("aria-labelledby=\"search-heading\""), "{by}");
}

#[test]
fn link_renders_href_and_visible_text() {
    let markup = Link::new("/about", "About us").render().into_string();
    assert!(markup.contains("href=\"/about\""), "{markup}");
    assert!(markup.contains(">About us<"), "{markup}");
    assert!(!markup.contains("aria-label"), "{markup}");
}

#[test]
fn link_new_tab_sets_rel_noopener() {
    let markup = Link::new("https://example.com", "Docs")
        .new_tab()
        .render()
        .into_string();
    assert!(markup.contains("target=\"_blank\""), "{markup}");
    assert!(markup.contains("rel=\"noopener noreferrer\""), "{markup}");
}

#[test]
fn icon_link_uses_aria_label() {
    let glyph = html! { span aria-hidden="true" { "gh" } };
    let markup = Link::icon("https://example.com", glyph, "GitHub")
        .render()
        .into_string();
    assert!(markup.contains("href=\"https://example.com\""), "{markup}");
    assert!(markup.contains("aria-label=\"GitHub\""), "{markup}");
}

#[test]
fn menu_item_defaults_to_button_with_role() {
    let markup = MenuItem::new("Settings").render().into_string();
    assert!(markup.contains("role=\"menuitem\""), "{markup}");
    assert!(markup.contains("type=\"button\""), "{markup}");
    assert!(markup.contains(">Settings<"), "{markup}");
    assert!(!markup.contains("aria-label"), "{markup}");
}

#[test]
fn menu_item_with_href_renders_link() {
    let markup = MenuItem::new("Home").href("/").render().into_string();
    assert!(markup.contains("role=\"menuitem\""), "{markup}");
    assert!(markup.contains("href=\"/\""), "{markup}");
    assert!(markup.contains(">Home<"), "{markup}");
}

#[test]
fn icon_menu_item_uses_aria_label() {
    let cog = html! { span aria-hidden="true" { "*" } };
    let markup = MenuItem::new("Settings").icon(cog).render().into_string();
    assert!(markup.contains("role=\"menuitem\""), "{markup}");
    assert!(markup.contains("aria-label=\"Settings\""), "{markup}");
}

#[test]
fn text_area_carries_value_as_text_content_not_attribute() {
    let markup = TextArea::new("bio")
        .value("Hello world")
        .rows(6)
        .cols(40)
        .maxlength(500)
        .label("Bio")
        .render()
        .into_string();
    assert!(
        markup.contains("<label for=\"bio\">Bio</label>"),
        "{markup}"
    );
    // The value rides as the element's text content, never a `value=` attribute.
    assert!(
        markup.contains("<textarea") && markup.contains(">Hello world</textarea>"),
        "{markup}"
    );
    assert!(
        !markup.contains("value="),
        "value must not be an attribute: {markup}"
    );
    assert!(markup.contains("rows=\"6\""), "{markup}");
    assert!(markup.contains("cols=\"40\""), "{markup}");
    assert!(markup.contains("maxlength=\"500\""), "{markup}");
}

#[test]
fn text_area_aria_label_variant() {
    let markup = TextArea::new("note")
        .aria_label("Note")
        .render()
        .into_string();
    assert!(markup.contains("aria-label=\"Note\""), "{markup}");
    assert!(!markup.contains("<label"), "{markup}");
}

#[test]
fn select_renders_each_option_with_value_label_and_selected() {
    let markup = Select::new("role")
        .option("", "— Select —")
        .options([
            SelectOption::new("admin", "Admin"),
            SelectOption::new("user", "User"),
        ])
        .selected_value("user")
        .required()
        .aria_required()
        .label("Role")
        .render()
        .into_string();
    assert!(
        markup.contains("<label for=\"role\">Role</label>"),
        "{markup}"
    );
    assert!(markup.contains("<select"), "{markup}");
    assert!(markup.contains("id=\"role\""), "{markup}");
    assert!(markup.contains("aria-required=\"true\""), "{markup}");
    assert!(markup.contains("required"), "{markup}");
    // Each option carries its value + visible label.
    assert!(markup.contains("value=\"admin\""), "{markup}");
    assert!(markup.contains(">Admin<"), "{markup}");
    assert!(markup.contains("value=\"user\""), "{markup}");
    assert!(markup.contains(">User<"), "{markup}");
    // The matching option (and only it) is selected.
    assert!(
        markup.contains("value=\"user\" selected"),
        "user option must be selected: {markup}"
    );
    assert!(
        !markup.contains("value=\"admin\" selected"),
        "admin option must not be selected: {markup}"
    );
}

#[test]
fn select_placeholder_option_can_be_disabled() {
    let markup = Select::new("role")
        .options([SelectOption::new("", "— Select —").disabled()])
        .labelled_by("role-heading")
        .render()
        .into_string();
    assert!(
        markup.contains("aria-labelledby=\"role-heading\""),
        "{markup}"
    );
    assert!(markup.contains("disabled"), "{markup}");
}

#[test]
fn checkbox_renders_type_checked_and_label_association() {
    let markup = Checkbox::new("accept_terms")
        .value("true")
        .checked(true)
        .required()
        .label("I accept the terms")
        .render()
        .into_string();
    assert!(markup.contains("type=\"checkbox\""), "{markup}");
    assert!(markup.contains("id=\"accept_terms\""), "{markup}");
    assert!(markup.contains("value=\"true\""), "{markup}");
    assert!(markup.contains("checked"), "{markup}");
    assert!(markup.contains("required"), "{markup}");
    // The `<label for>` associates with the input by id.
    assert!(
        markup.contains("<label for=\"accept_terms\">I accept the terms</label>"),
        "{markup}"
    );
    // Layout: input precedes the label for a checkbox.
    let input_pos = markup.find("<input").expect("input present");
    let label_pos = markup.find("<label").expect("label present");
    assert!(input_pos < label_pos, "input must precede label: {markup}");
}

#[test]
fn checkbox_unchecked_by_default() {
    let markup = Checkbox::new("subscribe")
        .aria_label("Subscribe")
        .render()
        .into_string();
    assert!(markup.contains("type=\"checkbox\""), "{markup}");
    assert!(!markup.contains("checked"), "{markup}");
    assert!(markup.contains("aria-label=\"Subscribe\""), "{markup}");
}

#[test]
fn file_field_renders_type_accept_and_multiple() {
    let markup = FileField::new("docs")
        .accept("application/pdf")
        .multiple()
        .aria_label("Documents")
        .render()
        .into_string();
    assert!(markup.contains("type=\"file\""), "{markup}");
    assert!(markup.contains("accept=\"application/pdf\""), "{markup}");
    assert!(markup.contains("multiple"), "{markup}");
    assert!(markup.contains("aria-label=\"Documents\""), "{markup}");
}

#[test]
fn visible_labeled_file_field_emits_no_competing_aria_label() {
    // The key #1933 fix: a caller's visible `<label for>` is the accessible name,
    // so the field emits NO derived `aria-label` to override it (the precedence
    // caveat of `direct_upload_input`).
    let markup = FileField::new("avatar")
        .accept("image/*")
        .label("Avatar")
        .render()
        .into_string();
    assert!(
        markup.contains("<label for=\"avatar\">Avatar</label>"),
        "{markup}"
    );
    assert!(markup.contains("type=\"file\""), "{markup}");
    assert!(
        !markup.contains("aria-label"),
        "a visible-labeled file field must not emit a competing aria-label: {markup}"
    );
}

#[test]
fn hx_escape_hatch_renders_attributes_in_order() {
    let markup = TextField::new("username")
        .hx("post", "/x")
        .hx("params", "not _submit_token")
        .label("Username")
        .render()
        .into_string();
    assert!(markup.contains("hx-post=\"/x\""), "{markup}");
    assert!(
        markup.contains("hx-params=\"not _submit_token\""),
        "{markup}"
    );
    // Insertion order is preserved.
    let post_pos = markup.find("hx-post").expect("hx-post present");
    let params_pos = markup.find("hx-params").expect("hx-params present");
    assert!(
        post_pos < params_pos,
        "hx order must be preserved: {markup}"
    );
}

#[test]
fn hx_escape_hatch_is_a_noop_when_empty() {
    // An empty hx set must render byte-identically to no escape hatch at all.
    let with_hx = TextField::new("email")
        .label("Email")
        .render()
        .into_string();
    let plain = TextField::new("email")
        .label("Email")
        .render()
        .into_string();
    assert_eq!(with_hx, plain, "empty hx must not change output");
    assert!(!with_hx.contains("hx-"), "{with_hx}");
}

#[test]
fn hx_escape_hatch_escapes_attribute_values() {
    let markup = Select::new("q")
        .option("a", "A")
        .hx("vals", "{\"x\": \"<b>&\"}")
        .aria_label("Query")
        .render()
        .into_string();
    // The value is HTML-attribute-escaped, never injected raw.
    assert!(markup.contains("hx-vals="), "{markup}");
    assert!(markup.contains("&lt;b&gt;"), "{markup}");
    assert!(markup.contains("&amp;"), "{markup}");
    assert!(
        !markup.contains("<b>&\""),
        "raw value must not leak: {markup}"
    );
}

#[test]
fn hx_lands_on_visible_labeled_control_not_the_label() {
    // Decisive regression guard (PR #1946 review). Every primitive renders a
    // visible `<label>` as a *sibling* of the control, and `with_hx_attrs` is
    // applied to the control sub-element ALONE — so the "first `>`" the splice
    // targets is the control's own opening tag, never the preceding `<label>`.
    // This pins that: with a visible label present, the `hx-*` attributes must
    // land inside the CONTROL's opening tag and must NOT appear on the `<label>`.

    // Returns the opening-tag substring `<tag …>` for the first `<tag` in `s`.
    fn opening_tag<'a>(s: &'a str, tag: &str) -> &'a str {
        let start = s
            .find(tag)
            .unwrap_or_else(|| panic!("{tag} present in: {s}"));
        let rel_end = s[start..].find('>').expect("opening tag closes with >");
        &s[start..=start + rel_end]
    }

    // `<label>` precedes `<textarea>` / `<select>` / file `<input>` / text
    // `<input>`; the checkbox emits `<input>` before `<label>`. In every case
    // the splice must target the control tag, not the label tag.
    let cases: [(&str, String); 5] = [
        (
            "<textarea",
            TextArea::new("bio")
                .label("Bio")
                .hx("post", "/v")
                .render()
                .into_string(),
        ),
        (
            "<select",
            Select::new("role")
                .option("admin", "Admin")
                .label("Role")
                .hx("post", "/v")
                .render()
                .into_string(),
        ),
        (
            "<input",
            FileField::new("avatar")
                .label("Avatar")
                .hx("post", "/v")
                .render()
                .into_string(),
        ),
        (
            "<input",
            TextField::new("email")
                .label("Email")
                .hx("post", "/v")
                .render()
                .into_string(),
        ),
        (
            "<input",
            Checkbox::new("terms")
                .label("I accept")
                .hx("post", "/v")
                .render()
                .into_string(),
        ),
    ];

    for (control_tag, markup) in &cases {
        assert!(
            opening_tag(markup, control_tag).contains("hx-post=\"/v\""),
            "hx-post must be inside the {control_tag} opening tag: {markup}"
        );
        assert!(
            !opening_tag(markup, "<label").contains("hx-post"),
            "hx-post must NOT be spliced onto the <label>: {markup}"
        );
    }
}

#[test]
fn selected_value_is_exclusive_and_clears_prior_selection() {
    // FIX (PR #1946 review): `<select>` is single-select, so `selected_value`
    // must authoritatively set exactly one selected option and clear any option
    // previously marked via `SelectOption::selected()`. Otherwise the rendered
    // markup carries two `selected` options (invalid HTML).
    let markup = Select::new("role")
        .options([
            SelectOption::new("admin", "Admin").selected(),
            SelectOption::new("user", "User"),
            SelectOption::new("other", "Other"),
        ])
        .selected_value("other")
        .aria_label("Role")
        .render()
        .into_string();
    // Exactly one `selected` occurrence, on the "other" option.
    assert_eq!(
        markup.matches(" selected").count(),
        1,
        "exactly one option may be selected: {markup}"
    );
    assert!(
        markup.contains("value=\"other\" selected"),
        "the chosen option must be selected: {markup}"
    );
    assert!(
        !markup.contains("value=\"admin\" selected"),
        "the pre-marked option must no longer be selected: {markup}"
    );
}

#[test]
fn hx_valid_multi_attr_chain_renders_all_in_order() {
    // A valid chain of `hx-*` attributes still renders every attribute, in
    // insertion order, with escaped values.
    let markup = Select::new("q")
        .option("a", "A")
        .hx("post", "/submit")
        .hx("target", "#result")
        .hx("swap", "outerHTML")
        .aria_label("Query")
        .render()
        .into_string();
    assert!(markup.contains("hx-post=\"/submit\""), "{markup}");
    assert!(markup.contains("hx-target=\"#result\""), "{markup}");
    assert!(markup.contains("hx-swap=\"outerHTML\""), "{markup}");
    let post = markup.find("hx-post").expect("hx-post present");
    let target = markup.find("hx-target").expect("hx-target present");
    let swap = markup.find("hx-swap").expect("hx-swap present");
    assert!(post < target && target < swap, "order preserved: {markup}");
}

#[test]
fn hx_rejects_attribute_name_injection() {
    // FIX (PR #1946 review): a name containing whitespace or `=` — which
    // `maud::Escaper` does NOT escape — must never break out into a separate,
    // injected attribute. In debug builds this trips a `debug_assert!`; in
    // release builds the malformed attribute is dropped (fail-safe). Either way,
    // no standalone `autofocus` / broken-out `="/v"` may appear in the output.
    let build = || {
        Select::new("q")
            .option("a", "A")
            .hx("post autofocus", "/v")
            .aria_label("Query")
            .render()
            .into_string()
    };

    #[cfg(debug_assertions)]
    {
        // The debug_assert! surfaces the developer error as a panic.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(build));
        assert!(
            result.is_err(),
            "an invalid hx name must trip debug_assert! in debug builds"
        );
    }

    #[cfg(not(debug_assertions))]
    {
        let markup = build();
        assert!(
            !markup.contains("autofocus"),
            "the injected attribute must not be emitted: {markup}"
        );
        assert!(
            !markup.contains("=\"/v\""),
            "the broken-out value must not be emitted: {markup}"
        );
        assert!(
            !markup.contains("hx-post autofocus"),
            "the malformed name must not be emitted: {markup}"
        );
    }
}

#[test]
fn splices_inside_html_block() {
    let page = html! {
        (Img::new("/logo.svg", "Autumn logo"))
        (TextField::new("email").input_type("email").label("Email address"))
        (Button::new("Save").submit())
    };
    let s = page.into_string();
    assert!(s.contains("alt=\"Autumn logo\""), "{s}");
    assert!(
        s.contains("<label for=\"email\">Email address</label>"),
        "{s}"
    );
    assert!(s.contains(">Save<"), "{s}");
}
