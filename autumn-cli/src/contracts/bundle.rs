//! The release bundle: every wire descriptor of one build, joined and sorted.
//!
//! The wire macros write one JSON file per endpoint, type, client, checked
//! caller, event and event consumer. [`Bundle::from_dir`] joins them. An
//! endpoint carries its request and response shapes inline. A caller call
//! names the endpoint it reaches by `service` and `endpoint`. An event carries
//! its payload shape inline, and a consumer names the event by its topic. The
//! bytes are the same on every rebuild: all lists are sorted, and no path or
//! time goes in.

use std::collections::BTreeSet;
use std::path::Path;

use autumn_macros_support::wire::{
    CallerCall, CallerDescriptor, ClientDescriptor, EndpointDescriptor, EventDescriptor,
    ListenerDescriptor, TypeDescriptor, WireTypeDescriptor,
};
use serde::{Deserialize, Serialize};

/// The bundle format of a bundle with no events. Its bytes are the same as
/// before events existed.
pub const BUNDLE_FORMAT: u32 = 1;

/// The bundle format of a bundle with events. An older CLI refuses it rather
/// than skip the events it cannot read.
pub const EVENTS_FORMAT: u32 = 2;

/// The type name of an endpoint with no request body.
pub const NO_BODY: &str = "NoBody";

/// The wire contract of one release.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bundle {
    /// The bundle format. A reader refuses a format it does not know.
    pub format: u32,
    /// Every endpoint, sorted by service, then name.
    pub endpoints: Vec<BundleEndpoint>,
    /// Every checked call, sorted.
    pub callers: Vec<BundleCall>,
    /// Every `#[event]`, sorted by topic. Left out of the bytes when empty,
    /// so a bundle with no events is the same as before events existed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<BundleEvent>,
    /// Every event consumer, sorted. Left out of the bytes when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub listeners: Vec<BundleListener>,
    /// Consumers the bundle has no topic for, as `krate::function consumes
    /// Path: reason`, sorted. The check cannot protect them, so it says so.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unresolved: Vec<String>,
}

/// One event with its payload shape inline.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct BundleEvent {
    /// The routing topic: `name`, plus `.vN` above version 1.
    pub topic: String,
    /// The topic without its version suffix.
    pub name: String,
    /// The schema version.
    pub version: u32,
    /// The crate that defines the event.
    pub krate: String,
    /// The event type's Rust name.
    pub type_name: String,
    /// `#[event(opaque)]`: the payload has no shape to check.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub opaque: bool,
    /// A unit struct: the payload is JSON `null`, not an object.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub unit: bool,
    /// The payload's shape.
    pub shape: WireTypeDescriptor,
}

/// One consumer of an event: a `#[listener]` or an `#[event_consumer]`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct BundleListener {
    /// The crate that holds the consumer.
    pub krate: String,
    /// The consumer function.
    pub function: String,
    /// The topic of the event it consumes.
    pub topic: String,
    /// Payload fields the consumer reads, by Rust name.
    pub reads: Vec<String>,
}

impl BundleListener {
    /// `krate::function`.
    #[must_use]
    pub fn consumer(&self) -> String {
        format!("{}::{}", self.krate, self.function)
    }
}

/// One endpoint with its shapes inline.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct BundleEndpoint {
    /// The service, from `#[endpoint(service = "…")]`.
    pub service: String,
    /// The endpoint name.
    pub name: String,
    /// The crate that defines the endpoint.
    pub krate: String,
    /// HTTP method, uppercase.
    pub method: String,
    /// Route path, with `{param}` placeholders.
    pub path: String,
    /// Whether the handler reads a JSON request body.
    pub has_body: bool,
    /// Whether the request body is a JSON array of `request`.
    pub request_list: bool,
    /// Whether the response body is a JSON array of `response`.
    pub response_list: bool,
    /// What the endpoint accepts.
    pub request: WireTypeDescriptor,
    /// What the endpoint produces.
    pub response: WireTypeDescriptor,
}

impl BundleEndpoint {
    /// `service.name`.
    #[must_use]
    pub fn id(&self) -> String {
        format!("{}.{}", self.service, self.name)
    }

    /// `METHOD /path`.
    #[must_use]
    pub fn route(&self) -> String {
        format!("{} {}", self.method, self.path)
    }
}

/// One call from a `#[contract_checked]` function to an endpoint.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct BundleCall {
    /// The crate that holds the caller.
    pub krate: String,
    /// The calling function.
    pub function: String,
    /// The service called.
    pub service: String,
    /// The endpoint called.
    pub endpoint: String,
    /// Response fields the caller reads, by Rust name.
    pub reads: Vec<String>,
    /// Request fields the call site sets, by Rust name. `None` when unknown.
    pub writes: Option<Vec<String>>,
}

impl BundleCall {
    /// `krate::function`.
    #[must_use]
    pub fn caller(&self) -> String {
        format!("{}::{}", self.krate, self.function)
    }
}

impl Bundle {
    /// The endpoint `service.name`, if the bundle has it.
    #[must_use]
    pub fn endpoint(&self, service: &str, name: &str) -> Option<&BundleEndpoint> {
        self.endpoints
            .iter()
            .find(|e| e.service == service && e.name == name)
    }

    /// The event with this topic, if the bundle has it.
    #[must_use]
    pub fn event(&self, topic: &str) -> Option<&BundleEvent> {
        self.events.iter().find(|e| e.topic == topic)
    }

