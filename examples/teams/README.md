# teams — organization membership, roles, and email invitations

A complete, runnable multi-user SaaS application built from Autumn's shipped
primitives: session-based authentication, row-level multi-tenancy, and the
Mail stack — composed into the "missing middle" every team-shaped B2B app
needs (issue #1261). Sign up, get your own organization as its `Owner`,
invite a teammate by email, and watch them join with the role you assigned.

## How it works

| Piece | Primitive |
|-------|-----------|
| Signup / login / logout | `Session` + `hash_password`/`verify_password` (bcrypt) |
| Tenant per active organization | `#[repository(Membership, tenant_scoped)]` / `#[repository(Invitation, tenant_scoped)]`, tenant resolved from the session (issue #695) |
| Role in the active organization | closed `Role` enum (`Owner`/`Admin`/`Member`) + `require_role`, layered on the same session `"role"` key `#[secured]`/`PolicyContext` already read (issue #496) — no second authorization mechanism |
| Invitation email | `#[mailer]` templated `InvitationMailer` |
| Server-rendered UI | Maud templates + htmx + Tailwind |

Unlike `examples/saas` (one tenant permanently fixed to each user at signup),
here a user can belong to any number of organizations via `Membership` rows;
the *active* organization is resolved at login/signup/invite-accept and
stored in the session, and is switchable via `POST /organizations/{id}/switch`.

## Prerequisites

- Rust 1.88.0+
- PostgreSQL (a `docker-compose.yml` is included for local development)

## Quick start

```bash
docker compose up -d        # start Postgres
autumn migrate              # create the users/organizations/memberships/invitations tables
autumn dev                  # run the app at http://localhost:3000
```

Then open <http://localhost:3000>, sign up (you become the `Owner` of your own
organization), and visit `/members` to invite a teammate by email and role.
The dev mailbox (`[mail] transport = "log"`) prints the invite email —
including the accept link — to the server log.

### Success check

```bash
# After signing up in the browser, the member list serves 200 OK:
curl -i http://localhost:3000/members --cookie "autumn.sid=<your-session>"
```

## Tests

```bash
cargo test -p teams                                          # smoke tests (no Docker)
cargo test -p teams -- --include-ignored --test-threads=1    # full flow (needs Docker)
```

## Where to look

- `src/routes/auth.rs` — signup (auto-creates a personal organization as
  `Owner`), login (resolves the active organization + role), `establish_session`
- `src/routes/organizations.rs` — create an additional organization, switch
  the active one
- `src/routes/invitations.rs` — invite, accept (new-user and
  already-authenticated paths), revoke, resend
- `src/routes/members.rs` — member list, change role, remove member
  (`Admin`+, last-`Owner` protected)
- `src/role.rs` — the `Role` enum and `require_role` guard
- `src/mailers/invitation_mailer.rs` — the invite email template
- `src/models.rs` / `migrations/` — `Organization`, `Membership`, `Invitation`
