//! Reverse-proxy control for zero-downtime cutover (issue #1607, Slice 2).
//!
//! The public port is fronted by a reverse proxy; each app release binds a
//! PRIVATE loopback port. A redeploy stands the new release up on a *separate*
//! loopback port, health-checks it, then asks the proxy to atomically swap live
//! traffic from the old upstream to the new one and drain the old — this is the
//! cutover.
//!
//! ## What is real here
//!
//! - The [`ProxyController`] trait names the three operations the cutover
//!   orchestration needs — install the proxy, route it at an upstream (first
//!   deploy), and health-gated flip to a new upstream (redeploy). Each returns
//!   ordered [`DeployOp`]s that are driven over the injectable
//!   [`DeployExecutor`](super::exec::DeployExecutor) by
//!   [`run_ops`](super::exec::run_ops), so the exact remote command sequence is
//!   unit-testable without a live host.
//! - [`KamalProxyController`] keeps the exact `kamal-proxy` CLI invocations in
//!   ONE place: install prepares a supervised `kamal-proxy run` on the public
//!   port, and both the initial route and the health-gated flip are the same
//!   `kamal-proxy deploy <service> --target host:port` command — kamal-proxy
//!   blocks until the target passes its health check (pointed at the candidate's
//!   `/ready`), then atomically swaps and drains the old target.
//!
//! ## Swapping the proxy later (Caddy)
//!
//! The proxy is CONFIRMED as kamal-proxy for this slice, but the boundary is
//! drawn so a `CaddyController` could replace it without touching the cutover
//! orchestration in [`super::exec`]: it would implement the same
//! [`ProxyController`] trait with Caddy admin-API calls instead —
//! `route`/`flip` become `POST`/`PATCH` against `/config/...` (a `curl` op to
//! `http://127.0.0.1:2019/...`) and `ensure_installed` provisions the Caddy
//! service. Nothing outside this module encodes a kamal-proxy-specific shape.
//!
//! ## What is deferred
//!
//! - Live ssh is not exercised here (Slice 4's CI container harness).
//!
//! ## Host preparation (issue #1607, AC-1)
//!
//! [`ProxyController::binary_install_ops`] is how a controller PREPARES a bare
//! host: it returns the ops that install its proxy binary, and the deploy runs
//! them only when the compat probe reports
//! [`ProxyCompatFailure::binary_missing`]. [`KamalProxyController`] installs the
//! pinned [`KAMAL_PROXY_KNOWN_GOOD_VERSION`] build (see
//! [`KamalProxyController::install_shell`] for why it comes out of the container
//! image); a controller that ships its own binary inherits the `None` default and
//! is never asked to prepare anything.

use super::exec::{DeployOp, FileContents, ProxyServiceOptions, RemoteCommand, shell_quote};

/// Absolute path the proxy binary is expected at on the target.
const KAMAL_PROXY_BIN: &str = "/usr/local/bin/kamal-proxy";

/// Systemd unit path supervising the proxy process. `pub` (crate-internal in this
/// binary crate) so the deploy-start probe ([`super::exec::probe_deploy_state`]) can
/// read the installed unit's `--http-port` in the same round-trip (#2073).
pub const KAMAL_PROXY_UNIT_PATH: &str = "/etc/systemd/system/kamal-proxy.service";

/// Known-good kamal-proxy version this controller's CLI contract was verified
/// against — the same version the real-VPS validation harness pins
/// (`scripts/deploy-real-vps-validate.sh`, issue #2052). Named in the
/// incompatibility message (issue #2053) so an operator knows exactly what to
/// install when host preparation is declined. It is also the version `autumn
/// deploy` INSTALLS itself on a host that has no kamal-proxy at all (issue #1607,
/// AC-1) — a host that already has one keeps whatever build it has, so this stays
/// the version to pin to when the compat probe fails on an existing install.
pub const KAMAL_PROXY_KNOWN_GOOD_VERSION: &str = "v0.9.2";

/// Content digest of the `basecamp/kamal-proxy` image
/// [`KAMAL_PROXY_KNOWN_GOOD_VERSION`] named when this constant was last verified
/// (an OCI image index covering linux/amd64 and linux/arm64).
///
/// Host preparation pulls `…:{version}@{digest}`, never the bare tag. A Docker Hub
/// tag is MUTABLE — whatever it resolves to at pull time would become a
/// root-executed binary at [`KAMAL_PROXY_BIN`] on every bare host — and `docker
/// create` will happily reuse a poisoned *local* image under that tag without ever
/// contacting the registry. Digest-pinning closes both: the daemon verifies the
/// manifest digest on pull and on local lookup, so a re-pushed tag fails the
/// install instead of silently replacing the binary.
///
/// Bump this together with [`KAMAL_PROXY_KNOWN_GOOD_VERSION`], and only after
/// re-reading the digest from the registry for the new tag:
///
/// ```text
/// T=$(curl -sS "https://auth.docker.io/token?service=registry.docker.io&scope=repository:basecamp/kamal-proxy:pull" | jq -r .token)
/// curl -sSI -H "Authorization: Bearer $T" \
///   -H "Accept: application/vnd.oci.image.index.v1+json" \
///   https://registry-1.docker.io/v2/basecamp/kamal-proxy/manifests/<tag> | grep -i docker-content-digest
/// ```
pub const KAMAL_PROXY_KNOWN_GOOD_DIGEST: &str =
    "sha256:826a6f66c6ba26ac26197ac8755804403c9bb617b90cfac25c7972154c5328ab";

/// The exact `deploy`-subcommand flags [`KamalProxyController::deploy_shell`]
/// emits on every route/flip. These ARE the cutover contract: if a future
/// kamal-proxy renames or removes one of them, a real cutover would break with no
/// warning (issue #2053). The compat probe requires every one of these to appear
/// in `kamal-proxy deploy --help`, so the required set can never drift from what
/// the controller actually passes.
const REQUIRED_DEPLOY_FLAGS: &[&str] = &[
    "--target",
    "--health-check-path",
    "--deploy-timeout",
    "--drain-timeout",
];

/// Extra `deploy` flags required ONLY when TLS is enabled ([`KamalProxyController::with_tls_host`]),
/// matching the `--host <h> --tls` segment `deploy_shell` adds for a TLS app.
const REQUIRED_TLS_DEPLOY_FLAGS: &[&str] = &["--host", "--tls"];

/// Extra `deploy` flag the post-restart live-route RE-REGISTER emits (issue
/// #2071). The re-register in [`KamalProxyController::refresh_installed_ops`] runs
/// `kamal-proxy deploy … --force` (NOT the route/flip), so `--force` is part of
/// the CLI surface this controller can emit and the compat probe must guard it too
/// — otherwise a drifted binary lacking `--force` would only surface at
/// re-register time (post-restart), the worst possible moment. It is kept separate
/// from [`REQUIRED_DEPLOY_FLAGS`] precisely because it is NOT emitted on every
/// route/flip, only on the re-register.
const REQUIRED_REREGISTER_FLAGS: &[&str] = &["--force"];

/// A read-only CLI-surface compatibility probe a [`ProxyController`] can declare,
/// run ONCE before any cutover (issue #2053).
///
/// It pairs the remote command to run — whose combined stdout/stderr is the
/// proxy's own help/surface output — with a pure `verdict` over that output. The
/// verdict returns `Ok(())` when the installed proxy still supports every
/// subcommand/flag the cutover depends on (a compatible host passes silently) and
/// `Err(message)` with a clear, actionable operator message when it does not, so
/// the deploy can fail closed BEFORE touching live traffic.
///
/// The command is deliberately side-effect-free (a `--help` invocation), never
/// fails the ssh step itself (the Rust side owns the verdict), and carries no
/// secret — so it is safe to run and log at deploy time.
pub struct ProxyCompatProbe {
    /// The read-only remote command whose output the verdict inspects.
    pub command: RemoteCommand,
    /// Pure verdict over the command's combined output.
    verdict: CompatVerdict,
}

/// The pure verdict a [`ProxyCompatProbe`] applies to its command's combined
/// output: `Ok(())` when compatible, or a [`ProxyCompatFailure`] otherwise.
type CompatVerdict = Box<dyn Fn(&str) -> Result<(), ProxyCompatFailure> + Send + Sync>;

/// A failed compat verdict: the actionable operator message, plus whether the
/// probe reached **no working binary at all** (issue #1607, AC-1).
///
/// That one bit is what separates a host the deploy can PREPARE (nothing is
/// installed — install the pinned build and carry on) from one it must refuse (a
/// responding binary whose CLI surface has drifted: somebody else's working
/// install, possibly shared with another app on the host, which is not ours to
/// replace silently).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyCompatFailure {
    /// Clear, actionable, secret-free operator message.
    pub message: String,
    /// `true` when no usable proxy binary was reached — the case host preparation
    /// can fix.
    pub binary_missing: bool,
}

impl ProxyCompatProbe {
    /// Build a probe from its command and a pure verdict closure.
    #[must_use]
    pub fn new(
        command: RemoteCommand,
        verdict: impl Fn(&str) -> Result<(), ProxyCompatFailure> + Send + Sync + 'static,
    ) -> Self {
        Self {
            command,
            verdict: Box::new(verdict),
        }
    }

    /// Apply the verdict to the probe command's combined output.
    ///
    /// # Errors
    ///
    /// Returns the actionable operator message (and whether the binary was missing
    /// entirely) when the installed proxy's CLI surface is incompatible with what
    /// the cutover requires.
    pub fn assess(&self, output: &str) -> Result<(), ProxyCompatFailure> {
        (self.verdict)(output)
    }
}

impl std::fmt::Debug for ProxyCompatProbe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The verdict closure is not `Debug`; expose only the command.
        f.debug_struct("ProxyCompatProbe")
            .field("command", &self.command)
            .finish_non_exhaustive()
    }
}

/// Controls the reverse proxy that fronts the public port.
///
/// The methods return ordered [`DeployOp`]s (rather than driving the executor
/// directly) so the proxy steps slot into the same recorded op sequence as the
/// rest of the deploy — the health-gated swap ([`Self::flip_op`]) is the core of
/// the cutover and must be assertable in-order against the recording fake.
///
/// A `CaddyController` is the swappable alternative (admin-API PATCH); see the
/// [module docs](self).
pub trait ProxyController {
    /// Ordered ops that make the proxy installed and supervised on
    /// `public_port` (idempotent — safe to run on every deploy).
    fn ensure_installed_ops(&self, public_port: u16) -> Vec<DeployOp>;

