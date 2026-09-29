// The endpoint name becomes the marker type's identifier, so a name that is not
// identifier-shaped is refused rather than panicking the macro.
use autumn_web::prelude::*;

#[derive(serde::Serialize, serde::Deserialize, WireShape)]
pub struct Item {
    pub id: String,
}

#[endpoint(service = "catalog", name = "get-item")]
#[get("/items")]
#[public]
pub async fn get_item() -> AutumnResult<Json<Item>> {
    todo!()
}

fn main() {}
