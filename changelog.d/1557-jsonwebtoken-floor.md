### Security

- **deps:** raise the `jsonwebtoken` floor to `10.3.0` so a downstream
  resolution can no longer select a release older than the fix for its
  claim-validation advisory (PR #1557).
