// The route macro's `Extension<T>` refusal only scans for that one name — it
// says nothing about the whole-`Request` extractor, which can read
// `.extensions()` by hand and observe the same origin-only state. The sealed
// `EdgeExtract` whitelist in `autumn_edge::handler` closes this: `Request` is
// simply not on the list, whatever a handler does with it.
#![allow(unused_imports)]

use autumn_web::{edge, get};

#[get("/tenant")]
#[edge]
async fn tenant(req: axum::extract::Request) -> &'static str {
    let _ = req.extensions();
    "tenant"
}

fn main() {}
