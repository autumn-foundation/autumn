use autumn_web::{edge, post};

// The edge lane is read-path only: a write route can never be served from the
// capsule, so opting one in is a compile error rather than a silent no-op.
#[post("/items")]
#[edge]
async fn create_item() -> &'static str {
    "created"
}

fn main() {}
