//! `MediaMTX` host provisioning for autumn-media (issue #1974, Slice 7).
//!
//! [autumn-media](../../../../autumn-media-plugin/index.html) turns `MediaMTX` +
//! `FFmpeg` into the streaming substrate for an Autumn app: RTMP/WHIP ingest,
//! HLS/WebRTC/WHEP playback, recording, and a control API — served by a Go
//! binary that is a **host prerequisite, not a cargo dependency**. This module
//! provisions that binary on a bare host from `autumn deploy`, mirroring exactly
//! how [`super::proxy::KamalProxyController`] provisions kamal-proxy: a
//! non-Rust host daemon is a systemd unit written and enabled by ordered
//! [`DeployOp`]s over ssh; the binary itself (download / version pin) is left to
//! host bootstrap.
//!
//! ## What is real here
//!
//! - [`MediaMtxHostConfig`] deserializes the `[media.mediamtx]` deploy section
//!   (defaults everywhere, so a project that never runs the media controller is
//!   unaffected). The `MediaMTX` ports are real [`u16`] constants with defaults
//!   ([`MEDIAMTX_API_PORT`] etc.), NOT parsed out of URL strings.
//! - [`render_mediamtx_yml`] renders the ingest/playback/recording config
//!   (generalized from arroyo's `mediamtx.yml`), and [`render_mediamtx_unit`]
//!   renders the `/etc/systemd/system/<unit>.service` unit — both pure, so the
//!   exact rendered text is unit-testable.
//! - [`MediaMtxController::ensure_installed_ops`] emits the ordered
//!   [`DeployOp`]s (mkdir the config dir, write the yml, write the unit,
//!   `daemon-reload && enable --now && restart`) driven over the injectable
//!   [`DeployExecutor`], so the remote command
//!   sequence is assertable against a recording fake with no live host. It
//!   no-ops (empty op vector) when the section is not `enabled`.
//! - Five fail-closed doctor checks — [`mediamtx_ports_distinct`],
//!   [`ffmpeg_preflight`], [`mediamtx_binary_preflight`],
//!   [`recordings_dir_writable`], and [`mediamtx_ports_available`] — most run over
//!   the same executor and all return the shared [`PreflightCheck`] type, so a
//!   media host can be graded the same way `autumn deploy check` grades the app
//!   host. [`mediamtx_ports_distinct`] is a **pure config** check (issue #2051,
//!   Finding P): it rejects a config that binds two same-protocol listeners to one
//!   port before provisioning, so `systemctl restart` cannot fail after cutover on
//!   a config that a fresh-host `ss` scan would pass. The binary preflight runs
//!   before cutover so a host missing the (deferred-provisioned) `MediaMTX` binary
//!   fails fast instead of committing the app and then failing the post-cutover
//!   restart. An unverifiable result (transport error, unparseable output) reports
//!   a clear failure — never a silent pass.
//!
//! ## `FFmpeg` path verification (concrete literals only)
//!
//! `[media.ffmpeg] bin` may be a concrete path (the default `/usr/bin/ffmpeg` or
//! any explicit path), a `${VAR}` placeholder, or overridden at runtime by the
//! `AUTUMN_MEDIA__FFMPEG__BIN` environment variable — the plugin resolves the
//! latter two from the **service's own environment** before it ever shells out to
//! `FFmpeg`. The deploy side does not have and must not guess that service
//! environment: the generated service env-file (`build_env_file`)
//! propagates only `AUTUMN_ENV`, the signing secret, and the database URL — never
//! the operator's local shell. Resolving `[media.ffmpeg] bin` against the CLI's
//! local process env (as an earlier revision did) therefore probes the *wrong*
//! environment, so [`ffmpeg_preflight`] verifies **only a concrete literal** `bin`
//! — an environment-independent value it can probe correctly.
//!
//! **Deferred, not failed.** An env/interpolation-indirected path — an empty
//! value, or one carrying a `${...}` placeholder (including
//! `${AUTUMN_MEDIA__FFMPEG__BIN}`) — is **deferred to runtime** with a clear
//! non-passing, **non-blocking** outcome ("`FFmpeg` path uses env/interpolation
//! indirection resolved in the service environment; deferring verification to
//! runtime") rather than resolved against the CLI's local env and probed. It sets
//! [`PreflightCheck::deferred`], so [`PreflightCheck::blocking`] is `false` and
//! the deferred result is surfaced as a **warning that does not abort `deploy
//! up`** — the deployed service resolves and runs whatever *its* environment
//! names, so blocking the deploy on a path the deploy side cannot (and must not)
//! verify would be wrong, while reporting it as a plain pass would falsely claim
//! success. A **concrete literal** `FFmpeg` path that is missing / not executable
//! / not actually `FFmpeg` still fails closed (blocking) as before.
//!
//! ## `MediaMTX` = a SEPARATE host daemon
//!
//! Compose/Fly deployments can still run `MediaMTX` as a separate service/app
//! (arroyo does exactly this). This controller is for the `autumn deploy`
//! (bare-host) path: it stands `MediaMTX` up as a host systemd unit alongside the
//! reverse-proxied app. The two share nothing but the host.
//!
//! ## CSP requirement (the app owns its CSP profile)
//!
//! Apps embedding the players must allow the `MediaMTX` origins in their
//! `content_security_policy`: `connect-src`/`media-src` for the WebRTC (`:8889`),
//! HLS (`:8888`), and playback (`:9996`) origins, plus `frame-src` for WebRTC,
//! and the object-store origin in `media-src` for recorded playback. This module
//! documents the required origins ([`MediaMtxHostConfig::required_csp_origins`]);
//! the app owns the actual directive. See arroyo's `autumn.toml` /
//! `autumn-fly.toml` for the concrete `connect-src`/`media-src`/`frame-src`
//! strings (local origins collapse to `https://*.fly.dev` + the object store in
//! production).
//!
//! ## What is deferred
//!
//! - The `MediaMTX` binary provisioning (download / version pin) is a host
//!   bootstrap step, exactly as autumn does for kamal-proxy — this controller
//!   assumes the binary is at [`MediaMtxHostConfig::binary_path`].
//! - Wiring these executor-driven doctor checks into the offline `autumn doctor`
//!   CLI (which has no ssh executor) is left to a follow-up; the checks
//!   themselves are complete, fail-closed, and unit-tested via the recording
//!   fake, and are collected by [`collect_media_doctor_checks`] for a caller that
//!   already holds a live [`DeployExecutor`].

use serde::Deserialize;

use super::PreflightCheck;
use super::exec::{DeployExecutor, DeployOp, FileContents, RemoteCommand};

// ── MediaMTX ports (real u16 constants, NOT parsed from URLs) ────────────────

/// `MediaMTX` RTMP/WHIP ingest port.
pub const MEDIAMTX_RTMP_PORT: u16 = 1935;
/// `MediaMTX` HLS playback port.
pub const MEDIAMTX_HLS_PORT: u16 = 8888;
/// `MediaMTX` WebRTC/WHEP playback port.
pub const MEDIAMTX_WEBRTC_PORT: u16 = 8889;
/// `MediaMTX` recording-playback port.
pub const MEDIAMTX_PLAYBACK_PORT: u16 = 9996;
/// `MediaMTX` control-API port.
pub const MEDIAMTX_API_PORT: u16 = 9997;
/// `MediaMTX` WebRTC local UDP port (ICE host candidates).
pub const MEDIAMTX_WEBRTC_LOCAL_UDP_PORT: u16 = 8189;

// ── LL-HLS window (copied verbatim from arroyo's mediamtx.yml) ───────────────
//
// The DVR-on-live window = `HLS_SEGMENT_COUNT * HLS_SEGMENT_DURATION` = 60 * 2s
// = 120s. Kept as constants (not config) so the low-latency segmenting contract
// matches the players' assumptions; changing them means updating the app's
// advertised DVR window to match.

/// LL-HLS variant selector.
const HLS_VARIANT: &str = "lowLatency";
/// LL-HLS partial-segment duration.
const HLS_PART_DURATION: &str = "200ms";
/// LL-HLS full-segment duration.
const HLS_SEGMENT_DURATION: &str = "2s";
/// LL-HLS retained-segment count (× segment duration = the DVR window).
const HLS_SEGMENT_COUNT: u32 = 60;

// ── Config-section defaults (free fns so serde + `Default` share one source) ──

fn default_recordings_dir() -> String {
    "/recordings".to_owned()
}

fn default_record_delete_after() -> String {
    "72h".to_owned()
}

fn default_config_path() -> String {
    "/etc/mediamtx/mediamtx.yml".to_owned()
}

fn default_binary_path() -> String {
    "/usr/local/bin/mediamtx".to_owned()
}

fn default_unit_name() -> String {
    "mediamtx".to_owned()
}

const fn default_api_port() -> u16 {
    MEDIAMTX_API_PORT
}

const fn default_rtmp_port() -> u16 {
    MEDIAMTX_RTMP_PORT
}

const fn default_hls_port() -> u16 {
    MEDIAMTX_HLS_PORT
}

const fn default_webrtc_port() -> u16 {
    MEDIAMTX_WEBRTC_PORT
}

const fn default_playback_port() -> u16 {
    MEDIAMTX_PLAYBACK_PORT
}

const fn default_webrtc_local_udp() -> u16 {
    MEDIAMTX_WEBRTC_LOCAL_UDP_PORT
}

/// Default `FFmpeg` binary path — the plugin's `[media.ffmpeg] bin` default.
/// Shared so the deploy-side `FFmpeg` preflight and the plugin agree on the
/// fallback path.
pub const DEFAULT_FFMPEG_BIN: &str = "/usr/bin/ffmpeg";

fn default_ffmpeg_bin() -> String {
    DEFAULT_FFMPEG_BIN.to_owned()
}

/// Host-provisioning settings for `MediaMTX`, read from the `[media.mediamtx]`
/// deploy section.
///
/// Every field has a serde default (via the struct-level `#[serde(default)]` +
/// the [`Default`] impl), so a bare `[media.mediamtx]` table — or none at all —
/// yields the localhost dev shape with the controller disabled. Deserialized
/// out of the merged `autumn.toml` value via [`media_host_config_from_value`]
/// (mirroring how the plugin reads `[media]`), so it never touches
/// `autumn-web`'s strict `AutumnConfig` schema — no `autumn-media-plugin`
/// dependency and no change to the app config type.
///
/// ## Deploy-side ports ↔ app runtime base URLs must stay consistent
///
/// The `*_port` fields here (`api_port` / `rtmp_port` / `hls_port` /
/// `webrtc_port` / `playback_port`) only **provision the daemon's listeners** in
/// the rendered `mediamtx.yml` (see [`render_mediamtx_yml`]). They are a separate
/// config surface from the *app's* runtime `MediaMtxConfig` `*_base` URLs
/// (`api_base` / `hls_base` / `webrtc_base` / playback base), which the deployed
/// app/clients build request URLs from and which default to the standard ports
/// (`9997`/`8888`/`8889`/`9996`). This module deliberately does **not** read or
/// unify with the plugin's `MediaMtxConfig` — that autumn-cli→autumn-media-plugin
/// coupling is out of slice-7 scope (a plugin-owned config-model change; issue
/// #1974, codex PR #2051 finding). Consequence: **if you customize a
/// `[media.mediamtx] *_port` here you MUST update the matching app-side `*_base`
/// URL to the same port**, or the app/clients will keep calling the standard-port
/// origin the daemon no longer binds. The two surfaces are default-aligned, so
/// the common case needs no action.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct MediaMtxHostConfig {
    /// When `false` (the default) the controller is a no-op — it emits no
    /// [`DeployOp`]s, so an app that never provisions `MediaMTX` is unaffected.
    pub enabled: bool,
    /// Control-API listen port.
    pub api_port: u16,
    /// RTMP/WHIP ingest listen port.
    pub rtmp_port: u16,
    /// HLS playback listen port.
    pub hls_port: u16,
    /// WebRTC/WHEP playback listen port.
    pub webrtc_port: u16,
    /// Recording-playback listen port.
    pub playback_port: u16,
    /// WebRTC local UDP port for ICE host candidates.
    pub webrtc_local_udp: u16,
    /// Root directory recordings are written under (`recordPath` prefix).
    pub recordings_dir: String,
    /// `MediaMTX` `recordDeleteAfter` retention window (e.g. `72h`).
    pub record_delete_after: String,
    /// Remote path the rendered `mediamtx.yml` is written to.
    pub config_path: String,
    /// Remote path the `MediaMTX` binary is expected at (host bootstrap installs
    /// it; the controller does not download it).
    pub binary_path: String,
    /// systemd unit name (without the `.service` suffix).
    pub unit_name: String,
    /// Extra hostnames/IPs announced as WebRTC ICE candidates
    /// (`webrtcAdditionalHosts`) — e.g. a public address in production.
    pub webrtc_additional_hosts: Vec<String>,
}

impl Default for MediaMtxHostConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            api_port: default_api_port(),
            rtmp_port: default_rtmp_port(),
            hls_port: default_hls_port(),
            webrtc_port: default_webrtc_port(),
            playback_port: default_playback_port(),
            webrtc_local_udp: default_webrtc_local_udp(),
            recordings_dir: default_recordings_dir(),
            record_delete_after: default_record_delete_after(),
            config_path: default_config_path(),
            binary_path: default_binary_path(),
            unit_name: default_unit_name(),
            webrtc_additional_hosts: Vec::new(),
        }
    }
}

