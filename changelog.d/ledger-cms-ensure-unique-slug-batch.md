### Performance

- **cms: `content::ensure_unique_slug` allocates a slug in one statement.**
  Saving a post or page whose title collided with existing content
  (a recurring title, a date-stamped post, a bulk import) previously ran
  one `SELECT COUNT(*)` round trip per candidate suffix in a loop — up to
  199 sequential statements to find a free slug. It now builds the same
  ordered candidate list the loop would have walked and checks it in one
  `slug = ANY(...)` query, so allocation costs one round trip regardless of
  how many prior collisions exist. Candidate order, scoping and the
  existing 199-candidate error boundary are unchanged.
