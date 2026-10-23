//! Drives `search_tools` (progressive MCP tool discovery, #3337) through the
//! production `/mcp` endpoint over a realistic catalog: 300 tools, spread
//! over 30 resources, each with a summary, a tag and path parameters.
//!
//! Every round sends a two-term query (`refund invoice`) and a one-term
//! prefix query (`report`), the shapes an agent sends when it looks for a
//! tool. Like the other benches here it is `harness = false` and only
//! sanity-checks that the searches find something.
//!
//! ```sh
//! cargo build --release -p autumn-web --features mcp --bench mcp_search
//! BIN=$(find target/release/deps -maxdepth 1 -name "mcp_search-*" -type f ! -name "*.d")
//! valgrind --tool=callgrind --callgrind-out-file=cg0.out    "$BIN" --iterations 0
//! valgrind --tool=callgrind --callgrind-out-file=cg1000.out "$BIN" --iterations 1000
//! callgrind_annotate --threshold=90 cg1000.out | head -40
//! # marginal Ir/round = (cg1000 total - cg0 total) / 1000
//! valgrind --tool=dhat --dhat-out-file=dh0.json   "$BIN" --iterations 0
//! valgrind --tool=dhat --dhat-out-file=dh200.json "$BIN" --iterations 200
//! ```

use autumn_web::mcp::McpCatalog;
use autumn_web::openapi::{ApiDoc, SchemaEntry, SchemaKind};
use autumn_web::test::TestApp;
use autumn_web::{Route, RouteIdempotency, RouteTimeout};
use serde_json::{Value, json};

const RESOURCES: [&str; 30] = [
    "invoice",
    "refund",
    "customer",
    "report",
    "article",
    "ticket",
    "account",
    "payment",
    "subscription",
    "plan",
    "coupon",
    "order",
    "shipment",
    "product",
    "category",
    "review",
    "comment",
    "user",
    "team",
    "project",
    "task",
    "milestone",
    "document",
    "template",
    "webhook",
    "audit",
    "export",
    "import",
    "notification",
    "setting",
];
const ACTIONS: [&str; 10] = [
    "list", "get", "create", "update", "delete", "archive", "restore", "search", "export", "count",
];

fn leak(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

fn route(resource: &'static str, action: &'static str) -> Route {
    let path = leak(format!("/api/{resource}s/{action}"));
    let op = leak(format!("{action}_{resource}"));
    let api_doc = ApiDoc {
        method: "GET",
        path,
        operation_id: op,
        summary: Some(leak(format!(
            "{action} the {resource} records that belong to the current tenant"
        ))),
        tags: leak_tags(resource),
        response: Some(SchemaEntry {
            name: "Item",
            kind: SchemaKind::Ref,
            identity: None,
        }),
        mcp_tool: true,
        ..ApiDoc::default()
    };
    Route {
        method: http::Method::GET,
        path,
        handler: axum::routing::any(|| async { "{}" }),
        name: op,
        api_doc,
        repository: None,
        idempotency: RouteIdempotency::Direct,
        timeout: RouteTimeout::Inherit,
        criticality: autumn_web::admission::Criticality::Default,
        seo: autumn_web::seo::SeoRouteDefaults::EMPTY,
        api_version: None,
        sunset_opt_out: false,
    }
}

fn leak_tags(tag: &'static str) -> &'static [&'static str] {
    Box::leak(vec![tag].into_boxed_slice())
}

fn main() {
    let mut iterations: u32 = 1_000;
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < raw.len() {
        match raw[i].as_str() {
            "--iterations" => {
                let v = raw.get(i + 1).expect("--iterations requires a value");
                iterations = v
                    .parse()
                    .unwrap_or_else(|e| panic!("bad --iterations {v:?}: {e}"));
                i += 2;
            }
            other => panic!("unrecognized argument {other:?}; accepts --iterations <N>"),
        }
    }

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let routes: Vec<Route> = RESOURCES
        .iter()
        .flat_map(|r| ACTIONS.iter().map(move |a| route(r, a)))
        .collect();
    let client = TestApp::new()
        .routes(routes)
        .mcp_catalog(McpCatalog::Progressive)
        .mount_mcp("/mcp")
        .build();

    let search = |query: &'static str| {
        let client = &client;
        async move {
            let resp = client
                .post("/mcp")
                .json(&json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
                    "params":{"name":"search_tools","arguments":{"query":query}}}))
                .send()
                .await;
            resp.assert_ok();
            resp.json::<Value>()
        }
    };

    rt.block_on(async {
        for _ in 0..20 {
            let _ = search("refund invoice").await;
        }
        let out = search("refund invoice").await;
        let total = out["result"]["structuredContent"]["total"]
            .as_u64()
            .unwrap_or(0);
        assert!(total > 0, "search found nothing: {out}");
        for _ in 0..iterations {
            std::hint::black_box(search("refund invoice").await);
            std::hint::black_box(search("report").await);
        }
    });
}
