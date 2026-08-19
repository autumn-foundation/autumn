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
