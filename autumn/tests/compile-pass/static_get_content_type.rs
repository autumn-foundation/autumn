use autumn_web::prelude::*;

#[static_get("/logo.png", content_type = "application/octet-stream")]
async fn logo() -> Vec<u8> {
    Vec::new()
}

fn main() {
    let meta = __autumn_static_meta_logo();
    assert_eq!(meta.content_type, Some("application/octet-stream"));
}
