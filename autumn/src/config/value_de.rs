//! One serde deserializer for the merged config table (#2795).
//!
//! `toml::from_str` makes one `Deserialize` copy of each config struct for
//! each toml access type: table, array, datetime and span. This deserializer
//! reads a `toml::Value` through one map type and one sequence type. Each
//! config struct then compiles once, and `autumn-web` builds faster.

use std::fmt;

use serde::de::{
    self, DeserializeOwned, DeserializeSeed, EnumAccess, MapAccess, SeqAccess, VariantAccess,
    Visitor,
};

/// The key that toml uses to carry a datetime through serde.
///
/// `toml::Value` and `toml_datetime::Datetime` read a one-entry map with this
/// key as a datetime. A test keeps the two in step.
const DATETIME_KEY: &str = "$__toml_private_datetime";

/// Deserialize `T` from a merged config value.
///
/// An error names the key path, for example ``in `server.port` ``.
pub(crate) fn from_value<T: DeserializeOwned>(value: toml::Value) -> Result<T, toml::de::Error> {
    T::deserialize(ValueDe(value)).map_err(Error::into_toml)
}

/// An error and the key path where it occurred.
#[derive(Debug)]
pub(crate) struct Error {
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

/// A deserializer over one `toml::Value`.
struct ValueDe(toml::Value);

impl<'de> de::Deserializer<'de> for ValueDe {
    type Error = Error;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        match self.0 {
            toml::Value::String(v) => visitor.visit_string(v),
            toml::Value::Integer(v) => visitor.visit_i64(v),
            toml::Value::Float(v) => visitor.visit_f64(v),
            toml::Value::Boolean(v) => visitor.visit_bool(v),
            toml::Value::Datetime(v) => {
                let mut table = toml::Table::new();
                table.insert(DATETIME_KEY.to_owned(), toml::Value::String(v.to_string()));
                // `&mut`, as for a table: one map type for every visitor.
                visitor.visit_map(&mut TableAccess::new(table))
            }
            toml::Value::Array(v) => {
                let len = v.len();
                let mut access = ArrayAccess::new(v);
                let out = visitor.visit_seq(&mut access)?;
                if access.iter.len() == 0 {
                    Ok(out)
                } else {
                    Err(de::Error::invalid_length(len, &"fewer elements in array"))
                }
            }
            toml::Value::Table(v) => {
                let len = v.len();
                let mut access = TableAccess::new(v);
                let out = visitor.visit_map(&mut access)?;
                if access.iter.len() == 0 {
                    Ok(out)
                } else {
                    Err(de::Error::invalid_length(len, &"fewer elements in map"))
                }
            }
        }
    }

    // A present value is `Some`. A missing key is `None`, as in toml.
    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_some(self)
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Error> {
        visitor.visit_newtype_struct(self)
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Error> {
        match self.0 {
            toml::Value::String(variant) => visitor.visit_enum(Variant {
                name: variant,
                value: None,
            }),
            toml::Value::Table(table) => {
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
                visitor.visit_enum(Variant {
                    name,
                    value: Some(value),
                })
            }
            _ => Err(de::Error::invalid_type(
                de::Unexpected::UnitVariant,
                &"string only",
            )),
        }
    }

    fn deserialize_ignored_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        drop(self);
        visitor.visit_unit()
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
        bytes byte_buf unit unit_struct seq tuple tuple_struct map struct
        identifier
    }
}

/// Map access over a table. A datetime uses it too, as a one-entry table.
struct TableAccess {
    iter: toml::map::IntoIter<String, toml::Value>,
    value: Option<(String, toml::Value)>,
}

impl TableAccess {
    fn new(table: toml::Table) -> Self {
        Self {
            iter: table.into_iter(),
            value: None,
        }
    }
}

impl<'de> MapAccess<'de> for TableAccess {
    type Error = Error;

