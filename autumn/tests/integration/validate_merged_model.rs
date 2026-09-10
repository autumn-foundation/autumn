//! Merged-model validation covers rules that cannot validate `Patch<T>`.

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
            card -> Text,
            label -> Text,
            nested_value -> Text,
        }
    }
}

use schema::merged_hosts;

#[derive(
    Clone,
    Debug,
    Default,
    PartialEq,
    serde::Deserialize,
    serde::Serialize,
    autumn_web::reexports::diesel::AsExpression,
    autumn_web::reexports::diesel::FromSqlRow,
    validator::Validate,
)]
#[diesel(sql_type = autumn_web::reexports::diesel::sql_types::Text)]
pub struct NestedValue {
    #[validate(length(min = 1))]
    value: String,
}

impl
    autumn_web::reexports::diesel::serialize::ToSql<
        autumn_web::reexports::diesel::sql_types::Text,
        autumn_web::reexports::diesel::pg::Pg,
    > for NestedValue
{
    fn to_sql<'b>(
        &'b self,
        out: &mut autumn_web::reexports::diesel::serialize::Output<
            'b,
            '_,
            autumn_web::reexports::diesel::pg::Pg,
        >,
    ) -> autumn_web::reexports::diesel::serialize::Result {
        <String as autumn_web::reexports::diesel::serialize::ToSql<
            autumn_web::reexports::diesel::sql_types::Text,
            autumn_web::reexports::diesel::pg::Pg,
        >>::to_sql(&self.value, out)
    }
}

impl
    autumn_web::reexports::diesel::deserialize::FromSql<
        autumn_web::reexports::diesel::sql_types::Text,
        autumn_web::reexports::diesel::pg::Pg,
    > for NestedValue
{
    fn from_sql(
        bytes: autumn_web::reexports::diesel::pg::PgValue<'_>,
    ) -> autumn_web::reexports::diesel::deserialize::Result<Self> {
        <String as autumn_web::reexports::diesel::deserialize::FromSql<
            autumn_web::reexports::diesel::sql_types::Text,
            autumn_web::reexports::diesel::pg::Pg,
        >>::from_sql(bytes)
        .map(|value| Self { value })
    }
}

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
    #[validate(ip)]
    pub ip: Option<String>,
    #[validate(does_not_contain(pattern = "bad"))]
    pub blurb: String,
    #[validate(custom(function = "no_uppercase"))]
    pub slug: String,
    #[validate(credit_card)]
    pub card: String,
    #[validate(non_control_character)]
    pub label: String,
    #[validate(nested)]
    pub nested_value: NestedValue,
}

fn valid_current() -> MergedHost {
    MergedHost {
        id: 1,
        ip: Some("10.0.0.1".to_string()),
        blurb: "all good".to_string(),
        slug: "host-one".to_string(),
        card: "4111111111111111".to_string(),
        label: "server".to_string(),
        nested_value: NestedValue {
            value: "value".to_string(),
        },
    }
}

#[test]
fn update_model_patch_struct_still_drops_walled_validators() {
    let patch = UpdateMergedHost {
        ip: Patch::Set(Some("not-an-ip".to_string())),
        blurb: Patch::Set("this is bad".to_string()),
        slug: Patch::Set("SHOUTING".to_string()),
        card: Patch::Set("invalid".to_string()),
        label: Patch::Set("bad\u{0007}".to_string()),
        nested_value: Patch::Set(NestedValue {
            value: String::new(),
        }),
    };
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
        card: Patch::Set("5555555555554444".to_string()),
        label: Patch::Set("gateway".to_string()),
        nested_value: Patch::Set(NestedValue {
            value: "updated".to_string(),
        }),
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
fn from_patch_rejects_invalid_credit_card() {
    let patch = UpdateMergedHost {
        card: Patch::Set("invalid".to_string()),
        ..Default::default()
    };
    let err = <UpdateDraft<MergedHost> as MergedHostDraftExt>::from_patch(&valid_current(), &patch)
        .expect_err("an invalid card must be rejected");
    assert_eq!(
        err.status(),
        autumn_web::reexports::http::StatusCode::UNPROCESSABLE_ENTITY
    );
    assert!(err.to_string().contains("card"));
}

#[test]
fn from_patch_rejects_control_character() {
    let patch = UpdateMergedHost {
        label: Patch::Set("bad\u{0007}".to_string()),
        ..Default::default()
    };
    let err = <UpdateDraft<MergedHost> as MergedHostDraftExt>::from_patch(&valid_current(), &patch)
        .expect_err("a control character must be rejected");
    assert_eq!(
        err.status(),
        autumn_web::reexports::http::StatusCode::UNPROCESSABLE_ENTITY
    );
    assert!(err.to_string().contains("label"));
}

#[test]
fn from_patch_rejects_invalid_nested_value() {
    let patch = UpdateMergedHost {
        nested_value: Patch::Set(NestedValue {
            value: String::new(),
        }),
        ..Default::default()
    };
    let err = <UpdateDraft<MergedHost> as MergedHostDraftExt>::from_patch(&valid_current(), &patch)
        .expect_err("an invalid nested value must be rejected");
    assert_eq!(
        err.status(),
        autumn_web::reexports::http::StatusCode::UNPROCESSABLE_ENTITY
    );
}

// Validate `must_match` after merging because it compares two fields.

mod cross_field_schema {
    autumn_web::reexports::diesel::table! {
        cross_field_hosts (id) {
            id -> Int8,
            password -> Text,
            password_confirm -> Text,
        }
    }
}
use cross_field_schema::cross_field_hosts;

#[autumn_web::model(table = "cross_field_hosts")]
pub struct CrossFieldHost {
    #[id]
    pub id: i64,
    #[validate(must_match(other = "password_confirm"))]
    pub password: String,
    pub password_confirm: String,
}

fn valid_cross_field_current() -> CrossFieldHost {
    CrossFieldHost {
        id: 1,
        password: "secret1".to_string(),
        password_confirm: "secret1".to_string(),
    }
}

#[test]
fn update_model_patch_struct_still_drops_must_match() {
    let patch = UpdateCrossFieldHost {
        password: Patch::Set("new-secret".to_string()),
        password_confirm: Patch::Set("does-not-match".to_string()),
    };
    assert!(
        patch.validate().is_ok(),
        "must_match must remain create-only on the patch struct"
    );
}

#[test]
fn from_patch_rejects_must_match_invalid_once_merged() {
    let current = valid_cross_field_current();
    let patch = UpdateCrossFieldHost {
        password: Patch::Set("new-secret".to_string()),
        ..Default::default()
    };
    let err = <UpdateDraft<CrossFieldHost> as CrossFieldHostDraftExt>::from_patch(&current, &patch)
        .expect_err("a merged password/password_confirm mismatch must be rejected");
    assert_eq!(
        err.status(),
        autumn_web::reexports::http::StatusCode::UNPROCESSABLE_ENTITY,
        "merged-model must_match failures must surface as 422"
    );
}

#[test]
fn from_patch_accepts_must_match_valid_once_merged() {
    let current = valid_cross_field_current();
    let patch = UpdateCrossFieldHost {
        password: Patch::Set("secret2".to_string()),
        password_confirm: Patch::Set("secret2".to_string()),
    };
    let draft =
        <UpdateDraft<CrossFieldHost> as CrossFieldHostDraftExt>::from_patch(&current, &patch)
            .expect("a matching password/password_confirm merge must pass validation");
    assert_eq!(draft.after().password, "secret2");
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
