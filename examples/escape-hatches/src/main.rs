//! Entry point. `src/lib.rs` builds the app, so tests can build the same one.

#[autumn_web::main]
async fn main() {
    escape_hatches::app().run().await;
}
