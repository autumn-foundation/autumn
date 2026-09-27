// The route macro replaces the whole handler with its diagnostic, which leaves
// the imports unused. Silenced so the golden below is the macro error alone —
// rustc's unused-import note text is not a thing this fixture is testing.
#![allow(unused_imports)]

use autumn_web::{edge, static_get};

// A `#[static_get]` page is pre-rendered at build time and served CDN-side
// already, so `#[edge]` adds nothing and is rejected rather than ignored.
#[static_get("/about")]
#[edge]
async fn about() -> &'static str {
    "about"
}

fn main() {}
