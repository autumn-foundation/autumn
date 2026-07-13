//! Issue #1706: the accessible forms of the typed a11y primitives compile.
//!
//! Each primitive can only be constructed with an accessible name, and only a
//! labeled `TextField` can be rendered — these are exactly those forms.

use autumn_web::a11y::{Button, ButtonType, Img, TextField};
use autumn_web::html;
use maud::Render;

fn main() {
    // Informative image: alt is a required positional argument.
    let _informative = Img::new("/logo.png", "Company logo")
        .class("logo")
        .width(120)
        .height(40)
        .render();

    // Decorative image: explicit empty alt, opt-in.
    let _decorative = Img::decorative("/divider.png").render();

    // Text button carries its own accessible name.
    let _save = Button::new("Save").submit().render();

    // Icon button routes the accessible name to aria-label.
    let icon = html! { span aria-hidden="true" { "x" } };
    let _close = Button::icon(icon, "Close").kind(ButtonType::Button).render();

    // A labeled text field is renderable; the label is associated with the input.
    let _email = TextField::new("email")
        .input_type("email")
        .required()
        .label("Email address")
        .render();

    // aria-label and aria-labelledby are equally valid labeling strategies.
    let _search = TextField::new("q").aria_label("Search").render();
    let _named = TextField::new("q").labelled_by("search-heading").render();
}
