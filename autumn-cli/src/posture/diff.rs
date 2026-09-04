//! The rules: what counts as *widening* an app's security surface.
//!
//! Every finding is derived from two manifests, never from source. A route is
//! identified by `(path, method)`; anything cosmetic — the handler's name, the
//! file and line it lives on, the module it was moved into — is invisible here
//! by construction, because a refactor that flags the security gate is a gate
//! nobody keeps.
//!
//! Two rules hold everywhere below, and both are load-bearing:
//!
//! 1. **Every dimension is compared from both sides.** A fact that *disappears*
//!    from the head manifest is a fact that was lost. Walking only the head
//!    would miss it — and the manifest drops entries as well as changing them:
//!    adding `POST` to `security.csrf.safe_methods` turns CSRF validation off
//!    for every POST *and* deletes those routes from the csrf dimension.
//! 2. **Each finding carries a [`Finding::fingerprint`]**: the security-relevant
//!    delta, escaped, and the only field besides kind/method/path that reaches
//!    the acknowledgment digest. Human-facing `before`/`after` text stays out of
//!    it, so a later *narrowing* on an already-acknowledged route does not
//!    invalidate the acknowledgment, and no crafted route path can make one
//!    finding hash like two.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use super::model::{
    CAPTURE, CATCH_ALL, PostureManifest, RouteEntry, RouteKey, escape_field, escape_list,
    hex_digest, is_open, normalize_captures,
};

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
    /// Stable machine tag, e.g. `route_added_open`. Part of the acknowledgment
    /// digest, so renaming one invalidates existing acknowledgments — treat it
    /// as a wire format.
    pub kind: &'static str,
    pub severity: Severity,
    /// HTTP method, or `*` for a finding that is not about one route.
    pub method: String,
    /// Route path, header name, or `*`.
    pub path: String,
    /// Posture before, in the base manifest. Human-facing.
    pub before: String,
    /// Posture after, in the head manifest. Human-facing.
    pub after: String,
    /// The security-relevant delta this finding *is*, in a form that changes
    /// exactly when its security meaning changes: `class:gated->public`,
    /// `roles+editor`, `scopes-admin`. Part of the acknowledgment digest.
    pub fingerprint: String,
    /// One sentence naming what actually moved.
    pub detail: String,
}

impl Finding {
    /// The canonical line this finding contributes to the acknowledgment digest.
    ///
    /// Every field is escaped before being joined, so a route path containing a
    /// tab or a newline cannot forge extra fields or extra lines. Without that,
    /// one crafted route hashes identically to a set of ordinary ones — and an
    /// acknowledgment for the crafted set silently covers the ordinary ones.
    #[must_use]
    pub fn canonical(&self) -> String {
        format!(
            "{}\t{}\t{}\t{}",
            self.kind,
            escape_field(&self.method),
            escape_field(&self.path),
            escape_field(&self.fingerprint)
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
            .then_with(|| a.fingerprint.cmp(&b.fingerprint))
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

/// Index routes by `(path, method)`, **merging** duplicate keys into the widest
/// posture their entries jointly describe.
///
/// A manifest should never carry the same key twice, but "should never" is not
/// a guarantee about a file on disk, and letting either entry win makes the
/// verdict depend on array order — with the order that hides the wider entry
/// being the one that passes.
///
/// Picking a winner cannot work, because two postures can be *incomparable*:
/// `roles: ["admin"]` and `roles: ["editor"]` neither contains the other, so
/// any ranking of them is arbitrary and one of the two orders hides a newly
/// admitted role. Merging has no such gap: the union of the two is at least as
/// wide as either, so the diff can only ever over-report.
fn route_index(m: &PostureManifest) -> BTreeMap<RouteKey, RouteEntry> {
    let mut index: BTreeMap<RouteKey, RouteEntry> = BTreeMap::new();
    for entry in &m.dimensions.routes.entries {
        index
            .entry(entry.key())
            .and_modify(|existing| *existing = widest_of(existing, entry))
            .or_insert_with(|| entry.clone());
    }
    index
}

/// The posture that admits every caller either of these two does, dimension by
/// dimension, in the direction the framework's own semantics give it.
fn widest_of(a: &RouteEntry, b: &RouteEntry) -> RouteEntry {
    // Roles are OR-ed, so the union admits at least as many principals — and an
    // *empty* list is widest of all, since `#[secured]` with no roles admits
    // every authenticated session.
    let roles = if a.roles.is_empty() || b.roles.is_empty() {
        Vec::new()
    } else {
        a.role_set().union(&b.role_set()).cloned().collect()
    };
    // Scopes are AND-ed, so requiring only what both require admits at least as
    // many tokens.
    let scopes = a
        .scope_set()
        .intersection(&b.scope_set())
        .cloned()
        .collect();
    RouteEntry {
        path: a.path.clone(),
        method: a.method.clone(),
        classification: if is_open(&a.classification) {
            a.classification.clone()
        } else {
            b.classification.clone()
        },
        roles,
        scopes,
        // A record-level check only holds if every entry claims it.
        policy: a.policy && b.policy,
    }
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
                fingerprint: format!("added:{}", entry.classification),
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
                fingerprint: "removed".to_owned(),
                detail: "route no longer mounted".to_owned(),
            });
            report_shadow_exposure(key, entry, &after, out);
        }
    }
}

