//! End-to-end compile-time data-classification tests (issue #1654).
//!
//! Proves the headline acceptance criteria over a real `#[model]`:
//! * a `#[classified]` field carries its classification on the *generated type*
//!   (`Classified<String, CustomerEmailClassified>`), not in a name denylist;
//! * a declared declassification boundary releases the value for a recorded
//!   purpose, and the released value serializes into the `Json` sink;
//! * the build-time data-flow manifest lists each classified field and every
//!   sink it is proven reachable to (empty reachable set == no leak);
//! * an unclassified model is completely unaffected.
//!
//! The *negative* half of the guarantee -- that a leak is a compile error -- is
//! pinned by the trybuild fixtures in `tests/compile-fail/classified_*.rs`,
//! because a test that must not compile cannot live in a compiled test binary.
//!
//! NB: sync Diesel imports are kept *function-local* so the sync `RunQueryDsl`
//! never enters the module scope where `#[model]` expands its async query code.

#![cfg(feature = "db")]

use autumn_web::classify::manifest::{
    ClassifiedFieldDescriptor, DataFlowManifest, DeclassificationDescriptor,
};
use autumn_web::classify::{Classification, ClassifiedField, DeclassificationRecord, Sink};

diesel::table! {
    customers (id) {
        id -> Integer,
        name -> Text,
        email -> Text,
    }
}

#[autumn_web::model(table = "customers")]
pub struct Customer {
    pub id: i32,
    pub name: String,
    /// Personal data: released only through a declared boundary.
    #[classified]
    pub email: String,
}

diesel::table! {
    widgets (id) {
        id -> Integer,
        label -> Text,
    }
}

/// A model with no classified column: nothing about it changes (AC5).
#[autumn_web::model(table = "widgets")]
pub struct Widget {
    pub id: i32,
    pub label: String,
}

autumn_web::declassify! {
    /// Support agents need the customer's email address to answer the ticket.
    pub SUPPORT_LOOKUP: CustomerEmailClassified => JsonResponse,
    purpose = "support_lookup",
    reason = "Support agents need the email address to answer the ticket.",
}

fn customer() -> Customer {
    Customer {
        id: 1,
        name: "Ada".to_string(),
        email: "ada@example.com".to_string().into(),
    }
}

// ── AC1: the classification is carried on the generated type ─────────────────

#[test]
fn classified_field_marker_carries_model_field_and_tier() {
    assert_eq!(CustomerEmailClassified::MODEL, "Customer");
    // Module-qualified: the manifest keys on this, so two crates that each
    // define a `Customer` with a classified `email` stay separate rows.
    assert!(
        CustomerEmailClassified::MODEL_PATH.ends_with("::data_classification::Customer"),
        "{}",
        CustomerEmailClassified::MODEL_PATH
    );
    assert_eq!(CustomerEmailClassified::FIELD, "email");
    assert_eq!(
        CustomerEmailClassified::CLASSIFICATION,
        Classification::PersonalData
    );
}

#[test]
fn classified_field_is_wrapped_in_the_taint_type() {
    let c = customer();
    // The generated field type is the taint wrapper, not a bare `String`.
    let _: &autumn_web::classify::Classified<String, CustomerEmailClassified> = &c.email;
    assert_eq!(c.name, "Ada");
}

#[test]
fn debug_output_never_renders_classified_plaintext() {
    let rendered = format!("{:?}", customer());
    assert!(
        !rendered.contains("ada@example.com"),
        "classified plaintext leaked into Debug: {rendered}"
    );
    assert!(rendered.contains("<classified>"), "{rendered}");
}

// ── AC3: a declared boundary releases the value for a recorded purpose ───────

