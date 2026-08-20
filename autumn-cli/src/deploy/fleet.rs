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

/// Where one host ended up after a fleet rollout (issue #1621, AC-3/AC-6).
///
/// Recorded per host by the driver and reported on **every** exit path — success
/// or halt — because a halted rollout is exactly the moment an operator has no
/// other source of truth about which host is running what.
///
/// The variants are deliberately the ones the EXISTING per-host executor already
/// distinguishes, so nothing is inferred: [`exec::execute_with_teardown`] returns
/// `CandidateRolledBack`/`FirstDeployTornDown` **only** for a failure at or before
/// the go-live boundary (traffic never moved, the candidate was cleaned up), and
/// the raw error otherwise (the host is live on the new release). The finer
/// post-boundary classification — housekeeping vs ambiguous-markers vs functional —
/// and the fleet-wide compensation it drives land with the next slice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HostOutcome {
    /// The rollout halted before this host was reached. Nothing was mutated on it.
    Untouched,
    /// The host is serving the new release.
    Serving,
    /// A pre-boundary failure on a REDEPLOY host: the candidate was torn down and
    /// this host's previous release is still serving.
    RolledBack {
        /// Label of the step that failed.
        failed_step: &'static str,
    },
    /// A pre-boundary failure on a FIRST-deploy host: there was no previous
    /// release, so the candidate was torn down and nothing serves here.
    TornDown {
        /// Label of the step that failed.
        failed_step: &'static str,
    },
    /// A post-boundary failure: traffic has already moved, so this host IS running
    /// the new release even though the deploy reported failure.
    LiveOnNew {
        /// Label of the step that failed.
        failed_step: &'static str,
    },
}

/// The step label to attribute a failure to — always a `&'static str` op label, so
/// a fleet error can never carry a shell line, a remote path, or a driver message.
///
/// Secrets discipline is load-bearing here: `DeployExecError`'s own `Display` is
/// already redacted, but the fleet error/summary types quote only labels and host
/// names, never a formatted source error.
pub(crate) const fn failed_step_label(err: &exec::DeployExecError) -> &'static str {
    match err {
        exec::DeployExecError::CommandFailed { label, .. } => label,
        exec::DeployExecError::CandidateRolledBack { failed_step, .. }
        | exec::DeployExecError::FirstDeployTornDown { failed_step, .. }
        | exec::DeployExecError::RollbackFailed { failed_step, .. } => failed_step,
        exec::DeployExecError::UploadFailed { .. } => "upload",
        exec::DeployExecError::Stage { .. } => "stage-local-file",
        exec::DeployExecError::Spawn { .. } => "ssh-transport",
        exec::DeployExecError::PreflightAborted { .. } => "preflight",
        exec::DeployExecError::ProxyIncompatible { .. } => "proxy-compat-probe",
        exec::DeployExecError::NoPreviousRelease => "resolve-previous",
    }
}

/// Classify one host's execution failure into its [`HostOutcome`] — pure, and the
/// discriminator AC-3 turns on.
///
/// Anything that is NOT one of the executor's two clean-failure variants is
/// treated as **live on the new release**: that is the fail-closed reading, since
/// `execute_with_teardown` returns the raw error precisely when the failure landed
/// *after* the go-live boundary (or when the boundary could not be located, in
/// which case it deliberately refuses to guess).
pub(crate) const fn classify_failure(err: &exec::DeployExecError) -> HostOutcome {
    match err {
        exec::DeployExecError::CandidateRolledBack { failed_step, .. } => {
            HostOutcome::RolledBack { failed_step }
        }
        exec::DeployExecError::FirstDeployTornDown { failed_step, .. } => {
            HostOutcome::TornDown { failed_step }
        }
        _ => HostOutcome::LiveOnNew {
            failed_step: failed_step_label(err),
        },
    }
}

/// The loud line printed when a fleet rollout schedules no migration at all.
///
/// [`exec::first_deploy_ops`] carries no migrate op (a first deploy has never run
/// migrations — a documented single-host limitation), so an all-first-deploy fleet
/// migrates NOWHERE. That is fine for a brand-new app and catastrophic for one
/// with pending migrations, so it is said out loud rather than inferred from the
/// absence of a `migrate` line.
pub(crate) const FLEET_NO_MIGRATION_NOTE: &str = "no host in this fleet is on a previous release, so this rollout runs NO migrations \
     (a first deploy never does) — run `autumn migrate` yourself before serving traffic";

