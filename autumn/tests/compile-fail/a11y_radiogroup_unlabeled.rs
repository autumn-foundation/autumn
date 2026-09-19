//! Issue #1706: an unlabeled `RadioGroup` cannot be rendered. WCAG 1.3.1 /
//! 3.3.2 / 4.1.2 — a radio group needs a name for the group itself, not just
//! for each choice, so only `RadioGroup<Labeled>` implements `Render`.
//!
//! The presentational, validation and htmx setters live on both states, so
//! setting every one of them still leaves the group unlabeled and unrenderable.

use autumn_web::a11y::{RadioGroup, RadioOption};
use maud::Render;

fn main() {
    let _markup = RadioGroup::new("speed", RadioOption::new("standard", "Standard"))
        .option(RadioOption::new("express", "Express"))
        .checked_value("express")
        .required()
        .aria_required()
        .aria_invalid(true)
        .described_by("speed-error")
        .class("field")
        .id_prefix("row-7")
        .label_class("field__legend")
        .hx("post", "/quote")
        .render();
}
