//! Integration tests for the `react-graphql` example.
//!
//! Two tiers, following the pattern in `docs/guide/testing.md`:
//!
//! - **No Docker** — the page shell, the committed-SDL drift gate, and the
//!   plugin conformance harness. These run in every `cargo test`.
//! - **Docker** — everything that touches rows. `TestDb` starts one shared
//!   Postgres testcontainer per test binary; each test applies the example's
//!   real embedded migration, truncates, and seeds through the repository.
//!   They are `#[ignore]`d and share one table, so run them serially:
//!
//! ```text
//! cargo test -p react-graphql --test graphql_api                                   # tier 1
//! cargo test -p react-graphql --test graphql_api -- --include-ignored --test-threads=1   # both
//! ```

use autumn_web::plugin::Plugin;
use autumn_web::plugin_conformance::{ConformanceConfig, run_conformance};
use autumn_web::test::{TestApp, TestClient, TestDb};
use react_graphql::graphql_plugin::PLUGIN_NAME;
use react_graphql::repositories::{NoteRepository, PgNoteRepository};
use react_graphql::{GRAPHQL_PATH, MIGRATIONS, graphql, notes, seed_notes};
use serde_json::{Value, json};

// ── Tier 1: no database ────────────────────────────────────────────────────

/// The app without a pool: enough for the shell and the plugin's own routes.
fn client_without_db() -> TestClient {
    TestApp::new()
        .routes(react_graphql::routes())
        .plugin(graphql())
        .build()
}

#[tokio::test]
async fn index_serves_the_react_shell() {
    let client = client_without_db();
    let response = client.get("/").send().await;
    response
        .assert_ok()
        .assert_header_contains("content-type", "text/html")
        // React mounts here; the bundle and stylesheet come from the
        // framework's `/static` mount via `asset_url`.
        .assert_selector("#root")
        .assert_attr("script[type=module]", "src", "/static/app/app.js")
        .assert_attr("link[rel=stylesheet]", "href", "/static/app/app.css");
}

#[tokio::test]
async fn sdl_route_serves_the_schema_as_text() {
    let client = client_without_db();
    let response = client.get(&format!("{GRAPHQL_PATH}/sdl")).send().await;
    response
        .assert_ok()
        .assert_header_contains("content-type", "text/plain")
        .assert_body_contains("type Note {")
        .assert_body_contains("createNote(input: NewNoteInput!): Note!");
}

/// `GET` is safe and cacheable by contract; a mutation must not be reachable
/// through it. Refused before any resolver runs — no pool is needed to see it.
#[tokio::test]
async fn a_mutation_over_get_is_refused_with_405() {
    let client = client_without_db();
    let mutation = "mutation%20%7B%20deleteNote(id%3A%201)%20%7D";
    let response = client
        .get(&format!("{GRAPHQL_PATH}?query={mutation}"))
        .send()
        .await;
    response
        .assert_status(405)
        .assert_body_contains("mutation operations are not allowed over GET");

    // A named operation is selected by `operationName`, same rule.
    let doc = "query%20A%20%7B%20notes%20%7B%20id%20%7D%20%7D%20mutation%20B%20%7B%20deleteNote(id%3A%201)%20%7D";
    client
        .get(&format!("{GRAPHQL_PATH}?query={doc}&operationName=B"))
        .send()
        .await
        .assert_status(405);
    // ...and the query in the same document is still fine (fails later on the
    // missing pool, as a redacted GraphQL error, not at the transport).
    client
        .get(&format!("{GRAPHQL_PATH}?query={doc}&operationName=A"))
        .send()
        .await
        .assert_ok();
}

/// Two plugins built from the same root types can be mounted at two paths:
/// each router carries its own schema, so neither overwrites the other.
#[tokio::test]
async fn two_plugins_coexist_at_different_paths() {
    let client = TestApp::new()
        .plugin(graphql())
        .plugin(graphql().path("/graphql-v2").without_sdl())
        .build();

    client.get("/graphql/sdl").send().await.assert_ok();
    client
        .get("/graphql-v2/sdl")
        .send()
        .await
        .assert_status(404);
    for path in ["/graphql", "/graphql-v2"] {
        let body = gql_at(&client, path, "{ __typename }").await;
        assert_eq!(body["data"]["__typename"], "Query", "at {path}: {body}");
    }
}

