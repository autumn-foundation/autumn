//! Pluggable conflict resolution.
//!
//! A conflict occurs when a pushed [`Change`] was based on a server version
//! that is no longer current (another device wrote in between). Resolvers
//! run **server-side**, inside the push transaction; the resolved row is
//! assigned a new version so every device — including the losing writer —
//! converges on it via its next pull.

use std::cmp::Ordering;

use super::protocol::{Change, RemoteRow};

/// The resolver's verdict for one conflicting change.
#[derive(Debug, Clone, PartialEq, Eq)]
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
/// writes in conflict — and breaks exact ties deterministically on the
/// lexicographically greater device id. Replace it (any
/// [`ConflictResolver`]) if your data needs merging or clock trust is
/// unacceptable.
#[derive(Debug, Clone, Copy, Default)]
pub struct LwwResolver;

impl ConflictResolver for LwwResolver {
    fn resolve(&self, client_device_id: &str, client: &Change, server: &RemoteRow) -> Resolution {
        match client.updated_at.cmp(&server.updated_at) {
            Ordering::Greater => Resolution::TakeClient,
            Ordering::Less => Resolution::KeepServer,
            Ordering::Equal => {
                if client_device_id > server.device_id.as_str() {
                    Resolution::TakeClient
                } else {
                    Resolution::KeepServer
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::protocol::Op;
    use chrono::{Duration, Utc};

    fn client_change(updated_at: chrono::DateTime<Utc>) -> Change {
        Change {
            change_id: "c1".into(),
            collection: "notes".into(),
            pk: "n1".into(),
            op: Op::Upsert,
            payload: Some(serde_json::json!({"v": "client"})),
            base_version: 1,
            updated_at,
        }
    }

    fn server_row(updated_at: chrono::DateTime<Utc>, device_id: &str) -> RemoteRow {
        RemoteRow {
            collection: "notes".into(),
            pk: "n1".into(),
            payload: Some(serde_json::json!({"v": "server"})),
            version: 2,
            deleted: false,
            updated_at,
            device_id: device_id.into(),
        }
    }

    #[test]
    fn newer_client_write_wins() {
        let now = Utc::now();
        let verdict = LwwResolver.resolve(
            "device-b",
            &client_change(now + Duration::seconds(5)),
            &server_row(now, "device-a"),
        );
        assert_eq!(verdict, Resolution::TakeClient);
    }

    #[test]
    fn newer_server_write_wins() {
        let now = Utc::now();
        let verdict = LwwResolver.resolve(
            "device-b",
            &client_change(now - Duration::seconds(5)),
            &server_row(now, "device-a"),
        );
        assert_eq!(verdict, Resolution::KeepServer);
    }

    #[test]
    fn exact_tie_breaks_on_device_id_deterministically() {
        let now = Utc::now();
        let win = LwwResolver.resolve(
            "device-b",
            &client_change(now),
            &server_row(now, "device-a"),
        );
        assert_eq!(win, Resolution::TakeClient, "greater device id wins ties");
        let lose = LwwResolver.resolve(
            "device-a",
            &client_change(now),
            &server_row(now, "device-b"),
        );
        assert_eq!(lose, Resolution::KeepServer);
    }
}