    /// Parse a bundle and refuse a format this CLI does not know.
    ///
    /// # Errors
    /// The bytes are not a bundle, or the format is not [`BUNDLE_FORMAT`].
    pub fn from_slice(bytes: &[u8]) -> Result<Self, String> {
        let bundle: Self =
            serde_json::from_slice(bytes).map_err(|e| format!("not a contract bundle: {e}"))?;
        if !(BUNDLE_FORMAT..=EVENTS_FORMAT).contains(&bundle.format) {
            return Err(format!(
                "contract bundle format {} is not supported; this CLI reads formats \
                 {BUNDLE_FORMAT} to {EVENTS_FORMAT}",
                bundle.format
            ));
        }
        Ok(bundle)
    }

    /// The canonical bytes: pretty JSON and a final newline.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        // Serializing plain structs, strings, and vectors cannot fail.
        let mut bytes = serde_json::to_vec_pretty(self).unwrap_or_default();
        bytes.push(b'\n');
        bytes
    }

    /// Join the descriptors in `dir` into a bundle.
    ///
    /// # Errors
    /// A descriptor does not parse, a name resolves to zero or two targets,
    /// or the directory holds no endpoint.
    pub fn from_dir(dir: &Path) -> Result<Self, String> {
        let endpoints: Vec<EndpointDescriptor> = read_all(dir, "endpoint.")?;
        let types: Vec<TypeDescriptor> = read_all(dir, "type.")?;
        let clients: Vec<ClientDescriptor> = read_all(dir, "client.")?;
        let callers: Vec<CallerDescriptor> = read_all(dir, "caller.")?;
        let events: Vec<EventDescriptor> = read_all(dir, "event.")?;
        let listeners: Vec<ListenerDescriptor> = read_all(dir, "listener.")?;
        if endpoints.is_empty() && events.is_empty() {
            return Err(format!(
                "no endpoint or event descriptor in {}: no crate in the pass has an \
                 `#[endpoint]` or an `#[event]`",
                dir.display()
            ));
        }

        let mut seen = BTreeSet::new();
        let mut joined = Vec::with_capacity(endpoints.len());
        for endpoint in &endpoints {
            if !seen.insert((endpoint.service.clone(), endpoint.name.clone())) {
                return Err(format!(
                    "two endpoints are named `{}`; give one a different name",
                    endpoint.id()
                ));
            }
            joined.push(BundleEndpoint {
                service: endpoint.service.clone(),
                name: endpoint.name.clone(),
                krate: endpoint.krate.clone(),
                method: endpoint.method.clone(),
                path: endpoint.path.clone(),
                // A descriptor without the flag predates it: infer it.
                has_body: endpoint.has_body || endpoint.request_type != NO_BODY,
                request_list: endpoint.request_list,
                response_list: endpoint.response_list,
                request: resolve_type(&endpoint.request_type, endpoint, &types)?,
                response: resolve_type(&endpoint.response_type, endpoint, &types)?,
            });
        }
        joined.sort();

        let mut calls = BTreeSet::new();
        for caller in &callers {
            for call in &caller.calls {
                let Some(target) = resolve_call(caller, call, &clients, &endpoints)? else {
                    continue;
                };
                calls.insert(BundleCall {
                    krate: caller.krate.clone(),
                    function: caller.function.clone(),
                    service: target.service.clone(),
                    endpoint: target.name.clone(),
                    reads: sorted(call.reads.clone()),
                    writes: call.writes.clone().map(sorted),
                });
            }
        }

        let joined_events = join_events(&events)?;
        let (joined_listeners, unresolved) = join_listeners(&listeners, &events)?;
        let format =
            if joined_events.is_empty() && joined_listeners.is_empty() && unresolved.is_empty() {
                BUNDLE_FORMAT
            } else {
                EVENTS_FORMAT
            };
        Ok(Self {
            format,
            endpoints: joined,
            callers: calls.into_iter().collect(),
            events: joined_events,
            listeners: joined_listeners,
            unresolved,
        })
    }
}

/// Every event with a `name`, keyed by topic. Two events with one topic are
/// an error: the bus cannot tell their payloads apart.
///
/// An event without `name` routes on its module path, which no descriptor
/// knows. It is left out, and so are its consumers.
fn join_events(events: &[EventDescriptor]) -> Result<Vec<BundleEvent>, String> {
    let mut joined: Vec<BundleEvent> = Vec::with_capacity(events.len());
    for event in events {
        let (Some(topic), Some(name)) = (event.topic(), event.base_name()) else {
            continue;
        };
        if let Some(other) = joined.iter().find(|e| e.topic == topic) {
            return Err(format!(
                "events `{}::{}` and `{}::{}` both have topic `{topic}`; give one another \
                 name or version",
                other.krate, other.type_name, event.krate, event.type_name
            ));
        }
        joined.push(BundleEvent {
            topic,
            name: name.to_owned(),
            version: event.version,
            krate: event.krate.clone(),
            type_name: event.type_name.clone(),
            opaque: event.opaque,
            unit: event.unit,
            shape: canonical(event.shape.clone()),
        });
    }
    joined.sort();
    Ok(joined)
}

/// Every consumer, with the topic of the event it reads, and the consumers
/// with no topic.
///
/// A consumer has no topic when no descriptor has its event's type name (a
/// hand-written `impl Event`, a `use … as` alias, or a crate outside the
/// pass), or when the event has no `name`.
fn join_listeners(
    listeners: &[ListenerDescriptor],
    events: &[EventDescriptor],
) -> Result<(Vec<BundleListener>, Vec<String>), String> {
    let mut joined = BTreeSet::new();
    let mut unresolved = BTreeSet::new();
    for listener in listeners {
        let event = resolve_event(listener, events)?;
        let Some(topic) = event.and_then(EventDescriptor::topic) else {
            let reason = if event.is_some() {
                "the event has no `name`"
            } else {
                "no `#[event]` in the pass has that type name"
            };
            unresolved.insert(format!(
                "`{}::{}` consumes `{}`: {reason}",
                listener.krate, listener.function, listener.event
            ));
            continue;
        };
        joined.insert(BundleListener {
            krate: listener.krate.clone(),
            function: listener.function.clone(),
            topic,
            reads: sorted(listener.reads.clone()),
        });
    }
    Ok((
        joined.into_iter().collect(),
        unresolved.into_iter().collect(),
    ))
}

