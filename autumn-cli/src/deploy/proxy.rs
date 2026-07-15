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
    /// --tls` and the supervised `run` unit opens the HTTPS port. `None` (the
    /// default) leaves every command byte-for-byte HTTP-only.
    tls_host: Option<String>,
}

/// Default drain window for the old target after a swap.
const DEFAULT_DRAIN_TIMEOUT_SECS: u64 = 30;

/// HTTPS port the proxy terminates TLS on when `[deploy.tls]` is enabled
/// (kamal-proxy provisions the certificate via Let's Encrypt / ACME).
const DEFAULT_HTTPS_PORT: u16 = 443;

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
        // TLS (opt-in): `--host <host> --tls` sits in a STABLE position between
        // the health-check path and the timeouts. When `tls_host` is `None` the
        // segment is empty and the command is byte-for-byte the HTTP-only form.
        let tls = self.tls_host.as_deref().map_or_else(String::new, |host| {
            format!("--host {host} --tls ", host = shell_quote(host))
        });
        format!(
            "kamal-proxy deploy {service} --target {target} \
             --health-check-path {path} {tls}--deploy-timeout {deploy}s --drain-timeout {drain}s",
            service = shell_quote(service),
            target = shell_quote(target),
            path = shell_quote(&self.health_check_path),
            deploy = self.deploy_timeout_secs,
            drain = self.drain_timeout_secs,
        )
    }

    /// Render the systemd unit that supervises `kamal-proxy run` on the public
    /// port. Pure — exposed for unit assertions.
    ///
    /// When `tls_enabled`, the `run` command also opens `--https-port 443` so the
    /// proxy terminates TLS there (automatic Let's Encrypt via kamal-proxy's
    /// built-in `--tls`). When disabled the `ExecStart` is byte-for-byte the
    /// HTTP-only form.
    #[must_use]
    pub fn render_proxy_unit(public_port: u16, tls_enabled: bool) -> String {
        let https = if tls_enabled {
            format!(" --https-port {DEFAULT_HTTPS_PORT}")
        } else {
            String::new()
        };
        format!(
            "[Unit]\n\
             Description=kamal-proxy (Autumn deploy front)\n\
             After=network-online.target\n\
             Wants=network-online.target\n\
             \n\
             [Service]\n\
             Type=simple\n\
             ExecStart={KAMAL_PROXY_BIN} run --http-port {public_port}{https}\n\
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
            DeployOp::WriteFile {
                label: "proxy-write-unit",
                contents: FileContents::Plain(Self::render_proxy_unit(
                    public_port,
                    self.tls_host.is_some(),
                )),
                remote_path: KAMAL_PROXY_UNIT_PATH.to_owned(),
                mode: Some(0o644),
            },
            DeployOp::Run(RemoteCommand::new(
                "proxy-install",
                "systemctl daemon-reload && systemctl enable --now kamal-proxy.service",
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
            "kamal-proxy deploy 'myapp' --target '127.0.0.1:3002' \
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
    fn tls_host_wires_host_and_tls_into_deploy_and_https_port_into_run() {
        // Opt-in TLS (#1969): `[deploy.tls] enabled = true, host = "app.example.com"`
        // resolves to a controller with `tls_host = Some(..)`.
        let proxy = KamalProxyController::new(60).with_tls_host(Some("app.example.com".to_owned()));

        // The flip (and the identical route) carry `--host '<host>' --tls` in the
        // STABLE position between the health-check path and the timeouts.
        let DeployOp::Run(flip) = proxy.flip_op("myapp", "127.0.0.1:3002") else {
            panic!("flip_op must be a Run op");
        };
        assert_eq!(
            flip.shell,
            "kamal-proxy deploy 'myapp' --target '127.0.0.1:3002' \
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

        // The supervised `run` unit opens the HTTPS port alongside the HTTP port so
        // the proxy actually terminates TLS on 443.
        let ops = proxy.ensure_installed_ops(8080);
        let DeployOp::WriteFile {
            contents: FileContents::Plain(unit),
            ..
        } = &ops[0]
        else {
            panic!("op 0 must write the proxy unit");
        };
        assert!(
            unit.contains("kamal-proxy run --http-port 8080 --https-port 443"),
            "TLS run unit must open both ports, got: {unit}",
        );
    }

    #[test]
    fn ensure_installed_supervises_proxy_on_public_port() {
        let proxy = KamalProxyController::new(60);
        let ops = proxy.ensure_installed_ops(8080);
        assert_eq!(ops.len(), 2);
        // First writes the proxy systemd unit bound to the public port…
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
                assert!(unit.contains("kamal-proxy run --http-port 8080"));
            }
            other => panic!("op 0 should write the proxy unit, got {other:?}"),
        }
        // …then daemon-reloads and enables it.
        assert_eq!(ops[1].label(), "proxy-install");
    }
}
