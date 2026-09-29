### Documentation

- **cloud-native:** the guide now names the probe endpoints under the words
  readers search for, and says what pointing the wrong probe at `/health`
  costs. `docs/guide/cloud-native.md` already mapped `/live`, `/ready`,
  `/startup` and `/health` to their probes, correctly — but under a heading
  spelled with the single word "Probes", so "liveness probe" and "readiness
  probe" (the `livenessProbe` and `readinessProbe` keys a reader is filling in
  when they ask) matched no slug, H1 or heading in the 165-page guide, and
  "health check" landed only on `fleet-deploys.md`'s rollout-gating section.
  The section is retitled "Probes: liveness, readiness, and startup" — the
  page and every inbound link are unchanged — and now states what each
  endpoint reflects, that the four paths are `[health]` defaults
  (`live_path`, `ready_path`, `startup_path`, `path`) and that
  `[health] enabled = false` suppresses all four. The standing warning not to
  point all three at `/health` now gives its reason: `/health` is a readiness
  answer, so it returns `503` for conditions a restart does not fix — a
  saturated connection pool, a replica that cannot safely serve reads, a
  readiness indicator reporting down, a drain in progress — and a *liveness*
  probe reading one of those has the orchestrator kill a process that was
  working. The section also records what readiness does **not** check: the
  built-in `db` indicator reports pool availability, not primary connectivity,
  so an unreachable primary can leave `/ready` at `200` while idle connections
  remain.
  Six reader spellings that returned nothing now land on the section, gated by
  `scripts/check-docs-retrieval.sh` and `scripts/check-docs-aliases.sh`.
