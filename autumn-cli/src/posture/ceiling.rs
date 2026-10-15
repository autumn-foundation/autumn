//! The ceiling: an acknowledgment that says "no wider than this".
//!
//! A bare digest is compared for equality, so it cannot tell a constraint that
//! appeared after the acknowledgment from one that vanished (#2497). A marker
//! with a ceiling carries the approved posture of each widened route instead.
//! Verification then asks one question: is the route, now, no wider than that?
//!
//! ```text
//! /ack-posture 3f2a91c0d47b5e6a ceiling=GET|%2Fadmin|public|0:|0:|0|on|0:|0  reason
//! ```
//!
//! Narrowing passes. Dropping a constraint re-blocks. Findings that name no
//! single route at head keep the exact digest.

use std::collections::{BTreeMap, BTreeSet};

use super::diff::Finding;
use super::model::{PostureManifest, RouteKey};

/// One route's whole posture, in a form that can be ordered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutePosture {
    classification: String,
    roles: BTreeSet<String>,
    scopes: BTreeSet<String>,
    policy: bool,
    csrf: Csrf,
    checks: BTreeSet<String>,
    mtls: bool,
}

/// CSRF state of a route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Csrf {
    On,
    Off,
    Absent,
}

impl RoutePosture {
    /// Whether `self` admits no caller that `ceiling` refuses.
    ///
    /// Uses the differ's widening rules: roles OR-ed, scopes AND-ed, an open
    /// classification widest, the rest booleans.
    #[must_use]
    pub fn no_wider_than(&self, ceiling: &Self) -> bool {
        let _ = ceiling;
        todo!("red phase")
    }
}

/// Approved postures, by route.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Ceiling {
    routes: BTreeMap<RouteKey, RoutePosture>,
}

impl Ceiling {
    /// Whether no route is covered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.routes.is_empty()
    }

    /// The text form carried in a marker. Deterministic, no whitespace.
    #[must_use]
    pub fn encode(&self) -> String {
        todo!("red phase")
    }

    /// Parse [`Self::encode`] output. `None` for anything malformed.
    #[must_use]
    pub fn decode(text: &str) -> Option<Self> {
        let _ = text;
        todo!("red phase")
    }
}

/// What a widening set is bound to: identity plus the current ceiling.
#[derive(Debug, Clone)]
pub struct Cover {
    /// Postures at head of the routes that widened.
    pub ceiling: Ceiling,
    identity: String,
}

/// Split `widening` into route findings (bound by ceiling) and the rest
/// (bound exactly), against the head manifest.
#[must_use]
pub fn cover(widening: &[&Finding], head: &PostureManifest) -> Cover {
    let _ = (widening, head);
    todo!("red phase")
}

impl Cover {
    /// The digest a ceiling marker carries for the current head.
    #[must_use]
    pub fn seal(&self) -> String {
        todo!("red phase")
    }

