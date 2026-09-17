//! Build-checked typed contracts between two Autumn services (issue #1755).
//!
//! **Experimental.** Everything in this module, the input syntax of its four
//! macros, and the JSON descriptor format under `target/autumn-contracts/` may
//! change in any release — see `STABILITY.md`. This is a first slice: one
//! workspace, synchronous request/response, JSON over HTTP.
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
    /// Fields the request type puts on the wire when a caller sends one.
    ///
    /// Differs from [`Endpoint::REQUEST_FIELDS`] under a directional serde
    /// attribute: a field can be demanded by the callee and still be one the
    /// request type sometimes — or never — sends.
    const REQUEST_SENT_FIELDS: &'static [WireField] = <Self::Request as WireShape>::SERIALIZED;
    /// Fields the endpoint produces in its response body.
    const RESPONSE_FIELDS: &'static [WireField] = <Self::Response as WireShape>::SERIALIZED;
}

/// One endpoint of a generated client, as its const table records it.
///
/// `wire_client!` emits the table; `#[contract_checked]` asserts against it by
/// method name. Going through the table rather than naming each endpoint type
/// directly is what lets a call to a method the client does NOT declare — one
/// someone added in their own extension trait — cost nothing instead of naming
/// a type that does not exist.
#[derive(Debug, Clone, Copy)]
pub struct ClientEndpoint {
    /// The generated method's name.
    pub method: &'static str,
    /// Fields the endpoint produces in its response body.
    pub response: &'static [WireField],
    /// Fields the endpoint accepts in its request body.
    pub request: &'static [WireField],
    /// Fields the request type puts on the wire.
    pub request_sent: &'static [WireField],
}

/// The entry for `method`, if the client declares it as an endpoint.
const fn entry_of<'a>(table: &'a [ClientEndpoint], method: &str) -> Option<&'a ClientEndpoint> {
    let mut i = 0;
    while i < table.len() {
        if str_eq(table[i].method, method) {
            return Some(&table[i]);
        }
        i += 1;
    }
    None
}

/// Whether the endpoint produces `field` — vacuously true when `method` is not
/// one of this client's endpoints.
#[must_use]
pub const fn client_produces(table: &[ClientEndpoint], method: &str, field: &str) -> bool {
    match entry_of(table, method) {
        Some(entry) => has_field(entry.response, field),
        None => true,
    }
}

/// Whether the endpoint accepts `field` — vacuously true when `method` is not
/// one of this client's endpoints.
#[must_use]
pub const fn client_accepts(table: &[ClientEndpoint], method: &str, field: &str) -> bool {
    match entry_of(table, method) {
        Some(entry) => has_field(entry.request, field),
        None => true,
    }
}

