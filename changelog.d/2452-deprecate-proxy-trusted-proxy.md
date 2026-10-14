### Deprecated

- **security:** `security::TrustedProxy` and `security::proxy::extract_client_ip`
  are deprecated (issue #2452). The framework never used them. For client IPs,
  use `ProxyResolver`. Both now share its CIDR matcher. We will remove them in
  the next breaking release.