/// The one-line reminder printed once, after the fleet's single migration lands.
pub(crate) const FLEET_SCHEMA_MOVED_NOTE: &str = "the schema has moved; from here an automatic rollback restores BINARIES only — \
     it never rolls a migration back";

/// The `deploy up` fleet header: rollout order, each host's probed mode, and where
/// the single fleet-wide migration lands (issue #1621, §8.1).
///
/// Rendered **only** when more than one host is configured, so single-host output
/// stays byte-identical to pre-#1621.
///
/// `writable_db_configured` gates the no-migration warning: an app with no
/// writable database has no schema to be behind, so warning about it would be
/// noise.
pub(crate) fn fleet_rollout_lines(
    plan: &FleetPlan,
    release_id: &str,
    writable_db_configured: bool,
) -> Vec<String> {
    let count = plan.hosts.len();
    let mut lines = Vec::with_capacity(count + 3);
    lines.push(format!(
        "Rolling release {release_id} across {count} hosts, ONE AT A TIME, in `[deploy] hosts` \
         order:"
    ));
    for (index, host) in plan.hosts.iter().enumerate() {
        let mode = match host.mode {
            HostMode::First => "first deploy",
            HostMode::Redeploy => "zero-downtime redeploy",
        };
        lines.push(format!("  {}. {} — {mode}", index + 1, host.host));
    }
    match plan.migrating_host() {
        Some(migrating) => {
            let skipped: Vec<&str> = plan
                .hosts
                .iter()
                .filter(|h| h.host != migrating.host)
                .map(|h| h.host.as_str())
                .collect();
            lines.push(format!(
                "  \u{2192} migrate ({} only \u{2014} the schema is fleet-wide; {} skip it)",
                migrating.host,
                skipped.join(", "),
            ));
        }
        None if writable_db_configured => {
            lines.push(format!("  \u{26A0}\u{FE0F}  {FLEET_NO_MIGRATION_NOTE}"));
        }
        None => {}
    }
    lines
}

/// The per-host state table printed at the END of every fleet rollout — success or
/// halt (issue #1621, §8.2).
///
/// On a halt this is the operator's only source of truth about which host runs
/// what, so it names the recovery command for each host left on the new release
/// and states plainly that the schema was NOT rolled back.
pub(crate) fn fleet_summary_lines(
    plan: &FleetPlan,
    outcomes: &[HostOutcome],
    release_id: &str,
) -> Vec<String> {
    let mut lines = vec!["Fleet state:".to_owned()];
    let width = plan
        .hosts
        .iter()
        .map(|h| h.host.chars().count())
        .max()
        .unwrap_or(0);
    for (host, outcome) in plan.hosts.iter().zip(outcomes) {
        let state = match outcome {
            HostOutcome::Untouched => "untouched (not reached)".to_owned(),
            HostOutcome::Serving => format!("serving {release_id}"),
            // Traffic already moved before the failure, so this host IS on the new
            // release — saying only "failed" would be the dangerous half-truth.
            HostOutcome::LiveOnNew { failed_step } => {
                format!(
                    "serving {release_id} \u{2014} but `{failed_step}` failed AFTER the cutover"
                )
            }
            HostOutcome::RolledBack { failed_step } => {
                format!("previous release still serving (rolled back at `{failed_step}`)")
            }
            HostOutcome::TornDown { failed_step } => {
                format!("NOTHING serving (first deploy torn down at `{failed_step}`)")
            }
        };
        let marker = match outcome {
            HostOutcome::Serving => "\u{2705}",
            HostOutcome::Untouched => "\u{2013}",
            _ => "\u{274C}",
        };
        lines.push(format!(
            "  {marker} {host:<width$}  {state}",
            host = host.host,
        ));
    }
    let on_new: Vec<&str> = plan
        .hosts
        .iter()
        .zip(outcomes)
        .filter(|(_, outcome)| {
            matches!(
                outcome,
                HostOutcome::Serving | HostOutcome::LiveOnNew { .. }
            )
        })
        .map(|(host, _)| host.host.as_str())
        .collect();
    if !on_new.is_empty() {
        lines.push(format!(
            "  On {release_id}: {}. Roll a host back with `autumn deploy rollback`.",
            on_new.join(", "),
        ));
        lines.push(format!("  \u{26A0}\u{FE0F}  {FLEET_SCHEMA_MOVED_NOTE}"));
    }
    lines
}

