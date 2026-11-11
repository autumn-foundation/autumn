// Nested reads and writes compile. A caller reads `item.details.color` and
// sets `Details { … }` inside the request. A listener reads
// `event.address.city`. The const assertions check the first segment only.
use autumn_web::http::Client;
use autumn_web::prelude::*;

#[derive(Default, serde::Serialize, serde::Deserialize, WireShape)]
pub struct Details {
    pub color: Option<String>,
    pub size: u32,
}

#[derive(serde::Serialize, serde::Deserialize, WireShape)]
pub struct Item {
    pub id: String,
    pub details: Details,
}

#[derive(Default, serde::Serialize, serde::Deserialize, WireShape)]
pub struct NewItem {
    pub name: String,
    pub details: Details,
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
    let label = format!("{} {}", item.details.size, item.id);
    let Item { details, .. } = item;
    let created = catalog
        .create_item(NewItem {
            name: label,
            details: Details {
                color: details.color,
                ..Default::default()
            },
        })
        .await?;
    Ok(created.id)
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, WireShape)]
pub struct Address {
    pub city: String,
    pub zip: Option<String>,
}

#[event(name = "contact.moved")]
pub struct ContactMoved {
    pub id: i64,
    pub address: Address,
}

#[listener(ContactMoved, durable)]
async fn on_moved(_state: AppState, event: ContactMoved) -> AutumnResult<()> {
    let _ = (event.id, event.address.city);
    Ok(())
}

fn main() {
    let _ = CatalogClient::new("http://catalog", Client::new());
}
