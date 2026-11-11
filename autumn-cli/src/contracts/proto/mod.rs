//! `autumn contracts export --proto`: protobuf schemas from the wire contract.
//!
//! The Rust types stay the source of truth. The export reads a bundle and
//! writes text only: one file per service for its endpoints, one file per
//! crate for its events, and a `buf.yaml`. JSON stays the wire format, so
//! each field's `json_name` is its serde wire name. `proto.lock` keeps each
//! field number. See [`lock`].

pub mod lock;
#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use autumn_macros_support::wire::{WireFieldDescriptor, WireType, WireTypeDescriptor, format};

use super::bundle::{Bundle, BundleEndpoint, BundleEvent};
use lock::{Lock, Wanted};

/// The buf configuration the export writes beside the files.
pub const BUF_YAML: &str = "buf.yaml";

const STRUCT_PROTO: &str = "google/protobuf/struct.proto";
const TIMESTAMP_PROTO: &str = "google/protobuf/timestamp.proto";
const EMPTY_PROTO: &str = "google/protobuf/empty.proto";
const ANNOTATIONS_PROTO: &str = "google/api/annotations.proto";

const VALUE: &str = "google.protobuf.Value";
const TIMESTAMP: &str = "google.protobuf.Timestamp";
const EMPTY: &str = "google.protobuf.Empty";

/// What the export adds to the plain mapping.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProtoOptions {
    /// Add `google.api.http` options to each rpc.
    pub http_annotations: bool,
    /// Map a `date-time` string to `google.protobuf.Timestamp`.
    pub well_known_types: bool,
}

/// The files of one export and the updated lock.
#[derive(Debug, Clone)]
pub struct ProtoExport {
    /// Each file's text, by its path under the output directory, with `/`.
    pub files: BTreeMap<String, String>,
    /// The lock with every number this export gave.
    pub lock: Lock,
}

/// Export `bundle` as protobuf files, with the numbers in `lock`.
///
/// # Errors
/// The bundle has no routed endpoint and no event, two names clash after
/// the mapping, or the lock refuses a number. See [`Lock::assign`].
pub fn export(bundle: &Bundle, lock: &Lock, options: ProtoOptions) -> Result<ProtoExport, String> {
    let mut lock = lock.clone();
    let mut files = BTreeMap::new();

    let mut services: BTreeMap<&str, Vec<&BundleEndpoint>> = BTreeMap::new();
    for endpoint in bundle.endpoints.iter().filter(|e| !e.unrouted) {
        services
            .entry(&endpoint.service)
            .or_default()
            .push(endpoint);
    }
    for (service, endpoints) in &services {
        let package = format!("autumn.{}", package_part(service));
        let mut file = File::new(bundle, &package, options);
        file.service(service, endpoints, &mut lock)?;
        files.insert(path_of(&package, &package_part(service)), file.render());
    }

    let mut crates: BTreeMap<String, Vec<&BundleEvent>> = BTreeMap::new();
    for event in &bundle.events {
        crates
            .entry(package_part(&event.krate))
            .or_default()
            .push(event);
    }
    for (krate, events) in &crates {
        let package = format!("autumn.{krate}.events");
        let mut file = File::new(bundle, &package, options);
        file.events(events, &mut lock)?;
        files.insert(path_of(&package, "events"), file.render());
    }

    if files.is_empty() {
        return Err(
            "the bundle has no routed endpoint and no event, so there is nothing to export"
                .to_owned(),
        );
    }
    files.insert(BUF_YAML.to_owned(), buf_yaml(options));
    Ok(ProtoExport { files, lock })
}

/// The path of a package's file: its directory, then `<stem>.proto`.
fn path_of(package: &str, stem: &str) -> String {
    format!("{}/{stem}.proto", package.replace('.', "/"))
}

/// The buf configuration. The exceptions keep the JSON the same as what
/// Autumn sends.
fn buf_yaml(options: ProtoOptions) -> String {
    let deps = if options.http_annotations {
        "deps:\n  - buf.build/googleapis/googleapis\n"
    } else {
        ""
    };
    format!(
        "# Written by `autumn contracts export --proto`. Do not edit.\n\
         version: v2\n\
         {deps}\
         lint:\n  \
           use:\n    \
             - STANDARD\n  \
           except:\n    \
             # An rpc returns the message Autumn sends as the JSON body. A\n    \
             # `<Rpc>Response` wrapper would change that JSON.\n    \
             - RPC_RESPONSE_STANDARD_NAME\n    \
             - RPC_REQUEST_RESPONSE_UNIQUE\n    \
             # The package is the service name. Autumn has no API version.\n    \
             - PACKAGE_VERSION_SUFFIX\n\
         breaking:\n  \
           use:\n    \
             # JSON is the wire format.\n    \
             - WIRE_JSON\n"
    )
}