    /// Whether a marker (`digest` + `payload`) approves the current head.
    #[must_use]
    pub fn accepts(&self, payload: &str, digest: &str) -> bool {
        let _ = (payload, digest);
        todo!("red phase")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::posture::diff::{Severity, diff, widening};

    /// A one-route manifest; `extra` adds csrf, authz and mtls rows.
    struct Fixture {
        class: &'static str,
        roles: Vec<&'static str>,
        scopes: Vec<&'static str>,
        policy: bool,
        csrf: Option<bool>,
        checks: Vec<&'static str>,
        mtls: bool,
    }

    impl Fixture {
        fn new(class: &'static str) -> Self {
            Self {
                class,
                roles: vec![],
                scopes: vec![],
                policy: false,
                csrf: None,
                checks: vec![],
                mtls: false,
            }
        }
        fn roles(mut self, r: &[&'static str]) -> Self {
            self.roles = r.to_vec();
            self
        }
        fn scopes(mut self, s: &[&'static str]) -> Self {
            self.scopes = s.to_vec();
            self
        }
        fn policy(mut self, p: bool) -> Self {
            self.policy = p;
            self
        }
        fn csrf(mut self, enforced: bool) -> Self {
            self.csrf = Some(enforced);
            self
        }
        fn checks(mut self, c: &[&'static str]) -> Self {
            self.checks = c.to_vec();
            self
        }
        fn mtls(mut self, m: bool) -> Self {
            self.mtls = m;
            self
        }

        fn manifest(&self) -> PostureManifest {
            let list = |v: &[&str]| {
                v.iter()
                    .map(|s| format!("\"{s}\""))
                    .collect::<Vec<_>>()
                    .join(",")
            };
            let csrf = self.csrf.map_or(String::new(), |enforced| {
                format!(
                    r#"{{"path":"/a","method":"POST","csrf_enforced":{enforced},"exempt":false}}"#
                )
            });
            let authz = self
                .checks
                .iter()
                .map(|c| {
                    format!(
                        r#"{{"path":"/a","method":"POST","name":"h","action":"{c}","resource":"R","provenance":"provable"}}"#
                    )
                })
                .collect::<Vec<_>>()
                .join(",");
            let json = format!(
                r#"{{"schema_version":4,"dimensions":{{
                  "routes":{{"provenance":"provable","source":"m","entries":[
                    {{"path":"/a","method":"POST","name":"h","classification":"{}",
                      "roles":[{}],"scopes":[{}],"policy":{},"source":"user",
                      "location":"x:1","provenance":"provable"}}]}},
                  "csrf":{{"provenance":"declared","source":"c","exempt_paths":[],"entries":[{csrf}]}},
                  "security_headers":{{"provenance":"declared","source":"c","entries":[]}},
                  "authorization_policies":{{"provenance":"provable","source":"m","runtime_caveat":"x","entries":[{authz}]}},
                  "mtls":{{"provenance":"declared","source":"c","mode":"off","required_paths":[],
                           "entries":[{{"path":"/a","method":"POST","mtls_required":{}}}]}}
                }},"excluded":[]}}"#,
                self.class,
                list(&self.roles),
                list(&self.scopes),
                self.policy,
                self.mtls
            );
            PostureManifest::parse(&json, "t.json").expect("fixture parses")
        }

        fn posture(&self) -> RoutePosture {
            let head = self.manifest();
            let f = Finding {
                kind: "k",
                severity: Severity::Widening,
                method: "POST".to_owned(),
                path: "/a".to_owned(),
                before: String::new(),
                after: String::new(),
                fingerprint: String::new(),
                detail: String::new(),
            };
            cover(&[&f], &head)
                .ceiling
                .routes
                .remove(&("/a".to_owned(), "POST".to_owned()))
                .expect("route is covered")
        }
    }

    /// Whether the differ calls `to` a widening of `from`.
    fn differ_widens(from: &Fixture, to: &Fixture) -> bool {
        !widening(&diff(&from.manifest(), &to.manifest())).is_empty()
    }

    fn grid() -> Vec<Fixture> {
        let mut all = Vec::new();
        for class in ["gated", "public", "framework"] {
            for roles in [&[][..], &["admin"], &["admin", "editor"], &["editor"]] {
                for scopes in [&[][..], &["mfa"], &["mfa", "write"]] {
                    for policy in [false, true] {
                        all.push(
                            Fixture::new(class)
                                .roles(roles)
                                .scopes(scopes)
                                .policy(policy),
                        );
                    }
                }
            }
        }
        all
    }

    // ── ordering ────────────────────────────────────────────────────────────

    /// The ordering is the differ's own widening rule, not a second opinion.
    #[test]
    fn ordering_agrees_with_the_differ_on_every_pair() {
        let all = grid();
        for ceiling in &all {
            for head in &all {
                assert_eq!(
                    head.posture().no_wider_than(&ceiling.posture()),
                    !differ_widens(ceiling, head),
                    "ceiling {:?}/{:?}/{:?}/{} vs head {:?}/{:?}/{:?}/{}",
                    ceiling.class,
                    ceiling.roles,
                    ceiling.scopes,
                    ceiling.policy,
                    head.class,
                    head.roles,
                    head.scopes,
                    head.policy,
                );
            }
        }
    }

    #[test]
    fn csrf_checks_and_mtls_follow_the_differ_too() {
        let all = [
            Fixture::new("gated").roles(&["a"]).csrf(true),
            Fixture::new("gated").roles(&["a"]).csrf(false),
            Fixture::new("gated").roles(&["a"]).csrf(true).checks(&["read"]),
            Fixture::new("gated").roles(&["a"]).csrf(true).checks(&["write"]),
            Fixture::new("gated")
                .roles(&["a"])
                .csrf(true)
                .checks(&["read", "write"]),
            Fixture::new("gated").roles(&["a"]).csrf(true).mtls(true),
        ];
        for ceiling in &all {
            for head in &all {
                if differ_widens(ceiling, head) {
                    assert!(
                        !head.posture().no_wider_than(&ceiling.posture()),
                        "the differ widens, so the ordering must too"
                    );
                }
            }
        }
    }

