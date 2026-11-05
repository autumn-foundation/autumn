//! Cross-release compatibility: can two releases run side by side?
//!
//! A rolling deploy has two windows. If the callees ship first, new callees
//! serve the deployed callers. If the callers ship first, new callers call the
//! deployed callees. One rule set, [`check_call`], runs in both windows: it
//! compares what a caller's own release expects with what the serving release
//! does. A window with no break is a safe deploy order.
//!
//! Fields match by wire key (`wire_name` and aliases), not by Rust name, so a
//! `#[serde(rename)]` that keeps the key is not a break. Path parameters match
//! by position, not by name.

use std::collections::BTreeSet;
use std::fmt::Write as _;

use autumn_macros_support::wire::{WireFieldDescriptor, WireTypeDescriptor};

use super::bundle::{Bundle, BundleCall, BundleEndpoint};

/// The two deploy windows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Window {
    /// Callees ship first: new callees serve deployed callers.
    CalleeFirst,
    /// Callers ship first: new callers call deployed callees.
    CallerFirst,
}

/// What breaks between one caller and the callee that serves it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum BreakKind {
    /// No endpoint serves a deployed route any more.
    RouteRemoved,
    /// The caller's route has no endpoint on the serving side.
    RouteMissing,
    /// The caller reads a response field the callee does not produce.
    ResponseFieldMissing(String),
    /// The caller's response type needs a field the callee may leave out.
    ResponseDecodeFails(String),
    /// The caller's response type denies unknown fields, and the callee
    /// produces one.
    ResponseFieldRejected(String),
    /// A response field changed its Rust type.
    ResponseTypeChanged {
        /// The field's wire key.
        field: String,
        /// The type the caller decodes.
        caller: String,
        /// The type the callee produces.
        callee: String,
    },
    /// The callee requires a request field the caller does not always send.
    RequestFieldNotSent(String),
    /// The caller sets a request field the callee ignores.
    RequestFieldDropped(String),
    /// The callee denies unknown fields, and the caller may send one.
    RequestFieldRejected(String),
    /// A request field changed its Rust type.
    RequestTypeChanged {
        /// The field's wire key.
        field: String,
        /// The type the caller sends.
        caller: String,
        /// The type the callee decodes.
        callee: String,
    },
}

/// One break, with what a report needs to name it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Break {
    /// The window the break happens in.
    pub window: Window,
    /// `service.name` of the endpoint, as the caller's release knows it.
    pub endpoint: String,
    /// `METHOD /path`, as the caller's release knows it.
    pub route: String,
    /// The service the endpoint belongs to.
    pub service: String,
    /// `krate::function`, or `None` for a route removed with no known caller.
    pub caller: Option<String>,
    /// The caller's crate, when known.
    pub caller_krate: Option<String>,
    /// What breaks.
    pub kind: BreakKind,
}

/// The result of one comparison.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Report {
    /// Breaks if the callees ship first.
    pub callee_first: Vec<Break>,
    /// Breaks if the callers ship first.
    pub caller_first: Vec<Break>,
}

/// Which deploy orders are safe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Safe in any order.
    Compatible,
    /// Safe only if the callees ship first.
    CalleeFirst,
    /// Safe only if the callers ship first.
    CallerFirst,
    /// Safe in no order.
    Incompatible,
}

/// The deploy order an operator declares with `--order`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum Order {
    /// No fixed order: both windows must be safe.
    #[default]
    Any,
    /// The callees ship before their callers.
    CalleeFirst,
    /// The callers ship before their callees.
    CallerFirst,
}

/// Compare the current release with the deployed one, in both windows.
#[must_use]
pub fn compare(deployed: &Bundle, current: &Bundle) -> Report {
    let mut callee_first = Vec::new();
    // A deployed route must still be served. Clients outside the bundle may
    // call it, so this holds with no known caller.
    for old in &deployed.endpoints {
        if serving(current, old).is_none() {
            callee_first.push(Break {
                window: Window::CalleeFirst,
                endpoint: old.id(),
                route: old.route(),
                service: old.service.clone(),
                caller: None,
                caller_krate: None,
                kind: BreakKind::RouteRemoved,
            });
        }
    }
    for call in &deployed.callers {
        callee_first.extend(
            check_call(Window::CalleeFirst, deployed, call, current)
                .into_iter()
                // The route break above already names this route.
                .filter(|b| b.kind != BreakKind::RouteMissing),
        );
    }
    let mut caller_first = Vec::new();
    for call in &current.callers {
        caller_first.extend(check_call(Window::CallerFirst, current, call, deployed));
    }
    callee_first.sort();
    callee_first.dedup();
    caller_first.sort();
    caller_first.dedup();
    Report {
        callee_first,
        caller_first,
    }
}

