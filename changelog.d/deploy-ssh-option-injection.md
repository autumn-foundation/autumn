### Security

- **deploy:** `autumn deploy` and `autumn doctor` refuse a `[deploy] user` or
  host that `ssh` or `scp` can read as an option, for example
  `-oProxyCommand=…`. Before this change, such a value in `autumn.toml` could
  run a local command on the machine that ran the deploy. The `ssh` and `scp`
  argv also ends the options with `--` before the destination.
- **deploy:** a `[deploy] user` must be 1 to 64 ASCII letters, digits, `.`,
  `_`, `-` or `@`. A host must be a host name, an IPv4 address or a plain IPv6
  address. A config with other values that worked before is now refused. Use
  a `Host` alias in `~/.ssh/config` for such a host.

### Fixed

- **deploy:** `scp` now uploads to an IPv6 `[deploy]` host. The `scp`
  destination puts the host in brackets.
