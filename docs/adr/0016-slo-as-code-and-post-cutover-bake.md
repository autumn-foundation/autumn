# ADR 0016: SLOs as Code and a Post-Cutover Bake

- Status: Accepted
- Date: 2026-10-07
- Deciders: Autumn maintainers
- Tags: operations, deploy, observability, kubernetes

## Context

Most outages come from changes. `autumn deploy` rolled back only when the new
release failed `/ready` before the cutover. A release that started and then
failed real requests stayed live. The framework also had no SLO tooling, no
dashboards and no Kubernetes manifests (ADR 0001, phase 3). Issue #3069.

## Decision

1. **SLOs live in `autumn.toml`** as `[[slo]]` tables (`autumn_web::slo`).
   `AutumnConfig` holds them, so strict config validation in the `prod`
   profile accepts them and rejects a typo. The app does not read them at run
   time.
2. **`autumn slo generate` writes the monitoring files**: Prometheus recording
   rules and multiwindow, multi-burn-rate alerts (14.4× over 1h/5m, 6× over
   6h/30m, 1× over 3d/6h), a `PrometheusRule`, a Grafana dashboard, an Argo
   Rollouts `AnalysisTemplate`, Flagger `MetricTemplate`s and Helm values. The
   output is deterministic and has a `--check` mode.
3. **All objective math is integer ppm.** Thresholds in the files are exact
   decimals, so the golden files are stable.
4. **One rollback limit everywhere: a burn rate of 14.4×.** The Argo analysis,
   the Flagger analysis and the deploy bake fail a release that burns its error
   budget at the fast page rate.
5. **The deploy bake samples `/actuator/metrics` over SSH.** Each sample is one
   remote command (`sleep N && curl …`). The CLI reads no clock, so the
   recording executor drives the tests. A breach rolls the host back with the
   existing compensation path. In a fleet it halts the rollout with the step
   `bake`, and the existing fleet compensation rolls back every host on the new
   release.
6. **Thin traffic never rolls back.** Fewer than `min_requests` new responses
   give no verdict. A counter reset (a restart) and an unreadable sample always
   roll back.
7. **`autumn release init --target kubernetes`** writes a Helm chart and a
   Kustomize base. The chart computes `terminationGracePeriodSeconds` from the
   same values that set the drain window, and refuses a buffer under 1 s.
8. **Argo canaries use the pod-template hash as the `version` label.** The hash
   does not change on promotion, so a later analysis cannot mix pods.
9. **CI checks the generated files with the real tools** (`helm lint`,
   `kubeconform`, `kustomize`, `promtool`), pinned and checksum-verified.
10. **Verus proves the bake verdict** (`verification/bake_verdict.rs`): thin
    traffic never rolls back, a restart always does, the error gate is
    monotonic, and a pass is sound. A property test checks that the runtime
    `judge` agrees with the model.

## Alternatives

- **Query Prometheus from the deploy CLI.** Not every VPS deploy has a
  Prometheus. The app's own metrics are always there. Rejected.
- **Bake from a local clock and HTTP client.** The app port is loopback-only on
  the host, and a local clock makes the tests slow or flaky. Rejected.
- **Use `AUTUMN_CANARY=true` as the Argo canary label.** The pods keep the
  label after promotion, so the next analysis mixes old and new pods. Rejected.
- **Put the analysis templates in the Helm chart.** Helm and Argo both use
  `{{ }}`, and the templates depend on the SLOs, not on the chart. Rejected.
- **Bake on by default.** It changes the duration of every existing deploy.
  Rejected; it is opt-in.

## Consequences

- Route and latency SLOs need the request-duration histogram from issue #3064.
  Until the app exports it, those rules return no data. All-routes
  availability works today.
- The bake judges the whole app, so it uses only SLOs with no `route`.
  `/actuator/metrics` has p50, p95 and p99 only, so a latency objective maps to
  the nearest quantile at or below it.
- A failed bake rolls back binaries only. A migration that already ran stays
  applied, as for every automatic rollback.
- The chart has no migration Job and no Ingress.
