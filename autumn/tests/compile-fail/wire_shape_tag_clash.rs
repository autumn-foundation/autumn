// Two fields with one `#[wire(tag = N)]` would share one protobuf field
// number, so a consumer could not tell them apart.
use autumn_web::prelude::*;

#[derive(serde::Serialize, serde::Deserialize, WireShape)]
pub struct Item {
    #[wire(tag = 1)]
    pub id: String,
    #[wire(tag = 1)]
    pub name: String,
}

fn main() {}
