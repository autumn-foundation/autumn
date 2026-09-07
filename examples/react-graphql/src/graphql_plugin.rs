//! A generic GraphQL adapter for Autumn, packaged as a [`Plugin`].
//!
//! This module knows nothing about notes. Hand it any
//! [`async_graphql::Schema`] and it will:
//!
//! - mount `POST {path}` (JSON request body) and `GET {path}?query=…`
//!   (GraphQL-over-HTTP query-string form, **queries only** — a mutation or
//!   subscription over `GET` is refused with `405`, since a `GET` is what
//!   caches, prefetchers and cross-site navigations replay freely) through
//!   [`AppBuilder::nest`];
//! - mount `GET {path}/sdl`, serving the schema definition language so a
//!   client can generate types from the running server;
//! - inject the request's [`AppState`] into every execution's context data,
//!   so resolvers reach `AppState` extensions (stores, services, pools) the
//!   same way a route handler does;
//! - hand the schema to its handlers as **router-local** state (an
//!   `axum::Extension` layer on the nested router), not as an `AppState`
//!   extension keyed by type — so two schemas built from the same root types
//!   can be mounted at two paths without one overwriting the other;
//! - declare its routes with [`AppBuilder::declare_plugin_routes`], so
//!   `autumn routes` lists them with plugin attribution and
//!   `autumn routes audit` sees a covered mount rather than an opaque router;
//! - state a [`PluginContract`] naming the `autumn-web` series it targets;
//! - optionally wrap its whole router in a guard layer ([`GraphqlPlugin::guard`])
//!   — the only way to guard a nested router, since `AppBuilder::scoped`
//!   applies to the `routes![]` it is given, not to routers a plugin nests.
//!
//! It lives inside this example for readability, but it is exactly the shape
//! a published `autumn-plugin-graphql` crate would take — nothing here depends
//! on the surrounding crate.

use std::borrow::Cow;

use async_graphql::{ObjectType, Schema, SubscriptionType};
use autumn_web::app::AppBuilder;
use autumn_web::plugin::Plugin;
use autumn_web::plugin_contract::PluginContract;
use autumn_web::route_listing::{RouteClassification, RouteInfo};
use autumn_web::{AppState, AutumnError, AutumnResult};
use axum::Router;
use axum::extract::{Extension, Query, State};
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::{get, post};

/// Name this plugin reports in `PluginContract`, `autumn routes`, and
/// `autumn plugin-check` output.
pub const PLUGIN_NAME: &str = "react-graphql::GraphqlPlugin";

/// Mounts an `async_graphql::Schema` on an Autumn app.
///
/// ```rust,ignore
/// autumn_web::app()
///     .plugin(GraphqlPlugin::new(schema).path("/graphql"))
///     .run()
///     .await;
/// ```
pub struct GraphqlPlugin<Q, M, S> {
    schema: Schema<Q, M, S>,
    path: String,
    serve_sdl: bool,
    /// A layer applied over the whole nested router, plus the label
    /// `autumn routes` shows for it. Boxed as a router transform so the
    /// plugin stays non-generic over the layer type.
    guard: Option<Guard>,
}

type RouterTransform = Box<dyn FnOnce(Router<AppState>) -> Router<AppState> + Send>;

struct Guard {
    apply: RouterTransform,
    label: String,
}