/// Test-only fixtures shared by this module's unit tests and the `deploy` plan↔ops
/// drift guard, so both drive the ops through the SAME per-host inputs the slice-3
/// driver will use.
#[cfg(test)]
pub(crate) mod test_support {
    use super::{FleetPlan, HostMode, HostOpsInput, HostPlan, host_ops};
    use crate::deploy::ResolvedDeployConfig;
    use crate::deploy::exec::test_support::{FleetTape, RecordedCall, RecordingExecutor};
    use crate::deploy::exec::{
        DeployOp, ManifestUpload, ProxyServiceOptions, SLOT_BLUE, Secret, SlotPlan,
    };
    use crate::deploy::{ResolvedFleet, render_app_unit};
    use std::path::{Path, PathBuf};
    use std::rc::Rc;

    /// One host's scripted responses inside a [`FleetRecorder`].
    #[derive(Default)]
    struct HostScript {
        host: String,
        stdout: Vec<(&'static str, String)>,
        fail: Vec<&'static str>,
    }

    /// A fleet-shaped recording fake: ONE
    /// [`RecordingExecutor`](crate::deploy::exec::test_support::RecordingExecutor)
    /// per host, all writing `(host, call)` onto one shared tape (issue #1621,
    /// plan §9.2).
    ///
    /// The shared tape is the point. A per-host call list can say "host B ran
    /// `start-candidate`", but it cannot say **when** relative to host A — and the
    /// whole safety claim of a rolling deploy is an ordering claim ("host k+1 is
    /// not touched until host k has cut over"). The tape makes that assertable
    /// directly.
    ///
    /// Every executor it hands out is [`RecordingExecutor::strict`], so a host
    /// whose probe was not scripted panics loudly instead of silently taking the
    /// first-deploy branch on empty stdout.
    #[derive(Default)]
    pub(crate) struct FleetRecorder {
        tape: FleetTape,
        scripts: Vec<HostScript>,
    }

    impl FleetRecorder {
        /// An empty recorder; hosts are registered by [`Self::script`] /
        /// [`Self::fail`] / [`Self::host`].
        pub(crate) fn new() -> Self {
            Self::default()
        }

        fn entry(&mut self, host: &str) -> &mut HostScript {
            if let Some(index) = self.scripts.iter().position(|s| s.host == host) {
                return &mut self.scripts[index];
            }
            self.scripts.push(HostScript {
                host: host.to_owned(),
                ..HostScript::default()
            });
            self.scripts
                .last_mut()
                .expect("a script was just pushed for this host")
        }

        /// Script one host's stdout for `label`.
        pub(crate) fn script(
            mut self,
            host: &str,
            label: &'static str,
            stdout: impl Into<String>,
        ) -> Self {
            let stdout = stdout.into();
            self.entry(host).stdout.push((label, stdout));
            self
        }

        /// Make one host's `label` fail (a scripted `CommandFailed`).
        pub(crate) fn fail(mut self, host: &str, label: &'static str) -> Self {
            self.entry(host).fail.push(label);
            self
        }

        /// The executor factory the fleet driver is injected with: one strict,
        /// tape-sharing fake per host.
        pub(crate) fn executor(&self, cfg: &ResolvedDeployConfig) -> RecordingExecutor {
            let host = cfg.host.clone().unwrap_or_default();
            let mut exec = RecordingExecutor::new()
                .strict()
                .recording_as(host.clone(), Rc::clone(&self.tape));
            if let Some(script) = self.scripts.iter().find(|s| s.host == host) {
                for (label, stdout) in &script.stdout {
                    exec = exec.with_stdout(label, stdout.clone());
                }
                for label in &script.fail {
                    exec = exec.failing(label);
                }
            }
            exec
        }

        /// One host's calls, in order (uploads included).
        pub(crate) fn calls_for(&self, host: &str) -> Vec<RecordedCall> {
            self.tape
                .borrow()
                .iter()
                .filter(|(h, _)| h == host)
                .map(|(_, call)| call.clone())
                .collect()
        }

        /// One host's `Run` labels, in order.
        pub(crate) fn run_labels_for(&self, host: &str) -> Vec<&'static str> {
            self.calls_for(host)
                .iter()
                .filter_map(RecordedCall::run_label)
                .collect()
        }

