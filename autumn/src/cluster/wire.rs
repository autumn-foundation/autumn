//! Frame codec, authenticated envelope, and replay suppression.
//!
//! # Frame
//!
//! `u32` big-endian length prefix, then that many bytes of JSON. The declared
//! length is checked against [`MAX_FRAME_BYTES`] **before** anything is
//! allocated, so a hostile prefix cannot make the receiver reserve 4 GiB.
//!
//! # Envelope
//!
//! ```text
//! { v, key_id, cluster, sender, incarnation, seq, payload, mac }
//! ```
//!
//! `payload` is the serialized [`ClusterMessage`] as a string; `mac` is
//! HMAC-SHA256 (via [`crate::security::hmac_sha256_hex`]) over the
//! length-delimited concatenation of `v ‖ cluster ‖ sender ‖ incarnation ‖ seq
//! ‖ payload`, compared in constant time with `subtle`. The MAC is verified
//! **before** `serde_json` is allowed near the payload.
//!
//! Each field in the signing input answers a specific attack:
//!
//! - `sender` — a reflected frame cannot masquerade as its own origin;
//! - `cluster` — a frame from a different cluster sharing the secret is refused;
//! - `incarnation` + `seq` — per-`(sender, incarnation)` high-watermarks drop
//!   replays, and re-keying the watermark on incarnation survives a restart;
//! - `key_id` — reserved (always `0` in this slice) for future key rotation;
//! - `v` — reserved for a future wire revision.
//!
//! Self-origin frames are dropped by the **authenticated sender id**, never by
//! source address.
//!
//! Every decode path is total: malformed input is dropped and counted, never
//! panicked on and never fatal to the receive loop.
//!
//! RED PHASE (TDD): bodies are inert stubs — see the module docs on [`super`].

// autumn-determinism-gate: production code in this module must read time and
// mint identifiers through the framework's injected seams (ClockSource /
// Entropy), never `Instant::now()` / `Utc::now()` / `SystemTime::now()` /
// `Uuid::new_v4()` directly. See CONTRIBUTING.md "Determinism seam gate"
// (issue #1797). Justify exceptions with
// #[allow(clippy::disallowed_methods, reason = "…")] at the narrowest scope.
#![cfg_attr(not(test), deny(clippy::disallowed_methods))]
// autumn-panic-gate: request-path module — production code path must be panic-free.
// See CONTRIBUTING.md "Request-path panic gate". Justify exceptions with
// #[allow(clippy::<lint>, reason = "…")] at the narrowest scope.
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::todo,
        clippy::unimplemented,
        clippy::indexing_slicing,
        clippy::string_slice,
        clippy::arithmetic_side_effects,
    )
)]

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::membership::ClusterState;
use super::{Incarnation, NodeId};

/// Wire revision. Bumped only for an incompatible envelope change.
pub(crate) const WIRE_VERSION: u8 = 1;

/// Signing-key identifier. Reserved for rotation; always `0` in this slice.
pub(crate) const CURRENT_KEY_ID: u8 = 0;

/// Hard cap on a single frame's JSON body, checked before allocation.
pub(crate) const MAX_FRAME_BYTES: usize = 65_536;

/// Width of the big-endian length prefix.
pub(crate) const LENGTH_PREFIX_BYTES: usize = 4;

/// The messages a node can send. Exactly one of them is periodic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ClusterMessage {
    /// The whole replicated document. This IS the heartbeat.
    StatePush(ClusterState),
    /// Best-effort clean-departure notice, scoped to the sender's incarnation.
    /// Advisory only: the suspicion timeout is the correctness path.
    Leave,
}

/// The authenticated wrapper every frame carries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Envelope {
    /// Wire revision ([`WIRE_VERSION`]).
    pub(crate) v: u8,
    /// Signing-key id ([`CURRENT_KEY_ID`]).
    #[serde(default)]
    pub(crate) key_id: u8,
    /// Cluster name — a frame from another cluster is refused even under the
    /// same secret.
    pub(crate) cluster: String,
    /// Authenticated sender id.
    pub(crate) sender: NodeId,
    /// The sender's incarnation, which keys the replay watermark.
    pub(crate) incarnation: Incarnation,
    /// Per-`(sender, incarnation)` monotonic sequence number.
    pub(crate) seq: u64,
    /// The serialized [`ClusterMessage`].
    pub(crate) payload: String,
    /// Hex HMAC-SHA256 over [`signing_input`].
    pub(crate) mac: String,
}

