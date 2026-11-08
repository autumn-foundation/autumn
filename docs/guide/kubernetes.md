# Kubernetes

`autumn release init --target kubernetes` writes a Helm chart and a Kustomize
base for your app. Both use the Autumn probe and shutdown contract, so a
rolling update drops no requests. Each [role](jobs.md) gets its own workload.

```bash
autumn release init --target kubernetes
```

The command writes the usual `Dockerfile`, `.dockerignore` and
`autumn.production.toml.example`, and also:

| Path | What it is |
|---|---|
| `deploy/helm/` | A Helm chart |
| `deploy/kustomize/base/` | A Kustomize base. It lists one component per role. |
| `deploy/kustomize/components/<role>/` | Deployment and PodDisruptionBudget. A role that serves HTTP also gets a Service and an Ingress. |

Build and push the image first. See [the deployment guide](deployment.md).

---

## Install

With Helm:

```bash
helm upgrade --install shop deploy/helm \
  --set image.repository=registry.example.com/shop \
  --set image.tag=1.4.0 \
  --set 'trustedHosts[0]=shop.example.com'
```

`trustedHosts` is required. The prod profile does not start without
`[security.trusted_hosts]`, so the chart refuses to render without it. The
chart also trusts the pod IP: Prometheus scrapes a pod by its IP, and sends
the IP as the `Host` header.

With Kustomize, make an overlay that uses the base. Set the image, the Secret
and your host names in the overlay. The base sets
`AUTUMN_SECURITY__TRUSTED_HOSTS__HOSTS` to a placeholder host and the pod IP.
Then apply the overlay:

```bash
kubectl apply -k deploy/kustomize/overlays/prod
```

Put `AUTUMN_DATABASE__PRIMARY_URL` and `AUTUMN_SECURITY__SIGNING_SECRET` in a
Secret. Load it with `envFrom` (Helm value `envFrom`). The `prod` profile does
not start without a signing secret.

---

## One workload per role

The generator reads `[roles.*]` in `autumn.toml`. It writes one workload for
each role:

| Input | Roles |
|---|---|
| `--roles web,worker,critical` | These roles. Each is a built-in role or a `[roles.<name>]` preset. |
| `--split-workers`, or one or more presets | `web`, `worker`, then each preset in name order. |
| Nothing | `combined`. |

```bash
autumn release init --target kubernetes --roles web,worker,critical
```

Each workload sets `AUTUMN_ROLE` to its role. A preset reads its `pin`,
`workers` and `mount` from `autumn.toml`, so the manifests do not repeat
them.

Only a role that serves HTTP (base `combined` or `web`) gets a Service, an
Ingress, an Argo `Rollout` or a Flagger `Canary`. A worker serves only the
probes and the actuator on the app port. It keeps its `/startup`, `/live` and
`/ready` probes.

Object names are `<name>-<role>`, at most 55 characters. `<name>` is the
Helm full name, or the app name in Kustomize. In a name, `_` in a role becomes
`-`: `bulk_jobs` gives `shop-bulk-jobs` and `components/bulk-jobs/`. The pods
of a role have the label `app.kubernetes.io/component: <role>`. In Helm they
also have `app: <name>-<role>`, because Flagger selects on `app`. Each
Service, PDB and HPA selects one role.

A role name must end in a letter or a digit. Names that YAML reads as a
boolean or null (`on`, `off`, `yes`, `no`, `y`, `n`, `true`, `false`, `null`,
`nil`) are refused.

In the Helm chart, set each role in `values.yaml`:

```yaml
roles:
  web:
    enabled: true
    servesHttp: true
    replicaCount: 2
    pin: []              # AUTUMN_JOBS__PIN. A preset pin wins.
    env: []              # added to the shared env
    resources: {}        # replaces the shared resources
    autoscaling:
      enabled: false
      minReplicas: 2
      maxReplicas: 10
      targetCPUUtilizationPercentage: 80
    ingress:
      enabled: false
      className: ""
      annotations: {}
      hosts: []          # - host: shop.example.com
      tls: []
  worker:
    enabled: true
    servesHttp: false
    replicaCount: 1
    # ...
```

`servesHttp` comes from the base of the role. Do not change it by hand. The
hosts of a role Ingress are trusted hosts too. Set `roles.<name>.enabled=false`
to leave out a role. The chart refuses to render with no role on. The
top-level `replicaCount` moved to `roles.<name>.replicaCount`. The chart
refuses to render when you set the old key.

With Kustomize, remove a line from `components:` in
`base/kustomization.yaml` to leave out a role. Each role has a placeholder
host in its trusted hosts. An HTTP role also has it in `ingress.yaml`. Set
your hosts in the overlay.

