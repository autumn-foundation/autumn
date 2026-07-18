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
//! - The exact `kamal-proxy` binary provisioning (download/version pin) is left
//!   to host bootstrap; [`KamalProxyController::ensure_installed_ops`] supervises
//!   it via systemd and assumes the binary is on `PATH`/`/usr/local/bin`.
//! - Live ssh is not exercised here (Slice 4's CI container harness).

use super::exec::{DeployOp, FileContents, RemoteCommand, shell_quote};

/// Absolute path the proxy binary is expected at on the target.
const KAMAL_PROXY_BIN: &str = "/usr/local/bin/kamal-proxy";

/// Systemd unit path supervising the proxy process.
const KAMAL_PROXY_UNIT_PATH: &str = "/etc/systemd/system/kamal-proxy.service";

/// Known-good kamal-proxy version this controller's CLI contract was verified
/// against — the same version the real-VPS validation harness pins
/// (`scripts/deploy-real-vps-validate.sh`, issue #2052). Named in the
/// incompatibility message (issue #2053) so an operator knows exactly what to
/// install on host bootstrap. kamal-proxy is otherwise consumed UNPINNED from
/// host bootstrap, so this is the version to pin to when the compat probe fails.
pub const KAMAL_PROXY_KNOWN_GOOD_VERSION: &str = "v0.9.2";

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
/// output: `Ok(())` when compatible, `Err(message)` with an actionable operator
/// message otherwise.
type CompatVerdict = Box<dyn Fn(&str) -> Result<(), String> + Send + Sync>;

impl ProxyCompatProbe {
    /// Build a probe from its command and a pure verdict closure.
    #[must_use]
    pub fn new(
        command: RemoteCommand,
        verdict: impl Fn(&str) -> Result<(), String> + Send + Sync + 'static,
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
    /// Returns the actionable operator message when the installed proxy's CLI
    /// surface is incompatible with what the cutover requires.
    pub fn assess(&self, output: &str) -> Result<(), String> {
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

    /// The single `kamal-proxy deploy` invocation shared by the initial route and
    /// the health-gated flip. Centralized so the exact CLI contract lives in one
    /// place (a Caddy controller would replace THIS with an admin-API call).
    fn deploy_shell(&self, service: &str, target: &str) -> String {
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
        let tls = self.tls_host.as_deref().map_or_else(String::new, |host| {
            format!("--host {host} --tls ", host = shell_quote(host))
        });
        format!(
            "env -u XDG_RUNTIME_DIR kamal-proxy deploy {service} --target {target} \
             --health-check-path {path} {tls}--deploy-timeout {deploy}s --drain-timeout {drain}s",
            service = shell_quote(service),
            target = shell_quote(target),
            path = shell_quote(&self.health_check_path),
            deploy = self.deploy_timeout_secs,
            drain = self.drain_timeout_secs,
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
    /// output — the base set plus the TLS flags when TLS is enabled.
    #[must_use]
    pub fn required_deploy_flags(&self) -> Vec<&'static str> {
        let mut flags = REQUIRED_DEPLOY_FLAGS.to_vec();
        if self.tls_host.is_some() {
            flags.extend_from_slice(REQUIRED_TLS_DEPLOY_FLAGS);
        }
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
        DeployOp::Run(RemoteCommand::new(
            "proxy-route",
            self.deploy_shell(service, upstream),
        ))
    }

    fn flip_op(&self, service: &str, new_upstream: &str) -> DeployOp {
        DeployOp::Run(RemoteCommand::new(
            "proxy-flip",
            self.deploy_shell(service, new_upstream),
        ))
    }

    fn compat_probe(&self) -> Option<ProxyCompatProbe> {
        // Capture the required flag set (which depends on this controller's TLS
        // config) into the pure verdict closure, so the probe is self-contained
        // and Caddy-swappable through the trait.
        let required = self.required_deploy_flags();
        Some(ProxyCompatProbe::new(
            Self::compat_probe_command(),
            move |output| {
                assess_kamal_proxy_deploy_help(output, &required).map_err(|issue| issue.message())
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
                 `kamal-proxy deploy --help` (missing or not executable). Install a \
                 known-good kamal-proxy (pin {pin}) in the target's host bootstrap \
                 before deploying — see scripts/deploy-real-vps-validate.sh. Aborting \
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

/// Does the probe output signal that the `kamal-proxy` binary itself is absent or
/// not executable (as opposed to responding with help/usage text)? Matches the
/// common shell/OS "not found" phrasings across `bash`/`sh`/`env` so a missing
/// binary is classified as unusable rather than misread as "missing every flag".
fn output_signals_missing_binary(output: &str) -> bool {
    let lower = output.to_ascii_lowercase();
    lower.contains("command not found")
        || lower.contains("no such file or directory")
        || lower.contains("kamal-proxy: not found")
        || lower.contains("executable file not found")
}

/// Pure verdict on a `kamal-proxy deploy --help` capture given the flags the
/// caller requires (issue #2053).
///
/// A compatible surface — every required flag present, `deploy` intact, a real
/// binary — returns `Ok(())` (the deploy proceeds untouched). Otherwise it
/// classifies the failure so the caller can fail closed with a precise message.
/// The checks are ordered so the most specific cause wins: binary-missing, then a
/// renamed/removed `deploy` subcommand, then empty output, then absent flags.
fn assess_kamal_proxy_deploy_help(
    output: &str,
    required_flags: &[&'static str],
) -> Result<(), KamalProxyCompatIssue> {
    if output_signals_missing_binary(output) {
        return Err(KamalProxyCompatIssue::BinaryUnusable);
    }
    // cobra prints `Error: unknown command "deploy" for "kamal-proxy"` (to stderr,
    // folded in via 2>&1) when the subcommand was renamed/removed.
    let lower = output.to_ascii_lowercase();
    if lower.contains("unknown command") && lower.contains("deploy") {
        return Err(KamalProxyCompatIssue::DeploySubcommandMissing);
    }
    // No output at all: we cannot confirm the surface, so fail closed rather than
    // assume compatibility.
    if output.trim().is_empty() {
        return Err(KamalProxyCompatIssue::BinaryUnusable);
    }
    let missing: Vec<&'static str> = required_flags
        .iter()
        .copied()
        .filter(|flag| !output.contains(*flag))
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(KamalProxyCompatIssue::MissingFlags(missing))
    }
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
         \x20     --drain-timeout duration      How long to drain the old target\n"
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
            ],
        );
        // Every required flag is one deploy_shell actually emits, so the contract
        // can never drift from what the controller passes.
        let flip_shell = with_tls.deploy_shell("svc", "127.0.0.1:3001");
        for flag in with_tls.required_deploy_flags() {
            assert!(
                flip_shell.contains(flag),
                "required flag {flag} must be one deploy_shell emits, got: {flip_shell}",
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
                           --deploy-timeout d\n  --drain-timeout d\n";
        assert_eq!(
            KamalProxyController::new(60).assess_deploy_help(no_tls_help),
            Ok(()),
        );
    }

    #[test]
    fn tls_controller_requires_tls_flags() {
        // With TLS enabled, a help capture missing --tls is incompatible for THIS
        // controller even though a non-TLS controller would accept it.
        let no_tls_help = "Flags:\n  --target x\n  --health-check-path p\n  \
                           --deploy-timeout d\n  --drain-timeout d\n  --host h\n";
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
            err.contains("--target"),
            "verdict message names the flag: {err}"
        );
    }
}