/// `(service, METHOD, path)` with path parameter names dropped.
fn route_key(endpoint: &BundleEndpoint) -> (String, String, String) {
    (
        endpoint.service.clone(),
        endpoint.method.to_ascii_uppercase(),
        normalize_path(&endpoint.path),
    )
}

/// A path with each `{name}` replaced by `{}`. The name is not on the wire.
fn normalize_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    let mut in_param = false;
    for ch in path.chars() {
        match ch {
            '{' => {
                in_param = true;
                out.push_str("{}");
            }
            '}' => in_param = false,
            _ if in_param => {}
            _ => out.push(ch),
        }
    }
    out.trim_end_matches('/').to_owned()
}

/// The endpoint in `side` that serves `wanted`'s route. A same-named endpoint
/// wins when two serve one route.
fn serving<'a>(side: &'a Bundle, wanted: &BundleEndpoint) -> Option<&'a BundleEndpoint> {
    let key = route_key(wanted);
    let mut found = side.endpoints.iter().filter(|e| route_key(e) == key);
    let first = found.next()?;
    if first.name == wanted.name {
        return Some(first);
    }
    Some(found.find(|e| e.name == wanted.name).unwrap_or(first))
}

/// Every wire key a field answers to: its name and, on decode, its aliases.
fn keys(field: &WireFieldDescriptor) -> impl Iterator<Item = &str> {
    std::iter::once(field.wire_name.as_str()).chain(field.aliases.iter().map(String::as_str))
}

/// The field in `fields` that a key matches.
fn by_key<'a>(fields: &'a [WireFieldDescriptor], key: &str) -> Option<&'a WireFieldDescriptor> {
    fields.iter().find(|f| f.wire_name == key)
}

/// The field in `fields` that accepts any key of `field`.
fn accepting<'a>(
    fields: &'a [WireFieldDescriptor],
    field: &WireFieldDescriptor,
) -> Option<&'a WireFieldDescriptor> {
    fields
        .iter()
        .find(|f| keys(f).any(|k| k == field.wire_name))
}

/// The produced field that one decoded field reads.
fn produced_for<'a>(
    produced: &'a [WireFieldDescriptor],
    decoded: &WireFieldDescriptor,
) -> Option<&'a WireFieldDescriptor> {
    keys(decoded).find_map(|k| by_key(produced, k))
}

/// A type with path qualifiers dropped: `std::string::String` is `String`.
fn normalize_type(ty: &str) -> String {
    let mut out = String::with_capacity(ty.len());
    let mut word = String::new();
    let mut chars = ty.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch.is_alphanumeric() || ch == '_' {
            word.push(ch);
            continue;
        }
        if ch == ':' && chars.peek() == Some(&':') {
            chars.next();
            word.clear();
            continue;
        }
        out.push_str(&word);
        word.clear();
        if !ch.is_whitespace() {
            out.push(ch);
        }
    }
    out.push_str(&word);
    out
}

