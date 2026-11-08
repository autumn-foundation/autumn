//! Event wire contracts at run time: the name, the version and the shape that
//! `#[event]` records, and consumers that compile because they read only
//! fields the event sends.

use autumn_web::events::Event;
use autumn_web::outbox::OutboxMessage;
use autumn_web::prelude::*;
use autumn_web::wire::WireShape;

#[event(name = "contact.updated")]
struct ContactUpdated {
    id: i64,
    email: String,
    #[serde(skip_serializing)]
    internal: String,
}

#[event(name = "contact.updated", version = 2)]
struct ContactUpdatedV2 {
    id: i64,
    #[serde(rename = "emailAddress")]
    email: String,
}

#[event]
struct Unnamed {
    id: i64,
}

#[event(version = 2)]
struct UnnamedV2 {
    id: i64,
}

#[event(name = "page.loaded", opaque)]
struct PageLoaded {
    #[serde(flatten)]
    counts: std::collections::BTreeMap<String, i64>,
}

#[listener(ContactUpdated)]
async fn on_contact_updated(_state: AppState, event: ContactUpdated) -> AutumnResult<()> {
    let ContactUpdated { email, .. } = event.clone();
    let _ = (event.id, email);
    Ok(())
}

#[listener(ContactUpdatedV2, durable)]
async fn on_contact_updated_v2(_state: AppState, event: ContactUpdatedV2) -> AutumnResult<()> {
    let _ = format!("{} {}", event.id, event.email);
    Ok(())
}

#[event_consumer(ContactUpdated)]
async fn post_contact(_state: AppState, message: OutboxMessage) -> AutumnResult<()> {
    let contact = message.payload_as::<ContactUpdated>()?;
    let _ = contact.email;
    Ok(())
}

#[test]
fn version_one_keeps_the_bare_name() {
    assert_eq!(ContactUpdated::NAME, "contact.updated");
    assert_eq!(ContactUpdated::VERSION, 1);
}

#[test]
fn a_later_version_lives_beside_version_one() {
    assert_eq!(ContactUpdatedV2::NAME, "contact.updated.v2");
    assert_eq!(ContactUpdatedV2::VERSION, 2);
}

#[test]
fn an_event_without_a_name_is_module_qualified() {
    assert!(Unnamed::NAME.ends_with("::Unnamed"), "{}", Unnamed::NAME);
    assert!(
        UnnamedV2::NAME.ends_with("::UnnamedV2.v2"),
        "{}",
        UnnamedV2::NAME
    );
    assert_eq!(UnnamedV2::VERSION, 2);
}

#[test]
fn the_event_shape_is_its_wire_shape() {
    let shape = ContactUpdated::SHAPE.expect("an event has a shape");
    let names = |fields: &[autumn_web::wire::WireField]| -> Vec<&str> {
        fields.iter().map(|f| f.rust_name).collect()
    };
    assert_eq!(names(shape.serialized), ["id", "email"]);
    assert_eq!(names(shape.deserialized), ["id", "email", "internal"]);
    assert_eq!(<ContactUpdated as WireShape>::TYPE_NAME, "ContactUpdated");
    assert!(autumn_web::wire::event_delivers(
        ContactUpdated::SHAPE,
        "email"
    ));
    assert!(!autumn_web::wire::event_delivers(
        ContactUpdated::SHAPE,
        "internal"
    ));
    // The skipped field is not on the wire, so a round trip drops it.
    let sent = ContactUpdated {
        id: 1,
        email: "a@example.com".to_owned(),
        internal: "secret".to_owned(),
    };
    let json = autumn_web::reexports::serde_json::to_value(&sent).expect("serializes");
    assert!(json.get("internal").is_none(), "{json}");
    assert_eq!(sent.internal, "secret");
    let v2 = ContactUpdatedV2::SHAPE.expect("an event has a shape");
    assert_eq!(v2.serialized[1].wire_name, "emailAddress");
}

#[test]
fn an_opaque_event_has_no_shape() {
    assert!(PageLoaded::SHAPE.is_none());
    assert_eq!(PageLoaded::NAME, "page.loaded");
    assert_eq!(PageLoaded::VERSION, 1);
}

#[test]
fn checked_consumers_still_register_and_run() {
    let infos = listeners![on_contact_updated, on_contact_updated_v2];
    assert_eq!(infos[0].event_name, "contact.updated");
    assert_eq!(infos[1].event_name, "contact.updated.v2");
    let _ = post_contact;
}
