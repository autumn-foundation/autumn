// `#[endpoint]` below the route attribute reads a signature the route macro has
// already rewritten, so it is refused rather than guessed at.
use autumn_web::prelude::*;

#[derive(serde::Serialize, serde::Deserialize, WireShape)]
pub struct Item {
    pub id: String,
}

#[get("/items/{id}")]
#[endpoint(service = "catalog")]
#[public]
pub async fn get_item(id: Path<String>) -> AutumnResult<Json<Item>> {
    let _ = id;
    todo!()
}

fn main() {}
