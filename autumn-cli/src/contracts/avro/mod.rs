//! `autumn contracts export --avro`: Avro schemas from the wire contract.
//!
//! The Rust types stay the source of truth. The export reads a bundle and
//! writes text only: one `.avsc` per event topic and, with `--endpoints`, one
//! per request and response body. JSON stays the wire format, so a schema
//! describes the data, not Avro's JSON encoding. Field names are the serde
//! wire names.
//!
//! A form that Avro cannot hold stops the export with a message that names
//! the field and the fix. All such messages are given together.

#[cfg(test)]
mod agreement;
#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use autumn_macros_support::wire::{WireFieldDescriptor, WireType, WireTypeDescriptor, format};

use super::bundle::{Bundle, BundleEndpoint, BundleEvent, NO_BODY, WIRE_TYPE_FORMAT};
use super::proto::{package_part, pascal};

/// The key that names the topic of an event schema.
pub const TOPIC_KEY: &str = "autumn.topic";

/// The key that names the endpoint of a body schema.
pub const ENDPOINT_KEY: &str = "autumn.endpoint";

/// The key that holds a format that Avro has no logical type for.
pub const FORMAT_KEY: &str = "autumn.format";

/// The directory of the body schemas under the output directory.
pub const ENDPOINTS_DIR: &str = "endpoints";

/// The widest line the writer puts a nested value on.
const WIDTH: usize = 80;

/// What the export writes in addition to the events.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AvroOptions {
    /// Also write the request and response bodies of each endpoint.
    pub endpoints: bool,
}

/// The files of one export.
#[derive(Debug, Clone, Default)]
pub struct AvroExport {
    /// Each file's text, by its path under the output directory, with `/`.
    pub files: BTreeMap<String, String>,
}

/// Export `bundle` as Avro schemas.
///
/// # Errors
/// A form has no Avro type, a name is not a valid Avro name or file name,
/// two names clash, or there is nothing to export.
pub fn export(bundle: &Bundle, options: AvroOptions) -> Result<AvroExport, String> {
    if bundle.format < WIRE_TYPE_FORMAT {
        return Err(format!(
            "the bundle is format {}, which has no wire types. Avro needs them. Export the \
             bundle again with this CLI",
            bundle.format
        ));
    }
    let mut files = BTreeMap::new();
    let mut errors = Vec::new();
    let mut put = |path: String, owner: &str, schema: Result<Json, Vec<String>>| match schema {
        Ok(json) => {
            if let Some((other, _)) = files.get(&path) {
                errors.push(format!(
                    "{other} and {owner} both write `{path}`. Rename one"
                ));
            } else {
                files.insert(path, (owner.to_owned(), json.render()));
            }
        }
        Err(e) => errors.extend(e),
    };

    for event in &bundle.events {
        let subject = format!("event `{}`", event.topic);
        match file_part(&event.topic, &subject) {
            Ok(()) => put(
                format!("{}.avsc", event.topic),
                &subject,
                event_schema(bundle, event),
            ),
            Err(e) => put(String::new(), &subject, Err(vec![e])),
        }
    }
    if options.endpoints {
        for endpoint in bundle.endpoints.iter().filter(|e| !e.unrouted) {
            let id = endpoint.id();
            let subject = format!("endpoint `{id}`");
            if let Err(e) = file_part(&endpoint.service, &subject)
                .and_then(|()| file_part(&endpoint.name, &subject))
            {
                put(String::new(), &subject, Err(vec![e]));
                continue;
            }
            let owner = format!("{subject} in crate `{}`", endpoint.krate);
            if endpoint.has_body {
                put(
                    format!("{ENDPOINTS_DIR}/{id}.request.avsc"),
                    &owner,
                    body_schema(bundle, endpoint, Side::Request),
                );
            }
            if endpoint.response.name != NO_BODY {
                put(
                    format!("{ENDPOINTS_DIR}/{id}.response.avsc"),
                    &owner,
                    body_schema(bundle, endpoint, Side::Response),
                );
            }
        }
    }

    let mut by_case: BTreeMap<String, &String> = BTreeMap::new();
    for path in files.keys() {
        if let Some(other) = by_case.insert(path.to_ascii_lowercase(), path) {
            errors.push(format!(
                "`{other}` and `{path}` differ only in case, and some file systems do not \
                 tell them apart. Rename one topic or endpoint"
            ));
        }
    }
    if !errors.is_empty() {
        return Err(errors.join("\n"));
    }
    if files.is_empty() {
        return Err(if options.endpoints {
            "the bundle has no event and no endpoint body, so there is nothing to export".to_owned()
        } else {
            "the bundle has no event, so there is nothing to export. Add `--endpoints` to \
             export the endpoint bodies"
                .to_owned()
        });
    }
    Ok(AvroExport {
        files: files
            .into_iter()
            .map(|(path, (_, text))| (path, text))
            .collect(),
    })
}