impl<Q, M, S> GraphqlPlugin<Q, M, S>
where
    Q: ObjectType + 'static,
    M: ObjectType + 'static,
    S: SubscriptionType + 'static,
{
    /// Wrap a schema. Mounts at `/graphql` with the SDL route enabled.
    #[must_use]
    pub fn new(schema: Schema<Q, M, S>) -> Self {
        Self {
            schema,
            path: "/graphql".to_owned(),
            serve_sdl: true,
            guard: None,
        }
    }

    /// Guard every route this plugin mounts with `layer` — for example
    /// `RequireApiToken`, so `/graphql` needs `Authorization: Bearer …`.
    ///
    /// This is the seam a nested router needs: `AppBuilder::scoped(prefix,
    /// layer, routes![…])` wraps only the routes handed to it, and a raw
    /// router registered with `nest` is never among them. The layer is the
    /// outermost on the plugin's router, so it runs before any handler,
    /// including `GET /sdl`. `label` is what `autumn routes` lists as the
    /// route's middleware; the routes are then classified `Gated` rather
    /// than `Public`.
    #[must_use]
    pub fn guard<L>(mut self, layer: L, label: impl Into<String>) -> Self
    where
        L: tower::Layer<axum::routing::Route> + Clone + Send + Sync + 'static,
        L::Service: tower::Service<axum::extract::Request> + Clone + Send + Sync + 'static,
        <L::Service as tower::Service<axum::extract::Request>>::Response: IntoResponse + 'static,
        <L::Service as tower::Service<axum::extract::Request>>::Error:
            Into<std::convert::Infallible> + 'static,
        <L::Service as tower::Service<axum::extract::Request>>::Future: Send + 'static,
    {
        self.guard = Some(Guard {
            apply: Box::new(move |router| router.layer(layer)),
            label: label.into(),
        });
        self
    }

    /// Mount under a different path (for example `/api/graphql`).
    #[must_use]
    pub fn path(mut self, path: impl Into<String>) -> Self {
        self.path = path.into();
        self
    }

    /// Do not serve `GET {path}/sdl`.
    #[must_use]
    pub fn without_sdl(mut self) -> Self {
        self.serve_sdl = false;
        self
    }

    /// The routes this plugin mounts, as `autumn routes` will list them.
    ///
    /// Without a [`guard`](Self::guard) every route is classified
    /// [`RouteClassification::Public`] — the plugin mounts no guard of its
    /// own. With one, they are `Gated` and carry the guard's label as their
    /// middleware.
    #[must_use]
    pub fn route_infos(&self) -> Vec<RouteInfo> {
        let guard = self.guard.as_ref().map(|g| g.label.as_str());
        let mut routes = vec![
            route(
                "POST",
                self.path.clone(),
                "graphql_plugin::post_graphql",
                guard,
            ),
            route(
                "GET",
                self.path.clone(),
                "graphql_plugin::get_graphql",
                guard,
            ),
        ];
        if self.serve_sdl {
            routes.push(route(
                "GET",
                format!("{}/sdl", self.path),
                "graphql_plugin::sdl",
                guard,
            ));
        }
        routes
    }

    /// The raw axum router `build` nests at `path`.
    ///
    /// The schema rides on the router as an `Extension` layer: every request
    /// that reaches these routes carries this plugin's schema, and a second
    /// plugin at another path carries its own, even when the two share
    /// `Q`/`M`/`S` and would collide as a type-keyed `AppState` extension.
    fn router(&mut self) -> Router<AppState> {
        let mut router = Router::new().route(
            "/",
            post(post_graphql::<Q, M, S>).get(get_graphql::<Q, M, S>),
        );
        if self.serve_sdl {
            router = router.route("/sdl", get(sdl::<Q, M, S>));
        }
        let router = router.layer(Extension(self.schema.clone()));
        // Applied last, so the guard is the outermost layer and runs first.
        match self.guard.take() {
            Some(guard) => (guard.apply)(router),
            None => router,
        }
    }
}

impl<Q, M, S> Plugin for GraphqlPlugin<Q, M, S>
where
    Q: ObjectType + 'static,
    M: ObjectType + 'static,
    S: SubscriptionType + 'static,
{
    /// Keyed by mount path, so two schemas can coexist at different paths
    /// while a second plugin at the *same* path is still refused.
    fn name(&self) -> Cow<'static, str> {
        Cow::Owned(format!("{PLUGIN_NAME}@{}", self.path))
    }

    fn contract(&self) -> Option<PluginContract> {
        Some(
            PluginContract::new(PLUGIN_NAME)
                .plugin_version(env!("CARGO_PKG_VERSION"))
                .autumn_web("0.7"),
        )
    }

    fn build(mut self, app: AppBuilder) -> AppBuilder {
        let routes = self.route_infos();
        let router = self.router();
        tracing::info!(path = %self.path, sdl = self.serve_sdl, "mounting GraphQL endpoint");
        app.nest(&self.path, router).declare_plugin_routes(routes)
    }
}

fn route(method: &str, path: String, handler: &str, guard: Option<&str>) -> RouteInfo {
    RouteInfo {
        method: method.to_owned(),
        path,
        handler: handler.to_owned(),
        classification: guard.map_or(RouteClassification::Public, |_| RouteClassification::Gated),
        middleware: guard.map(str::to_owned).into_iter().collect(),
        ..Default::default()
    }
}

/// Execute one request, with `AppState` available to resolvers as context data.
async fn execute<Q, M, S>(
    schema: &Schema<Q, M, S>,
    state: AppState,
    request: async_graphql::Request,
) -> axum::Json<async_graphql::Response>
where
    Q: ObjectType + 'static,
    M: ObjectType + 'static,
    S: SubscriptionType + 'static,
{
    axum::Json(schema.execute(request.data(state)).await)
}