/// Why a frame was refused. Every variant is a counted, observable outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum RejectReason {
    /// The bytes are not a well-formed frame or envelope.
    Malformed,
    /// The declared length exceeds [`MAX_FRAME_BYTES`].
    Oversized,
    /// The MAC does not verify under the configured secret.
    BadMac,
    /// The envelope names a different cluster.
    WrongCluster,
    /// The envelope names an unsupported wire revision or key id.
    UnsupportedVersion,
    /// The sequence number is at or below the known high-watermark.
    Replay,
    /// The authenticated sender is this node.
    SelfOrigin,
}

impl RejectReason {
    /// Stable label used in metrics and logs.
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Malformed => "malformed",
            Self::Oversized => "oversized",
            Self::BadMac => "bad_mac",
            Self::WrongCluster => "wrong_cluster",
            Self::UnsupportedVersion => "unsupported_version",
            Self::Replay => "replay",
            Self::SelfOrigin => "self_origin",
        }
    }
}

/// The exact bytes the MAC covers: a length-delimited concatenation, so no
/// field boundary can be shifted without changing the input.
pub(crate) fn signing_input(
    v: u8,
    cluster: &str,
    sender: &str,
    incarnation: Incarnation,
    seq: u64,
    payload: &[u8],
) -> Vec<u8> {
    // RED-PHASE STUB: must emit `v ‖ len(cluster) ‖ cluster ‖ … ‖ payload`.
    let _ = (v, cluster, sender, incarnation, seq, payload);
    Vec::new()
}

/// Serialize `message` and wrap it in a signed [`Envelope`].
///
/// Returns `None` when the message cannot be serialized — a caller must drop
/// the send, never panic.
pub(crate) fn sign_envelope(
    secret: &[u8],
    cluster: &str,
    sender: &str,
    incarnation: Incarnation,
    seq: u64,
    message: &ClusterMessage,
) -> Option<Envelope> {
    // RED-PHASE STUB: serialize, build the signing input, HMAC it.
    let _ = (secret, cluster, sender, incarnation, seq, message);
    None
}

/// Length-prefix a serialized envelope. `None` when it does not fit
/// [`MAX_FRAME_BYTES`] or cannot be serialized.
pub(crate) fn encode_frame(envelope: &Envelope) -> Option<Vec<u8>> {
    // RED-PHASE STUB.
    let _ = envelope;
    None
}

/// Read a length prefix, refusing zero and anything over [`MAX_FRAME_BYTES`]
/// **before** a buffer of that size is reserved.
pub(crate) const fn frame_len(prefix: [u8; LENGTH_PREFIX_BYTES]) -> Option<usize> {
    // RED-PHASE STUB: must accept a legal length and refuse 0 / oversized.
    let _ = prefix;
    None
}

/// Parse a complete frame (prefix + body) into an envelope. Total: any
/// malformed input yields `None`.
pub(crate) fn decode_frame(frame: &[u8]) -> Option<Envelope> {
    // RED-PHASE STUB.
    let _ = frame;
    None
}

/// Verifies inbound frames and remembers replay high-watermarks.
///
/// Owned by the receive loop; never shared, so a plain `&mut self` suffices.
#[derive(Debug)]
pub(crate) struct FrameVerifier {
    cluster: String,
    local_id: NodeId,
    secret: Vec<u8>,
    /// `(sender, incarnation) -> highest sequence accepted`.
    watermarks: BTreeMap<(NodeId, Incarnation), u64>,
    rejected: u64,
}

impl FrameVerifier {
    /// A verifier for `cluster`, refusing frames whose sender is `local_id`.
    pub(crate) fn new(
        cluster: impl Into<String>,
        local_id: impl Into<String>,
        secret: Vec<u8>,
    ) -> Self {
        Self {
            cluster: cluster.into(),
            local_id: local_id.into(),
            secret,
            watermarks: BTreeMap::new(),
            rejected: 0,
        }
    }

