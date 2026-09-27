Prompt-Version: v1

# Build brief: Habit Tracker — Ruby on Rails

Read `prompts/_core.md` and `SPEC.md` first; they define the app, the exact JSON
API contract, and the streak semantics. Everything there applies. Below are the
Rails-specific bootstrapping notes.

## Framework facts

- Use **Ruby on Rails** (a full app, not `--api`-only, since you need a
  server-rendered HTML view).
- Scaffold: `rails new habittracker` (SQLite default is fine; Postgres also ok).
- **Models.**
  - `rails g model Habit name:string description:text` (`created_at` is provided
    by Rails timestamps).
  - `rails g model Completion habit:references date:date` and add a unique index
    on `[:habit_id, :date]` in the migration so a duplicate raises
    `ActiveRecord::RecordNotUnique` → rescue and return **409**.
  - `Habit has_many :completions, dependent: :destroy` (cascade on delete).
- **Routes** (`config/routes.rb`):
  ```ruby
  root "habits#page"                       # GET / → HTML view
  namespace :api do
    resources :habits do
      post :complete, on: :member          # POST /api/habits/:id/complete
    end
  end
  ```
- **Controllers.** `Api::HabitsController` renders JSON with explicit statuses:
  `render json: habit, status: :created` (201), `head :no_content` (204),
  `status: :conflict` (409), `status: :unprocessable_entity` (422),
  `status: :not_found` (404). A separate `HabitsController#page` renders an
  `.html.erb` view (`text/html`).
- **Validation.** `validates :name, presence: true`. Parse the completion date
  with `Date.iso8601(params[:date])` and rescue `ArgumentError` → 422/400.
- **Streak.** Compute `current_streak` per `SPEC.md` §3 in Ruby from the ordered
  completion dates; `history` = dates sorted descending as ISO strings.
- **Seed / demo data.** `db/seeds.rb` creating a couple of demo habits with
  completions; run via `rails db:seed`.
- **Tests.** Minitest (`rails test`) or RSpec request specs covering the core
  behavior.

## run.sh

```sh
#!/usr/bin/env sh
set -e
bundle install
bin/rails db:prepare        # create + migrate
bin/rails db:seed
exec bin/rails server -b 0.0.0.0 -p "${PORT:-8080}"
```

`rails server` blocks while running.
