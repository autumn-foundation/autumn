//! The ceiling: an acknowledgment that says "no wider than this".
//!
//! A bare digest must equal the current digest. It cannot show if a constraint
//! was added or removed (#2497). A ceiling marker holds the approved posture of
//! each widened route. The check asks one question: is the route no wider than
//! this?
//!
//! ```text
//! /ack-posture 3f2a91c0d47b5e6a ceiling=GET|/admin|public|0:|0:|0|on|0:|0  reason
//! ```
//!
//! A narrower route passes. A removed constraint blocks. A finding with no
//! single route at head uses the exact digest.

use std::collections::{BTreeMap, BTreeSet};

use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};

use super::ack::{CEILING_KEY, short};
use super::diff::{Finding, authz_index, checks_at, csrf_index, mtls_index, route_index};
use super::model::{PostureManifest, RouteKey, hex_digest, is_open, normalize_captures};
use super::verify::marker_matches;

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
    /// The guard is the app's claim (`scoped_guarded`), not a macro proof. A
    /// proven ceiling does not admit it.
    declared: bool,
}

/// CSRF state of a route. `Absent`: no row, as for a safe method.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Csrf {
    On,
    Off,
    Absent,
}

impl RoutePosture {
    /// Whether `self` admits no caller that `ceiling` refuses.
    ///
    /// Uses the differ's widening rules. Roles are OR-ed. Scopes are AND-ed. An
    /// open classification is widest.
    #[must_use]
    pub fn no_wider_than(&self, ceiling: &Self) -> bool {
        let (open, ceiling_open) = (
            is_open(&self.classification),
            is_open(&ceiling.classification),
        );
        if open && !ceiling_open {
            return false;
        }
        // A route that stopped being open is narrower, whatever its roles. The
        // differ stops comparing roles, scopes and policy there too.
        let local = (ceiling_open && !open)
            || (self.roles_within(ceiling)
                && ceiling.scopes.is_subset(&self.scopes)
                && implies(ceiling.policy, self.policy));
        local
            && implies(ceiling.csrf == Csrf::On, self.csrf == Csrf::On)
            && ceiling.checks.is_subset(&self.checks)
            && implies(ceiling.mtls, self.mtls)
            // Guard strength only matters against a ceiling that has a guard.
            // A route that stopped being open is narrower, declared or not.
            && implies(!ceiling_open && !ceiling.declared, !self.declared)
    }

    /// An empty role list admits all signed-in callers. It is the widest.
    fn roles_within(&self, ceiling: &Self) -> bool {
        ceiling.roles.is_empty() || (!self.roles.is_empty() && self.roles.is_subset(&ceiling.roles))
    }

    fn encode(&self, out: &mut String) {
        let list = |items: &BTreeSet<String>| {
            let items: Vec<String> = items.iter().map(|i| enc(i)).collect();
            format!("{}:{}", items.len(), items.join(","))
        };
        let csrf = match self.csrf {
            Csrf::On => "on",
            Csrf::Off => "off",
            Csrf::Absent => "absent",
        };
        let fields = [
            enc(&self.classification),
            list(&self.roles),
            list(&self.scopes),
            u8::from(self.policy).to_string(),
            csrf.to_owned(),
            list(&self.checks),
            u8::from(self.mtls).to_string(),
        ];
        out.push_str(&fields.join("|"));
        // Written only when set, so a marker for a proven route keeps the
        // text and the seal it always had.
        if self.declared {
            out.push_str("|declared");
        }
    }

    fn decode(fields: &[&str]) -> Option<Self> {
        let (fields, declared) = match fields {
            [rest @ .., "declared"] => (rest, true),
            _ => (fields, false),
        };
        let [class, roles, scopes, policy, csrf, checks, mtls] = fields else {
            return None;
        };
        Some(Self {
            classification: dec(class)?,
            roles: dec_list(roles)?,
            scopes: dec_list(scopes)?,
            policy: dec_flag(policy)?,
            csrf: match *csrf {
                "on" => Csrf::On,
                "off" => Csrf::Off,
                "absent" => Csrf::Absent,
                _ => return None,
            },
            checks: dec_list(checks)?,
            mtls: dec_flag(mtls)?,
            declared,
        })
    }
}

/// `a` holds only if `b` does: a guard the ceiling has must still be there.
const fn implies(a: bool, b: bool) -> bool {
    !a || b
}

/// Everything outside this set is percent-encoded, so the separators used
/// below (`| , ; :`) and whitespace can never appear inside a value.
const UNRESERVED: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~')
    .remove(b'/')
    .remove(b'{')
    .remove(b'}');

/// Whether `payload` uses only the characters [`Ceiling::encode`] writes.
#[must_use]
pub fn is_payload(payload: &str) -> bool {
    !payload.is_empty()
        && payload
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-._~/{}%|,;:".contains(c))
}

