//!
//! Integration tests for the typed accessible UI primitives (issue #1706).
//!
//! These assert the *rendered* markup carries the accessible-name attributes
//! each primitive is responsible for. The compile-time obligation (that an
//! inaccessible primitive cannot be constructed or rendered at all) is proven
//! separately by the trybuild fixtures in `tests/compile-fail/a11y_*.rs`.
//!
#![cfg(feature = "maud")]

use autumn_web::a11y::{Button, Img, TextField};
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
