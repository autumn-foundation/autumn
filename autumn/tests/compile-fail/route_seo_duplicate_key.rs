use autumn_web::get;

#[get("/about", seo(title = "A", title = "B"))]
async fn about() -> &'static str {
    "About"
}

fn main() {}
