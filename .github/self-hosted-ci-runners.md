# Self-hosted CI runners on Hetzner Cloud

Operator runbook for autumn's self-hosted GitHub Actions capacity.

## 1. Overview

GitHub Actions caps a single account at **20 concurrent jobs** across all
repositories. autumn's CI fan-out peaks at roughly **360 pending jobs** (the
`test` OS matrix, the `generator-conformance` toolchain × variant matrix, the
`fuzz` target matrix, `feature-combinations`, `coverage`, `loom`, plus the other
per-PR gates), so a busy day spends most of its wall-clock time queued behind
that cap rather than running.

This change adds an **optional, opt-in self-hosted runner lane** on Hetzner
Cloud that soaks up the heavy, long-running Linux jobs so they no longer compete
for the 20 hosted slots. A single provisioning workflow spins up one Hetzner VM
hosting **N ephemeral runners** (default 6), and a reusable routing workflow
decides — per job, per event — whether a heavy lane targets those runners or
falls back to GitHub-hosted `ubuntu-latest`. Nothing is on by default: the lane
only activates once you both provision the box **and** flip one repo variable.

## 2. Security posture (READ FIRST)

**autumn is a PUBLIC repository.** Self-hosted runners attached to a public repo
are a well-known supply-chain risk: without gating, a pull request from a fork
could run attacker-controlled code (build scripts, test code, `run:` steps) on
your own hardware, with your network position. Every control below exists to
close that hole. Do not weaken one without understanding the others.

- **(a) Base-repo-event routing (the structural fork boundary).** All routing
  decisions flow through `.github/workflows/runner-routing.yml`, a reusable
  workflow whose single job emits the `runs-on` value for heavy lanes. It selects
  the self-hosted lane **only** for events whose workflow *definition* comes from
  the base repo — `push`, `workflow_dispatch`, and `schedule`. **Every
  `pull_request` — fork AND same-repo — stays on `ubuntu-latest`**, so
  fork-controlled workflow code can never select the self-hosted runner. This is
  a structural boundary (which events may reach the lane at all), enforced by
  construction, not by an in-workflow check.

  **Why an in-workflow `head.repo == base` check is insufficient:** for a
  `pull_request`, GitHub executes the workflow files *from the PR head*, which a
  fork controls. A fork could therefore edit `runner-routing.yml` itself (or a
  caller's `runs-on`) to emit the self-hosted labels and delete any
  `head.repo.full_name == github.repository` comparison guarding them — the check
  runs on code the attacker supplied, so it cannot be trusted to keep forks off
  the box. Excluding `pull_request` from the lane entirely closes that hole:
  because `push` / `workflow_dispatch` / `schedule` run the workflow definition
  from the base repo, the routing logic itself is always base-repo-controlled.
- **(b) Ephemeral runners.** Each runner registers with `--ephemeral`, so it
  accepts exactly **one job**, then deregisters and the systemd unit restarts
  the slot to re-register fresh. No workspace, environment, cache, or process
  state is carried from one job to the next, which defeats the "poison the
  runner for the next job" persistence attack. (Even with trusted-only routing,
  this is the belt to that suspenders.)
