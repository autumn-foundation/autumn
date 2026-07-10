use autumn_web::prelude::*;

#[get("/")]
async fn index() -> &'static str {
    "Welcome to Autumn!"
}

#[get("/hello")]
async fn hello() -> &'static str {
    "Hello, Autumn!"
}

#[get("/hello/{name}")]
async fn hello_name(name: Path<String>) -> String {
    format!("Hello, {}!", *name)
}

/// A maud-rendered page that mounts a Yew CSR "island" (SPIKE).
///
/// maud owns the whole page. The `<div data-autumn-island="flock">` is the
/// mount point; the deferred ES module `flock-boot.js` runs `wasm-bindgen`'s
/// `init()`, resolves the element, then calls the island's `mount(el, count)`
/// with that `web_sys::Element`, which renders the Yew `Flock` component (a
/// "literary boids" flocking animation) into it. The component owns its own
/// `<canvas>` via a `NodeRef` — the mount point is a plain container, not a
/// `<canvas>`, because a canvas's children are unsupported-fallback content
/// (never rendered) and so cannot host the component's canvas + controls. The
/// initial boid count flows in via the `data-count` attribute.
///
/// Instantiating the island's WebAssembly module requires `'wasm-unsafe-eval'`
/// in the page's `script-src`. That is set as an explicit **app-level** CSP in
/// `autumn.toml` — Autumn ships no wasm flag; the custom policy is this app's
/// own choice. Build the island first:
/// `examples/island-flock/build-island.sh`. See
/// `docs/guide/wasm-islands.md`.
#[get("/flock")]
async fn flock() -> Markup {
    html! {
        (PreEscaped("<!DOCTYPE html>"))
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { "Autumn · Literary boids" }
            }
            body {
                h1 { "Literary boids" }
                p {
                    "Every glyph is a flocking agent that becomes the last character "
                    "of Autumn's own source code it eats — O(N²) neighbor math every "
                    "frame, running entirely client-side in WebAssembly. No server "
                    "round-trips."
                }
                // Container mount point. The boot loader resolves this element
                // and passes it straight to mount(); the data-* attributes carry
                // the island name + initial boid count. The Yew component renders
                // its own <canvas> + controls inside this <div>.
                div id="flock" data-autumn-island="flock" data-count="120" {}
                // External module (script-src 'self'); no inline script, no nonce.
                script type="module" src=(asset_url("islands/flock-boot.js")) defer {}
            }
        }
    }
}

#[autumn_web::main]
async fn main() {
    autumn_web::app()
        .routes(routes![index, hello, hello_name, flock])
        .run()
        .await;
}
