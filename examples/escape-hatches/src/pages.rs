//! Browser pages. This is the convention: `#[get]`, a repository, `Markup`.

use autumn_web::prelude::*;

use crate::repositories::{PgProductRepository, ProductRepository, product_by_sku};

/// The page frame. The error pages (H10) use it too.
#[must_use]
pub fn layout(title: &str, body: &Markup) -> Markup {
    html! {
        (maud::DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (title) " · Stockroom" }
            }
            body {
                header { nav { a href="/" { "Stockroom" } " · " a href="/supplier/items" { "Supplier catalog" } } }
                main { (body) }
            }
        }
    }
}

/// The product list.
#[get("/")]
#[public]
pub async fn index(repo: PgProductRepository) -> AutumnResult<Markup> {
    let products = repo.find_all().await?;
    Ok(layout(
        "Products",
        &html! {
            h1 { "Products" }
            table {
                thead { tr { th { "SKU" } th { "Name" } th { "Category" } th { "Stock" } } }
                tbody {
                    @for product in &products {
                        tr {
                            td { a href={ "/products/" (product.sku) } { (product.sku) } }
                            td { (product.name) }
                            td { (product.category) }
                            td { (product.stock) }
                        }
                    }
                }
            }
        },
    ))
}

/// One product.
#[get("/products/{sku}")]
#[public]
pub async fn product(Path(sku): Path<String>, repo: PgProductRepository) -> AutumnResult<Markup> {
    let product = product_by_sku(&repo, &sku).await?;
    Ok(layout(
        &product.name,
        &html! {
            h1 { (product.name) }
            dl {
                dt { "SKU" } dd { (product.sku) }
                dt { "Category" } dd { (product.category) }
                dt { "Stock" } dd { (product.stock) }
                dt { "Price" } dd { (format_cents(product.price_cents)) }
            }
        },
    ))
}

fn format_cents(cents: i64) -> String {
    format!("${}.{:02}", cents / 100, cents % 100)
}