/// Where a message comes from. Two origins with one name in one file get
/// longer names.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Origin {
    /// A nested `WireShape` type, by its id.
    Type(String),
    /// A body type with no id in the bundle.
    Body { krate: String, name: String },
    /// An event payload, by its topic.
    Event(String),
}

/// One field of a message, before it has a number.
struct Spec<'a> {
    wire_name: &'a str,
    rust_name: &'a str,
    wire_type: WireType,
    tag: Option<u32>,
    notes: Vec<String>,
}

/// One field, ready to render.
struct Field {
    label: &'static str,
    ty: String,
    name: String,
    number: u32,
    json_name: String,
    notes: Vec<String>,
}

/// One message, ready to render.
#[derive(Default)]
struct Message {
    comments: Vec<String>,
    fields: Vec<Field>,
    reserved_numbers: Vec<u32>,
    reserved_names: Vec<String>,
}

/// One rpc, ready to render.
struct Rpc {
    name: String,
    request: String,
    response: String,
    method: String,
    path: String,
    body: Option<String>,
}

/// One `.proto` file while it is built.
struct File<'b> {
    bundle: &'b Bundle,
    package: String,
    options: ProtoOptions,
    imports: BTreeSet<&'static str>,
    /// The message name of each origin in this file.
    names: BTreeMap<Origin, String>,
    messages: BTreeMap<String, Message>,
    service: Option<(String, Vec<Rpc>)>,
}

impl<'b> File<'b> {
    fn new(bundle: &'b Bundle, package: &str, options: ProtoOptions) -> Self {
        Self {
            bundle,
            package: package.to_owned(),
            options,
            imports: BTreeSet::new(),
            names: BTreeMap::new(),
            messages: BTreeMap::new(),
            service: None,
        }
    }

    /// The service of `endpoints`, with each message it needs.
    fn service(
        &mut self,
        service: &str,
        endpoints: &[&BundleEndpoint],
        lock: &mut Lock,
    ) -> Result<(), String> {
        let mut roots = Vec::new();
        for endpoint in endpoints {
            if endpoint.has_body && endpoint.request_list {
                roots.push(self.body_origin(endpoint, &endpoint.request));
            }
            if !endpoint.response.unit {
                roots.push(self.body_origin(endpoint, &endpoint.response));
            }
        }
        self.name_origins(roots)?;

        let mut rpcs = Vec::new();
        let mut rpc_names = BTreeSet::new();
        for endpoint in endpoints {
            let name = pascal(&endpoint.name);
            if !rpc_names.insert(name.clone()) {
                return Err(format!(
                    "service `{service}`: two endpoints map to rpc `{name}`; rename one"
                ));
            }
            let request = format!("{name}Request");
            let body = self.request(endpoint, &request, lock)?;
            let response = self.response(endpoint, lock)?;
            rpcs.push(Rpc {
                name,
                request,
                response,
                method: endpoint.method.clone(),
                path: endpoint.path.clone(),
                body,
            });
        }
        if self.options.http_annotations {
            self.imports.insert(ANNOTATIONS_PROTO);
        }
        self.service = Some((format!("{}Service", pascal(service)), rpcs));
        Ok(())
    }

    /// One message for each event, with each message it needs.
    fn events(&mut self, events: &[&BundleEvent], lock: &mut Lock) -> Result<(), String> {
        let roots = events.iter().map(|e| self.event_origin(e)).collect();
        self.name_origins(roots)?;
        for event in events {
            let origin = self.event_origin(event);
            let name = self.names[&origin].clone();
            let mut comments = vec![format!("Event topic: `{}`.", event.topic)];
            if event.opaque {
                comments.push(
                    "Opaque event: the contract has no field list, so this message has no \
                     fields."
                        .to_owned(),
                );
            }
            self.type_message(&name, &event.shape, comments, lock)?;
        }
        Ok(())
    }

