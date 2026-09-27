// The client declares a path parameter the endpoint's route no longer takes.
use autumn_web::http::Client;
use autumn_web::prelude::*;

#[derive(serde::Serialize, serde::Deserialize, WireShape)]
pub struct Item {
    pub id: String,
}

#[endpoint(service = "catalog")]
#[get("/items/{sku}")]
#[public]
pub async fn get_item(sku: Path<String>) -> AutumnResult<Json<Item>> {
    let _ = sku;
    todo!()
}

wire_client! {
    name = CatalogClient,
    endpoints = [get_item_endpoint(id)],
}

fn main() {
    let _ = CatalogClient::new("http://catalog", Client::new());
}
