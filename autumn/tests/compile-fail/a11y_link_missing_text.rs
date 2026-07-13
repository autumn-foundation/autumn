//! Issue #1706: a `Link` without link text does not compile. WCAG 2.4.4 /
//! 4.1.2 — the accessible name (link text) is a mandatory positional argument,
//! so a text-less link is unrepresentable.

use autumn_web::a11y::Link;

fn main() {
    let _link = Link::new("/about");
}