    /// The origin of a body type: the nested type with the same name and
    /// shape, else the body itself.
    fn body_origin(&self, endpoint: &BundleEndpoint, shape: &WireTypeDescriptor) -> Origin {
        self.same_type(&shape.name, None, shape).map_or_else(
            || Origin::Body {
                krate: endpoint.krate.clone(),
                name: shape.name.clone(),
            },
            Origin::Type,
        )
    }

    fn event_origin(&self, event: &BundleEvent) -> Origin {
        if event.opaque {
            return Origin::Event(event.topic.clone());
        }
        self.same_type(&event.type_name, Some(&event.krate), &event.shape)
            .map_or_else(|| Origin::Event(event.topic.clone()), Origin::Type)
    }

    /// The id of a nested type named `name` with the fields of `shape`.
    fn same_type(
        &self,
        name: &str,
        krate: Option<&str>,
        shape: &WireTypeDescriptor,
    ) -> Option<String> {
        self.bundle
            .types
            .iter()
            .find(|t| {
                t.id.rsplit("::").next() == Some(name)
                    && krate.is_none_or(|k| t.id.split("::").next() == Some(&package_part(k)))
                    && t.shape.serialized == shape.serialized
                    && t.shape.deserialized == shape.deserialized
                    && t.shape.unit == shape.unit
            })
            .map(|t| t.id.clone())
    }

    /// Give each origin in `roots`, and each type they reach, a message name.
    fn name_origins(&mut self, roots: Vec<Origin>) -> Result<(), String> {
        let mut seen: BTreeSet<Origin> = BTreeSet::new();
        let mut queue = roots;
        while let Some(origin) = queue.pop() {
            if !seen.insert(origin.clone()) {
                continue;
            }
            let Some(shape) = self.shape_of(&origin) else {
                continue;
            };
            for spec in specs(shape) {
                let mut found = Vec::new();
                spec.wire_type
                    .clone()
                    .for_each_object(&mut |id: &mut String| {
                        found.push(id.clone());
                    });
                for id in found {
                    if self.bundle.nested(&id).is_some() {
                        queue.push(Origin::Type(id));
                    }
                }
            }
        }
        let mut by_name: BTreeMap<String, Vec<Origin>> = BTreeMap::new();
        for origin in seen {
            by_name
                .entry(self.short_name(&origin))
                .or_default()
                .push(origin);
        }
        for (name, origins) in by_name {
            if let [origin] = origins.as_slice() {
                self.names.insert(origin.clone(), name);
                continue;
            }
            for origin in origins {
                self.names.insert(origin.clone(), long_name(&origin));
            }
        }
        let mut taken = BTreeMap::new();
        for (origin, name) in &self.names {
            if let Some(other) = taken.insert(name.clone(), origin.clone()) {
                return Err(format!(
                    "package `{}`: {other:?} and {origin:?} both map to message `{name}`; \
                     rename one Rust type",
                    self.package
                ));
            }
        }
        Ok(())
    }

    /// The message name of an origin when no other origin in the file has
    /// it: the Rust type name.
    fn short_name(&self, origin: &Origin) -> String {
        match origin {
            Origin::Type(id) => id.rsplit("::").next().unwrap_or(id).to_owned(),
            Origin::Body { name, .. } => name.clone(),
            Origin::Event(topic) => self
                .bundle
                .event(topic)
                .map_or_else(|| pascal(topic), |e| e.type_name.clone()),
        }
    }

