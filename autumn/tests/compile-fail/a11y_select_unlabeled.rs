//! Issue #1706: an unlabeled `Select` cannot be rendered. WCAG 1.3.1 / 3.3.2 /
//! 4.1.2 — only `Select<Labeled>` implements `Render`, so calling `.render()`
//! on a `Select<NoLabel>` (no `.label(..)`/`.aria_label(..)`/`.labelled_by(..)`)
//! is a compile error, even when options are present.

use autumn_web::a11y::Select;
use maud::Render;

fn main() {
    let _markup = Select::new("role").option("admin", "Admin").render();
}
