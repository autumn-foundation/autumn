//! `MediaMTX` transport: control-API client plus ingest/playback URL builders.
//!
//! This module is a faithful port of the `MediaMTX` transport surface from the
//! Arroyo reference application, generalized away from any domain type:
//!
//! - [`MediaMtxClient`] wraps one reusable [`reqwest::Client`] and exposes the
//!   `fetch_*` / [`kick_publisher`](MediaMtxClient::kick_publisher) calls
//!   against the `MediaMTX` v3 control API. Every reader deliberately returns an
//!   `Option`/enum sentinel — never a `Result` — so a caller (e.g. a
//!   ghost-stream reaper) can distinguish "`MediaMTX` is down" from "no
//!   publisher is connected".
//! - [`MediaUrls`] holds the resolved origin bases and builds every
//!   browser-facing / server-side URL (`RTMP` ingest, `HLS`/`WHEP` playback,
//!   `WebRTC`/`WHIP` publish, recorded-`VOD` playback).
//!
//! Unlike Arroyo — whose configured bases already embedded a `/live` path
//! segment — the plugin's [`MediaMtxConfig`] carries pure origins, so the
//! playback/publish builders insert the `live/` segment themselves.

use std::collections::HashMap;
use std::path::{Component, Path};
use std::time::Duration;

use chrono::{DateTime, NaiveDateTime, SecondsFormat, Utc};
use reqwest::header;
use serde::Serialize;

use crate::config::MediaMtxConfig;

/// Default per-request timeout for the control-API reader calls.
const DEFAULT_TIMEOUT_SECS: u64 = 3;
/// Timeout used for the best-effort publisher-kick calls.
const KICK_TIMEOUT_SECS: u64 = 5;
/// Page size for the paginated `MediaMTX` v3 paths-list fetch.
const PATHS_ITEMS_PER_PAGE: u32 = 1000;
/// Hard cap on paths-list pages walked per fetch.
///
/// Guards against a malformed or absent `pageCount` ever driving an unbounded
/// loop; at 1000 items per page this still covers a million live paths.
const MAX_PATHS_PAGES: u64 = 1000;

// ── Status enums ────────────────────────────────────────────────────────────

/// Live ingest state as reported by the `MediaMTX` status API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IngestStatus {
    /// `MediaMTX` is actively receiving a publish on this path.
    Receiving {
        /// Human-readable source label (e.g. `RTMP`, `WHIP`).
        source_kind: String,
    },
    /// The path is registered but no publisher is active.
    NotReceiving,
    /// The `MediaMTX` status API is unreachable.
    Unavailable,
}

/// Concurrent playback state for a live channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ViewerCount {
    /// The channel is not live, so viewer count is intentionally not shown.
    Offline,
    /// `MediaMTX` returned a concrete active-reader count for this path.
    Available {
        /// Number of active readers on the path.
        count: u64,
    },
    /// The `MediaMTX` status API is unreachable or returned an unexpected body.
    Unavailable,
}

/// Combined runtime status from a single `MediaMTX` path response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamStatus {
    /// Ingest (publisher) state for the path.
    pub ingest_status: IngestStatus,
    /// Concurrent viewer count for the path.
    pub viewer_count: ViewerCount,
}

/// Ingest quality data extracted from a single `MediaMTX` path response.
#[derive(Debug, Clone, Serialize)]
pub struct StreamQualityStats {
    /// Total bytes received from the publisher.
    pub bytes_received: u64,
    /// Total bytes sent to readers.
    pub bytes_sent: u64,
    /// When the path became ready, if reported.
    pub ready_time: Option<String>,
}

const fn unavailable_stream_status() -> StreamStatus {
    StreamStatus {
        ingest_status: IngestStatus::Unavailable,
        viewer_count: ViewerCount::Unavailable,
    }
}

const fn not_receiving_stream_status() -> StreamStatus {
    StreamStatus {
        ingest_status: IngestStatus::NotReceiving,
        viewer_count: ViewerCount::Available { count: 0 },
    }
}

/// Map a raw `MediaMTX` source `type` string to a human-readable label.
pub(crate) fn map_source_type(raw: &str) -> String {
    match raw {
        "rtmpConn" => "RTMP".to_owned(),
        "webRTCSession" => "WebRTC / Browser Studio".to_owned(),
        "whipSession" => "WHIP".to_owned(),
        "srtConn" => "SRT".to_owned(),
        other => other.to_owned(),
    }
}

// ── Pure parsers ────────────────────────────────────────────────────────────

/// Extract a concurrent viewer count from a `MediaMTX` path response.
#[must_use]
pub fn viewer_count_from_path_json(json: &serde_json::Value) -> Option<u64> {
    json.get("readers")
        .and_then(serde_json::Value::as_array)
        .map(|readers| readers.len() as u64)
}

/// Extract per-stream-key viewer counts from the `MediaMTX` paths list response.
#[must_use]
pub fn viewer_counts_from_paths_json(json: &serde_json::Value) -> Option<HashMap<String, u64>> {
    let items = json.get("items")?.as_array()?;
    let mut counts = HashMap::new();
    for item in items {
        let Some(path) = item.get("name").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let Some(stream_key) = path.strip_prefix("live/") else {
            continue;
        };
        let Some(count) = viewer_count_from_path_json(item) else {
            continue;
        };
        counts.insert(stream_key.to_owned(), count);
    }
    Some(counts)
}

/// Resolve the total page count from a `MediaMTX` paths-list response.
///
/// Clamped to at least 1: an absent or malformed `pageCount` is treated as a
/// single page so the paginated fetchers can never infinite-loop.
fn page_count_from_paths_json(json: &serde_json::Value) -> u64 {
    json.get("pageCount")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(1)
        .max(1)
}

