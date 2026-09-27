### Performance

- **cms: `content::ensure_unique_slug` no longer walks candidates one round
  trip at a time.** Saving a post or page whose title collided with
  existing content (a recurring title, a date-stamped post, a bulk import)
  previously ran one `SELECT COUNT(*)` round trip per candidate suffix in a
  loop — up to 199 sequential statements to find a free slug. It now probes
  the first candidate alone (unchanged cost for the common, no-collision
  case), and only when that collides fetches the rest of the same ordered
  candidate list in one `slug = ANY(...)` query — at most 2 round trips
  regardless of how many prior collisions exist. Candidate order, scoping
  and the existing 199-candidate error boundary are unchanged.