/// Whether `text` is a schema that this export wrote: a JSON object with
/// the topic key or the endpoint key. Only such a file is removed as stale.
#[must_use]
pub fn is_exported(text: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(text).is_ok_and(|v| {
        [TOPIC_KEY, ENDPOINT_KEY]
            .iter()
            .any(|key| v.get(key).is_some_and(serde_json::Value::is_string))
    })
}

/// The schema of one event topic.
fn event_schema(bundle: &Bundle, event: &BundleEvent) -> Result<Json, Vec<String>> {
    let mut doc = format!("topic: {}, version {}", event.topic, event.version);
    let marker = (TOPIC_KEY, Json::str(&event.topic));
    if event.opaque {
        doc.push_str(
            ". Opaque event: the contract has no field list, so the data is the JSON text \
             as bytes",
        );
        return Ok(Json::Obj(vec![
            ("type".to_owned(), Json::str("bytes")),
            ("doc".to_owned(), Json::Str(doc)),
            (marker.0.to_owned(), marker.1),
        ]));
    }
    if event.unit {
        doc.push_str(". Autumn sends JSON null for this event, not an object");
    }
    let mut writer = Writer::new(bundle, format!("event `{}`", event.topic), None);
    let name = Name {
        namespace: format!("autumn.{}", package_part(&event.krate)),
        name: pascal(&event.topic),
    };
    let own = bundle.same_type(&event.type_name, Some(&event.krate), &event.shape);
    let record = writer.top_record(&name, &event.shape, own, doc, marker);
    writer.finish(record)
}

/// Which body of an endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Side {
    Request,
    Response,
}

/// The schema of one request or response body.
fn body_schema(
    bundle: &Bundle,
    endpoint: &BundleEndpoint,
    side: Side,
) -> Result<Json, Vec<String>> {
    let (shape, list, word, verb) = match side {
        Side::Request => (&endpoint.request, endpoint.request_list, "request", "reads"),
        Side::Response => (
            &endpoint.response,
            endpoint.response_list,
            "response",
            "sends",
        ),
    };
    let id = endpoint.id();
    let mut doc = format!(
        "endpoint: {id}, {} {}, {word} body",
        endpoint.method, endpoint.path
    );
    if list {
        doc.push_str(". The body is a JSON array of the item record");
    } else if shape.unit {
        let _ = write!(doc, ". Autumn {verb} JSON null, not an object");
    }
    let marker = (ENDPOINT_KEY, Json::str(&id));
    let mut writer = Writer::new(bundle, format!("endpoint `{id}` {word} body"), Some(side));
    let item = if list { "_item" } else { "" };
    let name = Name {
        namespace: format!("autumn.{}", package_part(&endpoint.service)),
        name: pascal(&format!("{}_{word}{item}", endpoint.name)),
    };
    let own = bundle.same_type(&shape.name, None, shape);
    if !list {
        let record = writer.top_record(&name, shape, own, doc, marker);
        return writer.finish(record);
    }
    let record = writer.record(&name, shape, own, None, "");
    writer.finish(Json::Obj(vec![
        ("type".to_owned(), Json::str("array")),
        ("doc".to_owned(), Json::Str(doc)),
        (marker.0.to_owned(), marker.1),
        ("items".to_owned(), record),
    ]))
}

/// An Avro full name.
struct Name {
    namespace: String,
    name: String,
}

