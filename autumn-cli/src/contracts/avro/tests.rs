//! Golden tests for the Avro export.
//!
//! Each case writes its files under `tests/golden/contracts/avro/<case>/`.
//! Set `UPDATE_GOLDEN=1` to write the goldens again, then review the diff.
//! Each schema must parse with the `apache-avro` crate.

// Route paths such as `/items/{id}` are data here, not format strings.
#![allow(clippy::literal_string_with_formatting_args)]

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use apache_avro::Schema;
use autumn_macros_support::wire::{WireFieldDescriptor, WireType, WireTypeDescriptor, format};
use proptest::prelude::*;

use super::{AvroOptions, ENDPOINT_KEY, TOPIC_KEY, export, is_exported};
use crate::contracts::bundle::{
    Bundle, BundleEndpoint, BundleEvent, BundleType, NO_BODY, WIRE_TYPE_FORMAT,
};

fn golden_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/contracts/avro")
}

const UPDATE: &str = "If the change is intended, run `UPDATE_GOLDEN=1 cargo test -p \
                      autumn-cli --bin autumn contracts::avro` and review the diff.";

/// Compare the files of `case` with its golden directory. Each file must
/// parse as an Avro schema.
fn golden(case: &str, files: &BTreeMap<String, String>) {
    for (path, text) in files {
        if let Err(e) = Schema::parse_str(text) {
            panic!("{case}/{path} does not parse with apache-avro: {e}\n{text}");
        }
    }
    let dir = golden_root().join(case);
    if std::env::var("UPDATE_GOLDEN").is_ok() {
        let _ = std::fs::remove_dir_all(&dir);
        for (path, text) in files {
            let path = dir.join(path);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
            std::fs::write(path, text).expect("write golden");
        }
    }
    let have = read_tree(&dir);
    assert_eq!(
        have.keys().collect::<Vec<_>>(),
        files.keys().collect::<Vec<_>>(),
        "{case}: the file list differs from the golden directory. {UPDATE}"
    );
    for (path, text) in files {
        assert_eq!(&have[path], text, "{case}/{path} differs. {UPDATE}");
    }
}

