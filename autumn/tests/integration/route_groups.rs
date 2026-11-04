//! Route groups and per-role mounting (`[roles.<name>] mount`, issue #3544).

use std::borrow::Cow;

use autumn_web::app::AppBuilder;
use autumn_web::config::AutumnConfig;
use autumn_web::plugin::Plugin;
use autumn_web::prelude::*;
use autumn_web::test::TestApp;

#[get("/landing", group = "marketing")]
async fn landing() -> &'static str {
    "landing"
}

#[get("/desk")]
async fn desk() -> &'static str {
    "desk"
}

#[get("/about")]
async fn about() -> &'static str {
    "about"
}

#[get("/crm")]
async fn crm_home() -> &'static str {
    "crm"
}

/// A plugin with the default route group: its name.
struct CrmPlugin;

impl Plugin for CrmPlugin {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("crm")
    }

    fn build(self, app: AppBuilder) -> AppBuilder {
        app.routes(routes![crm_home])
    }
}

/// A plugin that puts its routes in the `marketing` group.
struct LeadFormsPlugin;

#[get("/leads")]
async fn leads() -> &'static str {
    "leads"
}

impl Plugin for LeadFormsPlugin {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("lead-forms")
    }

    fn route_group(&self) -> Cow<'static, str> {
        Cow::Borrowed("marketing")
    }

    fn build(self, app: AppBuilder) -> AppBuilder {
        app.routes(routes![leads])
    }
}

const FLEET: &str = r#"
[roles.marketing]
base = "web"
mount = ["marketing"]

[roles.service]
base = "web"
mount = ["service", "app", "crm"]
"#;

fn config_for(role: &str) -> AutumnConfig {
    toml::from_str(&format!("role = \"{role}\"\n{FLEET}")).expect("roles config parses")
}

fn fleet_app(role: &str) -> TestApp {
    TestApp::new()
        .config(config_for(role))
        .routes(routes![landing, about])
        .routes_in("service", routes![desk])
        .plugin(CrmPlugin)
        .plugin(LeadFormsPlugin)
}

#[tokio::test]
async fn marketing_role_serves_its_group_and_404s_the_rest() {
    let client = fleet_app("marketing").build();
    client.get("/landing").send().await.assert_ok();
    client.get("/leads").send().await.assert_ok();
    client.get("/desk").send().await.assert_status(404);
    client.get("/about").send().await.assert_status(404);
    client.get("/crm").send().await.assert_status(404);
}

#[tokio::test]
async fn service_role_serves_its_groups() {
    let client = fleet_app("service").build();
    client.get("/desk").send().await.assert_ok();
    client.get("/about").send().await.assert_ok();
    client.get("/crm").send().await.assert_ok();
    client.get("/landing").send().await.assert_status(404);
    client.get("/leads").send().await.assert_status(404);
}

#[tokio::test]
async fn a_role_without_mount_serves_every_group() {
    let client = fleet_app("combined").build();
    for path in ["/landing", "/leads", "/desk", "/about", "/crm"] {
        client.get(path).send().await.assert_ok();
    }
}

#[tokio::test]
async fn probes_and_actuator_work_on_every_role() {
    for role in ["marketing", "service", "combined", "web"] {
        let client = fleet_app(role).build();
        for path in ["/live", "/ready", "/startup", "/health", "/actuator/health"] {
            let status = client.get(path).send().await.status;
            assert!(status.is_success(), "role {role}: {path} returned {status}");
        }
    }
}

#[tokio::test]
async fn path_helpers_build_for_an_unmounted_group() {
    // The marketing role does not serve `/desk`, but a link to it still
    // builds: the URL is valid for the deployment.
    let _client = fleet_app("marketing").build();
    assert_eq!(__autumn_path_desk(), "/desk");
}

#[test]
#[should_panic(expected = "no route is in that group")]
fn a_mount_entry_that_names_no_group_fails_the_build() {
    let config: AutumnConfig = toml::from_str(
        r#"
role = "typo"

[roles.typo]
base = "web"
mount = ["markting"]
"#,
    )
    .expect("roles config parses");
    let _client = TestApp::new()
        .config(config)
        .routes(routes![landing])
        .build();
}

#[test]
fn a_mount_on_a_worker_base_fails_the_load() {
    let err = toml::from_str::<AutumnConfig>(
        r#"
[roles.bulk]
base = "worker"
mount = ["marketing"]
"#,
    )
    .expect("parses")
    .validate()
    .expect_err("a worker base serves no routes");
    assert!(err.to_string().contains("mount"), "{err}");
}

#[cfg(feature = "openapi")]
#[tokio::test]
async fn openapi_document_lists_only_the_role_routes() {
    use autumn_web::openapi::OpenApiConfig;
    let client = fleet_app("marketing")
        .openapi(OpenApiConfig::new("Fleet", "1.0.0"))
        .build();
    let response = client.get("/openapi.json").send().await;
    response.assert_ok();
    let body: serde_json::Value = response.json();
    assert!(body["paths"]["/landing"].is_object(), "{body}");
    assert!(body["paths"]["/leads"].is_object(), "{body}");
    assert!(body["paths"]["/desk"].is_null(), "{body}");
    assert!(body["paths"]["/about"].is_null(), "{body}");
}

