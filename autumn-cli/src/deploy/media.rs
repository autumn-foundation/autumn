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
//!   [`DeployExecutor`](super::exec::DeployExecutor), so the remote command
//!   sequence is assertable against a recording fake with no live host. It
//!   no-ops (empty op vector) when the section is not `enabled`.
//! - Three fail-closed doctor checks — [`ffmpeg_preflight`],
//!   [`recordings_dir_writable`], and [`mediamtx_ports_available`] — run over the
//!   same executor and return the shared [`PreflightCheck`] type, so a media host
//!   can be graded the same way `autumn deploy check` grades the app host. An
//!   unverifiable result (transport error, unparseable output) reports a clear
//!   failure — never a silent pass.
//!
//! ## `FFmpeg` path resolution (decoupled from the plugin)
//!
//! `[media.ffmpeg] bin` may be written as a `${VAR}` placeholder or overridden by
//! the `AUTUMN_MEDIA__FFMPEG__BIN` environment variable — the plugin resolves both
//! before it ever shells out to `FFmpeg`. So the deploy-side preflight must probe
//! the binary the app will *actually* run, not the raw TOML literal.
//! [`resolve_ffmpeg_bin`] mirrors the plugin's resolution: the
//! `AUTUMN_MEDIA__FFMPEG__BIN` process-env override wins over the configured
//! value, and a whole-string `${VAR}` value is interpolated from the process env
//! (unset ⇒ empty). It is a deliberately decoupled re-implementation — this crate
//! takes **no** `autumn-media-plugin` dependency — so it matches only the plugin's
//! *whole-string* `${VAR}` rule; an *embedded* placeholder (`/opt/${VER}/ffmpeg`)
//! is left literal and therefore still contains `${`.
//!
//! **Fail-closed-honest.** When resolution leaves the path empty or still
//! containing an unresolved `${`, [`ffmpeg_preflight`] does **not** probe a literal
//! placeholder or a wrong default — it returns a clear non-passing outcome
//! ("ffmpeg path unresolved (env/interpolation indirection); deferring
//! verification to runtime") so the operator sees the indirection rather than a
//! false pass/fail against the wrong binary. Known limitation of the decoupled
//! resolver: an embedded `${...}` the plugin *could* resolve is reported as
//! deferred here (the plugin only interpolates whole-string placeholders, so full
//! parity holds for the whole-string form).
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
/// out of the raw `autumn.toml` string via [`MediaMtxHostConfig::from_toml_str`]
/// (mirroring how the plugin reads `[media]`), so it never touches
/// `autumn-web`'s strict `AutumnConfig` schema — no `autumn-media-plugin`
/// dependency and no change to the app config type.
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

/// Read the `[media.ffmpeg] bin` path out of a raw `autumn.toml` string,
/// defaulting to [`DEFAULT_FFMPEG_BIN`] when the section (or the whole file) is
/// absent. Companion to [`MediaMtxHostConfig::from_toml_str`] so the deploy
/// `FFmpeg` preflight checks the same binary the app resolves.
///
/// # Errors
///
/// Returns the [`toml::de::Error`] when the string does not parse.
pub fn ffmpeg_bin_from_toml_str(toml_str: &str) -> Result<String, toml::de::Error> {
    let root: MediaTomlRoot = toml::from_str(toml_str)?;
    Ok(root.media.ffmpeg.bin)
}

