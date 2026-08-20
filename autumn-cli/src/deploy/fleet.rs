//! Fleet planning for `autumn deploy` (issue #1621) — the PURE layer.
//!
//! A fleet rollout is a **thin, pure orchestration shell above unchanged per-host
//! op builders**. Nothing below [`exec::SshTarget`] moves: this module decides
//! *which* host does *what*, and then delegates to [`exec::first_deploy_ops`] /
//! [`exec::cutover_ops`] verbatim. Everything here is a pure function of its
//! inputs — no I/O, no clock, no host contact — so the "no mixed versions"
//! reasoning is unit-testable against synthetic probe modes with no server.
//!
//! Three rules are load-bearing and easy to break by accident:
//!
//! 1. **Each host's ops stay in their OWN `Vec`.** No code path may concatenate
//!    two hosts' [`exec::DeployOp`] vectors. [`exec::execute_with_teardown`]
//!    resolves the auto-rollback boundary with
//!    `ops.iter().position(|op| op.label() == boundary_label)` — the FIRST match —
//!    so a flat fleet-wide vector would classify every later host's *pre*-flip
//!    failure as post-boundary and silently disable teardown, leaving a candidate
//!    half-installed with no rollback. [`host_ops`] therefore returns one vector
//!    per [`HostPlan`], and `every_fleet_host_vector_carries_exactly_one_boundary_label`
//!    is the tripwire.
//!
//! 2. **Host identity NEVER enters [`exec::RemoteCommand::label`].** Labels are
//!    `&'static str` and are load-bearing three times over: the boundary lookup
//!    above, `CandidateRolledBack { failed_step }`, and every exact-vector test.
//!    Making a label host-specific would need a `Box::leak` (an unbounded
//!    per-host-per-run leak with non-deterministic identity that clippy will not
//!    flag) and would break boundary matching. The host travels in this module's
//!    plan/result types — never in an op.
//!
//! 3. **Exactly ONE `release_id` per fleet run.** It is minted once by the driver
//!    and threaded into every host's builders, so every host's `current` symlink
//!    resolves to the same release, drift reporting is meaningful, and a rollback
//!    has a single target. Regenerating per host would give un-comparable version
//!    identities and permanent reported drift.
//!
//! The migration is scheduled by [`migrate_placement`]: the first host in rollout
//! order that is a *redeploy*, since a first-deploy host has no live release to
//! keep serving if the migration fails, and [`exec::first_deploy_ops`] carries no
//! migrate op at all. Every other host builds with [`exec::MigrateStep::Skip`].

// Every item here is crate-internal by design (see the note on `mod fleet` in
// `deploy.rs`). In this bin-only crate `deploy` is a private module, so clippy
// flags each `pub(crate)` as redundant; they are kept to document the intended
// visibility rather than widening to `pub`.
#![allow(clippy::redundant_pub_crate)]
// The planning types and the ops helper are consumed by this module's tests and
// by the plan drift guard today; the serial rollout DRIVER that consumes them in
// production lands in the next slice of #1621 (mirroring how `ResolvedFleet`
// itself was landed).
#![cfg_attr(not(test), allow(dead_code))]

use std::path::Path;

use super::exec::{
    self, DeployOp, ManifestUpload, MigrateStep, ProxyServiceOptions, Secret, SlotPlan,
};
use super::proxy::ProxyController;
use super::{ResolvedDeployConfig, ResolvedFleet};

/// The single fleet-wide sentence `deploy plan` prints about migrate placement.
///
/// Held as a constant so the plan text and the drift guard that checks it against
/// the real op sequence (`fleet_plan_matches_fleet_ops_sequence`) can never
/// disagree about what was promised.
pub(crate) const FLEET_MIGRATE_PLACEMENT_NOTE: &str = "[migrate] runs once, on the first host still on a previous release, before its \
     cutover — hosts 2..N skip it";

/// Which path a single host takes in a fleet rollout.
///
/// This is the payload-free projection of [`exec::DeployMode`]: the planner only
/// needs to know *first deploy vs redeploy*, while the live slot that
/// `DeployMode::Redeploy` carries is a per-host detail belonging to that host's
/// [`SlotPlan`], resolved by the driver from the same probe. Keeping the planning
/// input this small is what makes the whole module testable from synthetic values
/// with no probe round-trip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HostMode {
    /// No promoted `current` release yet — [`exec::first_deploy_ops`].
    First,
    /// A `current` release already serves — [`exec::cutover_ops`].
    Redeploy,
}