#[test]
fn route_group_field_follows_the_attribute_and_in_group() {
    use autumn_web::RouteGroup;
    let route = routes![landing].remove(0);
    assert_eq!(
        route.group.as_ref().map(RouteGroup::as_str),
        Some("marketing")
    );
    let moved = routes![desk].remove(0).in_group("service");
    assert_eq!(
        moved.group.as_ref().map(RouteGroup::as_str),
        Some("service")
    );
    assert_eq!(routes![desk].remove(0).group, None);
}

fn narrow(role: &str, mount: &str) -> AutumnConfig {
    toml::from_str(&format!(
        "role = \"{role}\"\n[roles.{role}]\nbase = \"web\"\nmount = [{mount}]\n"
    ))
    .expect("roles config parses")
}

#[tokio::test]
async fn a_plugin_group_defaults_to_the_plugin_name() {
    let client = TestApp::new()
        .config(narrow("crm_only", "\"crm\""))
        .routes(routes![about])
        .plugin(CrmPlugin)
        .build();
    client.get("/crm").send().await.assert_ok();
    client.get("/about").send().await.assert_status(404);
}

#[tokio::test]
async fn routes_in_keeps_a_route_attribute_group() {
    let client = TestApp::new()
        .config(narrow("site", "\"marketing\""))
        .routes_in("service", routes![landing, desk])
        .build();
    client.get("/landing").send().await.assert_ok();
    client.get("/desk").send().await.assert_status(404);
}

#[test]
fn in_group_overrides_a_route_attribute_group() {
    let route = routes![landing].remove(0).in_group("service");
    assert_eq!(route.group_name(), "service");
    assert_eq!(routes![desk].remove(0).group_name(), "app");
}

#[get("/outer-before")]
async fn outer_before() -> &'static str {
    "outer-before"
}

#[get("/outer-after")]
async fn outer_after() -> &'static str {
    "outer-after"
}

#[get("/inner")]
async fn inner() -> &'static str {
    "inner"
}

#[get("/scoped")]
async fn scoped_route() -> &'static str {
    "scoped"
}

struct InnerPlugin;

impl Plugin for InnerPlugin {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("inner")
    }

    fn build(self, app: AppBuilder) -> AppBuilder {
        app.routes(routes![inner])
    }
}

/// Adds routes in every form, and a nested plugin between them.
struct OuterPlugin;

impl Plugin for OuterPlugin {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("outer")
    }

    fn build(self, app: AppBuilder) -> AppBuilder {
        app.routes(routes![outer_before])
            .plugin(InnerPlugin)
            .routes(routes![outer_after])
            .scoped(
                "/outer",
                tower::layer::util::Identity::new(),
                routes![scoped_route],
            )
            .merge(
                axum::Router::new()
                    .route("/outer-merged", axum::routing::get(|| async { "merged" })),
            )
            .nest(
                "/outer-nested",
                axum::Router::new().route("/x", axum::routing::get(|| async { "nested" })),
            )
    }
}

#[tokio::test]
async fn nested_plugins_keep_their_own_groups() {
    let outer_paths = [
        "/outer-before",
        "/outer-after",
        "/outer/scoped",
        "/outer-merged",
        "/outer-nested/x",
    ];
    let outer = TestApp::new()
        .config(narrow("outer_only", "\"outer\""))
        .plugin(OuterPlugin)
        .build();
    for path in outer_paths {
        outer.get(path).send().await.assert_ok();
    }
    outer.get("/inner").send().await.assert_status(404);

    let inner = TestApp::new()
        .config(narrow("inner_only", "\"inner\""))
        .plugin(OuterPlugin)
        .build();
    inner.get("/inner").send().await.assert_ok();
    for path in outer_paths {
        inner.get(path).send().await.assert_status(404);
    }
}

/// A plugin whose `route_group()` is blank.
struct BlankGroupPlugin;

impl Plugin for BlankGroupPlugin {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("blank-group")
    }

    fn route_group(&self) -> Cow<'static, str> {
        Cow::Borrowed(" ")
    }

    fn build(self, app: AppBuilder) -> AppBuilder {
        app
    }
}

#[test]
#[should_panic(expected = "route_group() must not be blank")]
fn a_blank_plugin_route_group_panics() {
    let _ = autumn_web::app().plugin(BlankGroupPlugin);
}

#[test]
#[should_panic(expected = "must not be blank")]
fn a_blank_routes_in_group_panics() {
    let _ = autumn_web::app().routes_in("", routes![desk]);
}

#[tokio::test]
async fn actuator_info_shows_the_mount_on_a_narrow_role() {
    let client = fleet_app("marketing").build();
    let response = client.get("/actuator/info").send().await;
    response.assert_ok();
    let body: serde_json::Value = response.json();
    assert_eq!(
        body["role"]["mount"],
        serde_json::json!(["marketing"]),
        "{body}"
    );
}