/// The event a consumer reads, by its type path. `None` when no descriptor
/// has that type name.
///
/// A crate-qualified path (`core::ContactUpdated`) narrows the match to that
/// crate. Otherwise, or when no crate has that name, an event in the
/// consumer's own crate wins, and else the name must be unique.
fn resolve_event<'a>(
    listener: &ListenerDescriptor,
    events: &'a [EventDescriptor],
) -> Result<Option<&'a EventDescriptor>, String> {
    let segments: Vec<&str> = listener.event.split("::").collect();
    let name = segments.last().copied().unwrap_or_default();
    let named: Vec<&EventDescriptor> = events.iter().filter(|e| e.type_name == name).collect();
    let in_crate = |krate: &str| -> Vec<&EventDescriptor> {
        named
            .iter()
            .copied()
            .filter(|e| e.krate.replace('-', "_") == krate)
            .collect()
    };
    let own = listener.krate.replace('-', "_");
    let at = format!(
        "`{}::{}` consumes `{}`",
        listener.krate, listener.function, listener.event
    );
    // A first segment that names no crate in the pass is a local module
    // (`events::Created`) or a renamed dependency (`contracts::Created`). The
    // path cannot tell them apart. With an event of that name both in the
    // consumer's own crate and in another crate, it refuses to guess.
    let unknown_prefix = match segments.as_slice() {
        [first, _, ..] if !["crate", "self", "super"].contains(first) => {
            let prefixed = in_crate(first);
            if !prefixed.is_empty() {
                return pick(&prefixed, &at, name).map(Some);
            }
            true
        }
        _ => false,
    };
    let preferred = in_crate(&own);
    if unknown_prefix
        && !preferred.is_empty()
        && named.iter().any(|e| e.krate.replace('-', "_") != own)
    {
        return Err(format!(
            "{at}, and `{}` names no crate in the pass, so the event is ambiguous; write \
             `crate::…` for an event in this crate, or the package name of the dependency",
            segments[0]
        ));
    }
    match (preferred.as_slice(), named.as_slice()) {
        (_, []) => Ok(None),
        ([], [one]) => Ok(Some(*one)),
        ([], _) => Err(format!(
            "{at}, and more than one crate defines an `#[event]` named `{name}`; write the \
             crate in the path"
        )),
        (preferred, _) => pick(preferred, &at, name).map(Some),
    }
}

/// The one event among `candidates`, all from one crate.
fn pick<'a>(
    candidates: &[&'a EventDescriptor],
    at: &str,
    name: &str,
) -> Result<&'a EventDescriptor, String> {
    match candidates {
        [one] => Ok(*one),
        _ => Err(format!(
            "{at}, and crate `{}` has more than one `#[event]` named `{name}`; give each \
             another type name, for example `{name}V2`",
            candidates.first().map_or("", |e| e.krate.as_str())
        )),
    }
}

/// Sort and dedup a list of names.
fn sorted(mut names: Vec<String>) -> Vec<String> {
    names.sort();
    names.dedup();
    names
}

/// Sort a shape's fields and aliases, so a reorder in source does not change
/// the bytes. Field order is not part of a JSON object.
fn canonical(mut shape: WireTypeDescriptor) -> WireTypeDescriptor {
    for field in shape
        .serialized
        .iter_mut()
        .chain(shape.deserialized.iter_mut())
    {
        field.aliases.sort();
        field.aliases.dedup();
    }
    shape.serialized.sort();
    shape.deserialized.sort();
    shape
}

/// The shape of type `name`, as the endpoint's crate sees it.
///
/// A type in the endpoint's own crate wins. Otherwise exactly one crate must
/// define it. `NoBody` has no fields.
fn resolve_type(
    name: &str,
    endpoint: &EndpointDescriptor,
    types: &[TypeDescriptor],
) -> Result<WireTypeDescriptor, String> {
    if name == NO_BODY {
        return Ok(WireTypeDescriptor {
            name: NO_BODY.to_owned(),
            ..WireTypeDescriptor::default()
        });
    }
    let named: Vec<&TypeDescriptor> = types.iter().filter(|t| t.shape.name == name).collect();
    let own = named.iter().find(|t| t.krate == endpoint.krate);
    let found = match (own, named.as_slice()) {
        (Some(own), _) => *own,
        (None, [one]) => *one,
        (None, []) => {
            return Err(format!(
                "endpoint `{}` uses type `{name}`, which has no descriptor; derive `WireShape` \
                 on it",
                endpoint.id()
            ));
        }
        (None, _) => {
            return Err(format!(
                "endpoint `{}` uses type `{name}`, and more than one crate defines a type of \
                 that name",
                endpoint.id()
            ));
        }
    };
    Ok(canonical(found.shape.clone()))
}

/// The client a call goes through.
///
/// A crate-qualified path (`shared::CatalogClient`) narrows the match to that
/// crate. Otherwise a client in the caller's own crate wins, and else the
/// name must be unique across the pass.
fn resolve_client<'a>(
    caller: &CallerDescriptor,
    call: &CallerCall,
    clients: &'a [ClientDescriptor],
) -> Result<&'a ClientDescriptor, String> {
    let segments: Vec<&str> = call.client.split("::").collect();
    let name = segments.last().copied().unwrap_or_default();
    let named: Vec<&ClientDescriptor> = clients.iter().filter(|c| c.name == name).collect();
    let hint = match segments.as_slice() {
        [first, _, ..] if !["crate", "self", "super"].contains(first) => (*first).to_owned(),
        _ => caller.krate.replace('-', "_"),
    };
    let preferred: Vec<&ClientDescriptor> = named
        .iter()
        .copied()
        .filter(|c| c.krate.replace('-', "_") == hint)
        .collect();
    match (preferred.as_slice(), named.as_slice()) {
        ([one], _) | ([], [one]) => Ok(*one),
        (_, []) => Err(format!(
            "`{}`, and no crate in the pass has a `wire_client!` of that name",
            call.client
        )),
        _ => Err(format!(
            "`{}`, and more than one `wire_client!` has that name",
            call.client
        )),
    }
}

