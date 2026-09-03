//! The rules: what counts as *widening* an app's security surface.
//!
//! Every finding is derived from two manifests, never from source. A route is
//! identified by `(path, method)`; anything cosmetic — the handler's name, the
//! file and line it lives on, the module it was moved into — is invisible here
//! by construction, because a refactor that flags the security gate is a gate
//! nobody keeps.

use std::collections::{BTreeMap, BTreeSet};

use super::model::{PostureManifest, RouteEntry, RouteKey, is_open};

/// Which way a change moves the security surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// More reachable than before. Blocks until acknowledged.
    Widening,
    /// Changed, but not provably in either direction. Annotates only.
    Neutral,
    /// Less reachable than before. Annotates only.
    Narrowing,
}

impl Severity {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Widening => "widening",
            Self::Neutral => "neutral",
            Self::Narrowing => "narrowing",
        }
    }
}

/// One posture change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// Stable machine tag, e.g. `route_added_public`. Part of the acknowledgment
    /// digest, so renaming one invalidates existing acknowledgments — treat it
    /// as a wire format.
    pub kind: &'static str,
    pub severity: Severity,
    /// HTTP method, or `*` for a finding that is not about one route.
    pub method: String,
    /// Route path, header name, or `*`.
    pub path: String,
    /// Posture before, in the base manifest.
    pub before: String,
    /// Posture after, in the head manifest.
    pub after: String,
    /// One sentence naming what actually moved.
    pub detail: String,
}

impl Finding {
    /// The canonical line this finding contributes to the acknowledgment digest.
    #[must_use]
    pub fn canonical(&self) -> String {
        format!(
            "{}\t{}\t{}\t{}\t{}",
            self.kind, self.method, self.path, self.before, self.after
        )
    }
}

/// Compare two manifests, newest posture last.
///
/// Findings come back in a stable order — widening first (that is what a
/// reviewer must read), then neutral, then narrowing, each sorted by path and
/// method — so the report and the acknowledgment digest are deterministic.
#[must_use]
pub fn diff(base: &PostureManifest, head: &PostureManifest) -> Vec<Finding> {
    let mut findings = Vec::new();
    diff_routes(base, head, &mut findings);
    diff_authorization_policies(base, head, &mut findings);
    diff_csrf(base, head, &mut findings);
    diff_headers(base, head, &mut findings);
    findings.sort_by(|a, b| {
        a.severity
            .cmp(&b.severity)
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| a.method.cmp(&b.method))
            .then_with(|| a.kind.cmp(b.kind))
    });
    findings
}

/// Only the findings that block.
#[must_use]
pub fn widening(findings: &[Finding]) -> Vec<&Finding> {
    findings
        .iter()
        .filter(|f| f.severity == Severity::Widening)
        .collect()
}

fn route_index(m: &PostureManifest) -> BTreeMap<RouteKey, &RouteEntry> {
    m.dimensions
        .routes
        .entries
        .iter()
        .map(|e| (e.key(), e))
        .collect()
}

fn diff_routes(base: &PostureManifest, head: &PostureManifest, out: &mut Vec<Finding>) {
    let before = route_index(base);
    let after = route_index(head);

    for (key, entry) in &after {
        let Some(previous) = before.get(key) else {
            let open = is_open(&entry.classification);
            out.push(Finding {
                kind: if open {
                    "route_added_open"
                } else {
                    "route_added_gated"
                },
                severity: if open {
                    Severity::Widening
                } else {
                    Severity::Neutral
                },
                method: entry.method.clone(),
                path: entry.path.clone(),
                before: "absent".to_owned(),
                after: entry.posture_label(),
                detail: if open {
                    format!(
                        "new route reachable without a proven guard ({})",
                        entry.posture_label()
                    )
                } else {
                    "new guarded route".to_owned()
                },
            });
            continue;
        };
        compare_route(previous, entry, out);
    }

    for (key, entry) in &before {
        if !after.contains_key(key) {
            out.push(Finding {
                kind: "route_removed",
                severity: Severity::Narrowing,
                method: entry.method.clone(),
                path: entry.path.clone(),
                before: entry.posture_label(),
                after: "absent".to_owned(),
                detail: "route no longer mounted".to_owned(),
            });
        }
    }
}