    /// The shape of an origin.
    fn shape_of(&self, origin: &Origin) -> Option<&'b WireTypeDescriptor> {
        match origin {
            Origin::Type(id) => self.bundle.nested(id).map(|t| &t.shape),
            Origin::Body { krate, name } => self
                .bundle
                .endpoints
                .iter()
                .filter(|e| &e.krate == krate)
                .flat_map(|e| [&e.request, &e.response])
                .find(|s| &s.name == name),
            Origin::Event(topic) => self.bundle.event(topic).map(|e| &e.shape),
        }
    }

    /// The `<Rpc>Request` message: path parameters, then the body. Returns
    /// the `body` of the HTTP rule.
    fn request(
        &mut self,
        endpoint: &BundleEndpoint,
        name: &str,
        lock: &mut Lock,
    ) -> Result<Option<String>, String> {
        let params = path_params(&endpoint.path);
        let mut fields: Vec<Spec<'_>> = Vec::new();
        let mut body = None;
        let mut comments = Vec::new();
        if endpoint.has_body && endpoint.request_list {
            let origin = self.body_origin(endpoint, &endpoint.request);
            let element = self.names[&origin].clone();
            self.message_of(&origin, lock)?;
            fields.push(Spec {
                wire_name: "items",
                rust_name: "items",
                wire_type: WireType::array(WireType::object(&element)),
                tag: None,
                notes: vec!["body".to_owned()],
            });
            comments.push(format!(
                "Autumn reads the body as a bare JSON array of `{element}`, not as this object."
            ));
            body = Some("items".to_owned());
        } else if endpoint.has_body {
            if endpoint.request.unit {
                comments.push("Autumn reads the body as JSON null.".to_owned());
            }
            fields = specs(&endpoint.request);
            body = Some("*".to_owned());
        }
        for param in params.iter().rev() {
            if let Some(spec) = fields.iter_mut().find(|s| s.wire_name == param.as_str()) {
                spec.notes.insert(0, "path".to_owned());
                continue;
            }
            fields.insert(
                0,
                Spec {
                    wire_name: param,
                    rust_name: param,
                    wire_type: WireType::string(),
                    tag: None,
                    notes: vec!["path".to_owned()],
                },
            );
        }
        // Keep the path parameters first, in path order.
        fields.sort_by_key(|s| !params.iter().any(|p| p == s.wire_name));
        let message = self.build(name, fields, comments, lock)?;
        self.insert(name, message)?;
        Ok(body)
    }

    /// The response message name.
    fn response(&mut self, endpoint: &BundleEndpoint, lock: &mut Lock) -> Result<String, String> {
        if endpoint.response.unit {
            self.imports.insert(EMPTY_PROTO);
            return Ok(EMPTY.to_owned());
        }
        let origin = self.body_origin(endpoint, &endpoint.response);
        let element = self.names[&origin].clone();
        self.message_of(&origin, lock)?;
        if endpoint.response_list {
            return self.wrapper(&WireType::array(WireType::object(&element)), lock);
        }
        Ok(element)
    }

    /// Build the message of `origin`, once.
    fn message_of(&mut self, origin: &Origin, lock: &mut Lock) -> Result<(), String> {
        let name = self.names[origin].clone();
        if self.messages.contains_key(&name) {
            return Ok(());
        }
        let Some(shape) = self.shape_of(origin) else {
            return Ok(());
        };
        let comment = match origin {
            Origin::Type(id) => format!("Rust type `{id}`."),
            Origin::Body { name, .. } => format!("Rust type `{name}`."),
            Origin::Event(topic) => format!("Event topic: `{topic}`."),
        };
        self.type_message(&name, shape, vec![comment], lock)
    }

    /// The message of a type's shape.
    fn type_message(
        &mut self,
        name: &str,
        shape: &WireTypeDescriptor,
        mut comments: Vec<String>,
        lock: &mut Lock,
    ) -> Result<(), String> {
        if self.messages.contains_key(name) {
            return Ok(());
        }
        // A placeholder stops a type that names itself.
        self.messages.insert(name.to_owned(), Message::default());
        if shape.unit {
            comments.push("Autumn sends JSON null for this type.".to_owned());
        }
        if shape.closed {
            comments.push("Autumn refuses a key that is not a field here.".to_owned());
        }
        let message = self.build(name, specs(shape), comments, lock)?;
        self.messages.insert(name.to_owned(), message);
        Ok(())
    }

    /// Number and map the fields of message `name`.
    fn build(
        &mut self,
        name: &str,
        specs: Vec<Spec<'_>>,
        comments: Vec<String>,
        lock: &mut Lock,
    ) -> Result<Message, String> {
        let full = format!("{}.{name}", self.package);
        let wanted: Vec<Wanted> = specs
            .iter()
            .map(|s| Wanted {
                name: s.wire_name.to_owned(),
                tag: s.tag,
            })
            .collect();
        let numbers = lock.assign(&full, &wanted)?;
        let mut fields = Vec::with_capacity(specs.len());
        for (spec, number) in specs.into_iter().zip(numbers) {
            let (label, ty, mut notes) = self.field_type(&spec.wire_type, lock)?;
            notes.splice(0..0, spec.notes);
            fields.push(Field {
                label,
                ty,
                name: field_name(spec.wire_name, spec.rust_name),
                number,
                json_name: spec.wire_name.to_owned(),
                notes,
            });
        }
        fields.sort_by_key(|f| f.number);

        let entry = &lock.messages[&full];
        let mut reserved_names: BTreeSet<String> = entry
            .reserved
            .iter()
            .filter_map(|r| r.name.as_deref())
            .map(|n| field_name(n, n))
            .collect();
        // A proto name is unique in the message, and is not a reserved one.
        let mut used = BTreeSet::new();
        for field in &mut fields {
            if used.contains(&field.name) || reserved_names.contains(&field.name) {
                field.name = format!("{}_{}", field.name, field.number);
            }
            used.insert(field.name.clone());
        }
        reserved_names.retain(|n| !used.contains(n));
        Ok(Message {
            comments,
            fields,
            reserved_numbers: entry.reserved.iter().map(|r| r.number).collect(),
            reserved_names: reserved_names.into_iter().collect(),
        })
    }

    /// The label, the type and the notes of a field.
    fn field_type(
        &mut self,
        wire: &WireType,
        lock: &mut Lock,
    ) -> Result<(&'static str, String, Vec<String>), String> {
        let notes = format_note(wire).into_iter().collect();
        Ok(match wire {
            WireType::Array { of } => ("repeated ", self.element(of, lock)?, notes),
            WireType::Map { of } => {
                let value = self.element(of, lock)?;
                ("", format!("map<string, {value}>"), notes)
            }
            WireType::Nullable { of } => match of.as_ref() {
                WireType::Any => ("", self.single(of, lock)?, notes),
                WireType::Array { .. } | WireType::Map { .. } => {
                    let (label, ty, mut notes) = self.field_type(of, lock)?;
                    notes.push("nullable: null reads as empty".to_owned());
                    (label, ty, notes)
                }
                inner if self.is_message(inner) => {
                    let mut notes = notes;
                    notes.push("nullable".to_owned());
                    ("", self.single(inner, lock)?, notes)
                }
                inner => ("optional ", self.single(inner, lock)?, notes),
            },
            other => ("", self.single(other, lock)?, notes),
        })
    }

    /// The type of one value in a `repeated` field or a map.
    fn element(&mut self, wire: &WireType, lock: &mut Lock) -> Result<String, String> {
        match wire {
            WireType::Nullable { of } if matches!(of.as_ref(), WireType::Any) => {
                self.single(of, lock)
            }
            WireType::Array { .. } | WireType::Map { .. } | WireType::Nullable { .. } => {
                self.wrapper(wire, lock)
            }
            other => self.single(other, lock),
        }
    }

    /// The type of one singular value.
    fn single(&mut self, wire: &WireType, lock: &mut Lock) -> Result<String, String> {
        Ok(match wire {
            WireType::String { format } => {
                if self.options.well_known_types && format.as_deref() == Some(format::DATE_TIME) {
                    self.imports.insert(TIMESTAMP_PROTO);
                    TIMESTAMP.to_owned()
                } else {
                    "string".to_owned()
                }
            }
            WireType::Integer { bits, signed, .. } => match (bits, signed) {
                (0..=32, true) => "int32".to_owned(),
                (0..=32, false) => "uint32".to_owned(),
                (33..=64, true) => "int64".to_owned(),
                (33..=64, false) => "uint64".to_owned(),
                _ => {
                    self.imports.insert(STRUCT_PROTO);
                    VALUE.to_owned()
                }
            },
            WireType::Number => "double".to_owned(),
            WireType::Boolean => "bool".to_owned(),
            WireType::Object { type_ref } => {
                if let Some(name) = self.names.get(&Origin::Type(type_ref.clone())).cloned() {
                    self.message_of(&Origin::Type(type_ref.clone()), lock)?;
                    name
                } else if self.messages.contains_key(type_ref)
                    || self.names.values().any(|n| n == type_ref)
                {
                    // A name this file gave already, such as a list element.
                    type_ref.clone()
                } else {
                    self.imports.insert(STRUCT_PROTO);
                    VALUE.to_owned()
                }
            }
            WireType::Any | WireType::Nullable { .. } => {
                if let WireType::Nullable { of } = wire
                    && !matches!(of.as_ref(), WireType::Any)
                {
                    return self.single(of, lock);
                }
                self.imports.insert(STRUCT_PROTO);
                VALUE.to_owned()
            }
            WireType::Array { .. } | WireType::Map { .. } => self.wrapper(wire, lock)?,
        })
    }

    /// Whether a value of this type maps to a message, which has presence.
    fn is_message(&self, wire: &WireType) -> bool {
        match wire {
            WireType::Object { type_ref } => {
                self.names.contains_key(&Origin::Type(type_ref.clone()))
                    || self.names.values().any(|n| n == type_ref)
            }
            WireType::String { format } => {
                self.options.well_known_types && format.as_deref() == Some(format::DATE_TIME)
            }
            _ => false,
        }
    }

    /// A wrapper message for a shape that protobuf cannot write in place.
    fn wrapper(&mut self, wire: &WireType, lock: &mut Lock) -> Result<String, String> {
        let name = format!("{}Wrapper", self.shape_name(wire));
        if self.messages.contains_key(&name) {
            return Ok(name);
        }
        let (field_name, comment) = match wire {
            WireType::Array { .. } => (
                "values",
                "Wrapper. In the JSON, this is a bare array, not an object.",
            ),
            WireType::Map { .. } => (
                "values",
                "Wrapper. In the JSON, this is the bare map, not an object with `values`.",
            ),
            _ => (
                "value",
                "Wrapper. In the JSON, this is the value or null, not an object.",
            ),
        };
        self.messages.insert(name.clone(), Message::default());
        let (label, ty, notes) = self.field_type(wire, lock)?;
        self.messages.insert(
            name.clone(),
            Message {
                comments: vec![comment.to_owned()],
                fields: vec![Field {
                    label,
                    ty,
                    name: field_name.to_owned(),
                    number: 1,
                    json_name: field_name.to_owned(),
                    notes,
                }],
                ..Message::default()
            },
        );
        Ok(name)
    }

    /// A short name for a shape: `DoubleList`, `NullableString`, `ItemMap`.
    fn shape_name(&self, wire: &WireType) -> String {
        match wire {
            WireType::String { format } => {
                if self.options.well_known_types && format.as_deref() == Some(format::DATE_TIME) {
                    "Timestamp".to_owned()
                } else {
                    "String".to_owned()
                }
            }
            WireType::Integer { bits, signed, .. } => match (bits, signed) {
                (0..=32, true) => "Int32".to_owned(),
                (0..=32, false) => "Uint32".to_owned(),
                (33..=64, true) => "Int64".to_owned(),
                (33..=64, false) => "Uint64".to_owned(),
                _ => "Value".to_owned(),
            },
            WireType::Number => "Double".to_owned(),
            WireType::Boolean => "Bool".to_owned(),
            WireType::Any => "Value".to_owned(),
            WireType::Object { type_ref } => self
                .names
                .get(&Origin::Type(type_ref.clone()))
                .cloned()
                .unwrap_or_else(|| {
                    if self.names.values().any(|n| n == type_ref) {
                        type_ref.clone()
                    } else {
                        "Value".to_owned()
                    }
                }),
            WireType::Array { of } => format!("{}List", self.shape_name(of)),
            WireType::Map { of } => format!("{}Map", self.shape_name(of)),
            WireType::Nullable { of } => format!("Nullable{}", self.shape_name(of)),
        }
    }

    fn insert(&mut self, name: &str, message: Message) -> Result<(), String> {
        if self.messages.contains_key(name) {
            return Err(format!(
                "package `{}`: message `{name}` is defined twice; rename the Rust type",
                self.package
            ));
        }
        self.messages.insert(name.to_owned(), message);
        Ok(())
    }

    /// The file's text.
    fn render(&self) -> String {
        let mut out = String::new();
        out.push_str(
            "// Generated by `autumn contracts export --proto`. Do not edit.\n\
             // The Rust types are the source of truth. JSON is the wire format.\n\n\
             syntax = \"proto3\";\n\n",
        );
        let _ = writeln!(out, "package {};\n", self.package);
        for import in &self.imports {
            let _ = writeln!(out, "import \"{import}\";");
        }
        if !self.imports.is_empty() {
            out.push('\n');
        }
        let mut first = true;
        for (name, message) in &self.messages {
            if !first {
                out.push('\n');
            }
            first = false;
            render_message(&mut out, name, message);
        }
        if let Some((name, rpcs)) = &self.service {
            out.push('\n');
            self.render_service(&mut out, name, rpcs);
        }
        out
    }

    fn render_service(&self, out: &mut String, name: &str, rpcs: &[Rpc]) {
        let _ = writeln!(out, "service {name} {{");
        for (i, rpc) in rpcs.iter().enumerate() {
            if i > 0 {
                out.push('\n');
            }
            let _ = writeln!(out, "  // {} {}", rpc.method, rpc.path);
            let head = format!(
                "  rpc {}({}) returns ({})",
                rpc.name, rpc.request, rpc.response
            );
            if !self.options.http_annotations {
                let _ = writeln!(out, "{head};");
                continue;
            }
            let _ = writeln!(out, "{head} {{");
            let _ = writeln!(out, "    option (google.api.http) = {{");
            let pattern = http_pattern(&rpc.path);
            match rpc.method.to_ascii_lowercase().as_str() {
                verb @ ("get" | "put" | "post" | "delete" | "patch") => {
                    let _ = writeln!(out, "      {verb}: {}", quoted(&pattern));
                }
                _ => {
                    let _ = writeln!(
                        out,
                        "      custom: {{kind: {}, path: {}}}",
                        quoted(&rpc.method),
                        quoted(&pattern)
                    );
                }
            }
            if let Some(body) = &rpc.body {
                let _ = writeln!(out, "      body: {}", quoted(body));
            }
            let _ = writeln!(out, "    }};");
            let _ = writeln!(out, "  }}");
        }
        let _ = writeln!(out, "}}");
    }
}

