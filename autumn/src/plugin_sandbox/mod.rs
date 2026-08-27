//! Capability-sandboxed WASM plugins (issue #1609).
//!
//! > **Experimental.** Everything in this module — the wire protocol, the
//! > artifact container, and these types — is outside Autumn's SemVer
//! > commitments while the capability vocabulary settles. See `STABILITY.md`.
//!
//! Autumn has two plugin lanes, and they answer different questions.
//!
//! | | [`Plugin`](crate::plugin::Plugin) | [`SandboxedPlugin`] |
//! | --- | --- | --- |
//! | What it receives | the whole `AppBuilder` | nothing; the framework mounts it from a manifest |
//! | Ambient authority | the host process's | none |
//! | A panic in it | takes the process down | a 502 on its own prefix |
//! | Written in | Rust, compiled into your binary | anything that targets `wasm32-wasip1` |
//! | Reviewing it means reading | the crate and its dependency tree | one page of TOML and a digest |
//!
//! The native trait is the right trade for a plugin you wrote or a first-party
//! crate you already trust, and it is unchanged. This module is the trade for a
//! plugin you have not audited and do not intend to.
//!
//! # The flow, and where each piece enforces something
//!
//! ```text
//!  plugin.toml + plugin.wasm
//!        │  autumn plugin package
//!        ▼
//!  hello.autumn-plugin ──────────► artifact: the manifest describes THESE bytes
//!        │  SandboxedPlugin::from_file
//!        ▼
//!  manifest ─────────────────────► fail-closed: an unknown word is a refusal
//!        │
//!        ▼
//!  host ─────────────────────────► the guest's authority IS the shim's import list
//!        │
//!        ▼
//!  plugin ───────────────────────► the manifest's routes ARE the mount
//! ```
//!
//! Each module's own header explains its half in full; start with
//! [`manifest`] for what an operator reviews, [`host`] for what the sandbox
//! actually withholds, and `docs/guide/sandboxed-plugins.md` for the narrative.

pub mod artifact;
pub mod host;
pub mod manifest;
pub mod plugin;
pub mod wire;

/// Hand-written WAT guests the escape corpus is built from.
///
/// Test-only: exposed under `test-support` so the consolidated integration
/// suite and this crate's unit tests share one corpus.
#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub mod test_guests;

pub use artifact::{ArtifactError, MAX_MANIFEST_BYTES, MAX_MODULE_BYTES, SandboxArtifact};
pub use host::{
    CapabilityDenial, DeniedCapability, SandboxFailure, SandboxHost, SandboxLoadError,
    SandboxOutcome,
};
pub use manifest::{
    DeclaredRoute, MAX_CONCURRENCY, MAX_FOOTPRINT_BYTES, MAX_FUEL, MAX_MEMORY_BYTES,
    MAX_REQUEST_BODY_BYTES, MAX_RESPONSE_BYTES, ManifestError, ResourceLimits, SandboxCapability,
    SandboxManifest, WIRE_VERSION,
};
pub use plugin::{SANDBOX_ATTRIBUTION_HEADER, SandboxPluginError, SandboxedPlugin};
pub use wire::{
    ALLOWED_RESPONSE_CONTENT_TYPES, ALLOWED_RESPONSE_HEADERS, RESERVED_RESPONSE_HEADERS,
    SENSITIVE_REQUEST_HEADERS, SandboxRequest, SandboxResponse, WireError, response_header_allowed,
};
