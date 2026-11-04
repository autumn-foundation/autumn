//! Per-role route mounting (#3544).
//!
//! A `[roles.<name>] mount = [...]` list names the route groups a role
//! serves. [`MountTargets::retain`] drops every route of another group from
//! the lists the router, the graph and the `OpenAPI` document read. Framework
//! routes are not in these lists, so every role keeps them.

use std::collections::BTreeSet;

use crate::app::ScopedGroup;
use crate::config::Role;
use crate::route::{Route, RouteGroup};
use crate::route_listing::{RouteInfo, RouteSource};
use crate::state::AppState;

/// A `mount` entry names a group that no route is in.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "role \"{role}\" mounts route group \"{group}\", but no route is in that group; \
     route groups: {}",
    known_groups_text(.known)
)]
pub struct UnknownMountGroup {
    /// The role name.
    pub role: String,
    /// The `mount` entry.
    pub group: String,
    /// The groups the app has, sorted.
    pub known: Vec<String>,
}

fn known_groups_text(known: &[String]) -> String {
    if known.is_empty() {
        "none".to_owned()
    } else {
        known.join(", ")
    }
}

/// Give each route without a group the group `group`.
///
/// # Panics
///
/// Panics if `group` is blank.
#[track_caller]
pub fn put_in_group(group: &RouteGroup, routes: Vec<Route>) -> Vec<Route> {
    assert!(
        !group.as_str().trim().is_empty(),
        "the route group name must not be blank"
    );
    routes
        .into_iter()
        .map(|route| {
            if route.group.is_some() {
                route
            } else {
                route.in_group(group.clone())
            }
        })
        .collect()
}

/// Whether the URL `path` matches the route `pattern`. `{name}` matches one
/// segment. `{*name}` matches the rest of the path.
pub fn path_matches(pattern: &str, path: &str) -> bool {
    let mut pattern = pattern.trim_matches('/').split('/');
    let mut path = path.trim_matches('/').split('/');
    loop {
        match (pattern.next(), path.next()) {
            (None, None) => return true,
            (Some(p), _) if p.starts_with("{*") => return true,
            (Some(p), Some(s)) if p.starts_with('{') && p.ends_with('}') && !s.is_empty() => {}
            (Some(p), Some(s)) if p == s => {}
            _ => return false,
        }
    }
}

/// The route lists of a builder that a role mount filters.
///
/// `route_sources` is parallel to `routes` when it has the same length. The
/// two `*_groups` lists are parallel to their routers: `None`, or a missing
/// entry, is shared and stays on every role.
pub struct MountTargets<'a> {
    pub routes: &'a mut Vec<Route>,
    pub route_sources: &'a mut Vec<RouteSource>,
    pub scoped_groups: &'a mut Vec<ScopedGroup>,
    pub merge_routers: &'a mut Vec<axum::Router<AppState>>,
    pub merge_router_groups: &'a mut Vec<Option<RouteGroup>>,
    pub nest_routers: &'a mut Vec<(String, axum::Router<AppState>)>,
    pub nest_router_groups: &'a mut Vec<Option<RouteGroup>>,
    pub declared_routes: &'a mut Vec<RouteInfo>,
}

