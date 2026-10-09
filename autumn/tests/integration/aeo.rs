//! Agent readiness (AEO) wiring: Markdown negotiation, homepage `Link`
//! headers, and the generated documents served from the router fallback.

use autumn_web::config::AutumnConfig;
use autumn_web::prelude::*;
use autumn_web::reexports::axum::response::Html;
use autumn_web::test::{TestApp, TestClient};

#[get("/", seo(title = "Home", description = "Start here"))]
async fn home() -> Markup {
    html! {
        (maud::DOCTYPE)
        html {
            head { title { "Home" } meta name="description" content="Start here"; }
            body {
                nav { a href="/about" { "About" } }
                main { h1 { "Welcome" } p { "Read " a href="/about" { "about us" } "." } }
            }
        }
    }
}

#[get("/about", seo(title = "About", description = "Who we are"))]
async fn about() -> Markup {
    html! { html { body { main { h1 { "About" } } } } }
}

#[get("/tagged")]
async fn tagged() -> impl IntoResponse {
    (
        [
            ("etag", "\"v1\""),
            ("last-modified", "Wed, 21 Oct 2015 07:28:00 GMT"),
        ],
        Html("<p>tagged</p>"),
    )
}

#[get("/big")]
async fn big() -> Html<String> {
    Html(format!("<p>{}</p>", "x".repeat(4096)))
}

#[get("/data")]
async fn data() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "ok": true }))
}

#[get("/llms.txt")]
async fn custom_llms() -> &'static str {
    "# Custom\n"
}

fn client() -> TestClient {
    TestApp::new()
        .routes(routes![home, about, tagged, big, data])
        .build()
}

fn client_with(config: AutumnConfig) -> TestClient {
    TestApp::new()
        .config(config)
        .routes(routes![home, about, tagged, big, data])
        .build()
}

fn vary_has_accept(value: Option<&str>) -> bool {
    value.is_some_and(|v| {
        v.split(',')
            .any(|p| p.trim().eq_ignore_ascii_case("accept"))
    })
}

// ── Markdown negotiation ─────────────────────────────────────────────────

#[tokio::test]
async fn accept_markdown_returns_markdown() {
    let res = client()
        .get("/")
        .header("accept", "text/markdown")
        .send()
        .await;
    res.assert_ok();
    assert_eq!(
        res.header("content-type"),
        Some("text/markdown; charset=utf-8")
    );
    assert!(
        vary_has_accept(res.header("vary")),
        "{:?}",
        res.header("vary")
    );
    let tokens: usize = res.header("x-markdown-tokens").unwrap().parse().unwrap();
    assert!(tokens > 0);
    assert_eq!(
        res.header("content-signal"),
        Some("search=yes, ai-input=yes, ai-train=no")
    );
    let body = res.text();
    assert!(body.starts_with("---\ntitle: \"Home\"\n"), "{body}");
    assert!(body.contains("# Welcome"), "{body}");
    assert!(body.contains("[about us](/about)"), "{body}");
    assert!(!body.contains("<main>"), "{body}");
}

#[tokio::test]
async fn browsers_still_get_html_with_vary_accept() {
    let res = client()
        .get("/about")
        .header("accept", "text/html,application/xhtml+xml,*/*;q=0.8")
        .send()
        .await;
    res.assert_ok();
    assert!(res.header("content-type").unwrap().starts_with("text/html"));
    assert!(
        vary_has_accept(res.header("vary")),
        "{:?}",
        res.header("vary")
    );
    assert!(res.text().contains("<h1>About</h1>"));
}

#[tokio::test]
async fn html_preferred_by_q_value_stays_html() {
    let res = client()
        .get("/about")
        .header("accept", "text/html, text/markdown;q=0.5")
        .send()
        .await;
    assert!(res.header("content-type").unwrap().starts_with("text/html"));
}

#[tokio::test]
async fn markdown_rewrites_validators() {
    let res = client()
        .get("/tagged")
        .header("accept", "text/markdown")
        .send()
        .await;
    assert_eq!(
        res.header("content-type"),
        Some("text/markdown; charset=utf-8")
    );
    let etag = res.header("etag").expect("etag kept as a markdown variant");
    assert!(etag.starts_with("W/") && etag != "\"v1\"", "{etag}");
    assert!(res.header("last-modified").is_none());
    assert_eq!(res.text(), "tagged\n");
}

