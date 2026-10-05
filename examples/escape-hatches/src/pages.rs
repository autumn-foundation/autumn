//! Browser pages. This is the convention: `#[get]`, a repository, `Markup`.

use autumn_web::prelude::*;

use crate::repositories::PgProductRepository;

/// The product list.
#[get("/")]
#[public]
pub async fn index(_repo: PgProductRepository) -> AutumnResult<Markup> {
    todo!("pages")
}

/// One product.
#[get("/products/{sku}")]
#[public]
pub async fn product(Path(_sku): Path<String>, _repo: PgProductRepository) -> AutumnResult<Markup> {
    todo!("pages")
}
