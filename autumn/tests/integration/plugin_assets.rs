//! Plugin-provided assets: a plugin installs a `PluginAssets` bundle and the
//! framework serves it at content-hashed URLs that `asset_url` resolves.

use autumn_web::app::AppBuilder;
use autumn_web::assets::{PluginAssets, asset_url};
use autumn_web::plugin::Plugin;
use autumn_web::route_listing::{RouteClassification, RouteSource};
use autumn_web::test::TestApp;

static CHARTS: PluginAssets = PluginAssets::from_files(
    "it-charts",
    &[
        ("charts.js", b"window.Charts = {};"),
        ("css/charts.css", b".chart{display:block}"),
    ],
);

/// A different bundle under the same namespace as [`CHARTS`].
static CHARTS_IMPOSTOR: PluginAssets =
    PluginAssets::from_files("it-charts", &[("charts.js", b"window.Other = {};")]);

struct ChartsPlugin;

impl Plugin for ChartsPlugin {
    fn name(&self) -> std::borrow::Cow<'static, str> {
        "it-charts-plugin".into()
    }

    fn build(self, app: AppBuilder) -> AppBuilder {
        app.plugin_assets(&CHARTS)
    }
}

/// A second plugin that ships the very same bundle (say, a companion
/// plugin of the same family). Installing it twice must be harmless.
struct ChartsCompanionPlugin;

impl Plugin for ChartsCompanionPlugin {
    fn name(&self) -> std::borrow::Cow<'static, str> {
        "it-charts-companion".into()
    }

    fn build(self, app: AppBuilder) -> AppBuilder {
        app.plugin_assets(&CHARTS)
    }
}

struct ImpostorPlugin;

impl Plugin for ImpostorPlugin {
    fn name(&self) -> std::borrow::Cow<'static, str> {
        "it-charts-impostor".into()
    }

    fn build(self, app: AppBuilder) -> AppBuilder {
        app.plugin_assets(&CHARTS_IMPOSTOR)
    }
}

#[tokio::test]
async fn installed_bundle_is_served_at_hashed_and_plain_urls() {
    let client = TestApp::new().plugin(ChartsPlugin).build();

    let asset = CHARTS.get("charts.js").expect("bundle has charts.js");
    let hashed = client.get(asset.url()).send().await;
    hashed
        .assert_ok()
        .assert_header("cache-control", "public, max-age=31536000, immutable")
        .assert_header("content-type", "text/javascript; charset=utf-8");
    assert_eq!(hashed.body, b"window.Charts = {};");
    let etag = hashed.header("etag").expect("etag").to_owned();

    let plain = client
        .get("/static/_plugins/it-charts/charts.js")
        .send()
        .await;
    plain
        .assert_ok()
        .assert_header("cache-control", "public, max-age=0, must-revalidate");
    assert_eq!(plain.body, b"window.Charts = {};");

    let revalidated = client
        .get("/static/_plugins/it-charts/charts.js")
        .header("if-none-match", &etag)
        .send()
        .await;
    revalidated.assert_status(304);
    assert!(revalidated.body.is_empty());

    let nested = CHARTS.get("css/charts.css").expect("bundle has the css");
    client
        .get(nested.url())
        .send()
        .await
        .assert_ok()
        .assert_header("content-type", "text/css; charset=utf-8");

    // A stale hash (an older build's URL) and an unknown file both 404
    // rather than falling through to some other handler.
    client
        .get("/static/_plugins/it-charts/charts.00000000.js")
        .send()
        .await
        .assert_status(404);
    client
        .get("/static/_plugins/it-charts/missing.js")
        .send()
        .await
        .assert_status(404);
}

#[tokio::test]
async fn asset_url_resolves_an_installed_bundle() {
    let _client = TestApp::new().plugin(ChartsPlugin).build();
    let url = asset_url("_plugins/it-charts/charts.js");
    assert_eq!(url, CHARTS.url("charts.js"));
    assert!(
        url.starts_with("/static/_plugins/it-charts/charts.")
            && url.rsplit('.').next() == Some("js"),
        "{url}"
    );
    // An unknown file in a known bundle keeps the plain path.
    assert_eq!(
        asset_url("_plugins/it-charts/nope.js"),
        "/static/_plugins/it-charts/nope.js"
    );
}

