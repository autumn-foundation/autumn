// The caller builds its request with `..Default::default()` and omits a field
// the service requires. This compiles; the request 400s in production.
use autumn_web::http::Client;
use autumn_web::prelude::*;

#[derive(serde::Serialize, serde::Deserialize, WireShape)]
pub struct Item {
    pub id: String,
}

#[derive(Default, serde::Serialize, serde::Deserialize, WireShape)]
pub struct NewItem {
    pub name: String,
    pub price_cents: u32,
}

#[endpoint(service = "catalog")]
#[post("/items")]
#[public]
pub async fn create_item(body: Json<NewItem>) -> AutumnResult<Json<Item>> {
    let _ = body;
    todo!()
}

wire_client! {
    name = CatalogClient,
    endpoints = [create_item_endpoint],
}

#[contract_checked(client = CatalogClient)]
async fn create(catalog: CatalogClient) -> AutumnResult<String> {
    let item = catalog
        .create_item(NewItem {
            name: "Kettle".to_owned(),
            ..Default::default()
        })
        .await?;
    Ok(item.id)
}

fn main() {
    let _ = CatalogClient::new("http://catalog", Client::new());
}