#[tokio::test]
async fn oversized_pages_stay_html() {
    let mut config = AutumnConfig::default();
    config.aeo.markdown_max_bytes = 1024;
    let res = client_with(config)
        .get("/big")
        .header("accept", "text/markdown")
        .send()
        .await;
    res.assert_ok();
    assert!(res.header("content-type").unwrap().starts_with("text/html"));
    assert_eq!(res.text().len(), 4096 + "<p></p>".len());
}

#[tokio::test]
async fn non_html_and_error_responses_are_untouched() {
    let c = client();
    let json = c
        .get("/data")
        .header("accept", "text/markdown")
        .send()
        .await;
    assert!(
        json.header("content-type")
            .unwrap()
            .starts_with("application/json")
    );
    let missing = c
        .get("/nope")
        .header("accept", "text/markdown")
        .send()
        .await;
    assert_eq!(missing.status, 404);
    assert_ne!(
        missing.header("content-type"),
        Some("text/markdown; charset=utf-8")
    );
}

#[tokio::test]
async fn markdown_can_be_turned_off() {
    let mut config = AutumnConfig::default();
    config.aeo.markdown = false;
    let res = client_with(config)
        .get("/")
        .header("accept", "text/markdown")
        .send()
        .await;
    assert!(res.header("content-type").unwrap().starts_with("text/html"));
    assert!(!vary_has_accept(res.header("vary")));
}

// ── Homepage Link headers ────────────────────────────────────────────────

#[tokio::test]
async fn homepage_sends_agent_link_headers() {
    let c = client();
    let res = c.get("/").send().await;
    let link = res.header("link").expect("homepage Link header");
    assert!(link.contains("rel=\"describedby\""), "{link}");
    assert!(link.contains("rel=\"ai-catalog\""), "{link}");
    assert!(c.get("/about").send().await.header("link").is_none());
}

// ── Generated documents ──────────────────────────────────────────────────

#[tokio::test]
async fn llms_txt_lists_seo_pages() {
    let res = client().get("/llms.txt").send().await;
    res.assert_ok();
    assert_eq!(
        res.header("content-type"),
        Some("text/plain; charset=utf-8")
    );
    let body = res.text();
    assert!(
        body.contains("- [About](http://localhost/about): Who we are"),
        "{body}"
    );
}

#[tokio::test]
async fn an_app_route_wins_over_a_generated_document() {
    let res = TestApp::new()
        .routes(routes![custom_llms])
        .build()
        .get("/llms.txt")
        .send()
        .await;
    assert_eq!(res.text(), "# Custom\n");
}

#[tokio::test]
async fn skills_and_ard_are_served() {
    let c = client();
    let index = c.get("/.well-known/agent-skills/index.json").send().await;
    index.assert_ok();
    let json: serde_json::Value = serde_json::from_str(&index.text()).unwrap();
    let url = json["skills"][0]["url"].as_str().unwrap().to_owned();
    let skill = c.get(&url).send().await;
    skill.assert_ok();
    assert!(skill.text().starts_with("---\nname: site-guide\n"));

    let ard = c.get("/.well-known/ai-catalog.json").send().await;
    ard.assert_ok();
    assert_eq!(ard.header("access-control-allow-origin"), Some("*"));
}

#[tokio::test]
async fn documents_answer_get_and_head_only() {
    let c = client();
    let head = c.head("/llms.txt").send().await;
    head.assert_ok();
    let post = c.post("/llms.txt").send().await;
    assert_ne!(post.status, 200);
}

#[tokio::test]
async fn disabled_aeo_serves_nothing() {
    let mut config = AutumnConfig::default();
    config.aeo.enabled = false;
    let c = client_with(config);
    assert_eq!(c.get("/llms.txt").send().await.status, 404);
    assert!(c.get("/").send().await.header("link").is_none());
    let md = c.get("/").header("accept", "text/markdown").send().await;
    assert!(md.header("content-type").unwrap().starts_with("text/html"));
}