#[test]
fn bundle_routes_are_declared_as_public_plugin_routes() {
    let app = autumn_web::app().plugin(ChartsPlugin);
    let infos = app.plugin_route_infos().expect("route infos");
    let asset_routes: Vec<_> = infos
        .iter()
        .filter(|info| info.path.starts_with("/static/_plugins/it-charts/"))
        .collect();
    assert_eq!(asset_routes.len(), 4, "two files, two URLs each: {infos:?}");
    for info in asset_routes {
        assert_eq!(info.method, "GET");
        assert_eq!(info.classification, RouteClassification::Public);
        assert_eq!(
            info.middleware,
            [autumn_web::assets::PLUGIN_ASSETS_ROUTE_MARKER],
            "bundle routes carry the asset marker the conformance checks look for"
        );
        assert_eq!(
            info.source,
            RouteSource::Plugin("it-charts-plugin".to_owned())
        );
    }
}

#[tokio::test]
async fn the_same_bundle_installed_by_two_plugins_is_served_once() {
    let client = TestApp::new()
        .plugin(ChartsPlugin)
        .plugin(ChartsCompanionPlugin)
        .build();
    client
        .get(&CHARTS.url("charts.js"))
        .send()
        .await
        .assert_ok();
}

#[test]
#[should_panic(expected = "two different PluginAssets bundles use the namespace `it-charts`")]
fn a_second_bundle_with_a_taken_namespace_is_refused() {
    let _app = autumn_web::app()
        .plugin(ChartsPlugin)
        .plugin(ImpostorPlugin);
}

#[test]
#[should_panic(expected = "two different PluginAssets bundles use the namespace `it-charts`")]
fn test_app_refuses_a_taken_namespace_too() {
    let _app = TestApp::new().plugin(ChartsPlugin).plugin(ImpostorPlugin);
}

/// The `plugin_assets!` macro embeds a directory: nested files are included,
/// dotfiles are not.
#[cfg(feature = "embed-assets")]
#[tokio::test]
async fn plugin_assets_macro_embeds_a_directory() {
    static EMBEDDED: PluginAssets = autumn_web::plugin_assets!(
        "it-embedded",
        "$CARGO_MANIFEST_DIR/tests/fixtures/plugin_assets"
    );

    let paths: Vec<&str> = EMBEDDED.iter().map(|a| a.logical_path()).collect();
    assert_eq!(paths, ["css/theme.css", "init.js"]);

    struct EmbeddedPlugin;
    impl Plugin for EmbeddedPlugin {
        fn build(self, app: AppBuilder) -> AppBuilder {
            app.plugin_assets(&EMBEDDED)
        }
    }

    let client = TestApp::new().plugin(EmbeddedPlugin).build();
    let response = client.get(&EMBEDDED.url("init.js")).send().await;
    response.assert_ok();
    assert_eq!(
        response.body,
        std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/plugin_assets/init.js"
        ))
        .unwrap()
    );
}

/// Installing a bundle exempts only the bundle's own `GET` routes from the
/// `/static` namespace refusal. A plugin that declares any other method at
/// one of those paths is still refused at startup.
#[test]
#[should_panic(expected = "DuplicateUserRoute")]
fn a_non_get_route_declared_at_a_bundle_path_is_still_refused() {
    struct PostOverAssetPlugin;
    impl Plugin for PostOverAssetPlugin {
        fn name(&self) -> std::borrow::Cow<'static, str> {
            "it-post-over-asset".into()
        }
        fn build(self, app: AppBuilder) -> AppBuilder {
            app.plugin_assets(&CHARTS).declare_plugin_routes(vec![
                autumn_web::route_listing::RouteInfo {
                    method: "POST".to_owned(),
                    path: "/static/_plugins/it-charts/charts.js".to_owned(),
                    handler: "it-post-over-asset::post".to_owned(),
                    ..Default::default()
                },
            ])
        }
    }
    let _client = TestApp::new().plugin(PostOverAssetPlugin).build();
}

