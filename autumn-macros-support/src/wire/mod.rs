//! The wire descriptors: what one service endpoint puts on, and takes off,
//! the wire.
//!
//! The wire macros write these as JSON build artifacts. `#[contract_checked]`
//! reads them back to name a field in a diagnostic. `autumn contracts export`
//! joins them into one release bundle, and `autumn contracts check` compares
//! two bundles.
//!
//! Field names are recorded twice. `rust_name` is what a caller writes in
//! source; `wire_name` is what serde puts in the JSON. They differ under
//! `#[serde(rename)]` / `#[serde(rename_all)]`. A check inside one build keys
//! on `rust_name`. A check across two releases keys on `wire_name`.

use serde::{Deserialize, Serialize};

mod resolve;

pub use resolve::{
    Crates, FRAMEWORK_CRATE, NO_BODY, Resolved, Scope, child_module, crate_root, is_known_leaf,
    module_of_file, resolve_type, type_id, type_paths,
};

/// One field of a request or response type, as serde treats it.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct WireFieldDescriptor {
    /// The field's Rust identifier — what a call site writes.
    pub rust_name: String,
    /// The JSON key serde uses. Equals `rust_name` unless renamed.
    pub wire_name: String,
    /// The field's type, as written in the struct.
    pub ty: String,
    /// Whether the field must be present on the wire in this direction.
    ///
    /// For a request: the callee rejects a body that omits it. For a response:
    /// the callee always produces it, so a caller may read it unconditionally.
    pub required: bool,
    /// Extra keys `#[serde(alias = "…")]` also accepts, deserialize side only.
    ///
    /// Carried so a later version diff reads a rename-plus-alias — the standard
    /// non-breaking field rename — as the compatible change it is.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
    /// `#[wire(opaque)]`: the check compares this field by its type name
    /// only, and does not look for a shape of that type.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub opaque: bool,
    /// The ids of the `WireShape` types that this field's type names, in
    /// source order. The export fills them in. See [`type_id`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub nested: Vec<String>,
}

/// The serde-visible shape of one request or response type.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
pub struct WireTypeDescriptor {
    /// The type's Rust name.
    pub name: String,
    /// Fields this type puts on the wire when serialized.
    pub serialized: Vec<WireFieldDescriptor>,
    /// Fields this type accepts off the wire when deserialized.
    pub deserialized: Vec<WireFieldDescriptor>,
    /// Whether `#[serde(deny_unknown_fields)]` closes the object.
    ///
    /// Adding it is a breaking change across a deploy window — an older
    /// caller's extra key starts being rejected — so the artifact records it.
    #[serde(default)]
    pub closed: bool,
    /// A unit struct (`struct Empty;`). serde writes it as JSON `null`, not
    /// as an object. Its field sets are empty, as for `struct Empty {}`, so
    /// this flag tells the two apart.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub unit: bool,
}

impl WireTypeDescriptor {
    /// The serialized field with this Rust name, if the type produces one.
    #[must_use]
    pub fn produced(&self, rust_name: &str) -> Option<&WireFieldDescriptor> {
        self.serialized.iter().find(|f| f.rust_name == rust_name)
    }

    /// The deserialized field with this Rust name, if the type accepts one.
    #[must_use]
    pub fn accepted(&self, rust_name: &str) -> Option<&WireFieldDescriptor> {
        self.deserialized.iter().find(|f| f.rust_name == rust_name)
    }
}