    /// Verify and decode one frame.
    ///
    /// Order is load-bearing: length cap, then envelope parse, then MAC, then
    /// cluster/version, then self-origin, then the replay watermark. The
    /// payload is only parsed once the MAC has verified.
    pub(crate) fn accept(&mut self, frame: &[u8]) -> Result<(Envelope, ClusterMessage), RejectReason> {
        // RED-PHASE STUB: refuses everything and counts nothing.
        let _ = frame;
        Err(RejectReason::Malformed)
    }

    /// The number of frames this verifier has refused, for any reason.
    pub(crate) const fn rejected_total(&self) -> u64 {
        self.rejected
    }

    /// The cluster name this verifier accepts.
    pub(crate) fn cluster(&self) -> &str {
        &self.cluster
    }

    /// The secret this verifier checks MACs against.
    pub(crate) fn secret(&self) -> &[u8] {
        &self.secret
    }

    /// The highest sequence accepted so far for `(sender, incarnation)`.
    pub(crate) fn watermark(&self, sender: &str, incarnation: Incarnation) -> Option<u64> {
        self.watermarks
            .get(&(sender.to_owned(), incarnation))
            .copied()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ClusterMessage, Envelope, FrameVerifier, LENGTH_PREFIX_BYTES, MAX_FRAME_BYTES,
        RejectReason, encode_frame, frame_len, sign_envelope,
    };
    use crate::cluster::membership::ClusterState;

    const SECRET: &[u8] = b"cluster-test-secret-0123456789ab";
    const CLUSTER: &str = "autumn";
    const LOCAL: &str = "node-local";
    const REMOTE: &str = "node-remote";

    fn verifier() -> FrameVerifier {
        FrameVerifier::new(CLUSTER, LOCAL, SECRET.to_vec())
    }

    fn envelope_for(
        secret: &[u8],
        cluster: &str,
        sender: &str,
        seq: u64,
        message: &ClusterMessage,
    ) -> Option<Envelope> {
        sign_envelope(secret, cluster, sender, 1, seq, message)
    }

    fn frame_for(
        secret: &[u8],
        cluster: &str,
        sender: &str,
        seq: u64,
        message: &ClusterMessage,
    ) -> Vec<u8> {
        envelope_for(secret, cluster, sender, seq, message)
            .as_ref()
            .and_then(encode_frame)
            .unwrap_or_default()
    }

    #[test]
    fn envelope_roundtrip() {
        let message = ClusterMessage::StatePush(ClusterState::default());
        let frame = frame_for(SECRET, CLUSTER, REMOTE, 1, &message);
        assert!(
            !frame.is_empty(),
            "signing and encoding a StatePush must produce a frame"
        );

        let result = verifier().accept(&frame);
        assert!(
            matches!(&result, Ok((envelope, decoded))
                if envelope.sender == REMOTE && decoded == &message),
            "a correctly signed frame must round-trip to its envelope and message; got {result:?}"
        );
    }

    #[test]
    fn tampered_frame_rejected() {
        let message = ClusterMessage::StatePush(ClusterState::default());
        let envelope = envelope_for(SECRET, CLUSTER, REMOTE, 1, &message);
        assert!(envelope.is_some(), "signing must produce an envelope");
        let Some(mut envelope) = envelope else { return };

        // Tamper with the signed payload without re-signing.
        envelope.payload.push_str("-tampered");
        let frame = encode_frame(&envelope).unwrap_or_default();

        let mut verifier = verifier();
        let result = verifier.accept(&frame);
        assert_eq!(
            result.err(),
            Some(RejectReason::BadMac),
            "a flipped payload byte must fail the MAC before the payload is parsed"
        );
        assert_eq!(
            verifier.rejected_total(),
            1,
            "a rejected frame must be counted (a silent drop is not observable)"
        );
    }

    #[test]
    fn wrong_secret_rejected() {
        let frame = frame_for(b"a-completely-different-secret-xx", CLUSTER, REMOTE, 1, &ClusterMessage::Leave);
        let mut verifier = verifier();
        let result = verifier.accept(&frame);

        assert_eq!(
            result.err(),
            Some(RejectReason::BadMac),
            "a frame signed with another secret must not verify"
        );
        assert_eq!(
            verifier.rejected_total(),
            1,
            "the rejection must be counted"
        );
    }

