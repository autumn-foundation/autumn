//! Structured `Query<T>` decoding — sequences and nested objects (issue #1972).
//!
//! Before this, `Query<T>` delegated to `axum::extract::Query` →
//! `serde_urlencoded`, which is strictly flat: a `Vec<String>` field fed
//! `?tags=a&tags=b` failed with `invalid type: string "a", expected a
//! sequence`, and a nested struct field was unrepresentable by *any* encoding.
//! That made the MCP tool contract unhonorable — `tools/call` dispatch renders
//! an array query argument as repeated keys, which the handler then rejected.
//!
//! These tests pin the decoding contract end to end, through the real
//! extractor, for both the shapes that already worked (flat scalars — parity)
//! and the shapes that did not (sequences, nested objects, arrays of objects).

use autumn_web::prelude::*;
use autumn_web::test::{TestApp, TestClient};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Default)]
struct Filter {
    status: String,
    limit: Option<u32>,
}

#[derive(Serialize, Deserialize)]
struct Item {
    sku: String,
    qty: u32,
}

#[derive(Serialize, Deserialize, Default)]
struct SearchArgs {
    q: String,
    page: Option<u32>,
    tags: Option<Vec<String>>,
    filter: Option<Filter>,
    items: Option<Vec<Item>>,
}

#[get("/search")]
async fn search(Query(args): Query<SearchArgs>) -> AutumnResult<Json<SearchArgs>> {
    Ok(Json(args))
}

fn client() -> TestClient {
    TestApp::new().routes(routes![search]).build()
}

/// Parity: the flat scalar shape `serde_urlencoded` already handled must decode
/// exactly as before — string passthrough plus integer coercion from text.
#[tokio::test]
async fn flat_scalars_still_decode() {
    let resp = client().get("/search?q=foo&page=2").send().await;
    resp.assert_ok();
    let out: serde_json::Value = resp.json();
    assert_eq!(out["q"], "foo");
    assert_eq!(out["page"], 2);
    assert!(out["tags"].is_null(), "absent field stays None: {out}");
}

/// An absent optional field is `None`, and an entirely empty query string is
/// still a valid decode for an all-defaultable struct.
#[tokio::test]
async fn absent_optionals_are_none() {
    let resp = client().get("/search?q=").send().await;
    resp.assert_ok();
    let out: serde_json::Value = resp.json();
    assert_eq!(out["q"], "");
    assert!(out["page"].is_null());
    assert!(out["filter"].is_null());
}

/// Repeated keys collect into a sequence — the `OpenAPI` `form`/`explode` shape
/// MCP dispatch already emitted but the handler could not previously decode.
#[tokio::test]
async fn repeated_keys_decode_into_a_sequence() {
    let resp = client().get("/search?q=x&tags=a&tags=b").send().await;
    resp.assert_ok();
    let out: serde_json::Value = resp.json();
    assert_eq!(out["tags"], serde_json::json!(["a", "b"]));
}

/// A single repeated-key occurrence is still a one-element sequence, so a
/// client need not know the arity up front.
#[tokio::test]
async fn single_occurrence_decodes_into_a_one_element_sequence() {
    let resp = client().get("/search?q=x&tags=only").send().await;
    resp.assert_ok();
    let out: serde_json::Value = resp.json();
    assert_eq!(out["tags"], serde_json::json!(["only"]));
}

/// The explicit empty-bracket append form (`tags[]=a&tags[]=b`).
#[tokio::test]
async fn empty_bracket_append_form_decodes_into_a_sequence() {
    let resp = client().get("/search?q=x&tags[]=a&tags[]=b").send().await;
    resp.assert_ok();
    let out: serde_json::Value = resp.json();
    assert_eq!(out["tags"], serde_json::json!(["a", "b"]));
}

/// Explicit indices, including out-of-order and gapped ones, decode in
/// ascending index order — the same compaction `nested_form` applies to
/// `items[0]` / `items[2]` after client-side row removal.
#[tokio::test]
async fn indexed_form_decodes_in_ascending_index_order() {
    let resp = client().get("/search?q=x&tags[2]=c&tags[0]=a").send().await;
    resp.assert_ok();
    let out: serde_json::Value = resp.json();
    assert_eq!(out["tags"], serde_json::json!(["a", "c"]));
}

/// A nested object field decodes from the bracketed form — the shape the issue
/// reports as unrepresentable, which forced builders onto JSON-in-a-string.
#[tokio::test]
async fn nested_object_field_decodes() {
    let resp = client()
        .get("/search?q=x&filter[status]=open&filter[limit]=5")
        .send()
        .await;
    resp.assert_ok();
    let out: serde_json::Value = resp.json();
    assert_eq!(out["filter"]["status"], "open");
    assert_eq!(out["filter"]["limit"], 5);
}

