// `#[event]` refuses a payload whose wire shape it cannot read, and names the
// `opaque` opt-out.
use autumn_web::prelude::*;

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
pub struct Extra {
    pub note: String,
}

#[event(name = "contact.updated")]
pub struct ContactUpdated {
    pub id: i64,
    #[serde(flatten)]
    pub extra: Extra,
}

fn main() {}