fn enc(value: &str) -> String {
    utf8_percent_encode(value, UNRESERVED).to_string()
}

/// Strict: a `%` must start a two-digit escape, and nothing outside the
/// encoder's alphabet may appear.
fn dec(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                if !bytes.get(i + 1..i + 3)?.iter().all(u8::is_ascii_hexdigit) {
                    return None;
                }
                i += 3;
            }
            b if b.is_ascii_alphanumeric() || b"-._~/{}".contains(&b) => i += 1,
            _ => return None,
        }
    }
    percent_decode_str(value)
        .decode_utf8()
        .ok()
        .map(std::borrow::Cow::into_owned)
}

fn dec_flag(value: &str) -> Option<bool> {
    match value {
        "0" => Some(false),
        "1" => Some(true),
        _ => None,
    }
}

/// `n:a,b,c` — the count tells `[]` from `[""]`.
fn dec_list(value: &str) -> Option<BTreeSet<String>> {
    let (count, items) = value.split_once(':')?;
    let count: usize = count.parse().ok()?;
    if count == 0 {
        return items.is_empty().then(BTreeSet::new);
    }
    let parsed: Vec<String> = items.split(',').map(dec).collect::<Option<_>>()?;
    let set: BTreeSet<String> = parsed.iter().cloned().collect();
    (parsed.len() == count && set.len() == count).then_some(set)
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
        let entries: Vec<String> = self
            .routes
            .iter()
            .map(|((path, method), posture)| {
                let mut entry = format!("{}|{}|", enc(method), enc(path));
                posture.encode(&mut entry);
                entry
            })
            .collect();
        entries.join(";")
    }

    /// Parse [`Self::encode`] output. `None` for anything malformed.
    #[must_use]
    pub fn decode(text: &str) -> Option<Self> {
        let mut routes = BTreeMap::new();
        for entry in text.split(';') {
            let fields: Vec<&str> = entry.split('|').collect();
            let (method, path, rest) = match fields.as_slice() {
                [method, path, rest @ ..] => (dec(method)?, dec(path)?, rest),
                _ => return None,
            };
            let posture = RoutePosture::decode(rest)?;
            if routes.insert((path, method), posture).is_some() {
                return None;
            }
        }
        Some(Self { routes })
    }
}

/// What a widening set is bound to: identity plus the current ceiling.
#[derive(Debug, Clone)]
pub struct Cover {
    /// Postures at head of the routes that widened.
    pub ceiling: Ceiling,
    /// Digest of the findings the ceiling does not cover.
    identity: String,
}

/// Split `widening` into route findings (bound by ceiling) and the rest
/// (bound exactly), against the head manifest.
#[must_use]
pub fn cover(widening: &[&Finding], head: &PostureManifest) -> Cover {
    let routes = route_index(head);
    let csrf = csrf_index(head);
    let bindings = authz_index(head);
    let mtls = mtls_index(head);

    let mut ceiling = Ceiling::default();
    let mut lines = Vec::with_capacity(widening.len());
    for finding in widening {
        let key = (normalize_captures(&finding.path), finding.method.clone());
        let Some(entry) = routes.get(&key) else {
            lines.push(format!("exact\t{}", finding.canonical()));
            continue;
        };
        let posture =
            RoutePosture {
                classification: entry.classification.clone(),
                roles: entry.role_set(),
                scopes: entry.scope_set(),
                policy: entry.policy,
                csrf: csrf.get(&key).map_or(Csrf::Absent, |(enforced, ..)| {
                    if *enforced { Csrf::On } else { Csrf::Off }
                }),
                checks: checks_at(&key, &bindings).into_iter().collect(),
                mtls: mtls.contains(&key),
                // An open route has no guard to be weaker.
                declared: entry.is_declared() && !is_open(&entry.classification),
            };
        // The ceiling alone bounds a route finding. Its kind can change when
        // the route narrows, and one route can leave the set, so neither is
        // part of the identity.
        ceiling.routes.insert(key, posture);
    }
    lines.sort();
    lines.dedup();
    Cover {
        ceiling,
        identity: hex_digest(lines.join("\n").as_bytes()),
    }
}

impl Cover {
    /// The text after the phrase in the line a reviewer pastes: a ceiling
    /// marker when a route widened, else the short exact digest.
    #[must_use]
    pub fn marker(&self, exact: &str) -> String {
        if self.ceiling.is_empty() {
            return short(exact);
        }
        format!(
            "{} {CEILING_KEY}{}",
            short(&self.seal()),
            self.ceiling.encode()
        )
    }

    fn seal_of(&self, payload: &str) -> String {
        hex_digest(format!("ceiling-v1\n{}\n{payload}", self.identity).as_bytes())
    }

    /// The digest a ceiling marker carries for the current head.
    ///
    /// It also covers the payload. A cut or wrong paste fails. It is not read
    /// as a smaller ceiling.
    #[must_use]
    pub fn seal(&self) -> String {
        self.seal_of(&self.ceiling.encode())
    }