/// Whether a `MediaMTX` paths-list response carries no `items` (a well-formed
/// empty page), used as a defensive pagination stop.
fn paths_page_is_empty(json: &serde_json::Value) -> bool {
    json.get("items")
        .and_then(serde_json::Value::as_array)
        .is_none_or(Vec::is_empty)
}

/// Extract per-stream-key ingest statuses from the `MediaMTX` paths list
/// response.
///
/// Returns `None` when the payload is not a well-formed paths list (treated
/// as "API unavailable" by callers). Non-`live/` paths are ignored. A stream
/// key that is absent from the returned map has no registered path at all,
/// which callers should treat as [`IngestStatus::NotReceiving`].
#[must_use]
pub fn ingest_statuses_from_paths_json(
    json: &serde_json::Value,
) -> Option<HashMap<String, IngestStatus>> {
    let items = json.get("items")?.as_array()?;
    let mut statuses = HashMap::new();
    for item in items {
        let Some(path) = item.get("name").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let Some(stream_key) = path.strip_prefix("live/") else {
            continue;
        };
        statuses.insert(stream_key.to_owned(), ingest_status_from_path_json(item));
    }
    Some(statuses)
}

/// Extract quality stats from a `MediaMTX` path response.
///
/// Returns `None` when the path has no active source (stream is not receiving).
#[must_use]
pub fn quality_stats_from_path_json(json: &serde_json::Value) -> Option<StreamQualityStats> {
    // Only surface stats when there is an active publisher.
    if json.get("source").is_none_or(serde_json::Value::is_null) {
        return None;
    }
    let bytes_received = json
        .get("bytesReceived")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let bytes_sent = json
        .get("bytesSent")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let ready_time = json
        .get("readyTime")
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned);
    Some(StreamQualityStats {
        bytes_received,
        bytes_sent,
        ready_time,
    })
}

/// Extract ingest and viewer-count state from one `MediaMTX` path response.
#[must_use]
pub fn stream_status_from_path_json(json: &serde_json::Value) -> StreamStatus {
    let viewer_count = viewer_count_from_path_json(json)
        .map_or(ViewerCount::Unavailable, |count| ViewerCount::Available {
            count,
        });
    StreamStatus {
        ingest_status: ingest_status_from_path_json(json),
        viewer_count,
    }
}

fn ingest_status_from_path_json(json: &serde_json::Value) -> IngestStatus {
    if json.get("source").is_some_and(|source| !source.is_null()) {
        let source_kind = json
            .get("source")
            .and_then(|source| source.get("type"))
            .and_then(serde_json::Value::as_str)
            .map_or_else(|| "Unknown".to_owned(), map_source_type);
        IngestStatus::Receiving { source_kind }
    } else {
        IngestStatus::NotReceiving
    }
}

// ── Control-API client ──────────────────────────────────────────────────────

/// Reusable `MediaMTX` v3 control-API client.
///
/// Holds one `reqwest::Client` (a 3-second read timeout) and the API base
/// origin (e.g. `http://127.0.0.1:9997`). Every reader returns an `Option`/enum
/// sentinel rather than a `Result`, so callers can distinguish a transport
/// failure (`None` / [`IngestStatus::Unavailable`]) from a reachable API with no
/// active publisher ([`IngestStatus::NotReceiving`]).
#[derive(Debug, Clone)]
pub struct MediaMtxClient {
    http: reqwest::Client,
    api_base: String,
}

