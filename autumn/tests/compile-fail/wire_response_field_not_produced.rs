// A response field the caller reads is `#[serde(skip_serializing)]`, so the
// service never puts it on the wire. The types still line up, so nothing but
// the contract check can catch this.
use autumn_web::http::Client;
use autumn_web::prelude::*;

#[derive(serde::Serialize, serde::Deserialize, WireShape)]
pub struct Item {
    pub id: String,
    #[serde(skip_serializing)]
    pub name: String,
}

#[endpoint(service = "catalog")]
#[get("/items/{id}")]
#[public]
pub async fn get_item(id: Path<String>) -> AutumnResult<Json<Item>> {
    let _ = id;
    todo!()
}

wire_client! {
    name = CatalogClient,
    endpoints = [get_item_endpoint(id)],
}

#[contract_checked(client = CatalogClient)]
async fn show(catalog: CatalogClient) -> AutumnResult<String> {
    let item = catalog.get_item("1", NoBody).await?;
    Ok(item.name)
}

fn main() {
    let _ = CatalogClient::new("http://catalog", Client::new());
}