    /// Whether a marker (`digest` + `payload`) approves the current head.
    #[must_use]
    pub fn accepts(&self, payload: &str, digest: &str) -> bool {
        let Some(approved) = Ceiling::decode(payload) else {
            return false;
        };
        marker_matches(&self.seal_of(&approved.encode()), digest)
            && self.ceiling.routes.iter().all(|(key, now)| {
                approved
                    .routes
                    .get(key)
                    .is_some_and(|ceiling| now.no_wider_than(ceiling))
            })
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
        declared: bool,
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
                declared: false,
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
        /// The guard is a layer the app declared, not a macro proof.
        fn declared(mut self) -> Self {
            self.declared = true;
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
                      "location":"x:1","provenance":"{}"}}]}},
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
                if self.declared {
                    "declared"
                } else {
                    "provable"
                },
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
            Fixture::new("gated")
                .roles(&["a"])
                .csrf(true)
                .checks(&["read"]),
            Fixture::new("gated")
                .roles(&["a"])
                .csrf(true)
                .checks(&["write"]),
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

    // ── guard provenance ────────────────────────────────────────────────────

    /// A proven ceiling does not admit a route whose guard is only declared.
    /// The reverse is a narrowing, and a declared ceiling admits both.
    #[test]
    fn a_declared_guard_is_wider_than_a_proven_one() {
        let proven = Fixture::new("gated").roles(&["admin"]).posture();
        let declared = Fixture::new("gated").roles(&["admin"]).declared().posture();
        assert!(!declared.no_wider_than(&proven));
        assert!(proven.no_wider_than(&declared));
        assert!(declared.no_wider_than(&declared));
    }

    /// The ordering follows the differ, which reports the same change as
    /// `guard_declared`.
    #[test]
    fn guard_provenance_follows_the_differ_too() {
        let proven = Fixture::new("gated").roles(&["admin"]);
        let declared = Fixture::new("gated").roles(&["admin"]).declared();
        assert!(differ_widens(&proven, &declared));
        assert!(!differ_widens(&declared, &proven));
        assert!(!declared.posture().no_wider_than(&proven.posture()));
        assert!(proven.posture().no_wider_than(&declared.posture()));
    }

    /// Closing an open route is a narrowing even when a layer only declares the
    /// new guard. The differ agrees: it stops comparing at that point.
    #[test]
    fn a_declared_guard_is_within_an_open_ceiling() {
        let open = Fixture::new("public");
        let declared = Fixture::new("gated").declared();
        assert!(!differ_widens(&open, &declared));
        assert!(declared.posture().no_wider_than(&open.posture()));
        assert!(
            Fixture::new("gated")
                .posture()
                .no_wider_than(&open.posture())
        );
    }

    /// An open route has no guard, so its provenance changes nothing.
    #[test]
    fn provenance_on_an_open_route_is_ignored() {
        let plain = Fixture::new("public").posture();
        let flagged = Fixture::new("public").declared().posture();
        assert_eq!(plain, flagged);
    }

    /// A proven route keeps the text it always had, so a marker already in a
    /// PR keeps its seal. A declared one adds a trailing field and round-trips.
    #[test]
    fn declared_is_written_only_when_set() {
        let encode = |f: &Fixture| {
            let mut out = String::new();
            f.posture().encode(&mut out);
            out
        };
        let proven = encode(&Fixture::new("gated").roles(&["admin"]));
        let declared = encode(&Fixture::new("gated").roles(&["admin"]).declared());
        assert!(!proven.contains("declared"), "{proven}");
        assert_eq!(declared, format!("{proven}|declared"));

        let route = |posture: &str| format!("POST|%2Fa|{posture}");
        let old = Ceiling::decode(&route(&proven)).expect("a 7-field entry still parses");
        let new = Ceiling::decode(&route(&declared)).expect("a declared entry parses");
        assert!(!old.routes[&("/a".to_owned(), "POST".to_owned())].declared);
        assert!(new.routes[&("/a".to_owned(), "POST".to_owned())].declared);
        assert_eq!(Ceiling::decode(&new.encode()), Some(new));
    }

    /// The reported gap: a marker approved for a route whose guard was proven
    /// must not approve the same route once a layer only declares the guard.
    #[test]
    fn a_proven_marker_does_not_approve_a_declared_guard() {
        let widen = finding("/a", "x");
        let proven_head = Fixture::new("gated").manifest();
        let approved = cover(&[&widen], &proven_head);
        let (payload, digest) = (approved.ceiling.encode(), approved.seal());
        assert!(
            approved.accepts(&payload, &digest),
            "the marker approves its own head"
        );

        let declared_head = Fixture::new("gated").declared().manifest();
        let after = cover(&[&widen], &declared_head);
        assert!(
            !after.accepts(&payload, &digest),
            "a weaker guard needs its own acknowledgment"
        );
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