    /// Ordered ops that REFRESH the supervised proxy on a redeploy/upgrade (issue
    /// #2070): (idempotently) re-write + enable the unit, and — for a controller
    /// that supervises an unpinned long-running proxy (kamal-proxy) — restart it to
    /// adopt a CHANGED unit while IMMEDIATELY re-registering the still-live
    /// `live_upstream` (`host:port`, the slot serving at cutover-start) so the
    /// public port is routeless for only ~the restart rather than the entire
    /// candidate-start + migrate + readiness window a naive restart would strand.
    ///
    /// `snapshot_path` is a per-deploy scratch path the implementation may use to
    /// capture the pre-write unit so it can decide whether the unit ACTUALLY changed
    /// (an unchanged unit must NOT restart — that preserves today's steady-state
    /// no-restart behavior).
    ///
    /// `reregister_options` is the proxy TLS/host the still-live OLD release was
    /// registered with (recovered from the `shared/proxy-options` marker, issue
    /// #2074). The change-gated re-register carries THESE options — NOT the
    /// controller's new config — so a later-op failure + rollback leaves the old
    /// release on its own host/TLS rather than the new/removed one.
    ///
    /// The default is exactly [`Self::ensure_installed_ops`] (write + enable, NO
    /// restart): a controller that provisions its own pinned binary/config and has
    /// nothing to "restart to adopt" — e.g. a future `CaddyController` — inherits
    /// the first-deploy behavior unchanged, so only kamal-proxy overrides this.
    fn refresh_installed_ops(
        &self,
        public_port: u16,
        service: &str,
        live_upstream: &str,
        snapshot_path: &str,
        reregister_options: &ProxyServiceOptions,
    ) -> Vec<DeployOp> {
        let _ = (service, live_upstream, snapshot_path, reregister_options);
        self.ensure_installed_ops(public_port)
    }

    /// The proxy `ServiceOptions` (TLS on/off + host) THIS controller registers on a
    /// deploy (issue #2074), persisted to the `shared/proxy-options` marker so the
    /// NEXT redeploy can preserve the old release's own options across the durability
    /// re-register. The default is TLS-off (a controller that terminates no TLS —
    /// e.g. a future `CaddyController`); [`KamalProxyController`] returns its
    /// configured `tls_host`.
    fn proxy_service_options(&self) -> ProxyServiceOptions {
        ProxyServiceOptions {
            tls: false,
            host: None,
        }
    }

    /// Op that points `service`'s public port at `upstream` (`host:port`) for the
    /// first time (first deploy — there is nothing to swap yet).
    fn route_op(&self, service: &str, upstream: &str) -> DeployOp;

    /// Op that health-gates the candidate at `new_upstream` and, once it passes,
    /// atomically swaps live traffic to it and drains the old target. This is the
    /// cutover.
    fn flip_op(&self, service: &str, new_upstream: &str) -> DeployOp;

    /// Optional CLI-surface compatibility probe (issue #2053), run once BEFORE any
    /// cutover so a drifted/renamed proxy CLI fails the deploy closed instead of
    /// breaking a live cutover with no warning.
    ///
    /// `None` (the default) means the controller declares no probe — the caller
    /// then skips the check entirely (a `CaddyController` that provisions its own
    /// pinned binary needs no CLI-surface guardrail). A controller that consumes an
    /// unpinned external binary (kamal-proxy) returns `Some`.
    fn compat_probe(&self) -> Option<ProxyCompatProbe> {
        None
    }

    /// Ordered ops that INSTALL this controller's proxy binary on a host that has
    /// none (issue #1607, AC-1: "target host precondition is at most a stock Ubuntu
    /// LTS with SSH access — the command performs any remaining host preparation
    /// itself").
    ///
    /// Run only when [`Self::compat_probe`]'s verdict reports
    /// [`ProxyCompatFailure::binary_missing`], so a host that already has a working
    /// proxy is never touched and a responding-but-drifted binary is never replaced.
    ///
    /// `None` (the default) means the controller cannot prepare a host — the caller
    /// then reports the missing binary and stops. A controller whose binary ships
    /// with the controller itself would also return `None`.
    fn binary_install_ops(&self) -> Option<Vec<DeployOp>> {
        None
    }

    /// One line naming what [`Self::binary_install_ops`] is about to do to the
    /// operator's server, printed before the install runs.
    ///
    /// Host preparation is the one part of a deploy that changes the target beyond
    /// the app, so it is announced rather than merely logged as an op label — and
    /// the announcement belongs to the controller that owns the ops, not to the
    /// driver, which knows nothing about which proxy it is driving.
    fn binary_install_note(&self) -> String {
        "installing the reverse-proxy binary".to_owned()
    }
}

/// [`ProxyController`] backed by [kamal-proxy](https://github.com/basecamp/kamal-proxy).
///
/// The health-gated upstream swap is `kamal-proxy deploy`, which blocks until the
/// target passes its health check before swapping — so a candidate that never
/// reports `/ready` never receives live traffic.
#[derive(Debug, Clone)]
pub struct KamalProxyController {
    /// Path kamal-proxy health-checks on the candidate before swapping.
    health_check_path: String,
    /// How long the flip waits for the candidate to become healthy.
    deploy_timeout_secs: u64,
    /// How long the old target is drained after the swap.
    drain_timeout_secs: u64,
    /// Public hostname to terminate TLS for. `Some` ONLY when `[deploy.tls]` is
    /// enabled with a valid host: the deploy commands then carry `--host <host>
    /// --tls`, which is what actually turns TLS on for the app (kamal-proxy
    /// provisions a Let's Encrypt cert on-demand for that host). `None` (the
    /// default) leaves the per-app deploy commands byte-for-byte HTTP-only. This
    /// does NOT affect the shared `run` unit, which is TLS-invariant — 443 is
    /// bound by kamal-proxy's default HTTPS listener either way (see
    /// [`Self::render_proxy_unit`]).
    tls_host: Option<String>,
}

/// Default drain window for the old target after a swap.
const DEFAULT_DRAIN_TIMEOUT_SECS: u64 = 30;

impl KamalProxyController {
    /// Build a controller whose flip health-check points at the candidate's
    /// `/ready` and whose deploy timeout matches the deploy's readiness window.
    ///
    /// TLS is off by default (`tls_host = None`); opt in with
    /// [`Self::with_tls_host`].
    #[must_use]
    pub fn new(readiness_timeout_secs: u64) -> Self {
        Self {
            health_check_path: "/ready".to_owned(),
            deploy_timeout_secs: readiness_timeout_secs,
            drain_timeout_secs: DEFAULT_DRAIN_TIMEOUT_SECS,
            tls_host: None,
        }
    }

    /// Set the public hostname the proxy terminates TLS for (`[deploy.tls]`
    /// opt-in). Pass `Some(host)` only when TLS is enabled and the host is valid;
    /// `None` (the default) keeps the proxy HTTP-only with unchanged commands.
    #[must_use]
    pub fn with_tls_host(mut self, tls_host: Option<String>) -> Self {
        self.tls_host = tls_host;
        self
    }

    /// The single `kamal-proxy deploy` invocation shared by the initial route, the
    /// health-gated flip, and the post-restart live-route re-register. Centralized
    /// so the exact CLI contract lives in one place (a Caddy controller would
    /// replace THIS with an admin-API call).
    ///
    /// `force` appends `--force`, which makes kamal-proxy install+persist the route
    /// WITHOUT waiting for a fresh `/ready` health check (issue #2071). It is passed
    /// `true` ONLY by the re-register in [`Self::refresh_installed_ops`] — which is
    /// re-pinning a route to the release that is ALREADY LIVE and serving, so a
    /// fresh readiness gate is both unnecessary and dangerous (a transient `/ready`
    /// blip during the one-time durability restart could otherwise strand `:80` or
    /// abort the deploy). The initial route ([`Self::route_op`]) and the cutover
    /// flip ([`Self::flip_op`]) pass `false` and STAY health-gated (unchanged): they
    /// route to a candidate whose readiness must genuinely be proven first.
    fn deploy_shell(&self, service: &str, target: &str, force: bool) -> String {
        // The common route/flip/re-register uses THIS controller's configured
        // `tls_host`; only the #2074 durability re-register overrides it (below).
        self.deploy_shell_with_tls(service, target, force, self.tls_host.as_deref())
    }

    /// [`Self::deploy_shell`] with a per-call TLS host override (issue #2074).
    ///
    /// The durability-refresh re-register of the still-live OLD release must carry
    /// that release's OWN `--host/--tls` (recovered from the `shared/proxy-options`
    /// marker), NOT the controller's new config, so a rollback never strands the old
    /// release behind the new/removed host. `tls_host = None` emits the HTTP-only
    /// form (no `--host/--tls` — a TLS-removed re-register). The route/flip delegate
    /// here via [`Self::deploy_shell`] with `self.tls_host`, so their output is
    /// unchanged.
    fn deploy_shell_with_tls(
        &self,
        service: &str,
        target: &str,
        force: bool,
        tls_host: Option<&str>,
    ) -> String {
        // Every string parameter is shell-quoted so a service name, upstream, or
        // health-check path carrying query params / special chars can't break out
        // of the command. The numeric timeouts need no quoting.
        //
        // Control-socket fidelity (issue #1948 item 4): kamal-proxy resolves its
        // control socket at `$XDG_RUNTIME_DIR/kamal-proxy.sock`, falling back to
        // `/tmp/kamal-proxy.sock` when unset. The supervised `kamal-proxy run`
        // systemd SERVICE has no `XDG_RUNTIME_DIR` (-> `/tmp`), but the ssh
        // session this command runs in gets `XDG_RUNTIME_DIR=/run/user/0` from
        // pam_systemd on a real host — a DIFFERENT path — so a naive invocation
        // fails with "connect: no such file or directory". Prefixing with
        // `env -u XDG_RUNTIME_DIR` pins the CLI to the same `/tmp` fallback the
        // service used, so both agree regardless of pam_systemd — no need to
        // disable pam_systemd on the host (the container e2e fixture used to work
        // around this by disabling it; the real-VPS shape does not).
        //
        // TLS (opt-in): `--host <host> --tls` sits in a STABLE position between
        // the health-check path and the timeouts. When `tls_host` is `None` the
        // segment is empty and the command is byte-for-byte the HTTP-only form.
        let tls = tls_host.map_or_else(String::new, |host| {
            format!("--host {host} --tls ", host = shell_quote(host))
        });
        // `--force` (re-register only, issue #2071) trails the stable flag block, so
        // for the common route/flip (`force = false`) the command is byte-for-byte
        // the prior health-gated form.
        let force_flag = if force { " --force" } else { "" };
        format!(
            "env -u XDG_RUNTIME_DIR kamal-proxy deploy {service} --target {target} \
             --health-check-path {path} {tls}--deploy-timeout {deploy}s \
             --drain-timeout {drain}s{force_flag}",
            service = shell_quote(service),
            target = shell_quote(target),
            path = shell_quote(&self.health_check_path),
            deploy = self.deploy_timeout_secs,
            drain = self.drain_timeout_secs,
        )
    }

