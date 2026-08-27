//! Capability-sandboxed WASM plugins.

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

pub use artifact::{ArtifactError, SandboxArtifact};
pub use host::{
    CapabilityDenial, DeniedCapability, SandboxFailure, SandboxHost, SandboxLoadError,
    SandboxOutcome,
};
pub use manifest::{
    ManifestError, ResourceLimits, SandboxCapability, SandboxManifest, WIRE_VERSION,
};
pub use plugin::{SANDBOX_ATTRIBUTION_HEADER, SandboxPluginError, SandboxedPlugin};
pub use wire::{GuestFrame, HostFrame, SandboxRequest, SandboxResponse, WireError};
