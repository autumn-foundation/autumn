# Autumn Edge Capsule Example

One app, two substrates. The same handler source serves from the origin binary
and from a portable `wasm32-wasip1` **edge capsule** a CDN can run — and a
conformance suite proves the two answer byte-identically.

Issue [#1790](https://github.com/autumn-foundation/autumn/issues/1790); the
narrative is `docs/guide/edge.md`, the design record is
`docs/adr/0011-edge-capsule-read-lane.md`.

## What it demonstrates

| Feature | Where | What it does |
|---------|-------|--------------|
| `#[edge]` | `src/handlers.rs` | Marks a `GET` route as edge-eligible; refuses anything the edge cannot serve at compile time |
| `#[edge(needs(kv))]` | `src/handlers.rs` | Declares a platform seam the host must mediate, checked *before* dispatch |
| `edge_routes![]` | `src/handlers.rs` | Collects the edge lane's route table, mirroring `routes![]` |
| `autumn_edge::serve` | `src/bin/edge-capsule.rs` | The whole of a capsule's `main`: three lines |
| `EdgeCache` | `src/handlers.rs` | One extractor, two implementations — the app's cache at the origin, a host round trip at the edge |
| `AppBuilder::with_edge_kv` | `src/main.rs` | Puts a store behind that seam at the origin |
| Origin-only code | `src/origin.rs` | A `POST` route the edge declines and the origin answers, with no glue |
| Conformance | `tests/conformance.rs` | Runs one request corpus through the native lane, a real wasm artifact, and the full origin app |

The module split is the whole trick:

```
src/handlers.rs   compiles for BOTH targets — only #[edge] GET routes live here
src/origin.rs     #[cfg(not(target_arch = "wasm32"))] — anything autumn-web offers
src/main.rs       the origin binary: mounts both
src/bin/edge-capsule.rs   the capsule: mounts handlers.rs only
```

## Prerequisites

- Rust 1.88.0+
- The WASI target, for building or testing the capsule:

  ```bash
  rustup target add wasm32-wasip1
  ```

No database, no external services, and no wasm runtime to install — the
conformance suite embeds a pure-Rust interpreter.

## Quick start

From the **workspace root**:

```bash
cargo run -p edge-greeting
```

The origin starts on `http://localhost:3000` and serves everything, edge routes
included.

### Prove it works

```bash
curl http://localhost:3000/greet/ada
# => Hello, ada!

curl http://localhost:3000/note/greeting
# => greeting: the origin published this note to the edge

curl "http://localhost:3000/stats?tag=one&tag=two"
# => routes=5 tags=[tag=one tag=two] ratio=0.666667

curl -X POST http://localhost:3000/feedback -d 'nice'
# => Thanks — the origin recorded 4 byte(s) of feedback.
```

## Building the capsule

`autumn build` emits the capsule alongside the native binary whenever the app
has `#[edge]` routes — one invocation, one codebase:

```bash
autumn build            # release build: native binary + edge capsule
autumn build --edge     # force the edge step in a debug build
```

```text
🍂 Edge capsule: 5 route(s) (greet, note, stats, count, boom) → target/wasm32-wasip1/release/edge-capsule.wasm (553 KB)
```

(This app registers no `#[static_get]` routes, so the static-render step that
runs *after* the edge step reports "No static routes registered" and `autumn
build` exits non-zero — the same thing it does for any app without pre-rendered
pages. The capsule above is already written by then.)

The equivalent by hand, which is exactly what the CLI runs:

```bash
cargo build -p edge-greeting --target wasm32-wasip1 --release --bin edge-capsule
ls -la target/wasm32-wasip1/release/edge-capsule.wasm
```

The artifact is **never** copied into `dist/` or `static/`. It is not an asset a
browser fetches; it is a program the CDN runs.

## Running the conformance suite

This is the interesting part. Every test builds a real capsule from these
sources and compares it against both the native edge lane and the full origin
app over a shared request corpus — percent-encoding, `%2F` inside a segment,
trailing slashes, repeated query keys, float and integer rendering, a stripped
credential, and each of the four ways the edge can decline.

```bash
cargo test -p edge-greeting --test conformance -- --ignored --test-threads=1 --nocapture
```

The tests are `#[ignore]`d because they need the `wasm32-wasip1` target; CI runs
them in the dedicated `edge-conformance` job. `--test-threads=1` is required:
the suite installs a panic hook around the deliberately-panicking `/boom`
handler, and a panic hook is process-global.

## Available routes

| Method | Path | Lane | Response |
|--------|------|------|----------|
| GET | `/greet/{name}` | edge + origin | `Hello, <name>!` plus an `x-greeting-lane` header |
| GET | `/note/{key}` | edge (`needs(kv)`) + origin | the cached note, or a miss message |
| GET | `/stats` | edge + origin | route count, the query tags in order, a formatted float |
| GET | `/stats/count` | edge + origin | a bare `usize` |
| GET | `/boom` | edge + origin | panics on purpose: a trap at the edge, a 500 at the origin |
| POST | `/feedback` | origin only | the write path — declined at the edge with `method_not_edge_eligible` |
