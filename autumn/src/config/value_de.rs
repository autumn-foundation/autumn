//! One serde deserializer for config (#2795).
//!
//! `toml::from_str` makes one `Deserialize` copy of each config struct for
//! each toml access type: table, array, datetime and span. The schema walk
//! made one more. [`ConfigDe`] does both jobs through one map type, one
//! sequence type and one enum type. Each config struct then compiles once,
//! and `autumn-web` builds faster.
//!
//! - [`ConfigDe::Value`] reads a merged `toml::Value`.
//! - [`ConfigDe::Schema`] walks the config types with placeholder values and
//!   records the fields of each table. See `AutumnConfig::get_schema_keys`.

use std::fmt;
use std::sync::PoisonError;

use serde::de::{
    self, DeserializeOwned, DeserializeSeed, EnumAccess, MapAccess, SeqAccess, VariantAccess,
    Visitor,
};

use super::{AnyProbe, SchemaDeserializer};

/// The key that toml uses to carry a datetime through serde.
///
/// `toml::Value` and `toml_datetime::Datetime` read a one-entry map with this
/// key as a datetime. A test checks that the two agree.
const DATETIME_KEY: &str = "$__toml_private_datetime";

/// Deserialize `T` from a merged config value.
///
/// An error names the key path, for example ``in `server.port` ``.
pub(super) fn from_value<T: DeserializeOwned>(value: toml::Value) -> Result<T, toml::de::Error> {
    T::deserialize(ConfigDe::Value(value)).map_err(Error::into_toml)
}

/// An error and the key path where it occurred.
#[derive(Debug)]
pub(super) struct Error {
    message: String,
    /// Path segments, innermost first.
    path: Vec<Segment>,
}

#[derive(Debug)]
enum Segment {
    Key(String),
    Index(usize),
}

impl Error {
    fn at(mut self, segment: Segment) -> Self {
        self.path.push(segment);
        self
    }

    fn path(&self) -> String {
        let mut out = String::new();
        for segment in self.path.iter().rev() {
            match segment {
                Segment::Key(key) => {
                    if !out.is_empty() {
                        out.push('.');
                    }
                    out.push_str(key);
                }
                Segment::Index(index) => {
                    out.push('[');
                    out.push_str(&index.to_string());
                    out.push(']');
                }
            }
        }
        out
    }

    fn into_toml(self) -> toml::de::Error {
        <toml::de::Error as de::Error>::custom(self)
    }

    /// The error type of the public [`SchemaDeserializer`].
    pub(super) fn into_value_error(self) -> de::value::Error {
        <de::value::Error as de::Error>::custom(self)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.path.is_empty() {
            f.write_str(&self.message)
        } else {
            write!(f, "{}\nin `{}`", self.message, self.path())
        }
    }
}

impl std::error::Error for Error {}

impl de::Error for Error {
    fn custom<T: fmt::Display>(msg: T) -> Self {
        Self {
            message: msg.to_string(),
            path: Vec::new(),
        }
    }
}

/// The config deserializer. See the module docs.
pub(super) enum ConfigDe {
    /// Read this value.
    Value(toml::Value),
    /// Walk the type and record its fields.
    Schema(SchemaDeserializer),
}

impl ConfigDe {
    fn key(key: &str) -> Self {
        Self::Value(toml::Value::String(key.to_owned()))
    }
}

/// Read a value. Every type hint comes here, as in toml: the value decides.
fn read_value<'de, V: Visitor<'de>>(value: toml::Value, visitor: V) -> Result<V::Value, Error> {
    match value {
        toml::Value::String(v) => visitor.visit_string(v),
        toml::Value::Integer(v) => visitor.visit_i64(v),
        toml::Value::Float(v) => visitor.visit_f64(v),
        toml::Value::Boolean(v) => visitor.visit_bool(v),
        toml::Value::Datetime(v) => {
            let mut table = toml::Table::new();
            table.insert(DATETIME_KEY.to_owned(), toml::Value::String(v.to_string()));
            visitor.visit_map(&mut Entries::table(table))
        }
        // As in toml, a visitor can stop before the last item.
        toml::Value::Array(v) => visitor.visit_seq(&mut Items::Array {
            iter: v.into_iter(),
            index: 0,
        }),
        toml::Value::Table(v) => visitor.visit_map(&mut Entries::table(v)),
    }
}

/// The serde name of a value's type, for an error.
fn unexpected(value: &toml::Value) -> de::Unexpected<'_> {
    match value {
        toml::Value::String(v) => de::Unexpected::Str(v),
        toml::Value::Integer(v) => de::Unexpected::Signed(*v),
        toml::Value::Float(v) => de::Unexpected::Float(*v),
        toml::Value::Boolean(v) => de::Unexpected::Bool(*v),
        toml::Value::Datetime(_) => de::Unexpected::Other("datetime"),
        toml::Value::Array(_) => de::Unexpected::Seq,
        toml::Value::Table(_) => de::Unexpected::Map,
    }
}

