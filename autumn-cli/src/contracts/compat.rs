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
//!
//! A field whose type is a nested `WireShape` type is compared field by
//! field, with the same rules, under a dotted name (`details.size`). A read
//! or a write of a nested field is a dotted path too: `details.color`. The
//! paths under a field are its read-set and write-set one level down. A unit
//! struct is JSON `null`, so a unit type and an object type do not match.
//! Both need a bundle of [`IDENTITY_FORMAT`](super::bundle::IDENTITY_FORMAT):
//! an older bundle does not record
//! them, and the check then compares the type names only.
//!
//! A field's wire type is its JSON form. When both sides record one, a new
//! form with the same Rust type is a break. A bundle older than
//! [`WIRE_TYPE_FORMAT`](super::bundle::WIRE_TYPE_FORMAT) records none.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use autumn_macros_support::wire::{WireFieldDescriptor, WireTypeDescriptor};

use super::bundle::{Bundle, BundleCall, BundleEndpoint, BundleEvent, BundleListener, BundleType};

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
    /// A new caller calls a route that no `routes![…]` in its own release
    /// registers. It breaks when both sides run the new release.
    RouteNotServed,
    /// The response body is JSON `null` (a unit struct) on one side and an
    /// object on the other.
    ResponseUnitChanged,
    /// The request body is JSON `null` (a unit struct) on one side and an
    /// object on the other.
    RequestUnitChanged,
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
    /// A response field changed its Rust type or its JSON form.
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
    /// A request field changed its Rust type or its JSON form.
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
    /// The payload is JSON `null` (a unit struct) on one side and an object
    /// on the other.
    EventUnitChanged,
    /// The consumer reads an event field the producer does not send.
    EventFieldMissing(String),
    /// The consumer's event type needs a field the producer may leave out.
    EventDecodeFails(String),
    /// The consumer's event type denies unknown fields, and the producer
    /// sends one.
    EventFieldRejected(String),
    /// The consumer's event type gets one field under two keys.
    EventDuplicateKey(String),
    /// An event field changed its Rust type or its JSON form.
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
                | Self::EventUnitChanged
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
    for old in deployed.endpoints.iter().filter(|e| !e.unrouted) {
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
    // A new caller of a route that its own release does not serve fails once
    // both sides run the new release, whatever the order. A caller that the
    // deployed release already left without a route is not new.
    for call in &current.callers {
        let Some(view) = current.endpoint(&call.service, &call.endpoint) else {
            continue;
        };
        if serving(current, view).is_some() || already_unserved(deployed, call) {
            continue;
        }
        new_callers.push(call_break(
            Window::CallerFirst,
            view,
            call,
            BreakKind::RouteNotServed,
        ));
        let named = old_callers.iter().any(|b| {
            b.kind == BreakKind::RouteMissing
                && b.endpoint == view.id()
                && b.caller.as_deref() == Some(call.caller().as_str())
        });
        if !named {
            old_callers.push(call_break(
                Window::CalleeFirst,
                view,
                call,
                BreakKind::RouteNotServed,
            ));
        }
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

/// Whether the deployed release has a call from the same function to the
/// same endpoint, and does not serve its route either.
fn already_unserved(deployed: &Bundle, call: &BundleCall) -> bool {
    deployed.callers.iter().any(|c| {
        (&c.krate, &c.function, &c.service, &c.endpoint)
            == (&call.krate, &call.function, &call.service, &call.endpoint)
    }) && deployed
        .endpoint(&call.service, &call.endpoint)
        .is_some_and(|view| serving(deployed, view).is_none())
}

/// A break of one call, named as the caller's release knows the endpoint.
fn call_break(window: Window, view: &BundleEndpoint, call: &BundleCall, kind: BreakKind) -> Break {
    Break {
        window,
        endpoint: view.id(),
        route: view.route(),
        service: view.service.clone(),
        caller: Some(call.caller()),
        caller_krate: Some(call.krate.clone()),
        kind,
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
        // A version that this consumer's crate does not read in its own
        // release is lost to it while this window lasts. Another crate that
        // still reads that version does not run this consumer's effects.
        let lost = sending_release.events.iter().any(|e| {
            e.name == view.name
                && !built_against
                    .listeners
                    .iter()
                    .any(|l| l.topic == e.topic && l.krate == listener.krate)
        });
        if window == Window::CalleeFirst || lost {
            report(BreakKind::EventTopicMissing);
        }
        return out;
    };
    if view.opaque || sent.opaque {
        return out;
    }
    if view.unit != sent.unit {
        // `null` where an object was, or the reverse: no field matters.
        report(BreakKind::EventUnitChanged);
        return out;
    }
    let mut walk = Walk::new(built_against, sending_release);
    for issue in decode_issues(
        &view.shape,
        &sent.shape.serialized,
        &listener.reads,
        &mut walk,
    ) {
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
/// wins when two serve one route. An unrouted endpoint serves nothing.
fn serving<'a>(side: &'a Bundle, wanted: &BundleEndpoint) -> Option<&'a BundleEndpoint> {
    let key = route_key(wanted);
    let mut found = side
        .endpoints
        .iter()
        .filter(|e| !e.unrouted && route_key(e) == key);
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

/// The two type labels when a decoder of `reader` cannot read what
/// `writer` holds, else `None`.
///
/// The Rust types come first. When they read, and both fields record a wire
/// type, the wire types must read too: a new JSON form with the same Rust
/// type is a break. A label then names both, as `ty as wire`.
fn type_change(
    reader: &WireFieldDescriptor,
    writer: &WireFieldDescriptor,
) -> Option<(String, String)> {
    if !type_reads(&reader.ty, &writer.ty) {
        return Some((reader.ty.clone(), writer.ty.clone()));
    }
    match (&reader.wire_type, &writer.wire_type) {
        (Some(read), Some(written)) if !read.reads(written) => Some((
            format!("{} as {read}", reader.ty),
            format!("{} as {written}", writer.ty),
        )),
        _ => None,
    }
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
    // A route that the caller's own release does not serve is not a break
    // of this window. `compare` reports a new one.
    if serving(built_against, view).is_none() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut report = |kind| out.push(call_break(window, view, call, kind));
    let Some(serve) = serving(serving_release, view) else {
        report(BreakKind::RouteMissing);
        return out;
    };
    let mut decode = Walk::new(built_against, serving_release);
    response_breaks(view, serve, call, &mut decode, &mut report);
    let mut encode = Walk::new(serving_release, built_against);
    request_breaks(view, serve, call, &mut encode, &mut report);
    out
}

/// The state of one walk through nested types. `reader` holds the types of
/// the side that decodes, `writer` the types of the side that encodes.
///
/// Field names are relative to the level they are found on. The level above
/// adds its field name, so `color` becomes `details.color`. Each type pair is
/// compared once per read-set or write-set. The result is kept, so fields
/// that share a type and a set cost one comparison.
struct Walk<'a> {
    reader: &'a Bundle,
    writer: &'a Bundle,
    /// Both bundles record nested types and the `unit` flag.
    typed: bool,
    /// The comparisons on the current path, so a recursive type ends. A
    /// read-set gets shorter at each level, so it ends too.
    path: BTreeSet<WalkKey>,
    /// The decode result of each type pair and read-set compared so far.
    decoded: BTreeMap<WalkKey, Vec<Decode>>,
    /// The request result of each type pair and write-set compared so far.
    encoded: BTreeMap<WalkKey, Vec<BreakKind>>,
}

/// One comparison: the reader's type id, the writer's type id, and the
/// read-set or write-set. A write-set of `None` is unknown.
type WalkKey = (String, String, Option<Vec<String>>);

impl<'a> Walk<'a> {
    const fn new(reader: &'a Bundle, writer: &'a Bundle) -> Self {
        Self {
            reader,
            writer,
            typed: reader.knows_types() && writer.knows_types(),
            path: BTreeSet::new(),
            decoded: BTreeMap::new(),
            encoded: BTreeMap::new(),
        }
    }

    /// The nested type pairs that two matched fields name, by position. An
    /// opaque field and an older bundle give none. `Err` holds the two
    /// fields' type labels when the two lists differ in length: one side has
    /// a shape where the other has none.
    fn pairs(
        &self,
        reader: &WireFieldDescriptor,
        writer: &WireFieldDescriptor,
    ) -> Result<Vec<(&'a BundleType, &'a BundleType)>, (String, String)> {
        if !self.typed || reader.opaque || writer.opaque {
            return Ok(Vec::new());
        }
        if reader.nested.len() != writer.nested.len() {
            let label = |f: &WireFieldDescriptor| format!("{} [{}]", f.ty, f.nested.join(", "));
            return Err((label(reader), label(writer)));
        }
        Ok(reader
            .nested
            .iter()
            .zip(&writer.nested)
            .filter_map(|(r, w)| Some((self.reader.nested(r)?, self.writer.nested(w)?)))
            .collect())
    }

    /// The decode issues of one type pair, with names relative to it.
    /// `reads` is the read-set at this level.
    fn decode(
        &mut self,
        reader: &'a BundleType,
        writer: &'a BundleType,
        reads: Vec<String>,
    ) -> Vec<Decode> {
        let key = (reader.id.clone(), writer.id.clone(), Some(reads));
        if let Some(done) = self.decoded.get(&key) {
            return done.clone();
        }
        if !self.path.insert(key.clone()) {
            return Vec::new();
        }
        let reads = key.2.as_deref().unwrap_or_default();
        let mut found = decode_issues(&reader.shape, &writer.shape.serialized, reads, self);
        found.truncate(MAX_NESTED_ISSUES);
        self.path.remove(&key);
        self.decoded.insert(key, found.clone());
        found
    }

    /// The request issues of one type pair, with names relative to it.
    /// `receiver` decodes what `sender` sends. `writes` is the write-set at
    /// this level, `None` when it is unknown.
    fn encode(
        &mut self,
        receiver: &'a BundleType,
        sender: &'a BundleType,
        writes: Option<Vec<String>>,
    ) -> Vec<BreakKind> {
        let key = (receiver.id.clone(), sender.id.clone(), writes);
        if let Some(done) = self.encoded.get(&key) {
            return done.clone();
        }
        if !self.path.insert(key.clone()) {
            return Vec::new();
        }
        let mut found = request_issues(&sender.shape, &receiver.shape, key.2.as_deref(), self);
        found.truncate(MAX_NESTED_ISSUES);
        self.path.remove(&key);
        self.encoded.insert(key, found.clone());
        found
    }
}

/// The most issues one nested type pair reports. A type that many fields
/// share at many levels has one issue per path, and the paths grow fast. One
/// issue is enough for the verdict.
const MAX_NESTED_ISSUES: usize = 100;

/// `field.name`: a nested name under the field that holds it.
fn under(field: &str, name: &str) -> String {
    format!("{field}.{name}")
}

/// The top-level field of each path: `details.color` gives `details`.
fn top_level(paths: &[String]) -> BTreeSet<&str> {
    paths
        .iter()
        .map(|p| p.split('.').next().unwrap_or_default())
        .collect()
}

/// The paths under `field`, without its prefix: `details.color` under
/// `details` gives `color`.
fn below(paths: &[String], field: &str) -> Vec<String> {
    paths
        .iter()
        .filter_map(|p| p.strip_prefix(field)?.strip_prefix('.'))
        .map(str::to_owned)
        .collect()
}

impl Decode {
    /// This issue one level down, under `field`.
    fn under(self, field: &str) -> Self {
        match self {
            Self::Missing(f) => Self::Missing(under(field, &f)),
            Self::Fails(f) => Self::Fails(under(field, &f)),
            Self::Rejected(f) => Self::Rejected(under(field, &f)),
            Self::DuplicateKey(f) => Self::DuplicateKey(under(field, &f)),
            Self::TypeChanged {
                field: f,
                reader,
                writer,
            } => Self::TypeChanged {
                field: under(field, &f),
                reader,
                writer,
            },
        }
    }
}

impl BreakKind {
    /// This request break one level down, under `field`. Other kinds do not
    /// occur on a nested level and stay as they are.
    fn under(self, field: &str) -> Self {
        match self {
            Self::RequestFieldNotSent(f) => Self::RequestFieldNotSent(under(field, &f)),
            Self::RequestFieldDropped(f) => Self::RequestFieldDropped(under(field, &f)),
            Self::RequestFieldRejected(f) => Self::RequestFieldRejected(under(field, &f)),
            Self::RequestDuplicateKey(f) => Self::RequestDuplicateKey(under(field, &f)),
            Self::RequestTypeChanged {
                field: f,
                caller,
                callee,
            } => Self::RequestTypeChanged {
                field: under(field, &f),
                caller,
                callee,
            },
            other => other,
        }
    }
}

/// `Name (JSON null)` or `Name (JSON object)`.
fn unit_label(shape: &WireTypeDescriptor) -> String {
    let form = if shape.unit { "null" } else { "object" };
    format!("{} (JSON {form})", shape.name)
}

/// Response rules: the caller decodes the callee's output with its own type.
fn response_breaks(
    view: &BundleEndpoint,
    serve: &BundleEndpoint,
    call: &BundleCall,
    walk: &mut Walk<'_>,
    report: &mut impl FnMut(BreakKind),
) {
    if view.response_list != serve.response_list {
        // An object where an array was, or the reverse: no field matters.
        report(BreakKind::ResponseListChanged);
        return;
    }
    if walk.typed && view.response.unit != serve.response.unit {
        // `null` where an object was, or the reverse. Also for list elements.
        report(BreakKind::ResponseUnitChanged);
        return;
    }
    for issue in decode_issues(
        &view.response,
        &serve.response.serialized,
        &call.reads,
        walk,
    ) {
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
    /// A field changed its Rust type or its JSON form.
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
///
/// A nested type of a matched field is compared in the same way, one level
/// down, with the reads under that field.
fn decode_issues(
    reader: &WireTypeDescriptor,
    produced: &[WireFieldDescriptor],
    reads: &[String],
    walk: &mut Walk<'_>,
) -> Vec<Decode> {
    let mut out = Vec::new();
    let decoded = &reader.deserialized;
    let mut named = BTreeSet::new();
    for read in top_level(reads) {
        let Some(field) = decoded.iter().find(|f| f.rust_name == read) else {
            continue;
        };
        if produced_for(produced, field).is_none() {
            named.insert(field.wire_name.clone());
            out.push(Decode::Missing(field.wire_name.clone()));
        }
    }
    for field in decoded {
        let name = field.wire_name.clone();
        if key_hits(produced, field) > 1 {
            out.push(Decode::DuplicateKey(name.clone()));
        }
        match produced_for(produced, field).map(|made| (made, type_change(field, made))) {
            Some((_, Some((reader, writer)))) => {
                out.push(Decode::TypeChanged {
                    field: name,
                    reader,
                    writer,
                });
            }
            Some((made, None)) => {
                if field.required && !made.required {
                    out.push(Decode::Fails(name.clone()));
                }
                let pairs = match walk.pairs(field, made) {
                    Ok(pairs) => pairs,
                    Err((reader, writer)) => {
                        out.push(Decode::TypeChanged {
                            field: name.clone(),
                            reader,
                            writer,
                        });
                        Vec::new()
                    }
                };
                for (theirs, ours) in pairs {
                    if theirs.shape.unit != ours.shape.unit {
                        out.push(Decode::TypeChanged {
                            field: name.clone(),
                            reader: unit_label(&theirs.shape),
                            writer: unit_label(&ours.shape),
                        });
                        continue;
                    }
                    let deeper = walk.decode(theirs, ours, below(reads, &field.rust_name));
                    out.extend(deeper.into_iter().map(|d| d.under(&field.wire_name)));
                }
            }
            None if field.required && !named.contains(&field.wire_name) => {
                out.push(Decode::Fails(name));
            }
            None => {}
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
    walk: &mut Walk<'_>,
    report: &mut impl FnMut(BreakKind),
) {
    if view.request_list != serve.request_list {
        report(BreakKind::RequestListChanged);
        return;
    }
    if !view.has_body && serve.has_body {
        // The client sends no body and no content type. A `Json` extractor
        // refuses that request, whatever its fields are.
        report(BreakKind::RequestBodyMissing);
    }
    if walk.typed && view.has_body && serve.has_body && view.request.unit != serve.request.unit {
        report(BreakKind::RequestUnitChanged);
        return;
    }
    for kind in request_issues(&view.request, &serve.request, call.writes.as_deref(), walk) {
        report(kind);
    }
}

/// The request rules for one level: `sender` is the caller's type, and
/// `receiver` the callee's. `writes` is the call site's write-set at this
/// level, `None` when it is unknown. A nested level has a known write-set
/// only when the call site sets it with an inline literal.
fn request_issues(
    sender: &WireTypeDescriptor,
    receiver: &WireTypeDescriptor,
    writes: Option<&[String]>,
    walk: &mut Walk<'_>,
) -> Vec<BreakKind> {
    let mut out = Vec::new();
    let mut report = |kind| out.push(kind);
    let sent = &sender.serialized;
    let accepted = &receiver.deserialized;
    let written = top_level(writes.unwrap_or_default());
    // With an unknown write-set, a field the type sometimes sends counts as
    // sent only when the caller's own release already required it: the
    // one-build check accepted that call. A requirement that is new in the
    // serving release gets no such benefit.
    let own_release_requires = |f: &WireFieldDescriptor| {
        sender
            .deserialized
            .iter()
            .any(|d| d.required && d.wire_name == f.wire_name)
    };
    let always_sent = |f: &WireFieldDescriptor| {
        f.required
            || written.contains(f.rust_name.as_str())
            || (writes.is_none() && own_release_requires(f))
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
    for write in &written {
        let Some(field) = sent.iter().find(|f| f.rust_name == *write) else {
            continue;
        };
        if accepting(accepted, field).is_none() && !receiver.closed {
            report(BreakKind::RequestFieldDropped(field.wire_name.clone()));
        }
    }
    for field in sent {
        match accepting(accepted, field).map(|into| (into, type_change(into, field))) {
            Some((_, Some((receiving, sending)))) => {
                report(BreakKind::RequestTypeChanged {
                    field: field.wire_name.clone(),
                    caller: sending,
                    callee: receiving,
                });
            }
            Some((into, None)) => {
                let pairs = match walk.pairs(into, field) {
                    Ok(pairs) => pairs,
                    Err((receiving, sending)) => {
                        report(BreakKind::RequestTypeChanged {
                            field: field.wire_name.clone(),
                            caller: sending,
                            callee: receiving,
                        });
                        Vec::new()
                    }
                };
                for (theirs, ours) in pairs {
                    if theirs.shape.unit != ours.shape.unit {
                        report(BreakKind::RequestTypeChanged {
                            field: field.wire_name.clone(),
                            caller: unit_label(&ours.shape),
                            callee: unit_label(&theirs.shape),
                        });
                        continue;
                    }
                    // A nested write-set is known when the call site sets
                    // a field under this one.
                    let nested = writes
                        .map(|w| below(w, &field.rust_name))
                        .filter(|w| !w.is_empty());
                    for kind in walk.encode(theirs, ours, nested) {
                        report(kind.under(&field.wire_name));
                    }
                }
            }
            None if receiver.closed => {
                report(BreakKind::RequestFieldRejected(field.wire_name.clone()));
            }
            None => {}
        }
    }
    out
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
        let _ = writeln!(
            out,
            "  - {} ({}): {}",
            b.endpoint,
            b.route,
            endpoint_detail(b)
        );
    }
    out.push('\n');
}

/// The text for one endpoint break.
fn endpoint_detail(b: &Break) -> String {
    let who = b.caller.as_deref().map_or_else(
        || "any client".to_owned(),
        |caller| format!("caller `{caller}`"),
    );
    let (callee, caller_release) = match b.window {
        Window::CalleeFirst => ("the new callee", "its deployed type"),
        Window::CallerFirst => ("the deployed callee", "its new type"),
    };
    match &b.kind {
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
        BreakKind::RouteNotServed => format!(
            "{who} calls this route, and no `routes![…]` in its own release registers the \
                 handler"
        ),
        BreakKind::ResponseUnitChanged => format!(
            "the response body is JSON `null` on one side and an object on the other, so \
                 {who} cannot decode what {callee} sends"
        ),
        BreakKind::RequestUnitChanged => format!(
            "the request body is JSON `null` on one side and an object on the other, so \
                 {callee} cannot decode what {who} sends"
        ),
        // Rendered by `event_detail`.
        BreakKind::EventTopicMissing
        | BreakKind::EventUnitChanged
        | BreakKind::EventFieldMissing(_)
        | BreakKind::EventDecodeFails(_)
        | BreakKind::EventFieldRejected(_)
        | BreakKind::EventDuplicateKey(_)
        | BreakKind::EventTypeChanged { .. } => String::new(),
    }
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
        BreakKind::EventUnitChanged => format!(
            "the payload is JSON `null` on one side and an object on the other, so {who} cannot \
             decode what {producer} sends"
        ),
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
    use autumn_macros_support::wire::{WireType, WireTypeDescriptor};

    fn field(rust: &str, wire: &str, ty: &str, required: bool) -> WireFieldDescriptor {
        WireFieldDescriptor {
            rust_name: rust.to_owned(),
            wire_name: wire.to_owned(),
            ty: ty.to_owned(),
            required,
            aliases: Vec::new(),
            ..Default::default()
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
            ..Default::default()
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
                    unrouted: false,
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
                    unrouted: false,
                },
            ],
            callers: vec![
                call("add_item", "create_item", &["id"], Some(&["name"])),
                call("show_item", "get_item", &["name"], None),
            ],
            events: Vec::new(),
            listeners: Vec::new(),
            unresolved: Vec::new(),
            types: Vec::new(),
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
            unit: false,
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
    fn a_consumer_that_moves_to_v2_is_not_covered_by_another_crate_on_v1() {
        let mut deployed = with_events();
        deployed.listeners.push(BundleListener {
            krate: "billing".to_owned(),
            ..listener("on_contact_billing", "contact.updated", &["id"])
        });
        let mut current = deployed.clone();
        current.events.push(event(
            2,
            contact_shape(&[("id", "i64", true), ("email_address", "String", true)]),
        ));
        // `marketing` moves to v2; `billing` stays on v1.
        current.listeners.retain(|l| l.krate != "marketing");
        current.listeners.push(listener(
            "on_contact",
            "contact.updated.v2",
            &["email_address"],
        ));
        let report = compare(&deployed, &current);
        assert_eq!(kinds(&report.caller_first), [BreakKind::EventTopicMissing]);
        assert_eq!(
            report.caller_first[0].caller.as_deref(),
            Some("marketing::on_contact")
        );
        assert_eq!(report.verdict(), Verdict::CalleeFirst);
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
    fn a_unit_event_that_gains_fields_is_a_break_in_both_orders() {
        let mut deployed = with_events();
        deployed.events[0].unit = true;
        deployed.events[0].shape = contact_shape(&[]);
        deployed.listeners = vec![listener("on_contact", "contact.updated", &[])];
        let mut current = with_events();
        current.events[0].shape = contact_shape(&[("trace", "Option<String>", true)]);
        current.listeners = vec![listener("on_contact", "contact.updated", &[])];
        let report = compare(&deployed, &current);
        assert_eq!(kinds(&report.callee_first), [BreakKind::EventUnitChanged]);
        assert_eq!(kinds(&report.caller_first), [BreakKind::EventUnitChanged]);
        assert_eq!(report.verdict(), Verdict::Incompatible);
        assert!(report.render(Order::Any).contains("JSON `null`"));
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

    // --- unit bodies, nested types, route registration ---

    /// A format-3 copy of release 1.
    fn typed_release() -> Bundle {
        Bundle {
            format: crate::contracts::bundle::IDENTITY_FORMAT,
            ..release_1()
        }
    }

    #[test]
    fn a_unit_response_that_becomes_an_object_breaks_both_windows() {
        let mut deployed = typed_release();
        ep(&mut deployed, "get_item").response = WireTypeDescriptor {
            unit: true,
            ..shape("Empty", Vec::new(), Vec::new())
        };
        let mut current = typed_release();
        ep(&mut current, "get_item").response = shape("Empty", Vec::new(), Vec::new());
        let report = compare(&deployed, &current);
        assert_eq!(
            kinds(&report.callee_first),
            [BreakKind::ResponseUnitChanged]
        );
        assert_eq!(
            kinds(&report.caller_first),
            [BreakKind::ResponseUnitChanged]
        );
        assert!(report.render(Order::Any).contains("JSON `null`"));
    }

    #[test]
    fn an_older_bundle_does_not_know_unit_so_unit_is_not_compared() {
        let deployed = release_1();
        let mut current = typed_release();
        ep(&mut current, "get_item").response.unit = true;
        ep(&mut current, "get_item").response.serialized.clear();
        let report = compare(&deployed, &current);
        assert!(!kinds(&report.callee_first).contains(&BreakKind::ResponseUnitChanged));
    }

    #[test]
    fn a_unit_request_body_that_becomes_an_object_is_a_break() {
        let mut deployed = typed_release();
        ep(&mut deployed, "create_item").request = WireTypeDescriptor {
            unit: true,
            ..shape("Empty", Vec::new(), Vec::new())
        };
        let mut current = typed_release();
        ep(&mut current, "create_item").request = shape("Empty", Vec::new(), Vec::new());
        let report = compare(&deployed, &current);
        assert!(kinds(&report.callee_first).contains(&BreakKind::RequestUnitChanged));
        assert!(kinds(&report.caller_first).contains(&BreakKind::RequestUnitChanged));
    }

    fn details(fields: Vec<WireFieldDescriptor>) -> crate::contracts::bundle::BundleType {
        crate::contracts::bundle::BundleType {
            id: "mesh-catalog::Details".to_owned(),
            shape: shape("Details", fields.clone(), fields),
        }
    }

    /// Release 1 with `Item.details: Details { size, color }`.
    fn nested_release() -> Bundle {
        let mut bundle = typed_release();
        let mut nested = field("details", "details", "Details", true);
        nested.nested = vec!["mesh-catalog::Details".to_owned()];
        for e in &mut bundle.endpoints {
            if e.name == "get_item" {
                e.response.serialized.push(nested.clone());
                e.response.deserialized.push(nested.clone());
            } else {
                e.request.serialized.push(nested.clone());
                e.request.deserialized.push(nested.clone());
            }
        }
        bundle.types = vec![details(vec![plain("color", true), plain("size", true)])];
        bundle
    }

    #[test]
    fn a_required_field_removed_from_a_nested_type_breaks_the_old_decode() {
        let deployed = nested_release();
        let mut current = nested_release();
        current.types = vec![details(vec![plain("size", true)])];
        let report = compare(&deployed, &current);
        assert!(
            kinds(&report.callee_first)
                .contains(&BreakKind::ResponseDecodeFails("details.color".to_owned())),
            "{report:?}"
        );
        assert!(
            kinds(&report.caller_first)
                .contains(&BreakKind::RequestFieldNotSent("details.color".to_owned())),
            "{report:?}"
        );
    }

    #[test]
    fn a_nested_unit_type_that_becomes_an_object_is_a_break() {
        let deployed = nested_release();
        let mut current = nested_release();
        current.types[0].shape.unit = true;
        current.types[0].shape.serialized.clear();
        current.types[0].shape.deserialized.clear();
        let report = compare(&deployed, &current);
        assert!(
            report.callee_first.iter().any(|b| matches!(
                &b.kind,
                BreakKind::ResponseTypeChanged { field, .. } if field == "details"
            )),
            "{report:?}"
        );
    }

    #[test]
    fn an_unchanged_nested_type_passes() {
        let report = compare(&nested_release(), &nested_release());
        assert_eq!(report.verdict(), Verdict::Compatible, "{report:?}");
    }

    // --- nested read-sets and write-sets ---

    /// `Details { color: Option<String>, size }`, or without `color`.
    fn optional_color(with_color: bool) -> crate::contracts::bundle::BundleType {
        let mut sent = vec![plain("size", true)];
        let mut decoded = vec![plain("size", true)];
        if with_color {
            sent.insert(0, field("color", "color", "Option<String>", true));
            decoded.insert(0, field("color", "color", "Option<String>", false));
        }
        crate::contracts::bundle::BundleType {
            id: "mesh-catalog::Details".to_owned(),
            shape: shape("Details", sent, decoded),
        }
    }

    /// [`nested_release`] with an optional `color`, and these sets.
    fn nested_sets(with_color: bool, reads: &[&str], writes: Option<&[&str]>) -> Bundle {
        let mut bundle = nested_release();
        bundle.types = vec![optional_color(with_color)];
        bundle.callers = vec![
            call("add_item", "create_item", &["id"], writes),
            call("show_item", "get_item", reads, None),
        ];
        bundle
    }

    #[test]
    fn an_optional_nested_field_a_deployed_caller_reads_removed_is_a_break() {
        let deployed = nested_sets(true, &["details", "details.color"], Some(&["name"]));
        let current = nested_sets(false, &["details"], Some(&["name"]));
        let report = compare(&deployed, &current);
        assert_eq!(
            kinds(&report.callee_first),
            [BreakKind::ResponseFieldMissing("details.color".to_owned())]
        );
        assert!(report.caller_first.is_empty(), "{report:?}");
        assert_eq!(report.verdict(), Verdict::CallerFirst);
    }

    #[test]
    fn a_new_optional_nested_field_a_new_caller_reads_needs_the_callee_first() {
        let deployed = nested_sets(false, &["details"], Some(&["name"]));
        let current = nested_sets(true, &["details", "details.color"], Some(&["name"]));
        let report = compare(&deployed, &current);
        assert!(report.callee_first.is_empty(), "{report:?}");
        assert_eq!(
            kinds(&report.caller_first),
            [BreakKind::ResponseFieldMissing("details.color".to_owned())]
        );
        assert_eq!(report.verdict(), Verdict::CalleeFirst);
    }

    #[test]
    fn a_nested_field_the_call_site_sets_and_the_callee_drops_is_a_break() {
        let deployed = nested_sets(
            true,
            &["id"],
            Some(&["details", "details.color", "details.size", "name"]),
        );
        let current = nested_sets(false, &["id"], Some(&["details", "details.size", "name"]));
        let report = compare(&deployed, &current);
        assert_eq!(
            kinds(&report.callee_first),
            [BreakKind::RequestFieldDropped("details.color".to_owned())]
        );
        assert!(report.caller_first.is_empty(), "{report:?}");
    }

    #[test]
    fn a_nested_read_without_its_prefix_still_reads_the_top_field() {
        let deployed = nested_sets(true, &["details.color"], Some(&["name"]));
        let mut current = nested_sets(true, &["id"], Some(&["name"]));
        let item = ep(&mut current, "get_item");
        for fields in [
            &mut item.response.serialized,
            &mut item.response.deserialized,
        ] {
            fields.retain(|f| f.rust_name != "details");
        }
        let report = compare(&deployed, &current);
        assert!(
            kinds(&report.callee_first)
                .contains(&BreakKind::ResponseFieldMissing("details".to_owned())),
            "{report:?}"
        );
    }

    #[test]
    fn a_nested_read_set_applies_only_to_the_field_it_is_under() {
        // `details` and `billing` share one type. The caller reads
        // `details.color` only, so a cache keyed by the type pair alone
        // would also name `billing.color`.
        let mut deployed = nested_sets(true, &["billing", "details", "details.color"], None);
        let mut current = nested_sets(false, &["billing", "details"], None);
        for bundle in [&mut deployed, &mut current] {
            let item = ep(bundle, "get_item");
            let mut billing = item.response.serialized.last().expect("details").clone();
            billing.rust_name = "billing".to_owned();
            billing.wire_name = "billing".to_owned();
            item.response.serialized.push(billing.clone());
            item.response.deserialized.push(billing);
        }
        let report = compare(&deployed, &current);
        assert_eq!(
            kinds(&report.callee_first),
            [BreakKind::ResponseFieldMissing("details.color".to_owned())]
        );
    }

    #[test]
    fn a_nested_write_set_counts_a_field_the_type_may_omit_only_when_set() {
        // `Details.code` may be left off the wire, and the callee needs it.
        let release = |writes: &[&str]| {
            let mut bundle = nested_sets(true, &["id"], Some(writes));
            let shape = &mut bundle.types[0].shape;
            shape.serialized.push(plain("code", false));
            shape.deserialized.push(plain("code", true));
            // The request only: a response decode would need `code` too.
            let item = &mut ep(&mut bundle, "get_item").response;
            for fields in [&mut item.serialized, &mut item.deserialized] {
                fields.retain(|f| f.rust_name != "details");
            }
            bundle
        };
        let set = release(&["details", "details.code", "name"]);
        assert_eq!(compare(&set, &set).verdict(), Verdict::Compatible);
        let unset = release(&["details", "details.size", "name"]);
        assert!(
            kinds(&compare(&unset, &unset).callee_first)
                .contains(&BreakKind::RequestFieldNotSent("details.code".to_owned())),
        );
        // `details: value` (not a literal): the nested write-set is unknown,
        // and the caller's own release already needed `code`.
        let unknown = release(&["details", "name"]);
        assert_eq!(compare(&unknown, &unknown).verdict(), Verdict::Compatible);
    }

    #[test]
    fn an_optional_nested_event_field_a_deployed_consumer_reads_removed_is_a_break() {
        let with = |color: bool, reads: &[&str]| {
            let mut bundle = nested_sets(color, &["id"], None);
            let mut details = field("details", "details", "Details", true);
            details.nested = vec!["mesh-catalog::Details".to_owned()];
            let mut payload = contact_shape(&[("id", "i64", true)]);
            payload.serialized.push(details.clone());
            payload.deserialized.push(details);
            bundle.events = vec![event(1, payload)];
            bundle.listeners = vec![listener("on_contact", "contact.updated", reads)];
            bundle
        };
        let deployed = with(true, &["details", "details.color"]);
        let current = with(false, &["details"]);
        let report = compare(&deployed, &current);
        assert_eq!(
            kinds(&report.callee_first),
            [BreakKind::EventFieldMissing("details.color".to_owned())]
        );
        assert!(report.caller_first.is_empty(), "{report:?}");
    }

    #[test]
    fn an_endpoint_removed_from_its_router_is_a_removed_route() {
        let deployed = typed_release();
        let mut current = typed_release();
        ep(&mut current, "get_item").unrouted = true;
        let report = compare(&deployed, &current);
        assert_eq!(kinds(&report.callee_first), [BreakKind::RouteMissing]);
        assert_eq!(kinds(&report.caller_first), [BreakKind::RouteNotServed]);
        assert_eq!(report.verdict(), Verdict::Incompatible);
        assert!(report.render(Order::Any).contains("no `routes![…]`"));
    }

    #[test]
    fn a_new_caller_of_a_new_unrouted_endpoint_breaks_both_windows() {
        let deployed = typed_release();
        let mut current = typed_release();
        let mut fresh = ep(&mut current, "get_item").clone();
        fresh.name = "get_price".to_owned();
        fresh.path = "/prices/{id}".to_owned();
        fresh.unrouted = true;
        current.endpoints.push(fresh);
        current
            .callers
            .push(call("show_price", "get_price", &["name"], None));
        let report = compare(&deployed, &current);
        assert_eq!(kinds(&report.callee_first), [BreakKind::RouteNotServed]);
        assert_eq!(kinds(&report.caller_first), [BreakKind::RouteNotServed]);
    }

    #[test]
    fn a_caller_the_deployed_release_already_left_unserved_is_not_new() {
        let mut release = typed_release();
        ep(&mut release, "get_item").unrouted = true;
        let report = compare(&release, &release);
        assert_eq!(report.verdict(), Verdict::Compatible, "{report:?}");
    }

    #[test]
    fn an_unrouted_endpoint_whose_route_another_endpoint_serves_is_served() {
        let deployed = typed_release();
        let mut current = typed_release();
        let mut v2 = ep(&mut current, "get_item").clone();
        v2.name = "get_item_v2".to_owned();
        current.endpoints.push(v2);
        ep(&mut current, "get_item").unrouted = true;
        let report = compare(&deployed, &current);
        assert_eq!(report.verdict(), Verdict::Compatible, "{report:?}");
    }

    #[test]
    fn a_field_that_gains_a_shape_on_one_side_is_a_type_change() {
        let deployed = nested_release();
        let mut current = nested_release();
        for e in &mut current.endpoints {
            for f in e
                .response
                .serialized
                .iter_mut()
                .chain(e.request.serialized.iter_mut())
            {
                f.nested.clear();
            }
        }
        let report = compare(&deployed, &current);
        assert!(
            report.callee_first.iter().any(|b| matches!(
                &b.kind,
                BreakKind::ResponseTypeChanged { field, .. } if field == "details"
            )),
            "{report:?}"
        );
    }

    #[test]
    fn two_fields_of_one_nested_type_are_both_reported() {
        let mut deployed = nested_release();
        let mut current = nested_release();
        for bundle in [&mut deployed, &mut current] {
            let item = ep(bundle, "get_item");
            let mut second = item.response.serialized.last().expect("details").clone();
            second.rust_name = "billing".to_owned();
            second.wire_name = "billing".to_owned();
            item.response.serialized.push(second.clone());
            item.response.deserialized.push(second);
        }
        current.types = vec![details(vec![plain("size", true)])];
        let report = compare(&deployed, &current);
        let fails: Vec<&BreakKind> = report
            .callee_first
            .iter()
            .map(|b| &b.kind)
            .filter(|k| matches!(k, BreakKind::ResponseDecodeFails(_)))
            .collect();
        assert!(fails.contains(&&BreakKind::ResponseDecodeFails("details.color".to_owned())));
        assert!(fails.contains(&&BreakKind::ResponseDecodeFails("billing.color".to_owned())));
    }

    #[test]
    fn a_caller_that_reads_more_of_a_route_that_was_already_unserved_is_not_new() {
        let mut deployed = typed_release();
        ep(&mut deployed, "get_item").unrouted = true;
        let mut current = deployed.clone();
        for c in &mut current.callers {
            if c.endpoint == "get_item" {
                c.reads.push("id".to_owned());
            }
        }
        assert_eq!(compare(&deployed, &current).verdict(), Verdict::Compatible);
    }

    #[test]
    fn a_type_that_many_fields_share_at_many_levels_is_compared_once_per_pair() {
        // Eight fields of the next level's type, eight levels deep: a walk
        // without a memo takes 8^8 steps.
        let mut bundle = typed_release();
        let level = |n: usize| format!("mesh-catalog::L{n}");
        for n in 0..8 {
            let fields: Vec<WireFieldDescriptor> = (0..8)
                .map(|i| {
                    let mut f = field(
                        &format!("f{i}"),
                        &format!("f{i}"),
                        &format!("L{}", n + 1),
                        true,
                    );
                    f.nested = vec![level(n + 1)];
                    f
                })
                .collect();
            bundle.types.push(crate::contracts::bundle::BundleType {
                id: level(n),
                shape: shape(&format!("L{n}"), fields.clone(), fields),
            });
        }
        bundle.types.push(crate::contracts::bundle::BundleType {
            id: level(8),
            shape: shape("L8", vec![plain("x", true)], vec![plain("x", true)]),
        });
        bundle.types.sort();
        let mut root = field("root", "root", "L0", true);
        root.nested = vec![level(0)];
        let item = ep(&mut bundle, "get_item");
        item.response.serialized.push(root.clone());
        item.response.deserialized.push(root);
        let mut current = bundle.clone();
        let last = current
            .types
            .iter_mut()
            .find(|t| t.id == level(8))
            .expect("L8");
        last.shape.serialized.clear();
        let started = std::time::Instant::now();
        let report = compare(&bundle, &current);
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        assert!(
            kinds(&report.callee_first).contains(&BreakKind::ResponseDecodeFails(
                "root.f0.f0.f0.f0.f0.f0.f0.f0.x".to_owned()
            )),
            "{report:?}"
        );
    }

    #[test]
    fn an_unrouted_endpoint_with_no_caller_is_a_removed_route() {
        let mut deployed = typed_release();
        deployed.callers.retain(|c| c.endpoint != "get_item");
        let mut current = deployed.clone();
        ep(&mut current, "get_item").unrouted = true;
        let report = compare(&deployed, &current);
        assert_eq!(kinds(&report.callee_first), [BreakKind::RouteRemoved]);
    }

    #[test]
    fn a_route_the_deployed_release_did_not_serve_is_not_removed() {
        let mut deployed = typed_release();
        deployed.callers.retain(|c| c.endpoint != "get_item");
        ep(&mut deployed, "get_item").unrouted = true;
        let current = deployed.clone();
        assert_eq!(compare(&deployed, &current).verdict(), Verdict::Compatible);
    }

    // --- wire types ---

    /// A wire type with the same JSON form in both directions.
    fn set_wire(fields: &mut [WireFieldDescriptor], name: &str, wire: &WireType) {
        for f in fields.iter_mut().filter(|f| f.rust_name == name) {
            f.wire_type = Some(wire.clone());
        }
    }

    /// Release 1 with `Item.created_at: DateTime<Utc>` in `form`.
    fn dated_release(form: &WireType) -> Bundle {
        let mut bundle = Bundle {
            format: crate::contracts::bundle::WIRE_TYPE_FORMAT,
            ..release_1()
        };
        for e in &mut bundle.endpoints {
            for side in [&mut e.request, &mut e.response] {
                if side.name == "NoBody" {
                    continue;
                }
                let at = field("created_at", "created_at", "DateTime<Utc>", true);
                side.serialized.push(at.clone());
                side.deserialized.push(at);
                set_wire(&mut side.serialized, "created_at", form);
                set_wire(&mut side.deserialized, "created_at", form);
            }
        }
        bundle
    }

    fn unix_seconds() -> WireType {
        WireType::Integer {
            bits: 64,
            signed: true,
            format: Some(autumn_macros_support::wire::format::UNIX_SECONDS.to_owned()),
        }
    }

    fn rfc3339() -> WireType {
        WireType::string_in(autumn_macros_support::wire::format::DATE_TIME)
    }

    #[test]
    fn a_new_json_form_with_the_same_rust_type_is_a_break_in_both_windows() {
        let deployed = dated_release(&rfc3339());
        let current = dated_release(&unix_seconds());
        let report = compare(&deployed, &current);
        let response = BreakKind::ResponseTypeChanged {
            field: "created_at".to_owned(),
            caller: "DateTime<Utc> as string(date-time)".to_owned(),
            callee: "DateTime<Utc> as integer(i64, unix-seconds)".to_owned(),
        };
        assert!(
            kinds(&report.callee_first).contains(&response),
            "{report:?}"
        );
        let request = BreakKind::RequestTypeChanged {
            field: "created_at".to_owned(),
            caller: "DateTime<Utc> as integer(i64, unix-seconds)".to_owned(),
            callee: "DateTime<Utc> as string(date-time)".to_owned(),
        };
        assert!(kinds(&report.caller_first).contains(&request), "{report:?}");
        assert_eq!(report.verdict(), Verdict::Incompatible);
        let text = report.render(Order::Any);
        assert!(
            text.contains("`DateTime<Utc> as integer(i64, unix-seconds)`"),
            "{text}"
        );
    }

    #[test]
    fn the_same_json_form_is_compatible() {
        let report = compare(&dated_release(&rfc3339()), &dated_release(&rfc3339()));
        assert_eq!(report, Report::default());
    }

    #[test]
    fn a_bundle_without_wire_types_compares_the_rust_type_only() {
        let mut deployed = dated_release(&rfc3339());
        deployed.format = crate::contracts::bundle::IDENTITY_FORMAT;
        for e in &mut deployed.endpoints {
            for f in e
                .request
                .serialized
                .iter_mut()
                .chain(e.request.deserialized.iter_mut())
                .chain(e.response.serialized.iter_mut())
                .chain(e.response.deserialized.iter_mut())
            {
                f.wire_type = None;
            }
        }
        let report = compare(&deployed, &dated_release(&unix_seconds()));
        assert_eq!(report, Report::default());
    }

    #[test]
    fn a_rust_type_change_names_the_rust_types_only() {
        let deployed = dated_release(&rfc3339());
        let mut current = dated_release(&unix_seconds());
        for e in &mut current.endpoints {
            for f in e
                .response
                .serialized
                .iter_mut()
                .chain(e.response.deserialized.iter_mut())
                .filter(|f| f.rust_name == "created_at")
            {
                f.ty = "i64".to_owned();
            }
        }
        let report = compare(&deployed, &current);
        assert!(
            kinds(&report.callee_first).contains(&BreakKind::ResponseTypeChanged {
                field: "created_at".to_owned(),
                caller: "DateTime<Utc>".to_owned(),
                callee: "i64".to_owned(),
            }),
            "{report:?}"
        );
    }

    #[test]
    fn a_decoder_of_a_nullable_form_reads_the_plain_form() {
        // `type When = Option<DateTime<Utc>>` with `#[wire(type = "string",
        // format = "date-time", nullable)]` on the caller side only.
        let mut deployed = dated_release(&rfc3339());
        let current = dated_release(&rfc3339());
        for e in &mut deployed.endpoints {
            set_wire(
                &mut e.response.deserialized,
                "created_at",
                &WireType::nullable(rfc3339()),
            );
        }
        let report = compare(&deployed, &current);
        assert!(
            !report.callee_first.iter().any(|b| matches!(
                &b.kind,
                BreakKind::ResponseTypeChanged { field, .. } if field == "created_at"
            )),
            "{report:?}"
        );
    }

    #[test]
    fn an_event_field_with_a_new_json_form_is_a_break() {
        let mut deployed = with_events();
        deployed.format = crate::contracts::bundle::WIRE_TYPE_FORMAT;
        let mut current = deployed.clone();
        let text = WireType::string();
        let number = WireType::integer(64, true);
        set_wire(&mut deployed.events[0].shape.serialized, "email", &text);
        set_wire(&mut deployed.events[0].shape.deserialized, "email", &text);
        set_wire(&mut current.events[0].shape.serialized, "email", &number);
        set_wire(&mut current.events[0].shape.deserialized, "email", &number);
        let report = compare(&deployed, &current);
        assert_eq!(
            kinds(&report.callee_first),
            [BreakKind::EventTypeChanged {
                field: "email".to_owned(),
                consumer: "String as string".to_owned(),
                producer: "String as integer(i64)".to_owned(),
            }]
        );
    }

    #[test]
    fn an_unchanged_release_with_a_one_sided_custom_function_is_compatible() {
        // `serialize_with` writes `any`. The other side reads the mapped form.
        let mut release = dated_release(&rfc3339());
        for e in &mut release.endpoints {
            for side in [&mut e.request, &mut e.response] {
                set_wire(&mut side.serialized, "created_at", &WireType::Any);
            }
        }
        assert_eq!(compare(&release, &release), Report::default());
    }

    #[test]
    fn any_on_one_side_is_not_compared() {
        // A custom serde function with no override makes its form `any`.
        let deployed = dated_release(&rfc3339());
        let current = dated_release(&WireType::Any);
        assert_eq!(compare(&deployed, &current), Report::default());
    }

    #[test]
    fn a_new_json_form_inside_a_nested_type_is_a_break() {
        let mut deployed = nested_release();
        deployed.format = crate::contracts::bundle::WIRE_TYPE_FORMAT;
        let mut current = deployed.clone();
        for (bundle, form) in [
            (&mut deployed, WireType::string()),
            (&mut current, WireType::integer(32, false)),
        ] {
            let shape = &mut bundle.types[0].shape;
            set_wire(&mut shape.serialized, "size", &form);
            set_wire(&mut shape.deserialized, "size", &form);
        }
        let report = compare(&deployed, &current);
        assert!(
            report.callee_first.iter().any(|b| matches!(
                &b.kind,
                BreakKind::ResponseTypeChanged { field, .. } if field == "details.size"
            )),
            "{report:?}"
        );
    }
}
