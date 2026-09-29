//! RED a11y fixture — a small dashboard route written in raw `maud::html!`
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
//! all four rules fire deterministically.
//!
//! The markup is split across a route handler and two helpers it calls, so the
//! fixture also exercises route attribution: every finding — including the ones
//! in `account_form` and `logo_banner` — is keyed to `GET /settings`, while the
//! second route (`GET /about`), which calls neither helper, stays clean.

use maud::{html, Markup};

#[get("/settings")]
pub async fn view() -> Markup {
    html! {
        main {
            h1 { "Account settings" }
            (account_form())
            (logo_banner())
        }
    }
}

fn account_form() -> Markup {
    html! {
        // `label` (WCAG 1.3.1 / 3.3.2 / 4.1.2) — a text input with no
        // associated `<label for=…>`, `aria-label`, or `aria-labelledby`.
        form {
            input type="text" name="email" id="email";

            // `button-name` (WCAG 4.1.2) — a button with an empty static
            // body and no accessible name.
            button type="submit" {}
        }

        // `label` (again) — a `<select>` with no associated label.
        select name="role" {
            option value="admin" { "Admin" }
            option value="viewer" { "Viewer" }
        }
    }
}

fn logo_banner() -> Markup {
    html! {
        // `image-alt` (WCAG 1.1.1) — an `<img>` with no `alt`/aria/title.
        img src="/logo.png";
    }
}

/// A second route that renders only accessible markup and calls neither
/// helper, so the manifest must report it separately and cleanly — proof that
/// attribution is per route, not "blame every route for every finding".
#[get("/about")]
pub async fn about() -> Markup {
    html! {
        main {
            h1 { "About" }
            p { "A dashboard." }
        }
    }
}
