//! Issue #1706: a `Button` without an accessible name does not compile.
//! WCAG 4.1.2 — the name is a required argument, so a name-less button is
//! unrepresentable.

use autumn_web::a11y::Button;

fn main() {
    let _button = Button::new();
}
