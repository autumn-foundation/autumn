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

## Two modes: edit-and-run, or a prompt

Rust has no stable `eval`. Thus every console is a trade. Autumn gives you two,
on the same scaffolded binary:

| | `autumn console` (edit and run) | `autumn console --repl` (prompt) |
| --- | --- | --- |
| Cost of one question | A compile | A keystroke |
| What you call | All of Rust: your real types, every API | Reads only: `find_all`, `find_by_id`, `count` |
| What you see | Real model values | A JSON projection of each row |
| Editor support | Autocompletion, type checks | None |
| History | Your file | Line history, kept between sessions |

Use the prompt for "what is in this row?" questions. Use the playground for
everything else. The edit-and-run mode follows loco.rs. The prompt uses
[Rhai](https://rhai.rs), a scripting language for Rust.

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
// The generated trait must be in scope for its methods to resolve:
use repositories::post::{PgPostRepository, PostRepository};

let repo = PgPostRepository::with_pool_untracked(ctx.pool().clone());
for post in repo.find_all().await.unwrap() {
    println!("{} {}", post.id, post.title);
}
// ───────────────────────────────────────────────────────────────────────
```

Then:

```bash
autumn console
```

## The REPL (`--repl`)

```bash
autumn console --repl
```

This builds the playground one time, then opens a prompt. Each repository is a
Rhai module:

```text
autumn> PostRepository::count()
2
autumn> PostRepository::find_by_id(1)
{
  "body": "Hello",
  "id": 1,
  "title": "First"
}
autumn> PostRepository::find_all().len()
2
autumn> let p = PostRepository::find_by_id(2); p.title
Second
```

- `help()` lists the commands and the repositories.
- `models()` and `repositories()` return what is registered.
- `exit`, `quit`, `:q` or Ctrl-D stops the prompt. At the prompt, Ctrl-C
  clears the line. While a line runs, Ctrl-C stops the process.
- Variables stay between lines.
- History is in `target/autumn/repl_history.txt`.

### How it works

- `--repl` builds with `--features playground,autumn-web/repl`. It does
  **not** change `Cargo.toml`. A normal `cargo build` never gets Rhai.
- `#[model]` and `#[repository]` register themselves through `inventory`.
  You write no glue for each model. The modules come in through the same
  `#[path]` lines the playground uses.
- The template line `autumn_web::console_repl!(ctx.pool());` opens the
  prompt. Without `--repl`, that line does nothing.
- Repository methods are `async`. Rhai is synchronous. Each call blocks on the
  runtime handle, with a 30-second limit.
- A failed call, a time-out, or a panic is a script error. The prompt stays
  open.

### What you see

A row is the JSON that `Json(model)` sends. Thus `#[private]` and
`#[serde(skip)]` fields do not show. A model with `#[classified]` columns
shows its other columns only. A repository over a hand-written model uses
that model's `Serialize` impl.

### Limits

- Reads only: `find_all`, `find_by_id` and `count`. Writes, derived finders and
  your own functions are not available.
- `find_by_id` returns `()` when no row has that id.
- A tenant-scoped or sharded repository needs request context. At the prompt,
  its reads return a script error.
- Rhai uses `smartstring`. It adds `impl Add<SmartString> for String`. Thus
  `String + &String` in code that the playground compiles does not infer.
  Write `s + other.as_str()` or `format!` instead.

### A playground from before `--repl`

An older `src/bin/playground.rs` has no `console_repl!` line. `--repl` stops
and writes nothing. Add this line after `SeedContext::build()`:

```rust
autumn_web::console_repl!(ctx.pool());
```

Or regenerate the file with `autumn console --force`. This replaces your edits.

## Flags

| Flag | Effect |
| --- | --- |
| `--profile <name>` | Profile forwarded to the playground via `AUTUMN_ENV` (default `dev`). Selects the `[profile.<name>.database]` section in `autumn.toml`. |
| `-p, --package <name>` | Target a workspace member instead of the current directory. |
| `--force` | Overwrite the playground with a fresh copy of the template. |
| `--scaffold-only` | Scaffold and wire the playground, then stop — don't build or run it. |
| `--repl` | Open an interactive Rhai prompt instead of running the playground code. See [The REPL](#the-repl---repl). |

`--profile` also selects which `[profile.<name>.database]` section of
`autumn.toml` supplies the URL, so `autumn console --profile demo` talks to the
same database `AUTUMN_ENV=demo autumn dev` would. (`autumn dev` has no
`--profile` of its own — it reads the profile from the environment, which is
what `--profile` sets here.)

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

On the first run, `autumn console` makes two idempotent edits to `Cargo.toml`,
each reported on stderr:

```toml
[features]
playground = ["autumn-web/seed"]

[[bin]]
name = "playground"
path = "src/bin/playground.rs"
required-features = ["playground"]
```

Edits go through a format-preserving TOML editor and are written atomically, so
comments, key order, and hand-formatted arrays survive and an interrupted run
cannot truncate the file. Your `autumn-web` dependency line is never touched. A
second `autumn console` leaves `Cargo.toml` byte-identical.

The playground source is written in two steps — staged next to its destination
first, moved into place only after `Cargo.toml` has been saved. So a run that
cannot write the source at all (read-only `src`, full disk) leaves your manifest
exactly as it was, rather than registering a target with no file behind it.

If you already declare either of these, `autumn console` adapts rather than
duplicating:

- **An existing `playground` feature** keeps everything it already enables;
  `autumn-web/seed` is merged in as one extra entry if it isn't there already.
  (It has to be: the playground imports `autumn_web::seed::SeedContext`.)
- **An optional dependency named `playground`** keeps working. Cargo gives it
  an implicit `playground = ["dep:playground"]` feature; declaring the key
  without that edge would make Cargo reject the manifest, so the edge is
  carried over when the feature is created.
- **An existing `[[bin]] name = "playground"`** is left exactly as written and
  the playground is scaffolded at *its* path — including when the entry has no
  `path` key, where Cargo infers `src/bin/playground.rs`. Appending a second
  target with the same name would make Cargo reject the manifest outright, so
  the existing one is always reused.

### Why the feature gate matters

`required-features` keeps the playground **out of your default build**.
`cargo build`, `cargo test`, `autumn dev`, and `autumn build` all skip the
target; only `autumn console` (which passes `--features playground`) compiles
it.

That matters because the playground compiles your `models`, `repositories`, and
`policies` into a *separate* crate, and generated code there isn't always
self-contained — an `autumn generate scaffold --live` repository renders
`crate::routes::posts::paths::show(...)`, and `routes` reaches into
`src/main.rs`, which no binary target can see. Without the gate, a playground
that failed to compile would break `autumn dev` for the whole project. With it,
a compile error is a console problem you see immediately and nothing else
changes.

It also means the `seed` feature (which implies `db`) never reaches the normal
builds of a deliberately database-free app.

### Removing the playground

Delete `src/bin/playground.rs` **and** its `[[bin]]` block. The `playground`
feature can stay or go; it costs nothing when unused.

### When `autumn console` refuses

The isolation above is a guarantee, not a best effort. If your manifest is in a
state where it cannot hold, `autumn console` stops **before writing anything**
— manifest byte-identical, no playground scaffolded — and tells you the one
line to change:

| Situation | Why it's refused |
| --- | --- |
| `[[bin]] name = "playground"` exists without `required-features = ["playground"]` | Reusing an ungated target would put the seed-dependent playground into every `cargo build`. |
| `[[bin]] name = "playground"`'s gate names a feature `playground` doesn't enable | `required-features` is an *all-of* list. `autumn console` activates the default features plus `playground`, so a gate like `["playground", "tools"]` can never be satisfied and Cargo would decline to build the target. Either drop the extra, or declare it and have `playground` enable it. This includes `["playground", "default"]` in a manifest with no `[features] default` — `default` is not implicitly present, and Cargo refuses a gate naming a feature that was never declared. |
| The playground path is a directory, or a file blocks a parent directory | A directory at `src/bin/playground.rs`, or a file where `src/bin` needs to be, means the playground cannot be written. Caught before the manifest is touched, so a failed run never leaves the feature and `[[bin]]` entry behind with no source file. |
| `default` enables `playground` (directly or through another feature) | The gate becomes vacuous — the playground would be in every build. |
| Something other than a regular file sits at the playground path, or a non-directory blocks a parent (a file at `src/bin`) | The scaffold could not be written, and finding that out mid-run would leave the `Cargo.toml` edits behind with no playground to go with them. |
| Edition 2015 with no hand-declared **binary** targets | Declaring the first `[[bin]]` turns off auto-discovery of the rest — dropping your `src/main.rs` and `src/bin/*.rs` from the build — and a scaffolded file would meanwhile be auto-discovered as an *ungated* binary. A `[lib]` doesn't count: bin auto-discovery is still on. Add the `[[bin]]` block yourself (the error prints it), then re-run. An `edition.workspace = true` member inherits the workspace's edition, so this only applies if *that* is 2015. |

Each of these is a one-line fix in your `Cargo.toml`, after which `autumn
console` proceeds normally.

## Not included

- A REPL over arbitrary Rust. `--repl` evaluates Rhai over registered
  repositories only (see above).
- Writes from the prompt.
- Remote or production console attach.
- Pretty-printing helpers in the edit-and-run playground.
- An auto-imported prelude of every model — the modules are declared for you,
  but you add the `use` lines you want.

## See also

- [Seeding](seeding.md) — `autumn seed`, which shares this bootstrap.
- [Repositories](repositories.md) — the generated data-access API you'll call
  from the playground.