#[test]
fn declassifying_at_a_boundary_yields_the_value_and_records_the_release() {
    let records = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = std::sync::Arc::clone(&records);
    let _guard = autumn_web::classify::capture_releases(move |record: &DeclassificationRecord| {
        sink.lock().expect("release recorder").push(record.clone());
    });

    let released: String = customer().email.declassify(&SUPPORT_LOOKUP);
    assert_eq!(released, "ada@example.com");

    // Filter rather than count: the observer registry is process-wide, and the
    // sibling test that serializes a released view runs in parallel with this
    // one, releasing the same column.
    let captured = records.lock().expect("release recorder").clone();
    let mine: Vec<_> = captured
        .iter()
        .filter(|r| r.model == "Customer" && r.field == "email")
        .collect();
    assert!(!mine.is_empty(), "{captured:?}");
    let record = mine[0];
    assert_eq!(record.classification, Classification::PersonalData);
    assert_eq!(record.purpose, "support_lookup");
    assert_eq!(record.sink, Sink::JsonResponse);
    assert_eq!(
        record.reason,
        "Support agents need the email address to answer the ticket."
    );
}

#[test]
fn a_released_value_serializes_into_the_json_sink() {
    #[derive(serde::Serialize)]
    struct SupportView {
        name: String,
        email: String,
    }

    let c = customer();
    let view = SupportView {
        name: c.name,
        email: c.email.declassify(&SUPPORT_LOOKUP),
    };
    let body = serde_json::to_string(&autumn_web::extract::Json(view).0).expect("serialize");
    assert!(body.contains("ada@example.com"), "{body}");
}

// ── AC4: the data-flow manifest ──────────────────────────────────────────────

#[test]
fn manifest_lists_the_classified_field_and_its_reachable_sink() {
    let manifest = autumn_web::classify::manifest::audit();
    let flow = manifest
        .fields
        .iter()
        .find(|f| f.model == "Customer" && f.field == "email")
        .unwrap_or_else(|| panic!("Customer.email missing from manifest: {manifest:?}"));
    assert_eq!(flow.classification, Classification::PersonalData);
    assert_eq!(flow.reachable_sinks.len(), 1, "{flow:?}");
    assert_eq!(flow.reachable_sinks[0].sink, Sink::JsonResponse);
    assert_eq!(flow.reachable_sinks[0].purpose, "support_lookup");
}

#[test]
fn manifest_reports_an_empty_reachable_set_for_a_never_released_field() {
    let manifest = DataFlowManifest::build(
        &[ClassifiedFieldDescriptor {
            model: "Order",
            model_path: "shop::models::Order",
            field: "card_number",
            classification: Classification::PersonalData,
        }],
        &[],
    );
    assert_eq!(manifest.fields.len(), 1);
    assert!(
        manifest.fields[0].reachable_sinks.is_empty(),
        "{manifest:?}"
    );
    assert!(
        manifest.summary().contains("no sink"),
        "{}",
        manifest.summary()
    );
}

#[test]
fn manifest_join_is_keyed_on_model_and_field() {
    let manifest = DataFlowManifest::build(
        &[
            ClassifiedFieldDescriptor {
                model: "User",
                model_path: "shop::models::User",
                field: "email",
                classification: Classification::PersonalData,
            },
            ClassifiedFieldDescriptor {
                model: "Order",
                model_path: "shop::models::Order",
                field: "email",
                classification: Classification::PersonalData,
            },
        ],
        &[DeclassificationDescriptor {
            model: "User",
            model_path: "shop::models::User",
            field: "email",
            classification: Classification::PersonalData,
            purpose: "support_lookup",
            sink: Sink::JsonResponse,
            reason: "Support agents need it.",
        }],
    );
    let user = &manifest.fields[1];
    assert_eq!(
        (user.model.as_str(), user.field.as_str()),
        ("User", "email")
    );
    assert_eq!(user.reachable_sinks.len(), 1);
    let order = &manifest.fields[0];
    assert_eq!(
        (order.model.as_str(), order.field.as_str()),
        ("Order", "email")
    );
    assert!(order.reachable_sinks.is_empty());
}

