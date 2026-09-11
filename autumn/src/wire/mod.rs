//! Build-checked typed contracts between two Autumn services (issue #1755).
//!
//! Two Autumn services in one Cargo workspace share one compiler-checked
//! contract: the callee's handler signatures. Nothing is hand-maintained, and
//! nothing is generated from a parallel IDL.
//!
//! # The three pieces
//!
//! | Where | What | Does |
//! |---|---|---|
//! | callee | `#[derive(WireShape)]` | records a DTO's serde-visible field shape |
//! | callee | `#[endpoint(service = "…")]` | marks a handler, emits its `Endpoint` impl and a JSON descriptor |
//! | caller | `wire_client!` | generates the typed client from those `Endpoint` impls |
//! | caller | `#[contract_checked]` | fails the build when a call site and the endpoint disagree |
//!
//! # What it catches that the type checker does not
//!
//! Both ends share the same Rust types, so a removed field already breaks the
//! build. Three breaks do not:
//!
//! 1. `NewItem { name, ..Default::default() }` at the caller, plus a new
//!    required field on the callee. Compiles; 400s at runtime.
//! 2. `#[serde(skip_serializing)]` added to a response field a caller reads.
//!    Compiles; the field silently stops arriving.
//! 3. `#[serde(skip_deserializing)]` on a request field a caller sets.
//!    Compiles; the value is silently dropped.
//!
//! # Example
//!
//! ```text
//! // catalog service
//! #[derive(serde::Serialize, serde::Deserialize, WireShape)]
//! pub struct Item { pub id: String, pub name: String }
//!
//! #[endpoint(service = "catalog")]
//! #[get("/items/{id}")]
//! async fn get_item(id: Path<String>) -> AutumnResult<Json<Item>> { … }
//!
//! // storefront
//! wire_client! { name = CatalogClient, endpoints = [catalog::get_item_endpoint] }
//!
//! #[contract_checked(client = CatalogClient)]
//! async fn page(catalog: CatalogClient, id: Path<String>) -> AutumnResult<Markup> {
//!     let item = catalog.get_item(&*id, NoBody).await?;
//!     Ok(html! { h1 { (item.name) } })
//! }
//! ```
//!
//! See `docs/guide/wire-contracts.md`.

#[cfg(feature = "http-client")]
mod call;

#[cfg(feature = "http-client")]
pub use call::{WireError, call};

/// One field of a request or response type, as serde treats it.
///
/// Emitted by `#[derive(WireShape)]`. Fields serde never puts on the wire in a
/// given direction are absent from that direction's table rather than flagged,
/// so "is this field on the wire?" is one lookup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WireField {
    /// The field's Rust identifier — what a call site writes.
    pub rust_name: &'static str,
    /// The JSON key serde uses. Differs under `#[serde(rename)]`.
    pub wire_name: &'static str,
    /// The field's type, as written in the struct.
    pub ty: &'static str,
    /// Whether the field must be present on the wire in this direction.
    pub required: bool,
}

/// A type's serde-visible shape, in both directions.
///
/// Derive it with `#[derive(WireShape)]`; do not implement it by hand, or the
/// contract stops describing the code that actually runs.
pub trait WireShape {
    /// The type's Rust name.
    const TYPE_NAME: &'static str;
    /// Fields this type puts on the wire when serialized.
    const SERIALIZED: &'static [WireField];
    /// Fields this type accepts off the wire when deserialized.
    const DESERIALIZED: &'static [WireField];
}

/// The request type of an endpoint that takes no body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct NoBody;

impl WireShape for NoBody {
    const TYPE_NAME: &'static str = "NoBody";
    const SERIALIZED: &'static [WireField] = &[];
    const DESERIALIZED: &'static [WireField] = &[];
}

/// A list endpoint's body carries its element's shape.
///
/// A caller reads fields off an *element*, not off the list, and a loop
/// variable is not something `#[contract_checked]` tracks — so a list response
/// is checked by type, not field by field. The shape is still carried so the
/// descriptor describes what actually goes on the wire.
impl<T: WireShape> WireShape for Vec<T> {
    const TYPE_NAME: &'static str = <T as WireShape>::TYPE_NAME;
    const SERIALIZED: &'static [WireField] = <T as WireShape>::SERIALIZED;
    const DESERIALIZED: &'static [WireField] = <T as WireShape>::DESERIALIZED;
}

