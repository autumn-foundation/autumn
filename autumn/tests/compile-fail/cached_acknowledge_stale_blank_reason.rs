// Compile-fail: an acknowledged-stale opt-out must carry its justification.
use autumn_web::cached;

#[cached(acknowledge_stale = "   ")]
async fn recent_posts() -> Vec<String> {
    Vec::new()
}

fn main() {}
