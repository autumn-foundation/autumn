//! Schema.org structured data as JSON-LD.
//!
//! [`JsonLd`] builds one schema.org node and renders it as a
//! `<script type="application/ld+json">` block. Search engines and AI agents
//! read that block to learn what a page is about: a recipe, a product, an
//! event.
//!
//! ```
//! use autumn_web::json_ld::JsonLd;
//!
//! let recipe = JsonLd::new("Recipe")
//!     .set("name", "Pancakes")
//!     .set("recipeYield", 4)
//!     .set_opt("description", None::<String>)
//!     .set("author", JsonLd::node("Person").set("name", "Ada"));
//!
//! let json = recipe.to_json();
//! assert!(json.contains(r#""@context":"https://schema.org""#));
//! assert!(json.contains(r#""@type":"Recipe""#));
//! assert!(!json.contains("description"));
//! ```
//!
//! # Escaping
//!
//! A JSON string can hold `</script>`, and the HTML parser ends the script
//! element there, whatever the JSON means. [`JsonLd::to_json`] writes every
//! `<`, `>` and `&` as a `\u00XX` escape, and U+2028 and U+2029 as `\u2028`
//! and `\u2029`. The escapes appear only inside JSON strings, so a JSON parser
//! reads back the same value. The output has no `<` at all, so no value can
//! close the element or open an HTML comment. `verification/json_ld_escape.rs`
//! proves this for the escaper.
//!
//! [`JsonLd::to_json_unescaped`] is the plain JSON, for an API response or a
//! file. Do not put it in a page.
//!
//! With [`SeoMeta::json_ld`](crate::seo::SeoMeta::json_ld) the block renders
//! with the page's other meta tags.

use serde::Serialize;
use serde_json::{Map, Value};

/// The schema.org `@context` IRI.
pub const SCHEMA_ORG_CONTEXT: &str = "https://schema.org";

/// One schema.org node: an object with an `@type` and its properties.
///
/// [`JsonLd::new`] makes a top-level node, with `@context`. [`JsonLd::node`]
/// makes a nested one, without it. A node set as a property value of another
/// node drops its `@context`, so either works there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonLd {
    object: Map<String, Value>,
}

impl JsonLd {
    /// A top-level node of `schema_type` (`"Recipe"`, `"Product"`), with
    /// `"@context": "https://schema.org"`.
    #[must_use]
    pub fn new(schema_type: impl Into<String>) -> Self {
        let mut object = Map::new();
        object.insert(
            "@context".to_owned(),
            Value::String(SCHEMA_ORG_CONTEXT.to_owned()),
        );
        object.insert("@type".to_owned(), Value::String(schema_type.into()));
        Self { object }
    }

    /// A nested node of `schema_type`, without `@context`.
    #[must_use]
    pub fn node(schema_type: impl Into<String>) -> Self {
        let mut object = Map::new();
        object.insert("@type".to_owned(), Value::String(schema_type.into()));
        Self { object }
    }

    /// A top-level `@graph` of several nodes under one `@context`. Each
    /// node's own `@context` is dropped.
    #[must_use]
    pub fn graph(nodes: impl IntoIterator<Item = Self>) -> Self {
        let mut object = Map::new();
        object.insert(
            "@context".to_owned(),
            Value::String(SCHEMA_ORG_CONTEXT.to_owned()),
        );
        let nodes = nodes
            .into_iter()
            .map(|node| Value::Object(node.into_nested()))
            .collect();
        object.insert("@graph".to_owned(), Value::Array(nodes));
        Self { object }
    }

    /// Set `@id`, the node's IRI. Other nodes can point at it.
    #[must_use]
    pub fn id(self, iri: impl Into<String>) -> Self {
        self.set("@id", iri.into())
    }

    /// Set the property `name` to `value`.
    ///
    /// `value` is anything [`Serialize`]: a string, a number, a
    /// `jiff` date (it writes ISO 8601), a `Vec`, or another [`JsonLd`].
    /// A value that serializes to `null`, or does not serialize, removes the
    /// property. A nested node's `@context` is dropped.
    #[must_use]
    pub fn set(mut self, name: impl Into<String>, value: impl Serialize) -> Self {
        let name = name.into();
        match serde_json::to_value(value) {
            Ok(Value::Null) | Err(_) => {
                self.object.remove(&name);
            }
            Ok(value) => {
                self.object.insert(name, strip_nested_context(value));
            }
        }
        self
    }

    /// Set the property `name` when `value` is `Some`. `None` leaves the
    /// node as it is.
    #[must_use]
    pub fn set_opt<T: Serialize>(self, name: impl Into<String>, value: Option<T>) -> Self {
        match value {
            Some(value) => self.set(name, value),
            None => self,
        }
    }

