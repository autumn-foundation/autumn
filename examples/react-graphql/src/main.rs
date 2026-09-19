//! Entry point. Everything interesting is in `src/lib.rs` — see that file's
//! docs for the layout — so the integration tests can build the same app.

#[autumn_web::main]
async fn main() {
    react_graphql::app().run().await;
}
