# Team membership, roles, and email invitations

`autumn generate teams` scaffolds organization membership for an existing
Autumn app: organizations (tenants), a closed `Owner`/`Admin`/`Member` role
per membership, and email invitations — issue #1261.

```bash
autumn generate teams
```

Unlike the other generators, `teams` takes no name — it always emits the
same fixed `Organization`/`Membership`/`Invitation` set under `src/teams/`.

It does not invent a new authorization mechanism. It composes three already
stable Autumn primitives:

- `#[repository(..., tenant_scoped)]` (issue #695) — every `Membership`/
  `Invitation` read and write is automatically filtered/stamped by the
  active organization.
- The session `"role"` key (issue #496) — the same key
  `#[secured("...")]`/`PolicyContext::has_role` already read.
- The Mail stack (`#[mailer]`) — the generated `InvitationMailer` sends the
  invite email.

## What it generates

| File | Purpose |
| --- | --- |
| `src/teams/mod.rs` | Module doc (the composition-boundary contract below) + `pub mod` declarations |
| `src/teams/schema.rs` | Diesel `table!` blocks for `organizations`, `memberships`, `invitations` |
| `src/teams/models.rs` | `Organization`, `Membership`, `Invitation` `#[model]` structs + form structs |
| `src/teams/role.rs` | The `Role` enum, `require_role`, `establish_org_session` |
| `src/teams/repositories.rs` | `#[repository]` traits for all three models |
| `src/teams/mailers/invitation_mailer.rs` | `InvitationMailer` (`#[mailer]`) |
| `src/teams/routes/organizations.rs` | `create_organization`, `switch_organization`, `provision_default_organization` |
| `src/teams/routes/invitations.rs` | `create_invitation`, `show_invitation`, `accept_invitation`, `revoke_invitation`, `resend_invitation` |
| `src/teams/routes/members.rs` | `list_members`, `change_role`, `remove_member` |
| `migrations/<timestamp>_create_teams/` | `organizations`/`memberships`/`invitations` tables |
| `src/main.rs` (modified) | `mod teams;` + the ten routes above wired into `routes![...]` |
| `Cargo.toml` (modified) | `"mail"` feature enabled on the `autumn-web` dependency |

It also prints a warning reminding you to configure `[tenancy]` in
`autumn.toml` if you haven't already (see below) — that file's shape varies
too much across projects/profiles for the generator to safely edit for you.

## The composition boundary with your own auth

`teams` deliberately does **not** generate a `routes/auth.rs`: your app's
login/signup already exists (most likely from `autumn generate auth`), and
this module has no idea what your `User` model or `users` table look like.
`Membership::user_id` / `Invitation::invited_by_user_id` are bare `i64` with
no foreign-key coupling to your schema.

**If you generated auth with a custom resource name** (e.g.
`autumn generate auth Account`, whose table is `accounts` rather than the
default `users`), you have one more manual edit: `src/teams/routes/
invitations.rs`'s `caller_email_lookup` module hard-codes a `diesel::table!
{ users (id) { ... } }` declaration for the accept-invitation email check.
`teams` has no way to know which name you used — it's a separate generator
invocation with no shared state — so this is a placeholder, not something
introspected from your project. Rename it to match your actual table, or
every invitation acceptance will fail at runtime with a missing-relation
database error.

Two lines of integration code — the issue's ≤ 3 commands / ≤ 20 lines
success metric — are all that's needed:

**1. At signup**, right after your handler creates the account row, call
`provision_default_organization` to create the new user's personal
organization and make them its `Owner`:

```rust
teams::routes::organizations::provision_default_organization(
    user.id, &mut db,
).await?;
```

`db` is your own signup handler's `Db` extractor — the organization and
membership inserts run in one transaction on it, so a failure between the
two can never leave an orphaned organization with no members.

That covers the organization + membership pair, but not your own preceding
user insert — a plain function call invoked *after* that insert has no way
to join a transaction it wasn't called inside of. If your signup handler
doesn't wrap its own user insert in a transaction (e.g.
`autumn generate auth`'s scaffolded `signup` doesn't), a failure in
`provision_default_organization` still leaves that account stranded with no
organization and no way to retry signup (the email is already taken).

**If you need full 3-way atomicity** (account + organization + membership
all-or-nothing), wrap your own user insert in `db.tx(...)` and call
`provision_default_organization_on_conn` — the same two inserts, minus the
transaction wrapper — on the same `conn`:

```rust
use scoped_futures::ScopedFutureExt;

let user: User = db
    .tx(move |conn| {
        async move {
            let user: User = diesel::insert_into(users::table)
                .values(&new_user)
                .returning(User::as_returning())
                .get_result(conn)
                .await?;
            teams::routes::organizations::provision_default_organization_on_conn(
                conn, user.id,
            )
            .await?;
            Ok::<_, AutumnError>(user)
        }
        .scope_boxed()
    })
    .await?;
```

This means inlining your account-creation insert into a transaction — a
manual step, since `teams` and your auth generator are independent and
neither knows about the other's schema or transaction boundaries. Not
needed for most apps (a stranded, retriable-by-changing-the-email account
row is a rare edge case, not silent data corruption), but available when
you want the stronger guarantee.

This deliberately does **not** touch the session — signup and "the user is
authenticated" aren't the same event for every app. `autumn generate auth`'s
own scaffolded `signup` creates the account but doesn't log it in: it's
gated on email confirmation, and only starts a session once the user
actually confirms and logs in. If this call established the org session
here, appending it to that handler — the most natural place to call it —
would silently authenticate an unconfirmed account, bypassing the
confirmation gate. Establishing the session for the newly-provisioned
organization is step 2's job, below, which picks it up the first time the
user actually logs in.

**2. At login**, after your handler authenticates the user, resolve their
active organization/role and set it on the session:

```rust
let memberships = membership_repo.across_tenants().find_by_user_id(user.id).await?;
if let Some(active) = memberships.into_iter().min_by_key(|m| m.id) {
    let role = teams::role::Role::parse(&active.role).unwrap_or(teams::role::Role::Member);
    teams::role::establish_org_session(&session, user.id, &active.tenant_id, role).await;
}
```

`establish_org_session` deliberately never calls `session.rotate_id()` —
that stays your login/signup handler's own responsibility, once per
authentication event.

**3. At account deletion**, before (or as part of) your handler deletes the
account row, remove that user's team memberships too:

```rust
teams::routes::organizations::remove_all_memberships(user.id, &mut db).await?;
```

`Membership::user_id` has no foreign key back to your `users` table (see
above) — a generated app's `autumn generate auth` `account_destroy` handler
deletes the account row and cascades to its own tracked-session table, but
has no idea `memberships` exists, so those rows are left behind untouched.
Skipping this step means a deleted user's session cookie on another
still-logged-in device keeps working against every team route: `require_role`
only re-checks the live `Membership` row (issue #1261's own
stale-cache-safety guarantee), not whether the account it belongs to still
exists. `remove_all_memberships` runs across every organization the user
belongs to in one transaction — account deletion has no single active
organization to scope the removal to — and, for any organization where the
user is the sole `Owner`, promotes another existing member (preferring an
`Admin`) to `Owner` first, so the same last-owner protection
`change_role`/`remove_member` enforce for interactive requests isn't
silently bypassed by this bulk removal.

Make sure your account-deletion flow also invalidates every *session* for
the deleted user, not just the current device's — `remove_all_memberships`
only removes `memberships` rows. A handler that trusts a bare
`session.get("user_id")` (as every `teams` route does — see above) has no
way to independently tell a still-signed-in session on another device apart
from a live account, since it doesn't know your session/tracked-session
scheme. If your account-deletion handler doesn't already revoke every
device's session for that user (most session backends support "invalidate
all sessions for this user"; a single `session.destroy()` only clears the
*current* request's cookie), a stale session on another device could still
call `POST /organizations` and mint a brand-new organization — that route
has no existing membership to check against, so removing this user's old
memberships doesn't prevent it from creating new ones.

`remove_all_memberships` opens and commits its own transaction, independent
of whatever your account-deletion handler does around it. Called before the
account row is deleted, a subsequent failure of that delete leaves a
still-live account permanently stripped of its memberships; called after, a
failure partway through this call itself leaves a deleted account's
memberships behind — the exact gap this function exists to close. If you
need the two to commit or roll back together, wrap your own account
`DELETE` in `db.tx(...)` and call `remove_all_memberships_on_conn` — the
same cleanup, minus the transaction wrapper — on the same `conn`:

```rust
use scoped_futures::ScopedFutureExt;

db.tx(move |conn| {
    async move {
        teams::routes::organizations::remove_all_memberships_on_conn(conn, user.id).await?;
        diesel::delete(users::table.find(user.id)).execute(conn).await?;
        Ok::<_, AutumnError>(())
    }
    .scope_boxed()
})
.await?;
```

## Guarding routes by role

Gate any admin-only handler the same way `require_role` is used throughout
the generated `src/teams/routes/`:

```rust
teams::role::require_role(&session, &membership_repo, teams::role::Role::Admin).await?;
```

`membership_repo` is a `teams::repositories::PgMembershipRepository` handler
extractor (`membership_repo: teams::repositories::PgMembershipRepository`) —
`require_role` re-reads the caller's live `Membership` row through it rather
than trusting a cached session value, so a revoked/demoted member's stale
session can't keep passing this check. `require_role` returns the caller's
resolved `Role` on success, so a handler that needs to branch on it (e.g. to
show owner-only controls) does not have to look it up twice.

## `[tenancy]` configuration

`Membership`/`Invitation` are `tenant_scoped`, so `autumn.toml` needs:

```toml
[tenancy]
enabled = true
source = "session"
session_key = "organization_id"
```

`autumn generate teams` warns if this looks unconfigured; it never edits
`autumn.toml` for you.

If you want a logged-out invitee to be able to load the invite-accept page
without first signing in, add `/invite` — **not** `/invitations` — to
`public_paths`:

```toml
[tenancy]
public_paths = ["/", "/login", "/signup", "/invite"]
```

`public_paths` matches by path *prefix*. The invitee-facing accept flow
(`GET /invite/{token}`, `POST /invite/{token}/accept`) deliberately lives
under its own `/invite` prefix, separate from the Admin-only
`/invitations` routes (`create`/`revoke`/`resend`) in
`src/teams/routes/invitations.rs`. Listing `/invitations` in
`public_paths` instead would also exempt those Admin routes from tenant
resolution, making every `tenant_scoped` write they do fail with "no
tenant context was established".

## CSRF

Generated forms (`create_invitation`, `change_role`, `remove_member`,
`show_invitation`'s accept form, etc.) embed a hidden `_csrf` field, sourced
from an `Option<CsrfToken>` extractor that degrades to an empty value when
`CsrfLayer` isn't mounted (e.g. `[security.csrf] enabled = false`). This
works even though the module renders through its own minimal,
dependency-free page wrapper (`minimal_page`) rather than your app's own
layout — no further wiring is needed here. If you swap these handlers to
render through your own `crate::layout` instead, carry the same
`csrf_value(&csrf)` pattern (or your layout's equivalent) into every form
you keep.

## Known simplifications versus `examples/teams`

`teams` adapts a hand-built, fully-wired reference application
(`examples/teams` in the Autumn repository) into a generator that composes
into *any* existing app, which forces a few simplifications where the
reference app could reach into its own `users` table directly and this
generator cannot:

- **Accepting an invitation requires an authenticated session.** The
  reference app inlines "create an account and join" for a logged-out
  invitee directly against its own `users` table; this generator can't,
  since it doesn't know your `User` shape. A logged-out visitor sees a
  message pointing at `/login` and `/signup` and can revisit the same
  invitation link afterward.
- **The member roster shows `user_id`, not an email or display name.** Join
  in your own `users` table if you want a friendlier label.
- **Pages render through a minimal, dependency-free HTML wrapper**
  (`teams::routes::minimal_page`), not your app's own `crate::layout` — the
  generator doesn't assume anything about that function's signature or
  branding. Swap the handlers in `src/teams/routes/` to use your own layout
  once the integration seam above is wired.
- **`teams` trusts `session.get("user_id")` on its own, the same as any
  ordinary `#[secured]` route** — it has no way to additionally verify the
  account still exists or that the session was re-validated against your
  app's own tracked-session table (see the account-deletion note above).
  This isn't unique to `teams`: it's true of any handler in your app that
  only checks session presence rather than explicitly calling something
  like a generated `require_tracked_session`. If your app's account
  deletion doesn't revoke every device's session for that user, a stale
  session survives account deletion the same way it would on any other
  route, generated or hand-written.

For the fully-wired version of all three flows (including inline
account-creation-on-accept and email-labeled member rows), read
`examples/teams` — it's the working reference this generator was adapted
from.
