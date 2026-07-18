# Application Starters

`autumn new <name>` scaffolds a minimal base project. A **starter** instead
scaffolds a complete, runnable application archetype — so you reach a working,
domain-shaped app in minutes instead of hand-assembling primitives. Starters
come in two flavours:

- **Built-in starters** — curated, vetted, and version-locked to the CLI. They
  apply instantly with no network fetch.
- **Community starters** — any git repository (or local directory) that follows
  the starter manifest format. Their provenance is shown and confirmed before
  anything is fetched or applied.

Both flavours share one format, so a starter you publish behaves exactly like a
built-in one.

---

## Using a built-in starter

List what ships with the CLI:

```bash
autumn new --list-starters
```

```text
Available built-in starters:

  saas  Multi-tenant SaaS: session auth + row-level tenancy + tenant-scoped dashboard

Scaffold one with:   autumn new <name> --starter <starter>
Community starters:  autumn new <name> --starter <git-url|owner/repo>[@ref] [--yes]
```

Scaffold the flagship multi-tenant SaaS app and run it end to end:

```bash
autumn new acme --starter saas
cd acme
docker compose up -d        # start Postgres (or point [database].url at your own)
autumn migrate
autumn dev
```

Sign up an organisation and you land on a tenant-scoped dashboard that serves
`200 OK` and only ever shows your organisation's data. The `saas` starter
composes only already-shipped primitives — session auth, row-level
multi-tenancy, repositories, and sessions — and is itself the committed
[`examples/saas`](../../examples/saas) app, covered by the same CI drift gate as
every other example, so it cannot rot silently.

Bare `autumn new <name>` (no `--starter`) keeps today's minimal-base behaviour
unchanged.

---

## Using a community (git) starter

Point `--starter` at a git repository. Both a full URL and an `owner/repo`
GitHub shorthand are accepted, and you can pin a tag, branch, or revision:

```bash
# owner/repo shorthand → https://github.com/owner/repo.git
autumn new acme --starter your-org/autumn-starter-cms

# pin a ref with an @suffix …
autumn new acme --starter your-org/autumn-starter-cms@v1.2.0

# … or with --starter-ref (mutually exclusive with @suffix)
autumn new acme --starter https://github.com/your-org/cms.git --starter-ref main
```

A local directory works too, which is how you test a starter before publishing:

```bash
autumn new acme --starter ./path/to/my-starter
```

### Provenance and confirmation

Before fetching or applying anything that did not ship with the CLI, Autumn
prints the resolved source and asks for confirmation:

```text
Community starter (git): https://github.com/your-org/cms.git (ref: v1.2.0)
Proceed? [y/N]
```

In non-interactive use (CI, scripts) there is no TTY to prompt, so you must pass
`--yes` to proceed:

```bash
autumn new acme --starter your-org/cms@v1.2.0 --yes
```

You own trust in the source you name: Autumn surfaces the provenance and
requires confirmation, but it does not sandbox or audit the starter's code.

---

## The starter manifest

Every starter — built-in or community — has an `autumn-starter.toml` at its root:

```toml
[starter]
# Machine name. For built-ins this is the `--starter` value; for community
# starters it is informational.
name = "saas"

# One-line description shown by `autumn new --list-starters`.
description = "Multi-tenant SaaS: session auth + row-level tenancy + tenant-scoped dashboard"

# Optional: files (relative to the starter root, forward slashes) that must be
# copied byte-for-byte WITHOUT template substitution — binary assets, vendored
# JS, pre-rendered fixtures. Omit or leave empty if there are none.
verbatim = ["static/img/logo.png"]

# Optional: notes printed after a successful scaffold. The same `{{…}}` tokens
# as the template files are substituted before display.
post_scaffold_notes = """
Next steps:
  cd {{project_name}}
  autumn migrate && autumn dev
"""
```

Every other file in the starter tree is a **template**. When a project is
scaffolded, each template file is rendered with the same substitution path
`autumn new` uses for its base project:

| Token | Replaced with |
|-------|---------------|
| `{{project_name}}` | the name passed to `autumn new` (e.g. `acme`) |
| `{{crate_name}}` | `project_name` with `-` replaced by `_` (e.g. `acme`) |
| `{{autumn_version}}` | the `autumn-web` version this CLI was built against |
| `{{rust_version}}` | the MSRV stamped into generated `Cargo.toml` files |

Files listed under `verbatim` (and any file that is not valid UTF-8) are copied
unchanged, so binary assets are never corrupted by substitution. The manifest
itself is never written into the scaffolded project.

---

## Authoring and publishing a community starter

1. **Start from a working app.** The cleanest way to build a starter is to make
   a real, compiling project, then replace the project-specific bits with the
   template tokens above. Use `{{crate_name}}` for the package name in
   `Cargo.toml` and `{{project_name}}` wherever a human-readable name appears.

2. **Add `autumn-starter.toml`** at the repository root with at least `name` and
   `description`. Declare any binary/verbatim assets under `verbatim`.

3. **Test it locally** before pushing — the local-directory form exercises the
   exact same path as a git starter, with no network:

   ```bash
   autumn new try-it --starter ./my-starter --yes
   cd try-it && cargo build
   ```

4. **Publish it as a git repository.** Tag releases so users can pin them:

   ```bash
   autumn new acme --starter your-org/my-starter@v1.0.0
   ```

That's it — git URLs are the v1 distribution channel. There is no central
registry to register with; share the `owner/repo` and a ref and you are done.

---

## Reference

- [`examples/saas`](../../examples/saas) — the flagship built-in starter, in its
  rendered form.
- [`STABILITY.md`](../../STABILITY.md) — `--starter`, `--list-starters`,
  `--starter-ref`, and `--yes` are stable CLI surface.
