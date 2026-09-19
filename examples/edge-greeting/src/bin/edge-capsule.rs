//! The edge capsule: the whole of a capsule's `main`.
//!
//! Built with `cargo build --target wasm32-wasip1 --release --bin edge-capsule`
//! (or automatically, by `autumn build`). `serve` reads request frames from
//! stdin and answers each one on stdout until the host closes the stream.

fn main() {
    autumn_edge::serve(edge_greeting::handlers::edge_routes());
}
