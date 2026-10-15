### Deprecated

- **security:** `security::TrustedProxy` and `security::proxy::extract_client_ip`
  are deprecated (issue #2452). This is a non-breaking change. The framework
  never used them. For client IPs, use `ProxyResolver`. Both now share its CIDR
  matcher.
