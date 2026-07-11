//! Compiled regression guard for #1719 / Codex P2.
//!
//! `#[autumn_web::model]` generates an `Update{Model}` whose fields are
//! `Patch<T>` and which `#[derive(validator::Validate)]`. For a field like
//! `#[validate(ip)] ip: Option<String>`, the update field is
//! `Patch<Option<String>>`. `validator` 0.20 provides `ValidateIp` ONLY via the
//! blanket `impl<T: ToString> ValidateIp for T` (validation/ip.rs) — there is no
//! `impl ValidateIp for Option<T>` — so `Patch<Option<String>>: ValidateIp` is
//! unsatisfiable and, before the macro fix, this module FAILED TO COMPILE with
//! the trait bound `Option<String>: ValidateIp` not satisfied.
//!
//! The macro now drops `ip` from the generated PATCH fields for `Option<…>`
//! columns only, so this module compiles. Because it lives in the consolidated
//! `integration_tests` binary, `cargo build --tests -p autumn-web` is the real
//! build-time regression guard.

#![cfg(feature = "db")]

use validator::Validate;

use autumn_web::hooks::Patch;

mod schema {
    autumn_web::reexports::diesel::table! {
        patch_ip_hosts (id) {
            id -> Int8,
            ip -> Nullable<Text>,
            ip2 -> Text,
            name -> Nullable<Text>,
        }
    }
}

use schema::patch_ip_hosts;

#[autumn_web::model(table = "patch_ip_hosts")]
pub struct PatchIpHost {
    #[id]
    pub id: i64,
    // Breaking case: Option<String> + ip. `ip` is dropped from the generated
    // `UpdatePatchIpHost` patch field (no `impl ValidateIp for Option<T>`).
    #[validate(ip)]
    pub ip: Option<String>,
    // Non-Option + ip: `ip` stays enforced on the update path
    // (`Patch<String>: ValidateIp` holds via the `ToString` blanket).
    #[validate(ip)]
    pub ip2: String,
    // Option + length: `length` must NOT be over-filtered (validator ships an
    // `Option<T>` impl for `ValidateLength`).
    #[validate(length(min = 1))]
    pub name: Option<String>,
}

#[test]
fn update_model_compiles_and_validates_expected_fields() {
    // A fully valid patch validates cleanly.
    let ok = UpdatePatchIpHost {
        ip: Patch::Set(Some("192.168.0.1".to_string())),
        ip2: Patch::Set("::1".to_string()),
        name: Patch::Set(Some("db".to_string())),
    };
    assert!(ok.validate().is_ok(), "valid patch must pass validation");

    // `ip` on the non-Option `ip2` field is STILL enforced on the update path.
    let bad_ip2 = UpdatePatchIpHost {
        ip: Patch::Unchanged,
        ip2: Patch::Set("not-an-ip".to_string()),
        name: Patch::Unchanged,
    };
    assert!(
        bad_ip2.validate().is_err(),
        "an invalid `ip2` (non-Option ip column) must fail validation on update"
    );

    // `length` on the Option `name` column is NOT over-filtered.
    let bad_name = UpdatePatchIpHost {
        ip: Patch::Unchanged,
        ip2: Patch::Unchanged,
        name: Patch::Set(Some(String::new())),
    };
    assert!(
        bad_name.validate().is_err(),
        "an empty `name` (Option length column) must fail validation on update"
    );

    // Documented tradeoff: `ip` is dropped from the Option `ip` patch field, so
    // an invalid value there is NOT caught on the update path (it is still
    // enforced on create, where the derive unwraps the Option).
    let unenforced_ip = UpdatePatchIpHost {
        ip: Patch::Set(Some("not-an-ip".to_string())),
        ip2: Patch::Unchanged,
        name: Patch::Unchanged,
    };
    assert!(
        unenforced_ip.validate().is_ok(),
        "`ip` is intentionally not enforced on the Option<String> update field"
    );
}
