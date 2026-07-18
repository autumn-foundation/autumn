//! #1778 — validate the effective *merged model* on the update path.
//!
//! `#[autumn_web::model]` now derives `validator::Validate` on the read model
//! and keeps the field `#[validate(...)]` rules there. The generated
//! `from_patch(current, patch)` merges the patch onto a clone of the existing
//! row and validates the resulting *concrete* model before returning the draft.
//!
//! Because the merged model's fields are concrete `T` (not `Patch<T>`), this
//! path enforces the validators that hit hard `E0119` trait-coherence walls on
//! `Patch<T>` — `ip` on an `Option<_>` column and `does_not_contain` — and the
//! cross-field / struct-level ones (`custom`) that no single-field `Patch<T>`
//! trait can express. All of them are still (correctly) dropped from the
//! `Update{Model}` `Patch<T>` fields by the denylist, so the *only* thing that
//! rejects an invalid-once-merged patch here is the merged-model check.
//!
//! Living in the consolidated `integration_tests` binary makes
//! `cargo build --tests -p autumn-web` the compile-time regression guard: the
//! module wouldn't compile at all if the merged-model validators reintroduced
//! the E0119 walls.

#![cfg(feature = "db")]

use validator::Validate;

use autumn_web::hooks::{Patch, UpdateDraft};

mod schema {
    autumn_web::reexports::diesel::table! {
        merged_hosts (id) {
            id -> Int8,
            ip -> Nullable<Text>,
            blurb -> Text,
            slug -> Text,
        }
    }
}

use schema::merged_hosts;

/// Reject any value containing an uppercase letter — stands in for the
/// cross-field / struct-level `custom` validators that have no `Patch<T>` impl
/// and are therefore create-only on the patch-struct path, but run here against
/// the concrete merged value.
fn no_uppercase(value: &str) -> Result<(), validator::ValidationError> {
    if value.chars().any(char::is_uppercase) {
        return Err(validator::ValidationError::new("no_uppercase"));
    }
    Ok(())
}

#[autumn_web::model(table = "merged_hosts")]
pub struct MergedHost {
    #[id]
    pub id: i64,
    // `ip` on an `Option<String>` column: the E0119 wall. Dropped from the
    // generated `UpdateMergedHost` patch field, enforced here on the merged
    // concrete `Option<String>`.
    #[validate(ip)]
    pub ip: Option<String>,
    // `does_not_contain`: blanket-inversion coherence wall on `Patch<T>`.
    // Dropped from the patch field, enforced on the merged concrete `String`.
    #[validate(does_not_contain(pattern = "bad"))]
    pub blurb: String,
    // `custom`: no `Patch<T>` impl (struct/cross-field class). Dropped from the
    // patch field, enforced on the merged concrete value.
    #[validate(custom(function = "no_uppercase"))]
    pub slug: String,
}

fn valid_current() -> MergedHost {
    MergedHost {
        id: 1,
        ip: Some("10.0.0.1".to_string()),
        blurb: "all good".to_string(),
        slug: "host-one".to_string(),
    }
}

/// The `Update{Model}` still (correctly) drops these validators from its
/// `Patch<T>` fields, so validating the *patch struct* on its own never rejects
/// them — proving the merged-model path is what does the work below.
#[test]
fn update_model_patch_struct_still_drops_walled_validators() {
    let patch = UpdateMergedHost {
        ip: Patch::Set(Some("not-an-ip".to_string())),
        blurb: Patch::Set("this is bad".to_string()),
        slug: Patch::Set("SHOUTING".to_string()),
    };
    // On the patch struct alone every walled/cross-field validator is dropped,
    // so it "passes". The merged model (below) is where they are enforced.
    assert!(
        patch.validate().is_ok(),
        "patch-struct validation must remain create-only for the walled validators"
    );
}

#[test]
fn from_patch_accepts_a_valid_merged_model() {
    let current = valid_current();
    let patch = UpdateMergedHost {
        ip: Patch::Set(Some("192.168.0.2".to_string())),
        blurb: Patch::Set("still fine".to_string()),
        slug: Patch::Set("host-two".to_string()),
    };
    let draft = <UpdateDraft<MergedHost> as MergedHostDraftExt>::from_patch(&current, &patch)
        .expect("a valid merged model must pass validation");
    assert_eq!(draft.after().ip.as_deref(), Some("192.168.0.2"));
    assert_eq!(draft.after().slug, "host-two");
}

#[test]
fn from_patch_rejects_ip_invalid_once_merged() {
    // `ip` is dropped from the `Patch<Option<String>>` field (E0119), so this
    // is caught *only* by validating the merged model.
    let current = valid_current();
    let patch = UpdateMergedHost {
        ip: Patch::Set(Some("not-an-ip".to_string())),
        ..Default::default()
    };
    let err = <UpdateDraft<MergedHost> as MergedHostDraftExt>::from_patch(&current, &patch)
        .expect_err("an invalid merged `ip` must be rejected");
    assert_eq!(
        err.status(),
        autumn_web::reexports::http::StatusCode::UNPROCESSABLE_ENTITY,
        "merged-model validation failures must surface as 422"
    );
}

#[test]
fn from_patch_rejects_does_not_contain_invalid_once_merged() {
    // `does_not_contain` is dropped from the patch field; the merged concrete
    // `String` is what enforces it.
    let current = valid_current();
    let patch = UpdateMergedHost {
        blurb: Patch::Set("contains a bad word".to_string()),
        ..Default::default()
    };
    let err = <UpdateDraft<MergedHost> as MergedHostDraftExt>::from_patch(&current, &patch)
        .expect_err("a merged `blurb` containing the forbidden pattern must be rejected");
    assert_eq!(
        err.status(),
        autumn_web::reexports::http::StatusCode::UNPROCESSABLE_ENTITY
    );
}

#[test]
fn from_patch_rejects_custom_invalid_once_merged() {
    // `custom` has no `Patch<T>` impl; only the merged model runs it.
    let current = valid_current();
    let patch = UpdateMergedHost {
        slug: Patch::Set("Has-Uppercase".to_string()),
        ..Default::default()
    };
    let err = <UpdateDraft<MergedHost> as MergedHostDraftExt>::from_patch(&current, &patch)
        .expect_err("a merged `slug` failing the custom rule must be rejected");
    assert_eq!(
        err.status(),
        autumn_web::reexports::http::StatusCode::UNPROCESSABLE_ENTITY
    );
}

#[test]
fn from_patch_validates_fields_left_untouched_by_the_patch() {
    // The patch only touches `slug`; `blurb` keeps the existing (invalid) row
    // value. Merged-model validation sees the whole record, so a pre-existing
    // violation surfaces — proving we validate the *effective* model, not just
    // the changed fields.
    let mut current = valid_current();
    current.blurb = "already bad".to_string();
    let patch = UpdateMergedHost {
        slug: Patch::Set("fresh-slug".to_string()),
        ..Default::default()
    };
    let err = <UpdateDraft<MergedHost> as MergedHostDraftExt>::from_patch(&current, &patch)
        .expect_err("an untouched-but-invalid field must still be caught on merge");
    assert_eq!(
        err.status(),
        autumn_web::reexports::http::StatusCode::UNPROCESSABLE_ENTITY
    );
}