/// Compare one route that exists on both sides.
fn compare_route(before: &RouteEntry, after: &RouteEntry, out: &mut Vec<Finding>) {
    let label_before = before.posture_label();
    let label_after = after.posture_label();

    if before.classification != after.classification {
        let was_open = is_open(&before.classification);
        let now_open = is_open(&after.classification);
        let (kind, severity, detail) = match (was_open, now_open) {
            (false, true) => (
                "classification_downgraded",
                Severity::Widening,
                format!(
                    "guard removed: {} \u{2192} {}",
                    before.classification, after.classification
                ),
            ),
            (true, false) => (
                "classification_upgraded",
                Severity::Narrowing,
                format!(
                    "guard added: {} \u{2192} {}",
                    before.classification, after.classification
                ),
            ),
            _ => (
                "classification_changed",
                Severity::Neutral,
                format!(
                    "classification {} \u{2192} {}",
                    before.classification, after.classification
                ),
            ),
        };
        out.push(Finding {
            kind,
            severity,
            method: after.method.clone(),
            path: after.path.clone(),
            before: label_before.clone(),
            after: label_after.clone(),
            detail,
        });
        // A route that stopped being gated has already been reported in the
        // strongest terms available; enumerating the roles it also lost would
        // add rows without adding information.
        if was_open != now_open {
            return;
        }
    }

    compare_roles(before, after, &label_before, &label_after, out);
    compare_scopes(before, after, &label_before, &label_after, out);

    if before.policy && !after.policy {
        out.push(Finding {
            kind: "policy_removed",
            severity: Severity::Widening,
            method: after.method.clone(),
            path: after.path.clone(),
            before: label_before,
            after: label_after,
            detail: "record-level policy check removed".to_owned(),
        });
    } else if !before.policy && after.policy {
        out.push(Finding {
            kind: "policy_added",
            severity: Severity::Narrowing,
            method: after.method.clone(),
            path: after.path.clone(),
            before: label_before,
            after: label_after,
            detail: "record-level policy check added".to_owned(),
        });
    }
}

/// Roles are OR-ed (`#[secured("a", "b")]` admits *either*), so **adding** one
/// admits more principals and **removing** one admits fewer — the opposite of
/// the intuition scopes create. Emptying the list entirely is the widest move
/// of all: `#[secured]` with no roles admits every authenticated session.
fn compare_roles(
    before: &RouteEntry,
    after: &RouteEntry,
    label_before: &str,
    label_after: &str,
    out: &mut Vec<Finding>,
) {
    let was: BTreeSet<String> = before.role_set();
    let now: BTreeSet<String> = after.role_set();
    if was == now {
        return;
    }
    let added: Vec<String> = now.difference(&was).cloned().collect();
    let removed: Vec<String> = was.difference(&now).cloned().collect();

    if !was.is_empty() && now.is_empty() {
        out.push(Finding {
            kind: "roles_cleared",
            severity: Severity::Widening,
            method: after.method.clone(),
            path: after.path.clone(),
            before: label_before.to_owned(),
            after: label_after.to_owned(),
            detail: format!(
                "role requirement dropped ({}) — any authenticated session now passes",
                removed.join(", ")
            ),
        });
        return;
    }
    if !added.is_empty() {
        out.push(Finding {
            kind: "roles_widened",
            severity: Severity::Widening,
            method: after.method.clone(),
            path: after.path.clone(),
            before: label_before.to_owned(),
            after: label_after.to_owned(),
            detail: format!(
                "role{} {} now also admitted",
                plural(added.len()),
                added.join(", ")
            ),
        });
    }
    if !removed.is_empty() {
        out.push(Finding {
            kind: "roles_narrowed",
            severity: Severity::Narrowing,
            method: after.method.clone(),
            path: after.path.clone(),
            before: label_before.to_owned(),
            after: label_after.to_owned(),
            detail: format!(
                "role{} {} no longer admitted",
                plural(removed.len()),
                removed.join(", ")
            ),
        });
    }
}

