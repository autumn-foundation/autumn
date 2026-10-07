# SLOs as Code

Declare your service level objectives (SLOs) in `autumn.toml`. Then run one
command to make the monitoring files:

- Prometheus recording rules and multiwindow, multi-burn-rate alerts.
- A Grafana dashboard.
- Argo Rollouts and Flagger analysis templates. A canary fails when it burns
  the error budget too fast.

`autumn deploy up` also reads the SLOs. It can bake each new release. It rolls
the release back when the release burns the budget too fast. See
[Bake and roll back](#bake-and-roll-back).

---

## Declare an SLO

Add one `[[slo]]` table for each objective:

```toml
# All routes: 99.9 % of responses are not 5xx.
[[slo]]
name = "availability"
objective = 99.9
sli = "availability"

# One route: 99 % of requests finish in 250 ms or less.
[[slo]]
name = "orders-latency"
objective = 99.0
sli = "latency"
route = "/api/orders"
threshold_ms = 250
description = "Customers see their orders quickly."
```

| Key | Required | Meaning |
|---|---|---|
| `name` | yes | A unique name: lowercase letters, digits and `-`. It starts with a letter. |
| `objective` | yes | The target percentage of good events. It is more than 0 and less than 100, with four decimals or fewer. |
| `sli` | yes | `"availability"` or `"latency"`. See the next table. |
| `route` | no | The matched route pattern, for example `/api/orders/{id}`. Printable ASCII only, with no space, `"`, `\`, `` ` ``, `{{` or `}}`. When you do not set it, the SLO covers all routes. |
| `threshold_ms` | latency only | The latency limit. It must be a histogram bucket bound: 1, 5, 10, 25, 50, 100, 250, 500, 1000, 2500, 5000 or 10000. |
| `description` | no | Text for the alert annotations and the dashboard. It must not contain `{{` or `}}`, because Prometheus reads annotations as templates. |

| SLI | Good events | All events |
|---|---|---|
| `availability` | Responses that are not 5xx | All responses |
| `latency` | Non-5xx requests that finish in `threshold_ms` or less | Non-5xx requests. With no `route`, requests that match no route are not counted. |

The latency SLI does not count 5xx responses, so a fast error does not make
latency look good. The availability SLI counts them.

The app does not read these tables at run time. Strict config validation
includes these keys. The `prod` profile accepts them. It rejects a key with a
typo.

### Which metrics each SLO reads

| SLO | Series |
|---|---|
| Availability, all routes | `autumn_http_responses_total{status}` |
| Availability, one route | `autumn_http_request_duration_seconds_count{route, status_class}` |
| Latency | `autumn_http_request_duration_seconds_count{route, status_class}` and `autumn_http_request_duration_seconds_bucket{route, status_class, le}` |

Prometheus 3 stores a whole-second bucket bound such as `le="1"` as
`le="1.0"`. The generated queries match both forms.

The route and latency SLOs use the request-duration histogram (issue #3064).
Until your app exports it, those rules return no data, so their alerts never
fire and their canary checks pass. `autumn slo generate` warns about each such
SLO. The all-routes availability SLO works with the counters that every Autumn
app exports today.

---

## Generate the files

```bash
autumn slo generate
```

The command writes six files to `deploy/slo/`:

| File | Use it with |
|---|---|
| `prometheus-rules.yaml` | A Prometheus rule file (`rule_files:`) |
| `prometheus-rule.yaml` | The same rules as a prometheus-operator `PrometheusRule` |
| `grafana-dashboard.json` | Grafana: import the dashboard |
| `argo-analysis-template.yaml` | Argo Rollouts |
| `flagger-metric-templates.yaml` | Flagger |
| `helm-values.yaml` | The Helm chart from `autumn release init --target kubernetes` |

The output is deterministic. Commit it. Then check it in CI:

```bash
autumn slo generate --check
```

`--check` writes nothing. It exits with code 1 when a file is missing or out of
date.

| Flag | Default | Meaning |
|---|---|---|
| `--out-dir DIR` | `deploy/slo` | The output directory. |
| `--app NAME` | `[deploy] app_name`, then the package name, then `app` | The `app` label and the Kubernetes object names. |
| `--selector MATCHERS` | none | Extra label matchers for every query, for example `'job="shop",namespace="prod"'`. Each value is in double quotes. |
| `--prometheus-url URL` | `http://prometheus.monitoring.svc:9090` | The Prometheus address in the analysis templates. |
| `--check` | off | Compare only. |

> **Set `--selector` when one Prometheus scrapes more than one app.** Without
> it, the queries add the series of every app together.

Each recorded series and alert has a `slo_scope` label: a hash of the
`--selector` text. Every query that reads a recorded series matches it. Two
rule sets for one app (for example staging and prod on one Prometheus) then
never read or write each other's series. Each `=` matcher in `--selector` also
becomes a plain label, so you can route alerts on it. The dashboard UID and
title include the selector too.

`--selector` values hold printable ASCII only. The generator rejects the label
names it sets itself, such as `slo`, `severity` and `slo_scope`.

---

## The alerts

For each SLO, the rules record:

- the objective, as `autumn_slo:objective:ratio`;
- the 5m rate of bad events and of all events, as `autumn_slo:sli_bad:rate5m`
  and `autumn_slo:sli_total:rate5m`;
- the error ratio over 5m, 30m, 1h, 6h, 3d and 30d, as
  `autumn_slo:sli_error:ratio_rate<window>`.

Every recorded series has the labels `app`, `slo`, `slo_scope` and the
`--selector` `=` labels. The 3d and 30d ratios add up the recorded 5m rates
with `sum_over_time`, so they load few samples. They need 30 days of
Prometheus retention. With less, the 30-day panels cover less time. They also
lose the events of a gap in rule evaluation longer than 5 minutes. Keep the
rule evaluation interval at 5 minutes or less.

Three alerts follow the Google SRE workbook. All three are named
`AutumnSloErrorBudgetBurn`:

| Burn rate | Long window | Short window | `severity` |
|---|---|---|---|
| 14.4× | 1h | 5m | `page` |
| 6× | 6h | 30m | `page` |
| 1× | 3d | 6h | `ticket` |

A burn rate of 1 spends the error budget in exactly 30 days. For a 99.9 % SLO,
the 14.4× alert fires when more than 1.44 % of responses fail over both windows.
The short window stops the alert soon after the problem stops.

The thresholds are exact decimals. The generator uses integer math, so 14.4 ×
0.001 is `0.0144` and not `0.014400000000000001`.

When the objective is low (for example 90 %), 14.4× the budget is more than
100 %. The fast page can then never fire. The generator writes a warning.

---

## The dashboard

The dashboard has one row for each SLO:

- **SLI (30d)**: the fraction of good events over 30 days.
- **Error budget left (30d)**: 1 minus the 30-day burn rate.
- **Burn rate**: the 1h, 6h and 3d burn rates.

The data source is a dashboard variable. Select your Prometheus after import.

---

## Canary analysis

Each analysis query returns the **burn rate** of the canary pods: the error
ratio divided by the error budget. A canary fails above 14.4, the fast page
rate. The deploy bake uses the same limit.

### Argo Rollouts

`argo-analysis-template.yaml` holds one `AnalysisTemplate` named `<app>-slo`,
with one metric for each SLO. It selects canary pods by the `version` label.
The Helm chart sets `AUTUMN_DEPLOY_VERSION` to the pod-template hash, and
passes the canary hash to the template:

```bash
kubectl apply -f deploy/slo/argo-analysis-template.yaml
helm upgrade --install shop deploy/helm \
  -f deploy/slo/helm-values.yaml --set rollout.enabled=true
```

A metric passes when there is no data yet. It fails after two results above
the limit (`failureLimit: 1`).

### Flagger

`flagger-metric-templates.yaml` holds one `MetricTemplate` for each SLO. It
selects canary pods with the Flagger `{{ target }}` pod-name pattern. It needs
the `namespace` and `pod` labels on your series. The chart's `PodMonitor`
(`metrics.podMonitor.enabled=true`) adds them. Each query covers 5 minutes, so
one late scrape does not empty the result. Scrape at least every 30 s.

```bash
kubectl apply -f deploy/slo/flagger-metric-templates.yaml
helm upgrade --install shop deploy/helm \
  -f deploy/slo/helm-values.yaml --set flagger.enabled=true
```

The chart puts each template in the `Canary` analysis with
`thresholdRange.max: 14.4`. Flagger needs traffic to judge a canary. Set
`flagger.provider`, and `flagger.ingressRef` or `flagger.service` for your
ingress, mesh or gateway. Add a load
test in `flagger.webhooks` when your app has little traffic.

---

## Bake and roll back

After each host cuts over, `autumn deploy up` can wait and sample the new
release. On a bad result, it rolls the release back. This is the bake. It is
off by default.

```toml
[deploy.bake]
duration_secs = 300   # bake each host for 5 minutes
interval_secs = 10    # sample every 10 s
min_requests = 20     # give no verdict on fewer responses
```

Or turn it on for one deploy:

```bash
autumn deploy up --bake-secs 300
```

The limits:

- **5xx ratio.** `max_error_rate` when you set it. Otherwise the 14.4× burn
  rate of the strictest all-routes availability SLO. Otherwise 5 %.
- **Latency.** `max_p99_ms` when you set it. Otherwise one limit for each
  all-routes latency SLO, on the p99, p95 or p50 that matches its objective.
  When two SLOs map to one quantile, the smaller limit wins. Otherwise no
  latency check.

The bake rolls the host back when the 5xx ratio or the latency is above the
limit, when the process restarts, or when `/actuator/metrics` does not answer
two times in a row. Fewer than `min_requests` new responses never cause a
rollback. One 5xx alone never causes a rollback.

See [the deployment guide](deployment.md#bake-roll-back-on-post-cutover-metrics)
for the fleet behavior and the output.

---

## Validate the files yourself

CI checks the golden files with the real tools. You can run the same checks:

```bash
promtool check rules deploy/slo/prometheus-rules.yaml
kubeconform -strict \
  -schema-location default \
  -schema-location 'https://raw.githubusercontent.com/datreeio/CRDs-catalog/main/{{.Group}}/{{.ResourceKind}}_{{.ResourceAPIVersion}}.json' \
  deploy/slo/argo-analysis-template.yaml deploy/slo/flagger-metric-templates.yaml
```

## See also

- [Kubernetes](kubernetes.md): the Helm chart and the Kustomize base.
- [Staged and Zero-Downtime Deploys](staged-deploys.md): canaries and the
  `version` label.
- [Operator Alerts](operator-alerts.md): the in-process alerts that need no
  Prometheus.