/// Probe a value of unknown shape in the schema walk.
///
/// A scalar probe is the default. `get_schema_keys` escalates a path whose
/// visitor rejects the probe (Str, then Map, then Seq) and walks again.
fn probe_any<'de, V: Visitor<'de>>(
    walk: SchemaDeserializer,
    visitor: V,
) -> Result<V::Value, Error> {
    let path = walk.path.join(".");
    let probe = walk
        .any_probe
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&path)
        .copied()
        .unwrap_or(AnyProbe::Str);
    let rejected = walk.rejected.clone();
    let result = match probe {
        // "0" is a string, and it also parses as an integer or a duration.
        AnyProbe::Str => visitor.visit_str("0"),
        // An empty map or sequence. The field is a leaf, and the walk goes on.
        AnyProbe::Map => visitor.visit_map(&mut Entries::fields(&[], walk)),
        AnyProbe::Seq => visitor.visit_seq(&mut Items::Probe { done: true, walk }),
    };
    if result.is_err() {
        rejected
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(path);
    }
    result
}

/// One `deserialize_*` method: a value reads itself, the walk gives `$probe`.
macro_rules! hinted {
    ($($method:ident => |$visitor:ident| $probe:expr;)*) => {$(
        fn $method<V: Visitor<'de>>(self, $visitor: V) -> Result<V::Value, Error> {
            match self {
                Self::Value(value) => read_value(value, $visitor),
                Self::Schema(_) => $probe,
            }
        }
    )*};
}

impl<'de> de::Deserializer<'de> for ConfigDe {
    type Error = Error;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        match self {
            Self::Value(value) => read_value(value, visitor),
            Self::Schema(walk) => probe_any(walk, visitor),
        }
    }

    hinted! {
        deserialize_bool => |visitor| visitor.visit_bool(false);
        deserialize_i8 => |visitor| visitor.visit_i8(0);
        deserialize_i16 => |visitor| visitor.visit_i16(0);
        deserialize_i32 => |visitor| visitor.visit_i32(0);
        deserialize_i64 => |visitor| visitor.visit_i64(0);
        deserialize_i128 => |visitor| visitor.visit_i128(0);
        deserialize_u8 => |visitor| visitor.visit_u8(0);
        deserialize_u16 => |visitor| visitor.visit_u16(0);
        deserialize_u32 => |visitor| visitor.visit_u32(0);
        deserialize_u64 => |visitor| visitor.visit_u64(0);
        deserialize_u128 => |visitor| visitor.visit_u128(0);
        deserialize_f32 => |visitor| visitor.visit_f32(0.0);
        deserialize_f64 => |visitor| visitor.visit_f64(0.0);
        deserialize_char => |visitor| visitor.visit_char('\0');
        deserialize_str => |visitor| visitor.visit_str("");
        deserialize_string => |visitor| visitor.visit_string(String::new());
        deserialize_bytes => |visitor| visitor.visit_bytes(&[]);
        deserialize_byte_buf => |visitor| visitor.visit_byte_buf(Vec::new());
        deserialize_unit => |visitor| visitor.visit_unit();
        deserialize_identifier => |visitor| visitor.visit_str("");
    }

    // A present value is `Some`. A missing key is `None`, as in toml.
    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_some(self)
    }

    fn deserialize_unit_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Error> {
        match self {
            Self::Value(value) => read_value(value, visitor),
            Self::Schema(_) => visitor.visit_unit(),
        }
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Error> {
        visitor.visit_newtype_struct(self)
    }

    fn deserialize_seq<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        match self {
            Self::Value(value) => read_value(value, visitor),
            Self::Schema(walk) => visitor.visit_seq(&mut Items::Probe { done: false, walk }),
        }
    }

    fn deserialize_tuple<V: Visitor<'de>>(
        self,
        _len: usize,
        visitor: V,
    ) -> Result<V::Value, Error> {
        self.deserialize_seq(visitor)
    }

    fn deserialize_tuple_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _len: usize,
        visitor: V,
    ) -> Result<V::Value, Error> {
        self.deserialize_seq(visitor)
    }

    fn deserialize_map<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        match self {
            Self::Value(value) => read_value(value, visitor),
            Self::Schema(walk) => visitor.visit_map(&mut Entries::fields(&[], walk)),
        }
    }

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Error> {
        match self {
            Self::Value(value) => read_value(value, visitor),
            Self::Schema(walk) => {
                walk.schema
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .insert(
                        walk.path.join("."),
                        fields.iter().map(|&s| s.to_owned()).collect(),
                    );
                visitor.visit_map(&mut Entries::fields(fields, walk))
            }
        }
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        _name: &'static str,
        variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Error> {
        match self {
            Self::Value(toml::Value::String(name)) => {
                visitor.visit_enum(Variant::Value { name, value: None })
            }
            Self::Value(toml::Value::Table(table)) => {
                let len = table.len();
                let mut entries = table.into_iter();
                match (entries.next(), entries.next()) {
                    (Some((name, value)), None) => visitor.visit_enum(Variant::Value {
                        name,
                        value: Some(value),
                    }),
                    _ => Err(de::Error::custom(format!(
                        "wanted exactly 1 element, found {len} elements"
                    ))),
                }
            }
            Self::Value(value) => Err(de::Error::invalid_type(
                unexpected(&value),
                &"string or table",
            )),
            // The first real variant name. An empty name is an unknown
            // variant, and that error stops the walk for every later
            // section. The walk does not go into the variant.
            Self::Schema(_) => visitor.visit_enum(Variant::Schema(
                variants.first().copied().unwrap_or_default(),
            )),
        }
    }

    fn deserialize_ignored_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        drop(self);
        visitor.visit_unit()
    }
}