/// Whether the call site names every request field the endpoint requires and
/// the request type may keep off the wire — vacuously true when `method` is not
/// one of this client's endpoints.
#[must_use]
pub const fn client_request_covered(
    table: &[ClientEndpoint],
    method: &str,
    supplied: &[&str],
) -> bool {
    match entry_of(table, method) {
        Some(entry) => omittable_required_covered(entry.request_sent, entry.request, supplied),
        None => true,
    }
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

/// Whether every request field that the callee requires but the request type
/// may leave off the wire is named at the call site.
///
/// Both ends share the request type, so serialization normally emits every
/// field — a `..rest` initializer sends a default value, not nothing, and
/// omitting a field from the literal is therefore NOT a wire break. One shape
/// is different: a field carrying `#[serde(skip_serializing_if = …)]`, or
/// `#[serde(skip_serializing)]`, can be absent from the body while the callee
/// still demands it. Those are the fields a call site has to name.
///
/// `serialized` and `deserialized` are the request type's two tables;
/// `supplied` is what the call site sets explicitly.
#[must_use]
pub const fn omittable_required_covered(
    serialized: &[WireField],
    deserialized: &[WireField],
    supplied: &[&str],
) -> bool {
    let mut i = 0;
    while i < deserialized.len() {
        let field = &deserialized[i];
        if field.required {
            // Never serialized at all: the request type can no more send this
            // field than the call site can, so naming it would not help.
            if !has_field(serialized, field.rust_name) {
                return false;
            }
            // Conditionally serialized: only a call site that sets it can be
            // sure it goes out.
            if !always_serialized(serialized, field.rust_name)
                && !contains_str(supplied, field.rust_name)
            {
                return false;
            }
        }
        i += 1;
    }
    true
}

/// Whether `fields` produces `rust_name` on every serialization.
const fn always_serialized(fields: &[WireField], rust_name: &str) -> bool {
    let mut i = 0;
    while i < fields.len() {
        if str_eq(fields[i].rust_name, rust_name) {
            return fields[i].required;
        }
        i += 1;
    }
    false
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
                // An unterminated placeholder is a malformed template, not a
                // parameter list that happens to be shorter. Refusing it here
                // is what keeps `render_path` from emitting the stray `{` into
                // a URL.
                return false;
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
/// `/` or `?` cannot reshape the URL — and `.`, `..` and the empty string are
/// encoded too, because a URL parser resolves those away and would otherwise
/// let a caller-supplied value retarget the call at a sibling route.
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
///
/// `.` is in that set, so a bare `.` or `..` would survive as a dot segment and
/// `remove_dot_segments` would then resolve it away: `/items/../reviews`
/// becomes `/reviews`. Those two values, and the empty string, are therefore
/// encoded whole — the segment stays one segment whatever the value is.
fn encode_segment(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    if value.is_empty() {
        return "%20".to_owned();
    }
    if value == "." {
        return "%2E".to_owned();
    }
    if value == ".." {
        return "%2E%2E".to_owned();
    }
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

    /// `name` is required and always serialized; `price_cents` is optional.
    /// Neither has to be named at a call site.
    #[test]
    fn an_always_serialized_field_never_has_to_be_named_at_a_call_site() {
        assert!(omittable_required_covered(FIELDS, FIELDS, &[]));
        assert!(omittable_required_covered(&[], &[], &[]));
    }

    #[test]
    fn a_required_field_the_request_may_omit_must_be_named() {
        // `sku` is required off the wire but conditionally serialized, so a
        // call site that does not set it can send a body without it.
        const SER: &[WireField] = &[WireField {
            rust_name: "sku",
            wire_name: "sku",
            ty: "String",
            required: false,
        }];
        const DE: &[WireField] = &[WireField {
            rust_name: "sku",
            wire_name: "sku",
            ty: "String",
            required: true,
        }];
        assert!(!omittable_required_covered(SER, DE, &[]));
        assert!(omittable_required_covered(SER, DE, &["sku"]));
    }

    #[test]
    fn a_required_field_the_request_can_never_send_is_uncoverable() {
        const DE: &[WireField] = &[WireField {
            rust_name: "sku",
            wire_name: "sku",
            ty: "String",
            required: true,
        }];
        // `#[serde(skip_serializing)]`: absent from the serialized table, so
        // naming it at the call site cannot put it on the wire either.
        assert!(!omittable_required_covered(&[], DE, &[]));
        assert!(!omittable_required_covered(&[], DE, &["sku"]));
    }

    // The checks must hold in const context — that is the whole mechanism.
    const _: () = assert!(has_field(FIELDS, "name"));
    const _: () = assert!(!has_field(FIELDS, "sku"));
    const _: () = assert!(omittable_required_covered(FIELDS, FIELDS, &[]));

    #[test]
    fn path_params_are_matches_name_and_order() {
        assert!(path_params_are("/items/{id}", &["id"]));
        assert!(path_params_are("/a/{x}/b/{y}", &["x", "y"]));
        assert!(path_params_are("/items", &[]));
        assert!(!path_params_are("/a/{x}/b/{y}", &["y", "x"]));
        assert!(!path_params_are("/items/{id}", &[]));
        assert!(!path_params_are("/items", &["id"]));
        assert!(!path_params_are("/items/{sku}", &["id"]));
        // A malformed template is refused rather than read as a short list.
        assert!(!path_params_are("/a/{x}/b/{y", &["x"]));
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
        // A dot segment would be resolved away by a URL parser, retargeting
        // the call at a sibling route.
        assert_eq!(
            render_path("/items/{id}/reviews", &[("id", "..")]),
            "/items/%2E%2E/reviews"
        );
        assert_eq!(render_path("/items/{id}", &[("id", ".")]), "/items/%2E");
        assert_eq!(render_path("/items/{id}", &[("id", "")]), "/items/%20");
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