/// The endpoint one caller call reaches: caller -> client -> marker ->
/// endpoint. `None` for an extension method, which makes no wire call.
fn resolve_call<'a>(
    caller: &CallerDescriptor,
    call: &CallerCall,
    clients: &[ClientDescriptor],
    endpoints: &'a [EndpointDescriptor],
) -> Result<Option<&'a EndpointDescriptor>, String> {
    let at = format!(
        "`{}::{}` calls `{}`",
        caller.krate, caller.function, call.method
    );
    let client = resolve_client(caller, call, clients).map_err(|e| format!("{at} through {e}"))?;
    // A method that is not one of the client's endpoints is an extension
    // method. It makes no wire call, so it has no contract to record.
    let Some(marker) = client
        .endpoints
        .iter()
        .find(|e| e.method == call.method)
        .map(|e| e.marker.as_str())
    else {
        return Ok(None);
    };
    let segments: Vec<&str> = marker.split("::").collect();
    let ident = segments.last().copied().unwrap_or_default();
    // A path that starts with a crate name narrows the match to that crate.
    let crate_hint = match segments.as_slice() {
        [first, _, ..] if !["crate", "self", "super"].contains(first) => Some((*first).to_owned()),
        // `crate::…` is relative to the crate that declares the client.
        [_, _, ..] => Some(client.krate.replace('-', "_")),
        _ => None,
    };
    let candidates: Vec<&EndpointDescriptor> = endpoints
        .iter()
        .filter(|e| e.endpoint_ident == ident)
        .collect();
    let by_crate: Vec<&EndpointDescriptor> = candidates
        .iter()
        .copied()
        .filter(|e| crate_hint.as_deref() == Some(e.krate.replace('-', "_").as_str()))
        .collect();
    match (by_crate.as_slice(), candidates.as_slice()) {
        ([one], _) | ([], [one]) => Ok(Some(one)),
        (_, []) => Err(format!(
            "{at} (`{marker}`), and no `#[endpoint]` in the pass defines `{ident}`"
        )),
        _ => Err(format!(
            "{at} (`{marker}`), and more than one crate defines `{ident}`"
        )),
    }
}

