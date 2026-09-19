use autumn_web::static_get;

#[static_get("/about", seo(titel = "About"))]
async fn about() -> &'static str {
    "About"
}

fn main() {}
