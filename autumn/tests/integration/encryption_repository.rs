//! Compile-level proof that a full versioned `#[repository]` over a model with
//! encrypted columns expands and type-checks (insert/update/bulk/upsert + version
//! history composition). No sync Diesel imports here (would clash with the
//! generated async query code).

#![cfg(feature = "db")]

use autumn_web::model;
use autumn_web::repository;

diesel::table! {
    vault_entries (id) {
        id -> Int8,
        label -> Text,
        secret -> Text,
        lookup_key -> Text,
    }
}

#[model(table = "vault_entries")]
pub struct VaultEntry {
    #[id]
    pub id: i64,
    pub label: String,
    // Randomized + version history stores ciphertext (opt-in) instead of a marker.
    #[encrypted(versioned_ciphertext)]
    pub secret: String,
    // Deterministic + decrypted in admin read views (opt-in).
    #[encrypted(deterministic, admin_visible)]
    pub lookup_key: String,
}

#[repository(VaultEntry, table = "vault_entries", versioned = true)]
pub trait VaultEntryRepository {}

#[test]
fn encrypted_columns_registered_and_versioned() {
    use autumn_web::encryption;
    use autumn_web::version_history::VersionedRecord;

    // The model registers its encrypted columns for composition.
    assert_eq!(
        VaultEntry::__AUTUMN_ENCRYPTED_COLUMNS,
        &["secret", "lookup_key"]
    );

    // `lookup_key` (default version-history behaviour) is treated as sensitive,
    // so its plaintext never reaches the version table.
    let cols = VaultEntry::version_sensitive_columns();
    assert!(
        cols.contains(&"lookup_key"),
        "default encrypted column auto-sensitive: {cols:?}"
    );
    // `secret` opted into `versioned_ciphertext`, so it is EXCLUDED from the
    // sensitive list (its before/after are stored as ciphertext instead).
    assert!(
        !cols.contains(&"secret"),
        "versioned_ciphertext column is not a sensitive marker: {cols:?}"
    );

    // Opt-in flags round-trip through the registry.
    let secret = encryption::registered_encrypted_columns()
        .into_iter()
        .find(|d| d.table == "vault_entries" && d.column == "secret")
        .expect("secret registered");
    assert!(secret.versioned_ciphertext);
    // admin_visible: lookup_key is shown, secret is redacted.
    assert!(!encryption::admin_redacts_column_name("lookup_key"));
    assert!(encryption::admin_redacts_column_name("secret"));
}

#[test]
fn missing_keys_fail_fast_naming_the_credential_path() {
    // This binary registers encrypted columns (VaultEntry), so boot validation
    // against an empty credentials store must fail fast (mirroring #597),
    // naming both the offending column and the missing credential path.
    let empty = autumn_web::credentials::CredentialsStore::default();
    let err = autumn_web::encryption::init_attribute_encryption(&empty)
        .expect_err("must fail when keys are missing but encrypted columns exist");
    assert!(
        err.contains("active_record_encryption.primary_key"),
        "diagnostic must name the missing credential path: {err}"
    );
    assert!(
        err.contains("vault_entries"),
        "diagnostic must name the column: {err}"
    );
}
