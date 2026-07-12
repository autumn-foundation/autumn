Prompt-Version: v1

# Build brief: Habit Tracker — Phoenix (Elixir)

Read `prompts/_core.md` and `SPEC.md` first; they define the app, the exact JSON
API contract, and the streak semantics. Everything there applies. Below are the
Phoenix-specific bootstrapping notes.

## Framework facts

- Use the **Phoenix** web framework (Elixir) with **Ecto** for persistence.
- Scaffold: `mix phx.new habit_tracker` (Postgres by default; SQLite via
  `--database sqlite3` is also acceptable). This gives you a controller-rendered
  HTML view out of the box for `GET /`.
- **Schemas / migrations.**
  - `Habit` (`name`, `description` nullable, `inserted_at` → expose as
    `created_at`).
  - `Completion` (`belongs_to :habit`, `date :date`) with a unique index on
    `[:habit_id, :date]` in the migration. Insert via a changeset with
    `unique_constraint(:date, name: :completions_habit_id_date_index)`; a
    changeset error on the unique constraint → **409**. Configure the habit's
    `has_many :completions` with `on_delete: :delete_all` (cascade).
  - `mix ecto.create && mix ecto.migrate`.
- **Router / controllers** (`lib/habit_tracker_web/router.ex`):
  - A `:api` pipeline (`plug :accepts, ["json"]`) scoped at `/api` with a
    `HabitController` implementing the JSON endpoints. Set statuses explicitly
    with `put_status(conn, :created | :no_content | :conflict |
    :unprocessable_entity | :not_found)` and `json/2` / `send_resp/3`.
  - A `:browser` pipeline serving `GET /` → an HTML template (`text/html`)
    listing habits (a controller + `.html.heex` template, or a LiveView).
- **Validation.** Ecto changeset `validate_required([:name])` → 422. Parse the
  completion date with `Date.from_iso8601/1`; an `:error` result → 422/400.
- **Streak.** Compute `current_streak` per `SPEC.md` §3 in Elixir from the
  completion dates; `history` = dates sorted descending as ISO `YYYY-MM-DD`
  strings.
- **Seed / demo data.** `priv/repo/seeds.exs` inserting a couple of demo habits
  with completions; run via `mix run priv/repo/seeds.exs`.
- **Tests.** `mix test` with `Phoenix.ConnTest` for the JSON + HTML endpoints.

## run.sh

```sh
#!/usr/bin/env sh
set -e
mix deps.get
mix ecto.setup                      # create + migrate + seed (aliases in mix.exs)
PORT="${PORT:-8080}" exec mix phx.server
```

Ensure the endpoint reads `PORT` (Phoenix's generated `runtime.exs`/`config`
supports `http: [port: String.to_integer(System.get_env("PORT") || "8080")]`).
`mix phx.server` blocks while running.
