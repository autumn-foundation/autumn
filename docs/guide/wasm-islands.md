# WASM islands (Yew CSR) — recipe

Autumn is server-first: the default output is a maud-rendered HTML page, and
htmx covers most interactivity without a JS build step. Occasionally, though,
you want something htmx and server rendering genuinely **cannot** do: a widget
that runs heavy compute and animates every frame, entirely on the client, with
no server round-trip in the loop. This recipe shows how to drop **one**
self-contained client-side widget, compiled from Rust to WebAssembly with
[Yew](https://yew.rs), into an otherwise server-rendered maud page.

The motivating example is **"literary boids"** — a
[Reynolds flocking](https://en.wikipedia.org/wiki/Boids) simulation where each
glyph-agent becomes the last character of **Autumn's own source code** it eats.

> **Status: spike / escape hatch, not a framework subsystem.** There is no
> `autumn generate island`, no `IslandCx`, no build-pipeline integration. You
> wire the pieces by hand. That is deliberate. A complete, working copy of
> everything here lives in `examples/island-flock/` +
> `examples/flock/src/main.rs` (the `GET /` route). The crate is a peer of the
> `flock` example and builds its wasm into `examples/flock/static/islands/`.

<!-- cli-surface-allow: autumn generate island — named above only to state that it does not exist -->


## Why a WASM island (and not htmx)

htmx is a swap engine: an event fires, the browser asks the server for a chunk
of HTML, the server renders it, the fragment is swapped in. That model is a poor
fit when the interaction is **its own compute**:

- **Per-frame O(N²) work.** Literary boids runs three neighbour passes
  (separation, alignment, cohesion) over every pair of boids, ~30 times a
  second. With a few hundred boids that is tens of thousands of distance
  calculations per frame. Round-tripping that to the server 30×/s is absurd;
  it belongs on the client, and Rust→wasm runs it at near-native speed.
- **Continuous animation.** The canvas repaints on `requestAnimationFrame`.
  There is no "event → fragment" story here; there is a running loop that owns a
  pixel buffer.
- **Rich local interaction.** Pause/resume and reseed mutate in-memory
  simulation state instantly, with no network in the path.

When you have all three — heavy per-frame compute + animation + local
interaction — a wasm island is the right tool. For everything else, reach for
htmx first.

## The concept

The boundary between server and client is a **plain DOM element plus serialized
props** — nothing more:

1. **maud owns the page.** Your handler renders the full HTML, including an
   *empty* mount element and a `<script type="module">` that loads the island.
2. **The island mounts into that element** with Yew's client-side renderer
   (`Renderer::with_root_and_props`). It manages only the DOM *inside* it.
3. **No SSR, no hydration.** The server never renders the widget's markup, so
   there is no hydration-mismatch class of bug, and Yew's experimental SSR
   surface is never touched. Props travel as `data-*` attributes.

Because the two crates compile for different targets and never share a
compilation unit, maud's `html!` macro and Yew's `html!` macro **never
collide** — a concern that turns out to be a non-issue in this architecture.

```
┌────────────────────────── server (native, autumn-web) ───────────────────────────┐
│  #[get("/")] async fn index() -> Markup {                                          │
│      html! { div id="flock" data-autumn-island="flock" data-count="120" {}        │
│              script type="module" src=(asset_url("islands/flock-boot.js")) defer;} │
│  }                                                                                 │
└───────────────────────────────────────────────────────────────────────────────────┘
                    │ HTML + empty mount div + module script
                    ▼
┌────────────────────────── browser (wasm32, yew CSR) ─────────────────────────────┐
│  flock-boot.js → init() (instantiate .wasm) → mount(el, 120)                      │
│  yew::Renderer::<Flock>::with_root_and_props(el, props).render()                  │
│  Flock owns a <canvas>; a requestAnimationFrame loop ticks the World + repaints   │
└───────────────────────────────────────────────────────────────────────────────────┘
```

> **Why the mount point is a `<div>`, not a `<canvas>`.** The `Flock` component
> owns its own `<canvas>` (via a `NodeRef`) *and* a control strip (buttons +
> readout). A `<canvas>`'s child nodes are *unsupported-fallback content* —
> browsers never render them when canvas is available — so mounting the
> component into a `<canvas>` would leave its canvas + controls invisible. The
> mount point is therefore a plain container `<div>`; the component renders the
> real canvas inside it.

## Crate layout

The island is a **separate crate** that only ever builds for
`wasm32-unknown-unknown`. It links `yew` and cannot compile for the host, so it
must be kept out of the native workspace build:

- Give it its own `[workspace]` table (so it is its own workspace root), **and**
- add it to `exclude` in the repo-root `Cargo.toml`.

Together these guarantee `cargo build/clippy --workspace` never tries to build
it natively.

`examples/island-flock/Cargo.toml`:

```toml
[workspace]           # own workspace root — not a member of the repo workspace

[package]
name = "autumn-island-flock"
version = "0.0.0"
edition = "2024"
publish = false

[lib]
crate-type = ["cdylib", "rlib"]   # cdylib → the .wasm; rlib → unit-testable

[dependencies]
yew = { version = "0.21", default-features = false, features = ["csr"] }  # CSR only
wasm-bindgen = "0.2"
js-sys = "0.3"          # Math::random() — the sim's only source of randomness
gloo-render = "0.2"     # leak-free requestAnimationFrame wrapper

[dependencies.web-sys]
version = "0.3"
features = [            # 2D canvas text animation + DOM access from mount()
    "Window", "Document", "Element", "HtmlElement",
    "HtmlCanvasElement", "CanvasRenderingContext2d",
]

[profile.release]     # small artifact
opt-level = "z"
lto = true
codegen-units = 1
panic = "abort"
strip = true
```

Repo-root `Cargo.toml`:

```toml
[workspace]
exclude = ["fuzz", "examples/island-flock"]
```

### The inlined simulation

The flocking mechanic (ported verbatim from a skunkworks `literary-boids`
experiment's `boid.rs` / `world.rs`) lives entirely inside the island crate, in
`src/sim.rs`. It is plain, dependency-light Rust — `Vec2` math, a `Dna` genome,
a `Boid`, and a `World` that ticks the ecosystem — with exactly two translations
for the browser:

- **randomness** goes through `js_sys::Math::random()` (no `rand` crate), and
- **colour** is a CSS colour string (no `ratatui::Color`), drawn onto a canvas.

Every tick, `World::update()` does the O(N²) flocking pass, feeds boids
(adopting the eaten character as their glyph), reproduces well-fed boids up to a
population cap, and removes starved ones. "Syntax physics" nudges a boid's genome
by what kind of character it just ate — punctuation makes it skittish, digits
make it march in step, vowels widen its view — which is what gives a flock of
code its lively, legible texture.

### The embedded corpus (Autumn's own source)

The food supply is **real Autumn source code**, embedded so the island crate is
fully self-contained and does not depend on the framework's on-disk layout at
compile time. `src/corpus.rs` is a one-liner:

```rust
/// Static snapshot of Autumn's own source, used as the flock's food supply.
pub const AUTUMN_SOURCE: &str = include_str!("corpus.txt");
```

`corpus.txt` is a static snapshot — representative, punctuation-dense excerpts
stitched together from `autumn/src/lib.rs`, `autumn/src/prelude.rs`, and the
macro-heavy `autumn-macros/src/route.rs` (roughly `#[get("/")]`, `html! { … }`,
`pub use crate::…`, `-> TokenStream`). `World::new` filters out whitespace and
serves the remaining characters cyclically as food; a boid that eats one becomes
that character. Because it is a snapshot, it will not track edits to those files
— regenerate it if you want it fresh.

### The component + mount entry (`src/lib.rs`, abridged)

```rust
#[function_component(Flock)]
pub fn flock(props: &FlockProps) -> Html {
    let canvas_ref = use_node_ref();
    // ... pause / reseed state ...
    use_effect_with((/* reseed gen */, props.count), move |_| {
        // resolve the <canvas>, get its 2D context, build World::new(AUTUMN_SOURCE, count),
        // and drive a self-rescheduling gloo-render requestAnimationFrame loop that
        // (every ~30ms) ticks the world and repaints. Cleanup cancels the frame.
    });
    html! {
        <div class="flock-island">
            <div class="flock-controls">/* Pause/Resume, Reseed, live readout */</div>
            <canvas id="flock-canvas" ref={canvas_ref} width="840" height="560" />
        </div>
    }
}

#[wasm_bindgen]
pub fn mount(el: web_sys::Element, count: u32) {
    yew::Renderer::<Flock>::with_root_and_props(el, FlockProps { count }).render();
}
```

Each frame clears to a dark background, then draws every food glyph in dim green
and every boid glyph in its DNA colour with `ctx.fill_text(...)`. World
coordinates `[0, 200]²` map onto the 840×560 canvas by a per-axis scale, with the
Y axis flipped (canvas Y grows downward; the sim's grows upward).

## Build commands

We use bare **`wasm-bindgen --target web`** rather than `trunk`. `--target web`
emits an ES module you can `import` directly from a `<script type="module">`,
which drops cleanly into a maud-owned page (trunk wants to own an `index.html`).

Install the toolchain once. **Pin `wasm-bindgen-cli` to the exact
`wasm-bindgen` library version your crate resolves to** — a mismatch produces
cryptic runtime errors:

```bash
rustup target add wasm32-unknown-unknown
# check the resolved lib version: grep -A1 'name = "wasm-bindgen"' Cargo.lock
cargo install wasm-bindgen-cli --version 0.2.126
# optional size pass:
cargo install wasm-opt   # or install binaryen
```

Then run the build script (`examples/island-flock/build-island.sh`),
which wraps the three steps:

```bash
cd examples/island-flock && ./build-island.sh
# 1. cargo build --target wasm32-unknown-unknown --release
# 2. wasm-bindgen --target web --no-typescript --out-dir ../flock/static/islands <crate>.wasm
# 3. wasm-opt -Oz (optional, only if wasm-opt is on PATH)
```

> The build script honors `CARGO_TARGET_DIR`. If it is unset, cargo writes to
> `<crate>/target` and the paths change accordingly.

`wasm-bindgen --target web` writes two files into `static/islands/`:

- `autumn_island_flock.js` — the ES-module glue (default export `init`, plus
  your `#[wasm_bindgen]` exports like `mount`)
- `autumn_island_flock_bg.wasm` — the module

For the flock these are ~29 KB of JS glue and ~212 KB of wasm before `wasm-opt`
(Yew carries a virtual-DOM runtime; a Leptos CSR island would be smaller — see
*Limitations*).

## The loader

An **external** ES module — no inline script, so it works under `script-src
'self'` with no nonce (`examples/flock/static/islands/flock-boot.js`):

```js
import init, { mount } from './autumn_island_flock.js';
await init();                                    // fetch + instantiate the .wasm
const el = document.querySelector('[data-autumn-island="flock"]');
if (el) mount(el, Number(el.dataset.count ?? '120'));   // props via data-*
```

The `await init()` step is the one the CSP's `'wasm-unsafe-eval'` token
authorizes — it compiles/instantiates the WebAssembly module. Reference the
loader from maud with `asset_url(...)` so the release fingerprint is picked up
automatically:

```rust
script type="module" src=(asset_url("islands/flock-boot.js")) defer {}
```

## Props via data attributes

The mount `<div>` carries the island name and its initial props as `data-*`
attributes; the loader reads them and passes them into `mount`:

```rust
div id="flock" data-autumn-island="flock" data-count="120" {}
```

`data-autumn-island="flock"` is the selector the loader keys on; `data-count`
becomes the initial boid population. For richer props, embed a
`<script type="application/json">` block next to the div and parse it at mount
time instead of packing everything into attributes.

## The custom CSP (the app's choice, not a framework flag)

Instantiating WebAssembly in the browser is CSP-gated: it requires
**`'wasm-unsafe-eval'`** in `script-src`. Autumn's default CSP is strict and
does **not** include it, and **Autumn ships no wasm flag or primitive** — there
is no framework switch that relaxes the policy for you. Emitting a wasm-friendly
policy is entirely the **application's** decision.

The mechanism is the existing `content_security_policy` field under
`[security.headers]` (see `autumn::security::config::HeadersConfig`). When you
set it to an explicit string, Autumn emits that string **verbatim** as the
`Content-Security-Policy` header (and, per
`autumn::security::headers`, an explicit policy automatically opts out of nonce
injection — you own the policy end to end).

The flock example sets its policy to the framework default **plus** the single
`'wasm-unsafe-eval'` token in `script-src` — every other directive is
byte-for-byte the default (`autumn::security::config::default_content_security_policy`):

```toml
# examples/flock/autumn.toml
[security.headers]
content_security_policy = "default-src 'self'; img-src 'self' data:; style-src 'self' 'unsafe-inline'; script-src 'self' 'wasm-unsafe-eval'; connect-src 'self'; form-action 'self'; frame-ancestors 'none'; base-uri 'self'"
```

You can confirm it at runtime:

```bash
curl -sD - -o /dev/null http://127.0.0.1:3000/ | grep -i content-security-policy
# content-security-policy: default-src 'self'; … script-src 'self' 'wasm-unsafe-eval'; …
```

Same-origin `.wasm` fetch and the JS glue are already allowed by `'self'`, and
`.wasm` is already mapped to `application/wasm` by Autumn's asset middleware —
no other CSP or content-type change is needed.

## Security notes

- **`'wasm-unsafe-eval'` is not `'unsafe-eval'`.** It permits **WebAssembly
  compilation only**; it does **not** re-enable JavaScript `eval()` /
  `new Function()`. It is a far narrower relaxation than `'unsafe-eval'`, but it
  is still a relaxation — scope it to apps that actually ship an island, and
  keep every other directive strict (as above, only the one token is added).
- **An island is trusted first-party code, not a sandbox.** It runs with the
  page's full authority: same-origin `fetch`, cookies, `localStorage`, the whole
  DOM. There is no privilege boundary between the island and the rest of the
  page. Treat island source exactly like the rest of your first-party app code.
- **This is *not* a way to run untrusted / tenant code.** If you need to execute
  code you do not control (a customer's plugin, user-authored scripts), a wasm
  island on the same origin is the **wrong** tool — it inherits full page
  authority. Tenant code needs real **origin isolation**: serve it from a
  separate subdomain inside a sandboxed `<iframe>`, so the browser's
  same-origin policy — not a CSP token — is the containment boundary.

## Serving and caching the `.wasm`

Drop the wasm-bindgen output under the example's `static/islands/`. Autumn
serves `static/` at `/static/` out of the box, so no route wiring is required.

- **Dev** (`cargo run`): `asset_url("islands/flock-boot.js")` returns
  `/static/islands/flock-boot.js` verbatim — edits are visible immediately.
- **Release** (`autumn build --release`): the asset pipeline fingerprints and
  long-caches files it knows about (`public, max-age=31536000, immutable`). The
  `.wasm`/`.js` pair flows through the existing manifest unchanged. The one
  wrinkle: the loader imports the glue by a relative name
  (`./autumn_island_flock.js`), so keep the glue+wasm pair addressed the way
  wasm-bindgen emitted them, and let `asset_url` fingerprint the entry
  `flock-boot.js`.

## Add your own island (walkthrough)

1. `cp -r examples/island-flock my-app/widgets/my-island` and rename the
   crate in its `Cargo.toml`.
2. Add `"my-app/widgets/my-island"` to `exclude` in the repo-root
   `Cargo.toml` (it already has its own `[workspace]` table).
3. Write your component + a `#[wasm_bindgen] pub fn mount(el: web_sys::Element, /* props */)`.
4. Copy `build-island.sh`, update `CRATE_NAME`, and also point `OUT_DIR` at
   your own app's `static/islands` (e.g. `OUT_DIR="../<their-app>/static/islands"`)
   — it defaults to the flock example's dir and is relative to the island
   crate dir. Then run it; the artifacts land in that `OUT_DIR`.
5. Copy `flock-boot.js` → `my-island-boot.js`; update the import filename and
   the `data-autumn-island` selector.
6. In a maud handler, render the empty mount div + the module script
   (`asset_url("islands/my-island-boot.js")`).
7. In `autumn.toml`, set an explicit `content_security_policy` = *default* +
   `'wasm-unsafe-eval'` in `script-src` (copy the flock example's string).
8. `cargo run`, open the page, confirm the widget mounts.

## Limitations & honest caveats

- **This is a spike.** No generator, no `cargo run` hook that rebuilds the wasm
  on `.rs` change (Autumn's poll-based live-reload does not know about the
  island crate), no version-bump integration. You run `build-island.sh` by
  hand. The committed `autumn_island_flock.{js,wasm}` artifacts mean the demo
  runs on a fresh checkout without a wasm toolchain; the trade-off is that they
  can drift from the source until you rebuild. Wiring the island rebuild into the
  dev-loop watcher is the main piece a productionized version would need.
- **The corpus is a static snapshot.** `corpus.txt` does not track edits to the
  Autumn files it was excerpted from; regenerate it by hand if you want it fresh.
- **Yew was chosen for the cleanest "mount into someone else's div" story**
  (`with_root_and_props`) and because CSR needs only the `csr` feature. It also
  has the *least* momentum of the Rust UI frameworks; **Leptos** (CSR
  `mount_to`) is a higher-momentum alternative with a smaller per-island
  runtime if this ever graduates. Dioxus is a poor fit for this shape.
- **Share `serde` DTOs, not `#[model]` types.** Autumn's `#[model]` derive emits
  unconditional diesel/`Preloadable` impls that are native-only; they will not
  compile for wasm32. A plain `serde` struct in a shared crate is the correct
  wire boundary.