impl Name {
    fn full(&self) -> String {
        format!("{}.{}", self.namespace, self.name)
    }

    /// The name of a nested type: `krate::module::Type` becomes the name
    /// `Type` in namespace `autumn.krate.module`.
    fn of_id(id: &str) -> Self {
        let mut parts: Vec<&str> = id.split("::").collect();
        let last = parts.pop().unwrap_or(id);
        let mut namespace = vec!["autumn".to_owned()];
        if let Some((krate, modules)) = parts.split_first() {
            namespace.push(package_part(krate));
            namespace.extend(modules.iter().map(|m| ident(m)));
        }
        Self {
            namespace: namespace.join("."),
            name: ident(last),
        }
    }
}

/// One schema while it is built.
struct Writer<'b> {
    bundle: &'b Bundle,
    /// What the messages name, such as an event and its topic.
    subject: String,
    /// The body the schema is for. `None` is an event, which can be made
    /// opaque.
    side: Option<Side>,
    /// The Avro full name of each nested type id.
    names: BTreeMap<String, String>,
    /// The id, or the subject, that owns each full name.
    owners: BTreeMap<String, String>,
    errors: Vec<String>,
}

impl<'b> Writer<'b> {
    const fn new(bundle: &'b Bundle, subject: String, side: Option<Side>) -> Self {
        Self {
            bundle,
            subject,
            side,
            names: BTreeMap::new(),
            owners: BTreeMap::new(),
            errors: Vec::new(),
        }
    }

    fn finish(self, schema: Json) -> Result<Json, Vec<String>> {
        if self.errors.is_empty() {
            Ok(schema)
        } else {
            Err(self.errors)
        }
    }

    /// The top record: its doc and marker go after the name.
    fn top_record(
        &mut self,
        name: &Name,
        shape: &WireTypeDescriptor,
        own: Option<&str>,
        doc: String,
        marker: (&str, Json),
    ) -> Json {
        self.record(name, shape, own, Some((doc, marker)), "")
    }

    /// A record of `shape` under `name`. `own` is the id of the type when it
    /// is a nested type too, so a field that names it refers to this record.
    fn record(
        &mut self,
        name: &Name,
        shape: &WireTypeDescriptor,
        own: Option<&str>,
        top: Option<(String, (&str, Json))>,
        path: &str,
    ) -> Json {
        let full = name.full();
        let owner = own.map_or_else(|| "the top record".to_owned(), |id| format!("`{id}`"));
        self.claim(&full, &owner);
        if let Some(id) = own {
            self.names.insert(id.to_owned(), full.clone());
        }

        let mut out = vec![
            ("type".to_owned(), Json::str("record")),
            ("name".to_owned(), Json::str(&name.name)),
            ("namespace".to_owned(), Json::str(&name.namespace)),
        ];
        match top {
            Some((doc, (key, value))) => {
                out.push(("doc".to_owned(), Json::Str(doc)));
                out.push((key.to_owned(), value));
            }
            None if shape.unit => out.push((
                "doc".to_owned(),
                Json::str("Autumn sends JSON null for this type, not an object"),
            )),
            None => {}
        }
        let fields = fields(shape, self.side);
        let names: BTreeSet<&str> = fields.iter().map(|f| f.wire_name).collect();
        // serde gives a key to the first field with that alias.
        let mut used: BTreeSet<&str> = BTreeSet::new();
        let mut list = Vec::with_capacity(fields.len());
        for field in &fields {
            let at = join(path, field.wire_name);
            if !is_name(field.wire_name) {
                self.errors.push(format!(
                    "{}: field `{at}`: `{}` is not an Avro name. An Avro name has only ASCII \
                     letters, digits and `_`, and does not start with a digit. Rename the \
                     field with `#[serde(rename = \"…\")]`",
                    self.subject, field.wire_name
                ));
                continue;
            }
            let wire = match &field.wire {
                Ok(wire) => wire,
                Err((written, read)) => {
                    self.errors.push(format!(
                        "{}: field `{at}`: serde writes `{written}` and reads `{read}`. Avro \
                         needs one form. Give both sides one `#[wire(type = …)]`",
                        self.subject
                    ));
                    continue;
                }
            };
            // A field that the JSON can leave out needs a default. A list
            // or a map is empty, and another form is nullable.
            let (wire, default) = match wire {
                WireType::Nullable { .. } => (wire.clone(), Some(Json::Null)),
                WireType::Array { .. } if field.optional => {
                    (wire.clone(), Some(Json::Arr(Vec::new())))
                }
                WireType::Map { .. } if field.optional => {
                    (wire.clone(), Some(Json::Obj(Vec::new())))
                }
                _ if field.optional => (WireType::nullable(wire.clone()), Some(Json::Null)),
                _ => (wire.clone(), None),
            };
            let Some(ty) = self.ty(&wire, &at) else {
                continue;
            };
            let mut entry = vec![
                ("name".to_owned(), Json::str(field.wire_name)),
                ("type".to_owned(), ty),
            ];
            if let Some(default) = default {
                entry.push(("default".to_owned(), default));
            }
            let mut aliases: Vec<Json> = Vec::new();
            for alias in field.aliases {
                // An alias that no Avro writer can have, that is another
                // field's name, or that an earlier field has, is left out.
                if is_name(alias) && !names.contains(alias.as_str()) && used.insert(alias) {
                    aliases.push(Json::str(alias));
                }
            }
            if !aliases.is_empty() {
                entry.push(("aliases".to_owned(), Json::Arr(aliases)));
            }
            list.push(Json::Obj(entry));
        }
        out.push(("fields".to_owned(), Json::Arr(list)));
        Json::Obj(out)
    }