/// One service endpoint's contract, as `#[endpoint]` wrote it.
///
/// Field shapes are not inlined here. A type is described once, in its own
/// `TypeDescriptor` artifact, and referenced by name — the same DTO usually
/// serves several endpoints, and one description keeps them from drifting.
/// A reader joins the two into a [`ResolvedEndpoint`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndpointDescriptor {
    /// The service this endpoint belongs to, from `#[endpoint(service = "…")]`.
    pub service: String,
    /// The endpoint name — the handler's function name unless overridden.
    pub name: String,
    /// The marker type `#[endpoint]` generates, e.g. `get_item_endpoint`.
    pub endpoint_ident: String,
    /// The crate the endpoint is defined in.
    pub krate: String,
    /// HTTP method, uppercase.
    pub method: String,
    /// Route path, with `{param}` placeholders intact.
    pub path: String,
    /// Rust name of the request body type, or `NoBody`.
    pub request_type: String,
    /// Rust name of the response body type.
    pub response_type: String,
    /// Whether the handler reads a JSON request body. `Json<NoBody>` reads
    /// one, so the type name alone cannot tell.
    #[serde(default)]
    pub has_body: bool,
    /// Whether the request body is a JSON array of `request_type`.
    #[serde(default)]
    pub request_list: bool,
    /// Whether the response body is a JSON array of `response_type`.
    #[serde(default)]
    pub response_list: bool,
    /// The handler function's name. `routes![…]` names the handler, not the
    /// endpoint, so the export joins the two by it.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub handler: String,
    /// The module of the handler's source file. See [`module_of_file`].
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub module: String,
    /// The request type's path as written, for example `shared::NewItem`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub request_path: String,
    /// The response type's path as written, for example `shared::Item`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub response_path: String,
}

impl EndpointDescriptor {
    /// `service.name` — how a diagnostic names this endpoint.
    #[must_use]
    pub fn id(&self) -> String {
        format!("{}.{}", self.service, self.name)
    }

    /// The artifact file name for this endpoint, unique within a workspace.
    #[must_use]
    pub fn artifact_file_name(&self) -> String {
        format!(
            "endpoint.{}.{}.{}.json",
            self.krate, self.service, self.name
        )
    }

    /// The request type's path. A descriptor without a path gives its name.
    #[must_use]
    pub fn request_path(&self) -> &str {
        or_name(&self.request_path, &self.request_type)
    }

    /// The response type's path. A descriptor without a path gives its name.
    #[must_use]
    pub fn response_path(&self) -> &str {
        or_name(&self.response_path, &self.response_type)
    }

    /// The handler's name. A descriptor without one gives the endpoint name.
    #[must_use]
    pub fn handler(&self) -> &str {
        or_name(&self.handler, &self.name)
    }
}

/// `value`, or `fallback` when `value` is empty.
const fn or_name<'a>(value: &'a str, fallback: &'a str) -> &'a str {
    if value.is_empty() { fallback } else { value }
}

/// One DTO's shape, as `#[derive(WireShape)]` wrote it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypeDescriptor {
    /// The crate the type is defined in.
    pub krate: String,
    /// The module of the type's source file. See [`module_of_file`].
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub module: String,
    /// The type's shape.
    #[serde(flatten)]
    pub shape: WireTypeDescriptor,
}

impl TypeDescriptor {
    /// The artifact file name for this type. Two types with one name in one
    /// source file share it.
    #[must_use]
    pub fn artifact_file_name(&self) -> String {
        let module = if self.module.is_empty() {
            String::new()
        } else {
            format!("{}.", self.module.replace("::", "."))
        };
        format!("type.{}.{module}{}.json", self.krate, self.shape.name)
    }

    /// The type's id: `krate::module::Name`. See [`type_id`].
    #[must_use]
    pub fn id(&self) -> String {
        type_id(&self.krate, &self.module, &self.shape.name)
    }
}

/// An endpoint with both of its type shapes filled in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedEndpoint {
    /// The endpoint itself.
    pub endpoint: EndpointDescriptor,
    /// What the endpoint accepts in its request body.
    pub request: WireTypeDescriptor,
    /// What the endpoint produces in its response body.
    pub response: WireTypeDescriptor,
}

/// One `wire_client!` client: the endpoint marker behind each method.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientDescriptor {
    /// The crate that declares the client.
    pub krate: String,
    /// The client type's name, e.g. `CatalogClient`.
    pub name: String,
    /// One entry per generated method.
    pub endpoints: Vec<ClientEndpoint>,
}

/// One method of a generated client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientEndpoint {
    /// The method name, e.g. `get_item`.
    pub method: String,
    /// The marker path as written, e.g. `mesh_catalog::get_item_endpoint`.
    pub marker: String,
}

impl ClientDescriptor {
    /// The artifact file name for this client, unique within a crate.
    #[must_use]
    pub fn artifact_file_name(&self) -> String {
        format!("client.{}.{}.json", self.krate, self.name)
    }
}

