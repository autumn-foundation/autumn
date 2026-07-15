//! GREEN a11y fixture — the SAME dashboard view as `../red/dashboard.rs`, with
//! every accessibility defect fixed, so `autumn a11y verify` reports ZERO
//! findings and exits 0 (issue #1932, part of #1706).
//!
//! This file is NOT compiled by cargo; it only has to tokenize (the verifier
//! scans token streams). The corrections are made in raw `maud::html!` here so
//! the red→green diff is the clearest possible teaching artifact:
//!
//!   - input:  add an associated `<label for="email">` + matching `id="email"`.
//!   - button: give it visible text content ("Save").
//!   - img:    add a descriptive `alt="Company logo"`.
//!   - select: add an associated `<label for="role">` + matching `id="role"`.
//!
//! The very same clean result is achievable via the typed
//! `autumn_web::a11y` primitives (`Img::new(src, alt)`, `Button::new(name)`,
//! `TextField::new(..).label(..)`), which discharge each accessible-name
//! obligation at COMPILE time. The verifier intentionally does NOT re-scan code
//! written through those primitives — it only nets the raw-`html!` escape hatch
//! exercised here.

use maud::{html, Markup};

pub fn view() -> Markup {
    html! {
        main {
            h1 { "Account settings" }

            // `label` fixed — the input now has an associated `<label for>`.
            form {
                label for="email" { "Email address" }
                input type="text" name="email" id="email";

                // `button-name` fixed — the button has visible text content.
                button type="submit" { "Save" }
            }

            // `image-alt` fixed — the image now has descriptive alt text.
            img src="/logo.png" alt="Company logo";

            // `label` fixed — the select now has an associated `<label for>`.
            label for="role" { "Role" }
            select name="role" id="role" {
                option value="admin" { "Admin" }
                option value="viewer" { "Viewer" }
            }
        }
    }
}
