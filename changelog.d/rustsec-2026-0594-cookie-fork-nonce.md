### Security

- Bumped `cookie` 0.18.2 → 0.18.3 in the lockfile, closing RUSTSEC-2026-0594
  (private-cookie nonces could repeat across processes that `fork()`).
  Autumn reaches `cookie` only through `reqwest`'s client cookie jar, which
  does not encrypt cookies, so the advisory's path is not reachable.
