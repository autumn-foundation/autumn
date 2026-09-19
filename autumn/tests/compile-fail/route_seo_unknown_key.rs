use autumn_web::get;

#[get("/about", seo(titel = "About"))]
async fn about() -> &'static str {
    "About"
}

fn main() {}
