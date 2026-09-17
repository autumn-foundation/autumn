// A client with no value in the annotated function would leave the attribute
// checking nothing, so it is refused.
use autumn_web::prelude::*;

pub struct CatalogClient;

#[contract_checked(client = CatalogClient)]
async fn show() -> AutumnResult<String> {
    Ok(String::new())
}

fn main() {}
