// A request field the caller sets is `#[serde(skip_deserializing)]`, so the
// service drops it. The caller still compiles against the same struct.
use autumn_web::http::Client;
use autumn_web::prelude::*;

#[derive(serde::Serialize, serde::Deserialize, WireShape)]
pub struct Item {
    pub id: String,
}

#[derive(Default, serde::Serialize, serde::Deserialize, WireShape)]
pub struct NewItem {
    pub name: String,
    #[serde(skip_deserializing)]
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
            price_cents: 2499,
        })
        .await?;
    Ok(item.id)
}

fn main() {
    let _ = CatalogClient::new("http://catalog", Client::new());
}
