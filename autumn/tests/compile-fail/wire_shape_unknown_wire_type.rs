// `#[wire(type = …)]` takes a known JSON type only. A typo must not write a
// wire type that an IDL export cannot map.
use autumn_web::prelude::*;

#[derive(serde::Serialize, serde::Deserialize, WireShape)]
pub struct Item {
    pub id: String,
    #[wire(type = "timestamp")]
    pub created_at: i64,
}

fn main() {}
