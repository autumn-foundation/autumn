//! Frame codec, authenticated envelope, and replay suppression.
//!
//! # Frame
//!
//! `u32` big-endian length prefix, then exactly that many bytes of JSON. `N` is
//! capped at [`MAX_FRAME_BYTES`] and the cap is checked **before any
//! allocation**, so a hostile prefix cannot make the receiver reserve 4 GiB.
//!
//! # Envelope
//!
//! ```text
//! { v, key_id, cluster, sender, incarnation, seq, payload, mac }
//! ```
//!
//! `payload` is the serialized [`ClusterMessage`] as a JSON string; `mac` is
//! lowercase-hex HMAC-SHA256 (via [`crate::security::hmac_sha256_hex`]) over
//! the **length-delimited** concatenation
//! `v ‖ cluster ‖ sender ‖ incarnation ‖ seq ‖ payload`, compared in constant
//! time with `subtle`. The MAC is verified **before** `serde_json` is allowed
//! near the payload.
//!
//! `key_id` is deliberately outside the signing input: it *selects* the key
//! rather than being protected by it, so flipping it selects a key that does
//! not exist and the MAC then fails.
//!
//! # Receive-path totality, with exactly one exception
//!
//! Every rejection is counted with a named reason and nothing ever panics.
//! Steps 2-7 drop the offending frame and continue the read loop. Step 1 — a
//! length prefix of `0` or over the cap — is the one deliberate exception: the
//! stream framing itself can no longer be trusted, so the **connection closes**
//! and the peer re-dials with backoff (see
//! [`RejectReason::closes_connection`]). Closing a connection is never an
//! eviction; connection state carries zero liveness meaning.
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

/// Envelope version. Any other value is dropped.
pub(crate) const WIRE_VERSION: u8 = 1;

/// Signing-key identifier. Reserved for rotation; always `0` in this slice.
pub(crate) const CURRENT_KEY_ID: u8 = 0;

/// Hard cap on a single frame's JSON body, checked before allocation.
pub(crate) const MAX_FRAME_BYTES: usize = 65_536;

/// Width of the big-endian length prefix.
pub(crate) const LENGTH_PREFIX_BYTES: usize = 4;

/// The messages a node can send. Internally tagged, so an unknown future
/// variant is a clean drop rather than a parse failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum ClusterMessage {
    /// The whole replicated document. This IS the heartbeat.
    StatePush {
        /// The sender's document at the moment it pushed.
        state: ClusterState,
    },
    /// Best-effort clean-departure notice. It carries no fields at all: it
    /// applies to the `(sender, incarnation)` pair in the authenticated
    /// envelope, so a captured leave can never be replayed against a newer
    /// incarnation of that node. Advisory only — the suspicion timeout is the
    /// correctness path.
    Leave,
}

/// The authenticated wrapper every frame carries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Envelope {
    /// Envelope version ([`WIRE_VERSION`]).
    pub(crate) v: u8,
    /// Signing-key id ([`CURRENT_KEY_ID`]). Outside the signing input.
    #[serde(default)]
    pub(crate) key_id: u8,
    /// Sender's cluster name — a frame from another cluster is refused even
    /// under the same secret.
    pub(crate) cluster: String,
    /// Authenticated sender id. The source address is never an identity.
    pub(crate) sender: NodeId,
    /// The sender's incarnation when the frame was produced.
    pub(crate) incarnation: Incarnation,
    /// Per-sender counter, reset to `0` when the incarnation increases.
    pub(crate) seq: u64,
    /// The inner message as a JSON string. Opaque until the MAC verifies.
    pub(crate) payload: String,
    /// Lowercase hex HMAC-SHA256, 64 characters.
    pub(crate) mac: String,
}

/// Why a frame was refused. Labels match the guide's receive-path table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum RejectReason {
    /// Step 1: `N == 0` or `N > MAX_FRAME_BYTES`. **Closes the connection.**
    Oversize,
    /// Step 2: the envelope is not well-formed JSON, or is missing a field.
    Malformed,
    /// Step 3: `v` is not [`WIRE_VERSION`].
    Version,
    /// Step 3: `key_id` is not [`CURRENT_KEY_ID`].
    KeyId,
    /// Step 3: the envelope names a different cluster.
    Cluster,
    /// Step 4: the MAC does not verify under the configured secret.
    Mac,
    /// Step 5: the authenticated sender is this node.
    SelfOrigin,
    /// Step 6: at or below the per-sender replay watermark.
    Replay,
    /// Step 7: the (authenticated) payload is not a known message.
    Payload,
}