    /// The node's `@type`, when it has one.
    #[must_use]
    pub fn schema_type(&self) -> Option<&str> {
        self.object.get("@type").and_then(Value::as_str)
    }

    /// Whether the node has a property besides `@context` and `@type`. A
    /// nested node built from optional columns that were all empty has none:
    /// `.set_opt("offers", Some(offer).filter(JsonLd::has_properties))`
    /// leaves it out.
    #[must_use]
    pub fn has_properties(&self) -> bool {
        self.object
            .keys()
            .any(|key| key != "@context" && key != "@type")
    }

    /// The property `name`.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Value> {
        self.object.get(name)
    }

    /// The node as a JSON value.
    #[must_use]
    pub fn to_value(&self) -> Value {
        Value::Object(self.object.clone())
    }

    /// The node as plain JSON, not escaped for HTML. Use
    /// [`JsonLd::to_json`] for a page.
    #[must_use]
    pub fn to_json_unescaped(&self) -> String {
        Value::Object(self.object.clone()).to_string()
    }

    /// The node as JSON that is safe inside a `<script>` element. See the
    /// module docs on escaping.
    #[must_use]
    pub fn to_json(&self) -> String {
        escape_script_json(&self.to_json_unescaped())
    }

    /// The `<script type="application/ld+json">` element.
    #[cfg(feature = "maud")]
    #[must_use]
    pub fn render(&self) -> maud::Markup {
        maud::PreEscaped(format!(
            r#"<script type="application/ld+json">{}</script>"#,
            self.to_json()
        ))
    }

    fn into_nested(mut self) -> Map<String, Value> {
        self.object.remove("@context");
        self.object
    }
}

impl Serialize for JsonLd {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.object.serialize(serializer)
    }
}

#[cfg(feature = "maud")]
impl maud::Render for JsonLd {
    fn render(&self) -> maud::Markup {
        Self::render(self)
    }
}

/// Drop `@context` from a nested node, and from each node of an array.
fn strip_nested_context(value: Value) -> Value {
    match value {
        Value::Object(mut object) => {
            object.remove("@context");
            Value::Object(object)
        }
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(|item| match item {
                    Value::Object(mut object) => {
                        object.remove("@context");
                        Value::Object(object)
                    }
                    other => other,
                })
                .collect(),
        ),
        other => other,
    }
}

