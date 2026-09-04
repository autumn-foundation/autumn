# The Security Posture Gate

`terraform plan`, for your app's authentication surface.

[`autumn routes audit`](./route-auth-coverage.md) proves what your security
surface **is**: every mounted route, classified from macro-expanded
`#[secured]` / `#[authorize]` / `#[public]` code. This page is about the next
question — what a pull request **did** to it, and whether a human agreed.

```
### 🛡️ Security posture diff

**1 widening change — acknowledgment required.** This pull request makes part
of the app reachable by more callers than before.

| Change | Method | Path | Before | After |
|---|---|---|---|---|
| guard removed: gated → public | `GET` | `/admin/users` | `gated (roles: admin)` | `public` |

To acknowledge these exact changes, comment on this pull request with:

    /ack-posture 4f8a1c0d9e2b7a35
```

That comment is the only way past the gate, and it is recorded on the pull
request, by a named human, at the moment the decision was made. "Who approved
making this endpoint public?" now has an answer.

---

## What blocks, and what doesn't

Only **widening** blocks. Everything else annotates, and a pull request that
changes no posture posts nothing at all.

| Change | Verdict |
|---|---|
| New route, `public` or unclassified | **blocks** |
| `gated` / `framework` → `public` | **blocks** |
| Role requirement dropped (`#[secured("admin")]` → `#[secured]`) | **blocks** |
| Role **added** — roles are OR-ed, so another role admits more callers | **blocks** |
| Scope **removed** — scopes are AND-ed, so one fewer lets more tokens in | **blocks** |
| `#[authorize]` binding removed, or a policy check dropped | **blocks** |
| CSRF enforcement lost on a route (disabled, or newly exempt) | **blocks** |
| A prefix added to `security.csrf.exempt_paths` | **blocks** |
| A security header stops being emitted | **blocks** |
| Route removed, **and a weaker route still answers its URLs** | **blocks** |
| Last route at a path removed, **and a public route inherits the URL** | **blocks** |
| New `gated` route that **takes a stricter route's URLs over** | **blocks** |
| New `gated` or framework-owned route | annotates |
| Route removed, gate strengthened, scope added, policy added | annotates |
| A prefix withdrawn from `security.csrf.exempt_paths` | annotates |
| A security header's *value* changed (e.g. a new CSP) | annotates |
| Handler renamed, moved to another file, re-moduled | **nothing at all** |

Two of those rows are worth dwelling on, because they run against intuition:

- **Adding a role widens.** `#[secured("admin", "editor")]` admits *either*
  role. Adding `editor` lets a strictly larger set of people through.
- **Removing a scope widens.** `__check_secured_scopes` requires *all* listed
  scopes. Dropping one lets a strictly larger set of tokens through.

### Deleting a route can widen; adding one can too

The two conditional rows are the gate's least obvious behaviour, and they exist
because **a route is not a URL**. The router matches a static segment before a
dynamic one and mounts both happily, so `/users/me` and `/users/{id}` coexist
with the static one winning.

- **Delete `/users/me`** — gated — while a **public** `/users/{id}` remains, and
  that URL does not go away. It falls through to the public route, and the guard
  is gone. Reported as `route_shadow_exposed`.
- **Add `/users/me`** — restricted to `editor` — beside a `/users/{id}`
  restricted to `admin`, and the new route takes that URL over. Editors reach
  what needed `admin` a moment ago, while the `/users/{id}` entry sits unchanged
  in both manifests. Reported as `route_added_shadowing`.

A path is a node, not a set of routes, so there is a third case. Every method
at one path shares a single handler table, and while any method is mounted the
path answers **405** for the rest. Delete the *last* route at `/users/me` and
that node goes with it: `POST /users/me`, previously a 405, now reaches
whatever less specific route covers it. Reported as `route_path_exposed`, and
blocking only when that route is public — newly reachable but guarded
annotates, exactly as a new guarded route does. While *any* method stays
mounted at the path, though, the node keeps answering (with a 405) and nothing
is inherited at all.

All of them are decided by precedence, not by overlap: a survivor that was
*already* answering the URL gains nothing, so deleting a gated `/users/{id}`
beside a public `/users/me` is an ordinary removal. Where several routes could
take a deleted route's URLs, only the one that actually wins them counts —
`/records/me/{id}` beats `/records/{user}/private`, so the latter's posture is
irrelevant to that deletion. The same analysis covers
`#[authorize]` bindings — a check that disappears from a URL that is still
answered is `authorization_binding_displaced` — and it follows the methods a
route really answers, so a `WS` route mounted as a `GET`, or a `GET` that also
serves `HEAD`, is compared as the router will compare it.

