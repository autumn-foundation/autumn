//! A generic GraphQL adapter for Autumn, packaged as a [`Plugin`].
//!
//! This module knows nothing about notes. Hand it any
//! [`async_graphql::Schema`] and it will:
//!
//! - mount `POST {path}` (JSON request body) and `GET {path}?query=…`
//!   (GraphQL-over-HTTP query-string form) through [`AppBuilder::nest`];
//! - mount `GET {path}/sdl`, serving the schema definition language so a
//!   client can generate types from the running server;
//! - inject the request's [`AppState`] into every execution's context data,
//!   so resolvers reach `AppState` extensions (stores, services, pools) the
//!   same way a route handler does;
//! - install the schema itself as an `AppState` extension (via
//!   [`AppBuilder::state_initializer`]), which is how the handlers below find
//!   it and how the app can fetch it back;
//! - declare its routes with [`AppBuilder::declare_plugin_routes`], so
//!   `autumn routes` lists them with plugin attribution and
//!   `autumn routes audit` sees a covered mount rather than an opaque router;
//! - state a [`PluginContract`] naming the `autumn-web` series it targets.
//!
//! It lives inside this example for readability, but it is exactly the shape
//! a published `autumn-plugin-graphql` crate would take — nothing here depends
//! on the surrounding crate.

use std::borrow::Cow;
use std::sync::Arc;

use async_graphql::{ObjectType, Schema, SubscriptionType};
use autumn_web::app::AppBuilder;
use autumn_web::plugin::Plugin;
use autumn_web::plugin_contract::PluginContract;
use autumn_web::route_listing::{RouteClassification, RouteInfo};
use autumn_web::{AppState, AutumnError, AutumnResult};
use axum::Router;
use axum::extract::{RawQuery, State};
use axum::http::header;
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
        }
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
    /// Every route is classified [`RouteClassification::Public`]: the plugin
    /// mounts no guard of its own. Wrap the mount in your own layer (or put
    /// the app behind a session) if the schema must not be public.
    #[must_use]
    pub fn route_infos(&self) -> Vec<RouteInfo> {
        let mut routes = vec![
            route("POST", self.path.clone(), "graphql_plugin::post_graphql"),
            route("GET", self.path.clone(), "graphql_plugin::get_graphql"),
        ];
        if self.serve_sdl {
            routes.push(route(
                "GET",
                format!("{}/sdl", self.path),
                "graphql_plugin::sdl",
            ));
        }
        routes
    }

    /// The raw axum router `build` nests at `path`.
    fn router(&self) -> Router<AppState> {
        let mut router = Router::new().route(
            "/",
            post(post_graphql::<Q, M, S>).get(get_graphql::<Q, M, S>),
        );
        if self.serve_sdl {
            router = router.route("/sdl", get(sdl::<Q, M, S>));
        }
        router
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

    fn build(self, app: AppBuilder) -> AppBuilder {
        let router = self.router();
        let routes = self.route_infos();
        tracing::info!(path = %self.path, sdl = self.serve_sdl, "mounting GraphQL endpoint");
        let schema = self.schema;
        app.state_initializer(move |state| state.insert_extension(schema))
            .nest(&self.path, router)
            .declare_plugin_routes(routes)
    }
}

fn route(method: &str, path: String, handler: &str) -> RouteInfo {
    RouteInfo {
        method: method.to_owned(),
        path,
        handler: handler.to_owned(),
        classification: RouteClassification::Public,
        ..Default::default()
    }
}

/// Fetch the schema `build` installed on `AppState`.
fn schema<Q, M, S>(state: &AppState) -> AutumnResult<Arc<Schema<Q, M, S>>>
where
    Q: ObjectType + 'static,
    M: ObjectType + 'static,
    S: SubscriptionType + 'static,
{
    state
        .extension::<Schema<Q, M, S>>()
        .ok_or_else(|| AutumnError::internal_server_error_msg("GraphQL schema is not installed"))
}

/// Execute one request, with `AppState` available to resolvers as context data.
async fn execute<Q, M, S>(
    state: AppState,
    request: async_graphql::Request,
) -> AutumnResult<axum::Json<async_graphql::Response>>
where
    Q: ObjectType + 'static,
    M: ObjectType + 'static,
    S: SubscriptionType + 'static,
{
    let schema = schema::<Q, M, S>(&state)?;
    let response = schema.execute(request.data(state)).await;
    Ok(axum::Json(response))
}

/// `POST {path}` with a JSON body of `{ query, variables?, operationName? }`.
async fn post_graphql<Q, M, S>(
    State(state): State<AppState>,
    axum::Json(request): axum::Json<async_graphql::Request>,
) -> AutumnResult<axum::Json<async_graphql::Response>>
where
    Q: ObjectType + 'static,
    M: ObjectType + 'static,
    S: SubscriptionType + 'static,
{
    execute::<Q, M, S>(state, request).await
}

/// `GET {path}?query=…&variables=…&operationName=…` — the query-string form
/// from the GraphQL-over-HTTP spec, handy for `curl` and for cacheable reads.
async fn get_graphql<Q, M, S>(
    State(state): State<AppState>,
    RawQuery(query_string): RawQuery,
) -> AutumnResult<axum::Json<async_graphql::Response>>
where
    Q: ObjectType + 'static,
    M: ObjectType + 'static,
    S: SubscriptionType + 'static,
{
    let raw =
        query_string.ok_or_else(|| AutumnError::bad_request_msg("missing `query` parameter"))?;
    let request = async_graphql::http::parse_query_string(&raw)
        .map_err(|err| AutumnError::bad_request_msg(format!("invalid GraphQL request: {err}")))?;
    execute::<Q, M, S>(state, request).await
}

/// `GET {path}/sdl` — the schema in GraphQL SDL, as `text/plain`.
async fn sdl<Q, M, S>(State(state): State<AppState>) -> AutumnResult<impl IntoResponse>
where
    Q: ObjectType + 'static,
    M: ObjectType + 'static,
    S: SubscriptionType + 'static,
{
    let schema = schema::<Q, M, S>(&state)?;
    Ok((
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        schema.sdl(),
    ))
}
