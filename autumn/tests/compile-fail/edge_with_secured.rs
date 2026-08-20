// The route macro replaces the whole handler with its diagnostic, which leaves
// the imports unused. Silenced so the golden below is the macro error alone —
// rustc's unused-import note text is not a thing this fixture is testing.
#![allow(unused_imports)]

use autumn_web::{edge, get, secured};

// The edge capsule has no session or auth state, so an authenticated route
// cannot be served from it — stacking the two attributes is a compile error in
// either order.
#[get("/dashboard")]
#[edge]
#[secured("admin")]
async fn dashboard() -> &'static str {
    "dash"
}

fn main() {}
