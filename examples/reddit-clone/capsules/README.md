# Committed failure capsule

`dev-trigger-error.json` is a **real** failure capsule, produced by Autumn's own
recorder — not a hand-written sketch. It is the artifact the
[`autumn replay` walkthrough](../README.md#failure-capsules-record--autumn-replay)
in this example's README runs against, and
`tests/failure_capsule.rs` parses it through the same `Capsule::from_json` the
replay CLI uses, so a schema change breaks the test rather than the walkthrough.

## Why this one is safe to commit, and yours is not

Everything in [`docs/guide/failure-capsules.md`](../../../docs/guide/failure-capsules.md#security-a-capsule-is-production-data)
about capsules being production data applies to every capsule *you* record.
Database result rows are raw `PostgreSQL` protocol bytes and are **not** masked
— Autumn has no idea which column is a national ID. `tmp/autumn-capsules` is
gitignored for that reason.

This capsule is the deliberate exception because of what it does *not* contain:

- It was recorded from `/dev/trigger-error`, a route that touches no database,
  so `db` is `null` — there is no tape and no rows.
- It is an unauthenticated `GET` with no body, no query string and no cookies,
  so `redacted_keys` is empty: there was nothing to redact.
- The failure is a `parse::<i32>()` on a hardcoded string, so the recorded
  outcome message (`invalid digit found in string`) quotes no user input.

Re-record it after changing the route:

```bash
UPDATE_CAPSULE_FIXTURE=1 cargo test -p reddit-clone --test failure_capsule
```

Read the diff before committing the result.
