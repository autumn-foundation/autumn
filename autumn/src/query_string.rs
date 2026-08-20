//! Structured query-string decoding for [`Query<T>`](crate::extract::Query)
//! (issue #1972).
//!
//! # Why this exists
//!
//! `Query<T>` used to delegate straight to
//! [`serde_urlencoded`](https://docs.rs/serde_urlencoded), which is **strictly
//! flat**: it decodes `?q=foo&page=2` and nothing else. A `Vec<String>` field
//! fed the repeated-key form `?tags=a&tags=b` failed with `invalid type: string
//! "a", expected a sequence`, and a nested struct field was unrepresentable by
//! any encoding. That left MCP tool contracts unhonorable — `tools/call`
//! dispatch renders an array query argument as repeated keys, so a tool whose
//! `inputSchema` advertised `tags: array` dispatched a request its own handler
//! rejected — and pushed builders onto comma-separated strings and
//! JSON-in-a-string fields.
//!
//! # Wire format
//!
//! The decoder is a **superset** of the flat form: a query string with unique
//! scalar keys decodes exactly as it did before. On top of that it accepts the
//! bracketed dialect the framework already speaks in
//! [`nested_form`](crate::nested_form):
//!
//! ```text
//! q=foo                       // scalar
//! page=2                      // scalar, coerced to the field's type
//! tags=a&tags=b               // repeated key   → sequence ["a", "b"]
//! tags[]=a&tags[]=b           // append form    → sequence ["a", "b"]
//! tags[0]=a&tags[2]=c         // indexed form   → sequence ["a", "c"]
//! filter[status]=open         // nested object  → { status: "open" }
//! items[0][sku]=A-1           // array of objects
//! ```
//!
//! Percent-encoded brackets (`%5B` / `%5D`) are the same thing: keys are
//! percent-decoded before parsing, so a client that encodes them round-trips
//! identically.
//!
//! # Deliberate semantics
//!
//! * **Scalar coercion matches `serde_urlencoded`.** Values arrive as text and
//!   are parsed with [`str::parse`] into the field's type, so `page=2` fills a
//!   `u32` and `flag=true` fills a `bool`. An `Option<T>` field that is
//!   *present but empty* (`?page=`) still visits `Some`, exactly as
//!   `serde_urlencoded` does — only an **absent** key is `None`.
//! * **First occurrence wins for scalars.** `?q=a&q=b` fills a `String` field
//!   with `"a"`. Deserializing the same key into a sequence field yields both.
//! * **Shape conflicts are errors.** `?filter=flat&filter[status]=open` uses
//!   one key as both a scalar and a container; that is rejected rather than
//!   resolved by last-write-wins.
//! * **Malformed brackets stay literal.** A key the bracket grammar cannot
//!   parse (`weird[unclosed`) is used verbatim as a flat key, so a stray
//!   bracket never turns into a parse failure.
//! * **Nesting is depth-capped** at [`MAX_DEPTH`] and indices key an ordered
//!   map rather than a `Vec`, so neither deep nesting nor a huge index
//!   (`tags[4000000000]=x`) lets a request drive unbounded allocation.
//!
//! # Compatibility note
//!
//! Brackets are now *structure*, not part of the key text. A target that types
//! a parameter as a plain value — `Query<HashMap<String, String>>` is the
//! common one — used to receive `?filter[a]=1` as the literal key
//! `"filter[a]"`; it now sees a nested object and reports a decode error
//! naming the fix. Type such a field as a nested struct, or as
//! `HashMap<String, serde_json::Value>` to accept either shape.

// autumn-panic-gate: request-path module — production code path must be panic-free.
// See CONTRIBUTING.md "Request-path panic gate". Justify exceptions with
// #[allow(clippy::<lint>, reason = "…")] at the narrowest scope.
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::todo,
        clippy::unimplemented,
        clippy::indexing_slicing,
        clippy::string_slice,
        clippy::arithmetic_side_effects,
    )
)]

use std::collections::BTreeMap;
use std::fmt;

use serde::de::{
    self, DeserializeOwned, DeserializeSeed, EnumAccess, MapAccess, SeqAccess, VariantAccess,
    Visitor,
};

/// Maximum bracket nesting depth accepted in a query key.
///
/// `a[b][c]` is depth 3. Anything deeper is rejected as a decode error rather
/// than allocated, so a hostile query string cannot drive unbounded tree
/// construction. Real nested query arguments are one or two levels deep.
pub const MAX_DEPTH: usize = 16;

// ──────────────────────────────────────────────────────────────────
// Error
// ──────────────────────────────────────────────────────────────────

/// A query-string decode failure, carrying the field path it occurred at.
///
/// Rendered as `filter.limit: invalid digit found in string` so a caller sees
/// *which* parameter failed, not just that something did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryError {
    /// Field path from the root of the query object, outermost first.
    path: Vec<String>,
    message: String,
}

