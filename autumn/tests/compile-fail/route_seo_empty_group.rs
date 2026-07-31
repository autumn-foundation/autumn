use autumn_web::get;

#[get("/about", seo())]
async fn about() -> &'static str {
    "About"
}

fn main() {}
