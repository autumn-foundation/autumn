// The route macro replaces the whole handler with its diagnostic, which leaves
// the imports unused. Silenced so the golden below is the macro error alone —
// rustc's unused-import note text is not a thing this fixture is testing.
#![allow(unused_imports)]

use autumn_web::{edge, get};

#[derive(Clone)]
struct StampLayer;

// `#[intercept]` layers are origin-only tower middleware: the native mount
// wraps the handler in them, the edge capsule cannot, and a silently different
// response would defeat the byte-identity guarantee — so stacking the two
// attributes is a compile error.
#[get("/stamped")]
#[edge]
#[intercept(StampLayer)]
async fn stamped() -> &'static str {
    "stamped"
}

fn main() {}