impl QueryError {
    fn msg(message: impl Into<String>) -> Self {
        Self {
            path: Vec::new(),
            message: message.into(),
        }
    }

    /// Prepend one path segment as the error bubbles out of a nested value.
    fn with_segment(mut self, segment: impl Into<String>) -> Self {
        self.path.insert(0, segment.into());
        self
    }
}

impl fmt::Display for QueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.path.is_empty() {
            f.write_str(&self.message)
        } else {
            write!(f, "{}: {}", self.path.join("."), self.message)
        }
    }
}

impl std::error::Error for QueryError {}

impl de::Error for QueryError {
    fn custom<T: fmt::Display>(msg: T) -> Self {
        Self::msg(msg.to_string())
    }
}

// ──────────────────────────────────────────────────────────────────
// Key parsing
// ──────────────────────────────────────────────────────────────────

/// One bracketed step in a query key.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Segment<'a> {
    /// `k[]` — append at the next free position.
    Append,
    /// `k[3]` — an explicit position.
    Index(usize),
    /// `k[name]` — a named entry.
    Key(&'a str),
}

/// Split `base[seg][seg]…` into its base key and bracketed segments.
///
/// Returns `None` when the remainder after the first `[` is not a well-formed
/// run of bracket groups covering the whole key — the caller then treats the
/// key as a flat literal, so a stray bracket is never a hard error.
fn parse_key(key: &str) -> Option<(&str, Vec<Segment<'_>>)> {
    let open = key.find('[')?;
    let (base, mut rest) = key.split_at(open);
    let mut segments = Vec::new();
    while !rest.is_empty() {
        // Every remaining group must start with `[` and close with `]`.
        let (inner, remainder) = rest.strip_prefix('[')?.split_once(']')?;
        // A nested `[` inside a group means the key is malformed, not nested.
        if inner.contains('[') {
            return None;
        }
        segments.push(classify_segment(inner));
        rest = remainder;
    }
    Some((base, segments))
}

/// Classify one bracket group's contents.
///
/// An all-digit group is a position. Its value is **saturated** rather than
/// wrapped or rejected, so an absurd index (`tags[99999999999999999999]`) still
/// sorts last instead of changing the key's meaning between 32- and 64-bit
/// targets.
fn classify_segment(inner: &str) -> Segment<'_> {
    if inner.is_empty() {
        return Segment::Append;
    }
    if inner.bytes().all(|b| b.is_ascii_digit()) {
        return Segment::Index(inner.parse().unwrap_or(usize::MAX));
    }
    Segment::Key(inner)
}

// ──────────────────────────────────────────────────────────────────
// Tree
// ──────────────────────────────────────────────────────────────────

/// One decoded position in the query tree.
#[derive(Debug)]
enum Node {
    /// Every value submitted under this exact key path, in submission order.
    ///
    /// Holding all of them (rather than collapsing to one) is what lets the
    /// same node serve a scalar field (first wins) and a sequence field (all
    /// values) without the parser needing to know the target type.
    Scalar(Vec<String>),
    /// Positional children (`k[]`, `k[3]`), ordered by index. A `BTreeMap`
    /// keeps sparse and out-of-order indices cheap: `tags[4000000000]` costs
    /// one entry, not four billion.
    Seq(BTreeMap<usize, Self>),
    /// Named children (`k[name]`).
    Map(BTreeMap<String, Self>),
}

impl Node {
    /// Human-readable shape name, for conflict and type-mismatch messages.
    const fn kind(&self) -> &'static str {
        match self {
            Self::Scalar(_) => "a value",
            Self::Seq(_) => "a sequence",
            Self::Map(_) => "an object",
        }
    }
}

