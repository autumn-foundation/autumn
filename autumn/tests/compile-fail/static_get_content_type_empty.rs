use autumn_web::static_get;

#[static_get("/logo.png", content_type = "")]
async fn logo() -> &'static str {
    "logo"
}

fn main() {}
