// The caller leaves a required request field to `..Default::default()`, and the
// request type drops that field from the body when it is empty — so the body
// goes out without it and the service rejects the request. This compiles, and
// the type checker has nothing to say about it.
use autumn_web::http::Client;
use autumn_web::prelude::*;

#[derive(serde::Serialize, serde::Deserialize, WireShape)]
pub struct Item {
    pub id: String,
}

#[derive(Default, serde::Serialize, serde::Deserialize, WireShape)]
pub struct NewItem {
    pub name: String,
    // Required by the service, and absent from the body when empty.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub request_id: String,
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