    #[test]
    fn wrong_cluster_name_rejected() {
        let frame = frame_for(SECRET, "some-other-cluster", REMOTE, 1, &ClusterMessage::Leave);
        let mut verifier = verifier();
        let result = verifier.accept(&frame);

        assert_eq!(
            result.err(),
            Some(RejectReason::WrongCluster),
            "a frame naming a different cluster must be refused even under the same secret"
        );
        assert_eq!(
            verifier.rejected_total(),
            1,
            "the rejection must be counted"
        );
    }

    #[test]
    fn stale_sequence_dropped() {
        let frame = frame_for(SECRET, CLUSTER, REMOTE, 5, &ClusterMessage::Leave);
        let mut verifier = verifier();

        assert!(
            verifier.accept(&frame).is_ok(),
            "the first frame at seq 5 must be accepted"
        );
        assert_eq!(
            verifier.watermark(REMOTE, 1),
            Some(5),
            "accepting seq 5 must raise the (sender, incarnation) high-watermark"
        );

        let replayed = verifier.accept(&frame);
        assert_eq!(
            replayed.err(),
            Some(RejectReason::Replay),
            "replaying a frame at or below the watermark must be dropped"
        );
        assert_eq!(
            verifier.rejected_total(),
            1,
            "the replay must be counted"
        );
    }

    #[test]
    fn self_origin_frame_dropped() {
        // Correctly signed, but the authenticated sender is us: a reflected
        // frame. It must be dropped by sender id, not by source address.
        let frame = frame_for(SECRET, CLUSTER, LOCAL, 1, &ClusterMessage::Leave);
        let mut verifier = verifier();
        let result = verifier.accept(&frame);

        assert_eq!(
            result.err(),
            Some(RejectReason::SelfOrigin),
            "a frame whose authenticated sender is this node must be dropped"
        );
        assert_eq!(
            verifier.rejected_total(),
            1,
            "the rejection must be counted"
        );
    }

    #[test]
    fn oversized_frame_rejected_before_alloc() {
        let cap = u32::try_from(MAX_FRAME_BYTES).unwrap_or(u32::MAX);

        assert_eq!(
            frame_len(cap.saturating_add(1).to_be_bytes()),
            None,
            "a length prefix above MAX_FRAME_BYTES must be refused before allocating"
        );
        assert_eq!(
            frame_len(u32::MAX.to_be_bytes()),
            None,
            "a 4 GiB length prefix must be refused before allocating"
        );
        assert_eq!(
            frame_len(0u32.to_be_bytes()),
            None,
            "a zero-length frame is malformed"
        );
        assert_eq!(
            frame_len(64u32.to_be_bytes()),
            Some(64),
            "a legal length prefix must be accepted (otherwise nothing can ever be read)"
        );

        let mut oversized = cap.saturating_add(1).to_be_bytes().to_vec();
        oversized.extend_from_slice(b"{}");
        assert_eq!(
            verifier().accept(&oversized).err(),
            Some(RejectReason::Oversized),
            "the verifier must refuse an oversized declared length"
        );
    }

    #[test]
    fn truncated_or_malformed_frame_dropped_without_panic() {
        let mut verifier = verifier();
        let mut truncated = 64u32.to_be_bytes().to_vec();
        truncated.extend_from_slice(b"only-a-few-bytes");

        for bad in [
            b"".as_slice(),
            b"\x00".as_slice(),
            &truncated,
            b"not-a-frame-at-all".as_slice(),
        ] {
            assert!(
                verifier.accept(bad).is_err(),
                "malformed input must be dropped, never accepted: {bad:?}"
            );
        }
        assert!(
            LENGTH_PREFIX_BYTES == 4,
            "the length prefix width is part of the wire contract"
        );

        // Positive control: the receive path survives garbage and still accepts
        // a valid frame afterwards (totality means "continue", not "give up").
        let good = frame_for(SECRET, CLUSTER, REMOTE, 1, &ClusterMessage::Leave);
        assert!(
            verifier.accept(&good).is_ok(),
            "after malformed input the verifier must still accept a valid frame"
        );
    }
}
