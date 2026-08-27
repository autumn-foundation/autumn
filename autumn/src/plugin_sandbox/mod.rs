//! Capability-sandboxed WASM plugins.

pub mod artifact;
pub mod manifest;
pub mod wire;

pub use artifact::{ArtifactError, SandboxArtifact};
pub use wire::{GuestFrame, HostFrame, SandboxRequest, SandboxResponse, WireError};
pub use manifest::{
    ManifestError, ResourceLimits, SandboxCapability, SandboxManifest, WIRE_VERSION,
};