    /// Record that `owner` uses `full`. Two owners of one name stop the
    /// export.
    fn claim(&mut self, full: &str, owner: &str) {
        match self.owners.get(full) {
            Some(other) if other != owner => self.errors.push(format!(
                "{}: {other} and {owner} both map to the Avro name `{full}`. Rename one Rust \
                 type",
                self.subject
            )),
            Some(_) => {}
            None => {
                self.owners.insert(full.to_owned(), owner.to_owned());
            }
        }
    }

    /// The Avro type of `wire`, or `None` after an error.
    fn ty(&mut self, wire: &WireType, at: &str) -> Option<Json> {
        let refuse = |me: &mut Self, problem: String| {
            me.errors
                .push(format!("{}: field `{at}`: {problem}", me.subject));
            None
        };
        match wire {
            WireType::String { format: None } => Some(Json::str("string")),
            WireType::String { format: Some(f) } if f == format::UUID => Some(Json::Obj(vec![
                ("type".to_owned(), Json::str("string")),
                ("logicalType".to_owned(), Json::str("uuid")),
            ])),
            WireType::String { format: Some(f) } => Some(Json::Obj(vec![
                ("type".to_owned(), Json::str("string")),
                (FORMAT_KEY.to_owned(), Json::str(f)),
            ])),
            WireType::Integer {
                bits,
                signed,
                format,
            } => {
                let Some(base) = integer(*bits, *signed) else {
                    return refuse(
                        self,
                        format!(
                            "`{wire}` does not fit in an Avro `long`. Use a smaller integer. \
                             Or send it as a string with a serde adapter, such as \
                             `serde_with::DisplayFromStr`, and name that form with \
                             `#[wire(type = \"string\")]`"
                        ),
                    );
                };
                Some(format.as_ref().map_or_else(
                    || Json::str(base),
                    |f| {
                        Json::Obj(vec![
                            ("type".to_owned(), Json::str(base)),
                            (FORMAT_KEY.to_owned(), Json::str(f)),
                        ])
                    },
                ))
            }
            WireType::Number => Some(Json::str("double")),
            WireType::Boolean => Some(Json::str("boolean")),
            WireType::Array { of } => {
                let items = self.ty(of, &format!("{at}[]"))?;
                Some(Json::Obj(vec![
                    ("type".to_owned(), Json::str("array")),
                    ("items".to_owned(), items),
                ]))
            }
            WireType::Map { of } => {
                let values = self.ty(of, &format!("{at}{{}}"))?;
                Some(Json::Obj(vec![
                    ("type".to_owned(), Json::str("map")),
                    ("values".to_owned(), values),
                ]))
            }
            WireType::Nullable { of } => {
                // A bundle file can hold a nullable in a nullable. Avro has
                // no union in a union, and the two mean the same.
                let mut of = of.as_ref();
                while let WireType::Nullable { of: inner } = of {
                    of = inner;
                }
                let inner = self.ty(of, at)?;
                Some(Json::Arr(vec![Json::str("null"), inner]))
            }
            WireType::Object { type_ref } => {
                // A type with a name is defined already: its record takes the
                // name before its fields.
                if let Some(full) = self.names.get(type_ref) {
                    return Some(Json::Str(full.clone()));
                }
                let Some(nested) = self.bundle.nested(type_ref) else {
                    return refuse(
                        self,
                        format!(
                            "names type `{type_ref}`, and the bundle has no shape for it. \
                             Derive `WireShape` on it, or name the JSON form with \
                             `#[wire(type = …)]`"
                        ),
                    );
                };
                let name = Name::of_id(type_ref);
                Some(self.record(&name, &nested.shape, Some(type_ref), None, at))
            }
            WireType::Any => {
                let fix = if self.side.is_none() {
                    ", or mark the event `#[event(opaque)]`"
                } else {
                    ""
                };
                refuse(
                    self,
                    format!(
                        "the JSON form is `any`, so Avro has no type for it. Name the form \
                         with `#[wire(type = …)]`{fix}"
                    ),
                )
            }
        }
    }
}

