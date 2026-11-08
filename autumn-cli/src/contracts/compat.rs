//! Cross-release compatibility: can two releases run side by side?
//!
//! A rolling deploy has two windows. If the callees ship first, new callees
//! serve the deployed callers. If the callers ship first, new callers call the
//! deployed callees. One rule set, [`check_call`], runs in both windows: it
//! compares what a caller's own release expects with what the serving release
//! does. A window with no break is a safe deploy order.
//!
//! Events use the same two windows. The producer side counts as the callee,
//! and the consumer side as the caller: [`check_listener`] compares what a
//! consumer's own release decodes with what the other release sends.
//!
//! Fields match by wire key (`wire_name` and aliases), not by Rust name, so a
//! `#[serde(rename)]` that keeps the key is not a break. Path parameters match
//! by position, not by name.

use std::collections::BTreeSet;
use std::fmt::Write as _;

use autumn_macros_support::wire::{WireFieldDescriptor, WireTypeDescriptor};

use super::bundle::{Bundle, BundleCall, BundleEndpoint, BundleEvent, BundleListener};

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
    /// The response body changed between a JSON object and a JSON array.
    ResponseListChanged,
    /// The caller reads a response field the callee does not produce.
    ResponseFieldMissing(String),
    /// The caller's response type needs a field the callee may leave out.
    ResponseDecodeFails(String),
    /// The caller's response type denies unknown fields, and the callee
    /// produces one.
    ResponseFieldRejected(String),
    /// The caller's response type gets one field under two keys: its name
    /// and an alias. serde refuses that object.
    ResponseDuplicateKey(String),
    /// A response field changed its Rust type.
    ResponseTypeChanged {
        /// The field's wire key.
        field: String,
        /// The type the caller decodes.
        caller: String,
        /// The type the callee produces.
        callee: String,
    },
    /// The caller sends no body, and the callee now reads one.
    RequestBodyMissing,
    /// The request body changed between a JSON object and a JSON array.
    RequestListChanged,
    /// The callee requires a request field the caller does not always send.
    RequestFieldNotSent(String),
    /// The caller sets a request field the callee ignores.
    RequestFieldDropped(String),
    /// The callee denies unknown fields, and the caller may send one.
    RequestFieldRejected(String),
    /// The callee gets one field under two keys: its name and an alias.
    /// serde refuses that object.
    RequestDuplicateKey(String),
    /// A request field changed its Rust type.
    RequestTypeChanged {
        /// The field's wire key.
        field: String,
        /// The type the caller sends.
        caller: String,
        /// The type the callee decodes.
        callee: String,
    },
    /// No producer in the serving release sends the consumer's topic.
    EventTopicMissing,
    /// The consumer reads an event field the producer does not send.
    EventFieldMissing(String),
    /// The consumer's event type needs a field the producer may leave out.
    EventDecodeFails(String),
    /// The consumer's event type denies unknown fields, and the producer
    /// sends one.
    EventFieldRejected(String),
    /// The consumer's event type gets one field under two keys.
    EventDuplicateKey(String),
    /// An event field changed its Rust type.
    EventTypeChanged {
        /// The field's wire key.
        field: String,
        /// The type the consumer decodes.
        consumer: String,
        /// The type the producer sends.
        producer: String,
    },
}