/// The `[media]` table wrapper used to deserialize a full `autumn.toml` string
/// down to the `MediaMTX` host section, sidestepping `autumn-web`'s strict
/// `AutumnConfig` schema (mirrors the plugin's own `AutumnTomlRoot`).
#[derive(Debug, Default, Deserialize)]
struct MediaTomlRoot {
    #[serde(default)]
    media: MediaSection,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct MediaSection {
    mediamtx: MediaMtxHostConfig,
    ffmpeg: FfmpegSection,
}

/// The `[media.ffmpeg]` sub-table — only the `bin` path is relevant to the
/// deploy-side `FFmpeg` preflight.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
struct FfmpegSection {
    bin: String,
}

impl Default for FfmpegSection {
    fn default() -> Self {
        Self {
            bin: default_ffmpeg_bin(),
        }
    }
}

impl MediaMtxHostConfig {
    /// The `MediaMTX` origins an embedding app must allow in its
    /// `connect-src`/`media-src` (and `frame-src` for WebRTC) CSP directives, in
    /// the local `http://127.0.0.1:<port>` shape.
    ///
    /// Returns the WebRTC (`webrtc_port`), HLS (`hls_port`), and playback
    /// (`playback_port`) origins — the three browser-facing `MediaMTX` ports. The
    /// control API (`api_port`) is server-side only (browsers never hit it) and
    /// so is deliberately excluded. In production these origins collapse to the
    /// public `MediaMTX` origin (e.g. `https://<app>-mediamtx.fly.dev`), and the
    /// object-store origin must additionally be allowed in `media-src` for
    /// recorded playback — see the module docs.
    #[must_use]
    pub fn required_csp_origins(&self) -> Vec<String> {
        vec![
            format!("http://127.0.0.1:{}", self.webrtc_port),
            format!("http://127.0.0.1:{}", self.hls_port),
            format!("http://127.0.0.1:{}", self.playback_port),
        ]
    }
}

/// Deserialize the `[media]` host config out of an **already-merged**
/// `autumn.toml` root [`toml::Value`] — base ← inline `[profile.<name>]` ←
/// `autumn-<profile>.toml` — returning both the [`MediaMtxHostConfig`] and the
/// `[media.ffmpeg] bin` path (issue #2051, Finding O).
///
/// The deploy-side loader resolves the media config under the **target deploy
/// profile** using the SAME base+profile TOML layering `AutumnConfig::load()`
/// applies to the app config, so a profiled deploy (`prod`/`staging`) sees the
/// profile's `[media.mediamtx]` / `[media.ffmpeg]` (from
/// `[profile.<name>.media.*]` or `autumn-<profile>.toml`) instead of the base
/// default. Rather than round-tripping the merged value back through a string
/// (which can panic on `toml::to_string` table-ordering), the caller merges raw
/// `toml::Value`s and deserializes the `[media]` subtree here.
///
/// Fail-closed: an *absent*
/// `[media]` subtree maps to the disabled default, but a subtree present with an
/// **ill-typed value in any contributing layer** is a hard [`Err`] (the merged
/// value carries the bad type), so the caller aborts `deploy plan`/`up` rather
/// than deploying a media-enabled app WITHOUT provisioning `MediaMTX`.
///
/// # Errors
///
/// Returns the [`toml::de::Error`] when the merged `[media.mediamtx]` /
/// `[media.ffmpeg]` subtree has a wrong-typed value.
pub fn media_host_config_from_value(
    root: toml::Value,
) -> Result<(MediaMtxHostConfig, String), toml::de::Error> {
    let parsed: MediaTomlRoot = root.try_into()?;
    Ok((parsed.media.mediamtx, parsed.media.ffmpeg.bin))
}

/// Render the `MediaMTX` `mediamtx.yml`, parameterized by [`MediaMtxHostConfig`].
///
/// Generalized from arroyo's broadcast-only `mediamtx.yml`: same LL-HLS window
/// (`hlsVariant: lowLatency`, `hlsPartDuration: 200ms`, `hlsSegmentDuration: 2s`,
/// `hlsSegmentCount: 60`), same fmp4 recording defaults under `recordings_dir`,
/// and the same WebRTC config — plus a **`~^room/.+$` path matcher** for
/// autumn-media's Rooms primitive (Slice 6), which arroyo lacks (it is
/// broadcast-only). Pure — exposed for unit assertions.
///
/// # `MediaMTX` schema currency (validated against v1.19.x / `bluenviron/mediamtx:1`)
///
/// `MediaMTX` rejects an **unknown** config field at startup (`json: unknown
/// field ...`), and this config is installed by a post-cutover `systemctl
/// restart` — so a single stale key would commit the app release then leave the
/// daemon dead and playback stranded. Every key rendered here is therefore
/// cross-checked against the current `MediaMTX` reference config
/// (<https://github.com/bluenviron/mediamtx/blob/main/mediamtx.yml> and
/// `internal/conf/conf.go`), the schema targeted by arroyo's pinned
/// `bluenviron/mediamtx:1` image. Notably the HLS CORS key is the plural array
/// `hlsAllowOrigins: ['*']` — the singular scalar `hlsAllowOrigin` is a
/// deprecated alias, so the current plural form is emitted (issue #2051,
/// Finding S). The globals (`api`/`apiAddress`, `playback`/`playbackAddress`,
/// `rtmp`/`rtmpAddress`, `hls`/`hlsAddress`/`hlsVariant`/`hlsPartDuration`/
/// `hlsSegmentCount`/`hlsSegmentDuration`, `webrtc`/`webrtcAddress`/
/// `webrtcAllowOrigins`/`webrtcLocalUDPAddress`/`webrtcAdditionalHosts`, the
/// `rtsp`/`srt`/`moq`/`metrics`/`pprof` disable toggles), the `pathDefaults`
/// recording keys (`record`/`recordPath`/`recordFormat`/`recordPartDuration`/
/// `recordSegmentDuration`/`recordDeleteAfter`), and the `paths` matchers all
/// match that reference in name and value shape. Future edits: re-validate any
/// added key against that reference before shipping it.
#[must_use]
pub fn render_mediamtx_yml(cfg: &MediaMtxHostConfig) -> String {
    // `webrtcAdditionalHosts` renders as an inline YAML list. Each host is
    // single-quoted so an IPv6 literal or a host with special chars stays a
    // single scalar; empty -> `[]`.
    let webrtc_hosts = if cfg.webrtc_additional_hosts.is_empty() {
        "[]".to_owned()
    } else {
        let joined = cfg
            .webrtc_additional_hosts
            .iter()
            .map(|h| yaml_single_quote(h))
            .collect::<Vec<_>>()
            .join(", ");
        format!("[{joined}]")
    };

    // The two config-derived string values in this template (the `recordPath`
    // prefix built from `recordings_dir`, and the `recordDeleteAfter` retention
    // window) are rendered as single-quoted YAML scalars (issue #2051, Finding T
    // sweep). A plain scalar carrying a leading/trailing space or a YAML
    // structural char (`:`/`#`/`{`/`[`/…) would otherwise break parsing — and a
    // `mediamtx.yml` that fails to parse strands the daemon on the deferred
    // post-cutover `systemctl restart`, the worst time. Quoting a path/duration
    // is loss-free for MediaMTX (both parse the same string either way).
    let record_path = yaml_single_quote(&format!(
        "{}/%path/%Y-%m-%d_%H-%M-%S-%f",
        cfg.recordings_dir
    ));
    let record_delete = yaml_single_quote(&cfg.record_delete_after);

    format!(
        "logLevel: info\n\
         \n\
         # Only the protocols autumn-media manages are enabled below (RTMP ingest,\n\
         # HLS, WebRTC/WHEP playback, the control API, and the recording-playback\n\
         # server). Every other protocol MediaMTX turns on by default is disabled\n\
         # explicitly, so the systemd unit only ever opens the ports this deploy\n\
         # plan declares — an occupied default RTSP (:8554), SRT (:8890), or MoQ\n\
         # (:8892) port cannot make `systemctl restart mediamtx` fail after\n\
         # preflight passed. (`rtsp: no` also disables RTSPS; `moq: no` closes the\n\
         # default-on MoQ HTTP/2+HTTP/3 listeners — autumn-media does not use MoQ;\n\
         # `metrics`/`pprof` default off but are pinned off for the same 'declared\n\
         # ports only' contract.)\n\
         rtsp: no\n\
         srt: no\n\
         moq: no\n\
         metrics: no\n\
         pprof: no\n\
         \n\
         # The control API is the server-side control plane: the co-located app\n\
         # reaches it at 127.0.0.1:{api} and it is intentionally excluded from the\n\
         # browser-facing CSP origins, so bind it to loopback only rather than\n\
         # every interface. The viewer/ingest listeners below (RTMP, HLS,\n\
         # WebRTC/WHEP, playback) stay bound on all interfaces so encoders and\n\
         # browsers can reach them.\n\
         api: true\n\
         apiAddress: 127.0.0.1:{api}\n\
         \n\
         playback: true\n\
         playbackAddress: :{playback}\n\
         \n\
         rtmp: true\n\
         rtmpAddress: :{rtmp}\n\
         \n\
         hls: true\n\
         hlsAddress: :{hls}\n\
         hlsAllowOrigins: ['*']\n\
         hlsVariant: {hls_variant}\n\
         hlsPartDuration: {hls_part}\n\
         # DVR-on-live window: retained window = hlsSegmentCount x hlsSegmentDuration\n\
         # = {seg_count} x {seg_dur}. Surfaced to the player so the UI never promises\n\
         # more rewind than MediaMTX actually keeps.\n\
         hlsSegmentCount: {seg_count}\n\
         hlsSegmentDuration: {seg_dur}\n\
         \n\
         webrtc: true\n\
         webrtcAddress: :{webrtc}\n\
         webrtcAllowOrigins: ['*']\n\
         webrtcLocalUDPAddress: :{webrtc_udp}\n\
         webrtcAdditionalHosts: {webrtc_hosts}\n\
         \n\
         pathDefaults:\n\
         \x20\x20source: publisher\n\
         \x20\x20record: true\n\
         \x20\x20recordPath: {rec_path}\n\
         \x20\x20recordFormat: fmp4\n\
         \x20\x20recordPartDuration: 1s\n\
         \x20\x20recordSegmentDuration: 5m\n\
         \x20\x20recordDeleteAfter: {rec_delete}\n\
         \n\
         paths:\n\
         # Broadcast ingest (single-publisher channels), as in arroyo.\n\
         \x20\x20~^live/.+$:\n\
         \x20\x20\x20\x20source: publisher\n\
         # autumn-media Rooms (Slice 6): room/{{namespace?}}/{{room_id}}/{{participant}}.\n\
         # arroyo is broadcast-only and has no room matcher; this is the addition.\n\
         \x20\x20~^room/.+$:\n\
         \x20\x20\x20\x20source: publisher\n",
        api = cfg.api_port,
        playback = cfg.playback_port,
        rtmp = cfg.rtmp_port,
        hls = cfg.hls_port,
        webrtc = cfg.webrtc_port,
        webrtc_udp = cfg.webrtc_local_udp,
        hls_variant = HLS_VARIANT,
        hls_part = HLS_PART_DURATION,
        seg_count = HLS_SEGMENT_COUNT,
        seg_dur = HLS_SEGMENT_DURATION,
        webrtc_hosts = webrtc_hosts,
        rec_path = record_path,
        rec_delete = record_delete,
    )
}

/// Render `value` as a single-quoted YAML scalar (the only escape inside a YAML
/// single-quoted scalar is a doubled `''` for a literal quote). Used for the
/// config-derived string values in [`render_mediamtx_yml`] (record path, retention
/// window, WebRTC additional hosts) so a value carrying whitespace or a YAML
/// structural char stays a single scalar and never breaks `mediamtx.yml` parsing.
#[must_use]
fn yaml_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

/// Render `value` as a systemd double-quoted `ExecStart` argument (issue #2051,
/// Finding T). systemd is **not** a shell: it splits `ExecStart` on whitespace but
/// honors its own C-style quoting, where a double-quoted token is one argument and
/// `\\` / `\"` are the escapes for a literal backslash / quote. So a
/// `binary_path`/`config_path` containing a space (e.g. `/opt/media mtx/mediamtx`)
/// must be wrapped in double quotes or the deferred post-cutover
/// `systemctl restart` parses the command as the truncated path `/opt/media`.
/// Backslash is escaped first, then the quote, so the result round-trips exactly.
#[must_use]
fn systemd_quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Render the systemd unit that supervises `MediaMTX`, styled like
/// [`super::render_app_unit`] / [`super::proxy::KamalProxyController::render_proxy_unit`].
///
/// `ExecStart` is `"<binary_path>" "<config_path>"` — both paths are
/// [`systemd_quote`]d (issue #2051, Finding T) so a path containing whitespace
/// stays a single argument instead of splitting the command; `Restart=always` (a
/// media daemon that dies must come straight back so ingest/playback recover
/// without operator intervention). Pure — exposed for unit assertions.
#[must_use]
pub fn render_mediamtx_unit(cfg: &MediaMtxHostConfig) -> String {
    format!(
        "[Unit]\n\
         Description=MediaMTX (autumn-media host daemon)\n\
         After=network-online.target\n\
         Wants=network-online.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={bin} {config}\n\
         Restart=always\n\
         RestartSec=2\n\
         \n\
         [Install]\n\
         WantedBy=multi-user.target\n",
        bin = systemd_quote(&cfg.binary_path),
        config = systemd_quote(&cfg.config_path),
    )
}

/// The parent directory of a remote unix path — everything before the last `/`,
/// or `None` when the path has no parent segment (a bare filename, or a
/// root-level path whose parent `/` always exists).
///
/// Used to `mkdir -p` a config file's directory before `scp` writes into it:
/// [`super::exec::SshExecutor::upload`] shells out to `scp`, which does **not**
/// create missing parent directories, so the default
/// `config_path = /etc/mediamtx/mediamtx.yml` would otherwise fail at
/// `media-write-config` on the first deploy (the `/etc/mediamtx` dir does not
/// exist on a fresh host). Pure — unit-tested.
#[must_use]
fn parent_dir(path: &str) -> Option<&str> {
    match path.rsplit_once('/') {
        Some((parent, _)) if !parent.is_empty() => Some(parent),
        _ => None,
    }
}

/// Provisions `MediaMTX` as a host systemd unit, modeled on
/// [`super::proxy::KamalProxyController`].
///
/// The single [`Self::ensure_installed_ops`] method returns the ordered
/// [`DeployOp`]s (rather than driving the executor directly) so the media steps
/// slot into the same recorded op sequence as the rest of the deploy and are
/// assertable in-order against the recording fake.
#[derive(Debug, Clone)]
pub struct MediaMtxController {
    cfg: MediaMtxHostConfig,
}

impl MediaMtxController {
    /// Build a controller for the given host config.
    #[must_use]
    pub const fn new(cfg: MediaMtxHostConfig) -> Self {
        Self { cfg }
    }

    /// Remote path of the systemd unit (`/etc/systemd/system/<unit>.service`).
    #[must_use]
    pub fn unit_path(&self) -> String {
        format!("/etc/systemd/system/{}.service", self.cfg.unit_name)
    }

