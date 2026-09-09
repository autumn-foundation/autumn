//! Issue #1706: an unlabeled `RadioGroup` cannot be rendered. WCAG 1.3.1 /
//! 3.3.2 / 4.1.2 — a radio group needs a name for the group itself, not just
//! for each choice, so only `RadioGroup<Labeled>` implements `Render`.

use autumn_web::a11y::{RadioGroup, RadioOption};
use maud::Render;

fn main() {
    let _markup = RadioGroup::new("speed")
        .option(RadioOption::new("standard", "Standard"))
        .render();
}
