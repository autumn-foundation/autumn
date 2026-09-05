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
//! # What the root re-exports
//!
//! Everything a caller needs to *use* the sandbox: the plugin and host types,
//! the manifest vocabulary, the capability backends and their reference
//! implementations, and every `MAX_*` ceiling — because a ceiling nobody can
//! name is a ceiling nobody can plan against. What stays behind
//! `capability::render::` and `capability::audit::` is the machinery those
//! types are built from.
//!
//! Each module's own header explains its half in full; start with
//! [`manifest`] for what an operator reviews, [`host`] for what the sandbox
//! actually withholds, and `docs/guide/sandboxed-plugins.md` for the narrative.

pub mod artifact;
pub mod capability;
pub mod grants;
pub mod host;
pub mod manifest;
pub mod plugin;
pub mod slots;
pub mod wire;

/// Hand-written WAT guests the escape corpus is built from.
///
/// Test-only: exposed under `test-support` so the consolidated integration
/// suite and this crate's unit tests share one corpus.
#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub mod test_guests;

pub use artifact::{
    ArtifactError, MAX_ARTIFACT_BYTES, MAX_MANIFEST_BYTES, MAX_MODULE_BYTES, SandboxArtifact,
    read_bounded,
};
pub use capability::{
    ActivitySummary, CacheKvStore, CallResult, CallValue, CapabilityCall, CapabilityEvent,
    CapabilityOutcome, CapabilityRateLimiter, CapabilityRuntime, CapabilityServices, DenialReason,
    FragmentNode, JobSink, KvStore, MAX_EVENTS, MAX_KV_KEY_BYTES, MAX_LOG_EVENTS,
    MAX_OUTBOUND_HEADERS, MAX_ROW_COLUMNS, MAX_ROW_ID_BYTES, MAX_TARGET_CHARS,
    MAX_VALUE_TEXT_BYTES, MemoryJobSink, MemoryKvStore, MemoryPluginStore, NO_TENANT, OutboundHttp,
    OutboundRequest, OutboundResponse, PluginActivityLog, PluginJob, PluginRow, PluginStore,
    PluginValue, RecordingHttp, RenderError, Scope, StoreError,
};
pub use grants::{
    CapabilityGrants, CapabilityQuotas, ConsentDelta, MAX_GRANT_ENTRIES, MAX_GRANT_IDENT_LEN,
    MAX_HOST_LEN, MAX_QUOTA, is_grantable_host, is_grantable_ident, is_grantable_name,
};
pub use host::{
    CapabilityDenial, DeniedCapability, MAX_INIT_SECTION_BYTES, MAX_INIT_SEGMENTS,
    MAX_QUEUED_REPLY_BYTES, MAX_TABLE_ELEMENTS, SandboxFailure, SandboxHost, SandboxLoadError,
    SandboxOutcome, SandboxRenderOutcome,
};
pub use manifest::{
    DeclaredRoute, MAX_CONCURRENCY, MAX_FOOTPRINT_BYTES, MAX_FUEL, MAX_MEMORY_BYTES,
    MAX_REQUEST_BODY_BYTES, MAX_REQUEST_BODY_TIMEOUT_MS, MAX_RESPONSE_BYTES, ManifestError,
    ResourceLimits, SandboxCapability, SandboxManifest, WIRE_VERSION,
};
pub use plugin::{SANDBOX_ATTRIBUTION_HEADER, SandboxPluginError, SandboxedPlugin};
pub use slots::{RenderSlots, SlotError};
pub use wire::{
    ALLOWED_REQUEST_HEADERS, ALLOWED_RESPONSE_CONTENT_TYPES, ALLOWED_RESPONSE_HEADERS, OwnedRoutes,
    SandboxRequest, SandboxResponse, WireError, request_header_allowed, response_header_allowed,
};

// ── fuzzing seams (issue #1611's `__fuzz` convention) ────────────────────
//
// Everything here decodes bytes that came out of an artifact the operator
// explicitly did not audit, which is exactly the shape #1611 asks to be fuzzed.
// The functions are thin wrappers so `fuzz/` needs no knowledge of the module's
// internals and the crate-private wire types stay crate-private.

/// Decode a `.autumn-plugin` container. Fuzzing seam; not a public API.
#[doc(hidden)]
#[must_use]
pub fn __fuzz_read_artifact(bytes: &[u8]) -> bool {
    SandboxArtifact::read(bytes).is_ok()
}

/// Parse one NDJSON frame as a guest would have written it. Fuzzing seam.
#[doc(hidden)]
#[must_use]
pub fn __fuzz_parse_guest_frame(line: &str) -> bool {
    wire::from_line::<wire::GuestFrame>(line).is_ok()
}

/// Parse a manifest. Fuzzing seam; not a public API.
#[doc(hidden)]
#[must_use]
pub fn __fuzz_parse_manifest(src: &str) -> bool {
    SandboxManifest::parse(src).is_ok()
}

/// Parse a fragment tree and render it to HTML, as a render hook's answer is
/// (issue #1632). Fuzzing seam; not a public API.
///
/// The one place in this subsystem where guest-supplied structure becomes markup
/// that a *browser* then parses, so it is the one whose failure mode is stored
/// XSS rather than a refused request. The rendering ceilings — depth, node
/// count, bytes — are what the fuzzer is being pointed at: a tree inside every
/// structural bound can still be built to blow one of them, and the difference
/// between refusing and recursing is a stack.
#[doc(hidden)]
#[must_use]
pub fn __fuzz_render_fragment(line: &str) -> bool {
    let Ok(nodes) = serde_json::from_str::<Vec<capability::FragmentNode>>(line) else {
        return false;
    };
    capability::render::render(&nodes, 64 * 1024).is_ok()
}

/// The single-binary promise is a *manifest* property, not a source property.
///
/// "The app still deploys as one binary" holds because the sandbox is an
/// in-process interpreter — no daemon, no subprocess, no native codegen backend
/// and no sidecar artifact. One dependency edge would end that quietly, with a
/// symptom (a build that needs a C toolchain, a runtime that needs a helper on
/// PATH) three levels away from the line that caused it. These tests read this
/// crate's own manifest and fail on that edit directly.
#[cfg(test)]
mod manifest_guard {
    const MANIFEST: &str = include_str!("../../Cargo.toml");

    /// The `plugin-sandbox = [...]` line, comments dropped.
    fn feature_line() -> &'static str {
        MANIFEST
            .lines()
            .map(str::trim)
            .find(|line| line.starts_with("plugin-sandbox ="))
            .expect("the plugin-sandbox feature is declared")
    }

    #[test]
    fn the_feature_adds_exactly_one_dependency() {
        assert_eq!(
            feature_line(),
            r#"plugin-sandbox = ["dep:wasmi"]"#,
            "enabling the sandbox must pull in the interpreter and nothing else"
        );
    }

    #[test]
    fn the_interpreter_is_optional_so_a_default_build_never_links_it() {
        let wasmi = MANIFEST
            .lines()
            .map(str::trim)
            .find(|line| line.starts_with("wasmi ="))
            .expect("wasmi is a dependency");
        assert!(wasmi.contains("optional = true"), "{wasmi}");
        let default = MANIFEST
            .lines()
            .map(str::trim)
            .find(|line| line.starts_with("default = ["))
            .expect("a default feature set");
        assert!(
            !default.contains("plugin-sandbox"),
            "the sandbox must stay opt-in: {default}"
        );
    }
}