#[get("/robots.txt")]
async fn custom_robots() -> &'static str {
    "User-agent: *\nDisallow: /private\n"
}

#[get("/private")]
async fn private_page() -> impl IntoResponse {
    (
        autumn_web::reexports::http::StatusCode::UNAUTHORIZED,
        "sign in",
    )
}

#[tokio::test]
async fn robots_txt_is_served_by_default_and_an_app_route_wins() {
    let res = client().get("/robots.txt").send().await;
    res.assert_ok();
    let body = res.text();
    assert!(body.contains("User-agent: *"), "{body}");
    assert!(body.contains("User-agent: GPTBot"), "{body}");
    assert!(body.contains("Content-Signal: search=yes"), "{body}");

    let custom = TestApp::new()
        .routes(routes![custom_robots])
        .build()
        .get("/robots.txt")
        .send()
        .await;
    assert_eq!(custom.text(), "User-agent: *\nDisallow: /private\n");
}

#[tokio::test]
async fn documents_carry_an_etag_and_answer_304() {
    let c = client();
    let first = c.get("/llms.txt").send().await;
    let etag = first.header("etag").expect("etag").to_owned();
    let again = c
        .get("/llms.txt")
        .header("if-none-match", &etag)
        .send()
        .await;
    assert_eq!(again.status, 304);
}

#[tokio::test]
async fn head_with_markdown_accept_mirrors_get_headers() {
    let res = client()
        .head("/about")
        .header("accept", "text/markdown")
        .send()
        .await;
    res.assert_ok();
    assert_eq!(
        res.header("content-type"),
        Some("text/markdown; charset=utf-8")
    );
    assert!(vary_has_accept(res.header("vary")));
}

#[tokio::test]
async fn documents_never_carry_a_set_cookie() {
    let mut config = AutumnConfig::default();
    config.security.csrf.enabled = true;
    let c = client_with(config);
    let page = c.get("/about").send().await;
    let doc = c.get("/llms.txt").send().await;
    doc.assert_ok();
    assert!(doc.header("set-cookie").is_none(), "{:?}", doc.headers);
    // Control: the CSRF layer does set its cookie on a page.
    assert!(page.header("set-cookie").is_some(), "{:?}", page.headers);
}

#[tokio::test]
async fn a_401_points_at_the_protected_resource_metadata() {
    let mut config = AutumnConfig::default();
    config.seo.base_url = Some("https://shop.example.com".to_owned());
    config.aeo.oauth.authorization_servers = vec!["https://auth.example.com".to_owned()];
    let res = TestApp::new()
        .config(config)
        .routes(routes![private_page])
        .build()
        .get("/private")
        .send()
        .await;
    assert_eq!(res.status, 401);
    assert_eq!(
        res.header("www-authenticate"),
        Some(
            "Bearer resource_metadata=\"https://shop.example.com/.well-known/oauth-protected-resource\""
        )
    );
}

#[cfg(feature = "mcp")]
mod with_mcp {
    use super::*;

    #[get("/api/todos")]
    #[api_doc(mcp, summary = "List todos")]
    async fn list_todos() -> Json<Vec<String>> {
        Json(vec!["one".to_owned()])
    }

    fn mcp_client(secure: bool) -> TestClient {
        let mut config = AutumnConfig::default();
        config.aeo.auth_md.registration_url = Some("https://todos.example/tokens".to_owned());
        let app = TestApp::new()
            .config(config)
            .routes(routes![home, list_todos])
            .openapi(autumn_web::openapi::OpenApiConfig::new("Todos", "3.0.0"))
            .mount_mcp("/mcp");
        let app = if secure {
            let store = std::sync::Arc::new(autumn_web::auth::InMemoryApiTokenStore::default());
            app.secure_mcp(autumn_web::auth::RequireApiToken::new(store))
        } else {
            app
        };
        app.build()
    }

