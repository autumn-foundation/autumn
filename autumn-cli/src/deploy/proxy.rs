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
}

/// Default drain window for the old target after a swap.
const DEFAULT_DRAIN_TIMEOUT_SECS: u64 = 30;

impl KamalProxyController {
    /// Build a controller whose flip health-check points at the candidate's
    /// `/ready` and whose deploy timeout matches the deploy's readiness window.
    #[must_use]
    pub fn new(readiness_timeout_secs: u64) -> Self {
        Self {
            health_check_path: "/ready".to_owned(),
            deploy_timeout_secs: readiness_timeout_secs,
            drain_timeout_secs: DEFAULT_DRAIN_TIMEOUT_SECS,
        }
    }

    /// The single `kamal-proxy deploy` invocation shared by the initial route and
    /// the health-gated flip. Centralized so the exact CLI contract lives in one
    /// place (a Caddy controller would replace THIS with an admin-API call).
    fn deploy_shell(&self, service: &str, target: &str) -> String {
        format!(
            "kamal-proxy deploy {service} --target {target} \
             --health-check-path {path} --deploy-timeout {deploy}s --drain-timeout {drain}s",
            service = shell_quote(service),
            target = target,
            path = self.health_check_path,
            deploy = self.deploy_timeout_secs,
            drain = self.drain_timeout_secs,
        )
    }

    /// Render the systemd unit that supervises `kamal-proxy run` on the public
    /// port. Pure — exposed for unit assertions.
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
            DeployOp::WriteFile {
                label: "proxy-write-unit",
                contents: FileContents::Plain(Self::render_proxy_unit(public_port)),
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
            "kamal-proxy deploy 'myapp' --target 127.0.0.1:3002 \
             --health-check-path /ready --deploy-timeout 60s --drain-timeout 30s",
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