impl MountTargets<'_> {
    /// Every route group in the lists, sorted.
    pub fn groups(&self) -> BTreeSet<String> {
        let mut groups: BTreeSet<String> = self
            .routes
            .iter()
            .chain(self.scoped_groups.iter().flat_map(|g| g.routes.iter()))
            .map(|route| route.group_name().to_owned())
            .collect();
        groups.extend(
            self.merge_router_groups
                .iter()
                .chain(self.nest_router_groups.iter())
                .flatten()
                .map(|group| group.as_str().to_owned()),
        );
        groups.extend(self.declared_routes.iter().filter_map(|r| r.group.clone()));
        groups
    }

    /// Drop the routes of every group `role` does not mount.
    ///
    /// A role without `mount` keeps everything. So does a role that serves no
    /// HTTP: it builds the probe-only router and reads none of the lists.
    ///
    /// # Errors
    ///
    /// Returns [`UnknownMountGroup`] when `mount` names a group that no route
    /// is in. The lists do not change then.
    pub fn retain(self, role: &Role) -> Result<(), UnknownMountGroup> {
        let Some(mount) = role.mount().filter(|_| role.serves_http()) else {
            return Ok(());
        };
        let known = self.groups();
        if let Some(group) = mount.iter().find(|group| !known.contains(group.as_str())) {
            return Err(UnknownMountGroup {
                role: role.name().to_owned(),
                group: group.clone(),
                known: known.into_iter().collect(),
            });
        }
        let keep = |group: &str| role.mounts_group(group);

        let routes = std::mem::take(self.routes);
        let sources = std::mem::take(self.route_sources);
        let parallel = sources.len() == routes.len();
        let mut sources = sources.into_iter();
        for route in routes {
            let source = if parallel { sources.next() } else { None };
            if keep(route.group_name()) {
                self.routes.push(route);
                self.route_sources.extend(source);
            }
        }

        for group in self.scoped_groups.iter_mut() {
            group.routes.retain(|route| keep(route.group_name()));
        }
        self.scoped_groups.retain(|group| !group.routes.is_empty());

        retain_tagged(self.merge_routers, self.merge_router_groups, &keep);
        retain_tagged(self.nest_routers, self.nest_router_groups, &keep);

        self.declared_routes
            .retain(|route| route.group.as_deref().is_none_or(keep));
        Ok(())
    }
}

/// Keep each item whose group `keep` admits. An item with no group, or with
/// no tag at all, is shared and stays.
fn retain_tagged<T>(
    items: &mut Vec<T>,
    groups: &mut Vec<Option<RouteGroup>>,
    keep: &impl Fn(&str) -> bool,
) {
    let all = std::mem::take(items);
    let tags = std::mem::take(groups);
    let mut tags = tags.into_iter();
    for item in all {
        let tag = tags.next().flatten();
        if tag.as_ref().is_none_or(|group| keep(group.as_str())) {
            items.push(item);
            groups.push(tag);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retain_tagged_keeps_shared_and_untagged_items() {
        let mut items = vec!["a", "b", "shared", "untagged"];
        let mut groups = vec![
            Some(RouteGroup::from_static("a")),
            Some(RouteGroup::from_static("b")),
            None,
        ];
        retain_tagged(&mut items, &mut groups, &|g| g == "a");
        assert_eq!(items, vec!["a", "shared", "untagged"]);
        assert_eq!(groups.len(), 3);
    }

    #[test]
    fn unknown_mount_group_names_the_known_groups() {
        let err = UnknownMountGroup {
            role: "site".to_owned(),
            group: "markting".to_owned(),
            known: vec!["app".to_owned(), "marketing".to_owned()],
        };
        let text = err.to_string();
        assert!(text.contains("\"markting\""), "{text}");
        assert!(text.contains("no route is in that group"), "{text}");
        assert!(text.ends_with("route groups: app, marketing"), "{text}");
    }

    #[test]
    fn path_matches_literal_param_and_catch_all_segments() {
        assert!(path_matches("/", "/"));
        assert!(path_matches("/about", "/about"));
        assert!(path_matches("/about", "/about/"));
        assert!(!path_matches("/about", "/team"));
        assert!(path_matches("/posts/{slug}", "/posts/hello"));
        assert!(!path_matches("/posts/{slug}", "/posts"));
        assert!(!path_matches("/posts/{slug}", "/posts/a/b"));
        assert!(path_matches("/docs/{*rest}", "/docs/a/b/c"));
        assert!(!path_matches("/", "/about"));
    }

    #[test]
    #[should_panic(expected = "must not be blank")]
    fn a_blank_group_is_refused() {
        let _ = put_in_group(&RouteGroup::from_static(" "), Vec::new());
    }
}
