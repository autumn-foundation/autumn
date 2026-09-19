//! Run the catalog service.

#[autumn_web::main]
async fn main() {
    autumn_web::app().routes(mesh_catalog::routes()).run().await;
}