        /// Global index of the first call on `host` whose label is NOT in
        /// `read_only` — i.e. that host's first MUTATING op (an upload counts).
        pub(crate) fn first_mutating(&self, host: &str, read_only: &[&str]) -> Option<usize> {
            self.tape.borrow().iter().position(|(h, call)| {
                h == host && !call.run_label().is_some_and(|l| read_only.contains(&l))
            })
        }

        /// Global index of the first `Run` of `label` on `host`.
        pub(crate) fn index_of(&self, host: &str, label: &str) -> Option<usize> {
            self.tape
                .borrow()
                .iter()
                .position(|(h, call)| h == host && call.run_label() == Some(label))
        }

        /// Every call across the fleet that is NOT one of the `read_only` probe
        /// labels — the "did anything get mutated?" query. Uploads are always
        /// mutating (they have no label to allowlist).
        pub(crate) fn mutating(&self, read_only: &[&str]) -> Vec<(String, RecordedCall)> {
            self.tape
                .borrow()
                .iter()
                .filter(|(_, call)| !call.run_label().is_some_and(|l| read_only.contains(&l)))
                .cloned()
                .collect()
        }
    }

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
    fn failure_classification_distinguishes_a_clean_host_from_a_live_one() {
        // #1621 (AC-3): the whole "no mixed versions" decision turns on ONE
        // question per host — did traffic move? `execute_with_teardown` already
        // answers it: it returns `CandidateRolledBack`/`FirstDeployTornDown` ONLY
        // for a failure at or before the go-live boundary, and the raw error
        // otherwise. Nothing here re-derives that; it only names the two cases.
        let boxed = || {
            Box::new(exec::DeployExecError::CommandFailed {
                label: "readiness-gate",
                message: "scripted".to_owned(),
            })
        };
        assert_eq!(
            classify_failure(&exec::DeployExecError::CandidateRolledBack {
                failed_step: "readiness-gate",
                source: boxed(),
            }),
            HostOutcome::RolledBack {
                failed_step: "readiness-gate"
            },
            "a pre-boundary redeploy failure leaves the previous release serving"
        );
        assert_eq!(
            classify_failure(&exec::DeployExecError::FirstDeployTornDown {
                failed_step: "readiness-gate",
                source: boxed(),
            }),
            HostOutcome::TornDown {
                failed_step: "readiness-gate"
            },
            "a pre-boundary first deploy has no previous release to fall back to"
        );
        // Fail closed: anything else means the executor did NOT tear down, which it
        // only declines to do when the failure landed after the boundary (or when
        // the boundary could not be located) — either way, assume the host is live.
        assert_eq!(
            classify_failure(&exec::DeployExecError::CommandFailed {
                label: "drain-old",
                message: "scripted".to_owned(),
            }),
            HostOutcome::LiveOnNew {
                failed_step: "drain-old"
            },
            "a post-boundary failure leaves the host running the NEW release"
        );
    }

    #[test]
    fn failed_step_labels_are_static_and_never_quote_a_driver_message() {
        // #1621: every fleet-facing error/summary field is a host name or a
        // `&'static str` op label. A migration driver error can embed the database
        // URL, so a fleet error must never format a source error into its own text.
        assert_eq!(
            failed_step_label(&exec::DeployExecError::CommandFailed {
                label: "migrate",
                message: "postgres://user:pw@db/app is unreachable".to_owned(),
            }),
            "migrate",
            "the label is taken, the message is not"
        );
        assert_eq!(
            failed_step_label(&exec::DeployExecError::UploadFailed {
                remote_path: "/srv/autumn/myapp/shared/autumn.env".to_owned(),
                message: "scripted".to_owned(),
            }),
            "upload",
            "an upload failure attributes to a fixed label, never the remote path"
        );
        assert_eq!(
            failed_step_label(&exec::DeployExecError::ProxyIncompatible {
                message: "missing --drain-timeout".to_owned(),
            }),
            "proxy-compat-probe",
        );
        assert_eq!(
            failed_step_label(&exec::DeployExecError::NoPreviousRelease),
            "resolve-previous",
        );
    }