/// The smallest Avro integer type that holds each value of the Rust
/// integer: `int` up to 32 signed or 31 unsigned bits, `long` up to 64
/// signed or 63 unsigned bits. `None` when no Avro type holds it.
///
/// `verification/avro_integer_width.rs` proves this table.
#[must_use]
pub const fn integer(bits: u8, signed: bool) -> Option<&'static str> {
    match (signed, bits) {
        (true, 0..=32) | (false, 0..=31) => Some("int"),
        (true, 33..=64) | (false, 32..=63) => Some("long"),
        _ => None,
    }
}

/// One field of a record: its wire name, its form, and its aliases.
struct FieldSpec<'a> {
    wire_name: &'a str,
    /// The form, or the two forms that the two sides disagree on.
    wire: Result<WireType, (WireType, WireType)>,
    /// The JSON can leave the field out: serde does not always write it, or
    /// does not need it to read the type.
    optional: bool,
    aliases: &'a [String],
}

/// The fields of a type: each wire name once, the serialized side first.
///
/// A side with no wire type is `any`. When the two sides differ, only a
/// nullable form of the other fits both.
///
/// `side` is the body a schema is for. `None` is an event, which a producer
/// writes and a consumer reads.
fn fields(shape: &WireTypeDescriptor, side: Option<Side>) -> Vec<FieldSpec<'_>> {
    let form =
        |f: Option<&WireFieldDescriptor>| f.map(|f| f.wire_type.clone().unwrap_or(WireType::Any));
    let mut out: Vec<FieldSpec<'_>> = Vec::new();
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
        let wire = match (form(produced), form(accepted)) {
            (Some(t), None) | (None, Some(t)) => Ok(t),
            (Some(p), Some(a)) if p == a => Ok(p),
            (Some(p), Some(a)) if WireType::nullable(p.clone()) == a => Ok(a),
            (Some(p), Some(a)) if WireType::nullable(a.clone()) == p => Ok(p),
            (Some(p), Some(a)) => Err((p, a)),
            (None, None) => Ok(WireType::Any),
        };
        let written = produced.is_some_and(|f| f.required);
        let read = accepted.is_some_and(|f| f.required);
        let optional = match side {
            None => !written || accepted.is_some_and(|f| !f.required),
            Some(Side::Request) => !read,
            Some(Side::Response) => !written,
        };
        out.push(FieldSpec {
            wire_name: &field.wire_name,
            wire,
            optional,
            aliases: accepted.map_or(&[], |f| f.aliases.as_slice()),
        });
    }
    out
}

/// `parent.name`, or `name` at the top.
fn join(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_owned()
    } else {
        format!("{parent}.{name}")
    }
}