    /// Ordered ops that render `mediamtx.yml` + the systemd unit to the host and
    /// make `MediaMTX` enabled, running, and reloaded — **restarting the daemon
    /// only when the rendered config/unit actually changed, or when the unit is
    /// not already running.**
    ///
    /// Returns an **empty vector when the section is not `enabled`** — the
    /// controller is a no-op for an app that does not provision `MediaMTX`. When
    /// enabled it emits, in order:
    /// 1. `Run` `mkdir -p <config parent dir>` — `scp` (the upload transport)
    ///    does not create parents, so the config dir must exist before op 2
    ///    writes the staged config into it (omitted only for a parent-less
    ///    `config_path`). The unit's parent (`/etc/systemd/system`) is a standard
    ///    existing dir and needs no `mkdir`.
    /// 2. `WriteFile` the rendered `mediamtx.yml` to a **temp path**
    ///    (`<config_path>.autumn-new`, mode `0644`) — a sibling of the live file,
    ///    never over it, so the live config is untouched until op 4 proves a real
    ///    change.
    /// 3. `WriteFile` the rendered unit to a **temp path**
    ///    (`/etc/systemd/system/<unit>.service.autumn-new`, mode `0644`).
    /// 4. `Run` a single idempotent shell command that `cmp -s`-compares each
    ///    staged temp file to its live counterpart (a missing live file counts as
    ///    changed), copies over only the changed ones (tracking a `changed`
    ///    flag), removes the temp files, runs `systemctl daemon-reload` +
    ///    `systemctl enable <unit>.service` (both idempotent), and restarts the
    ///    unit **only if** `changed` is set **or** the unit is not currently
    ///    active (`systemctl is-active --quiet <unit>.service` fails).
    ///
    /// The load-bearing behaviour (issue #2051, Finding H): a pure app redeploy
    /// or a no-op media-config redeploy leaves the config and unit byte-identical
    /// and the unit already active, so **no restart runs** — active
    /// RTMP/WebRTC/HLS sessions are never killed before cutover. A genuine config
    /// or unit change (or a dead/never-started unit) still restarts, so the
    /// freshest config always takes effect.
    #[must_use]
    pub fn ensure_installed_ops(&self) -> Vec<DeployOp> {
        if !self.cfg.enabled {
            return Vec::new();
        }
        let unit = self.cfg.unit_name.clone();
        let config_tmp = format!("{}.autumn-new", self.cfg.config_path);
        let unit_path = self.unit_path();
        let unit_tmp = format!("{unit_path}.autumn-new");
        let mut ops = Vec::new();
        // Create the config's parent dir before scp writes the staged file into
        // it — `scp` never creates missing parents, so the default
        // `/etc/mediamtx/mediamtx.yml` would fail at `media-write-config` on a
        // fresh host without this. The temp file is a sibling of the live config,
        // so this one `mkdir` covers both. Skipped for a parent-less path.
        if let Some(parent) = parent_dir(&self.cfg.config_path) {
            ops.push(DeployOp::Run(RemoteCommand::new(
                "media-prepare-dirs",
                format!("mkdir -p {}", super::exec::shell_quote(parent)),
            )));
        }
        // Stage config + unit to TEMP paths (never directly over the live files),
        // so op 4 can diff them and restart only on a real change.
        ops.push(DeployOp::WriteFile {
            label: "media-write-config",
            contents: FileContents::Plain(render_mediamtx_yml(&self.cfg)),
            remote_path: config_tmp.clone(),
            mode: Some(0o644),
        });
        ops.push(DeployOp::WriteFile {
            label: "media-write-unit",
            contents: FileContents::Plain(render_mediamtx_unit(&self.cfg)),
            remote_path: unit_tmp.clone(),
            mode: Some(0o644),
        });

        let cfg_live = super::exec::shell_quote(&self.cfg.config_path);
        let cfg_new = super::exec::shell_quote(&config_tmp);
        let unit_live = super::exec::shell_quote(&unit_path);
        let unit_new = super::exec::shell_quote(&unit_tmp);
        // The systemd unit id passed to `systemctl` is a config-derived string
        // (`unit_name`), so shell-quote `<unit_name>.service` (issue #2051, Finding
        // T sweep) — an unquoted unit name containing whitespace/special chars
        // would otherwise split the `systemctl enable/is-active/restart` arguments.
        let unit_service = super::exec::shell_quote(&format!("{unit}.service"));
        // POSIX-sh, fail-fast (`set -e`): compare each staged file to its live
        // counterpart; a missing live file (`cmp` non-zero) or a real diff copies
        // it over and flags `changed`. daemon-reload + enable are idempotent. The
        // unit is restarted ONLY when the config/unit changed OR it is not
        // already active — an app-only redeploy over unchanged, already-running
        // media never interrupts live sessions.
        let install = format!(
            "set -e; \
             changed=0; \
             if ! cmp -s {cfg_new} {cfg_live}; then cp {cfg_new} {cfg_live}; chmod 0644 {cfg_live}; changed=1; fi; \
             if ! cmp -s {unit_new} {unit_live}; then cp {unit_new} {unit_live}; chmod 0644 {unit_live}; changed=1; fi; \
             rm -f {cfg_new} {unit_new}; \
             systemctl daemon-reload; \
             systemctl enable {unit_service}; \
             if [ \"$changed\" -ne 0 ] || ! systemctl is-active --quiet {unit_service}; then \
             systemctl restart {unit_service}; \
             fi",
        );
        ops.push(DeployOp::Run(RemoteCommand::new("media-install", install)));
        ops
    }
}

// ── Doctor checks (fail-closed, executor-driven) ─────────────────────────────
//
// Each check runs a bounded remote command over the injectable `DeployExecutor`
// and returns the shared `PreflightCheck` type. An unverifiable result (the
// command could not run, or its output does not prove the healthy case) reports
// a clear failure — never a silent pass — so a broken media host is caught
// before ingest/playback/encode silently fail at runtime.

/// The check name for the `FFmpeg` preflight.
pub const CHECK_FFMPEG_PREFLIGHT: &str = "media_ffmpeg_preflight";
/// The check name for the recordings-directory-writable probe.
pub const CHECK_RECORDINGS_DIR_WRITABLE: &str = "media_recordings_dir_writable";
/// The check name for the `MediaMTX` port-availability probe.
pub const CHECK_MEDIAMTX_PORTS_AVAILABLE: &str = "media_mediamtx_ports_available";
/// The check name for the `MediaMTX` binary-executable preflight.
pub const CHECK_MEDIAMTX_BINARY: &str = "media_mediamtx_binary";
/// The check name for the `MediaMTX` distinct-listener-port config preflight.
pub const CHECK_MEDIAMTX_PORTS_DISTINCT: &str = "media_mediamtx_ports_distinct";

/// Remediation hint shown when `FFmpeg` does not resolve.
const FFMPEG_HINT: &str =
    "Install FFmpeg on the host (apt package) and set `[media.ffmpeg] bin` to its path";
/// Remediation hint shown when the recordings dir is not writable.
const RECORDINGS_HINT: &str =
    "Create `[media.mediamtx] recordings_dir` on the host and make it writable by the media user";
/// Remediation hint shown when a `MediaMTX` port is occupied or unverifiable.
const PORTS_HINT: &str =
    "Free the conflicting MediaMTX ports on the host (or stop the service already bound to them)";
/// Remediation hint shown when two same-protocol `MediaMTX` listeners are
/// configured onto one port.
const PORTS_DISTINCT_HINT: &str = "Give each MediaMTX TCP listener (`rtmp_port`/`hls_port`/`webrtc_port`/`playback_port`/`api_port`) \
     a distinct port in `[media.mediamtx]`";
/// Remediation hint shown when the `MediaMTX` binary is missing / not executable.
const MEDIAMTX_BINARY_HINT: &str = "Install the MediaMTX binary at `[media.mediamtx] binary_path` on the host — a host-bootstrap \
     prerequisite (download/version-pin the Go binary, exactly as autumn does for kamal-proxy)";

/// Remediation hint shown when `[media.ffmpeg] bin` is env/interpolation
/// indirected and therefore deferred to the service's own runtime resolution.
const FFMPEG_UNRESOLVED_HINT: &str = "Set `[media.ffmpeg] bin` to a concrete path to verify it here, or ensure \
     AUTUMN_MEDIA__FFMPEG__BIN / the referenced ${VAR} is set in the SERVICE \
     environment (the deploy side defers env-indirected paths to runtime)";

/// Grade that `FFmpeg` resolves on the host and reports a version banner
/// (generalized from arroyo's boot preflight).
///
/// The deploy side verifies **only a concrete literal** `[media.ffmpeg] bin` (the
/// default `/usr/bin/ffmpeg` or any explicit path): that value is
/// environment-independent, so probing it here matches the binary the service
/// will run. Any **env/interpolation-indirected** path — an empty value, or one
/// carrying a `${...}` placeholder (including `${AUTUMN_MEDIA__FFMPEG__BIN}`) — is
/// resolved by the deployed service from ITS OWN environment, which the deploy
/// side neither has nor may guess (`build_env_file` does not propagate the
/// operator's local shell). Probing a value resolved against the CLI's local env
/// would be a false pass/fail against a binary the service will not use, so such a
/// path is **deferred to runtime**: a non-passing but **non-blocking** result
/// (`deferred: true`, so [`PreflightCheck::blocking`] is `false`) that surfaces as
/// a warning and does **not** abort `deploy up` — the deployed service resolves
/// `FFmpeg` from its own environment at runtime. Otherwise it runs `<bin>
/// -version` over the executor and requires the standard `ffmpeg version` banner
/// in stdout — a **concrete literal** binary that is missing, not executable, or
/// is not actually `FFmpeg` (no banner) is a clear, **blocking** failure.
/// Fail-closed: a transport error is a blocking failure, not a pass.
#[must_use]
pub fn ffmpeg_preflight(exec: &impl DeployExecutor, ffmpeg_bin: &str) -> PreflightCheck {
    // Env/interpolation indirection the deployed service resolves from its OWN
    // environment (empty value, or a `${...}` placeholder such as
    // `${AUTUMN_MEDIA__FFMPEG__BIN}`): defer rather than resolve/probe against the
    // CLI's local env, which the service does not share.
    if ffmpeg_bin.trim().is_empty() || ffmpeg_bin.contains("${") {
        return PreflightCheck {
            name: CHECK_FFMPEG_PREFLIGHT,
            passed: false,
            deferred: true,
            detail: format!(
                "FFmpeg path uses env/interpolation indirection (`{ffmpeg_bin}`) resolved in \
                 the service environment; deferring verification to runtime"
            ),
            hint: Some(FFMPEG_UNRESOLVED_HINT),
        };
    }
    let cmd = RemoteCommand::new(
        "media-ffmpeg-preflight",
        format!("{} -version", super::exec::shell_quote(ffmpeg_bin)),
    );
    match exec.run(&cmd) {
        Ok(out) if out.stdout.contains("ffmpeg version") => PreflightCheck {
            name: CHECK_FFMPEG_PREFLIGHT,
            passed: true,
            deferred: false,
            detail: format!("FFmpeg resolves at `{ffmpeg_bin}` and reports a version banner"),
            hint: None,
        },
        Ok(_) => PreflightCheck {
            name: CHECK_FFMPEG_PREFLIGHT,
            passed: false,
            deferred: false,
            detail: format!(
                "`{ffmpeg_bin} -version` ran but did not report an `ffmpeg version` banner — \
                 the binary is not FFmpeg"
            ),
            hint: Some(FFMPEG_HINT),
        },
        Err(err) => PreflightCheck {
            name: CHECK_FFMPEG_PREFLIGHT,
            passed: false,
            deferred: false,
            detail: format!("could not run `{ffmpeg_bin} -version` on the host: {err}"),
            hint: Some(FFMPEG_HINT),
        },
    }
}

/// Grade that the `MediaMTX` recordings directory exists and is writable on the
/// host.
///
/// Runs `test -d <dir> && test -w <dir>` over the executor. Fail-closed: a
/// missing/unwritable directory (non-zero exit → transport error) and any other
/// transport failure both report a clear failure.
#[must_use]
pub fn recordings_dir_writable(
    exec: &impl DeployExecutor,
    cfg: &MediaMtxHostConfig,
) -> PreflightCheck {
    let dir = &cfg.recordings_dir;
    let quoted = super::exec::shell_quote(dir);
    let cmd = RemoteCommand::new(
        "media-recordings-dir",
        format!("test -d {quoted} && test -w {quoted}"),
    );
    match exec.run(&cmd) {
        Ok(_) => PreflightCheck {
            name: CHECK_RECORDINGS_DIR_WRITABLE,
            passed: true,
            deferred: false,
            detail: format!("recordings directory `{dir}` exists and is writable"),
            hint: None,
        },
        Err(err) => PreflightCheck {
            name: CHECK_RECORDINGS_DIR_WRITABLE,
            passed: false,
            deferred: false,
            detail: format!("recordings directory `{dir}` is missing or not writable: {err}"),
            hint: Some(RECORDINGS_HINT),
        },
    }
}

/// The transport protocol of a listening socket (the `ss` Netid column). Only
/// `tcp` / `udp` are relevant to `MediaMTX`; any other Netid is ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Protocol {
    Tcp,
    Udp,
}

/// The `MediaMTX` ports a bare host must have free before provisioning, **each
/// tagged with the protocol `MediaMTX` actually binds** (issue #2051, Finding I):
/// the five TCP listeners (RTMP, HLS, WebRTC/WHEP, playback, control API) plus the
/// WebRTC local **UDP** port (`webrtcLocalUDPAddress`, ICE host candidates). A
/// listener conflicts only when it matches BOTH the port AND the protocol — a
/// same-port/different-protocol listener (e.g. a TCP service on the UDP-only
/// `:8189`) is not a conflict.
#[must_use]
fn mediamtx_required_ports(cfg: &MediaMtxHostConfig) -> Vec<(u16, Protocol)> {
    vec![
        (cfg.api_port, Protocol::Tcp),
        (cfg.rtmp_port, Protocol::Tcp),
        (cfg.hls_port, Protocol::Tcp),
        (cfg.webrtc_port, Protocol::Tcp),
        (cfg.playback_port, Protocol::Tcp),
        (cfg.webrtc_local_udp, Protocol::Udp),
    ]
}

