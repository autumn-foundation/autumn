use autumn_web::get;

#[get("/landing", group = " ")]
async fn landing() -> &'static str {
    "landing"
}

fn main() {}