/// An array of objects uses the same `items[i][field]` wire format the
/// `NestedChangesetForm` has always used, so the framework speaks one dialect.
#[tokio::test]
async fn array_of_objects_decodes() {
    let resp = client()
        .get("/search?q=x&items[0][sku]=A-1&items[0][qty]=2&items[1][sku]=B-2&items[1][qty]=3")
        .send()
        .await;
    resp.assert_ok();
    let out: serde_json::Value = resp.json();
    assert_eq!(out["items"][0]["sku"], "A-1");
    assert_eq!(out["items"][0]["qty"], 2);
    assert_eq!(out["items"][1]["sku"], "B-2");
    assert_eq!(out["items"][1]["qty"], 3);
}

/// Percent-encoded brackets are the same wire form: `form_urlencoded` decodes
/// `%5B`/`%5D` before the path is parsed, so a client that encodes them (as
/// `serde_urlencoded::to_string` does) round-trips identically.
#[tokio::test]
async fn percent_encoded_brackets_are_equivalent() {
    let resp = client()
        .get("/search?q=x&filter%5Bstatus%5D=open")
        .send()
        .await;
    resp.assert_ok();
    let out: serde_json::Value = resp.json();
    assert_eq!(out["filter"]["status"], "open");
}

/// A malformed bracket key is not a parse failure: it stays a literal key, and
/// an unknown literal key is ignored like any other unknown query parameter.
#[tokio::test]
async fn malformed_bracket_keys_stay_literal_and_are_ignored() {
    let resp = client().get("/search?q=x&weird[unclosed=1").send().await;
    resp.assert_ok();
    let out: serde_json::Value = resp.json();
    assert_eq!(out["q"], "x");
}

/// A value that cannot coerce to the field's type is still a 400 with the
/// Problem Details contract — the pre-existing behaviour for `?page=nope`.
#[tokio::test]
async fn scalar_coercion_failure_is_a_problem_details_400() {
    let resp = client().get("/search?q=x&page=not-a-number").send().await;
    resp.assert_status(400);
    resp.assert_header_contains("content-type", "application/problem+json");
    let out: serde_json::Value = resp.json();
    assert_eq!(out["code"], "autumn.bad_request");
}

/// A shape conflict (the same key used as both a scalar and a container) is a
/// deterministic 400 rather than a silent last-write-wins.
#[tokio::test]
async fn conflicting_shapes_for_one_key_are_rejected() {
    let resp = client()
        .get("/search?q=x&filter=flat&filter[status]=open")
        .send()
        .await;
    resp.assert_status(400);
}

/// Nesting is depth-capped so a hostile query string cannot drive unbounded
/// recursion during tree construction.
#[tokio::test]
async fn excessive_nesting_depth_is_rejected() {
    let deep = "filter".to_owned() + &"[x]".repeat(64);
    let resp = client().get(&format!("/search?q=x&{deep}=1")).send().await;
    resp.assert_status(400);
}

/// A key the grammar cannot resolve poisons only that key. Junk parameters a
/// handler never reads (ad tracking, crawler noise) stay ignorable, exactly as
/// they were when the bracketed form was merely an unrecognised key name.
#[tokio::test]
async fn unresolvable_keys_the_handler_ignores_do_not_fail_the_request() {
    let resp = client()
        .get("/search?q=x&utm=1&utm[source]=news&junk[a][b][c][d][e][f][g][h][i][j][k][l][m][n][o][p][q]=1")
        .send()
        .await;
    resp.assert_ok();
    let out: serde_json::Value = resp.json();
    assert_eq!(out["q"], "x");
}

/// A key submitted twice for a single-valued field fails closed rather than
/// quietly resolving to one of the two values — the parameter-pollution
/// hardening `serde_urlencoded` + serde's derive provided before.
#[tokio::test]
async fn a_duplicated_scalar_parameter_is_rejected() {
    let resp = client().get("/search?q=first&q=second").send().await;
    resp.assert_status(400);
}

/// A coercion failure must not echo the submitted text: the message lands in
/// the 400 body and in every error reporter, and a query parameter can hold a
/// secret.
#[tokio::test]
async fn a_failed_coercion_does_not_echo_the_value() {
    let resp = client().get("/search?q=x&page=SUPERSECRET").send().await;
    resp.assert_status(400);
    let body: serde_json::Value = resp.json();
    let rendered = body.to_string();
    assert!(
        !rendered.contains("SUPERSECRET"),
        "value must not be reflected: {rendered}"
    );
    assert!(
        rendered.contains("page"),
        "the field path is still named: {rendered}"
    );
}

/// A very large explicit index is accepted as a sparse position rather than
/// preallocating: indices key an ordered map, never a `Vec` of that length.
#[tokio::test]
async fn huge_indices_do_not_preallocate() {
    let resp = client()
        .get("/search?q=x&tags[4000000000]=late&tags[1]=early")
        .send()
        .await;
    resp.assert_ok();
    let out: serde_json::Value = resp.json();
    assert_eq!(out["tags"], serde_json::json!(["early", "late"]));
}
