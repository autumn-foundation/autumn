use autumn_web::{edge, static_get};

// A `#[static_get]` page is pre-rendered at build time and served CDN-side
// already, so `#[edge]` adds nothing and is rejected rather than ignored.
#[static_get("/about")]
#[edge]
async fn about() -> &'static str {
    "about"
}

fn main() {}
