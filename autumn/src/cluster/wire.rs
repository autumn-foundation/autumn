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
// `pub` throughout this file is crate-visible only: the enclosing `cluster`
// submodule is itself `pub(crate)`, so nothing here escapes the crate
// (clippy::redundant_pub_crate).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq as _;

use super::membership::ClusterState;
use super::{Incarnation, NodeId};
use crate::security::hmac_sha256_hex;

/// Envelope version. Any other value is dropped.
pub const WIRE_VERSION: u8 = 1;

/// Signing-key identifier. Reserved for rotation; always `0` in this slice.
pub const CURRENT_KEY_ID: u8 = 0;

/// Hard cap on a single frame's JSON body, checked before allocation.
pub const MAX_FRAME_BYTES: usize = 65_536;

/// Width of the big-endian length prefix.
pub const LENGTH_PREFIX_BYTES: usize = 4;

/// The messages a node can send. Internally tagged, so an unknown future
/// variant is a clean drop rather than a parse failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClusterMessage {
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
pub struct Envelope {
    /// Envelope version ([`WIRE_VERSION`]).
    pub v: u8,
    /// Signing-key id ([`CURRENT_KEY_ID`]). Outside the signing input.
    ///
    /// **Required, deliberately not `#[serde(default)]`.** The guide's receive
    /// path is normative that an envelope missing a field is dropped as
    /// `malformed`, and a default here would silently make one documented field
    /// optional — an alternate implementation written against the published
    /// table would then disagree with this parser about what is well-formed.
    pub key_id: u8,
    /// Sender's cluster name — a frame from another cluster is refused even
    /// under the same secret.
    pub cluster: String,
    /// Authenticated sender id. The source address is never an identity.
    pub sender: NodeId,
    /// The sender's incarnation when the frame was produced.
    pub incarnation: Incarnation,
    /// Per-sender counter, reset to `0` when the incarnation increases.
    pub seq: u64,
    /// The inner message as a JSON string. Opaque until the MAC verifies.
    pub payload: String,
    /// Lowercase hex HMAC-SHA256, 64 characters.
    pub mac: String,
}

/// Why a frame was refused. Labels match the guide's receive-path table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RejectReason {
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
    pub const fn label(self) -> &'static str {
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
    pub const fn closes_connection(self) -> bool {
        matches!(self, Self::Oversize)
    }
}

/// The exact bytes the MAC covers: a length-delimited concatenation, so no
/// field value can be shifted into another field.
///
/// `L(x) = 8-byte big-endian byte length of x, followed by x`.
pub fn signing_input(
    v: u8,
    cluster: &str,
    sender: &str,
    incarnation: Incarnation,
    seq: u64,
    payload: &[u8],
) -> Vec<u8> {
    /// `L(x)`: the 8-byte big-endian length of `bytes`, then `bytes` itself.
    fn delimited(out: &mut Vec<u8>, bytes: &[u8]) {
        let len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(bytes);
    }

    // Six delimiters (48 bytes) plus the three fixed-width values.
    let mut out = Vec::with_capacity(
        65_usize
            .saturating_add(cluster.len())
            .saturating_add(sender.len())
            .saturating_add(payload.len()),
    );
    delimited(&mut out, &[v]);
    delimited(&mut out, cluster.as_bytes());
    delimited(&mut out, sender.as_bytes());
    delimited(&mut out, &incarnation.to_be_bytes());
    delimited(&mut out, &seq.to_be_bytes());
    delimited(&mut out, payload);
    out
}

/// Serialize `message` and wrap it in a signed [`Envelope`].
///
/// Returns `None` when the message cannot be serialized — a caller must drop
/// the send, never panic.
pub fn sign_envelope(
    secret: &[u8],
    cluster: &str,
    sender: &str,
    incarnation: Incarnation,
    seq: u64,
    message: &ClusterMessage,
) -> Option<Envelope> {
    let payload = serde_json::to_string(message).ok()?;
    let mac = hmac_sha256_hex(
        secret,
        &signing_input(
            WIRE_VERSION,
            cluster,
            sender,
            incarnation,
            seq,
            payload.as_bytes(),
        ),
    );
    Some(Envelope {
        v: WIRE_VERSION,
        key_id: CURRENT_KEY_ID,
        cluster: cluster.to_owned(),
        sender: sender.to_owned(),
        incarnation,
        seq,
        payload,
        mac,
    })
}

