// A flattened field hides keys the descriptor cannot see, so the derive refuses
// the type rather than publishing a shape that is not true of the wire.
use autumn_web::prelude::*;

#[derive(serde::Serialize, serde::Deserialize)]
pub struct Extra {
    pub tier: String,
}

#[derive(serde::Serialize, serde::Deserialize, WireShape)]
pub struct Item {
    pub id: String,
    #[serde(flatten)]
    pub extra: Extra,
}

fn main() {}
