# autumn-edge

The edge lane for [`autumn-web`](https://crates.io/crates/autumn-web) applications: the
runtime, wire protocol, and platform seams that let opt-in read-path `GET` handlers compile
into a portable `wasm32-wasip1` artifact and run at a CDN edge, with the origin binary
staying the authority and the fallback.

This crate is deliberately tiny and dependency-light — it is the only Autumn crate that
compiles for **both** `x86_64` and `wasm32-wasip1`. It never links tokio, hyper, or mio.

```toml
[dependencies]
autumn-edge = "0.6.0"

[target.'cfg(not(target_arch = "wasm32"))'.dependencies]
autumn-web = { version = "0.6.0", features = ["edge"] }
```

## What is here

| Piece | Purpose |
| --- | --- |
| `EdgeRoute` / `build_edge_router` | the same axum router the origin builds, restricted to the edge route table |
| `EdgeKv` / `EdgeCache` | the mediated platform seam — one handler source, two substrates |
| `serve` / `serve_io` | the guest dialogue loop (NDJSON over stdio) |
| `wire` | frame types, header canonicalization, fallthrough reasons |
| `conformance` | the shared projection + verdict used by the byte-identity test |
| `host` (feature) | a reference wasmi host with a hand-written WASI shim, for tests |

See `docs/guide/edge.md` in the Autumn repository for the protocol specification, the
header contract, and the determinism rules.
