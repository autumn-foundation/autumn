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
            // Normalized, like the routes and the manifest digest: a capture
            // rename is not a posture change, so it must not re-block a
            // widening a reviewer has already approved. The report still shows
            // the path as the author wrote it.
            escape_field(&normalize_captures(&self.path)),
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
    let (keys_before, keys_after) = (route_keys(base), route_keys(head));

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
            if !open {
                report_displacement(key, entry, &before, (&keys_before, &keys_after), out);
            }
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
            report_shadow_exposure(key, entry, &after, (&keys_before, &keys_after), out);
        }
    }
    report_path_exposure(base, head, &before, &after, out);
}

/// A path is a node, not a set of routes.
///
/// Autumn groups every method at one path into a single `MethodRouter`, so
/// while any method is mounted there the path answers 405 for the rest.
/// Removing the *last* route at a path takes the node away, and those
/// previously-405 requests start reaching a less specific route. Filtering by
/// method intersection first skipped that: the removed `GET` and the surviving
/// `POST` share nothing, yet `POST` on that URL went from 405 to served.
///
/// Newly reachable through an *open* route is a widening, exactly as a new open
/// route is; newly reachable through a guarded one annotates, exactly as a new
/// guarded route does.
fn report_path_exposure(
    base: &PostureManifest,
    head: &PostureManifest,
    before: &BTreeMap<RouteKey, RouteEntry>,
    after: &BTreeMap<RouteKey, RouteEntry>,
    out: &mut Vec<Finding>,
) {
    let keys_before = route_keys(base);
    let keys_after = route_keys(head);
    let paths_after: BTreeSet<&String> = keys_after.iter().map(|(path, _)| path).collect();

    // Every path the head no longer mounts at all, and the methods it answered.
    let mut vanished: BTreeMap<&String, BTreeSet<String>> = BTreeMap::new();
    for (path, method) in &keys_before {
        if !paths_after.contains(path) {
            vanished
                .entry(path)
                .or_default()
                .extend(answered_methods(method, path, &keys_before));
        }
    }

    for (path, served) in vanished {
        let candidates: Vec<&RouteKey> = keys_after
            .iter()
            .filter(|(p, _)| takes_precedence(path, p))
            .collect();
        for key in undominated(path, &candidates) {
            let Some(survivor) = after.get(key) else {
                continue;
            };
            let open = is_open(&survivor.classification);
            let (survivor_path, survivor_method) = key;
            let newly: Vec<String> = answered_methods(survivor_method, survivor_path, &keys_after)
                .difference(&served)
                .cloned()
                .collect();
            if newly.is_empty() {
                continue;
            }
            let written = before
                .iter()
                .find(|((p, _), _)| p == path)
                .map_or_else(|| path.clone(), |(_, entry)| entry.path.clone());
            out.push(Finding {
                kind: "route_path_exposed",
                // Newly reachable through an open route blocks, exactly as a
                // new open route does; through a guarded one it annotates,
                // exactly as a new guarded route does.
                severity: if open {
                    Severity::Widening
                } else {
                    Severity::Neutral
                },
                method: newly.join(", "),
                path: written.clone(),
                before: "405 (no handler at this path)".to_owned(),
                after: survivor.posture_label(),
                fingerprint: format!(
                    "path-exposed:{}",
                    escape_list(&[
                        escape_list(&newly),
                        survivor.method.clone(),
                        survivor.path.clone(),
                        posture_fingerprint(survivor),
                    ])
                ),
                detail: format!(
                    "nothing is mounted at this path any more, so {} on it no longer stops at a \
                     405 — `{} {}` answers it now ({})",
                    newly.join(", "),
                    survivor.method,
                    survivor.path,
                    survivor.posture_label()
                ),
            });
        }
    }
}

/// Adding a route does not always add a URL.
///
/// The mirror of [`report_shadow_exposure`]: a new route that is *more
/// specific* than an existing one takes that route's requests over. A
/// `/users/me` restricted to `editor`, mounted beside a `/users/{id}`
/// restricted to `admin`, hands `/users/me` to editors — while the dynamic
/// entry sits unchanged in both manifests, so the addition read as a neutral
/// new guarded route and nothing blocked.
fn report_displacement(
    (path, method): &RouteKey,
    added: &RouteEntry,
    before: &BTreeMap<RouteKey, RouteEntry>,
    (keys_before, keys_after): (&BTreeSet<RouteKey>, &BTreeSet<RouteKey>),
    out: &mut Vec<Finding>,
) {
    // What the new route takes over is whatever *was* serving those URLs, asked
    // of the base with the same predicate a removal asks of the head.
    for key in takers((path, method), keys_after, keys_before) {
        let Some(displaced) = before.get(&key) else {
            continue;
        };
        // What changed for those URLs is the posture they used to demand
        // against the one they demand now — decided, as everywhere else here,
        // by `compare_route` rather than by a second opinion.
        let mut probe = Vec::new();
        compare_route(displaced, added, &mut probe);
        if !probe.iter().any(|f| f.severity == Severity::Widening) {
            continue;
        }
        out.push(Finding {
            kind: "route_added_shadowing",
            severity: Severity::Widening,
            method: method.clone(),
            path: added.path.clone(),
            before: displaced.posture_label(),
            after: added.posture_label(),
            // Both postures, so an acknowledgment binds to the change it was
            // written for and not merely to the pair of paths.
            fingerprint: format!(
                "added-shadowing:{}",
                escape_list(&[
                    displaced.method.clone(),
                    displaced.path.clone(),
                    posture_fingerprint(displaced),
                    posture_fingerprint(added),
                ])
            ),
            detail: format!(
                "this route is more specific than `{} {}`, so it takes those requests over and \
                 admits callers that one refused ({} → {})",
                displaced.method,
                displaced.path,
                displaced.posture_label(),
                added.posture_label()
            ),
        });
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
    (keys_before, keys_after): (&BTreeSet<RouteKey>, &BTreeSet<RouteKey>),
    out: &mut Vec<Finding>,
) {
    for key in takers((path, method), keys_before, keys_after) {
        let Some(survivor) = after.get(&key) else {
            continue;
        };
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
            // The survivor's *posture* is part of what a reviewer acknowledged,
            // not just its path. When the same pull request removes the guarded
            // route and adds the one that now covers it, the survivor is absent
            // from the base — so loosening it later is only a neutral
            // `route_added_gated`, and a fingerprint naming the path alone left
            // the digest unmoved and the old acknowledgment standing.
            fingerprint: format!(
                "shadow-exposed:{}",
                escape_list(&[
                    survivor.method.clone(),
                    survivor.path.clone(),
                    posture_fingerprint(survivor),
                ])
            ),
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

/// A route's posture, encoded so an acknowledgment binds to exactly it.
///
/// Nested `escape_list`s rather than a joined string: role and scope names are
/// unrestricted string literals, so any separator picked here is one a name can
/// contain — the same ambiguity the manifest digest already had to close.
fn posture_fingerprint(entry: &RouteEntry) -> String {
    let roles: Vec<String> = entry.role_set().into_iter().collect();
    let scopes: Vec<String> = entry.scope_set().into_iter().collect();
    escape_list(&[
        entry.classification.clone(),
        escape_list(&roles),
        escape_list(&scopes),
        entry.policy.to_string(),
    ])
}

/// The HTTP methods a declared method actually answers at runtime.
///
/// A route's declared method is not the set of requests it takes: the router
/// mounts `WS` as a plain `GET` (`routes_audit`'s own `effective_mount_method`
/// does the same), and axum serves `HEAD` through a `#[get]` handler. So a
/// surviving `GET` route picks up the `HEAD` and `WS` traffic of a deleted one
/// at the same URL, and comparing declared methods exactly skipped precisely
/// those survivors.
fn effective_methods(method: &str) -> BTreeSet<String> {
    let declared = method.to_ascii_uppercase();
    let mut answered = BTreeSet::new();
    // Only a *genuine* `GET` is also served for `HEAD`. A websocket upgrade is
    // not, which the router states in exactly those terms — so the `HEAD` alias
    // is added before the `WS` fold, never after it.
    if declared == "GET" {
        answered.insert("HEAD".to_owned());
    }
    answered.insert(if declared == "WS" {
        "GET".to_owned()
    } else {
        declared
    });
    answered
}

/// The methods a route answers *in the manifest it belongs to*.
///
/// Every comparison goes through here rather than through the declared method:
/// there is no context-free "do these two methods overlap" left, because the
/// answer always depended on what else the manifest mounts.
///
/// The `GET`→`HEAD` expansion is a **fallback**, not a takeover: axum keeps an
/// explicit `HEAD` handler mounted at the same path, so a `GET` there answers
/// `GET` alone. Without that, a new guarded `GET` read as displacing an
/// explicit `HEAD` that goes on answering exactly as it did.
fn answered_methods(method: &str, path: &str, routes: &BTreeSet<RouteKey>) -> BTreeSet<String> {
    let mut answered = effective_methods(method);
    if !method.eq_ignore_ascii_case("HEAD")
        && routes.contains(&(path.to_owned(), "HEAD".to_owned()))
    {
        answered.remove("HEAD");
    }
    answered
}

/// The routes on the other side of the change that answer `subject`'s URLs.
///
/// Every "which route does this URL go to" question in this module is this one,
/// and each site that answered it privately needed the same three corrections
/// in turn: the path node has to be accounted for, the methods have to actually
/// transfer, and the candidates have to be ranked per URL. So there is one
/// answer now, and it is symmetric — a removal asks it of the head, an addition
/// asks it of the base, and the reasoning is identical in both directions.
fn takers(
    (path, method): (&str, &str),
    subject_keys: &BTreeSet<RouteKey>,
    other_keys: &BTreeSet<RouteKey>,
) -> Vec<RouteKey> {
    let answered = answered_methods(method, path, subject_keys);
    // Within one path node the request never leaves the node, so whichever
    // handler *there* answers the method takes it. That covers the route
    // itself, and it covers an explicit `HEAD` trading places with a `GET`'s
    // `HEAD` fallback — a transfer inside the node, which a bare "the node
    // exists" test read as no transfer at all.
    let same_path: Vec<RouteKey> = other_keys
        .iter()
        .filter(|(p, m)| p == path && !answered.is_disjoint(&answered_methods(m, p, other_keys)))
        .cloned()
        .collect();
    if !same_path.is_empty() {
        return same_path;
    }
    // A path node the other side mounts owns that URL for every method — it
    // answers 405 where it has no handler rather than letting the request fall
    // through — so nothing passes to another path.
    if other_keys.iter().any(|(p, _)| p == path) {
        return Vec::new();
    }
    let candidates: Vec<&RouteKey> = other_keys
        .iter()
        .filter(|(p, m)| {
            takes_precedence(path, p) && !answered.is_disjoint(&answered_methods(m, p, other_keys))
        })
        .collect();
    undominated(path, &candidates)
        .into_iter()
        .cloned()
        .collect()
}

/// Every `(normalized path, method)` a manifest mounts.
fn route_keys(m: &PostureManifest) -> BTreeSet<RouteKey> {
    m.dimensions
        .routes
        .entries
        .iter()
        .map(RouteEntry::key)
        .collect()
}

/// Among the routes that could take `subject`'s URLs, the ones that actually
/// receive some of them.
///
/// Precedence ranks the takers, but it ranks them *per URL*: outranking a
/// candidate somewhere is not the same as taking everything it would have got.
/// Removing `/records/me/{id}`, the guarded `/records/{user}/private` wins
/// `/records/me/private` and nothing else — every other `/records/me/*` still
/// goes to a public `/records/{user}/{id}`, which a pairwise test discarded.
///
/// So a candidate drops out only when another one outranks it *and* covers
/// every URL it would have taken. Routes sharing a path never outrank each
/// other: they are the same node.
fn undominated<'a>(subject: &str, takers: &[&'a RouteKey]) -> Vec<&'a RouteKey> {
    takers
        .iter()
        .filter(|(path, _)| {
            let Some(share) = intersect(subject, path) else {
                return false;
            };
            !takers.iter().any(|(other, _)| {
                other != path && takes_precedence(other, path) && covers(other, &share)
            })
        })
        .copied()
        .collect()
}