/// Escape serialized JSON for a `<script>` element.
///
/// `<`, `>` and `&` become `\u003c`, `\u003e` and `\u0026`. U+2028 and
/// U+2029 become `\u2028` and `\u2029`. Outside a string JSON has none of
/// these characters, so the escapes land only inside strings, and a JSON
/// parser reads back the same value. Input that is not JSON gets the same
/// replacement, but the result means what the input meant only when the
/// input was JSON.
#[must_use]
pub fn escape_script_json(json: &str) -> String {
    let mut out = String::with_capacity(json.len());
    for ch in json.chars() {
        match ch {
            '<' => out.push_str("\\u003c"),
            '>' => out.push_str("\\u003e"),
            '&' => out.push_str("\\u0026"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn new_node_carries_context_and_type() {
        let node = JsonLd::new("Recipe");
        assert_eq!(
            node.to_value(),
            json!({"@context": "https://schema.org", "@type": "Recipe"})
        );
        assert_eq!(node.schema_type(), Some("Recipe"));
    }

    #[test]
    fn nested_node_has_no_context() {
        let node = JsonLd::node("Person").set("name", "Ada");
        assert_eq!(node.to_value(), json!({"@type": "Person", "name": "Ada"}));
    }

    #[test]
    fn a_top_level_node_set_as_a_value_loses_its_context() {
        let node = JsonLd::new("Recipe").set("author", JsonLd::new("Person").set("name", "Ada"));
        assert_eq!(
            node.get("author"),
            Some(&json!({"@type": "Person", "name": "Ada"}))
        );
        let list = JsonLd::new("Recipe").set(
            "author",
            vec![JsonLd::new("Person"), JsonLd::node("Organization")],
        );
        assert_eq!(
            list.get("author"),
            Some(&json!([{"@type": "Person"}, {"@type": "Organization"}]))
        );
    }

    #[test]
    fn set_opt_none_and_null_leave_no_property() {
        let node = JsonLd::new("Thing")
            .set_opt("description", None::<&str>)
            .set("name", "x")
            .set("name", Option::<&str>::None);
        assert_eq!(node.get("description"), None);
        assert_eq!(node.get("name"), None);
        let node = JsonLd::new("Thing").set_opt("name", Some("x"));
        assert_eq!(node.get("name"), Some(&json!("x")));
    }

    #[test]
    fn has_properties_ignores_context_and_type() {
        assert!(!JsonLd::new("Offer").has_properties());
        assert!(
            !JsonLd::node("Offer")
                .set_opt("price", None::<u8>)
                .has_properties()
        );
        assert!(JsonLd::node("Offer").set("price", 3).has_properties());
        let node = JsonLd::new("Product").set_opt(
            "offers",
            Some(JsonLd::node("Offer")).filter(JsonLd::has_properties),
        );
        assert_eq!(node.get("offers"), None);
    }

    #[test]
    fn id_sets_the_iri() {
        let node = JsonLd::new("WebPage").id("https://example.com/#page");
        assert_eq!(node.get("@id"), Some(&json!("https://example.com/#page")));
    }

    #[test]
    fn jiff_dates_serialize_as_iso_8601() {
        let date = jiff::civil::date(2026, 11, 27);
        let ts: jiff::Timestamp = "2026-11-27T10:00:00Z".parse().expect("timestamp");
        let node = JsonLd::new("Event")
            .set("startDate", date)
            .set("endDate", ts);
        assert_eq!(node.get("startDate"), Some(&json!("2026-11-27")));
        assert_eq!(node.get("endDate"), Some(&json!("2026-11-27T10:00:00Z")));
    }

    #[test]
    fn graph_holds_nodes_under_one_context() {
        let graph = JsonLd::graph([
            JsonLd::new("Organization").set("name", "Acme"),
            JsonLd::new("WebSite").set("name", "Acme site"),
        ]);
        assert_eq!(
            graph.to_value(),
            json!({
                "@context": "https://schema.org",
                "@graph": [
                    {"@type": "Organization", "name": "Acme"},
                    {"@type": "WebSite", "name": "Acme site"},
                ]
            })
        );
    }

    #[test]
    fn script_close_in_a_value_cannot_end_the_element() {
        let node = JsonLd::new("Article").set("headline", "</script><script>alert(1)</script>");
        let json = node.to_json();
        assert!(!json.contains('<'), "{json}");
        assert!(!json.contains('>'), "{json}");
        let back: Value = serde_json::from_str(&json).expect("escaped JSON parses");
        assert_eq!(back, node.to_value());
    }

    #[test]
    fn comment_open_ampersand_and_line_separators_are_escaped() {
        let text = "<!-- a & b \u{2028} c \u{2029} d -->";
        let node = JsonLd::new("Thing").set("name", text);
        let json = node.to_json();
        assert!(json.contains(r"\u003c!--"), "{json}");
        assert!(json.contains(r"\u0026"), "{json}");
        assert!(json.contains(r"\u2028"), "{json}");
        assert!(json.contains(r"\u2029"), "{json}");
        assert!(!json.contains('\u{2028}'));
        let back: Value = serde_json::from_str(&json).expect("escaped JSON parses");
        assert_eq!(back.get("name"), Some(&json!(text)));
    }

    #[test]
    fn escape_leaves_other_text_alone() {
        assert_eq!(escape_script_json(r#"{"a":"b é"}"#), r#"{"a":"b é"}"#);
        assert_eq!(escape_script_json(""), "");
    }

    #[test]
    fn unescaped_json_is_plain() {
        let node = JsonLd::new("Thing").set("name", "a<b");
        assert!(node.to_json_unescaped().contains("a<b"));
    }

    #[cfg(feature = "maud")]
    #[test]
    fn render_wraps_the_escaped_json_in_a_script_element() {
        let node = JsonLd::new("Thing").set("name", "</script>");
        let html = node.render().into_string();
        assert!(
            html.starts_with(r#"<script type="application/ld+json">{"#),
            "{html}"
        );
        assert!(html.ends_with("}</script>"), "{html}");
        assert_eq!(html.matches("</script").count(), 1, "{html}");
        assert_eq!(maud::Render::render(&node).into_string(), html);
    }

    #[test]
    fn serialize_writes_the_object() {
        let node = JsonLd::node("Thing").set("name", "x");
        assert_eq!(
            serde_json::to_value(&node).expect("serialize"),
            json!({"@type": "Thing", "name": "x"})
        );
    }

    mod props {
        use super::super::*;
        use proptest::prelude::*;

        const LINE_SEPARATOR: char = '\u{2028}';
        const PARAGRAPH_SEPARATOR: char = '\u{2029}';

        proptest! {
            #[test]
            fn escaped_output_has_no_markup_bytes_and_round_trips(s in any::<String>()) {
                let node = JsonLd::new("Thing").set("name", s.clone());
                let json = node.to_json();
                prop_assert!(!json.contains('<'));
                prop_assert!(!json.contains('>'));
                prop_assert!(!json.contains('&'));
                prop_assert!(!json.contains(LINE_SEPARATOR));
                prop_assert!(!json.contains(PARAGRAPH_SEPARATOR));
                let back: Value = serde_json::from_str(&json).expect("parses");
                prop_assert_eq!(back.get("name"), Some(&Value::String(s)));
            }
        }
    }
}
