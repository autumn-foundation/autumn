//! Avro schema resolution against `autumn contracts check`, for each event
//! pair of the cross-release suite.
//!
//! The deploy windows map to Avro resolution so:
//!
//! * New producers with deployed consumers (`callee_first`): the old schema
//!   reads data that the new schema wrote.
//! * New consumers with deployed producers (`caller_first`): the new schema
//!   reads data that the old schema wrote.
//!
//! Avro compares one subject at a time, so only topics that both releases
//! have are compared. Each case where the two differ names the reason.

use apache_avro::Schema;
use apache_avro::schema_compatibility::SchemaCompatibility;
use autumn_macros_support::wire::{WireFieldDescriptor, WireType, WireTypeDescriptor};

use super::{AvroOptions, export};
use crate::contracts::bundle::{Bundle, BundleEvent, BundleListener, BundleType, WIRE_TYPE_FORMAT};
use crate::contracts::compat::compare;

/// The Rust type and the wire type of a fixture field.
fn wire_of(ty: &str) -> WireType {
    match ty {
        "String" => WireType::string(),
        "i32" => WireType::integer(32, true),
        "i64" => WireType::integer(64, true),
        "u32" => WireType::integer(32, false),
        "Address" => WireType::object("core::Address"),
        "Vec<String>" => WireType::array(WireType::string()),
        _ => ty
            .strip_prefix("Option<")
            .and_then(|t| t.strip_suffix('>'))
            .map_or_else(
                || panic!("no wire type for `{ty}`"),
                |inner| WireType::nullable(wire_of(inner)),
            ),
    }
}

fn field(name: &str, ty: &str, required: bool) -> WireFieldDescriptor {
    WireFieldDescriptor {
        rust_name: name.to_owned(),
        wire_name: name.to_owned(),
        ty: ty.to_owned(),
        wire_type: Some(wire_of(ty)),
        required,
        ..WireFieldDescriptor::default()
    }
}

/// `(name, Rust type)`. The producer sends each field. An `Option` is not
/// required on decode.
fn contact_shape(fields: &[(&str, &str)]) -> WireTypeDescriptor {
    WireTypeDescriptor {
        name: "ContactUpdated".to_owned(),
        serialized: fields.iter().map(|(n, t)| field(n, t, true)).collect(),
        deserialized: fields
            .iter()
            .map(|(n, t)| field(n, t, !t.starts_with("Option<")))
            .collect(),
        ..WireTypeDescriptor::default()
    }
}

fn event(version: u32, shape: WireTypeDescriptor) -> BundleEvent {
    let name = "contact.updated".to_owned();
    BundleEvent {
        topic: autumn_macros_support::wire::versioned_topic(&name, version),
        name,
        version,
        krate: "core".to_owned(),
        type_name: format!("ContactUpdatedV{version}"),
        opaque: false,
        unit: false,
        shape,
    }
}

fn listener(function: &str, topic: &str, reads: &[&str]) -> BundleListener {
    BundleListener {
        krate: "marketing".to_owned(),
        function: function.to_owned(),
        topic: topic.to_owned(),
        reads: reads.iter().map(|r| (*r).to_owned()).collect(),
    }
}

fn base() -> Bundle {
    Bundle {
        format: WIRE_TYPE_FORMAT,
        ..Bundle::default()
    }
}

/// `contact.updated { id, email }` and a consumer of `email`.
fn with_events() -> Bundle {
    let mut bundle = base();
    bundle.events = vec![event(
        1,
        contact_shape(&[("id", "i64"), ("email", "String")]),
    )];
    bundle.listeners = vec![listener("on_contact", "contact.updated", &["email"])];
    bundle
}

fn v2_email_address() -> BundleEvent {
    event(
        2,
        contact_shape(&[("id", "i64"), ("email_address", "String")]),
    )
}

/// One release pair, what each tool says per window, and why they differ.
struct Case {
    name: &'static str,
    deployed: Bundle,
    current: Bundle,
    /// `(callee_first safe, caller_first safe)` from the check.
    check: (bool, bool),
    /// `(old reads new, new reads old)` from Avro resolution.
    avro: (bool, bool),
    /// Empty when the two agree in both windows.
    why: &'static str,
}

const TOPICS: &str = "topics: a new version is a new subject. A registry does not know \
                      which topics a producer still sends";