And one row is deliberately weak: a security header's value changing is
reported, never blocked. Whether one Content-Security-Policy is weaker than
another is not decidable from the strings, and a gate that blocks on
"the CSP changed" is a gate a team mutes by the end of the week.

---

## Turning it on

> **The CLI has to be new enough.** The scaffolded workflow installs the
> `autumn` release your app's `autumn-web` version tracks. If `routes
> posture` postdates that release, it falls back — for that run only — to
> the latest published release instead of staying stuck. The fallback isn't
> permanent: it stops firing on its own once a release in your app's own
> compatible series adds the command, and until then the gate still runs
> (rather than staying red until you raise `autumn-web` yourself), just
> under a CLI that may not exactly match your `autumn-web` version. If even
> the latest release lacks the command, the first run fails, naming it. It
> fails rather than skipping on purpose: a gate that waves a pull request
> through because its own tooling is too old is worse than a red one.


**New apps** get it by default: `autumn new` scaffolds
`.github/workflows/posture-gate.yml` alongside `ci.yml`.

**Existing apps** adopt it with the same one command that reconciles every
other framework-owned file:

```bash
autumn upgrade          # preview — shows the workflow it would add
autumn upgrade --apply  # write it
```

Then commit a baseline, which is the manifest the gate diffs against:

```bash
autumn routes audit --manifest security-posture.json
git add security-posture.json
```

Enabling the gate on a repository whose posture has not changed never breaks
it. The first run finds no baseline on the base branch, blocks nothing, and
says so.

Two settings finish the job:

- Make **Security posture diff** a required check in *Settings → Branches*.
  Until it is required, the gate reports; it does not gate.
- Put `.github/workflows/` under CODEOWNERS review. Like every `pull_request`
  workflow on GitHub, this one runs from the pull request's own copy of itself,
  so a change to the gate is only as trusted as the review of that change. The
  workflow refuses to run a pull request that edits it — land workflow changes
  on their own, reviewed by someone who owns that path.

Deleting the committed manifest does not quietly turn the gate off either: a
pull request that removes it fails the check, because a merged deletion would
disarm the gate for every pull request after it. And a build that fails does
not turn it off by omission — the verdict job runs even when the manifest job
does not, and fails closed rather than being skipped (GitHub counts a skipped
required check as satisfied).

### If your app is a Cargo workspace member

`autumn upgrade` will not write this workflow into a workspace *member*, for
the same reason it does not write `ci.yml` there: GitHub only runs workflows
from the repository root, so a member's `.github/workflows/` never executes at
all, and seeding one would look like adoption while gating nothing.

Adopt it by hand at the root instead — three edits to the scaffolded file:

```yaml
env:
  # Point at the member's manifest.
  POSTURE_MANIFEST: apps/my-app/security-posture.json

# …in the `manifest` job:
      - name: Build this commit's posture manifest
        # Select the member package.
        run: autumn routes audit -p my-app --manifest "$POSTURE_MANIFEST"
