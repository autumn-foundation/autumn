### Security

- **plugin-sandbox:** a sandboxed plugin can no longer use an encoded path
  segment to send a user's authenticated request to an application route
  (#2463). A manifest route must have one spelling: it may not contain a dot
  segment in any spelling (`%2e` included), an escaped ASCII character, or
  lower-case hex. A plugin redirect may not name a dot segment in any spelling,
  or an encoded `/` or `\`.