/// One row per event test in `compat.rs`, plus cases that show a reason.
#[allow(clippy::too_many_lines)]
fn cases() -> Vec<Case> {
    let mut out = Vec::new();
    let mut add = |name, deployed, current, check, avro, why| {
        out.push(Case {
            name,
            deployed,
            current,
            check,
            avro,
            why,
        });
    };

    let mut current = with_events();
    current.events[0].shape = contact_shape(&[("id", "i64")]);
    current.listeners = vec![listener("on_contact", "contact.updated", &["id"])];
    add(
        "a_removed_event_field_a_deployed_consumer_reads_fails",
        with_events(),
        current,
        (false, true),
        (false, true),
        "",
    );

    let mut current = with_events();
    current.events[0].shape = contact_shape(&[
        ("id", "i64"),
        ("email", "String"),
        ("phone", "Option<String>"),
    ]);
    add(
        "a_new_event_field_passes",
        with_events(),
        current,
        (true, true),
        (true, true),
        "",
    );

    let mut current = with_events();
    current.events[0].shape =
        contact_shape(&[("id", "i64"), ("email", "String"), ("phone", "String")]);
    current.listeners = vec![listener("on_contact", "contact.updated", &["phone"])];
    add(
        "a_consumer_that_starts_reading_a_new_field_needs_the_producer_first",
        with_events(),
        current,
        (true, false),
        (true, false),
        "",
    );

    let mut current = with_events();
    current.events[0].shape = contact_shape(&[("email", "String")]);
    add(
        "an_unread_required_event_field_removed_breaks_the_old_decode",
        with_events(),
        current,
        (false, true),
        (false, true),
        "",
    );

    let mut current = with_events();
    current.events[0].shape = contact_shape(&[("id", "i64"), ("email", "u32")]);
    add(
        "an_event_field_type_change_is_a_break",
        with_events(),
        current,
        (false, false),
        (false, false),
        "",
    );

    let mut deployed = with_events();
    deployed.events[0].shape.closed = true;
    let mut current = deployed.clone();
    current.events[0].shape = contact_shape(&[
        ("id", "i64"),
        ("email", "String"),
        ("phone", "Option<String>"),
    ]);
    current.events[0].shape.closed = true;
    add(
        "a_closed_event_type_rejects_a_new_field",
        deployed,
        current,
        (false, true),
        (true, true),
        "closed types: an Avro reader skips a field it does not know. \
         `deny_unknown_fields` refuses it",
    );

    let mut current = with_events();
    current.events = vec![event(
        2,
        contact_shape(&[("id", "i64"), ("email", "String")]),
    )];
    current.listeners = vec![listener("on_contact", "contact.updated.v2", &["email"])];
    add(
        "a_topic_no_new_producer_sends_breaks_a_deployed_consumer",
        with_events(),
        current,
        (false, false),
        (true, true),
        TOPICS,
    );

    add(
        "a_new_consumer_of_a_topic_the_deployed_release_lacks_is_not_a_break",
        base(),
        with_events(),
        (true, true),
        (true, true),
        "",
    );

    let mut current = with_events();
    current.events.push(v2_email_address());
    current.listeners = vec![listener(
        "on_contact",
        "contact.updated.v2",
        &["email_address"],
    )];
    add(
        "a_v2_beside_v1_needs_the_producer_first_when_the_consumer_moves_to_v2",
        with_events(),
        current,
        (true, false),
        (true, true),
        TOPICS,
    );

    let mut current = with_events();
    current.events.push(v2_email_address());
    current.listeners.push(listener(
        "on_contact_v2",
        "contact.updated.v2",
        &["email_address"],
    ));
    add(
        "a_v2_beside_v1_is_safe_while_the_consumer_reads_both",
        with_events(),
        current,
        (true, true),
        (true, true),
        "",
    );

    let mut deployed = with_events();
    deployed.listeners.push(BundleListener {
        krate: "billing".to_owned(),
        ..listener("on_contact_billing", "contact.updated", &["id"])
    });
    let mut current = deployed.clone();
    current.events.push(v2_email_address());
    current.listeners.retain(|l| l.krate != "marketing");
    current.listeners.push(listener(
        "on_contact",
        "contact.updated.v2",
        &["email_address"],
    ));
    add(
        "a_consumer_that_moves_to_v2_is_not_covered_by_another_crate_on_v1",
        deployed,
        current,
        (true, false),
        (true, true),
        TOPICS,
    );

    let mut deployed = with_events();
    deployed.events[0].krate = "mesh-catalog".to_owned();
    let mut current = deployed.clone();
    current.events[0].shape = contact_shape(&[("id", "i64")]);
    current.listeners.clear();
    add(
        "a_deploy_order_names_an_event_crate_by_its_service",
        deployed,
        current,
        (false, true),
        (false, true),
        "",
    );

    let mut both = with_events();
    both.events.push(v2_email_address());
    both.listeners = vec![listener(
        "on_contact",
        "contact.updated.v2",
        &["email_address"],
    )];
    add(
        "the_check_reports_when_v1_can_be_dropped",
        both.clone(),
        both,
        (true, true),
        (true, true),
        "",
    );

    let mut unseen = with_events();
    unseen.events.push(event(
        2,
        contact_shape(&[("id", "i64"), ("email", "String")]),
    ));
    unseen.listeners = vec![listener("on_contact", "contact.updated.v2", &["email"])];
    unseen.unresolved = vec!["`marketing::on_old` consumes `Updated`".to_owned()];
    add(
        "an_unseen_consumer_is_named_and_no_version_is_called_droppable",
        unseen.clone(),
        unseen,
        (true, true),
        (true, true),
        "",
    );

    let mut kept = with_events();
    kept.events.push(event(
        2,
        contact_shape(&[("id", "i64"), ("email", "String")]),
    ));
    add(
        "v1_is_kept_while_any_consumer_reads_it",
        kept.clone(),
        kept,
        (true, true),
        (true, true),
        "",
    );

    let mut deployed = with_events();
    deployed.events[0].unit = true;
    deployed.events[0].shape = contact_shape(&[]);
    deployed.events[0].shape.unit = true;
    deployed.listeners = vec![listener("on_contact", "contact.updated", &[])];
    let mut current = with_events();
    current.events[0].shape = contact_shape(&[("trace", "Option<String>")]);
    current.listeners = vec![listener("on_contact", "contact.updated", &[])];
    add(
        "a_unit_event_that_gains_fields_is_a_break_in_both_orders",
        deployed,
        current,
        (false, false),
        (true, true),
        "unit events: Autumn sends JSON null for a unit event, so new fields change the \
         JSON form. In Avro, a unit event is a record with no fields",
    );

    let mut current = with_events();
    current.events[0].opaque = true;
    current.events[0].shape = WireTypeDescriptor::default();
    add(
        "an_opaque_event_is_not_checked",
        with_events(),
        current,
        (true, true),
        (false, false),
        "opaque events: the check does not compare an opaque event. Avro compares the \
         old record with `bytes`",
    );

    let mut current = with_events();
    current.events[0].shape = contact_shape(&[("id", "i64")]);
    current.listeners.clear();
    add(
        "an_event_break_names_the_topic_the_consumer_and_the_field",
        with_events(),
        current,
        (false, true),
        (false, true),
        "",
    );

    let mut current = with_events();
    current.events[0].shape = contact_shape(&[("id", "i64"), ("email", "i64")]);
    current.events[0].shape.serialized[1].ty = "String".to_owned();
    current.events[0].shape.deserialized[1].ty = "String".to_owned();
    add(
        "an_event_field_with_a_new_json_form_is_a_break",
        with_events(),
        current,
        (false, false),
        (false, false),
        "",
    );

    // Cases that are not in `compat.rs`. Each shows a difference by design,
    // or a rule both tools share.

    let mut deployed = with_events();
    deployed.events[0].shape = contact_shape(&[
        ("id", "i64"),
        ("email", "String"),
        ("phone", "Option<String>"),
    ]);
    deployed.listeners = vec![listener("on_contact", "contact.updated", &["phone"])];
    let mut current = with_events();
    current.listeners.clear();
    add(
        "an_optional_field_a_consumer_reads_is_removed",
        deployed,
        current,
        (false, true),
        (true, true),
        "read-sets: Avro checks only the fields in the schema, and fills a missing \
         nullable field with null. Autumn checks what each consumer reads",
    );

    let mut current = with_events();
    current.events[0].shape = contact_shape(&[("id", "i64"), ("email_address", "String")]);
    current.events[0].shape.deserialized[1].aliases = vec!["email".to_owned()];
    current.listeners = vec![listener(
        "on_contact",
        "contact.updated",
        &["email_address"],
    )];
    add(
        "a_renamed_field_with_an_alias",
        with_events(),
        current,
        (false, true),
        (false, true),
        "",
    );

    let mut deployed = with_events();
    deployed.events[0].shape = contact_shape(&[("id", "i32"), ("email", "String")]);
    add(
        "a_wider_integer",
        deployed,
        with_events(),
        (false, false),
        (false, true),
        "integer widths: Avro promotes `int` to `long`. The check compares Rust types, \
         and `i32` is not `i64`",
    );

    // `#[serde(default)] tags: Vec<String>`: always written, not needed to read.
    let mut current = with_events();
    current.events[0]
        .shape
        .serialized
        .push(field("tags", "Vec<String>", true));
    current.events[0]
        .shape
        .deserialized
        .push(field("tags", "Vec<String>", false));
    add(
        "a_new_field_with_a_serde_default",
        with_events(),
        current,
        (true, true),
        (true, true),
        "",
    );

    // `#[serde(skip_serializing_if = "Vec::is_empty")]` on a field that a
    // deployed consumer needs.
    let mut deployed = with_events();
    deployed.events[0]
        .shape
        .serialized
        .push(field("tags", "Vec<String>", true));
    deployed.events[0]
        .shape
        .deserialized
        .push(field("tags", "Vec<String>", true));
    let mut current = deployed.clone();
    current.events[0].shape.serialized[2].required = false;
    add(
        "a_field_that_becomes_skipped_when_empty",
        deployed,
        current,
        (false, true),
        (true, true),
        "presence: Avro data always holds each field, so the new schema gives the \
         field a default. The check sees that the JSON can leave out a field that a \
         deployed consumer needs",
    );

    let place = |id: &str| BundleType {
        id: id.to_owned(),
        shape: WireTypeDescriptor {
            name: "Address".to_owned(),
            serialized: vec![field("city", "String", true)],
            deserialized: vec![field("city", "String", true)],
            ..WireTypeDescriptor::default()
        },
    };
    let mut deployed = with_events();
    deployed.events[0].shape = contact_shape(&[("id", "i64"), ("home", "Address")]);
    deployed.types = vec![place("core::Address")];
    let mut current = deployed.clone();
    let moved = Some(WireType::object("core::model::Address"));
    let shape = &mut current.events[0].shape;
    shape.serialized[1].wire_type.clone_from(&moved);
    shape.deserialized[1].wire_type = moved;
    current.types = vec![place("core::model::Address")];
    add(
        "a_nested_type_that_moves_to_another_module",
        deployed,
        current,
        (true, true),
        (true, true),
        "",
    );

    // `home.zip` is optional, and the deployed consumer reads it.
    let address = |zip: bool| {
        let mut fields = vec![field("city", "String", true)];
        if zip {
            fields.push(field("zip", "Option<String>", false));
        }
        BundleType {
            id: "core::Address".to_owned(),
            shape: WireTypeDescriptor {
                name: "Address".to_owned(),
                serialized: fields.clone(),
                deserialized: fields,
                ..WireTypeDescriptor::default()
            },
        }
    };
    let mut deployed = with_events();
    deployed.events[0].shape = contact_shape(&[("id", "i64"), ("home", "Address")]);
    let shape = &mut deployed.events[0].shape;
    for home in [&mut shape.serialized[1], &mut shape.deserialized[1]] {
        home.nested = vec!["core::Address".to_owned()];
    }
    deployed.types = vec![address(true)];
    deployed.listeners = vec![listener(
        "on_contact",
        "contact.updated",
        &["home", "home.zip"],
    )];
    let mut current = deployed.clone();
    current.types = vec![address(false)];
    current.listeners = vec![listener("on_contact", "contact.updated", &["home"])];
    add(
        "an_optional_nested_event_field_a_deployed_consumer_reads_removed_is_a_break",
        deployed,
        current,
        (false, true),
        (true, true),
        "reads: Avro gives a missing optional field its default. The check knows that a \
         deployed consumer reads the field, so it would get `None` with no error",
    );

    out
}

