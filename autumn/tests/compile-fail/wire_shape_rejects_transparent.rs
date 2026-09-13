// A `transparent` struct serializes as its one field's value, so there is no
// object on the wire for a field table to describe.
use autumn_web::prelude::*;

#[derive(serde::Serialize, serde::Deserialize, WireShape)]
#[serde(transparent)]
pub struct Token {
    pub value: String,
}

fn main() {}