/// One service endpoint's contract, as `#[endpoint]` derived it from the
/// handler's own signature.
///
/// The associated types carry the callee's real Rust types, so a caller names
/// them through the endpoint marker and never has to spell out a module path.
pub trait Endpoint {
    /// The JSON request body, or [`NoBody`].
    ///
    /// `Send + Sync` because [`call`] holds a reference to it across an await,
    /// and a handler's future has to stay `Send`.
    type Request: WireShape + serde::Serialize + Send + Sync;
    /// The JSON response body.
    type Response: WireShape + serde::de::DeserializeOwned + Send;

    /// The service name from `#[endpoint(service = "…")]`.
    const SERVICE: &'static str;
    /// The endpoint name — the handler's function name unless overridden.
    const NAME: &'static str;
    /// HTTP method, uppercase.
    const METHOD: &'static str;
    /// Route path, with `{param}` placeholders intact.
    const PATH: &'static str;
    /// Whether the endpoint takes a JSON request body.
    const HAS_BODY: bool;

    /// Fields the endpoint accepts in its request body.
    const REQUEST_FIELDS: &'static [WireField] = <Self::Request as WireShape>::DESERIALIZED;
    /// Fields the endpoint produces in its response body.
    const RESPONSE_FIELDS: &'static [WireField] = <Self::Response as WireShape>::SERIALIZED;
}

/// Whether `fields` contains one named `rust_name`.
///
/// `#[contract_checked]` calls this from a `const _: () = assert!(…)`, so a
/// contract breach is a const-eval failure carrying the macro's own message.
#[must_use]
pub const fn has_field(fields: &[WireField], rust_name: &str) -> bool {
    let mut i = 0;
    while i < fields.len() {
        if str_eq(fields[i].rust_name, rust_name) {
            return true;
        }
        i += 1;
    }
    false
}

/// Whether every required field in `fields` appears in `supplied`.
///
/// Used for a request built with a `..rest` initializer, which is the one shape
/// that can omit a field without the type checker noticing.
#[must_use]
pub const fn required_covered(fields: &[WireField], supplied: &[&str]) -> bool {
    let mut i = 0;
    while i < fields.len() {
        if fields[i].required && !contains_str(supplied, fields[i].rust_name) {
            return false;
        }
        i += 1;
    }
    true
}

/// Whether `template`'s `{…}` placeholders are exactly `declared`, in order.
///
/// `wire_client!` asserts this so a client's declared path parameters cannot
/// drift from the route the endpoint actually serves.
#[must_use]
pub const fn path_params_are(template: &str, declared: &[&str]) -> bool {
    let bytes = template.as_bytes();
    let (mut i, mut seen) = (0, 0);
    while i < bytes.len() {
        if bytes[i] == b'{' {
            let start = i + 1;
            let mut end = start;
            while end < bytes.len() && bytes[end] != b'}' {
                end += 1;
            }
            if end >= bytes.len() {
                // An unterminated placeholder is not a parameter.
                return seen == declared.len();
            }
            if seen >= declared.len() || !str_eq(declared[seen], slice_str(template, start, end)) {
                return false;
            }
            seen += 1;
            i = end + 1;
        } else {
            i += 1;
        }
    }
    seen == declared.len()
}

/// `&s[start..end]`, usable in a const assertion.
///
/// Placeholder names are ASCII identifiers, so the byte range is always a
/// char boundary; a non-ASCII name yields an empty slice and fails the
/// comparison rather than panicking.
const fn slice_str(s: &str, start: usize, end: usize) -> &str {
    let bytes = s.as_bytes();
    let mut i = start;
    while i < end {
        if bytes[i] >= 0x80 {
            return "";
        }
        i += 1;
    }
    // SAFETY-free alternative to slicing: `str::split_at` is const-stable and
    // panics on a non-boundary, which the ASCII check above rules out.
    let (head, _) = s.split_at(end);
    let (_, tail) = head.split_at(start);
    tail
}

/// Substitute `{name}` placeholders in a route path.
///
/// Values are percent-encoded as a single path segment, so a value containing
/// `/` or `?` cannot reshape the URL.
#[must_use]
pub fn render_path(template: &str, params: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        rest = &rest[open + 1..];
        let Some(close) = rest.find('}') else {
            out.push('{');
            break;
        };
        let name = &rest[..close];
        if let Some((_, value)) = params.iter().find(|(k, _)| *k == name) {
            out.push_str(&encode_segment(value));
        } else {
            // `wire_client!` supplies every placeholder, so this is
            // unreachable from generated code; leaving the placeholder intact
            // is better than producing a URL that silently drops it.
            out.push('{');
            out.push_str(name);
            out.push('}');
        }
        rest = &rest[close + 1..];
    }
    out.push_str(rest);
    out
}

