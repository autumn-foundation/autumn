//! End-to-end MCP tool derivation for `#[repository(..., mcp)]` CRUD routes.
//!
//! DB-free: instead of mounting a live app (the generated handlers need a
//! pool at request time), these tests collect the generated routes' `ApiDoc`
//! metadata and run it through the same production entry point the router
//! uses when assembling the `/mcp` endpoint
//! (`autumn_web::mcp::derive_tools`, cf. `router.rs`).
//!
//! Covers the acceptance criteria:
//! * Bare `mcp` derives all five CRUD tools, including the 204 No Content
//!   DELETE (readOnly/destructive safety annotations track the verb).
//! * Tool input schemas reflect the typed contract (path `id`, JSON `body`).
//! * `mcp = "read"` derives only the list/get tools.

#![cfg(all(feature = "db", feature = "mcp"))]

use autumn_web::mcp::derive_tools;

mod schema {
    autumn_web::reexports::diesel::table! {
        gears (id) {
            id -> Int8,
            name -> Text,
        }
    }
}

use schema::gears;

#[autumn_web::model]
pub struct Gear {
    #[id]
    pub id: i64,
    pub name: String,
}

#[autumn_web::repository(Gear, api = "/api/gears", mcp)]
pub trait GearRepository {}

mod schema2 {
    autumn_web::reexports::diesel::table! {
        pulleys (id) {
            id -> Int8,
            name -> Text,
        }
    }
}

use schema2::pulleys;

#[autumn_web::model]
pub struct Pulley {
    #[id]
    pub id: i64,
    pub name: String,
}

#[autumn_web::repository(Pulley, api = "/api/pulleys", mcp = "read")]
pub trait PulleyRepository {}

fn gear_docs() -> Vec<autumn_web::openapi::ApiDoc> {
    vec![
        __autumn_route_info_gear_api_list().api_doc,
        __autumn_route_info_gear_api_get().api_doc,
        __autumn_route_info_gear_api_create().api_doc,
        __autumn_route_info_gear_api_update().api_doc,
        __autumn_route_info_gear_api_delete().api_doc,
    ]
}

#[test]
fn repository_mcp_derives_five_tools_with_verb_annotations() {
    let tools = derive_tools(&gear_docs(), false, None);
    let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
    assert_eq!(
        names,
        vec![
            "gear_api_list",
            "gear_api_get",
            "gear_api_create",
            "gear_api_update",
            "gear_api_delete"
        ],
        "bare `mcp` must derive all five CRUD tools"
    );

    let by = |n: &str| tools.iter().find(|t| t.name() == n).unwrap();
    assert_eq!(by("gear_api_list").annotations()["readOnlyHint"], true);
    assert_eq!(by("gear_api_get").annotations()["readOnlyHint"], true);
    assert_eq!(by("gear_api_create").annotations()["readOnlyHint"], false);
    // The 204 No Content DELETE survives derivation and carries the
    // destructive hint.
    let delete = by("gear_api_delete");
    assert_eq!(delete.annotations()["readOnlyHint"], false);
    assert_eq!(delete.annotations()["destructiveHint"], true);
    assert_eq!(delete.method(), "DELETE");
    assert_eq!(delete.path_template(), "/api/gears/{id}");
}

#[test]
fn repository_mcp_input_schemas_reflect_typed_contract() {
    let tools = derive_tools(&gear_docs(), false, None);
    let by = |n: &str| tools.iter().find(|t| t.name() == n).unwrap();

    // list: no params at all — an empty-properties object.
    let list = &by("gear_api_list").input_schema();
    assert_eq!(list["type"], "object");
    assert!(
        list["properties"]
            .as_object()
            .is_none_or(|p| !p.contains_key("id") && !p.contains_key("body")),
        "list takes neither id nor body: {list}"
    );

    // get/delete: required string `id` path param.
    for op in ["gear_api_get", "gear_api_delete"] {
        let schema = &by(op).input_schema();
        assert_eq!(schema["properties"]["id"]["type"], "string", "{op}");
        assert!(
            schema["required"]
                .as_array()
                .is_some_and(|r| r.iter().any(|v| v == "id")),
            "{op} must require id: {schema}"
        );
    }

    // create/update: required `body` object derived from NewGear/UpdateGear.
    for op in ["gear_api_create", "gear_api_update"] {
        let schema = &by(op).input_schema();
        assert!(
            schema["properties"]["body"].is_object(),
            "{op} must take a body: {schema}"
        );
        assert!(
            schema["required"]
                .as_array()
                .is_some_and(|r| r.iter().any(|v| v == "body")),
            "{op} must require body: {schema}"
        );
    }
}

#[test]
fn repository_mcp_read_derives_only_list_and_get() {
    let docs = vec![
        __autumn_route_info_pulley_api_list().api_doc,
        __autumn_route_info_pulley_api_get().api_doc,
        __autumn_route_info_pulley_api_create().api_doc,
        __autumn_route_info_pulley_api_update().api_doc,
        __autumn_route_info_pulley_api_delete().api_doc,
    ];
    let tools = derive_tools(&docs, false, None);
    let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
    assert_eq!(
        names,
        vec!["pulley_api_list", "pulley_api_get"],
        "`mcp = \"read\"` must derive only the read tools"
    );
}