/// `POST {path}` with a JSON body of `{ query, variables?, operationName? }`.
async fn post_graphql<Q, M, S>(
    State(state): State<AppState>,
    Extension(schema): Extension<Schema<Q, M, S>>,
    axum::Json(request): axum::Json<async_graphql::Request>,
) -> axum::Json<async_graphql::Response>
where
    Q: ObjectType + 'static,
    M: ObjectType + 'static,
    S: SubscriptionType + 'static,
{
    execute(&schema, state, request).await
}

/// The GraphQL-over-HTTP `GET` parameters. `operationName` is the spelling
/// the spec (and every client) uses; `operation_name` is accepted too.
/// `variables` and `extensions` arrive as JSON text inside the URL.
#[derive(serde::Deserialize)]
struct GetParams {
    #[serde(default)]
    query: String,
    #[serde(alias = "operationName")]
    operation_name: Option<String>,
    variables: Option<String>,
    extensions: Option<String>,
}

impl GetParams {
    fn into_request(self) -> AutumnResult<async_graphql::Request> {
        if self.query.trim().is_empty() {
            return Err(AutumnError::bad_request_msg("missing `query` parameter"));
        }
        let mut request = async_graphql::Request::new(self.query);
        if let Some(name) = self.operation_name {
            request = request.operation_name(name);
        }
        if let Some(raw) = self.variables {
            let json: serde_json::Value = serde_json::from_str(&raw).map_err(|err| {
                AutumnError::bad_request_msg(format!("invalid `variables`: {err}"))
            })?;
            request = request.variables(async_graphql::Variables::from_json(json));
        }
        if let Some(raw) = self.extensions {
            request.extensions = serde_json::from_str(&raw).map_err(|err| {
                AutumnError::bad_request_msg(format!("invalid `extensions`: {err}"))
            })?;
        }
        Ok(request)
    }
}

/// `GET {path}?query=…&variables=…&operationName=…` — the query-string form
/// from the GraphQL-over-HTTP spec, handy for `curl` and for cacheable reads.
///
/// Only `query` operations are accepted here. A `GET` is safe and cacheable by
/// contract, so a mutation smuggled into a URL could be replayed by a
/// prefetcher, a shared cache, or a cross-site link; the spec's answer is
/// `405 Method Not Allowed`, and the request is refused before any resolver
/// runs.
async fn get_graphql<Q, M, S>(
    State(state): State<AppState>,
    Extension(schema): Extension<Schema<Q, M, S>>,
    Query(params): Query<GetParams>,
) -> AutumnResult<axum::Json<async_graphql::Response>>
where
    Q: ObjectType + 'static,
    M: ObjectType + 'static,
    S: SubscriptionType + 'static,
{
    let request = params.into_request()?;
    ensure_query_operation(&request)?;
    Ok(execute(&schema, state, request).await)
}

/// Refuse anything but a `query` operation, before execution.
///
/// A document that fails to parse is let through: the executor will report
/// the syntax error in the normal GraphQL `errors` shape, which is more useful
/// to a client than a transport-level rejection.
fn ensure_query_operation(request: &async_graphql::Request) -> AutumnResult<()> {
    use async_graphql::parser::types::{DocumentOperations, OperationType};

    let Ok(document) = async_graphql::parser::parse_query(&request.query) else {
        return Ok(());
    };
    let operation_type = match (&document.operations, request.operation_name.as_deref()) {
        (DocumentOperations::Single(op), _) => Some(op.node.ty),
        (DocumentOperations::Multiple(ops), Some(name)) => ops.get(name).map(|op| op.node.ty),
        // Several named operations and no selection: the executor will reject
        // that as ambiguous; refusing here would only mask its message.
        (DocumentOperations::Multiple(_), None) => None,
    };
    match operation_type {
        Some(OperationType::Query) | None => Ok(()),
        Some(other) => Err(AutumnError::bad_request_msg(format!(
            "{other} operations are not allowed over GET; use POST"
        ))
        .with_status(StatusCode::METHOD_NOT_ALLOWED)),
    }
}

/// `GET {path}/sdl` — the schema in GraphQL SDL, as `text/plain`.
async fn sdl<Q, M, S>(Extension(schema): Extension<Schema<Q, M, S>>) -> impl IntoResponse
where
    Q: ObjectType + 'static,
    M: ObjectType + 'static,
    S: SubscriptionType + 'static,
{
    (
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        schema.sdl(),
    )
}