/// One `#[contract_checked]` function and the calls it makes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallerDescriptor {
    /// The crate that holds the function.
    pub krate: String,
    /// The function's name.
    pub function: String,
    /// Every client call in the function, in source order.
    pub calls: Vec<CallerCall>,
}

/// One call through a generated client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallerCall {
    /// The client type's path, as `#[contract_checked(client = …)]` gives
    /// it, e.g. `CatalogClient` or `shared::CatalogClient`.
    pub client: String,
    /// The client method called. It is also the endpoint's name.
    pub method: String,
    /// Response fields the caller reads, by Rust name, sorted.
    pub reads: Vec<String>,
    /// Request fields the call site sets, by Rust name, sorted. `None` when
    /// the request is not an inline struct literal.
    pub writes: Option<Vec<String>>,
}

impl CallerDescriptor {
    /// The artifact file name for this function.
    ///
    /// Two functions in one crate can have the same name. `key` makes the
    /// name unique; the caller derives it from the function's tokens.
    #[must_use]
    pub fn artifact_file_name(&self, key: &str) -> String {
        format!("caller.{}.{}.{key}.json", self.krate, self.function)
    }
}

/// One `#[event]` payload, as the macro wrote it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventDescriptor {
    /// The crate that defines the event.
    pub krate: String,
    /// The module of the event's source file. See [`module_of_file`].
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub module: String,
    /// The event type's Rust name.
    pub type_name: String,
    /// The name from `#[event(name = "…")]`, if set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The schema version from `#[event(version = N)]`. Default 1.
    pub version: u32,
    /// `#[event(opaque)]`: the payload has no wire shape to check.
    #[serde(default)]
    pub opaque: bool,
    /// A unit struct (`struct Ping;`): the payload is JSON `null`, not an
    /// object. Its shape is empty, like an object with no fields, so this
    /// flag tells the two apart.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub unit: bool,
    /// The payload's shape. Empty when `opaque`.
    pub shape: WireTypeDescriptor,
}

impl EventDescriptor {
    /// The topic without its version suffix: the `name`.
    ///
    /// `None` for an event without `name`. It routes on `module_path!()`,
    /// which a macro cannot see, so the bundle does not know its topic.
    #[must_use]
    pub fn base_name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// The topic: the base name, plus `.vN` when the version is above 1.
    #[must_use]
    pub fn topic(&self) -> Option<String> {
        self.base_name()
            .map(|base| versioned_topic(base, self.version))
    }

    /// The artifact file name for this event. `key` keeps two events with
    /// one type name in one crate apart.
    #[must_use]
    pub fn artifact_file_name(&self, key: &str) -> String {
        format!(
            "event.{}.{}.v{}.{key}.json",
            self.krate, self.type_name, self.version
        )
    }
}

/// `base` for version 1, else `base.vN`.
///
/// Version 1 keeps the bare name, so an event that adds `version` keeps its
/// topic.
#[must_use]
pub fn versioned_topic(base: &str, version: u32) -> String {
    if version > 1 {
        format!("{base}.v{version}")
    } else {
        base.to_owned()
    }
}

/// One consumer of an event: a `#[listener]` or an `#[event_consumer]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListenerDescriptor {
    /// The crate that holds the consumer.
    pub krate: String,
    /// The consumer function's name.
    pub function: String,
    /// The event type's path, as the attribute gives it.
    pub event: String,
    /// Payload fields the consumer reads, by Rust name, sorted.
    pub reads: Vec<String>,
}

impl ListenerDescriptor {
    /// The artifact file name for this consumer. `key` keeps two functions
    /// with one name in one crate apart.
    #[must_use]
    pub fn artifact_file_name(&self, key: &str) -> String {
        format!("listener.{}.{}.{key}.json", self.krate, self.function)
    }
}

/// One `routes![…]` call: the handlers it registers.
///
/// Only the export pass writes it. An `#[endpoint]` that no `routes![…]`
/// registers is not served.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoutesDescriptor {
    /// The crate that calls `routes![…]`.
    pub krate: String,
    /// The module of the call's source file. See [`module_of_file`].
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub module: String,
    /// Each handler path as written, for example `get_item` or
    /// `mesh_catalog::get_item`.
    pub handlers: Vec<String>,
}

