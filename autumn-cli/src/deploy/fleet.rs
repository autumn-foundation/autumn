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
//!    two hosts' [`exec::DeployOp`] vectors. `exec::execute_with_teardown`
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
/// media fleet, TLS fleet) are prologue concerns and live with the driver
/// (`refuse_sqlite_fleet` and friends in `deploy.rs`), not here: they grade the
/// project's CONFIG, while this grades the plan's internal consistency.
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
/// The first five variants are deliberately the ones the EXISTING per-host
/// executor already distinguishes, so nothing is inferred:
/// `exec::execute_with_teardown` returns `CandidateRolledBack`/`FirstDeployTornDown`
/// **only** for a failure at or before the go-live boundary (traffic never moved,
/// the candidate was cleaned up), and the raw error otherwise (the host is live on
/// the new release). [`classify_post_boundary`] then refines that raw
/// "live on the new release" case by the failing LABEL, and the compensation
/// variants record what the fleet driver did about it afterwards.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HostOutcome {
    /// The rollout halted before this host was reached. Nothing was mutated on it.
    Untouched,
    /// The host is serving the new release.
    Serving,
    /// A post-boundary HOUSEKEEPING failure ([`PostBoundaryClass::Housekeeping`]):
    /// the host is live and healthy on the new release, and only bookkeeping that
    /// does not touch traffic failed. The rollout CONTINUES past it (§4.6).
    Degraded {
        /// Label of the housekeeping step that failed.
        label: &'static str,
    },
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
    /// A post-boundary FUNCTIONAL failure: traffic has already moved, so this host
    /// IS running the new release even though the deploy reported failure.
    LiveOnNew {
        /// Label of the step that failed.
        failed_step: &'static str,
    },
    /// A post-boundary failure at `commit-markers`
    /// ([`PostBoundaryClass::Ambiguous`]): the host is on the new release AND its
    /// marker triple is mid-transaction, so it is **never** rolled back
    /// automatically — [`exec::resolve_rollback_target`] could resolve the wrong
    /// release. Fails closed to a manual recovery.
    AmbiguousMarkers,
    /// Fleet compensation rolled this host back to its previous release.
    CompensatedRollback,
    /// Fleet compensation removed this host's just-completed FIRST deploy, so it is
    /// back to nothing-installed (see [`RollbackAction::Teardown`]).
    CompensatedTeardown,
    /// Fleet compensation was attempted on this host and FAILED — it is still on
    /// the new release. Never swallowed: the failing step is named.
    CompensationFailed {
        /// Label of the compensation step that failed.
        failed_step: &'static str,
    },
    /// Fleet compensation was deliberately NOT attempted on this host: it is still
    /// on the new release and needs a human (see the `MANUAL_*` reasons).
    Manual {
        /// Why the automatic rollback was declined — a `&'static str`, never a
        /// shell line or a driver message.
        reason: &'static str,
    },
}

/// How bad a POST-boundary failure is (issue #1621, §4.6).
///
/// "Post-boundary" means `exec::execute_with_teardown` returned the RAW executor
/// error rather than one of its two clean-failure variants: the go-live op already
/// succeeded, no teardown ran, and the host **is** on the new release. What to do
/// about that depends entirely on WHICH step failed, and the three answers are
/// materially different — which is why this is a classification and not a boolean.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PostBoundaryClass {
    /// The host is live and healthy; only bookkeeping failed. **Warn and continue**
    /// the rollout — rolling a whole fleet back because an `rm -rf` of old release
    /// dirs failed on host 3 is a self-inflicted outage.
    Housekeeping,
    /// The marker triple is mid-transaction. **Halt, and never auto-roll-back this
    /// host** — the rollback target cannot be trusted.
    Ambiguous,
    /// The host is live on the new release and something that matters failed.
    /// **Halt and compensate**, including this host.
    Functional,
}

/// Post-boundary labels that do not affect traffic (issue #1621, §4.6).
///
/// All three run strictly AFTER `proxy-flip`, so the proxy is already serving the
/// new release when they run:
///
/// - `record-proxy-options` — writes the `shared/proxy-options` marker. Its failure
///   is invisible today and fails the NEXT deploy closed
///   (`refuse_unprovable_proxy_options`), which is exactly why it must be surfaced
///   rather than swallowed.
/// - `drain-old` — disables the now-idle old slot unit. Two slots running is
///   untidy, not an outage.
/// - `prune` — removes old release dirs. Disk hygiene.
pub(crate) const HOUSEKEEPING_LABELS: [&str; 3] = ["record-proxy-options", "drain-old", "prune"];

/// The one post-boundary label whose failure makes a host's rollback target
/// unprovable: `commit-markers` writes previous-release + `current` + live-slot as
/// ONE remote transaction, so a failure inside it can leave any subset applied.
pub(crate) const AMBIGUOUS_MARKERS_LABEL: &str = "commit-markers";

/// Classify a POST-boundary failure by its op label (issue #1621, §4.6, T1.15).
///
/// Unknown labels fall through to [`PostBoundaryClass::Functional`] — the
/// fail-closed direction. A future op added after the boundary is then treated as
/// mattering until someone deliberately adds it to [`HOUSEKEEPING_LABELS`], rather
/// than being silently waved through as harmless.
pub(crate) fn classify_post_boundary(label: &str) -> PostBoundaryClass {
    if HOUSEKEEPING_LABELS.contains(&label) {
        PostBoundaryClass::Housekeeping
    } else if label == AMBIGUOUS_MARKERS_LABEL {
        PostBoundaryClass::Ambiguous
    } else {
        PostBoundaryClass::Functional
    }
}

/// Why an automatic rollback was declined at `commit-markers`.
pub(crate) const MANUAL_AMBIGUOUS_MARKERS: &str =
    "release markers left mid-transaction by `commit-markers`";

/// Why an automatic rollback was declined by the target-dir precheck (§4.7).
pub(crate) const MANUAL_ROLLBACK_TARGET_MISSING: &str = "rollback target release dir missing";

/// Why an automatic rollback was declined when the target-dir probe proved nothing.
pub(crate) const MANUAL_ROLLBACK_TARGET_UNPROVABLE: &str =
    "rollback target release dir could not be verified";

/// Why an automatic rollback was declined when the host records no previous release.
pub(crate) const MANUAL_NO_PREVIOUS_RELEASE: &str = "no previous release recorded to roll back to";

/// What the fleet does to ONE host when compensating a halted rollout (issue
/// #1621, §4.7).
///
/// The `usize` is the host's index in rollout order, which is index-parallel to
/// `FleetPlan::hosts` / `ResolvedFleet::hosts` / the probe list — the same
/// convention the driver uses everywhere, and the reason no host name appears here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RollbackAction {
    /// Roll this host back to its previous release with the EXISTING on-demand
    /// rollback primitives ([`exec::resolve_rollback_target`] →
    /// [`exec::rollback_ops`] → [`exec::execute_rollback`]).
    Rollback(usize),
    /// Remove this host's just-completed FIRST deploy
    /// ([`exec::first_deploy_teardown_ops`]).
    ///
    /// A first deploy has no `shared/previous-release` marker, so there is nothing
    /// to roll back TO: the honest compensation is to return the host to
    /// nothing-installed, which is also the state that makes the next `deploy up`
    /// correctly take the First path again. A half-installed host an external load
    /// balancer may already be probing is worse than a clean absence.
    ///
    /// **Known gap (documented, not fixed here):** [`ProxyController`] exposes no
    /// deregister op, so the host's kamal-proxy still holds a route for the service
    /// pointing at the (now stopped) slot port — that host's public port answers
    /// 502 instead of connection-refused until it is deployed again. Removing the
    /// route needs a new `ProxyController` method and its own exact-vector tests;
    /// it is tracked as follow-up work, and the state table names the host so the
    /// operator is never surprised by it.
    Teardown(usize),
    /// Do NOT touch this host automatically; report it and the reason.
    Manual(usize, &'static str),
}

/// The compensation set for a halted rollout — **pure**, in strict REVERSE rollout
/// order (issue #1621, §4.7, AC-3).
///
/// Reverse order is not cosmetic: the hosts that cut over LAST are the ones that
/// have been serving the new release for the shortest time, and undoing them first
/// shrinks the mixed window from the newest end. Every host that is on the new
/// release is in the set, including the host that failed post-boundary and
/// including hosts marked [`HostOutcome::Degraded`] — a degraded host is live and
/// healthy on the NEW release, so leaving it there while every other host returns
/// to the old one is precisely the mixed-version fleet AC-3 forbids. (Its
/// housekeeping debris is still reported: the driver records it independently of
/// this outcome slot.)
///
/// Hosts that are already clean ([`HostOutcome::RolledBack`] /
/// [`HostOutcome::TornDown`] — their own per-host teardown ran) and hosts that were
/// never reached are absent. `modes` decides rollback-vs-teardown per host; the
/// function is total, so a short `modes` slice simply yields a shorter set rather
/// than panicking mid-rollout.
pub(crate) fn fleet_rollback_set(
    outcomes: &[HostOutcome],
    modes: &[HostMode],
) -> Vec<RollbackAction> {
    outcomes
        .iter()
        .zip(modes)
        .enumerate()
        .rev()
        .filter_map(|(index, (outcome, mode))| match outcome {
            // On the new release: compensate by this host's deploy path.
            HostOutcome::Serving | HostOutcome::Degraded { .. } | HostOutcome::LiveOnNew { .. } => {
                Some(match mode {
                    HostMode::First => RollbackAction::Teardown(index),
                    HostMode::Redeploy => RollbackAction::Rollback(index),
                })
            }
            // On the new release, but its rollback target cannot be trusted.
            HostOutcome::AmbiguousMarkers => {
                Some(RollbackAction::Manual(index, MANUAL_AMBIGUOUS_MARKERS))
            }
            // Already clean, never reached, or already compensated.
            HostOutcome::Untouched
            | HostOutcome::RolledBack { .. }
            | HostOutcome::TornDown { .. }
            | HostOutcome::CompensatedRollback
            | HostOutcome::CompensatedTeardown
            | HostOutcome::CompensationFailed { .. }
            | HostOutcome::Manual { .. } => None,
        })
        .collect()
}

/// The step label to attribute a failure to — always a `&'static str` op label, so
/// a fleet error can never carry a shell line, a remote path, or a driver message.
///
/// Secrets discipline is load-bearing here: `DeployExecError`'s own `Display` is
/// already redacted, but the fleet error/summary types quote only labels and host
/// names, never a formatted source error.
pub(crate) fn failed_step_label(err: &exec::DeployExecError) -> &'static str {
    match err {
        exec::DeployExecError::CommandFailed { label, .. } => label,
        exec::DeployExecError::CandidateRolledBack { failed_step, .. }
        | exec::DeployExecError::FirstDeployTornDown { failed_step, .. }
        | exec::DeployExecError::RollbackFailed { failed_step, .. } => failed_step,
        // The post-cutover wrapper is a CLASSIFIER input, not a reporting one: it
        // adds the op that was running, while the operator-facing step label stays
        // exactly what the underlying failure would have reported on its own. A
        // dropped transport is still attributed to the transport, never to a
        // fabricated step (see `attributed_step_label` for the other question).
        exec::DeployExecError::PostCutover { source, .. } => failed_step_label(source),
        exec::DeployExecError::UploadFailed { .. } => "upload",
        exec::DeployExecError::Stage { .. } => "stage-local-file",
        exec::DeployExecError::Spawn { .. } => "ssh-transport",
        exec::DeployExecError::PreflightAborted { .. } => "preflight",
        exec::DeployExecError::ProxyIncompatible { .. } => "proxy-compat-probe",
        exec::DeployExecError::NoPreviousRelease => "resolve-previous",
    }
}

/// The label of the op that was actually RUNNING when this failure landed, when
/// the error proves one — `None` when nothing does (issue #1621, §4.6).
///
/// Deliberately not the same question as [`failed_step_label`], which always
/// answers something so it can be printed. Several executor errors name no op at
/// all ([`exec::DeployExecError::Spawn`] and friends are raised by the local
/// transport, not by a step), and post-boundary that distinction decides whether a
/// host may be touched — so it is asked explicitly rather than inferred from a
/// synthesized label string.
///
/// `exec::execute_with_teardown` attaches the op label to every failure past a
/// known go-live boundary, which is what makes this answerable for the error shapes
/// that carry none of their own.
const fn attributed_step_label(err: &exec::DeployExecError) -> Option<&'static str> {
    match err {
        exec::DeployExecError::CommandFailed { label, .. } => Some(label),
        exec::DeployExecError::PostCutover { failed_step, .. }
        | exec::DeployExecError::CandidateRolledBack { failed_step, .. }
        | exec::DeployExecError::FirstDeployTornDown { failed_step, .. }
        | exec::DeployExecError::RollbackFailed { failed_step, .. } => Some(failed_step),
        exec::DeployExecError::Spawn { .. }
        | exec::DeployExecError::UploadFailed { .. }
        | exec::DeployExecError::Stage { .. }
        | exec::DeployExecError::PreflightAborted { .. }
        | exec::DeployExecError::ProxyIncompatible { .. }
        | exec::DeployExecError::NoPreviousRelease => None,
    }
}

