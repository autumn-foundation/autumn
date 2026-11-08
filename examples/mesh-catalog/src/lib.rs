//! The callee half of the wire-contract example (issue #1755).
//!
//! Two handlers, each marked `#[endpoint]`. The attribute reads the request and
//! response types straight off the signature and emits the contract the
//! storefront is compiled against — there is no IDL and no hand-written schema.
//!
//! One event, `ItemRepriced`. `#[event]` records its payload shape, and the
//! storefront's listener is checked against it.

use autumn_web::prelude::*;

/// A catalog item, as the service puts it on the wire.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, WireShape)]
pub struct Item {
    /// Shelf label. Optional, and no caller reads it, so a later release may
    /// remove it.
    pub subtitle: Option<String>,
    /// Stable item identifier.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Price in cents.
    pub price_cents: u32,
}

/// What a caller sends to create an item.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, WireShape)]
pub struct NewItem {
    /// Display name. Required.
    pub name: String,
    /// Price in cents. Required.
    pub price_cents: u32,
    /// Idempotency key. Required by the service — and dropped from the body
    /// when it is empty, which is what makes it the one field a call site has
    /// to set for itself. A `..rest` initializer would leave it empty, the
    /// body would go out without it, and the service would reject the request.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub request_id: String,
    /// Optional note. Absent from a request body is fine.
    #[serde(default)]
    pub note: Option<String>,
}

/// Sent when an item's price changes. The storefront keeps its own copy of
/// the price, so it reads this event instead of calling `get_item`.
#[event(name = "catalog.item_repriced")]
pub struct ItemRepriced {
    /// Stable item identifier.
    pub id: String,
    /// New price in cents.
    pub price_cents: u32,
}

impl Item {
    /// A stand-in record. A real service would read one from its store; the
    /// example keeps every construction in one place so the README's seeded
    /// change is a one-line edit.
    fn sample(id: String, name: String, price_cents: u32) -> Self {
        Self {
            subtitle: None,
            id,
            name,
            price_cents,
        }
    }
}

/// Fetch one item.
#[endpoint(service = "catalog")]
#[get("/items/{id}")]
#[public]
pub async fn get_item(id: Path<String>) -> AutumnResult<Json<Item>> {
    Ok(Json(Item::sample(
        id.to_string(),
        format!("Item {}", *id),
        1299,
    )))
}

/// Create an item.
#[endpoint(service = "catalog")]
#[post("/items")]
#[public]
pub async fn create_item(body: Json<NewItem>) -> AutumnResult<Json<Item>> {
    Ok(Json(Item::sample(
        "new".to_owned(),
        body.0.name,
        body.0.price_cents,
    )))
}

/// The catalog service's routes.
#[must_use]
pub fn routes() -> Vec<autumn_web::Route> {
    routes![get_item, create_item]
}