#[tokio::test]
async fn a_get_without_a_query_string_is_a_400() {
    client_without_db()
        .get(GRAPHQL_PATH)
        .send()
        .await
        .assert_status(400);
}

/// Without a pool, resolvers fail as a GraphQL error on the field, not as a
/// 500 — the transport stays healthy. And because a GraphQL response is an
/// HTTP 200 that never meets the framework's problem-details redaction, the
/// resolver's error mapper redacts server-side (`5xx`) messages itself: the
/// client sees a generic message plus the status, never the detail.
#[tokio::test]
async fn a_missing_pool_is_a_redacted_graphql_error() {
    let body = gql(&client_without_db(), "{ notes { id } }", json!({})).await;
    let error = &body["errors"][0];
    assert_eq!(error["message"], "internal server error", "got: {body}");
    assert_eq!(error["extensions"]["status"], 503, "got: {body}");
    assert!(
        !body.to_string().contains("pool"),
        "server-side detail must not reach the wire: {body}"
    );
}

/// `schema.graphql` is the contract the TypeScript client is written against.
/// This keeps the committed file in step with what the server actually
/// serves; on a mismatch, regenerate it from a running server:
///
/// ```text
/// curl -s 127.0.0.1:3000/graphql/sdl > examples/react-graphql/schema.graphql
/// ```
#[test]
fn committed_schema_matches_the_live_sdl() {
    let live = notes::build_schema().sdl();
    let committed = include_str!("../schema.graphql");
    assert_eq!(
        live.trim(),
        committed.trim(),
        "schema.graphql is stale — regenerate it with \
         `curl -s 127.0.0.1:3000/graphql/sdl > examples/react-graphql/schema.graphql`\n\
         --- live SDL ---\n{live}"
    );
}

/// The plugin passes the framework's own plugin conformance harness: its
/// routes are attributed to it, live under its prefix, collide with nothing,
/// and its contract names no experimental surface.
#[test]
fn graphql_plugin_passes_plugin_conformance() {
    let plugin = graphql();
    let plugin_name = plugin.name().into_owned();
    let contract = plugin.contract().expect("plugin declares a contract");
    assert_eq!(contract.plugin, PLUGIN_NAME);

    let routes = autumn_web::app()
        .plugin(plugin)
        .plugin_route_infos()
        .expect("route manifest");

    let config = ConformanceConfig::new(&plugin_name)
        .prefix(GRAPHQL_PATH)
        .contract(contract);
    let report = run_conformance(&config, &routes);
    assert!(report.passed(), "{}", report.to_text_report());

    let mut declared: Vec<String> = routes
        .iter()
        .filter(|r| matches!(&r.source, autumn_web::route_listing::RouteSource::Plugin(n) if *n == plugin_name))
        .map(|r| format!("{} {}", r.method, r.path))
        .collect();
    declared.sort();
    assert_eq!(
        declared,
        ["GET /graphql", "GET /graphql/sdl", "POST /graphql"],
        "the plugin declares exactly the routes it mounts"
    );
}

// ── Tier 2: Postgres testcontainer ─────────────────────────────────────────

/// A migrated, truncated, seeded `notes` table and a client over it.
///
/// The migration is the example's real embedded one, applied through
/// `autumn_web::migrate::run_pending` — the same code path `cargo run` uses
/// on boot — so a test never drifts from what the app actually creates.
async fn seeded_client() -> TestClient {
    let db = TestDb::shared().await;
    let url = db.url().to_owned();
    tokio::task::spawn_blocking(move || autumn_web::migrate::run_pending(&url, MIGRATIONS))
        .await
        .expect("migration task")
        .expect("apply the notes migration");
    db.execute_sql("TRUNCATE notes RESTART IDENTITY").await;

    // Seed through the repository (hooks + validation), exactly as boot does.
    let repo = PgNoteRepository::with_pool_untracked(db.pool());
    repo.save_many(&seed_notes()).await.expect("seed");

    TestApp::new()
        .routes(react_graphql::routes())
        .plugin(graphql())
        .with_db(db.pool())
        .build()
}