#[test]
fn manifest_round_trips_through_the_dump_marker() {
    let manifest = autumn_web::classify::manifest::audit();
    let dump = manifest.to_dump_line();
    let parsed = autumn_web::classify::manifest::parse_manifest_dump(&format!(
        "some unrelated startup line\n{dump}\n"
    ))
    .expect("manifest dump parses");
    assert_eq!(parsed.schema_version, manifest.schema_version);
    assert_eq!(parsed.fields.len(), manifest.fields.len());
}

// ── AC5: unclassified models are untouched ───────────────────────────────────

#[test]
fn an_unclassified_model_still_serializes_normally() {
    let widget = Widget {
        id: 7,
        label: "gizmo".to_string(),
    };
    let body = serde_json::to_string(&widget).expect("serialize");
    assert_eq!(body, r#"{"id":7,"label":"gizmo"}"#);
}

// ── AC5: the existing name-based redaction is untouched ─────────────────────

#[test]
fn the_name_based_log_scrubber_still_filters_unclassified_payloads() {
    use autumn_web::log::filter::ParameterFilter;

    let filter = ParameterFilter::default();
    let scrubbed = filter.scrub_json(&serde_json::json!({
        "password": "hunter2",
        "ssn": "123-45-6789",
        "note": "keep me",
    }));
    assert_eq!(scrubbed["password"], "[FILTERED]");
    assert_eq!(scrubbed["ssn"], "[FILTERED]");
    assert_eq!(scrubbed["note"], "keep me");
}

#[test]
fn a_classified_model_still_accepts_personal_data_on_the_write_path() {
    // Taking personal data *in* is not a release, so the write structs still
    // deserialize a create body unchanged. What they must never do is hand the
    // value back out -- hence the wrapper on the field (see
    // `the_write_structs_cannot_hand_the_plaintext_back_out` below) and the
    // absence of the column from their `Serialize`.
    let new = NewCustomer {
        name: "Ada".to_string(),
        email: "ada@example.com".to_string().into(),
    };
    let body = serde_json::to_string(&new).expect("serialize");
    assert!(!body.contains("ada@example.com"), "{body}");
    assert!(body.contains("Ada"), "{body}");

    let round_tripped: NewCustomer =
        serde_json::from_str(r#"{"name":"Ada","email":"ada@example.com"}"#).expect("deserialize");
    assert!(
        !format!("{round_tripped:?}").contains("ada@example.com"),
        "write-struct Debug must redact the classified column"
    );
}

#[test]
fn the_write_structs_cannot_hand_the_plaintext_back_out() {
    // Review round 2 of #1654 (P1). `NewCustomer`/`UpdateCustomer` are how an
    // application *receives* a classified value and their fields are `pub`, so
    // a bare `String` here was a complete bypass of the guarantee: a handler
    // could write `Json(View { email: input.email })` and release personal data
    // with no boundary and no audit record. `skip_serializing` did not help --
    // it only governs serializing the write struct itself, not moving a value
    // out of it.
    //
    // The wrapper is what closes it. The compile-fail half of this pair is
    // `tests/compile-fail/classified_write_struct_leak.rs`; here we check the
    // runtime-visible half, that the value is still *usable* on the write path
    // it was accepted on.
    let new = NewCustomer {
        name: "Ada".to_string(),
        email: "ada@example.com".to_string().into(),
    };
    // Equality still works through the wrapper, so the value is not write-only.
    assert_eq!(new.email, "ada@example.com".to_string().into());
    assert_ne!(new.email, "grace@example.com".to_string().into());

    // The patch field carries it too.
    let patch: UpdateCustomer =
        serde_json::from_str(r#"{"email":"ada@example.com"}"#).expect("deserialize");
    let autumn_web::hooks::Patch::Set(email) = patch.email else {
        panic!("the patch must decode a Set value");
    };
    assert_eq!(email, "ada@example.com".to_string().into());
    assert!(
        !format!("{email:?}").contains("ada@example.com"),
        "the patch value must redact in Debug too"
    );
}