/// Deleting a route does not always remove the URL.
///
/// The router matches a static segment before a dynamic one and mounts both —
/// `/users/me` beside `/users/{id}` is not a conflict, per the conflict matrix
/// in `router.rs`. So deleting the gated static route hands its URL to whatever
/// still covers it. Read as a plain removal that is a narrowing, and the guard
/// is gone with nothing to acknowledge.
fn report_shadow_exposure(
    (path, method): &RouteKey,
    removed: &RouteEntry,
    after: &BTreeMap<RouteKey, RouteEntry>,
    out: &mut Vec<Finding>,
) {
    for ((survivor_path, survivor_method), survivor) in after {
        if survivor_method != method || !shapes_overlap(path, survivor_path) {
            continue;
        }
        // Only a survivor that admits callers the deleted route refused is a
        // widening — and "admits more" is decided by the same comparison the
        // rest of this module uses, so the two cannot drift apart.
        let mut probe = Vec::new();
        compare_route(removed, survivor, &mut probe);
        if !probe.iter().any(|f| f.severity == Severity::Widening) {
            continue;
        }
        out.push(Finding {
            kind: "route_shadow_exposed",
            severity: Severity::Widening,
            method: method.clone(),
            path: removed.path.clone(),
            before: removed.posture_label(),
            after: survivor.posture_label(),
            fingerprint: format!("shadow-exposed:{}", escape_field(&survivor.path)),
            detail: format!(
                "removing this route does not remove the URL: `{} {}` still matches it and \
                 admits callers this route refused ({} → {})",
                survivor.method,
                survivor.path,
                removed.posture_label(),
                survivor.posture_label()
            ),
        });
    }
}

/// Whether two route *shapes* can match the same URL.
///
/// Both paths arrive normalized, so a capture is `CAPTURE` and a catch-all is
/// `CATCH_ALL`. Overlap, not equality, is the question: the router mounts a
/// static route beside a dynamic one that covers it, so removing the static one
/// is only safe if nothing wider is left underneath.
fn shapes_overlap(a: &str, b: &str) -> bool {
    let a: Vec<&str> = a.split('/').collect();
    let b: Vec<&str> = b.split('/').collect();
    for i in 0..a.len().max(b.len()) {
        match (a.get(i), b.get(i)) {
            (Some(x), Some(y)) => {
                // A catch-all swallows every remaining segment.
                if x.contains(CATCH_ALL) || y.contains(CATCH_ALL) {
                    return true;
                }
                if !segments_overlap(x, y) {
                    return false;
                }
            }
            // One path ran out: the other still demands a segment, and a
            // capture matches text, never nothing.
            _ => return false,
        }
    }
    true
}

/// Whether one path segment can match the same text as another.
fn segments_overlap(a: &str, b: &str) -> bool {
    if !a.contains(CAPTURE) && !b.contains(CAPTURE) {
        return a == b;
    }
    // A capture matches any non-empty text in its position, but the literal
    // text around it still has to line up — `/file.{ext}` covers `/file.json`
    // and not `/other.json`.
    let (a_prefix, a_suffix) = literal_edges(a);
    let (b_prefix, b_suffix) = literal_edges(b);
    (a_prefix.starts_with(b_prefix) || b_prefix.starts_with(a_prefix))
        && (a_suffix.ends_with(b_suffix) || b_suffix.ends_with(a_suffix))
}

/// The literal text before the first capture and after the last one. A segment
/// with no capture is its own prefix and suffix.
fn literal_edges(segment: &str) -> (&str, &str) {
    match (segment.find(CAPTURE), segment.rfind(CAPTURE)) {
        (Some(first), Some(last)) => (&segment[..first], &segment[last + CAPTURE.len_utf8()..]),
        _ => (segment, segment),
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
            fingerprint: format!("class:{}->{}", before.classification, after.classification),
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
            fingerprint: "policy-removed".to_owned(),
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
            fingerprint: "policy-added".to_owned(),
            detail: "record-level policy check added".to_owned(),
        });
    }
}