/// Render the key path up to (and including) `upto` segments, for error text.
///
/// Built only when a conflict is being reported — the success path never
/// allocates a path string.
fn key_path(base: &str, segments: &[Segment<'_>], upto: usize) -> String {
    let mut out = base.to_owned();
    for segment in segments.iter().take(upto) {
        match segment {
            Segment::Key(name) => {
                out.push('[');
                out.push_str(name);
                out.push(']');
            }
            Segment::Index(index) => {
                out.push('[');
                out.push_str(&index.to_string());
                out.push(']');
            }
            Segment::Append => out.push_str("[]"),
        }
    }
    out
}

/// Insert one decoded `key=value` pair into the tree rooted at `root`.
fn insert(
    root: &mut BTreeMap<String, Node>,
    base: &str,
    segments: &[Segment<'_>],
    value: String,
) -> Result<(), QueryError> {
    // `segments.len()` counts the bracketed steps; the base key is the first
    // level, so the total depth is `len + 1` — expressed as `>=` to stay clear
    // of the request-path gate's arithmetic ban.
    if segments.len() >= MAX_DEPTH {
        return Err(QueryError::msg(format!(
            "query key nesting exceeds the maximum depth of {MAX_DEPTH}"
        ))
        .with_segment(base));
    }

    // Descend to the node this key addresses, creating containers on the way.
    // The *next* segment decides which container the current entry must be, so
    // the shape is always known before the entry is materialized.
    let mut node = root
        .entry(base.to_owned())
        .or_insert_with(|| empty_for(segments.first()));

    for (position, segment) in segments.iter().enumerate() {
        let next = segments.get(position.saturating_add(1));
        match segment {
            Segment::Key(name) => {
                let Node::Map(entries) = node else {
                    return Err(conflict(base, segments, position, node, "an object"));
                };
                node = entries
                    .entry((*name).to_owned())
                    .or_insert_with(|| empty_for(next));
            }
            Segment::Index(index) => {
                let Node::Seq(entries) = node else {
                    return Err(conflict(base, segments, position, node, "a sequence"));
                };
                node = entries.entry(*index).or_insert_with(|| empty_for(next));
            }
            Segment::Append => {
                let Node::Seq(entries) = node else {
                    return Err(conflict(base, segments, position, node, "a sequence"));
                };
                // `k[]` appends after the highest position seen so far, so
                // repeated appends stay in submission order even when mixed
                // with explicit indices.
                let index = entries
                    .keys()
                    .next_back()
                    .map_or(0, |last| last.saturating_add(1));
                node = entries.entry(index).or_insert_with(|| empty_for(next));
            }
        }
    }

    match node {
        Node::Scalar(values) => {
            values.push(value);
            Ok(())
        }
        other => Err(conflict(base, segments, segments.len(), other, "a value")),
    }
}

/// The empty container a segment's *successor* requires.
const fn empty_for(next: Option<&Segment<'_>>) -> Node {
    match next {
        None => Node::Scalar(Vec::new()),
        Some(Segment::Key(_)) => Node::Map(BTreeMap::new()),
        Some(Segment::Append | Segment::Index(_)) => Node::Seq(BTreeMap::new()),
    }
}

fn conflict(
    base: &str,
    segments: &[Segment<'_>],
    upto: usize,
    found: &Node,
    wanted: &str,
) -> QueryError {
    QueryError::msg(format!(
        "query key `{}` is used as both {} and {wanted}",
        key_path(base, segments, upto),
        found.kind()
    ))
}

/// Build the query tree from an already-percent-decoded pair sequence.
fn build_tree<'a>(
    pairs: impl Iterator<Item = (std::borrow::Cow<'a, str>, std::borrow::Cow<'a, str>)>,
) -> Result<BTreeMap<String, Node>, QueryError> {
    let mut root = BTreeMap::new();
    for (key, value) in pairs {
        match parse_key(&key) {
            Some((base, segments)) => insert(&mut root, base, &segments, value.into_owned())?,
            // Not a well-formed bracket key: treat it as a flat literal name.
            None => insert(&mut root, &key, &[], value.into_owned())?,
        }
    }
    Ok(root)
}

/// Decode a raw (still percent-encoded) query string into `T`.
///
/// The string is the part **after** `?`, exactly as
/// [`Uri::query`](axum::http::Uri::query) returns it. An empty string decodes
/// into any type whose fields are all optional or defaulted.
///
/// # Errors
///
/// Returns a [`QueryError`] when the key grammar conflicts (one key used as
/// both a scalar and a container), when nesting exceeds [`MAX_DEPTH`], or when
/// a value cannot be coerced into the target field's type.
pub fn from_query_str<T: DeserializeOwned>(query: &str) -> Result<T, QueryError> {
    let root = build_tree(url::form_urlencoded::parse(query.as_bytes()))?;
    T::deserialize(NodeDeserializer {
        node: &Node::Map(root),
    })
}

// ──────────────────────────────────────────────────────────────────
// Scalar (leaf) deserializer
// ──────────────────────────────────────────────────────────────────

/// Deserializer for one textual value, coercing with [`str::parse`] exactly as
/// `serde_urlencoded`'s part deserializer does.
struct ValueDeserializer<'a> {
    value: &'a str,
}

macro_rules! parse_scalar {
    ($($method:ident => $visit:ident : $ty:ty),* $(,)?) => {
        $(
            fn $method<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, QueryError> {
                let parsed: $ty = self.value.parse().map_err(|_| {
                    QueryError::msg(format!(
                        "invalid value `{}` for {}",
                        self.value,
                        stringify!($ty)
                    ))
                })?;
                visitor.$visit(parsed)
            }
        )*
    };
}

impl<'de> de::Deserializer<'de> for ValueDeserializer<'_> {
    type Error = QueryError;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, QueryError> {
        visitor.visit_str(self.value)
    }

    parse_scalar! {
        deserialize_bool => visit_bool: bool,
        deserialize_i8 => visit_i8: i8,
        deserialize_i16 => visit_i16: i16,
        deserialize_i32 => visit_i32: i32,
        deserialize_i64 => visit_i64: i64,
        deserialize_i128 => visit_i128: i128,
        deserialize_u8 => visit_u8: u8,
        deserialize_u16 => visit_u16: u16,
        deserialize_u32 => visit_u32: u32,
        deserialize_u64 => visit_u64: u64,
        deserialize_u128 => visit_u128: u128,
        deserialize_f32 => visit_f32: f32,
        deserialize_f64 => visit_f64: f64,
        deserialize_char => visit_char: char,
    }

    fn deserialize_str<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, QueryError> {
        visitor.visit_str(self.value)
    }

    fn deserialize_string<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, QueryError> {
        visitor.visit_str(self.value)
    }

    fn deserialize_bytes<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, QueryError> {
        visitor.visit_bytes(self.value.as_bytes())
    }

    fn deserialize_byte_buf<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, QueryError> {
        visitor.visit_bytes(self.value.as_bytes())
    }

    /// A key that is *present* always visits `Some`, matching
    /// `serde_urlencoded`: only an absent key deserializes as `None`.
    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, QueryError> {
        visitor.visit_some(self)
    }

    fn deserialize_unit<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, QueryError> {
        visitor.visit_unit()
    }

    fn deserialize_unit_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, QueryError> {
        visitor.visit_unit()
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, QueryError> {
        visitor.visit_newtype_struct(self)
    }

    fn deserialize_seq<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value, QueryError> {
        Err(QueryError::msg(format!(
            "expected a sequence, found the value `{}`",
            self.value
        )))
    }

    fn deserialize_tuple<V: Visitor<'de>>(
        self,
        _len: usize,
        visitor: V,
    ) -> Result<V::Value, QueryError> {
        self.deserialize_seq(visitor)
    }

    fn deserialize_tuple_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _len: usize,
        visitor: V,
    ) -> Result<V::Value, QueryError> {
        self.deserialize_seq(visitor)
    }

    fn deserialize_map<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value, QueryError> {
        Err(QueryError::msg(format!(
            "expected an object, found the value `{}`",
            self.value
        )))
    }

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, QueryError> {
        self.deserialize_map(visitor)
    }

    /// A bare value selects a unit variant (`?sort=asc` → `Sort::Asc`).
    fn deserialize_enum<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, QueryError> {
        visitor.visit_enum(UnitVariant { value: self.value })
    }

    fn deserialize_identifier<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, QueryError> {
        visitor.visit_str(self.value)
    }

    fn deserialize_ignored_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, QueryError> {
        visitor.visit_unit()
    }
}