impl MediaMtxHostConfig {
    /// Deserialize the `[media.mediamtx]` section out of a raw `autumn.toml`
    /// string. A file with no `[media.mediamtx]` table yields
    /// [`MediaMtxHostConfig::default`] (disabled), so a non-media project reads
    /// as "controller off".
    ///
    /// Fail-closed contract: an *absent* table maps to `Ok(default)`, but a table
    /// that is *present with an ill-typed value* is a hard `Err` — the caller
    /// must abort rather than fall back to the disabled default, otherwise a
    /// media-enabled app would deploy WITHOUT provisioning `MediaMTX`.
    ///
    /// # Errors
    ///
    /// Returns the [`toml::de::Error`] when the string does not parse or a
    /// present `[media.mediamtx]` / `[media.ffmpeg]` value has the wrong type.
    pub fn from_toml_str(toml_str: &str) -> Result<Self, toml::de::Error> {
        let root: MediaTomlRoot = toml::from_str(toml_str)?;
        Ok(root.media.mediamtx)
    }

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

/// Render the `MediaMTX` `mediamtx.yml`, parameterized by [`MediaMtxHostConfig`].
///
/// Generalized from arroyo's broadcast-only `mediamtx.yml`: same LL-HLS window
/// (`hlsVariant: lowLatency`, `hlsPartDuration: 200ms`, `hlsSegmentDuration: 2s`,
/// `hlsSegmentCount: 60`), same fmp4 recording defaults under `recordings_dir`,
/// and the same WebRTC config — plus a **`~^room/.+$` path matcher** for
/// autumn-media's Rooms primitive (Slice 6), which arroyo lacks (it is
/// broadcast-only). Pure — exposed for unit assertions.
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
            .map(|h| format!("'{}'", h.replace('\'', "''")))
            .collect::<Vec<_>>()
            .join(", ");
        format!("[{joined}]")
    };

    format!(
        "logLevel: info\n\
         \n\
         # Only the protocols autumn-media manages are enabled below (RTMP ingest,\n\
         # HLS, WebRTC/WHEP playback, the control API, and the recording-playback\n\
         # server). Every other protocol MediaMTX turns on by default is disabled\n\
         # explicitly, so the systemd unit only ever opens the ports this deploy\n\
         # plan declares — an occupied default RTSP (:8554) or SRT (:8890) port\n\
         # cannot make `systemctl restart mediamtx` fail after preflight passed.\n\
         # (`rtsp: no` also disables RTSPS; `metrics`/`pprof` default off but are\n\
         # pinned off for the same 'declared ports only' contract. This MediaMTX\n\
         # release line ships no MoQ server, so there is no MoQ listener to close.)\n\
         rtsp: no\n\
         srt: no\n\
         metrics: no\n\
         pprof: no\n\
         \n\
         api: true\n\
         apiAddress: :{api}\n\
         \n\
         playback: true\n\
         playbackAddress: :{playback}\n\
         \n\
         rtmp: true\n\
         rtmpAddress: :{rtmp}\n\
         \n\
         hls: true\n\
         hlsAddress: :{hls}\n\
         hlsAllowOrigin: \"*\"\n\
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
         \x20\x20recordPath: {rec_dir}/%path/%Y-%m-%d_%H-%M-%S-%f\n\
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
        rec_dir = cfg.recordings_dir,
        rec_delete = cfg.record_delete_after,
    )
}

