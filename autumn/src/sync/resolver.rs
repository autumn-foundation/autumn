//! Pluggable conflict resolution.
//!
//! A conflict occurs when a pushed [`Change`] was based on a server version
//! that is no longer current (another device wrote in between). Resolvers
//! run **server-side**, inside the push transaction; the resolved row is
//! assigned a new version so every device — including the losing writer —
//! converges on it via its next pull.

use super::protocol::{Change, RemoteRow};

/// The resolver's verdict for one conflicting change.
#[derive(Debug, Clone, PartialEq)]
pub enum Resolution {
    /// Keep the server row's content (the pushed change loses).
    KeepServer,
    /// Apply the pushed change over the server row (the client wins).
    TakeClient,
    /// Replace the row's payload with a merged JSON document (the row is
    /// live after a merge, even if one side was a delete).
    Merge(serde_json::Value),
}

/// Server-side conflict policy, applied when a pushed change's
/// `base_version` does not match the server row's current version.
pub trait ConflictResolver: Send + Sync {
    /// Decide the winning state for a conflicting write.
    ///
    /// `client_device_id` is the pushing device; `client` is the pushed
    /// change; `server` is the current (newer) server row it collided with.
    fn resolve(&self, client_device_id: &str, client: &Change, server: &RemoteRow) -> Resolution;
}

/// Default last-write-wins resolver.
///
/// Compares the two conflicting writes' `updated_at` wall-clock stamps —
/// the only place device clocks are consulted, and only between the two
/// writes in conflict — and breaks exact ties deterministically on device
/// id. Replace it (any [`ConflictResolver`]) if your data needs merging or
/// clock trust is unacceptable.
#[derive(Debug, Clone, Copy, Default)]
pub struct LwwResolver;

impl ConflictResolver for LwwResolver {
    fn resolve(&self, _client_device_id: &str, _client: &Change, _server: &RemoteRow) -> Resolution {
        // RED-phase stub: always keeps the server row.
        Resolution::KeepServer
    }
}