impl MediaMtxClient {
    /// Build a client from the resolved [`MediaMtxConfig`].
    ///
    /// Constructs one reusable `reqwest::Client` with a 3-second timeout. If the
    /// client fails to build (practically never) it falls back to
    /// [`reqwest::Client::default`], keeping construction infallible.
    #[must_use]
    pub fn new(config: &MediaMtxConfig) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(DEFAULT_TIMEOUT_SECS))
            .build()
            .unwrap_or_default();
        Self {
            http,
            api_base: config.api_base.clone(),
        }
    }

    /// The configured control-API base origin.
    #[must_use]
    pub fn api_base(&self) -> &str {
        &self.api_base
    }

    /// Fetch one page of the `MediaMTX` v3 paths list.
    ///
    /// Returns `None` on any transport, non-success, or parse failure so the
    /// paginated fetchers preserve their "`MediaMTX` is down" sentinel.
    async fn fetch_paths_page(&self, page: u64) -> Option<serde_json::Value> {
        let url = format!(
            "{}/v3/paths/list?itemsPerPage={PATHS_ITEMS_PER_PAGE}&page={page}",
            self.api_base
        );
        let response = self.http.get(&url).send().await.ok()?;
        if !response.status().is_success() {
            return None;
        }
        response.json::<serde_json::Value>().await.ok()
    }

    /// Read ingest statuses for every path from the `MediaMTX` v3 paths API.
    ///
    /// Walks every page of the paginated list so paths beyond the first
    /// `itemsPerPage` are visible. Returns `None` when the API is unreachable or
    /// returns an unexpected body so callers (a stream reaper) can distinguish
    /// "`MediaMTX` is down" from "no publisher" — a transport failure must never
    /// be treated as feed loss.
    pub async fn fetch_ingest_statuses(&self) -> Option<HashMap<String, IngestStatus>> {
        let mut merged = HashMap::new();
        for page in 0..MAX_PATHS_PAGES {
            let json = self.fetch_paths_page(page).await?;
            let statuses = ingest_statuses_from_paths_json(&json)?;
            merged.extend(statuses);
            if paths_page_is_empty(&json) || page + 1 >= page_count_from_paths_json(&json) {
                break;
            }
        }
        Some(merged)
    }

    /// Read all active path viewer counts from the `MediaMTX` v3 paths API.
    ///
    /// Walks every page of the paginated list so paths beyond the first
    /// `itemsPerPage` are counted.
    pub async fn fetch_viewer_counts(&self) -> Option<HashMap<String, u64>> {
        let mut merged = HashMap::new();
        for page in 0..MAX_PATHS_PAGES {
            let json = self.fetch_paths_page(page).await?;
            let counts = viewer_counts_from_paths_json(&json)?;
            merged.extend(counts);
            if paths_page_is_empty(&json) || page + 1 >= page_count_from_paths_json(&json) {
                break;
            }
        }
        Some(merged)
    }

    /// Read ingest and viewer state for one channel from the `MediaMTX` v3 paths
    /// API.
    ///
    /// This shares one request and timeout budget for callers that need both
    /// dashboard states. A `404` means the path is unregistered (reachable API,
    /// no publisher) → [`not_receiving_stream_status`]; any other non-2xx or a
    /// transport/parse failure → [`unavailable_stream_status`].
    pub async fn fetch_stream_status(&self, stream_key: &str) -> StreamStatus {
        let url = format!("{}/v3/paths/get/live%2F{stream_key}", self.api_base);
        let Ok(response) = self.http.get(&url).send().await else {
            return unavailable_stream_status();
        };

        if response.status().as_u16() == 404 {
            return not_receiving_stream_status();
        }
        if !response.status().is_success() {
            return unavailable_stream_status();
        }

        let Ok(json) = response.json::<serde_json::Value>().await else {
            return unavailable_stream_status();
        };
        stream_status_from_path_json(&json)
    }

    /// Read a single channel viewer count from the `MediaMTX` v3 paths API.
    ///
    /// A missing path means the API is reachable and there are no active
    /// readers. Transport failures and unexpected payloads become
    /// [`ViewerCount::Unavailable`].
    pub async fn fetch_viewer_count(&self, stream_key: &str) -> ViewerCount {
        self.fetch_stream_status(stream_key).await.viewer_count
    }

    /// Read ingest state for `stream_key` from the `MediaMTX` v3 paths API.
    ///
    /// Returns [`IngestStatus::Unavailable`] on any network or parse error so
    /// callers never need to distinguish transport failures from reception
    /// failures.
    pub async fn fetch_ingest_status(&self, stream_key: &str) -> IngestStatus {
        self.fetch_stream_status(stream_key).await.ingest_status
    }

    /// Fetch ingest quality stats for one channel from the `MediaMTX` v3 paths
    /// API.
    ///
    /// Returns `None` when the path is not active or the API is unreachable.
    pub async fn fetch_stream_quality(&self, stream_key: &str) -> Option<StreamQualityStats> {
        let url = format!("{}/v3/paths/get/live%2F{stream_key}", self.api_base);
        let response = self.http.get(&url).send().await.ok()?;
        if !response.status().is_success() {
            return None;
        }
        let json = response.json::<serde_json::Value>().await.ok()?;
        quality_stats_from_path_json(&json)
    }

    /// Best-effort kick of any publisher currently streaming on `old_key`'s
    /// path.
    ///
    /// Looks up the active source from the paths API, then POSTs a kick to the
    /// appropriate connection endpoint (`rtmpconns`, `webrtcsessions`, or
    /// `whipsessions`). Errors and an unreachable API are silently ignored — the
    /// key is already invalidated elsewhere, so the worst outcome is the
    /// publisher continues briefly before the server drops the idle path.
    pub async fn kick_publisher(&self, old_key: &str) {
        let path_url = format!("{}/v3/paths/get/live%2F{old_key}", self.api_base);
        let Ok(resp) = self
            .http
            .get(&path_url)
            .timeout(Duration::from_secs(KICK_TIMEOUT_SECS))
            .send()
            .await
        else {
            return;
        };
        if !resp.status().is_success() {
            return;
        }
        let Ok(json) = resp.json::<serde_json::Value>().await else {
            return;
        };
        let source = json.get("source");
        let source_type = source
            .and_then(|s| s.get("type"))
            .and_then(serde_json::Value::as_str);
        let source_id = source
            .and_then(|s| s.get("id"))
            .and_then(serde_json::Value::as_str);
        if let (Some(stype), Some(sid)) = (source_type, source_id) {
            let segment = match stype {
                "rtmpConn" => "rtmpconns",
                "webRTCSession" => "webrtcsessions",
                "whipSession" => "whipsessions",
                _ => return,
            };
            let kick_url = format!("{}/v3/{segment}/kick/{sid}", self.api_base);
            let _ = self
                .http
                .post(&kick_url)
                .timeout(Duration::from_secs(KICK_TIMEOUT_SECS))
                .send()
                .await;
        }
    }
}

// ── URL builders ────────────────────────────────────────────────────────────

/// Resolved `MediaMTX` origin bases plus every URL the transport builds.
///
/// The bases are **pure origins** (no `/live` segment), so the playback/publish
/// builders insert the `live/` path segment themselves. The `RTMP` base already
/// ends in `/live` by convention (matching `MediaMTX`), so `RTMP` ingest is a
/// bare join.
#[derive(Debug, Clone)]
#[allow(clippy::struct_field_names)] // every field is genuinely an origin base URL
pub struct MediaUrls {
    rtmp_base: String,
    hls_base: String,
    hls_probe_base: String,
    webrtc_base: String,
    playback_base: String,
    playback_probe_base: String,
    api_base: String,
}