/// The pattern matching exactly the URLs both patterns match, if any.
fn intersect(a: &str, b: &str) -> Option<String> {
    let a: Vec<&str> = a.split('/').collect();
    let b: Vec<&str> = b.split('/').collect();
    let mut out: Vec<String> = Vec::new();
    for i in 0..a.len().max(b.len()) {
        match (a.get(i), b.get(i)) {
            // A catch-all takes the other pattern's remaining segments.
            (Some(x), Some(_)) if x.contains(CATCH_ALL) => {
                out.extend(b[i..].iter().map(|s| (*s).to_owned()));
                return Some(out.join("/"));
            }
            (Some(_), Some(y)) if y.contains(CATCH_ALL) => {
                out.extend(a[i..].iter().map(|s| (*s).to_owned()));
                return Some(out.join("/"));
            }
            (Some(x), Some(y)) if segments_overlap(x, y) => {
                // The more specific segment is what both match.
                let narrower = if specificity(x) <= specificity(y) {
                    x
                } else {
                    y
                };
                out.push((*narrower).to_owned());
            }
            _ => return None,
        }
    }
    Some(out.join("/"))
}

/// Whether every URL `target` matches is also matched by `pattern`.
fn covers(pattern: &str, target: &str) -> bool {
    let pattern: Vec<&str> = pattern.split('/').collect();
    let target: Vec<&str> = target.split('/').collect();
    for i in 0..pattern.len().max(target.len()) {
        match (pattern.get(i), target.get(i)) {
            (Some(p), Some(_)) if p.contains(CATCH_ALL) => return true,
            (Some(p), Some(t)) => {
                // A capture covers anything in its position; a literal covers
                // only the identical literal, and never a capture.
                if p.contains(CAPTURE) {
                    if !segments_overlap(p, t) {
                        return false;
                    }
                } else if p != t {
                    return false;
                }
            }
            _ => return false,
        }
    }
    true
}

/// Whether `first` overlaps `second` *and* wins the overlap at the router.
///
/// Overlap alone is never the question, because the router has a precedence
/// rule: a static segment beats a capture, which beats a catch-all, decided at
/// the first position where the two differ. Both directions of the shadow
/// analysis are this one predicate.
///
/// - **A removal**: `takes_precedence(removed, survivor)` asks whether the
///   deleted route was the one serving the overlap, so the survivor inherits
///   it. Deleting a gated `/users/{id}` beside a public `/users/me` exposes
///   nothing — that URL was already going to `/users/me`.
/// - **An addition**: `takes_precedence(added, displaced)` asks whether the new
///   route takes the overlap away from an existing one. A new `/users/me` wins
///   that URL from `/users/{id}` the moment it is mounted.
fn takes_precedence(first: &str, second: &str) -> bool {
    if !shapes_overlap(first, second) {
        return false;
    }
    let first: Vec<&str> = first.split('/').collect();
    let second: Vec<&str> = second.split('/').collect();
    for i in 0..first.len().max(second.len()) {
        // A path that has run out of segments is the less specific of the two:
        // only a catch-all can be matching in its place.
        let mine = first.get(i).map_or(u8::MAX, |s| specificity(s));
        let theirs = second.get(i).map_or(u8::MAX, |s| specificity(s));
        if mine != theirs {
            return mine < theirs;
        }
    }
    // Equally specific the whole way down. The router rejects such a pair as a
    // conflict, so the manifest is malformed — report rather than stay quiet.
    true
}