/// Map access: the entries of a table, or the fields of a walked struct.
/// A datetime is a one-entry table.
enum Entries {
    Table {
        iter: toml::map::IntoIter<String, toml::Value>,
        value: Option<(String, toml::Value)>,
    },
    Fields {
        fields: std::slice::Iter<'static, &'static str>,
        current: Option<&'static str>,
        walk: SchemaDeserializer,
    },
}

impl Entries {
    fn table(table: toml::Table) -> Self {
        Self::Table {
            iter: table.into_iter(),
            value: None,
        }
    }

    fn fields(fields: &'static [&'static str], walk: SchemaDeserializer) -> Self {
        Self::Fields {
            fields: fields.iter(),
            current: None,
            walk,
        }
    }
}

impl<'de> MapAccess<'de> for Entries {
    type Error = Error;

    fn next_key_seed<K: DeserializeSeed<'de>>(
        &mut self,
        seed: K,
    ) -> Result<Option<K::Value>, Error> {
        match self {
            Self::Table { iter, value } => {
                let Some((key, next)) = iter.next() else {
                    return Ok(None);
                };
                // A key error names the key itself, so the path stops at the
                // table.
                let out = seed.deserialize(ConfigDe::key(&key))?;
                *value = Some((key, next));
                Ok(Some(out))
            }
            Self::Fields {
                fields, current, ..
            } => {
                let Some(&field) = fields.next() else {
                    return Ok(None);
                };
                *current = Some(field);
                seed.deserialize(ConfigDe::key(field)).map(Some)
            }
        }
    }

    fn next_value_seed<V: DeserializeSeed<'de>>(&mut self, seed: V) -> Result<V::Value, Error> {
        match self {
            Self::Table { value, .. } => {
                let Some((key, next)) = value.take() else {
                    return Err(de::Error::custom("value is missing"));
                };
                seed.deserialize(ConfigDe::Value(next))
                    .map_err(|e| e.at(Segment::Key(key)))
            }
            Self::Fields { current, walk, .. } => {
                let Some(field) = current.take() else {
                    return Err(de::Error::custom("value is missing"));
                };
                let mut path = walk.path.clone();
                path.push(field.to_owned());
                seed.deserialize(ConfigDe::Schema(SchemaDeserializer {
                    path,
                    ..walk.clone()
                }))
            }
        }
    }

    fn size_hint(&self) -> Option<usize> {
        match self {
            Self::Table { iter, .. } => Some(iter.len()),
            Self::Fields { fields, .. } => Some(fields.len()),
        }
    }
}

/// Sequence access: the items of an array, or one probe item in the walk.
enum Items {
    Array {
        iter: std::vec::IntoIter<toml::Value>,
        index: usize,
    },
    Probe {
        done: bool,
        walk: SchemaDeserializer,
    },
}

impl<'de> SeqAccess<'de> for Items {
    type Error = Error;

    fn next_element_seed<T: DeserializeSeed<'de>>(
        &mut self,
        seed: T,
    ) -> Result<Option<T::Value>, Error> {
        match self {
            Self::Array { iter, index } => {
                let Some(value) = iter.next() else {
                    return Ok(None);
                };
                let at = *index;
                *index = at.saturating_add(1);
                seed.deserialize(ConfigDe::Value(value))
                    .map(Some)
                    .map_err(|e| e.at(Segment::Index(at)))
            }
            Self::Probe { done, walk } => {
                if *done {
                    return Ok(None);
                }
                *done = true;
                seed.deserialize(ConfigDe::Schema(walk.clone())).map(Some)
            }
        }
    }

    fn size_hint(&self) -> Option<usize> {
        match self {
            Self::Array { iter, .. } => Some(iter.len()),
            Self::Probe { done, .. } => Some(usize::from(!*done)),
        }
    }
}

/// Enum access: a variant name and its payload, or a walked enum.
enum Variant {
    Value {
        name: String,
        value: Option<toml::Value>,
    },
    /// The walk reports this variant and does not go into it.
    Schema(&'static str),
}

impl<'de> EnumAccess<'de> for Variant {
    type Error = Error;
    type Variant = Self;

    fn variant_seed<V: DeserializeSeed<'de>>(self, seed: V) -> Result<(V::Value, Self), Error> {
        let name = match &self {
            Self::Value { name, .. } => seed.deserialize(ConfigDe::key(name))?,
            Self::Schema(name) => seed.deserialize(ConfigDe::key(name))?,
        };
        Ok((name, self))
    }
}

impl<'de> VariantAccess<'de> for Variant {
    type Error = Error;

    fn unit_variant(self) -> Result<(), Error> {
        match self {
            Self::Value { value: None, .. } | Self::Schema(_) => Ok(()),
            Self::Value {
                value: Some(toml::Value::Table(table)),
                ..
            } if table.is_empty() => Ok(()),
            Self::Value {
                name,
                value: Some(value),
            } => {
                let err: Error = de::Error::invalid_type(unexpected(&value), &"unit variant");
                Err(err.at(Segment::Key(name)))
            }
        }
    }

