# Chapter 10: Configuration and Production Defaults

**Goal:** By the end of this chapter, you will understand Autumn's profile-aware
configuration system, know how to override settings with environment variables,
and have a clear mental model for probes, logging, telemetry, sessions, and
deployment defaults in development and production.

---

## The Five Layers

Autumn resolves configuration in this order:

1. Framework defaults
2. Smart defaults for `dev` or `prod`
3. `autumn.toml`
4. `autumn-{profile}.toml`
5. `AUTUMN_*` environment variables

That ordering keeps local defaults readable while still letting deployment
systems override the real runtime knobs at the edge.

## Local-Safe vs Production-Safe

Autumn still has a deliberate split between safe local defaults and safe
distributed defaults.

Local-safe defaults:

- in-memory sessions
- pretty logs in `dev`
- process-local `#[scheduled]` tasks
- one-process startup with no explicit migration job

Production-safe expectations:

- `AUTUMN_PROFILE=prod`
- probe wiring for `/live`, `/ready`, and `/startup`
- JSON logs or OTLP export
- Redis-backed sessions for multi-replica deployments
- explicit migration job before web replicas start

The framework warns if the `prod` profile still uses in-memory sessions without
`session.allow_memory_in_production = true`.

The `prod` profile also turns on overload and database protections: load
shedding, a `30s` statement timeout, a `60s` idle-in-transaction timeout, and
`strict_config`. See
[Production Protections](../cloud-native.md#production-protections) for the
values and the opt-outs.

## `autumn.toml` Sections That Matter First

The new scaffold keeps `autumn.toml` short, but the first sections worth
understanding are:

- `[server]` for `host`, `port`, and `shutdown_timeout_secs`
- `[log]` for `level` and `format`
- `[health]` for `/health`, `/live`, `/ready`, and `/startup`
- `[telemetry]` for OTLP export and service metadata
- `[session]` and `[session.redis]` when you move beyond a single process
- `[database]` for primary/write URL, optional replica/read URL, pool sizes,
  replica fallback behavior, and connect timeout
- `[actuator]` for prefix and sensitive endpoint exposure

## Environment Variable Overrides

Environment variables always win. A few high-signal examples:

```bash
AUTUMN_PROFILE=prod
AUTUMN_SERVER__PORT=8080
AUTUMN_LOG__FORMAT=Json
AUTUMN_TELEMETRY__ENABLED=true
AUTUMN_TELEMETRY__OTLP_ENDPOINT=http://otel-collector:4317
AUTUMN_SESSION__BACKEND=redis
AUTUMN_SESSION__REDIS__URL=redis://redis:6379
```

The naming rule is `AUTUMN_SECTION__FIELD`, with nested sections using another
double underscore.

## Probes and Actuator

Autumn auto-mounts four probe endpoints:

- `/live`
- `/ready`
- `/startup`
- `/health`

Recommended use:

- liveness probe -> `/live`
- readiness probe -> `/ready`
- startup probe -> `/startup`

Actuator endpoints live under `/actuator` by default and expose health, info,
metrics, config properties, loggers, and task visibility based on the
`actuator.sensitive` setting. The Prometheus scrape endpoint
(`/actuator/prometheus`) is governed separately by `actuator.prometheus`
(default `true`), so platform scraping works with `sensitive = false` — see the
[deployment guide](../deployment.md#prometheus-metrics-for-platform-scraping).

## Telemetry

For production, treat telemetry as a first-class config section rather than a
later chore:

```toml
[telemetry]
enabled = true
service_name = "my-app"
service_namespace = "apps"
environment = "production"
otlp_endpoint = "http://otel-collector:4317"
protocol = "Grpc"
```

Use `Json` logs or OTLP tracing in production. `Pretty` logs are for humans,
not aggregators.

## `#[scheduled]` vs Harvest

Use `#[scheduled]` when the task is light, in-process, and you can tolerate one
copy per replica or your application code can coordinate ownership explicitly.

Use Harvest when the work needs durability, retries, workflow history, or clear
singleton semantics across multiple replicas. The framework-level scheduler is
not a distributed job system. Harvest is a companion project with its own
release train because it integrates with Autumn Web rather than being part of
the core web crate.

## Deployment Checkpoint

At this point your app should have:

1. profile-aware config with env overrides
2. probe endpoints wired for your platform
3. telemetry config ready for an OTLP collector
4. a decision on whether sessions stay local or move to Redis
5. a clear plan for migrations and background work before you deploy replicas

---

Previous: [Chapter 9 - Error Handling](09-errors.md) | Next: [Chapter 11 - Writing Integration Tests](11-testing.md)
