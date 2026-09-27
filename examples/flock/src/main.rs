//! The `flock` example: a minimal Autumn app whose home page mounts a Yew CSR
//! "island" (SPIKE — see `docs/guide/wasm-islands.md`).
//!
//! The whole server is a single maud-rendered page at `GET /`. maud owns the
//! page; the `<div data-autumn-island="flock">` is the mount point; the
//! deferred ES module `flock-boot.js` runs `wasm-bindgen`'s `init()`, resolves
//! the element, then calls the island's `mount(el, count)` with that
//! `web_sys::Element`, which renders the Yew `Flock` component (a "literary
//! boids" flocking animation) into it. The component owns its own `<canvas>`
//! via a `NodeRef` — the mount point is a plain container, not a `<canvas>`,
//! because a canvas's children are unsupported-fallback content (never
//! rendered) and so cannot host the component's canvas + controls. The initial
//! boid count flows in via the `data-count` attribute.
//!
//! Instantiating the island's WebAssembly module requires `'wasm-unsafe-eval'`
//! in the page's `script-src`. That is set as an explicit **app-level** CSP in
//! `autumn.toml` — Autumn ships no wasm flag; the custom policy is this app's
//! own choice. Build the island first:
//! `examples/island-flock/build-island.sh`. See `docs/guide/wasm-islands.md`.
use autumn_web::prelude::*;

/// A maud-rendered page that mounts the Yew "literary boids" island.
#[get("/")]
async fn index() -> Markup {
    html! {
        (PreEscaped("<!DOCTYPE html>"))
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { "Autumn · Literary boids" }
            }
            body {
                // `<main>` wraps all of the page's content (both the
                // server-rendered heading/paragraph and the mount point the
                // Yew island later renders its canvas + controls into) as the
                // very first thing in `<body>` — there is no nav/header
                // before it, so a skip link would add a tab stop that
                // bypasses nothing (the same reasoning `todo-app`/
                // `media-room` established in #2483; `autumn check --a11y`'s
                // `bypass` rule already exempts this shape).
                main id="main-content" {
                    h1 { "Literary boids" }
                    p {
                        "Every glyph is a flocking agent that becomes the last character "
                        "of Autumn's own source code it eats — O(N²) neighbor math every "
                        "frame, running entirely client-side in WebAssembly."
                    }
                    // Container mount point. The boot loader resolves this
                    // element and passes it straight to mount(); the data-*
                    // attributes carry the island name + initial boid count.
                    // The Yew component renders its own <canvas> + controls
                    // inside this <div> — see `examples/island-flock`'s
                    // `Flock` component, which itself emits no landmark of
                    // its own, so the island's content stays inside this
                    // `<main>` both before and after it mounts.
                    div id="flock" data-autumn-island="flock" data-count="120" {}
                }
                // External module (script-src 'self'); no inline script, no
                // nonce. Kept after `</main>` (rather than in `<head>`) so
                // `<main>` stays the literal first child of `<body>`.
                script type="module" src=(asset_url("islands/flock-boot.js")) defer {}
            }
        }
    }
}

#[autumn_web::main]
async fn main() {
    autumn_web::app().routes(routes![index]).run().await;
}

#[cfg(test)]
mod tests {
    use super::index;

    /// Regression test for the a11y fix (`autumn check --a11y` `bypass`,
    /// `landmark-one-main`): the page's content — including the island mount
    /// point — is wrapped in a `<main>` landmark, and it is the first thing
    /// in `<body>`, so no skip link is needed either (mirrors `todo-app`/
    /// `media-room`, established in #2483).
    #[tokio::test]
    async fn index_wraps_content_in_a_main_landmark_first_in_body() {
        let html = index().await.into_string();

        assert!(
            html.contains(r#"<main id="main-content">"#),
            "missing <main> landmark: {html}"
        );
        let body_open = html.find("<body>").expect("has <body>") + "<body>".len();
        let main_open = html.find("<main").expect("has <main>");
        assert!(
            !html[body_open..main_open].contains('<'),
            "<main> must be the first element in <body> (nothing to skip \
             past), so autumn check --a11y's bypass rule does not ask for a \
             pointless skip link: {html}"
        );
        // The island mount point stays inside <main> both before and after
        // the Yew component hydrates it (issue: the component itself emits
        // no landmark of its own — see examples/island-flock/src/lib.rs).
        let main_close = html.find("</main>").expect("has </main>");
        let island_pos = html
            .find(r#"data-autumn-island="flock""#)
            .expect("has the island mount marker");
        assert!(
            (main_open..main_close).contains(&island_pos),
            "island mount point must be inside <main>: {html}"
        );
    }
}