/// Check one call against the endpoint that serves it.
///
/// `caller_side` is the caller's own release: it gives the shapes the caller
/// was built against. `callee_side` is the release that serves the call.
#[must_use]
pub fn check_call(
    window: Window,
    caller_side: &Bundle,
    call: &BundleCall,
    callee_side: &Bundle,
) -> Vec<Break> {
    let Some(view) = caller_side.endpoint(&call.service, &call.endpoint) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut report = |kind| {
        out.push(Break {
            window,
            endpoint: view.id(),
            route: view.route(),
            service: view.service.clone(),
            caller: Some(call.caller()),
            caller_krate: Some(call.krate.clone()),
            kind,
        });
    };
    let Some(serve) = serving(callee_side, view) else {
        report(BreakKind::RouteMissing);
        return out;
    };

    // Response: the caller decodes the callee's output with its own type.
    let decoded = &view.response.deserialized;
    let produced = &serve.response.serialized;
    let mut named = BTreeSet::new();
    for read in &call.reads {
        let Some(field) = decoded.iter().find(|f| &f.rust_name == read) else {
            continue;
        };
        if produced_for(produced, field).is_none() {
            named.insert(field.wire_name.clone());
            report(BreakKind::ResponseFieldMissing(field.wire_name.clone()));
        }
    }
    for field in decoded {
        match produced_for(produced, field) {
            Some(made) if normalize_type(&made.ty) != normalize_type(&field.ty) => {
                report(BreakKind::ResponseTypeChanged {
                    field: field.wire_name.clone(),
                    caller: field.ty.clone(),
                    callee: made.ty.clone(),
                });
            }
            Some(made) if field.required && !made.required => {
                report(BreakKind::ResponseDecodeFails(field.wire_name.clone()));
            }
            None if field.required && !named.contains(&field.wire_name) => {
                report(BreakKind::ResponseDecodeFails(field.wire_name.clone()));
            }
            _ => {}
        }
    }
    if view.response.closed {
        for made in produced {
            if accepting(decoded, made).is_none() {
                report(BreakKind::ResponseFieldRejected(made.wire_name.clone()));
            }
        }
    }

    // Request: the callee decodes what the caller's own type sends.
    let sent = &view.request.serialized;
    let accepted = &serve.request.deserialized;
    let writes = call.writes.as_deref().unwrap_or_default();
    let always_sent = |f: &WireFieldDescriptor| f.required || writes.contains(&f.rust_name);
    for need in accepted.iter().filter(|f| f.required) {
        let covered = keys(need).any(|k| by_key(sent, k).is_some_and(always_sent));
        if !covered {
            report(BreakKind::RequestFieldNotSent(need.wire_name.clone()));
        }
    }
    for write in writes {
        let Some(field) = sent.iter().find(|f| &f.rust_name == write) else {
            continue;
        };
        if accepting(accepted, field).is_none() && !serve.request.closed {
            report(BreakKind::RequestFieldDropped(field.wire_name.clone()));
        }
    }
    for field in sent {
        match accepting(accepted, field) {
            Some(into) if normalize_type(&into.ty) != normalize_type(&field.ty) => {
                report(BreakKind::RequestTypeChanged {
                    field: field.wire_name.clone(),
                    caller: field.ty.clone(),
                    callee: into.ty.clone(),
                });
            }
            None if serve.request.closed => {
                report(BreakKind::RequestFieldRejected(field.wire_name.clone()));
            }
            _ => {}
        }
    }
    out
}

impl Report {
    /// Which deploy orders are safe.
    #[must_use]
    pub fn verdict(&self) -> Verdict {
        match (self.callee_first.is_empty(), self.caller_first.is_empty()) {
            (true, true) => Verdict::Compatible,
            (true, false) => Verdict::CalleeFirst,
            (false, true) => Verdict::CallerFirst,
            (false, false) => Verdict::Incompatible,
        }
    }

    /// The process exit code for a declared order.
    ///
    /// `0` safe. `1` not safe. `2` safe only in an order that `--order` did
    /// not declare.
    #[must_use]
    pub fn exit_code(&self, order: Order) -> i32 {
        match (order, self.verdict()) {
            (Order::Any, Verdict::Compatible) => 0,
            (Order::Any, Verdict::CalleeFirst | Verdict::CallerFirst) => 2,
            (Order::Any, Verdict::Incompatible) => 1,
            (Order::CalleeFirst, _) => i32::from(!self.callee_first.is_empty()),
            (Order::CallerFirst, _) => i32::from(!self.caller_first.is_empty()),
        }
    }