/// How specific a normalized path segment is, lowest first, in the router's own
/// order: a static segment beats a capture, which beats a catch-all.
fn specificity(segment: &str) -> u8 {
    if segment.contains(CATCH_ALL) {
        2
    } else {
        u8::from(segment.contains(CAPTURE))
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
            // What still guards the route is part of what was acknowledged: a
            // constant identity let the record check go, then the scope that
            // replaced it, under one marker.
            fingerprint: format!("policy-removed:{}", posture_fingerprint(after)),
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
    let now_sorted: Vec<String> = now.iter().cloned().collect();

    if !removed.is_empty() {
        out.push(Finding {
            kind: "scopes_widened",
            severity: Severity::Widening,
            method: after.method.clone(),
            path: after.path.clone(),
            before: label_before.to_owned(),
            after: label_after.to_owned(),
            // The scopes still required are part of what was acknowledged:
            // naming only the lost one let `{read}` → `{mfa}` → `{}` keep a
            // single identity, so the acknowledgment for the swap covered the
            // drop as well.
            fingerprint: format!(
                "scopes-{}",
                escape_list(&[escape_list(&removed), escape_list(&now_sorted)])
            ),
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
    let routes_before = route_keys(base);
    let routes_after = route_keys(head);

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
        // reported as `route_removed`, and that is a narrowing, not a widening
        // — but only when the URL went with it. A surviving route whose shape
        // and methods overlap still answers that URL, so the record-level check
        // really did disappear from something reachable.
        // Every route that answers this one's URLs after the change — none if
        // the path node is gone entirely, and none if it survives without this
        // method, since that is a 405 rather than a fall-through.
        let takers = takers((path, method), &routes_before, &routes_after);
        if takers.is_empty() {
            continue;
        }
        // ...unless the route that now answers it performs the very same
        // check, in which case nothing changed for that URL's callers. Both
        // routes being `gated` with a record-level check is not enough: the
        // route comparison cannot see *which* check, so `read_self` giving way
        // to `read_any` reads as no change at all up there.
        let fell_through = !routes_after.contains(&(path.clone(), method.clone()));
        // Preserved only if *every* taker performs the same check: one that
        // does not means some URL reaches a route without it. Asking whether
        // any taker carries it let a check survive on the route that loses.
        if fell_through
            && takers.iter().all(|(p, m)| {
                after.contains_key(&(p.clone(), m.clone(), action.clone(), resource.clone()))
            })
        {
            continue;
        }
        let mut detail =
            format!("record-level authorization `{action}` on `{resource}` no longer checked");
        if fell_through {
            detail.push_str(
                " (the route carrying it is gone, but the URL is not: another mounted route \
                 answers it now, without this check)",
            );
        }

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
                "authz-removed:{}",
                escape_list(&[
                    action.clone(),
                    resource.clone(),
                    // Whatever checks these URLs now get, whether that is
                    // another route's after a fall-through or this route's own
                    // after a swap. Leaving the exact-route case out let
                    // `read_any` give way to `read_all` without moving the
                    // digest, so the acknowledgment for one stood for the
                    // other.
                    checks_on((path, method), &after, &routes_after, &routes_before),
                ])
            ),
            detail,
        });
    }
    report_displaced_bindings(base, head, &before, &after, out);
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

/// Emit the routes that lost CSRF, collapsed into one finding or one each.
fn report_csrf_loss(
    lost: BTreeMap<RouteKey, (String, bool)>,
    head_routes: &BTreeMap<RouteKey, RouteEntry>,
    collapse: bool,
    out: &mut Vec<Finding>,
) {
    if collapse {
        let routes: Vec<String> = lost
            .iter()
            .map(|((_, method), (written_as, _))| format!("{method} {written_as}"))
            .collect();
        // The fingerprint names the *routes*, not the spellings: renaming a
        // capture changes neither the URL set that lost CSRF nor what a
        // reviewer acknowledged about it.
        // Each route *and what still guards it*, for the same reason the
        // per-route fingerprint carries it.
        let guarded_keys: Vec<String> = lost
            .keys()
            .map(|(path, method)| {
                escape_list(&[
                    method.clone(),
                    path.clone(),
                    head_routes
                        .get(&(path.clone(), method.clone()))
                        .map_or_else(String::new, posture_fingerprint),
                ])
            })
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
                &hex_digest(escape_list(&guarded_keys).as_bytes())[..16]
            ),
            detail: format!(
                "CSRF enforcement lost on all {} mutating routes: {}",
                routes.len(),
                routes.join(", ")
            ),
        });
    } else {
        for (key, (path, exempt)) in lost {
            let guard = head_routes
                .get(&key)
                .map_or_else(String::new, posture_fingerprint);
            let (_, method) = key;
            out.push(Finding {
                kind: "csrf_enforcement_removed",
                severity: Severity::Widening,
                method,
                path,
                before: "csrf enforced".to_owned(),
                after: "csrf not enforced".to_owned(),
                fingerprint: format!("csrf-removed:{guard}"),
                detail: if exempt {
                    "CSRF enforcement lost: this route now matches a configured exempt prefix"
                        .to_owned()
                } else {
                    "CSRF enforcement lost".to_owned()
                },
            });
        }
    }
}

/// Whether `prefix` exempts `path`, spelled exactly as the CSRF middleware
/// spells it — so "is this prefix already covered by that one" is asked in the
/// same terms the runtime will answer it in.
fn exempts(prefix: &str, path: &str) -> bool {
    if path == prefix {
        return true;
    }
    path.strip_prefix(prefix)
        .is_some_and(|rest| prefix.ends_with('/') || rest.starts_with('/'))
}

/// Configured exemption prefixes, which the per-route rows cannot show.
///
/// The audit asks whether a route *template* matches a prefix; the runtime asks
/// it of the concrete request path. Exempting `/users/me` therefore leaves
/// `POST /users/{id}` recorded as enforced while those requests stop being
/// validated — a widening with no row to carry it. Compared conservatively: any
/// prefix that was not there before can only exempt more URLs.
fn diff_csrf_exemptions(base: &PostureManifest, head: &PostureManifest, out: &mut Vec<Finding>) {
    let before: BTreeSet<&String> = base.dimensions.csrf.exempt_paths.iter().collect();
    let after: BTreeSet<&String> = head.dimensions.csrf.exempt_paths.iter().collect();

    // Coverage, not spelling. Replacing `/api` with `/api/private` exempts a
    // strict subset of what was exempt before, so calling the new prefix a
    // widening blocks a change that restores CSRF for most of `/api`.
    let added: Vec<String> = after
        .iter()
        .filter(|p| !before.iter().any(|old| exempts(old, p)))
        .map(|p| (*p).clone())
        .collect();
    let removed: Vec<String> = before
        .iter()
        .filter(|p| !after.iter().any(|new| exempts(new, p)))
        .map(|p| (*p).clone())
        .collect();

    if !added.is_empty() {
        out.push(Finding {
            kind: "csrf_exemption_added",
            severity: Severity::Widening,
            method: "*".to_owned(),
            path: "*".to_owned(),
            before: "not exempt".to_owned(),
            after: added.join(", "),
            fingerprint: format!("csrf-exempt-added:{}", escape_list(&added)),
            detail: format!(
                "CSRF validation is now skipped for {}, which the per-route rows cannot show: \
                 the audit matches a prefix against a route template, the runtime against the \
                 request path",
                added.join(", ")
            ),
        });
    }
    if !removed.is_empty() {
        out.push(Finding {
            kind: "csrf_exemption_removed",
            severity: Severity::Narrowing,
            method: "*".to_owned(),
            path: "*".to_owned(),
            before: removed.join(", "),
            after: "not exempt".to_owned(),
            fingerprint: format!("csrf-exempt-removed:{}", escape_list(&removed)),
            detail: format!(
                "CSRF validation is enforced again for {}",
                removed.join(", ")
            ),
        });
    }
}

/// The checks a manifest performs on the URLs a route answers, as one string.
///
/// What an acknowledgment of a fall-through or a displacement actually says is
/// "I accept that this URL is checked by *that* instead" — so weakening what it
/// is checked by has to invalidate it. The replacing route is absent from the
/// base, so its binding reads only as a neutral addition; without this in the
/// fingerprint the digest never moved.
fn checks_on(
    (path, method): (&str, &str),
    bindings: &BTreeMap<AuthzKey, String>,
    routes: &BTreeSet<RouteKey>,
    from_routes: &BTreeSet<RouteKey>,
) -> String {
    let mut per_route: Vec<String> = takers((path, method), from_routes, routes)
        .into_iter()
        .map(|(p, m)| {
            // Route identity is kept, not just the union of checks: a losing
            // route carrying the same names would otherwise mask a change in
            // the one that actually serves the URL.
            let mut checks: Vec<String> = bindings
                .keys()
                .filter(|(bp, bm, _, _)| *bp == p && *bm == m)
                .map(|(_, _, action, resource)| format!("{action} on {resource}"))
                .collect();
            checks.sort();
            checks.dedup();
            escape_list(&[m, p, escape_list(&checks)])
        })
        .collect();
    per_route.sort();
    escape_list(&per_route)
}

