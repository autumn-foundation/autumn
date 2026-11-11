//! Golden tests for the protobuf export.
//!
//! Each case writes its file tree, with `proto.lock`, under
//! `tests/golden/contracts/proto/<case>/`. Set `UPDATE_GOLDEN=1` to write the
//! goldens again, then review the diff.

// Route paths such as `/items/{id}` are data here, not format strings.
#![allow(clippy::literal_string_with_formatting_args)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use autumn_macros_support::wire::{WireFieldDescriptor, WireType, WireTypeDescriptor, format};

use super::lock::Lock;
use super::{ProtoOptions, export};
use crate::contracts::bundle::{
    Bundle, BundleEndpoint, BundleEvent, BundleType, NO_BODY, WIRE_TYPE_FORMAT,
};

/// The lock file name in a golden tree.
const LOCK_FILE: &str = "proto.lock";

fn golden_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/contracts/proto")
}

/// Compare a file tree and its lock with the golden directory of `case`.
fn golden(case: &str, files: &BTreeMap<String, String>, lock: &Lock) {
    let dir = golden_root().join(case);
    let mut want = files.clone();
    want.insert(LOCK_FILE.to_owned(), lock.render());
    if std::env::var("UPDATE_GOLDEN").is_ok() {
        let _ = std::fs::remove_dir_all(&dir);
        for (path, text) in &want {
            let path = dir.join(path);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
            std::fs::write(path, text).expect("write golden");
        }
    }
    let mut have = BTreeMap::new();
    read_tree(&dir, &dir, &mut have);
    assert_eq!(
        have.keys().collect::<Vec<_>>(),
        want.keys().collect::<Vec<_>>(),
        "{case}: the file list differs from the golden directory. If the change is \
         intended, run `UPDATE_GOLDEN=1 cargo test -p autumn-cli --bin autumn \
         contracts::proto` and review the diff."
    );
    for (path, text) in &want {
        assert_eq!(
            &have[path], text,
            "{case}/{path} differs from its golden file. If the change is intended, run \
             `UPDATE_GOLDEN=1 cargo test -p autumn-cli --bin autumn contracts::proto` and \
             review the diff."
        );
    }
}