/// A listening socket parsed from `ss -H -tulnp`: the local port, its transport
/// protocol (the Netid column — issue #2051, Finding I), and the owning process
/// name (`None` when `ss` could not attribute the socket — e.g. the
/// `users:(("name",…))` column is absent because the caller lacked privilege).
#[derive(Debug, Clone, PartialEq, Eq)]
struct ListeningSocket {
    port: u16,
    protocol: Protocol,
    owner: Option<String>,
}

/// Parse the listening sockets out of `ss -H -tulnp` output.
///
/// Each row's 1st column (index 0) is the `Netid` (`tcp` / `udp`) and its 5th
/// column (index 4: `Netid State Recv-Q Send-Q Local Peer …`) is the
/// `Local Address:Port` — the port is the tail after the last `:` (covers IPv4
/// `0.0.0.0:8888`, IPv6 `[::]:8888`, and `*:8888`). The owning process is read
/// from the trailing `users:(("<name>",pid=…,fd=…))` column (added by `-p`), or
/// `None` when that column is absent. Rows whose Netid is not `tcp`/`udp`, or
/// whose port does not parse, are skipped. Pure — unit-tested against captured
/// `ss` output.
#[must_use]
fn parse_listening_sockets(ss_output: &str) -> Vec<ListeningSocket> {
    ss_output
        .lines()
        .filter_map(|line| {
            let mut cols = line.split_whitespace();
            let protocol = match cols.next()? {
                "tcp" => Protocol::Tcp,
                "udp" => Protocol::Udp,
                _ => return None,
            };
            // We already consumed index 0 (Netid); the Local column is index 4,
            // i.e. the 4th remaining column (skip State/Recv-Q/Send-Q).
            let local = cols.nth(3)?;
            let port = local.rsplit(':').next()?.parse::<u16>().ok()?;
            Some(ListeningSocket {
                port,
                protocol,
                owner: process_name_from_ss_line(line),
            })
        })
        .collect()
}

/// Extract the owning process name from an `ss -p` line's
/// `users:(("<name>",pid=…,fd=…))` column, if present. Returns the first process
/// name (a socket shared by several PIDs of the same binary lists them all, but
/// the first name is the owning binary). `None` when the column is absent.
#[must_use]
fn process_name_from_ss_line(line: &str) -> Option<String> {
    // `…users:(("mediamtx",pid=10,fd=7))` → the text after the opening `"`.
    let after_quote = line.split_once("users:((\"")?.1;
    let name = after_quote.split_once('"')?.0;
    (!name.is_empty()).then(|| name.to_owned())
}

/// The managed `MediaMTX` process name, derived from the configured binary path's
/// basename (`/usr/local/bin/mediamtx` → `mediamtx`). This is the `comm` `ss`
/// reports for our own daemon; the kernel truncates `comm` to 15 chars, which the
/// short `mediamtx` name never hits.
#[must_use]
fn managed_process_name(cfg: &MediaMtxHostConfig) -> &str {
    cfg.binary_path
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(cfg.binary_path.as_str())
}

/// Whether our configured `MediaMTX` systemd unit is currently active, via
/// `systemctl is-active --quiet <unit>.service` (exit `0` ⇒ active). Any failure
/// (inactive, unknown unit, or transport error) is read as **not active** — the
/// fail-closed direction for [`mediamtx_ports_available`]'s Finding-J gate. Run
/// at most once per check and the result reused.
#[must_use]
fn unit_is_active(exec: &impl DeployExecutor, unit_name: &str) -> bool {
    // `unit_name` is a config-derived string, so shell-quote `<unit>.service`
    // (issue #2051, Finding T sweep) rather than interpolating it bare.
    let unit_service = super::exec::shell_quote(&format!("{unit_name}.service"));
    let cmd = RemoteCommand::new(
        "media-unit-active",
        format!("systemctl is-active --quiet {unit_service}"),
    );
    exec.run(&cmd).is_ok()
}

/// Grade that the `MediaMTX` ports are free on the host (no *foreign* service is
/// already bound to them).
///
/// **Protocol-aware and unit-verified** (issue #2051, Findings I + J). It runs a
/// single port scan — `ss -H -tulnp` (the `-p` adds the owning-process column,
/// the leading Netid gives the protocol) over the executor — and classifies each
/// configured target `(port, protocol)`:
///
/// - No **same-protocol** listener on the port → free → pass. A same-port but
///   *different-protocol* listener is NOT a conflict: the WebRTC local port
///   `:8189` is UDP-only, so a TCP service on `8189` (or a UDP service on a TCP
///   `MediaMTX` port) never cross-conflicts (Finding I).
/// - Every same-protocol listener owned by *our own* managed `MediaMTX` (process
///   basename of [`MediaMtxHostConfig::binary_path`]) **and** our configured unit
///   reports active (`systemctl is-active --quiet <unit>.service`) → a same-port
///   redeploy of our own daemon, not a conflict → pass (informational note).
/// - A same-protocol listener owned by a `MediaMTX` process while our configured
///   unit is **not** active (manual run, a differently-named service, or a
///   crashed unit) → conflict → fail-closed (Finding J): `enable --now` would
///   start a *second* instance over ports the old process still owns.
/// - A same-protocol listener owned by any *other* process, **or** whose owner
///   cannot be attributed (empty process column — e.g. insufficient privilege) →
///   conflict → fail-closed. `autumn deploy` runs as root managing systemd, so
///   `-p` attribution is normally available; an unattributable listener on a
///   target port is deliberately treated as a conflict rather than assumed benign.
///
/// The unit `is-active` query runs at most **once** (only when a MediaMTX-owned
/// port is actually found) and the result is reused across the held ports.
///
/// Fail-closed: if `ss` cannot run, or its output does not parse into any
/// recognizable listener, the check fails with a "could not verify" message
/// rather than passing blind.
#[must_use]
pub fn mediamtx_ports_available(
    exec: &impl DeployExecutor,
    cfg: &MediaMtxHostConfig,
) -> PreflightCheck {
    // `-p` attaches the owning-process column so a target port held by our own
    // managed MediaMTX (a same-port redeploy) is distinguished from a foreign
    // conflict; the Netid column gives the protocol. Deploy runs as root, so `-p`
    // attribution is available.
    let cmd = RemoteCommand::new("media-ports", "ss -H -tulnp".to_owned());
    let output = match exec.run(&cmd) {
        Ok(out) => out.stdout,
        Err(err) => {
            return PreflightCheck {
                name: CHECK_MEDIAMTX_PORTS_AVAILABLE,
                passed: false,
                deferred: false,
                detail: format!("could not verify MediaMTX port availability (`ss` failed): {err}"),
                hint: Some(PORTS_HINT),
            };
        }
    };

    let sockets = parse_listening_sockets(&output);
    // Fail-closed: a non-empty `ss` run that yields zero parseable sockets means we
    // could not actually read the host's listener state — treat it as
    // unverifiable rather than a pass.
    if sockets.is_empty() && !output.trim().is_empty() {
        return PreflightCheck {
            name: CHECK_MEDIAMTX_PORTS_AVAILABLE,
            passed: false,
            deferred: false,
            detail: "could not verify MediaMTX port availability: `ss` output did not parse into \
                     any listening ports"
                .to_owned(),
            hint: Some(PORTS_HINT),
        };
    }

    let ours = managed_process_name(cfg);
    // Ports held by a foreign / unattributable same-protocol listener.
    let mut foreign: Vec<u16> = Vec::new();
    // Ports held only by our own MediaMTX process (pending the Finding-J
    // is-active gate below).
    let mut ours_ports: Vec<u16> = Vec::new();
    for (port, protocol) in mediamtx_required_ports(cfg) {
        // A listener conflicts only when it matches BOTH the port AND the
        // protocol (Finding I) — a same-port/different-protocol listener is not
        // a conflict.
        let mut matching = sockets
            .iter()
            .filter(|s| s.port == port && s.protocol == protocol)
            .peekable();
        if matching.peek().is_none() {
            continue; // no same-protocol listener → free
        }
        // A listener owned by our own mediamtx is (provisionally) ours; a foreign
        // process OR an unattributable listener (owner `None`) is a conflict.
        if matching.all(|s| s.owner.as_deref() == Some(ours)) {
            ours_ports.push(port);
        } else {
            foreign.push(port);
        }
    }

    // Finding J: a MediaMTX-owned port counts as "ours" only when our configured
    // unit is actually active. If the ports are held by a `mediamtx` process that
    // is NOT our managed unit (manual run / differently-named service / crashed),
    // treat them as conflicts (fail-closed) — `enable --now` must not start a
    // second instance over them. The is-active query runs at most once.
    let mut held_by_us: Vec<u16> = Vec::new();
    let mut unmanaged: Vec<u16> = Vec::new();
    if !ours_ports.is_empty() {
        if unit_is_active(exec, &cfg.unit_name) {
            held_by_us = ours_ports;
        } else {
            unmanaged = ours_ports;
        }
    }

    let mut conflict_msgs: Vec<String> = Vec::new();
    foreign.sort_unstable();
    foreign.dedup();
    if !foreign.is_empty() {
        conflict_msgs.push(format!(
            "MediaMTX port(s) already in use by another service on the host: {}",
            join_ports(&foreign)
        ));
    }
    unmanaged.sort_unstable();
    unmanaged.dedup();
    if !unmanaged.is_empty() {
        conflict_msgs.push(format!(
            "MediaMTX port(s) {} are held by a `{ours}` process, but the configured \
             `{}` unit is not active — an unmanaged/manual MediaMTX (or a crashed unit) still \
             owns them, so starting our unit would launch a second instance over them",
            join_ports(&unmanaged),
            cfg.unit_name,
        ));
    }
    if !conflict_msgs.is_empty() {
        return PreflightCheck {
            name: CHECK_MEDIAMTX_PORTS_AVAILABLE,
            passed: false,
            deferred: false,
            detail: conflict_msgs.join("; "),
            hint: Some(PORTS_HINT),
        };
    }

    let detail = if held_by_us.is_empty() {
        "all MediaMTX ports are free on the host".to_owned()
    } else {
        format!(
            "MediaMTX ports are available; port(s) {} are already held by our own managed \
             MediaMTX (same-port redeploy)",
            join_ports(&held_by_us),
        )
    };
    PreflightCheck {
        name: CHECK_MEDIAMTX_PORTS_AVAILABLE,
        passed: true,
        deferred: false,
        detail,
        hint: None,
    }
}

