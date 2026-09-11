//! The wire descriptor — what one service endpoint puts on, and takes off, the
//! wire (issue #1755).
//!
//! `#[endpoint]` emits one of these per endpoint, both as a compile-time const
//! table in the callee crate and as a JSON build artifact. The artifact is the
//! machine-readable half: `#[contract_checked]` reads it back to name an
//! offending field in a diagnostic, and later slices diff two of them to reason
//! about a rolling-deploy window.
//!
//! Field names are recorded twice. `rust_name` is what a caller writes in
//! source; `wire_name` is what serde puts in the JSON. They differ under
//! `#[serde(rename)]` / `#[serde(rename_all)]`. Checks key on `rust_name`
//! because that is what a call site names; `wire_name` is carried for tooling.

use serde::{Deserialize, Serialize};

/// One field of a request or response type, as serde treats it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireFieldDescriptor {
    /// The field's Rust identifier — what a call site writes.
    pub rust_name: String,
    /// The JSON key serde uses. Equals `rust_name` unless renamed.
    pub wire_name: String,
    /// The field's type, as written in the struct.
    pub ty: String,
    /// Whether the field must be present on the wire in this direction.
    ///
    /// For a request: the callee rejects a body that omits it. For a response:
    /// the callee always produces it, so a caller may read it unconditionally.
    pub required: bool,
}

/// The serde-visible shape of one request or response type.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct WireTypeDescriptor {
    /// The type's Rust name.
    pub name: String,
    /// Fields this type puts on the wire when serialized.
    pub serialized: Vec<WireFieldDescriptor>,
    /// Fields this type accepts off the wire when deserialized.
    pub deserialized: Vec<WireFieldDescriptor>,
}

impl WireTypeDescriptor {
    /// The serialized field with this Rust name, if the type produces one.
    #[must_use]
    pub fn produced(&self, rust_name: &str) -> Option<&WireFieldDescriptor> {
        self.serialized.iter().find(|f| f.rust_name == rust_name)
    }

    /// The deserialized field with this Rust name, if the type accepts one.
    #[must_use]
    pub fn accepted(&self, rust_name: &str) -> Option<&WireFieldDescriptor> {
        self.deserialized.iter().find(|f| f.rust_name == rust_name)
    }
}

/// One service endpoint's contract, as `#[endpoint]` wrote it.
///
/// Field shapes are not inlined here. A type is described once, in its own
/// `TypeDescriptor` artifact, and referenced by name — the same DTO usually
/// serves several endpoints, and one description keeps them from drifting.
/// [`crate::wire::store::resolve`] joins the two into a [`ResolvedEndpoint`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndpointDescriptor {
    /// The service this endpoint belongs to, from `#[endpoint(service = "…")]`.
    pub service: String,
    /// The endpoint name — the handler's function name unless overridden.
    pub name: String,
    /// The marker type `#[endpoint]` generates, e.g. `get_item_endpoint`.
    pub endpoint_ident: String,
    /// The crate the endpoint is defined in.
    pub krate: String,
    /// HTTP method, uppercase.
    pub method: String,
    /// Route path, with `{param}` placeholders intact.
    pub path: String,
    /// Rust name of the request body type, or `NoBody`.
    pub request_type: String,
    /// Rust name of the response body type.
    pub response_type: String,
}

impl EndpointDescriptor {
    /// `service.name` — how a diagnostic names this endpoint.
    #[must_use]
    pub fn id(&self) -> String {
        format!("{}.{}", self.service, self.name)
    }

    /// The artifact file name for this endpoint, unique within a workspace.
    #[must_use]
    pub fn artifact_file_name(&self) -> String {
        format!(
            "endpoint.{}.{}.{}.json",
            self.krate, self.service, self.name
        )
    }
}

/// One DTO's shape, as `#[derive(WireShape)]` wrote it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypeDescriptor {
    /// The crate the type is defined in.
    pub krate: String,
    /// The type's shape.
    #[serde(flatten)]
    pub shape: WireTypeDescriptor,
}

impl TypeDescriptor {
    /// The artifact file name for this type, unique within a workspace.
    #[must_use]
    pub fn artifact_file_name(&self) -> String {
        format!("type.{}.{}.json", self.krate, self.shape.name)
    }
}

/// An endpoint with both of its type shapes filled in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedEndpoint {
    /// The endpoint itself.
    pub endpoint: EndpointDescriptor,
    /// What the endpoint accepts in its request body.
    pub request: WireTypeDescriptor,
    /// What the endpoint produces in its response body.
    pub response: WireTypeDescriptor,
}
