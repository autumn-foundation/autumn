### Fixed

- **Impersonation now reserves the live step-up session key (issue #2359):**
  `RESERVED_SESSION_KEYS` grew from six entries to seven with the addition of
  the live step-up claim `last_strong_auth_at`. Previously only the *stash*
  key was reserved, so configuring `[auth].session_key =
  "last_strong_auth_at"` passed the reserved-key check: the swap then stashed
  the operator's claim aside and wrote the target id into the key
  `check_step_up` reads as a Unix timestamp, and a numeric target id in the
  neighbourhood of current epoch seconds satisfied step-up with no
  reauthentication. Both the startup log (`AppBuilder::impersonation_gate`)
  and `begin_impersonation`'s refusal now cover the collision.
- **The impersonation audit-attribution doc no longer overstates the
  guarantee (issue #2359):** `audit_actor_id`'s docs and the authentication
  guide now say plainly that automatic impersonator attribution covers
  `#[repository(versioned)]` writes and the audit call sites the framework
  owns (they read `Current`). An `AuditEvent` built by hand records whatever
  `actor_id` the handler passes, so the guide points integrators at
  `impersonation::audit_actor_id(&session, &user).await` (or
  `Current::actor()`) for that path.
