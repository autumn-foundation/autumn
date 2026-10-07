use autumn_web::get;

#[get("/batch", criticality = "urgent")]
async fn batch() -> &'static str {
    "batch"
}

fn main() {}
