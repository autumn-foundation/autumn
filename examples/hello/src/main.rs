use autumn_web::prelude::*;

#[get("/")]
async fn index() -> &'static str {
    "Welcome to Autumn!"
}

#[get("/hello")]
async fn hello() -> &'static str {
    "Hello, Autumn!"
}

#[get("/hello/{name}")]
async fn hello_name(name: Path<String>) -> String {
    format!("Hello, {}!", *name)
}

#[autumn_web::main]
async fn main() {
    autumn_web::app()
        .routes(routes![index, hello, hello_name])
        .run()
        .await;
}