    /// The one remote command that installs the pinned kamal-proxy on a host that
    /// has none (issue #1607, AC-1).
    ///
    /// # Why the binary comes out of a container image
    ///
    /// kamal-proxy publishes **no release binaries** — upstream ships it only as the
    /// `basecamp/kamal-proxy` container image (Homebrew builds from source). So the
    /// honest way to land the same static binary the project publishes is to pull
    /// the pinned image and copy the binary out of it, which is exactly what this
    /// repo's real-VPS validation has always done by hand
    /// (`scripts/deploy-real-vps-validate.sh`). Kamal itself installs a container
    /// runtime on the target during `kamal setup` for the same reason.
    ///
    /// # Never replaces an existing binary
    ///
    /// The whole op is gated on the compat probe reporting no usable binary, but the
    /// verdict is an INFERENCE from `--help` output: a `kamal-proxy` that resolves
    /// through a different `PATH` entry, or a future release whose CLI surface drifted
    /// so far that no required flag rendered, both classify as "no binary". Neither is
    /// ours to overwrite — the binary may be shared with another service on the host —
    /// so the invariant is ENFORCED here rather than merely inferred upstream: the op
    /// refuses outright when anything already exists at the target path. The operator
    /// then gets that message instead of a silently downgraded proxy.
    ///
    /// # What it does, in order
    ///
    /// 1. install the two host packages a deploy needs and a minimal image may
    ///    lack — `curl` (the readiness gate polls `/ready` **on the host**) and a
    ///    container runtime — but ONLY the ones actually missing, refreshing the apt
    ///    index at most once and never interactively;
    /// 2. make sure the container daemon is up (idempotent);
    /// 3. copy the binary out of the pinned image into a TEMP path, mark it `0755`,
    ///    and `mv` it into place — so the supervised path is never a half-written
    ///    file, even if the copy dies midway;
    /// 4. remove the scratch container, then VERIFY the installed binary answers
    ///    `kamal-proxy deploy --help` — an install that "succeeded" without landing
    ///    a working binary must fail here, not at the first cutover.
    ///
    /// `set -e` makes any step's failure fail the op, which the caller turns into an
    /// actionable abort before anything is cut over. It carries no secret, so it is
    /// safe to log.
    ///
    /// Because this whole op is gated on the proxy binary being ABSENT, it is the
    /// bare-host path: a host that already has kamal-proxy was prepared by somebody
    /// (an earlier `autumn deploy`, or the operator) and is assumed to still have
    /// `curl`, exactly as it was before host preparation existed.
    fn install_shell() -> String {
        let pin = KAMAL_PROXY_KNOWN_GOOD_VERSION;
        let digest = KAMAL_PROXY_KNOWN_GOOD_DIGEST;
        let bin = KAMAL_PROXY_BIN;
        format!(
            "( set -e; \
             if [ -e '{bin}' ]; then \
             echo 'kamal-proxy is already installed at {bin}; refusing to replace it' >&2; \
             exit 1; \
             fi; \
             need=''; \
             command -v curl >/dev/null 2>&1 || need=\"$need curl\"; \
             command -v docker >/dev/null 2>&1 || need=\"$need docker.io\"; \
             if [ -n \"$need\" ]; then \
             export DEBIAN_FRONTEND=noninteractive; \
             apt-get update -qq; \
             apt-get install -y -qq --no-install-recommends $need; \
             fi; \
             systemctl start docker >/dev/null 2>&1 || true; \
             cid=$(docker create 'basecamp/kamal-proxy:{pin}@{digest}'); \
             docker cp \"$cid:/usr/local/bin/kamal-proxy\" '{bin}.autumn-new'; \
             docker rm -f \"$cid\" >/dev/null 2>&1 || true; \
             chmod 755 '{bin}.autumn-new'; \
             mv -f '{bin}.autumn-new' '{bin}'; \
             '{bin}' deploy --help >/dev/null 2>&1 || {{ \
             echo 'the installed kamal-proxy does not run on this host' >&2; exit 1; }} \
             ) || {{ \
             echo 'autumn deploy could not prepare this host with kamal-proxy {pin}. It needs \
an apt-based (Debian/Ubuntu) host, outbound HTTPS to the distro mirrors and to Docker Hub, and \
an SSH user that can install packages. Install kamal-proxy {pin} at {bin} yourself and set \
`[deploy] install_proxy = false` in autumn.toml to skip this step.' >&2; \
             exit 1; }}"
        )
    }

    /// The read-only command the compat probe runs: `kamal-proxy deploy --help`
    /// (issue #2053).
    ///
    /// `deploy` is the ONE subcommand both the initial route and the health-gated
    /// flip use, so its help output is the authoritative surface for the cutover
    /// contract. `--help` is a built-in that survives across kamal-proxy releases
    /// (v0.9.2, which dropped the `version` subcommand, still has it — so this is
    /// the reliable probe the harness settled on). `2>&1` folds cobra's
    /// error/usage output (e.g. an `unknown command` when `deploy` was renamed)
    /// into the captured stream, and `|| true` keeps the ssh step itself from
    /// failing — the Rust-side verdict, not the exit status, decides compatibility.
    #[must_use]
    pub fn compat_probe_command() -> RemoteCommand {
        RemoteCommand::new(
            "proxy-compat-probe",
            "kamal-proxy deploy --help 2>&1 || true",
        )
    }

    /// The `deploy` flags this controller requires to be present in the probe
    /// output — the base set, the TLS flags when TLS is enabled, and the
    /// re-register `--force` (#2071). The compat probe guards every flag the
    /// controller can emit on ANY deploy invocation, so `--force` is included even
    /// though only the post-restart re-register passes it.
    #[must_use]
    pub fn required_deploy_flags(&self) -> Vec<&'static str> {
        let mut flags = REQUIRED_DEPLOY_FLAGS.to_vec();
        if self.tls_host.is_some() {
            flags.extend_from_slice(REQUIRED_TLS_DEPLOY_FLAGS);
        }
        flags.extend_from_slice(REQUIRED_REREGISTER_FLAGS);
        flags
    }

    /// Pure verdict on a `kamal-proxy deploy --help` capture for this controller's
    /// TLS configuration (issue #2053). Exposed for unit assertions.
    ///
    /// # Errors
    ///
    /// Returns the specific [`KamalProxyCompatIssue`] when the installed binary is
    /// missing/unusable, has no `deploy` subcommand, or is missing a flag the
    /// cutover passes.
    // The production path applies the verdict through the trait's `compat_probe`
    // closure (which calls the free `assess_kamal_proxy_deploy_help`); this typed
    // wrapper exists for direct unit assertions, mirroring `run_ops`'s test-only
    // reachability in `super::exec`.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn assess_deploy_help(&self, output: &str) -> Result<(), KamalProxyCompatIssue> {
        assess_kamal_proxy_deploy_help(output, &self.required_deploy_flags())
    }

    /// Render the systemd unit that supervises `kamal-proxy run` on the public
    /// port. Pure — exposed for unit assertions.
    ///
    /// The `run` command passes only `--http-port {public_port}`. kamal-proxy
    /// binds its HTTPS listener on 443 BY DEFAULT and that listener cannot be
    /// disabled, so 443 is served with no explicit `--https-port` — the flag
    /// would change no ports and only make the unit differ cosmetically. The
    /// unit is therefore TLS-invariant: it is byte-for-byte identical whether or
    /// not any app opts into TLS, so a non-TLS app deploy never rewrites (and
    /// never restarts) the shared proxy. Per-app TLS is enabled separately at
    /// `deploy` time via `--host <h> --tls` (see [`Self::deploy_shell`]), which
    /// makes kamal-proxy provision a Let's Encrypt cert on-demand for that host
    /// on the always-bound 443.
    ///
    /// ## Reboot durability
    ///
    /// kamal-proxy `run` persists the live routing table on every `deploy` and
    /// RESTORES it on startup (`RestoreLastSavedState`) from its state file at
    /// `$HOME/.config/kamal-proxy/kamal-proxy.state`. Under a bare systemd unit
    /// `HOME` is unset, so that state dir falls back to ephemeral `/tmp` and the
    /// deploy-time route snapshot is wiped on reboot — after a power-cycle `run`
    /// restores an empty routing table and `:80` has no upstream. `StateDirectory`
    /// makes systemd create/own a persistent `/var/lib/kamal-proxy` that survives
    /// reboot, and `Environment=HOME=/var/lib/kamal-proxy` points kamal-proxy's
    /// state (and cert) dir there — so the state file lives at
    /// `/var/lib/kamal-proxy/.config/kamal-proxy/kamal-proxy.state` and the
    /// deployed route is restored on boot. This is independent of the control
    /// socket (`XDG_RUNTIME_DIR`-derived, see [`Self::deploy_shell`]): the
    /// deploy-time CLI talks to `run` over that socket and writes no state, so the
    /// `env -u XDG_RUNTIME_DIR` socket pin is untouched.
    #[must_use]
    pub fn render_proxy_unit(public_port: u16) -> String {
        format!(
            "[Unit]\n\
             Description=kamal-proxy (Autumn deploy front)\n\
             After=network-online.target\n\
             Wants=network-online.target\n\
             \n\
             [Service]\n\
             Type=simple\n\
             StateDirectory=kamal-proxy\n\
             Environment=HOME=/var/lib/kamal-proxy\n\
             ExecStart={KAMAL_PROXY_BIN} run --http-port {public_port}\n\
             Restart=on-failure\n\
             RestartSec=2\n\
             \n\
             [Install]\n\
             WantedBy=multi-user.target\n",
        )
    }
}

impl ProxyController for KamalProxyController {
    fn ensure_installed_ops(&self, public_port: u16) -> Vec<DeployOp> {
        vec![
            // Write the freshly-rendered unit directly to its final path. The unit
            // is invariant to any app's TLS state — `run` always binds both the
            // HTTP and HTTPS listeners (see [`Self::render_proxy_unit`]) — so it
            // does not change when an app opts into TLS, and there is nothing to
            // "restart to adopt". Writing straight to the final path (rather than a
            // fixed staging path + diff/`mv`) also avoids a concurrent-deploy race:
            // kamal-proxy is SHARED ingress, and two `deploy up` runs against the
            // same host would otherwise interleave on one fixed staging file.
            DeployOp::WriteFile {
                label: "proxy-write-unit",
                contents: FileContents::Plain(Self::render_proxy_unit(public_port)),
                remote_path: KAMAL_PROXY_UNIT_PATH.to_owned(),
                mode: Some(0o644),
            },
            // Reload systemd so the (possibly first-ever) unit is picked up, then
            // `enable --now` to make sure the shared proxy is enabled and running.
            // Idempotent and safe on every deploy.
            DeployOp::Run(RemoteCommand::new(
                "proxy-install",
                "systemctl daemon-reload && systemctl enable --now kamal-proxy.service".to_owned(),
            )),
        ]
    }

