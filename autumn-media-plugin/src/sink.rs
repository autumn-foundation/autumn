//! [`MediaArtifactSink`] — the app-supplied completion callback the generic
//! media workflows invoke once a derived artifact is encoded and persisted.
//!
//! This is the media-workflow analogue of autumn-web's `OutboundWebhookHandler`
//! (the pluggable store an app implements): the built-in `#[job]` handlers in
//! [`crate::workflows`] run the `FFmpeg` encode, upload the output through
//! [`crate::storage::MediaStorage`], then hand a fully-described
//! [`MediaArtifact`] back to the app through this trait so the app can record
//! the public URL against its own schema — exactly the `record_*_completed`
//! activity Arroyo's Harvest workflows used to perform, but harvest-free.
//!
//! The trait is written with hand-rolled boxed futures (rather than
//! `#[async_trait]`) to mirror autumn-web's own webhook seam and avoid pulling a
//! new dependency into this crate.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::error::MediaError;

/// The boxed future the sink callbacks return.
pub type MediaSinkFuture<'a> = Pin<Box<dyn Future<Output = Result<(), MediaError>> + Send + 'a>>;

/// The class of derived media artifact a workflow produced.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaArtifactKind {
    /// A single poster/thumbnail frame extracted from a recording.
    Thumbnail,
    /// A seek-preview sprite mosaic (plus its companion `WebVTT` track carried
    /// as the artifact's secondary file).
    Preview,
    /// A transcoded window of a recording (a clip/highlight MP4).
    Transcode,
}

impl MediaArtifactKind {
    /// Stable lowercase label for logs and app-side matching.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Thumbnail => "thumbnail",
            Self::Preview => "preview",
            Self::Transcode => "transcode",
        }
    }
}

/// One persisted file belonging to a [`MediaArtifact`].
///
/// `key` is the caller-relative storage key, `storage_uri` is the backend URI
/// (`s3://…` or a local path) exactly as [`crate::storage::StoredObject`]
/// records it, and `public_url` is the browser-facing URL.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaArtifactFile {
    /// Caller-relative object key the file was persisted under.
    pub key: String,
    /// Backend URI (`s3://{bucket}/{key}` or a local path).
    pub storage_uri: String,
    /// Public URL a browser fetches the object at.
    pub public_url: String,
    /// MIME content type the file was stored with.
    pub content_type: String,
    /// Size of the encoded output in bytes.
    pub bytes: u64,
}

/// A fully-described artifact a media workflow produced and persisted.
///
/// The generalized, harvest-free form of the payload Arroyo's
/// `record_thumbnail_completed` / `record_preview_completed` /
/// `record_highlight_completed` activities consumed: it names the workflow that
/// produced it, the kind of artifact, the caller-supplied `source_id` the app
/// keys its own row on, the primary output file, an optional `secondary` file
/// (the seek-preview `WebVTT` track), and free-form `metadata`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaArtifact {
    /// The kind of artifact produced.
    pub kind: MediaArtifactKind,
    /// The workflow id that produced this artifact (caller-supplied in the job
    /// args), so the app can correlate the callback with the request.
    pub workflow_id: String,
    /// The caller-supplied identifier for the source the artifact derives from
    /// (e.g. a broadcast/stream id), threaded straight through from the job
    /// args so the app can key its own row on it.
    pub source_id: Option<String>,
    /// The primary persisted output (the sprite JPEG for a preview, the poster
    /// JPEG for a thumbnail, the MP4 for a transcode).
    pub primary: MediaArtifactFile,
    /// An optional companion file — the seek-preview `WebVTT` track for a
    /// preview artifact; `None` for thumbnail/transcode.
    pub secondary: Option<MediaArtifactFile>,
    /// Free-form workflow metadata (deterministically ordered for stable
    /// logging/serialization).
    pub metadata: BTreeMap<String, String>,
}

