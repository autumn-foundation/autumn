//! Issue #1706: an unlabeled `FileField` cannot be rendered. WCAG 1.3.1 /
//! 3.3.2 / 4.1.2 — only `FileField<Labeled>` implements `Render`, so calling
//! `.render()` on a `FileField<NoLabel>` (no `.label(..)`/`.aria_label(..)`/
//! `.labelled_by(..)`) is a compile error.

use autumn_web::a11y::FileField;
use maud::Render;

fn main() {
    let _markup = FileField::new("avatar").accept("image/*").render();
}