/// Roles are OR-ed (`#[secured("a", "b")]` admits *either*), so **adding** one
/// admits more principals and **removing** one admits fewer — the opposite of
/// the intuition scopes create.
///
/// Two boundary cases run the other way, and both come from the same fact:
/// `#[secured]` with *no* roles admits every authenticated session. So emptying
/// a non-empty list is the widest move available, and putting the first role on
/// an empty list is a narrowing, not a widening.
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
            fingerprint: format!("roles-cleared:{}", escape_list(&removed)),
            detail: format!(
                "role requirement dropped ({}) — any authenticated session now passes",
                removed.join(", ")
            ),
        });
        return;
    }
    if was.is_empty() {
        // The gate went from "any authenticated session" to "one of these
        // roles". Strictly fewer callers, however many roles were named.
        out.push(Finding {
            kind: "roles_narrowed",
            severity: Severity::Narrowing,
            method: after.method.clone(),
            path: after.path.clone(),
            before: label_before.to_owned(),
            after: label_after.to_owned(),
            fingerprint: format!("roles-first:{}", escape_list(&added)),
            detail: format!(
                "route now requires role{} {} — previously any authenticated session passed",
                plural(added.len()),
                added.join(", ")
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
            fingerprint: format!("roles+{}", escape_list(&added)),
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
            fingerprint: format!("roles-{}", escape_list(&removed)),
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
            fingerprint: format!("scopes-{}", escape_list(&removed)),
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
            fingerprint: format!("scopes+{}", escape_list(&added)),
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
///
/// The path is normalized exactly as route identity is, so a renamed capture
/// does not put one untouched binding on both sides of the set difference —
/// which read as a removal, and blocked a pull request that changed nothing.
/// The path as the author wrote it is the value, for the finding's text.
type AuthzKey = (String, String, String, String);

fn authz_index(m: &PostureManifest) -> BTreeMap<AuthzKey, String> {
    m.dimensions
        .authorization_policies
        .entries
        .iter()
        .map(|e| {
            (
                (
                    normalize_captures(&e.path),
                    e.method.clone(),
                    e.action.clone(),
                    e.resource.clone(),
                ),
                e.path.clone(),
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

    // Bindings the same route *gained*, so a removal that is really half of a
    // rename can say so instead of reading as an unexplained loss.
    let gained_on: BTreeMap<RouteKey, Vec<String>> =
        after.keys().filter(|key| !before.contains_key(*key)).fold(
            BTreeMap::new(),
            |mut acc, (path, method, action, resource)| {
                acc.entry((path.clone(), method.clone()))
                    .or_default()
                    .push(format!("{action} on {resource}"));
                acc
            },
        );

    for (key, written_as) in &before {
        if after.contains_key(key) {
            continue;
        }
        let (path, method, action, resource) = key;
        // A binding that vanished because the whole route did is already
        // reported as `route_removed`, and that is a narrowing, not a widening.
        if !routes_after.contains(&(path.clone(), method.clone())) {
            continue;
        }
        let mut detail =
            format!("record-level authorization `{action}` on `{resource}` no longer checked");

        // A resource *rename* looks exactly like a removal plus an addition,
        // and nothing in the manifest can tell the two apart. Stay conservative
        // — this still blocks — but name the pairing so a reviewer who is
        // looking at a rename can acknowledge it in one step instead of
        // wondering what was lost.
        if let Some(gained) = gained_on.get(&(path.clone(), method.clone())) {
            let _ = write!(
                detail,
                " (the same route gained: {}; a renamed resource reads as a removal plus an \
                 addition, which a manifest cannot tell from a real one)",
                gained.join(", ")
            );
        }
        out.push(Finding {
            kind: "authorization_binding_removed",
            severity: Severity::Widening,
            method: method.clone(),
            path: written_as.clone(),
            before: format!("authorize({action}, {resource})"),
            after: "none".to_owned(),
            fingerprint: format!(
                "authz-removed:{}:{}",
                escape_field(action),
                escape_field(resource)
            ),
            detail,
        });
    }
    for (key, written_as) in &after {
        if before.contains_key(key) {
            continue;
        }
        let (_, method, action, resource) = key;
        out.push(Finding {
            kind: "authorization_binding_added",
            severity: Severity::Narrowing,
            method: method.clone(),
            path: written_as.clone(),
            before: "none".to_owned(),
            after: format!("authorize({action}, {resource})"),
            fingerprint: format!(
                "authz-added:{}:{}",
                escape_field(action),
                escape_field(resource)
            ),
            detail: format!("record-level authorization `{action}` on `{resource}` now checked"),
        });
    }
}

/// `(normalized path, method) -> (enforced, exempt, path as written)`.
///
/// Keyed the way routes are. Raw paths let a renamed capture hide a CSRF loss
/// outright: the head entry matched no base entry, and the disappearance check
/// below could not catch it either, because `routes_after` holds normalized
/// keys. The path as the author wrote it rides along for the finding's text.
fn csrf_index(m: &PostureManifest) -> BTreeMap<RouteKey, (bool, bool, String)> {
    let mut index: BTreeMap<RouteKey, (bool, bool, String)> = BTreeMap::new();
    for e in &m.dimensions.csrf.entries {
        let key = (normalize_captures(&e.path), e.method.clone());
        index
            .entry(key)
            .and_modify(|(enforced, exempt, _)| {
                // Two entries for one route shape merge to the *widest* of
                // them, exactly as duplicate route entries do: whichever sorts
                // last must not get to declare the route protected.
                *enforced = *enforced && e.csrf_enforced;
                *exempt = *exempt || e.exempt;
            })
            .or_insert_with(|| (e.csrf_enforced, e.exempt, e.path.clone()));
    }
    index
}

fn diff_csrf(base: &PostureManifest, head: &PostureManifest, out: &mut Vec<Finding>) {
    let before = csrf_index(base);
    let after = csrf_index(head);
    let routes_after: BTreeSet<RouteKey> = head
        .dimensions
        .routes
        .entries
        .iter()
        .map(RouteEntry::key)
        .collect();

    // Keyed rather than pushed, so the two passes below cannot report the same
    // route twice and the order is the key order either way.
    let mut lost: BTreeMap<RouteKey, (String, bool)> = BTreeMap::new();
    let mut gained: Vec<(RouteKey, String)> = Vec::new();
    for (key, (enforced_now, exempt_now, written_as)) in &after {
        match before.get(key).map(|(enforced, ..)| *enforced) {
            Some(true) if !enforced_now => {
                lost.insert(key.clone(), (written_as.clone(), *exempt_now));
            }
            Some(false) if *enforced_now => gained.push((key.clone(), written_as.clone())),
            _ => {}
        }
    }
    // The other direction: an entry that *left* the dimension entirely. The
    // csrf dimension holds only mutating routes, so widening
    // `security.csrf.safe_methods` deletes entries rather than flipping them —
    // and at runtime those requests stop being validated.
    for (key, (enforced_before, _, written_as)) in &before {
        if *enforced_before && !after.contains_key(key) && routes_after.contains(key) {
            lost.entry(key.clone())
                .or_insert_with(|| (written_as.clone(), false));
        }
    }

    // One collapsed finding when CSRF went off everywhere: an app that flips
    // `security.csrf.enabled` produces one row per mutating route otherwise,
    // and a 200-row table is a table nobody reads.
    let all_off_now = after.values().all(|(enforced, ..)| !enforced);
    let any_on_before = before.values().any(|entry| entry.0);
    if all_off_now && any_on_before && lost.len() > 1 {
        let routes: Vec<String> = lost
            .iter()
            .map(|((_, method), (written_as, _))| format!("{method} {written_as}"))
            .collect();
        // The fingerprint names the *routes*, not the spellings: renaming a
        // capture changes neither the URL set that lost CSRF nor what a
        // reviewer acknowledged about it.
        let keys: Vec<String> = lost
            .keys()
            .map(|(path, method)| format!("{method} {path}"))
            .collect();
        out.push(Finding {
            kind: "csrf_disabled",
            severity: Severity::Widening,
            method: "*".to_owned(),
            path: "*".to_owned(),
            before: "csrf enforced".to_owned(),
            after: format!("csrf not enforced on {} routes", routes.len()),
            // The collapsed finding must still say *which* routes lost it, or
            // an acknowledgment for one set would silently cover another.
            fingerprint: format!(
                "csrf-disabled:{}",
                &hex_digest(escape_list(&keys).as_bytes())[..16]
            ),
            detail: format!(
                "CSRF enforcement lost on all {} mutating routes: {}",
                routes.len(),
                routes.join(", ")
            ),
        });
    } else {
        for ((_, method), (path, exempt)) in lost {
            out.push(Finding {
                kind: "csrf_enforcement_removed",
                severity: Severity::Widening,
                method,
                path,
                before: "csrf enforced".to_owned(),
                after: "csrf not enforced".to_owned(),
                fingerprint: "csrf-removed".to_owned(),
                detail: if exempt {
                    "CSRF enforcement lost: this route now matches a configured exempt prefix"
                        .to_owned()
                } else {
                    "CSRF enforcement lost".to_owned()
                },
            });
        }
    }
    for ((_, method), path) in gained {
        out.push(Finding {
            kind: "csrf_enforcement_added",
            severity: Severity::Narrowing,
            method,
            path,
            before: "csrf not enforced".to_owned(),
            after: "csrf enforced".to_owned(),
            fingerprint: "csrf-added".to_owned(),
            detail: "CSRF enforcement gained".to_owned(),
        });
    }
}

fn headers_index(m: &PostureManifest) -> BTreeMap<&str, (bool, &str)> {
    m.dimensions
        .security_headers
        .entries
        .iter()
        .map(|e| (e.header.as_str(), (e.emitted, e.value.as_str())))
        .collect()
}

fn diff_headers(base: &PostureManifest, head: &PostureManifest, out: &mut Vec<Finding>) {
    let before = headers_index(base);
    let after = headers_index(head);

    let removed = |header: &str, value_before: &str| Finding {
        kind: "security_header_removed",
        severity: Severity::Widening,
        method: "*".to_owned(),
        path: header.to_owned(),
        before: value_before.to_owned(),
        after: "not emitted".to_owned(),
        fingerprint: "header-removed".to_owned(),
        detail: format!("security header `{header}` is no longer emitted"),
    };

    for (header, (emitted_now, value_now)) in &after {
        match before.get(header) {
            None => {
                if *emitted_now {
                    out.push(Finding {
                        kind: "security_header_added",
                        severity: Severity::Narrowing,
                        method: "*".to_owned(),
                        path: (*header).to_owned(),
                        before: "not emitted".to_owned(),
                        after: (*value_now).to_owned(),
                        fingerprint: "header-added".to_owned(),
                        detail: format!("security header `{header}` is now emitted"),
                    });
                }
            }
            Some((emitted_before, value_before)) => {
                if *emitted_before && !*emitted_now {
                    out.push(removed(header, value_before));
                } else if !*emitted_before && *emitted_now {
                    out.push(Finding {
                        kind: "security_header_added",
                        severity: Severity::Narrowing,
                        method: "*".to_owned(),
                        path: (*header).to_owned(),
                        before: "not emitted".to_owned(),
                        after: (*value_now).to_owned(),
                        fingerprint: "header-added".to_owned(),
                        detail: format!("security header `{header}` is now emitted"),
                    });
                } else if value_before != value_now {
                    // Deliberately neutral. Whether one CSP is weaker than
                    // another is not decidable from the strings, and a gate
                    // that blocks on "the CSP changed" is a gate teams turn
                    // off. It is reported so a human can look, never so a robot
                    // can refuse.
                    out.push(Finding {
                        kind: "security_header_value_changed",
                        severity: Severity::Neutral,
                        method: "*".to_owned(),
                        path: (*header).to_owned(),
                        before: (*value_before).to_owned(),
                        after: (*value_now).to_owned(),
                        fingerprint: "header-value-changed".to_owned(),
                        detail: format!("security header `{header}` value changed — review by eye"),
                    });
                }
            }
        }
    }
    // A header entry that left the manifest entirely is the same loss as one
    // flipped to `emitted: false`.
    for (header, (emitted_before, value_before)) in &before {
        if *emitted_before && !after.contains_key(header) {
            out.push(removed(header, value_before));
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

    // ── security regressions (review findings) ──────────────────────────────

    /// A fact that vanishes from the head manifest is still a fact that was
    /// lost. Adding `POST` to `security.csrf.safe_methods` turns CSRF
    /// validation off for every POST *and* removes those routes from the
    /// manifest's csrf dimension, so a head-only walk sees nothing at all.
    #[test]
    fn csrf_entries_that_disappear_from_the_manifest_are_still_widening() {
        // The route is still mounted on both sides — only its csrf entry is
        // gone, which is exactly what widening `security.csrf.safe_methods`
        // does: the route stays, its CSRF validation does not.
        let r = route("/pay", "POST", "public", &[], &[], false);
        let base = manifest(
            &r,
            r#"{"path":"/pay","method":"POST","csrf_enforced":true,"exempt":false}"#,
            "",
            "",
        );
        let head = manifest(&r, "", "", "");
        let f = only(diff(&base, &head));
        assert_eq!(f.severity, Severity::Widening, "{f:?}");
        assert_eq!(f.path, "/pay");
    }

    /// Same shape for headers: an entry that stops being emitted *by vanishing*
    /// is the same loss as one flipped to `emitted: false`.
    #[test]
    fn security_headers_that_disappear_from_the_manifest_are_still_widening() {
        let base = manifest(
            "",
            "",
            r#"{"header":"x_frame_options","value":"DENY","emitted":true}"#,
            "",
        );
        let head = manifest("", "", "", "");
        let f = only(diff(&base, &head));
        assert_eq!(f.kind, "security_header_removed");
        assert_eq!(f.severity, Severity::Widening);
    }

    /// A header that was never emitted and then vanishes is not a loss.
    #[test]
    fn a_header_that_was_not_emitted_and_then_vanishes_is_not_a_finding() {
        let base = manifest(
            "",
            "",
            r#"{"header":"referrer_policy","value":"","emitted":false}"#,
            "",
        );
        let head = manifest("", "", "", "");
        assert!(diff(&base, &head).is_empty());
    }

    /// A CSRF entry that vanished because its route did is already reported as
    /// `route_removed`; reporting it twice — once as a narrowing and once as a
    /// widening — would block a PR for deleting a route.
    #[test]
    fn a_csrf_entry_that_left_with_its_route_is_not_a_widening() {
        let base = manifest(
            &route("/pay", "POST", "public", &[], &[], false),
            r#"{"path":"/pay","method":"POST","csrf_enforced":true,"exempt":false}"#,
            "",
            "",
        );
        let head = manifest("", "", "", "");
        let findings = diff(&base, &head);
        assert_eq!(kinds(&findings), vec!["route_removed"]);
    }

    /// `#[secured]` with no roles admits every authenticated session, so
    /// *adding* the first role narrows the surface — the one case where an
    /// added role is not a widening.
    #[test]
    fn adding_the_first_role_to_an_unrestricted_gate_is_narrowing() {
        let base = routes_only(&route("/admin", "GET", "gated", &[], &[], false));
        let head = routes_only(&route("/admin", "GET", "gated", &["admin"], &[], false));
        let f = only(diff(&base, &head));
        assert_eq!(f.kind, "roles_narrowed");
        assert_eq!(f.severity, Severity::Narrowing);
    }

    /// The canonical form a finding contributes to the acknowledgment digest is
    /// delimiter-separated, and route paths and role names are app-controlled.
    /// If they are not escaped, one crafted route forges the digest of a
    /// two-finding set — and a stale acknowledgment then covers a widening
    /// nobody approved.
    #[test]
    fn a_crafted_path_cannot_forge_the_canonical_form_of_two_findings() {
        let smuggled = Finding {
            kind: "route_added_open",
            severity: Severity::Widening,
            method: "GET".to_owned(),
            path: "/a\tabsent\tpublic\nroute_added_open\tGET\t/admin".to_owned(),
            before: "absent".to_owned(),
            after: "public".to_owned(),
            fingerprint: "added:public".to_owned(),
            detail: String::new(),
        };
        let plain_a = Finding {
            path: "/a".to_owned(),
            ..smuggled.clone()
        };
        let plain_b = Finding {
            path: "/admin".to_owned(),
            ..smuggled.clone()
        };
        assert_ne!(
            smuggled.canonical(),
            format!("{}\n{}", plain_a.canonical(), plain_b.canonical()),
            "a tab or newline inside a field must not read as a field separator"
        );
        assert!(
            !smuggled.canonical().contains('\n'),
            "the canonical form of one finding is one line"
        );
    }

    /// A manifest should never carry the same `(path, method)` twice, but "should
    /// never" is not a guarantee about a file on disk. If the last entry won,
    /// the verdict would depend on array order — and the order that hides a
    /// public route would be the one that passes.
    #[test]
    fn a_duplicate_route_key_merges_to_the_most_open_posture_either_way() {
        let base = routes_only(&route("/a", "GET", "gated", &["admin"], &[], false));
        let gated = route("/a", "GET", "gated", &["admin"], &[], false);
        let public = route("/a", "GET", "public", &[], &[], false);

        let gated_first = routes_only(&format!("{gated},{public}"));
        let public_first = routes_only(&format!("{public},{gated}"));

        for head in [&gated_first, &public_first] {
            let f = only(diff(&base, head));
            assert_eq!(
                f.kind, "classification_downgraded",
                "a duplicate key must not let the open entry hide behind the gated one"
            );
        }
        assert_eq!(diff(&base, &gated_first), diff(&base, &public_first));
    }

    /// Two duplicate `gated` entries differing only in roles must not let array
    /// order decide: an empty role list admits every authenticated session,
    /// so it is the wider of the two and has to win either way.
    #[test]
    fn a_duplicate_route_key_merges_roles_too() {
        let base = routes_only(&route("/a", "GET", "gated", &["admin"], &[], false));
        let with_role = route("/a", "GET", "gated", &["admin"], &[], false);
        let no_roles = route("/a", "GET", "gated", &[], &[], false);

        let role_first = routes_only(&format!("{with_role},{no_roles}"));
        let none_first = routes_only(&format!("{no_roles},{with_role}"));

        for head in [&role_first, &none_first] {
            let f = only(diff(&base, head));
            assert_eq!(
                f.kind, "roles_cleared",
                "the entry admitting every authenticated session must win"
            );
            assert_eq!(f.severity, Severity::Widening);
        }
        assert_eq!(diff(&base, &role_first), diff(&base, &none_first));
    }

    /// The router matches on shape, not on what the author called a capture:
    /// `/users/{id}` and `/users/{user_id}` accept the same URLs. Keying on the
    /// raw string let a rename read as one route removed (narrowing) plus one
    /// guarded route added (neutral) — so renaming the capture in the same
    /// change that loosened the guard slipped the widening past entirely.
    #[test]
    fn renaming_a_capture_does_not_hide_a_loosened_guard() {
        let base = routes_only(&route(
            "/users/{id}",
            "GET",
            "gated",
            &["admin"],
            &[],
            false,
        ));
        let head = routes_only(&route("/users/{user_id}", "GET", "public", &[], &[], false));

        let f = only(diff(&base, &head));
        assert_eq!(f.kind, "classification_downgraded");
        assert_eq!(f.severity, Severity::Widening);
    }

    /// A pure rename, with the guard untouched, is not a change at all.
    #[test]
    fn renaming_a_capture_alone_produces_no_finding() {
        let base = routes_only(&route("/a/{id}", "GET", "gated", &["admin"], &[], false));
        let head = routes_only(&route("/a/{ident}", "GET", "gated", &["admin"], &[], false));
        assert!(diff(&base, &head).is_empty());
    }

    /// Capture *kinds* still differ: one segment is not the rest of the path,
    /// so those are genuinely different URL sets and must not collapse.
    #[test]
    fn a_segment_capture_and_a_wildcard_are_not_the_same_route() {
        let base = routes_only(&route("/f/{name}", "GET", "gated", &["admin"], &[], false));
        let head = routes_only(&route("/f/{*rest}", "GET", "public", &[], &[], false));

        let findings = diff(&base, &head);
        assert!(
            findings.iter().any(|f| f.kind == "route_added_open"),
            "the wildcard is a new, wider route, not an edit of the old one: {findings:?}"
        );
    }

    /// The router matches a static segment before a dynamic one and mounts
    /// both — `/users/me` beside `/users/{id}` is not a conflict, per the
    /// conflict matrix in `router.rs`. So deleting the gated static route does
    /// not remove the URL; it hands it to the public dynamic one. Read as a
    /// plain removal, that is a *narrowing*, and authentication vanishes from
    /// `/users/me` with nothing to acknowledge.
    #[test]
    fn removing_a_route_that_a_public_one_still_covers_is_a_widening() {
        let public_by_id = route("/users/{id}", "GET", "public", &[], &[], false);
        let gated_me = route("/users/me", "GET", "gated", &["user"], &[], false);
        let base = routes_only(&format!("{public_by_id},{gated_me}"));
        let head = routes_only(&public_by_id);

        let findings = diff(&base, &head);
        let widening = widening(&findings);
        assert_eq!(widening.len(), 1, "{findings:#?}");
        assert_eq!(widening[0].kind, "route_shadow_exposed");
        assert_eq!(widening[0].path, "/users/me");
        assert!(
            widening[0].detail.contains("/users/{id}"),
            "the finding must name the route that now serves the URL: {:?}",
            widening[0]
        );
    }

    /// The survivor has to actually admit more. Deleting a gated route while an
    /// equally gated dynamic route covers it loses no guard.
    #[test]
    fn removing_a_route_a_gated_one_covers_is_not_a_widening() {
        let by_id = route("/users/{id}", "GET", "gated", &["user"], &[], false);
        let me = route("/users/me", "GET", "gated", &["user"], &[], false);
        let base = routes_only(&format!("{by_id},{me}"));
        let head = routes_only(&by_id);

        let findings = diff(&base, &head);
        assert!(widening(&findings).is_empty(), "{findings:#?}");
    }

    /// And a route nothing covers is an ordinary removal.
    #[test]
    fn removing_an_uncovered_route_stays_a_narrowing() {
        let base = routes_only(&format!(
            "{},{}",
            route("/health", "GET", "public", &[], &[], false),
            route("/admin/me", "GET", "gated", &["admin"], &[], false)
        ));
        let head = routes_only(&route("/health", "GET", "public", &[], &[], false));

        let findings = diff(&base, &head);
        assert_eq!(kinds(&findings), vec!["route_removed"], "{findings:#?}");
    }

    /// A catch-all covers everything below it, so deleting a guarded route that
    /// sits under one is the same exposure.
    #[test]
    fn a_catch_all_that_swallows_a_removed_route_is_a_widening() {
        let catch_all = route("/files/{*path}", "GET", "public", &[], &[], false);
        let secret = route("/files/secret", "GET", "gated", &["admin"], &[], false);
        let base = routes_only(&format!("{catch_all},{secret}"));
        let head = routes_only(&catch_all);

        let findings = diff(&base, &head);
        let widening = widening(&findings);
        assert_eq!(widening.len(), 1, "{findings:#?}");
        assert_eq!(widening[0].kind, "route_shadow_exposed");
    }

    /// Axum writes a literal brace as `{{`, so `/{{foo}}` and `/{{bar}}` are two
    /// different URLs and the conflict matrix mounts both. Reading each escape
    /// as a capture collapsed them into one key, so adding a second public
    /// literal-brace route produced no `route_added_open` at all.
    #[test]
    fn escaped_literal_braces_are_not_captures() {
        let foo = route("/{{foo}}", "GET", "public", &[], &[], false);
        let bar = route("/{{bar}}", "GET", "public", &[], &[], false);
        let base = routes_only(&foo);
        let head = routes_only(&format!("{foo},{bar}"));

        let findings = diff(&base, &head);
        assert_eq!(kinds(&findings), vec!["route_added_open"], "{findings:#?}");
        assert_eq!(findings[0].path, "/{{bar}}");
    }

    /// Normalizing the key widens the class of entries that can collide, so
    /// two csrf entries for the same route shape must merge to the *widest*
    /// reading — as duplicate route entries already do — rather than letting
    /// whichever sorts last decide whether the route is protected.
    #[test]
    fn duplicate_csrf_entries_merge_to_the_widest() {
        let routes = route("/pay/{id}", "POST", "gated", &["user"], &[], false);
        let base = manifest(
            &routes,
            r#"{"path":"/pay/{id}","method":"POST","csrf_enforced":true,"exempt":false}"#,
            "",
            "",
        );
        let off = r#"{"path":"/pay/{id}","method":"POST","csrf_enforced":false,"exempt":true}"#;
        let on = r#"{"path":"/pay/{other}","method":"POST","csrf_enforced":true,"exempt":false}"#;

        for entries in [format!("{off},{on}"), format!("{on},{off}")] {
            let head = manifest(&routes, &entries, "", "");
            let findings = diff(&base, &head);
            assert_eq!(
                kinds(&findings),
                vec!["csrf_enforcement_removed"],
                "one entry says the route is unprotected, so it is: {findings:#?}"
            );
        }
    }

    /// The same rename, one dimension over. CSRF entries were keyed on the raw
    /// path, so renaming the capture in the change that turned CSRF off left
    /// the head entry unable to match the base entry — and the disappearance
    /// check could not save it either, since `routes_after` holds normalized
    /// keys. The result was a payment route losing CSRF with nothing to
    /// acknowledge.
    #[test]
    fn renaming_a_capture_does_not_hide_a_csrf_loss() {
        let base = manifest(
            &route("/pay/{id}", "POST", "gated", &["user"], &[], false),
            r#"{"path":"/pay/{id}","method":"POST","csrf_enforced":true,"exempt":false}"#,
            "",
            "",
        );
        let head = manifest(
            &route("/pay/{payment_id}", "POST", "gated", &["user"], &[], false),
            r#"{"path":"/pay/{payment_id}","method":"POST","csrf_enforced":false,"exempt":true}"#,
            "",
            "",
        );

        let findings = diff(&base, &head);
        let f = only(findings.clone());
        assert_eq!(f.kind, "csrf_enforcement_removed");
        assert_eq!(f.severity, Severity::Widening);
        assert_eq!(widening(&findings).len(), 1);
    }

    /// And the same rename with CSRF *untouched* must stay silent: a route
    /// keyed two different ways would otherwise read as one entry lost and one
    /// gained.
    #[test]
    fn renaming_a_capture_alone_is_not_a_csrf_change() {
        let entry = |path: &str| {
            format!(r#"{{"path":"{path}","method":"POST","csrf_enforced":true,"exempt":false}}"#)
        };
        let base = manifest(
            &route("/pay/{id}", "POST", "gated", &["user"], &[], false),
            &entry("/pay/{id}"),
            "",
            "",
        );
        let head = manifest(
            &route("/pay/{payment_id}", "POST", "gated", &["user"], &[], false),
            &entry("/pay/{payment_id}"),
            "",
            "",
        );
        assert!(diff(&base, &head).is_empty(), "{:#?}", diff(&base, &head));
    }

    /// Authorization bindings, third dimension, same key. Here the raw path
    /// cost a false *block* rather than a miss: the binding appeared in both
    /// set differences, so an untouched `#[authorize]` read as a removal.
    #[test]
    fn renaming_a_capture_alone_is_not_an_authorization_change() {
        let binding = |path: &str| {
            format!(r#"{{"path":"{path}","method":"GET","action":"read","resource":"Note"}}"#)
        };
        let base = manifest(
            &route("/notes/{id}", "GET", "gated", &["user"], &[], true),
            "",
            "",
            &binding("/notes/{id}"),
        );
        let head = manifest(
            &route("/notes/{note_id}", "GET", "gated", &["user"], &[], true),
            "",
            "",
            &binding("/notes/{note_id}"),
        );
        assert!(diff(&base, &head).is_empty(), "{:#?}", diff(&base, &head));
    }

    /// Two duplicate entries can be *incomparable*: `["admin"]` and `["editor"]`
    /// neither contains the other, so no ranking of them is principled and one
    /// of the two array orders would hide the newly admitted role. Merging the
    /// sets has no such gap.
    #[test]
    fn incomparable_duplicate_roles_merge_instead_of_racing() {
        let base = routes_only(&route("/a", "GET", "gated", &["admin"], &[], false));
        let admin = route("/a", "GET", "gated", &["admin"], &[], false);
        let editor = route("/a", "GET", "gated", &["editor"], &[], false);

        let admin_first = routes_only(&format!("{admin},{editor}"));
        let editor_first = routes_only(&format!("{editor},{admin}"));

        for head in [&admin_first, &editor_first] {
            let f = only(diff(&base, head));
            assert_eq!(f.kind, "roles_widened");
            assert!(
                f.detail.contains("editor"),
                "the newly admitted role must be named whichever order it appears in: {f:?}"
            );
        }
        assert_eq!(diff(&base, &admin_first), diff(&base, &editor_first));
    }

    /// The same, for the AND-ed dimension: requiring only what both entries
    /// require is the wider reading.
    #[test]
    fn duplicate_scope_sets_merge_to_what_both_require() {
        let base = routes_only(&route(
            "/a",
            "POST",
            "gated",
            &[],
            &["read", "write"],
            false,
        ));
        let read = route("/a", "POST", "gated", &[], &["read"], false);
        let write = route("/a", "POST", "gated", &[], &["write"], false);

        let read_first = routes_only(&format!("{read},{write}"));
        let write_first = routes_only(&format!("{write},{read}"));

        for head in [&read_first, &write_first] {
            let f = only(diff(&base, head));
            assert_eq!(f.kind, "scopes_widened");
            assert_eq!(f.severity, Severity::Widening);
        }
        assert_eq!(diff(&base, &read_first), diff(&base, &write_first));
    }

    /// The acknowledgment digest must describe the *set* of widenings, not the
    /// order the manifest happened to list its entries in.
    #[test]
    fn findings_do_not_depend_on_manifest_entry_order() {
        let base = routes_only(&format!(
            "{},{}",
            route("/a", "GET", "gated", &["admin"], &[], false),
            route("/b", "GET", "gated", &["admin"], &[], false)
        ));
        let forward = routes_only(&format!(
            "{},{}",
            route("/a", "GET", "public", &[], &[], false),
            route("/b", "GET", "public", &[], &[], false)
        ));
        let reversed = routes_only(&format!(
            "{},{}",
            route("/b", "GET", "public", &[], &[], false),
            route("/a", "GET", "public", &[], &[], false)
        ));
        assert_eq!(diff(&base, &forward), diff(&base, &reversed));
    }

    /// Two different sets of routes losing CSRF must not share one
    /// acknowledgment digest — otherwise an ack for the first push silently
    /// covers a later push that disabled CSRF somewhere else entirely.
    #[test]
    fn the_collapsed_csrf_finding_still_identifies_its_routes() {
        let entries = |paths: &[&str], enforced: bool| {
            paths
                .iter()
                .map(|p| {
                    format!(
                        r#"{{"path":"{p}","method":"POST","csrf_enforced":{enforced},"exempt":false}}"#
                    )
                })
                .collect::<Vec<_>>()
                .join(",")
        };
        let first = diff(
            &manifest("", &entries(&["/r0", "/r1", "/r2"], true), "", ""),
            &manifest("", &entries(&["/r0", "/r1", "/r2"], false), "", ""),
        );
        let second = diff(
            &manifest("", &entries(&["/x0", "/x1", "/x2"], true), "", ""),
            &manifest("", &entries(&["/x0", "/x1", "/x2"], false), "", ""),
        );
        assert_eq!(only(first.clone()).kind, "csrf_disabled");
        assert_eq!(only(second.clone()).kind, "csrf_disabled");
        assert_ne!(
            only(first).canonical(),
            only(second).canonical(),
            "different routes, different acknowledgment"
        );
    }

    /// `#[ws]` routes are listed with the synthetic `WS` method on both sides,
    /// so they key up like any other route.
    #[test]
    fn a_websocket_route_is_compared_like_any_other() {
        let base = routes_only(&route("/live", "WS", "gated", &["member"], &[], false));
        let head = routes_only(&route("/live", "WS", "public", &[], &[], false));
        let f = only(diff(&base, &head));
        assert_eq!(f.method, "WS");
        assert_eq!(f.severity, Severity::Widening);
    }

    /// A header key that only one side knows about: added-and-emitted narrows,
    /// and (covered above) vanished-while-emitted widens.
    #[test]
    fn a_header_present_only_in_the_head_manifest_is_reported() {
        let base = manifest("", "", "", "");
        let head = manifest(
            "",
            "",
            r#"{"header":"referrer_policy","value":"no-referrer","emitted":true}"#,
            "",
        );
        let f = only(diff(&base, &head));
        assert_eq!(f.kind, "security_header_added");
        assert_eq!(f.severity, Severity::Narrowing);
    }

    /// A resource rename is a removal plus an addition, and no manifest can tell
    /// it from a real loss — so it still blocks, but the report says why the two
    /// rows belong together.
    #[test]
    fn a_renamed_authorize_resource_blocks_but_explains_the_pairing() {
        let r = route("/posts/{id}", "PUT", "gated", &["user"], &[], true);
        let binding = |resource: &str| {
            format!(
                r#"{{"path":"/posts/{{id}}","method":"PUT","name":"h","action":"update","resource":"{resource}","provenance":"provable"}}"#
            )
        };
        let findings = diff(
            &manifest(&r, "", "", &binding("Post")),
            &manifest(&r, "", "", &binding("Article")),
        );
        let removed = findings
            .iter()
            .find(|f| f.kind == "authorization_binding_removed")
            .expect("still reported");
        assert_eq!(removed.severity, Severity::Widening);
        assert!(
            removed.detail.contains("Article"),
            "the paired addition is named so a rename can be acknowledged in one step: {removed:?}"
        );
    }

    /// A later *narrowing* on a route whose widening was already acknowledged
    /// must not change that widening's canonical form — otherwise the gate
    /// re-asks for an acknowledgment nobody can see a reason for.
    #[test]
    fn a_later_narrowing_does_not_disturb_an_earlier_findings_identity() {
        let base = routes_only(&route("/admin", "GET", "gated", &["admin"], &[], false));
        let first = routes_only(&route("/admin", "GET", "public", &[], &[], false));
        // The follow-up commit adds a record-level policy check: strictly
        // narrower, and it changes the rendered posture label.
        let second = routes_only(&route("/admin", "GET", "public", &[], &[], true));

        let a = only(diff(&base, &first));
        let b = diff(&base, &second)
            .into_iter()
            .find(|f| f.kind == "classification_downgraded")
            .expect("still the same widening");
        assert_eq!(a.canonical(), b.canonical());
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
        // Same inputs, same order, every time — and see
        // `findings_do_not_depend_on_manifest_entry_order` for the property
        // that actually matters.
        assert_eq!(findings, diff(&base, &head));
    }
}
