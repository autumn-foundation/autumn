### Fixed

- **plugin sandbox:** the host counts the values and nesting of a guest frame
  before it parses the frame. A frame that no legal guest sends is refused with
  no large allocation (issue #2523).
- **plugin sandbox:** a `db-query` page counts its JSON syntax toward the result
  ceiling, so the reply always fits the room the host reserved (issue #2523).
- **plugin sandbox:** a plugin that drops calls without a pause no longer keeps
  one drop record open for hours. A summary window now counts only the drops
  inside it (issue #2523).