fn render_message(out: &mut String, name: &str, message: &Message) {
    for comment in &message.comments {
        let _ = writeln!(out, "// {comment}");
    }
    let empty = message.fields.is_empty()
        && message.reserved_numbers.is_empty()
        && message.reserved_names.is_empty();
    if empty {
        let _ = writeln!(out, "message {name} {{}}");
        return;
    }
    let _ = writeln!(out, "message {name} {{");
    for field in &message.fields {
        let _ = write!(
            out,
            "  {}{} {} = {} [json_name = {}];",
            field.label,
            field.ty,
            field.name,
            field.number,
            quoted(&field.json_name)
        );
        if field.notes.is_empty() {
            out.push('\n');
        } else {
            let _ = writeln!(out, " // {}", field.notes.join(", "));
        }
    }
    if !message.reserved_numbers.is_empty() {
        let numbers: Vec<String> = message
            .reserved_numbers
            .iter()
            .map(ToString::to_string)
            .collect();
        let _ = writeln!(out, "  reserved {};", numbers.join(", "));
    }
    if !message.reserved_names.is_empty() {
        let names: Vec<String> = message.reserved_names.iter().map(|n| quoted(n)).collect();
        let _ = writeln!(out, "  reserved {};", names.join(", "));
    }
    let _ = writeln!(out, "}}");
}

