//! `#[model]` field-attribute integration tests for `#[private]` (#1374) and
//! `#[normalize]` (#1379).
//!
//! These exercise the generated code end-to-end in memory (no DB connection
//! required): JSON serialization for `#[private]`, and the `Normalize` /
//! `NormalizedModel` impls for `#[normalize]`. DB round-trips are covered by the
//! macro-expansion assertions in `autumn-macros` (the derived-finder and save
//! codegen), since the async repository requires Postgres.
//!
//! NB: keep sync Diesel imports function-local so the sync `RunQueryDsl` never
//! enters the module scope where `#[model]` expands its async query code.

#![cfg(feature = "db")]

use autumn_web::normalize::{Normalize, NormalizedModel};

diesel::table! {
    accounts (id) {
        id -> BigInt,
        email -> Text,
        password_hash -> Text,
        display_name -> Text,
    }
}

#[autumn_web::model(table = "accounts")]
pub struct Account {
    #[id]
    pub id: i64,
    #[normalize(trim, downcase)]
    pub email: String,
    #[private]
    pub password_hash: String,
    #[normalize(squish)]
    pub display_name: String,
}

// ── #1374: `#[private]` hides a column from JSON ─────────────────────────

#[test]
fn private_column_is_absent_from_json() {
    let account = Account {
        id: 7,
        email: "user@example.com".into(),
        password_hash: "argon2-super-secret".into(),
        display_name: "Ada".into(),
    };
    let value = serde_json::to_value(&account).unwrap();
    let obj = value.as_object().unwrap();

    // The sensitive column must NOT appear in the JSON body...
    assert!(
        !obj.contains_key("password_hash"),
        "`#[private]` password_hash leaked into JSON: {value}"
    );
    // ...and the plaintext must not appear anywhere in the serialized body.
    assert!(
        !value.to_string().contains("argon2-super-secret"),
        "private value leaked into JSON: {value}"
    );
    // Public columns are still serialized.
    assert!(obj.contains_key("email"));
    assert!(obj.contains_key("display_name"));
    assert!(obj.contains_key("id"));
}

#[test]
fn private_column_still_deserializes_and_writes() {
    // Write/deserialize path is unaffected: the client can still *set* the
    // private column (a NewAccount binds it), even though it never reads back.
    let new = NewAccount {
        email: "x@y.com".into(),
        password_hash: "hash-to-store".into(),
        display_name: "Grace".into(),
    };
    // Deserialize round-trip into the query struct still accepts the field.
    let json = r#"{"id":1,"email":"x@y.com","password_hash":"h","display_name":"g"}"#;
    let decoded: Account = serde_json::from_str(json).unwrap();
    assert_eq!(decoded.password_hash, "h");
    assert_eq!(new.password_hash, "hash-to-store");
}

// ── #1374 × form_for: `#[private]` still appears in a form ────────────────

#[test]
fn private_column_still_appears_in_form_fields() {
    // Decision: `#[private]` affects only *serialization* (the read side). A
    // private column remains editable — you must be able to *set* a password —
    // so it still appears in `form_fields()` (the write side).
    use autumn_web::form::FormModel;
    let names: Vec<String> = Account::form_fields().into_iter().map(|f| f.name).collect();
    assert!(
        names.iter().any(|n| n == "password_hash"),
        "private column must remain in form_fields (write side): {names:?}"
    );
    assert!(names.iter().any(|n| n == "email"));
}

// ── #1379: `#[normalize]` canonicalizes on write and lookup ───────────────

#[test]
fn normalize_runs_on_the_write_struct() {
    let mut new = NewAccount {
        email: "  Foo@X.COM ".into(),
        password_hash: "h".into(),
        display_name: "  Grace   Hopper  ".into(),
    };
    new.normalize();
    assert_eq!(new.email, "foo@x.com", "trim+downcase on write");
    assert_eq!(new.display_name, "Grace Hopper", "squish on write");
}

#[test]
fn normalize_lookup_canonicalizes_finder_arguments() {
    // find_by_email("FOO@X.COM") must match the stored `foo@x.com` row.
    assert_eq!(
        Account::normalize_lookup("email", "  FOO@X.COM ").as_deref(),
        Some("foo@x.com")
    );
    // A non-normalized column is left untouched.
    assert_eq!(Account::normalize_lookup("password_hash", "  Kept "), None);
    // The generic helper falls back to the raw value for unknown columns.
    assert_eq!(
        autumn_web::normalize::normalize_lookup_value::<Account>("password_hash", "  Kept "),
        "  Kept "
    );
}

#[test]
fn normalize_is_idempotent() {
    let once = {
        let mut n = NewAccount {
            email: "  Foo@X.COM ".into(),
            password_hash: "h".into(),
            display_name: "  a   b ".into(),
        };
        n.normalize();
        n
    };
    let twice = {
        let mut n = once.clone();
        n.normalize();
        n
    };
    assert_eq!(once.email, twice.email);
    assert_eq!(once.display_name, twice.display_name);
    assert_eq!(twice.email, "foo@x.com");
}
