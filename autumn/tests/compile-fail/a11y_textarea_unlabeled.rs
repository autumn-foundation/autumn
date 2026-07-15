//! Issue #1706: an unlabeled `TextArea` cannot be rendered. WCAG 1.3.1 /
//! 3.3.2 / 4.1.2 — only `TextArea<Labeled>` implements `Render`, so calling
//! `.render()` on a `TextArea<NoLabel>` (no `.label(..)`/`.aria_label(..)`/
//! `.labelled_by(..)`) is a compile error.

use autumn_web::a11y::TextArea;
use maud::Render;

fn main() {
    let _markup = TextArea::new("bio").render();
}
