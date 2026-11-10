//! One serde deserializer for the merged config table (#2795).
//!
//! `toml::from_str` makes one `Deserialize` copy of each config struct for
//! each toml access type: table, array, datetime and span. This deserializer
//! reads a `toml::Value` through one map type and one sequence type. Each
//! config struct then compiles once, and `autumn-web` builds faster.

use serde::de::DeserializeOwned;

/// Deserialize `T` from a merged config value.
///
/// An error names the key path, for example ``in `server.port` ``.
pub(crate) fn from_value<T: DeserializeOwned>(_value: toml::Value) -> Result<T, toml::de::Error> {
    Err(serde::de::Error::custom("not implemented"))
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