impl HostMode {
    /// Project a probed [`exec::DeployMode`] onto the planning input. A thin
    /// mapping (rather than reusing `DeployMode` directly) keeps the planner
    /// decoupled from the probe's slot payload.
    pub(crate) const fn from_deploy_mode(mode: &exec::DeployMode) -> Self {
        match mode {
            exec::DeployMode::First => Self::First,
            exec::DeployMode::Redeploy { .. } => Self::Redeploy,
        }
    }
}

/// Where in the rollout the single fleet-wide migration runs (issue #1621, AC-4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MigratePlacement {
    /// Index, in rollout order, of the first host taking the redeploy path.
    FirstRedeploy(usize),
    /// No host in this fleet is a redeploy, so nothing migrates — today's
    /// documented "the first deploy does not migrate" limitation, generalised.
    None,
}

/// The index of the first host in rollout order whose mode is
/// [`HostMode::Redeploy`], or [`MigratePlacement::None`].
///
/// Pure and total: an empty slice, or one with no redeploy, yields `None`.
pub(crate) fn migrate_placement(modes: &[HostMode]) -> MigratePlacement {
    modes
        .iter()
        .position(|mode| matches!(mode, HostMode::Redeploy))
        .map_or(MigratePlacement::None, MigratePlacement::FirstRedeploy)
}

/// What one host does in a fleet rollout.
///
/// Deliberately NOT the host's ops: the ops are built from this plus that host's
/// resolved config and slot layout (see [`host_ops`]), one vector per host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HostPlan {
    /// The SSH-reachable address, carried here — never in an op label (rule 2).
    pub(crate) host: String,
    /// Which per-host builder this host takes.
    pub(crate) mode: HostMode,
    /// Whether this host carries the fleet's single migration.
    pub(crate) migrate: MigrateStep,
}

/// The whole rollout, one entry per host, in rollout (declaration) order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FleetPlan {
    /// Per-host plans, in rollout order.
    pub(crate) hosts: Vec<HostPlan>,
}

impl FleetPlan {
    /// The host carrying the fleet's single migration, if any.
    pub(crate) fn migrating_host(&self) -> Option<&HostPlan> {
        self.hosts
            .iter()
            .find(|h| matches!(h.migrate, MigrateStep::Run))
    }
}

/// Why a fleet could not be planned.
///
/// Planning is pure and **total**: it returns a typed refusal rather than
/// panicking, because a panic here would abort a CLI process that may already
/// have cut hosts over. The operator-facing fleet-wide refusals (`SQLite` fleet,
/// media fleet, TLS fleet) are prologue concerns and land with the driver.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum FleetPlanError {
    /// The probe-mode list did not line up with the resolved host list — a caller
    /// bug (the two are produced together), reported instead of indexed past.
    #[error(
        "fleet plan input mismatch: {hosts} resolved host(s) but {modes} probe mode(s) — every \
         host must be probed before any host is touched (#1621)"
    )]
    ModeCountMismatch {
        /// Number of resolved hosts.
        hosts: usize,
        /// Number of probe modes supplied.
        modes: usize,
    },
}

