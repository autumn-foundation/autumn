use autumn_web::{get, routes};

#[get()]
async fn index() -> &'static str {
    "Hello!"
}

fn main() {
    let _ = routes![index];
}