```

Everything else — the base read, the acknowledgment harvest, the diff, the
verdict — works unchanged, because it only ever reads the manifest path.
Teaching `autumn upgrade` to reconcile workspace-root files on a member's
behalf is tracked separately.

---

## How a run works

The workflow is two jobs, and the split is the point:

- **`manifest`** compiles the pull request and emits its posture manifest.
  Compiling runs the pull request's own build scripts and procedural macros, so
  this job is treated as untrusted: it holds no write permission and reaches no
  verdict.
- **`posture`** never compiles anything. It downloads that manifest, diffs it
  with a CLI it installs itself, resolves acknowledgments, posts the comment,
  and decides.

So the machinery that decides — the diff, the acknowledgment check, the exit
code — cannot be replaced by code the pull request ships. What that does *not*
buy: the manifest itself is derived from building that code, and a build script
can make an app dump whatever route list it likes. A build-derived manifest is
only ever as trustworthy as your review of the build scripts and macros in the
diff; no gate can assert that away.

Step by step:

1. `autumn routes audit --manifest security-posture.json` builds this commit's
   manifest. That is the only build the gate pays for.
2. The workflow fails if the committed `security-posture.json` describes a
   different *posture* than the one just built — a stale committed manifest
   would make the diff lie. The comparison is by posture digest, not by bytes,
   so a moved line number or a renamed handler never turns into a
   regenerate-and-commit chore; a real posture change does.
3. The base branch's copy is read straight out of git
   (`git show origin/<base>:security-posture.json`). No second build.
4. `autumn routes posture diff` compares the two and exits `0`, `1`, or `2`.
5. One comment is posted, or updated in place if it already exists.

So the manifest is your app's posture **state file**: it lives in the
repository, it changes in the same pull request as the code, and it is the
artifact a release signs.

---

## Acknowledging a widening

Comment on the pull request with the line the report prints:

```
/ack-posture 4f8a1c0d9e2b7a35  intentional: public status page for launch week
```

Rules worth knowing:

- **Who.** The workflow asks GitHub for each commenter's **repository
  permission** (`repos/{owner}/{repo}/collaborators/{login}/permission`) and
  keeps only `admin`, `write` or `maintain`. It deliberately does *not* trust
  `author_association`: `MEMBER` means "member of the owning organization",
  which in an org with per-repository permissions includes people with no
  access to this repository at all. `autumn` itself has no GitHub identity and
  enforces no authorization — the workflow step is where that decision lives.
  A team that wants something narrower (excluding the pull request's own
  author, say — nothing stops a maintainer acknowledging their own widening
  today) edits that step. Note that editing it makes the file diverge from the
  scaffold, so `autumn upgrade` will report it as a conflict from then on
  rather than silently overwriting your version.
- **Re-running.** Posting the comment does not re-run the check: `pull_request`
  does not fire on comments. After commenting, re-run the *Security posture*
  check from the Checks tab, or push a commit.
- **What it binds to.** The digest is computed over the *exact set of widening
  findings*, and each one carries the whole posture of the route it names —
  classification, roles, scopes, policy, CSRF, `#[authorize]` bindings. Push ten
  more commits that touch none of those and the acknowledgment stays valid.
  Widen something **new** and the digest changes, no comment carries it, and the
  gate blocks again.

  The route's *whole* posture, because otherwise a second constraint can be
  added and then quietly withdrawn: acknowledge "public, behind a new `mfa`
  scope", drop `mfa` next push, and against the base the diff still shows only
  the original widening. The cost is that a later change which **narrows** that
  route also re-asks — the digest cannot tell a constraint that appeared from
  one that vanished. Re-acknowledging costs a comment on a diff that shows
  exactly what moved; the alternative loses a widening in silence.
  [#2497](https://github.com/autumn-foundation/autumn/issues/2497) tracks the
  format change that would satisfy both.
- **Where it doesn't apply.** Quoted lines (`> /ack-posture …`) and lines
  inside fenced code blocks never acknowledge anything, so quoting a colleague
  — which GitHub's reply button does for you — cannot approve a widening by
  accident.
- **Anything after the digest is the reason.** It is echoed back into the
  comment the gate posts, so the record reads as a decision, not a checksum.

### When the tool is wrong

Acknowledgment **is** the escape hatch, and it is the only one this gate
provides. There is no flag that disables it, hides the diff, or excludes a
route: a false positive is unblocked exactly the way a true positive is — by a
named person saying so on the pull request, in public, with a reason. A wrongly
blocked pull request is therefore always unblockable by the team alone, without
waiting on a framework release.

(What no in-repo gate can defend against is the repository's own settings: an
administrator can always dismiss a required check, and a pull request that
rewrites the workflow could rewrite this one. The workflow refuses to run when
it is itself edited, and CODEOWNERS on `.github/workflows/` is what turns that
refusal into a review.)

If the tool got it wrong, please also
[open an issue](https://github.com/autumn-foundation/autumn/issues) with the
two manifests; the diff rules are meant to be exact, and a false positive is a
bug, not a fact of life.

---

## Verifying at deploy time

The posture manifest a release ships is signed by the same keyless Sigstore
pipeline as everything else autumn ships — see
[Verify What You're Running](./supply-chain.md). The scaffolded deploy
workflows attest `security-posture.json` with
`actions/attest-build-provenance`, binding those exact bytes to the repository,
commit and CI run that produced them.

One command proves both halves at deploy time:

```bash
autumn routes posture verify \
  --manifest security-posture.json \
  --expect-digest 9b1c…  \
  --repo your-org/your-app
```

- **`--expect-digest`** — the posture digest recorded when the change was
  acknowledged (the gate's comment prints it, and
  `autumn routes posture digest --manifest security-posture.json` re-derives
  it). This answers *is this the posture a human approved?* It is computed over
  the manifest's security-relevant content only, so regenerating the manifest
  does not invalidate it while the posture holds.
- **`--repo`** — runs `gh attestation verify` under the hood. This answers *did
  this file come out of our CI, unmodified?*

It exits `0` only if every check that ran passed **and at least one actually
ran**. Tamper with one byte of a route's classification and the digest check
fails; substitute a manifest from somewhere else and the signature check fails;
omit both `--expect-digest` and `--repo` and the run fails rather than
reporting a pass it did not earn. `--expect-digest` takes the full 64-character
digest — the 16-character short form exists for the comment marker a human
types, not for a value a deploy script passes.

`--skip-signature` exists for genuinely offline hosts. It is reported as
**waived**, never as passed, and a run that waives both halves fails: a
verification that verified nothing is not a pass.

Prove the check is real, the same way the supply-chain guide does:

```bash
# Flip a route to public in a copy of the manifest…
sed 's/"gated"/"public"/' security-posture.json > tampered.json
autumn routes posture verify --manifest tampered.json \
  --expect-digest 9b1c… --skip-signature
# ✗ acknowledged-posture   does NOT match the acknowledged digest 9b1c… …
# exit status 1
```

---

## Command reference

| Command | What it does |
|---|---|
| `autumn routes posture diff --base B.json --head H.json` | The gate. Markdown by default; `--format json` / `text`. |
| `… --ack-file acks.txt` | Text harvested from the pull request, scanned for `/ack-posture` markers. |
| `… --ack <digest>` | The same acknowledgment, inline — for local runs. |
| `… --allow-missing-base` | No baseline yet: report, block nothing. |
| `… --output posture-diff.md` | Also write the report to a file. |
| `autumn routes posture digest --manifest M.json` | The posture digest a release records. |
| `autumn routes posture verify --manifest M.json --expect-digest D --repo o/r` | Deploy-time proof. |

Exit codes: **0** clean or acknowledged · **1** blocked (unacknowledged
widening, or failed verification) · **2** usage or I/O error. CI can tell "this
pull request widens the surface" from "the tool could not run".

---

## What it cannot see

The diff is exact about what the manifest proves, and silent about what it
doesn't. It inherits every boundary of
[the manifest's provenance classes](./security-posture-manifest.md):

- **Runtime enforcement.** This is a review-time and deploy-time control. It
  never blocks a request, and it does not change what `#[secured]` or
  `#[authorize]` do.
- **Which policy answers an `#[authorize]` check.** The manifest proves the
  `(action, resource)` binding; which `impl Policy<Resource>` is registered at
  boot is a runtime fact, disclosed as the dimension's `runtime_caveat`.
- **Plugin-contributed routes.** They are not in the manifest yet; that
  composes with the plugin manifest work in
  [#1601](https://github.com/autumn-foundation/autumn/issues/1601).
- **Anything the app doesn't mount through the route registry.** A hand-rolled
  `axum::Router` merged in at the edges is invisible to `routes audit`, and so
  invisible here.
- **A renamed `#[authorize]` resource.** Renaming `Post` to `Article` reads as
  one binding removed and another added, and no manifest can tell that from a
  genuine loss. It blocks, and the report names the pairing so you can
  acknowledge it in one step.
- **Middleware-imposed guards.** A route protected by a `RequireApiToken` layer
  rather than by `#[secured]` is `unclassified` to `routes audit`, so it never
  reaches this gate in the first place — `routes audit` fails on it first.
- **A build that lies.** The manifest comes from compiling the pull request, so
  a build script or procedural macro in the diff can influence what the app
  reports about itself. The gate isolates the *verdict* from that code (see
  "How a run works"), not the *measurement*. Review build scripts as the
  privileged code they are.
- **Which URLs a CSRF exemption prefix actually covers.** `routes audit` asks
  whether a route *template* matches a configured prefix, while the middleware
  asks it of the request path — so exempting `/users/me` leaves `POST
  /users/{id}` recorded as enforced. The gate therefore treats the prefix list
  itself as posture: adding a prefix blocks, whatever the per-route rows say.
  Expect the report to name the prefix rather than the routes it touches.
- **Routes that only exist in your deployed configuration.** `autumn routes
  audit` compiles with Cargo's default profile and default features, while your
  production image is typically `--release` and may enable extra features. A
  route behind `#[cfg(not(debug_assertions))]`, or behind a feature only the
  deployment turns on, is therefore absent from the manifest — and so invisible
  to this gate. If your app has such routes, audit the configuration you ship
  until [#2472](https://github.com/autumn-foundation/autumn/issues/2472) closes
  that gap.

---

## See also

- [Route auth coverage](./route-auth-coverage.md) — the default-deny audit this
  consumes.
- [Security posture manifest](./security-posture-manifest.md) — what each
  dimension proves, and what it merely declares.
- [Verify What You're Running](./supply-chain.md) — the signing and attestation
  pipeline this reuses.
- [Authorization](./authorization.md) — `#[authorize]` and record-level policy.