    /// The report as text.
    #[must_use]
    pub fn render(&self, order: Order) -> String {
        let mut out =
            String::from("Wire contracts: the current build against the deployed release.\n\n");
        render_window(
            &mut out,
            "If the callees ship first (new callees, deployed callers)",
            &self.callee_first,
        );
        render_window(
            &mut out,
            "If the callers ship first (new callers, deployed callees)",
            &self.caller_first,
        );
        let callees = |breaks: &[Break]| join(breaks.iter().map(|b| b.service.clone()));
        let callers = |breaks: &[Break]| {
            let named = join(breaks.iter().filter_map(|b| b.caller_krate.clone()));
            if named.is_empty() {
                "every caller".to_owned()
            } else {
                named
            }
        };
        let verdict = match self.verdict() {
            Verdict::Compatible => "Verdict: safe in any deploy order.\n".to_owned(),
            Verdict::Incompatible => {
                "Verdict: not safe in any deploy order. Keep the old shape or route until no \
                 deployed release needs it.\n"
                    .to_owned()
            }
            Verdict::CalleeFirst => format!(
                "Verdict: safe only if the callees ship first.\n  Deploy {} before {}.\n",
                callees(&self.caller_first),
                callers(&self.caller_first),
            ),
            Verdict::CallerFirst => format!(
                "Verdict: safe only if the callers ship first.\n  Deploy {} before {}.\n",
                callers(&self.callee_first),
                callees(&self.callee_first),
            ),
        };
        out.push_str(&verdict);
        let hint = match (order, self.verdict()) {
            (Order::Any, Verdict::CalleeFirst) => {
                "  To declare this order, run with --order callee-first.\n"
            }
            (Order::Any, Verdict::CallerFirst) => {
                "  To declare this order, run with --order caller-first.\n"
            }
            _ => "",
        };
        out.push_str(hint);
        if order != Order::Any {
            let safe = self.exit_code(order) == 0;
            let name = if order == Order::CalleeFirst {
                "callee-first"
            } else {
                "caller-first"
            };
            let _ = writeln!(
                out,
                "Declared order: {name}. {}",
                if safe { "Safe." } else { "Not safe." }
            );
        }
        out
    }
}

/// A sorted, comma-separated list of distinct names.
fn join(names: impl Iterator<Item = String>) -> String {
    names
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>()
        .join(", ")
}

