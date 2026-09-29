// `#[serde(into = "…")]` puts a different type's keys on the wire, so none of
// this struct's fields are on it. A descriptor listing them would be fiction.
use autumn_web::prelude::*;

#[derive(Clone, serde::Serialize)]
pub struct ItemDto {
    pub sku: String,
}

impl From<Item> for ItemDto {
    fn from(_: Item) -> Self {
        todo!()
    }
}

#[derive(Clone, serde::Serialize, WireShape)]
#[serde(into = "ItemDto")]
pub struct Item {
    pub id: String,
}

fn main() {}
