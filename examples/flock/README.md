# Autumn Flock Example — WASM Island

A minimal Autumn server whose **home page** (`GET /`) mounts a Yew
client-side-rendered (CSR) "island": **literary boids**, a
[Reynolds flocking](https://en.wikipedia.org/wiki/Boids) simulation where every
glyph is a flocking agent that becomes the last character of Autumn's own source
code it eats — O(N²) neighbor math every frame, running entirely client-side in
WebAssembly, with no server round-trips.

This is a **spike / escape hatch**, not a framework subsystem. It shows how to
drop one self-contained, Rust→WebAssembly widget into an otherwise
server-rendered maud page. The design notes live in
[`docs/guide/wasm-islands.md`](../../docs/guide/wasm-islands.md); the island
crate itself is [`examples/island-flock`](../island-flock).

## What it demonstrates

| Feature | Where | What it does |
|---------|-------|--------------|
| maud-owned page | `src/main.rs` | Server renders the full HTML page, including an empty mount `<div>` and a deferred module `<script>` |
| Island mount point | `src/main.rs` | `<div data-autumn-island="flock" data-count="120">` carries the island name + initial props as `data-*` attributes |
| ES-module loader | `static/islands/flock-boot.js` | `import init, { mount }` — instantiates the wasm, then mounts the Yew component into the div |
| Custom CSP | `autumn.toml` | Framework-default policy **plus** `'wasm-unsafe-eval'` in `script-src` — the app's own opt-in that authorizes WebAssembly instantiation |
| Static serving | automatic | Autumn serves `static/` at `/static/` with no route wiring; `asset_url(...)` resolves the loader |

## Prerequisites

- Rust 1.88.0+
- To (re)build the island's wasm artifacts:
  - the `wasm32-unknown-unknown` target: `rustup target add wasm32-unknown-unknown`
  - `wasm-bindgen-cli`, **pinned to the exact `wasm-bindgen` library version**
    the island crate resolves to (a mismatch produces cryptic runtime errors):
    `cargo install wasm-bindgen-cli --version <lib version>`
  - (optional) `wasm-opt` from binaryen for a size pass

No database or external services required.

The committed `static/islands/autumn_island_flock.{js,wasm}` artifacts mean the
demo runs on a fresh checkout **without** a wasm toolchain — you only need the
toolchain above if you want to rebuild the island from source.

## Quick start

From the **workspace root** (`autumn/`):

```bash
cargo run -p flock
```

The server starts on `http://localhost:3000`. Open it in a browser — the
flocking canvas animates immediately, entirely client-side.

### Prove it works

The home page carries the custom CSP that authorizes the wasm island:

```bash
curl -sD - -o /dev/null http://127.0.0.1:3000/ | grep -i content-security-policy
# content-security-policy: default-src 'self'; … script-src 'self' 'wasm-unsafe-eval'; …
```

### Rebuild the island (optional)

The wasm artifacts under `static/islands/` are committed, but you can rebuild
them from the island crate:

```bash
cd ../island-flock && ./build-island.sh
# emits autumn_island_flock.{js,wasm} into ../flock/static/islands/
```

Then re-run `cargo run -p flock` and reload the page.
