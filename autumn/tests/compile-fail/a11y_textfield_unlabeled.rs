//! Issue #1706: an unlabeled `TextField` cannot be rendered. WCAG 1.3.1 /
//! 3.3.2 / 4.1.2 — only `TextField<Labeled>` implements `Render`, so calling
//! `.render()` on a `TextField<NoLabel>` (no `.label(..)`/`.aria_label(..)`/
//! `.labelled_by(..)`) is a compile error.

use autumn_web::a11y::TextField;
use maud::Render;

fn main() {
    let _markup = TextField::new("email").render();
}
