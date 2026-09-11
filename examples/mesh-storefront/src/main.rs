//! The caller half of the wire-contract example (issue #1755).
//!
//! `wire_client!` builds `CatalogClient` from the catalog service's own
//! endpoint markers, so every method's request and response types are the
//! catalog's real types. `#[contract_checked]` then holds each call site to
//! what the catalog actually produces and accepts.
//!
//! To watch the check fire, see `examples/mesh-storefront/README.md`.

use autumn_web::http::Client;
use autumn_web::prelude::*;

wire_client! {
    name = CatalogClient,
    endpoints = [
        mesh_catalog::get_item_endpoint(id),
        mesh_catalog::create_item_endpoint,
    ],
}

/// Where the catalog service is listening.
fn catalog_base_url() -> String {
    std::env::var("CATALOG_URL").unwrap_or_else(|_| "http://127.0.0.1:3001".to_owned())
}

/// Render one item, reading `name` and `price_cents` off the response.
#[contract_checked(client = CatalogClient)]
#[get("/items/{id}")]
#[public]
async fn show_item(id: Path<String>, http: Client) -> AutumnResult<Markup> {
    let catalog = CatalogClient::new(catalog_base_url(), http);
    let item = catalog.get_item(&*id, NoBody).await?;
    Ok(html! {
        h1 { (item.name) }
        p { "$" (item.price_cents / 100) }
    })
}

/// Create an item, setting every field the catalog requires.
#[contract_checked(client = CatalogClient)]
#[post("/items")]
#[public]
async fn add_item(http: Client) -> AutumnResult<Markup> {
    let catalog = CatalogClient::new(catalog_base_url(), http);
    // A `..rest` initializer is the shape the contract check exists for: it
    // keeps compiling when the catalog adds a required field, and 400s in
    // production. The check turns that into a build failure here.
    let created = catalog
        .create_item(mesh_catalog::NewItem {
            name: "Kettle".to_owned(),
            price_cents: 2499,
            ..Default::default()
        })
        .await?;
    Ok(html! { p { "created " (created.id) } })
}

#[autumn_web::main]
async fn main() {
    autumn_web::app()
        .routes(routes![show_item, add_item])
        .run()
        .await;
}
