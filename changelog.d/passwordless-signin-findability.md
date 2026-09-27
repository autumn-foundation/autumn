### Documentation

- **authentication:** the guide now documents passwordless sign-in under the
  words readers search for. `autumn generate auth --magic-link` and
  `--passkeys` were named only in the flag table under "Quick start", so no
  slug, H1 or heading in the 164-page guide carried "magic link",
  "passwordless" or "passkey" — and because the corpus spelled it
  "magic-link", the reader's own spelling matched zero pages even on a
  full-text search. The new section also documents the `[auth.magic_link]`
  (`ttl_minutes`, `email_cooldown_secs`) and `[auth.webauthn]` (`rp_id`,
  `rp_name`, `rp_origin`) settings, which appeared on no reader-facing page:
  `rp_id` and `rp_origin` default to the empty string and the passkey routes
  return `500` until they are set.