/// The fields of a type: each wire name once, the produced side first.
fn specs(shape: &WireTypeDescriptor) -> Vec<Spec<'_>> {
    let mut out: Vec<Spec<'_>> = Vec::new();
    for field in shape.serialized.iter().chain(&shape.deserialized) {
        if out.iter().any(|s| s.wire_name == field.wire_name) {
            continue;
        }
        let produced = shape
            .serialized
            .iter()
            .find(|f| f.wire_name == field.wire_name);
        let accepted = shape
            .deserialized
            .iter()
            .find(|f| f.wire_name == field.wire_name);
        out.push(spec(produced, accepted, field));
    }
    out
}

/// One field from its two sides.
fn spec<'a>(
    produced: Option<&'a WireFieldDescriptor>,
    accepted: Option<&'a WireFieldDescriptor>,
    field: &'a WireFieldDescriptor,
) -> Spec<'a> {
    let known = |f: Option<&WireFieldDescriptor>| {
        f.and_then(|f| f.wire_type.clone())
            .filter(|t| *t != WireType::Any)
    };
    let wire_type = known(produced)
        .or_else(|| known(accepted))
        .unwrap_or(WireType::Any);
    let mut notes = Vec::new();
    if accepted.is_some_and(|f| f.required) {
        notes.push("required".to_owned());
    }
    match (produced.is_some(), accepted.is_some()) {
        (true, false) => notes.push("response only".to_owned()),
        (false, true) => notes.push("request only".to_owned()),
        _ => {}
    }
    if let Some(f) = accepted.filter(|f| !f.aliases.is_empty()) {
        notes.push(format!("also reads {}", f.aliases.join(", ")));
    }
    Spec {
        wire_name: &field.wire_name,
        rust_name: &field.rust_name,
        wire_type,
        tag: produced
            .and_then(|f| f.tag)
            .or_else(|| accepted.and_then(|f| f.tag)),
        notes,
    }
}