`autumn doctor` warns when a manifest names a role that `autumn.toml` does
not declare. See [`manifest_roles`](#doctor).

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
| `PodDisruptionBudget` | `maxUnavailable: 1`, one per role | A node drain removes one pod of a role at a time. |
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

In the Kustomize components, the four numbers are literals. A comment in
each `deployment.yaml` gives the sum. Change them together.

---

## Metrics

Each pod has `prometheus.io/*` annotations for `metrics.path`
(`/actuator/prometheus`) on the app port. Change `metrics.path` when you change
the `[actuator] prefix`.

With prometheus-operator, set `metrics.podMonitor.enabled=true` to render a
`PodMonitor`. The chart then removes the annotations, so Prometheus does not
scrape a pod two times. Set `metrics.podMonitor.labels` to the labels your
Prometheus selects on, for example `release: kube-prometheus-stack`. A
`PodMonitor` finds the Flagger primary and canary pods too. It sets the `job`
label to the release name, so use `--selector 'job="<release>"'` with
`autumn slo generate`. It copies the role label of the pod to each series.

Keep `/actuator/*` off the public internet. Block it at your Ingress.

---

## Canary deploys

The chart supports Argo Rollouts and Flagger. Turn on one of them, not both.
The chart refuses to render with both.

| Value | Effect |
|---|---|
| `rollout.enabled=true` | Renders an Argo Rollouts `Rollout` with canary steps (`rollout.steps`), not a Deployment, for each role that serves HTTP. A worker stays a Deployment. |
| `flagger.enabled=true` | Renders a Flagger `Canary` for each role that serves HTTP. Flagger makes the Services, so the chart does not. A role with its own Ingress uses it as the `ingressRef`. `flagger.ingressRef` can serve one role only: with two HTTP roles, set `roles.<name>.ingress` for each. |

The Argo Rollouts canary has no traffic router, so the weight is a pod count.
With 2 replicas, `setWeight: 20` and `setWeight: 50` each give 1 canary pod
(about 33 % of the traffic). Use more replicas for finer steps.

Flagger needs a traffic source to shift weight. Set `flagger.provider`. For
an ingress provider (for example `nginx`), also set `flagger.ingressRef`. For a
mesh or the Gateway API, put the provider fields (for example `gatewayRefs`)
in `flagger.service`. With the plain `kubernetes` provider, Flagger does a
blue/green test. A canary with no traffic gives no
metric values, and Flagger fails it. Add a load test in `flagger.webhooks`
when your app has little traffic.

The canary analysis comes from your SLOs. Run `autumn slo generate`, apply its
templates, and pass `deploy/slo/helm-values.yaml` to Helm:

```bash
autumn slo generate --app shop
kubectl apply -f deploy/slo/argo-analysis-template.yaml
helm upgrade --install shop deploy/helm \
  -f deploy/slo/helm-values.yaml --set rollout.enabled=true \
  --set 'trustedHosts[0]=shop.example.com'
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
`scripts/check-k8s-manifests.sh`. It checks two golden copies of the
generated output in `autumn-cli/tests/golden/release/`: an app with the
`web`, `worker` and `marketing` roles, and an app with only `combined`. A Rust
test keeps the copies equal to the generator output. The script runs:

- `helm lint --strict` with the default values, each canary mode, the
  `PodMonitor`, the Ingress and autoscaling.
- `helm template | kubeconform -strict` for each of those modes.
- Checks that each role has one workload, and that only HTTP roles have a
  Service.
- `kustomize build | kubeconform -strict` on each base.
- `kubeconform` on the generated SLO templates, and `promtool` on the rules.

Run the same check on your own chart:

```bash
helm lint --strict deploy/helm --set 'trustedHosts[0]=shop.example.com'
helm template shop deploy/helm --set 'trustedHosts[0]=shop.example.com' \
  | kubeconform -strict -summary
```

Object names are at most 55 characters, so Flagger can add `-primary` or
`-canary`. Use `nameOverride` or `fullnameOverride` to set them.

## Doctor

`autumn doctor` has a `manifest_roles` row. It reads the Helm `roles:` keys,
`AUTUMN_ROLE` in `deploy/kustomize/`, `fly.toml` and `docker-compose.yml`,
and `[[deploy.groups]] role`. It warns when one of them names a role that is
not a built-in role or a `[roles.<name>]` preset. A pod with such a role does
not boot. It also warns when a Helm `servesHttp` does not match the base of
the role. `autumn doctor --strict` fails on a warning.

## Not included

- A database migration Job. Run `autumn migrate` once before you roll out a
  release that needs it. See [the deployment guide](deployment.md).
