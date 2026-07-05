use autumn_web::prelude::*;

#[get("/")]
async fn index() -> &'static str {
    "Welcome to Autumn!"
}

#[get("/hello/{name}")]
async fn hello_name(name: autumn_web::extract::Path<String>) -> String {
    format!("Hello, {}!", *name)
}

#[autumn_web::main]
async fn main() {
    autumn_web::app()
        .routes(routes![index, hello_name])
        .run()
        .await;
}
