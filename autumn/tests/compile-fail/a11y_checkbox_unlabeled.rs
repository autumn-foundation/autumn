//! Issue #1706: an unlabeled `Checkbox` cannot be rendered. WCAG 1.3.1 /
//! 3.3.2 / 4.1.2 — only `Checkbox<Labeled>` implements `Render`, so calling
//! `.render()` on a `Checkbox<NoLabel>` (no `.label(..)`/`.aria_label(..)`/
//! `.labelled_by(..)`) is a compile error.

use autumn_web::a11y::Checkbox;
use maud::Render;

fn main() {
    let _markup = Checkbox::new("accept_terms").checked(true).render();
}