/// Whether `text` is an Avro name: `[A-Za-z_][A-Za-z0-9_]*`.
fn is_name(text: &str) -> bool {
    text.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && text.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// A Rust name as an Avro name: `r#` goes, another character that Avro does
/// not allow becomes `_`, and a name that starts with a digit gets `_` first.
fn ident(text: &str) -> String {
    let name: String = text
        .trim_start_matches("r#")
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    if is_name(&name) {
        name
    } else {
        format!("_{name}")
    }
}

/// Names that Windows keeps for devices, with any extension.
const DEVICES: &[&str] = &[
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
    "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

/// Refuse a topic or endpoint part that is not a safe file name.
fn file_part(text: &str, subject: &str) -> Result<(), String> {
    let first = text
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let safe = !text.is_empty()
        && !text.starts_with('.')
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        && !DEVICES.contains(&first.as_str());
    if safe {
        Ok(())
    } else {
        Err(format!(
            "{subject}: `{text}` is not a safe file name. The Avro export allows only ASCII \
             letters, digits, `.`, `_` and `-`, no `.` first, and no Windows device name \
             such as `con` or `aux` before the first `.`. Rename it"
        ))
    }
}

/// A JSON value with its keys in the order they are written.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Json {
    Null,
    Str(String),
    Arr(Vec<Self>),
    Obj(Vec<(String, Self)>),
}

impl Json {
    fn str(text: &str) -> Self {
        Self::Str(text.to_owned())
    }

    /// The file text: the value on its own lines, then a newline.
    fn render(&self) -> String {
        let mut out = String::new();
        self.write(&mut out, 0, 0, true);
        out.push('\n');
        out
    }

    /// The value on one line.
    fn inline(&self) -> String {
        match self {
            Self::Null => "null".to_owned(),
            Self::Str(text) => serde_json::to_string(text).unwrap_or_default(),
            Self::Arr(items) => {
                let items: Vec<String> = items.iter().map(Self::inline).collect();
                format!("[{}]", items.join(", "))
            }
            Self::Obj(entries) if entries.is_empty() => "{}".to_owned(),
            Self::Obj(entries) => {
                let entries: Vec<String> = entries
                    .iter()
                    .map(|(k, v)| format!("{}: {}", Self::str(k).inline(), v.inline()))
                    .collect();
                format!("{{ {} }}", entries.join(", "))
            }
        }
    }

    /// A record, and a non-empty field list, take one line per entry.
    fn spread(&self, key: &str) -> bool {
        match self {
            Self::Obj(entries) => entries.iter().any(|(k, _)| k == "fields"),
            Self::Arr(items) => key == "fields" && !items.is_empty(),
            Self::Null | Self::Str(_) => false,
        }
    }

    /// Write the value at `indent`, after `used` characters of its line.
    /// It goes on one line when that fits, else one line per entry.
    fn write(&self, out: &mut String, indent: usize, used: usize, spread: bool) {
        let inline = self.inline();
        if !spread && indent + used + inline.len() < WIDTH {
            out.push_str(&inline);
            return;
        }
        let pad = " ".repeat(indent + 2);
        match self {
            Self::Arr(items) if !items.is_empty() => {
                out.push_str("[\n");
                for (i, item) in items.iter().enumerate() {
                    out.push_str(&pad);
                    item.write(out, indent + 2, 0, item.spread(""));
                    out.push_str(if i + 1 < items.len() { ",\n" } else { "\n" });
                }
                out.push_str(&" ".repeat(indent));
                out.push(']');
            }
            Self::Obj(entries) if !entries.is_empty() => {
                out.push_str("{\n");
                for (i, (key, value)) in entries.iter().enumerate() {
                    let head = format!("{}: ", Self::str(key).inline());
                    out.push_str(&pad);
                    out.push_str(&head);
                    value.write(out, indent + 2, head.len(), value.spread(key));
                    out.push_str(if i + 1 < entries.len() { ",\n" } else { "\n" });
                }
                out.push_str(&" ".repeat(indent));
                out.push('}');
            }
            _ => out.push_str(&inline),
        }
    }
}
