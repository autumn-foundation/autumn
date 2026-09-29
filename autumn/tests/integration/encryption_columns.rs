//! End-to-end attribute-encryption integration tests over a real `SQLite` database.
//!
//! Proves the headline acceptance criteria:
//! * a `#[model]` field marked `#[encrypted]` persists as opaque ciphertext on
//!   disk while reading back as plaintext (zero per-call-site changes);
//! * deterministic columns support equality lookups;
//! * plaintext never appears in `Debug` output of the wrapper types.
//!
//! NB: sync Diesel imports are kept *function-local* so the sync `RunQueryDsl`
//! never enters the module scope where `#[model]` expands its async query code.

#![cfg(feature = "db")]

use autumn_web::encryption::{self, KeyRing, Mode};
// Only `ExpressionMethods` (for the `serialize_as` AsChangeset derive) — NOT the
// full prelude, whose sync `RunQueryDsl` would clash with the model's generated
// async query code.
use diesel::ExpressionMethods as _;

diesel::table! {
    secrets (id) {
        id -> Integer,
        email -> Text,
        api_token -> Text,
        note -> Text,
    }
}

#[autumn_web::model(table = "secrets")]
pub struct Secret {
    pub id: i32,
    #[encrypted(deterministic)]
    pub email: String,
    #[encrypted]
    pub api_token: String,
    pub note: String,
}

const KEY: &str = "1111111111111111111111111111111111111111111111111111111111111111";
const DET: &str = "3333333333333333333333333333333333333333333333333333333333333333";

// Process-stable, runtime-generated salt (not a hard-coded crypto value).
fn itest_salt() -> &'static [u8] {
    static S: std::sync::OnceLock<[u8; 16]> = std::sync::OnceLock::new();
    S.get_or_init(|| {
        let mut b = [0u8; 16];
        getrandom::getrandom(&mut b).expect("OS RNG");
        b
    })
}

fn install_ring() {
    let ring = KeyRing::from_master_hex(KEY, &[], Some(DET), itest_salt()).unwrap();
    encryption::install_key_ring(ring);
}

fn conn() -> diesel::SqliteConnection {
    use diesel::connection::SimpleConnection;
    use diesel::prelude::*;
    let mut c = SqliteConnection::establish(":memory:").unwrap();
    c.batch_execute(
        "CREATE TABLE secrets (id INTEGER PRIMARY KEY, email TEXT NOT NULL, \
         api_token TEXT NOT NULL, note TEXT NOT NULL)",
    )
    .unwrap();
    c
}

#[test]
fn encrypted_columns_are_ciphertext_on_disk_but_plaintext_in_rust() {
    use diesel::prelude::*;
    install_ring();
    let mut c = conn();

    diesel::insert_into(secrets::table)
        .values(NewSecret {
            email: "alice@example.com".into(),
            api_token: "sk_live_super_secret".into(),
            note: "plain note".into(),
        })
        .execute(&mut c)
        .unwrap();

    // Raw on-disk values (select into String bypasses the wrapper): the
    // encrypted columns must NOT contain the plaintext.
    let (raw_email, raw_token, raw_note): (String, String, String) = secrets::table
        .select((secrets::email, secrets::api_token, secrets::note))
        .first(&mut c)
        .unwrap();
    assert_ne!(
        raw_email, "alice@example.com",
        "email must be ciphertext at rest"
    );
    assert!(!raw_email.contains("alice"), "no plaintext leakage on disk");
    assert_ne!(raw_token, "sk_live_super_secret");
    assert!(!raw_token.contains("secret"));
    assert_eq!(raw_note, "plain note", "non-encrypted column is untouched");

    // The on-disk envelope is decryptable with the documented key material.
    let ring = encryption::key_ring().unwrap();
    assert_eq!(
        String::from_utf8(ring.decrypt(&raw_token).unwrap()).unwrap(),
        "sk_live_super_secret"
    );

    // Reading through the model yields plaintext with no call-site changes.
    let got: Secret = secrets::table
        .select(Secret::as_select())
        .first(&mut c)
        .unwrap();
    assert_eq!(got.email, "alice@example.com");
    assert_eq!(got.api_token, "sk_live_super_secret");
    assert_eq!(got.note, "plain note");
}

#[test]
fn deterministic_column_supports_equality_lookup() {
    use diesel::prelude::*;
    install_ring();
    let mut c = conn();

    for (e, t) in [("bob@example.com", "t1"), ("carol@example.com", "t2")] {
        diesel::insert_into(secrets::table)
            .values(NewSecret {
                email: e.into(),
                api_token: t.into(),
                note: "n".into(),
            })
            .execute(&mut c)
            .unwrap();
    }

    // Equality lookup against a deterministic-encrypted column: encrypt the
    // search value to its stable ciphertext and filter on it.
    let needle = encryption::deterministic_ciphertext("carol@example.com").unwrap();
    let found: Secret = secrets::table
        .filter(secrets::email.eq(needle))
        .select(Secret::as_select())
        .first(&mut c)
        .unwrap();
    assert_eq!(found.email, "carol@example.com");
    assert_eq!(found.api_token, "t2");
}

