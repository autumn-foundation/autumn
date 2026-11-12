### Security

- **agent authority:** a governed MCP tool call that the route refuses
  (`401`, `403` or `429` from `#[secured]`, CSRF, `#[throttle]` and the like)
  no longer spends its grant's `rate` cap. The bucket is shared by every
  caller of the grant, so before this an MCP client with no session could use
  up the cap with rejected calls and lock the action out for the agents the
  route admits.