/// The note for a format the proto type does not show.
fn format_note(wire: &WireType) -> Option<String> {
    match wire {
        WireType::String {
            format: Some(format),
        }
        | WireType::Integer {
            format: Some(format),
            ..
        } => Some(format!("format: {format}")),
        WireType::Integer { bits, .. } if *bits > 64 => Some(format!("{bits}-bit integer")),
        WireType::Array { of } | WireType::Map { of } | WireType::Nullable { of } => {
            format_note(of)
        }
        _ => None,
    }
}

/// The names in `{…}` segments of a route path, in order.
fn path_params(path: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = path;
    while let Some(start) = rest.find('{') {
        let Some(end) = rest[start..].find('}') else {
            break;
        };
        let inner = &rest[start + 1..start + end];
        let name = inner.trim_start_matches('*');
        let name = name.split(':').next().unwrap_or(name);
        if !name.is_empty() && !out.iter().any(|n| n == name) {
            out.push(name.to_owned());
        }
        rest = &rest[start + end + 1..];
    }
    out
}

/// The path in `google.api.http` syntax: `{*rest}` becomes `{rest=**}`.
fn http_pattern(path: &str) -> String {
    let mut out = String::new();
    let mut rest = path;
    while let Some(start) = rest.find('{') {
        let Some(end) = rest[start..].find('}') else {
            break;
        };
        out.push_str(&rest[..start]);
        let inner = &rest[start + 1..start + end];
        match inner.strip_prefix('*') {
            Some(name) => {
                let _ = write!(out, "{{{name}=**}}");
            }
            None => {
                let _ = write!(out, "{{{inner}}}");
            }
        }
        rest = &rest[start + end + 1..];
    }
    out.push_str(rest);
    out
}

