# The Data Playground (`autumn console`)

`autumn console` is Autumn's answer to `rails console`, `manage.py shell`, and
`iex -S mix`: a one-command, pre-wired place to run real queries against your
app's database.

```bash
autumn console
```

That's the whole usage. The first invocation scaffolds
`src/bin/playground.rs`, wires your `Cargo.toml` for it, then builds and runs
it. Every invocation after that just builds and runs whatever you last edited.

## Why it isn't a REPL

Rust has no stable `eval`, so there is no honest way to offer a line-by-line
interactive shell the way Ruby, Python, and Elixir do. Autumn follows the model
loco.rs uses instead: **edit and run**. You get a real Rust file, with your real
types, your real models, and your editor's autocompletion — and `autumn console`
handles the compile-and-run loop.

## What's already wired

The scaffolded playground hands you a live database with zero boilerplate:

- **Config and database-URL resolution identical to `autumn dev` and
  `autumn seed`** — `AUTUMN_DATABASE__PRIMARY_URL` → `AUTUMN_DATABASE__URL` →
  `DATABASE_URL` → `autumn.toml` (profile-aware). The console therefore always
  talks to the same database your app would, with no drift between "console" and
  "prod" connection logic.
- **A constructed async pool** — `ctx.pool()`.
- **A checked-out connection** — `db`, ready to pass to Diesel, model, and
  repository calls as `&mut db`.
- **Your app's data modules in scope.** A Cargo binary target is its own crate
  and cannot see `src/models/`, so the playground declares `schema`, `models`,
  `repositories`, and `policies` with `#[path]` for you. Add more `#[path]`
  lines the same way if a query needs another module.

Put your query between the `// ── your code here` markers:

```rust
// ── your code here ─────────────────────────────────────────────────────
let repo = repositories::post::PgPostRepository::with_pool_untracked(ctx.pool().clone());
for post in repo.find_all().await.unwrap() {
    println!("{} {}", post.id, post.title);
}
// ───────────────────────────────────────────────────────────────────────
```

Then:

```bash
autumn console
```

## Flags

| Flag | Effect |
| --- | --- |
| `--profile <name>` | Profile forwarded to the playground via `AUTUMN_ENV` (default `dev`). Selects the `[profile.<name>.database]` section in `autumn.toml`. |
| `-p, --package <name>` | Target a workspace member instead of the current directory. |
| `--force` | Overwrite the playground with a fresh copy of the template. |
| `--scaffold-only` | Scaffold and wire the playground, then stop — don't build or run it. |

`autumn c` is a shorthand alias for `autumn console`.

## Your edits are safe

Re-running `autumn console` **never** overwrites an existing playground. Once
the file is there, it is ordinary user code; the command only compiles and runs
it. Pass `--force` when you want the template back.

## Failures are loud

A missing database URL, an unparsable `autumn.toml`, or an unreachable server
prints the underlying error and exits non-zero — from the playground binary out
through `autumn console`'s own exit status. There is no silent success to
mistake for an empty result set.

## What it changes in your project

On the first run, `autumn console` makes three idempotent edits to
`Cargo.toml`, each reported on stderr:

1. registers the `playground` bin target;
2. enables the `autumn-web` `seed` feature (the playground bootstraps through
   `autumn_web::seed::SeedContext`, which is gated on it);
3. sets `default-run` to your package name when it isn't already set — without
   it, adding a second binary would make a bare `cargo run` ambiguous.

Edits go through a format-preserving TOML editor, so comments, key order, and
hand-formatted arrays survive. A second `autumn console` leaves `Cargo.toml`
byte-identical.

## Not included

- A line-by-line eval REPL (see above).
- Remote or production console attach.
- Readline history or pretty-printing helpers.
- An auto-imported prelude of every model — the modules are declared for you,
  but you add the `use` lines you want.

## See also

- [Seeding](seeding.md) — `autumn seed`, which shares this bootstrap.
- [Repositories](repositories.md) — the generated data-access API you'll call
  from the playground.