/// POST one operation and return the parsed response body.
async fn gql(client: &TestClient, query: &str, variables: Value) -> Value {
    let response = client
        .post(GRAPHQL_PATH)
        .json(&json!({ "query": query, "variables": variables }))
        .send()
        .await;
    response.assert_ok();
    response.json()
}

/// Same, against an arbitrary mount path and no variables.
async fn gql_at(client: &TestClient, path: &str, query: &str) -> Value {
    let response = client
        .post(path)
        .json(&json!({ "query": query }))
        .send()
        .await;
    response.assert_ok();
    response.json()
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn query_lists_the_seeded_notes_newest_first() {
    let client = seeded_client().await;
    let body = gql(&client, "{ notes { id title pinned } }", json!({})).await;

    assert!(body.get("errors").is_none(), "unexpected errors: {body}");
    let notes = body["data"]["notes"].as_array().expect("notes array");
    assert_eq!(notes.len(), 2, "two seeded notes: {body}");
    assert_eq!(notes[0]["title"], "Welcome to Autumn Notes");
    assert_eq!(notes[0]["pinned"], true);
    assert_eq!(notes[1]["title"], "Try the GraphQL endpoint");
    assert_eq!(notes[1]["pinned"], false);
}

/// The generated REST handler and the GraphQL resolver read the same rows
/// through the same repository.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn rest_and_graphql_see_the_same_rows() {
    let client = seeded_client().await;

    // The generated list handler returns a `Page` envelope; the rows are
    // under `content`.
    let rest: Value = client.get("/api/notes").send().await.assert_ok().json();
    let rest = rest["content"].as_array().expect("page content").clone();
    let graphql = gql(&client, "{ notes { id title } }", json!({})).await;
    let graphql = graphql["data"]["notes"].as_array().expect("array").clone();

    let mut rest_ids: Vec<i64> = rest.iter().map(|n| n["id"].as_i64().expect("id")).collect();
    let mut graphql_ids: Vec<i64> = graphql
        .iter()
        .map(|n| n["id"].as_i64().expect("id"))
        .collect();
    rest_ids.sort_unstable();
    graphql_ids.sort_unstable();
    assert_eq!(rest_ids, graphql_ids);
    assert_eq!(rest_ids, vec![1, 2]);

    client
        .get("/api/notes/2")
        .send()
        .await
        .assert_ok()
        .assert_json::<Value, _>(|note| assert_eq!(note["title"], "Welcome to Autumn Notes"));
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn mutations_round_trip_through_the_repository() {
    let client = seeded_client().await;

    // `#[normalize(trim)]` runs before the write; the returned row is what
    // Postgres stored.
    let created = gql(
        &client,
        "mutation($input: NewNoteInput!) { createNote(input: $input) { id title body pinned createdAt } }",
        json!({ "input": { "title": "  Buy apples  ", "body": "  Honeycrisp " } }),
    )
    .await;
    let note = &created["data"]["createNote"];
    assert_eq!(
        note["title"], "Buy apples",
        "title is trimmed by the hook: {created}"
    );
    assert_eq!(note["body"], "Honeycrisp");
    assert_eq!(note["pinned"], false);
    let id = note["id"].as_i64().expect("id");
    assert_eq!(id, 3, "BIGSERIAL continues after the two seeds");
    assert!(
        note["createdAt"].as_str().is_some_and(|s| s.contains('T')),
        "createdAt is RFC 3339: {created}"
    );

    // The toggle runs under `with_lock` (row lock + transaction) and touches
    // only `pinned`; every other column comes back unchanged.
    let toggled = gql(
        &client,
        "mutation($id: Int!) { togglePinned(id: $id) { id title pinned } }",
        json!({ "id": id }),
    )
    .await;
    assert_eq!(
        toggled["data"]["togglePinned"]["pinned"], true,
        "toggle: {toggled}"
    );
    assert_eq!(toggled["data"]["togglePinned"]["title"], "Buy apples");

    // The derived `find_by_pinned` finder, newest first.
    let pinned = gql(&client, "{ notes(pinnedOnly: true) { id } }", json!({})).await;
    let ids: Vec<i64> = pinned["data"]["notes"]
        .as_array()
        .expect("array")
        .iter()
        .map(|n| n["id"].as_i64().expect("id"))
        .collect();
    assert_eq!(ids, vec![id, 2], "pinned only, newest first: {pinned}");

    // It is pinned now, so unpin it first (the `before_delete` hook would
    // refuse otherwise — see `deleting_a_pinned_note_is_refused_by_the_hook`).
    gql(
        &client,
        "mutation($id: Int!) { togglePinned(id: $id) { pinned } }",
        json!({ "id": id }),
    )
    .await;
    let deleted = gql(
        &client,
        "mutation($id: Int!) { deleteNote(id: $id) }",
        json!({ "id": id }),
    )
    .await;
    assert_eq!(deleted["data"]["deleteNote"], true, "delete: {deleted}");
    let again = gql(
        &client,
        "mutation($id: Int!) { deleteNote(id: $id) }",
        json!({ "id": id }),
    )
    .await;
    assert_eq!(
        again["data"]["deleteNote"], false,
        "second delete finds nothing"
    );

    let missing = gql(
        &client,
        "query($id: Int!) { note(id: $id) { id } }",
        json!({ "id": id }),
    )
    .await;
    assert_eq!(missing["data"]["note"], Value::Null);
}

/// The model's `#[validate(length(min = 1))]` runs inside `repo.save`, after
/// `#[normalize(trim)]` has reduced the title to `""`, and surfaces as a
/// GraphQL field error — not a 500, and not a rule the resolver restated.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_blank_title_is_rejected_by_model_validation() {
    let client = seeded_client().await;
    let body = gql(
        &client,
        "mutation { createNote(input: { title: \"   \" }) { id } }",
        json!({}),
    )
    .await;
    let message = body["errors"][0]["message"]
        .as_str()
        .unwrap_or_else(|| panic!("expected a validation error, got: {body}"));
    assert!(
        message.starts_with("title: ") && message.contains("1–120"),
        "validation message from the model: {body}"
    );
    assert_eq!(body["data"], Value::Null);

    let count = gql(&client, "{ notes { id } }", json!({})).await;
    assert_eq!(
        count["data"]["notes"].as_array().expect("array").len(),
        2,
        "nothing was inserted"
    );
}