/// `EnumAccess` for the `?sort=asc` unit-variant form.
struct UnitVariant<'a> {
    value: &'a str,
}

impl<'de> EnumAccess<'de> for UnitVariant<'_> {
    type Error = QueryError;
    type Variant = Self;

    fn variant_seed<V: DeserializeSeed<'de>>(
        self,
        seed: V,
    ) -> Result<(V::Value, Self), QueryError> {
        let variant = seed.deserialize(ValueDeserializer { value: self.value })?;
        Ok((variant, self))
    }
}

impl<'de> VariantAccess<'de> for UnitVariant<'_> {
    type Error = QueryError;

    fn unit_variant(self) -> Result<(), QueryError> {
        Ok(())
    }

    fn newtype_variant_seed<T: DeserializeSeed<'de>>(
        self,
        _seed: T,
    ) -> Result<T::Value, QueryError> {
        Err(QueryError::msg(
            "expected a unit variant; a data-carrying variant needs the bracketed form \
             `key[variant][field]=…`",
        ))
    }

    fn tuple_variant<V: Visitor<'de>>(
        self,
        _len: usize,
        _visitor: V,
    ) -> Result<V::Value, QueryError> {
        Err(QueryError::msg("expected a unit variant"))
    }

    fn struct_variant<V: Visitor<'de>>(
        self,
        _fields: &'static [&'static str],
        _visitor: V,
    ) -> Result<V::Value, QueryError> {
        Err(QueryError::msg("expected a unit variant"))
    }
}

// ──────────────────────────────────────────────────────────────────
// Node deserializer
// ──────────────────────────────────────────────────────────────────

struct NodeDeserializer<'a> {
    node: &'a Node,
}

impl<'a> NodeDeserializer<'a> {
    /// The leaf view of a scalar node: its first submitted value (first
    /// occurrence wins), or an error naming the container shape found instead.
    fn as_value(&self, wanted: &str) -> Result<ValueDeserializer<'a>, QueryError> {
        match self.node {
            Node::Scalar(values) => Ok(ValueDeserializer {
                // A scalar node is only ever created by pushing a value, so it
                // is never empty; the fallback keeps this total regardless.
                value: values.first().map_or("", String::as_str),
            }),
            // The caller sent the bracketed form for a key the target types as
            // a plain value. Name the fix rather than just the mismatch: this is
            // the one shape that used to arrive as a literal `key[sub]` string
            // under `serde_urlencoded`.
            other => Err(QueryError::msg(format!(
                "expected {wanted}, found {} — the request used the bracketed form \
                 (`key[…]=`) for this parameter, so give the field a nested type or \
                 accept it as `serde_json::Value`",
                other.kind()
            ))),
        }
    }
}

