### Testing

- **📖 Folio: gate the direction the docs gates never ran (CLI coverage
  171/193, 0 defects, backlog 10):** the corpus carried twelve docs gates and
  every one of them ran docs → code — "is what we wrote still true?", which is
  drift. None asked "is what we shipped written down anywhere?", which is
  coverage, so a command could ship, work, carry good `--help` text and
  rustdoc, be documented on no page at all, and leave the whole gate tree
  green. The gap is invisible by construction: a gate that only reads the docs
  can never notice a command the docs never mention, and the reader concludes
  the feature does not exist. That is how all four `autumn token` subcommands
  reached 0.7 with "revoke api token" returning zero hits across 160 guide
  pages — the prose half was fixed in #2821, which deferred the gate on the
  grounds that a correct one has to reuse `check-docs-cli.sh`'s `resolve()`
  rather than re-implement it.

  `scripts/check-docs-cli-coverage.sh` runs that reverse direction, and it does
  reuse the sibling: surface, corpus, resolved invocations and the hidden
  command set all come from `check-docs-cli.sh`'s `--list`, `--corpus`,
  `--resolved` and `--list-hidden` modes, so the two cannot disagree about what
  a line names. A private matcher gets the shallow cases right and the deep
  ones wrong — `autumn openapi export` satisfying the top-level `export` (a
  different command, an offline diagnostic snapshot), `autumn db pull posts`
  reading its positional as a subcommand, the `autumn c` alias not counting for
  `console`. Coverage flows from a descendant to its ancestors and never the
  other way, which is the asymmetry that let `autumn token` look documented
  while all four of its subcommands were invisible.

  Of 193 command paths, 171 are documented — the surface net of alias
  spellings, which are folded onto the canonical command first: a
  `#[command(visible_alias = "c")]` makes `autumn c` another way to *type*
  `autumn console`, not a second command to document, and comparing spellings
  would demand both be written down independently. Eleven `destroy` subcommands are
  exempt under the reversal rule `generators.md` states over the family — an
  exemption conditional on both halves and checked on both: the rule sentence
  must still be on the page (delete it and all eleven report), and the matching
  `generate` must itself be documented, which is why `destroy inbound-mail` and
  `destroy policy` are *not* exempt. One is exempt as `#[command(hide = true)]`,
  read out of the derive input, so hiding a command exempts it with no edit to
  the gate. The remaining ten are a triaged backlog carrying a reason each; a
  newly shipped undocumented command is not on that list and fails, and an
  entry that becomes documented fails too, so the list cannot rot into
  unauditable waivers.