- **(c) Metadata service firewalled off.** The bootstrap adds `iptables` rules
  rejecting traffic to `169.254.169.254` on both the `OUTPUT` chain (any non-root
  host process, i.e. the `runner` user) and the `FORWARD` chain (Docker /
  testcontainer traffic, which bridges through `FORWARD` and would otherwise
  bypass the `OUTPUT`-owner rule), so a job can never read the Hetzner cloud
  metadata service or any data injected through it. The rules are saved to
  `/etc/iptables/rules.v4` and restored on boot by `netfilter-persistent`, so the
  block survives reboot. **Caveat:** because the `runner` user holds NOPASSWD
  `sudo` (required by ci.yml's `sudo` steps), a trusted job could still `sudo`
  its way to the metadata service as root — an accepted limitation consistent
  with the PAT residual risk below, since only trusted, same-repo code ever runs
  on the box.
- **(d) Repo-scoped registration PAT, root-only.** `RUNNER_REG_PAT` is a
  fine-grained PAT scoped to **this repo only** with **Administration: Read and
  write** — it can mint runner registration tokens for `madmax983/autumn` and
  nothing else, so its blast radius is this repo's runner registration. It is
  delivered to the VM **over SSH after boot** and stored `root:root 0600` at
  `/etc/autumn-runner/pat` — it is never placed in cloud user-data or the
  instance metadata, so it cannot be read back out through the metadata service.
  **Residual risk:** a *trusted* job runs as the `runner` user, which has
  passwordless `sudo` (autumn's own CI steps run `sudo apt-get` / `sudo rm`), so
  a trusted job could `sudo cat` the PAT. This is accepted because (i) only
  trusted, same-repo code ever runs there (control **a**), and (ii) the PAT is
  repo-scoped and easily rotated. **If you ever suspect compromise, revoke and
  re-issue the PAT immediately** (regenerate at
  Settings → Developer settings → Fine-grained tokens) and re-provision the box.
- **(e) Require approval for external contributors.** The fork boundary is
  already closed structurally by control **a** (no `pull_request` ever routes to
  self-hosted), so this is no longer the sole/primary defense — but keep it as
  recommended defense-in-depth for Actions security generally: set
  **Settings → Actions → General → Fork pull request workflows from outside
  collaborators → "Require approval for all external contributors"** (or the
  strictest option your plan offers) so a fork PR does not run any workflow at
  all — hosted or otherwise — until a maintainer approves it.

## 3. Architecture

```
Actions ▸ "Provision self-hosted runner" (workflow_dispatch)
    │  installs hcloud CLI, generates a throwaway SSH key
    ▼
Hetzner Cloud VM  (created via hcloud, cloud-init user-data = bootstrap.sh)
    │  cloud-init installs Docker + CI deps + the Actions runner tarball
    │  into N slot dirs, writes run-ephemeral.sh + a systemd template unit,
    │  firewalls the metadata service off from the runner user
    ▼
provisioner delivers RUNNER_REG_PAT over SSH (root-only /etc/autumn-runner/pat)
    │  then: systemctl enable --now autumn-runner@1 … autumn-runner@N
    ▼
each  autumn-runner@N  systemd unit  ──▶  run-ephemeral.sh N
    │  reads the PAT (root), POSTs it to the GitHub API to mint a fresh,
    │  short-lived registration token, config.sh --ephemeral as the runner user,
    │  then run.sh executes exactly ONE job
    └──▶ job finishes ▶ runner deregisters ▶ systemd Restart=always ▶ repeat
         (labels advertised: self-hosted, hetzner, linux, x64)
```

On each restart `run-ephemeral.sh` clears the slot's local runner config
(`.runner` / `.credentials`) before re-running `config.sh`, so an ephemeral slot
re-registers cleanly and keeps taking jobs beyond the first — otherwise the
leftover local config would make `config.sh` refuse with "already configured"
and the slot would die after a single job.

## 4. Prerequisites — secrets & variables

| Name | Kind | Status | Purpose |
|------|------|--------|---------|
| `HCLOUD_TOKEN` | Actions secret | already present (used by `deploy-real-vps.yml`) | Hetzner Cloud API token (Read & Write) — creates/deletes the VM and SSH keys. |
| `RUNNER_REG_PAT` | Actions secret | **you must create it** | Fine-grained PAT scoped to `madmax983/autumn` with **Administration: Read and write**. Used **only** to mint short-lived runner *registration* tokens on the box — it never appears in user-data. |
| `AUTUMN_SELF_HOSTED_HEAVY` | Actions **variable** | **you must set it to `1`** | The opt-in switch. Heavy trusted lanes route to self-hosted only while this equals `"1"`; unset/anything else ⇒ `ubuntu-latest`. |

### Create `RUNNER_REG_PAT`

1. **Profile → Settings → Developer settings → Personal access tokens →
   Fine-grained tokens → Generate new token.**
2. **Resource owner:** the account/org that owns `madmax983/autumn`.
3. **Repository access → Only select repositories → `madmax983/autumn`.**
4. **Repository permissions → Administration → Read and write.** (Leave
   everything else at *No access*. Administration write is what allows minting
   runner registration tokens; nothing more is needed.)
5. Set a short expiry and a calendar reminder to rotate it. Generate, copy.
6. In the repo: **Settings → Secrets and variables → Actions → Secrets → New
   repository secret**, name `RUNNER_REG_PAT`, paste the token.

#### `RUNNER_REG_PAT` requirements (the common silent-failure cause)

Registration happens **inside the systemd unit on the VM** (`run-ephemeral.sh`
POSTs `…/actions/runners/registration-token`, then `config.sh`), so a
mis-scoped or expired PAT makes registration 403/404 and the slots crash-loop —
without any error in the provision run. The PAT **must** be:

- a **fine-grained** PAT whose **resource owner** is the account/org that owns
  `madmax983/autumn`;
- granted **repository access** to `madmax983/autumn`;
- granted **Repository permissions → Administration: Read and write**;
- **not expired**.

Decisive manual check (prints `201` for a good PAT, `403`/`404` for a
bad-scope/expired one):

```sh
curl -sS -o /dev/null -w '%{http_code}\n' -X POST \
  -H "Authorization: Bearer <PAT>" \
  -H "Accept: application/vnd.github+json" \
  -H "X-GitHub-Api-Version: 2022-11-28" \
  https://api.github.com/repos/madmax983/autumn/actions/runners/registration-token
```

The provisioner now runs this exact preflight before creating the VM (failing
fast if it is not `201`) and, after enabling the units, **verifies at least one
Online runner belonging to THIS provision** before the run succeeds — so a bad
PAT surfaces in the run itself instead of hours later. Verification is
**by runner name**: runners are named with the workflow-controlled prefix
`hetzner-<server_name>-<slot>` (`RUNNER_NAME_PREFIX`, threaded through cloud-init
into `run-ephemeral.sh`), and the verify step polls for an Online runner whose
name starts with `hetzner-<server_name>-`. This is correct on both the fresh
provision path (new Online names) and the **`--replace` reprovision** of the
same-named server (the normal recovery path — `config.sh --replace` reconnects
the same stable runner names/ids, still Online). An earlier id-diff against a
pre-enable baseline false-*failed* that reprovision (the reconnected runner's id
was already in the baseline, so it was wrongly excluded); name-based matching
counts it. A stale runner from a *different* prior server has a different
name-prefix and won't match; a stale same-name runner from a dead VM is Offline
and is filtered out — so nothing can mask a failed new registration.

### Set `AUTUMN_SELF_HOSTED_HEAVY` (do this AFTER provisioning verifies)

- **Settings → Secrets and variables → Actions → Variables → New repository
  variable**, name `AUTUMN_SELF_HOSTED_HEAVY`, value `1`.

## 5. Provision

1. Confirm both secrets exist (§4).
2. **Actions → "Provision self-hosted runner" → Run workflow.** Inputs:
   - `server_type` — `cpx42` (8 vCPU / 16 GB, default) or `cpx32` (4 vCPU / 8 GB).
   - `location` — Hetzner location, default `nbg1`.
   - `ubuntu_image` — base image, default `ubuntu-24.04`.
   - `runner_count` — number of ephemeral slots, default `6`.
   - `server_name` — Hetzner server name and teardown handle, default
     `autumn-ci-runner`.
   - `admin_ssh_key_name` — optional; the name of an **existing** hcloud SSH key
     to also attach so you can SSH in for debugging. Leave blank if you don't
     need admin access (the provisioner always attaches its own throwaway key
     for setup, then deletes it).
3. Watch the run: it installs the `hcloud` CLI, creates the VM with
   `bootstrap.sh` as cloud-init user-data, waits for cloud-init to finish,
   delivers the PAT over SSH, and enables the `autumn-runner@1..N` units. The
   job summary prints the server name, type, location, IP, and slot count.
4. **Verify: Settings → Actions → Runners.** You should see N runners named
   `hetzner-<host>-<slot>` come **Idle**, each carrying the labels
   `self-hosted, hetzner, linux, x64`.
5. **Flip the switch:** set the repo variable `AUTUMN_SELF_HOSTED_HEAVY = 1`
   (§4). The next CI run routes heavy trusted lanes to the box.

## 6. Which lanes route

Routed to self-hosted **only on a base-repo-controlled event AND when opted-in**
(otherwise `ubuntu-latest`) — i.e. only on **push to trunk/trunk-dev,
`workflow_dispatch`, and `schedule`**. **PR-time heavy jobs deliberately stay on
`ubuntu-latest`** (the security trade-off from control **a**: a `pull_request`
runs fork-controlled workflow files, so it is never allowed to select the lane):

| Workflow | Job(s) routed | Notes |
|----------|---------------|-------|
| `ci.yml` | `test` (**ubuntu leg only**), `coverage`, `loom` | The `test` matrix routes *only* its `ubuntu-latest` leg via a conditional `runs-on`; `macos-latest` / `windows-latest` always stay hosted. |
| `fuzz.yml` | `fuzz` (all matrix targets) | Linux-only cargo-fuzz crash-gate burst; not a fidelity benchmark. |
| `generator-conformance.yml` | `compile-and-serve`, `app-variant-conformance`, `scaffold-postgres` | All Linux-only compile/serve gates. |
| `feature-combinations.yml` | `feature-combinations` | Linux-only `cargo hack` each-feature compile sweep. |

**Deliberately left on `ubuntu-latest` (not routed):**

- `ci.yml`: `lint`, `msrv`, `sqlite-runtime` — light/short Linux jobs; the `meta`
  routing job itself always runs hosted.
- `ci.yml` `test` macOS/Windows legs — the self-hosted box is Linux/x64 only.
- **All pull requests (fork AND same-repo)** — never routed by control **a**
  (a `pull_request` runs fork-controlled workflow files), always hosted.
- **Benchmark / timing workflows** — `cold-start-latency.yml`,
  `dev-loop-latency.yml`, `dev-loop-scaling.yml`, `runtime-latency.yml`. These
  measure latency / cold-start / throughput, where result fidelity depends on a
  stable, uncontended, known-hardware runner. Moving them to a shared self-hosted
  box (variable neighbour load, different CPU) would make their numbers
  incomparable, so they stay on GitHub-hosted runners regardless of the switch.
- Other workflows (`release*.yml`, `cli-release.yml`, `deploy-real-vps.yml`,
  `plugin-freshness.yml`, `publish-gate.yml`, `quickstart-gate.yml`,
  `release-image-boot.yml`, `fuzz-nightly.yml`, `claude*.yml`) are out of scope
  for this change.

**Future: PR-time offload.** Keeping PR fan-out on hosted runners is the current
cost of the structural boundary, not a permanent limit. The PR-time heavy jobs
can be safely offloaded later without ever exposing forks, via either a **merge
queue** (`merge_group` trigger — base-repo-controlled, so it may route to
self-hosted just like `push`) or a **maintainer-gated label + `workflow_dispatch`**
that re-runs the heavy lanes on demand once a maintainer has vetted the PR. Both
keep the "workflow definition comes from the base repo" invariant that control
**a** relies on.

## 7. Fallback / flip back in one move

To revert **every** heavy lane to `ubuntu-latest`, **unset or change the repo
variable `AUTUMN_SELF_HOSTED_HEAVY`** (any value other than `1` disables it).
The change takes effect on the **next** workflow run — the `meta` routing job
re-reads the variable each run, so no code change or redeploy is needed. This is
the one-switch kill for a box that is down, being rebuilt, or misbehaving.

If the box is simply offline while the variable is still `1`, jobs targeting the
self-hosted labels **queue harmlessly** (they wait for a matching runner) rather
than failing; flip the variable to drain them onto hosted runners immediately.

## 8. Teardown / recreate

- **Delete the VM:** `hcloud server delete autumn-ci-runner` (use whatever
  `server_name` you provisioned with). This destroys all slots at once.
- **Recreate:** re-run the **"Provision self-hosted runner"** workflow (§5).
- Ephemeral runners **auto-deregister** after each job, so a deleted box leaves
  no live registrations. Any **stale offline** entries left behind (e.g. from a
  hard-killed box) can be removed manually at **Settings → Actions → Runners**.
- Remember to flip `AUTUMN_SELF_HOSTED_HEAVY` off first if you want in-flight and
  future jobs to fall back to hosted runners while the box is gone.

## 9. Sizing & cost

| Type | vCPU | RAM | Disk | Suggested slots |
|------|------|-----|------|-----------------|
| `cpx32` | 4 | 8 GB | 160 GB | 3–4 |
| `cpx42` | 8 | 16 GB | 240 GB | 6 (recommended) |

Rust + Docker builds are **CPU- and disk-heavy**: a cold-cache workspace build
plus the wide-feature Postgres-testcontainer test step sits near a runner's disk
ceiling (ci.yml actively frees disk on hosted runners for exactly this reason),
and coverage instrumentation roughly doubles `target/` size. `cpx42` with 6
slots gives each concurrent job ~1.3 vCPU / ~2.7 GB / ~40 GB headroom, which
comfortably runs autumn's heaviest lanes in parallel. `cpx32` works for a
lighter cadence but expect fewer safe concurrent slots.

**Approximate monthly cost:** on the order of a low-double-digit EUR/month for a
`cpx32` and roughly double that for a `cpx42`, plus a small IPv4 surcharge and
egress. **These are ballpark figures — verify current Hetzner Cloud pricing at
<https://www.hetzner.com/cloud> before budgeting; do not treat them as exact.**
Billing is hourly, so deleting the box when idle (§8) genuinely stops the meter.

**Resize:** Hetzner can resize in place, but the clean, reproducible path here is
to **delete and re-provision** with a different `server_type` input (§5/§8) — the
box carries no durable state, so nothing is lost.

## 10. Maintenance

- **Reclaim disk between jobs.** Ephemeral runners reset the *workspace*, but the
  shared Docker image/layer cache and `target/` scratch grow over time. Add a
  periodic `docker system prune -af` (a small systemd timer or a root cron entry,
  e.g. nightly) so accumulated images/volumes don't eventually exhaust the disk.
- **Runners self-update.** The GitHub Actions runner auto-updates itself to
  stay within GitHub's supported-version window, so you do not need to
  re-provision just to pick up new runner releases. Re-provision only for OS
  patching cadence, a resize, or a suspected compromise.
- **Rotate `RUNNER_REG_PAT`** on its expiry (and immediately on any suspicion of
  compromise — see §2d): re-issue the fine-grained token, update the repo
  secret, and re-run the provision workflow so the box receives the new PAT.

## 11. Future: org-level runners

After the planned move to an `autumn-foundation` organization, this same
machinery can register at **org scope** instead of repo scope: point the
registration token endpoint at the org's runner API and set the PAT's
Administration permission at the org, so one Hetzner box (or a small pool)
provides shared self-hosted capacity across every autumn repo instead of just
this one. No implementation now — it is a one-line endpoint change plus an
org-scoped PAT when the org exists.