fn read_tree(root: &Path) -> BTreeMap<String, String> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, String>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries {
            let path = entry.expect("entry").path();
            if path.is_dir() {
                walk(root, &path, out);
            } else if path.extension().is_some_and(|e| e == "avsc") {
                let key = path
                    .strip_prefix(root)
                    .expect("under root")
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join("/");
                out.insert(key, std::fs::read_to_string(&path).expect("read"));
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

// --- fixtures ---

fn field(name: &str, wire: WireType) -> WireFieldDescriptor {
    let required = !matches!(wire, WireType::Nullable { .. });
    WireFieldDescriptor {
        rust_name: name.to_owned(),
        wire_name: name.to_owned(),
        ty: "T".to_owned(),
        wire_type: Some(wire),
        required,
        ..WireFieldDescriptor::default()
    }
}

fn shape(name: &str, fields: Vec<WireFieldDescriptor>) -> WireTypeDescriptor {
    WireTypeDescriptor {
        name: name.to_owned(),
        serialized: fields.clone(),
        deserialized: fields,
        ..WireTypeDescriptor::default()
    }
}

fn event(
    topic: &str,
    krate: &str,
    type_name: &str,
    fields: Vec<WireFieldDescriptor>,
) -> BundleEvent {
    let (name, version) = match topic.rsplit_once(".v") {
        Some((name, v)) if v.bytes().all(|b| b.is_ascii_digit()) && !v.is_empty() => {
            (name.to_owned(), v.parse().expect("version"))
        }
        _ => (topic.to_owned(), 1),
    };
    BundleEvent {
        topic: topic.to_owned(),
        name,
        version,
        krate: krate.to_owned(),
        type_name: type_name.to_owned(),
        opaque: false,
        unit: false,
        shape: shape(type_name, fields),
    }
}

fn nested(id: &str, fields: Vec<WireFieldDescriptor>) -> BundleType {
    BundleType {
        id: id.to_owned(),
        shape: shape(id.rsplit("::").next().expect("name"), fields),
    }
}

fn bundle(events: Vec<BundleEvent>, types: Vec<BundleType>) -> Bundle {
    let mut bundle = Bundle {
        format: WIRE_TYPE_FORMAT,
        events,
        types,
        ..Bundle::default()
    };
    bundle.events.sort();
    bundle.types.sort();
    bundle
}

fn run(bundle: &Bundle) -> BTreeMap<String, String> {
    export(bundle, AvroOptions::default())
        .unwrap_or_else(|e| panic!("exports: {e}"))
        .files
}

fn refusal(bundle: &Bundle, options: AvroOptions) -> String {
    match export(bundle, options) {
        Ok(out) => panic!("want a refusal, got {:?}", out.files.keys()),
        Err(e) => e,
    }
}

fn contact_updated() -> BundleEvent {
    event(
        "contact.updated",
        "core",
        "ContactUpdated",
        vec![
            field("id", WireType::string_in(format::UUID)),
            field("email", WireType::string()),
            field("phone", WireType::nullable(WireType::string())),
        ],
    )
}

// --- goldens ---

#[test]
fn the_example_of_the_proposal_is_the_exact_output() {
    let files = run(&bundle(vec![contact_updated()], Vec::new()));
    golden("contact", &files);
    assert_eq!(
        files["contact.updated.avsc"],
        r#"{
  "type": "record",
  "name": "ContactUpdated",
  "namespace": "autumn.core",
  "doc": "topic: contact.updated, version 1",
  "autumn.topic": "contact.updated",
  "fields": [
    { "name": "id", "type": { "type": "string", "logicalType": "uuid" } },
    { "name": "email", "type": "string" },
    { "name": "phone", "type": ["null", "string"], "default": null }
  ]
}
"#
    );
}

#[test]
fn each_wire_type_maps_as_the_table_says() {
    let mapped = event(
        "catalog.mapped",
        "mesh-catalog",
        "Mapped",
        vec![
            field("text", WireType::string()),
            field("id", WireType::string_in(format::UUID)),
            field("at", WireType::string_in(format::DATE_TIME)),
            field("price", WireType::string_in(format::DECIMAL)),
            field("tiny", WireType::integer(8, true)),
            field("small", WireType::integer(16, true)),
            field("int", WireType::integer(32, true)),
            field("byte", WireType::integer(8, false)),
            field("short", WireType::integer(16, false)),
            field("unsigned", WireType::integer(32, false)),
            field("long", WireType::integer(64, true)),
            field(
                "seconds",
                WireType::Integer {
                    bits: 64,
                    signed: true,
                    format: Some(format::UNIX_SECONDS.to_owned()),
                },
            ),
            field("ratio", WireType::Number),
            field("flag", WireType::Boolean),
            field("tags", WireType::array(WireType::string())),
            field("scores", WireType::map(WireType::Number)),
            field("note", WireType::nullable(WireType::string())),
            field(
                "ref_id",
                WireType::nullable(WireType::string_in(format::UUID)),
            ),
            field(
                "gaps",
                WireType::array(WireType::nullable(WireType::integer(32, true))),
            ),
            field(
                "maybe_list",
                WireType::nullable(WireType::array(WireType::Boolean)),
            ),
            field(
                "grid",
                WireType::array(WireType::array(WireType::integer(16, true))),
            ),
        ],
    );
    golden("mapping", &run(&bundle(vec![mapped], Vec::new())));
}

#[test]
fn an_alias_is_an_avro_alias_and_a_bad_alias_is_left_out() {
    let mut renamed = event(
        "contact.renamed",
        "core",
        "ContactRenamed",
        vec![
            field("id", WireType::integer(64, true)),
            field("email_address", WireType::string()),
        ],
    );
    // `e-mail` is no Avro name, and `id` is another field's name.
    renamed.shape.deserialized[1].aliases = vec![
        "email".to_owned(),
        "e-mail".to_owned(),
        "id".to_owned(),
        "mail".to_owned(),
    ];
    golden("aliases", &run(&bundle(vec![renamed], Vec::new())));
}

#[test]
fn a_field_the_json_can_leave_out_has_a_default() {
    let mut ev = event(
        "stock.counted",
        "core",
        "StockCounted",
        vec![
            field("id", WireType::integer(64, true)),
            field("count", WireType::integer(32, false)),
            field("tags", WireType::array(WireType::string())),
            field("labels", WireType::map(WireType::string())),
            field("primary", WireType::string()),
            field("secondary", WireType::string()),
        ],
    );
    // `#[serde(default)] count`: always written, not needed to read.
    ev.shape.deserialized[1].required = false;
    // `skip_serializing_if` on `tags` and `labels`.
    ev.shape.serialized[2].required = false;
    ev.shape.serialized[3].required = false;
    // Two fields with one alias: serde gives the key to the first.
    ev.shape.deserialized[4].aliases = vec!["main".to_owned()];
    ev.shape.deserialized[5].aliases = vec!["main".to_owned()];
    // `#[serde(skip_serializing, default)] internal`: read only.
    ev.shape
        .deserialized
        .push(field("internal", WireType::string()));
    let files = run(&bundle(vec![ev], Vec::new()));
    let text = &files["stock.counted.avsc"];
    assert_eq!(text.matches("\"main\"").count(), 1, "{text}");
    golden("presence", &files);
}

#[test]
fn a_nullable_in_a_nullable_is_one_union() {
    let twice = WireType::Nullable {
        of: Box::new(WireType::Nullable {
            of: Box::new(WireType::string()),
        }),
    };
    let files = run(&bundle(
        vec![event(
            "note.added",
            "core",
            "NoteAdded",
            vec![field("note", twice)],
        )],
        Vec::new(),
    ));
    let text = &files["note.added.avsc"];
    assert!(
        text.contains(r#"{ "name": "note", "type": ["null", "string"], "default": null }"#),
        "{text}"
    );
}

#[test]
fn a_unit_event_is_a_record_with_no_fields() {
    let mut ping = event("ping", "core", "Ping", Vec::new());
    ping.unit = true;
    ping.shape.unit = true;
    let mut ping_2 = event("ping.v2", "core", "PingV2", Vec::new());
    ping_2.unit = true;
    ping_2.shape.unit = true;
    let files = run(&bundle(vec![ping, ping_2], Vec::new()));
    assert!(files.contains_key("ping.v2.avsc"), "{files:?}");
    golden("unit", &files);
}

#[test]
fn a_nested_type_is_defined_once_then_named() {
    let order = event(
        "orders.placed",
        "mesh-orders",
        "OrderPlaced",
        vec![
            field("ship_to", WireType::object("mesh-orders::model::Address")),
            field(
                "bill_to",
                WireType::nullable(WireType::object("mesh-orders::model::Address")),
            ),
            field("tree", WireType::object("mesh-orders::model::Node")),
            field("legacy", WireType::object("mesh-orders::legacy::Address")),
            field(
                "by_name",
                WireType::map(WireType::object("mesh-orders::model::Address")),
            ),
        ],
    );
    let types = vec![
        nested(
            "mesh-orders::model::Address",
            vec![
                field("street", WireType::string()),
                field("zip", WireType::nullable(WireType::string())),
            ],
        ),
        nested(
            "mesh-orders::legacy::Address",
            vec![field("line", WireType::string())],
        ),
        nested(
            "mesh-orders::model::Node",
            vec![
                field("label", WireType::string()),
                field(
                    "children",
                    WireType::array(WireType::object("mesh-orders::model::Node")),
                ),
            ],
        ),
    ];
    let files = run(&bundle(vec![order], types));
    let text = &files["orders.placed.avsc"];
    assert_eq!(
        text.matches("\"name\": \"Address\"").count(),
        2,
        "each Address is defined once:\n{text}"
    );
    golden("nested", &files);
}

#[test]
fn an_event_that_names_its_own_type_refers_to_its_record() {
    let mut node = event(
        "tree.grown",
        "core",
        "Node",
        vec![
            field("label", WireType::string()),
            field("children", WireType::array(WireType::object("core::Node"))),
        ],
    );
    node.shape.name = "Node".to_owned();
    let types = vec![nested("core::Node", node.shape.serialized.clone())];
    let files = run(&bundle(vec![node], types));
    let text = &files["tree.grown.avsc"];
    assert_eq!(text.matches("\"type\": \"record\"").count(), 1, "{text}");
    assert!(
        text.contains("\"items\": \"autumn.core.TreeGrown\""),
        "{text}"
    );
}

#[test]
fn an_opaque_event_is_bytes_with_a_note() {
    let mut opaque = event("audit.logged", "core", "AuditLogged", Vec::new());
    opaque.opaque = true;
    golden("opaque", &run(&bundle(vec![opaque], Vec::new())));
}

fn body(name: &str, fields: Vec<WireFieldDescriptor>) -> WireTypeDescriptor {
    shape(name, fields)
}

fn no_body() -> WireTypeDescriptor {
    WireTypeDescriptor {
        name: NO_BODY.to_owned(),
        unit: true,
        ..WireTypeDescriptor::default()
    }
}

fn endpoint(name: &str, method: &str, path: &str) -> BundleEndpoint {
    BundleEndpoint {
        service: "catalog".to_owned(),
        name: name.to_owned(),
        krate: "mesh-catalog".to_owned(),
        method: method.to_owned(),
        path: path.to_owned(),
        request: no_body(),
        response: no_body(),
        ..BundleEndpoint::default()
    }
}

fn endpoints_bundle() -> Bundle {
    let item = body(
        "Item",
        vec![
            field("id", WireType::string()),
            field("price_cents", WireType::integer(32, false)),
        ],
    );
    let mut create = endpoint("create_item", "POST", "/items");
    create.has_body = true;
    create.request = body("NewItem", vec![field("name", WireType::string())]);
    create.response = item.clone();
    let mut list = endpoint("list_items", "GET", "/items");
    list.response = item.clone();
    list.response_list = true;
    let mut ping = endpoint("ping", "POST", "/ping");
    ping.response.name = "Pong".to_owned();
    let mut gone = endpoint("gone", "DELETE", "/gone/{id}");
    gone.unrouted = true;
    gone.response = item;
    let mut bundle = bundle(vec![contact_updated()], Vec::new());
    bundle.endpoints = vec![create, gone, list, ping];
    bundle
}

#[test]
fn endpoints_add_each_body_and_skip_no_body_and_unrouted_routes() {
    let with = export(&endpoints_bundle(), AvroOptions { endpoints: true })
        .expect("exports")
        .files;
    assert!(!with.keys().any(|k| k.contains("gone")), "{with:?}");
    assert!(
        !with.contains_key("endpoints/catalog.list_items.request.avsc"),
        "a GET with no body has no request schema"
    );
    golden("endpoints", &with);
    let without = run(&endpoints_bundle());
    assert_eq!(
        without.keys().collect::<Vec<_>>(),
        ["contact.updated.avsc"],
        "endpoints are off by default"
    );
}

#[test]
fn two_exports_of_one_bundle_give_the_same_bytes() {
    let one = export(&endpoints_bundle(), AvroOptions { endpoints: true }).expect("one");
    let two = export(&endpoints_bundle(), AvroOptions { endpoints: true }).expect("two");
    assert_eq!(one.files, two.files);
}

#[test]
fn an_exported_schema_has_the_marker_and_another_file_does_not() {
    let files = export(&endpoints_bundle(), AvroOptions { endpoints: true })
        .expect("exports")
        .files;
    for (path, text) in &files {
        assert!(is_exported(text), "{path}");
    }
    assert!(files["contact.updated.avsc"].contains(TOPIC_KEY));
    assert!(files["endpoints/catalog.create_item.request.avsc"].contains(ENDPOINT_KEY));
    for other in [
        r#"{"type": "record", "name": "Mine", "fields": []}"#,
        "not json",
        r#"{"type": "string", "autumn.topic": 3}"#,
    ] {
        assert!(!is_exported(other), "{other}");
    }
}

// --- refusals ---

/// Each case that the export refuses, with the bundle and options.
#[allow(clippy::too_many_lines)]
fn refusal_cases() -> Vec<(&'static str, Bundle, AvroOptions)> {
    let one = |fields: Vec<WireFieldDescriptor>| {
        bundle(
            vec![event("stock.counted", "core", "StockCounted", fields)],
            Vec::new(),
        )
    };
    let events = AvroOptions::default();
    let mut two_forms = field("count", WireType::integer(32, true));
    two_forms.wire_type = Some(WireType::string());
    let mut two_forms_shape = one(vec![field("count", WireType::integer(32, true))]);
    two_forms_shape.events[0].shape.deserialized = vec![two_forms];

    let mut any_body = endpoints_bundle();
    any_body.endpoints[0].request = body("NewItem", vec![field("meta", WireType::Any)]);

    let mut same_path = endpoints_bundle();
    let mut twin = same_path.endpoints[0].clone();
    twin.krate = "other-crate".to_owned();
    same_path.endpoints.push(twin);

    let mut nothing = bundle(Vec::new(), Vec::new());
    nothing.endpoints = endpoints_bundle().endpoints;

    vec![
        (
            "u64",
            one(vec![field("count", WireType::integer(64, false))]),
            events,
        ),
        (
            "i128",
            one(vec![field("count", WireType::integer(128, true))]),
            events,
        ),
        (
            "u128",
            one(vec![field("count", WireType::integer(128, false))]),
            events,
        ),
        (
            "u64_in_a_list",
            one(vec![field(
                "counts",
                WireType::array(WireType::integer(64, false)),
            )]),
            events,
        ),
        ("any", one(vec![field("meta", WireType::Any)]), events),
        (
            "nullable_any",
            one(vec![field("meta", WireType::nullable(WireType::Any))]),
            events,
        ),
        (
            "object_with_no_shape",
            one(vec![field("place", WireType::object("core::Place"))]),
            events,
        ),
        ("two_forms", two_forms_shape, events),
        (
            "field_name",
            one(vec![field("price-cents", WireType::integer(32, true))]),
            events,
        ),
        (
            "topic_file_name",
            bundle(
                vec![event("../stock", "core", "Stock", Vec::new())],
                Vec::new(),
            ),
            events,
        ),
        (
            "topic_case",
            bundle(
                vec![
                    event("Stock.counted", "core", "StockA", Vec::new()),
                    event("stock.counted", "core", "StockB", Vec::new()),
                ],
                Vec::new(),
            ),
            events,
        ),
        (
            "type_name_clash",
            bundle(
                vec![event(
                    "stock.counted",
                    "core",
                    "StockCounted",
                    vec![
                        field("a", WireType::object("core::a_b::Place")),
                        field("b", WireType::object("core::a-b::Place")),
                    ],
                )],
                vec![
                    nested("core::a_b::Place", vec![field("x", WireType::Number)]),
                    nested("core::a-b::Place", vec![field("y", WireType::Number)]),
                ],
            ),
            events,
        ),
        ("endpoint_any", any_body, AvroOptions { endpoints: true }),
        (
            "topic_device_name",
            bundle(
                vec![event("aux.created", "core", "AuxCreated", Vec::new())],
                Vec::new(),
            ),
            events,
        ),
        ("same_path", same_path, AvroOptions { endpoints: true }),
        ("nothing_to_export", nothing, events),
    ]
}

#[test]
fn each_refusal_names_the_field_and_the_fix() {
    let mut text = String::new();
    for (case, bundle, options) in refusal_cases() {
        let _ = write!(text, "## {case}\n{}\n\n", refusal(&bundle, options));
    }
    let path = golden_root().join("refusals.txt");
    if std::env::var("UPDATE_GOLDEN").is_ok() {
        std::fs::create_dir_all(golden_root()).expect("mkdir");
        std::fs::write(&path, &text).expect("write golden");
    }
    let want = std::fs::read_to_string(&path).unwrap_or_default();
    assert_eq!(text, want, "refusals.txt differs. {UPDATE}");
}

#[test]
fn a_u64_with_a_wire_override_is_a_long() {
    // `#[wire(type = "integer")]` on a `u64` records a signed 64-bit integer.
    let files = run(&bundle(
        vec![event(
            "stock.counted",
            "core",
            "StockCounted",
            vec![field("count", WireType::integer(64, true))],
        )],
        Vec::new(),
    ));
    assert!(
        files["stock.counted.avsc"].contains(r#"{ "name": "count", "type": "long" }"#),
        "{files:?}"
    );
}

#[test]
fn all_refusals_are_reported_together() {
    let both = bundle(
        vec![
            event("a.one", "core", "One", vec![field("x", WireType::Any)]),
            event(
                "b.two",
                "core",
                "Two",
                vec![field("y", WireType::integer(64, false))],
            ),
        ],
        Vec::new(),
    );
    let text = refusal(&both, AvroOptions::default());
    assert!(
        text.contains("`a.one`") && text.contains("`b.two`"),
        "{text}"
    );
}

#[test]
fn every_golden_schema_parses_with_apache_avro() {
    let all = read_tree(&golden_root());
    assert!(all.len() >= 8, "{:?}", all.keys());
    for (path, text) in all {
        if let Err(e) = Schema::parse_str(&text) {
            panic!("{path} does not parse with apache-avro: {e}");
        }
    }
}

// --- property ---

fn leaf() -> impl Strategy<Value = WireType> {
    prop_oneof![
        Just(WireType::string()),
        Just(WireType::string_in(format::UUID)),
        Just(WireType::string_in(format::DATE)),
        (
            prop::sample::select(vec![8_u8, 16, 32, 64, 128]),
            any::<bool>()
        )
            .prop_map(|(bits, signed)| WireType::integer(bits, signed)),
        Just(WireType::Number),
        Just(WireType::Boolean),
        Just(WireType::Any),
        Just(WireType::object("core::Inner")),
        Just(WireType::object("core::Missing")),
    ]
}

fn wire() -> impl Strategy<Value = WireType> {
    leaf().prop_recursive(3, 12, 1, |inner| {
        prop_oneof![
            inner.clone().prop_map(WireType::array),
            inner.clone().prop_map(WireType::map),
            inner.prop_map(WireType::nullable),
        ]
    })
}

fn names() -> impl Strategy<Value = String> {
    prop::sample::select(vec![
        "id",
        "name",
        "price-cents",
        "_x",
        "x1",
        "type",
        "1st",
        "café",
        "Name",
    ])
    .prop_map(str::to_owned)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// An export either refuses or writes schemas that apache-avro parses.
    #[test]
    fn an_export_parses_or_refuses(
        fields in prop::collection::btree_map(names(), wire(), 0..6),
        inner in prop::collection::btree_map(names(), wire(), 0..4),
        unit in any::<bool>(),
        opaque in any::<bool>(),
    ) {
        let fields: Vec<WireFieldDescriptor> =
            fields.into_iter().map(|(n, w)| field(&n, w)).collect();
        let mut ev = event("prop.event", "core", "PropEvent", fields);
        ev.opaque = opaque;
        if opaque {
            ev.shape = WireTypeDescriptor::default();
        }
        if unit && !opaque {
            ev.unit = true;
            ev.shape.unit = true;
            ev.shape.serialized.clear();
            ev.shape.deserialized.clear();
        }
        let inner: Vec<WireFieldDescriptor> =
            inner.into_iter().map(|(n, w)| field(&n, w)).collect();
        let b = bundle(vec![ev], vec![nested("core::Inner", inner)]);
        if let Ok(out) = export(&b, AvroOptions::default()) {
            for (path, text) in &out.files {
                prop_assert!(Schema::parse_str(text).is_ok(), "{path}:\n{text}");
                prop_assert!(is_exported(text));
            }
            let again = export(&b, AvroOptions::default()).expect("again");
            prop_assert_eq!(out.files, again.files);
        }
    }
}
