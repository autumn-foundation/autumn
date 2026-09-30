### Security

- **Channels OOB attribute injection:** `publish_oob` fragments now
  attribute-escape the `hx-swap-oob` value injected onto the fragment root and
  the element id in an `OobSwap::Delete` tombstone. A value or id derived from
  user data could previously close the quoted attribute and inject markup
  (`"><script>…`).