fn read_tree(root: &Path, dir: &Path, out: &mut BTreeMap<String, String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries {
        let path = entry.expect("entry").path();
        if path.is_dir() {
            read_tree(root, &path, out);
        } else {
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

fn field(name: &str, wire: WireType, required: bool) -> WireFieldDescriptor {
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

fn no_body() -> WireTypeDescriptor {
    WireTypeDescriptor {
        name: NO_BODY.to_owned(),
        unit: true,
        ..WireTypeDescriptor::default()
    }
}

fn get(name: &str, path: &str, response: WireTypeDescriptor) -> BundleEndpoint {
    BundleEndpoint {
        service: "catalog".to_owned(),
        name: name.to_owned(),
        krate: "mesh-catalog".to_owned(),
        method: "GET".to_owned(),
        path: path.to_owned(),
        request: no_body(),
        response,
        ..BundleEndpoint::default()
    }
}

fn bundle(endpoints: Vec<BundleEndpoint>, types: Vec<BundleType>) -> Bundle {
    let mut bundle = Bundle {
        format: WIRE_TYPE_FORMAT,
        endpoints,
        types,
        ..Bundle::default()
    };
    bundle.endpoints.sort();
    bundle.types.sort();
    bundle
}

fn plain() -> ProtoOptions {
    ProtoOptions::default()
}

fn run(bundle: &Bundle, options: ProtoOptions) -> (BTreeMap<String, String>, Lock) {
    let out = export(bundle, &Lock::default(), options).expect("exports");
    (out.files, out.lock)
}

/// One field per row of the mapping table.
fn mapping_bundle() -> Bundle {
    let fields = vec![
        field("text", WireType::string(), true),
        field("at", WireType::string_in(format::DATE_TIME), true),
        field("id", WireType::string_in(format::UUID), true),
        field("small", WireType::integer(8, true), true),
        field("count", WireType::integer(32, true), true),
        field("big", WireType::integer(64, true), true),
        field("size", WireType::integer(32, false), true),
        field("total", WireType::integer(64, false), true),
        field("huge", WireType::integer(128, true), true),
        field("ratio", WireType::Number, true),
        field("on", WireType::Boolean, true),
        field("tags", WireType::array(WireType::string()), true),
        field("labels", WireType::map(WireType::integer(32, false)), true),
        field("detail", WireType::object("mesh_catalog::Detail"), true),
        field("note", WireType::nullable(WireType::string()), false),
        field(
            "maybe_detail",
            WireType::nullable(WireType::object("mesh_catalog::Detail")),
            false,
        ),
        field(
            "maybe_tags",
            WireType::nullable(WireType::array(WireType::string())),
            false,
        ),
        field("extra", WireType::Any, true),
        field("maybe_extra", WireType::nullable(WireType::Any), false),
        field(
            "seconds",
            WireType::Integer {
                bits: 64,
                signed: true,
                format: Some(format::UNIX_SECONDS.to_owned()),
            },
            true,
        ),
    ];
    let detail = BundleType {
        id: "mesh_catalog::Detail".to_owned(),
        shape: shape("Detail", vec![field("color", WireType::string(), true)]),
    };
    bundle(
        vec![get("get_item", "/items/{id}", shape("Item", fields))],
        vec![detail],
    )
}

#[test]
fn the_mapping_table_matches_the_golden_files() {
    let (files, lock) = run(&mapping_bundle(), plain());
    golden("mapping", &files, &lock);
}

#[test]
fn well_known_types_map_a_date_time_to_a_timestamp() {
    let options = ProtoOptions {
        well_known_types: true,
        ..plain()
    };
    let (files, lock) = run(&mapping_bundle(), options);
    golden("well_known_types", &files, &lock);
}

#[test]
fn each_wrapper_case_matches_the_golden_files() {
    let grid = shape(
        "Grid",
        vec![
            field(
                "rows",
                WireType::array(WireType::array(WireType::Number)),
                true,
            ),
            field(
                "cells",
                WireType::array(WireType::nullable(WireType::string())),
                true,
            ),
            field(
                "scores",
                WireType::map(WireType::nullable(WireType::integer(32, true))),
                true,
            ),
            field(
                "groups",
                WireType::map(WireType::array(WireType::string())),
                true,
            ),
            field(
                "nested_maps",
                WireType::array(WireType::map(WireType::Boolean)),
                true,
            ),
            field(
                "maybe_items",
                WireType::array(WireType::nullable(WireType::object("mesh_catalog::Item"))),
                true,
            ),
        ],
    );
    let item = shape("Item", vec![field("id", WireType::string(), true)]);
    let list = BundleEndpoint {
        response_list: true,
        ..get("list_items", "/items", item.clone())
    };
    let upload = BundleEndpoint {
        name: "put_items".to_owned(),
        method: "PUT".to_owned(),
        path: "/shelves/{shelf}/items".to_owned(),
        has_body: true,
        request_list: true,
        request: item.clone(),
        response: no_body(),
        ..get("", "", no_body())
    };
    let b = bundle(
        vec![get("get_grid", "/grid", grid), list, upload],
        vec![BundleType {
            id: "mesh_catalog::Item".to_owned(),
            shape: item,
        }],
    );
    let (files, lock) = run(&b, plain());
    golden("wrappers", &files, &lock);
}

#[test]
fn json_name_is_the_wire_name_and_the_proto_name_is_snake_case() {
    let mut price = field("price_cents", WireType::integer(32, false), true);
    price.wire_name = "priceCents".to_owned();
    let mut kind = field("kind", WireType::string(), true);
    kind.wire_name = "@type".to_owned();
    let b = bundle(
        vec![get(
            "get_item",
            "/items/{id}",
            shape("Item", vec![price, kind]),
        )],
        vec![],
    );
    let (files, _) = run(&b, plain());
    let text = &files["autumn/catalog/catalog.proto"];
    assert!(
        text.contains("uint32 price_cents = 1 [json_name = \"priceCents\"];"),
        "{text}"
    );
    assert!(
        text.contains("string type = 2 [json_name = \"@type\"];"),
        "{text}"
    );
}

#[test]
fn path_parameters_and_the_body_join_into_one_request_with_http_annotations() {
    let body = shape(
        "Patch",
        vec![
            field("id", WireType::string(), true),
            field("name", WireType::string(), true),
            field("note", WireType::nullable(WireType::string()), false),
        ],
    );
    let patch = BundleEndpoint {
        method: "PATCH".to_owned(),
        has_body: true,
        request: body,
        ..get(
            "patch_item",
            "/shops/{shop}/items/{id}",
            shape("Item", vec![field("id", WireType::string(), true)]),
        )
    };
    let delete = BundleEndpoint {
        method: "DELETE".to_owned(),
        ..get("delete_item", "/items/{id}", no_body())
    };
    let files_of = |http_annotations| {
        let options = ProtoOptions {
            http_annotations,
            ..plain()
        };
        run(
            &bundle(vec![patch.clone(), delete.clone()], vec![]),
            options,
        )
    };
    let (files, lock) = files_of(true);
    golden("http_annotations", &files, &lock);
    let (files, _) = files_of(false);
    let text = &files["autumn/catalog/catalog.proto"];
    assert!(!text.contains("google.api.http"), "{text}");
    assert!(text.contains("// PATCH /shops/{shop}/items/{id}"), "{text}");
}

#[test]
fn events_go_in_one_file_per_crate() {
    let event = |topic: &str, krate: &str, type_name: &str| BundleEvent {
        topic: topic.to_owned(),
        name: topic.to_owned(),
        version: 1,
        krate: krate.to_owned(),
        type_name: type_name.to_owned(),
        opaque: false,
        unit: false,
        shape: shape(
            type_name,
            vec![
                field("id", WireType::string(), true),
                field("price", WireType::object("mesh_catalog::Price"), true),
            ],
        ),
    };
    let mut b = bundle(
        vec![],
        vec![BundleType {
            id: "mesh_catalog::Price".to_owned(),
            shape: shape(
                "Price",
                vec![field("cents", WireType::integer(32, false), true)],
            ),
        }],
    );
    b.events = vec![
        event("catalog.item_repriced", "mesh-catalog", "ItemRepriced"),
        event("orders.placed", "mesh-orders", "OrderPlaced"),
        BundleEvent {
            opaque: true,
            shape: WireTypeDescriptor::default(),
            ..event("orders.audit", "mesh-orders", "Audit")
        },
    ];
    b.events.sort();
    let (files, lock) = run(&b, plain());
    golden("events", &files, &lock);
}

#[test]
fn two_exports_give_the_same_bytes() {
    let b = mapping_bundle();
    let first = export(&b, &Lock::default(), plain()).expect("exports");
    let second = export(&b, &first.lock, plain()).expect("exports");
    assert_eq!(first.files, second.files);
    assert_eq!(first.lock.render(), second.lock.render());
}

#[test]
fn a_removed_field_is_reserved_by_number_and_name() {
    let before = bundle(
        vec![get(
            "get_item",
            "/items/{id}",
            shape(
                "Item",
                vec![
                    field("id", WireType::string(), true),
                    field("sku", WireType::string(), true),
                ],
            ),
        )],
        vec![],
    );
    let after = bundle(
        vec![get(
            "get_item",
            "/items/{id}",
            shape(
                "Item",
                vec![
                    field("id", WireType::string(), true),
                    field("name", WireType::string(), true),
                ],
            ),
        )],
        vec![],
    );
    let first = export(&before, &Lock::default(), plain()).expect("exports");
    let second = export(&after, &first.lock, plain()).expect("exports");
    golden("removed_field", &second.files, &second.lock);
}

#[test]
fn a_wire_tag_sets_the_field_number() {
    let mut id = field("id", WireType::string(), true);
    id.tag = Some(5);
    let b = bundle(
        vec![get(
            "get_item",
            "/items/{id}",
            shape("Item", vec![id, field("name", WireType::string(), true)]),
        )],
        vec![],
    );
    let (files, _) = run(&b, plain());
    let text = &files["autumn/catalog/catalog.proto"];
    assert!(
        text.contains("string id = 5 [json_name = \"id\"];"),
        "{text}"
    );
    assert!(
        text.contains("string name = 6 [json_name = \"name\"];"),
        "{text}"
    );
}

#[test]
fn two_types_with_one_name_get_names_from_their_ids() {
    let a = shape("Item", vec![field("a", WireType::string(), true)]);
    let b_shape = shape("Item", vec![field("b", WireType::string(), true)]);
    let pair = shape(
        "Pair",
        vec![
            field("left", WireType::object("shop::Item"), true),
            field("right", WireType::object("shop::api::Item"), true),
        ],
    );
    let b = bundle(
        vec![get("get_pair", "/pair", pair)],
        vec![
            BundleType {
                id: "shop::Item".to_owned(),
                shape: a,
            },
            BundleType {
                id: "shop::api::Item".to_owned(),
                shape: b_shape,
            },
        ],
    );
    let (files, _) = run(&b, plain());
    let text = &files["autumn/catalog/catalog.proto"];
    assert!(text.contains("message ShopItem {"), "{text}");
    assert!(text.contains("message ShopApiItem {"), "{text}");
    assert!(text.contains("ShopApiItem right = 2"), "{text}");
}

#[test]
fn an_unrouted_endpoint_is_left_out() {
    let item = shape("Item", vec![field("id", WireType::string(), true)]);
    let gone = BundleEndpoint {
        unrouted: true,
        ..get("old_item", "/old/{id}", item.clone())
    };
    let b = bundle(vec![get("get_item", "/items/{id}", item), gone], vec![]);
    let (files, _) = run(&b, plain());
    let text = &files["autumn/catalog/catalog.proto"];
    assert!(!text.contains("OldItem"), "{text}");
}

#[test]
fn a_bundle_with_nothing_to_export_is_refused() {
    let item = shape("Item", vec![field("id", WireType::string(), true)]);
    let gone = BundleEndpoint {
        unrouted: true,
        ..get("old_item", "/old/{id}", item)
    };
    let err = export(&bundle(vec![gone], vec![]), &Lock::default(), plain()).expect_err("refused");
    assert!(err.contains("no routed endpoint"), "{err}");
}

#[test]
fn the_buf_config_names_the_lint_exceptions() {
    let (files, _) = run(&mapping_bundle(), plain());
    let yaml = &files["buf.yaml"];
    assert!(yaml.contains("RPC_RESPONSE_STANDARD_NAME"), "{yaml}");
    assert!(yaml.contains("WIRE_JSON"), "{yaml}");
    assert!(!yaml.contains("googleapis"), "{yaml}");
}

#[test]
fn a_type_reached_only_from_a_request_body_is_a_message() {
    let body = shape(
        "NewItem",
        vec![field(
            "detail",
            WireType::object("mesh_catalog::Detail"),
            true,
        )],
    );
    let create = BundleEndpoint {
        method: "POST".to_owned(),
        has_body: true,
        request: body,
        ..get("create_item", "/items", no_body())
    };
    let detail = BundleType {
        id: "mesh_catalog::Detail".to_owned(),
        shape: shape("Detail", vec![field("color", WireType::string(), true)]),
    };
    let (files, _) = run(&bundle(vec![create], vec![detail]), plain());
    let text = &files["autumn/catalog/catalog.proto"];
    assert!(text.contains("Detail detail = 1"), "{text}");
    assert!(text.contains("message Detail {"), "{text}");
    assert!(!text.contains("google.protobuf.Value"), "{text}");
}

#[test]
fn a_rust_type_named_like_a_wrapper_or_a_request_is_refused() {
    let wrapper_named = shape(
        "Item",
        vec![
            field("first", WireType::object("shop::StringListWrapper"), true),
            field(
                "second",
                WireType::array(WireType::array(WireType::string())),
                true,
            ),
        ],
    );
    let user = BundleType {
        id: "shop::StringListWrapper".to_owned(),
        shape: shape(
            "StringListWrapper",
            vec![field("code", WireType::string(), true)],
        ),
    };
    let b = bundle(vec![get("get_item", "/items", wrapper_named)], vec![user]);
    let err = export(&b, &Lock::default(), plain()).expect_err("refused");
    assert!(err.contains("`StringListWrapper`"), "{err}");

    let request_named = shape(
        "Item",
        vec![field("x", WireType::object("shop::GetItemRequest"), true)],
    );
    let user = BundleType {
        id: "shop::GetItemRequest".to_owned(),
        shape: shape("GetItemRequest", vec![field("q", WireType::string(), true)]),
    };
    let b = bundle(vec![get("get_item", "/items", request_named)], vec![user]);
    let err = export(&b, &Lock::default(), plain()).expect_err("refused");
    assert!(err.contains("`GetItemRequest`"), "{err}");
}

fn annotated() -> ProtoOptions {
    ProtoOptions {
        http_annotations: true,
        ..plain()
    }
}

#[test]
fn an_http_template_names_the_proto_field_of_each_path_parameter() {
    let b = bundle(
        vec![get(
            "get_file",
            "/users/{userId}/files/{*rest}",
            shape("Item", vec![field("id", WireType::string(), true)]),
        )],
        vec![],
    );
    let (files, _) = run(&b, annotated());
    let text = &files["autumn/catalog/catalog.proto"];
    assert!(
        text.contains("get: \"/users/{user_id}/files/{rest=**}\""),
        "{text}"
    );
    assert!(
        text.contains("string user_id = 1 [json_name = \"userId\"]"),
        "{text}"
    );
}

#[test]
fn a_body_field_that_is_also_a_path_parameter_keeps_the_body_whole() {
    let body = shape(
        "Patch",
        vec![
            field("id", WireType::string(), true),
            field("name", WireType::string(), true),
        ],
    );
    let patch = BundleEndpoint {
        method: "PATCH".to_owned(),
        has_body: true,
        request: body,
        ..get("patch_item", "/items/{id}", no_body())
    };
    let (files, _) = run(&bundle(vec![patch], vec![]), annotated());
    let text = &files["autumn/catalog/catalog.proto"];
    assert!(
        text.contains("string id = 1 [json_name = \"id\"]; // path"),
        "{text}"
    );
    assert!(
        text.contains("Patch body = 2 [json_name = \"body\"]; // body"),
        "{text}"
    );
    assert!(text.contains("message Patch {"), "{text}");
    assert!(text.contains("body: \"body\""), "{text}");
}

#[test]
fn a_get_with_a_body_has_no_body_in_its_http_rule() {
    let search = BundleEndpoint {
        has_body: true,
        request: shape("Query", vec![field("q", WireType::string(), true)]),
        ..get("search", "/search", no_body())
    };
    let (files, _) = run(&bundle(vec![search], vec![]), annotated());
    let text = &files["autumn/catalog/catalog.proto"];
    assert!(!text.contains("body: \"*\""), "{text}");
    assert!(
        text.contains("google.api.http has no body for GET"),
        "{text}"
    );
}

#[test]
fn a_list_response_names_the_wrapper_field_as_the_response_body() {
    let item = shape("Item", vec![field("id", WireType::string(), true)]);
    let list = BundleEndpoint {
        response_list: true,
        ..get("list_items", "/items", item)
    };
    let (files, _) = run(&bundle(vec![list], vec![]), annotated());
    let text = &files["autumn/catalog/catalog.proto"];
    assert!(text.contains("response_body: \"values\""), "{text}");
}

#[test]
fn a_unit_response_says_that_autumn_sends_null() {
    let unit = WireTypeDescriptor {
        name: "Done".to_owned(),
        unit: true,
        ..WireTypeDescriptor::default()
    };
    let (files, _) = run(&bundle(vec![get("ping", "/ping", unit)], vec![]), plain());
    let text = &files["autumn/catalog/catalog.proto"];
    assert!(text.contains("returns (google.protobuf.Empty)"), "{text}");
    assert!(text.contains("Autumn sends JSON null"), "{text}");
}

#[test]
fn a_nullable_value_field_is_a_plain_field() {
    let item = shape(
        "Item",
        vec![
            field(
                "big",
                WireType::nullable(WireType::integer(128, false)),
                false,
            ),
            field(
                "other",
                WireType::nullable(WireType::object("not::Here")),
                false,
            ),
        ],
    );
    let (files, _) = run(&bundle(vec![get("get_item", "/i", item)], vec![]), plain());
    let text = &files["autumn/catalog/catalog.proto"];
    assert!(!text.contains("optional google.protobuf.Value"), "{text}");
}

#[test]
fn two_body_types_with_one_name_and_two_shapes_are_refused() {
    let a = shape("Item", vec![field("a", WireType::string(), true)]);
    let b_shape = shape("Item", vec![field("b", WireType::string(), true)]);
    let b = bundle(
        vec![get("get_a", "/a", a), get("get_b", "/b", b_shape)],
        vec![],
    );
    let err = export(&b, &Lock::default(), plain()).expect_err("refused");
    assert!(err.contains("message `MeshCatalogItem`"), "{err}");
}

#[test]
fn two_services_in_one_package_are_refused() {
    let item = shape("Item", vec![field("id", WireType::string(), true)]);
    let one = BundleEndpoint {
        service: "order-api".to_owned(),
        ..get("a", "/a", item.clone())
    };
    let two = BundleEndpoint {
        service: "order_api".to_owned(),
        ..get("b", "/b", item)
    };
    let err =
        export(&bundle(vec![one, two], vec![]), &Lock::default(), plain()).expect_err("refused");
    assert!(err.contains("autumn.order_api"), "{err}");
}

#[test]
fn removing_a_field_does_not_rename_a_live_field() {
    let mut camel = field("item_id_camel", WireType::string(), true);
    camel.wire_name = "itemId".to_owned();
    let both = bundle(
        vec![get(
            "get_item",
            "/i",
            shape(
                "Item",
                vec![field("item_id", WireType::string(), true), camel],
            ),
        )],
        vec![],
    );
    let one = bundle(
        vec![get(
            "get_item",
            "/i",
            shape("Item", vec![field("item_id", WireType::string(), true)]),
        )],
        vec![],
    );
    let first = export(&both, &Lock::default(), plain()).expect("exports");
    let second = export(&one, &first.lock, plain()).expect("exports");
    let text = &second.files["autumn/catalog/catalog.proto"];
    assert!(
        text.contains("string item_id = 1 [json_name = \"item_id\"]"),
        "{text}"
    );
    assert!(text.contains("reserved 2;"), "{text}");
    assert!(!text.contains("reserved \"item_id\""), "{text}");
}

#[test]
fn a_dedup_suffix_never_clashes_with_another_field() {
    let mut upper = field("upper", WireType::string(), true);
    upper.wire_name = "A".to_owned();
    let item = shape(
        "Item",
        vec![
            field("a", WireType::string(), true),
            field("a_3", WireType::string(), true),
            upper,
        ],
    );
    let (files, _) = run(&bundle(vec![get("get_item", "/i", item)], vec![]), plain());
    let text = &files["autumn/catalog/catalog.proto"];
    let names: Vec<&str> = text
        .lines()
        .filter(|l| l.contains("[json_name"))
        .filter_map(|l| l.split_whitespace().nth(1))
        .collect();
    let distinct: std::collections::BTreeSet<&&str> = names.iter().collect();
    assert_eq!(distinct.len(), names.len(), "{text}");
}