/// Scopes are AND-ed (`__check_secured_scopes` requires *all* of them), so
/// **removing** one lets more tokens through and **adding** one lets fewer.
fn compare_scopes(
    before: &RouteEntry,
    after: &RouteEntry,
    label_before: &str,
    label_after: &str,
    out: &mut Vec<Finding>,
) {
    let was: BTreeSet<String> = before.scope_set();
    let now: BTreeSet<String> = after.scope_set();
    if was == now {
        return;
    }
    let added: Vec<String> = now.difference(&was).cloned().collect();
    let removed: Vec<String> = was.difference(&now).cloned().collect();

    if !removed.is_empty() {
        out.push(Finding {
            kind: "scopes_widened",
            severity: Severity::Widening,
            method: after.method.clone(),
            path: after.path.clone(),
            before: label_before.to_owned(),
            after: label_after.to_owned(),
            detail: format!(
                "scope{} {} no longer required",
                plural(removed.len()),
                removed.join(", ")
            ),
        });
    }
    if !added.is_empty() {
        out.push(Finding {
            kind: "scopes_narrowed",
            severity: Severity::Narrowing,
            method: after.method.clone(),
            path: after.path.clone(),
            before: label_before.to_owned(),
            after: label_after.to_owned(),
            detail: format!(
                "scope{} {} now required",
                plural(added.len()),
                added.join(", ")
            ),
        });
    }
}

const fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// `#[authorize("action", resource = Resource)]` bindings, keyed by route.
fn authz_index(m: &PostureManifest) -> BTreeSet<(String, String, String, String)> {
    m.dimensions
        .authorization_policies
        .entries
        .iter()
        .map(|e| {
            (
                e.path.clone(),
                e.method.clone(),
                e.action.clone(),
                e.resource.clone(),
            )
        })
        .collect()
}

fn diff_authorization_policies(
    base: &PostureManifest,
    head: &PostureManifest,
    out: &mut Vec<Finding>,
) {
    let before = authz_index(base);
    let after = authz_index(head);
    let routes_after: BTreeSet<RouteKey> = head
        .dimensions
        .routes
        .entries
        .iter()
        .map(RouteEntry::key)
        .collect();

    for (path, method, action, resource) in before.difference(&after) {
        // A binding that vanished because the whole route did is already
        // reported as `route_removed`, and that is a narrowing, not a widening.
        if !routes_after.contains(&(path.clone(), method.clone())) {
            continue;
        }
        out.push(Finding {
            kind: "authorization_binding_removed",
            severity: Severity::Widening,
            method: method.clone(),
            path: path.clone(),
            before: format!("authorize({action}, {resource})"),
            after: "none".to_owned(),
            detail: format!(
                "record-level authorization `{action}` on `{resource}` no longer checked"
            ),
        });
    }
    for (path, method, action, resource) in after.difference(&before) {
        out.push(Finding {
            kind: "authorization_binding_added",
            severity: Severity::Narrowing,
            method: method.clone(),
            path: path.clone(),
            before: "none".to_owned(),
            after: format!("authorize({action}, {resource})"),
            detail: format!("record-level authorization `{action}` on `{resource}` now checked"),
        });
    }
}

/// `(path, method) -> (enforced, exempt)`.
fn csrf_index(m: &PostureManifest) -> BTreeMap<RouteKey, (bool, bool)> {
    m.dimensions
        .csrf
        .entries
        .iter()
        .map(|e| {
            (
                (e.path.clone(), e.method.clone()),
                (e.csrf_enforced, e.exempt),
            )
        })
        .collect()
}