/// Length-prefix a serialized envelope. `None` when it does not fit
/// [`MAX_FRAME_BYTES`] or cannot be serialized — the sender applies the same
/// cap rather than emitting something the peer must reject.
pub fn encode_frame(envelope: &Envelope) -> Option<Vec<u8>> {
    let body = serde_json::to_vec(envelope).ok()?;
    // The sender applies the receiver's cap rather than emitting a frame the
    // peer is obliged to refuse (and to close the connection over).
    if body.is_empty() || body.len() > MAX_FRAME_BYTES {
        return None;
    }
    let len = u32::try_from(body.len()).ok()?;
    Some(framed(len.to_be_bytes(), &body))
}

/// How many bytes [`encode_frame`]'s body would be, whether or not it fits
/// [`MAX_FRAME_BYTES`].
///
/// Only ever called on the failure path, so paying for a second serialization
/// buys the one number an operator needs — "how far over the cap am I?" — at no
/// cost to the path that succeeds. `None` when the envelope cannot be
/// serialized at all.
pub fn encoded_body_len(envelope: &Envelope) -> Option<usize> {
    serde_json::to_vec(envelope).ok().map(|body| body.len())
}

/// Join a length prefix and the body it declares into one frame buffer.
///
/// The single place the on-wire layout `prefix ‖ body` is spelled out: the
/// encoder builds a frame this way and the TCP reader re-assembles one this way
/// after reading the two halves separately, and a reader that disagreed with
/// the encoder about the layout would be a bug nothing else could catch.
pub fn framed(prefix: [u8; LENGTH_PREFIX_BYTES], body: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(body.len().saturating_add(LENGTH_PREFIX_BYTES));
    frame.extend_from_slice(&prefix);
    frame.extend_from_slice(body);
    frame
}

/// Read a length prefix, refusing `0` and anything over [`MAX_FRAME_BYTES`]
/// **before** a buffer of that size is reserved.
pub const fn frame_len(prefix: [u8; LENGTH_PREFIX_BYTES]) -> Option<usize> {
    // The comparison happens on the declared `u32`, so nothing of that size is
    // ever reserved: an attacker's 4 GiB prefix costs four bytes and a branch.
    match u32::from_be_bytes(prefix) {
        0 => None,
        declared if declared as usize > MAX_FRAME_BYTES => None,
        declared => Some(declared as usize),
    }
}

/// Take exactly the body a frame declares.
///
/// [`RejectReason::Oversize`] is the one connection-fatal verdict (step 1: a
/// declared length of `0` or over the cap). [`RejectReason::Malformed`] covers
/// a buffer too short to hold the prefix, or short of the body it declares —
/// a partial read, which says nothing about the framing itself.
fn frame_body(frame: &[u8]) -> Result<&[u8], RejectReason> {
    let prefix: [u8; LENGTH_PREFIX_BYTES] = frame
        .get(..LENGTH_PREFIX_BYTES)
        .and_then(|head| <[u8; LENGTH_PREFIX_BYTES]>::try_from(head).ok())
        .ok_or(RejectReason::Malformed)?;
    let declared = frame_len(prefix).ok_or(RejectReason::Oversize)?;
    frame
        .get(LENGTH_PREFIX_BYTES..LENGTH_PREFIX_BYTES.saturating_add(declared))
        .ok_or(RejectReason::Malformed)
}