impl MediaUrls {
    /// Resolve the URL bases from a [`MediaMtxConfig`], filling the probe bases
    /// from the config's [`hls_probe_base`](MediaMtxConfig::hls_probe_base) /
    /// [`playback_probe_base`](MediaMtxConfig::playback_probe_base) accessors.
    ///
    /// Trailing slashes are stripped from every base so joins are uniform.
    #[must_use]
    pub fn from_config(config: &MediaMtxConfig) -> Self {
        let trim = |value: &str| value.trim_end_matches('/').to_owned();
        Self {
            rtmp_base: trim(&config.rtmp_base),
            hls_base: trim(&config.hls_base),
            hls_probe_base: trim(config.hls_probe_base()),
            webrtc_base: trim(&config.webrtc_base),
            playback_base: trim(&config.playback_base),
            playback_probe_base: trim(config.playback_probe_base()),
            api_base: trim(&config.api_base),
        }
    }

    /// The configured control-API base origin.
    #[must_use]
    pub fn api_base(&self) -> &str {
        &self.api_base
    }

    /// `RTMP` ingest URL. The `RTMP` base already ends in `/live`, so this is a
    /// bare join.
    #[must_use]
    pub fn rtmp_ingest_url(&self, stream_key: &str) -> String {
        format!("{}/{stream_key}", self.rtmp_base)
    }

    /// Browser-facing `HLS` playback manifest URL.
    #[must_use]
    pub fn hls_playback_url(&self, stream_key: &str) -> String {
        format!("{}/live/{stream_key}/index.m3u8", self.hls_base)
    }

    /// Same as [`hls_playback_url`](Self::hls_playback_url) but resolves through
    /// the server-side probe base, so a capture loop can reach `MediaMTX` from
    /// inside the app container even when the browser-facing URL points at a
    /// host network address.
    #[must_use]
    pub fn internal_hls_playback_url(&self, stream_key: &str) -> String {
        format!("{}/live/{stream_key}/index.m3u8", self.hls_probe_base)
    }

    /// Browser-facing `WebRTC`/`WHEP` playback URL.
    #[must_use]
    pub fn webrtc_playback_url(&self, stream_key: &str) -> String {
        format!("{}/live/{stream_key}/whep", self.webrtc_base)
    }

    /// Browser-facing `WebRTC` publish URL.
    #[must_use]
    pub fn webrtc_publish_url(&self, stream_key: &str) -> String {
        format!("{}/live/{stream_key}/publish", self.webrtc_base)
    }

    /// Browser-facing `WHIP` publish URL.
    #[must_use]
    pub fn whip_publish_url(&self, stream_key: &str) -> String {
        format!("{}/live/{stream_key}/whip", self.webrtc_base)
    }

    /// `WHIP` publish URL for an arbitrary `MediaMTX` `path`.
    ///
    /// Unlike [`whip_publish_url`](Self::whip_publish_url) — the broadcast
    /// helper that inserts the `live/` segment — this takes the full
    /// `MediaMTX` path verbatim, so the rooms surface (whose participant paths
    /// are `room/…`, not `live/…`) composes publish URLs through it.
    #[must_use]
    pub fn whip_publish_url_for_path(&self, path: &str) -> String {
        format!("{}/{path}/whip", self.webrtc_base)
    }

    /// `WHEP` read (subscribe) URL for an arbitrary `MediaMTX` `path`.
    ///
    /// Mirrors [`whip_publish_url_for_path`](Self::whip_publish_url_for_path)
    /// for the subscribe side: the rooms surface builds each peer's subscribe
    /// URL with it (the broadcast [`webrtc_playback_url`](Self::webrtc_playback_url)
    /// only serves `live/…` paths).
    #[must_use]
    pub fn whep_read_url(&self, path: &str) -> String {
        format!("{}/{path}/whep", self.webrtc_base)
    }

    /// Browser-facing recorded-`VOD` playback URL for a finished recording.
    ///
    /// Returns `None` when the recording path does not resolve to a
    /// `live/<key>` path under `recordings_root`, or when the duration is
    /// non-positive. `started_at` / `ended_at` are the recording's UTC bounds.
    #[must_use]
    pub fn recording_playback_url(
        &self,
        recording_path: &Path,
        recordings_root: &Path,
        started_at: NaiveDateTime,
        ended_at: NaiveDateTime,
    ) -> Option<String> {
        recording_url(
            recording_path,
            recordings_root,
            started_at,
            ended_at,
            &self.playback_base,
        )
    }

    /// Server-side recorded-`VOD` probe URL, resolved through the playback probe
    /// base (see [`recording_playback_url`](Self::recording_playback_url)).
    #[must_use]
    pub fn recording_probe_url(
        &self,
        recording_path: &Path,
        recordings_root: &Path,
        started_at: NaiveDateTime,
        ended_at: NaiveDateTime,
    ) -> Option<String> {
        recording_url(
            recording_path,
            recordings_root,
            started_at,
            ended_at,
            &self.playback_probe_base,
        )
    }
}

// ── Recorded-VOD free functions ─────────────────────────────────────────────

fn recording_url(
    recording_path: &Path,
    recordings_root: &Path,
    started_at: NaiveDateTime,
    ended_at: NaiveDateTime,
    playback_base: &str,
) -> Option<String> {
    let mediamtx_path = recording_mediamtx_path(recording_path, recordings_root)?;
    let duration_millis = (ended_at - started_at).num_milliseconds();
    if duration_millis <= 0 {
        return None;
    }
    let start = DateTime::<Utc>::from_naive_utc_and_offset(started_at, Utc)
        .to_rfc3339_opts(SecondsFormat::AutoSi, true);
    let duration = duration_seconds_param(duration_millis);
    let mut url =
        reqwest::Url::parse(&format!("{}/get", playback_base.trim_end_matches('/'))).ok()?;
    url.query_pairs_mut()
        .append_pair("path", &mediamtx_path)
        .append_pair("start", &start)
        .append_pair("duration", &duration)
        .append_pair("format", "fmp4");
    Some(url.to_string())
}

