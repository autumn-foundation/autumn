### Security

- **deploy:** `autumn deploy` and `autumn doctor` refuse a `[deploy]` host
  that `ssh` or `scp` can read as an option, for example
  `-oProxyCommand=…`. Before this change, such a value in `autumn.toml` could
  run a local command on the machine that ran the deploy. The `ssh` and `scp`
  argv also ends the options with `--` before the destination.
- **deploy:** a host must be an IPv6 address without brackets (a zone such as
  `%eth0` is permitted), or a host name or IPv4 address. A bracketed address
  or a `host:port` value is refused. Use a `Host` alias in `~/.ssh/config`
  for such a host.

### Fixed

- **deploy:** `scp` now uploads to an IPv6 `[deploy]` host. The `scp`
  destination puts the host in brackets.
