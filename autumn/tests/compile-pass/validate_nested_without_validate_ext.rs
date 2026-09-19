// Compile-pass: `#[validate(nested)]` compiles cleanly when the struct's own
// defining module does not import `autumn_web`'s `ValidateExt` (or the
// prelude), even though another module in the same crate does. Method
// resolution for the bare `.validate()` call `nested`'s codegen emits is
// scoped to where the derive is textually expanded -- the struct's own
// module -- not to any downstream consumer's module. This is the workaround
// named in `ValidateExt`'s doc comment and in the companion compile-fail
// fixture `validate_nested_collides_with_validate_ext.rs` (issue #1751).
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

// A separate module importing the prelude does not affect `Customer`'s own
// module above -- proving the hazard is genuinely scope-local, not crate-wide.
mod uses_prelude_elsewhere {
    #[allow(unused_imports)]
    use autumn_web::prelude::*;

    pub fn touch(c: &super::Customer) -> bool {
        validator::Validate::validate(c).is_ok()
    }
}

fn main() {
    let c = Customer {
        address: Address {
            street: "1 Main St".to_string(),
        },
    };
    assert!(uses_prelude_elsewhere::touch(&c));
}
