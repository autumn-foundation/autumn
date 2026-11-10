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
/// key as a datetime. A test keeps the two in step.
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
        toml::Value::Array(v) => {
            let len = v.len();
            let mut access = Items::Array {
                iter: v.into_iter(),
                index: 0,
            };
            let out = visitor.visit_seq(&mut access)?;
            if access.is_empty() {
                Ok(out)
            } else {
                Err(de::Error::invalid_length(len, &"fewer elements in array"))
            }
        }
        toml::Value::Table(v) => {
            let len = v.len();
            let mut access = Entries::table(v);
            let out = visitor.visit_map(&mut access)?;
            if access.is_empty() {
                Ok(out)
            } else {
                Err(de::Error::invalid_length(len, &"fewer elements in map"))
            }
        }
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
                if table.len() != 1 {
                    return Err(de::Error::custom(format!(
                        "wanted exactly 1 element, found {} elements",
                        table.len()
                    )));
                }
                let Some((name, value)) = table.into_iter().next() else {
                    return Err(de::Error::custom(
                        "wanted exactly 1 element, found 0 elements",
                    ));
                };
                visitor.visit_enum(Variant::Value {
                    name,
                    value: Some(value),
                })
            }
            Self::Value(_) => Err(de::Error::invalid_type(
                de::Unexpected::UnitVariant,
                &"string only",
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

    fn is_empty(&self) -> bool {
        match self {
            Self::Table { iter, .. } => iter.len() == 0,
            Self::Fields { fields, .. } => fields.len() == 0,
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

impl Items {
    fn is_empty(&self) -> bool {
        match self {
            Self::Array { iter, .. } => iter.len() == 0,
            Self::Probe { done, .. } => *done,
        }
    }
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
            Self::Value { name, .. } => {
                let err: Error = de::Error::invalid_type(de::Unexpected::Map, &"unit variant");
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
        self.compound_variant(visitor, "tuple variant")
    }

    fn struct_variant<V: Visitor<'de>>(
        self,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Error> {
        self.compound_variant(visitor, "struct variant")
    }
}

impl Variant {
    fn compound_variant<'de, V: Visitor<'de>>(
        self,
        visitor: V,
        expected: &'static str,
    ) -> Result<V::Value, Error> {
        match self {
            Self::Value {
                name,
                value: Some(value),
            } => read_value(value, visitor).map_err(|e| e.at(Segment::Key(name))),
            Self::Value { value: None, .. } => Err(de::Error::invalid_type(
                de::Unexpected::UnitVariant,
                &expected,
            )),
            Self::Schema(_) => visitor.visit_unit(),
        }
    }
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
            assert_same_config(&text, &path.display().to_string());
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

    fn assert_same_config(text: &str, label: &str) {
        let expected = toml::from_str::<crate::config::AutumnConfig>(text);
        let actual = from_value::<crate::config::AutumnConfig>(value(text));
        match (expected, actual) {
            (Ok(expected), Ok(actual)) => assert_eq!(
                format!("{expected:?}"),
                format!("{actual:?}"),
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

    /// `toml::from_str` into a config struct brings back the copies this
    /// module removes. Outside tests, config code parses text only into
    /// `toml::Table` or `toml::Value`.
    #[test]
    fn config_code_parses_text_only_into_raw_toml() {
        let src = include_str!("../config.rs");
        let body = src
            .split("\n#[cfg(test)]\nmod tests {")
            .next()
            .expect("source");
        for (number, line) in body.lines().enumerate() {
            let Some(at) = line.find("toml::from_str") else {
                continue;
            };
            let rest = &line[at + "toml::from_str".len()..];
            assert!(
                rest.starts_with("::<toml::Table>(") || rest.starts_with("::<toml::Value>("),
                "config.rs:{}: name the raw toml type: {}",
                number + 1,
                line.trim()
            );
        }
    }
}