fn diff_csrf(base: &PostureManifest, head: &PostureManifest, out: &mut Vec<Finding>) {
    let before = csrf_index(base);
    let after = csrf_index(head);

    let mut lost: Vec<(RouteKey, bool)> = Vec::new();
    let mut gained: Vec<RouteKey> = Vec::new();
    for (key, (enforced_now, exempt_now)) in &after {
        match before.get(key).map(|(enforced, _)| *enforced) {
            Some(true) if !enforced_now => lost.push((key.clone(), *exempt_now)),
            Some(false) if *enforced_now => gained.push(key.clone()),
            _ => {}
        }
    }

    // One collapsed finding when CSRF went off everywhere: an app that flips
    // `security.csrf.enabled` produces one row per mutating route otherwise,
    // and a 200-row table is a table nobody reads.
    let all_off_now = !after.is_empty() && after.values().all(|(enforced, _)| !enforced);
    let any_on_before = before.values().any(|(enforced, _)| *enforced);
    if all_off_now && any_on_before && lost.len() > 1 {
        out.push(Finding {
            kind: "csrf_disabled",
            severity: Severity::Widening,
            method: "*".to_owned(),
            path: "*".to_owned(),
            before: "csrf enforced".to_owned(),
            after: "csrf not enforced".to_owned(),
            detail: format!(
                "CSRF enforcement lost on all {} mutating routes",
                lost.len()
            ),
        });
    } else {
        for ((path, method), exempt) in lost {
            out.push(Finding {
                kind: "csrf_enforcement_removed",
                severity: Severity::Widening,
                method,
                path,
                before: "csrf enforced".to_owned(),
                after: "csrf not enforced".to_owned(),
                detail: if exempt {
                    "CSRF enforcement lost: this route now matches a configured exempt prefix"
                        .to_owned()
                } else {
                    "CSRF enforcement lost".to_owned()
                },
            });
        }
    }
    for (path, method) in gained {
        out.push(Finding {
            kind: "csrf_enforcement_added",
            severity: Severity::Narrowing,
            method,
            path,
            before: "csrf not enforced".to_owned(),
            after: "csrf enforced".to_owned(),
            detail: "CSRF enforcement gained".to_owned(),
        });
    }
}