    fn newtype_variant_seed<T: DeserializeSeed<'de>>(self, seed: T) -> Result<T::Value, Error> {
        match self {
            Self::Value {
                name,
                value: Some(value),
            } => seed
                .deserialize(ConfigDe::Value(value))
                .map_err(|e| e.at(Segment::Key(name))),
            Self::Value { value: None, .. } => Err(de::Error::invalid_type(
                de::Unexpected::UnitVariant,
                &"newtype variant",
            )),
            // A fresh walk: the payload's fields are not recorded.
            Self::Schema(_) => seed.deserialize(ConfigDe::Schema(SchemaDeserializer::new())),
        }
    }

    fn tuple_variant<V: Visitor<'de>>(self, _len: usize, visitor: V) -> Result<V::Value, Error> {
        self.compound_variant(visitor, &[], "tuple variant")
    }

    fn struct_variant<V: Visitor<'de>>(
        self,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Error> {
        self.compound_variant(visitor, fields, "struct variant")
    }
}

impl Variant {
    /// Read a tuple or struct variant. A struct variant refuses a key that
    /// is not one of its `fields`, as toml does.
    fn compound_variant<'de, V: Visitor<'de>>(
        self,
        visitor: V,
        fields: &'static [&'static str],
        expected: &'static str,
    ) -> Result<V::Value, Error> {
        match self {
            Self::Value {
                name,
                value: Some(value),
            } => check_struct_keys(&value, fields)
                .and_then(|()| read_value(value, visitor))
                .map_err(|e| e.at(Segment::Key(name))),
            Self::Value { value: None, .. } => Err(de::Error::invalid_type(
                de::Unexpected::UnitVariant,
                &expected,
            )),
            Self::Schema(_) => visitor.visit_unit(),
        }
    }
}

/// Refuse a table key that the struct variant does not have.
fn check_struct_keys(value: &toml::Value, fields: &'static [&'static str]) -> Result<(), Error> {
    let toml::Value::Table(table) = value else {
        return Ok(());
    };
    if fields.is_empty() {
        return Ok(());
    }
    let extra: Vec<&str> = table
        .keys()
        .map(String::as_str)
        .filter(|key| !fields.contains(key))
        .collect();
    if extra.is_empty() {
        return Ok(());
    }
    Err(de::Error::custom(format!(
        "unexpected keys in table: {}, available keys: {}",
        extra.join(", "),
        fields.join(", ")
    )))
}

#[cfg(test)]
mod tests {
    use super::from_value;
    use serde::Deserialize;
    use std::collections::BTreeMap;

    fn value(text: &str) -> toml::Value {
        toml::Value::Table(toml::from_str::<toml::Table>(text).expect("valid TOML"))
    }

    fn parse<T: serde::de::DeserializeOwned>(text: &str) -> Result<T, toml::de::Error> {
        from_value(value(text))
    }

    #[derive(Debug, Deserialize, PartialEq)]
    struct Server {
        port: u16,
        host: String,
        #[serde(default)]
        tls: Option<bool>,
    }

    #[derive(Debug, Deserialize, PartialEq)]
    struct Root {
        server: Server,
    }

    #[test]
    fn reads_nested_tables_and_scalars() {
        let root: Root = parse("[server]\nport = 8080\nhost = \"0.0.0.0\"\n").unwrap();
        assert_eq!(
            root,
            Root {
                server: Server {
                    port: 8080,
                    host: "0.0.0.0".to_owned(),
                    tls: None,
                },
            }
        );
    }

    #[test]
    fn an_error_names_the_key_path() {
        let err = parse::<Root>("[server]\nport = \"abc\"\nhost = \"x\"\n").unwrap_err();
        let text = err.to_string();
        assert!(text.contains("invalid type"), "{text}");
        assert!(text.contains("in `server.port`"), "{text}");
    }

    #[test]
    fn an_out_of_range_integer_is_an_error() {
        let err = parse::<Root>("[server]\nport = 70000\nhost = \"x\"\n").unwrap_err();
        assert!(err.to_string().contains("in `server.port`"), "{err}");
    }

    #[test]
    fn a_missing_field_names_its_table() {
        let err = parse::<Root>("[server]\nhost = \"x\"\n").unwrap_err();
        let text = err.to_string();
        assert!(text.contains("missing field `port`"), "{text}");
        assert!(text.contains("in `server`"), "{text}");
    }

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    #[allow(dead_code, reason = "only the error is read")]
    struct Strict {
        name: String,
    }

    #[test]
    fn an_unknown_field_names_its_table() {
        #[derive(Debug, Deserialize)]
        #[allow(dead_code, reason = "only the error is read")]
        struct Outer {
            inner: Strict,
        }
        let err = parse::<Outer>("[inner]\nname = \"a\"\nnmae = \"b\"\n").unwrap_err();
        let text = err.to_string();
        assert!(text.contains("unknown field `nmae`"), "{text}");
        assert!(text.contains("in `inner`"), "{text}");
    }

    #[derive(Debug, Deserialize, PartialEq)]
    struct Queue {
        name: String,
        workers: u32,
    }

    #[derive(Debug, Deserialize, PartialEq)]
    struct Jobs {
        queues: Vec<Queue>,
    }