/// Percent-encode one URL path segment, keeping only the unreserved set.
fn encode_segment(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(byte as char);
        } else {
            out.push('%');
            out.push(HEX[usize::from(byte >> 4)] as char);
            out.push(HEX[usize::from(byte & 0x0f)] as char);
        }
    }
    out
}

/// `str` equality, usable in a const assertion.
const fn str_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}

/// `slice::contains` for `&str`, usable in a const assertion.
const fn contains_str(haystack: &[&str], needle: &str) -> bool {
    let mut i = 0;
    while i < haystack.len() {
        if str_eq(haystack[i], needle) {
            return true;
        }
        i += 1;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIELDS: &[WireField] = &[
        WireField {
            rust_name: "name",
            wire_name: "name",
            ty: "String",
            required: true,
        },
        WireField {
            rust_name: "price_cents",
            wire_name: "priceCents",
            ty: "Option<u32>",
            required: false,
        },
    ];

    #[test]
    fn has_field_matches_the_rust_name_not_the_wire_name() {
        assert!(has_field(FIELDS, "price_cents"));
        assert!(!has_field(FIELDS, "priceCents"));
        assert!(!has_field(FIELDS, "sku"));
    }

    #[test]
    fn has_field_does_not_match_a_prefix() {
        assert!(!has_field(FIELDS, "nam"));
        assert!(!has_field(FIELDS, "names"));
    }

    #[test]
    fn required_covered_ignores_optional_fields() {
        assert!(required_covered(FIELDS, &["name"]));
        assert!(!required_covered(FIELDS, &["price_cents"]));
        assert!(required_covered(&[], &[]));
    }

    // The checks must hold in const context — that is the whole mechanism.
    const _: () = assert!(has_field(FIELDS, "name"));
    const _: () = assert!(!has_field(FIELDS, "sku"));
    const _: () = assert!(required_covered(FIELDS, &["name"]));
    const _: () = assert!(!required_covered(FIELDS, &[]));

    #[test]
    fn path_params_are_matches_name_and_order() {
        assert!(path_params_are("/items/{id}", &["id"]));
        assert!(path_params_are("/a/{x}/b/{y}", &["x", "y"]));
        assert!(path_params_are("/items", &[]));
        assert!(!path_params_are("/a/{x}/b/{y}", &["y", "x"]));
        assert!(!path_params_are("/items/{id}", &[]));
        assert!(!path_params_are("/items", &["id"]));
        assert!(!path_params_are("/items/{sku}", &["id"]));
    }

    const _: () = assert!(path_params_are("/items/{id}", &["id"]));
    const _: () = assert!(!path_params_are("/items/{id}", &["sku"]));

    #[test]
    fn render_path_percent_encodes_each_segment() {
        assert_eq!(render_path("/items/{id}", &[("id", "a b")]), "/items/a%20b");
        assert_eq!(
            render_path("/items/{id}", &[("id", "../admin")]),
            "/items/..%2Fadmin"
        );
        assert_eq!(render_path("/items", &[]), "/items");
        assert_eq!(
            render_path("/a/{x}/b/{y}", &[("x", "1"), ("y", "2")]),
            "/a/1/b/2"
        );
    }

    #[test]
    fn render_path_leaves_an_unsupplied_placeholder_visible() {
        assert_eq!(render_path("/items/{id}", &[]), "/items/{id}");
    }

    #[test]
    fn a_list_carries_its_elements_shape() {
        struct Item;
        impl WireShape for Item {
            const TYPE_NAME: &'static str = "Item";
            const SERIALIZED: &'static [WireField] = FIELDS;
            const DESERIALIZED: &'static [WireField] = FIELDS;
        }
        assert_eq!(<Vec<Item> as WireShape>::SERIALIZED.len(), FIELDS.len());
        assert_eq!(<Vec<Item> as WireShape>::TYPE_NAME, "Item");
    }

    #[test]
    fn no_body_is_empty_in_both_directions() {
        assert!(<NoBody as WireShape>::SERIALIZED.is_empty());
        assert!(<NoBody as WireShape>::DESERIALIZED.is_empty());
    }
}