/// App-supplied completion callback for the generic media workflows.
///
/// Implement this on the application side (Arroyo/Nexus) to record a produced
/// artifact against your own schema. It parallels autumn-web's
/// `OutboundWebhookHandler`: the plugin never touches app tables — it hands the
/// finished artifact (or a failure) to this trait.
pub trait MediaArtifactSink: Send + Sync + 'static {
    /// Record a successfully encoded and persisted artifact.
    fn on_completed(&self, artifact: MediaArtifact) -> MediaSinkFuture<'_>;

    /// Record that a workflow failed before producing an artifact.
    ///
    /// `error` is the stringified [`MediaError`]. This is best-effort: the
    /// workflow already logs and returns the underlying error to the durable
    /// job engine for retry, so an implementation should not itself fail the
    /// job — return `Ok(())` unless it genuinely could not record the failure.
    fn on_failed(
        &self,
        kind: MediaArtifactKind,
        workflow_id: String,
        error: String,
    ) -> MediaSinkFuture<'_>;
}

/// `AppState` extension wrapping the app-supplied [`MediaArtifactSink`].
///
/// The workflow `#[job]` handlers resolve this from `AppState`; when it is
/// absent they persist the artifact but log that no sink recorded it.
#[derive(Clone)]
pub struct MediaArtifactSinkExt(pub Arc<dyn MediaArtifactSink>);

#[cfg(test)]
pub(crate) mod tests {
    use super::{
        MediaArtifact, MediaArtifactFile, MediaArtifactKind, MediaArtifactSink, MediaSinkFuture,
    };
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A fake sink that counts completed/failed callbacks, used across the
    /// workflow tests too.
    pub struct CountingSink {
        pub completed: AtomicUsize,
        pub failed: AtomicUsize,
        pub last_key: std::sync::Mutex<Option<String>>,
    }

    impl CountingSink {
        pub fn new() -> Self {
            Self {
                completed: AtomicUsize::new(0),
                failed: AtomicUsize::new(0),
                last_key: std::sync::Mutex::new(None),
            }
        }
    }

    impl MediaArtifactSink for CountingSink {
        fn on_completed(&self, artifact: MediaArtifact) -> MediaSinkFuture<'_> {
            self.completed.fetch_add(1, Ordering::SeqCst);
            *self.last_key.lock().expect("lock") = Some(artifact.primary.key);
            Box::pin(async { Ok(()) })
        }

        fn on_failed(
            &self,
            _kind: MediaArtifactKind,
            _workflow_id: String,
            _error: String,
        ) -> MediaSinkFuture<'_> {
            self.failed.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(()) })
        }
    }

    fn sample_artifact() -> MediaArtifact {
        MediaArtifact {
            kind: MediaArtifactKind::Thumbnail,
            workflow_id: "wf-1".to_owned(),
            source_id: Some("stream-7".to_owned()),
            primary: MediaArtifactFile {
                key: "thumbnails/7.jpg".to_owned(),
                storage_uri: "/media/thumbnails/7.jpg".to_owned(),
                public_url: "/media/thumbnails/7.jpg".to_owned(),
                content_type: "image/jpeg".to_owned(),
                bytes: 2048,
            },
            secondary: None,
            metadata: BTreeMap::new(),
        }
    }

    #[test]
    fn artifact_kind_labels_are_stable() {
        assert_eq!(MediaArtifactKind::Thumbnail.as_str(), "thumbnail");
        assert_eq!(MediaArtifactKind::Preview.as_str(), "preview");
        assert_eq!(MediaArtifactKind::Transcode.as_str(), "transcode");
    }

    #[test]
    fn artifact_round_trips_through_json() {
        let artifact = sample_artifact();
        let json = serde_json::to_string(&artifact).expect("serialize");
        let back: MediaArtifact = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(artifact, back);
    }

    #[tokio::test]
    async fn sink_on_completed_receives_the_artifact() {
        let sink = Arc::new(CountingSink::new());
        sink.on_completed(sample_artifact()).await.expect("ok");
        assert_eq!(sink.completed.load(Ordering::SeqCst), 1);
        assert_eq!(sink.failed.load(Ordering::SeqCst), 0);
        assert_eq!(
            sink.last_key.lock().expect("lock").as_deref(),
            Some("thumbnails/7.jpg")
        );
    }
}