/// Render the systemd unit that supervises `MediaMTX`, styled like
/// [`super::render_app_unit`] / [`super::proxy::KamalProxyController::render_proxy_unit`].
///
/// `ExecStart` is `<binary_path> <config_path>`; `Restart=always` (a media
/// daemon that dies must come straight back so ingest/playback recover without
/// operator intervention). Pure — exposed for unit assertions.
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
        bin = cfg.binary_path,
        config = cfg.config_path,
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
             systemctl enable {unit}.service; \
             if [ \"$changed\" -ne 0 ] || ! systemctl is-active --quiet {unit}.service; then \
             systemctl restart {unit}.service; \
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

/// Remediation hint shown when `FFmpeg` does not resolve.
const FFMPEG_HINT: &str =
    "Install FFmpeg on the host (apt package) and set `[media.ffmpeg] bin` to its path";
/// Remediation hint shown when the recordings dir is not writable.
const RECORDINGS_HINT: &str =
    "Create `[media.mediamtx] recordings_dir` on the host and make it writable by the media user";
/// Remediation hint shown when a `MediaMTX` port is occupied or unverifiable.
const PORTS_HINT: &str =
    "Free the conflicting MediaMTX ports on the host (or stop the service already bound to them)";

/// The plugin's `FFmpeg` bin env override (`[media.ffmpeg] bin`). Set in the
/// deploy/runtime environment, it wins over the configured TOML value — exactly
/// as `autumn-media-plugin` resolves it before running any workflow.
pub const FFMPEG_BIN_ENV_OVERRIDE: &str = "AUTUMN_MEDIA__FFMPEG__BIN";

/// Remediation hint shown when the resolved `FFmpeg` path is still an unresolved
/// `${VAR}` / env indirection the deploy-side resolver cannot expand.
const FFMPEG_UNRESOLVED_HINT: &str = "Set AUTUMN_MEDIA__FFMPEG__BIN (or export the ${VAR} it references) in the deploy \
     environment so the FFmpeg path resolves to a concrete binary";

/// Resolve `[media.ffmpeg] bin` to the path the app will actually run, from the
/// process environment — mirroring `autumn-media-plugin`'s own resolution without
/// depending on the plugin. Delegates to [`resolve_ffmpeg_bin_with`] with a
/// `std::env`-backed lookup.
#[must_use]
pub fn resolve_ffmpeg_bin(configured: &str) -> String {
    resolve_ffmpeg_bin_with(configured, |key| std::env::var(key).ok())
}

/// Pure resolver core (testable with an injected `lookup`, so no process-env
/// mutation is needed): the `AUTUMN_MEDIA__FFMPEG__BIN` override wins over
/// `configured`, and a **whole-string** `${VAR}` value is interpolated from
/// `lookup` (unset ⇒ empty string, matching the plugin's `resolve_placeholder`).
/// An *embedded* placeholder is deliberately left literal — the plugin only
/// interpolates whole-string placeholders — so it survives as an unresolved
/// `${...}` the caller's guard treats as deferred.
#[must_use]
fn resolve_ffmpeg_bin_with(configured: &str, lookup: impl Fn(&str) -> Option<String>) -> String {
    // Whole-string `${VAR}` interpolation of the configured value (plugin parity:
    // only an exact `${VAR}` is expanded; unset resolves to empty).
    let interpolated = configured
        .strip_prefix("${")
        .and_then(|rest| rest.strip_suffix('}'))
        .map_or_else(
            || configured.to_owned(),
            |var| lookup(var).unwrap_or_default(),
        );
    // The env override wins (verbatim), like the plugin's `override_string` applied
    // after interpolation. A blank/whitespace override is treated as unset so an
    // empty export never silently becomes the path.
    match lookup(FFMPEG_BIN_ENV_OVERRIDE) {
        Some(v) if !v.trim().is_empty() => v,
        _ => interpolated,
    }
}

/// Grade that `FFmpeg` resolves on the host and reports a version banner
/// (generalized from arroyo's boot preflight).
///
/// First resolves `ffmpeg_bin` via [`resolve_ffmpeg_bin`] (env override +
/// whole-string `${VAR}` interpolation) so the probe targets the binary the app
/// will actually run — not a raw `${VAR}` literal. **Fail-closed-honest:** if the
/// resolved path is empty or still contains an unresolved `${`, it does NOT probe
/// (a literal placeholder / wrong default would be a false pass/fail); it reports
/// a clear non-passing "deferred to runtime" outcome instead. Otherwise it runs
/// `<resolved> -version` over the executor and requires the standard
/// `ffmpeg version` banner in stdout — a binary that is missing, not executable,
/// or is not actually `FFmpeg` (no banner) is a clear failure. Fail-closed: a
/// transport error is a failure, not a pass.
#[must_use]
pub fn ffmpeg_preflight(exec: &impl DeployExecutor, ffmpeg_bin: &str) -> PreflightCheck {
    let resolved = resolve_ffmpeg_bin(ffmpeg_bin);
    // Unverifiable path: empty (an unset whole-string `${VAR}`) or an embedded
    // placeholder we cannot expand. Defer to runtime rather than probe the wrong
    // binary — the plugin resolves these itself before it runs FFmpeg.
    if resolved.trim().is_empty() || resolved.contains("${") {
        return PreflightCheck {
            name: CHECK_FFMPEG_PREFLIGHT,
            passed: false,
            detail: format!(
                "FFmpeg path unresolved (env/interpolation indirection): `{ffmpeg_bin}` \
                 resolves to `{resolved}`; deferring verification to runtime"
            ),
            hint: Some(FFMPEG_UNRESOLVED_HINT),
        };
    }
    let cmd = RemoteCommand::new(
        "media-ffmpeg-preflight",
        format!("{} -version", super::exec::shell_quote(&resolved)),
    );
    match exec.run(&cmd) {
        Ok(out) if out.stdout.contains("ffmpeg version") => PreflightCheck {
            name: CHECK_FFMPEG_PREFLIGHT,
            passed: true,
            detail: format!("FFmpeg resolves at `{resolved}` and reports a version banner"),
            hint: None,
        },
        Ok(_) => PreflightCheck {
            name: CHECK_FFMPEG_PREFLIGHT,
            passed: false,
            detail: format!(
                "`{resolved} -version` ran but did not report an `ffmpeg version` banner — \
                 the binary is not FFmpeg"
            ),
            hint: Some(FFMPEG_HINT),
        },
        Err(err) => PreflightCheck {
            name: CHECK_FFMPEG_PREFLIGHT,
            passed: false,
            detail: format!("could not run `{resolved} -version` on the host: {err}"),
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
            detail: format!("recordings directory `{dir}` exists and is writable"),
            hint: None,
        },
        Err(err) => PreflightCheck {
            name: CHECK_RECORDINGS_DIR_WRITABLE,
            passed: false,
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
    let cmd = RemoteCommand::new(
        "media-unit-active",
        format!("systemctl is-active --quiet {unit_name}.service"),
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

/// Collect the three `MediaMTX` host doctor checks against a live executor.
///
/// Runs the `FFmpeg` preflight (against `ffmpeg_bin`), the recordings-dir probe,
/// and the port-availability probe — returning the shared [`PreflightCheck`]
/// results so a caller that already holds a [`DeployExecutor`] can grade a media
/// host the same way `autumn deploy check` grades the app host. `ffmpeg_bin` is
/// the resolved `[media.ffmpeg] bin` (the plugin's default is `/usr/bin/ffmpeg`).
#[must_use]
pub fn collect_media_doctor_checks(
    exec: &impl DeployExecutor,
    cfg: &MediaMtxHostConfig,
    ffmpeg_bin: &str,
) -> Vec<PreflightCheck> {
    vec![
        ffmpeg_preflight(exec, ffmpeg_bin),
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

    #[test]
    fn empty_toml_yields_disabled_defaults() {
        let cfg = MediaMtxHostConfig::from_toml_str("").expect("empty toml parses");
        assert!(!cfg.enabled);
        assert_eq!(cfg.api_port, MEDIAMTX_API_PORT);
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
        let cfg = MediaMtxHostConfig::from_toml_str(toml).expect("parses");
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
        let cfg = MediaMtxHostConfig::from_toml_str("[deploy]\nhost = \"h\"\n").expect("parses");
        assert!(!cfg.enabled);
    }

    #[test]
    fn ffmpeg_bin_defaults_and_reads_from_media_ffmpeg() {
        assert_eq!(
            ffmpeg_bin_from_toml_str("").expect("empty parses"),
            DEFAULT_FFMPEG_BIN
        );
        let bin =
            ffmpeg_bin_from_toml_str("[media.ffmpeg]\nbin = \"/opt/ffmpeg\"\n").expect("parses");
        assert_eq!(bin, "/opt/ffmpeg");
    }

    #[test]
    fn from_toml_str_fails_closed_on_malformed_mediamtx_section() {
        // Finding C: a PRESENT `[media.mediamtx]` with an ill-typed value is a hard
        // error, so the caller must abort rather than silently disable provisioning.
        // `enabled` as a string, not a bool:
        assert!(
            MediaMtxHostConfig::from_toml_str("[media.mediamtx]\nenabled = \"yes\"\n").is_err()
        );
        // A port field as a non-number:
        assert!(
            MediaMtxHostConfig::from_toml_str("[media.mediamtx]\napi_port = \"nope\"\n").is_err()
        );
    }

    #[test]
    fn media_config_fails_closed_on_malformed_ffmpeg_section() {
        // Finding C: a malformed `[media.ffmpeg]` is also a hard error — both
        // deploy-side parsers deserialize the whole `[media]` subtree, so neither
        // can be tricked into the disabled default by a broken ffmpeg table.
        let toml = "[media.ffmpeg]\nbin = 123\n";
        assert!(ffmpeg_bin_from_toml_str(toml).is_err());
        assert!(MediaMtxHostConfig::from_toml_str(toml).is_err());
    }

    // ── mediamtx.yml rendering ───────────────────────────────────────────────

    #[test]
    fn rendered_yml_contains_every_port() {
        let yml = render_mediamtx_yml(&MediaMtxHostConfig::default());
        assert!(yml.contains("apiAddress: :9997"), "api port: {yml}");
        assert!(yml.contains("playbackAddress: :9996"), "playback: {yml}");
        assert!(yml.contains("rtmpAddress: :1935"), "rtmp: {yml}");
        assert!(yml.contains("hlsAddress: :8888"), "hls: {yml}");
        assert!(yml.contains("webrtcAddress: :8889"), "webrtc: {yml}");
        assert!(yml.contains("webrtcLocalUDPAddress: :8189"), "udp: {yml}");
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
        assert!(yml.contains("recordPath: /recordings/%path/%Y-%m-%d_%H-%M-%S-%f"));
        assert!(yml.contains("recordFormat: fmp4"));
        assert!(yml.contains("recordDeleteAfter: 72h"));
        // WebRTC config.
        assert!(yml.contains("webrtc: true"));
        assert!(yml.contains("webrtcAdditionalHosts: []"));
    }

    #[test]
    fn rendered_yml_disables_unmanaged_protocols_but_keeps_managed_ones() {
        // Finding E: MediaMTX fills omitted protocol flags from its own defaults, so
        // RTSP (:8554) and SRT (:8890) would otherwise listen even though the deploy
        // plan never declares/checks those ports — an occupied default port could
        // then fail `systemctl restart mediamtx` after preflight passed. The template
        // pins every unmanaged default-on protocol OFF so the unit only opens the
        // declared ports.
        let yml = render_mediamtx_yml(&MediaMtxHostConfig::default());
        assert!(yml.contains("rtsp: no"), "RTSP must be disabled: {yml}");
        assert!(yml.contains("srt: no"), "SRT must be disabled: {yml}");
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
        assert!(yml.contains("apiAddress: :19997"));
        assert!(yml.contains("hlsAddress: :18888"));
        assert!(yml.contains("recordPath: /data/rec/%path/"));
        assert!(yml.contains("recordDeleteAfter: 24h"));
        assert!(yml.contains("webrtcAdditionalHosts: ['a.example.com', 'b.example.com']"));
    }

    // ── systemd unit rendering ───────────────────────────────────────────────

    #[test]
    fn rendered_unit_uses_binary_and_config_paths_and_restarts_always() {
        let unit = render_mediamtx_unit(&MediaMtxHostConfig::default());
        assert!(
            unit.contains("ExecStart=/usr/local/bin/mediamtx /etc/mediamtx/mediamtx.yml\n"),
            "ExecStart: {unit}"
        );
        assert!(unit.contains("Restart=always\n"), "restart: {unit}");
        assert!(unit.contains("After=network-online.target\n"));
        assert!(unit.contains("WantedBy=multi-user.target\n"));
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
                assert!(unit.contains("ExecStart=/usr/local/bin/mediamtx"));
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
        assert!(
            shell.contains("systemctl enable mediamtx.service"),
            "{shell}"
        );
        assert!(
            !shell.contains("enable --now"),
            "must not `enable --now` (that would start unconditionally): {shell}"
        );
        // Restart gated on change-OR-inactive, never unconditional.
        assert!(
            shell.contains(
                "if [ \"$changed\" -ne 0 ] || ! systemctl is-active --quiet mediamtx.service; then"
            ),
            "restart must be gated on change or an inactive unit: {shell}"
        );
        assert!(
            shell.contains("systemctl restart mediamtx.service"),
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
        assert!(cmd.shell.contains("systemctl enable mediamtx-prod.service"));
        assert!(
            cmd.shell
                .contains("is-active --quiet mediamtx-prod.service")
        );
        assert!(cmd.shell.contains("restart mediamtx-prod.service"));
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
        assert!(check.hint.is_some());
    }

    #[test]
    fn ffmpeg_preflight_fails_when_binary_missing() {
        // A non-zero exit (missing / not executable) surfaces as a transport
        // error — fail-closed, not a pass.
        let exec = RecordingExecutor::failing_on("media-ffmpeg-preflight");
        let check = ffmpeg_preflight(&exec, "/usr/bin/ffmpeg");
        assert!(!check.passed);
        assert!(check.detail.contains("could not run"));
    }

    // ── Finding G: FFmpeg path resolution (env override + `${VAR}`) ───────────
    //
    // The resolver core is exercised through the injected-`lookup` variant so no
    // process-env mutation (unsafe under edition 2024, and racy across parallel
    // tests) is needed. `ffmpeg_preflight`'s public wrapper reads `std::env`, but
    // its unverifiable-path guard is exercised via `resolve_ffmpeg_bin_with` +
    // the fake executor below.

    #[test]
    fn resolve_honors_env_override_over_toml_value() {
        // Finding G (1): AUTUMN_MEDIA__FFMPEG__BIN wins over the configured value.
        let resolved = resolve_ffmpeg_bin_with("/usr/bin/ffmpeg", |k| {
            (k == FFMPEG_BIN_ENV_OVERRIDE).then(|| "/opt/custom/ffmpeg".to_owned())
        });
        assert_eq!(resolved, "/opt/custom/ffmpeg");
        // A blank override is treated as unset — the TOML value stands.
        let resolved_blank = resolve_ffmpeg_bin_with("/usr/bin/ffmpeg", |k| {
            (k == FFMPEG_BIN_ENV_OVERRIDE).then(|| "   ".to_owned())
        });
        assert_eq!(resolved_blank, "/usr/bin/ffmpeg");
    }

    #[test]
    fn resolve_interpolates_whole_string_placeholder_from_env() {
        // Finding G (2): a whole-string `${VAR}` value is expanded from the env.
        let resolved = resolve_ffmpeg_bin_with("${FF_BIN}", |k| {
            (k == "FF_BIN").then(|| "/usr/local/bin/ffmpeg".to_owned())
        });
        assert_eq!(resolved, "/usr/local/bin/ffmpeg");
        // An unset whole-string placeholder resolves to empty (plugin parity).
        let unset = resolve_ffmpeg_bin_with("${FF_BIN}", |_| None);
        assert_eq!(unset, "");
    }

    #[test]
    fn ffmpeg_preflight_defers_on_unresolved_path_without_probing() {
        // Finding G (3): an empty resolution (unset whole-string `${VAR}`) and an
        // embedded placeholder we can't expand both DEFER — a clear non-passing
        // outcome, and crucially NO probe of a literal placeholder runs.
        for configured in ["${FF_BIN_UNSET_XYZ}", "/opt/${VER}/ffmpeg"] {
            let exec = RecordingExecutor::new();
            let check = ffmpeg_preflight(&exec, configured);
            assert!(!check.passed, "must be non-passing for `{configured}`");
            assert!(
                check.detail.contains("unresolved") && check.detail.contains("deferring"),
                "detail must name the deferral for `{configured}`: {}",
                check.detail
            );
            assert!(check.hint.is_some());
            // Fail-closed-honest: never probed the wrong/placeholder binary.
            assert!(
                exec.labels().is_empty(),
                "no probe must run for `{configured}`, ran {:?}",
                exec.labels()
            );
        }
    }

    #[test]
    fn ffmpeg_preflight_probes_a_concrete_resolved_path() {
        // A concrete (non-placeholder, no override) path resolves to itself and IS
        // probed — the healthy path is unchanged by the resolver.
        let exec = RecordingExecutor::new()
            .with_stdout("media-ffmpeg-preflight", "ffmpeg version 6.1.1 Copyright");
        let check = ffmpeg_preflight(&exec, "/usr/bin/ffmpeg");
        assert!(check.passed, "detail: {}", check.detail);
        assert_eq!(exec.labels(), vec!["media-ffmpeg-preflight"]);
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
    fn collect_runs_all_three_checks_in_order() {
        let exec = RecordingExecutor::new()
            .with_stdout("media-ffmpeg-preflight", "ffmpeg version 6.1")
            .with_stdout(
                "media-ports",
                "tcp LISTEN 0 128 0.0.0.0:22 0.0.0.0:* users:((\"sshd\",pid=5,fd=3))\n",
            );
        let checks =
            collect_media_doctor_checks(&exec, &MediaMtxHostConfig::default(), "/usr/bin/ffmpeg");
        assert_eq!(checks.len(), 3);
        assert!(checks.iter().all(|c| c.passed), "all should pass");
        assert_eq!(
            exec.labels(),
            vec![
                "media-ffmpeg-preflight",
                "media-recordings-dir",
                // The ports check is a single process-aware `ss -tulnp` scan; the
                // old `systemctl is-active` gate was removed (Finding D).
                "media-ports"
            ]
        );
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
