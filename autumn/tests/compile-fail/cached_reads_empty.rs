// Compile-fail: `#[cached(reads())]` declares nothing, which would silently
// weaken the coherence gate to "no dependencies" instead of "not declared".
use autumn_web::cached;

#[cached(reads())]
async fn recent_posts() -> Vec<String> {
    Vec::new()
}

fn main() {}