/// The post-boundary class for one executor ERROR, rather than for a bare label
/// (issue #1621, §4.6).
///
/// [`classify_post_boundary`] answers "how bad is a failure of THIS op", which is
/// the right question only when the op is known. Attribution is then used in
/// exactly one direction — it can make the verdict more conservative, never less:
///
/// * **`commit-markers` always wins.** The marker triple is written as one remote
///   transaction, so any failure inside it leaves the rollback target unprovable —
///   whether it arrived as a non-zero remote exit or as a dropped transport that
///   never launched the command. Keying that guard on the error shape is how a host
///   gets auto-rolled-back onto a stale `shared/previous-release` target.
/// * **A label-less failure never DOWNGRADES to housekeeping.** A transport that
///   dropped during `drain-old` is not proof that only bookkeeping broke: the deploy
///   host can no longer talk to this host at all, so nothing about it is provable
///   and it stays [`PostBoundaryClass::Functional`] — halt and compensate — exactly
///   as it does today.
/// * **An unattributable failure fails CLOSED.** If nothing names the op, we cannot
///   say which post-cutover step broke, which is precisely when the rollback target
///   is least provable — so decline to touch the host
///   ([`PostBoundaryClass::Ambiguous`]) rather than compensate it blind.
fn post_boundary_class(err: &exec::DeployExecError, reported_label: &str) -> PostBoundaryClass {
    match attributed_step_label(err) {
        // The marker transaction (whatever error shape carried the failure), or a
        // failure that names no op at all: both leave the rollback target unprovable.
        Some(AMBIGUOUS_MARKERS_LABEL) | None => PostBoundaryClass::Ambiguous,
        // Otherwise today's policy, keyed on the label the failure REPORTS — so a
        // label-less transport error is never waved through as housekeeping.
        Some(_) => classify_post_boundary(reported_label),
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
pub(crate) fn classify_failure(err: &exec::DeployExecError) -> HostOutcome {
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

/// The outcome the fleet driver records for one failed host: [`classify_failure`]
/// (did traffic move?) refined by [`classify_post_boundary`] (how bad is it?).
///
/// Kept as its own function — rather than folded into [`classify_failure`] —
/// because the two questions have different inputs and different failure modes.
/// `classify_failure` reads the executor's TYPED error and is the discriminator
/// AC-3 turns on; this one encodes a policy about which post-cutover steps matter
/// (see [`post_boundary_class`] for how the failing op is attributed). A
/// pre-boundary outcome passes through untouched: the executor already tore that
/// candidate down, so there is nothing left to classify.
pub(crate) fn classify_host_outcome(err: &exec::DeployExecError) -> HostOutcome {
    match classify_failure(err) {
        HostOutcome::LiveOnNew { failed_step } => match post_boundary_class(err, failed_step) {
            PostBoundaryClass::Housekeeping => HostOutcome::Degraded { label: failed_step },
            PostBoundaryClass::Ambiguous => HostOutcome::AmbiguousMarkers,
            PostBoundaryClass::Functional => HostOutcome::LiveOnNew { failed_step },
        },
        clean => clean,
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

/// The loud line printed when first-deploy hosts sit AHEAD of the fleet's single
/// migration in rollout order (issue #1621, AC-4).
///
/// [`migrate_placement`] puts the migration on the first host whose probed mode is
/// `Redeploy` — a first-deploy host has no live release to keep serving if the
/// migration fails, so it is the wrong place for it. The consequence is that any
/// first deploy listed BEFORE that host runs [`exec::first_deploy_ops`] (which
/// carries no migrate op) and goes live on the new release while the schema is
/// still the old one. Declaration order is a documented contract — the guide tells
/// operators to order the list themselves — so the rollout is not silently
/// reordered; the hazard is named, with the hosts it applies to and the one-line
/// remedy.
pub(crate) const FLEET_FIRST_BEFORE_MIGRATE_NOTE: &str = "go live on the new release BEFORE the migration runs (a first deploy never migrates) \
     — if this release needs the new schema they will serve against the old one until \
     the migrating host is reached; list an already-deployed host first to avoid it";

/// The one-line reminder printed once, after the fleet's single migration lands.
pub(crate) const FLEET_SCHEMA_MOVED_NOTE: &str = "the schema has moved; from here an automatic rollback restores BINARIES only — \
     it never rolls a migration back";

/// The recovery lever named for every host the summary reports as still on the new
/// release (issue #1621, §8.2).
///
/// Both halves matter: `--only <host>` is how a single stranded host is reversed
/// without disturbing the ones that are fine, and the bare command is how a fleet
/// that halted mid-rollout converges back onto one release.
pub(crate) const FLEET_RECOVERY_HINT: &str = "Roll ONE back with `autumn deploy rollback --only <host>`, or the whole fleet with \
     `autumn deploy rollback`.";

/// The line the summary prints once a fleet rollout has actually COMPENSATED hosts
/// after a migration ran (issue #1621, §4.7, T1.12).
///
/// The compensation op vectors are [`exec::rollback_ops`] /
/// [`exec::first_deploy_teardown_ops`], neither of which contains a `migrate` op by
/// construction — so the binaries went back and the schema did not. That asymmetry
/// is the single most dangerous thing about an automatic fleet rollback, so it is
/// stated out loud rather than left to be inferred from the absence of a line.
/// (`rolling_deploy_blocked` grades `up.sql` only, so an automatic `migrate down`
/// would run exactly the SQL nothing reviews, unattended, mid-flip.)
pub(crate) const FLEET_SCHEMA_NOT_ROLLED_BACK_NOTE: &str = "the compensating rollback restored BINARIES only — the migration that already ran was \
     NOT rolled back; confirm the schema still fits the release now serving";

/// Whether this rollout moved the schema, judged conservatively (issue #1621).
///
/// True when the plan scheduled a migration AND the host carrying it was reached at
/// all. It deliberately does NOT try to prove the one-shot itself succeeded: the
/// only signal available is the failing step label, and a pre-boundary failure
/// *after* `migrate` (a readiness timeout, say) moves the schema and still rolls
/// the candidate back. Erring toward "the schema moved" costs one line of output;
/// erring the other way lets an operator roll binaries back believing the schema
/// came with them.
pub(crate) fn schema_moved(plan: &FleetPlan, outcomes: &[HostOutcome]) -> bool {
    plan.hosts.iter().zip(outcomes).any(|(host, outcome)| {
        host.migrate == MigrateStep::Run && !matches!(outcome, HostOutcome::Untouched)
    })
}

/// The exact by-hand recovery instructions for a host the fleet deliberately did
/// NOT roll back (issue #1621, §4.7/§8.2).
///
/// Printed for every [`RollbackAction::Manual`], because "we did not touch it" is
/// only useful next to "here is what to look at". The command is read-only and
/// carries nothing secret: an SSH user, a host, and the two marker paths the deploy
/// already prints on every run. It deliberately does NOT tell the operator to run
/// `autumn deploy rollback --only <host>`, even though that command exists: every
/// `MANUAL_*` reason is a case where the ROLLBACK TARGET ITSELF is in doubt (markers
/// mid-transaction, target dir missing, target unprovable, no previous release), so
/// pointing at the command that trusts that target is precisely the wrong advice.
/// The markers come first; the command comes after a human has read them.
pub(crate) fn manual_recovery_lines(cfg: &ResolvedDeployConfig, reason: &str) -> Vec<String> {
    let host = cfg.host.as_deref().unwrap_or_default();
    let shared = cfg.shared_dir();
    vec![
        format!("  \u{2757} {host}: NOT rolled back automatically \u{2014} {reason}."),
        format!(
            "     inspect: ssh {user}@{host} 'cat {shared}/previous-release {shared}/live-slot; \
             ls {releases}'",
            user = cfg.user,
            releases = cfg.releases_dir(),
        ),
        "     the previous-release marker names the release dir, slot and port this host \
         should return to; restore it by hand before deploying the fleet again."
            .to_owned(),
    ]
}

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
    let host_word = if count == 1 { "host" } else { "hosts" };
    lines.push(format!(
        "Rolling release {release_id} across {count} {host_word}, ONE AT A TIME, in \
         `[deploy] hosts` order:"
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
            // A `--only`-narrowed repair reaches here with a one-host plan, so there
            // is nobody left to skip the migration; the "; N skip it" clause would
            // render as a dangling fragment ("fleet-wide;  skip it").
            lines.push(if skipped.is_empty() {
                format!(
                    "  \u{2192} migrate ({} only \u{2014} the schema is fleet-wide)",
                    migrating.host,
                )
            } else {
                format!(
                    "  \u{2192} migrate ({} only \u{2014} the schema is fleet-wide; {} skip it)",
                    migrating.host,
                    skipped.join(", "),
                )
            });
            // Hosts listed AHEAD of the migrating one that are first deploys cut over
            // before the schema moves (AC-4). Named rather than reordered — see
            // `FLEET_FIRST_BEFORE_MIGRATE_NOTE`.
            let before_migration: Vec<&str> = plan
                .hosts
                .iter()
                .take_while(|h| h.host != migrating.host)
                .filter(|h| h.mode == HostMode::First)
                .map(|h| h.host.as_str())
                .collect();
            if writable_db_configured && !before_migration.is_empty() {
                lines.push(format!(
                    "  \u{26A0}\u{FE0F}  {} {FLEET_FIRST_BEFORE_MIGRATE_NOTE}",
                    before_migration.join(", "),
                ));
            }
        }
        None if writable_db_configured => {
            lines.push(format!("  \u{26A0}\u{FE0F}  {FLEET_NO_MIGRATION_NOTE}"));
        }
        None => {}
    }
    lines
}

/// The ONE per-host table renderer every fleet view goes through (issue #1621,
/// §7.3): the end-of-rollout summary, the fleet rollback summary, the maintenance
/// fan-out summary, and `deploy status`.
///
/// Sharing the primitive is the point. Several surfaces describing the same fleet
/// in slightly different layouts is how an operator ends up comparing tables that
/// disagree at 3 am; here they cannot, because the host column width, the marker
/// slot and the row shape are computed in exactly one place. Each caller supplies
/// its own marker and its own already-composed state text.
fn state_table_lines(title: &str, rows: &[(&'static str, &str, String)]) -> Vec<String> {
    let width = rows
        .iter()
        .map(|(_, host, _)| host.chars().count())
        .max()
        .unwrap_or(0);
    let mut lines = Vec::with_capacity(rows.len() + 1);
    lines.push(title.to_owned());
    for (marker, host, state) in rows {
        lines.push(format!("  {marker} {host:<width$}  {state}"));
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
    let mut rows: Vec<(&'static str, &str, String)> = Vec::with_capacity(plan.hosts.len());
    for (host, outcome) in plan.hosts.iter().zip(outcomes) {
        let state = match outcome {
            HostOutcome::Untouched => "untouched (not reached)".to_owned(),
            HostOutcome::Serving => format!("serving {release_id}"),
            // Live and healthy on the new release: only bookkeeping failed. The
            // remedy is named because a failed `record-proxy-options` makes the NEXT
            // deploy of this host fail closed.
            HostOutcome::Degraded { label } => format!(
                "serving {release_id} \u{2014} DEGRADED: `{label}` failed after the cutover; \
                 traffic is fine, but repair it (a redeploy of this host does) before the \
                 next deploy refuses it"
            ),
            // Traffic already moved before the failure, so this host IS on the new
            // release — saying only "failed" would be the dangerous half-truth.
            HostOutcome::LiveOnNew { failed_step } => {
                format!(
                    "serving {release_id} \u{2014} but `{failed_step}` failed AFTER the cutover"
                )
            }
            HostOutcome::AmbiguousMarkers => format!(
                "serving {release_id} \u{2014} `{AMBIGUOUS_MARKERS_LABEL}` failed AFTER the \
                 cutover, so the release markers are mid-transaction and this host was NOT \
                 rolled back automatically"
            ),
            HostOutcome::Manual { reason } => {
                format!("serving {release_id} \u{2014} NOT rolled back automatically: {reason}")
            }
            HostOutcome::RolledBack { failed_step } => {
                format!("previous release still serving (rolled back at `{failed_step}`)")
            }
            HostOutcome::TornDown { failed_step } => {
                format!("NOTHING serving (first deploy torn down at `{failed_step}`)")
            }
            HostOutcome::CompensatedRollback => {
                "previous release restored (rolled back by the fleet)".to_owned()
            }
            HostOutcome::CompensatedTeardown => {
                "NOTHING serving (the fleet removed this host's new first deploy; its proxy \
                 still holds the route until the next deploy)"
                    .to_owned()
            }
            HostOutcome::CompensationFailed { failed_step } => format!(
                "serving {release_id} \u{2014} the compensating rollback FAILED at \
                 `{failed_step}`"
            ),
        };
        let marker = match outcome {
            HostOutcome::Serving => "\u{2705}",
            HostOutcome::Untouched => "\u{2013}",
            HostOutcome::Degraded { .. } => "\u{26A0}\u{FE0F} ",
            HostOutcome::CompensatedRollback | HostOutcome::CompensatedTeardown => {
                "\u{21A9}\u{FE0F} "
            }
            _ => "\u{274C}",
        };
        rows.push((marker, host.host.as_str(), state));
    }
    let mut lines = state_table_lines("Fleet state:", &rows);
    let on_new: Vec<&str> = plan
        .hosts
        .iter()
        .zip(outcomes)
        .filter(|(_, outcome)| {
            matches!(
                outcome,
                HostOutcome::Serving
                    | HostOutcome::Degraded { .. }
                    | HostOutcome::LiveOnNew { .. }
                    | HostOutcome::AmbiguousMarkers
                    | HostOutcome::Manual { .. }
                    | HostOutcome::CompensationFailed { .. }
            )
        })
        .map(|(host, _)| host.host.as_str())
        .collect();
    if !on_new.is_empty() {
        lines.push(format!(
            "  On {release_id}: {}. {FLEET_RECOVERY_HINT}",
            on_new.join(", ")
        ));
        // Gated on the MIGRATION, not on liveness: `on_new` says hosts are serving
        // the new release, which is a different question. An all-first-deploy fleet
        // schedules no migration at all, and the same run's header says so out loud
        // ("run `autumn migrate` yourself before serving traffic") — asserting the
        // schema moved here would negate that instruction on the last line before
        // the green one.
        if schema_moved(plan, outcomes) {
            lines.push(format!("  \u{26A0}\u{FE0F}  {FLEET_SCHEMA_MOVED_NOTE}"));
        }
    }
    // Say the binaries-only truth once the fleet has ACTUALLY undone hosts: at that
    // point the operator is looking at a fleet back on the old release with a schema
    // that is not.
    let compensated = outcomes.iter().any(|outcome| {
        matches!(
            outcome,
            HostOutcome::CompensatedRollback
                | HostOutcome::CompensatedTeardown
                | HostOutcome::CompensationFailed { .. }
        )
    });
    if compensated && schema_moved(plan, outcomes) {
        lines.push(format!(
            "  \u{26A0}\u{FE0F}  {FLEET_SCHEMA_NOT_ROLLED_BACK_NOTE}"
        ));
    }
    lines
}

/// The closing line of a successful fleet rollout (issue #1621, §8.2).
///
/// Pure so both shapes can be pinned by a unit test rather than by reading
/// stderr. Two axes, and both must be said out loud:
///
/// * **Degraded hosts.** Every host serves the new release, so this is still a
///   success — but a host whose `record-proxy-options` never landed fails its
///   NEXT deploy closed, so an unqualified "complete" would hide it.
/// * **A narrowed run.** `--only` builds the plan, the outcomes and the state
///   table over the SELECTED hosts only, so `total` is the narrowed count. Saying
///   "all {total} hosts serving …" there reads as a full-fleet success while the
///   hosts the operator deliberately skipped are still on their old release —
///   hundreds of lines after the `--only` warning that said so. The excluded hosts
///   are counted, never named as a state: their state was never probed (probing
///   skipped hosts is deliberately refused), so naming a release for them would be
///   inventing it.
pub(crate) fn fleet_completion_line(
    release_id: &str,
    total: usize,
    configured_host_count: usize,
    degraded: usize,
) -> String {
    let hosts_word = if total == 1 { "host" } else { "hosts" };
    let untouched = configured_host_count.saturating_sub(total);
    let degraded_clause = if degraded == 0 {
        String::new()
    } else {
        format!(
            " {degraded} finished with post-cutover housekeeping failures (above) \u{2014} \
             repair them before the next deploy."
        )
    };
    let marker = if degraded == 0 {
        "\u{2705}"
    } else {
        "\u{26A0}\u{FE0F} "
    };
    if untouched == 0 {
        // Byte-identical to pre-review for a full fleet.
        return if degraded == 0 {
            format!(
                "\n\u{2705} Fleet deploy complete \u{2014} all {total} hosts serving {release_id}."
            )
        } else {
            format!(
                "\n\u{26A0}\u{FE0F}  Fleet deploy complete \u{2014} all {total} hosts serve \
                 {release_id}, but {degraded} finished with post-cutover housekeeping failures \
                 (above). Repair them before the next deploy."
            )
        };
    }
    let untouched_word = if untouched == 1 { "host" } else { "hosts" };
    format!(
        "\n{marker} `--only` run complete \u{2014} the {total} {hosts_word} it targeted now serve \
         {release_id}.{degraded_clause} THIS RUN WAS NARROWED: the other {untouched} configured \
         {untouched_word} were NOT touched and may be on another release \u{2014} finish with a \
         full `autumn deploy up` (no `--only`) so the fleet converges."
    )
}

// ── `deploy status`: per-host state and drift (issue #1621, AC-6) ────────────

/// The deployed release a host reports, or the honest absence of one.
///
/// `Unknown` is a first-class, DISTINCT state — never folded into drift. The only
/// release identity a host can be asked for is its `current` symlink's basename
/// (no runtime endpoint reports one: the probe body's `version` is the framework
/// crate version, identical on every host forever), and that symlink can be
/// missing on a host that has never been deployed, dangling mid-incident, or
/// unreadable because the SSH round-trip degraded. Calling any of those "a
/// different version" would report a mixed fleet that does not exist, and a false
/// alarm at 3 am is worse than no alarm.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReleaseId {
    /// The release id read from the host's `current` symlink.
    Known(String),
    /// The host did not report a release (never deployed, symlink unreadable, or
    /// the host was unreachable). Reported, never counted as drift.
    Unknown,
}

/// State drift: the live-slot marker names a different slot than the proxy is
/// actually serving.
///
/// Not cosmetic — the next redeploy picks the candidate slot from this marker, so
/// a drifted marker makes it restart the slot that is CURRENTLY SERVING, taking
/// the host down mid-deploy. (`deploy up` repairs it automatically; `status`
/// surfaces it before then.)
pub(crate) const DRIFT_LIVE_SLOT_MARKER: &str =
    "live-slot marker disagrees with the slot kamal-proxy is serving";

/// State drift: this host has no release deployed at all, while the rest of the
/// fleet is serving one (issue #1621, AC-6).
///
/// Deliberately NOT version drift — [`ReleaseId::Unknown`] never contributes to
/// that, and it must not start to. This is a different question: `HostMode::First`
/// is a PROVEN absence (the probe shell tests `[ -L current ]`, so even a dangling
/// symlink reports `Redeploy`), so a fleet whose peers are serving a release has a
/// host that is carrying none of the traffic the operator provisioned it for. That
/// is exactly the state the standing `deploy status --strict` alert exists to catch
/// — a compensated first deploy, or a host added to `[deploy] hosts` and never
/// deployed.
pub(crate) const DRIFT_HOST_NOT_DEPLOYED: &str = "no release is deployed on this host while the rest of the fleet is serving one — it is \
     carrying no traffic (deploy it with `autumn deploy up`)";

/// State drift: the `shared/proxy-options` marker exists but cannot be parsed.
///
/// The refuse-unprovable-proxy-options guard fails CLOSED on this, so the next
/// deploy of this host is already broken — it just has not been attempted yet.
/// That is exactly the kind of debris a status command exists to surface.
pub(crate) const DRIFT_PROXY_OPTIONS_UNREADABLE: &str =
    "shared/proxy-options marker is unreadable — the NEXT deploy of this host will refuse";

/// State drift: the installed kamal-proxy unit binds a different public port than
/// this project configures.
///
/// The redeploy path refuses a concurrent public-port change (it would strand the
/// old port mid-cutover), so this host cannot be redeployed until the two agree.
pub(crate) const DRIFT_PROXY_PORT_MISMATCH: &str =
    "the installed proxy unit binds a different public port than `[server] port` configures";

/// State drift: this host claims a promoted release its `current` symlink could
/// not be resolved to (issue #1621, review round 2).
///
/// The probe shell tests `[ -L current ]`, which succeeds for a symlink whose
/// target cannot be canonicalized, so such a host reports `HostMode::Redeploy`
/// while `readlink -f` yields nothing and the release reads back
/// [`ReleaseId::Unknown`]. That combination is not "we have not looked" — it is a
/// host that says it is serving a release nobody can name, which is exactly the
/// unprovable state this feature fails closed on.
///
/// It stays out of VERSION drift (an unknown release still names no version to be
/// mixed with), and it is the reason the NEXT deploy matters: `commit-markers`
/// copies `readlink current` verbatim into `previous-release`, so deploying this
/// host records an unresolvable directory as its rollback target — the rollback
/// then refuses (`probe_rollback_target_dir` fails closed) instead of working.
pub(crate) const DRIFT_RELEASE_UNREADABLE: &str = "this host has a `current` symlink but the release it points at could not be read (a broken \
     symlink or a missing releases dir) — repair it before the next deploy, which would record \
     that unresolvable target as this host's rollback point";

/// State drift: this host's live slot unit could not be read, so the CLI cannot
/// prove WHICH maintenance flag file the running app polls (issue #1621, review
/// round 1).
///
/// The maintenance column reports `?` rather than a confident `ON`/`off` — an
/// operator closing a live window must never be told it is closed on the strength
/// of a file the running unit may not even read.
pub(crate) const DRIFT_MAINTENANCE_UNPROVEN: &str = "the live slot unit could not be read, so which maintenance flag file this host's app \
     polls is unknown — the maintenance column reports `?` rather than guessing";

/// State drift: this host's app polls a maintenance flag file OTHER than the
/// per-app shared one (issue #1621, review round 1).
///
/// Almost always a slot unit rendered BEFORE #1621: it carries no
/// `AUTUMN_MAINTENANCE_FLAG_FILE`, so the runtime falls back to the cwd-relative
/// `tmp/autumn-maintenance.json` under its release dir. The maintenance column
/// reports THAT file (the truth for this host), but the flag is orphaned by the
/// next cutover and a fleet-wide `deploy maintenance` reaches it only best-effort.
pub(crate) const DRIFT_MAINTENANCE_UNSHARED_FLAG: &str = "this host's app polls a release-local maintenance flag file, not the shared one (its slot \
     unit predates AUTUMN_MAINTENANCE_FLAG_FILE) — redeploy it so maintenance survives cutovers";

/// The sentence printed under the status table to bound the `last deploy` column
/// (issue #1621, AC-6).
///
/// The column exists because AC-6 asks for the per-host last deploy result, and no
/// other on-host artefact answers it. Its scope is narrow and stating it is the
/// whole point: the marker is written by the ops that COMPLETE a cutover, so a
/// deploy that failed BEFORE the cutover boundary is torn down without touching
/// it and the host still shows its previous action. It is therefore the host's own
/// last completed action, never a verdict on the last fleet rollout — the rollout
/// itself exits non-zero and prints the per-host outcome table.
pub(crate) const LAST_DEPLOY_SCOPE_NOTE: &str = "`last deploy` is the last action each host COMPLETED (`deployed` / `rolled back`, with \
     the host's UTC time) \u{2014} a `rolled back` host was compensated or rolled back after its \
     last cutover. A deploy that failed BEFORE cutover never changes it, so it is not a verdict \
     on the last rollout, and it is reported rather than counted as drift.";

/// The sentence every maintenance-related surface prints, because the mechanism is
/// counter-intuitive and getting it wrong causes an outage (plan §6.4).
///
/// `build_maintenance_layer` puts `/ready` on the probe-bypass list, so a host in
/// maintenance mode keeps answering its load balancer's health check with 200 and
/// stays in rotation, serving 503s with `Retry-After` to real users. That is
/// DELIBERATE — gating `/ready` would eject every host from the pool the moment
/// maintenance was enabled — but it means maintenance mode is not a drain, and
/// `deploy status` therefore reports readiness and maintenance in SEPARATE columns.
pub(crate) const MAINTENANCE_DOES_NOT_DRAIN_NOTE: &str = "maintenance mode does NOT drain a host from your load balancer: `/ready` stays 200 by \
     design, so a maintained host keeps taking traffic and answers it with 503. Drain at the \
     load balancer if you need the host out of rotation.";

/// One host's observed state, as `deploy status` reports it (issue #1621, AC-6).
///
/// Pure data assembled from a read-only probe. It carries the FACTS, never a
/// verdict: [`fleet_drift`] is the (pure) function that turns a fleet of these into
/// a report, so the drift rules are unit-testable against synthetic hosts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HostStatus {
    /// The SSH-reachable address (never an op label — rule 2).
    pub(crate) host: String,
    /// Whether the read-only probe completed at all. Everything below is
    /// unknown/false for an unreachable host, and [`fleet_drift`] never derives
    /// drift from it — a network outage is not a deploy defect.
    pub(crate) reachable: bool,
    /// First-deploy vs redeploy, as probed.
    pub(crate) mode: Option<HostMode>,
    /// The release the host's `current` symlink names.
    pub(crate) release: ReleaseId,
    /// The slot serving now, reconciled against the live proxy.
    pub(crate) live_slot: Option<&'static str>,
    /// HTTP status the live slot's loopback `/ready` returned, `None` when nothing
    /// answered.
    pub(crate) ready_code: Option<u16>,
    /// Maintenance as the host's RUNNING slot unit observes it — three-valued, so
    /// an unprovable state is never rendered as a confident on/off (issue #1621,
    /// review round 1). ORTHOGONAL to `ready_code` — see
    /// [`MAINTENANCE_DOES_NOT_DRAIN_NOTE`].
    pub(crate) maintenance: exec::MaintenanceStatus,
    /// Which flag file that verdict came from.
    pub(crate) maintenance_flag_source: exec::MaintenanceFlagSource,
    /// The `--http-port` the installed proxy unit binds.
    pub(crate) installed_proxy_port: exec::InstalledProxyPort,
    /// The TLS/host options the last forward deploy recorded.
    pub(crate) proxy_options: exec::ProxyOptionsMarker,
    /// The public port THIS project configures, for the mismatch comparison.
    pub(crate) public_port: u16,
    /// Whether the live-slot marker disagrees with the running proxy.
    pub(crate) live_slot_marker_drift: bool,
    /// The last state-changing deploy action this host COMPLETED (AC-6's third
    /// per-host fact), or `None` when the host has never completed one and when
    /// the marker could not be read.
    ///
    /// Scope is narrow and stated in the output: it is the host's own last
    /// completed action (`deployed` / `rolled back` + when), NOT a verdict on the
    /// last fleet rollout — a host whose deploy failed before the cutover boundary
    /// is torn down without rewriting it. See `exec::last_deploy_marker`.
    pub(crate) last_deploy: Option<exec::LastDeploy>,
}

impl HostStatus {
    /// The status of a host whose read-only probe could not be completed.
    ///
    /// Deliberately not an error: `deploy status` reports the whole fleet or it is
    /// useless during the incident it exists for, so one unreachable host is a row,
    /// not an abort.
    pub(crate) fn unreachable(host: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            reachable: false,
            mode: None,
            release: ReleaseId::Unknown,
            live_slot: None,
            ready_code: None,
            // Nothing was probed, so nothing is proved — and `fleet_drift` never
            // derives drift from an unreachable host, so this stays a bare `?`.
            maintenance: exec::MaintenanceStatus::Unknown,
            maintenance_flag_source: exec::MaintenanceFlagSource::Unknown,
            installed_proxy_port: exec::InstalledProxyPort::Absent,
            proxy_options: exec::ProxyOptionsMarker::Absent,
            public_port: 0,
            live_slot_marker_drift: false,
            last_deploy: None,
        }
    }

    /// Assemble a status row from one host's read-only probe — PURE, so every
    /// mapping rule is testable from synthetic probe values.
    pub(crate) fn from_probe(
        cfg: &ResolvedDeployConfig,
        public_port: u16,
        probe: &exec::HostStatusProbe,
    ) -> Self {
        let reconcile = probe.reconcile(cfg, public_port);
        Self {
            host: cfg.host.clone().unwrap_or_default(),
            reachable: true,
            mode: Some(HostMode::from_deploy_mode(&probe.deploy.mode)),
            release: probe
                .deploy
                .current_release_dir
                .as_deref()
                .and_then(exec::release_id_from_dir)
                .map_or(ReleaseId::Unknown, |id| ReleaseId::Known(id.to_owned())),
            live_slot: reconcile.as_ref().map(|r| r.live_slot),
            ready_code: probe.ready_code,
            maintenance: probe.maintenance,
            maintenance_flag_source: probe.maintenance_flag_source,
            installed_proxy_port: probe.deploy.installed_proxy_port.clone(),
            proxy_options: probe.deploy.last_proxy_options.clone(),
            public_port,
            live_slot_marker_drift: reconcile.is_some_and(|r| r.repair),
            last_deploy: probe.last_deploy.clone(),
        }
    }
}

/// What `deploy status` concluded about the fleet (issue #1621, §7.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DriftReport {
    /// Each host's reported release, in declaration order — the EVIDENCE, printed
    /// alongside the verdict so an operator can check it.
    pub(crate) releases: Vec<(String, ReleaseId)>,
    /// More than one DISTINCT KNOWN release across the fleet.
    pub(crate) version_drift: bool,
    /// Per-host state disagreements, each with its reason. Kept separate from
    /// `version_drift` because these are the failures that will make a FUTURE
    /// deploy fail closed — folding them into "version drift" would hide them
    /// behind a perfectly converged fleet.
    pub(crate) state_drift: Vec<(String, &'static str)>,
}

impl DriftReport {
    /// Whether ANY drift was found — the `--strict` exit condition.
    pub(crate) const fn drifted(&self) -> bool {
        self.version_drift || !self.state_drift.is_empty()
    }

    /// Hosts that reported no release, named so "unknown" is visible rather than
    /// inferred from a blank column.
    pub(crate) fn unknown_releases(&self) -> Vec<&str> {
        self.releases
            .iter()
            .filter(|(_, release)| matches!(release, ReleaseId::Unknown))
            .map(|(host, _)| host.as_str())
            .collect()
    }

    /// The distinct KNOWN releases across the fleet, in first-seen order — what
    /// version drift is actually about.
    pub(crate) fn known_releases(&self) -> Vec<&str> {
        let mut seen: Vec<&str> = Vec::new();
        for (_, release) in &self.releases {
            if let ReleaseId::Known(id) = release
                && !seen.contains(&id.as_str())
            {
                seen.push(id.as_str());
            }
        }
        seen
    }
}

/// Reduce a fleet's observed status to a drift report — PURE (issue #1621, §7.2).
///
/// Two rules carry all the weight:
///
/// 1. **Version drift is more than one DISTINCT KNOWN release.** `Unknown` never
///    contributes (see [`ReleaseId`]). A partially-compensated fleet legitimately
///    holds up to three distinct ids, and the report says so — that is information,
///    not corruption.
/// 2. **State drift is per host and separate.** Each reason is a `&'static str`
///    (never a shell line or a driver error), and each names a condition that will
///    make a FUTURE deploy of that host fail closed or take the wrong slot.
///
/// An UNREACHABLE host contributes an `Unknown` release and no state drift at all:
/// its markers were never read, so deriving drift from their absence would report
/// a network outage as a deploy defect.
pub(crate) fn fleet_drift(hosts: &[HostStatus]) -> DriftReport {
    let releases: Vec<(String, ReleaseId)> = hosts
        .iter()
        .map(|status| (status.host.clone(), status.release.clone()))
        .collect();

    let mut known: Vec<&str> = Vec::new();
    for status in hosts {
        if let ReleaseId::Known(id) = &status.release
            && !known.contains(&id.as_str())
        {
            known.push(id.as_str());
        }
    }

    // Whether ANY reachable host proved it is serving a release — the peer a
    // not-deployed host is judged against.
    let any_release_deployed = hosts
        .iter()
        .any(|status| status.reachable && matches!(status.release, ReleaseId::Known(_)));

    let mut state_drift: Vec<(String, &'static str)> = Vec::new();
    for status in hosts.iter().filter(|status| status.reachable) {
        // A PROVEN absence, judged against the fleet: nothing is deployed here while
        // a peer is serving a release. A fleet that has never been deployed at all
        // has no peer to be out of step with, so it stays silent.
        if status.mode == Some(HostMode::First) && any_release_deployed {
            state_drift.push((status.host.clone(), DRIFT_HOST_NOT_DEPLOYED));
        }
        // The mirror image, and the one `--strict` used to exit 0 on: the host
        // PROVED it has a `current` symlink (that is what `Redeploy` means) yet the
        // release behind it could not be read. Unknown still contributes no VERSION
        // drift — rule 1 above is untouched — but it is actionable marker damage,
        // so it is state drift. Judged against nothing: a lone damaged host needs
        // the same alert a damaged host in a big fleet does.
        if status.mode == Some(HostMode::Redeploy) && matches!(status.release, ReleaseId::Unknown) {
            state_drift.push((status.host.clone(), DRIFT_RELEASE_UNREADABLE));
        }
        if status.live_slot_marker_drift {
            state_drift.push((status.host.clone(), DRIFT_LIVE_SLOT_MARKER));
        }
        if matches!(status.proxy_options, exec::ProxyOptionsMarker::Unreadable) {
            state_drift.push((status.host.clone(), DRIFT_PROXY_OPTIONS_UNREADABLE));
        }
        // Only a PROVEN different port is drift: `Absent` (no unit yet) and
        // `Unreadable` are handled by the deploy path's own fail-closed guard, and
        // reporting them here would flag every never-deployed host.
        if matches!(status.installed_proxy_port, exec::InstalledProxyPort::Port(port) if port != status.public_port)
        {
            state_drift.push((status.host.clone(), DRIFT_PROXY_PORT_MISMATCH));
        }
        // Review round 1: the maintenance column is only as good as the CLI's
        // knowledge of WHICH file the running unit polls. Both failure shapes are
        // named here rather than buried in the column, because both need an
        // operator action (inspect the unit / redeploy the host). A host with
        // nothing deployed has no slot unit by definition and is already reported
        // as such, so it is not double-flagged.
        if status.mode == Some(HostMode::Redeploy) {
            match status.maintenance_flag_source {
                exec::MaintenanceFlagSource::Unknown => {
                    state_drift.push((status.host.clone(), DRIFT_MAINTENANCE_UNPROVEN));
                }
                exec::MaintenanceFlagSource::Unshared => {
                    state_drift.push((status.host.clone(), DRIFT_MAINTENANCE_UNSHARED_FLAG));
                }
                exec::MaintenanceFlagSource::Shared => {}
            }
        }
    }

    DriftReport {
        version_drift: known.len() > 1,
        releases,
        state_drift,
    }
}

/// One status row's eight display cells, in column order: mode, release, slot,
/// readiness, maintenance, proxy port, last deploy, drift reasons. Extracted from
/// [`fleet_status_lines`] so the renderer stays within the line budget and the
/// cell wording has one home.
///
/// The last-deploy cell is AC-6's third per-host fact. It reads `last deploy: ?`
/// whenever the marker is absent — a host that has never completed a cutover, or
/// one deployed by a CLI that predates the marker — because "we could not tell"
/// must never render as a result.
fn status_row_cells(status: &HostStatus, report: &DriftReport) -> [String; 8] {
    [
        match status.mode {
            _ if !status.reachable => "unreachable".to_owned(),
            Some(HostMode::First) => "not deployed".to_owned(),
            Some(HostMode::Redeploy) => "deployed".to_owned(),
            None => "unknown".to_owned(),
        },
        match &status.release {
            ReleaseId::Known(id) => id.clone(),
            ReleaseId::Unknown => "release unknown".to_owned(),
        },
        status.live_slot.unwrap_or("slot ?").to_owned(),
        status
            .ready_code
            .map_or_else(|| "ready ?".to_owned(), |code| format!("ready {code}")),
        match status.maintenance {
            exec::MaintenanceStatus::On => "maintenance ON".to_owned(),
            exec::MaintenanceStatus::Off => "maintenance off".to_owned(),
            // Fail closed: "we could not tell" is its own cell, never an `off` that
            // reads as proof the host is serving traffic normally.
            exec::MaintenanceStatus::Unknown => "maintenance ?".to_owned(),
        },
        match &status.installed_proxy_port {
            exec::InstalledProxyPort::Port(port) => format!("proxy {port}"),
            exec::InstalledProxyPort::Absent => "proxy absent".to_owned(),
            exec::InstalledProxyPort::Unreadable => "proxy ?".to_owned(),
        },
        status.last_deploy.as_ref().map_or_else(
            || "last deploy: ?".to_owned(),
            |last| {
                let mut cell = format!("last deploy: {}", last.result);
                if let Some(at) = &last.at {
                    cell.push(' ');
                    cell.push_str(at);
                }
                cell
            },
        ),
        {
            let reasons: Vec<&str> = report
                .state_drift
                .iter()
                .filter(|(host, _)| *host == status.host)
                .map(|(_, reason)| *reason)
                .collect();
            if reasons.is_empty() {
                String::new()
            } else {
                format!("\u{26A0}\u{FE0F}  {}", reasons.join("; "))
            }
        },
    ]
}

/// The `deploy status` table: one aligned row per host, then the verdict.
///
/// Rendered through [`state_table_lines`] — the SAME primitive the rollout summary
/// and the fleet rollback summary use — so the fleet views can never disagree
/// about how a host row looks (plan §7.3).
///
/// Readiness and maintenance are deliberately SEPARATE columns; see
/// [`MAINTENANCE_DOES_NOT_DRAIN_NOTE`].
pub(crate) fn fleet_status_lines(hosts: &[HostStatus], report: &DriftReport) -> Vec<String> {
    let cells: Vec<[String; 8]> = hosts
        .iter()
        .map(|status| status_row_cells(status, report))
        .collect();

    // Column widths, so the composite "state" the shared row renderer prints is
    // itself aligned. The last column is ragged by design (it is prose).
    let mut widths = [0_usize; 8];
    for row in &cells {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }

    let rows: Vec<(&'static str, &str, String)> = hosts
        .iter()
        .zip(&cells)
        .map(|(status, cells)| {
            let marker = if !status.reachable {
                "\u{274C}"
            } else if report
                .state_drift
                .iter()
                .any(|(host, _)| *host == status.host)
                || status.maintenance == exec::MaintenanceStatus::On
            {
                "\u{26A0}\u{FE0F} "
            } else {
                "\u{2705}"
            };
            let mut state = String::new();
            for (index, cell) in cells.iter().enumerate() {
                if index > 0 {
                    state.push_str("  ");
                }
                state.push_str(cell);
                // Pad every column but the last (the ragged prose column) by hand
                // — no `format!` allocation per cell.
                if index + 1 < cells.len() {
                    for _ in cell.chars().count()..widths[index] {
                        state.push(' ');
                    }
                }
            }
            (marker, status.host.as_str(), state.trim_end().to_owned())
        })
        .collect();

    let mut lines = state_table_lines("Fleet status:", &rows);

    if report.version_drift {
        lines.push(format!(
            "  \u{26A0}\u{FE0F}  VERSION DRIFT: {} distinct releases across this fleet ({}). \
             Converge with `autumn deploy up`, or roll the fleet back with \
             `autumn deploy rollback`.",
            report.known_releases().len(),
            report.known_releases().join(", "),
        ));
    }
    let unknown = report.unknown_releases();
    if !unknown.is_empty() {
        // Two different facts, deliberately split: a host that reported
        // `HostMode::First` PROVED it has no `current` symlink, while every other
        // unknown really is "we could not read it". Describing the first as an
        // unreadable symlink sends the operator to inspect something that was never
        // there.
        let (not_deployed, rest): (Vec<&str>, Vec<&str>) = unknown.iter().partition(|host| {
            hosts.iter().any(|status| {
                status.host == **host && status.reachable && status.mode == Some(HostMode::First)
            })
        });
        // Review round 2: a REACHABLE host that reports a `current` symlink whose
        // release could not be read is now state drift, and its reason is printed
        // per host below. Repeating it here under "reported, not counted as drift"
        // would contradict that line — so this footer now covers only the hosts it
        // is still true for (chiefly the unreachable ones, whose markers were never
        // read at all).
        let unreadable: Vec<&str> = rest
            .into_iter()
            .filter(|host| {
                !hosts.iter().any(|status| {
                    status.host == **host
                        && status.reachable
                        && status.mode == Some(HostMode::Redeploy)
                })
            })
            .collect();
        if !not_deployed.is_empty() {
            lines.push(format!(
                "  \u{2139}\u{FE0F}  no release deployed on {} \u{2014} nothing is installed \
                 there yet.",
                not_deployed.join(", "),
            ));
        }
        if !unreadable.is_empty() {
            // Named, and explicitly NOT called drift — the evidence matters more than
            // the verdict here.
            lines.push(format!(
                "  \u{2139}\u{FE0F}  release unknown on {} (the `current` symlink was unreadable) \
                 \u{2014} reported, not counted as drift.",
                unreadable.join(", "),
            ));
        }
    }
    for (host, reason) in &report.state_drift {
        lines.push(format!("  \u{26A0}\u{FE0F}  {host}: {reason}"));
    }
    // AC-6's third fact carries its own limits, printed where it is read rather
    // than only in the guide — a column whose scope is misread is worse than no
    // column. Only when there is a reachable host, so an all-unreachable report
    // stays about the outage.
    if hosts.iter().any(|status| status.reachable) {
        lines.push(format!("  \u{2139}\u{FE0F}  {LAST_DEPLOY_SCOPE_NOTE}"));
    }
    if hosts
        .iter()
        .any(|status| status.maintenance == exec::MaintenanceStatus::On)
    {
        lines.push(format!(
            "  \u{2139}\u{FE0F}  {MAINTENANCE_DOES_NOT_DRAIN_NOTE}"
        ));
    }
    lines
}

// ── Fleet-wide `deploy maintenance` (issue #1621, AC-5) ──────────────────────

/// What the maintenance fan-out did to ONE host (issue #1621, §6.2).
///
/// Best-effort-and-aggregate — the OPPOSITE of the rollout driver. Turning
/// maintenance on for 2 of 3 hosts and then "rolling back" the 2 would push users
/// back into the very window the operator is trying to close, so a partial result
/// is REPORTED, never compensated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MaintenanceOutcome {
    /// The file the host's RUNNING slot unit polls was written/removed — either it
    /// IS the shared authoritative path (a #1621 unit), or its own path was written
    /// alongside the shared one (amendment A2, for a unit deployed before #1621).
    Applied,
    /// Only the shared path was written/removed, and that is the whole job: no
    /// release is promoted on this host, so no running unit polls anything else.
    AppliedSharedOnly,
    /// The shared (authoritative) flag WAS written/removed, but the file the host's
    /// RUNNING unit polls was NOT: its unit could not be read, or the write to that
    /// path failed (issue #1621, review round 3).
    ///
    /// **Counts as a failure** even though the host did change. A pre-#1621 unit
    /// ignores the shared flag entirely, so `on` here may leave the application
    /// serving traffic and `off` may leave it maintained — reporting either as
    /// success is the lie this variant exists to prevent.
    LiveUnitUnchanged {
        /// Label of the step that could not be proved or failed.
        failed_step: &'static str,
    },
    /// The change failed on this host at this step, before anything was written.
    /// Never swallowed, and never a shell line — the label only.
    Failed {
        /// Label of the op that failed.
        failed_step: &'static str,
    },
}

impl MaintenanceOutcome {
    /// Whether this host counts as a failure for the exit code.
    ///
    /// Fails CLOSED: a host whose running unit's flag could not be proved counts as
    /// failed even though its shared flag changed, because the application may not
    /// have observed the change at all.
    pub(crate) const fn failed(self) -> bool {
        matches!(self, Self::Failed { .. } | Self::LiveUnitUnchanged { .. })
    }
}

/// The per-host table printed at the end of a `deploy maintenance on|off` fan-out.
///
/// Printed on EVERY exit path: after a partial change the operator must know which
/// hosts DID change (they may need reversing by hand), and the orthogonality note
/// is repeated because assuming maintenance drains a host from the LB causes an
/// outage (plan §6.4).
pub(crate) fn fleet_maintenance_summary_lines(
    hosts: &[String],
    outcomes: &[MaintenanceOutcome],
    on: bool,
) -> Vec<String> {
    let verb = if on { "enabled" } else { "disabled" };
    let mut rows: Vec<(&'static str, &str, String)> = Vec::with_capacity(hosts.len());
    for (host, outcome) in hosts.iter().zip(outcomes) {
        let (marker, state) = match outcome {
            MaintenanceOutcome::Applied => ("\u{2705}", format!("maintenance {verb}")),
            MaintenanceOutcome::AppliedSharedOnly => (
                "\u{26A0}\u{FE0F} ",
                format!(
                    "maintenance {verb} (shared flag only \u{2014} no release is promoted on \
                     this host, so no running unit polls a release-local flag)"
                ),
            ),
            MaintenanceOutcome::LiveUnitUnchanged { failed_step } => (
                "\u{274C}",
                format!(
                    "PARTIAL \u{2014} shared flag {written}, but the file this host's RUNNING \
                     unit polls was NOT (failed at `{failed_step}`), so this host may still \
                     be {state}",
                    written = if on { "written" } else { "removed" },
                    state = if on {
                        "serving traffic"
                    } else {
                        "in maintenance"
                    },
                ),
            ),
            MaintenanceOutcome::Failed { failed_step } => (
                "\u{274C}",
                format!("UNCHANGED \u{2014} failed at `{failed_step}`"),
            ),
        };
        rows.push((marker, host.as_str(), state));
    }
    let mut lines = state_table_lines("Fleet maintenance state:", &rows);

    let changed: Vec<&str> = hosts
        .iter()
        .zip(outcomes)
        // Only the hosts that are DONE. A partially-changed host is named on the
        // failed side instead, and its own row spells out that its shared flag did
        // change — listing it as simply "changed" would read as "this host is
        // maintained", which is the one thing it is not known to be.
        .filter(|(_, outcome)| !outcome.failed())
        .map(|(host, _)| host.as_str())
        .collect();
    let failed: Vec<&str> = hosts
        .iter()
        .zip(outcomes)
        .filter(|(_, outcome)| outcome.failed())
        .map(|(host, _)| host.as_str())
        .collect();
    if !failed.is_empty() && !changed.is_empty() {
        // Naming the hosts that DID change is the point: a partial change is never
        // rolled back automatically (that would reopen the window being closed), so
        // the operator needs the exact list to reverse by hand if that is the call.
        lines.push(format!(
            "  \u{26A0}\u{FE0F}  Changed anyway: {changed}. NOT reversed automatically \u{2014} \
             reverse them with `autumn deploy maintenance {reverse}` if the fleet must stay \
             consistent while {failed} is repaired.",
            changed = changed.join(", "),
            reverse = if on { "off" } else { "on" },
            failed = failed.join(", "),
        ));
    }
    if on {
        lines.push(format!(
            "  \u{2139}\u{FE0F}  {MAINTENANCE_DOES_NOT_DRAIN_NOTE}"
        ));
    }
    lines
}

// ── `--only`: the repair lever (issue #1621, §3.2) ───────────────────────────

/// The loud warning printed whenever `--only` excludes a configured host.
///
/// `--only` is the lever an operator reaches for when one host is broken, and its
/// whole purpose is to touch a SUBSET — which is, definitionally, the mixed-version
/// fleet the rollout driver otherwise works so hard to prevent. Making that trade
/// silently would train operators to leave fleets half-deployed; saying it out loud
/// every single time is the point.
pub(crate) const FLEET_ONLY_MIXED_WARNING: &str = "`--only` narrows this command to part of the fleet, so the hosts it skips keep \
     running whatever they are running now — THE FLEET MAY END UP MIXED. This is a \
     repair lever, not a faster deploy: finish with a full `autumn deploy up` (no \
     `--only`) so every host converges on one release (#1621).";

/// The warning block for a `--only` selection — empty when nothing was excluded.
///
/// Pure and total: an empty or full selection prints nothing, so an ordinary deploy
/// (and every single-host deploy, which can never exclude anything) is byte-identical
/// to pre-#1621.
pub(crate) fn only_selection_warning_lines(
    configured: &[String],
    selected: &[String],
) -> Vec<String> {
    let skipped: Vec<&str> = configured
        .iter()
        .filter(|host| !selected.iter().any(|picked| picked == *host))
        .map(String::as_str)
        .collect();
    if skipped.is_empty() {
        return Vec::new();
    }
    vec![
        format!("\u{26A0}\u{FE0F}  {FLEET_ONLY_MIXED_WARNING}"),
        format!(
            "   this run touches: {}",
            selected
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(", "),
        ),
        format!("   left as they are: {}", skipped.join(", ")),
        String::new(),
    ]
}

// ── Fleet-wide `deploy rollback` (issue #1621, §3.2) ─────────────────────────

/// What a fleet-wide rollback did to ONE host.
///
/// Only three states, because a fleet rollback is best-effort-continue: every host
/// is attempted, and the interesting question afterwards is simply whether it came
/// back, had nothing to come back to, or failed trying.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RollbackOutcome {
    /// The host is serving its previous release again.
    RolledBack,
    /// The host records no previous release, so there is nothing to roll back to —
    /// a first deploy clears the marker, and a host just added to the fleet never
    /// had one. Reported and SKIPPED: it is not a failure of this host's rollback,
    /// and it must not stop the other hosts.
    NoPreviousRelease,
    /// The rollback was attempted on this host and failed at this step. Never
    /// swallowed, and never a shell line — the label only.
    Failed {
        /// Label of the step that failed.
        failed_step: &'static str,
    },
}

/// The per-host table printed at the end of a fleet-wide rollback.
///
/// Printed on **every** exit path: after a partial rollback the operator's fleet is
/// deliberately mixed, and this table is the only record of which half is which.
pub(crate) fn fleet_rollback_summary_lines(
    hosts: &[String],
    outcomes: &[RollbackOutcome],
) -> Vec<String> {
    let mut rows: Vec<(&'static str, &str, String)> = Vec::with_capacity(hosts.len());
    for (host, outcome) in hosts.iter().zip(outcomes) {
        let (marker, state) = match outcome {
            RollbackOutcome::RolledBack => {
                ("\u{21A9}\u{FE0F} ", "previous release restored".to_owned())
            }
            RollbackOutcome::NoPreviousRelease => (
                "\u{26A0}\u{FE0F} ",
                "SKIPPED \u{2014} no previous release recorded on this host, so there is \
                 nothing to roll back to; it still serves what it served before"
                    .to_owned(),
            ),
            RollbackOutcome::Failed { failed_step } => (
                "\u{274C}",
                format!(
                    "NOT rolled back \u{2014} failed at `{failed_step}`; this host still \
                     serves what it served before"
                ),
            ),
        };
        rows.push((marker, host.as_str(), state));
    }
    let mut lines = state_table_lines("Fleet rollback state:", &rows);
    let stalled: Vec<&str> = hosts
        .iter()
        .zip(outcomes)
        .filter(|(_, outcome)| !matches!(outcome, RollbackOutcome::RolledBack))
        .map(|(host, _)| host.as_str())
        .collect();
    if !stalled.is_empty() {
        // A partly-rolled-back fleet is mixed on purpose-by-accident: say which half
        // is which, and name the single-host lever for finishing the job.
        lines.push(format!(
            "  \u{26A0}\u{FE0F}  The fleet is now MIXED: {} did not roll back. Retry one host \
             with `autumn deploy rollback --only <host>`, or deploy the intended release to \
             it explicitly.",
            stalled.join(", "),
        ));
    }
    // The mirror of the rollout's promise, and just as important here: a rollback
    // moves binaries, never schema.
    lines.push(
        "  \u{26A0}\u{FE0F}  This restored BINARIES only \u{2014} no migration was rolled \
         back; confirm the schema still fits the release now serving."
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
        transport_fail: Vec<&'static str>,
        upload_fail: Vec<String>,
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

        /// Make one host's `label` fail as a dropped SSH TRANSPORT rather than a
        /// remote non-zero exit — the shape that carries no op label and so
        /// classifies as a FUNCTIONAL post-boundary failure (see
        /// [`RecordingExecutor::transport_failing`]).
        pub(crate) fn transport_fail(mut self, host: &str, label: &'static str) -> Self {
            self.entry(host).transport_fail.push(label);
            self
        }

        /// Make one host's UPLOAD fail for any remote path containing `fragment`
        /// (#1621). Uploads carry no label, so this is keyed on the destination —
        /// how a test fails a `WriteFile` op (e.g. the maintenance flag write) the
        /// way a dying scp would.
        pub(crate) fn fail_upload(mut self, host: &str, fragment: impl Into<String>) -> Self {
            self.entry(host).upload_fail.push(fragment.into());
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
                for label in &script.transport_fail {
                    exec = exec.transport_failing(label);
                }
                for fragment in &script.upload_fail {
                    exec = exec.failing_upload(fragment.clone());
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

        /// Global tape positions of every `Run` of `label`, on ANY host — the query
        /// "did this label ever run again, and when relative to that one?" that
        /// compensation assertions need (a compensated host runs `proxy-flip`
        /// twice: once to cut over, once to come back).
        pub(crate) fn positions_of(&self, label: &str) -> Vec<usize> {
            self.tape
                .borrow()
                .iter()
                .enumerate()
                .filter(|(_, (_, call))| call.run_label() == Some(label))
                .map(|(index, _)| index)
                .collect()
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
    fn the_rollout_header_drops_the_skip_clause_for_a_one_host_plan() {
        // #1621 review finding 4. A `--only`-narrowed repair reaches this renderer
        // with a ONE-host plan (`single` is false whenever the config declares more
        // than one host), so nobody is left to "skip it": `skipped.join(", ")` is
        // empty and the sentence used to render as "…fleet-wide;  skip it)" — a
        // dangling clause with a doubled space, on the one line that tells the
        // operator where the fleet's single migration lands.
        let fleet = fleet_of(&["web-b"]);
        let plan =
            plan_fleet(&fleet, &[HostMode::Redeploy]).expect("a one-host plan is well-formed");
        let rendered = fleet_rollout_lines(&plan, "20260714T120000Z", true).join("\n");

        assert!(
            rendered.contains("  \u{2192} migrate (web-b only \u{2014} the schema is fleet-wide)"),
            "a one-host plan must render the migrate line with no skip clause:\n{rendered}"
        );
        assert!(
            !rendered.contains("skip it"),
            "there is no other host in this run to skip the migration:\n{rendered}"
        );
        assert!(
            !rendered.contains(";  "),
            "no dangling clause may survive on the migrate line:\n{rendered}"
        );
        assert!(
            rendered.contains("across 1 host,"),
            "a one-host plan must not say `across 1 hosts`:\n{rendered}"
        );
    }

    #[test]
    fn the_rollout_header_warns_when_first_deploys_cut_over_before_the_migration() {
        // #1621 (AC-4): the migration lands on the first REDEPLOY host in rollout
        // order, so every first-deploy host listed ahead of it goes live on the new
        // release before the schema moves — `first_deploy_ops` carries no migrate op.
        // Declaration order is a documented contract and reordering it silently would
        // be worse, so the hazard is said out loud, naming the hosts it applies to.
        let fleet = fleet_of(&["10.0.0.2", "10.0.0.3", "10.0.0.1"]);
        let plan = plan_fleet(
            &fleet,
            &[HostMode::First, HostMode::First, HostMode::Redeploy],
        )
        .expect("a well-formed fleet plans");

        let warned = fleet_rollout_lines(&plan, "20260714T120000Z", true).join("\n");
        assert!(
            warned.contains(FLEET_FIRST_BEFORE_MIGRATE_NOTE),
            "hosts that cut over before the fleet's only migration must be warned \
             about:\n{warned}"
        );
        assert!(
            warned.contains("10.0.0.2, 10.0.0.3") && !warned.contains("10.0.0.1 go live"),
            "the warning must name exactly the hosts ahead of the migrating one:\n{warned}"
        );
        let quiet = fleet_rollout_lines(&plan, "20260714T120000Z", false).join("\n");
        assert!(
            !quiet.contains(FLEET_FIRST_BEFORE_MIGRATE_NOTE),
            "an app with no writable database has no schema to be behind:\n{quiet}"
        );

        // The mixed fleet whose migrating host runs FIRST is silent: nothing goes
        // live before the schema moves, which is what AC-4 asks for.
        let ordered = plan_fleet(
            &fleet,
            &[HostMode::Redeploy, HostMode::First, HostMode::First],
        )
        .expect("a well-formed fleet plans");
        let silent = fleet_rollout_lines(&ordered, "20260714T120000Z", true).join("\n");
        assert!(
            !silent.contains(FLEET_FIRST_BEFORE_MIGRATE_NOTE),
            "the migration runs before any cutover here — there is nothing to \
             warn about:\n{silent}"
        );
        // …and so is an all-redeploy fleet, whose every host waits for the migration.
        let all_redeploy = plan_fleet(
            &fleet,
            &[HostMode::Redeploy, HostMode::Redeploy, HostMode::Redeploy],
        )
        .expect("a well-formed fleet plans");
        assert!(
            !fleet_rollout_lines(&all_redeploy, "20260714T120000Z", true)
                .join("\n")
                .contains(FLEET_FIRST_BEFORE_MIGRATE_NOTE),
            "no first deploy, no pre-migration cutover"
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
    fn the_completion_line_is_honest_about_a_narrowed_run() {
        // #1621 review finding 2. `--only` narrows the plan, the outcomes AND the
        // state table, so `total` is the SELECTED count: "all 1 hosts serving R2"
        // on a 3-host config reads as a full-fleet success while web-a/web-c are
        // still on the previous release. The closing line must say it was narrowed
        // and that the untouched hosts may be on another release.
        let narrowed = fleet_completion_line("20260714T120000Z", 1, 3, 0);
        assert!(
            !narrowed.contains("Fleet deploy complete"),
            "a narrowed run must not claim the fleet is complete:\n{narrowed}"
        );
        assert!(
            narrowed.contains("`--only` run complete")
                && narrowed.contains("the 1 host it targeted now serve 20260714T120000Z")
                && narrowed.contains("THIS RUN WAS NARROWED")
                && narrowed.contains("the other 2 configured hosts were NOT touched")
                && narrowed.contains("may be on another release")
                && narrowed.contains("full `autumn deploy up` (no `--only`)"),
            "the narrowed line must name the narrowing, the untouched count and the \
             converging command:\n{narrowed}"
        );
        // Degraded hosts on a narrowed run keep BOTH warnings.
        let narrowed_degraded = fleet_completion_line("20260714T120000Z", 2, 5, 1);
        assert!(
            narrowed_degraded.contains("THIS RUN WAS NARROWED")
                && narrowed_degraded.contains("the other 3 configured hosts")
                && narrowed_degraded.contains("post-cutover housekeeping failures"),
            "a narrowed run with a degraded host must say both:\n{narrowed_degraded}"
        );

        // A FULL run is byte-identical to the pre-review text — the e2e harness
        // pins this exact string.
        assert_eq!(
            fleet_completion_line("20260714T120000Z", 2, 2, 0),
            "\n\u{2705} Fleet deploy complete \u{2014} all 2 hosts serving 20260714T120000Z.",
        );
        assert_eq!(
            fleet_completion_line("20260714T120000Z", 3, 3, 1),
            "\n\u{26A0}\u{FE0F}  Fleet deploy complete \u{2014} all 3 hosts serve \
             20260714T120000Z, but 1 finished with post-cutover housekeeping failures (above). \
             Repair them before the next deploy.",
        );
        // `configured_host_count` is never *smaller* than the plan, but a defensive
        // saturating subtraction must not turn an ordinary run into a narrowed one.
        assert!(
            fleet_completion_line("R", 3, 0, 0).contains("Fleet deploy complete"),
            "an under-reported configured count must not fabricate a narrowed run"
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
    fn classify_post_boundary_separates_housekeeping_from_functional_failures() {
        // #1621 (AC-3, T1.15 / §4.6). After the go-live boundary the host IS on the
        // new release, so "it failed" is not enough to decide anything: the three
        // answers below are materially different, and getting them wrong in either
        // direction is an outage — roll a fleet back because `prune` failed and you
        // caused one; auto-roll-back on mid-transaction markers and you may restore
        // the WRONG release.
        for label in ["record-proxy-options", "drain-old", "prune"] {
            assert_eq!(
                classify_post_boundary(label),
                PostBoundaryClass::Housekeeping,
                "`{label}` runs after the flip and does not touch traffic, so the rollout \
                 must continue"
            );
        }
        assert_eq!(
            classify_post_boundary("commit-markers"),
            PostBoundaryClass::Ambiguous,
            "`commit-markers` writes the marker triple as one transaction — a failure \
             inside it leaves the rollback target unprovable, so it must fail closed"
        );
        // Fail closed on anything unknown: a NEW op added after the boundary is
        // treated as mattering until someone deliberately lists it as housekeeping.
        for label in ["proxy-flip", "some-future-post-flip-op", ""] {
            assert_eq!(
                classify_post_boundary(label),
                PostBoundaryClass::Functional,
                "an unrecognised post-boundary label (`{label}`) must fail closed to \
                 Functional, never be waved through as housekeeping"
            );
        }
    }

    #[test]
    fn a_post_boundary_housekeeping_failure_degrades_instead_of_halting() {
        // #1621 (§4.6): the composition the driver actually calls. A pre-boundary
        // outcome passes through untouched (the executor already tore the candidate
        // down); a post-boundary raw error is refined by its label.
        let boxed = || {
            Box::new(exec::DeployExecError::CommandFailed {
                label: "prune",
                message: "scripted".to_owned(),
            })
        };
        assert_eq!(
            classify_host_outcome(&exec::DeployExecError::CommandFailed {
                label: "prune",
                message: "scripted".to_owned(),
            }),
            HostOutcome::Degraded { label: "prune" },
            "a failed `prune` leaves the host live and healthy on the new release"
        );
        assert_eq!(
            classify_host_outcome(&exec::DeployExecError::CommandFailed {
                label: "commit-markers",
                message: "scripted".to_owned(),
            }),
            HostOutcome::AmbiguousMarkers,
            "a failed `commit-markers` must never be auto-rolled-back"
        );
        assert_eq!(
            classify_host_outcome(&exec::DeployExecError::CommandFailed {
                label: "some-future-post-flip-op",
                message: "scripted".to_owned(),
            }),
            HostOutcome::LiveOnNew {
                failed_step: "some-future-post-flip-op"
            },
            "an unknown post-boundary failure halts and compensates"
        );
        assert_eq!(
            classify_host_outcome(&exec::DeployExecError::CandidateRolledBack {
                failed_step: "readiness-gate",
                source: boxed(),
            }),
            HostOutcome::RolledBack {
                failed_step: "readiness-gate"
            },
            "a pre-boundary outcome is already clean and must pass through unrefined"
        );
    }

    #[test]
    fn a_post_boundary_failure_is_classified_by_the_op_that_was_running() {
        // #1621 (§4.6). The never-auto-roll-back guard is about an OP, not about an
        // error shape. `DeployExecError::Spawn` carries no label of its own, so
        // before `execute_with_teardown` attached one a dropped transport during
        // `commit-markers` classified as Functional and the host was auto-rolled-back
        // onto whatever `shared/previous-release` still named — a marker the failed
        // command never rewrote.
        let transport = || {
            Box::new(exec::DeployExecError::Spawn {
                program: "ssh".to_owned(),
                source: std::io::Error::other("scripted"),
            })
        };
        assert_eq!(
            classify_host_outcome(&exec::DeployExecError::PostCutover {
                failed_step: "commit-markers",
                source: transport(),
            }),
            HostOutcome::AmbiguousMarkers,
            "a transport that dropped during `commit-markers` leaves the marker triple \
             exactly as unprovable as a non-zero exit does — it must fail closed too"
        );
        // Attribution only ever makes the verdict MORE conservative. A transport that
        // dropped during housekeeping is not proof that only bookkeeping broke: the
        // deploy host can no longer talk to this host at all, so it stays Functional.
        assert_eq!(
            classify_host_outcome(&exec::DeployExecError::PostCutover {
                failed_step: "drain-old",
                source: transport(),
            }),
            HostOutcome::LiveOnNew {
                failed_step: "ssh-transport"
            },
            "a dropped transport must never be waved through as harmless housekeeping"
        );
        // …and a housekeeping op that really did report its own failure still degrades.
        assert_eq!(
            classify_host_outcome(&exec::DeployExecError::PostCutover {
                failed_step: "prune",
                source: Box::new(exec::DeployExecError::CommandFailed {
                    label: "prune",
                    message: "scripted".to_owned(),
                }),
            }),
            HostOutcome::Degraded { label: "prune" },
            "a remote `prune` failure is still bookkeeping — the rollout continues"
        );
        // Nothing names the failing op: we cannot say WHICH post-cutover step broke,
        // which is exactly when the rollback target is least provable. Fail closed to
        // "do not touch this host" rather than compensating it blind.
        assert_eq!(
            classify_host_outcome(&exec::DeployExecError::Spawn {
                program: "ssh".to_owned(),
                source: std::io::Error::other("scripted"),
            }),
            HostOutcome::AmbiguousMarkers,
            "an unattributable post-boundary failure must decline the automatic rollback"
        );
    }

    #[test]
    fn the_compensation_set_is_the_reverse_of_the_rollout_order() {
        // #1621 (AC-3, T1.13 / §4.7). Everything on the NEW release is compensated,
        // newest cutover first; everything already clean or never reached is left
        // alone. Mode decides HOW: a redeploy host rolls back to its previous
        // release, a first-deploy host has no previous release and is torn down.
        let actions = fleet_rollback_set(
            &[
                HostOutcome::Serving,
                HostOutcome::Serving,
                HostOutcome::LiveOnNew {
                    failed_step: "record-live-slot",
                },
                HostOutcome::Untouched,
            ],
            &[
                HostMode::Redeploy,
                HostMode::First,
                HostMode::Redeploy,
                HostMode::Redeploy,
            ],
        );
        assert_eq!(
            actions,
            vec![
                RollbackAction::Rollback(2),
                RollbackAction::Teardown(1),
                RollbackAction::Rollback(0),
            ],
            "compensation runs strictly newest-first, includes the post-boundary host \
             itself, skips the unreached host, and tears down (not rolls back) a first \
             deploy"
        );
    }

    #[test]
    fn a_degraded_host_is_still_compensated_when_the_fleet_rolls_back() {
        // #1621 (§4.7): a degraded host is live and HEALTHY on the new release —
        // that is exactly why it must come back with everyone else. Leaving it
        // forward while hosts 1..k return to the old release is the mixed-version
        // fleet AC-3 forbids. (Its housekeeping debris is reported separately, from
        // the driver's own record, so compensating it does not erase the warning.)
        let actions = fleet_rollback_set(
            &[
                HostOutcome::Degraded { label: "prune" },
                HostOutcome::LiveOnNew {
                    failed_step: "record-live-slot",
                },
            ],
            &[HostMode::Redeploy, HostMode::Redeploy],
        );
        assert_eq!(
            actions,
            vec![RollbackAction::Rollback(1), RollbackAction::Rollback(0)],
            "a degraded host must not be stranded on the new release"
        );
    }

    #[test]
    fn clean_and_ambiguous_hosts_are_never_auto_rolled_back() {
        // #1621 (§4.7): the two "hands off" cases. A host whose own pre-boundary
        // teardown ran is already back where it started — touching it again would
        // roll it back PAST the release it is serving. A host whose markers are
        // mid-transaction cannot be resolved safely, so it fails closed to Manual.
        let actions = fleet_rollback_set(
            &[
                HostOutcome::RolledBack {
                    failed_step: "readiness-gate",
                },
                HostOutcome::TornDown {
                    failed_step: "readiness-gate",
                },
                HostOutcome::AmbiguousMarkers,
            ],
            &[HostMode::Redeploy, HostMode::First, HostMode::Redeploy],
        );
        assert_eq!(
            actions,
            vec![RollbackAction::Manual(2, MANUAL_AMBIGUOUS_MARKERS)],
            "only the ambiguous host is reported, and never touched"
        );
        assert!(
            fleet_rollback_set(&[], &[]).is_empty(),
            "an empty fleet compensates nothing"
        );
    }

    #[test]
    fn the_summary_reports_degraded_compensated_and_manual_hosts() {
        // #1621 (§8.2): after a compensated halt the state table is the operator's
        // only source of truth, so every distinct state must render as itself —
        // "rolled back by the fleet" is a very different sentence from "rolled back
        // at `readiness-gate`", and a degraded host must not read as a failure.
        let fleet = fleet_of(&["web-a", "web-b", "web-c", "web-d"]);
        let plan = plan_fleet(
            &fleet,
            &[
                HostMode::Redeploy,
                HostMode::Redeploy,
                HostMode::First,
                HostMode::Redeploy,
            ],
        )
        .expect("a well-formed fleet plans");
        let rendered = fleet_summary_lines(
            &plan,
            &[
                HostOutcome::CompensatedRollback,
                HostOutcome::Degraded { label: "prune" },
                HostOutcome::CompensatedTeardown,
                HostOutcome::AmbiguousMarkers,
            ],
            "20260714T120000Z",
        )
        .join("\n");

        assert!(
            rendered.contains("previous release restored"),
            "a fleet-compensated host must say so, not borrow the per-host wording:\n\
             {rendered}"
        );
        assert!(
            rendered.contains("DEGRADED") && rendered.contains("prune"),
            "a degraded host must name the housekeeping step that failed:\n{rendered}"
        );
        assert!(
            rendered.contains("NOTHING serving") && rendered.contains("still holds the route"),
            "a torn-down first deploy must state the proxy-route residue:\n{rendered}"
        );
        assert!(
            rendered.contains("mid-transaction") && rendered.contains("NOT rolled back"),
            "an ambiguous host must say it was deliberately left alone:\n{rendered}"
        );
        assert!(
            rendered.contains(FLEET_SCHEMA_NOT_ROLLED_BACK_NOTE),
            "a compensated fleet whose migration ran must state that the schema did NOT \
             come back with the binaries:\n{rendered}"
        );
    }

    #[test]
    fn the_schema_note_tracks_whether_the_migration_host_was_reached() {
        // #1621 (T1.12): the note is conservative by design — it fires whenever the
        // migrating host was REACHED, because the only evidence available is a step
        // label and a pre-boundary failure after `migrate` still moves the schema.
        let fleet = fleet_of(&["web-a", "web-b"]);
        let migrating = plan_fleet(&fleet, &[HostMode::Redeploy, HostMode::Redeploy])
            .expect("a well-formed fleet plans");
        assert!(
            schema_moved(
                &migrating,
                &[
                    HostOutcome::RolledBack {
                        failed_step: "readiness-gate"
                    },
                    HostOutcome::Untouched
                ]
            ),
            "a pre-boundary failure AFTER `migrate` still moved the schema"
        );
        assert!(
            !schema_moved(
                &migrating,
                &[HostOutcome::Untouched, HostOutcome::Untouched]
            ),
            "an unreached migrating host migrated nothing"
        );
        let first_only = plan_fleet(&fleet, &[HostMode::First, HostMode::First])
            .expect("a well-formed fleet plans");
        assert!(
            !schema_moved(&first_only, &[HostOutcome::Serving, HostOutcome::Serving]),
            "an all-first-deploy fleet schedules no migration at all"
        );
        // A compensated fleet that never migrated must not claim a schema it does
        // not have.
        let rendered = fleet_summary_lines(
            &first_only,
            &[
                HostOutcome::CompensatedTeardown,
                HostOutcome::CompensatedTeardown,
            ],
            "20260714T120000Z",
        )
        .join("\n");
        assert!(
            !rendered.contains(FLEET_SCHEMA_NOT_ROLLED_BACK_NOTE),
            "no migration ran, so there is no schema statement to make:\n{rendered}"
        );
        // …and neither must the SUCCESS path. An all-first-deploy fleet is the shape
        // the header warns runs NO migrations ("run `autumn migrate` yourself before
        // serving traffic"); ending the same run with "the schema has moved" negates
        // that instruction on the last line the operator reads before the green one.
        let served = fleet_summary_lines(
            &first_only,
            &[HostOutcome::Serving, HostOutcome::Serving],
            "20260714T120000Z",
        )
        .join("\n");
        assert!(
            !served.contains(FLEET_SCHEMA_MOVED_NOTE),
            "a rollout that scheduled no migration must never claim the schema moved:\n{served}"
        );
        assert!(
            served.contains(FLEET_RECOVERY_HINT),
            "the recovery lever is still named — hosts ARE on the new release:\n{served}"
        );
        // The note is still printed where it is true: a fleet that did migrate.
        let migrated = fleet_summary_lines(
            &migrating,
            &[HostOutcome::Serving, HostOutcome::Serving],
            "20260714T120000Z",
        )
        .join("\n");
        assert!(
            migrated.contains(FLEET_SCHEMA_MOVED_NOTE),
            "the migrating fleet DID move the schema and must say so:\n{migrated}"
        );
    }

    #[test]
    fn manual_recovery_lines_name_the_markers_and_carry_no_secret() {
        // #1621 (§8.2): "we did not touch it" is only useful next to "here is what
        // to look at". The command is read-only, works today, and quotes nothing but
        // the SSH user, the host and the marker paths the deploy already prints.
        let cfg = &fleet_of(&["web-b"]).hosts[0];
        let rendered = manual_recovery_lines(cfg, MANUAL_AMBIGUOUS_MARKERS).join("\n");

        assert!(
            rendered.contains("web-b") && rendered.contains(MANUAL_AMBIGUOUS_MARKERS),
            "the block must name the host and the reason:\n{rendered}"
        );
        assert!(
            rendered.contains("previous-release") && rendered.contains("live-slot"),
            "the block must name both markers a human has to reconcile:\n{rendered}"
        );
        assert!(
            !rendered.contains("autumn deploy rollback"),
            "`deploy rollback` has no single target under `[deploy] hosts` today, so the \
             manual block must not promise it:\n{rendered}"
        );
        for secret in [
            "AUTUMN_SECURITY__SIGNING_SECRET",
            "postgres://",
            "topsecret",
        ] {
            assert!(
                !rendered.contains(secret),
                "manual recovery output must never carry `{secret}`:\n{rendered}"
            );
        }
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
    #[test]
    fn only_warns_loudly_whenever_it_excludes_a_configured_host() {
        // #1621 (§3.2). `--only` is a repair lever: it deliberately leaves the
        // hosts it skips on their old release, which IS the mixed fleet the rollout
        // driver otherwise refuses to create. That trade is never made silently —
        // the warning names every skipped host and the command that converges the
        // fleet again.
        let configured = ["web-a".to_owned(), "web-b".to_owned(), "web-c".to_owned()];
        let rendered = only_selection_warning_lines(&configured, &["web-b".to_owned()]).join("\n");

        assert!(
            rendered.contains(FLEET_ONLY_MIXED_WARNING),
            "a narrowing selection must print the mixed-fleet warning:\n{rendered}"
        );
        assert!(
            rendered.contains("web-a") && rendered.contains("web-c"),
            "the warning must name the hosts left behind:\n{rendered}"
        );
        assert!(
            rendered.contains("web-b"),
            "the warning must name the hosts it WILL touch:\n{rendered}"
        );
        assert!(
            rendered.contains("autumn deploy up"),
            "the warning must name the command that converges the fleet again:\n{rendered}"
        );
    }

    #[test]
    fn a_selection_that_excludes_nothing_prints_nothing() {
        // AC-1: an ordinary deploy — and every single-host deploy, which can never
        // exclude anything — must be byte-identical to pre-#1621, so the warning
        // block is empty unless a host was actually skipped.
        let configured = ["web-a".to_owned(), "web-b".to_owned()];
        assert!(
            only_selection_warning_lines(&configured, &configured).is_empty(),
            "selecting the whole fleet excludes nothing and must print nothing"
        );
        assert!(
            only_selection_warning_lines(&["web-a".to_owned()], &["web-a".to_owned()]).is_empty(),
            "a single-host deploy must print nothing new"
        );
    }

    #[test]
    fn the_fleet_rollback_table_names_every_host_and_its_state() {
        // #1621 (§3.2): after a partial rollback the fleet is deliberately mixed,
        // and this table is the operator's only record of which half is which — so
        // every host appears, including the ones nothing happened to.
        let hosts = ["web-a".to_owned(), "web-b".to_owned(), "web-c".to_owned()];
        let rendered = fleet_rollback_summary_lines(
            &hosts,
            &[
                RollbackOutcome::RolledBack,
                RollbackOutcome::NoPreviousRelease,
                RollbackOutcome::Failed {
                    failed_step: "readiness-gate",
                },
            ],
        )
        .join("\n");

        for host in &hosts {
            assert!(
                rendered.contains(host.as_str()),
                "every attempted host must appear in the table:\n{rendered}"
            );
        }
        assert!(
            rendered.contains("readiness-gate"),
            "a failed host must name the step it failed at:\n{rendered}"
        );
        assert!(
            rendered.contains("no previous release"),
            "a skipped host must say WHY nothing happened to it:\n{rendered}"
        );
        for secret in [
            "AUTUMN_SECURITY__SIGNING_SECRET",
            "postgres://",
            "topsecret",
        ] {
            assert!(
                !rendered.contains(secret),
                "the rollback table must never carry `{secret}`:\n{rendered}"
            );
        }
    }
    // ── `deploy status`: per-host state and drift (issue #1621, AC-6) ─────────

    /// A synthetic status row, with every "everything is fine" default in place so
    /// each test varies exactly the one fact it is about.
    fn status(host: &str, release: Option<&str>) -> HostStatus {
        HostStatus {
            host: host.to_owned(),
            reachable: true,
            mode: Some(HostMode::Redeploy),
            release: release.map_or(ReleaseId::Unknown, |id| ReleaseId::Known(id.to_owned())),
            live_slot: Some(exec::SLOT_BLUE),
            ready_code: Some(200),
            maintenance: exec::MaintenanceStatus::Off,
            maintenance_flag_source: exec::MaintenanceFlagSource::Shared,
            installed_proxy_port: exec::InstalledProxyPort::Port(3000),
            proxy_options: exec::ProxyOptionsMarker::Options(exec::ProxyServiceOptions {
                tls: false,
                host: None,
            }),
            public_port: 3000,
            live_slot_marker_drift: false,
            last_deploy: Some(exec::LastDeploy {
                result: exec::LAST_DEPLOY_DEPLOYED.to_owned(),
                at: Some("2026-07-14T12:00:03Z".to_owned()),
            }),
        }
    }

    #[test]
    fn fleet_drift_reports_two_different_releases_as_version_drift() {
        // #1621 (T1.20, AC-6): the whole point of `deploy status`. Two hosts on
        // different KNOWN releases is a mixed fleet — the state a rolling deploy
        // exists to avoid — so it must be reported as drift, with the evidence.
        let report = fleet_drift(&[
            status("web-a", Some("20260714T120000Z")),
            status("web-b", Some("20260714T130000Z")),
        ]);
        assert!(report.version_drift, "differing releases are version drift");
        assert_eq!(
            report.releases,
            vec![
                (
                    "web-a".to_owned(),
                    ReleaseId::Known("20260714T120000Z".to_owned())
                ),
                (
                    "web-b".to_owned(),
                    ReleaseId::Known("20260714T130000Z".to_owned())
                ),
            ],
            "the report names each host's release as its evidence"
        );
        assert!(
            report.state_drift.is_empty(),
            "version drift is not state drift: {:?}",
            report.state_drift
        );

        // The converging case: one release everywhere is NOT drift.
        let converged = fleet_drift(&[
            status("web-a", Some("20260714T120000Z")),
            status("web-b", Some("20260714T120000Z")),
            status("web-c", Some("20260714T120000Z")),
        ]);
        assert!(!converged.version_drift);
    }

    #[test]
    fn fleet_drift_never_treats_an_unknown_release_as_drift() {
        // #1621 (T1.20): an unreadable `current` symlink means "we do not know",
        // not "this host differs". A false "your fleet is mixed" at 3 am trains
        // operators to ignore the warning, which is worse than no warning — so
        // Unknown is a DISTINCT reported state that never contributes to drift.
        let all_unknown = fleet_drift(&[status("web-a", None), status("web-b", None)]);
        assert!(
            !all_unknown.version_drift,
            "an all-unknown fleet is not drifted, it is unreported"
        );
        assert_eq!(
            all_unknown.releases,
            vec![
                ("web-a".to_owned(), ReleaseId::Unknown),
                ("web-b".to_owned(), ReleaseId::Unknown),
            ],
            "…and the unknowns are still named"
        );
        assert!(all_unknown.unknown_releases().contains(&"web-a"));

        // One known + one unknown is likewise not drift: there is exactly one known
        // release, and the unknown is reported separately.
        let mixed = fleet_drift(&[
            status("web-a", Some("20260714T120000Z")),
            status("web-b", None),
        ]);
        assert!(!mixed.version_drift, "one known release is not drift");
        assert_eq!(mixed.unknown_releases(), vec!["web-b"]);
    }

    #[test]
    fn fleet_drift_reports_state_drift_separately_from_version_drift() {
        // #1621 (T1.20, plan §7.2): these are two different things and collapsing
        // them hides the dangerous one. Every state-drift reason below makes a
        // FUTURE deploy of that host fail closed or take the wrong slot, on a fleet
        // that is perfectly converged version-wise.
        let mut marker_drift = status("web-a", Some("r1"));
        marker_drift.live_slot_marker_drift = true;
        let mut unreadable_options = status("web-b", Some("r1"));
        unreadable_options.proxy_options = exec::ProxyOptionsMarker::Unreadable;
        let mut wrong_port = status("web-c", Some("r1"));
        wrong_port.installed_proxy_port = exec::InstalledProxyPort::Port(8080);

        let report = fleet_drift(&[marker_drift, unreadable_options, wrong_port]);
        assert!(
            !report.version_drift,
            "every host is on r1 — this is state drift, not version drift"
        );
        assert_eq!(
            report.state_drift,
            vec![
                ("web-a".to_owned(), DRIFT_LIVE_SLOT_MARKER),
                ("web-b".to_owned(), DRIFT_PROXY_OPTIONS_UNREADABLE),
                ("web-c".to_owned(), DRIFT_PROXY_PORT_MISMATCH),
            ],
            "each host is named with its own reason"
        );
        assert!(
            report.drifted(),
            "state drift alone must make --strict fail"
        );

        // A clean fleet reports nothing at all.
        let clean = fleet_drift(&[status("web-a", Some("r1")), status("web-b", Some("r1"))]);
        assert!(!clean.drifted() && clean.state_drift.is_empty());
    }

    #[test]
    fn fleet_drift_flags_a_host_that_is_not_deployed_at_all() {
        // #1621 (AC-6). A host the probe proved has NO `current` symlink is not
        // "unknown", it is empty: `HostMode::First` is a positive fact (the probe
        // shell tests `[ -L current ]`, and even a dangling symlink takes the
        // redeploy branch). Folding it into `ReleaseId::Unknown` made `--strict`
        // exit 0 on a fleet running a fraction of its capacity — the documented cron
        // alert never fired — and blamed an "unreadable symlink" that was never there.
        let mut not_deployed = status("web-b", None);
        not_deployed.mode = Some(HostMode::First);
        let report = fleet_drift(&[status("web-a", Some("r1")), not_deployed.clone()]);
        assert!(
            !report.version_drift,
            "one known release is still not VERSION drift — this is a separate signal"
        );
        assert_eq!(
            report.state_drift,
            vec![("web-b".to_owned(), DRIFT_HOST_NOT_DEPLOYED)],
            "a host carrying no release while its peers serve one is drift"
        );
        assert!(
            report.drifted(),
            "`deploy status --strict` must exit non-zero on it"
        );

        // A fleet where NOTHING is deployed yet is not drifted: there is no peer
        // serving a release for it to be out of step with (the first `deploy up` of a
        // brand-new fleet must not page anyone).
        let mut second = status("web-a", None);
        second.mode = Some(HostMode::First);
        let fresh = fleet_drift(&[second, not_deployed.clone()]);
        assert!(
            !fresh.drifted() && fresh.state_drift.is_empty(),
            "a fleet that has never been deployed is not drifted: {:?}",
            fresh.state_drift
        );

        // The row and the footer must say what is actually true.
        let rows = [status("web-a", Some("r1")), not_deployed];
        let rendered = fleet_status_lines(&rows, &fleet_drift(&rows)).join("\n");
        assert!(
            rendered.contains(DRIFT_HOST_NOT_DEPLOYED),
            "the drift reason must be named on the row:\n{rendered}"
        );
        assert!(
            !rendered.contains("web-b, ") && rendered.contains("no release deployed on web-b"),
            "a host that is not deployed must be described as such:\n{rendered}"
        );
        assert!(
            !rendered.contains("`current` symlink was unreadable"),
            "…and never blamed on a symlink that was never there:\n{rendered}"
        );
    }

    #[test]
    fn a_deployed_host_whose_release_is_unreadable_is_state_drift() {
        // #1621 review round 2. `[ -L current ]` succeeds for a symlink whose target
        // cannot be canonicalized, so such a host reports `Redeploy` while `readlink
        // -f` yields nothing and the release reads back `Unknown`. That combination
        // used to produce ONLY the footer line explicitly labelled "reported, not
        // counted as drift", so `deploy status --strict` exited 0 on a host with
        // actionable marker damage — and the next deploy's `commit-markers` copies
        // `readlink current` verbatim into `previous-release`, recording an
        // unresolvable directory as that host's rollback point.
        let damaged = status("web-b", None); // Redeploy + ReleaseId::Unknown
        let rows = [status("web-a", Some("r1")), damaged];
        let report = fleet_drift(&rows);

        // Rule 1 is untouched: an unknown release still names no version, so it can
        // never make a converged fleet look mixed.
        assert!(
            !report.version_drift,
            "unknown is still never VERSION drift: {:?}",
            report.releases
        );
        assert_eq!(
            report.state_drift,
            vec![("web-b".to_owned(), DRIFT_RELEASE_UNREADABLE)],
            "the damaged host is named with its own reason"
        );
        assert!(
            report.drifted(),
            "`deploy status --strict` must exit non-zero on it"
        );

        // A lone damaged host is still drift — there is no peer to be judged
        // against, because the damage is on the host itself.
        let alone = [status("web-a", None)];
        assert!(fleet_drift(&alone).drifted());

        let rendered = fleet_status_lines(&rows, &report).join("\n");
        assert!(
            rendered.contains(DRIFT_RELEASE_UNREADABLE),
            "the drift reason must be named on the row:\n{rendered}"
        );
        assert!(
            !rendered.contains("reported, not counted as drift"),
            "the footer must not contradict the drift it is now counted as:\n{rendered}"
        );

        // An UNREACHABLE host keeps the old, still-true footer: its markers were
        // never read, so nothing about them is drift.
        let outage = [
            status("web-a", Some("r1")),
            HostStatus::unreachable("web-b"),
        ];
        let outage_report = fleet_drift(&outage);
        assert!(outage_report.state_drift.is_empty());
        assert!(
            fleet_status_lines(&outage, &outage_report)
                .join("\n")
                .contains("reported, not counted as drift"),
            "an unreachable host is still reported, not blamed",
        );
    }

    #[test]
    fn fleet_drift_never_flags_a_host_it_could_not_reach() {
        // An unreachable host has no facts, so inventing drift from its absent
        // markers would report the network outage as a deploy defect. It is listed
        // with an unknown release and nothing else.
        let report = fleet_drift(&[
            status("web-a", Some("r1")),
            HostStatus::unreachable("web-b"),
        ]);
        assert!(!report.version_drift);
        assert!(
            report.state_drift.is_empty(),
            "an unreachable host contributes no state drift: {:?}",
            report.state_drift
        );
        assert_eq!(report.unknown_releases(), vec!["web-b"]);
    }

    #[test]
    fn fleet_status_table_shows_readiness_and_maintenance_as_separate_columns() {
        // #1621 (plan §6.4/§7.3): maintenance mode does NOT drain a host from a load
        // balancer — `/ready` is on the probe-bypass list, so a maintained host stays
        // in rotation serving 503s to real users. Reporting them in one column would
        // teach exactly the wrong mental model, so they are separate and the table
        // says so.
        let mut maintained = status("web-b", Some("r1"));
        maintained.maintenance = exec::MaintenanceStatus::On;
        let rows = [status("web-a", Some("r1")), maintained];
        let report = fleet_drift(&rows);
        let rendered = fleet_status_lines(&rows, &report).join("\n");

        for host in ["web-a", "web-b"] {
            assert!(rendered.contains(host), "every host is a row:\n{rendered}");
        }
        assert!(rendered.contains("r1"), "the release column:\n{rendered}");
        assert!(rendered.contains("200"), "the ready column:\n{rendered}");
        assert!(
            rendered.contains(MAINTENANCE_DOES_NOT_DRAIN_NOTE),
            "the table must state the orthogonality out loud:\n{rendered}"
        );
    }

    #[test]
    fn a_torn_down_host_never_renders_as_a_successful_deploy() {
        // #1621 (AC-6, audit gap G3). A host compensated by `CompensatedTeardown`
        // has nothing installed, and its teardown records
        // `LAST_DEPLOY_TORN_DOWN` over the `deployed` its first deploy wrote. The
        // status column must show that verbatim: `not deployed` in the mode cell
        // and a last-deploy cell that does not claim a deploy.
        let mut torn_down = status("web-a", None);
        torn_down.mode = Some(HostMode::First);
        torn_down.release = ReleaseId::Unknown;
        torn_down.last_deploy = Some(exec::LastDeploy {
            result: exec::LAST_DEPLOY_TORN_DOWN.to_owned(),
            at: Some("2026-07-14T12:00:09Z".to_owned()),
        });
        let rows = [torn_down, status("web-b", Some("r1"))];
        let report = fleet_drift(&rows);
        let rendered = fleet_status_lines(&rows, &report).join("\n");

        assert!(
            rendered.contains("last deploy: torn down 2026-07-14T12:00:09Z"),
            "the torn-down host must report what actually happened:\n{rendered}"
        );
        let web_a_row = rendered
            .lines()
            .find(|line| line.contains("web-a"))
            .expect("web-a has a row");
        assert!(
            !web_a_row.contains("last deploy: deployed"),
            "a host with nothing installed must never read as a successful \
             deploy: {web_a_row}"
        );
        assert!(
            web_a_row.contains("not deployed"),
            "…and the mode cell still says so out loud: {web_a_row}"
        );
    }

    #[test]
    fn fleet_status_table_names_every_drift_reason() {
        let mut drifted = status("web-b", Some("r2"));
        drifted.proxy_options = exec::ProxyOptionsMarker::Unreadable;
        let rows = [status("web-a", Some("r1")), drifted];
        let report = fleet_drift(&rows);
        let rendered = fleet_status_lines(&rows, &report).join("\n");
        assert!(
            rendered.contains(DRIFT_PROXY_OPTIONS_UNREADABLE),
            "a state-drift reason must be spelled out, not abbreviated:\n{rendered}"
        );
        assert!(
            rendered.contains("r1") && rendered.contains("r2"),
            "version drift must show WHICH releases:\n{rendered}"
        );
    }
}