    #[tokio::test]
    async fn server_card_api_catalog_and_link_headers() {
        let c = mcp_client(false);
        for path in ["/.well-known/mcp/server-card.json", "/mcp/server-card"] {
            let res = c.get(path).send().await;
            res.assert_ok();
            let card: serde_json::Value = serde_json::from_str(&res.text()).unwrap();
            assert_eq!(card["serverInfo"]["version"], "3.0.0", "{path}");
            assert_eq!(card["tools"][0]["name"], "list_todos", "{card}");
        }
        let catalog = c.get("/.well-known/api-catalog").send().await;
        catalog.assert_ok();
        assert!(
            catalog
                .header("content-type")
                .unwrap()
                .starts_with("application/linkset+json")
        );
        let head = c.head("/.well-known/api-catalog").send().await;
        assert!(head.header("link").unwrap().contains("rel=\"api-catalog\""));

        let link = c.get("/").send().await.header("link").unwrap().to_owned();
        assert!(link.contains("rel=\"api-catalog\""), "{link}");
        assert!(link.contains("rel=\"service-desc\""), "{link}");

        let auth = c.get("/auth.md").send().await;
        auth.assert_ok();
        assert!(auth.text().starts_with("# Todos auth.md\n"));

        let preflight = c.options("/mcp/server-card").send().await;
        assert_eq!(preflight.status, 204);
        assert_eq!(preflight.header("access-control-allow-origin"), Some("*"));

        let js = c.get("/_autumn/webmcp.js").send().await;
        js.assert_ok();
        assert!(js.text().contains("registerTool"));
        assert!(js.text().contains("\"list_todos\""));
    }

    #[tokio::test]
    async fn secured_mcp_hides_tools() {
        let card = mcp_client(true)
            .get("/.well-known/mcp/server-card.json")
            .send()
            .await;
        card.assert_ok();
        let card: serde_json::Value = serde_json::from_str(&card.text()).unwrap();
        assert!(card.get("tools").is_none(), "{card}");
    }
}

#[tokio::test]
async fn registered_skills_are_published() {
    let skill =
        autumn_web::aeo::AgentSkill::new("refunds", "Draft a refund.", "# Refunds\n").unwrap();
    let c = TestApp::new().agent_skill(skill).build();
    let res = c
        .get("/.well-known/agent-skills/refunds/SKILL.md")
        .send()
        .await;
    res.assert_ok();
    assert!(res.text().contains("# Refunds"));
    let index = c.get("/.well-known/agent-skills/index.json").send().await;
    assert!(index.text().contains("\"refunds\""));
}

// ── Commerce ─────────────────────────────────────────────────────────────

#[cfg(feature = "http-client")]
mod commerce {
    use super::*;
    use autumn_web::aeo::commerce::{
        AcpDiscovery, PaidRoute, UcpProfile, decode_header, encode_header,
    };
    use serde_json::json;

    #[get("/api")]
    async fn api() -> Json<serde_json::Value> {
        Json(json!({ "data": 42 }))
    }

    #[get("/api/broken")]
    async fn broken() -> autumn_web::reexports::http::StatusCode {
        autumn_web::reexports::http::StatusCode::INTERNAL_SERVER_ERROR
    }

    fn paid_config() -> AutumnConfig {
        let mut config = AutumnConfig::default();
        config.aeo.x402 = toml::from_str(
            r#"
            facilitator_url = "https://facilitator.example"
            pay_to = "0xabc"
            network = "eip155:84532"
            asset = "0xusdc"
            "#,
        )
        .unwrap();
        for path in ["/api", "/api/broken"] {
            config.aeo.paid_routes.push(
                toml::from_str::<PaidRoute>(&format!(
                    "method = \"GET\"\npath = \"{path}\"\namount = \"10000\"\ndescription = \"Data\""
                ))
                .unwrap(),
            );
        }
        config
    }

    fn signature(accepted: &serde_json::Value) -> String {
        encode_header(&json!({
            "x402Version": 2,
            "accepted": accepted,
            "payload": { "signature": "0xsig" },
        }))
    }

    #[tokio::test]
    async fn unpaid_request_gets_a_402_challenge() {
        let res = TestApp::new()
            .config(paid_config())
            .routes(routes![api])
            .build()
            .get("/api")
            .send()
            .await;
        assert_eq!(res.status, 402);
        assert_eq!(res.header("cache-control"), Some("no-store"));
        let required = decode_header(res.header("payment-required").unwrap()).unwrap();
        assert_eq!(required["x402Version"], 2);
        assert_eq!(required["accepts"][0]["amount"], "10000");
        assert_eq!(required["resource"]["url"], "http://localhost/api");
    }