    #[test]
    fn a_posture_is_no_wider_than_itself() {
        for f in grid() {
            assert!(f.posture().no_wider_than(&f.posture()));
        }
    }

    // ── encoding ────────────────────────────────────────────────────────────

    #[test]
    fn encoding_round_trips_hostile_names() {
        let f = Fixture::new("gated")
            .roles(&["a,b", "", "x|y;z", "100%", "sp ace"])
            .scopes(&["é", "a=b"])
            .csrf(true)
            .checks(&["do it"]);
        let head = f.manifest();
        let widen = Finding {
            kind: "k",
            severity: Severity::Widening,
            method: "POST".to_owned(),
            path: "/a".to_owned(),
            before: String::new(),
            after: String::new(),
            fingerprint: String::new(),
            detail: String::new(),
        };
        let ceiling = cover(&[&widen], &head).ceiling;
        let text = ceiling.encode();
        assert!(
            !text.contains(char::is_whitespace),
            "one token on a comment line: {text}"
        );
        assert_eq!(Ceiling::decode(&text), Some(ceiling));
    }

    #[test]
    fn decoding_rejects_anything_malformed() {
        for bad in [
            "",
            "GET",
            "GET|%2Fa",
            "GET|%2Fa|gated|0:|0:|2|on|0:|0",
            "GET|%2Fa|gated|0:|0:|0|maybe|0:|0",
            "GET|%2Fa|gated|3:a|0:|0|on|0:|0",
            "GET|%2Fa|gated|0:|0:|0|on|0:|0|extra",
            "GET|%ZZ|gated|0:|0:|0|on|0:|0",
        ] {
            assert_eq!(Ceiling::decode(bad), None, "{bad:?}");
        }
    }

    // ── cover ───────────────────────────────────────────────────────────────

    fn finding(path: &str, fingerprint: &str) -> Finding {
        Finding {
            kind: "classification_downgraded",
            severity: Severity::Widening,
            method: "POST".to_owned(),
            path: path.to_owned(),
            before: String::new(),
            after: String::new(),
            fingerprint: fingerprint.to_owned(),
            detail: String::new(),
        }
    }

    #[test]
    fn a_route_finding_is_covered_and_a_global_one_is_not() {
        let head = Fixture::new("public").manifest();
        let route = finding("/a", "x");
        let global = finding("*", "h");
        let cover = cover(&[&route, &global], &head);
        assert_eq!(cover.ceiling.routes.len(), 1);
    }

    #[test]
    fn the_seal_ignores_the_route_fingerprint_but_not_a_global_one() {
        let head = Fixture::new("public").manifest();
        let seal = |route_fp: &str, global_fp: &str| {
            let (r, g) = (finding("/a", route_fp), finding("*", global_fp));
            cover(&[&r, &g], &head).seal()
        };
        assert_eq!(seal("one", "h"), seal("two", "h"));
        assert_ne!(seal("one", "h"), seal("one", "other"));
    }

    #[test]
    fn accepts_the_marker_it_printed() {
        let head = Fixture::new("public").manifest();
        let f = finding("/a", "x");
        let c = cover(&[&f], &head);
        assert!(c.accepts(&c.ceiling.encode(), &c.seal()));
    }

    #[test]
    fn rejects_a_ceiling_edited_after_it_was_sealed() {
        let f = finding("/a", "x");
        let c = cover(&[&f], &Fixture::new("gated").roles(&["a"]).manifest());
        let public = cover(&[&f], &Fixture::new("public").manifest());
        assert!(!c.accepts(&public.ceiling.encode(), &c.seal()));
    }

    #[test]
    fn rejects_a_ceiling_that_omits_a_widened_route() {
        let head = Fixture::new("public").manifest();
        let f = finding("/a", "x");
        let c = cover(&[&f], &head);
        let empty = Ceiling::default().encode();
        assert!(!c.accepts(&empty, &c.seal()));
    }
}