    fn route_op(&self, service: &str, upstream: &str) -> DeployOp {
        // First-deploy route: health-gated (force = false) — the candidate's
        // readiness must be proven before it receives live traffic.
        DeployOp::Run(RemoteCommand::new(
            "proxy-route",
            self.deploy_shell(service, upstream, false),
        ))
    }

    fn flip_op(&self, service: &str, new_upstream: &str) -> DeployOp {
        // Cutover flip: health-gated (force = false) — this is THE readiness gate
        // that keeps an unready candidate from ever taking live traffic.
        DeployOp::Run(RemoteCommand::new(
            "proxy-flip",
            self.deploy_shell(service, new_upstream, false),
        ))
    }

    fn refresh_installed_ops(
        &self,
        public_port: u16,
        service: &str,
        live_upstream: &str,
        snapshot_path: &str,
        reregister_options: &ProxyServiceOptions,
    ) -> Vec<DeployOp> {
        let unit = shell_quote(KAMAL_PROXY_UNIT_PATH);
        let snap = shell_quote(snapshot_path);

        // (1) Snapshot the CURRENTLY-installed unit's CONTENT hash BEFORE the write
        // below overwrites it. A rewrite always bumps the file's mtime even when the
        // bytes are identical, so systemd's own change tracking (`NeedDaemonReload`)
        // cannot distinguish "actually changed" from "merely rewritten" — a content
        // hash can. An absent file yields an empty hash, which compares unequal to
        // the freshly-written one and so counts as "changed" (correct: a brand-new
        // unit is a change). This is deliberately a HASH, not a copy of the unit
        // (one of the two mechanisms the design allows), so the scratch file is tiny.
        let snapshot = DeployOp::Run(RemoteCommand::new(
            "proxy-snapshot-unit",
            format!("{{ sha256sum {unit} 2>/dev/null || true; }} | cut -d' ' -f1 > {snap}"),
        ));

        let mut ops = Vec::with_capacity(4);
        ops.push(snapshot);
        // (2)+(3) The idempotent install — write the refreshed unit to its FINAL path
        // + daemon-reload + `enable --now` — byte-for-byte the first-deploy path
        // (labels `proxy-write-unit` / `proxy-install`). This is what makes the
        // reboot-durable `StateDirectory`/`HOME` unit (#2069) reach EXISTING hosts on
        // every redeploy. On its own it restarts NO running process: `enable --now`
        // never relaunches an already-active unit, so a live proxy keeps serving with
        // its old settings until the change-gated restart below (if any).
        ops.extend(self.ensure_installed_ops(public_port));
        // (4) Restart ONLY when the unit CONTENT actually changed AND the proxy is
        // live, then IMMEDIATELY re-register the current live upstream. Rationale:
        //   * A fresh `kamal-proxy run` restores its LAST-SAVED routing table on
        //     start; on the first upgrade onto the persistent `StateDirectory` that
        //     table is empty, so a bare restart would leave the public port routeless
        //     until the next deploy step re-registered it — and a naive restart at
        //     cutover-start would strand :80 for the WHOLE candidate-start + migrate +
        //     readiness window (a zero-downtime violation). Re-registering the CURRENT
        //     live upstream (the slot serving at cutover-start) right after the
        //     restart bounds the routeless window to ~the restart itself.
        //   * The re-register uses `--force` (#2071). It is re-pinning a route to the
        //     release that is ALREADY LIVE and serving, so a fresh readiness gate is
        //     both unnecessary and DANGEROUS: a health-gated re-register would block
        //     on a fresh `/ready` pass, and a transient `/ready` blip during the
        //     one-time restart could strand :80 or abort the whole deploy (up to
        //     30×deploy-timeout). `--force` makes kamal-proxy install+persist the
        //     route IMMEDIATELY (it skips `WaitUntilHealthy` but still runs
        //     `saveStateSnapshot`), and the target self-heals via background health
        //     checks (~1s) — during a real blip it starts serving the instant
        //     `/ready` recovers, WITHOUT failing the deploy. (A residual ~1s
        //     `TargetStateAdding` window before the first background `/ready` pass
        //     remains — kamal-proxy won't route to an unready backend — a documented
        //     footnote, not a hard failure.)
        //   * The restart and the re-register are ONE remote command, so no SSH
        //     round-trip can interpose between them. Because `--force` returns
        //     WITHOUT waiting for readiness, the re-register succeeds on the first
        //     attempt in the normal case; the long 30×deploy-timeout burn is gone.
        //     A SMALL bounded retry (5) is kept only to ride out a transient CLI /
        //     control-socket hiccup in the brief window before the just-restarted
        //     proxy's socket accepts a connection.
        //   * An UNCHANGED unit (steady-state redeploy) takes NEITHER branch — today's
        //     no-restart behavior is preserved exactly, so a routine redeploy never
        //     churns the shared proxy.
        // The re-register uses `deploy_shell_with_tls` (with `force = true`), so it
        // keeps the `env -u XDG_RUNTIME_DIR` control-socket pin (#2019). It carries
        // `reregister_options`' `--host/--tls` — the still-live OLD release's OWN
        // options recovered from the `shared/proxy-options` marker (issue #2074), NOT
        // this controller's new config — so a failed rollback leaves the old release
        // on its own host/TLS instead of stranding it behind the new/removed host.
        // The candidate flip (`flip_op`) still uses `self.tls_host` (the new config),
        // so the new release gets the new host. The `|| exit 1` fail-fast on
        // `systemctl restart` is unchanged. On an ABSENT marker the redeploy arm
        // passes the new config as `reregister_options` (proceed-as-legacy: the
        // durability-upgrade deploy's kamal-proxy table is empty, so there is nothing
        // to preserve — refusing there would deadlock every pre-#2074 host's first
        // redeploy); an UNREADABLE marker is refused at pre-flight before this runs.
        ops.push(DeployOp::Run(RemoteCommand::new(
            "proxy-restart-if-changed",
            format!(
                "new=\"$( {{ sha256sum {unit} 2>/dev/null || true; }} | cut -d' ' -f1 )\"; \
                 old=\"$(cat {snap} 2>/dev/null || true)\"; \
                 rm -f {snap}; \
                 if [ \"$new\" = \"$old\" ]; then exit 0; fi; \
                 if ! systemctl is-active --quiet kamal-proxy.service; then exit 0; fi; \
                 systemctl restart kamal-proxy.service || exit 1; \
                 n=0; \
                 until {reregister}; do \
                 n=$((n+1)); \
                 if [ \"$n\" -ge 5 ]; then \
                 echo 'kamal-proxy did not accept the live route after restart' >&2; exit 1; fi; \
                 sleep 0.5; \
                 done",
                reregister = self.deploy_shell_with_tls(
                    service,
                    live_upstream,
                    true,
                    reregister_options.host.as_deref()
                ),
            ),
        )));
        ops
    }

    fn proxy_service_options(&self) -> ProxyServiceOptions {
        // Authoritative "what did we actually register": `deploy_shell` emits
        // `--host/--tls` iff `tls_host` is `Some`, so the marker mirrors that exactly
        // (a configured-but-blank host resolves to `tls_host = None` upstream → TLS-off).
        ProxyServiceOptions {
            tls: self.tls_host.is_some(),
            host: self.tls_host.clone(),
        }
    }

    fn binary_install_ops(&self) -> Option<Vec<DeployOp>> {
        Some(vec![DeployOp::Run(RemoteCommand::new(
            "install-proxy",
            Self::install_shell(),
        ))])
    }

    fn binary_install_note(&self) -> String {
        format!(
            "no kamal-proxy on this host — installing {KAMAL_PROXY_KNOWN_GOOD_VERSION} at \
             {KAMAL_PROXY_BIN}, from the pinned basecamp/kamal-proxy image. This also \
             installs, and leaves behind, any of `curl` and a container runtime the host \
             is missing. Decline with `[deploy] install_proxy = false`."
        )
    }

    fn compat_probe(&self) -> Option<ProxyCompatProbe> {
        // Capture the required flag set (which depends on this controller's TLS
        // config) into the pure verdict closure, so the probe is self-contained
        // and Caddy-swappable through the trait.
        let required = self.required_deploy_flags();
        Some(ProxyCompatProbe::new(
            Self::compat_probe_command(),
            move |output| {
                assess_kamal_proxy_deploy_help(output, &required).map_err(|issue| {
                    ProxyCompatFailure {
                        message: issue.message(),
                        // Only "no working binary answered at all" is a host-prep
                        // case; a responding-but-drifted binary is not ours to
                        // replace. See [`ProxyCompatFailure`].
                        binary_missing: issue == KamalProxyCompatIssue::BinaryUnusable,
                    }
                })
            },
        ))
    }
}

/// Why the installed kamal-proxy's CLI surface is incompatible with what the
/// cutover requires (issue #2053). Each variant carries enough to build a clear,
/// actionable operator [`Self::message`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KamalProxyCompatIssue {
    /// The probe reached no working `kamal-proxy` binary (missing, not executable,
    /// or produced no help output at all).
    BinaryUnusable,
    /// The binary responded but has no `deploy` subcommand (renamed/removed) — the
    /// route/flip command the cutover is built on is gone.
    DeploySubcommandMissing,
    /// `kamal-proxy deploy` exists but is missing flag(s) the cutover passes; the
    /// CLI surface has drifted from what this release was built against.
    MissingFlags(Vec<&'static str>),
}