/// Forward a scalar-shaped `deserialize_*` call to the leaf deserializer.
macro_rules! forward_scalar {
    ($($method:ident),* $(,)?) => {
        $(
            fn $method<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, QueryError> {
                self.as_value("a value")?.$method(visitor)
            }
        )*
    };
}

impl<'de> de::Deserializer<'de> for NodeDeserializer<'_> {
    type Error = QueryError;

    /// Self-describing decode, used by untyped targets such as
    /// `serde_json::Value`: a multi-valued key becomes a sequence, a single
    /// value stays a string.
    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, QueryError> {
        match self.node {
            Node::Scalar(values) if values.len() == 1 => {
                visitor.visit_str(values.first().map_or("", String::as_str))
            }
            Node::Scalar(_) | Node::Seq(_) => self.deserialize_seq(visitor),
            Node::Map(_) => self.deserialize_map(visitor),
        }
    }

    forward_scalar! {
        deserialize_bool,
        deserialize_i8, deserialize_i16, deserialize_i32, deserialize_i64, deserialize_i128,
        deserialize_u8, deserialize_u16, deserialize_u32, deserialize_u64, deserialize_u128,
        deserialize_f32, deserialize_f64,
        deserialize_char, deserialize_str, deserialize_string,
        deserialize_bytes, deserialize_byte_buf,
        deserialize_identifier,
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, QueryError> {
        visitor.visit_some(self)
    }

    fn deserialize_unit<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, QueryError> {
        visitor.visit_unit()
    }

    fn deserialize_unit_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, QueryError> {
        visitor.visit_unit()
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, QueryError> {
        visitor.visit_newtype_struct(self)
    }

    fn deserialize_seq<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, QueryError> {
        match self.node {
            // Repeated keys: every submitted value is one element.
            Node::Scalar(values) => visitor.visit_seq(ValueSeq {
                values: values.iter(),
            }),
            // Indexed/append form: ascending index order, gaps compacted.
            Node::Seq(entries) => visitor.visit_seq(NodeSeq {
                entries: entries.iter(),
            }),
            // An object addressed as a sequence yields its `(key, value)`
            // pairs, so the `Query<Vec<(String, String)>>` idiom keeps working.
            Node::Map(entries) => visitor.visit_seq(PairSeq {
                pairs: flatten_pairs(entries).into_iter(),
            }),
        }
    }

    fn deserialize_tuple<V: Visitor<'de>>(
        self,
        _len: usize,
        visitor: V,
    ) -> Result<V::Value, QueryError> {
        self.deserialize_seq(visitor)
    }

    fn deserialize_tuple_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _len: usize,
        visitor: V,
    ) -> Result<V::Value, QueryError> {
        self.deserialize_seq(visitor)
    }

    fn deserialize_map<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, QueryError> {
        match self.node {
            Node::Map(entries) => visitor.visit_map(NodeMap {
                entries: entries.iter(),
                value: None,
                key: String::new(),
            }),
            other => Err(QueryError::msg(format!(
                "expected an object, found {}",
                other.kind()
            ))),
        }
    }

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, QueryError> {
        self.deserialize_map(visitor)
    }

    /// `?sort=asc` selects a unit variant; `?sort[custom][field]=x` selects a
    /// data-carrying variant through the single-entry object form.
    fn deserialize_enum<V: Visitor<'de>>(
        self,
        name: &'static str,
        variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, QueryError> {
        match self.node {
            Node::Scalar(_) => self
                .as_value("a value")?
                .deserialize_enum(name, variants, visitor),
            Node::Map(entries) if entries.len() == 1 => match entries.iter().next() {
                Some((variant, content)) => visitor.visit_enum(NodeVariant { variant, content }),
                // Unreachable given the `len() == 1` guard, but the request-path
                // gate forbids proving that with a panic.
                None => Err(QueryError::msg("expected an enum variant name")),
            },
            other => Err(QueryError::msg(format!(
                "expected an enum variant name or a single-entry object, found {}",
                other.kind()
            ))),
        }
    }

    /// Unknown keys are discarded without walking their subtree.
    fn deserialize_ignored_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, QueryError> {
        visitor.visit_unit()
    }
}