/// `asset_url` resolves a namespace process-wide, so a second app in the
/// process installing a *different* bundle under a taken namespace is
/// refused rather than silently resolved against the first app's bundle.
#[test]
#[should_panic(
    expected = "two different PluginAssets bundles use the namespace `it-charts` in one process"
)]
fn another_app_in_the_process_cannot_reuse_a_namespace() {
    let _first = autumn_web::app().plugin(ChartsPlugin);
    let _second = autumn_web::app().plugin(ImpostorPlugin);
}

/// The asset marker exempts a route from `autumn plugin-check`'s prefix and
/// sensitive-name checks, so only `AppBuilder::plugin_assets` may set it: a
/// plugin that declares a route carrying it has the label stripped.
#[test]
fn declare_plugin_routes_strips_a_forged_asset_marker() {
    struct ForgingPlugin;
    impl Plugin for ForgingPlugin {
        fn name(&self) -> std::borrow::Cow<'static, str> {
            "it-forging".into()
        }
        fn build(self, app: AppBuilder) -> AppBuilder {
            app.declare_plugin_routes(vec![autumn_web::route_listing::RouteInfo {
                method: "GET".to_owned(),
                path: "/static/_plugins/it-forging/admin".to_owned(),
                middleware: vec![
                    autumn_web::assets::PLUGIN_ASSETS_ROUTE_MARKER.to_owned(),
                    "secured".to_owned(),
                ],
                ..Default::default()
            }])
        }
    }
    let infos = autumn_web::app()
        .plugin(ForgingPlugin)
        .plugin_route_infos()
        .expect("route infos");
    let forged = infos
        .iter()
        .find(|info| info.path == "/static/_plugins/it-forging/admin")
        .expect("declared route is listed");
    assert_eq!(forged.middleware, ["secured"]);
}

/// An app route at the same URL as a bundle file is a typed
/// `DuplicateUserRoute` refusal at startup, not an axum overlapping-route
/// panic: the bundle's routes stay visible to the duplicate-route preflight.
#[test]
#[should_panic(expected = "DuplicateUserRoute")]
fn an_app_route_at_a_bundle_url_is_a_typed_collision() {
    #[autumn_web::get("/static/_plugins/it-charts/charts.js")]
    async fn shadow() -> &'static str {
        "shadowed"
    }
    let _client = TestApp::new()
        .routes(autumn_web::routes![shadow])
        .plugin(ChartsPlugin)
        .build();
}

/// A plugin that hand-declares a `GET` at a bundle path, without the marker
/// only `AppBuilder::plugin_assets` can attach, gets no exemption: it is a
/// typed collision with the bundle's own route, never a silent shadow. (At a
/// `/static/_plugins/` path no bundle serves, the `/static` namespace refusal
/// catches it instead; the router's own tests cover that case.)
#[test]
#[should_panic(expected = "incoming: \"it-declares-over-asset::get\"")]
fn an_unmarked_get_declared_at_a_bundle_path_is_refused() {
    struct DeclaresOverAssetPlugin;
    impl Plugin for DeclaresOverAssetPlugin {
        fn name(&self) -> std::borrow::Cow<'static, str> {
            "it-declares-over-asset".into()
        }
        fn build(self, app: AppBuilder) -> AppBuilder {
            app.plugin_assets(&CHARTS).declare_plugin_routes(vec![
                autumn_web::route_listing::RouteInfo {
                    method: "GET".to_owned(),
                    path: "/static/_plugins/it-charts/charts.js".to_owned(),
                    handler: "it-declares-over-asset::get".to_owned(),
                    ..Default::default()
                },
            ])
        }
    }
    let _client = TestApp::new().plugin(DeclaresOverAssetPlugin).build();
}
