### Testing

- **Docs gate: a capability's page must carry the word readers search it by
  [no-plugin].**
  `scripts/check-docs-aliases.sh` joins the docs-only CI job. The existing
  docs gates all ask whether a page is *true* — its links, commands, config
  keys, symbols, actuator URLs and macro arguments. None asks whether a reader
  *finds* it. `check-docs-orphans.sh` is the near miss: it proves a page is
  reachable by clicking from an entry surface, which is a different question,
  since nobody clicks through a 159-page guide with no index — they type the
  word they already have. Baseline: `autumn generate auth User --totp` ships
  two-factor authentication, and `docs/guide/authentication.md` documented it
  accurately as "TOTP" and "Multi-factor" while never once saying "2FA" or
  "two-factor". Searching the reader-facing corpus for those words returned
  four hits and not one was an answer — two were `%2F` URL escapes read as
  "2FA", one an example URL on the rate-limiting page, one an agent skill file.
  The page is unchanged in substance; it now names the capability in the
  reader's words in its intro, its generator-flag table and its "where to go
  next" list, so the search lands on the page that already answered the
  question. The gate unescapes `%XX` before matching (the escapes are why a
  plain grep reported the term as present) and a row is satisfied only by the
  page that answers the question, never by an incidental hit elsewhere. Sixteen
  capability rows ship green; the table is declared rather than discovered, so
  naming a new capability means adding a row.