/// Turn a resolved fleet plus one probed mode per host into the executable plan
/// (issue #1621, AC-3/AC-4) — **pure, no I/O**.
///
/// This is where "no mixed versions" is actually won: the migration is assigned to
/// exactly one host, and every other host is explicitly told to skip it.
///
/// # Errors
///
/// Returns [`FleetPlanError::ModeCountMismatch`] when `modes` does not have one
/// entry per resolved host.
pub(crate) fn plan_fleet(
    fleet: &ResolvedFleet,
    modes: &[HostMode],
) -> Result<FleetPlan, FleetPlanError> {
    if fleet.hosts.len() != modes.len() {
        return Err(FleetPlanError::ModeCountMismatch {
            hosts: fleet.hosts.len(),
            modes: modes.len(),
        });
    }

    let placement = migrate_placement(modes);
    let hosts = fleet
        .hosts
        .iter()
        .zip(modes)
        .enumerate()
        .map(|(index, (cfg, mode))| HostPlan {
            // `ResolvedFleet` guarantees a non-blank host for every entry; the
            // total fallback keeps this function panic-free rather than
            // `expect`-ing an invariant enforced two modules away.
            host: cfg.host.clone().unwrap_or_default(),
            mode: *mode,
            // Exactly one host runs the migration; everyone else skips it. A
            // `HostMode::First` host can never be the placement (a first deploy
            // has no live release to keep serving if the migration fails, and
            // `first_deploy_ops` carries no migrate op), so its `Skip` is both
            // correct and inert.
            migrate: if placement == MigratePlacement::FirstRedeploy(index) {
                MigrateStep::Run
            } else {
                MigrateStep::Skip
            },
        })
        .collect();
    Ok(FleetPlan { hosts })
}

/// Everything ONE host's op vector needs beyond its [`HostPlan`].
///
/// The fleet-wide values (`env_file`, `binary_local`, `manifests`, `release_id`)
/// are identical for every host by construction — that is rule 3 — while `cfg`,
/// `unit` and `slots` are that host's own. The driver builds one of these per host
/// and hands it straight to [`host_ops`].
pub(crate) struct HostOpsInput<'a, P: ProxyController> {
    /// This host's resolved config (differs from its siblings only in `host`).
    pub(crate) cfg: &'a ResolvedDeployConfig,
    /// The proxy controller (per-host kamal-proxy, not a fleet load balancer).
    pub(crate) proxy: &'a P,
    /// This host's rendered slot unit, for its own candidate slot/port.
    pub(crate) unit: &'a str,
    /// The env-file body — byte-identical on every host, hence a shared secret
    /// cloned per host rather than rebuilt.
    pub(crate) env_file: &'a Secret,
    /// Local path to the release binary uploaded to every host.
    pub(crate) binary_local: &'a Path,
    /// Config manifests uploaded into every host's release dir.
    pub(crate) manifests: &'a [ManifestUpload],
    /// The ONE release id for this fleet run (rule 3).
    pub(crate) release_id: &'a str,
    /// This host's blue/green slot layout.
    pub(crate) slots: &'a SlotPlan,
    /// Proxy options to preserve on this host's durability re-register (`#2074`);
    /// unused on the first-deploy path.
    pub(crate) reregister_options: &'a ProxyServiceOptions,
}

/// Build ONE host's ordered op vector from its plan, via the existing per-host
/// builders — unchanged, unwrapped, and never concatenated with another host's
/// (rule 1).
///
/// A [`HostMode::First`] host always plans [`MigrateStep::Skip`]
/// ([`exec::first_deploy_ops`] has no migrate op at all), so `plan.migrate` only
/// ever reaches [`exec::cutover_ops`].
pub(crate) fn host_ops<P: ProxyController>(
    plan: &HostPlan,
    input: &HostOpsInput<'_, P>,
) -> Vec<DeployOp> {
    match plan.mode {
        HostMode::First => exec::first_deploy_ops(
            input.cfg,
            input.proxy,
            input.unit,
            input.env_file.clone(),
            input.binary_local,
            input.manifests,
            input.release_id,
            input.slots,
        ),
        HostMode::Redeploy => exec::cutover_ops(
            input.cfg,
            input.proxy,
            input.unit,
            input.env_file.clone(),
            input.binary_local,
            input.manifests,
            input.release_id,
            input.slots,
            input.reregister_options,
            plan.migrate,
        ),
    }
}