impl KamalProxyCompatIssue {
    /// A clear, actionable, secret-free operator message naming the exact problem
    /// and the remedy (pin kamal-proxy to the known-good version on host
    /// bootstrap), and stating that nothing was cut over.
    #[must_use]
    pub fn message(&self) -> String {
        let pin = KAMAL_PROXY_KNOWN_GOOD_VERSION;
        match self {
            Self::BinaryUnusable => format!(
                "the kamal-proxy binary at `{KAMAL_PROXY_BIN}` did not respond to \
                 `kamal-proxy deploy --help` (missing or not executable). Install \
                 kamal-proxy {pin} there — e.g. `docker create \
                 basecamp/kamal-proxy:{pin}` then `docker cp \
                 <id>:/usr/local/bin/kamal-proxy {KAMAL_PROXY_BIN} && chmod 755 \
                 {KAMAL_PROXY_BIN}` — or let `autumn deploy` install it for you by \
                 leaving `[deploy] install_proxy` at its default `true`. Aborting \
                 before any cutover, so live traffic was not touched."
            ),
            Self::DeploySubcommandMissing => format!(
                "the installed kamal-proxy has no `deploy` subcommand — the CLI \
                 surface this deploy is built on has changed. Pin kamal-proxy to a \
                 compatible version ({pin}) in the target's host bootstrap and \
                 redeploy. Aborting before any cutover, so live traffic was not \
                 touched."
            ),
            Self::MissingFlags(flags) => format!(
                "the installed kamal-proxy `deploy` command is missing flag(s) this \
                 deploy requires: {missing}. The kamal-proxy CLI surface has drifted \
                 from what this release was built against. Pin kamal-proxy to a \
                 compatible version ({pin}) in the target's host bootstrap and \
                 redeploy. Aborting before any cutover, so live traffic was not \
                 touched.",
                missing = flags.join(", "),
            ),
        }
    }
}

/// Is `flag` present in `output` as a standalone CLI flag token — not merely a
/// substring of a longer, renamed flag (e.g. `--target` inside `--target-host`,
/// or `--tls` inside `--tls-enabled`)?
///
/// A flag renamed to a superstring is EXACTLY the drift this probe must catch, so
/// a plain substring match would false-negative (report the old flag as present)
/// and silently defeat the guardrail. The match therefore requires a token
/// boundary on both sides: the char immediately before and after the match must be
/// neither an ASCII alphanumeric nor `-` (the characters that continue a flag
/// token). Boundaries are resolved from the surrounding `str` slices' `chars()`,
/// never by slicing mid-codepoint, so it is UTF-8-safe (the flag itself is ASCII,
/// so `find`/`end` always land on char boundaries).
fn output_lists_flag(output: &str, flag: &str) -> bool {
    let flen = flag.len();
    let mut search_from = 0;
    while let Some(rel) = output[search_from..].find(flag) {
        let start = search_from + rel;
        let end = start + flen;
        let before_ok = output[..start]
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_ascii_alphanumeric() && c != '-');
        let after_ok = output[end..]
            .chars()
            .next()
            .is_none_or(|c| !c.is_ascii_alphanumeric() && c != '-');
        if before_ok && after_ok {
            return true;
        }
        // The match started at the flag's leading `-` (ASCII), so `start + 1` is a
        // valid char boundary to resume from.
        search_from = start + 1;
    }
    false
}

