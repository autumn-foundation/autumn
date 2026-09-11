// The compatible half of the falsification: a caller that reads only fields the
// service produces, and supplies every field it requires, compiles. This is the
// "restore compatibility and the build is green again" case.
use autumn_web::http::Client;
use autumn_web::prelude::*;

#[derive(serde::Serialize, serde::Deserialize, WireShape)]
pub struct Item {
    pub id: String,
    pub name: String,
    // Renamed on the wire, and sometimes absent: neither is a break.
    #[serde(rename = "priceCents", skip_serializing_if = "Option::is_none")]
    pub price_cents: Option<u32>,
}

#[derive(Default, serde::Serialize, serde::Deserialize, WireShape)]
pub struct NewItem {
    pub name: String,
    #[serde(default)]
    pub note: Option<String>,
}

#[endpoint(service = "catalog")]
#[get("/items/{id}")]
#[public]
pub async fn get_item(id: Path<String>) -> AutumnResult<Json<Item>> {
    let _ = id;
    todo!()
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
    endpoints = [get_item_endpoint(id), create_item_endpoint],
}

#[contract_checked(client = CatalogClient)]
async fn show(catalog: CatalogClient) -> AutumnResult<String> {
    let item = catalog.get_item("1", NoBody).await?;
    let created = catalog
        .create_item(NewItem {
            name: item.name,
            ..Default::default()
        })
        .await?;
    Ok(created.id)
}

fn main() {
    let _ = CatalogClient::new("http://catalog", Client::new());
}
