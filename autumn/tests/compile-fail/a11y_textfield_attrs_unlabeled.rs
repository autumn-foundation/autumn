//! Issue #1706: the presentational/validation attributes added to `TextField`
//! (class, aria-invalid, aria-describedby, aria-required, minlength/maxlength,
//! min/max/step) do NOT lift the compile-time label obligation. Setting every
//! one of them on a `TextField<NoLabel>` and then calling `.render()` — without
//! any `.label(..)`/`.aria_label(..)`/`.labelled_by(..)` — is still a compile
//! error, because only `TextField<Labeled>` implements `Render`. WCAG 1.3.1 /
//! 3.3.2 / 4.1.2.

use autumn_web::a11y::TextField;
use maud::Render;

fn main() {
    let _markup = TextField::new("title")
        .input_type("text")
        .class("autumn-field__input autumn-field__input--invalid")
        .aria_invalid(true)
        .described_by("title-error")
        .required()
        .aria_required()
        .minlength(3)
        .maxlength(120)
        .render();
}