/// A protobuf string literal.
fn quoted(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A lower snake case identifier for a package part.
fn package_part(text: &str) -> String {
    let snake = snake(text);
    if snake.starts_with(|c: char| c.is_ascii_lowercase()) {
        snake
    } else {
        format!("p_{snake}")
    }
}

/// A lower snake case field name from the wire name.
///
/// A wire name with no letter, such as `@`, uses the Rust name.
fn field_name(wire_name: &str, rust_name: &str) -> String {
    let from_wire = snake(wire_name);
    let name = if from_wire.chars().any(|c| c.is_ascii_lowercase()) {
        from_wire
    } else {
        snake(rust_name)
    };
    if name.starts_with(|c: char| c.is_ascii_lowercase()) {
        name
    } else {
        format!("f_{name}")
    }
}

/// `priceCents`, `price-cents` and `PriceCents` all become `price_cents`.
fn snake(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 4);
    let mut prev: Option<char> = None;
    for c in text.chars() {
        if c.is_ascii_alphanumeric() {
            if c.is_ascii_uppercase()
                && prev.is_some_and(|p| p.is_ascii_lowercase() || p.is_ascii_digit())
            {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else if !out.is_empty() && !out.ends_with('_') {
            out.push('_');
        }
        prev = Some(c);
    }
    while out.ends_with('_') {
        out.pop();
    }
    out
}

/// `get_item` becomes `GetItem`; `catalog.item_repriced` becomes
/// `CatalogItemRepriced`.
fn pascal(text: &str) -> String {
    text.split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            chars.next().map_or_else(String::new, |first| {
                first.to_ascii_uppercase().to_string() + chars.as_str()
            })
        })
        .collect()
}

/// The message name of an origin that shares its short name.
fn long_name(origin: &Origin) -> String {
    match origin {
        Origin::Type(id) => pascal(id),
        Origin::Body { krate, name } => format!("{}{name}", pascal(krate)),
        Origin::Event(topic) => pascal(topic),
    }
}