/// Resolve a recording filesystem path to its `MediaMTX` `live/<key>` path.
///
/// The recording must sit exactly at `recordings_root/live/<key>` (two path
/// components after the root); anything else returns `None`.
#[must_use]
pub fn recording_mediamtx_path(recording_path: &Path, recordings_root: &Path) -> Option<String> {
    let relative = recording_path.strip_prefix(recordings_root).ok()?;
    let mut components = relative.components();
    let Component::Normal(first) = components.next()? else {
        return None;
    };
    if first != "live" {
        return None;
    }
    let Component::Normal(stream_key) = components.next()? else {
        return None;
    };
    if components.next().is_some() {
        return None;
    }
    let stream_key = stream_key.to_str()?.trim();
    if stream_key.is_empty() {
        return None;
    }
    Some(format!("live/{stream_key}"))
}

/// Render a duration (in milliseconds) as the numeric-seconds `duration` query
/// value `MediaMTX` expects — e.g. `200_500 -> "200.5"`, `3_600_000 -> "3600"`.
#[must_use]
pub fn duration_seconds_param(duration_millis: i64) -> String {
    let whole_seconds = duration_millis / 1_000;
    let millis = duration_millis % 1_000;
    if millis == 0 {
        whole_seconds.to_string()
    } else {
        format!("{whole_seconds}.{millis:03}")
            .trim_end_matches('0')
            .to_owned()
    }
}