/// Bindings the URLs of a displaced route lose to a new, more specific one.
///
/// The mirror of the fall-through case, and just as invisible: a displacement
/// can change the record-level check while nothing the route comparison sees
/// has moved — same roles, both `policy: true` — and the binding dimension sees
/// only an *added* binding, which is a narrowing. The URLs `/records/me` took
/// over went from `read_self` to `read_any` all the same.
fn report_displaced_bindings(
    base: &PostureManifest,
    head: &PostureManifest,
    before: &BTreeMap<AuthzKey, String>,
    after: &BTreeMap<AuthzKey, String>,
    out: &mut Vec<Finding>,
) {
    let keys_before = route_keys(base);
    let keys_after = route_keys(head);
    let head_routes = route_index(head);
    let mut reported: BTreeSet<(RouteKey, String, String)> = BTreeSet::new();

    for added_key in keys_after.difference(&keys_before) {
        let (added_path, added_method) = added_key;
        for displaced_key in takers((added_path, added_method), &keys_after, &keys_before) {
            let (displaced_path, displaced_method) = &displaced_key;
            for ((path, method, action, resource), written_as) in before {
                if path != displaced_path || method != displaced_method {
                    continue;
                }
                // The new route performing the very same check changes nothing
                // for the URLs it took over.
                let carried = (
                    added_path.clone(),
                    added_method.clone(),
                    action.clone(),
                    resource.clone(),
                );
                if after.contains_key(&carried)
                    || !reported.insert((added_key.clone(), action.clone(), resource.clone()))
                {
                    continue;
                }
                let written_added = head_routes
                    .get(added_key)
                    .map_or_else(|| added_path.clone(), |r| r.path.clone());
                out.push(Finding {
                    kind: "authorization_binding_displaced",
                    severity: Severity::Widening,
                    method: added_method.clone(),
                    path: written_added.clone(),
                    before: format!("authorize({action}, {resource})"),
                    after: "none".to_owned(),
                    fingerprint: format!(
                        "authz-displaced:{}",
                        escape_list(&[
                            written_as.clone(),
                            displaced_method.clone(),
                            action.clone(),
                            resource.clone(),
                            checks_on((added_path, added_method), after, &keys_after, &keys_before),
                        ])
                    ),
                    detail: format!(
                        "this route is more specific than `{displaced_method} {written_as}`, so it \
                         takes those requests over — and it does not perform that route's \
                         record-level `{action}` check on `{resource}`"
                    ),
                });
            }
        }
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
    diff_csrf_exemptions(base, head, out);
    // What still guards each route, for the fingerprints below: losing CSRF
    // behind a newly required scope is not the same decision as losing it with
    // that scope gone again.
    let head_routes = route_index(head);
    let before = csrf_index(base);
    let after = csrf_index(head);
    let routes_before = route_keys(base);
    let routes_after = route_keys(head);

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
        if !*enforced_before || after.contains_key(key) {
            continue;
        }
        // Reachable by the same rule the route findings use, not by an exact
        // key: a `WS` route and a `GET` route at one path are the same runtime
        // handler, so replacing an enforced `WS /x` with a `GET /x` that does
        // not enforce is a lost check, however the manifest spells the method.
        let (path, method) = key;
        let still_reachable = takers((path, method), &routes_before, &routes_after)
            .iter()
            .any(|taker| after.get(taker).is_none_or(|(enforced, ..)| !enforced));
        if still_reachable {
            lost.entry(key.clone())
                .or_insert_with(|| (written_as.clone(), false));
        }
    }

    // One collapsed finding when CSRF went off everywhere: an app that flips
    // `security.csrf.enabled` produces one row per mutating route otherwise,
    // and a 200-row table is a table nobody reads.
    let collapse = after.values().all(|(enforced, ..)| !enforced)
        && before.values().any(|entry| entry.0)
        && lost.len() > 1;
    report_csrf_loss(lost, &head_routes, collapse, out);

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

    /// A manifest whose csrf dimension carries configured exemption prefixes.
    fn manifest_exempt(routes: &str, csrf: &str, exempt: &[&str]) -> PostureManifest {
        let exempt = exempt
            .iter()
            .map(|p| format!("\"{p}\""))
            .collect::<Vec<_>>()
            .join(",");
        let json = format!(
            r#"{{"schema_version":3,"dimensions":{{
                 "routes":{{"provenance":"provable","source":"m","entries":[{routes}]}},
                 "csrf":{{"provenance":"declared","source":"c","exempt_paths":[{exempt}],"entries":[{csrf}]}},
                 "security_headers":{{"provenance":"declared","source":"c","entries":[]}},
                 "authorization_policies":{{"provenance":"provable","source":"m","runtime_caveat":"x","entries":[]}}
               }},"excluded":[]}}"#
        );
        PostureManifest::parse(&json, "test.json").expect("fixture parses")
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

    /// A path is a node, not a set of routes. Autumn groups every method at
    /// one path into a single `MethodRouter`, so while any method is mounted
    /// there the path answers 405 for the rest. Delete the *last* route at
    /// `/users/me` and the node goes with it — `POST /users/me`, previously a
    /// 405, now reaches the public `POST /users/{id}`. Filtering by method
    /// intersection first skipped that entirely.
    #[test]
    fn removing_the_last_route_at_a_path_exposes_its_other_methods() {
        let removed = route("/users/me", "GET", "gated", &["user"], &[], false);
        let survivor = route("/users/{id}", "POST", "public", &[], &[], false);
        let base = routes_only(&format!("{removed},{survivor}"));
        let head = routes_only(&survivor);

        let findings = diff(&base, &head);
        let widening = widening(&findings);
        assert_eq!(widening.len(), 1, "{findings:#?}");
        assert_eq!(widening[0].kind, "route_path_exposed");
        assert!(widening[0].method.contains("POST"), "{:?}", widening[0]);
    }

    /// Newly reachable but *guarded* is the same verdict as a new gated route:
    /// annotate, do not block.
    #[test]
    fn a_guarded_survivor_taking_a_vanished_path_is_not_a_widening() {
        let removed = route("/users/me", "GET", "gated", &["user"], &[], false);
        let survivor = route("/users/{id}", "POST", "gated", &["admin"], &[], false);
        let base = routes_only(&format!("{removed},{survivor}"));
        let head = routes_only(&survivor);

        assert!(
            widening(&diff(&base, &head)).is_empty(),
            "{:#?}",
            diff(&base, &head)
        );
    }

    /// Pairwise domination is not per-URL domination. Removing gated
    /// `/records/me/{id}`, the guarded `/records/{user}/private` wins only
    /// `/records/me/private` — every other `/records/me/*` goes to the public
    /// `/records/{user}/{id}`. Dropping a candidate because *some* other
    /// candidate outranks it discarded the public fallback entirely.
    #[test]
    fn a_taker_that_keeps_part_of_the_urls_is_not_dominated() {
        let removed = route("/records/me/{id}", "GET", "gated", &["user"], &[], false);
        let narrow = route(
            "/records/{user}/private",
            "GET",
            "gated",
            &["user"],
            &[],
            false,
        );
        let public_rest = route("/records/{user}/{id}", "GET", "public", &[], &[], false);

        let base = routes_only(&format!("{removed},{narrow},{public_rest}"));
        let head = routes_only(&format!("{narrow},{public_rest}"));

        let findings = diff(&base, &head);
        let widening = widening(&findings);
        assert_eq!(widening.len(), 1, "{findings:#?}");
        assert_eq!(widening[0].kind, "route_shadow_exposed");
        assert!(
            widening[0].detail.contains("/records/{user}/{id}"),
            "the public route takes every URL the narrow one does not: {:?}",
            widening[0]
        );
    }

    /// While *any* method stays mounted at a path, that path node answers the
    /// request — with a 405 — rather than letting it fall through. Removing
    /// `GET /users/me` beside a surviving `POST /users/me` exposes nothing.
    #[test]
    fn a_surviving_method_at_the_same_path_blocks_the_fallthrough() {
        let removed = route("/users/me", "GET", "gated", &["user"], &[], false);
        let same_path = route("/users/me", "POST", "gated", &["user"], &[], false);
        let dynamic = route("/users/{id}", "GET", "public", &[], &[], false);

        let base = routes_only(&format!("{removed},{same_path},{dynamic}"));
        let head = routes_only(&format!("{same_path},{dynamic}"));

        let findings = diff(&base, &head);
        assert!(
            widening(&findings).is_empty(),
            "the path node still answers, with a 405: {findings:#?}"
        );
    }

    /// Newly reachable but guarded annotates — which is what the function and
    /// the guide both say, and what the code did not do: it dropped the change
    /// entirely rather than reporting it as neutral.
    #[test]
    fn a_guarded_route_inheriting_a_vanished_path_is_annotated() {
        let removed = route("/users/me", "GET", "gated", &["user"], &[], false);
        let survivor = route("/users/{id}", "POST", "gated", &["admin"], &[], false);
        let base = routes_only(&format!("{removed},{survivor}"));
        let head = routes_only(&survivor);

        let findings = diff(&base, &head);
        assert!(widening(&findings).is_empty(), "{findings:#?}");
        let exposed = findings
            .iter()
            .find(|f| f.kind == "route_path_exposed")
            .expect("newly reachable, so it is reported");
        assert_eq!(exposed.severity, Severity::Neutral);
    }

    /// A route that another taker outranks never receives the URL, so it is not
    /// the fall-through target. `/records/me/{id}` wins `/records/me/private`
    /// over `/records/{user}/private`, so the latter being public exposes
    /// nothing.
    #[test]
    fn a_survivor_outranked_by_another_taker_is_not_the_fallthrough() {
        let gated = |path: &str| route(path, "GET", "gated", &["user"], &[], false);
        let removed = gated("/records/me/private");
        let wins = gated("/records/me/{id}");
        let public_loser = route("/records/{user}/private", "GET", "public", &[], &[], false);

        let base = routes_only(&format!("{removed},{wins},{public_loser}"));
        let head = routes_only(&format!("{wins},{public_loser}"));

        assert!(
            widening(&diff(&base, &head)).is_empty(),
            "the public route never sees that URL: {:#?}",
            diff(&base, &head)
        );
    }

    /// Several routes can take a deleted route's URLs, and precedence sends
    /// different URLs to different ones. Accepting the check from *any* of them
    /// suppressed the finding when the route that actually wins does not carry
    /// it: removing bound `/records/me/private`, `/records/me/{id}` wins over
    /// `/records/{user}/private`, so a check only the latter performs is gone
    /// from the URLs that matter.
    #[test]
    fn a_binding_only_some_takers_perform_is_still_a_widening() {
        let gated = |path: &str| route(path, "GET", "gated", &["user"], &[], true);
        let binding = |path: &str| {
            format!(
                r#"{{"path":"{path}","method":"GET","action":"read_self","resource":"Record"}}"#
            )
        };
        let removed = gated("/records/me/private");
        let wins = gated("/records/me/{id}");
        let also_takes = gated("/records/{user}/private");

        let base = manifest(
            &format!("{removed},{wins},{also_takes}"),
            "",
            "",
            &format!(
                "{},{}",
                binding("/records/me/private"),
                // Only the route that does *not* win carries the same check.
                binding("/records/{user}/private")
            ),
        );
        let head = manifest(
            &format!("{wins},{also_takes}"),
            "",
            "",
            &binding("/records/{user}/private"),
        );

        let findings = diff(&base, &head);
        let widening = widening(&findings);
        assert_eq!(widening.len(), 1, "{findings:#?}");
        assert_eq!(widening[0].kind, "authorization_binding_removed");
    }

    /// A `WS` route and a `GET` route at one path are the same runtime handler,
    /// and the csrf rows are keyed by the *declared* method. Replacing an
    /// enforced `WS /x` with a `GET /x` that does not enforce left the base row
    /// under a key the head does not have, and the exact-key disappearance
    /// check skipped it — while the route diff saw an equally guarded
    /// replacement, so nothing reported the lost CSRF at all.
    #[test]
    fn csrf_lost_when_a_ws_route_becomes_a_get_is_still_a_widening() {
        let base = manifest(
            &route("/x", "WS", "gated", &["user"], &[], false),
            r#"{"path":"/x","method":"WS","csrf_enforced":true,"exempt":false}"#,
            "",
            "",
        );
        let head = manifest(
            &route("/x", "GET", "gated", &["user"], &[], false),
            "",
            "",
            "",
        );

        let findings = diff(&base, &head);
        let widening = widening(&findings);
        assert_eq!(widening.len(), 1, "{findings:#?}");
        assert_eq!(widening[0].kind, "csrf_enforcement_removed");
    }

    /// And the CSRF acknowledgment binds to what still guards the route, like
    /// every other dimension: losing CSRF behind a new `mfa` scope is not the
    /// same decision as losing it with the scope gone again.
    #[test]
    fn a_csrf_acknowledgment_binds_to_what_still_guards_the_route() {
        let entry = |enforced: bool| {
            format!(
                r#"{{"path":"/pay","method":"POST","csrf_enforced":{enforced},"exempt":false}}"#
            )
        };
        let base = manifest(
            &route("/pay", "POST", "gated", &["admin"], &[], false),
            &entry(true),
            "",
            "",
        );
        let guarded_by = |scopes: &[&str]| {
            diff(
                &base,
                &manifest(
                    &route("/pay", "POST", "gated", &["admin"], scopes, false),
                    &entry(false),
                    "",
                    "",
                ),
            )
            .into_iter()
            .find(|f| f.kind == "csrf_enforcement_removed")
            .expect("losing CSRF is a widening")
            .canonical()
        };

        assert_ne!(guarded_by(&["mfa"]), guarded_by(&[]));
    }

    /// A policy acknowledgment has the same duty. Removing the record check
    /// while adding an `mfa` scope is a widening the reviewer weighs against
    /// what still guards the route; dropping `mfa` later leaves the same
    /// finding with a constant fingerprint, so the old marker covered a route
    /// less constrained than the one that was reviewed.
    #[test]
    fn a_policy_acknowledgment_binds_to_what_still_guards_the_route() {
        let base = routes_only(&route("/a", "PUT", "gated", &["user"], &[], true));
        let guarded_by = |scopes: &[&str]| {
            diff(
                &base,
                &routes_only(&route("/a", "PUT", "gated", &["user"], scopes, false)),
            )
            .into_iter()
            .find(|f| f.kind == "policy_removed")
            .expect("losing the record check is a widening")
            .canonical()
        };

        assert_ne!(guarded_by(&["mfa"]), guarded_by(&[]));
    }

    /// The acknowledgment digest keys on the same shape everything else does.
    /// A capture rename is not a posture change, so it must not re-block a
    /// widening a reviewer has already approved.
    #[test]
    fn renaming_a_capture_does_not_invalidate_an_acknowledgment() {
        let widened = |path: &str| {
            let base = routes_only(&route(path, "GET", "gated", &["admin"], &[], false));
            let head = routes_only(&route(
                path,
                "GET",
                "gated",
                &["admin", "editor"],
                &[],
                false,
            ));
            only(diff(&base, &head)).canonical()
        };

        assert_eq!(widened("/users/{id}"), widened("/users/{user_id}"));
    }

    /// A scope acknowledgment has to bind to the constraint that replaced the
    /// one it approved. Going from `{read}` to `{mfa}` is a widening whose
    /// fingerprint named only the lost `read`; a later push dropping `mfa`
    /// leaves the route requiring no scope at all, and neither the finding nor
    /// the digest moved, so the acknowledgment for one covered the other.
    #[test]
    fn a_scope_acknowledgment_binds_to_the_scope_that_replaced_it() {
        let base = routes_only(&route("/a", "GET", "gated", &["user"], &["read"], false));
        let now_requiring = |scopes: &[&str]| {
            diff(
                &base,
                &routes_only(&route("/a", "GET", "gated", &["user"], scopes, false)),
            )
            .into_iter()
            .find(|f| f.kind == "scopes_widened")
            .expect("losing `read` is a widening")
            .canonical()
        };

        assert_ne!(
            now_requiring(&["mfa"]),
            now_requiring(&[]),
            "acknowledging a swap to `mfa` must not authorize dropping it too"
        );
    }

    /// The replacement checks must come from the route that actually serves
    /// the URL. Collecting them from every overlapping route let a losing
    /// dynamic route's bindings mask the winner's: with `/records/{id}`
    /// carrying both `read_any` and `read_all`, the flattened set never moved
    /// when the exact route swapped one for the other.
    #[test]
    fn replacement_checks_come_only_from_the_route_that_serves_the_url() {
        let me = route("/records/me", "GET", "gated", &["user"], &[], true);
        let by_id = route("/records/{id}", "GET", "gated", &["user"], &[], true);
        let binding = |path: &str, action: &str| {
            format!(r#"{{"path":"{path}","method":"GET","action":"{action}","resource":"Record"}}"#)
        };
        let losing = format!(
            "{},{}",
            binding("/records/{id}", "read_any"),
            binding("/records/{id}", "read_all")
        );
        let base = manifest(
            &format!("{me},{by_id}"),
            "",
            "",
            &format!("{},{losing}", binding("/records/me", "read_self")),
        );
        let swapped_to = |action: &str| {
            diff(
                &base,
                &manifest(
                    &format!("{me},{by_id}"),
                    "",
                    "",
                    &format!("{},{losing}", binding("/records/me", action)),
                ),
            )
            .into_iter()
            .find(|f| f.kind == "authorization_binding_removed" && f.path == "/records/me")
            .expect("losing `read_self` is a widening")
            .canonical()
        };

        assert_ne!(swapped_to("read_any"), swapped_to("read_all"));
    }

    /// The reverse of the guarded-`GET` case, and it happens *inside* one path
    /// node. A `GET` answers `HEAD` by fallback until an explicit `HEAD` is
    /// mounted beside it, so adding an editor-only `HEAD /x` beside an
    /// admin-only `GET /x` hands that method to editors. Stopping at "the node
    /// already exists" made it a neutral new handler.
    #[test]
    fn an_explicit_head_takes_the_fallback_from_a_get_at_the_same_path() {
        let get = route("/x", "GET", "gated", &["admin"], &[], false);
        let head_route = route("/x", "HEAD", "gated", &["editor"], &[], false);
        let base = routes_only(&get);
        let head = routes_only(&format!("{get},{head_route}"));

        let findings = diff(&base, &head);
        let widening = widening(&findings);
        assert_eq!(widening.len(), 1, "{findings:#?}");
        assert_eq!(widening[0].kind, "route_added_shadowing");
    }

    /// And the same trade in the other direction: removing the explicit `HEAD`
    /// hands that method back to the `GET`'s fallback.
    #[test]
    fn removing_an_explicit_head_hands_it_to_the_get_at_the_same_path() {
        let get = route("/x", "GET", "gated", &["editor"], &[], false);
        let head_route = route("/x", "HEAD", "gated", &["admin"], &[], false);
        let base = routes_only(&format!("{get},{head_route}"));
        let head = routes_only(&get);

        let findings = diff(&base, &head);
        let widening = widening(&findings);
        assert_eq!(widening.len(), 1, "{findings:#?}");
        assert_eq!(widening[0].kind, "route_shadow_exposed");
    }

    /// A path node the base already mounts owns that URL for every method, so
    /// adding another method there displaces nothing — it is an ordinary new
    /// guarded handler on a node that was already answering.
    #[test]
    fn adding_a_method_to_an_existing_path_node_displaces_nothing() {
        let existing = route("/users/me", "POST", "gated", &["user"], &[], false);
        let dynamic = route("/users/{id}", "GET", "gated", &["admin"], &[], false);
        let added = route("/users/me", "GET", "gated", &["editor"], &[], false);

        let base = routes_only(&format!("{existing},{dynamic}"));
        let head = routes_only(&format!("{existing},{dynamic},{added}"));

        let findings = diff(&base, &head);
        assert!(
            widening(&findings).is_empty(),
            "the node already owned that URL: {findings:#?}"
        );
    }

    /// And the binding dimension stops at a surviving node too: removing a
    /// bound `GET /records/me` while `POST /records/me` stays mounted makes
    /// that URL a 405, not a fall-through.
    #[test]
    fn a_binding_does_not_fall_through_a_surviving_path_node() {
        let bound = route("/records/me", "GET", "gated", &["user"], &[], true);
        let same_path = route("/records/me", "POST", "gated", &["user"], &[], true);
        let dynamic = route("/records/{id}", "GET", "gated", &["user"], &[], true);
        let binding =
            r#"{"path":"/records/me","method":"GET","action":"read","resource":"Record"}"#;

        let base = manifest(&format!("{bound},{same_path},{dynamic}"), "", "", binding);
        let head = manifest(&format!("{same_path},{dynamic}"), "", "", "");

        let findings = diff(&base, &head);
        assert!(
            widening(&findings).is_empty(),
            "`GET /records/me` is not reachable at all now: {findings:#?}"
        );
    }

    /// The replacement binding matters when the route *stays* mounted too. A
    /// route that swaps `read_self` for `read_any` keeps its key, so nothing
    /// fell through — and with the replacement left out of the fingerprint, a
    /// later push to `read_all` moved neither the removal finding nor the
    /// digest, and the acknowledgment for `read_any` still stood.
    #[test]
    fn an_exact_route_acknowledgment_binds_to_its_replacement_check() {
        let by_id = route("/records/{id}", "GET", "gated", &["user"], &[], true);
        let binding = |action: &str| {
            format!(
                r#"{{"path":"/records/{{id}}","method":"GET","action":"{action}","resource":"Record"}}"#
            )
        };
        let base = manifest(&by_id, "", "", &binding("read_self"));
        let replaced_by = |action: &str| {
            diff(&base, &manifest(&by_id, "", "", &binding(action)))
                .into_iter()
                .find(|f| f.kind == "authorization_binding_removed")
                .expect("losing `read_self` is a widening")
                .canonical()
        };

        assert_ne!(replaced_by("read_any"), replaced_by("read_all"));
    }

    /// Ranking reaches the binding displacement too: a base route that never
    /// served the URL cannot have its check "lost" by the route that takes it.
    #[test]
    fn a_displaced_binding_is_compared_against_the_prior_winner() {
        let gated = |path: &str| route(path, "GET", "gated", &["user"], &[], true);
        let binding = |path: &str, action: &str| {
            format!(r#"{{"path":"{path}","method":"GET","action":"{action}","resource":"Record"}}"#)
        };
        let prior_owner = gated("/records/me/{id}");
        let never_served = gated("/records/{user}/private");
        let added = gated("/records/me/private");

        let base = manifest(
            &format!("{prior_owner},{never_served}"),
            "",
            "",
            &format!(
                "{},{}",
                binding("/records/me/{id}", "read_self"),
                binding("/records/{user}/private", "read_other")
            ),
        );
        let head = manifest(
            &format!("{prior_owner},{never_served},{added}"),
            "",
            "",
            &format!(
                "{},{},{}",
                binding("/records/me/{id}", "read_self"),
                binding("/records/{user}/private", "read_other"),
                // The new route carries the check the prior owner performed.
                binding("/records/me/private", "read_self")
            ),
        );

        let findings = diff(&base, &head);
        assert!(
            widening(&findings).is_empty(),
            "`read_other` never applied to that URL: {findings:#?}"
        );
    }

    /// The round-15 lesson, retrofitted to the binding findings. An
    /// acknowledgment of a fall-through says "I accept that this URL is now
    /// checked by *that* instead" — so weakening what it is now checked by has
    /// to invalidate it. The survivor is absent from the base, so its binding
    /// reads only as an addition, and a fingerprint naming the lost check alone
    /// left the digest unmoved.
    #[test]
    fn a_fallthrough_acknowledgment_binds_to_the_checks_that_replace_it() {
        let me = route("/records/me", "GET", "gated", &["user"], &[], true);
        let by_id = route("/records/{id}", "GET", "gated", &["user"], &[], true);
        let binding = |path: &str, action: &str| {
            format!(r#"{{"path":"{path}","method":"GET","action":"{action}","resource":"Record"}}"#)
        };
        let base = manifest(
            &format!("{me},{by_id}"),
            "",
            "",
            &format!(
                "{},{}",
                binding("/records/me", "read_self"),
                binding("/records/{id}", "read_any")
            ),
        );
        let lost_to = |action: &str| {
            diff(
                &base,
                &manifest(&by_id, "", "", &binding("/records/{id}", action)),
            )
            .into_iter()
            .find(|f| f.kind == "authorization_binding_removed" && f.path == "/records/me")
            .expect("the fall-through is a widening")
            .canonical()
        };

        assert_ne!(
            lost_to("read_any"),
            lost_to("read_weak"),
            "acknowledging a fall-through to `read_any` must not authorize one to `read_weak`"
        );
    }

    /// And the displacement side, which has the same shape.
    #[test]
    fn a_displaced_binding_acknowledgment_binds_to_the_replacing_checks() {
        let by_id = route("/records/{id}", "GET", "gated", &["user"], &[], true);
        let me = route("/records/me", "GET", "gated", &["user"], &[], true);
        let binding = |path: &str, action: &str| {
            format!(r#"{{"path":"{path}","method":"GET","action":"{action}","resource":"Record"}}"#)
        };
        let base = manifest(&by_id, "", "", &binding("/records/{id}", "read_self"));
        let displaced_by = |action: &str| {
            diff(
                &base,
                &manifest(
                    &format!("{by_id},{me}"),
                    "",
                    "",
                    &format!(
                        "{},{}",
                        binding("/records/{id}", "read_self"),
                        binding("/records/me", action)
                    ),
                ),
            )
            .into_iter()
            .find(|f| f.kind == "authorization_binding_displaced")
            .expect("the displacement is a widening")
            .canonical()
        };

        assert_ne!(displaced_by("read_any"), displaced_by("read_weak"));
    }

    /// The same explicit-`HEAD` lesson, in the binding dimension. With base
    /// `GET /x` (bound) and `HEAD /x` both mounted, deleting only the `GET`
    /// takes its URL away — the explicit `HEAD` owned `HEAD` all along and
    /// gains nothing. Reading the alias unconditionally reported the binding as
    /// lost from a still-reachable URL and blocked.
    #[test]
    fn a_binding_removed_beside_an_explicit_head_is_not_a_widening() {
        let get = route("/x", "GET", "gated", &["user"], &[], true);
        let head_route = route("/x", "HEAD", "gated", &["user"], &[], true);
        let binding = r#"{"path":"/x","method":"GET","action":"read","resource":"Thing"}"#;
        let base = manifest(&format!("{get},{head_route}"), "", "", binding);
        let head = manifest(&head_route, "", "", "");

        let findings = diff(&base, &head);
        assert!(
            widening(&findings).is_empty(),
            "the GET URL left with the route: {findings:#?}"
        );
    }

    /// Narrowing an exemption is not adding one. Every URL `/api/private`
    /// exempts was already exempt under `/api`, so replacing the broad prefix
    /// with the narrow one restores CSRF for most of `/api` — a literal set
    /// difference called the new spelling a widening and blocked it.
    #[test]
    fn narrowing_a_csrf_exemption_prefix_is_not_a_widening() {
        let route = route("/api/{rest}", "POST", "gated", &["user"], &[], false);
        let entry = r#"{"path":"/api/{rest}","method":"POST","csrf_enforced":false,"exempt":true}"#;
        let base = manifest_exempt(&route, entry, &["/api"]);
        let head = manifest_exempt(&route, entry, &["/api/private"]);

        let findings = diff(&base, &head);
        assert!(
            widening(&findings).is_empty(),
            "every URL still exempt was already exempt: {findings:#?}"
        );
        assert!(
            findings.iter().any(|f| f.kind == "csrf_exemption_removed"),
            "and the narrowing is still reported: {findings:#?}"
        );
    }

    /// Broadening one still blocks.
    #[test]
    fn broadening_a_csrf_exemption_prefix_is_a_widening() {
        let route = route("/api/{rest}", "POST", "gated", &["user"], &[], false);
        let entry = r#"{"path":"/api/{rest}","method":"POST","csrf_enforced":false,"exempt":true}"#;
        let base = manifest_exempt(&route, entry, &["/api/private"]);
        let head = manifest_exempt(&route, entry, &["/api"]);

        let findings = diff(&base, &head);
        let widening = widening(&findings);
        assert_eq!(widening.len(), 1, "{findings:#?}");
        assert_eq!(widening[0].kind, "csrf_exemption_added");
    }

    /// An exemption prefix is the one CSRF widening the per-route entries
    /// cannot show. `path_is_exempt` compares the route *template*, so
    /// `/users/{id}` stays `enforced` in the manifest, while the runtime
    /// compares the concrete request path and skips CSRF for `POST /users/me`.
    /// With `exempt_paths` dropped on parse, neither the digest nor the diff
    /// moved at all.
    #[test]
    fn adding_a_csrf_exemption_prefix_is_a_widening() {
        let route = route("/users/{id}", "POST", "gated", &["user"], &[], false);
        let entry = r#"{"path":"/users/{id}","method":"POST","csrf_enforced":true,"exempt":false}"#;
        let base = manifest_exempt(&route, entry, &[]);
        let head = manifest_exempt(&route, entry, &["/users/me"]);

        let findings = diff(&base, &head);
        let widening = widening(&findings);
        assert_eq!(widening.len(), 1, "{findings:#?}");
        assert_eq!(widening[0].kind, "csrf_exemption_added");
        assert!(
            widening[0].detail.contains("/users/me"),
            "{:?}",
            widening[0]
        );
    }

    /// Withdrawing one protects more, so it annotates rather than blocks.
    #[test]
    fn removing_a_csrf_exemption_prefix_is_a_narrowing() {
        let route = route("/users/{id}", "POST", "gated", &["user"], &[], false);
        let entry = r#"{"path":"/users/{id}","method":"POST","csrf_enforced":true,"exempt":false}"#;
        let base = manifest_exempt(&route, entry, &["/users/me"]);
        let head = manifest_exempt(&route, entry, &[]);

        let findings = diff(&base, &head);
        assert_eq!(
            kinds(&findings),
            vec!["csrf_exemption_removed"],
            "{findings:#?}"
        );
        assert!(widening(&findings).is_empty());
    }

    /// An explicit `HEAD` handler is kept by axum even when a `GET` is mounted
    /// at the same path — the `GET`'s automatic `HEAD` support is a *fallback*,
    /// not a takeover. Treating the alias as unconditional made a new guarded
    /// `GET` displace an explicit `HEAD` that keeps answering exactly as before.
    #[test]
    fn a_new_get_does_not_displace_an_explicit_head() {
        let explicit_head = route("/x", "HEAD", "gated", &["admin"], &[], false);
        let added_get = route("/x", "GET", "gated", &["editor"], &[], false);
        let base = routes_only(&explicit_head);
        let head = routes_only(&format!("{explicit_head},{added_get}"));

        let findings = diff(&base, &head);
        assert!(
            widening(&findings).is_empty(),
            "the explicit HEAD still answers HEAD: {findings:#?}"
        );
    }

    /// A displacement can change the record-level check without moving
    /// anything the route comparison sees: same roles, both `policy: true`,
    /// and the binding differ sees only an *added* binding, which is a
    /// narrowing. The URLs `/records/me` took over went from `read_self` to
    /// `read_any` all the same.
    #[test]
    fn a_binding_displaced_by_a_more_specific_route_is_a_widening() {
        let by_id = route("/records/{id}", "GET", "gated", &["user"], &[], true);
        let me = route("/records/me", "GET", "gated", &["user"], &[], true);
        let binding = |path: &str, action: &str| {
            format!(r#"{{"path":"{path}","method":"GET","action":"{action}","resource":"Record"}}"#)
        };
        let base = manifest(&by_id, "", "", &binding("/records/{id}", "read_self"));
        let head = manifest(
            &format!("{by_id},{me}"),
            "",
            "",
            &format!(
                "{},{}",
                binding("/records/{id}", "read_self"),
                binding("/records/me", "read_any")
            ),
        );

        let findings = diff(&base, &head);
        let widening = widening(&findings);
        assert_eq!(widening.len(), 1, "{findings:#?}");
        assert_eq!(widening[0].kind, "authorization_binding_displaced");
        assert!(
            widening[0].detail.contains("read_self"),
            "{:?}",
            widening[0]
        );
    }

    /// The same check carried onto the new route changes nothing.
    #[test]
    fn a_binding_carried_onto_the_displacing_route_is_not_a_widening() {
        let by_id = route("/records/{id}", "GET", "gated", &["user"], &[], true);
        let me = route("/records/me", "GET", "gated", &["user"], &[], true);
        let binding = |path: &str| {
            format!(
                r#"{{"path":"{path}","method":"GET","action":"read_self","resource":"Record"}}"#
            )
        };
        let base = manifest(&by_id, "", "", &binding("/records/{id}"));
        let head = manifest(
            &format!("{by_id},{me}"),
            "",
            "",
            &format!("{},{}", binding("/records/{id}"), binding("/records/me")),
        );

        assert!(
            widening(&diff(&base, &head)).is_empty(),
            "{:#?}",
            diff(&base, &head)
        );
    }

    /// The mirror of the removal case. Adding a route that is *more specific*
    /// than an existing one takes that route's requests over: with a base
    /// `/users/{id}` restricted to `admin`, a new `/users/me` restricted to
    /// `editor` wins `/users/me` outright, and editors reach what needed
    /// `admin` a moment ago. The dynamic entry never moved, so the new route
    /// was filed as a neutral `route_added_gated` and nothing blocked.
    #[test]
    fn adding_a_more_specific_route_that_admits_more_is_a_widening() {
        let by_id = route("/users/{id}", "GET", "gated", &["admin"], &[], false);
        let me = route("/users/me", "GET", "gated", &["editor"], &[], false);
        let base = routes_only(&by_id);
        let head = routes_only(&format!("{by_id},{me}"));

        let findings = diff(&base, &head);
        let widening = widening(&findings);
        assert_eq!(widening.len(), 1, "{findings:#?}");
        assert_eq!(widening[0].kind, "route_added_shadowing");
        assert_eq!(widening[0].path, "/users/me");
        assert!(
            widening[0].detail.contains("/users/{id}"),
            "the finding must name the route whose requests it takes: {:?}",
            widening[0]
        );
    }

    /// Ranking applies to the displacement side too: what a new route takes
    /// over is whatever *was* serving those URLs, which is the most specific
    /// base route overlapping them. Comparing against every overlapping base
    /// route blamed a stricter handler that never served the URL.
    #[test]
    fn a_displacement_compares_against_the_route_that_was_serving_the_url() {
        let prior_owner = route("/records/me/{id}", "GET", "gated", &["user"], &[], false);
        let never_served = route(
            "/records/{user}/private",
            "GET",
            "gated",
            &["admin"],
            &[],
            false,
        );
        let added = route("/records/me/private", "GET", "gated", &["user"], &[], false);

        let base = routes_only(&format!("{prior_owner},{never_served}"));
        let head = routes_only(&format!("{prior_owner},{never_served},{added}"));

        let findings = diff(&base, &head);
        assert!(
            widening(&findings).is_empty(),
            "the route that was serving that URL has the same guard: {findings:#?}"
        );
    }

    /// The same shape, admitting no one new, is the ordinary neutral addition.
    #[test]
    fn adding_a_more_specific_route_with_the_same_guard_is_neutral() {
        let by_id = route("/users/{id}", "GET", "gated", &["admin"], &[], false);
        let me = route("/users/me", "GET", "gated", &["admin"], &[], false);
        let base = routes_only(&by_id);
        let head = routes_only(&format!("{by_id},{me}"));

        let findings = diff(&base, &head);
        assert_eq!(kinds(&findings), vec!["route_added_gated"], "{findings:#?}");
    }

    /// And carving a *guarded* route out of a public one narrows those URLs.
    #[test]
    fn adding_a_guard_over_part_of_a_public_route_is_not_a_widening() {
        let by_id = route("/users/{id}", "GET", "public", &[], &[], false);
        let me = route("/users/me", "GET", "gated", &["admin"], &[], false);
        let base = routes_only(&by_id);
        let head = routes_only(&format!("{by_id},{me}"));

        assert!(
            widening(&diff(&base, &head)).is_empty(),
            "{:#?}",
            diff(&base, &head)
        );
    }

    /// Same lesson as the fall-through fingerprint: the acknowledgment binds to
    /// the posture it was given for, on both sides of the displacement.
    #[test]
    fn a_displacement_acknowledgment_does_not_survive_a_looser_route() {
        let by_id = route("/users/{id}", "GET", "gated", &["admin"], &[], false);
        let base = routes_only(&by_id);
        let added_with = |roles: &[&str]| {
            let me = route("/users/me", "GET", "gated", roles, &[], false);
            diff(&base, &routes_only(&format!("{by_id},{me}")))
                .into_iter()
                .find(|f| f.kind == "route_added_shadowing")
                .expect("the displacement is a widening")
                .canonical()
        };

        assert_ne!(added_with(&["editor"]), added_with(&["guest"]));
    }

    /// Overlap is not enough: the router matches a static segment before a
    /// dynamic one, so a surviving `/users/me` was *already* answering that URL
    /// before the gated `/users/{id}` was deleted. Nothing falls through to it;
    /// the deletion only takes the remaining dynamic URLs away. Reporting it
    /// blocked a pull request over an exposure that cannot happen.
    #[test]
    fn a_more_specific_survivor_gains_nothing_and_is_not_exposure() {
        let survivor = route("/users/me", "GET", "public", &[], &[], false);
        let removed = route("/users/{id}", "GET", "gated", &["user"], &[], false);
        let base = routes_only(&format!("{survivor},{removed}"));
        let head = routes_only(&survivor);

        assert!(
            widening(&diff(&base, &head)).is_empty(),
            "{:#?}",
            diff(&base, &head)
        );
    }

    /// Precedence decides it position by position, so a survivor that is less
    /// specific only where it matters still gains the overlap. `/a/x/{id}` beat
    /// `/a/{key}/b` at `/a/x/b` while it existed; now it does not.
    #[test]
    fn a_survivor_that_wins_the_overlap_only_after_the_deletion_is_exposure() {
        let survivor = route("/a/{key}/b", "GET", "public", &[], &[], false);
        let removed = route("/a/x/{id}", "GET", "gated", &["user"], &[], false);
        let base = routes_only(&format!("{survivor},{removed}"));
        let head = routes_only(&survivor);

        let findings = diff(&base, &head);
        let widening = widening(&findings);
        assert_eq!(widening.len(), 1, "{findings:#?}");
        assert_eq!(widening[0].kind, "route_shadow_exposed");
    }

    /// A binding is the same story: the URLs `/records/{id}` served are gone,
    /// and the static route that survives was already answering its own.
    #[test]
    fn a_binding_whose_urls_left_with_it_is_not_a_widening() {
        let me = route("/records/me", "GET", "gated", &["user"], &[], true);
        let by_id = route("/records/{id}", "GET", "gated", &["user"], &[], true);
        let binding = |path: &str, action: &str| {
            format!(r#"{{"path":"{path}","method":"GET","action":"{action}","resource":"Record"}}"#)
        };
        let base = manifest(
            &format!("{me},{by_id}"),
            "",
            "",
            &format!(
                "{},{}",
                binding("/records/me", "read_self"),
                binding("/records/{id}", "read_any")
            ),
        );
        let head = manifest(&me, "", "", &binding("/records/me", "read_self"));

        assert!(
            widening(&diff(&base, &head)).is_empty(),
            "{:#?}",
            diff(&base, &head)
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

    /// A route's declared method is not the set of requests it answers: the
    /// router mounts `WS` as a plain `GET`, and axum serves `HEAD` through a
    /// `#[get]` handler. So a public `GET /users/{id}` picks up the `HEAD` and
    /// `WS` traffic of a deleted guarded route at the same path, and comparing
    /// declared methods exactly skipped exactly those survivors.
    #[test]
    fn a_survivor_shadows_the_methods_it_actually_answers() {
        for removed_method in ["HEAD", "WS", "GET"] {
            let survivor = route("/users/{id}", "GET", "public", &[], &[], false);
            let guarded = route("/users/me", removed_method, "gated", &["user"], &[], false);
            let base = routes_only(&format!("{survivor},{guarded}"));
            let head = routes_only(&survivor);

            let findings = diff(&base, &head);
            assert!(
                findings.iter().any(|f| f.kind == "route_shadow_exposed"),
                "a GET survivor answers {removed_method} at that URL: {findings:#?}"
            );
        }
    }

    /// A websocket upgrade is not served for `HEAD`. The router says so in
    /// those words — only a genuine `GET` expands, "not the WS→GET alias" — so
    /// folding `WS` to `GET` and *then* adding `HEAD` invented an overlap and
    /// blocked a pull request over traffic the survivor never answers.
    #[test]
    fn a_websocket_survivor_does_not_answer_head() {
        let survivor = route("/live/{id}", "WS", "public", &[], &[], false);
        let guarded = route("/live/me", "HEAD", "gated", &["user"], &[], false);
        let base = routes_only(&format!("{survivor},{guarded}"));
        let head = routes_only(&survivor);

        let findings = diff(&base, &head);
        assert!(
            !findings.iter().any(|f| f.kind == "route_shadow_exposed"),
            "a websocket upgrade is not served for HEAD: {findings:#?}"
        );
    }

    /// An acknowledgment of a fall-through has to bind to the posture it was
    /// given for. When the same pull request removes a guarded route and adds
    /// the route that now covers it, the new route is absent from the base — so
    /// loosening it later is only a neutral `route_added_gated`, and with the
    /// survivor's posture out of the fingerprint the digest never moved and the
    /// old acknowledgment still stood.
    #[test]
    fn a_shadow_acknowledgment_does_not_survive_a_looser_survivor() {
        let base = routes_only(&route("/users/me", "GET", "gated", &["admin"], &[], false));
        let shadowed_by = |roles: &[&str]| {
            let findings = diff(
                &base,
                &routes_only(&route("/users/{id}", "GET", "gated", roles, &[], false)),
            );
            let shadow = findings
                .into_iter()
                .find(|f| f.kind == "route_shadow_exposed")
                .expect("the fall-through is a widening");
            shadow.canonical()
        };

        assert_ne!(
            shadowed_by(&["editor"]),
            shadowed_by(&["guest"]),
            "acknowledging a fall-through to `editor` must not authorize one to `guest`"
        );
    }

    /// The other direction: while the survivor itself is untouched, the digest
    /// must not move. An acknowledgment that a later, unrelated push
    /// invalidates is an escape hatch nobody can use.
    #[test]
    fn a_shadow_fingerprint_survives_an_unrelated_push() {
        let base = routes_only(&route("/users/me", "GET", "gated", &["admin"], &[], false));
        let survivor = route("/users/{id}", "GET", "gated", &["editor"], &[], false);
        let elsewhere = route("/reports", "GET", "gated", &["admin"], &[], false);

        let shadow = |routes: &str| {
            diff(&base, &routes_only(routes))
                .into_iter()
                .find(|f| f.kind == "route_shadow_exposed")
                .expect("the fall-through is a widening")
                .canonical()
        };

        assert_eq!(
            shadow(&survivor),
            shadow(&format!("{survivor},{elsewhere}"))
        );
    }

    /// And a method the survivor genuinely does not answer is not *shadowed* by
    /// it: the deleted `POST` handler has no `GET` successor to inherit its
    /// guard. The path node vanishing is a separate finding, covered by
    /// `removing_the_last_route_at_a_path_exposes_its_other_methods`.
    #[test]
    fn a_survivor_does_not_shadow_a_method_it_never_answers() {
        let survivor = route("/users/{id}", "GET", "public", &[], &[], false);
        let guarded = route("/users/me", "POST", "gated", &["user"], &[], false);
        let base = routes_only(&format!("{survivor},{guarded}"));
        let head = routes_only(&survivor);

        let findings = diff(&base, &head);
        assert!(
            !findings.iter().any(|f| f.kind == "route_shadow_exposed"),
            "{findings:#?}"
        );
    }

    /// The same fall-through, one dimension over. Both routes are `gated` with
    /// a record-level check, so the route comparison sees no widening — but the
    /// URL now resolves to a route that checks something *else*, and the
    /// binding dimension suppressed the loss because the exact route key was
    /// gone.
    #[test]
    fn a_binding_lost_to_fallthrough_is_still_a_widening() {
        let routes = |extra: &str| {
            let by_id = route("/records/{id}", "GET", "gated", &["user"], &[], true);
            if extra.is_empty() {
                by_id
            } else {
                format!("{by_id},{extra}")
            }
        };
        let binding = |path: &str, action: &str| {
            format!(r#"{{"path":"{path}","method":"GET","action":"{action}","resource":"Record"}}"#)
        };
        let base = manifest(
            &routes(&route("/records/me", "GET", "gated", &["user"], &[], true)),
            "",
            "",
            &format!(
                "{},{}",
                binding("/records/{id}", "read_any"),
                binding("/records/me", "read_self")
            ),
        );
        let head = manifest(&routes(""), "", "", &binding("/records/{id}", "read_any"));

        let findings = diff(&base, &head);
        let widening = widening(&findings);
        assert_eq!(widening.len(), 1, "{findings:#?}");
        assert_eq!(widening[0].kind, "authorization_binding_removed");
        assert!(
            widening[0].detail.contains("read_self"),
            "{:?}",
            widening[0]
        );
    }

    /// But if the route that now serves the URL performs the very same check,
    /// nothing changed for its callers.
    #[test]
    fn a_binding_the_surviving_route_still_performs_is_not_a_widening() {
        let by_id = route("/records/{id}", "GET", "gated", &["user"], &[], true);
        let me = route("/records/me", "GET", "gated", &["user"], &[], true);
        let binding = |path: &str| {
            format!(r#"{{"path":"{path}","method":"GET","action":"read","resource":"Record"}}"#)
        };
        let base = manifest(
            &format!("{by_id},{me}"),
            "",
            "",
            &format!("{},{}", binding("/records/{id}"), binding("/records/me")),
        );
        let head = manifest(&by_id, "", "", &binding("/records/{id}"));

        assert!(
            widening(&diff(&base, &head)).is_empty(),
            "{:#?}",
            diff(&base, &head)
        );
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