/// One `(key, value)` pair of a map addressed as a sequence.
enum PairValue<'a> {
    Value(&'a str),
    Node(&'a Node),
}

/// Flatten a map into `(key, value)` pairs, expanding a repeated key into one
/// pair per submitted value so the pair view matches the wire.
fn flatten_pairs<'a>(entries: &'a BTreeMap<String, Node>) -> Vec<(&'a str, PairValue<'a>)> {
    let mut out = Vec::new();
    for (key, node) in entries {
        match node {
            Node::Scalar(values) => {
                out.extend(values.iter().map(|v| (key.as_str(), PairValue::Value(v))));
            }
            other => out.push((key.as_str(), PairValue::Node(other))),
        }
    }
    out
}

struct ValueSeq<'a> {
    values: std::slice::Iter<'a, String>,
}

impl<'de> SeqAccess<'de> for ValueSeq<'_> {
    type Error = QueryError;

    fn next_element_seed<T: DeserializeSeed<'de>>(
        &mut self,
        seed: T,
    ) -> Result<Option<T::Value>, QueryError> {
        self.values
            .next()
            .map(|value| seed.deserialize(ValueDeserializer { value }))
            .transpose()
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.values.len())
    }
}

struct NodeSeq<'a> {
    entries: std::collections::btree_map::Iter<'a, usize, Node>,
}

impl<'de> SeqAccess<'de> for NodeSeq<'_> {
    type Error = QueryError;

    fn next_element_seed<T: DeserializeSeed<'de>>(
        &mut self,
        seed: T,
    ) -> Result<Option<T::Value>, QueryError> {
        let Some((index, node)) = self.entries.next() else {
            return Ok(None);
        };
        seed.deserialize(NodeDeserializer { node })
            .map(Some)
            .map_err(|err| err.with_segment(index.to_string()))
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.entries.len())
    }
}

struct PairSeq<'a> {
    pairs: std::vec::IntoIter<(&'a str, PairValue<'a>)>,
}

impl<'de> SeqAccess<'de> for PairSeq<'_> {
    type Error = QueryError;

    fn next_element_seed<T: DeserializeSeed<'de>>(
        &mut self,
        seed: T,
    ) -> Result<Option<T::Value>, QueryError> {
        let Some((key, value)) = self.pairs.next() else {
            return Ok(None);
        };
        seed.deserialize(PairDeserializer { key, value })
            .map(Some)
            .map_err(|err| err.with_segment(key))
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.pairs.len())
    }
}

/// Deserializes one `(key, value)` pair as a two-element tuple.
struct PairDeserializer<'a> {
    key: &'a str,
    value: PairValue<'a>,
}

impl<'de> de::Deserializer<'de> for PairDeserializer<'_> {
    type Error = QueryError;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, QueryError> {
        visitor.visit_seq(PairElements {
            key: Some(self.key),
            value: Some(self.value),
        })
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
        bytes byte_buf option unit unit_struct newtype_struct seq tuple
        tuple_struct map struct enum identifier ignored_any
    }
}

struct PairElements<'a> {
    key: Option<&'a str>,
    value: Option<PairValue<'a>>,
}

impl<'de> SeqAccess<'de> for PairElements<'_> {
    type Error = QueryError;

    fn next_element_seed<T: DeserializeSeed<'de>>(
        &mut self,
        seed: T,
    ) -> Result<Option<T::Value>, QueryError> {
        if let Some(key) = self.key.take() {
            return seed.deserialize(ValueDeserializer { value: key }).map(Some);
        }
        match self.value.take() {
            Some(PairValue::Value(value)) => {
                seed.deserialize(ValueDeserializer { value }).map(Some)
            }
            Some(PairValue::Node(node)) => seed.deserialize(NodeDeserializer { node }).map(Some),
            None => Ok(None),
        }
    }
}

struct NodeMap<'a> {
    entries: std::collections::btree_map::Iter<'a, String, Node>,
    value: Option<&'a Node>,
    key: String,
}

impl<'de> MapAccess<'de> for NodeMap<'_> {
    type Error = QueryError;

    fn next_key_seed<K: DeserializeSeed<'de>>(
        &mut self,
        seed: K,
    ) -> Result<Option<K::Value>, QueryError> {
        let Some((key, node)) = self.entries.next() else {
            return Ok(None);
        };
        self.value = Some(node);
        self.key = key.clone();
        seed.deserialize(ValueDeserializer { value: key }).map(Some)
    }

    fn next_value_seed<V: DeserializeSeed<'de>>(
        &mut self,
        seed: V,
    ) -> Result<V::Value, QueryError> {
        let node = self.value.take().ok_or_else(|| {
            QueryError::msg("internal error: query map value requested before its key")
        })?;
        // Tag the failure with the field it came from, so a caller sees
        // `filter.limit: invalid value …` rather than a bare parse error.
        seed.deserialize(NodeDeserializer { node })
            .map_err(|err| err.with_segment(self.key.clone()))
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.entries.len())
    }
}

/// `EnumAccess` for the single-entry-object variant form.
struct NodeVariant<'a> {
    variant: &'a str,
    content: &'a Node,
}

