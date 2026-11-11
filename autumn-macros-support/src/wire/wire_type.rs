//! The wire type of one field: what serde puts in the JSON, not the Rust type.
//!
//! `#[derive(WireShape)]` and `#[event]` write it. An IDL export maps it to
//! its own types. The cross-release check compares it, so a change of the
//! JSON form with the same Rust type is a break.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Known format names. A `#[wire(format = …)]` can use others.
pub mod format {
    /// RFC 3339 date and time, with an offset.
    pub const DATE_TIME: &str = "date-time";
    /// ISO 8601 date and time, with no offset.
    pub const LOCAL_DATE_TIME: &str = "local-date-time";
    /// ISO 8601 date.
    pub const DATE: &str = "date";
    /// ISO 8601 time of day, with no offset.
    pub const LOCAL_TIME: &str = "local-time";
    /// A hyphenated UUID.
    pub const UUID: &str = "uuid";
    /// A decimal number written as a string.
    pub const DECIMAL: &str = "decimal";
    /// A URL.
    pub const URI: &str = "uri";
    /// An IPv4 or an IPv6 address.
    pub const IP: &str = "ip";
    /// An IPv4 address.
    pub const IPV4: &str = "ipv4";
    /// An IPv6 address.
    pub const IPV6: &str = "ipv6";
    /// Seconds since the Unix epoch.
    pub const UNIX_SECONDS: &str = "unix-seconds";
    /// Milliseconds since the Unix epoch.
    pub const UNIX_MILLIS: &str = "unix-millis";
}

/// The JSON form of one field.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WireType {
    /// A JSON string. `format` names its text form. See [`mod@format`].
    String {
        /// The text form, such as `date-time` or `uuid`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        format: Option<String>,
    },
    /// A JSON integer.
    Integer {
        /// The width of the Rust integer. An override is 64 bits.
        bits: u8,
        /// Whether the Rust integer is signed.
        signed: bool,
        /// What the number means, such as `unix-seconds`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        format: Option<String>,
    },
    /// A JSON number that can have a fraction.
    Number,
    /// A JSON boolean.
    Boolean,
    /// A JSON array.
    Array {
        /// The type of each item.
        of: Box<Self>,
    },
    /// A JSON object with free keys, from a map type.
    Map {
        /// The type of each value.
        of: Box<Self>,
    },
    /// A nested `WireShape` type.
    Object {
        /// The type's id, `krate::module::Name`, after the export. In a
        /// descriptor that the macro wrote, the path as written.
        type_ref: String,
    },
    /// The value, or JSON `null`.
    Nullable {
        /// The type when not `null`.
        of: Box<Self>,
    },
    /// Any JSON value, or a form the macro cannot read: `serde_json::Value`,
    /// a custom serde function, or an unknown type in an opaque field. The
    /// check does not compare it.
    Any,
}

impl WireType {
    /// A string with no format.
    #[must_use]
    pub const fn string() -> Self {
        Self::String { format: None }
    }

    /// A string in `format`.
    #[must_use]
    pub fn string_in(format: &str) -> Self {
        Self::String {
            format: Some(format.to_owned()),
        }
    }

    /// An integer of `bits` width, with no format.
    #[must_use]
    pub const fn integer(bits: u8, signed: bool) -> Self {
        Self::Integer {
            bits,
            signed,
            format: None,
        }
    }

    /// An array of `of`.
    #[must_use]
    pub fn array(of: Self) -> Self {
        Self::Array { of: Box::new(of) }
    }

    /// A map with values of `of`.
    #[must_use]
    pub fn map(of: Self) -> Self {
        Self::Map { of: Box::new(of) }
    }

    /// `of`, or `null`. A type that is nullable already stays as it is.
    #[must_use]
    pub fn nullable(of: Self) -> Self {
        match of {
            Self::Nullable { .. } => of,
            other => Self::Nullable {
                of: Box::new(other),
            },
        }
    }

    /// A nested `WireShape` type.
    #[must_use]
    pub fn object(type_ref: &str) -> Self {
        Self::Object {
            type_ref: type_ref.to_owned(),
        }
    }

    /// Whether a decoder of this type reads what a writer of `writer` sends.
    ///
    /// The same JSON form reads, and a nullable type reads the type it
    /// wraps. Object ids are not compared: the check compares the fields of
    /// the two types. `Any` gives no data, so it reads and is read by all.
    #[must_use]
    pub fn reads(&self, writer: &Self) -> bool {
        self.same_form(writer) || matches!(self, Self::Nullable { of } if of.same_form(writer))
    }

    /// Whether two types can have the same JSON form. Object ids do not
    /// count, and `Any` matches all.
    fn same_form(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Any, _) | (_, Self::Any) | (Self::Object { .. }, Self::Object { .. }) => true,
            (Self::Array { of: a }, Self::Array { of: b })
            | (Self::Map { of: a }, Self::Map { of: b })
            | (Self::Nullable { of: a }, Self::Nullable { of: b }) => a.same_form(b),
            (a, b) => a == b,
        }
    }

    /// Call `f` on the `type_ref` of each object in this type.
    pub fn for_each_object(&mut self, f: &mut impl FnMut(&mut String)) {
        match self {
            Self::Object { type_ref } => f(type_ref),
            Self::Array { of } | Self::Map { of } | Self::Nullable { of } => of.for_each_object(f),
            Self::String { .. }
            | Self::Integer { .. }
            | Self::Number
            | Self::Boolean
            | Self::Any => {}
        }
    }
}