impl BreakKind {
    /// Whether this break is about an event, not an endpoint.
    #[must_use]
    pub const fn is_event(&self) -> bool {
        matches!(
            self,
            Self::EventTopicMissing
                | Self::EventFieldMissing(_)
                | Self::EventDecodeFails(_)
                | Self::EventFieldRejected(_)
                | Self::EventDuplicateKey(_)
                | Self::EventTypeChanged { .. }
        )
    }
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
    /// Facts that are not breaks, such as an event version that no consumer
    /// reads any more.
    pub notes: Vec<String>,
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

/// Exit code: safe in the declared order.
pub const EXIT_SAFE: i32 = 0;
/// Exit code: not safe in the declared order.
pub const EXIT_UNSAFE: i32 = 1;
/// Exit code: safe only in an order that `--order` did not declare. Clap uses
/// `2` for a usage error, so this is `3`.
pub const EXIT_ORDER: i32 = 3;

/// Compare the current release with the deployed one, in both windows.
#[must_use]
pub fn compare(deployed: &Bundle, current: &Bundle) -> Report {
    let mut old_callers = Vec::new();
    for call in &deployed.callers {
        old_callers.extend(check_call(Window::CalleeFirst, deployed, call, current));
    }
    // A deployed route must still be served. Clients outside the bundle may
    // call it, so this holds with no known caller. A caller break already
    // names the route when a deployed caller calls it.
    for old in &deployed.endpoints {
        let named = old_callers
            .iter()
            .any(|b| b.kind == BreakKind::RouteMissing && b.endpoint == old.id());
        if !named && serving(current, old).is_none() {
            old_callers.push(Break {
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
    let mut new_callers = Vec::new();
    for call in &current.callers {
        new_callers.extend(check_call(Window::CallerFirst, current, call, deployed));
    }
    for listener in &deployed.listeners {
        old_callers.extend(check_listener(
            Window::CalleeFirst,
            deployed,
            listener,
            current,
        ));
    }
    for listener in &current.listeners {
        new_callers.extend(check_listener(
            Window::CallerFirst,
            current,
            listener,
            deployed,
        ));
    }
    old_callers.sort();
    old_callers.dedup();
    new_callers.sort();
    new_callers.dedup();
    Report {
        callee_first: old_callers,
        caller_first: new_callers,
        notes: event_notes(deployed, current),
    }
}

/// The notes about events: the consumers the check cannot see, and the old
/// versions that can be dropped. No version is called droppable while an
/// unseen consumer may read it.
fn event_notes(deployed: &Bundle, current: &Bundle) -> Vec<String> {
    let unseen: BTreeSet<&String> = deployed
        .unresolved
        .iter()
        .chain(&current.unresolved)
        .collect();
    if unseen.is_empty() {
        return droppable_versions(deployed, current);
    }
    let mut notes = vec![format!(
        "{} event consumer(s) are not in the bundles, so the check does not protect them. \
         Use the event's own type name, and give the event a `name`.",
        unseen.len()
    )];
    notes.extend(unseen.into_iter().map(|u| format!("  {u}")));
    notes
}

/// Check one event consumer against the event the other release sends.
///
/// `built_against` is the consumer's own release: it gives the type the
/// consumer decodes with. `sending_release` produces the event.
///
/// When `sending_release` does not send the topic, a deployed consumer loses
/// its events. A new consumer of a topic that is not sent yet gets no events
/// until the producer ships. That is not a break for a new event. It is a
/// break when the sending release sends a version of the same event that no
/// consumer in the consumer's release reads: those events are lost.
#[must_use]
pub fn check_listener(
    window: Window,
    built_against: &Bundle,
    listener: &BundleListener,
    sending_release: &Bundle,
) -> Vec<Break> {
    let Some(view) = built_against.event(&listener.topic) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut report = |kind| {
        out.push(Break {
            window,
            endpoint: view.topic.clone(),
            route: "event".to_owned(),
            service: service_of(built_against, &view.krate),
            caller: Some(listener.consumer()),
            caller_krate: Some(listener.krate.clone()),
            kind,
        });
    };
    let Some(sent) = sending_release.event(&listener.topic) else {
        // Events of a version that the consumer's release does not read are
        // lost while this window lasts.
        let lost = sending_release.events.iter().any(|e| {
            e.name == view.name && !built_against.listeners.iter().any(|l| l.topic == e.topic)
        });
        if window == Window::CalleeFirst || lost {
            report(BreakKind::EventTopicMissing);
        }
        return out;
    };
    if view.opaque || sent.opaque {
        return out;
    }
    for issue in decode_issues(&view.shape, &sent.shape.serialized, &listener.reads) {
        report(match issue {
            Decode::Missing(f) => BreakKind::EventFieldMissing(f),
            Decode::Fails(f) => BreakKind::EventDecodeFails(f),
            Decode::Rejected(f) => BreakKind::EventFieldRejected(f),
            Decode::DuplicateKey(f) => BreakKind::EventDuplicateKey(f),
            Decode::TypeChanged {
                field,
                reader,
                writer,
            } => BreakKind::EventTypeChanged {
                field,
                consumer: reader,
                producer: writer,
            },
        });
    }
    out
}

/// How a deploy order names the crate that defines an event: the service of
/// its `#[endpoint]`s when it has exactly one, else the crate.
fn service_of(bundle: &Bundle, krate: &str) -> String {
    let services: BTreeSet<&str> = bundle
        .endpoints
        .iter()
        .filter(|e| e.krate == krate)
        .map(|e| e.service.as_str())
        .collect();
    match services.into_iter().collect::<Vec<_>>().as_slice() {
        [one] => (*one).to_owned(),
        _ => krate.to_owned(),
    }
}

/// A note for each event version below the newest that no consumer reads,
/// in the deployed release or in this one. The producer can stop sending it.
fn droppable_versions(deployed: &Bundle, current: &Bundle) -> Vec<String> {
    let read = |topic: &str| {
        deployed
            .listeners
            .iter()
            .chain(&current.listeners)
            .any(|l| l.topic == topic)
    };
    let newest = |event: &BundleEvent| {
        current
            .events
            .iter()
            .filter(|e| e.name == event.name)
            .map(|e| e.version)
            .max()
            .unwrap_or(event.version)
    };
    current
        .events
        .iter()
        .filter(|e| e.version < newest(e) && !read(&e.topic))
        .map(|e| {
            format!(
                "Event `{}` v{}: no consumer reads it in the deployed release or in this one. \
                 It can be dropped.",
                e.name, e.version
            )
        })
        .collect()
}

/// `(service, METHOD, path)` with path parameter names dropped.
fn route_key(endpoint: &BundleEndpoint) -> (String, String, String) {
    (
        endpoint.service.clone(),
        endpoint.method.to_ascii_uppercase(),
        normalize_path(&endpoint.path),
    )
}

/// A path with each `{name}` replaced by `{}`, and each `{*name}` by `{*}`.
/// The name is not on the wire. A trailing slash stays: it is another route.
fn normalize_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    let mut param: Option<String> = None;
    for ch in path.chars() {
        match (ch, param.as_mut()) {
            ('{', None) => param = Some(String::new()),
            ('}', Some(name)) => {
                out.push_str(if name.starts_with('*') { "{*}" } else { "{}" });
                param = None;
            }
            (_, Some(name)) => name.push(ch),
            (_, None) => out.push(ch),
        }
    }
    out
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

/// Whether a decoder of type `decoder` reads what a producer of type
/// `producer` writes. The same type reads, and `Option<T>` reads a `T`.
fn type_reads(decoder: &str, producer: &str) -> bool {
    let (decoder, producer) = (normalize_type(decoder), normalize_type(producer));
    decoder == producer || decoder == format!("Option<{producer}>")
}

/// How many fields in `fields` carry a key of `field`.
fn key_hits(fields: &[WireFieldDescriptor], field: &WireFieldDescriptor) -> usize {
    keys(field).filter(|k| by_key(fields, k).is_some()).count()
}

/// Check one call against the endpoint that serves it.
///
/// `built_against` is the caller's own release: it gives the shapes the
/// caller was built against. `serving_release` serves the call.
#[must_use]
pub fn check_call(
    window: Window,
    built_against: &Bundle,
    call: &BundleCall,
    serving_release: &Bundle,
) -> Vec<Break> {
    let Some(view) = built_against.endpoint(&call.service, &call.endpoint) else {
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
    let Some(serve) = serving(serving_release, view) else {
        report(BreakKind::RouteMissing);
        return out;
    };
    response_breaks(view, serve, call, &mut report);
    request_breaks(view, serve, call, &mut report);
    out
}

/// Response rules: the caller decodes the callee's output with its own type.
fn response_breaks(
    view: &BundleEndpoint,
    serve: &BundleEndpoint,
    call: &BundleCall,
    report: &mut impl FnMut(BreakKind),
) {
    if view.response_list != serve.response_list {
        // An object where an array was, or the reverse: no field matters.
        report(BreakKind::ResponseListChanged);
        return;
    }
    for issue in decode_issues(&view.response, &serve.response.serialized, &call.reads) {
        report(match issue {
            Decode::Missing(f) => BreakKind::ResponseFieldMissing(f),
            Decode::Fails(f) => BreakKind::ResponseDecodeFails(f),
            Decode::Rejected(f) => BreakKind::ResponseFieldRejected(f),
            Decode::DuplicateKey(f) => BreakKind::ResponseDuplicateKey(f),
            Decode::TypeChanged {
                field,
                reader,
                writer,
            } => BreakKind::ResponseTypeChanged {
                field,
                caller: reader,
                callee: writer,
            },
        });
    }
}

/// One way a reader's type fails on what a writer sends.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Decode {
    /// The reader reads a field the writer does not send.
    Missing(String),
    /// The reader's type needs a field the writer may leave out.
    Fails(String),
    /// The reader's type denies unknown fields, and the writer sends one.
    Rejected(String),
    /// The reader's type gets one field under two keys.
    DuplicateKey(String),
    /// A field changed its Rust type.
    TypeChanged {
        /// The field's wire key.
        field: String,
        /// The type the reader decodes.
        reader: String,
        /// The type the writer sends.
        writer: String,
    },
}

/// The decode rules, shared by responses and events: `reader` decodes what
/// `produced` holds, and the code reads the fields in `reads`.
fn decode_issues(
    reader: &WireTypeDescriptor,
    produced: &[WireFieldDescriptor],
    reads: &[String],
) -> Vec<Decode> {
    let mut out = Vec::new();
    let decoded = &reader.deserialized;
    let mut named = BTreeSet::new();
    for read in reads {
        let Some(field) = decoded.iter().find(|f| &f.rust_name == read) else {
            continue;
        };
        if produced_for(produced, field).is_none() {
            named.insert(field.wire_name.clone());
            out.push(Decode::Missing(field.wire_name.clone()));
        }
    }
    for field in decoded {
        if key_hits(produced, field) > 1 {
            out.push(Decode::DuplicateKey(field.wire_name.clone()));
        }
        match produced_for(produced, field) {
            Some(made) if !type_reads(&field.ty, &made.ty) => {
                out.push(Decode::TypeChanged {
                    field: field.wire_name.clone(),
                    reader: field.ty.clone(),
                    writer: made.ty.clone(),
                });
            }
            Some(made) if field.required && !made.required => {
                out.push(Decode::Fails(field.wire_name.clone()));
            }
            None if field.required && !named.contains(&field.wire_name) => {
                out.push(Decode::Fails(field.wire_name.clone()));
            }
            _ => {}
        }
    }
    if reader.closed {
        for made in produced {
            if accepting(decoded, made).is_none() {
                out.push(Decode::Rejected(made.wire_name.clone()));
            }
        }
    }
    out
}

/// Request rules: the callee decodes what the caller's own type sends.
fn request_breaks(
    view: &BundleEndpoint,
    serve: &BundleEndpoint,
    call: &BundleCall,
    report: &mut impl FnMut(BreakKind),
) {
    let sent = &view.request.serialized;
    let accepted = &serve.request.deserialized;
    if view.request_list != serve.request_list {
        report(BreakKind::RequestListChanged);
        return;
    }
    if !view.has_body && serve.has_body {
        // The client sends no body and no content type. A `Json` extractor
        // refuses that request, whatever its fields are.
        report(BreakKind::RequestBodyMissing);
    }
    let writes = call.writes.as_deref().unwrap_or_default();
    // With an unknown write-set, a field the type sometimes sends counts as
    // sent only when the caller's own release already required it: the
    // one-build check accepted that call. A requirement that is new in the
    // serving release gets no such benefit.
    let own_release_requires = |f: &WireFieldDescriptor| {
        view.request
            .deserialized
            .iter()
            .any(|d| d.required && d.wire_name == f.wire_name)
    };
    let always_sent = |f: &WireFieldDescriptor| {
        f.required
            || writes.contains(&f.rust_name)
            || (call.writes.is_none() && own_release_requires(f))
    };
    for need in accepted {
        if key_hits(sent, need) > 1 {
            report(BreakKind::RequestDuplicateKey(need.wire_name.clone()));
        }
    }
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
            Some(into) if !type_reads(&into.ty, &field.ty) => {
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
}

impl Report {
    /// Which deploy orders are safe.
    #[must_use]
    pub const fn verdict(&self) -> Verdict {
        match (self.callee_first.is_empty(), self.caller_first.is_empty()) {
            (true, true) => Verdict::Compatible,
            (true, false) => Verdict::CalleeFirst,
            (false, true) => Verdict::CallerFirst,
            (false, false) => Verdict::Incompatible,
        }
    }

    /// The process exit code for a declared order: [`EXIT_SAFE`],
    /// [`EXIT_UNSAFE`], or [`EXIT_ORDER`].
    #[must_use]
    pub fn exit_code(&self, order: Order) -> i32 {
        let safe_if = |breaks: &[Break]| {
            if breaks.is_empty() {
                EXIT_SAFE
            } else {
                EXIT_UNSAFE
            }
        };
        match (order, self.verdict()) {
            (Order::Any, Verdict::Compatible) => EXIT_SAFE,
            (Order::Any, Verdict::CalleeFirst | Verdict::CallerFirst) => EXIT_ORDER,
            (Order::Any, Verdict::Incompatible) => EXIT_UNSAFE,
            (Order::CalleeFirst, _) => safe_if(&self.callee_first),
            (Order::CallerFirst, _) => safe_if(&self.caller_first),
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
        for note in &self.notes {
            let _ = writeln!(out, "Note: {note}");
        }
        if !self.notes.is_empty() {
            out.push('\n');
        }
        let services_of = |breaks: &[Break]| join(breaks.iter().map(|b| b.service.clone()));
        let crates_of = |breaks: &[Break]| {
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
                services_of(&self.caller_first),
                crates_of(&self.caller_first),
            ),
            Verdict::CallerFirst => format!(
                "Verdict: safe only if the callers ship first.\n  Deploy {} before {}.\n",
                crates_of(&self.callee_first),
                services_of(&self.callee_first),
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
        if b.kind.is_event() {
            let _ = writeln!(out, "  - {} ({}): {}", b.endpoint, b.route, event_detail(b));
            continue;
        }
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
            BreakKind::ResponseDuplicateKey(f) => format!(
                "{who} decodes field `{f}` under its name or an alias, and {callee} produces \
                 both keys"
            ),
            BreakKind::ResponseListChanged => format!(
                "the response body is a JSON array on one side and an object on the other, so \
                 {who} cannot decode what {callee} sends"
            ),
            BreakKind::RequestListChanged => format!(
                "the request body is a JSON array on one side and an object on the other, so \
                 {callee} cannot decode what {who} sends"
            ),
            BreakKind::RequestBodyMissing => {
                format!("{who} sends no request body, and {callee} reads one")
            }
            BreakKind::RequestDuplicateKey(f) => format!(
                "{callee} decodes field `{f}` under its name or an alias, and {who} sends both \
                 keys"
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
            // Rendered by `event_detail`.
            BreakKind::EventTopicMissing
            | BreakKind::EventFieldMissing(_)
            | BreakKind::EventDecodeFails(_)
            | BreakKind::EventFieldRejected(_)
            | BreakKind::EventDuplicateKey(_)
            | BreakKind::EventTypeChanged { .. } => String::new(),
        };
        let _ = writeln!(out, "  - {} ({}): {detail}", b.endpoint, b.route);
    }
    out.push('\n');
}

/// The text for one event break.
fn event_detail(b: &Break) -> String {
    let who = b.caller.as_deref().map_or_else(
        || "a consumer".to_owned(),
        |consumer| format!("consumer `{consumer}`"),
    );
    let (producer, own_type) = match b.window {
        Window::CalleeFirst => ("the new producer", "its deployed type"),
        Window::CallerFirst => ("the deployed producer", "its new type"),
    };
    match &b.kind {
        BreakKind::EventTopicMissing => {
            format!("{who} reads this topic, and {producer} does not send it")
        }
        BreakKind::EventFieldMissing(f) => {
            format!("{who} reads field `{f}`, which {producer} does not send")
        }
        BreakKind::EventDecodeFails(f) => format!(
            "{who} decodes the event with {own_type}, which needs field `{f}`, and {producer} \
             does not always send it"
        ),
        BreakKind::EventFieldRejected(f) => format!(
            "{who} decodes the event with {own_type}, which denies unknown fields, and \
             {producer} sends `{f}`"
        ),
        BreakKind::EventDuplicateKey(f) => format!(
            "{who} decodes field `{f}` under its name or an alias, and {producer} sends both keys"
        ),
        BreakKind::EventTypeChanged {
            field,
            consumer,
            producer: theirs,
        } => format!(
            "event field `{field}` is `{theirs}` in {producer}, and {who} decodes it as \
             `{consumer}`"
        ),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::bundle::{BUNDLE_FORMAT, BundleEvent, BundleListener};
    use autumn_macros_support::wire::WireTypeDescriptor;

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
                    has_body: true,
                    request_list: false,
                    response_list: false,
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
                    has_body: false,
                    request_list: false,
                    response_list: false,
                    request: shape("NoBody", Vec::new(), Vec::new()),
                    response: item(),
                },
            ],
            callers: vec![
                call("add_item", "create_item", &["id"], Some(&["name"])),
                call("show_item", "get_item", &["name"], None),
            ],
            events: Vec::new(),
            listeners: Vec::new(),
            unresolved: Vec::new(),
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
        assert_eq!(kinds(&report.callee_first), [BreakKind::RouteMissing]);
        assert_eq!(kinds(&report.caller_first), [BreakKind::RouteMissing]);
        assert_eq!(report.verdict(), Verdict::Incompatible);
        assert_eq!(report.exit_code(Order::Any), 1);
    }

    #[test]
    fn a_route_with_no_known_caller_is_still_kept() {
        let mut deployed = release_1();
        deployed.callers.retain(|c| c.endpoint != "get_item");
        let mut current = deployed.clone();
        current.endpoints.retain(|e| e.name != "get_item");
        let report = compare(&deployed, &current);
        assert_eq!(kinds(&report.callee_first), [BreakKind::RouteRemoved]);
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
        assert_eq!(report.exit_code(Order::Any), EXIT_ORDER);
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

    // --- events ---

    /// `(name, type, always sent)`. An `Option` is not required on decode.
    fn contact_shape(fields: &[(&str, &str, bool)]) -> WireTypeDescriptor {
        let sent: Vec<WireFieldDescriptor> = fields
            .iter()
            .map(|(name, ty, required)| field(name, name, ty, *required))
            .collect();
        let decoded: Vec<WireFieldDescriptor> = fields
            .iter()
            .map(|(name, ty, required)| {
                field(name, name, ty, *required && !ty.starts_with("Option<"))
            })
            .collect();
        shape("ContactUpdated", sent, decoded)
    }

    fn event(version: u32, shape: WireTypeDescriptor) -> BundleEvent {
        let name = "contact.updated".to_owned();
        BundleEvent {
            topic: autumn_macros_support::wire::versioned_topic(&name, version),
            name,
            version,
            krate: "core".to_owned(),
            type_name: format!("ContactUpdatedV{version}"),
            opaque: false,
            shape,
        }
    }

    fn listener(function: &str, topic: &str, reads: &[&str]) -> BundleListener {
        BundleListener {
            krate: "marketing".to_owned(),
            function: function.to_owned(),
            topic: topic.to_owned(),
            reads: reads.iter().map(|r| (*r).to_owned()).collect(),
        }
    }

    /// Release 1 plus `contact.updated { id, email }` and a consumer of
    /// `email`.
    fn with_events() -> Bundle {
        let mut bundle = release_1();
        bundle.events = vec![event(
            1,
            contact_shape(&[("id", "i64", true), ("email", "String", true)]),
        )];
        bundle.listeners = vec![listener("on_contact", "contact.updated", &["email"])];
        bundle
    }

    #[test]
    fn a_removed_event_field_a_deployed_consumer_reads_fails() {
        let deployed = with_events();
        let mut current = with_events();
        current.events[0].shape = contact_shape(&[("id", "i64", true)]);
        // The new consumer cannot read it any more: it would not compile.
        current.listeners = vec![listener("on_contact", "contact.updated", &["id"])];
        let report = compare(&deployed, &current);
        assert_eq!(
            kinds(&report.callee_first),
            [BreakKind::EventFieldMissing("email".to_owned())]
        );
        assert!(report.caller_first.is_empty(), "{report:?}");
        assert_eq!(report.verdict(), Verdict::CallerFirst);
        assert_eq!(report.exit_code(Order::Any), EXIT_ORDER);
        assert_ne!(report.exit_code(Order::CalleeFirst), 0);
    }

    #[test]
    fn a_new_event_field_passes() {
        let deployed = with_events();
        let mut current = with_events();
        current.events[0].shape = contact_shape(&[
            ("id", "i64", true),
            ("email", "String", true),
            ("phone", "Option<String>", true),
        ]);
        let report = compare(&deployed, &current);
        assert_eq!(report, Report::default(), "{report:?}");
    }

    #[test]
    fn a_consumer_that_starts_reading_a_new_field_needs_the_producer_first() {
        let deployed = with_events();
        let mut current = with_events();
        current.events[0].shape = contact_shape(&[
            ("id", "i64", true),
            ("email", "String", true),
            ("phone", "String", true),
        ]);
        current.listeners = vec![listener("on_contact", "contact.updated", &["phone"])];
        let report = compare(&deployed, &current);
        assert!(report.callee_first.is_empty(), "{report:?}");
        assert_eq!(
            kinds(&report.caller_first),
            [BreakKind::EventFieldMissing("phone".to_owned())]
        );
        assert_eq!(report.verdict(), Verdict::CalleeFirst);
    }

    #[test]
    fn an_unread_required_event_field_removed_breaks_the_old_decode() {
        let deployed = with_events();
        let mut current = with_events();
        current.events[0].shape = contact_shape(&[("email", "String", true)]);
        let report = compare(&deployed, &current);
        assert_eq!(
            kinds(&report.callee_first),
            [BreakKind::EventDecodeFails("id".to_owned())]
        );
    }

    #[test]
    fn an_event_field_type_change_is_a_break() {
        let deployed = with_events();
        let mut current = with_events();
        current.events[0].shape = contact_shape(&[("id", "i64", true), ("email", "u32", true)]);
        let report = compare(&deployed, &current);
        assert_eq!(
            kinds(&report.callee_first),
            [BreakKind::EventTypeChanged {
                field: "email".to_owned(),
                consumer: "String".to_owned(),
                producer: "u32".to_owned(),
            }]
        );
        assert_eq!(report.verdict(), Verdict::Incompatible);
    }

    #[test]
    fn a_closed_event_type_rejects_a_new_field() {
        let mut deployed = with_events();
        deployed.events[0].shape.closed = true;
        let mut current = deployed.clone();
        current.events[0].shape = contact_shape(&[
            ("id", "i64", true),
            ("email", "String", true),
            ("phone", "Option<String>", true),
        ]);
        current.events[0].shape.closed = true;
        let report = compare(&deployed, &current);
        assert_eq!(
            kinds(&report.callee_first),
            [BreakKind::EventFieldRejected("phone".to_owned())]
        );
    }

    #[test]
    fn a_topic_no_new_producer_sends_breaks_a_deployed_consumer() {
        let deployed = with_events();
        let mut current = with_events();
        current.events = vec![event(
            2,
            contact_shape(&[("id", "i64", true), ("email", "String", true)]),
        )];
        current.listeners = vec![listener("on_contact", "contact.updated.v2", &["email"])];
        let report = compare(&deployed, &current);
        assert_eq!(kinds(&report.callee_first), [BreakKind::EventTopicMissing]);
        // v1 replaced by v2 in one release: no order is safe.
        assert_eq!(kinds(&report.caller_first), [BreakKind::EventTopicMissing]);
        assert_eq!(report.verdict(), Verdict::Incompatible);
        let text = report.render(Order::Any);
        assert!(text.contains("contact.updated (event)"), "{text}");
        assert!(text.contains("marketing::on_contact"), "{text}");
    }

    #[test]
    fn a_new_consumer_of_a_topic_the_deployed_release_lacks_is_not_a_break() {
        let deployed = release_1();
        let current = with_events();
        let report = compare(&deployed, &current);
        assert_eq!(report, Report::default(), "{report:?}");
    }

    #[test]
    fn a_v2_beside_v1_needs_the_producer_first_when_the_consumer_moves_to_v2() {
        let deployed = with_events();
        let mut current = with_events();
        current.events.push(event(
            2,
            contact_shape(&[("id", "i64", true), ("email_address", "String", true)]),
        ));
        current.listeners = vec![listener(
            "on_contact",
            "contact.updated.v2",
            &["email_address"],
        )];
        let report = compare(&deployed, &current);
        // The deployed consumer still gets v1 from the new producer.
        assert!(report.callee_first.is_empty(), "{report:?}");
        // A new consumer of v2 next to a producer of v1 only: v1 is lost.
        assert_eq!(kinds(&report.caller_first), [BreakKind::EventTopicMissing]);
        assert_eq!(report.verdict(), Verdict::CalleeFirst);
    }

    #[test]
    fn a_v2_beside_v1_is_safe_while_the_consumer_reads_both() {
        let deployed = with_events();
        let mut current = with_events();
        current.events.push(event(
            2,
            contact_shape(&[("id", "i64", true), ("email_address", "String", true)]),
        ));
        current.listeners.push(listener(
            "on_contact_v2",
            "contact.updated.v2",
            &["email_address"],
        ));
        let report = compare(&deployed, &current);
        assert_eq!(report, Report::default(), "{report:?}");
    }

    #[test]
    fn a_deploy_order_names_an_event_crate_by_its_service() {
        let mut deployed = with_events();
        deployed.events[0].krate = "mesh-catalog".to_owned();
        let mut current = deployed.clone();
        current.events[0].shape = contact_shape(&[("id", "i64", true)]);
        current.listeners.clear();
        let text = compare(&deployed, &current).render(Order::Any);
        assert!(text.contains("Deploy marketing before catalog."), "{text}");
    }

    #[test]
    fn the_check_reports_when_v1_can_be_dropped() {
        let mut deployed = with_events();
        deployed.events.push(event(
            2,
            contact_shape(&[("id", "i64", true), ("email_address", "String", true)]),
        ));
        deployed.listeners = vec![listener(
            "on_contact",
            "contact.updated.v2",
            &["email_address"],
        )];
        let current = deployed.clone();
        let report = compare(&deployed, &current);
        assert_eq!(report.verdict(), Verdict::Compatible);
        assert_eq!(report.notes.len(), 1, "{report:?}");
        assert!(report.notes[0].contains("`contact.updated`"), "{report:?}");
        assert!(report.notes[0].contains("v1"), "{report:?}");
        assert!(report.render(Order::Any).contains("can be dropped"));
    }

    #[test]
    fn an_unseen_consumer_is_named_and_no_version_is_called_droppable() {
        let mut deployed = with_events();
        deployed.events.push(event(
            2,
            contact_shape(&[("id", "i64", true), ("email", "String", true)]),
        ));
        deployed.listeners = vec![listener("on_contact", "contact.updated.v2", &["email"])];
        deployed.unresolved = vec![
            "`marketing::on_old` consumes `Updated`: no `#[event]` in the pass has that \
                  type name"
                .to_owned(),
        ];
        let report = compare(&deployed, &deployed.clone());
        assert_eq!(report.verdict(), Verdict::Compatible);
        let text = report.render(Order::Any);
        assert!(text.contains("not in the bundles"), "{text}");
        assert!(text.contains("marketing::on_old"), "{text}");
        assert!(!text.contains("can be dropped"), "{text}");
    }

    #[test]
    fn v1_is_kept_while_any_consumer_reads_it() {
        let mut current = with_events();
        current.events.push(event(
            2,
            contact_shape(&[("id", "i64", true), ("email", "String", true)]),
        ));
        let report = compare(&current, &current);
        assert!(report.notes.is_empty(), "{report:?}");
    }

    #[test]
    fn an_opaque_event_is_not_checked() {
        let deployed = with_events();
        let mut current = with_events();
        current.events[0].opaque = true;
        current.events[0].shape = WireTypeDescriptor::default();
        let report = compare(&deployed, &current);
        assert_eq!(report, Report::default(), "{report:?}");
    }

    #[test]
    fn an_event_break_names_the_topic_the_consumer_and_the_field() {
        let deployed = with_events();
        let mut current = with_events();
        current.events[0].shape = contact_shape(&[("id", "i64", true)]);
        current.listeners.clear();
        let text = compare(&deployed, &current).render(Order::Any);
        assert!(text.contains("contact.updated (event)"), "{text}");
        assert!(text.contains("consumer `marketing::on_contact`"), "{text}");
        assert!(text.contains("reads field `email`"), "{text}");
        assert!(text.contains("Deploy marketing before core."), "{text}");
    }

    // --- review regressions ---

    #[test]
    fn a_body_on_a_route_that_had_none_is_a_break() {
        let deployed = release_1();
        let mut current = release_1();
        ep(&mut current, "get_item").has_body = true;
        ep(&mut current, "get_item").request = shape(
            "Filter",
            vec![field("q", "q", "Option<String>", true)],
            vec![field("q", "q", "Option<String>", false)],
        );
        let report = compare(&deployed, &current);
        assert_eq!(kinds(&report.callee_first), [BreakKind::RequestBodyMissing]);
    }

    #[test]
    fn an_alias_and_its_target_key_both_produced_break_the_decode() {
        let mut deployed = release_1();
        for f in &mut ep(&mut deployed, "get_item").response.deserialized {
            if f.rust_name == "name" {
                f.aliases = vec!["title".to_owned()];
            }
        }
        let mut current = deployed.clone();
        let item = &mut ep(&mut current, "get_item").response;
        item.serialized.push(plain("title", true));
        item.deserialized.push(plain("title", true));
        let report = compare(&deployed, &current);
        assert!(
            kinds(&report.callee_first)
                .contains(&BreakKind::ResponseDuplicateKey("name".to_owned())),
            "{report:?}"
        );
    }

    #[test]
    fn a_removed_route_names_the_deployed_callers_in_the_order() {
        let deployed = release_1();
        let mut current = release_1();
        current.endpoints.retain(|e| e.name != "get_item");
        current.callers.retain(|c| c.endpoint != "get_item");
        let report = compare(&deployed, &current);
        assert_eq!(
            report
                .callee_first
                .iter()
                .map(|b| b.caller.clone())
                .collect::<Vec<_>>(),
            [Some("mesh-storefront::show_item".to_owned())]
        );
        let text = report.render(Order::Any);
        assert!(
            text.contains("Deploy mesh-storefront before catalog"),
            "{text}"
        );
    }

    #[test]
    fn an_unknown_write_set_is_not_a_break_in_an_unchanged_release() {
        let mut release = release_1();
        let req = &mut ep(&mut release, "get_item").request;
        req.name = "Lookup".to_owned();
        req.serialized.push(plain("token", false));
        req.deserialized.push(plain("token", true));
        assert_eq!(compare(&release, &release), Report::default());
    }

    #[test]
    fn a_field_that_becomes_optional_on_decode_is_safe_in_one_order() {
        let deployed = release_1();
        let mut current = release_1();
        // The callee now decodes `name` as optional. Deployed callers still
        // send a `String`: safe. New callers may leave it out: the old callee
        // needs it.
        let req = &mut ep(&mut current, "create_item").request;
        for f in &mut req.deserialized {
            if f.rust_name == "name" {
                f.ty = "Option<String>".to_owned();
                f.required = false;
            }
        }
        for f in &mut req.serialized {
            if f.rust_name == "name" {
                f.ty = "Option<String>".to_owned();
            }
        }
        let report = compare(&deployed, &current);
        assert_eq!(report.callee_first, Vec::new(), "{report:?}");
        assert_ne!(report.caller_first, Vec::new());
    }

    #[test]
    fn a_trailing_slash_is_a_different_route() {
        let deployed = release_1();
        let mut current = release_1();
        ep(&mut current, "create_item").path = "/items/".to_owned();
        assert_eq!(
            compare(&deployed, &current).verdict(),
            Verdict::Incompatible
        );
    }

    #[test]
    fn a_catch_all_is_not_a_plain_parameter() {
        let deployed = release_1();
        let mut current = release_1();
        ep(&mut current, "get_item").path = "/items/{*rest}".to_owned();
        assert_eq!(
            compare(&deployed, &current).verdict(),
            Verdict::Incompatible
        );
    }

    #[test]
    fn an_unknown_write_set_does_not_cover_a_field_that_became_required() {
        let mut deployed = release_1();
        let req = &mut ep(&mut deployed, "get_item").request;
        req.name = "Lookup".to_owned();
        // Release 1: sent only when not empty, and optional on decode.
        req.serialized.push(plain("token", false));
        req.deserialized.push(plain("token", false));
        let mut current = deployed.clone();
        let req = &mut ep(&mut current, "get_item").request;
        req.serialized.retain(|f| f.rust_name != "token");
        req.deserialized.retain(|f| f.rust_name != "token");
        req.serialized.push(plain("token", true));
        req.deserialized.push(plain("token", true));
        let report = compare(&deployed, &current);
        assert_eq!(
            kinds(&report.callee_first),
            [BreakKind::RequestFieldNotSent("token".to_owned())]
        );
    }

    #[test]
    fn a_json_no_body_extractor_still_needs_a_body() {
        let deployed = release_1();
        let mut current = release_1();
        // `Json<NoBody>`: the type name is `NoBody`, and a body is read.
        ep(&mut current, "get_item").has_body = true;
        let report = compare(&deployed, &current);
        assert_eq!(kinds(&report.callee_first), [BreakKind::RequestBodyMissing]);
    }

    #[test]
    fn an_object_body_that_becomes_a_list_is_a_break() {
        let deployed = release_1();
        let mut current = release_1();
        ep(&mut current, "get_item").response_list = true;
        let report = compare(&deployed, &current);
        assert_eq!(report.verdict(), Verdict::Incompatible);
        assert_eq!(
            kinds(&report.callee_first),
            [BreakKind::ResponseListChanged]
        );
        let mut current = release_1();
        ep(&mut current, "create_item").request_list = true;
        let report = compare(&deployed, &current);
        assert_eq!(kinds(&report.callee_first), [BreakKind::RequestListChanged]);
    }
}
