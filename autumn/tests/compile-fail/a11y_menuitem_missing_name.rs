//! Issue #1706: a `MenuItem` without an accessible name does not compile.
//! WCAG 4.1.2 — the name is a required argument, so a name-less menu item is
//! unrepresentable.

use autumn_web::a11y::MenuItem;

fn main() {
    let _item = MenuItem::new();
}