    #[tokio::test]
    async fn paid_request_is_verified_served_and_settled() {
        let mut app = TestApp::new().config(paid_config()).routes(routes![api]);
        let verify = app
            .http_mock("x402")
            .post("/verify")
            .respond_with(200, json!({ "isValid": true, "payer": "0xpayer" }));
        let settle = app.http_mock("x402").post("/settle").respond_with(
            200,
            json!({ "success": true, "transaction": "0xtx", "network": "eip155:84532" }),
        );
        let c = app.build();
        let challenge = c.get("/api").send().await;
        let required = decode_header(challenge.header("payment-required").unwrap()).unwrap();
        let res = c
            .get("/api")
            .header("payment-signature", &signature(&required["accepts"][0]))
            .send()
            .await;
        res.assert_ok();
        assert!(res.text().contains("42"));
        let receipt = decode_header(res.header("payment-response").unwrap()).unwrap();
        assert_eq!(receipt["transaction"], "0xtx");
        verify.expect_called(1);
        settle.expect_called(1);
    }

    #[tokio::test]
    async fn invalid_or_mismatched_payments_are_refused() {
        let mut app = TestApp::new().config(paid_config()).routes(routes![api]);
        let verify = app.http_mock("x402").post("/verify").respond_with(
            200,
            json!({ "isValid": false, "invalidReason": "insufficient_funds" }),
        );
        let c = app.build();
        let required = decode_header(
            c.get("/api")
                .send()
                .await
                .header("payment-required")
                .unwrap(),
        )
        .unwrap();
        let res = c
            .get("/api")
            .header("payment-signature", &signature(&required["accepts"][0]))
            .send()
            .await;
        assert_eq!(res.status, 402);
        let again = decode_header(res.header("payment-required").unwrap()).unwrap();
        assert_eq!(again["error"], "insufficient_funds");

        let mut cheap = required["accepts"][0].clone();
        cheap["amount"] = json!("1");
        let res = c
            .get("/api")
            .header("payment-signature", &signature(&cheap))
            .send()
            .await;
        assert_eq!(res.status, 402);
        verify.expect_called(1);

        let res = c
            .get("/api")
            .header("payment-signature", "%%%")
            .send()
            .await;
        assert_eq!(res.status, 400);
    }

    #[tokio::test]
    async fn a_failed_handler_is_never_settled() {
        let mut app = TestApp::new().config(paid_config()).routes(routes![broken]);
        let verify = app
            .http_mock("x402")
            .post("/verify")
            .respond_with(200, json!({ "isValid": true }));
        let settle = app
            .http_mock("x402")
            .post("/settle")
            .respond_with(200, json!({ "success": true }));
        let c = app.build();
        let required = decode_header(
            c.get("/api/broken")
                .send()
                .await
                .header("payment-required")
                .unwrap(),
        )
        .unwrap();
        let res = c
            .get("/api/broken")
            .header("payment-signature", &signature(&required["accepts"][0]))
            .send()
            .await;
        assert_eq!(res.status, 500);
        verify.expect_called(1);
        settle.expect_called(0);
    }

    #[post("/api/render")]
    async fn render_job() -> Json<serde_json::Value> {
        RENDERED.store(true, std::sync::atomic::Ordering::SeqCst);
        Json(json!({ "rendered": true }))
    }

    static RENDERED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

    #[tokio::test]
    async fn head_and_trailing_slash_requests_are_charged() {
        let c = TestApp::new()
            .config(paid_config())
            .routes(routes![api])
            .build();
        assert_eq!(c.head("/api").send().await.status, 402);
        assert_eq!(c.get("/api/").send().await.status, 402);
    }

