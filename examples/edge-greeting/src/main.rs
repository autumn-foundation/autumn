//! The origin binary: the whole app, serving every route.
//!
//! Nothing here is edge-specific except one line — `.with_edge_kv(...)`, which
//! puts a store behind the [`EdgeCache`](autumn_edge::EdgeCache) seam so an
//! `#[edge]` handler reads the same way at the origin as it does at the edge.
//! The edge routes are mounted exactly like the origin-only ones, because the
//! origin serves everything: that is what makes a fallthrough from the edge
//! require no glue at all.
//!
//! ```sh
//! cargo run -p edge-greeting
//! curl http://localhost:3000/greet/ada
//! ```

#[autumn_web::main]
async fn main() {
    autumn_web::app()
        .routes(edge_greeting::origin::routes())
        .with_edge_kv(edge_greeting::demo_kv())
        .run()
        .await;
}
