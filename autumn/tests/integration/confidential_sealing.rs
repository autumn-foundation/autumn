//! Sealing, unsealing and blind-index properties of `#[confidential]` (#1771).
//!
//! These prove the cryptographic core, with no database and no server key ring:
//! a root key that exists only on the client seals a value, the envelope carries
//! no plaintext, and only the owning key and field context recover it.

use autumn_web::confidential::{BlindIndex, FieldContext, RootKey, Sealed};

const PLAINTEXT: &str = "AUTUMN-CONFIDENTIAL-MARKER-diagnosis-hypertension";

fn ctx() -> FieldContext {
    FieldContext::for_record("notes", "body", "user-42", "note-1")
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
fn an_envelope_moved_to_another_owner_column_table_or_row_fails_to_unseal() {
    let key = RootKey::generate();
    let sealed = key.seal(&ctx(), PLAINTEXT).expect("seal");

    for (what, moved) in [
        (
            "owner",
            FieldContext::for_record("notes", "body", "user-7", "note-1"),
        ),
        (
            "column",
            FieldContext::for_record("notes", "title", "user-42", "note-1"),
        ),
        (
            "table",
            FieldContext::for_record("memos", "body", "user-42", "note-1"),
        ),
        (
            "row",
            FieldContext::for_record("notes", "body", "user-42", "note-2"),
        ),
        (
            "record binding",
            FieldContext::new("notes", "body", "user-42"),
        ),
    ] {
        assert!(
            key.unseal(&moved, &sealed).is_err(),
            "the {what} must be bound"
        );
    }
}

#[test]
fn a_context_without_a_record_leaves_rows_interchangeable() {
    // Stated as a test because the guide states it: `new` binds the column, not
    // the row, so an operator can move a value among that owner's own rows.
    let key = RootKey::generate();
    let column_scope = FieldContext::new("notes", "body", "user-42");
    let sealed = key.seal(&column_scope, PLAINTEXT).expect("seal");
    assert_eq!(
        key.unseal(&column_scope, &sealed).expect("unseal"),
        PLAINTEXT
    );
}

#[test]
fn the_context_encoding_is_injective() {
    // Length-prefixed parts, so no two different triples collide. A separator
    // byte would let ("notes", "bodyx", "user") and ("notes", "body", "xuser")
    // derive one key.
    let key = RootKey::generate();
    let a = FieldContext::new("notes", "bodyx", "user");
    let b = FieldContext::new("notes", "body", "xuser");
    let sealed = key.seal(&a, PLAINTEXT).expect("seal");
    assert!(key.unseal(&b, &sealed).is_err());
    assert_ne!(
        key.blind_index(&a, PLAINTEXT),
        key.blind_index(&b, PLAINTEXT)
    );
}

#[test]
fn a_relabelled_envelope_header_is_refused() {
    use base64::Engine as _;

    let key = RootKey::generate();
    let sealed = key.seal(&ctx(), PLAINTEXT).expect("seal");
    let mut raw = sealed.to_bytes().expect("decode");
    raw[1] = 0x02; // claim a version this build does not know

    let relabelled = base64::engine::general_purpose::STANDARD.encode(&raw);
    // Refused at the boundary, and — because the header is authenticated data —
    // it could not have unsealed even if the parser had accepted it.
    assert!(Sealed::from_envelope(relabelled).is_err());
}

#[test]
fn surrounding_whitespace_does_not_make_a_second_distinct_envelope() {
    let key = RootKey::generate();
    let sealed = key.seal(&ctx(), PLAINTEXT).expect("seal");
    let padded = Sealed::from_envelope(format!("  {}  ", sealed.as_envelope())).expect("parse");
    assert_eq!(padded, sealed, "one envelope has one canonical form");
}

#[test]
fn a_default_blind_index_is_not_a_shared_constant() {
    // A fixed default would be a token every defaulted row holds, so one lookup
    // would match rows across owners.
    assert_ne!(BlindIndex::default(), BlindIndex::default());
    assert_eq!(
        BlindIndex::default().as_token().len(),
        BlindIndex::TOKEN_LEN
    );
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

    // 1. The token is hex only, so it cannot carry the marker.
    assert!(
        t.chars().all(|c| c.is_ascii_hexdigit()),
        "token is hex: {t}"
    );

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

    // The record is deliberately NOT in key derivation: the token has to stay
    // comparable across the rows of one owner, or the equality lookup breaks.
    let other_row = FieldContext::for_record("notes", "body", "user-42", "note-2");
    assert_eq!(here, key.blind_index(&other_row, PLAINTEXT));
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
    assert!(
        serde_json::from_str::<BlindIndex>("\"ABCDEF\"").is_err(),
        "uppercase hex is not the canonical token form"
    );
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
