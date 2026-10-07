# Kubernetes

`autumn release init --target kubernetes` writes a Helm chart and a Kustomize
base for your app. Both use the Autumn probe and shutdown contract, so a
rolling update drops no requests.

```bash
autumn release init --target kubernetes
```

The command writes the usual `Dockerfile`, `.dockerignore` and
`autumn.production.toml.example`, and also:

| Path | What it is |
|---|---|
| `deploy/helm/` | A Helm chart |
| `deploy/kustomize/base/` | A Kustomize base: Deployment, Service, PodDisruptionBudget |

Build and push the image first. See [the deployment guide](deployment.md).

---

## Install

With Helm:

```bash
helm upgrade --install shop deploy/helm \
  --set image.repository=registry.example.com/shop \
  --set image.tag=1.4.0
```

With Kustomize, make an overlay that uses the base. Set the image and the
Secret in the overlay. Then apply the overlay:

```bash
kubectl apply -k deploy/kustomize/overlays/prod
```

Put `AUTUMN_DATABASE__PRIMARY_URL` and `AUTUMN_SECURITY__SIGNING_SECRET` in a
Secret. Load it with `envFrom` (Helm value `envFrom`). The `prod` profile does
not start without a signing secret.

---

## What the manifests set

| Item | Value | Why |
|---|---|---|
| `startupProbe` | `/startup`, every 2 s, 60 failures | Holds the other probes until startup is complete. |
| `livenessProbe` | `/live` | Restarts only a stuck process. A down dependency does not restart the pod. |
| `readinessProbe` | `/ready`, every 2 s, 1 failure | Removes the pod from the Service at the first 503. |
| `preStop` | `sleep 5` | Gives the endpoints controller time before SIGTERM. |
| `AUTUMN_SERVER__PRESTOP_GRACE_SECS` | `5` | After SIGTERM, `/ready` returns 503 for this time. |
| `AUTUMN_SERVER__SHUTDOWN_TIMEOUT_SECS` | `30` | In-flight requests and shutdown hooks finish in this time. |
| `terminationGracePeriodSeconds` | `50` | 5 + 5 + 30 + a buffer of 10. It is longer than the drain window. |
| `PodDisruptionBudget` | `maxUnavailable: 1` | A node drain removes one pod at a time. |
| Rolling update | `maxUnavailable: 0`, `maxSurge: 1` | A new pod is ready before an old pod stops. |
| Security | non-root UID 10001, no privilege escalation, all capabilities dropped | Matches the image user. |
| `automountServiceAccountToken` | `false` | The app does not call the Kubernetes API. |

Do not point a probe at `/health`. It fails when a dependency is down. See
[Cloud-Native Autumn](cloud-native.md) for the probe contract.

### The grace period stays in step

In the Helm chart, set the four `shutdown.*` values:

```yaml
shutdown:
  preStopSleepSeconds: 5
  prestopGraceSeconds: 5
  shutdownTimeoutSeconds: 30
  bufferSeconds: 10
```

The chart sets the two `AUTUMN_SERVER__*` variables from these values. It sets
`terminationGracePeriodSeconds` to their sum. The chart refuses to render when
`bufferSeconds` is less than 1.

In the Kustomize base, the four numbers are literals. A comment in
`deployment.yaml` gives the sum. Change them together.

---

## Metrics

Each pod has `prometheus.io/*` annotations for `metrics.path`
(`/actuator/prometheus`) on the app port. Change `metrics.path` when you change
the `[actuator] prefix`.

With prometheus-operator, set `metrics.podMonitor.enabled=true` to render a
`PodMonitor`. The chart then removes the annotations, so Prometheus does not
scrape a pod two times. Set `metrics.podMonitor.labels` to the labels your
Prometheus selects on, for example `release: kube-prometheus-stack`. A
`PodMonitor` finds the Flagger primary and canary pods too.

Keep `/actuator/*` off the public internet. Block it at your Ingress.

---

## Canary deploys

The chart supports Argo Rollouts and Flagger. Turn on one of them, not both.
The chart refuses to render with both.

| Value | Effect |
|---|---|
| `rollout.enabled=true` | Renders an Argo Rollouts `Rollout` with canary steps (`rollout.steps`), not a Deployment. |
| `flagger.enabled=true` | Renders a Flagger `Canary` for the Deployment. Flagger makes the Services, so the chart does not. |

The Argo Rollouts canary has no traffic router, so the weight is a pod count.
With 2 replicas, `setWeight: 20` and `setWeight: 50` each give 1 canary pod
(about 33 % of the traffic). Use more replicas for finer steps.

Flagger needs a traffic source to shift weight. Set `flagger.provider` (for
example `nginx`) and `flagger.ingressRef`. With the plain `kubernetes`
provider, Flagger does a blue/green test. A canary with no traffic gives no
metric values, and Flagger fails it. Add a load test in `flagger.webhooks`
when your app has little traffic.

The canary analysis comes from your SLOs. Run `autumn slo generate`, apply its
templates, and pass `deploy/slo/helm-values.yaml` to Helm:

```bash
autumn slo generate --app shop
kubectl apply -f deploy/slo/argo-analysis-template.yaml
helm upgrade --install shop deploy/helm \
  -f deploy/slo/helm-values.yaml --set rollout.enabled=true
```

The values set `analysis.templateName`, `analysis.metricTemplates` and
`analysis.maxBurnRate` (14.4). A canary fails when it burns its error budget
faster than that. See [SLOs as Code](slo.md#canary-analysis).

With Argo Rollouts, the chart sets `AUTUMN_DEPLOY_VERSION` to the
`rollouts-pod-template-hash` label. The `version` metric label then tells the
canary pods from the stable pods. Promotion does not change the label. A later
analysis cannot mix old and new pods.

---

## Validation in CI

The `Kubernetes manifests` job in this repository runs
`scripts/check-k8s-manifests.sh`. It renders the templates and runs:

- `helm lint --strict` with the default values, each canary mode, and the
  `PodMonitor`.
- `helm template | kubeconform -strict` for each of those modes.
- `kustomize build | kubeconform -strict` on the base.
- `kubeconform` on the generated SLO templates, and `promtool` on the rules.

Run the same check on your own chart:

```bash
helm lint --strict deploy/helm
helm template shop deploy/helm | kubeconform -strict -summary
```

Object names are at most 55 characters, so Flagger can add `-primary` or
`-canary`. Use `nameOverride` or `fullnameOverride` to set them.

## Not included

- A database migration Job. Run `autumn migrate` once before you roll out a
  release that needs it. See [the deployment guide](deployment.md).
- An Ingress. Add one for your ingress controller.
