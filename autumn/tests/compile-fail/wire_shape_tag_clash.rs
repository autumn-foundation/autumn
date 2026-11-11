// Two fields with one `#[wire(tag = N)]` share one protobuf field number.
// A consumer cannot tell them apart.
use autumn_web::prelude::*;

#[derive(serde::Serialize, serde::Deserialize, WireShape)]
pub struct Item {
    #[wire(tag = 1)]
    pub id: String,
    #[wire(tag = 1)]
    pub name: String,
}

fn main() {}