    #[tokio::test]
    async fn a_payment_header_works_once_and_paid_answers_are_private() {
        let mut app = TestApp::new().config(paid_config()).routes(routes![api]);
        let _verify = app
            .http_mock("x402")
            .post("/verify")
            .respond_with(200, json!({ "isValid": true }));
        let _settle = app
            .http_mock("x402")
            .post("/settle")
            .respond_with(200, json!({ "success": true, "transaction": "0xtx" }));
        let c = app.build();
        let required = decode_header(
            c.get("/api")
                .send()
                .await
                .header("payment-required")
                .unwrap(),
        )
        .unwrap();
        let sig = signature(&required["accepts"][0]);
        let paid = c.get("/api").header("payment-signature", &sig).send().await;
        paid.assert_ok();
        assert_eq!(paid.header("cache-control"), Some("private, no-store"));
        let replay = c.get("/api").header("payment-signature", &sig).send().await;
        assert_eq!(replay.status, 402);
        let again = decode_header(replay.header("payment-required").unwrap()).unwrap();
        assert_eq!(again["error"], "payment already used");
    }

    #[tokio::test]
    async fn an_unsafe_method_settles_before_its_handler_runs() {
        let mut config = paid_config();
        config.aeo.paid_routes = vec![PaidRoute::new("POST", "/api/render", "500")];
        let mut app = TestApp::new().config(config).routes(routes![render_job]);
        let _verify = app
            .http_mock("x402")
            .post("/verify")
            .respond_with(200, json!({ "isValid": true }));
        let _settle = app.http_mock("x402").post("/settle").respond_with(
            200,
            json!({ "success": false, "errorReason": "insufficient_funds" }),
        );
        let c = app.build();
        let required = decode_header(
            c.post("/api/render")
                .send()
                .await
                .header("payment-required")
                .unwrap(),
        )
        .unwrap();
        let res = c
            .post("/api/render")
            .header("payment-signature", &signature(&required["accepts"][0]))
            .send()
            .await;
        assert_eq!(res.status, 402);
        assert!(
            !RENDERED.load(std::sync::atomic::Ordering::SeqCst),
            "the handler must not run when settlement fails"
        );
    }

    #[tokio::test]
    async fn an_mpp_route_is_left_to_the_app() {
        let mut config = paid_config();
        for route in &mut config.aeo.paid_routes {
            route.mpp_method = Some("stripe".to_owned());
        }
        let res = TestApp::new()
            .config(config)
            .routes(routes![api])
            .build()
            .get("/api")
            .send()
            .await;
        res.assert_ok();
    }

    #[tokio::test]
    async fn ucp_and_acp_documents_are_served() {
        let ucp = UcpProfile::new(
            json!({"ucp": {"version": "2026-08-25", "services": {}, "payment_handlers": {}}}),
        )
        .unwrap();
        let acp = AcpDiscovery::new(json!({
            "protocol": {"name": "acp", "version": "2025-09-29", "supported_versions": ["2025-09-29"]},
            "api_base_url": "https://shop.example/api",
            "transports": ["rest"],
            "capabilities": {"services": ["checkout"]}
        }))
        .unwrap();
        let c = TestApp::new().ucp_profile(ucp).acp_discovery(acp).build();
        let res = c.get("/.well-known/ucp").send().await;
        res.assert_ok();
        assert!(res.text().contains("payment_handlers"));
        let res = c.get("/.well-known/acp.json").send().await;
        res.assert_ok();
        assert!(res.text().contains("\"acp\""));
    }

    #[cfg(feature = "openapi")]
    #[tokio::test]
    async fn mpp_payment_info_reaches_openapi() {
        let mut config = paid_config();
        config.aeo.paid_routes[0].mpp_method = Some("stripe".to_owned());
        let res = TestApp::new()
            .config(config)
            .routes(routes![api])
            .openapi(autumn_web::openapi::OpenApiConfig::new("Shop", "1.0.0"))
            .build()
            .get("/openapi.json")
            .send()
            .await;
        res.assert_ok();
        let spec: serde_json::Value = serde_json::from_str(&res.text()).unwrap();
        let op = &spec["paths"]["/api"]["get"];
        assert_eq!(op["x-payment-info"]["method"], "stripe", "{op}");
        assert_eq!(op["responses"]["402"]["description"], "Payment Required");
    }
}