/// Verifies inbound frames and remembers replay watermarks.
///
/// Owned by the receive loop; never shared, so a plain `&mut self` suffices.
///
/// [`Debug`] is hand-written and **omits the secret**: the key arrives here as
/// a plain `Vec<u8>` copied out of a
/// [`SecretString`](secrecy::SecretString), and a derived `Debug` would print
/// it as a byte array the first time somebody added `?verifier` to a log line.
pub struct FrameVerifier {
    cluster: String,
    local_id: NodeId,
    secret: Vec<u8>,
    /// `sender -> (highest incarnation accepted, highest seq at it)`. A higher
    /// incarnation adopts and **resets** the sequence watermark, which is what
    /// lets a restarted node rejoin.
    watermarks: BTreeMap<NodeId, (Incarnation, u64)>,
    rejected: u64,
}

impl std::fmt::Debug for FrameVerifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never the secret, and never the watermark table (unbounded).
        f.debug_struct("FrameVerifier")
            .field("cluster", &self.cluster)
            .field("local_id", &self.local_id)
            .field("senders", &self.watermarks.len())
            .field("rejected", &self.rejected)
            .finish_non_exhaustive()
    }
}

impl FrameVerifier {
    /// A verifier for `cluster`, refusing frames whose sender is `local_id`.
    pub fn new(cluster: impl Into<String>, local_id: impl Into<String>, secret: Vec<u8>) -> Self {
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
    pub fn accept(&mut self, frame: &[u8]) -> Result<(Envelope, ClusterMessage), RejectReason> {
        match self.verify(frame) {
            Ok(accepted) => Ok(accepted),
            Err(reason) => {
                // Every refusal is counted, whatever the reason: a silent drop
                // is not observable, and `frames_rejected_total{reason=…}` is
                // how an operator sees somebody talking to the port.
                self.rejected = self.rejected.saturating_add(1);
                Err(reason)
            }
        }
    }

    /// The receive path proper. [`Self::accept`] owns the counter so that no
    /// early return here can forget to increment it.
    fn verify(&mut self, frame: &[u8]) -> Result<(Envelope, ClusterMessage), RejectReason> {
        // 1. Length prefix — refused before anything of that size is reserved.
        let body = frame_body(frame)?;

        // 2. Envelope parse. Only the fixed envelope is parsed here; the
        //    payload stays an opaque string until the MAC verifies.
        let envelope: Envelope =
            serde_json::from_slice(body).map_err(|_| RejectReason::Malformed)?;

        // 3. Header checks.
        if envelope.v != WIRE_VERSION {
            return Err(RejectReason::Version);
        }
        if envelope.key_id != CURRENT_KEY_ID {
            return Err(RejectReason::KeyId);
        }
        if envelope.cluster != self.cluster {
            return Err(RejectReason::Cluster);
        }

        // 4. MAC, in constant time, before `serde_json` is allowed near the
        //    payload. Nothing below this line runs on unauthenticated bytes.
        let expected = hmac_sha256_hex(
            &self.secret,
            &signing_input(
                envelope.v,
                &envelope.cluster,
                &envelope.sender,
                envelope.incarnation,
                envelope.seq,
                envelope.payload.as_bytes(),
            ),
        );
        if !bool::from(expected.as_bytes().ct_eq(envelope.mac.as_bytes())) {
            return Err(RejectReason::Mac);
        }

        // 5. Self-origin, decided on the authenticated sender and never on the
        //    peer address.
        if envelope.sender == self.local_id {
            return Err(RejectReason::SelfOrigin);
        }

        // 6. Replay watermark: lower incarnation, or the same incarnation
        //    without a strictly higher seq, is a replay.
        if let Some(&(incarnation, seq)) = self.watermarks.get(&envelope.sender)
            && (envelope.incarnation < incarnation
                || (envelope.incarnation == incarnation && envelope.seq <= seq))
        {
            return Err(RejectReason::Replay);
        }

        // 7. Payload parse — only now.
        let message: ClusterMessage =
            serde_json::from_str(&envelope.payload).map_err(|_| RejectReason::Payload)?;

        // Accepted: adopt the incarnation (resetting the sequence watermark
        // when it rose) and remember the sequence at it.
        self.watermarks.insert(
            envelope.sender.clone(),
            (envelope.incarnation, envelope.seq),
        );
        Ok((envelope, message))
    }

    /// Forget everything remembered about `sender`, so its next frame is
    /// judged as if it had never been heard from.
    ///
    /// **Pruning a member must forget it whole.** A `Left` tombstone is pruned
    /// once the departure has had ten suspicion timeouts to propagate; from
    /// that moment the document holds no record of the departed node and
    /// nothing pushes to it, so the refutation channel that lets a node
    /// returning at a *lower* incarnation (a clock that stepped backwards)
    /// argue its way back is gone. Keeping the watermark past that point turns
    /// the returning node into a permanent one-way partition — every frame it
    /// sends is dropped as a replay and nothing can ever tell it why. Dropping
    /// the row re-opens a replay window only for a sender the node has already
    /// forgotten entirely, which is exactly the same window a fresh boot has.
    pub fn forget(&mut self, sender: &str) {
        self.watermarks.remove(sender);
    }

    /// The number of frames this verifier has refused, for any reason.
    pub const fn rejected_total(&self) -> u64 {
        self.rejected
    }

    /// The `(incarnation, seq)` watermark recorded for `sender`.
    ///
    /// Test-only: the watermark is replay-protection bookkeeping, and exposing
    /// it to production code would invite somebody to make a decision on it
    /// outside [`Self::accept`], which is the one place that may.
    #[cfg(test)]
    pub fn watermark(&self, sender: &str) -> Option<(Incarnation, u64)> {
        self.watermarks.get(sender).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CURRENT_KEY_ID, ClusterMessage, Envelope, FrameVerifier, LENGTH_PREFIX_BYTES,
        MAX_FRAME_BYTES, RejectReason, WIRE_VERSION, encode_frame, frame_len, framed,
        sign_envelope, signing_input,
    };
    use crate::cluster::membership::ClusterState;
    use crate::security::hmac_sha256_hex;

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

    /// Length-prefix arbitrary bytes, so a test can feed the verifier a body
    /// that is not something [`Envelope`] can produce.
    fn frame_bytes(body: &[u8]) -> Vec<u8> {
        let len = u32::try_from(body.len()).unwrap_or(u32::MAX);
        framed(len.to_be_bytes(), body)
    }

    /// A correctly signed envelope carrying `payload` verbatim — the only way
    /// to reach the payload-parse step with something `ClusterMessage` cannot
    /// serialize (an unknown future variant).
    fn signed_raw_payload(sender: &str, incarnation: u64, seq: u64, payload: &str) -> Envelope {
        let mac = hmac_sha256_hex(
            SECRET,
            &signing_input(
                WIRE_VERSION,
                CLUSTER,
                sender,
                incarnation,
                seq,
                payload.as_bytes(),
            ),
        );
        Envelope {
            v: WIRE_VERSION,
            key_id: CURRENT_KEY_ID,
            cluster: CLUSTER.to_owned(),
            sender: sender.to_owned(),
            incarnation,
            seq,
            payload: payload.to_owned(),
            mac,
        }
    }

    /// The verifier holds the HMAC key as a plain `Vec<u8>`, so `Debug` is
    /// hand-written to omit it. Re-deriving `Debug` would print the whole key
    /// the first time somebody added `?verifier` to the reject-path log line in
    /// the receive loop, and clippy has no lint for it.
    #[test]
    fn verifier_debug_never_prints_the_secret() {
        let mut verifier = verifier();
        let frame = frame_for(SECRET, CLUSTER, REMOTE, 1, 1, &ClusterMessage::Leave);
        assert!(
            verifier.accept(&frame).is_ok(),
            "sanity: the frame verifies"
        );

        let rendered = format!("{verifier:?}");
        assert!(
            !rendered.contains("secret"),
            "the secret field must not appear in Debug output at all; got {rendered}"
        );
        assert!(
            !rendered.contains("99, 108, 117"),
            "…and certainly not as the byte array a derived Debug would print \
             (`clu` = 99, 108, 117); got {rendered}"
        );
        assert!(
            rendered.contains(CLUSTER) && rendered.contains(LOCAL),
            "the diagnosable fields must still be there, or redaction has cost \
             the struct its usefulness; got {rendered}"
        );
    }

    /// Step 3, first half: `v != 1` is refused with `reason="version"`, and
    /// refused *before* the MAC — an older node must not be made to compute an
    /// HMAC over an envelope it could never have understood.
    #[test]
    fn wrong_envelope_version_rejected() {
        let signed = envelope_for(SECRET, CLUSTER, REMOTE, 1, 1, &state_push());
        assert!(signed.is_some(), "signing must produce an envelope");
        let Some(mut envelope) = signed else { return };
        envelope.v = WIRE_VERSION.saturating_add(1);
        let frame = encode_frame(&envelope).unwrap_or_default();

        let mut verifier = verifier();
        assert_eq!(
            verifier.accept(&frame).err(),
            Some(RejectReason::Version),
            "an envelope naming another wire version must be dropped as \
             `version`, not accepted and not mislabelled"
        );
        assert_eq!(
            verifier.rejected_total(),
            1,
            "the rejection must be counted"
        );
        assert!(
            !RejectReason::Version.closes_connection(),
            "a version mismatch drops the frame and reads on"
        );
        assert_eq!(
            verifier.watermark(REMOTE),
            None,
            "a refused frame must never advance the replay watermark"
        );
    }

    /// Step 3, second half: `key_id != 0` is refused with `reason="key_id"`.
    ///
    /// The `key_id` is deliberately outside the signing input — it *selects* a
    /// key rather than being protected by one — so this frame's MAC still
    /// verifies. The header check is the only thing standing between it and
    /// acceptance, which is exactly why it needs a test of its own.
    #[test]
    fn unknown_key_id_rejected() {
        let signed = envelope_for(SECRET, CLUSTER, REMOTE, 1, 1, &ClusterMessage::Leave);
        assert!(signed.is_some(), "signing must produce an envelope");
        let Some(mut envelope) = signed else { return };
        envelope.key_id = CURRENT_KEY_ID.saturating_add(1);
        let frame = encode_frame(&envelope).unwrap_or_default();

        let mut verifier = verifier();
        assert_eq!(
            verifier.accept(&frame).err(),
            Some(RejectReason::KeyId),
            "an envelope naming a key this node does not have must be dropped \
             as `key_id` — and it is NOT re-signed here, proving the field is \
             outside the MAC"
        );
        assert_eq!(
            verifier.rejected_total(),
            1,
            "the rejection must be counted"
        );
        assert_eq!(
            verifier.watermark(REMOTE),
            None,
            "a refused frame must never advance the replay watermark"
        );
    }

    /// Step 2: `key_id` is a required field. The guide's receive path is
    /// normative that an envelope missing a field is dropped as `malformed`,
    /// and the envelope table lists `key_id` among the fields every frame
    /// carries — a `#[serde(default)]` here would quietly accept it instead.
    #[test]
    fn envelope_missing_key_id_is_malformed() {
        let signed = envelope_for(SECRET, CLUSTER, REMOTE, 1, 1, &ClusterMessage::Leave);
        assert!(signed.is_some(), "signing must produce an envelope");
        let Some(envelope) = signed else { return };

        let mut json = serde_json::to_value(&envelope).unwrap_or(serde_json::Value::Null);
        assert!(
            json.get("key_id").is_some(),
            "sanity: a signed envelope must carry key_id in the first place"
        );
        if let Some(object) = json.as_object_mut() {
            object.remove("key_id");
        }
        let body = serde_json::to_vec(&json).unwrap_or_default();

        let mut verifier = verifier();
        assert_eq!(
            verifier.accept(&frame_bytes(&body)).err(),
            Some(RejectReason::Malformed),
            "an envelope with key_id omitted must be dropped as `malformed`, \
             matching the normative receive path — the parser and the published \
             format must agree on what is well-formed"
        );
        assert_eq!(
            verifier.rejected_total(),
            1,
            "the rejection must be counted"
        );
    }

    /// Step 7: an authenticated payload this node cannot understand is a clean
    /// `payload` drop, not an envelope-level `malformed` one — that difference
    /// is the documented forward-compatibility property (a newer node may add a
    /// message type without desyncing an older peer).
    #[test]
    fn unknown_payload_variant_rejected_after_the_mac() {
        let mut verifier = verifier();
        let unknown = signed_raw_payload(REMOTE, 4, 9, r#"{"type":"future_variant"}"#);
        let frame = encode_frame(&unknown).unwrap_or_default();

        assert_eq!(
            verifier.accept(&frame).err(),
            Some(RejectReason::Payload),
            "an unknown message type inside a VALID envelope must be dropped as \
             `payload` — mapping it to `malformed` would blame the envelope for \
             a payload a newer peer is entitled to send"
        );
        assert_eq!(
            verifier.rejected_total(),
            1,
            "the rejection must be counted"
        );
        assert_eq!(
            verifier.watermark(REMOTE),
            None,
            "a payload this node dropped must not advance the watermark, or the \
             sender's next (understandable) frame at the same seq is replayed away"
        );

        // A malformed payload behind the same valid envelope lands in the same
        // series, and the loop reads on either way.
        let garbage = signed_raw_payload(REMOTE, 4, 10, "not json at all");
        assert_eq!(
            verifier
                .accept(&encode_frame(&garbage).unwrap_or_default())
                .err(),
            Some(RejectReason::Payload),
            "a malformed authenticated payload is a `payload` drop too"
        );

        let good = frame_for(SECRET, CLUSTER, REMOTE, 4, 11, &ClusterMessage::Leave);
        assert!(
            verifier.accept(&good).is_ok(),
            "…and the verifier must still accept the sender's next good frame"
        );
    }

    /// Pruning a member must forget it **whole**: the replay watermark goes
    /// with the tombstone, so a node that comes back at a LOWER incarnation (a
    /// clock that stepped backwards) is judged as a fresh sender instead of
    /// being replay-dropped forever.
    ///
    /// Without this the returning node is permanently partitioned: its frames
    /// never reach the merge, the peer's pruned document holds no record about
    /// it to refute, and nothing on either side can break the deadlock.
    #[test]
    fn forgetting_a_sender_accepts_a_lower_incarnation_again() {
        let mut verifier = verifier();
        let high = frame_for(SECRET, CLUSTER, REMOTE, 5_000, 7, &ClusterMessage::Leave);
        assert!(
            verifier.accept(&high).is_ok(),
            "sanity: the departing node's frame must be accepted first"
        );

        let rejoin = frame_for(SECRET, CLUSTER, REMOTE, 42, 0, &ClusterMessage::Leave);
        assert_eq!(
            verifier.accept(&rejoin).err(),
            Some(RejectReason::Replay),
            "sanity: while the watermark stands, a lower incarnation is a replay \
             — otherwise this test proves nothing"
        );

        verifier.forget(REMOTE);
        assert_eq!(
            verifier.watermark(REMOTE),
            None,
            "forgetting a sender must drop its watermark row"
        );
        assert!(
            verifier.accept(&rejoin).is_ok(),
            "after the sender is forgotten its lower incarnation must be accepted \
             as a fresh sender — this is the only way back for a node that \
             restarted behind a backward clock step"
        );
        assert_eq!(
            verifier.watermark(REMOTE),
            Some((42, 0)),
            "…and the fresh watermark must start from the frame just accepted"
        );
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
        assert_eq!(
            verifier.rejected_total(),
            1,
            "the rejection must be counted"
        );
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
        assert_eq!(
            verifier.rejected_total(),
            1,
            "the rejection must be counted"
        );
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

        // A LEGAL length prefix over a body that is not an envelope — the
        // "malformed body" case this test is about. The bytes must be framed:
        // fed raw, `not-` reads as a declared length of 1,852,797,997, which is
        // over the cap and therefore the connection-fatal `Oversize` verdict
        // that `frame_len_refuses_zero_and_oversize` pins, not a malformed body
        // at all (issue #1762 red-phase input bug).
        let mut junk = 18u32.to_be_bytes().to_vec();
        junk.extend_from_slice(b"not-a-frame-at-all");

        for bad in [b"".as_slice(), b"\x00".as_slice(), &truncated, &junk] {
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
