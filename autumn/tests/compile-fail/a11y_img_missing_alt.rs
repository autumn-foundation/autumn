//! Issue #1706: an `Img` without alt text does not compile. WCAG 1.1.1 — the
//! accessible name is a mandatory positional argument, so an alt-less image is
//! unrepresentable.

use autumn_web::a11y::Img;

fn main() {
    let _img = Img::new("/logo.png");
}