impl RoutesDescriptor {
    /// The artifact file name for this call. `key` keeps two calls in one
    /// crate apart.
    #[must_use]
    pub fn artifact_file_name(&self, key: &str) -> String {
        format!("routes.{}.{key}.json", self.krate)
    }
}

/// A stable 64-bit FNV-1a hash, as 16 hex digits.
///
/// Use it for file names that must be the same on every build. The standard
/// hasher is random per process, so it cannot do this.
#[must_use]
pub fn stable_key(text: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_key_is_the_same_for_the_same_text() {
        assert_eq!(stable_key("fn a() {}"), stable_key("fn a() {}"));
        assert_ne!(stable_key("fn a() {}"), stable_key("fn b() {}"));
        assert_eq!(stable_key(""), "cbf29ce484222325");
    }

    #[test]
    fn a_caller_descriptor_round_trips_through_json() {
        let caller = CallerDescriptor {
            krate: "storefront".to_owned(),
            function: "show_item".to_owned(),
            calls: vec![CallerCall {
                client: "CatalogClient".to_owned(),
                method: "get_item".to_owned(),
                reads: vec!["name".to_owned()],
                writes: None,
            }],
        };
        let json = serde_json::to_string(&caller).expect("serializes");
        let back: CallerDescriptor = serde_json::from_str(&json).expect("parses");
        assert_eq!(back, caller);
        assert_eq!(
            caller.artifact_file_name("k"),
            "caller.storefront.show_item.k.json"
        );
    }

    fn event(name: Option<&str>, version: u32) -> EventDescriptor {
        EventDescriptor {
            krate: "core-app".to_owned(),
            module: String::new(),
            type_name: "ContactUpdated".to_owned(),
            name: name.map(str::to_owned),
            version,
            opaque: false,
            unit: false,
            shape: WireTypeDescriptor::default(),
        }
    }

    #[test]
    fn version_one_keeps_the_bare_topic() {
        assert_eq!(
            event(Some("contact.updated"), 1).topic().as_deref(),
            Some("contact.updated")
        );
    }

    #[test]
    fn a_later_version_adds_a_suffix_to_the_topic() {
        assert_eq!(
            event(Some("contact.updated"), 2).topic().as_deref(),
            Some("contact.updated.v2")
        );
        assert_eq!(
            event(Some("contact.updated"), 2).base_name(),
            Some("contact.updated")
        );
    }

    #[test]
    fn an_event_without_a_name_has_no_known_topic() {
        assert_eq!(event(None, 1).topic(), None);
        assert_eq!(event(None, 3).topic(), None);
    }

    #[test]
    fn an_event_descriptor_round_trips_and_names_its_file() {
        let e = event(Some("contact.updated"), 2);
        let json = serde_json::to_string(&e).expect("serializes");
        let back: EventDescriptor = serde_json::from_str(&json).expect("parses");
        assert_eq!(back, e);
        assert_eq!(
            e.artifact_file_name("k"),
            "event.core-app.ContactUpdated.v2.k.json"
        );
    }

    #[test]
    fn a_listener_descriptor_round_trips_and_names_its_file() {
        let l = ListenerDescriptor {
            krate: "marketing".to_owned(),
            function: "on_contact_updated".to_owned(),
            event: "core_app::ContactUpdated".to_owned(),
            reads: vec!["email".to_owned()],
        };
        let json = serde_json::to_string(&l).expect("serializes");
        let back: ListenerDescriptor = serde_json::from_str(&json).expect("parses");
        assert_eq!(back, l);
        assert_eq!(
            l.artifact_file_name("k"),
            "listener.marketing.on_contact_updated.k.json"
        );
    }

    #[test]
    fn a_client_descriptor_names_its_file_after_the_crate_and_client() {
        let client = ClientDescriptor {
            krate: "storefront".to_owned(),
            name: "CatalogClient".to_owned(),
            endpoints: Vec::new(),
        };
        assert_eq!(
            client.artifact_file_name(),
            "client.storefront.CatalogClient.json"
        );
    }
}