/// Join a sorted list of ports as a `"1935, 8888"` string for messages.
#[must_use]
fn join_ports(ports: &[u16]) -> String {
    ports
        .iter()
        .map(u16::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Grade that the configured `MediaMTX` binary exists and is executable on the
/// host.
///
/// The binary is a **host-bootstrap prerequisite** — the Go binary is
/// downloaded / version-pinned by host bootstrap (see the module rustdoc), never
/// provisioned by this module, exactly as autumn treats kamal-proxy. This check
/// verifies it is present before cutover.
///
/// **Why this must run in the pre-cutover preflight:** `provision_media_host`
/// (which writes the unit and runs `systemctl … restart`) is deliberately
/// deferred until AFTER the app deploy commits. Without this check a host missing
/// the binary passes the other three checks, the app cuts over, and only THEN
/// does the deferred `systemctl restart` fail on the missing `ExecStart` —
/// leaving the new release live without its media daemon. Verifying `binary_path`
/// up front turns that into a clean, fail-closed preflight failure before any
/// cutover, symmetric with [`ffmpeg_preflight`].
///
/// Runs `test -x <binary_path>` over the executor — non-mutating. Fail-closed: a
/// missing/not-executable binary (non-zero exit → transport error) and any other
/// transport failure both report a clear failure naming `binary_path`.
#[must_use]
pub fn mediamtx_binary_preflight(
    exec: &impl DeployExecutor,
    cfg: &MediaMtxHostConfig,
) -> PreflightCheck {
    let bin = &cfg.binary_path;
    let cmd = RemoteCommand::new(
        "media-mediamtx-binary",
        format!("test -x {}", super::exec::shell_quote(bin)),
    );
    match exec.run(&cmd) {
        Ok(_) => PreflightCheck {
            name: CHECK_MEDIAMTX_BINARY,
            passed: true,
            deferred: false,
            detail: format!("MediaMTX binary `{bin}` exists and is executable"),
            hint: None,
        },
        Err(err) => PreflightCheck {
            name: CHECK_MEDIAMTX_BINARY,
            passed: false,
            deferred: false,
            detail: format!(
                "MediaMTX binary `{bin}` is missing or not executable: {err} — MediaMTX cannot \
                 start, so the post-cutover `systemctl restart` would fail; the binary is a \
                 host-bootstrap prerequisite (download/version-pin it, like kamal-proxy)"
            ),
            hint: Some(MEDIAMTX_BINARY_HINT),
        },
    }
}

/// Grade that the configured `MediaMTX` **TCP listener ports are all distinct**
/// (issue #2051, Finding P) — a **pure config** check, no executor, no host I/O.
///
/// `MediaMTX` runs a separate server per protocol, each binding its own TCP
/// socket: RTMP (`rtmp_port`), HLS (`hls_port`), WebRTC/WHEP (`webrtc_port`),
/// recording-playback (`playback_port`), and the control API (`api_port`). If two
/// of those are configured to the same value, the rendered `mediamtx.yml` asks two
/// servers to bind one port. [`mediamtx_ports_available`]'s `ss` scan can still
/// pass on a fresh host (nothing is listening yet), so the conflict would only
/// surface as a failed **deferred** `systemctl restart` AFTER app cutover — the
/// worst time. This check catches it up front and fails closed.
///
/// The WebRTC local **UDP** port (`webrtc_local_udp`) is a *separate protocol
/// namespace*, so it may legitimately equal a TCP port and is deliberately not
/// compared against the TCP set (mirroring [`mediamtx_required_ports`]'s
/// protocol-aware conflict rule). Fail-closed message names the duplicated value
/// and every field bound to it.
#[must_use]
pub fn mediamtx_ports_distinct(cfg: &MediaMtxHostConfig) -> PreflightCheck {
    // The five TCP listeners MediaMTX binds — each its own server, so all must be
    // distinct. `webrtc_local_udp` is UDP and intentionally excluded.
    let tcp: [(u16, &str); 5] = [
        (cfg.rtmp_port, "rtmp_port"),
        (cfg.hls_port, "hls_port"),
        (cfg.webrtc_port, "webrtc_port"),
        (cfg.playback_port, "playback_port"),
        (cfg.api_port, "api_port"),
    ];
    // Group fields by port (BTreeMap → deterministic ascending-port ordering) and
    // report every port claimed by more than one field.
    let mut by_port: std::collections::BTreeMap<u16, Vec<&str>> = std::collections::BTreeMap::new();
    for (port, field) in tcp {
        by_port.entry(port).or_default().push(field);
    }
    let conflicts: Vec<String> = by_port
        .iter()
        .filter(|(_, fields)| fields.len() > 1)
        .map(|(port, fields)| format!("TCP port {port} is bound by {}", fields.join(", ")))
        .collect();

    if conflicts.is_empty() {
        PreflightCheck {
            name: CHECK_MEDIAMTX_PORTS_DISTINCT,
            passed: true,
            deferred: false,
            detail: "all MediaMTX TCP listener ports are distinct".to_owned(),
            hint: None,
        }
    } else {
        PreflightCheck {
            name: CHECK_MEDIAMTX_PORTS_DISTINCT,
            passed: false,
            deferred: false,
            detail: format!(
                "MediaMTX listener ports must be unique per protocol, but {} — two servers \
                 cannot bind the same TCP port, so `systemctl restart mediamtx` would fail after \
                 cutover",
                conflicts.join("; ")
            ),
            hint: Some(PORTS_DISTINCT_HINT),
        }
    }
}

/// Collect the five `MediaMTX` host doctor checks against a live executor.
///
/// Runs the distinct-listener-port **config** check first (pure — no host I/O, so
/// a duplicated-port config aborts before any probe), then the `FFmpeg` preflight
/// (against `ffmpeg_bin`), the `MediaMTX` binary-executable preflight, the
/// recordings-dir probe, and the port-availability probe — returning the shared
/// [`PreflightCheck`] results so a caller that already holds a [`DeployExecutor`]
/// can grade a media host the same way `autumn deploy check` grades the app host.
/// `ffmpeg_bin` is the resolved `[media.ffmpeg] bin` (the plugin's default is
/// `/usr/bin/ffmpeg`).
#[must_use]
pub fn collect_media_doctor_checks(
    exec: &impl DeployExecutor,
    cfg: &MediaMtxHostConfig,
    ffmpeg_bin: &str,
) -> Vec<PreflightCheck> {
    vec![
        mediamtx_ports_distinct(cfg),
        ffmpeg_preflight(exec, ffmpeg_bin),
        mediamtx_binary_preflight(exec, cfg),
        recordings_dir_writable(exec, cfg),
        mediamtx_ports_available(exec, cfg),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::path::Path;

    use super::super::exec::{CommandOutput, DeployExecError};

    /// A recording fake executor mirroring `exec.rs`'s test fake (which is
    /// private to that module): records `run` calls in order and returns
    /// scripted stdout / scripted failures per command label, so the controller
    /// ops and the doctor checks are assertable with no live host.
    #[derive(Default)]
    struct RecordingExecutor {
        run_labels: RefCell<Vec<&'static str>>,
        fail_labels: Vec<&'static str>,
        stdout_by_label: Vec<(&'static str, String)>,
    }

    impl RecordingExecutor {
        fn new() -> Self {
            Self::default()
        }

        fn failing_on(label: &'static str) -> Self {
            Self {
                fail_labels: vec![label],
                ..Self::default()
            }
        }

        fn with_stdout(mut self, label: &'static str, stdout: impl Into<String>) -> Self {
            self.stdout_by_label.push((label, stdout.into()));
            self
        }

        /// Chainable variant of [`Self::failing_on`]: mark an additional command
        /// label as failing (so a check can succeed on one command and fail on
        /// another — e.g. `ss` succeeds but `systemctl is-active` reports the unit
        /// inactive).
        fn failing(mut self, label: &'static str) -> Self {
            self.fail_labels.push(label);
            self
        }

        fn labels(&self) -> Vec<&'static str> {
            self.run_labels.borrow().clone()
        }
    }

    impl DeployExecutor for RecordingExecutor {
        fn run(&self, cmd: &RemoteCommand) -> Result<CommandOutput, DeployExecError> {
            self.run_labels.borrow_mut().push(cmd.label);
            if self.fail_labels.contains(&cmd.label) {
                return Err(DeployExecError::CommandFailed {
                    label: cmd.label,
                    message: "scripted failure".to_owned(),
                });
            }
            let stdout = self
                .stdout_by_label
                .iter()
                .find(|(l, _)| *l == cmd.label)
                .map(|(_, out)| out.clone())
                .unwrap_or_default();
            Ok(CommandOutput {
                stdout,
                stderr: String::new(),
            })
        }

        fn upload(
            &self,
            _local: &Path,
            _remote_path: &str,
            _mode: Option<u32>,
        ) -> Result<(), DeployExecError> {
            Ok(())
        }
    }

    // ── Config defaults / deserialization ────────────────────────────────────

    #[test]
    fn config_defaults_are_the_localhost_disabled_shape() {
        let cfg = MediaMtxHostConfig::default();
        assert!(!cfg.enabled, "controller is disabled by default");
        assert_eq!(cfg.api_port, MEDIAMTX_API_PORT);
        assert_eq!(cfg.rtmp_port, MEDIAMTX_RTMP_PORT);
        assert_eq!(cfg.hls_port, MEDIAMTX_HLS_PORT);
        assert_eq!(cfg.webrtc_port, MEDIAMTX_WEBRTC_PORT);
        assert_eq!(cfg.playback_port, MEDIAMTX_PLAYBACK_PORT);
        assert_eq!(cfg.webrtc_local_udp, MEDIAMTX_WEBRTC_LOCAL_UDP_PORT);
        assert_eq!(cfg.recordings_dir, "/recordings");
        assert_eq!(cfg.record_delete_after, "72h");
        assert_eq!(cfg.config_path, "/etc/mediamtx/mediamtx.yml");
        assert_eq!(cfg.binary_path, "/usr/local/bin/mediamtx");
        assert_eq!(cfg.unit_name, "mediamtx");
        assert!(cfg.webrtc_additional_hosts.is_empty());
    }

    /// Parse a raw `autumn.toml` string and deserialize its `[media]` subtree via
    /// the production [`media_host_config_from_value`] path — the deploy loader
    /// merges raw `toml::Value`s and calls that fn, so tests exercise the same
    /// deserializer over a single (unmerged) document.
    fn host_cfg_from_toml_str(s: &str) -> Result<(MediaMtxHostConfig, String), toml::de::Error> {
        media_host_config_from_value(toml::from_str(s)?)
    }

    #[test]
    fn empty_toml_yields_disabled_defaults() {
        let (cfg, bin) = host_cfg_from_toml_str("").expect("empty toml parses");
        assert!(!cfg.enabled);
        assert_eq!(cfg.api_port, MEDIAMTX_API_PORT);
        assert_eq!(bin, DEFAULT_FFMPEG_BIN);
    }

    #[test]
    fn toml_reads_the_media_mediamtx_section() {
        let toml = "\
[media.mediamtx]
enabled = true
recordings_dir = \"/srv/recordings\"
record_delete_after = \"48h\"
webrtc_additional_hosts = [\"stream.example.com\", \"203.0.113.7\"]
unit_name = \"mediamtx-prod\"
";
        let (cfg, _) = host_cfg_from_toml_str(toml).expect("parses");
        assert!(cfg.enabled);
        assert_eq!(cfg.recordings_dir, "/srv/recordings");
        assert_eq!(cfg.record_delete_after, "48h");
        assert_eq!(
            cfg.webrtc_additional_hosts,
            vec!["stream.example.com".to_owned(), "203.0.113.7".to_owned()]
        );
        assert_eq!(cfg.unit_name, "mediamtx-prod");
        // Unspecified fields keep their defaults.
        assert_eq!(cfg.hls_port, MEDIAMTX_HLS_PORT);
    }

    #[test]
    fn toml_without_media_table_is_default() {
        let (cfg, _) = host_cfg_from_toml_str("[deploy]\nhost = \"h\"\n").expect("parses");
        assert!(!cfg.enabled);
    }

    #[test]
    fn ffmpeg_bin_defaults_and_reads_from_media_ffmpeg() {
        assert_eq!(
            host_cfg_from_toml_str("").expect("empty parses").1,
            DEFAULT_FFMPEG_BIN
        );
        let (_, bin) =
            host_cfg_from_toml_str("[media.ffmpeg]\nbin = \"/opt/ffmpeg\"\n").expect("parses");
        assert_eq!(bin, "/opt/ffmpeg");
    }

    #[test]
    fn media_config_fails_closed_on_malformed_mediamtx_section() {
        // Finding C: a PRESENT `[media.mediamtx]` with an ill-typed value is a hard
        // error, so the caller must abort rather than silently disable provisioning.
        // `enabled` as a string, not a bool:
        assert!(host_cfg_from_toml_str("[media.mediamtx]\nenabled = \"yes\"\n").is_err());
        // A port field as a non-number:
        assert!(host_cfg_from_toml_str("[media.mediamtx]\napi_port = \"nope\"\n").is_err());
    }

    #[test]
    fn media_config_fails_closed_on_malformed_ffmpeg_section() {
        // Finding C: a malformed `[media.ffmpeg]` is also a hard error — the deploy
        // deserializer reads the whole `[media]` subtree, so a broken ffmpeg table
        // cannot be tricked into the disabled default.
        let toml = "[media.ffmpeg]\nbin = 123\n";
        assert!(host_cfg_from_toml_str(toml).is_err());
    }

    // ── mediamtx.yml rendering ───────────────────────────────────────────────

    #[test]
    fn rendered_yml_contains_every_port() {
        let yml = render_mediamtx_yml(&MediaMtxHostConfig::default());
        // The control API binds loopback-only (Finding M) — server-side control
        // plane for the co-located app, excluded from browser-facing CSP origins.
        assert!(
            yml.contains("apiAddress: 127.0.0.1:9997"),
            "api port: {yml}"
        );
        assert!(yml.contains("playbackAddress: :9996"), "playback: {yml}");
        assert!(yml.contains("rtmpAddress: :1935"), "rtmp: {yml}");
        assert!(yml.contains("hlsAddress: :8888"), "hls: {yml}");
        assert!(yml.contains("webrtcAddress: :8889"), "webrtc: {yml}");
        assert!(yml.contains("webrtcLocalUDPAddress: :8189"), "udp: {yml}");
    }

    #[test]
    fn rendered_yml_binds_api_to_loopback_but_leaves_public_listeners_open() {
        // Finding M: the control API is the server-side control plane (the
        // co-located app reaches it at 127.0.0.1) and is excluded from the
        // browser-facing CSP origins, so it must NOT listen on every interface.
        let yml = render_mediamtx_yml(&MediaMtxHostConfig::default());
        assert!(
            yml.contains("apiAddress: 127.0.0.1:9997"),
            "control API must bind loopback only: {yml}"
        );
        assert!(
            !yml.contains("apiAddress: :9997"),
            "control API must NOT bind all interfaces: {yml}"
        );
        // The viewer/ingest-facing listeners MUST stay bound on all interfaces so
        // encoders and browsers can reach them — none is loopback-bound.
        for public in [
            "rtmpAddress: :1935",
            "hlsAddress: :8888",
            "webrtcAddress: :8889",
            "playbackAddress: :9996",
        ] {
            assert!(yml.contains(public), "public listener {public}: {yml}");
        }
        assert!(
            !yml.contains("rtmpAddress: 127.0.0.1")
                && !yml.contains("hlsAddress: 127.0.0.1")
                && !yml.contains("webrtcAddress: 127.0.0.1")
                && !yml.contains("playbackAddress: 127.0.0.1"),
            "no public listener may be loopback-bound: {yml}"
        );
    }

    #[test]
    fn rendered_yml_has_ll_hls_recording_and_webrtc_config() {
        let yml = render_mediamtx_yml(&MediaMtxHostConfig::default());
        // LL-HLS window copied from arroyo.
        assert!(yml.contains("hlsVariant: lowLatency"));
        assert!(yml.contains("hlsPartDuration: 200ms"));
        assert!(yml.contains("hlsSegmentDuration: 2s"));
        assert!(yml.contains("hlsSegmentCount: 60"));
        // Recording config under the configured recordings dir.
        assert!(yml.contains("record: true"));
        // Config-derived path/duration values are single-quoted YAML scalars
        // (Finding T sweep) so a value with whitespace/special chars can't break
        // parsing.
        assert!(yml.contains("recordPath: '/recordings/%path/%Y-%m-%d_%H-%M-%S-%f'"));
        assert!(yml.contains("recordFormat: fmp4"));
        assert!(yml.contains("recordDeleteAfter: '72h'"));
        // WebRTC config.
        assert!(yml.contains("webrtc: true"));
        assert!(yml.contains("webrtcAdditionalHosts: []"));
    }

    #[test]
    fn rendered_yml_uses_current_mediamtx_cors_schema() {
        // Finding S (issue #2051): MediaMTX rejects unknown config fields at
        // startup, and this config is installed by a post-cutover `systemctl
        // restart`. The HLS CORS key is the plural array `hlsAllowOrigins` in the
        // current schema (v1.19.x / `bluenviron/mediamtx:1`); the singular scalar
        // `hlsAllowOrigin` is a deprecated alias, so the template must emit the
        // current plural form, matching the already-plural `webrtcAllowOrigins`.
        let yml = render_mediamtx_yml(&MediaMtxHostConfig::default());
        assert!(
            yml.contains("hlsAllowOrigins: ['*']"),
            "HLS CORS must use the current plural array key: {yml}"
        );
        assert!(
            !yml.contains("hlsAllowOrigin:"),
            "the deprecated singular hlsAllowOrigin key must not be rendered: {yml}"
        );
        assert!(
            yml.contains("webrtcAllowOrigins: ['*']"),
            "WebRTC CORS must use the plural array key: {yml}"
        );
    }

    #[test]
    fn rendered_yml_disables_unmanaged_protocols_but_keeps_managed_ones() {
        // Finding E/Q: MediaMTX fills omitted protocol flags from its own defaults, so
        // RTSP (:8554), SRT (:8890), and MoQ (:8892) would otherwise listen even though
        // the deploy plan never declares/checks those ports — an occupied default port
        // could then fail `systemctl restart mediamtx` after preflight passed. The
        // template pins every unmanaged default-on protocol OFF so the unit only opens
        // the declared ports.
        let yml = render_mediamtx_yml(&MediaMtxHostConfig::default());
        assert!(yml.contains("rtsp: no"), "RTSP must be disabled: {yml}");
        assert!(yml.contains("srt: no"), "SRT must be disabled: {yml}");
        assert!(yml.contains("moq: no"), "MoQ must be disabled: {yml}");
        assert!(yml.contains("metrics: no"), "metrics must be off: {yml}");
        assert!(yml.contains("pprof: no"), "pprof must be off: {yml}");
        // The managed protocols stay enabled (RTMP/HLS/WebRTC/API/playback).
        assert!(yml.contains("rtmp: true"), "rtmp managed: {yml}");
        assert!(yml.contains("hls: true"), "hls managed: {yml}");
        assert!(yml.contains("webrtc: true"), "webrtc managed: {yml}");
        assert!(yml.contains("api: true"), "api managed: {yml}");
        assert!(yml.contains("playback: true"), "playback managed: {yml}");
    }

    #[test]
    fn rendered_yml_has_both_live_and_room_path_matchers() {
        let yml = render_mediamtx_yml(&MediaMtxHostConfig::default());
        assert!(yml.contains("~^live/.+$:"), "broadcast matcher: {yml}");
        // The autumn-media addition arroyo lacks.
        assert!(yml.contains("~^room/.+$:"), "room matcher: {yml}");
    }

    #[test]
    fn rendered_yml_honors_custom_ports_dirs_and_hosts() {
        let cfg = MediaMtxHostConfig {
            api_port: 19997,
            hls_port: 18888,
            recordings_dir: "/data/rec".to_owned(),
            record_delete_after: "24h".to_owned(),
            webrtc_additional_hosts: vec!["a.example.com".to_owned(), "b.example.com".to_owned()],
            ..MediaMtxHostConfig::default()
        };
        let yml = render_mediamtx_yml(&cfg);
        assert!(yml.contains("apiAddress: 127.0.0.1:19997"));
        assert!(yml.contains("hlsAddress: :18888"));
        assert!(yml.contains("recordPath: '/data/rec/%path/%Y-%m-%d_%H-%M-%S-%f'"));
        assert!(yml.contains("recordDeleteAfter: '24h'"));
        assert!(yml.contains("webrtcAdditionalHosts: ['a.example.com', 'b.example.com']"));
    }

    // ── systemd unit rendering ───────────────────────────────────────────────

    #[test]
    fn rendered_unit_uses_binary_and_config_paths_and_restarts_always() {
        let unit = render_mediamtx_unit(&MediaMtxHostConfig::default());
        // Finding T: the binary/config paths are systemd double-quoted so a path
        // with whitespace stays one argument.
        assert!(
            unit.contains("ExecStart=\"/usr/local/bin/mediamtx\" \"/etc/mediamtx/mediamtx.yml\"\n"),
            "ExecStart: {unit}"
        );
        assert!(unit.contains("Restart=always\n"), "restart: {unit}");
        assert!(unit.contains("After=network-online.target\n"));
        assert!(unit.contains("WantedBy=multi-user.target\n"));
    }

    #[test]
    fn rendered_unit_quotes_whitespace_paths_in_execstart() {
        // Finding T (issue #2051): a `binary_path`/`config_path` containing
        // whitespace must be systemd double-quoted in `ExecStart`, or systemd
        // parses `ExecStart=/opt/media mtx/mediamtx ...` as the truncated command
        // `/opt/media` and the deferred post-cutover `systemctl restart` fails.
        let cfg = MediaMtxHostConfig {
            binary_path: "/opt/media mtx/mediamtx".to_owned(),
            config_path: "/etc/media mtx/mediamtx.yml".to_owned(),
            ..MediaMtxHostConfig::default()
        };
        let unit = render_mediamtx_unit(&cfg);
        // Each path is wrapped in systemd double quotes → one argument each.
        assert!(
            unit.contains(
                "ExecStart=\"/opt/media mtx/mediamtx\" \"/etc/media mtx/mediamtx.yml\"\n"
            ),
            "ExecStart must double-quote whitespace paths: {unit}"
        );
        // And NOT the bare, space-splitting form.
        assert!(
            !unit.contains("ExecStart=/opt/media mtx/mediamtx"),
            "ExecStart must not interpolate the path unquoted: {unit}"
        );
    }

    #[test]
    fn systemd_quote_escapes_backslash_and_double_quote() {
        // systemd C-style quoting: `\\` and `\"` are the escapes inside a
        // double-quoted token, escaped in that order so the value round-trips.
        assert_eq!(systemd_quote("/usr/bin/mediamtx"), "\"/usr/bin/mediamtx\"");
        assert_eq!(systemd_quote("/opt/a b/mtx"), "\"/opt/a b/mtx\"");
        assert_eq!(systemd_quote(r#"/a"b"#), "\"/a\\\"b\"");
        assert_eq!(systemd_quote(r"/a\b"), "\"/a\\\\b\"");
    }

    #[test]
    fn rendered_yml_quotes_whitespace_and_special_config_values() {
        // Finding T sweep: config-derived path/duration values are single-quoted
        // YAML scalars, so a recordings dir with a space or a YAML structural char
        // stays a single scalar instead of breaking `mediamtx.yml` parsing.
        let cfg = MediaMtxHostConfig {
            recordings_dir: "/mnt/rec ords".to_owned(),
            record_delete_after: "72h".to_owned(),
            ..MediaMtxHostConfig::default()
        };
        let yml = render_mediamtx_yml(&cfg);
        assert!(
            yml.contains("recordPath: '/mnt/rec ords/%path/%Y-%m-%d_%H-%M-%S-%f'"),
            "recordPath must be single-quoted: {yml}"
        );
        // A `#` in the recordings dir would otherwise start a YAML comment.
        let hashy = MediaMtxHostConfig {
            recordings_dir: "/mnt/rec#1".to_owned(),
            ..MediaMtxHostConfig::default()
        };
        let yml = render_mediamtx_yml(&hashy);
        assert!(
            yml.contains("recordPath: '/mnt/rec#1/%path/%Y-%m-%d_%H-%M-%S-%f'"),
            "a `#` path must be quoted, not truncated by a YAML comment: {yml}"
        );
    }

    #[test]
    fn install_op_quotes_whitespace_unit_name() {
        // Finding T sweep: the config-derived unit name is shell-quoted in every
        // systemctl invocation, so a unit name with whitespace does not split the
        // arguments.
        let cfg = MediaMtxHostConfig {
            enabled: true,
            unit_name: "media mtx".to_owned(),
            ..MediaMtxHostConfig::default()
        };
        let ops = MediaMtxController::new(cfg).ensure_installed_ops();
        let DeployOp::Run(cmd) = &ops[3] else {
            panic!("op 3 must be the install Run op");
        };
        assert!(
            cmd.shell.contains("systemctl enable 'media mtx.service'"),
            "unit name must be shell-quoted: {}",
            cmd.shell
        );
        assert!(
            cmd.shell
                .contains("systemctl is-active --quiet 'media mtx.service'"),
            "{}",
            cmd.shell
        );
        assert!(
            cmd.shell.contains("systemctl restart 'media mtx.service'"),
            "{}",
            cmd.shell
        );
    }

    // ── Controller op sequence ───────────────────────────────────────────────

    #[test]
    fn controller_noops_when_disabled() {
        let controller = MediaMtxController::new(MediaMtxHostConfig::default());
        assert!(
            controller.ensure_installed_ops().is_empty(),
            "a disabled controller emits no ops"
        );
    }

    #[test]
    fn controller_emits_write_write_install_when_enabled() {
        let cfg = MediaMtxHostConfig {
            enabled: true,
            ..MediaMtxHostConfig::default()
        };
        let controller = MediaMtxController::new(cfg);
        let ops = controller.ensure_installed_ops();
        assert_eq!(ops.len(), 4);

        // op 0: mkdir -p the config's parent dir BEFORE scp writes into it.
        let DeployOp::Run(mkdir) = &ops[0] else {
            panic!("op 0 must be the mkdir Run op, got {:?}", ops[0]);
        };
        assert_eq!(mkdir.label, "media-prepare-dirs");
        assert_eq!(mkdir.shell, "mkdir -p '/etc/mediamtx'");

        // Finding H: op 1 writes mediamtx.yml to a TEMP path (never over the live
        // file), so op 3 can diff it and restart only on a real change.
        match &ops[1] {
            DeployOp::WriteFile {
                label,
                contents,
                remote_path,
                mode,
            } => {
                assert_eq!(*label, "media-write-config");
                assert_eq!(remote_path, "/etc/mediamtx/mediamtx.yml.autumn-new");
                assert_eq!(*mode, Some(0o644));
                let FileContents::Plain(yml) = contents else {
                    panic!("config must be plain text");
                };
                assert!(yml.contains("~^room/.+$:"));
            }
            other => panic!("op 1 must write the config, got {other:?}"),
        }

        // op 2: write the unit to a TEMP path next to the live unit (0644).
        match &ops[2] {
            DeployOp::WriteFile {
                label,
                contents,
                remote_path,
                mode,
            } => {
                assert_eq!(*label, "media-write-unit");
                assert_eq!(
                    remote_path,
                    "/etc/systemd/system/mediamtx.service.autumn-new"
                );
                assert_eq!(*mode, Some(0o644));
                let FileContents::Plain(unit) = contents else {
                    panic!("unit must be plain text");
                };
                assert!(unit.contains("ExecStart=\"/usr/local/bin/mediamtx\""));
            }
            other => panic!("op 2 must write the unit, got {other:?}"),
        }

        // op 3 (the conditional-restart install) is asserted in
        // `controller_install_op_is_conditional_restart` below.
        assert!(matches!(&ops[3], DeployOp::Run(c) if c.label == "media-install"));
    }

    #[test]
    fn controller_install_op_is_conditional_restart() {
        // Finding H: op 3 must `cmp` the staged temp files against the live ones,
        // copy only the changed, and restart the unit ONLY on a real change or an
        // inactive unit — never unconditionally (which would kill live sessions on
        // a pure app redeploy).
        let cfg = MediaMtxHostConfig {
            enabled: true,
            ..MediaMtxHostConfig::default()
        };
        let ops = MediaMtxController::new(cfg).ensure_installed_ops();
        let DeployOp::Run(cmd) = &ops[3] else {
            panic!("op 3 must be the install Run op");
        };
        assert_eq!(cmd.label, "media-install");
        let shell = &cmd.shell;
        // Diffs the staged temp files against the live counterparts.
        assert!(
            shell.contains(
                "cmp -s '/etc/mediamtx/mediamtx.yml.autumn-new' '/etc/mediamtx/mediamtx.yml'"
            ),
            "must cmp the staged config against the live config: {shell}"
        );
        assert!(
            shell.contains(
                "cmp -s '/etc/systemd/system/mediamtx.service.autumn-new' \
                 '/etc/systemd/system/mediamtx.service'"
            ),
            "must cmp the staged unit against the live unit: {shell}"
        );
        // Copies over only the changed files and tracks a `changed` flag.
        assert!(
            shell.contains("changed=1"),
            "must track a changed flag: {shell}"
        );
        assert!(
            shell.contains(
                "cp '/etc/mediamtx/mediamtx.yml.autumn-new' '/etc/mediamtx/mediamtx.yml'"
            ),
            "must copy the staged config into place on change: {shell}"
        );
        // Cleans up the temp files.
        assert!(
            shell.contains(
                "rm -f '/etc/mediamtx/mediamtx.yml.autumn-new' \
                 '/etc/systemd/system/mediamtx.service.autumn-new'"
            ),
            "must clean up the staged temp files: {shell}"
        );
        // Idempotent daemon-reload + enable (NOT `enable --now` — start is handled
        // by the conditional restart).
        assert!(shell.contains("systemctl daemon-reload"), "{shell}");
        // The unit id is shell-quoted (`'mediamtx.service'`, Finding T sweep) in
        // every systemctl invocation.
        assert!(
            shell.contains("systemctl enable 'mediamtx.service'"),
            "{shell}"
        );
        assert!(
            !shell.contains("enable --now"),
            "must not `enable --now` (that would start unconditionally): {shell}"
        );
        // Restart gated on change-OR-inactive, never unconditional.
        assert!(
            shell.contains(
                "if [ \"$changed\" -ne 0 ] || ! systemctl is-active --quiet 'mediamtx.service'; then"
            ),
            "restart must be gated on change or an inactive unit: {shell}"
        );
        assert!(
            shell.contains("systemctl restart 'mediamtx.service'"),
            "{shell}"
        );
    }

    #[test]
    fn controller_honors_custom_unit_name_in_paths_and_command() {
        let cfg = MediaMtxHostConfig {
            enabled: true,
            unit_name: "mediamtx-prod".to_owned(),
            ..MediaMtxHostConfig::default()
        };
        let controller = MediaMtxController::new(cfg);
        assert_eq!(
            controller.unit_path(),
            "/etc/systemd/system/mediamtx-prod.service"
        );
        let ops = controller.ensure_installed_ops();
        let DeployOp::Run(cmd) = &ops[3] else {
            panic!("op 3 must be the install Run op");
        };
        assert!(
            cmd.shell
                .contains("systemctl enable 'mediamtx-prod.service'")
        );
        assert!(
            cmd.shell
                .contains("is-active --quiet 'mediamtx-prod.service'")
        );
        assert!(cmd.shell.contains("restart 'mediamtx-prod.service'"));
        // The staged temp unit tracks the custom unit name too.
        assert!(
            cmd.shell
                .contains("/etc/systemd/system/mediamtx-prod.service.autumn-new")
        );
    }

    #[test]
    fn controller_mkdirs_config_parent_before_writing_config() {
        // Finding B: `scp` never creates parent dirs, so a `mkdir -p` of the
        // config's parent must precede the `media-write-config` upload.
        let cfg = MediaMtxHostConfig {
            enabled: true,
            config_path: "/opt/media/etc/mediamtx.yml".to_owned(),
            ..MediaMtxHostConfig::default()
        };
        let ops = MediaMtxController::new(cfg).ensure_installed_ops();

        let mkdir_idx = ops
            .iter()
            .position(|op| op.label() == "media-prepare-dirs")
            .expect("a media-prepare-dirs op must be emitted");
        let write_idx = ops
            .iter()
            .position(|op| op.label() == "media-write-config")
            .expect("a media-write-config op must be emitted");
        assert!(
            mkdir_idx < write_idx,
            "mkdir must precede the config write (mkdir at {mkdir_idx}, write at {write_idx})"
        );

        let DeployOp::Run(mkdir) = &ops[mkdir_idx] else {
            panic!("media-prepare-dirs must be a Run op");
        };
        // The parent is derived from `config_path`, not hardcoded (shell-quoted).
        assert_eq!(mkdir.shell, "mkdir -p '/opt/media/etc'");
    }

    #[test]
    fn parent_dir_extracts_parent_or_none() {
        assert_eq!(
            parent_dir("/etc/mediamtx/mediamtx.yml"),
            Some("/etc/mediamtx")
        );
        assert_eq!(parent_dir("/opt/a/b/c.yml"), Some("/opt/a/b"));
        // Root-level and bare-filename paths have no parent to create.
        assert_eq!(parent_dir("/mediamtx.yml"), None);
        assert_eq!(parent_dir("mediamtx.yml"), None);
    }

    // ── Doctor: ffmpeg preflight ─────────────────────────────────────────────

    #[test]
    fn ffmpeg_preflight_passes_on_version_banner() {
        let exec = RecordingExecutor::new().with_stdout(
            "media-ffmpeg-preflight",
            "ffmpeg version 6.1.1 Copyright (c)",
        );
        let check = ffmpeg_preflight(&exec, "/usr/bin/ffmpeg");
        assert!(check.passed, "detail: {}", check.detail);
        assert_eq!(check.name, CHECK_FFMPEG_PREFLIGHT);
        assert_eq!(exec.labels(), vec!["media-ffmpeg-preflight"]);
    }

    #[test]
    fn ffmpeg_preflight_fails_without_banner() {
        // A binary that runs but is not FFmpeg (no banner) fails — fail-closed.
        let exec =
            RecordingExecutor::new().with_stdout("media-ffmpeg-preflight", "not-ffmpeg output");
        let check = ffmpeg_preflight(&exec, "/usr/bin/ffmpeg");
        assert!(!check.passed);
        // A concrete literal that is not FFmpeg is a BLOCKING failure (not
        // deferred) — it must still abort the deploy (Finding U, #2051).
        assert!(!check.deferred);
        assert!(check.blocking());
        assert!(check.hint.is_some());
    }

    #[test]
    fn ffmpeg_preflight_fails_when_binary_missing() {
        // A non-zero exit (missing / not executable) surfaces as a transport
        // error — fail-closed, not a pass.
        let exec = RecordingExecutor::failing_on("media-ffmpeg-preflight");
        let check = ffmpeg_preflight(&exec, "/usr/bin/ffmpeg");
        assert!(!check.passed);
        // A missing concrete literal is a BLOCKING failure, not deferred.
        assert!(!check.deferred);
        assert!(check.blocking());
        assert!(check.detail.contains("could not run"));
    }

    // ── Finding L: verify only a concrete literal; defer env-indirected paths ──
    //
    // The deploy side must NOT resolve `[media.ffmpeg] bin` against the operator's
    // LOCAL shell — the generated service env-file does not propagate it, so the
    // service would resolve a different path. Only an environment-independent
    // concrete literal is probed; anything env/interpolation-indirected (a
    // `${...}` placeholder — including `${AUTUMN_MEDIA__FFMPEG__BIN}` — or an empty
    // value) is deferred to runtime with NO probe, regardless of any local env.

    #[test]
    fn ffmpeg_preflight_defers_env_indirected_paths_without_probing() {
        // A whole-string `${VAR}`, an embedded placeholder, a value that relies on
        // the AUTUMN_MEDIA__FFMPEG__BIN override, and an empty value all DEFER — a
        // clear non-passing outcome, and crucially NO probe runs.
        for configured in [
            "${FF_BIN_UNSET_XYZ}",
            "/opt/${VER}/ffmpeg",
            // A value that relies on the AUTUMN_MEDIA__FFMPEG__BIN override — the
            // service resolves it from ITS own env; the deploy side must defer.
            "${AUTUMN_MEDIA__FFMPEG__BIN}",
            "",
        ] {
            let exec = RecordingExecutor::new();
            let check = ffmpeg_preflight(&exec, configured);
            assert!(!check.passed, "must be non-passing for `{configured}`");
            // Deferred, not failed: the service resolves FFmpeg from its own
            // runtime env, so this must NOT abort the deploy (Finding U, #2051).
            assert!(check.deferred, "must be deferred for `{configured}`");
            assert!(
                !check.blocking(),
                "deferred check must be non-blocking for `{configured}`"
            );
            assert!(
                check.detail.contains("indirection") && check.detail.contains("deferring"),
                "detail must name the deferral for `{configured}`: {}",
                check.detail
            );
            assert!(check.hint.is_some());
            // Fail-closed-honest: never probed a locally-resolved / placeholder path.
            assert!(
                exec.labels().is_empty(),
                "no probe must run for `{configured}`, ran {:?}",
                exec.labels()
            );
        }
    }

    #[test]
    fn ffmpeg_preflight_probes_a_concrete_literal_path() {
        // A concrete (non-placeholder) literal path IS probed verbatim — the
        // healthy, environment-independent path.
        let exec = RecordingExecutor::new()
            .with_stdout("media-ffmpeg-preflight", "ffmpeg version 6.1.1 Copyright");
        let check = ffmpeg_preflight(&exec, "/usr/bin/ffmpeg");
        assert!(check.passed, "detail: {}", check.detail);
        assert_eq!(exec.labels(), vec!["media-ffmpeg-preflight"]);
    }

    // ── Doctor: MediaMTX binary preflight (Finding N) ────────────────────────
    //
    // Provisioning (`systemctl … restart`) is deferred until AFTER the app
    // cutover commits, so a host missing the MediaMTX binary must be caught by a
    // non-mutating `test -x <binary_path>` check BEFORE cutover — otherwise the
    // app commits and the deferred restart then fails on the missing `ExecStart`.

    #[test]
    fn mediamtx_binary_preflight_passes_when_executable() {
        // `test -x` exits zero → Ok → pass.
        let exec = RecordingExecutor::new();
        let check = mediamtx_binary_preflight(&exec, &MediaMtxHostConfig::default());
        assert!(check.passed, "detail: {}", check.detail);
        assert_eq!(check.name, CHECK_MEDIAMTX_BINARY);
        assert_eq!(exec.labels(), vec!["media-mediamtx-binary"]);
        // Non-mutating: it only runs `test -x`, never a mutating command.
        assert!(check.hint.is_none());
    }

    #[test]
    fn mediamtx_binary_preflight_fails_when_missing_or_not_executable() {
        // A missing / non-executable binary makes `test -x` exit non-zero → the
        // fake surfaces a transport error → fail-closed, naming binary_path and
        // the deferred-restart consequence.
        let exec = RecordingExecutor::failing_on("media-mediamtx-binary");
        let cfg = MediaMtxHostConfig::default();
        let check = mediamtx_binary_preflight(&exec, &cfg);
        assert!(!check.passed);
        assert_eq!(check.name, CHECK_MEDIAMTX_BINARY);
        assert!(
            check.detail.contains(&cfg.binary_path),
            "detail must name binary_path: {}",
            check.detail
        );
        assert!(
            check.detail.contains("systemctl restart"),
            "detail must state the post-cutover restart would fail: {}",
            check.detail
        );
        assert!(check.hint.is_some());
    }

    #[test]
    fn mediamtx_binary_preflight_uses_test_x_and_shell_quotes_the_path() {
        // Verify the exact non-mutating command shape (shell-quoted binary_path).
        struct CaptureExec {
            cmd: RefCell<Option<String>>,
        }
        impl DeployExecutor for CaptureExec {
            fn run(&self, cmd: &RemoteCommand) -> Result<CommandOutput, DeployExecError> {
                *self.cmd.borrow_mut() = Some(cmd.shell.clone());
                Ok(CommandOutput {
                    stdout: String::new(),
                    stderr: String::new(),
                })
            }
            fn upload(
                &self,
                _local: &Path,
                _remote_path: &str,
                _mode: Option<u32>,
            ) -> Result<(), DeployExecError> {
                Ok(())
            }
        }
        let exec = CaptureExec {
            cmd: RefCell::new(None),
        };
        let cfg = MediaMtxHostConfig {
            binary_path: "/opt/media mtx/mediamtx".to_owned(),
            ..MediaMtxHostConfig::default()
        };
        let _ = mediamtx_binary_preflight(&exec, &cfg);
        let cmd = exec.cmd.borrow().clone().expect("command ran");
        assert!(cmd.starts_with("test -x "), "must be `test -x`: {cmd}");
        // Path with a space must be shell-quoted so it stays one argument.
        assert!(
            cmd.contains("'/opt/media mtx/mediamtx'"),
            "binary_path must be shell-quoted: {cmd}"
        );
    }

    // ── Doctor: recordings dir writable ──────────────────────────────────────

    #[test]
    fn recordings_dir_passes_when_writable() {
        let exec = RecordingExecutor::new();
        let check = recordings_dir_writable(&exec, &MediaMtxHostConfig::default());
        assert!(check.passed, "detail: {}", check.detail);
        assert_eq!(check.name, CHECK_RECORDINGS_DIR_WRITABLE);
        assert_eq!(exec.labels(), vec!["media-recordings-dir"]);
    }

    #[test]
    fn recordings_dir_fails_when_not_writable() {
        // `test -d && test -w` exits non-zero → transport error → fail-closed.
        let exec = RecordingExecutor::failing_on("media-recordings-dir");
        let check = recordings_dir_writable(&exec, &MediaMtxHostConfig::default());
        assert!(!check.passed);
        assert!(check.detail.contains("/recordings"));
        assert!(check.hint.is_some());
    }

    // ── Doctor: mediamtx ports available ─────────────────────────────────────

    #[test]
    fn parse_listening_sockets_extracts_port_protocol_and_owner() {
        // Finding I: the Netid (tcp/udp) column is parsed onto each socket.
        let ss = "\
tcp   LISTEN 0 128  0.0.0.0:8888  0.0.0.0:*  users:((\"mediamtx\",pid=10,fd=5))
tcp   LISTEN 0 128  [::]:22       [::]:*     users:((\"sshd\",pid=5,fd=3))
udp   UNCONN 0 0    *:8189        *:*
sctp  LISTEN 0 128  0.0.0.0:9999  0.0.0.0:*
";
        let sockets = parse_listening_sockets(ss);
        assert!(sockets.iter().any(|s| s.port == 8888
            && s.protocol == Protocol::Tcp
            && s.owner.as_deref() == Some("mediamtx")));
        assert!(sockets.iter().any(|s| s.port == 22
            && s.protocol == Protocol::Tcp
            && s.owner.as_deref() == Some("sshd")));
        // The UDP row has no `users:` column → unattributable owner, protocol UDP.
        assert!(
            sockets
                .iter()
                .any(|s| s.port == 8189 && s.protocol == Protocol::Udp && s.owner.is_none())
        );
        // A non-tcp/udp Netid (e.g. `sctp`) is skipped entirely.
        assert_eq!(sockets.len(), 3);
    }

    #[test]
    fn ports_available_passes_when_ports_free() {
        // ss shows only sshd — none of MediaMTX's ports are taken.
        let exec = RecordingExecutor::new().with_stdout(
            "media-ports",
            "tcp LISTEN 0 128 0.0.0.0:22 0.0.0.0:* users:((\"sshd\",pid=5,fd=3))\n",
        );
        let check = mediamtx_ports_available(&exec, &MediaMtxHostConfig::default());
        assert!(check.passed, "detail: {}", check.detail);
        assert_eq!(check.name, CHECK_MEDIAMTX_PORTS_AVAILABLE);
        assert!(check.detail.contains("free"), "detail: {}", check.detail);
    }

    #[test]
    fn ports_available_fails_when_foreign_process_holds_port() {
        // Finding D (b): a foreign process (nginx) is bound to the HLS port (8888).
        let exec = RecordingExecutor::new().with_stdout(
            "media-ports",
            "tcp LISTEN 0 128 0.0.0.0:22 0.0.0.0:* users:((\"sshd\",pid=5,fd=3))\n\
             tcp LISTEN 0 128 0.0.0.0:8888 0.0.0.0:* users:((\"nginx\",pid=99,fd=6))\n",
        );
        let check = mediamtx_ports_available(&exec, &MediaMtxHostConfig::default());
        assert!(!check.passed, "detail: {}", check.detail);
        assert!(check.detail.contains("8888"), "detail: {}", check.detail);
        assert!(check.detail.contains("another service"));
        assert!(check.hint.is_some());
    }

    #[test]
    fn ports_available_passes_when_our_mediamtx_owns_all_ports() {
        // Finding D (a): every required port is held by OUR managed mediamtx (a
        // same-port redeploy) — not a conflict, so the check passes and never
        // aborts the 2nd+ `deploy up`.
        let exec = RecordingExecutor::new().with_stdout(
            "media-ports",
            "tcp LISTEN 0 128 0.0.0.0:9997 0.0.0.0:* users:((\"mediamtx\",pid=10,fd=3))\n\
             tcp LISTEN 0 128 0.0.0.0:1935 0.0.0.0:* users:((\"mediamtx\",pid=10,fd=4))\n\
             tcp LISTEN 0 128 0.0.0.0:8888 0.0.0.0:* users:((\"mediamtx\",pid=10,fd=5))\n\
             tcp LISTEN 0 128 0.0.0.0:8889 0.0.0.0:* users:((\"mediamtx\",pid=10,fd=6))\n\
             tcp LISTEN 0 128 0.0.0.0:9996 0.0.0.0:* users:((\"mediamtx\",pid=10,fd=7))\n\
             udp UNCONN 0 0 0.0.0.0:8189 0.0.0.0:* users:((\"mediamtx\",pid=10,fd=8))\n",
        );
        let check = mediamtx_ports_available(&exec, &MediaMtxHostConfig::default());
        assert!(check.passed, "detail: {}", check.detail);
        assert!(
            check.detail.contains("our own managed"),
            "detail: {}",
            check.detail
        );
        // Finding J: because MediaMTX-owned ports were found, the is-active gate
        // runs exactly once after the `ss` scan (the fake reports the unit active
        // by default), so the same-port redeploy passes.
        assert_eq!(exec.labels(), vec!["media-ports", "media-unit-active"]);
    }

    #[test]
    fn ports_available_fails_when_changed_port_held_by_foreign_service() {
        // Finding D core: a redeploy that MOVED the HLS port to 8080, where a
        // foreign service is already bound. The old managed unit only held the old
        // ports, so a process-aware scan of the NEW target port must still fail —
        // exactly the case the removed is-active skip missed.
        let cfg = MediaMtxHostConfig {
            hls_port: 8080,
            ..MediaMtxHostConfig::default()
        };
        let exec = RecordingExecutor::new().with_stdout(
            "media-ports",
            // Our mediamtx still holds its OTHER (unchanged) ports, but the
            // newly-configured HLS port 8080 is held by a foreign nginx.
            "tcp LISTEN 0 128 0.0.0.0:9997 0.0.0.0:* users:((\"mediamtx\",pid=10,fd=3))\n\
             tcp LISTEN 0 128 0.0.0.0:1935 0.0.0.0:* users:((\"mediamtx\",pid=10,fd=4))\n\
             tcp LISTEN 0 128 0.0.0.0:8080 0.0.0.0:* users:((\"nginx\",pid=99,fd=6))\n",
        );
        let check = mediamtx_ports_available(&exec, &cfg);
        assert!(!check.passed, "detail: {}", check.detail);
        assert!(check.detail.contains("8080"), "detail: {}", check.detail);
    }

    #[test]
    fn ports_available_ignores_same_port_different_protocol_listener() {
        // Finding I: the WebRTC local port 8189 is UDP-only. A TCP service on 8189
        // is a DIFFERENT protocol, so it must NOT be flagged as a conflict.
        let exec = RecordingExecutor::new().with_stdout(
            "media-ports",
            "tcp LISTEN 0 128 0.0.0.0:22 0.0.0.0:* users:((\"sshd\",pid=5,fd=3))\n\
             tcp LISTEN 0 128 0.0.0.0:8189 0.0.0.0:* users:((\"someapp\",pid=77,fd=9))\n",
        );
        let check = mediamtx_ports_available(&exec, &MediaMtxHostConfig::default());
        assert!(check.passed, "detail: {}", check.detail);
        assert!(check.detail.contains("free"), "detail: {}", check.detail);

        // The mirror case: a UDP service on a TCP MediaMTX port (8888 is TCP) is
        // also a different protocol and not a conflict.
        let exec = RecordingExecutor::new().with_stdout(
            "media-ports",
            "udp UNCONN 0 0 0.0.0.0:8888 0.0.0.0:* users:((\"someapp\",pid=88,fd=4))\n",
        );
        let check = mediamtx_ports_available(&exec, &MediaMtxHostConfig::default());
        assert!(check.passed, "detail: {}", check.detail);
    }

    #[test]
    fn ports_available_fails_on_same_protocol_foreign_listener() {
        // Finding I: a TCP listener on the TCP HLS port (8888), owned by a foreign
        // process, IS a same-protocol conflict.
        let exec = RecordingExecutor::new().with_stdout(
            "media-ports",
            "tcp LISTEN 0 128 0.0.0.0:8888 0.0.0.0:* users:((\"nginx\",pid=99,fd=6))\n",
        );
        let check = mediamtx_ports_available(&exec, &MediaMtxHostConfig::default());
        assert!(!check.passed, "detail: {}", check.detail);
        assert!(check.detail.contains("8888"), "detail: {}", check.detail);
        assert!(check.detail.contains("another service"));
    }

    #[test]
    fn ports_available_fails_on_udp_foreign_listener_on_udp_target() {
        // Finding I: a UDP foreign listener on the UDP-target 8189 IS a conflict.
        let exec = RecordingExecutor::new().with_stdout(
            "media-ports",
            "udp UNCONN 0 0 0.0.0.0:8189 0.0.0.0:* users:((\"coturn\",pid=42,fd=8))\n",
        );
        let check = mediamtx_ports_available(&exec, &MediaMtxHostConfig::default());
        assert!(!check.passed, "detail: {}", check.detail);
        assert!(check.detail.contains("8189"), "detail: {}", check.detail);
    }

    #[test]
    fn ports_available_fails_closed_when_mediamtx_owns_ports_but_unit_inactive() {
        // Finding J: the ports are held by a `mediamtx` process, but OUR configured
        // unit is NOT active (manual run / differently-named service / crashed).
        // Starting our unit would launch a second instance over them → fail-closed.
        let exec = RecordingExecutor::new()
            .with_stdout(
                "media-ports",
                "tcp LISTEN 0 128 0.0.0.0:9997 0.0.0.0:* users:((\"mediamtx\",pid=10,fd=3))\n\
                 tcp LISTEN 0 128 0.0.0.0:8888 0.0.0.0:* users:((\"mediamtx\",pid=10,fd=5))\n",
            )
            // `systemctl is-active --quiet` exits non-zero → transport error →
            // read as NOT active.
            .failing("media-unit-active");
        let check = mediamtx_ports_available(&exec, &MediaMtxHostConfig::default());
        assert!(!check.passed, "detail: {}", check.detail);
        assert!(check.detail.contains("8888"), "detail: {}", check.detail);
        assert!(
            check.detail.contains("not active"),
            "detail: {}",
            check.detail
        );
        // The is-active gate ran exactly once, after the single ss scan.
        assert_eq!(exec.labels(), vec!["media-ports", "media-unit-active"]);
    }

    #[test]
    fn ports_available_does_not_query_is_active_when_no_mediamtx_port_held() {
        // Finding J: the is-active gate runs at most once, and only when a
        // MediaMTX-owned port is actually found. A foreign-only conflict (or a
        // free host) never queries it.
        let exec = RecordingExecutor::new().with_stdout(
            "media-ports",
            "tcp LISTEN 0 128 0.0.0.0:8888 0.0.0.0:* users:((\"nginx\",pid=99,fd=6))\n",
        );
        let check = mediamtx_ports_available(&exec, &MediaMtxHostConfig::default());
        assert!(!check.passed);
        assert_eq!(exec.labels(), vec!["media-ports"]);
    }

    #[test]
    fn ports_available_fails_closed_on_unattributable_listener() {
        // Finding D (e): a listener on a required port whose owner cannot be
        // attributed (no `users:` column — e.g. insufficient privilege) is treated
        // as a conflict, not assumed benign. Deploy runs as root so this is rare,
        // but fail-closed is the safe default.
        let exec = RecordingExecutor::new().with_stdout(
            "media-ports",
            "tcp LISTEN 0 128 0.0.0.0:22 0.0.0.0:* users:((\"sshd\",pid=5,fd=3))\n\
             tcp LISTEN 0 128 0.0.0.0:8888 0.0.0.0:*\n",
        );
        let check = mediamtx_ports_available(&exec, &MediaMtxHostConfig::default());
        assert!(!check.passed, "detail: {}", check.detail);
        assert!(check.detail.contains("8888"), "detail: {}", check.detail);
    }

    #[test]
    fn ports_available_fails_closed_when_ss_errors() {
        let exec = RecordingExecutor::failing_on("media-ports");
        let check = mediamtx_ports_available(&exec, &MediaMtxHostConfig::default());
        assert!(!check.passed);
        assert!(check.detail.contains("could not verify"));
    }

    #[test]
    fn ports_available_fails_closed_on_unparseable_output() {
        // A non-empty ss body that yields zero ports is unverifiable, not a pass.
        let exec =
            RecordingExecutor::new().with_stdout("media-ports", "garbage that has no ports\n");
        let check = mediamtx_ports_available(&exec, &MediaMtxHostConfig::default());
        assert!(!check.passed);
        assert!(check.detail.contains("could not verify"));
    }

    #[test]
    fn collect_runs_all_five_checks_in_order() {
        let exec = RecordingExecutor::new()
            .with_stdout("media-ffmpeg-preflight", "ffmpeg version 6.1")
            .with_stdout(
                "media-ports",
                "tcp LISTEN 0 128 0.0.0.0:22 0.0.0.0:* users:((\"sshd\",pid=5,fd=3))\n",
            );
        let checks =
            collect_media_doctor_checks(&exec, &MediaMtxHostConfig::default(), "/usr/bin/ffmpeg");
        assert_eq!(checks.len(), 5);
        assert!(checks.iter().all(|c| c.passed), "all should pass");
        // The pure ports-distinct config check (Finding P) leads and runs NO
        // command, so the recorded command order is unchanged from the four
        // executor-driven checks.
        assert_eq!(checks[0].name, CHECK_MEDIAMTX_PORTS_DISTINCT);
        assert_eq!(
            exec.labels(),
            vec![
                "media-ffmpeg-preflight",
                // The MediaMTX binary preflight (Finding N) runs before cutover,
                // symmetric with the FFmpeg preflight, so a host missing the
                // deferred-provisioned binary fails fast.
                "media-mediamtx-binary",
                "media-recordings-dir",
                // The ports check is a single process-aware `ss -tulnp` scan; the
                // old `systemctl is-active` gate was removed (Finding D).
                "media-ports"
            ]
        );
    }

    #[test]
    fn collect_with_env_indirected_ffmpeg_has_no_blocking_failures() {
        // Finding U (#2051): an env/interpolation-indirected `[media.ffmpeg] bin`
        // is DEFERRED to the service runtime — non-passing but non-blocking — so
        // the aggregate has zero blocking checks and `deploy up` proceeds (the
        // deploy gate counts `blocking()`, not `!passed`). A concrete missing
        // FFmpeg would instead block.
        let exec = RecordingExecutor::new().with_stdout(
            "media-ports",
            "tcp LISTEN 0 128 0.0.0.0:22 0.0.0.0:* users:((\"sshd\",pid=5,fd=3))\n",
        );
        let checks = collect_media_doctor_checks(
            &exec,
            &MediaMtxHostConfig::default(),
            "${AUTUMN_MEDIA__FFMPEG__BIN}",
        );
        // The ffmpeg check is present, deferred, and non-passing …
        let ffmpeg = checks
            .iter()
            .find(|c| c.name == CHECK_FFMPEG_PREFLIGHT)
            .expect("ffmpeg check present");
        assert!(ffmpeg.deferred && !ffmpeg.passed);
        // … and no probe ran for it (no ffmpeg-preflight label recorded).
        assert!(!exec.labels().contains(&"media-ffmpeg-preflight"));
        // The deploy gate keys on `blocking()`: the deferred ffmpeg does NOT
        // count, so the aggregate is non-blocking and the deploy is not aborted.
        assert_eq!(
            checks.iter().filter(|c| c.blocking()).count(),
            0,
            "deferred ffmpeg must not block the deploy"
        );
    }

    // ── Distinct listener ports (Finding P) ──────────────────────────────────

    #[test]
    fn ports_distinct_passes_for_default_config() {
        let check = mediamtx_ports_distinct(&MediaMtxHostConfig::default());
        assert!(
            check.passed,
            "default ports are all distinct: {}",
            check.detail
        );
        assert_eq!(check.name, CHECK_MEDIAMTX_PORTS_DISTINCT);
    }

    #[test]
    fn ports_distinct_fails_when_two_tcp_listeners_share_a_port() {
        // hls_port == playback_port: two MediaMTX servers asked to bind one TCP
        // port. `ss` passes on a fresh host, but the deferred restart fails.
        let cfg = MediaMtxHostConfig {
            hls_port: 8888,
            playback_port: 8888,
            ..MediaMtxHostConfig::default()
        };
        let check = mediamtx_ports_distinct(&cfg);
        assert!(!check.passed);
        // Names the duplicated value and BOTH conflicting fields.
        assert!(check.detail.contains("8888"), "detail: {}", check.detail);
        assert!(
            check.detail.contains("hls_port"),
            "detail: {}",
            check.detail
        );
        assert!(
            check.detail.contains("playback_port"),
            "detail: {}",
            check.detail
        );
        assert!(check.hint.is_some());
    }

    #[test]
    fn ports_distinct_allows_tcp_port_equal_to_udp_port() {
        // The WebRTC local UDP port (8189) is a separate protocol namespace, so a
        // TCP listener numerically equal to it is NOT a conflict.
        let cfg = MediaMtxHostConfig {
            rtmp_port: MEDIAMTX_WEBRTC_LOCAL_UDP_PORT,
            ..MediaMtxHostConfig::default()
        };
        assert_eq!(cfg.rtmp_port, cfg.webrtc_local_udp);
        let check = mediamtx_ports_distinct(&cfg);
        assert!(
            check.passed,
            "a TCP port equal to the UDP port is not a same-protocol conflict: {}",
            check.detail
        );
    }

    // ── media_host_config_from_value (Finding O merge target) ────────────────

    #[test]
    fn media_host_config_from_value_reads_merged_media_subtree() {
        let root: toml::Value = toml::from_str(
            "[media.mediamtx]\nenabled = true\napi_port = 19997\n[media.ffmpeg]\nbin = \"/opt/ff\"\n",
        )
        .expect("parses");
        let (cfg, bin) = media_host_config_from_value(root).expect("deserializes");
        assert!(cfg.enabled);
        assert_eq!(cfg.api_port, 19997);
        assert_eq!(bin, "/opt/ff");
    }

    #[test]
    fn media_host_config_from_value_absent_media_is_disabled_default() {
        let root: toml::Value = toml::from_str("[deploy]\nhost = \"h\"\n").expect("parses");
        let (cfg, bin) = media_host_config_from_value(root).expect("deserializes");
        assert!(!cfg.enabled);
        assert_eq!(bin, DEFAULT_FFMPEG_BIN);
    }

    #[test]
    fn media_host_config_from_value_fails_closed_on_ill_typed_value() {
        let root: toml::Value =
            toml::from_str("[media.mediamtx]\nenabled = \"yes\"\n").expect("parses");
        assert!(media_host_config_from_value(root).is_err());
    }

    // ── CSP origins ──────────────────────────────────────────────────────────

    #[test]
    fn required_csp_origins_lists_browser_facing_ports() {
        let origins = MediaMtxHostConfig::default().required_csp_origins();
        assert!(
            origins.contains(&"http://127.0.0.1:8889".to_owned()),
            "webrtc"
        );
        assert!(origins.contains(&"http://127.0.0.1:8888".to_owned()), "hls");
        assert!(
            origins.contains(&"http://127.0.0.1:9996".to_owned()),
            "playback"
        );
        // The control API (9997) is server-side only and must NOT be advertised.
        assert!(!origins.iter().any(|o| o.contains("9997")));
    }
}
