// The route macro replaces the whole handler with its diagnostic, which leaves
// the imports unused. Silenced so the golden below is the macro error alone —
// rustc's unused-import note text is not a thing this fixture is testing.
#![allow(unused_imports)]

use autumn_web::{edge, post};

// The edge lane is read-path only: a write route can never be served from the
// capsule, so opting one in is a compile error rather than a silent no-op.
#[post("/items")]
#[edge]
async fn create_item() -> &'static str {
    "created"
}

fn main() {}