/// The `deploy plan` fleet section: the rollout order and the migrate-placement
/// rule (issue #1621, AC-4).
///
/// `deploy plan` is **offline** — it contacts no host — so it cannot know which
/// hosts are first deploys and which are redeploys. It therefore renders the
/// migrate placement as the RULE `deploy up` applies after probing every host,
/// never as a named host; and, like [`super::build_deploy_plan`], the section is
/// descriptive rather than the thing that executes (the drift guard
/// `fleet_plan_matches_fleet_ops_sequence` is what keeps the two honest).
///
/// Only rendered when more than one host is configured, so single-host `deploy
/// plan` output stays byte-identical to pre-#1621.
pub(crate) fn fleet_plan_lines(hosts: &[String]) -> Vec<String> {
    let mut lines = Vec::with_capacity(hosts.len() + 5);
    lines.push(String::new());
    lines.push(format!(
        "Fleet rollout ({} hosts, in `[deploy] hosts` declaration order):",
        hosts.len()
    ));
    for (index, host) in hosts.iter().enumerate() {
        lines.push(format!("  {}. {host}", index + 1));
    }
    lines.push(
        "  Hosts roll out ONE AT A TIME in the order above: each host runs the steps \
         above in full and must finish its cutover before the next host is started, so \
         the rest of the fleet keeps serving throughout."
            .to_owned(),
    );
    lines.push(format!("  {FLEET_MIGRATE_PLACEMENT_NOTE}."));
    lines.push(
        "  This section is descriptive, like the steps above: `deploy plan` contacts no \
         host, so it cannot know which hosts are first deploys and which are redeploys \
         — the migrate placement is the rule `autumn deploy up` applies after probing \
         every host, not a host named here."
            .to_owned(),
    );
    lines
}

/// Test-only fixtures shared by this module's unit tests and the `deploy` plan↔ops
/// drift guard, so both drive the ops through the SAME per-host inputs the slice-3
/// driver will use.
#[cfg(test)]
pub(crate) mod test_support {
    use super::{FleetPlan, HostMode, HostOpsInput, HostPlan, host_ops};
    use crate::deploy::ResolvedDeployConfig;
    use crate::deploy::exec::{
        DeployOp, ManifestUpload, ProxyServiceOptions, SLOT_BLUE, Secret, SlotPlan,
    };
    use crate::deploy::{ResolvedFleet, render_app_unit};
    use std::path::{Path, PathBuf};

    /// The one release id every host in a test fleet deploys (rule 3).
    pub(crate) const RELEASE_ID: &str = "20260714T120000Z";
    /// Public port fronted by each host's proxy.
    pub(crate) const PUBLIC_PORT: u16 = 3000;

    fn manifests() -> Vec<ManifestUpload> {
        vec![ManifestUpload {
            local: PathBuf::from("/local/autumn.toml"),
            remote_basename: "autumn.toml".to_owned(),
        }]
    }

    /// Build ONE host's op vector exactly the way the driver will: its own
    /// [`SlotPlan`], its own rendered unit, and the fleet-wide release id.
    pub(crate) fn host_ops_for(cfg: &ResolvedDeployConfig, plan: &HostPlan) -> Vec<DeployOp> {
        let slots = match plan.mode {
            HostMode::First => SlotPlan::first(PUBLIC_PORT),
            HostMode::Redeploy => SlotPlan::redeploy(PUBLIC_PORT, SLOT_BLUE),
        };
        let release_dir = format!("{}/{RELEASE_ID}", cfg.releases_dir());
        let unit = render_app_unit(
            cfg,
            &release_dir,
            slots.candidate_port,
            slots.candidate_slot,
        );
        host_ops(
            plan,
            &HostOpsInput {
                cfg,
                proxy: &crate::deploy::proxy::KamalProxyController::new(60),
                unit: &unit,
                env_file: &Secret::new("AUTUMN_SECURITY__SIGNING_SECRET=x\n"),
                binary_local: Path::new("/local/target/release/myapp"),
                manifests: &manifests(),
                release_id: RELEASE_ID,
                slots: &slots,
                reregister_options: &ProxyServiceOptions {
                    tls: false,
                    host: None,
                },
            },
        )
    }

    /// Every host's ops, each in its OWN vector (rule 1 — never concatenated).
    pub(crate) fn per_host_ops(fleet: &ResolvedFleet, plan: &FleetPlan) -> Vec<Vec<DeployOp>> {
        fleet
            .hosts
            .iter()
            .zip(&plan.hosts)
            .map(|(cfg, host_plan)| host_ops_for(cfg, host_plan))
            .collect()
    }

