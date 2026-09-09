//! Issue #1706: a radio choice carries its own visible label. WCAG 1.3.1 /
//! 4.1.2 — `RadioOption::new` takes the value AND the label, so a choice with
//! no label does not build.

use autumn_web::a11y::{RadioGroup, RadioOption};
use maud::Render;

fn main() {
    let _markup = RadioGroup::new("speed")
        .option(RadioOption::new("standard"))
        .label("Shipping speed")
        .render();
}
