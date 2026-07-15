//! RED a11y fixture — a small dashboard view written in raw `maud::html!`
//! markup with four **genuine, static** accessibility defects that
//! `autumn a11y verify` reliably flags (issue #1932, part of #1706).
//!
//! This file is NOT compiled by cargo (`tests/fixtures` is not an auto-target);
//! it only has to be valid Rust that `proc_macro2::TokenStream::from_str` can
//! tokenize, because the verifier scans token streams, not a type-resolved AST.
//! The GREEN sibling (`../green/dashboard.rs`) is the same UI with every defect
//! fixed, so the pair is a clear red→green diff.
//!
//! Every defect below is a fully static, literal-attribute element that steps
//! around every one of the verifier's conservative skips (no splices, no
//! dynamic `id`/`for`, no dynamic `type`, no sibling `(expr)` fragments), so
//! all four rules fire deterministically. All four live in ONE `html!` block.

use maud::{html, Markup};

pub fn view() -> Markup {
    html! {
        main {
            h1 { "Account settings" }

            // `label` (WCAG 1.3.1 / 3.3.2 / 4.1.2) — a text input with no
            // associated `<label for=…>`, `aria-label`, or `aria-labelledby`.
            form {
                input type="text" name="email" id="email";

                // `button-name` (WCAG 4.1.2) — a button with an empty static
                // body and no accessible name.
                button type="submit" {}
            }

            // `image-alt` (WCAG 1.1.1) — an `<img>` with no `alt`/aria/title.
            img src="/logo.png";

            // `label` (again) — a `<select>` with no associated label.
            select name="role" {
                option value="admin" { "Admin" }
                option value="viewer" { "Viewer" }
            }
        }
    }
}