    #[test]
    fn the_rollout_header_names_the_single_migrating_host_and_who_skips() {
        // #1621 (AC-4, §8.1): three hosts is ~48 visually identical op lines, and
        // the first 3 a.m. question is "which host was that on?". The header answers
        // it up front, and states where the ONE migration lands.
        let fleet = fleet_of(&["web-a", "web-b", "web-c"]);
        let plan = plan_fleet(
            &fleet,
            &[HostMode::First, HostMode::Redeploy, HostMode::Redeploy],
        )
        .expect("a well-formed fleet plans");
        let rendered = fleet_rollout_lines(&plan, "20260714T120000Z", true).join("\n");

        assert!(
            rendered.contains("1. web-a — first deploy")
                && rendered.contains("2. web-b — zero-downtime redeploy")
                && rendered.contains("3. web-c — zero-downtime redeploy"),
            "the header must show rollout order and each host's probed mode:\n{rendered}"
        );
        assert!(
            rendered.contains("migrate (web-b only") && rendered.contains("web-a, web-c skip it"),
            "the header must name the migrating host and the hosts that skip:\n{rendered}"
        );
        assert!(
            !rendered.contains(FLEET_NO_MIGRATION_NOTE),
            "a fleet that DOES migrate must not warn that it does not:\n{rendered}"
        );
    }

    #[test]
    fn an_all_first_deploy_fleet_warns_only_when_a_database_is_configured() {
        // #1621 (AC-4): `first_deploy_ops` carries no migrate op, so an
        // all-first-deploy fleet migrates NOWHERE — today's documented single-host
        // limitation, generalised. That is fine for a brand-new app and
        // catastrophic for one with pending migrations, so it is said out loud —
        // but only when there is a writable database to be behind.
        let fleet = fleet_of(&["web-a", "web-b"]);
        let plan = plan_fleet(&fleet, &[HostMode::First, HostMode::First])
            .expect("a well-formed fleet plans");

        let warned = fleet_rollout_lines(&plan, "20260714T120000Z", true).join("\n");
        assert!(
            warned.contains(FLEET_NO_MIGRATION_NOTE) && warned.contains("autumn migrate"),
            "a database-backed all-first fleet must name the operator's step:\n{warned}"
        );
        let quiet = fleet_rollout_lines(&plan, "20260714T120000Z", false).join("\n");
        assert!(
            !quiet.contains(FLEET_NO_MIGRATION_NOTE),
            "an app with no writable database has no schema to warn about:\n{quiet}"
        );
    }

    #[test]
    fn the_summary_names_every_host_state_and_the_recovery_command() {
        // #1621 (§8.2): a halted rollout is exactly the moment the operator has no
        // other source of truth, so the last thing printed is per-host state — with
        // the recovery command and the plain statement that the schema did NOT move
        // back.
        let fleet = fleet_of(&["web-a", "web-b", "web-c"]);
        let plan = plan_fleet(
            &fleet,
            &[HostMode::Redeploy, HostMode::Redeploy, HostMode::Redeploy],
        )
        .expect("a well-formed fleet plans");
        let rendered = fleet_summary_lines(
            &plan,
            &[
                HostOutcome::Serving,
                HostOutcome::RolledBack {
                    failed_step: "readiness-gate",
                },
                HostOutcome::Untouched,
            ],
            "20260714T120000Z",
        )
        .join("\n");

        assert!(
            rendered.contains("web-a") && rendered.contains("serving 20260714T120000Z"),
            "a cut-over host must be reported with the release it serves:\n{rendered}"
        );
        assert!(
            rendered.contains("previous release still serving")
                && rendered.contains("readiness-gate"),
            "a rolled-back host must name the step that failed:\n{rendered}"
        );
        assert!(
            rendered.contains("untouched"),
            "an unreached host must be reported as untouched, not silently omitted:\n{rendered}"
        );
        assert!(
            rendered.contains("autumn deploy rollback") && rendered.contains("web-a"),
            "the summary must name the recovery command for hosts on the new release:\n{rendered}"
        );
        assert!(
            rendered.contains(FLEET_SCHEMA_MOVED_NOTE),
            "the summary must state that an automatic rollback restores binaries only:\n{rendered}"
        );

        // A fleet where nothing cut over offers no rollback command (there is
        // nothing to roll back) and makes no schema claim.
        let none = fleet_summary_lines(
            &plan,
            &[
                HostOutcome::TornDown {
                    failed_step: "readiness-gate",
                },
                HostOutcome::Untouched,
                HostOutcome::Untouched,
            ],
            "20260714T120000Z",
        )
        .join("\n");
        assert!(
            !none.contains("autumn deploy rollback") && !none.contains(FLEET_SCHEMA_MOVED_NOTE),
            "with no host on the new release there is nothing to roll back:\n{none}"
        );
        assert!(
            none.contains("NOTHING serving"),
            "a torn-down first deploy leaves nothing serving and must say so:\n{none}"
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