impl fmt::Display for WireType {
    /// A short form for a report, such as `nullable<string(date-time)>`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::String { format: None } => f.write_str("string"),
            Self::String {
                format: Some(format),
            } => write!(f, "string({format})"),
            Self::Integer {
                bits,
                signed,
                format,
            } => {
                let sign = if *signed { 'i' } else { 'u' };
                match format {
                    Some(format) => write!(f, "integer({sign}{bits}, {format})"),
                    None => write!(f, "integer({sign}{bits})"),
                }
            }
            Self::Number => f.write_str("number"),
            Self::Boolean => f.write_str("boolean"),
            Self::Array { of } => write!(f, "array<{of}>"),
            Self::Map { of } => write!(f, "map<{of}>"),
            Self::Object { type_ref } => write!(f, "object({type_ref})"),
            Self::Nullable { of } => write!(f, "nullable<{of}>"),
            Self::Any => f.write_str("any"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn json(t: &WireType) -> String {
        serde_json::to_string(t).expect("serializes")
    }

    #[test]
    fn each_kind_has_a_tagged_json_form() {
        let cases = [
            (WireType::string(), r#"{"kind":"string"}"#),
            (
                WireType::string_in(format::UUID),
                r#"{"kind":"string","format":"uuid"}"#,
            ),
            (
                WireType::integer(32, false),
                r#"{"kind":"integer","bits":32,"signed":false}"#,
            ),
            (WireType::Number, r#"{"kind":"number"}"#),
            (WireType::Boolean, r#"{"kind":"boolean"}"#),
            (
                WireType::array(WireType::Boolean),
                r#"{"kind":"array","of":{"kind":"boolean"}}"#,
            ),
            (
                WireType::map(WireType::Number),
                r#"{"kind":"map","of":{"kind":"number"}}"#,
            ),
            (
                WireType::object("shop::Item"),
                r#"{"kind":"object","type_ref":"shop::Item"}"#,
            ),
            (
                WireType::nullable(WireType::string()),
                r#"{"kind":"nullable","of":{"kind":"string"}}"#,
            ),
            (WireType::Any, r#"{"kind":"any"}"#),
        ];
        for (wire, want) in cases {
            assert_eq!(json(&wire), want);
            let back: WireType = serde_json::from_str(want).expect("parses");
            assert_eq!(back, wire);
        }
    }

    #[test]
    fn an_unknown_kind_does_not_parse() {
        assert!(serde_json::from_str::<WireType>(r#"{"kind":"tuple"}"#).is_err());
    }

    #[test]
    fn nullable_does_not_wrap_twice() {
        let once = WireType::nullable(WireType::Boolean);
        assert_eq!(WireType::nullable(once.clone()), once);
    }

    #[test]
    fn the_short_form_names_kind_width_and_format() {
        let t = WireType::Integer {
            bits: 64,
            signed: true,
            format: Some(format::UNIX_SECONDS.to_owned()),
        };
        assert_eq!(t.to_string(), "integer(i64, unix-seconds)");
        assert_eq!(
            WireType::nullable(WireType::array(WireType::string_in(format::DATE_TIME))).to_string(),
            "nullable<array<string(date-time)>>"
        );
        assert_eq!(
            WireType::map(WireType::integer(8, false)).to_string(),
            "map<integer(u8)>"
        );
        assert_eq!(WireType::object("a::B").to_string(), "object(a::B)");
        assert_eq!(WireType::Any.to_string(), "any");
    }

    #[test]
    fn a_type_reads_the_same_form_and_a_nullable_reads_its_inner_type() {
        let date = WireType::string_in(format::DATE_TIME);
        let seconds = WireType::Integer {
            bits: 64,
            signed: true,
            format: Some(format::UNIX_SECONDS.to_owned()),
        };
        assert!(date.reads(&date));
        assert!(!date.reads(&seconds), "a representation change");
        assert!(!date.reads(&WireType::string()), "a format change");
        assert!(WireType::nullable(date.clone()).reads(&date));
        assert!(!date.reads(&WireType::nullable(date.clone())), "null");
    }

    #[test]
    fn any_is_not_compared_at_any_depth() {
        let text = WireType::string();
        assert!(WireType::Any.reads(&text));
        assert!(text.reads(&WireType::Any));
        assert!(WireType::array(WireType::Any).reads(&WireType::array(text.clone())));
        assert!(!WireType::array(WireType::Any).reads(&WireType::map(text)));
    }

    #[test]
    fn object_ids_do_not_count_when_forms_are_compared() {
        let a = WireType::array(WireType::object("shop::Item"));
        let b = WireType::array(WireType::object("shop::api::Item"));
        assert!(a.reads(&b));
        assert!(!a.reads(&WireType::array(WireType::string())));
    }

    #[test]
    fn each_object_ref_can_be_rewritten() {
        let mut t = WireType::map(WireType::nullable(WireType::object("Item")));
        t.for_each_object(&mut |r: &mut String| *r = format!("shop::{r}"));
        assert_eq!(
            t,
            WireType::map(WireType::nullable(WireType::object("shop::Item")))
        );
    }
}
