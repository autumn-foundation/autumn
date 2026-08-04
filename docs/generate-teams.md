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

Two lines of integration code — the issue's ≤ 3 commands / ≤ 20 lines
success metric — are all that's needed:

**1. At signup**, after your handler establishes the base session, call
`provision_default_organization` to create the new user's personal
organization and make them its `Owner`:

```rust
teams::routes::organizations::provision_default_organization(
    &session, user.id, &mut db,
).await?;
```

`db` is your own signup handler's `Db` extractor — the organization and
membership inserts run in one transaction on it, so a failure between the
two can never leave an orphaned organization with no members.

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

Generated forms (`create_invitation`, `change_role`, `remove_member`, etc.)
do **not** carry a CSRF token — this module renders through its own minimal,
dependency-free page wrapper (`minimal_page`) rather than your app's own
layout, so it has no shared place to thread one through. If your app has
`[security.csrf]` enabled (the framework default), add a hidden `_csrf`
field to each generated `<form>` yourself once you've wired the handlers
through your own layout.

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

For the fully-wired version of all three flows (including inline
account-creation-on-accept and email-labeled member rows), read
`examples/teams` — it's the working reference this generator was adapted
from.