/// The `before_delete` hook guards every door: the seeded welcome note is
/// pinned, so `deleteNote` is refused until it is unpinned.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn deleting_a_pinned_note_is_refused_by_the_hook() {
    let client = seeded_client().await;

    let refused = gql(&client, "mutation { deleteNote(id: 2) }", json!({})).await;
    let message = refused["errors"][0]["message"]
        .as_str()
        .unwrap_or_else(|| panic!("expected a hook error, got: {refused}"));
    assert!(message.contains("pinned"), "got: {refused}");
    assert_eq!(
        refused["errors"][0]["extensions"]["status"], 422,
        "status travels in extensions: {refused}"
    );

    let still_there = gql(&client, "{ note(id: 2) { id pinned } }", json!({})).await;
    assert_eq!(
        still_there["data"]["note"]["pinned"], true,
        "rolled back: {still_there}"
    );

    gql(
        &client,
        "mutation { togglePinned(id: 2) { pinned } }",
        json!({}),
    )
    .await;
    let deleted = gql(&client, "mutation { deleteNote(id: 2) }", json!({})).await;
    assert_eq!(
        deleted["data"]["deleteNote"], true,
        "unpinned, so deletable: {deleted}"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn unknown_note_ids_are_graphql_errors() {
    let client = seeded_client().await;
    let body = gql(
        &client,
        "mutation { togglePinned(id: 9999) { id } }",
        json!({}),
    )
    .await;
    let error = &body["errors"][0];
    let message = error["message"].as_str().expect("error message");
    assert!(message.contains("9999"), "got: {body}");
    // `with_lock` refuses a missing row with a 404 `AutumnError`, and the
    // adapter carries that status like any other.
    assert_eq!(error["extensions"]["status"], 404, "got: {body}");
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn get_form_serves_reads_from_the_query_string() {
    let client = seeded_client().await;
    let response = client
        .get(&format!(
            "{GRAPHQL_PATH}?query=%7B%20notes%20%7B%20title%20%7D%20%7D"
        ))
        .send()
        .await;
    response.assert_ok();
    let body: Value = response.json();
    assert_eq!(body["data"]["notes"][0]["title"], "Welcome to Autumn Notes");
}
