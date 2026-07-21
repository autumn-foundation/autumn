# Runbook: Real-VPS deploy test lane

The `Deploy (Real VPS)` workflow (`.github/workflows/deploy-real-vps.yml`) runs
nightly (and on demand) and provisions a throwaway Hetzner Cloud server to
exercise a real end-to-end deploy, then destroys it in an `always()` teardown.

## Account server limit (shared with nexus CI)

**Symptom:** the workflow fails during provisioning with
`hcloud: server limit reached (resource_limit_exceeded)` right after the SSH key
is created and a server type/location is selected.

**Cause — not a leak.** The Hetzner Cloud account has a default limit of **5
servers per account**, and that account is **shared with the nexus CI runner
fleet**. When all 5 slots are occupied by (legitimate, long-lived) nexus runner
servers, there is no free slot for the Real-VPS run's ephemeral server, so
`hcloud server create` is rejected.

The Real-VPS lane's own resources are ephemeral and cleaned up every run:
- Server name: `autumn-rvps-<run_id>-<attempt>`, labelled `managed-by=autumn-rvps`.
- SSH key name: `autumn-rvps-key-<run_id>-<attempt>`.
- Both are deleted by the workflow's `if: always()` teardown step, so a normal
  run (success or failure) strands neither.

The capacity preflight step lists the current servers before provisioning, so a
full account is visible up front; the server-create step now maps the raw
`resource_limit_exceeded` error to this runbook instead of dying cryptically.

## Operational fix

Pick one:

1. **Request a server-limit increase** (preferred if both Real-VPS and nexus CI
   need to run): Hetzner Cloud Console -> the project -> **Limits** -> request an
   increase above 5. Once granted, re-run the workflow.
2. **Free a slot**: if a nexus CI runner server is stale/unneeded, remove it (via
   the nexus provisioning tooling) to free a slot, then re-run.

## Note on a hard-cancelled run

The teardown runs as an `if: always()` step, which covers step failures. If a run
is hard-cancelled or the runner dies before that step executes, its
`autumn-rvps-<run_id>-<attempt>` server/key could be stranded. Because every
Real-VPS server is labelled `managed-by=autumn-rvps`, a stranded one is
identifiable with `hcloud server list -l managed-by=autumn-rvps` and can be
deleted manually. (A dedicated automated sweep was intentionally left out: at the
default 5-server shared limit there is no backlog to clean, and the labelled
identity above makes a rare manual cleanup trivial.)
