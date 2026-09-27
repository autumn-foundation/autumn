//! The smallest Autumn app — and the smallest complete example of the
//! security-posture story.
//!
//! Every handler here is deliberately unauthenticated, so every one carries
//! `#[public]`. That is not ceremony: `autumn routes audit` refuses to classify
//! a route it cannot prove anything about, and `security-posture.json` next to
//! this file is the manifest it emits. `scripts/check-posture-gate.sh` diffs
//! that committed manifest against a freshly built one on every pull request,
//! so turning one of these routes into something wider — or dropping a guard
//! elsewhere — shows up as a posture finding rather than as three lines of
//! green diff. See `docs/guide/posture-gate.md`.

use autumn_web::prelude::*;

#[get("/")]
#[public]
async fn index() -> &'static str {
    "Welcome to Autumn!"
}

#[get("/hello")]
#[public]
async fn hello() -> &'static str {
    "Hello, Autumn!"
}

#[get("/hello/{name}")]
#[public]
async fn hello_name(name: Path<String>) -> String {
    format!("Hello, {}!", *name)
}

#[autumn_web::main]
async fn main() {
    autumn_web::app()
        .routes(routes![index, hello, hello_name])
        .run()
        .await;
}
