// Compile-fail: `#[validate(nested)]` collides with this crate's own
// `ValidateExt` when BOTH are in scope in the struct's own defining module.
// `validator_derive`'s `nested` codegen calls the nested field's value with
// bare method syntax -- `(&field).validate()` -- which is ambiguous between
// `validator::Validate::validate(&self)` and `ValidateExt::validate(self)`
// (a blanket `impl<T: validator::Validate> ValidateExt for T`, applicable to
// the nested field's type too). This is a real hazard for any
// `#[derive(validator::Validate)]` struct -- `#[model]`-generated structs
// included, since `#[model]` forwards `#[validate(...)]` attributes verbatim
// and adds no protection of its own (issue #1751; see the companion
// compile-pass fixture `validate_nested_without_validate_ext.rs`, which shows
// the collision does NOT occur when this same trait is simply not imported in
// the struct's own module).
use autumn_web::prelude::*;
use validator::Validate;

#[derive(Validate)]
pub struct Address {
    #[validate(length(min = 1))]
    pub street: String,
}

#[derive(Validate)]
pub struct Customer {
    #[validate(nested)]
    pub address: Address,
}

fn main() {}