/// The schema of each topic in an export, parsed.
fn schemas(bundle: &Bundle) -> Vec<(String, Schema)> {
    if bundle.events.is_empty() {
        return Vec::new();
    }
    let files = export(bundle, AvroOptions::default())
        .unwrap_or_else(|e| panic!("exports: {e}"))
        .files;
    files
        .into_iter()
        .map(|(path, text)| {
            let schema = Schema::parse_str(&text).unwrap_or_else(|e| panic!("{path}: {e}"));
            (path, schema)
        })
        .collect()
}

/// `(old reads new, new reads old)` over the topics both releases have.
fn avro_windows(deployed: &Bundle, current: &Bundle) -> (bool, bool) {
    let old = schemas(deployed);
    let new = schemas(current);
    let mut old_reads_new = true;
    let mut new_reads_old = true;
    for (path, old_schema) in &old {
        let Some((_, new_schema)) = new.iter().find(|(p, _)| p == path) else {
            continue;
        };
        old_reads_new &= SchemaCompatibility::can_read(new_schema, old_schema).is_ok();
        new_reads_old &= SchemaCompatibility::can_read(old_schema, new_schema).is_ok();
    }
    (old_reads_new, new_reads_old)
}

#[test]
fn avro_resolution_agrees_with_the_check_or_names_why() {
    let mut wrong = Vec::new();
    for case in cases() {
        let report = compare(&case.deployed, &case.current);
        let check = (
            report.callee_first.is_empty(),
            report.caller_first.is_empty(),
        );
        let avro = avro_windows(&case.deployed, &case.current);
        if check != case.check {
            wrong.push(format!(
                "{}: want check {:?}, got {check:?}: {report:?}",
                case.name, case.check
            ));
        }
        if avro != case.avro {
            wrong.push(format!(
                "{}: want Avro {:?}, got {avro:?}",
                case.name, case.avro
            ));
        }
        if (check == avro) != case.why.is_empty() {
            wrong.push(format!(
                "{}: the check gives {check:?} and Avro {avro:?}, so name a reason when \
                 they differ, and none when they agree (reason: `{}`)",
                case.name, case.why
            ));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

/// Cases that show a rule and have no test in `compat.rs`.
const EXTRA: &[&str] = &[
    "a_new_field_with_a_serde_default",
    "a_field_that_becomes_skipped_when_empty",
    "an_optional_field_a_consumer_reads_is_removed",
    "a_renamed_field_with_an_alias",
    "a_wider_integer",
    "a_nested_type_that_moves_to_another_module",
];

/// The name of each `#[test]` in `source`, with its body.
fn tests_in(source: &str) -> Vec<(&str, &str)> {
    let mut out = Vec::new();
    for chunk in source.split("#[test]").skip(1) {
        let rest = chunk
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty() && !l.starts_with("#[") && !l.starts_with("///"))
            .unwrap_or_default();
        let name = rest
            .strip_prefix("fn ")
            .and_then(|r| r.split('(').next())
            .unwrap_or_else(|| panic!("a `#[test]` with no `fn` after it: `{rest}`"));
        // The body ends at the first closing brace at the indent of `fn`.
        // A Windows checkout can have CRLF line ends.
        let end = ["\n    }\n", "\n    }\r\n"]
            .iter()
            .filter_map(|close| chunk.find(close))
            .min()
            .unwrap_or(chunk.len());
        out.push((name, chunk[..end].trim_end_matches('\r')));
    }
    out
}

/// A checkout with CRLF line ends gives the same tests and bodies.
#[test]
fn the_test_parser_reads_crlf_as_lf() {
    let lf = include_str!("../compat.rs").replace("\r\n", "\n");
    let crlf = lf.replace('\n', "\r\n");
    let normalize = |tests: Vec<(&str, &str)>| -> Vec<(String, String)> {
        tests
            .into_iter()
            .map(|(name, body)| (name.to_owned(), body.replace("\r\n", "\n")))
            .collect()
    };
    assert_eq!(normalize(tests_in(&crlf)), normalize(tests_in(&lf)));
}

/// Each event test in `compat.rs` has a case here, and each case that
/// mirrors a test still has its test.
#[test]
fn each_event_test_of_the_check_has_a_case() {
    let tests = tests_in(include_str!("../compat.rs"));
    let names: Vec<&str> = cases().iter().map(|c| c.name).collect();
    let events: Vec<&str> = tests
        .iter()
        .filter(|(_, body)| {
            [
                "with_events()",
                "listener(",
                "events[",
                "events.push",
                "events = vec",
            ]
            .iter()
            .any(|mark| body.contains(mark))
        })
        .map(|(name, _)| *name)
        .collect();
    assert!(
        events.len() >= 19,
        "the parser found too few event tests: {events:?}"
    );
    let missing: Vec<&&str> = events.iter().filter(|n| !names.contains(n)).collect();
    assert!(missing.is_empty(), "add a case for each: {missing:?}");
    let all: Vec<&str> = tests.iter().map(|(name, _)| *name).collect();
    let gone: Vec<&&str> = names
        .iter()
        .filter(|n| !EXTRA.contains(n) && !all.contains(n))
        .collect();
    assert!(
        gone.is_empty(),
        "no test in compat.rs has these names: {gone:?}"
    );
}