fn render_window(out: &mut String, title: &str, breaks: &[Break]) {
    if breaks.is_empty() {
        let _ = writeln!(out, "{title}: ok\n");
        return;
    }
    let _ = writeln!(out, "{title}: {} break(s)", breaks.len());
    for b in breaks {
        let who = b.caller.as_deref().map_or_else(
            || "any client".to_owned(),
            |caller| format!("caller `{caller}`"),
        );
        let (callee, caller_release) = match b.window {
            Window::CalleeFirst => ("the new callee", "its deployed type"),
            Window::CallerFirst => ("the deployed callee", "its new type"),
        };
        let detail = match &b.kind {
            BreakKind::RouteRemoved => {
                format!("{callee} no longer serves this route; {who} gets no endpoint")
            }
            BreakKind::RouteMissing => {
                format!("{who} calls this route, and {callee} does not serve it")
            }
            BreakKind::ResponseFieldMissing(f) => {
                format!("{who} reads response field `{f}`, which {callee} does not produce")
            }
            BreakKind::ResponseDecodeFails(f) => format!(
                "{who} decodes the response with {caller_release}, which needs field `{f}`, and \
                 {callee} does not always produce it"
            ),
            BreakKind::ResponseFieldRejected(f) => format!(
                "{who} decodes the response with {caller_release}, which denies unknown fields, \
                 and {callee} produces `{f}`"
            ),
            BreakKind::ResponseTypeChanged {
                field,
                caller,
                callee: theirs,
            } => format!(
                "response field `{field}` is `{theirs}` in {callee}, and {who} decodes it as \
                 `{caller}`"
            ),
            BreakKind::RequestFieldNotSent(f) => {
                format!("{callee} requires request field `{f}`, and {who} does not always send it")
            }
            BreakKind::RequestFieldDropped(f) => {
                format!("{who} sets request field `{f}`, and {callee} ignores it")
            }
            BreakKind::RequestFieldRejected(f) => {
                format!("{callee} denies unknown request fields, and {who} sends `{f}`")
            }
            BreakKind::RequestTypeChanged {
                field,
                caller,
                callee: theirs,
            } => format!(
                "{who} sends request field `{field}` as `{caller}`, and {callee} decodes it as \
                 `{theirs}`"
            ),
        };
        let _ = writeln!(out, "  - {} ({}): {detail}", b.endpoint, b.route);
    }
    out.push('\n');
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::bundle::BUNDLE_FORMAT;

    fn field(rust: &str, wire: &str, ty: &str, required: bool) -> WireFieldDescriptor {
        WireFieldDescriptor {
            rust_name: rust.to_owned(),
            wire_name: wire.to_owned(),
            ty: ty.to_owned(),
            required,
            aliases: Vec::new(),
        }
    }

    fn plain(name: &str, required: bool) -> WireFieldDescriptor {
        field(name, name, "String", required)
    }

    fn shape(
        name: &str,
        out: Vec<WireFieldDescriptor>,
        inn: Vec<WireFieldDescriptor>,
    ) -> WireTypeDescriptor {
        WireTypeDescriptor {
            name: name.to_owned(),
            serialized: out,
            deserialized: inn,
            closed: false,
        }
    }

    /// Release 1 of a catalog and its storefront.
    fn release_1() -> Bundle {
        Bundle {
            format: BUNDLE_FORMAT,
            endpoints: vec![
                BundleEndpoint {
                    service: "catalog".to_owned(),
                    name: "create_item".to_owned(),
                    krate: "mesh-catalog".to_owned(),
                    method: "POST".to_owned(),
                    path: "/items".to_owned(),
                    request: shape(
                        "NewItem",
                        vec![
                            plain("name", true),
                            field("note", "note", "Option<String>", true),
                        ],
                        vec![
                            plain("name", true),
                            field("note", "note", "Option<String>", false),
                        ],
                    ),
                    response: item(),
                },
                BundleEndpoint {
                    service: "catalog".to_owned(),
                    name: "get_item".to_owned(),
                    krate: "mesh-catalog".to_owned(),
                    method: "GET".to_owned(),
                    path: "/items/{id}".to_owned(),
                    request: shape("NoBody", Vec::new(), Vec::new()),
                    response: item(),
                },
            ],
            callers: vec![
                call("add_item", "create_item", &["id"], Some(&["name"])),
                call("show_item", "get_item", &["name"], None),
            ],
        }
    }

    fn item() -> WireTypeDescriptor {
        shape(
            "Item",
            vec![
                plain("id", true),
                plain("name", true),
                field("subtitle", "subtitle", "Option<String>", true),
            ],
            vec![
                plain("id", true),
                plain("name", true),
                field("subtitle", "subtitle", "Option<String>", false),
            ],
        )
    }

    fn call(function: &str, endpoint: &str, reads: &[&str], writes: Option<&[&str]>) -> BundleCall {
        BundleCall {
            krate: "mesh-storefront".to_owned(),
            function: function.to_owned(),
            service: "catalog".to_owned(),
            endpoint: endpoint.to_owned(),
            reads: reads.iter().map(|r| (*r).to_owned()).collect(),
            writes: writes.map(|w| w.iter().map(|x| (*x).to_owned()).collect()),
        }
    }

    fn ep<'a>(bundle: &'a mut Bundle, name: &str) -> &'a mut BundleEndpoint {
        bundle
            .endpoints
            .iter_mut()
            .find(|e| e.name == name)
            .expect("endpoint")
    }

    fn kinds(breaks: &[Break]) -> Vec<BreakKind> {
        breaks.iter().map(|b| b.kind.clone()).collect()
    }

    // --- the rules the issue names ---

    #[test]
    fn a_removed_response_field_that_a_caller_reads_fails() {
        let deployed = release_1();
        let mut current = release_1();
        let item = &mut ep(&mut current, "get_item").response;
        item.serialized.retain(|f| f.rust_name != "name");
        item.deserialized.retain(|f| f.rust_name != "name");
        // The new caller cannot read it any more: it would not compile.
        current.callers.retain(|c| c.function != "show_item");
        current
            .callers
            .push(call("show_item", "get_item", &["id"], None));
        let report = compare(&deployed, &current);
        assert!(
            kinds(&report.callee_first)
                .contains(&BreakKind::ResponseFieldMissing("name".to_owned())),
            "{report:?}"
        );
        assert_ne!(report.verdict(), Verdict::Compatible);
        assert_ne!(report.exit_code(Order::Any), 0);
        assert_ne!(report.exit_code(Order::CalleeFirst), 0);
    }

    #[test]
    fn a_removed_response_field_that_no_caller_reads_passes() {
        let deployed = release_1();
        let mut current = release_1();
        for name in ["get_item", "create_item"] {
            let item = &mut ep(&mut current, name).response;
            item.serialized.retain(|f| f.rust_name != "subtitle");
            item.deserialized.retain(|f| f.rust_name != "subtitle");
        }
        let report = compare(&deployed, &current);
        assert_eq!(report, Report::default(), "{report:?}");
        assert_eq!(report.verdict(), Verdict::Compatible);
        assert_eq!(report.exit_code(Order::Any), 0);
    }

    #[test]
    fn a_new_required_request_field_fails() {
        let deployed = release_1();
        let mut current = release_1();
        let req = &mut ep(&mut current, "create_item").request;
        req.serialized.push(plain("sku", true));
        req.deserialized.push(plain("sku", true));
        let report = compare(&deployed, &current);
        assert_eq!(
            kinds(&report.callee_first),
            [BreakKind::RequestFieldNotSent("sku".to_owned())]
        );
        assert_ne!(report.exit_code(Order::Any), 0);
    }

    #[test]
    fn a_new_optional_request_field_passes() {
        let deployed = release_1();
        let mut current = release_1();
        let req = &mut ep(&mut current, "create_item").request;
        req.serialized
            .push(field("coupon", "coupon", "Option<String>", true));
        req.deserialized
            .push(field("coupon", "coupon", "Option<String>", false));
        let report = compare(&deployed, &current);
        assert_eq!(report, Report::default(), "{report:?}");
        assert_eq!(report.exit_code(Order::Any), 0);
    }

    #[test]
    fn a_path_change_fails() {
        let deployed = release_1();
        let mut current = release_1();
        ep(&mut current, "get_item").path = "/catalog/items/{id}".to_owned();
        let report = compare(&deployed, &current);
        assert!(kinds(&report.callee_first).contains(&BreakKind::RouteRemoved));
        assert!(kinds(&report.caller_first).contains(&BreakKind::RouteMissing));
        assert_eq!(report.verdict(), Verdict::Incompatible);
        assert_eq!(report.exit_code(Order::Any), 1);
    }

    #[test]
    fn a_method_change_fails() {
        let deployed = release_1();
        let mut current = release_1();
        ep(&mut current, "get_item").method = "POST".to_owned();
        let report = compare(&deployed, &current);
        assert_eq!(report.verdict(), Verdict::Incompatible);
    }

    // --- deploy order ---

    #[test]
    fn a_path_change_that_keeps_the_old_route_is_safe_if_the_callee_ships_first() {
        let deployed = release_1();
        let mut current = release_1();
        let mut legacy = ep(&mut current, "get_item").clone();
        legacy.name = "get_item_legacy".to_owned();
        current.endpoints.push(legacy);
        ep(&mut current, "get_item").path = "/catalog/items/{id}".to_owned();
        let report = compare(&deployed, &current);
        assert_eq!(report.callee_first, Vec::new());
        assert_eq!(kinds(&report.caller_first), [BreakKind::RouteMissing]);
        assert_eq!(report.verdict(), Verdict::CalleeFirst);
        assert_eq!(report.exit_code(Order::Any), 2);
        assert_eq!(report.exit_code(Order::CalleeFirst), 0);
        assert_eq!(report.exit_code(Order::CallerFirst), 1);
        let text = report.render(Order::Any);
        assert!(
            text.contains("Deploy catalog before mesh-storefront"),
            "{text}"
        );
        assert!(text.contains("--order callee-first"), "{text}");
    }

    #[test]
    fn a_new_response_field_a_new_caller_reads_needs_the_callee_first() {
        let deployed = release_1();
        let mut current = release_1();
        let item = &mut ep(&mut current, "get_item").response;
        item.serialized
            .push(field("badge", "badge", "Option<String>", true));
        item.deserialized
            .push(field("badge", "badge", "Option<String>", false));
        current.callers.retain(|c| c.function != "show_item");
        current
            .callers
            .push(call("show_item", "get_item", &["badge", "name"], None));
        let report = compare(&deployed, &current);
        assert_eq!(report.callee_first, Vec::new());
        assert_eq!(
            kinds(&report.caller_first),
            [BreakKind::ResponseFieldMissing("badge".to_owned())]
        );
        assert_eq!(report.verdict(), Verdict::CalleeFirst);
    }

    #[test]
    fn a_removed_read_field_is_safe_if_the_callers_ship_first() {
        let deployed = release_1();
        let mut current = release_1();
        let item = &mut ep(&mut current, "get_item").response;
        item.serialized.retain(|f| f.rust_name != "name");
        item.deserialized.retain(|f| f.rust_name != "name");
        current.callers.retain(|c| c.function != "show_item");
        current
            .callers
            .push(call("show_item", "get_item", &["id"], None));
        let report = compare(&deployed, &current);
        assert_eq!(report.caller_first, Vec::new());
        assert_eq!(report.verdict(), Verdict::CallerFirst);
        assert_eq!(report.exit_code(Order::CallerFirst), 0);
        let text = report.render(Order::Any);
        assert!(
            text.contains("Deploy mesh-storefront before catalog"),
            "{text}"
        );
    }

    // --- serde-aware matching ---

    #[test]
    fn a_rust_rename_that_keeps_the_wire_key_passes() {
        let deployed = release_1();
        let mut current = release_1();
        for f in &mut ep(&mut current, "get_item").response.serialized {
            if f.rust_name == "name" {
                f.rust_name = "title".to_owned();
            }
        }
        for f in &mut ep(&mut current, "get_item").response.deserialized {
            if f.rust_name == "name" {
                f.rust_name = "title".to_owned();
            }
        }
        current.callers.retain(|c| c.function != "show_item");
        current
            .callers
            .push(call("show_item", "get_item", &["title"], None));
        let report = compare(&deployed, &current);
        assert_eq!(report, Report::default(), "{report:?}");
    }

    #[test]
    fn a_wire_rename_with_an_alias_is_safe_for_requests_old_callers_send() {
        let deployed = release_1();
        let mut current = release_1();
        let req = &mut ep(&mut current, "create_item").request;
        for f in req.serialized.iter_mut().chain(req.deserialized.iter_mut()) {
            if f.rust_name == "name" {
                f.wire_name = "title".to_owned();
            }
        }
        for f in &mut req.deserialized {
            if f.rust_name == "name" {
                f.aliases = vec!["name".to_owned()];
            }
        }
        let report = compare(&deployed, &current);
        assert_eq!(report.callee_first, Vec::new(), "{report:?}");
        // A new caller sends `title`; the old callee requires `name`.
        assert!(
            kinds(&report.caller_first)
                .contains(&BreakKind::RequestFieldNotSent("name".to_owned()))
        );
    }

    #[test]
    fn a_path_parameter_rename_is_not_a_route_change() {
        let deployed = release_1();
        let mut current = release_1();
        ep(&mut current, "get_item").path = "/items/{item_id}".to_owned();
        let report = compare(&deployed, &current);
        assert_eq!(report, Report::default(), "{report:?}");
    }

    #[test]
    fn an_unread_required_response_field_removed_breaks_the_old_decode() {
        let deployed = release_1();
        let mut current = release_1();
        // `id` is required on decode. `show_item` does not read it, but its
        // response type still needs it to decode.
        let item = &mut ep(&mut current, "get_item").response;
        item.serialized.retain(|f| f.rust_name != "id");
        item.deserialized.retain(|f| f.rust_name != "id");
        let report = compare(&deployed, &current);
        assert_eq!(
            kinds(&report.callee_first),
            [BreakKind::ResponseDecodeFails("id".to_owned())]
        );
    }

    #[test]
    fn a_response_field_that_becomes_sometimes_absent_breaks_a_required_decode() {
        let deployed = release_1();
        let mut current = release_1();
        for f in &mut ep(&mut current, "get_item").response.serialized {
            if f.rust_name == "id" {
                f.required = false;
            }
        }
        let report = compare(&deployed, &current);
        assert_eq!(
            kinds(&report.callee_first),
            [BreakKind::ResponseDecodeFails("id".to_owned())]
        );
    }

    #[test]
    fn a_closed_response_type_rejects_a_new_response_field() {
        let mut deployed = release_1();
        ep(&mut deployed, "get_item").response.closed = true;
        let mut current = release_1();
        ep(&mut current, "get_item").response.closed = true;
        let item = &mut ep(&mut current, "get_item").response;
        item.serialized
            .push(field("badge", "badge", "Option<String>", true));
        item.deserialized
            .push(field("badge", "badge", "Option<String>", false));
        let report = compare(&deployed, &current);
        assert_eq!(
            kinds(&report.callee_first),
            [BreakKind::ResponseFieldRejected("badge".to_owned())]
        );
    }

    #[test]
    fn a_closed_request_type_rejects_a_field_an_old_caller_sends() {
        let deployed = release_1();
        let mut current = release_1();
        let req = &mut ep(&mut current, "create_item").request;
        req.closed = true;
        req.serialized.retain(|f| f.rust_name != "note");
        req.deserialized.retain(|f| f.rust_name != "note");
        let report = compare(&deployed, &current);
        assert_eq!(
            kinds(&report.callee_first),
            [BreakKind::RequestFieldRejected("note".to_owned())]
        );
    }

    #[test]
    fn a_request_field_a_caller_sets_and_the_callee_drops_is_a_break() {
        let deployed = release_1();
        let mut current = release_1();
        let req = &mut ep(&mut current, "create_item").request;
        req.serialized
            .push(field("coupon", "coupon", "Option<String>", true));
        req.deserialized
            .push(field("coupon", "coupon", "Option<String>", false));
        current.callers.retain(|c| c.function != "add_item");
        current.callers.push(call(
            "add_item",
            "create_item",
            &["id"],
            Some(&["coupon", "name"]),
        ));
        let report = compare(&deployed, &current);
        assert_eq!(
            kinds(&report.caller_first),
            [BreakKind::RequestFieldDropped("coupon".to_owned())]
        );
        assert_eq!(report.verdict(), Verdict::CalleeFirst);
    }

    #[test]
    fn a_required_field_the_type_may_omit_counts_only_when_the_call_sets_it() {
        let mut deployed = release_1();
        let mut current = release_1();
        for bundle in [&mut deployed, &mut current] {
            let req = &mut ep(bundle, "create_item").request;
            req.serialized.push(plain("request_id", false));
            req.deserialized.push(plain("request_id", true));
        }
        assert_eq!(compare(&deployed, &current).callee_first.len(), 1);
        for bundle in [&mut deployed, &mut current] {
            bundle.callers.retain(|c| c.function != "add_item");
            bundle.callers.push(call(
                "add_item",
                "create_item",
                &["id"],
                Some(&["name", "request_id"]),
            ));
        }
        assert_eq!(compare(&deployed, &current), Report::default());
    }

    #[test]
    fn a_response_type_change_is_a_break_but_a_path_qualifier_is_not() {
        let deployed = release_1();
        let mut current = release_1();
        let item = &mut ep(&mut current, "get_item").response;
        for f in item
            .serialized
            .iter_mut()
            .chain(item.deserialized.iter_mut())
        {
            if f.rust_name == "name" {
                f.ty = "std::string::String".to_owned();
            }
        }
        assert_eq!(compare(&deployed, &current), Report::default());
        let item = &mut ep(&mut current, "get_item").response;
        for f in item
            .serialized
            .iter_mut()
            .chain(item.deserialized.iter_mut())
        {
            if f.rust_name == "name" {
                f.ty = "u32".to_owned();
            }
        }
        let report = compare(&deployed, &current);
        assert!(
            kinds(&report.callee_first).contains(&BreakKind::ResponseTypeChanged {
                field: "name".to_owned(),
                caller: "String".to_owned(),
                callee: "u32".to_owned(),
            })
        );
    }

    #[test]
    fn an_identical_release_is_compatible() {
        let report = compare(&release_1(), &release_1());
        assert_eq!(report, Report::default());
        assert_eq!(report.verdict(), Verdict::Compatible);
        assert!(report.render(Order::Any).contains("any deploy order"));
    }

    #[test]
    fn the_report_names_the_endpoint_the_caller_and_the_field() {
        let deployed = release_1();
        let mut current = release_1();
        let req = &mut ep(&mut current, "create_item").request;
        req.serialized.push(plain("sku", true));
        req.deserialized.push(plain("sku", true));
        let text = compare(&deployed, &current).render(Order::Any);
        assert!(text.contains("catalog.create_item (POST /items)"), "{text}");
        assert!(text.contains("mesh-storefront::add_item"), "{text}");
        assert!(text.contains("`sku`"), "{text}");
    }
}