    #[test]
    fn reads_arrays_of_tables() {
        let jobs: Jobs =
            parse("[[queues]]\nname = \"a\"\nworkers = 1\n[[queues]]\nname = \"b\"\nworkers = 2\n")
                .unwrap();
        assert_eq!(jobs.queues.len(), 2);
        assert_eq!(jobs.queues[1].name, "b");
    }

    #[test]
    fn an_error_in_an_array_names_the_index() {
        let err = parse::<Jobs>(
            "[[queues]]\nname = \"a\"\nworkers = 1\n[[queues]]\nname = \"b\"\nworkers = -2\n",
        )
        .unwrap_err();
        assert!(err.to_string().contains("in `queues[1].workers`"), "{err}");
    }

    #[derive(Debug, Deserialize, PartialEq)]
    #[serde(rename_all = "snake_case")]
    enum Mode {
        Off,
        Fixed(u32),
        Window { size: u32 },
    }

    #[derive(Debug, Deserialize, PartialEq)]
    struct Modes {
        a: Mode,
        b: Mode,
        c: Mode,
    }

    #[test]
    fn reads_unit_newtype_and_struct_variants() {
        let modes: Modes = parse("a = \"off\"\n[b]\nfixed = 3\n[c.window]\nsize = 9\n").unwrap();
        assert_eq!(
            modes,
            Modes {
                a: Mode::Off,
                b: Mode::Fixed(3),
                c: Mode::Window { size: 9 },
            }
        );
    }

    #[test]
    fn an_unknown_variant_is_an_error() {
        let err = parse::<Modes>("a = \"of\"\nb = \"off\"\nc = \"off\"\n").unwrap_err();
        let text = err.to_string();
        assert!(text.contains("unknown variant `of`"), "{text}");
        assert!(text.contains("in `a`"), "{text}");
    }

    #[derive(Debug, Deserialize, PartialEq)]
    #[serde(untagged)]
    enum SecsOrText {
        Secs(u64),
        Text(String),
    }

    #[derive(Debug, Deserialize, PartialEq)]
    struct Common {
        level: String,
    }

    #[derive(Debug, Deserialize, PartialEq)]
    struct Flat {
        #[serde(flatten)]
        common: Common,
        timeout: SecsOrText,
        other: SecsOrText,
        #[serde(default)]
        extra: BTreeMap<String, u8>,
    }

    #[test]
    fn reads_untagged_flattened_and_map_fields() {
        let flat: Flat =
            parse("level = \"info\"\ntimeout = 30\nother = \"5s\"\n[extra]\nx = 1\n").unwrap();
        assert_eq!(flat.common.level, "info");
        assert_eq!(flat.timeout, SecsOrText::Secs(30));
        assert_eq!(flat.other, SecsOrText::Text("5s".to_owned()));
        assert_eq!(flat.extra.get("x"), Some(&1));
    }

    #[test]
    fn a_datetime_stays_a_datetime_in_a_raw_value() {
        #[derive(Debug, Deserialize)]
        struct Raw {
            section: toml::Value,
            at: toml::value::Datetime,
        }
        let raw: Raw = parse("at = 2026-11-10T08:00:00Z\n[section]\nsince = 2026-01-02\n").unwrap();
        assert_eq!(raw.at.to_string(), "2026-11-10T08:00:00Z");
        let since = raw.section.get("since").expect("since");
        assert!(matches!(since, toml::Value::Datetime(_)), "{since:?}");
    }

    #[test]
    fn floats_and_bools_read_through() {
        #[derive(Debug, Deserialize, PartialEq)]
        struct S {
            ratio: f64,
            whole_as_float: f64,
            on: bool,
        }
        let s: S = parse("ratio = 0.25\nwhole_as_float = 2\non = true\n").unwrap();
        assert_eq!(
            s,
            S {
                ratio: 0.25,
                whole_as_float: 2.0,
                on: true,
            }
        );
    }

    /// The new path gives the same `AutumnConfig` as `toml::from_str` for
    /// every real config file in the repository.
    #[test]
    fn matches_toml_from_str_for_every_example_config() {
        let examples = std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../examples"));
        let mut files = Vec::new();
        for entry in std::fs::read_dir(examples).expect("examples dir") {
            let dir = entry.expect("entry").path();
            let Ok(inner) = std::fs::read_dir(&dir) else {
                continue;
            };
            for file in inner {
                let path = file.expect("entry").path();
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                let toml = path.extension().is_some_and(|ext| ext == "toml");
                if name.starts_with("autumn") && toml {
                    files.push(path);
                }
            }
        }
        assert!(
            files.len() >= 10,
            "found only {} example configs",
            files.len()
        );
        for path in files {
            let text = std::fs::read_to_string(&path).expect("read config");
            let label = path.display().to_string();
            if let Err(err) = toml::from_str::<crate::config::AutumnConfig>(&text) {
                panic!("{label} must be a valid config: {err}");
            }
            assert_same_config(&text, &label);
        }
    }

    #[test]
    fn matches_toml_from_str_for_the_profile_defaults() {
        for profile in ["dev", "prod"] {
            let defaults = super::super::profile_defaults_as_toml(profile);
            let text = toml::to_string(&defaults).expect("serialize defaults");
            assert_same_config(&text, profile);
        }
    }

