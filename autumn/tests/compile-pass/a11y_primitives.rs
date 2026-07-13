//! Issue #1706: the accessible forms of the typed a11y primitives compile.
//!
//! Each primitive can only be constructed with an accessible name, and only a
//! labeled `TextField` can be rendered — these are exactly those forms.

use autumn_web::a11y::{Button, ButtonType, Img, Link, MenuItem, TextField};
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

    // Link carries its visible text as the accessible name; href + text are both
    // required.
    let _about = Link::new("/about", "About us").class("nav").render();

    // A link opening in a new tab sets rel="noopener noreferrer".
    let _docs = Link::new("https://example.com", "Docs").new_tab().render();

    // Icon-only link routes the accessible name to aria-label.
    let glyph = html! { span aria-hidden="true" { "gh" } };
    let _github = Link::icon("https://example.com", glyph, "GitHub").render();

    // Menu item defaults to a button with role="menuitem".
    let _settings = MenuItem::new("Settings").render();

    // A menu item with an href renders as a link-style item.
    let _home = MenuItem::new("Home").href("/").render();

    // Icon-only menu item keeps its accessible name via aria-label.
    let cog = html! { span aria-hidden="true" { "*" } };
    let _icon_item = MenuItem::new("Settings").icon(cog).class("item").render();
}