    fn next_key_seed<K: DeserializeSeed<'de>>(
        &mut self,
        seed: K,
    ) -> Result<Option<K::Value>, Error> {
        let Some((key, value)) = self.iter.next() else {
            return Ok(None);
        };
        // The key goes through the value deserializer, so field-name enums
        // compile once. A key error names the key itself, so the path stops
        // at the table.
        let out = seed.deserialize(ValueDe(toml::Value::String(key.clone())))?;
        self.value = Some((key, value));
        Ok(Some(out))
    }

    fn next_value_seed<V: DeserializeSeed<'de>>(&mut self, seed: V) -> Result<V::Value, Error> {
        let Some((key, value)) = self.value.take() else {
            return Err(de::Error::custom("value is missing"));
        };
        seed.deserialize(ValueDe(value))
            .map_err(|e| e.at(Segment::Key(key)))
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.iter.len())
    }
}

/// Sequence access over an array.
struct ArrayAccess {
    iter: std::vec::IntoIter<toml::Value>,
    index: usize,
}

impl ArrayAccess {
    fn new(array: toml::value::Array) -> Self {
        Self {
            iter: array.into_iter(),
            index: 0,
        }
    }
}

impl<'de> SeqAccess<'de> for ArrayAccess {
    type Error = Error;

    fn next_element_seed<T: DeserializeSeed<'de>>(
        &mut self,
        seed: T,
    ) -> Result<Option<T::Value>, Error> {
        let Some(value) = self.iter.next() else {
            return Ok(None);
        };
        let index = self.index;
        self.index = index.saturating_add(1);
        seed.deserialize(ValueDe(value))
            .map(Some)
            .map_err(|e| e.at(Segment::Index(index)))
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.iter.len())
    }
}

/// Enum access for a variant name and an optional payload.
struct Variant {
    name: String,
    value: Option<toml::Value>,
}

impl<'de> EnumAccess<'de> for Variant {
    type Error = Error;
    type Variant = Self;

    fn variant_seed<V: DeserializeSeed<'de>>(self, seed: V) -> Result<(V::Value, Self), Error> {
        let name = seed.deserialize(ValueDe(toml::Value::String(self.name.clone())))?;
        Ok((name, self))
    }
}

impl<'de> VariantAccess<'de> for Variant {
    type Error = Error;

    fn unit_variant(self) -> Result<(), Error> {
        match self.value {
            None => Ok(()),
            Some(toml::Value::Table(table)) if table.is_empty() => Ok(()),
            Some(_) => {
                let err: Error = de::Error::invalid_type(de::Unexpected::Map, &"unit variant");
                Err(err.at(Segment::Key(self.name)))
            }
        }
    }

    fn newtype_variant_seed<T: DeserializeSeed<'de>>(self, seed: T) -> Result<T::Value, Error> {
        let Variant { name, value } = self;
        match value {
            Some(value) => seed
                .deserialize(ValueDe(value))
                .map_err(|e| e.at(Segment::Key(name))),
            None => Err(de::Error::invalid_type(
                de::Unexpected::UnitVariant,
                &"newtype variant",
            )),
        }
    }

    fn tuple_variant<V: Visitor<'de>>(self, _len: usize, visitor: V) -> Result<V::Value, Error> {
        let Variant { name, value } = self;
        match value {
            Some(value) => de::Deserializer::deserialize_any(ValueDe(value), visitor)
                .map_err(|e| e.at(Segment::Key(name))),
            None => Err(de::Error::invalid_type(
                de::Unexpected::UnitVariant,
                &"tuple variant",
            )),
        }
    }

    fn struct_variant<V: Visitor<'de>>(
        self,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Error> {
        let Variant { name, value } = self;
        match value {
            Some(value) => de::Deserializer::deserialize_any(ValueDe(value), visitor)
                .map_err(|e| e.at(Segment::Key(name))),
            None => Err(de::Error::invalid_type(
                de::Unexpected::UnitVariant,
                &"struct variant",
            )),
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
                if name.starts_with("autumn") && name.ends_with(".toml") {
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
