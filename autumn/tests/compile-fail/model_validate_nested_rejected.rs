// Compile-fail: `#[validate(nested)]` collides with this crate's own
// `ValidateExt` (also implementing `validate` for every `validator::Validate`
// type) and is rejected outright, before any codegen runs, rather than left
// to fail downstream with a cryptic `E0034` pointing into the derive
// expansion (issue #1751).
use autumn_web::model;

#[derive(validator::Validate)]
pub struct Address {
    #[validate(length(min = 1))]
    pub street: String,
}

#[model]
pub struct Order {
    #[id]
    pub id: i64,
    #[validate(nested)]
    pub shipping_address: Address,
}

fn main() {}