    /// The fleet's op labels flattened in rollout order — for assertions ONLY;
    /// execution never sees a flattened vector.
    pub(crate) fn fleet_op_labels(fleet: &ResolvedFleet, plan: &FleetPlan) -> Vec<&'static str> {
        per_host_ops(fleet, plan)
            .iter()
            .flat_map(|ops| ops.iter().map(DeployOp::label).collect::<Vec<_>>())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::per_host_ops;
    use super::*;
    use autumn_web::config::DeployConfig;

    /// A resolved fleet from the fleet spelling an operator would write.
    fn fleet_of(hosts: &[&str]) -> ResolvedFleet {
        ResolvedFleet::resolve(
            &DeployConfig {
                hosts: hosts.iter().map(|h| (*h).to_owned()).collect(),
                ..DeployConfig::default()
            },
            "myapp",
        )
        .expect("a well-formed fleet resolves")
    }

    fn labels(ops: &[DeployOp]) -> Vec<&'static str> {
        ops.iter().map(DeployOp::label).collect()
    }

    fn shell_for<'a>(ops: &'a [DeployOp], label: &str) -> Option<&'a str> {
        ops.iter().find_map(|op| match op {
            DeployOp::Run(cmd) if cmd.label == label => Some(cmd.shell.as_str()),
            _ => None,
        })
    }

    fn upload_path<'a>(ops: &'a [DeployOp], label: &str) -> Option<&'a str> {
        ops.iter().find_map(|op| match op {
            DeployOp::UploadFile {
                label: l,
                remote_path,
                ..
            } if *l == label => Some(remote_path.as_str()),
            _ => None,
        })
    }

    /// Plan a fleet and build every host's op vector, keeping each host's ops in
    /// its OWN `Vec` (rule 1 forbids concatenating them — `execute_with_teardown`
    /// resolves its boundary with the FIRST matching label, so a flat vector would
    /// silently disable teardown for every host after the first).
    fn plan_and_build(hosts: &[&str], modes: &[HostMode]) -> (FleetPlan, Vec<Vec<DeployOp>>) {
        let fleet = fleet_of(hosts);
        let plan = plan_fleet(&fleet, modes).expect("a well-formed fleet plans");
        let ops = per_host_ops(&fleet, &plan);
        (plan, ops)
    }

    #[test]
    fn fleet_plan_runs_migrations_exactly_once() {
        // #1621 (AC-4, T1.8): the schema is fleet-wide, so a rollout migrates ONCE,
        // before the first host's cutover. A naive per-host loop would run the
        // one-shot N times: the Postgres advisory lock keeps that safe, but hosts
        // 2..N each pay the lock wait and — worse — a migration failing on host 2
        // AFTER host 1 cut over is exactly the mixed-version fleet AC-3 forbids.
        let (plan, per_host) = plan_and_build(
            &["web-a", "web-b", "web-c"],
            &[HostMode::Redeploy, HostMode::Redeploy, HostMode::Redeploy],
        );

        // (a) the plan itself names exactly one migrating host — the first in
        // rollout order — and the built ops agree.
        assert_eq!(
            plan.migrating_host().map(|h| h.host.as_str()),
            Some("web-a"),
            "an all-redeploy fleet migrates on the first host, got: {:?}",
            plan.hosts
        );
        assert_eq!(
            plan.hosts
                .iter()
                .filter(|h| h.migrate == MigrateStep::Run)
                .count(),
            1,
            "exactly one host may be assigned the migration, got: {:?}",
            plan.hosts
        );

        let flat: Vec<&'static str> = per_host.iter().flat_map(|ops| labels(ops)).collect();
        assert_eq!(
            flat.iter().filter(|l| **l == "migrate").count(),
            1,
            "a fleet rollout must schedule exactly one migrate op, got: {flat:?}"
        );

        // (b) the single migrate precedes the FIRST cutover anywhere in the fleet,
        // so no host is serving the new release before the schema is at it.
        let migrate = flat
            .iter()
            .position(|l| *l == "migrate")
            .expect("the fleet migrates");
        let first_flip = flat
            .iter()
            .position(|l| *l == "proxy-flip" || *l == "proxy-route")
            .expect("the fleet cuts over somewhere");
        assert!(
            migrate < first_flip,
            "the migration must precede the first cutover in the fleet: \
             migrate at {migrate}, first boundary at {first_flip}, labels: {flat:?}"
        );

        // (c) the migrate op is still the real, blocking one-shot: `--wait` is what
        // propagates a failed migration into an abort, and `AUTUMN_MIGRATE=1` is the
        // runtime trigger.
        let migrate_shell = per_host
            .iter()
            .find_map(|ops| shell_for(ops, "migrate"))
            .expect("one host carries the migrate op");
        assert!(
            migrate_shell.contains("--setenv=AUTUMN_MIGRATE=1"),
            "the fleet migrate op must still trigger the app's migrate-only mode: {migrate_shell}"
        );
        assert!(
            migrate_shell.contains("--wait"),
            "the fleet migrate op must still block on the one-shot's exit status: {migrate_shell}"
        );
    }

    #[test]
    fn every_fleet_host_vector_carries_exactly_one_boundary_label() {
        // #1621 (AC-3, T1.9): `execute_with_teardown` resolves the boundary with
        // `ops.iter().position(|op| op.label() == boundary)` — the FIRST match. Two
        // hosts' ops in one vector would make every later host's pre-flip failure
        // look post-boundary and silently disable auto-rollback. This is the
        // structural tripwire for that: one boundary per host vector, `proxy-flip`
        // for a redeploy and `proxy-route` for a first deploy.
        let (plan, per_host) = plan_and_build(
            &["web-a", "web-b", "web-c"],
            &[HostMode::First, HostMode::Redeploy, HostMode::Redeploy],
        );

        for (host_plan, ops) in plan.hosts.iter().zip(&per_host) {
            let host_labels = labels(ops);
            let expected = match host_plan.mode {
                HostMode::First => "proxy-route",
                HostMode::Redeploy => "proxy-flip",
            };
            assert_eq!(
                host_labels.iter().filter(|l| **l == expected).count(),
                1,
                "{} must carry exactly one `{expected}` boundary, got: {host_labels:?}",
                host_plan.host
            );
            let other = match host_plan.mode {
                HostMode::First => "proxy-flip",
                HostMode::Redeploy => "proxy-route",
            };
            assert!(
                !host_labels.contains(&other),
                "{} must not carry the other mode's boundary `{other}`, got: {host_labels:?}",
                host_plan.host
            );
        }
    }

    #[test]
    fn migrate_placement_picks_the_first_redeploy_host_in_rollout_order() {
        // #1621 (AC-4, T1.10): the migration runs on the first host that is ALREADY
        // on a previous release — a first-deploy host has no live release to keep
        // serving if the migration fails, and `first_deploy_ops` deliberately
        // carries no migrate op at all.
        assert_eq!(
            migrate_placement(&[HostMode::First, HostMode::Redeploy, HostMode::Redeploy]),
            MigratePlacement::FirstRedeploy(1),
            "the migration belongs to the first REDEPLOY host, not the first host"
        );
        assert_eq!(
            migrate_placement(&[HostMode::Redeploy, HostMode::Redeploy]),
            MigratePlacement::FirstRedeploy(0),
            "an all-redeploy fleet migrates on host 1"
        );
        // An all-first-deploy fleet migrates NOWHERE — today's documented
        // single-host limitation, generalised. (The operator warning that names
        // `autumn migrate` lands with the driver.)
        assert_eq!(
            migrate_placement(&[HostMode::First, HostMode::First]),
            MigratePlacement::None,
            "an all-first-deploy fleet has no host to migrate on"
        );
        assert_eq!(
            migrate_placement(&[]),
            MigratePlacement::None,
            "an empty mode list places no migration"
        );
    }

    #[test]
    fn host_mode_projects_the_probed_deploy_mode() {
        // #1621: the planner's input is the payload-free projection of the probe's
        // `DeployMode`. The live slot the probe carries belongs to the host's
        // `SlotPlan`, not to the planning decision, so the driver maps once here
        // instead of threading the probe type through the pure layer.
        assert_eq!(
            HostMode::from_deploy_mode(&exec::DeployMode::First),
            HostMode::First,
            "an unpromoted host takes the first-deploy path"
        );
        assert_eq!(
            HostMode::from_deploy_mode(&exec::DeployMode::Redeploy {
                live_slot: exec::SLOT_GREEN
            }),
            HostMode::Redeploy,
            "a host already serving takes the cutover path, whichever slot is live"
        );
        assert_eq!(
            HostMode::from_deploy_mode(&exec::DeployMode::Redeploy {
                live_slot: exec::SLOT_BLUE
            }),
            HostMode::Redeploy,
            "the live slot must not change the planning decision"
        );
    }

    #[test]
    fn an_all_first_deploy_fleet_schedules_no_migration() {
        // #1621 (AC-4): the companion of the placement rule at the op level —
        // `first_deploy_ops` stays byte-identical, so an all-first fleet carries no
        // `migrate` op anywhere.
        let (plan, per_host) =
            plan_and_build(&["web-a", "web-b"], &[HostMode::First, HostMode::First]);
        assert!(
            plan.hosts.iter().all(|h| h.migrate == MigrateStep::Skip),
            "no first-deploy host may be assigned the migration, got: {:?}",
            plan.hosts
        );
        let flat: Vec<&'static str> = per_host.iter().flat_map(|ops| labels(ops)).collect();
        assert!(
            !flat.contains(&"migrate"),
            "an all-first-deploy fleet must schedule no migration, got: {flat:?}"
        );
    }

    #[test]
    fn every_fleet_host_deploys_the_identical_release_id() {
        // #1621 (AC-3/AC-6, T1.18): ONE release id per fleet run. Minting per host
        // would give un-comparable `current` symlink targets (permanent reported
        // drift) and no single rollback target for the whole fleet.
        let (_plan, per_host) = plan_and_build(
            &["web-a", "web-b", "web-c"],
            &[HostMode::Redeploy, HostMode::Redeploy, HostMode::Redeploy],
        );

        let expected_dir = "/srv/autumn/myapp/releases/20260714T120000Z";
        let mut binary_paths: Vec<&str> = Vec::new();
        for ops in &per_host {
            let binary = upload_path(ops, "upload-binary").expect("each host uploads the binary");
            assert_eq!(
                binary,
                format!("{expected_dir}/myapp"),
                "every host must upload into the SAME fleet release dir"
            );
            let prepare = shell_for(ops, "prepare-dirs").expect("each host prepares its dirs");
            assert!(
                prepare.contains(expected_dir),
                "every host must prepare the SAME fleet release dir: {prepare}"
            );
            binary_paths.push(binary);
        }
        binary_paths.dedup();
        assert_eq!(
            binary_paths.len(),
            1,
            "the fleet must deploy exactly one release id, got: {binary_paths:?}"
        );
    }

    #[test]
    fn plan_fleet_refuses_a_mode_count_mismatch_without_panicking() {
        // #1621: `plan_fleet` is pure and total — a caller that hands it a probe
        // list of the wrong length gets a typed refusal, never a panic mid-rollout.
        let fleet = fleet_of(&["web-a", "web-b"]);
        let err = plan_fleet(&fleet, &[HostMode::Redeploy])
            .expect_err("a short mode list must be refused");
        assert_eq!(
            err,
            FleetPlanError::ModeCountMismatch { hosts: 2, modes: 1 },
            "the refusal must name both counts, got: {err:?}"
        );
    }

    #[test]
    fn fleet_plan_lines_render_hosts_in_rollout_order_with_one_migrate_note() {
        // #1621 (AC-4): the offline `deploy plan` fleet section. `plan` contacts no
        // host, so it cannot know per-host first-vs-redeploy modes — it therefore
        // renders the migrate placement as a RULE, exactly once, not per host.
        let hosts = vec![
            "web-1.example.com".to_owned(),
            "web-2.example.com".to_owned(),
            "web-3.example.com".to_owned(),
        ];
        let rendered = fleet_plan_lines(&hosts).join("\n");

        let mut previous = 0usize;
        for host in &hosts {
            let at = rendered
                .find(host.as_str())
                .unwrap_or_else(|| panic!("{host} must appear in the fleet plan:\n{rendered}"));
            assert!(
                at >= previous,
                "hosts must render in declaration (rollout) order:\n{rendered}"
            );
            previous = at;
        }
        assert_eq!(
            rendered.matches(FLEET_MIGRATE_PLACEMENT_NOTE).count(),
            1,
            "the migrate placement is a single fleet-wide note, not a per-host line:\n{rendered}"
        );
    }
}