impl RejectReason {
    /// Stable label used in metrics and logs.
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Oversize => "oversize",
            Self::Malformed => "malformed",
            Self::Version => "version",
            Self::KeyId => "key_id",
            Self::Cluster => "cluster",
            Self::Mac => "mac",
            Self::SelfOrigin => "self_origin",
            Self::Replay => "replay",
            Self::Payload => "payload",
        }
    }

    /// Whether this rejection invalidates the stream's framing and therefore
    /// requires closing the connection.
    ///
    /// Only step 1 does: after a bad length prefix there is no way to know
    /// where the next frame starts. Everything else drops the frame and reads
    /// on.
    pub(crate) const fn closes_connection(self) -> bool {
        matches!(self, Self::Oversize)
    }
}

/// The exact bytes the MAC covers: a length-delimited concatenation, so no
/// field value can be shifted into another field.
///
/// `L(x) = 8-byte big-endian byte length of x, followed by x`.
pub(crate) fn signing_input(
    v: u8,
    cluster: &str,
    sender: &str,
    incarnation: Incarnation,
    seq: u64,
    payload: &[u8],
) -> Vec<u8> {
    // RED-PHASE STUB: must emit L(v) ‖ L(cluster) ‖ L(sender) ‖ L(incarnation)
    // ‖ L(seq) ‖ L(payload).
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
/// [`MAX_FRAME_BYTES`] or cannot be serialized — the sender applies the same
/// cap rather than emitting something the peer must reject.
pub(crate) fn encode_frame(envelope: &Envelope) -> Option<Vec<u8>> {
    // RED-PHASE STUB.
    let _ = envelope;
    None
}

/// Read a length prefix, refusing `0` and anything over [`MAX_FRAME_BYTES`]
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

/// Verifies inbound frames and remembers replay watermarks.
///
/// Owned by the receive loop; never shared, so a plain `&mut self` suffices.
#[derive(Debug)]
pub(crate) struct FrameVerifier {
    cluster: String,
    local_id: NodeId,
    secret: Vec<u8>,
    /// `sender -> (highest incarnation accepted, highest seq at it)`. A higher
    /// incarnation adopts and **resets** the sequence watermark, which is what
    /// lets a restarted node rejoin.
    watermarks: BTreeMap<NodeId, (Incarnation, u64)>,
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
    /// Order is load-bearing and matches the guide's receive-path table:
    /// length cap, envelope parse, header checks, MAC, self-origin, replay
    /// watermark, payload parse. The payload is only parsed once the MAC has
    /// verified.
    pub(crate) fn accept(
        &mut self,
        frame: &[u8],
    ) -> Result<(Envelope, ClusterMessage), RejectReason> {
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

    /// The local node id whose frames are dropped as self-origin.
    pub(crate) fn local_id(&self) -> &str {
        &self.local_id
    }

    /// The secret this verifier checks MACs against.
    pub(crate) fn secret(&self) -> &[u8] {
        &self.secret
    }

    /// The `(incarnation, seq)` watermark recorded for `sender`.
    pub(crate) fn watermark(&self, sender: &str) -> Option<(Incarnation, u64)> {
        self.watermarks.get(sender).copied()
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

    fn state_push() -> ClusterMessage {
        ClusterMessage::StatePush {
            state: ClusterState::default(),
        }
    }

    fn envelope_for(
        secret: &[u8],
        cluster: &str,
        sender: &str,
        incarnation: u64,
        seq: u64,
        message: &ClusterMessage,
    ) -> Option<Envelope> {
        sign_envelope(secret, cluster, sender, incarnation, seq, message)
    }

    fn frame_for(
        secret: &[u8],
        cluster: &str,
        sender: &str,
        incarnation: u64,
        seq: u64,
        message: &ClusterMessage,
    ) -> Vec<u8> {
        envelope_for(secret, cluster, sender, incarnation, seq, message)
            .as_ref()
            .and_then(encode_frame)
            .unwrap_or_default()
    }

    #[test]
    fn envelope_roundtrip() {
        let message = state_push();
        let frame = frame_for(SECRET, CLUSTER, REMOTE, 1, 1, &message);
        assert!(
            !frame.is_empty(),
            "signing and encoding a state push must produce a frame"
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
        let envelope = envelope_for(SECRET, CLUSTER, REMOTE, 1, 1, &state_push());
        assert!(envelope.is_some(), "signing must produce an envelope");
        let Some(mut envelope) = envelope else { return };

        // Tamper with the signed payload without re-signing.
        envelope.payload.push_str("-tampered");
        let frame = encode_frame(&envelope).unwrap_or_default();

        let mut verifier = verifier();
        let result = verifier.accept(&frame);
        assert_eq!(
            result.err(),
            Some(RejectReason::Mac),
            "a flipped payload byte must fail the MAC before the payload is parsed"
        );
        assert_eq!(
            verifier.rejected_total(),
            1,
            "a rejected frame must be counted (a silent drop is not observable)"
        );
        assert!(
            !RejectReason::Mac.closes_connection(),
            "a MAC failure drops the frame and reads on — it never closes the connection"
        );
    }

    #[test]
    fn wrong_secret_rejected() {
        let frame = frame_for(
            b"a-completely-different-secret-xx",
            CLUSTER,
            REMOTE,
            1,
            1,
            &ClusterMessage::Leave,
        );
        let mut verifier = verifier();
        let result = verifier.accept(&frame);

        assert_eq!(
            result.err(),
            Some(RejectReason::Mac),
            "a frame signed with another secret must not verify"
        );
        assert_eq!(verifier.rejected_total(), 1, "the rejection must be counted");
    }

    #[test]
    fn wrong_cluster_name_rejected() {
        let frame = frame_for(
            SECRET,
            "some-other-cluster",
            REMOTE,
            1,
            1,
            &ClusterMessage::Leave,
        );
        let mut verifier = verifier();
        let result = verifier.accept(&frame);

        assert_eq!(
            result.err(),
            Some(RejectReason::Cluster),
            "a frame naming a different cluster must be refused even under the same secret"
        );
        assert_eq!(verifier.rejected_total(), 1, "the rejection must be counted");
    }

    #[test]
    fn stale_sequence_dropped() {
        let frame = frame_for(SECRET, CLUSTER, REMOTE, 1, 5, &ClusterMessage::Leave);
        let mut verifier = verifier();

        assert!(
            verifier.accept(&frame).is_ok(),
            "the first frame at (incarnation 1, seq 5) must be accepted"
        );
        assert_eq!(
            verifier.watermark(REMOTE),
            Some((1, 5)),
            "accepting a frame must raise the per-sender watermark"
        );

        let replayed = verifier.accept(&frame);
        assert_eq!(
            replayed.err(),
            Some(RejectReason::Replay),
            "replaying a frame at or below the watermark must be dropped"
        );
        assert_eq!(verifier.rejected_total(), 1, "the replay must be counted");

        // A HIGHER incarnation adopts and resets the sequence watermark — this
        // is what lets a restarted node rejoin instead of being replay-locked.
        let rejoined = frame_for(SECRET, CLUSTER, REMOTE, 2, 0, &ClusterMessage::Leave);
        assert!(
            verifier.accept(&rejoined).is_ok(),
            "a higher incarnation at seq 0 must be accepted, not treated as a replay"
        );
        assert_eq!(
            verifier.watermark(REMOTE),
            Some((2, 0)),
            "a higher incarnation must adopt and reset the sequence watermark"
        );

        // …and the dead incarnation can no longer speak.
        let stale_incarnation = frame_for(SECRET, CLUSTER, REMOTE, 1, 6, &ClusterMessage::Leave);
        assert_eq!(
            verifier.accept(&stale_incarnation).err(),
            Some(RejectReason::Replay),
            "a frame from a lower incarnation must be dropped once a higher one is known"
        );
    }

    #[test]
    fn self_origin_frame_dropped() {
        // Correctly signed, but the authenticated sender is us: a reflected
        // frame. It must be dropped by sender id, not by source address.
        let frame = frame_for(SECRET, CLUSTER, LOCAL, 1, 1, &ClusterMessage::Leave);
        let mut verifier = verifier();
        let result = verifier.accept(&frame);

        assert_eq!(
            result.err(),
            Some(RejectReason::SelfOrigin),
            "a frame whose authenticated sender is this node must be dropped"
        );
        assert_eq!(verifier.rejected_total(), 1, "the rejection must be counted");
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
            Some(RejectReason::Oversize),
            "the verifier must refuse an oversized declared length"
        );
        assert!(
            RejectReason::Oversize.closes_connection(),
            "a bad length prefix desynchronizes the framing, so the connection must close"
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
            let result = verifier.accept(bad);
            assert!(
                result.is_err(),
                "malformed input must be dropped, never accepted: {bad:?}"
            );
            assert!(
                result
                    .err()
                    .is_some_and(|reason| !reason.closes_connection()),
                "a malformed body drops the frame and reads on; only a bad length \
                 prefix closes the connection: {bad:?}"
            );
        }
        assert_eq!(
            LENGTH_PREFIX_BYTES, 4,
            "the length prefix width is part of the wire contract"
        );

        // Positive control: the receive path survives garbage and still accepts
        // a valid frame afterwards (totality means "continue", not "give up").
        let good = frame_for(SECRET, CLUSTER, REMOTE, 1, 1, &ClusterMessage::Leave);
        assert!(
            verifier.accept(&good).is_ok(),
            "after malformed input the verifier must still accept a valid frame"
        );
    }
}
