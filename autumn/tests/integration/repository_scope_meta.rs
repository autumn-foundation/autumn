//! Pins the `scope_check` distribution across the auto-generated
//! `#[repository(api = ..., scope = ...)]` routes.
//!
//! Codex review (round 8) caught that emitting `scope_check` on
//! `*_api_get` / `*_api_create` / `*_api_update` / `*_api_delete`
//! made the prod-mode startup guard fire for missing-scope
//! registrations on apps that intentionally mounted only those
//! non-list routes — even though those handlers never call
//! `scope.list` at runtime. Only `*_api_list` should carry the
//! probe.

#![cfg(feature = "db")]

use autumn_web::authorization::{BoxFuture, Policy, PolicyContext, Scope};
use autumn_web::reexports::diesel_async::AsyncPgConnection;

mod schema {
    autumn_web::reexports::diesel::table! {
        scope_meta_widgets (id) {
            id -> Int8,
            name -> Text,
        }
    }
}

use schema::scope_meta_widgets;

#[autumn_web::model(table = "scope_meta_widgets")]
pub struct ScopeMetaWidget {
    #[id]
    pub id: i64,
    pub name: String,
}

#[derive(Default, Clone)]
pub struct ScopeMetaWidgetPolicy;
impl Policy<ScopeMetaWidget> for ScopeMetaWidgetPolicy {}

#[derive(Default, Clone)]
pub struct ScopeMetaWidgetScope;
impl Scope<ScopeMetaWidget> for ScopeMetaWidgetScope {
    fn list<'a>(
        &'a self,
        _ctx: &'a PolicyContext,
        _conn: &'a mut AsyncPgConnection,
    ) -> BoxFuture<'a, autumn_web::AutumnResult<Vec<ScopeMetaWidget>>> {
        Box::pin(async { Ok(Vec::new()) })
    }
}

#[autumn_web::repository(
    ScopeMetaWidget,
    table = "scope_meta_widgets",
    api = "/api/scope-meta-widgets",
    policy = ScopeMetaWidgetPolicy,
    scope = ScopeMetaWidgetScope,
)]
pub trait ScopeMetaWidgetRepository {}

#[test]
fn list_route_carries_scope_check_probe() {
    let meta = __autumn_route_info_scope_meta_widget_api_list()
        .repository
        .expect("repository meta present");
    assert!(
        meta.scope_check.is_some(),
        "list route must carry scope_check so the registry validator catches missing `.scope::<R, _>(...)`"
    );
    // policy_check is also present — list still uses can_show
    // when no scope is registered, but we have one here.
    assert!(meta.policy_check.is_some());
}

#[test]
fn get_route_does_not_carry_scope_check_probe() {
    let meta = __autumn_route_info_scope_meta_widget_api_get()
        .repository
        .expect("repository meta present");
    assert!(
        meta.scope_check.is_none(),
        "non-list routes must not carry scope_check — they never call scope.list at runtime"
    );
    assert!(
        meta.policy_check.is_some(),
        "non-list routes still call into Policy::can_*, so policy_check stays attached"
    );
}

#[test]
fn create_route_does_not_carry_scope_check_probe() {
    let meta = __autumn_route_info_scope_meta_widget_api_create()
        .repository
        .expect("repository meta present");
    assert!(meta.scope_check.is_none());
    assert!(meta.policy_check.is_some());
}

#[test]
fn update_route_does_not_carry_scope_check_probe() {
    let meta = __autumn_route_info_scope_meta_widget_api_update()
        .repository
        .expect("repository meta present");
    assert!(meta.scope_check.is_none());
    assert!(meta.policy_check.is_some());
}

#[test]
fn delete_route_does_not_carry_scope_check_probe() {
    let meta = __autumn_route_info_scope_meta_widget_api_delete()
        .repository
        .expect("repository meta present");
    assert!(meta.scope_check.is_none());
    assert!(meta.policy_check.is_some());
}
