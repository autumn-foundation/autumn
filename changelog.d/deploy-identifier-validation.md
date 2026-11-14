### Security

- **cli:** `autumn deploy` refuses a `[deploy]` `service_name`, `app_name`,
  `profile`, `user`, `app_dir` or host that a remote shell, `systemctl`,
  `ssh` or a unit file can read as code. Before, `service_name = "x; id"` ran
  `id` on the deploy host as the SSH user (by default `root`) on `up`,
  `rollback` and teardown. The check also applies to a name from the package
  name or the directory name. `autumn doctor` reports the same failure.
- **cli:** `autumn deploy` now shell-quotes every unit operand of `systemctl`
  and `systemd-run`. The `scp` upload refuses a remote path with a character
  that a remote shell can read as code.
- **cli:** A value that was accepted before can now fail, for example a
  relative `app_dir`. Set the key to a permitted value. See
  [Deployment](docs/guide/deployment.md) for the permitted characters.