fn diff_headers(base: &PostureManifest, head: &PostureManifest, out: &mut Vec<Finding>) {
    let before: BTreeMap<&str, (&bool, &str)> = base
        .dimensions
        .security_headers
        .entries
        .iter()
        .map(|e| (e.header.as_str(), (&e.emitted, e.value.as_str())))
        .collect();
    let after: BTreeMap<&str, (&bool, &str)> = head
        .dimensions
        .security_headers
        .entries
        .iter()
        .map(|e| (e.header.as_str(), (&e.emitted, e.value.as_str())))
        .collect();

    for (header, (emitted_now, value_now)) in &after {
        let Some((emitted_before, value_before)) = before.get(header) else {
            continue;
        };
        if **emitted_before && !**emitted_now {
            out.push(Finding {
                kind: "security_header_removed",
                severity: Severity::Widening,
                method: "*".to_owned(),
                path: (*header).to_owned(),
                before: (*value_before).to_owned(),
                after: "not emitted".to_owned(),
                detail: format!("security header `{header}` is no longer emitted"),
            });
        } else if !**emitted_before && **emitted_now {
            out.push(Finding {
                kind: "security_header_added",
                severity: Severity::Narrowing,
                method: "*".to_owned(),
                path: (*header).to_owned(),
                before: "not emitted".to_owned(),
                after: (*value_now).to_owned(),
                detail: format!("security header `{header}` is now emitted"),
            });
        } else if value_before != value_now {
            // Deliberately neutral. Whether one CSP is weaker than another is
            // not decidable from the strings, and a gate that blocks on
            // "the CSP changed" is a gate teams turn off. It is reported so a
            // human can look, never so a robot can refuse.
            out.push(Finding {
                kind: "security_header_value_changed",
                severity: Severity::Neutral,
                method: "*".to_owned(),
                path: (*header).to_owned(),
                before: (*value_before).to_owned(),
                after: (*value_now).to_owned(),
                detail: format!("security header `{header}` value changed — review by eye"),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::posture::model::PostureManifest;

    /// Build a manifest from route entries plus optional other dimensions.
    fn manifest(routes: &str, csrf: &str, headers: &str, authz: &str) -> PostureManifest {
        let json = format!(
            r#"{{"schema_version":3,"dimensions":{{
                 "routes":{{"provenance":"provable","source":"m","entries":[{routes}]}},
                 "csrf":{{"provenance":"declared","source":"c","exempt_paths":[],"entries":[{csrf}]}},
                 "security_headers":{{"provenance":"declared","source":"c","entries":[{headers}]}},
                 "authorization_policies":{{"provenance":"provable","source":"m","runtime_caveat":"x","entries":[{authz}]}}
               }},"excluded":[]}}"#
        );
        PostureManifest::parse(&json, "test.json").expect("fixture parses")
    }

    fn routes_only(routes: &str) -> PostureManifest {
        manifest(routes, "", "", "")
    }

    /// One route entry, spelled the way the manifest spells it.
    fn route(
        path: &str,
        method: &str,
        classification: &str,
        roles: &[&str],
        scopes: &[&str],
        policy: bool,
    ) -> String {
        let roles = roles
            .iter()
            .map(|r| format!("\"{r}\""))
            .collect::<Vec<_>>()
            .join(",");
        let scopes = scopes
            .iter()
            .map(|s| format!("\"{s}\""))
            .collect::<Vec<_>>()
            .join(",");
        format!(
            r#"{{"path":"{path}","method":"{method}","name":"h","classification":"{classification}",
                 "roles":[{roles}],"scopes":[{scopes}],"policy":{policy},"source":"user",
                 "location":"src/routes.rs:1","provenance":"provable"}}"#
        )
    }

    fn kinds(findings: &[Finding]) -> Vec<&'static str> {
        findings.iter().map(|f| f.kind).collect()
    }

    fn only(findings: Vec<Finding>) -> Finding {
        assert_eq!(
            findings.len(),
            1,
            "expected exactly one finding: {findings:#?}"
        );
        findings.into_iter().next().expect("one finding")
    }

    // ── the falsifiability trio from the issue ──────────────────────────────

    /// AC-7 (red): a role-gated route flipped to public is a widening finding
    /// that names the route.
    #[test]
    fn flipping_a_role_gated_route_to_public_is_a_widening_that_names_it() {
        let base = routes_only(&route(
            "/admin/users",
            "GET",
            "gated",
            &["admin"],
            &[],
            false,
        ));
        let head = routes_only(&route("/admin/users", "GET", "public", &[], &[], false));

        let findings = diff(&base, &head);
        let f = only(findings.clone());
        assert_eq!(f.kind, "classification_downgraded");
        assert_eq!(f.severity, Severity::Widening);
        assert_eq!(f.path, "/admin/users");
        assert_eq!(f.method, "GET");
        assert!(f.before.contains("gated"), "{f:?}");
        assert!(f.before.contains("admin"), "{f:?}");
        assert_eq!(f.after, "public");
        assert_eq!(widening(&findings).len(), 1);
    }

    /// AC-7 (green): the same handler renamed and moved to another file is not
    /// a posture change at all.
    #[test]
    fn a_cosmetic_refactor_produces_no_finding() {
        let base = routes_only(
            r#"{"path":"/admin/users","method":"GET","name":"list_users","classification":"gated",
                "roles":["admin"],"scopes":[],"policy":false,"source":"user",
                "location":"src/routes/admin.rs:12","module":"routes::admin","provenance":"provable"}"#,
        );
        let head = routes_only(
            r#"{"path":"/admin/users","method":"GET","name":"index","classification":"gated",
                "roles":["admin"],"scopes":[],"policy":false,"source":"user",
                "location":"src/routes/admin/users.rs:88","module":"routes::admin::users","provenance":"provable"}"#,
        );
        assert!(diff(&base, &head).is_empty(), "{:#?}", diff(&base, &head));
    }

    /// A PR that changes nothing at all is silent.
    #[test]
    fn an_identical_manifest_produces_no_finding() {
        let m = manifest(
            &route("/a", "GET", "gated", &["admin"], &[], false),
            r#"{"path":"/a","method":"POST","csrf_enforced":true,"exempt":false}"#,
            r#"{"header":"x_frame_options","value":"DENY","emitted":true}"#,
            r#"{"path":"/a","method":"GET","name":"h","action":"read","resource":"Post","provenance":"provable"}"#,
        );
        let m2 = manifest(
            &route("/a", "GET", "gated", &["admin"], &[], false),
            r#"{"path":"/a","method":"POST","csrf_enforced":true,"exempt":false}"#,
            r#"{"header":"x_frame_options","value":"DENY","emitted":true}"#,
            r#"{"path":"/a","method":"GET","name":"h","action":"read","resource":"Post","provenance":"provable"}"#,
        );
        assert!(diff(&m, &m2).is_empty());
    }

    // ── added / removed routes ──────────────────────────────────────────────

    #[test]
    fn a_new_public_route_is_widening() {
        let base = routes_only("");
        let head = routes_only(&route("/signup", "POST", "public", &[], &[], false));
        let f = only(diff(&base, &head));
        assert_eq!(f.kind, "route_added_open");
        assert_eq!(f.severity, Severity::Widening);
        assert_eq!(f.before, "absent");
    }

    #[test]
    fn a_new_unclassified_route_is_widening_too() {
        let base = routes_only("");
        let head = routes_only(&route("/oops", "POST", "unclassified", &[], &[], false));
        assert_eq!(only(diff(&base, &head)).severity, Severity::Widening);
    }

    #[test]
    fn a_new_gated_route_never_blocks() {
        let base = routes_only("");
        let head = routes_only(&route("/admin", "GET", "gated", &["admin"], &[], false));
        let f = only(diff(&base, &head));
        assert_eq!(f.kind, "route_added_gated");
        assert_eq!(f.severity, Severity::Neutral);
        assert!(widening(&diff(&base, &head)).is_empty());
    }

    #[test]
    fn a_new_framework_route_never_blocks() {
        let base = routes_only("");
        let head = routes_only(&route(
            "/actuator/health",
            "GET",
            "framework",
            &[],
            &[],
            false,
        ));
        assert_eq!(only(diff(&base, &head)).severity, Severity::Neutral);
    }

    #[test]
    fn a_removed_route_is_narrowing() {
        let base = routes_only(&route("/old", "GET", "public", &[], &[], false));
        let head = routes_only("");
        let f = only(diff(&base, &head));
        assert_eq!(f.kind, "route_removed");
        assert_eq!(f.severity, Severity::Narrowing);
    }

    #[test]
    fn the_same_path_on_a_different_method_is_a_different_route() {
        let base = routes_only(&route("/posts", "GET", "public", &[], &[], false));
        let head = routes_only(&format!(
            "{},{}",
            route("/posts", "GET", "public", &[], &[], false),
            route("/posts", "DELETE", "public", &[], &[], false)
        ));
        let f = only(diff(&base, &head));
        assert_eq!(f.method, "DELETE");
        assert_eq!(f.severity, Severity::Widening);
    }

    // ── gates ───────────────────────────────────────────────────────────────

    #[test]
    fn dropping_the_role_requirement_is_widening() {
        let base = routes_only(&route("/admin", "GET", "gated", &["admin"], &[], false));
        let head = routes_only(&route("/admin", "GET", "gated", &[], &[], false));
        let f = only(diff(&base, &head));
        assert_eq!(f.kind, "roles_cleared");
        assert_eq!(f.severity, Severity::Widening);
        assert!(f.detail.contains("admin"), "{f:?}");
    }

    /// Roles are OR-ed, so an extra role admits *more* people.
    #[test]
    fn adding_a_role_is_widening_because_roles_are_or_ed() {
        let base = routes_only(&route("/admin", "GET", "gated", &["admin"], &[], false));
        let head = routes_only(&route(
            "/admin",
            "GET",
            "gated",
            &["admin", "editor"],
            &[],
            false,
        ));
        let f = only(diff(&base, &head));
        assert_eq!(f.kind, "roles_widened");
        assert_eq!(f.severity, Severity::Widening);
        assert!(f.detail.contains("editor"), "{f:?}");
    }

    #[test]
    fn removing_one_of_several_roles_is_narrowing() {
        let base = routes_only(&route(
            "/admin",
            "GET",
            "gated",
            &["admin", "editor"],
            &[],
            false,
        ));
        let head = routes_only(&route("/admin", "GET", "gated", &["admin"], &[], false));
        let f = only(diff(&base, &head));
        assert_eq!(f.kind, "roles_narrowed");
        assert_eq!(f.severity, Severity::Narrowing);
    }

    #[test]
    fn reordering_roles_is_not_a_change() {
        let base = routes_only(&route(
            "/admin",
            "GET",
            "gated",
            &["admin", "editor"],
            &[],
            false,
        ));
        let head = routes_only(&route(
            "/admin",
            "GET",
            "gated",
            &["editor", "admin"],
            &[],
            false,
        ));
        assert!(diff(&base, &head).is_empty());
    }

    /// Scopes are AND-ed, so dropping one lets *more* tokens through.
    #[test]
    fn removing_a_scope_is_widening_because_scopes_are_and_ed() {
        let base = routes_only(&route(
            "/api",
            "POST",
            "gated",
            &[],
            &["posts:write", "admin"],
            false,
        ));
        let head = routes_only(&route(
            "/api",
            "POST",
            "gated",
            &[],
            &["posts:write"],
            false,
        ));
        let f = only(diff(&base, &head));
        assert_eq!(f.kind, "scopes_widened");
        assert_eq!(f.severity, Severity::Widening);
        assert!(f.detail.contains("admin"), "{f:?}");
    }

    #[test]
    fn adding_a_scope_is_narrowing() {
        let base = routes_only(&route(
            "/api",
            "POST",
            "gated",
            &[],
            &["posts:write"],
            false,
        ));
        let head = routes_only(&route(
            "/api",
            "POST",
            "gated",
            &[],
            &["posts:write", "admin"],
            false,
        ));
        assert_eq!(only(diff(&base, &head)).kind, "scopes_narrowed");
    }

    #[test]
    fn removing_the_policy_check_is_widening() {
        let base = routes_only(&route("/posts/1", "PUT", "gated", &["user"], &[], true));
        let head = routes_only(&route("/posts/1", "PUT", "gated", &["user"], &[], false));
        let f = only(diff(&base, &head));
        assert_eq!(f.kind, "policy_removed");
        assert_eq!(f.severity, Severity::Widening);
    }

    #[test]
    fn adding_the_policy_check_is_narrowing() {
        let base = routes_only(&route("/posts/1", "PUT", "gated", &["user"], &[], false));
        let head = routes_only(&route("/posts/1", "PUT", "gated", &["user"], &[], true));
        assert_eq!(only(diff(&base, &head)).severity, Severity::Narrowing);
    }

    #[test]
    fn gating_a_public_route_is_narrowing() {
        let base = routes_only(&route("/admin", "GET", "public", &[], &[], false));
        let head = routes_only(&route("/admin", "GET", "gated", &["admin"], &[], false));
        let f = only(diff(&base, &head));
        assert_eq!(f.kind, "classification_upgraded");
        assert_eq!(f.severity, Severity::Narrowing);
    }

    /// Losing the guard is reported once, in the strongest terms — not once for
    /// the classification and again for every role it took with it.
    #[test]
    fn a_route_that_loses_its_guard_reports_one_finding_not_three() {
        let base = routes_only(&route(
            "/admin",
            "GET",
            "gated",
            &["admin", "editor"],
            &["s"],
            true,
        ));
        let head = routes_only(&route("/admin", "GET", "public", &[], &[], false));
        let findings = diff(&base, &head);
        assert_eq!(kinds(&findings), vec!["classification_downgraded"]);
    }

    // ── authorization bindings ──────────────────────────────────────────────

    #[test]
    fn removing_an_authorize_binding_is_widening() {
        let r = route("/posts/1", "PUT", "gated", &["user"], &[], true);
        let base = manifest(
            &r,
            "",
            "",
            r#"{"path":"/posts/1","method":"PUT","name":"h","action":"update","resource":"Post","provenance":"provable"}"#,
        );
        let head = manifest(&r, "", "", "");
        let f = only(diff(&base, &head));
        assert_eq!(f.kind, "authorization_binding_removed");
        assert_eq!(f.severity, Severity::Widening);
        assert!(f.detail.contains("update"), "{f:?}");
        assert!(f.detail.contains("Post"), "{f:?}");
    }

    #[test]
    fn adding_an_authorize_binding_is_narrowing() {
        let r = route("/posts/1", "PUT", "gated", &["user"], &[], true);
        let base = manifest(&r, "", "", "");
        let head = manifest(
            &r,
            "",
            "",
            r#"{"path":"/posts/1","method":"PUT","name":"h","action":"update","resource":"Post","provenance":"provable"}"#,
        );
        assert_eq!(only(diff(&base, &head)).severity, Severity::Narrowing);
    }

    /// A binding that disappeared because the route did is not a second,
    /// widening finding — the route removal already says everything.
    #[test]
    fn a_binding_that_left_with_its_route_is_not_a_widening() {
        let base = manifest(
            &route("/posts/1", "DELETE", "gated", &["user"], &[], true),
            "",
            "",
            r#"{"path":"/posts/1","method":"DELETE","name":"h","action":"destroy","resource":"Post","provenance":"provable"}"#,
        );
        let head = manifest("", "", "", "");
        let findings = diff(&base, &head);
        assert_eq!(kinds(&findings), vec!["route_removed"]);
        assert!(widening(&findings).is_empty());
    }

    // ── CSRF ────────────────────────────────────────────────────────────────

    #[test]
    fn losing_csrf_enforcement_on_a_route_is_widening() {
        let base = manifest(
            "",
            r#"{"path":"/pay","method":"POST","csrf_enforced":true,"exempt":false}"#,
            "",
            "",
        );
        let head = manifest(
            "",
            r#"{"path":"/pay","method":"POST","csrf_enforced":false,"exempt":true}"#,
            "",
            "",
        );
        let f = only(diff(&base, &head));
        assert_eq!(f.kind, "csrf_enforcement_removed");
        assert_eq!(f.severity, Severity::Widening);
    }

    #[test]
    fn gaining_csrf_enforcement_is_narrowing() {
        let base = manifest(
            "",
            r#"{"path":"/pay","method":"POST","csrf_enforced":false,"exempt":true}"#,
            "",
            "",
        );
        let head = manifest(
            "",
            r#"{"path":"/pay","method":"POST","csrf_enforced":true,"exempt":false}"#,
            "",
            "",
        );
        assert_eq!(only(diff(&base, &head)).severity, Severity::Narrowing);
    }

    #[test]
    fn disabling_csrf_everywhere_collapses_into_one_finding() {
        let on = (0..5)
            .map(|i| {
                format!(r#"{{"path":"/r{i}","method":"POST","csrf_enforced":true,"exempt":false}}"#)
            })
            .collect::<Vec<_>>()
            .join(",");
        let off = (0..5)
            .map(|i| {
                format!(
                    r#"{{"path":"/r{i}","method":"POST","csrf_enforced":false,"exempt":false}}"#
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        let f = only(diff(
            &manifest("", &on, "", ""),
            &manifest("", &off, "", ""),
        ));
        assert_eq!(f.kind, "csrf_disabled");
        assert_eq!(f.severity, Severity::Widening);
        assert!(f.detail.contains('5'), "{f:?}");
    }

    // ── security headers ────────────────────────────────────────────────────

    #[test]
    fn dropping_a_security_header_is_widening() {
        let base = manifest(
            "",
            "",
            r#"{"header":"x_frame_options","value":"DENY","emitted":true}"#,
            "",
        );
        let head = manifest(
            "",
            "",
            r#"{"header":"x_frame_options","value":"","emitted":false}"#,
            "",
        );
        let f = only(diff(&base, &head));
        assert_eq!(f.kind, "security_header_removed");
        assert_eq!(f.severity, Severity::Widening);
    }

    #[test]
    fn adding_a_security_header_is_narrowing() {
        let base = manifest(
            "",
            "",
            r#"{"header":"referrer_policy","value":"","emitted":false}"#,
            "",
        );
        let head = manifest(
            "",
            "",
            r#"{"header":"referrer_policy","value":"no-referrer","emitted":true}"#,
            "",
        );
        assert_eq!(only(diff(&base, &head)).severity, Severity::Narrowing);
    }

    /// Whether one CSP is weaker than another is not decidable from the
    /// strings, so a value change annotates and never blocks.
    #[test]
    fn changing_a_security_header_value_is_neutral_not_widening() {
        let base = manifest(
            "",
            "",
            r#"{"header":"content_security_policy","value":"default-src 'self'","emitted":true}"#,
            "",
        );
        let head = manifest(
            "",
            "",
            r#"{"header":"content_security_policy","value":"default-src *","emitted":true}"#,
            "",
        );
        let f = only(diff(&base, &head));
        assert_eq!(f.kind, "security_header_value_changed");
        assert_eq!(f.severity, Severity::Neutral);
        assert!(widening(&diff(&base, &head)).is_empty());
    }

    // ── ordering ────────────────────────────────────────────────────────────

    #[test]
    fn widening_findings_are_listed_first_and_ordering_is_deterministic() {
        let base = routes_only(&format!(
            "{},{}",
            route("/keep", "GET", "gated", &["admin"], &[], false),
            route("/gone", "GET", "public", &[], &[], false)
        ));
        let head = routes_only(&format!(
            "{},{}",
            route("/keep", "GET", "public", &[], &[], false),
            route("/new", "GET", "public", &[], &[], false)
        ));
        let findings = diff(&base, &head);
        assert_eq!(findings[0].severity, Severity::Widening);
        assert_eq!(findings.last().unwrap().severity, Severity::Narrowing);
        // Same inputs, same order, every time.
        assert_eq!(findings, diff(&base, &head));
    }
}