impl<'de, 'a> EnumAccess<'de> for NodeVariant<'a> {
    type Error = QueryError;
    type Variant = NodeContent<'a>;

    fn variant_seed<V: DeserializeSeed<'de>>(
        self,
        seed: V,
    ) -> Result<(V::Value, NodeContent<'a>), QueryError> {
        let variant = seed.deserialize(ValueDeserializer {
            value: self.variant,
        })?;
        Ok((
            variant,
            NodeContent {
                content: self.content,
            },
        ))
    }
}

struct NodeContent<'a> {
    content: &'a Node,
}

impl<'de> VariantAccess<'de> for NodeContent<'_> {
    type Error = QueryError;

    fn unit_variant(self) -> Result<(), QueryError> {
        Ok(())
    }

    fn newtype_variant_seed<T: DeserializeSeed<'de>>(
        self,
        seed: T,
    ) -> Result<T::Value, QueryError> {
        seed.deserialize(NodeDeserializer { node: self.content })
    }

    fn tuple_variant<V: Visitor<'de>>(
        self,
        _len: usize,
        visitor: V,
    ) -> Result<V::Value, QueryError> {
        de::Deserializer::deserialize_seq(NodeDeserializer { node: self.content }, visitor)
    }

    fn struct_variant<V: Visitor<'de>>(
        self,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, QueryError> {
        de::Deserializer::deserialize_map(NodeDeserializer { node: self.content }, visitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, Deserialize, PartialEq)]
    struct Filter {
        status: String,
        limit: Option<u32>,
    }

    #[derive(Debug, Deserialize, PartialEq)]
    struct Item {
        sku: String,
        qty: u32,
    }

    #[derive(Debug, Deserialize, PartialEq, Default)]
    struct Args {
        q: Option<String>,
        page: Option<u32>,
        flag: Option<bool>,
        tags: Option<Vec<String>>,
        filter: Option<Filter>,
        items: Option<Vec<Item>>,
    }

    fn args(query: &str) -> Args {
        from_query_str(query).expect("decodes")
    }

    #[test]
    fn flat_scalars_match_the_previous_urlencoded_behaviour() {
        let out = args("q=foo&page=2&flag=true");
        assert_eq!(out.q.as_deref(), Some("foo"));
        assert_eq!(out.page, Some(2));
        assert_eq!(out.flag, Some(true));
    }

    #[test]
    fn absent_keys_are_none_and_an_empty_query_decodes() {
        assert_eq!(args(""), Args::default());
    }

    #[test]
    fn a_present_but_empty_optional_still_visits_some() {
        // Parity with `serde_urlencoded`: presence, not emptiness, decides.
        assert_eq!(args("q=").q.as_deref(), Some(""));
        assert!(
            from_query_str::<Args>("page=").is_err(),
            "an empty value for a numeric field is still a coercion failure"
        );
    }

    #[test]
    fn repeated_keys_fill_a_sequence_and_first_wins_for_a_scalar() {
        assert_eq!(
            args("tags=a&tags=b").tags.unwrap(),
            vec!["a".to_owned(), "b".to_owned()]
        );
        assert_eq!(args("q=first&q=second").q.as_deref(), Some("first"));
    }

    #[test]
    fn append_and_indexed_forms_fill_a_sequence() {
        assert_eq!(
            args("tags[]=a&tags[]=b").tags.unwrap(),
            vec!["a".to_owned(), "b".to_owned()]
        );
        assert_eq!(
            args("tags[1]=b&tags[0]=a").tags.unwrap(),
            vec!["a".to_owned(), "b".to_owned()]
        );
    }

    #[test]
    fn gapped_indices_compact_in_ascending_order() {
        assert_eq!(
            args("tags[4]=c&tags[0]=a&tags[2]=b").tags.unwrap(),
            vec!["a".to_owned(), "b".to_owned(), "c".to_owned()]
        );
    }

    #[test]
    fn an_absurd_index_sorts_last_without_preallocating() {
        assert_eq!(
            args("tags[99999999999999999999]=last&tags[3]=first")
                .tags
                .unwrap(),
            vec!["first".to_owned(), "last".to_owned()]
        );
    }

    #[test]
    fn appends_continue_after_the_highest_explicit_index() {
        assert_eq!(
            args("tags[]=a&tags[5]=b&tags[]=c").tags.unwrap(),
            vec!["a".to_owned(), "b".to_owned(), "c".to_owned()]
        );
    }

    #[test]
    fn nested_objects_decode() {
        assert_eq!(
            args("filter[status]=open&filter[limit]=5").filter.unwrap(),
            Filter {
                status: "open".to_owned(),
                limit: Some(5),
            }
        );
    }

    #[test]
    fn arrays_of_objects_decode() {
        let out = args("items[0][sku]=A-1&items[0][qty]=2&items[1][sku]=B-2&items[1][qty]=3");
        assert_eq!(
            out.items.unwrap(),
            vec![
                Item {
                    sku: "A-1".to_owned(),
                    qty: 2
                },
                Item {
                    sku: "B-2".to_owned(),
                    qty: 3
                },
            ]
        );
    }

    #[test]
    fn percent_encoded_brackets_parse_identically() {
        assert_eq!(
            args("filter%5Bstatus%5D=open").filter.unwrap().status,
            "open"
        );
    }

    #[test]
    fn plus_is_decoded_as_a_space() {
        assert_eq!(args("q=hello+world").q.as_deref(), Some("hello world"));
    }

    #[test]
    fn malformed_bracket_keys_stay_literal() {
        assert_eq!(parse_key("weird[unclosed"), None);
        assert_eq!(parse_key("a[b][c"), None);
        assert_eq!(parse_key("a[[b]]"), None);
        assert_eq!(parse_key("plain"), None);
        // A literal key with no matching field is simply ignored.
        assert_eq!(args("weird[unclosed=1&q=ok").q.as_deref(), Some("ok"));
    }

    #[test]
    fn bracket_keys_parse_into_segments() {
        assert_eq!(
            parse_key("items[0][sku]"),
            Some(("items", vec![Segment::Index(0), Segment::Key("sku")]))
        );
        assert_eq!(parse_key("tags[]"), Some(("tags", vec![Segment::Append])));
    }

    #[test]
    fn shape_conflicts_are_rejected() {
        let scalar_then_object = from_query_str::<Args>("filter=flat&filter[status]=open");
        assert!(scalar_then_object.is_err());
        let object_then_scalar = from_query_str::<Args>("filter[status]=open&filter=flat");
        assert!(object_then_scalar.is_err());
        let seq_then_object = from_query_str::<Args>("tags[0]=a&tags[name]=b");
        assert!(seq_then_object.is_err());
    }

    #[test]
    fn nesting_deeper_than_the_cap_is_rejected() {
        let deep = format!("a{}=1", "[x]".repeat(MAX_DEPTH));
        let err = from_query_str::<Args>(&deep).expect_err("depth-capped");
        assert!(err.to_string().contains("depth"), "{err}");
        // One level under the cap is still accepted.
        let ok = format!("a{}=1", "[x]".repeat(MAX_DEPTH - 2));
        assert!(from_query_str::<Args>(&ok).is_ok());
    }

    #[test]
    fn errors_name_the_failing_field_path() {
        let err = from_query_str::<Args>("filter[status]=open&filter[limit]=nope")
            .expect_err("coercion failure");
        assert!(err.to_string().starts_with("filter.limit:"), "{err}");
    }

    #[test]
    fn untyped_targets_decode_through_deserialize_any() {
        let out: serde_json::Value =
            from_query_str("q=foo&tags=a&tags=b&filter[status]=open").expect("decodes");
        assert_eq!(out["q"], "foo");
        assert_eq!(out["tags"], serde_json::json!(["a", "b"]));
        assert_eq!(out["filter"]["status"], "open");
    }

    #[test]
    fn map_targets_keep_working() {
        let out: std::collections::HashMap<String, String> =
            from_query_str("a=1&b=2").expect("decodes");
        assert_eq!(out.get("a").map(String::as_str), Some("1"));
        assert_eq!(out.get("b").map(String::as_str), Some("2"));
    }

    #[test]
    fn pair_sequence_targets_keep_working() {
        // The `Query<Vec<(String, String)>>` idiom: one pair per occurrence.
        let out: Vec<(String, String)> = from_query_str("a=1&a=2&b=3").expect("decodes");
        assert_eq!(
            out,
            vec![
                ("a".to_owned(), "1".to_owned()),
                ("a".to_owned(), "2".to_owned()),
                ("b".to_owned(), "3".to_owned()),
            ]
        );
    }

    #[test]
    fn unit_enum_variants_decode_from_a_bare_value() {
        #[derive(Debug, Deserialize, PartialEq)]
        #[serde(rename_all = "lowercase")]
        enum Sort {
            Asc,
            Desc,
        }
        #[derive(Debug, Deserialize)]
        struct Sorted {
            sort: Sort,
        }
        let out: Sorted = from_query_str("sort=desc").expect("decodes");
        assert_eq!(out.sort, Sort::Desc);
    }

    #[test]
    fn unknown_keys_are_ignored_including_nested_ones() {
        let out = args("q=ok&unknown[deep][deeper]=1&other=2");
        assert_eq!(out.q.as_deref(), Some("ok"));
    }

    #[test]
    fn a_missing_required_field_still_fails() {
        assert!(from_query_str::<Filter>("limit=5").is_err());
    }

    #[test]
    fn serde_renames_are_honored_because_serde_resolves_the_key() {
        #[derive(Debug, Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Renamed {
            word_count: u32,
        }
        let out: Renamed = from_query_str("wordCount=7").expect("decodes");
        assert_eq!(out.word_count, 7);
    }
}
