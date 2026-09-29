### Security

- **`autumn credentials edit` runs the editor you named:** `$VISUAL` /
  `$EDITOR` was split on whitespace, so an editor path containing a space
  (`C:\Program Files\Editor\edit.exe`) executed a different binary
  (`C:\Program`). The value is now parsed properly — an existing file path is
  used verbatim, POSIX shell quoting applies on Unix, a leading quoted program
  on Windows — and a value with unbalanced quotes is refused.
