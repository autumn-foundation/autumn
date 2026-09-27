### Fixed

- **🧭 Wayfinder: redisplay the dashboard on a rejected project name in
  `examples/saas` (error-path 0/1 → 1/1, name preserved):** an error-path
  inventory of `saas`'s tenant dashboard — the create-project form behind
  `POST /dashboard/projects`, the example's only hand-written
  content-authoring flow — found its one recoverable failure mode (a name
  that is empty after trimming, or over 200 characters, mirroring
  `Project`'s own `#[validate(length(min = 1, max = 200))]`) sent the
  submission through a bare `Err(AutumnError::unprocessable_msg(...))`
  instead of redisplaying the form: 0 of 1 failure modes was adjacent to
  cause, persisted in place, said how to recover, or preserved what the user
  typed — the visitor was bounced to the framework's generic error page,
  losing the project name they had just entered and the list of projects
  they were looking at. This is the same anti-pattern already fixed in
  `saas`/`teams`'s auth forms (#2530), `reddit-clone`'s create-community form
  (#2665) and `blog`'s post editor (#2687). Fix: the dashboard's markup moves
  into a shared `dashboard_page(tenant_id, total, projects, name, error)`
  render function, called by both the `GET /dashboard` handler and
  `create_project`'s failure branch, which now re-fetches the tenant's
  project list and count and re-renders the same page at HTTP 200 with the
  rejected name still in the field (`aria-invalid` set) and the message
  shown adjacent to the form, instead of navigating away — no redesign, no
  new dependency, the existing Tailwind classes untouched. Verified with a
  new integration test,
  `create_project_failure_redisplays_the_dashboard_with_name_preserved`
  (`cargo test -p saas --test integration_test -- --ignored`): an
  empty/whitespace-only name and a 201-character name both redisplay the
  dashboard with the validation message, the rejected name preserved in the
  field, and the pre-existing project list intact, with no project created
  by either rejected submission. `cargo clippy -p saas --all-targets -- -D
  warnings` and `cargo fmt --all -- --check`: clean.
- **`examples/saas` and the `saas` starter: a new project shows its own
  name, not the tenant id (issue #2854).** The `Project` fields were not in
  the column order of the `projects` table. The generated repository reads
  decode each row by position, so the dashboard showed the tenant id as the
  name of every project. The fields now follow the table order.