/// Pure verdict on a `kamal-proxy deploy --help` capture given the flags the
/// caller requires (issue #2053).
///
/// Decides on the **deploy help's own content first**, because the probe folds
/// the remote shell's stderr into the capture via `2>&1`: a healthy target whose
/// login shell prints startup noise (e.g. a `.bashrc` line failing with `foo:
/// command not found`) alongside a perfectly valid `deploy --help` must NEVER be
/// blocked — that would break a host that is already fine. So:
///
/// 1. If **every** required flag is present (boundary-matched), the deploy help
///    rendered correctly → `Ok(())`, regardless of any unrelated stderr noise in
///    the same stream. (A missing/broken binary or a removed subcommand prints
///    NONE of these flags, so their presence is a reliable "help rendered" signal.)
/// 2. If **some** required flag is present but not all, the help genuinely
///    rendered and a flag has drifted → [`KamalProxyCompatIssue::MissingFlags`]
///    (naming only the absent ones). Because at least one flag is present, shell
///    noise can't tip this into a false "binary missing" verdict.
/// 3. If **no** required flag is present, the deploy help did not render at all.
///    Classify why: a cobra `unknown command "deploy"` means the subcommand was
///    renamed/removed ([`KamalProxyCompatIssue::DeploySubcommandMissing`]);
///    anything else (a missing / non-executable / wrong-arch binary, or no output)
///    is [`KamalProxyCompatIssue::BinaryUnusable`]. Either way the deploy fails
///    closed.
fn assess_kamal_proxy_deploy_help(
    output: &str,
    required_flags: &[&'static str],
) -> Result<(), KamalProxyCompatIssue> {
    let missing: Vec<&'static str> = required_flags
        .iter()
        .copied()
        .filter(|flag| !output_lists_flag(output, flag))
        .collect();

    // (1) All required flags present → valid deploy help → compatible, even if the
    // capture also carries benign shell/login noise.
    if missing.is_empty() {
        return Ok(());
    }

    // (2) Some (but not all) required flags present → the deploy help rendered, so
    // this is real flag drift, not shell noise. Report exactly the absent flag(s).
    if missing.len() < required_flags.len() {
        return Err(KamalProxyCompatIssue::MissingFlags(missing));
    }

    // (3) No required flag rendered → the deploy help did not print. Distinguish a
    // renamed/removed `deploy` subcommand from an unusable binary; both fail closed.
    let lower = output.to_ascii_lowercase();
    if lower.contains("unknown command") && lower.contains("deploy") {
        return Err(KamalProxyCompatIssue::DeploySubcommandMissing);
    }
    Err(KamalProxyCompatIssue::BinaryUnusable)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kamal_proxy_flip_command_is_correct() {
        let proxy = KamalProxyController::new(60);
        let DeployOp::Run(cmd) = proxy.flip_op("myapp", "127.0.0.1:3002") else {
            panic!("flip_op must be a Run op");
        };
        assert_eq!(cmd.label, "proxy-flip");
        // Exact kamal-proxy CLI contract — the health check points at the
        // candidate's /ready and the deploy timeout is the readiness window.
        assert_eq!(
            cmd.shell,
            "env -u XDG_RUNTIME_DIR kamal-proxy deploy 'myapp' --target '127.0.0.1:3002' \
             --health-check-path '/ready' --deploy-timeout 60s --drain-timeout 30s",
        );
    }

    #[test]
    fn reregister_is_forced_but_route_and_flip_stay_health_gated() {
        // #2071: the post-restart re-register re-pins the ALREADY-LIVE upstream, so it
        // must use `--force` (install+persist immediately, no fresh readiness gate a
        // transient /ready blip could fail). The first-deploy route and the cutover
        // flip route to a candidate whose readiness must be PROVEN, so they stay
        // health-gated (NO --force) — unchanged.
        let proxy = KamalProxyController::new(60);

        // The re-register (last op of the refresh) is forced, socket-pinned, and
        // targets the LIVE upstream.
        let ops = proxy.refresh_installed_ops(
            8080,
            "myapp",
            "127.0.0.1:3001",
            "/tmp/snap.sha256",
            &ProxyServiceOptions {
                tls: false,
                host: None,
            },
        );
        let DeployOp::Run(restart) = ops.last().expect("has ops") else {
            panic!("last op must be the restart/re-register");
        };
        assert!(
            restart.shell.contains(
                "env -u XDG_RUNTIME_DIR kamal-proxy deploy 'myapp' --target '127.0.0.1:3001' \
                 --health-check-path '/ready' --deploy-timeout 60s --drain-timeout 30s --force"
            ),
            "re-register must be forced + socket-pinned + target the live upstream: {}",
            restart.shell,
        );
        // The `|| exit 1` fail-fast on the restart itself is preserved.
        assert!(
            restart
                .shell
                .contains("systemctl restart kamal-proxy.service || exit 1"),
            "restart fail-fast must be preserved: {}",
            restart.shell,
        );
        // With --force the deploy returns without waiting, so the long 30×retry burn
        // is gone — only a SMALL bounded retry (5) rides out a transient socket hiccup.
        assert!(
            restart.shell.contains("if [ \"$n\" -ge 5 ]"),
            "the retry ceiling is small (5), not the old 30: {}",
            restart.shell,
        );

        // The cutover flip and the first-deploy route are health-gated: NO --force.
        let DeployOp::Run(flip) = proxy.flip_op("myapp", "127.0.0.1:3002") else {
            panic!("flip_op must be a Run op");
        };
        assert!(
            !flip.shell.contains("--force"),
            "the cutover flip must STAY health-gated (no --force): {}",
            flip.shell,
        );
        let DeployOp::Run(route) = proxy.route_op("myapp", "127.0.0.1:3002") else {
            panic!("route_op must be a Run op");
        };
        assert!(
            !route.shell.contains("--force"),
            "the first-deploy route must STAY health-gated (no --force): {}",
            route.shell,
        );
    }

    #[test]
    fn route_and_flip_share_the_same_kamal_deploy_contract() {
        // For kamal-proxy the initial route and the health-gated flip are the
        // same `kamal-proxy deploy` command (only the label differs); a Caddy
        // controller is where they would diverge.
        let proxy = KamalProxyController::new(45);
        let (DeployOp::Run(route), DeployOp::Run(flip)) = (
            proxy.route_op("svc", "127.0.0.1:9001"),
            proxy.flip_op("svc", "127.0.0.1:9001"),
        ) else {
            panic!("route/flip must be Run ops");
        };
        assert_eq!(route.label, "proxy-route");
        assert_eq!(flip.label, "proxy-flip");
        assert_eq!(route.shell, flip.shell);
        assert!(flip.shell.contains("--deploy-timeout 45s"));
    }

    #[test]
    fn tls_host_wires_host_and_tls_into_deploy_and_leaves_run_unit_unchanged() {
        // Opt-in TLS (#1969): `[deploy.tls] enabled = true, host = "app.example.com"`
        // resolves to a controller with `tls_host = Some(..)`.
        let proxy = KamalProxyController::new(60).with_tls_host(Some("app.example.com".to_owned()));

        // The flip (and the identical route) carry `--host '<host>' --tls` in the
        // STABLE position between the health-check path and the timeouts. This is
        // the ONLY place TLS shows up — it turns TLS on for the app by making
        // kamal-proxy provision a Let's Encrypt cert on-demand for that host on
        // the always-bound 443.
        let DeployOp::Run(flip) = proxy.flip_op("myapp", "127.0.0.1:3002") else {
            panic!("flip_op must be a Run op");
        };
        assert_eq!(
            flip.shell,
            "env -u XDG_RUNTIME_DIR kamal-proxy deploy 'myapp' --target '127.0.0.1:3002' \
             --health-check-path '/ready' --host 'app.example.com' --tls \
             --deploy-timeout 60s --drain-timeout 30s",
        );
        let DeployOp::Run(route) = proxy.route_op("myapp", "127.0.0.1:3002") else {
            panic!("route_op must be a Run op");
        };
        assert_eq!(
            route.shell, flip.shell,
            "route and flip share the TLS contract"
        );

        // The supervised `run` unit passes ONLY `--http-port` — 443 is bound by
        // kamal-proxy's default HTTPS listener, so no explicit `--https-port` is
        // rendered. Enabling TLS therefore does not change the shared unit (see
        // `run_unit_is_tls_invariant`).
        let ops = proxy.ensure_installed_ops(8080);
        let DeployOp::WriteFile {
            contents: FileContents::Plain(unit),
            ..
        } = &ops[0]
        else {
            panic!("op 0 must write the proxy unit");
        };
        assert!(
            unit.contains("ExecStart=/usr/local/bin/kamal-proxy run --http-port 8080\n"),
            "run unit must pass only --http-port (443 is kamal-proxy's default), got: {unit}",
        );
        assert!(
            !unit.contains("--https-port"),
            "run unit must NOT pass an explicit --https-port, got: {unit}",
        );
    }

    #[test]
    fn run_unit_is_tls_invariant() {
        // The `run` unit passes only `--http-port` (443 is kamal-proxy's default
        // HTTPS listener, always bound), so the shared/global proxy unit is
        // byte-for-byte identical whether or not any app has TLS enabled — a
        // non-TLS app deploy never rewrites (and so never restarts) the shared
        // proxy.
        let http_only = KamalProxyController::new(60);
        let with_tls =
            KamalProxyController::new(60).with_tls_host(Some("app.example.com".to_owned()));

        let unit_for = |proxy: &KamalProxyController| {
            let ops = proxy.ensure_installed_ops(8080);
            let DeployOp::WriteFile {
                contents: FileContents::Plain(unit),
                ..
            } = &ops[0]
            else {
                panic!("op 0 must write the proxy unit");
            };
            unit.clone()
        };

        let http_only_unit = unit_for(&http_only);
        assert!(
            http_only_unit.contains("ExecStart=/usr/local/bin/kamal-proxy run --http-port 8080\n"),
            "run unit must pass only --http-port, got: {http_only_unit}",
        );
        assert!(
            !http_only_unit.contains("--https-port"),
            "run unit must NOT pass an explicit --https-port, got: {http_only_unit}",
        );
        assert_eq!(
            http_only_unit,
            unit_for(&with_tls),
            "the shared run unit must be identical regardless of any app's TLS flag",
        );
    }

    #[test]
    fn run_unit_persists_state_dir_so_routes_survive_reboot() {
        // kamal-proxy v0.9.2 `run` restores routes on boot from
        // `$HOME/.config/kamal-proxy/kamal-proxy.state`. A bare unit has no HOME
        // (state dir -> ephemeral /tmp), so the deploy-time route snapshot is wiped
        // on reboot and :80 comes back with no upstream. The `run` unit therefore
        // pins a persistent StateDirectory and points HOME at it so the deployed
        // route is restored after a power-cycle.
        let unit = KamalProxyController::render_proxy_unit(8080);
        assert!(
            unit.contains("StateDirectory=kamal-proxy\n"),
            "run unit must declare a persistent StateDirectory, got: {unit}",
        );
        assert!(
            unit.contains("Environment=HOME=/var/lib/kamal-proxy\n"),
            "run unit must point HOME at the persistent state dir, got: {unit}",
        );
        // Both lines live in the [Service] block (before [Install]).
        let service = unit
            .split("[Install]")
            .next()
            .expect("unit must have a [Service] section before [Install]");
        assert!(service.contains("StateDirectory=kamal-proxy\n"));
        assert!(service.contains("Environment=HOME=/var/lib/kamal-proxy\n"));

        // The persistent-state wiring is INDEPENDENT of the control socket: the
        // deploy-time CLI socket pin (#2019) writes no state and is untouched here.
        assert!(
            !unit.contains("XDG_RUNTIME_DIR"),
            "the run unit does not touch the control socket, got: {unit}",
        );
    }

    #[test]
    fn run_unit_state_dir_is_deterministic_across_renders() {
        // The persistent state dir must be stable across renders so successive
        // deploys/flips persist to (and a redeploy that changes the target updates)
        // the SAME state file — that is what makes the restored-on-boot route
        // reflect the latest deploy after a reboot.
        let a = KamalProxyController::render_proxy_unit(8080);
        let b = KamalProxyController::render_proxy_unit(8080);
        assert_eq!(a, b, "render_proxy_unit must be deterministic");
        // The state path is fixed regardless of the public port.
        let other_port = KamalProxyController::render_proxy_unit(9090);
        assert!(other_port.contains("StateDirectory=kamal-proxy\n"));
        assert!(other_port.contains("Environment=HOME=/var/lib/kamal-proxy\n"));
    }

    #[test]
    fn ensure_installed_supervises_proxy_on_public_port() {
        let proxy = KamalProxyController::new(60);
        let ops = proxy.ensure_installed_ops(8080);
        assert_eq!(ops.len(), 2);
        // Writes the proxy systemd unit (bound to the public port) directly to its
        // FINAL path — no staging path, no diff/`mv`, so concurrent shared-host
        // deploys can't race on a fixed staging file.
        match &ops[0] {
            DeployOp::WriteFile {
                label,
                contents,
                remote_path,
                ..
            } => {
                assert_eq!(*label, "proxy-write-unit");
                assert_eq!(remote_path, KAMAL_PROXY_UNIT_PATH);
                let FileContents::Plain(unit) = contents else {
                    panic!("proxy unit must be plain text");
                };
                assert!(
                    unit.contains("ExecStart=/usr/local/bin/kamal-proxy run --http-port 8080\n")
                );
                assert!(!unit.contains("--https-port"));
            }
            other => panic!("op 0 should write the proxy unit, got {other:?}"),
        }
        // …then a single idempotent daemon-reload + enable --now — no change-gated
        // restart (the unit is invariant to per-app TLS, so there is nothing to
        // restart to adopt).
        let DeployOp::Run(cmd) = &ops[1] else {
            panic!("op 1 must be the proxy-install Run op");
        };
        assert_eq!(cmd.label, "proxy-install");
        assert_eq!(
            cmd.shell,
            "systemctl daemon-reload && systemctl enable --now kamal-proxy.service",
        );
    }

    #[test]
    fn refresh_installed_ops_snapshots_installs_then_restarts_only_when_changed() {
        // Issue #2070: the redeploy refresh wraps the (unchanged) idempotent install
        // with a pre-write content snapshot and a change-gated restart that
        // re-registers the current live upstream — WITHOUT altering the shared
        // `ensure_installed_ops` shape (so the first-deploy path is untouched).
        let proxy = KamalProxyController::new(60);
        let ops = proxy.refresh_installed_ops(
            8080,
            "myapp",
            "127.0.0.1:3001",
            "/tmp/autumn-kamal-proxy-unit-REL.sha256",
            &ProxyServiceOptions {
                tls: false,
                host: None,
            },
        );
        assert_eq!(ops.len(), 4, "snapshot + write-unit + install + restart");

        // (1) snapshot the live unit's content hash into the scratch path.
        let DeployOp::Run(snapshot) = &ops[0] else {
            panic!("op 0 must be the snapshot Run op");
        };
        assert_eq!(snapshot.label, "proxy-snapshot-unit");
        assert_eq!(
            snapshot.shell,
            "{ sha256sum '/etc/systemd/system/kamal-proxy.service' 2>/dev/null || true; } \
             | cut -d' ' -f1 > '/tmp/autumn-kamal-proxy-unit-REL.sha256'",
        );

        // (2)+(3) the install ops are byte-for-byte `ensure_installed_ops` (write the
        // reboot-durable unit to its final path + the unchanged enable --now).
        let install = proxy.ensure_installed_ops(8080);
        match (&ops[1], &install[0]) {
            (
                DeployOp::WriteFile {
                    label: l1,
                    remote_path: p1,
                    contents: FileContents::Plain(u1),
                    ..
                },
                DeployOp::WriteFile {
                    contents: FileContents::Plain(u2),
                    ..
                },
            ) => {
                assert_eq!(*l1, "proxy-write-unit");
                assert_eq!(p1, KAMAL_PROXY_UNIT_PATH);
                assert_eq!(u1, u2, "refresh writes the same unit as first deploy");
                assert!(u1.contains("StateDirectory=kamal-proxy\n"));
                assert!(u1.contains("Environment=HOME=/var/lib/kamal-proxy\n"));
            }
            _ => panic!("op 1 must re-write the proxy unit"),
        }
        let DeployOp::Run(inst) = &ops[2] else {
            panic!("op 2 must be proxy-install");
        };
        assert_eq!(inst.label, "proxy-install");
        assert_eq!(
            inst.shell,
            "systemctl daemon-reload && systemctl enable --now kamal-proxy.service",
        );

        // (4) restart-if-changed: gate on a content change, only restart an ACTIVE
        // proxy, then re-register the current live upstream (socket-pinned, TLS-aware
        // via deploy_shell), with a bounded retry for the post-restart socket window.
        let DeployOp::Run(restart) = &ops[3] else {
            panic!("op 3 must be proxy-restart-if-changed");
        };
        assert_eq!(restart.label, "proxy-restart-if-changed");
        let s = &restart.shell;
        assert!(
            s.contains("if [ \"$new\" = \"$old\" ]; then exit 0; fi"),
            "unchanged unit → no restart: {s}"
        );
        assert!(
            s.contains("if ! systemctl is-active --quiet kamal-proxy.service; then exit 0; fi"),
            "restart only an active proxy: {s}"
        );
        assert!(
            s.contains("systemctl restart kamal-proxy.service"),
            "restart to adopt the changed unit: {s}"
        );
        assert!(
            s.contains(
                "env -u XDG_RUNTIME_DIR kamal-proxy deploy 'myapp' --target '127.0.0.1:3001' \
                 --health-check-path '/ready' --deploy-timeout 60s --drain-timeout 30s --force"
            ),
            "re-register the current live upstream, socket-pinned and forced (#2071): {s}"
        );
        // The re-register is FORCED (#2071): re-pinning the already-live upstream must
        // not block on a fresh readiness gate that a transient /ready blip could fail.
        assert!(s.contains("--force"), "re-register must be --force: {s}");
        assert!(
            s.contains("rm -f '/tmp/autumn-kamal-proxy-unit-REL.sha256'"),
            "the scratch snapshot is cleaned up: {s}"
        );
        // Order within the single command: gate → restart → re-register.
        let gate = s.find("if [ \"$new\" = \"$old\" ]").unwrap();
        let do_restart = s.find("systemctl restart kamal-proxy.service").unwrap();
        let reregister = s.find("kamal-proxy deploy 'myapp'").unwrap();
        assert!(
            gate < do_restart && do_restart < reregister,
            "gate→restart→re-register: {s}"
        );
    }

    #[test]
    fn refresh_installed_ops_tls_reregister_carries_host_and_tls() {
        // With TLS, the change-gated re-register carries the PRESERVED old release's
        // `--host <h> --tls` segment (issue #2074), so a restarted TLS proxy re-adopts
        // the live route with its own cert config intact. Here the preserved options
        // match the controller's config (a steady-state redeploy).
        let proxy = KamalProxyController::new(60).with_tls_host(Some("app.example.com".to_owned()));
        let ops = proxy.refresh_installed_ops(
            8080,
            "myapp",
            "127.0.0.1:3001",
            "/tmp/snap.sha256",
            &ProxyServiceOptions {
                tls: true,
                host: Some("app.example.com".to_owned()),
            },
        );
        let DeployOp::Run(restart) = ops.last().expect("has ops") else {
            panic!("last op must be the restart");
        };
        assert!(
            restart.shell.contains(
                "env -u XDG_RUNTIME_DIR kamal-proxy deploy 'myapp' --target '127.0.0.1:3001' \
                 --health-check-path '/ready' --host 'app.example.com' --tls \
                 --deploy-timeout 60s --drain-timeout 30s --force"
            ),
            "TLS re-register carries --host/--tls and --force: {}",
            restart.shell,
        );
    }

    #[test]
    fn refresh_reregister_preserves_old_options_while_flip_uses_new_host() {
        // #2074 PRESERVE: the operator changed `deploy.tls.host` to `new.example.com`,
        // but the still-live OLD release was registered under `old.example.com`. The
        // durability re-register must carry the OLD host (recovered from the marker),
        // while the candidate flip carries the NEW host — so a rollback leaves the old
        // release on its own host, not the new one.
        let proxy = KamalProxyController::new(60).with_tls_host(Some("new.example.com".to_owned()));
        let ops = proxy.refresh_installed_ops(
            8080,
            "myapp",
            "127.0.0.1:3001",
            "/tmp/snap.sha256",
            &ProxyServiceOptions {
                tls: true,
                host: Some("old.example.com".to_owned()),
            },
        );
        let DeployOp::Run(restart) = ops.last().expect("has ops") else {
            panic!("last op must be the restart/re-register");
        };
        assert!(
            restart.shell.contains("--host 'old.example.com' --tls")
                && !restart.shell.contains("new.example.com"),
            "re-register of the live OLD release preserves the OLD host, not the new: {}",
            restart.shell,
        );
        // The candidate flip still carries the NEW config host.
        let DeployOp::Run(flip) = proxy.flip_op("myapp", "127.0.0.1:3002") else {
            panic!("flip_op must be a Run op");
        };
        assert!(
            flip.shell.contains("--host 'new.example.com' --tls")
                && !flip.shell.contains("old.example.com"),
            "the candidate flip carries the NEW host: {}",
            flip.shell,
        );
    }

    #[test]
    fn refresh_reregister_can_preserve_tls_off_removing_a_host() {
        // #2074: a removed host is representable — preserving TLS-off options emits the
        // plain HTTP-only re-register (no `--host/--tls`), even when the controller's
        // NEW config turned TLS on. So a redeploy that ADDS a host never strands the
        // old (host-less) release behind the new host on a rollback.
        let proxy = KamalProxyController::new(60).with_tls_host(Some("new.example.com".to_owned()));
        let ops = proxy.refresh_installed_ops(
            8080,
            "myapp",
            "127.0.0.1:3001",
            "/tmp/snap.sha256",
            &ProxyServiceOptions {
                tls: false,
                host: None,
            },
        );
        let DeployOp::Run(restart) = ops.last().expect("has ops") else {
            panic!("last op must be the restart/re-register");
        };
        assert!(
            !restart.shell.contains("--host") && !restart.shell.contains("--tls "),
            "preserving TLS-off options emits an HTTP-only re-register: {}",
            restart.shell,
        );
    }

    #[test]
    fn proxy_service_options_mirror_the_configured_tls_host() {
        // The controller reports the options it actually registers, so the marker
        // written at deploy time matches what kamal-proxy was told (issue #2074).
        assert_eq!(
            KamalProxyController::new(60).proxy_service_options(),
            ProxyServiceOptions {
                tls: false,
                host: None,
            },
        );
        assert_eq!(
            KamalProxyController::new(60)
                .with_tls_host(Some("app.example.com".to_owned()))
                .proxy_service_options(),
            ProxyServiceOptions {
                tls: true,
                host: Some("app.example.com".to_owned()),
            },
        );
    }

    #[test]
    fn refresh_installed_ops_default_is_plain_install_no_restart() {
        // A controller that provisions its own pinned binary/config (e.g. a future
        // Caddy controller) inherits the trait default: refresh == ensure_installed,
        // with NO snapshot and NO restart op.
        struct PinnedController;
        impl ProxyController for PinnedController {
            fn ensure_installed_ops(&self, _public_port: u16) -> Vec<DeployOp> {
                vec![DeployOp::Run(RemoteCommand::new("install", "true"))]
            }
            fn route_op(&self, _service: &str, _upstream: &str) -> DeployOp {
                DeployOp::Run(RemoteCommand::new("route", "true"))
            }
            fn flip_op(&self, _service: &str, _new_upstream: &str) -> DeployOp {
                DeployOp::Run(RemoteCommand::new("flip", "true"))
            }
        }
        let refreshed = PinnedController.refresh_installed_ops(
            80,
            "svc",
            "127.0.0.1:9001",
            "/tmp/x",
            &ProxyServiceOptions {
                tls: false,
                host: None,
            },
        );
        let installed = PinnedController.ensure_installed_ops(80);
        assert_eq!(refreshed.len(), installed.len());
        assert!(
            refreshed
                .iter()
                .all(|op| op.label() != "proxy-restart-if-changed"
                    && op.label() != "proxy-snapshot-unit"),
            "the default refresh adds neither a snapshot nor a restart op",
        );
    }

    // --- CLI-surface compatibility probe (issue #2053) -----------------------

    /// A realistic `kamal-proxy deploy --help` capture carrying every flag the
    /// controller passes (base set + TLS flags). A compatible host looks like this.
    fn sample_deploy_help() -> &'static str {
        "Deploy a new version of a service\n\
         \n\
         Usage:\n  kamal-proxy deploy SERVICE [flags]\n\
         \n\
         Flags:\n\
         \x20     --target host:port            Target host and port to route to\n\
         \x20     --health-check-path string    Path kamal-proxy health-checks\n\
         \x20     --host strings                Host(s) to route\n\
         \x20     --tls                         Configure TLS for this service\n\
         \x20     --deploy-timeout duration     How long to wait for the target\n\
         \x20     --drain-timeout duration      How long to drain the old target\n\
         \x20     --force                       Skip health checks and force deployment\n"
    }

    #[test]
    fn compat_probe_command_is_the_readonly_deploy_help_check() {
        let cmd = KamalProxyController::compat_probe_command();
        assert_eq!(cmd.label, "proxy-compat-probe");
        // A side-effect-free `--help` on the cutover subcommand, combined streams,
        // never failing the ssh step (the Rust verdict decides compatibility).
        assert_eq!(cmd.shell, "kamal-proxy deploy --help 2>&1 || true");
    }

    #[test]
    fn required_flags_track_tls_config() {
        let http_only = KamalProxyController::new(60);
        assert_eq!(
            http_only.required_deploy_flags(),
            vec![
                "--target",
                "--health-check-path",
                "--deploy-timeout",
                "--drain-timeout",
                // The post-restart re-register (#2071) emits `--force`, so the probe
                // guards it too.
                "--force",
            ],
        );
        let with_tls =
            KamalProxyController::new(60).with_tls_host(Some("app.example.com".to_owned()));
        assert_eq!(
            with_tls.required_deploy_flags(),
            vec![
                "--target",
                "--health-check-path",
                "--deploy-timeout",
                "--drain-timeout",
                "--host",
                "--tls",
                "--force",
            ],
        );
        // Every required flag is one deploy_shell actually emits (checking the
        // re-register form, `force = true`, which is the superset that emits every
        // flag including `--force`), so the contract can never drift from what the
        // controller passes.
        let reregister_shell = with_tls.deploy_shell("svc", "127.0.0.1:3001", true);
        for flag in with_tls.required_deploy_flags() {
            assert!(
                reregister_shell.contains(flag),
                "required flag {flag} must be one deploy_shell emits, got: {reregister_shell}",
            );
        }
    }

    #[test]
    fn compatible_help_passes_silently() {
        // A host whose kamal-proxy still has every flag we use passes with no error
        // (MUST NOT break hosts that are already fine).
        assert_eq!(
            KamalProxyController::new(60).assess_deploy_help(sample_deploy_help()),
            Ok(()),
        );
        // Non-TLS controller does not require --host/--tls, so a help capture that
        // lacks them is still compatible for it.
        let no_tls_help = "Flags:\n  --target x\n  --health-check-path p\n  \
                           --deploy-timeout d\n  --drain-timeout d\n  --force\n";
        assert_eq!(
            KamalProxyController::new(60).assess_deploy_help(no_tls_help),
            Ok(()),
        );
    }

    #[test]
    fn tls_controller_requires_tls_flags() {
        // With TLS enabled, a help capture missing --tls is incompatible for THIS
        // controller even though a non-TLS controller would accept it. (`--force` is
        // present so the ONLY missing flag under test is `--tls`.)
        let no_tls_help = "Flags:\n  --target x\n  --health-check-path p\n  \
                           --deploy-timeout d\n  --drain-timeout d\n  --host h\n  --force\n";
        let with_tls =
            KamalProxyController::new(60).with_tls_host(Some("app.example.com".to_owned()));
        assert_eq!(
            with_tls.assess_deploy_help(no_tls_help),
            Err(KamalProxyCompatIssue::MissingFlags(vec!["--tls"])),
        );
    }

    #[test]
    fn a_renamed_flag_is_caught_with_the_exact_missing_flag() {
        // Simulate a future kamal-proxy that renamed --drain-timeout.
        let drifted = sample_deploy_help().replace("--drain-timeout", "--drain-window");
        let issue = KamalProxyController::new(60)
            .assess_deploy_help(&drifted)
            .expect_err("a renamed flag must be caught");
        assert_eq!(
            issue,
            KamalProxyCompatIssue::MissingFlags(vec!["--drain-timeout"]),
        );
        // The operator message names the missing flag, the pinned known-good
        // version, and states nothing was cut over.
        let msg = issue.message();
        assert!(
            msg.contains("--drain-timeout"),
            "message names the flag: {msg}"
        );
        assert!(msg.contains("v0.9.2"), "message names the pin: {msg}");
        assert!(
            msg.contains("before any cutover"),
            "message states nothing was cut over: {msg}",
        );
    }

    #[test]
    fn valid_help_with_benign_stderr_noise_still_passes() {
        // The probe folds stderr into the capture via 2>&1. A healthy target whose
        // login shell prints startup noise — here a .bashrc line failing with
        // `command not found` — must NOT block the deploy when `deploy --help`
        // itself rendered valid help (AC: never break a host that is already fine).
        let noisy = format!("bash: nvm: command not found\n{}", sample_deploy_help());
        assert_eq!(
            KamalProxyController::new(60).assess_deploy_help(&noisy),
            Ok(()),
            "valid help alongside unrelated stderr noise must pass",
        );
        // Same for a TLS controller (which requires the extra --host/--tls flags).
        let with_tls =
            KamalProxyController::new(60).with_tls_host(Some("app.example.com".to_owned()));
        assert_eq!(with_tls.assess_deploy_help(&noisy), Ok(()));
    }

    #[test]
    fn a_renamed_flag_amid_stderr_noise_is_still_caught_as_missing_not_unusable() {
        // The inverse guard: noise must not tip a genuine flag drift into a false
        // BinaryUnusable. Only --drain-timeout is renamed, so other flags render →
        // the help clearly printed, and the "command not found" noise is irrelevant.
        let drifted = sample_deploy_help().replace("--drain-timeout", "--drain-window");
        let noisy = format!("bash: nvm: command not found\n{drifted}");
        assert_eq!(
            KamalProxyController::new(60).assess_deploy_help(&noisy),
            Err(KamalProxyCompatIssue::MissingFlags(vec!["--drain-timeout"])),
        );
    }

    #[test]
    fn a_removed_deploy_subcommand_is_caught() {
        // cobra's error when `deploy` is renamed/removed.
        let output = "Error: unknown command \"deploy\" for \"kamal-proxy\"\n\
                      Run 'kamal-proxy --help' for usage.\n";
        let issue = KamalProxyController::new(60)
            .assess_deploy_help(output)
            .expect_err("a removed deploy subcommand must be caught");
        assert_eq!(issue, KamalProxyCompatIssue::DeploySubcommandMissing);
        assert!(issue.message().contains("no `deploy` subcommand"));
    }

    #[test]
    fn a_missing_binary_is_caught() {
        for output in [
            "bash: kamal-proxy: command not found\n",
            "/usr/local/bin/kamal-proxy: No such file or directory\n",
            // Non-executable file (bad mode) → the binary couldn't run.
            "bash: /usr/local/bin/kamal-proxy: Permission denied\n",
            // Wrong-arch / not a real binary (ENOEXEC) → couldn't run.
            "bash: /usr/local/bin/kamal-proxy: cannot execute binary file: Exec format error\n",
            "", // no output at all → cannot confirm → fail closed
        ] {
            assert_eq!(
                KamalProxyController::new(60).assess_deploy_help(output),
                Err(KamalProxyCompatIssue::BinaryUnusable),
                "output {output:?} must classify as an unusable binary",
            );
        }
        let msg = KamalProxyCompatIssue::BinaryUnusable.message();
        assert!(
            msg.contains("/usr/local/bin/kamal-proxy"),
            "names the path: {msg}"
        );
        assert!(msg.contains("v0.9.2"), "names the pin: {msg}");
    }

    #[test]
    fn a_prefix_renamed_flag_is_caught_by_the_boundary_matcher() {
        // A future kamal-proxy that renamed --target to --target-host: a plain
        // substring check would find "--target" inside "--target-host" and wrongly
        // pass, silently defeating the guardrail. The boundary matcher rejects the
        // superstring and reports the exact missing flag.
        let drifted = sample_deploy_help().replace("--target ", "--target-host ");
        assert!(
            drifted.contains("--target-host") && !drifted.contains("--target "),
            "fixture must carry the renamed superstring and no standalone --target",
        );
        assert_eq!(
            KamalProxyController::new(60).assess_deploy_help(&drifted),
            Err(KamalProxyCompatIssue::MissingFlags(vec!["--target"])),
        );
        // And the boundary matcher itself: a standalone flag matches, a superstring
        // does not, whichever side the extra token sits on.
        assert!(output_lists_flag("  --tls    Configure TLS\n", "--tls"));
        // Trailing boundary: a superstring flag is not a match.
        assert!(!output_lists_flag("  --tls-enabled  ...\n", "--tls"));
        // Leading boundary: a flag glued onto a preceding token is not a match.
        assert!(!output_lists_flag("run--tls now", "--tls"));
        // A `=value` form (no trailing space) is still a standalone flag.
        assert!(output_lists_flag("--target=host:port", "--target"));
    }

    #[test]
    fn compat_probe_trait_method_wires_command_and_verdict() {
        let probe = KamalProxyController::new(60)
            .compat_probe()
            .expect("kamal-proxy declares a compat probe");
        assert_eq!(
            probe.command.shell,
            "kamal-proxy deploy --help 2>&1 || true"
        );
        assert_eq!(probe.assess(sample_deploy_help()), Ok(()));
        let drifted = sample_deploy_help().replace("--target", "--upstream");
        let err = probe
            .assess(&drifted)
            .expect_err("drift must fail the verdict");
        assert!(
            err.message.contains("--target"),
            "verdict message names the flag: {}",
            err.message
        );
        // #1607: a RESPONDING binary is never a host-prep case, so the caller must
        // not be told it can fix this by installing one.
        assert!(
            !err.binary_missing,
            "flag drift is not a missing binary: {err:?}"
        );
    }

    #[test]
    fn the_verdict_reports_a_missing_binary_as_a_host_prep_case() {
        // #1607 (AC-1): an empty capture is "nothing answered" — the one case
        // `autumn deploy` fixes itself by installing the pinned build.
        let probe = KamalProxyController::new(60)
            .compat_probe()
            .expect("kamal-proxy declares a compat probe");
        let err = probe
            .assess("")
            .expect_err("no binary must fail the verdict");
        assert!(err.binary_missing, "an unusable binary is host prep's case");
        assert!(
            err.message.contains("install_proxy"),
            "the message names the opt-out that would decline the fix: {}",
            err.message
        );
    }

    #[test]
    fn the_install_op_lands_the_pinned_binary_atomically_at_the_supervised_path() {
        // #1607 (AC-1): host preparation must be safe to run against a stock Ubuntu
        // LTS host — non-interactive, idempotent about an existing container
        // runtime, and never leaving a half-written file at the path the proxy unit
        // executes.
        let ops = KamalProxyController::new(60)
            .binary_install_ops()
            .expect("kamal-proxy can prepare a host");
        assert_eq!(ops.len(), 1, "host prep is a single remote command");
        let DeployOp::Run(cmd) = &ops[0] else {
            panic!("host prep runs a command, it uploads nothing");
        };
        assert_eq!(cmd.label, "install-proxy");
        let shell = &cmd.shell;
        assert!(
            shell.contains(KAMAL_PROXY_KNOWN_GOOD_VERSION),
            "the image tag is pinned, never `latest`: {shell}"
        );
        assert!(
            !shell.contains(":latest"),
            "an unpinned tag would make host prep non-reproducible: {shell}"
        );
        assert!(
            shell.contains("command -v docker"),
            "an existing container runtime is reused: {shell}"
        );
        // The readiness gate polls `/ready` with `curl` ON THE HOST, so a minimal
        // image without curl must be equipped here or every deploy times out at the
        // gate. Both packages are probed individually so an equipped host installs
        // nothing.
        assert!(
            shell.contains("command -v curl") && shell.contains("curl\""),
            "host prep must ensure the readiness gate's curl: {shell}"
        );
        assert!(
            shell.contains("DEBIAN_FRONTEND=noninteractive"),
            "apt must never prompt mid-deploy: {shell}"
        );
        // Staged then moved: the supervised path is never a partially-copied file.
        let staged = format!("{KAMAL_PROXY_BIN}.autumn-new");
        assert!(
            shell.contains(&format!("chmod 755 '{staged}'"))
                && shell.contains(&format!("mv -f '{staged}' '{KAMAL_PROXY_BIN}'")),
            "the binary is staged, marked executable, then moved into place: {shell}"
        );
        assert!(
            shell.starts_with("( set -e;"),
            "any failing step must fail the whole op: {shell}"
        );
        // Digest-pinned, so a re-pushed tag (or a poisoned local image) cannot
        // become a root-executed binary on the target.
        assert!(
            shell.contains(&format!(
                "{KAMAL_PROXY_KNOWN_GOOD_VERSION}@{KAMAL_PROXY_KNOWN_GOOD_DIGEST}"
            )),
            "the image is pinned by DIGEST, not just by tag: {shell}"
        );
        // Never clobbers: the invariant is enforced at the point of action, not
        // merely inferred from the probe verdict upstream.
        assert!(
            shell.contains(&format!("if [ -e '{KAMAL_PROXY_BIN}' ]"))
                && shell.contains("refusing to replace it"),
            "the install must refuse to replace an existing binary: {shell}"
        );
        // Any failure carries the remedy — this is the most likely new failure on a
        // stock host, and the raw apt/docker stderr alone says nothing useful.
        assert!(
            shell.contains("install_proxy = false") && shell.contains("outbound HTTPS"),
            "a failed install must name what the host needs and the opt-out: {shell}"
        );
    }

    #[test]
    fn the_install_op_is_a_valid_posix_shell_script() {
        // The install shell is the one op with no PR-time end-to-end coverage (the
        // container fixture bakes kamal-proxy in, so the e2e always takes the
        // already-equipped path, and only the nightly real-VPS job runs it for
        // real). A syntax check is cheap and catches the whole class of quoting and
        // structure regressions on every PR.
        let shell = KamalProxyController::install_shell();
        let out = std::process::Command::new("sh")
            .arg("-n")
            .arg("-c")
            .arg(&shell)
            .output()
            .expect("spawn sh -n");
        assert!(
            out.status.success(),
            "the generated install shell must parse as POSIX sh:\n{shell}\n--- sh said ---\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        // Self-verifying: an install that lands nothing usable fails HERE rather
        // than at the first cutover.
        assert!(
            shell.contains(&format!(
                "'{KAMAL_PROXY_BIN}' deploy --help >/dev/null 2>&1 ||"
            )),
            "the install proves the binary at the SUPERVISED path works — verifying \
             the staging path would prove nothing about what was installed: {shell}"
        );
    }
}