/// Probe a recorded-`VOD` playback URL for availability.
///
/// Issues a ranged `GET` (`Range: bytes=0-0`) so it never downloads a whole
/// VOD, and accepts `200 OK` / `206 Partial Content` as "available". Any
/// transport error or other status is `false`.
pub async fn recording_available(http: &reqwest::Client, url: &str) -> bool {
    let Ok(response) = http
        .get(url)
        .header(header::RANGE, "bytes=0-0")
        .send()
        .await
    else {
        return false;
    };
    matches!(
        response.status(),
        reqwest::StatusCode::OK | reqwest::StatusCode::PARTIAL_CONTENT
    )
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::oneshot;

    use super::*;

    fn test_config() -> MediaMtxConfig {
        MediaMtxConfig {
            hls_probe_base: Some("http://mediamtx:8888".to_owned()),
            playback_probe_base: Some("http://mediamtx:9996".to_owned()),
            ..MediaMtxConfig::default()
        }
    }

    fn started_at() -> NaiveDateTime {
        chrono::NaiveDate::from_ymd_opt(2026, 5, 14)
            .unwrap()
            .and_hms_opt(10, 0, 0)
            .unwrap()
    }

    // ── Parsers ─────────────────────────────────────────────────────────────

    #[test]
    fn ingest_statuses_parsed_from_mediamtx_paths_list() {
        let json = serde_json::json!({
            "itemCount": 3,
            "pageCount": 1,
            "items": [
                {
                    "name": "live/key_receiving",
                    "source": { "type": "rtmpConn", "id": "abc" },
                    "readers": []
                },
                {
                    "name": "live/key_absent",
                    "source": null,
                    "readers": []
                },
                {
                    "name": "not-arroyo/other",
                    "source": { "type": "rtmpConn", "id": "def" },
                    "readers": []
                }
            ]
        });
        let statuses = ingest_statuses_from_paths_json(&json).expect("well-formed list must parse");
        assert!(matches!(
            statuses.get("key_receiving"),
            Some(IngestStatus::Receiving { .. })
        ));
        assert!(matches!(
            statuses.get("key_absent"),
            Some(IngestStatus::NotReceiving)
        ));
        assert!(
            !statuses.contains_key("other"),
            "non-live/ paths must be ignored"
        );

        // A malformed body (no items) must be treated as unavailable.
        assert!(ingest_statuses_from_paths_json(&serde_json::json!({"nope": true})).is_none());
    }

    #[test]
    fn viewer_counts_read_reader_arrays_and_strip_live_prefix() {
        let json = serde_json::json!({
            "items": [
                { "name": "live/room_a", "readers": [ {}, {}, {} ] },
                { "name": "live/room_b", "readers": [] },
                { "name": "other/room_c", "readers": [ {} ] }
            ]
        });
        let counts = viewer_counts_from_paths_json(&json).expect("well-formed list must parse");
        assert_eq!(counts.get("room_a"), Some(&3));
        assert_eq!(counts.get("room_b"), Some(&0));
        assert!(
            !counts.contains_key("room_c"),
            "non-live/ paths must be skipped"
        );

        assert!(viewer_counts_from_paths_json(&serde_json::json!({"nope": true})).is_none());
    }

    #[test]
    fn viewer_counts_skip_malformed_items_and_keep_valid_ones() {
        let json = serde_json::json!({
            "items": [
                { "name": "live/foo", "readers": [ {}, {} ] },
                { "readers": [ {} ] },
                { "name": "live/bar", "readers": [ {}, {}, {} ] },
                { "name": "other/baz", "readers": [ {} ] }
            ]
        });
        let counts = viewer_counts_from_paths_json(&json)
            .expect("a single malformed item must not blank the whole snapshot");
        assert_eq!(counts.get("foo"), Some(&2));
        assert_eq!(counts.get("bar"), Some(&3));
        assert!(
            !counts.contains_key("baz"),
            "non-live/ paths must be skipped"
        );
        assert_eq!(counts.len(), 2, "only the two valid live/ items are kept");
    }

    #[test]
    fn viewer_count_from_single_path() {
        assert_eq!(
            viewer_count_from_path_json(&serde_json::json!({ "readers": [ {}, {} ] })),
            Some(2)
        );
        assert_eq!(
            viewer_count_from_path_json(&serde_json::json!({ "readers": [] })),
            Some(0)
        );
        assert_eq!(
            viewer_count_from_path_json(&serde_json::json!({ "nope": true })),
            None
        );
    }

    #[test]
    fn quality_stats_require_active_source() {
        let active = serde_json::json!({
            "source": { "type": "rtmpConn" },
            "bytesReceived": 1234,
            "bytesSent": 42,
            "readyTime": "2026-05-14T10:00:00Z"
        });
        let stats = quality_stats_from_path_json(&active).expect("active source yields stats");
        assert_eq!(stats.bytes_received, 1234);
        assert_eq!(stats.bytes_sent, 42);
        assert_eq!(stats.ready_time.as_deref(), Some("2026-05-14T10:00:00Z"));

        // Missing byte counters default to zero.
        let sparse = serde_json::json!({ "source": { "type": "rtmpConn" } });
        let stats = quality_stats_from_path_json(&sparse).expect("active source yields stats");
        assert_eq!(stats.bytes_received, 0);
        assert_eq!(stats.bytes_sent, 0);
        assert_eq!(stats.ready_time, None);

        // Null / absent source → not receiving → None.
        assert!(quality_stats_from_path_json(&serde_json::json!({ "source": null })).is_none());
        assert!(quality_stats_from_path_json(&serde_json::json!({ "nope": true })).is_none());
    }

    #[test]
    fn stream_status_maps_source_and_readers() {
        let json = serde_json::json!({
            "source": { "type": "whipSession" },
            "readers": [ {}, {} ]
        });
        let status = stream_status_from_path_json(&json);
        assert_eq!(
            status.ingest_status,
            IngestStatus::Receiving {
                source_kind: "WHIP".to_owned()
            }
        );
        assert_eq!(status.viewer_count, ViewerCount::Available { count: 2 });

        // No readers array → viewer count unavailable, still not receiving.
        let json = serde_json::json!({ "source": null });
        let status = stream_status_from_path_json(&json);
        assert_eq!(status.ingest_status, IngestStatus::NotReceiving);
        assert_eq!(status.viewer_count, ViewerCount::Unavailable);
    }

    #[test]
    fn map_source_type_table() {
        assert_eq!(map_source_type("rtmpConn"), "RTMP");
        assert_eq!(map_source_type("webRTCSession"), "WebRTC / Browser Studio");
        assert_eq!(map_source_type("whipSession"), "WHIP");
        assert_eq!(map_source_type("srtConn"), "SRT");
        assert_eq!(map_source_type("mysteryConn"), "mysteryConn");
    }

    // ── URL builders (O1: /live inserted for pure-origin bases) ──────────────

    #[test]
    fn url_builders_insert_live_segment() {
        let urls = MediaUrls::from_config(&test_config());
        // rtmp_base already ends in /live → bare join, no extra segment.
        assert_eq!(urls.rtmp_ingest_url("sk"), "rtmp://127.0.0.1:1935/live/sk");
        assert_eq!(
            urls.hls_playback_url("sk"),
            "http://127.0.0.1:8888/live/sk/index.m3u8"
        );
        assert_eq!(
            urls.internal_hls_playback_url("sk"),
            "http://mediamtx:8888/live/sk/index.m3u8"
        );
        assert_eq!(
            urls.webrtc_playback_url("sk"),
            "http://127.0.0.1:8889/live/sk/whep"
        );
        assert_eq!(
            urls.webrtc_publish_url("sk"),
            "http://127.0.0.1:8889/live/sk/publish"
        );
        assert_eq!(
            urls.whip_publish_url("sk"),
            "http://127.0.0.1:8889/live/sk/whip"
        );
        assert_eq!(urls.api_base(), "http://127.0.0.1:9997");
    }

    #[test]
    fn raw_path_whip_whep_urls_take_the_path_verbatim() {
        let urls = MediaUrls::from_config(&MediaMtxConfig::default());
        // No `live/` insertion — the room path is used exactly as given.
        assert_eq!(
            urls.whip_publish_url_for_path("room/tenant-a/room1/part1"),
            "http://127.0.0.1:8889/room/tenant-a/room1/part1/whip"
        );
        assert_eq!(
            urls.whep_read_url("room/tenant-a/room1/part1"),
            "http://127.0.0.1:8889/room/tenant-a/room1/part1/whep"
        );
    }

    #[test]
    fn internal_hls_falls_back_to_hls_base_without_probe_override() {
        let urls = MediaUrls::from_config(&MediaMtxConfig::default());
        assert_eq!(
            urls.internal_hls_playback_url("sk"),
            "http://127.0.0.1:8888/live/sk/index.m3u8"
        );
    }

    #[test]
    fn url_builders_trim_trailing_base_slashes() {
        let config = MediaMtxConfig {
            hls_base: "http://127.0.0.1:8888/".to_owned(),
            ..MediaMtxConfig::default()
        };
        let urls = MediaUrls::from_config(&config);
        assert_eq!(
            urls.hls_playback_url("sk"),
            "http://127.0.0.1:8888/live/sk/index.m3u8"
        );
    }

    // ── Recorded-VOD URL / path / duration ───────────────────────────────────

    #[test]
    fn recording_playback_url_exact_string() {
        let config = MediaMtxConfig {
            playback_base: "http://public-playback.example".to_owned(),
            ..MediaMtxConfig::default()
        };
        let urls = MediaUrls::from_config(&config);
        let start = started_at();
        let end = start + chrono::Duration::hours(1);
        assert_eq!(
            urls.recording_playback_url(
                &PathBuf::from("recordings/live/sk_test"),
                &PathBuf::from("recordings"),
                start,
                end,
            )
            .as_deref(),
            Some(
                "http://public-playback.example/get?path=live%2Fsk_test&start=2026-05-14T10%3A00%3A00Z&duration=3600&format=fmp4"
            )
        );
    }

    #[test]
    fn recording_playback_url_preserves_subsecond_start_anchor() {
        let config = MediaMtxConfig {
            playback_base: "http://public-playback.example".to_owned(),
            ..MediaMtxConfig::default()
        };
        let urls = MediaUrls::from_config(&config);
        // A start anchor 250ms into the second must not be truncated away.
        let start = chrono::NaiveDate::from_ymd_opt(2026, 5, 14)
            .unwrap()
            .and_hms_milli_opt(10, 0, 0, 250)
            .unwrap();
        let end = start + chrono::Duration::hours(1);
        let url = urls
            .recording_playback_url(
                &PathBuf::from("recordings/live/sk_test"),
                &PathBuf::from("recordings"),
                start,
                end,
            )
            .expect("playback url should build");
        // AutoSi renders the millisecond component (percent-encoded `.` stays raw).
        assert!(
            url.contains("start=2026-05-14T10%3A00%3A00.250Z"),
            "sub-second start must be preserved, got {url}"
        );

        // A whole-second start still renders cleanly with no fractional digits.
        let whole = started_at();
        let whole_url = urls
            .recording_playback_url(
                &PathBuf::from("recordings/live/sk_test"),
                &PathBuf::from("recordings"),
                whole,
                whole + chrono::Duration::hours(1),
            )
            .expect("playback url should build");
        assert!(
            whole_url.contains("start=2026-05-14T10%3A00%3A00Z"),
            "whole-second start must render without a decimal, got {whole_url}"
        );
    }

    #[test]
    fn recording_probe_url_uses_probe_base() {
        let config = MediaMtxConfig {
            playback_base: "http://public-playback.example".to_owned(),
            playback_probe_base: Some("http://mediamtx:9996".to_owned()),
            ..MediaMtxConfig::default()
        };
        let urls = MediaUrls::from_config(&config);
        let start = started_at();
        let end = start + chrono::Duration::hours(1);
        let probe = urls
            .recording_probe_url(
                &PathBuf::from("recordings/live/sk_test"),
                &PathBuf::from("recordings"),
                start,
                end,
            )
            .expect("probe url should build");
        assert!(probe.starts_with("http://mediamtx:9996/get?"));
        assert!(probe.contains("path=live%2Fsk_test"));
    }

    #[test]
    fn recording_url_none_for_non_positive_duration() {
        let urls = MediaUrls::from_config(&MediaMtxConfig::default());
        let start = started_at();
        assert!(
            urls.recording_playback_url(
                &PathBuf::from("recordings/live/sk_test"),
                &PathBuf::from("recordings"),
                start,
                start,
            )
            .is_none()
        );
    }

    #[test]
    fn duration_param_numeric_seconds_without_units() {
        assert_eq!(duration_seconds_param(200_500), "200.5");
        assert_eq!(duration_seconds_param(3_600_000), "3600");
    }

    #[test]
    fn recording_mediamtx_path_valid_and_invalid_shapes() {
        let root = PathBuf::from("recordings");
        assert_eq!(
            recording_mediamtx_path(&PathBuf::from("recordings/live/sk_test"), &root).as_deref(),
            Some("live/sk_test")
        );
        // Wrong first component.
        assert!(
            recording_mediamtx_path(&PathBuf::from("recordings/vods/sk_test"), &root).is_none()
        );
        // Too many components.
        assert!(
            recording_mediamtx_path(&PathBuf::from("recordings/live/sk/extra"), &root).is_none()
        );
        // Missing key.
        assert!(recording_mediamtx_path(&PathBuf::from("recordings/live"), &root).is_none());
        // Path not under the recordings root.
        assert!(recording_mediamtx_path(&PathBuf::from("elsewhere/live/sk"), &root).is_none());
    }

    // ── Recording availability probe (local TcpListener stub) ────────────────

    async fn spawn_playback_probe_server(
        status: &'static str,
    ) -> (String, oneshot::Receiver<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test playback server should bind");
        let addr = listener
            .local_addr()
            .expect("test playback server should have address");
        let (send_request, receive_request) = oneshot::channel();
        tokio::spawn(async move {
            let (mut socket, _) = listener
                .accept()
                .await
                .expect("test playback server should accept one request");
            let mut buffer = [0_u8; 2048];
            let read = socket
                .read(&mut buffer)
                .await
                .expect("test playback server should read request");
            let request = String::from_utf8_lossy(&buffer[..read]).into_owned();
            let _ = send_request.send(request);
            let response =
                format!("HTTP/1.1 {status}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
            socket
                .write_all(response.as_bytes())
                .await
                .expect("test playback server should write response");
        });
        (format!("http://{addr}"), receive_request)
    }

    fn probe_client() -> reqwest::Client {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(DEFAULT_TIMEOUT_SECS))
            .build()
            .expect("probe client should build")
    }

    #[tokio::test]
    async fn recording_available_accepts_partial_content_with_range_header() {
        let (base_url, request) = spawn_playback_probe_server("206 Partial Content").await;
        let url = format!("{base_url}/get?path=live%2Fsk_test");
        assert!(recording_available(&probe_client(), &url).await);

        let request = request.await.expect("probe server should capture request");
        assert!(request.starts_with("GET /get?"));
        assert!(
            request.to_ascii_lowercase().contains("range: bytes=0-0"),
            "probe must avoid downloading whole VODs"
        );
    }

    #[tokio::test]
    async fn recording_available_rejects_missing_playback_media() {
        let (base_url, request) = spawn_playback_probe_server("404 Not Found").await;
        let url = format!("{base_url}/get?path=live%2Fsk_test");
        assert!(!recording_available(&probe_client(), &url).await);
        request.await.expect("probe server should capture request");
    }

    #[tokio::test]
    async fn client_constructs_and_reports_api_base() {
        let client = MediaMtxClient::new(&MediaMtxConfig::default());
        assert_eq!(client.api_base(), "http://127.0.0.1:9997");
    }

    // ── Paths-list pagination (local TcpListener stub) ───────────────────────

    /// Spawn a stub that serves the supplied paths-list pages, dispatching each
    /// request by its `page=` query parameter and serving a well-formed empty
    /// page for any out-of-range page.
    async fn spawn_paths_list_server(pages: Vec<serde_json::Value>) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test paths server should bind");
        let addr = listener
            .local_addr()
            .expect("test paths server should have address");
        let page_total = pages.len();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut buffer = [0_u8; 2048];
                let read = socket.read(&mut buffer).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&buffer[..read]).into_owned();
                let page = parse_page_param(&request);
                let body = pages.get(page).cloned().unwrap_or_else(|| {
                    serde_json::json!({
                        "itemCount": 0,
                        "pageCount": page_total,
                        "items": []
                    })
                });
                let body = body.to_string();
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
            }
        });
        format!("http://{addr}")
    }

    /// Extract the `page=` value from a raw HTTP request line (defaults to 0).
    fn parse_page_param(request: &str) -> usize {
        request
            .split("page=")
            .nth(1)
            .and_then(|rest| rest.split(|c: char| !c.is_ascii_digit()).next())
            .and_then(|digits| digits.parse().ok())
            .unwrap_or(0)
    }

    fn client_for(base: &str) -> MediaMtxClient {
        MediaMtxClient::new(&MediaMtxConfig {
            api_base: base.to_owned(),
            ..MediaMtxConfig::default()
        })
    }

    #[tokio::test]
    async fn fetch_viewer_counts_walks_all_pages() {
        let page0 = serde_json::json!({
            "itemCount": 3,
            "pageCount": 2,
            "items": [
                { "name": "live/room_a", "readers": [ {}, {} ] },
                { "name": "live/room_b", "readers": [ {} ] }
            ]
        });
        let page1 = serde_json::json!({
            "itemCount": 3,
            "pageCount": 2,
            "items": [
                { "name": "live/room_c", "readers": [ {}, {}, {} ] }
            ]
        });
        let base = spawn_paths_list_server(vec![page0, page1]).await;
        let counts = client_for(&base)
            .fetch_viewer_counts()
            .await
            .expect("reachable API must yield counts");
        assert_eq!(counts.get("room_a"), Some(&2));
        assert_eq!(counts.get("room_b"), Some(&1));
        assert_eq!(counts.get("room_c"), Some(&3), "second page must be walked");
        assert_eq!(counts.len(), 3);
    }

    #[tokio::test]
    async fn fetch_ingest_statuses_walks_all_pages() {
        let page0 = serde_json::json!({
            "itemCount": 2,
            "pageCount": 2,
            "items": [
                { "name": "live/first", "source": { "type": "rtmpConn", "id": "a" }, "readers": [] }
            ]
        });
        let page1 = serde_json::json!({
            "itemCount": 2,
            "pageCount": 2,
            "items": [
                { "name": "live/second", "source": null, "readers": [] }
            ]
        });
        let base = spawn_paths_list_server(vec![page0, page1]).await;
        let statuses = client_for(&base)
            .fetch_ingest_statuses()
            .await
            .expect("reachable API must yield statuses");
        assert!(matches!(
            statuses.get("first"),
            Some(IngestStatus::Receiving { .. })
        ));
        assert!(
            matches!(statuses.get("second"), Some(IngestStatus::NotReceiving)),
            "second page must be walked"
        );
        assert_eq!(statuses.len(), 2);
    }

    #[tokio::test]
    async fn fetch_ingest_statuses_single_page() {
        let page0 = serde_json::json!({
            "itemCount": 1,
            "pageCount": 1,
            "items": [
                { "name": "live/only", "source": { "type": "whipSession", "id": "z" }, "readers": [] }
            ]
        });
        let base = spawn_paths_list_server(vec![page0]).await;
        let statuses = client_for(&base)
            .fetch_ingest_statuses()
            .await
            .expect("reachable API must yield statuses");
        assert!(matches!(
            statuses.get("only"),
            Some(IngestStatus::Receiving { .. })
        ));
        assert_eq!(statuses.len(), 1);
    }

    #[tokio::test]
    async fn fetch_viewer_counts_absent_page_count_is_single_page() {
        // No `pageCount` field: must be treated as one page (never infinite-loop).
        let page0 = serde_json::json!({
            "items": [ { "name": "live/solo", "readers": [ {} ] } ]
        });
        let base = spawn_paths_list_server(vec![page0]).await;
        let counts = client_for(&base)
            .fetch_viewer_counts()
            .await
            .expect("reachable API must yield counts");
        assert_eq!(counts.get("solo"), Some(&1));
        assert_eq!(counts.len(), 1);
    }

    #[tokio::test]
    async fn fetch_viewer_counts_unreachable_api_is_none() {
        // Bind then drop to obtain a port with nothing listening (connection
        // refused is immediate) — the transport sentinel must stay `None`.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("should bind");
        let addr = listener.local_addr().expect("should have address");
        drop(listener);
        let client = client_for(&format!("http://{addr}"));
        assert!(client.fetch_viewer_counts().await.is_none());
        assert!(client.fetch_ingest_statuses().await.is_none());
    }

    #[test]
    fn page_count_clamps_and_defaults() {
        assert_eq!(
            page_count_from_paths_json(&serde_json::json!({ "pageCount": 4 })),
            4
        );
        // Absent → single page.
        assert_eq!(page_count_from_paths_json(&serde_json::json!({})), 1);
        // Malformed (zero) → clamped to at least one page.
        assert_eq!(
            page_count_from_paths_json(&serde_json::json!({ "pageCount": 0 })),
            1
        );
    }
}
