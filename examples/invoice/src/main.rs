// `routes!` needs the fully-qualified path (not a `use`-imported bare name)
// so it can resolve each handler's hidden `__autumn_route_info_*` companion
// function relative to the same path — see `examples/reddit-clone/src/main.rs`
// for the same cross-crate lib+bin pattern.
#[autumn_web::main]
async fn main() {
    autumn_web::app()
        .routes(autumn_web::routes![
            invoice::invoice_detail,
            invoice::invoice_pdf
        ])
        .run()
        .await;
}
