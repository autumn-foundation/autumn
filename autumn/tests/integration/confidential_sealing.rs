//! Sealing, unsealing and blind-index properties of `#[confidential]` (#1771).
//!
//! These prove the cryptographic core, with no database and no server key ring:
//! a root key that exists only on the client seals a value, the envelope carries
//! no plaintext, and only the owning key and field context recover it.

use autumn_web::confidential::{BlindIndex, FieldContext, RootKey, Sealed};

const PLAINTEXT: &str = "AUTUMN-CONFIDENTIAL-MARKER-diagnosis-hypertension";

fn ctx() -> FieldContext {
    FieldContext::new("notes", "body", "user-42")
}

#[test]
fn seal_round_trips_under_the_owning_key_and_context() {
    let key = RootKey::generate();
    let sealed = key.seal(&ctx(), PLAINTEXT).expect("seal");
    assert_eq!(key.unseal(&ctx(), &sealed).expect("unseal"), PLAINTEXT);
}

#[test]
fn the_envelope_carries_no_plaintext() {
    let key = RootKey::generate();
    let sealed = key.seal(&ctx(), PLAINTEXT).expect("seal");
    assert!(
        !sealed.as_envelope().contains(PLAINTEXT),
        "envelope must not contain the plaintext"
    );
    // The decoded bytes must not contain it either (base64 hides nothing).
    let raw = sealed.to_bytes().expect("decode");
    assert!(
        !raw.windows(PLAINTEXT.len())
            .any(|w| w == PLAINTEXT.as_bytes()),
        "raw envelope bytes must not contain the plaintext"
    );
}

#[test]
fn another_key_cannot_unseal() {
    let owner = RootKey::generate();
    let attacker = RootKey::generate();
    let sealed = owner.seal(&ctx(), PLAINTEXT).expect("seal");
    assert!(attacker.unseal(&ctx(), &sealed).is_err());
}

#[test]
fn an_envelope_moved_to_another_owner_or_column_fails_to_unseal() {
    let key = RootKey::generate();
    let sealed = key.seal(&ctx(), PLAINTEXT).expect("seal");

    let other_owner = FieldContext::new("notes", "body", "user-7");
    assert!(key.unseal(&other_owner, &sealed).is_err(), "owner is bound");

    let other_column = FieldContext::new("notes", "title", "user-42");
    assert!(
        key.unseal(&other_column, &sealed).is_err(),
        "column is bound"
    );

    let other_table = FieldContext::new("memos", "body", "user-42");
    assert!(key.unseal(&other_table, &sealed).is_err(), "table is bound");
}

#[test]
fn two_seals_of_one_plaintext_differ() {
    let key = RootKey::generate();
    let a = key.seal(&ctx(), PLAINTEXT).expect("seal");
    let b = key.seal(&ctx(), PLAINTEXT).expect("seal");
    assert_ne!(a.as_envelope(), b.as_envelope(), "sealing is randomized");
}

#[test]
fn debug_output_shows_no_ciphertext_or_key_material() {
    let key = RootKey::generate();
    let sealed = key.seal(&ctx(), PLAINTEXT).expect("seal");
    assert_eq!(format!("{sealed:?}"), "Sealed(<sealed>)");
    assert_eq!(format!("{key:?}"), "RootKey(<redacted>)");
}

// ── Blind index (AC4) ───────────────────────────────────────────────────────

#[test]
fn the_blind_index_is_deterministic_for_one_key_and_context() {
    let key = RootKey::generate();
    let a = key.blind_index(&ctx(), PLAINTEXT);
    let b = key.blind_index(&ctx(), PLAINTEXT);
    assert_eq!(a, b);
}

#[test]
fn the_blind_index_token_does_not_reveal_the_plaintext() {
    let key = RootKey::generate();
    let token = key.blind_index(&ctx(), PLAINTEXT);
    let t = token.as_token();

    // 1. The token is hex, so it can hold no fragment of the plaintext.
    assert!(
        t.chars().all(|c| c.is_ascii_hexdigit()),
        "token is hex: {t}"
    );
    assert!(!t.contains(PLAINTEXT));

    // 2. Its length is fixed, so it leaks no plaintext length.
    let short = key.blind_index(&ctx(), "a");
    let long = key.blind_index(&ctx(), &"x".repeat(4096));
    assert_eq!(t.len(), BlindIndex::TOKEN_LEN);
    assert_eq!(short.as_token().len(), BlindIndex::TOKEN_LEN);
    assert_eq!(long.as_token().len(), BlindIndex::TOKEN_LEN);

    // 3. Without the key the token cannot be recomputed, so an operator who
    //    guesses the plaintext still cannot confirm the guess.
    let operator = RootKey::generate();
    assert_ne!(operator.blind_index(&ctx(), PLAINTEXT), token);

    // 4. Different plaintexts give different tokens.
    assert_ne!(key.blind_index(&ctx(), "other value"), token);
}

#[test]
fn the_blind_index_is_bound_to_its_field_context() {
    let key = RootKey::generate();
    let here = key.blind_index(&ctx(), PLAINTEXT);
    let elsewhere = key.blind_index(&FieldContext::new("notes", "title", "user-42"), PLAINTEXT);
    assert_ne!(here, elsewhere, "the token is per column");
    let other_owner = key.blind_index(&FieldContext::new("notes", "body", "user-7"), PLAINTEXT);
    assert_ne!(here, other_owner, "the token is per owner");
}

// ── Wire shape ──────────────────────────────────────────────────────────────

#[test]
fn sealed_and_blind_index_travel_as_strings_on_the_wire() {
    let key = RootKey::generate();
    let sealed = key.seal(&ctx(), PLAINTEXT).expect("seal");
    let token = key.blind_index(&ctx(), PLAINTEXT);

    let json = serde_json::to_string(&sealed).expect("serialize");
    assert_eq!(json, format!("\"{}\"", sealed.as_envelope()));
    let back: Sealed = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(key.unseal(&ctx(), &back).expect("unseal"), PLAINTEXT);

    let json = serde_json::to_string(&token).expect("serialize");
    assert_eq!(json, format!("\"{}\"", token.as_token()));
    let back: BlindIndex = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, token);
}

#[test]
fn a_malformed_envelope_is_refused_at_the_boundary() {
    assert!(Sealed::from_envelope("not-base64!!".to_owned()).is_err());
    assert!(Sealed::from_envelope(String::new()).is_err());
    // Valid base64, wrong magic byte.
    assert!(Sealed::from_envelope("AAAAAAAAAAAAAAAAAAAAAAAA".to_owned()).is_err());
    assert!(serde_json::from_str::<Sealed>("\"oops\"").is_err());
    assert!(serde_json::from_str::<BlindIndex>("\"nothex\"").is_err());
}

#[test]
fn a_root_key_has_no_serialized_form() {
    // A compile-time property, asserted here as documentation: `RootKey` has no
    // `Serialize`, no `Display` and no accessor for its bytes, so there is no
    // expression that writes it to disk or a log line. The only escape is
    // `Drop`, which zeroizes.
    let key = RootKey::from_hex(&"ab".repeat(32)).expect("hex key");
    assert_eq!(format!("{key:?}"), "RootKey(<redacted>)");
    assert!(RootKey::from_hex("short").is_err());
}