/// Parse every `<prefix>*.json` file in `dir`, in file-name order.
fn read_all<T: serde::de::DeserializeOwned>(dir: &Path, prefix: &str) -> Result<Vec<T>, String> {
    let entries = std::fs::read_dir(dir).map_err(|e| format!("read {}: {e}", dir.display()))?;
    let mut paths: Vec<_> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(prefix))
                && p.extension().is_some_and(|e| e == "json")
        })
        .collect();
    paths.sort();
    paths
        .iter()
        .map(|path| {
            let bytes = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
            serde_json::from_slice(&bytes).map_err(|e| format!("parse {}: {e}", path.display()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use autumn_macros_support::wire::{
        CallerCall, ClientEndpoint, EventDescriptor, ListenerDescriptor, WireFieldDescriptor,
    };

    fn field(name: &str, required: bool) -> WireFieldDescriptor {
        WireFieldDescriptor {
            rust_name: name.to_owned(),
            wire_name: name.to_owned(),
            ty: "String".to_owned(),
            required,
            aliases: Vec::new(),
        }
    }

    fn write(dir: &Path, name: &str, value: &impl Serialize) {
        std::fs::write(
            dir.join(name),
            serde_json::to_vec_pretty(value).expect("serializes"),
        )
        .expect("writes");
    }

    fn endpoint(name: &str, path: &str, request: &str) -> EndpointDescriptor {
        EndpointDescriptor {
            service: "catalog".to_owned(),
            name: name.to_owned(),
            endpoint_ident: format!("{name}_endpoint"),
            krate: "mesh-catalog".to_owned(),
            method: "GET".to_owned(),
            path: path.to_owned(),
            request_type: request.to_owned(),
            response_type: "Item".to_owned(),
            has_body: request != "NoBody",
            ..EndpointDescriptor::default()
        }
    }

    fn item() -> TypeDescriptor {
        TypeDescriptor {
            krate: "mesh-catalog".to_owned(),
            shape: WireTypeDescriptor {
                name: "Item".to_owned(),
                serialized: vec![field("name", true), field("id", true)],
                deserialized: vec![field("name", true), field("id", true)],
                closed: false,
            },
        }
    }

    fn client() -> ClientDescriptor {
        ClientDescriptor {
            krate: "mesh-storefront".to_owned(),
            name: "CatalogClient".to_owned(),
            endpoints: vec![ClientEndpoint {
                method: "get_item".to_owned(),
                marker: "mesh_catalog::get_item_endpoint".to_owned(),
            }],
        }
    }

    fn caller(function: &str, reads: &[&str]) -> CallerDescriptor {
        CallerDescriptor {
            krate: "mesh-storefront".to_owned(),
            function: function.to_owned(),
            calls: vec![CallerCall {
                client: "CatalogClient".to_owned(),
                method: "get_item".to_owned(),
                reads: reads.iter().map(|r| (*r).to_owned()).collect(),
                writes: None,
            }],
        }
    }

    /// A descriptor directory for one callee and one caller.
    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let d = dir.path();
        write(
            d,
            "endpoint.mesh-catalog.catalog.get_item.json",
            &endpoint("get_item", "/items/{id}", "NoBody"),
        );
        write(d, "type.mesh-catalog.Item.json", &item());
        write(d, "client.mesh-storefront.CatalogClient.json", &client());
        write(
            d,
            "caller.mesh-storefront.show_item.1.json",
            &caller("show_item", &["name"]),
        );
        dir
    }

    #[test]
    fn the_bundle_joins_endpoints_types_and_callers() {
        let dir = fixture();
        let bundle = Bundle::from_dir(dir.path()).expect("bundle");
        assert_eq!(bundle.format, BUNDLE_FORMAT);
        let [ep] = bundle.endpoints.as_slice() else {
            panic!("one endpoint: {bundle:?}");
        };
        assert_eq!(ep.id(), "catalog.get_item");
        assert_eq!(ep.request.name, "NoBody");
        assert_eq!(ep.response.name, "Item");
        let [call] = bundle.callers.as_slice() else {
            panic!("one call: {bundle:?}");
        };
        assert_eq!(call.caller(), "mesh-storefront::show_item");
        assert_eq!(
            (call.service.as_str(), call.endpoint.as_str()),
            ("catalog", "get_item")
        );
        assert_eq!(call.reads, ["name"]);
    }

    #[test]
    fn fields_are_sorted_so_a_reorder_does_not_change_the_bytes() {
        let dir = fixture();
        let bundle = Bundle::from_dir(dir.path()).expect("bundle");
        let names: Vec<&str> = bundle.endpoints[0]
            .response
            .serialized
            .iter()
            .map(|f| f.rust_name.as_str())
            .collect();
        assert_eq!(names, ["id", "name"]);
    }

    #[test]
    fn the_bytes_are_the_same_whatever_order_the_files_were_written_in() {
        let first = fixture();
        let second = tempfile::tempdir().expect("tempdir");
        let d = second.path();
        // Same content, written in the opposite order, under other file names.
        write(
            d,
            "caller.mesh-storefront.show_item.zz.json",
            &caller("show_item", &["name"]),
        );
        write(d, "client.mesh-storefront.CatalogClient.json", &client());
        let mut reordered = item();
        reordered.shape.serialized.reverse();
        reordered.shape.deserialized.reverse();
        write(d, "type.mesh-catalog.Item.json", &reordered);
        write(
            d,
            "endpoint.mesh-catalog.catalog.get_item.json",
            &endpoint("get_item", "/items/{id}", "NoBody"),
        );
        let a = Bundle::from_dir(first.path()).expect("bundle").to_bytes();
        let b = Bundle::from_dir(d).expect("bundle").to_bytes();
        assert!(!a.is_empty());
        assert_eq!(a, b);
        assert!(a.ends_with(b"\n"));
    }

    #[test]
    fn the_bytes_round_trip() {
        let dir = fixture();
        let bundle = Bundle::from_dir(dir.path()).expect("bundle");
        let back = Bundle::from_slice(&bundle.to_bytes()).expect("parses");
        assert_eq!(back, bundle);
    }

    #[test]
    fn an_unknown_format_is_refused() {
        let err = Bundle::from_slice(br#"{"format": 99, "endpoints": [], "callers": []}"#)
            .expect_err("refused");
        Bundle::from_slice(br#"{"format": 2, "endpoints": [], "callers": []}"#)
            .expect("the events format parses");
        assert!(err.contains("format 99"), "{err}");
    }

    #[test]
    fn an_empty_directory_is_an_error_not_an_empty_contract() {
        let dir = tempfile::tempdir().expect("tempdir");
        let err = Bundle::from_dir(dir.path()).expect_err("refused");
        assert!(err.contains("no endpoint or event descriptor"), "{err}");
    }

    #[test]
    fn a_response_type_with_no_descriptor_is_an_error() {
        let dir = fixture();
        std::fs::remove_file(dir.path().join("type.mesh-catalog.Item.json")).expect("rm");
        let err = Bundle::from_dir(dir.path()).expect_err("refused");
        assert!(err.contains("`Item`"), "{err}");
    }

    #[test]
    fn a_caller_whose_endpoint_is_missing_is_an_error() {
        let dir = fixture();
        std::fs::remove_file(
            dir.path()
                .join("endpoint.mesh-catalog.catalog.get_item.json"),
        )
        .expect("rm");
        write(
            dir.path(),
            "endpoint.mesh-catalog.catalog.list.json",
            &endpoint("list", "/items", "NoBody"),
        );
        let err = Bundle::from_dir(dir.path()).expect_err("refused");
        assert!(err.contains("get_item_endpoint"), "{err}");
    }

    #[test]
    fn a_caller_whose_client_is_missing_is_an_error() {
        let dir = fixture();
        std::fs::remove_file(dir.path().join("client.mesh-storefront.CatalogClient.json"))
            .expect("rm");
        let err = Bundle::from_dir(dir.path()).expect_err("refused");
        assert!(err.contains("CatalogClient"), "{err}");
    }

    #[test]
    fn a_crate_qualified_marker_picks_its_crates_endpoint() {
        let dir = fixture();
        let mut other = endpoint("get_item", "/other/{id}", "NoBody");
        other.krate = "warehouse".to_owned();
        other.service = "warehouse".to_owned();
        write(
            dir.path(),
            "endpoint.warehouse.warehouse.get_item.json",
            &other,
        );
        let mut other_item = item();
        other_item.krate = "warehouse".to_owned();
        write(dir.path(), "type.warehouse.Item.json", &other_item);
        let bundle = Bundle::from_dir(dir.path()).expect("bundle");
        assert_eq!(bundle.callers[0].service, "catalog");
    }

    #[test]
    fn two_functions_with_one_name_keep_both_calls() {
        let dir = fixture();
        write(
            dir.path(),
            "caller.mesh-storefront.show_item.2.json",
            &caller("show_item", &["id"]),
        );
        let bundle = Bundle::from_dir(dir.path()).expect("bundle");
        assert_eq!(bundle.callers.len(), 2);
    }

    #[test]
    fn an_identical_call_is_kept_once() {
        let dir = fixture();
        write(
            dir.path(),
            "caller.mesh-storefront.show_item.2.json",
            &caller("show_item", &["name"]),
        );
        let bundle = Bundle::from_dir(dir.path()).expect("bundle");
        assert_eq!(bundle.callers.len(), 1);
        let _: BTreeSet<&BundleCall> = bundle.callers.iter().collect();
    }

    #[test]
    fn a_client_declared_in_another_crate_resolves_when_its_name_is_unique() {
        let dir = fixture();
        std::fs::remove_file(dir.path().join("client.mesh-storefront.CatalogClient.json"))
            .expect("rm");
        let mut shared = client();
        shared.krate = "mesh-clients".to_owned();
        write(
            dir.path(),
            "client.mesh-clients.CatalogClient.json",
            &shared,
        );
        let bundle = Bundle::from_dir(dir.path()).expect("bundle");
        assert_eq!(bundle.callers[0].endpoint, "get_item");
    }

    #[test]
    fn a_crate_relative_marker_resolves_in_the_crate_that_declares_the_client() {
        let dir = fixture();
        std::fs::remove_file(dir.path().join("client.mesh-storefront.CatalogClient.json"))
            .expect("rm");
        // The client lives in the catalog crate and names its marker
        // relative to that crate.
        let mut shared = client();
        shared.krate = "mesh-catalog".to_owned();
        shared.endpoints[0].marker = "crate::get_item_endpoint".to_owned();
        write(
            dir.path(),
            "client.mesh-catalog.CatalogClient.json",
            &shared,
        );
        // The caller's own crate also has an endpoint with that marker name.
        let mut local = endpoint("get_item", "/local/{id}", "NoBody");
        local.krate = "mesh-storefront".to_owned();
        local.service = "storefront".to_owned();
        write(
            dir.path(),
            "endpoint.mesh-storefront.storefront.get_item.json",
            &local,
        );
        let mut local_item = item();
        local_item.krate = "mesh-storefront".to_owned();
        write(dir.path(), "type.mesh-storefront.Item.json", &local_item);
        let bundle = Bundle::from_dir(dir.path()).expect("bundle");
        assert_eq!(bundle.callers[0].service, "catalog");
    }

    #[test]
    fn a_list_endpoint_resolves_its_element_shape_and_keeps_the_array() {
        let dir = fixture();
        let mut list = endpoint("list_items", "/items", "NoBody");
        list.response_list = true;
        write(
            dir.path(),
            "endpoint.mesh-catalog.catalog.list_items.json",
            &list,
        );
        let bundle = Bundle::from_dir(dir.path()).expect("bundle");
        let ep = bundle.endpoint("catalog", "list_items").expect("endpoint");
        assert!(ep.response_list);
        assert_eq!(ep.response.name, "Item");
        assert!(!ep.has_body);
    }

    #[test]
    fn an_extension_method_on_a_client_is_not_a_wire_call() {
        let dir = fixture();
        let mut with_extension = caller("show_item", &["name"]);
        with_extension.calls.push(CallerCall {
            client: "CatalogClient".to_owned(),
            method: "warm_cache".to_owned(),
            reads: Vec::new(),
            writes: None,
        });
        write(
            dir.path(),
            "caller.mesh-storefront.show_item.1.json",
            &with_extension,
        );
        let bundle = Bundle::from_dir(dir.path()).expect("bundle");
        assert_eq!(bundle.callers.len(), 1);
        assert_eq!(bundle.callers[0].endpoint, "get_item");
    }

    fn contact_event(krate: &str, version: u32) -> EventDescriptor {
        EventDescriptor {
            krate: krate.to_owned(),
            type_name: "ContactUpdated".to_owned(),
            name: Some("contact.updated".to_owned()),
            version,
            opaque: false,
            unit: false,
            shape: WireTypeDescriptor {
                name: "ContactUpdated".to_owned(),
                serialized: vec![field("id", true), field("email", true)],
                deserialized: vec![field("id", true), field("email", true)],
                closed: false,
            },
        }
    }

    fn listener(function: &str, event: &str, reads: &[&str]) -> ListenerDescriptor {
        ListenerDescriptor {
            krate: "marketing".to_owned(),
            function: function.to_owned(),
            event: event.to_owned(),
            reads: reads.iter().map(|r| (*r).to_owned()).collect(),
        }
    }

    /// A descriptor directory with one event and one listener of it.
    fn event_fixture() -> tempfile::TempDir {
        let dir = fixture();
        write(
            dir.path(),
            "event.core.ContactUpdated.json",
            &contact_event("core", 1),
        );
        write(
            dir.path(),
            "listener.marketing.on_contact.1.json",
            &listener(
                "on_contact",
                "core::ContactUpdated",
                &["email", "id", "email"],
            ),
        );
        dir
    }

    #[test]
    fn the_bundle_joins_events_and_their_listeners() {
        let dir = event_fixture();
        let bundle = Bundle::from_dir(dir.path()).expect("bundle");
        let [event] = bundle.events.as_slice() else {
            panic!("one event: {bundle:?}");
        };
        assert_eq!(event.topic, "contact.updated");
        assert_eq!(event.name, "contact.updated");
        assert_eq!(event.version, 1);
        let names: Vec<&str> = event
            .shape
            .serialized
            .iter()
            .map(|f| f.rust_name.as_str())
            .collect();
        assert_eq!(names, ["email", "id"], "fields are sorted");
        let [l] = bundle.listeners.as_slice() else {
            panic!("one listener: {bundle:?}");
        };
        assert_eq!(l.consumer(), "marketing::on_contact");
        assert_eq!(l.topic, "contact.updated");
        assert_eq!(l.reads, ["email", "id"], "reads are sorted and unique");
    }

    #[test]
    fn a_bundle_with_events_round_trips() {
        let dir = event_fixture();
        let bundle = Bundle::from_dir(dir.path()).expect("bundle");
        let back = Bundle::from_slice(&bundle.to_bytes()).expect("parses");
        assert_eq!(back, bundle);
    }

    #[test]
    fn a_bundle_with_no_events_has_the_same_bytes_as_before() {
        let dir = fixture();
        let text = String::from_utf8(Bundle::from_dir(dir.path()).expect("bundle").to_bytes())
            .expect("utf8");
        assert!(!text.contains("events"), "{text}");
        assert!(!text.contains("listeners"), "{text}");
        let old = br#"{"format": 1, "endpoints": [], "callers": []}"#;
        let bundle = Bundle::from_slice(old).expect("an old bundle still parses");
        assert!(bundle.events.is_empty() && bundle.listeners.is_empty());
    }

    #[test]
    fn a_pass_with_events_and_no_endpoint_is_a_bundle() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            "event.core.ContactUpdated.json",
            &contact_event("core", 1),
        );
        let bundle = Bundle::from_dir(dir.path()).expect("bundle");
        assert_eq!(bundle.events.len(), 1);
        assert!(bundle.endpoints.is_empty());
    }

    #[test]
    fn two_versions_of_one_event_are_two_topics() {
        let dir = event_fixture();
        let mut v2 = contact_event("core", 2);
        v2.type_name = "ContactUpdatedV2".to_owned();
        write(dir.path(), "event.core.ContactUpdatedV2.json", &v2);
        let bundle = Bundle::from_dir(dir.path()).expect("bundle");
        let topics: Vec<&str> = bundle.events.iter().map(|e| e.topic.as_str()).collect();
        assert_eq!(topics, ["contact.updated", "contact.updated.v2"]);
    }

    #[test]
    fn two_events_with_one_topic_are_an_error() {
        let dir = event_fixture();
        let mut other = contact_event("core", 1);
        other.type_name = "Other".to_owned();
        write(dir.path(), "event.core.Other.json", &other);
        let err = Bundle::from_dir(dir.path()).expect_err("refused");
        assert!(err.contains("contact.updated"), "{err}");
    }

    #[test]
    fn a_listener_of_an_event_with_no_descriptor_is_left_out() {
        // A hand-written `impl Event` writes no descriptor. Its consumer has
        // no topic in the bundle, and the export still works.
        let dir = event_fixture();
        std::fs::remove_file(dir.path().join("event.core.ContactUpdated.json")).expect("rm");
        let bundle = Bundle::from_dir(dir.path()).expect("bundle");
        assert!(bundle.listeners.is_empty() && bundle.events.is_empty());
        assert_eq!(
            bundle.unresolved,
            [
                "`marketing::on_contact` consumes `core::ContactUpdated`: no `#[event]` in the pass \
              has that type name"
            ]
        );
        assert_eq!(
            bundle.format, EVENTS_FORMAT,
            "an older CLI must not drop it"
        );
    }

    #[test]
    fn a_listener_that_names_its_event_through_an_alias_is_unresolved() {
        let dir = event_fixture();
        write(
            dir.path(),
            "listener.marketing.on_contact.1.json",
            &listener("on_contact", "Updated", &["email"]),
        );
        let bundle = Bundle::from_dir(dir.path()).expect("bundle");
        assert!(bundle.listeners.is_empty(), "{bundle:?}");
        assert_eq!(bundle.unresolved.len(), 1, "{bundle:?}");
        assert!(bundle.unresolved[0].contains("`Updated`"), "{bundle:?}");
    }

    #[test]
    fn an_event_without_a_name_and_its_listeners_are_left_out() {
        let dir = event_fixture();
        let mut unnamed = contact_event("core", 1);
        unnamed.name = None;
        write(dir.path(), "event.core.ContactUpdated.json", &unnamed);
        let bundle = Bundle::from_dir(dir.path()).expect("bundle");
        assert!(bundle.events.is_empty(), "{bundle:?}");
        assert!(bundle.listeners.is_empty(), "{bundle:?}");
        assert!(
            bundle.unresolved[0].ends_with("the event has no `name`"),
            "{bundle:?}"
        );
    }

    #[test]
    fn a_bundle_with_events_has_the_events_format() {
        let dir = event_fixture();
        let bundle = Bundle::from_dir(dir.path()).expect("bundle");
        assert_eq!(bundle.format, EVENTS_FORMAT);
        assert_eq!(
            Bundle::from_dir(fixture().path()).expect("bundle").format,
            BUNDLE_FORMAT
        );
    }

    #[test]
    fn a_path_whose_first_segment_is_a_module_falls_back_to_the_own_crate() {
        // Only the consumer's own crate has the event, so `events::` is a
        // local module.
        let dir = event_fixture();
        std::fs::remove_file(dir.path().join("event.core.ContactUpdated.json")).expect("rm");
        let mut own = contact_event("marketing", 1);
        own.name = Some("marketing.contact".to_owned());
        write(dir.path(), "event.marketing.ContactUpdated.json", &own);
        write(
            dir.path(),
            "listener.marketing.on_contact.1.json",
            &listener("on_contact", "events::ContactUpdated", &["id"]),
        );
        let bundle = Bundle::from_dir(dir.path()).expect("bundle");
        assert_eq!(bundle.listeners[0].topic, "marketing.contact");
    }

    #[test]
    fn a_crate_relative_path_picks_the_own_crate_beside_a_dependency() {
        let dir = event_fixture();
        let mut own = contact_event("marketing", 1);
        own.name = Some("marketing.contact".to_owned());
        write(dir.path(), "event.marketing.ContactUpdated.json", &own);
        write(
            dir.path(),
            "listener.marketing.on_contact.1.json",
            &listener("on_contact", "crate::events::ContactUpdated", &["id"]),
        );
        let bundle = Bundle::from_dir(dir.path()).expect("bundle");
        assert_eq!(bundle.listeners[0].topic, "marketing.contact");
    }

    #[test]
    fn two_events_with_one_type_name_in_one_crate_are_named_in_the_error() {
        let dir = event_fixture();
        let mut v2 = contact_event("core", 2);
        v2.name = Some("contact.updated".to_owned());
        write(dir.path(), "event.core.ContactUpdated.v2.k.json", &v2);
        let err = Bundle::from_dir(dir.path()).expect_err("refused");
        assert!(err.contains("crate `core` has more than one"), "{err}");
    }

    #[test]
    fn a_crate_qualified_event_path_picks_that_crates_event() {
        let dir = event_fixture();
        let mut other = contact_event("billing", 1);
        other.name = Some("billing.contact".to_owned());
        write(dir.path(), "event.billing.ContactUpdated.json", &other);
        let bundle = Bundle::from_dir(dir.path()).expect("bundle");
        assert_eq!(bundle.listeners[0].topic, "contact.updated");
    }

    #[test]
    fn an_unqualified_event_path_prefers_the_listeners_own_crate() {
        let dir = event_fixture();
        let mut own = contact_event("marketing", 1);
        own.name = Some("marketing.contact".to_owned());
        write(dir.path(), "event.marketing.ContactUpdated.json", &own);
        write(
            dir.path(),
            "listener.marketing.on_contact.1.json",
            &listener("on_contact", "ContactUpdated", &["id"]),
        );
        let bundle = Bundle::from_dir(dir.path()).expect("bundle");
        assert_eq!(bundle.listeners[0].topic, "marketing.contact");
    }

    #[test]
    fn a_renamed_dependency_path_is_not_taken_for_a_local_event() {
        // `contracts::ContactUpdated` where `contracts` is a renamed `core`,
        // and the consumer's own crate has a `ContactUpdated` too.
        let dir = event_fixture();
        let mut own = contact_event("marketing", 1);
        own.name = Some("marketing.contact".to_owned());
        write(dir.path(), "event.marketing.ContactUpdated.json", &own);
        write(
            dir.path(),
            "listener.marketing.on_contact.1.json",
            &listener("on_contact", "contracts::ContactUpdated", &["id"]),
        );
        let err = Bundle::from_dir(dir.path()).expect_err("refused");
        assert!(err.contains("`contracts` names no crate"), "{err}");
        assert!(err.contains("crate::"), "{err}");
    }

    #[test]
    fn a_renamed_dependency_path_resolves_when_only_one_crate_has_the_event() {
        let dir = event_fixture();
        write(
            dir.path(),
            "listener.marketing.on_contact.1.json",
            &listener("on_contact", "contracts::ContactUpdated", &["id"]),
        );
        let bundle = Bundle::from_dir(dir.path()).expect("bundle");
        assert_eq!(bundle.listeners[0].topic, "contact.updated");
    }

    #[test]
    fn an_ambiguous_unqualified_event_path_is_an_error() {
        let dir = event_fixture();
        let mut other = contact_event("billing", 1);
        other.name = Some("billing.contact".to_owned());
        write(dir.path(), "event.billing.ContactUpdated.json", &other);
        write(
            dir.path(),
            "listener.marketing.on_contact.1.json",
            &listener("on_contact", "ContactUpdated", &["id"]),
        );
        let err = Bundle::from_dir(dir.path()).expect_err("refused");
        assert!(err.contains("more than one crate"), "{err}");
    }

    #[test]
    fn an_opaque_event_is_kept_with_no_shape() {
        let dir = event_fixture();
        let mut opaque = contact_event("core", 1);
        opaque.opaque = true;
        opaque.shape = WireTypeDescriptor::default();
        write(dir.path(), "event.core.ContactUpdated.json", &opaque);
        let bundle = Bundle::from_dir(dir.path()).expect("bundle");
        assert!(bundle.events[0].opaque);
    }

    #[test]
    fn a_qualified_client_path_picks_that_crates_client() {
        let dir = fixture();
        // The caller's own crate has a `CatalogClient` that calls another
        // service. The function names the shared crate's client.
        let mut local = client();
        local.endpoints[0].marker = "warehouse::get_item_endpoint".to_owned();
        write(
            dir.path(),
            "client.mesh-storefront.CatalogClient.json",
            &local,
        );
        let mut shared = client();
        shared.krate = "shared-clients".to_owned();
        write(
            dir.path(),
            "client.shared-clients.CatalogClient.json",
            &shared,
        );
        let mut other = endpoint("get_item", "/w/{id}", "NoBody");
        other.krate = "warehouse".to_owned();
        other.service = "warehouse".to_owned();
        write(
            dir.path(),
            "endpoint.warehouse.warehouse.get_item.json",
            &other,
        );
        let mut other_item = item();
        other_item.krate = "warehouse".to_owned();
        write(dir.path(), "type.warehouse.Item.json", &other_item);
        let mut qualified = caller("show_item", &["name"]);
        qualified.calls[0].client = "shared_clients::CatalogClient".to_owned();
        write(
            dir.path(),
            "caller.mesh-storefront.show_item.1.json",
            &qualified,
        );
        let bundle = Bundle::from_dir(dir.path()).expect("bundle");
        assert_eq!(bundle.callers[0].service, "catalog");
    }
}