    #[test]
    fn matches_toml_from_str_for_mixed_shapes() {
        let text = r#"
role = "web"

[server]
port = 4000
host = "127.0.0.1"
shutdown_timeout_secs = 12

[log]
level = "warn"
format = "Json"

[database]
url = "postgres://localhost/app"
pool_size = 4
statement_timeout = "15s"

[jobs]
backend = "local"

[[slo]]
name = "checkout"
objective = 99.5
sli = "availability"
"#;
        if let Err(err) = toml::from_str::<crate::config::AutumnConfig>(text) {
            panic!("the sample must be a valid config: {err}");
        }
        assert_same_config(text, "mixed shapes");
    }

    /// `load` merges the layers into one table. A type error in that table
    /// names the full key, also when the section header is far from the key.
    #[test]
    fn load_names_the_key_of_a_bad_value() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("autumn.toml"),
            "[server]\nhost = \"127.0.0.1\"\nport = \"eighty\"\n",
        )
        .expect("write config");
        let env = crate::config::MockEnv::new()
            .with("AUTUMN_MANIFEST_DIR", dir.path().to_str().expect("utf-8"));
        let err = crate::config::AutumnConfig::load_with_env(&env).unwrap_err();
        let text = err.to_string();
        assert!(text.starts_with("invalid autumn.toml: "), "{text}");
        assert!(text.contains("in `server.port`"), "{text}");
    }

    #[test]
    fn a_datetime_into_a_string_field_is_an_error_as_in_toml() {
        #[derive(Debug, Deserialize)]
        #[allow(dead_code, reason = "only the error is read")]
        struct S {
            at: String,
        }
        let text = "at = 2026-11-10T08:00:00Z\n";
        assert!(toml::from_str::<S>(text).is_err());
        let err = parse::<S>(text).unwrap_err().to_string();
        assert!(err.contains("in `at`"), "{err}");
    }

    #[test]
    fn a_unit_variant_with_a_payload_is_an_error() {
        let err = parse::<Modes>("b = \"off\"\nc = \"off\"\n[a.off]\nx = 1\n").unwrap_err();
        assert!(err.to_string().contains("in `a.off`"), "{err}");
    }

    #[test]
    fn an_enum_of_the_wrong_type_names_the_type() {
        let err = parse::<Modes>("a = 1\nb = \"off\"\nc = \"off\"\n").unwrap_err();
        let text = err.to_string();
        assert!(
            text.contains("integer `1`, expected string or table"),
            "{text}"
        );
    }

    /// toml refuses an unknown key in a struct variant. So does this path.
    #[test]
    fn a_struct_variant_refuses_an_unknown_key() {
        let text = "a = \"off\"\nb = \"off\"\n[c.window]\nsize = 9\nsiez = 1\n";
        assert!(toml::from_str::<Modes>(text).is_err());
        let err = parse::<Modes>(text).unwrap_err().to_string();
        assert!(err.contains("unexpected keys in table: siez"), "{err}");
        assert!(err.contains("in `c.window`"), "{err}");
    }

    #[test]
    fn load_from_reads_a_file_and_names_the_key_of_a_bad_value() {
        use crate::config::{AutumnConfig, ConfigError};
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("autumn.toml");
        std::fs::write(&path, "[server]\nport = 4000\n").expect("write");
        assert_eq!(
            AutumnConfig::load_from(&path).expect("load").server.port,
            4000
        );
        std::fs::write(&path, "[server]\nport = \"x\"\n").expect("write");
        let err = AutumnConfig::load_from(&path).unwrap_err().to_string();
        assert!(err.contains("in `server.port`"), "{err}");
        std::fs::write(&path, "[server\n").expect("write");
        assert!(matches!(
            AutumnConfig::load_from(&path),
            Err(ConfigError::Parse(_))
        ));
    }

    /// The public `SchemaDeserializer` runs the walk of `get_schema_keys`.
    #[test]
    fn the_public_schema_deserializer_records_the_same_schema() {
        use serde::Deserialize as _;
        let walk = crate::config::SchemaDeserializer::new();
        let _ = crate::config::AutumnConfig::deserialize(walk.clone());
        let schema = walk.into_schema();
        assert!(
            schema.get("server").is_some_and(|f| f.contains("port")),
            "{schema:?}"
        );
        // `get_schema_keys` can walk again, so compare one table.
        let full = crate::config::AutumnConfig::get_schema_keys();
        assert_eq!(schema.get("server"), full.get("server"));
    }

    fn walk() -> super::ConfigDe {
        super::ConfigDe::Schema(crate::config::SchemaDeserializer::new())
    }

    #[test]
    fn an_enum_error_names_each_value_type() {
        let rest = "b = \"off\"\nc = \"off\"\n";
        for (head, expected) in [
            ("a = 1.5\n", "floating point `1.5`"),
            ("a = true\n", "boolean `true`"),
            ("a = 2026-01-02\n", "datetime"),
            ("a = [1]\n", "sequence"),
            (
                "a = { off = \"x\" }\n",
                "string \"x\", expected unit variant",
            ),
        ] {
            let err = parse::<Modes>(&format!("{head}{rest}"))
                .unwrap_err()
                .to_string();
            assert!(err.contains(expected), "{head}: {err}");
        }
    }

    #[test]
    fn an_enum_table_needs_exactly_one_key() {
        let rest = "b = \"off\"\nc = \"off\"\n";
        let two = parse::<Modes>(&format!("a = {{ off = {{}}, fixed = 1 }}\n{rest}"));
        assert!(two.unwrap_err().to_string().contains("found 2 elements"));
        let none = parse::<Modes>(&format!("a = {{}}\n{rest}"));
        assert!(none.unwrap_err().to_string().contains("found 0 elements"));
    }

    #[test]
    fn a_unit_variant_may_have_an_empty_table() {
        let modes: Modes = parse("a = { off = {} }\nb = \"off\"\nc = \"off\"\n").unwrap();
        assert_eq!(modes.a, Mode::Off);
    }

    #[test]
    fn a_variant_without_its_payload_is_an_error() {
        let newtype = parse::<Modes>("a = \"off\"\nb = \"fixed\"\nc = \"off\"\n");
        assert!(newtype.unwrap_err().to_string().contains("newtype variant"));
        let strukt = parse::<Modes>("a = \"off\"\nb = \"off\"\nc = \"window\"\n");
        assert!(strukt.unwrap_err().to_string().contains("struct variant"));
        let scalar = parse::<Modes>("a = \"off\"\nb = \"off\"\n[c]\nwindow = 3\n");
        assert!(scalar.unwrap_err().to_string().contains("in `c.window`"));
    }

    #[derive(Debug, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
    #[serde(rename_all = "snake_case")]
    enum Shape {
        Pair(u8, u8),
        Dot,
    }

    #[derive(Debug, Deserialize, PartialEq)]
    struct Shapes {
        s: Shape,
    }

    #[test]
    fn reads_a_tuple_variant() {
        let shapes: Shapes = parse("s = { pair = [1, 2] }\n").unwrap();
        assert_eq!(shapes.s, Shape::Pair(1, 2));
        let bare = parse::<Shapes>("s = \"pair\"\n").unwrap_err().to_string();
        assert!(bare.contains("tuple variant"), "{bare}");
        let item = parse::<Shapes>("s = { pair = [1, \"x\"] }\n")
            .unwrap_err()
            .to_string();
        assert!(item.contains("in `s.pair[1]`"), "{item}");
        let table = parse::<Shapes>("s = { pair = { x = 1 } }\n")
            .unwrap_err()
            .to_string();
        assert!(table.contains("in `s.pair`"), "{table}");
        assert_eq!(parse::<Shapes>("s = \"dot\"\n").unwrap().s, Shape::Dot);
    }

    #[derive(Debug, Deserialize, PartialEq)]
    struct Marker;

    #[derive(Debug, Deserialize, PartialEq)]
    struct Named(u8, u8);

    #[test]
    fn reads_tuples_and_refuses_a_value_for_a_unit_struct() {
        #[derive(Debug, Deserialize, PartialEq)]
        struct S {
            pair: (u8, u8),
            named: Named,
        }
        let s: S = parse("pair = [1, 2]\nnamed = [3, 4]\n").unwrap();
        assert_eq!(s.named, Named(3, 4));
        assert_eq!(s.pair, (1, 2));
        let marker = Marker::deserialize(super::ConfigDe::Value(toml::Value::Boolean(true)));
        assert!(marker.is_err());
    }

    /// A struct is a table. toml also reads a struct from an array, one item
    /// for each field in order. No config file is written that way, and the
    /// code for it is one function for each config struct. This path refuses
    /// the array.
    #[test]
    fn a_struct_is_a_table_and_not_an_array() {
        let text = "server = [8080, \"0.0.0.0\"]\n";
        assert!(toml::from_str::<Root>(text).is_ok(), "toml reads the array");
        let err = parse::<Root>(text).unwrap_err().to_string();
        assert!(
            err.contains("invalid type: sequence, expected struct Server"),
            "{err}"
        );
        assert!(err.contains("in `server`"), "{err}");
    }

    /// A scalar where a table must be gives the same error as before.
    #[test]
    fn a_struct_from_a_scalar_names_both_types() {
        let err = parse::<Root>("server = \"x\"\n").unwrap_err().to_string();
        assert!(
            err.contains("invalid type: string \"x\", expected struct Server"),
            "{err}"
        );
        assert!(err.contains("in `server`"), "{err}");
    }

    /// toml parses a bool, integer or char map key from its text. So does this
    /// path.
    #[test]
    fn map_keys_parse_as_in_toml() {
        #[derive(Debug, Deserialize, PartialEq)]
        struct Keys {
            ports: BTreeMap<u16, String>,
            flags: BTreeMap<bool, u8>,
            marks: BTreeMap<char, u8>,
            modes: BTreeMap<Shape, u8>,
            signed: BTreeMap<i64, u8>,
        }
        let text = "[ports]\n8080 = \"web\"\n[flags]\ntrue = 1\n[marks]\nx = 2\n\
                    [modes]\ndot = 3\n[signed]\n-4 = 5\n";
        let expected: Keys = toml::from_str(text).expect("toml reads the keys");
        let actual: Keys = parse(text).expect("the config path reads the keys");
        assert_eq!(actual, expected);
        assert_eq!(actual.ports.get(&8080).map(String::as_str), Some("web"));
    }

    /// A key that is not a value of the key type is an error in toml and here.
    /// The error names the table, as for an unknown field.
    #[test]
    fn a_bad_map_key_is_an_error_that_names_its_table() {
        #[derive(Debug, Deserialize)]
        #[allow(dead_code, reason = "only the error is read")]
        struct Keys {
            ports: BTreeMap<u16, String>,
        }
        let text = "[ports]\nhttp = \"web\"\n";
        assert!(toml::from_str::<Keys>(text).is_err());
        let err = parse::<Keys>(text).unwrap_err().to_string();
        assert!(err.contains("in `ports`"), "{err}");
        let too_large = "[ports]\n70000 = \"web\"\n";
        assert!(toml::from_str::<Keys>(too_large).is_err());
        assert!(parse::<Keys>(too_large).is_err());
    }

    /// A map key is not a variant with a payload.
    #[test]
    fn a_map_key_is_a_unit_variant_only() {
        #[derive(Debug, Deserialize)]
        #[allow(dead_code, reason = "only the error is read")]
        struct Keys {
            modes: BTreeMap<Shape, u8>,
        }
        let text = "[modes]\npair = 1\n";
        assert!(toml::from_str::<Keys>(text).is_err());
        let err = parse::<Keys>(text).unwrap_err().to_string();
        assert!(err.contains("tuple variant"), "{err}");
    }

    /// The schema walk feeds placeholders for every shape and does not stop.
    #[test]
    fn the_schema_walk_handles_every_shape() {
        #[derive(Debug, Deserialize, PartialEq)]
        enum Num {
            Num(u8),
            Off,
        }
        #[derive(Debug, Deserialize)]
        #[allow(dead_code, reason = "only the walk is tested")]
        enum Window {
            Window { size: u32 },
        }
        assert_eq!(Marker::deserialize(walk()).unwrap(), Marker);
        assert_eq!(Num::deserialize(walk()).unwrap(), Num::Num(0));
        // A tuple or struct variant is a leaf: the walk gives a unit.
        assert!(Shape::deserialize(walk()).is_err());
        assert!(Window::deserialize(walk()).is_err());
        // One probe item. A two-item tuple then reports its length.
        assert!(<(u8, u8)>::deserialize(walk()).is_err());
        assert!(Named::deserialize(walk()).is_err());
    }

    #[test]
    fn the_schema_walk_probes_a_sequence_when_asked() {
        let walk = crate::config::SchemaDeserializer::new();
        walk.any_probe
            .lock()
            .unwrap()
            .insert(String::new(), crate::config::AnyProbe::Seq);
        let value = toml::Value::deserialize(super::ConfigDe::Schema(walk)).unwrap();
        assert_eq!(value, toml::Value::Array(Vec::new()));
    }

    /// serde always asks for a key before its value. A value without a key is
    /// an error, not a panic.
    #[test]
    fn a_value_without_a_key_is_an_error() {
        use serde::de::MapAccess as _;
        let mut table = super::Entries::table(toml::Table::new());
        assert_eq!(table.size_hint(), Some(0));
        assert!(table.next_value::<u8>().is_err());
        let mut fields = super::Entries::fields(&[], crate::config::SchemaDeserializer::new());
        assert!(fields.next_value::<u8>().is_err());
    }

    /// The lines of the pretty `Debug` output, sorted. Two `HashMap`s with the
    /// same entries can list them in a different order.
    fn sorted_debug(config: &crate::config::AutumnConfig) -> Vec<String> {
        let mut lines: Vec<String> = format!("{config:#?}").lines().map(str::to_owned).collect();
        lines.sort();
        lines
    }

    fn assert_same_config(text: &str, label: &str) {
        let expected = toml::from_str::<crate::config::AutumnConfig>(text);
        let actual = from_value::<crate::config::AutumnConfig>(value(text));
        match (expected, actual) {
            (Ok(expected), Ok(actual)) => assert_eq!(
                sorted_debug(&expected),
                sorted_debug(&actual),
                "config differs for {label}"
            ),
            (Err(_), Err(_)) => {}
            (expected, actual) => panic!(
                "{label}: toml::from_str gave {:?}, from_value gave {:?}",
                expected.map(|_| ()),
                actual.map(|_| ())
            ),
        }
    }

    /// `toml::from_str` into a struct brings back the copies this module
    /// removes. Outside tests, config and capacity code parses text only
    /// into `toml::Table` or `toml::Value`.
    #[test]
    fn config_code_parses_text_only_into_raw_toml() {
        let sources = [
            ("config.rs", include_str!("../config.rs")),
            ("capacity.rs", include_str!("../capacity.rs")),
        ];
        for (file, src) in sources {
            // A Windows checkout can have CRLF line ends.
            let src = src.replace("\r\n", "\n");
            let body = src
                .split("\n#[cfg(test)]\nmod tests {")
                .next()
                .expect("source");
            assert!(body.len() < src.len(), "{file} has no test module marker");
            for (number, line) in body.lines().enumerate() {
                let Some(at) = line.find("toml::from_str") else {
                    continue;
                };
                let rest = &line[at + "toml::from_str".len()..];
                assert!(
                    rest.starts_with("::<toml::Table>(") || rest.starts_with("::<toml::Value>("),
                    "{file}:{}: name the raw toml type: {}",
                    number + 1,
                    line.trim()
                );
            }
        }
    }
}