#[test]
fn updates_re_encrypt_transparently() {
    use diesel::prelude::*;
    install_ring();
    let mut c = conn();
    diesel::insert_into(secrets::table)
        .values(NewSecret {
            email: "d@e.com".into(),
            api_token: "old".into(),
            note: "n".into(),
        })
        .execute(&mut c)
        .unwrap();

    diesel::update(secrets::table.filter(secrets::id.eq(1)))
        .set(secrets::api_token.eq(encryption::encrypt_text(Mode::Randomized, "rotated").unwrap()))
        .execute(&mut c)
        .unwrap();

    let got: Secret = secrets::table
        .select(Secret::as_select())
        .first(&mut c)
        .unwrap();
    assert_eq!(got.api_token, "rotated");
}

#[test]
fn wrapper_debug_redacts_plaintext_by_default() {
    let w = autumn_web::encryption::RandomizedText::from("topsecret".to_string());
    let dbg = format!("{w:?}");
    assert!(
        !dbg.contains("topsecret"),
        "wrapper Debug must redact: {dbg}"
    );
    assert!(dbg.contains("<encrypted>"));
}

#[test]
fn model_debug_redacts_encrypted_columns() {
    // The model holds plaintext in memory (for ergonomics) but its Debug impl
    // must never print encrypted-column values.
    let s = Secret {
        id: 7,
        email: "leak@example.com".into(),
        api_token: "sk_live_dont_log_me".into(),
        note: "fine to show".into(),
    };
    let dbg = format!("{s:?}");
    assert!(
        !dbg.contains("leak@example.com"),
        "email must be redacted: {dbg}"
    );
    assert!(
        !dbg.contains("sk_live_dont_log_me"),
        "token must be redacted: {dbg}"
    );
    assert!(
        dbg.contains("<encrypted>"),
        "redaction marker present: {dbg}"
    );
    assert!(
        dbg.contains("fine to show"),
        "non-encrypted field still shown: {dbg}"
    );
    // NewSecret (insert DTO) redacts too.
    let n = NewSecret {
        email: "leak@example.com".into(),
        api_token: "sk_live_dont_log_me".into(),
        note: "ok".into(),
    };
    let ndbg = format!("{n:?}");
    assert!(
        !ndbg.contains("sk_live_dont_log_me"),
        "NewX token must redact: {ndbg}"
    );
}

#[test]
fn commit_hook_payload_encrypts_encrypted_columns_recoverably() {
    install_ring();
    // Mirrors what the generated commit-hook codec does to a model snapshot
    // before persisting it to the durable autumn_repository_commit_hooks table.
    let mut v = serde_json::json!({
        "email": "a@b.com",
        "api_token": "sk_live_secret",
        "note": "plain",
    });
    encryption::encrypt_persisted_columns_in_value("secrets", &mut v);

    // Encrypted columns are now ciphertext (never plaintext); plain columns stay.
    assert_ne!(v["email"], "a@b.com");
    assert_ne!(v["api_token"], "sk_live_secret");
    assert_eq!(v["note"], "plain");
    // ...and they remain recoverable with the keys.
    assert_eq!(
        encryption::decrypt_text(v["api_token"].as_str().unwrap()).unwrap(),
        "sk_live_secret"
    );
    assert_eq!(
        encryption::decrypt_text(v["email"].as_str().unwrap()).unwrap(),
        "a@b.com"
    );
}

#[test]
fn commit_hook_codec_round_trips_plaintext_through_ciphertext() {
    install_ring();
    let original = Secret {
        id: 9,
        email: "round@trip.com".into(),
        api_token: "sk_live_round_trip".into(),
        note: "plain".into(),
    };

    // The durable payload (what lands in autumn_repository_commit_hooks) must be
    // ciphertext for encrypted columns — never plaintext.
    let payload = original.__autumn_commit_hook_to_value().unwrap();
    assert_ne!(payload["email"], "round@trip.com");
    assert_ne!(payload["api_token"], "sk_live_round_trip");
    assert!(!payload.to_string().contains("sk_live_round_trip"));

    // Reconstructing the model for a replayed after_*_commit hook recovers the
    // plaintext, matching the normal repository path.
    let restored = Secret::__autumn_commit_hook_from_value(payload).unwrap();
    assert_eq!(restored.email, "round@trip.com");
    assert_eq!(restored.api_token, "sk_live_round_trip");
    assert_eq!(restored.note, "plain");
}

#[test]
fn encrypted_columns_are_registered_for_composition() {
    // The macro registers encrypted columns for log-scrub / version-history /
    // admin composition.
    assert!(encryption::is_encrypted_column("secrets", "email"));
    assert!(encryption::is_encrypted_column("secrets", "api_token"));
    assert!(!encryption::is_encrypted_column("secrets", "note"));
    assert_eq!(Secret::__AUTUMN_ENCRYPTED_COLUMNS, &["email", "api_token"]);
}
