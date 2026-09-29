### Security

- **custom domains:** a tenant must now prove it controls a custom domain
  with a TXT token before autumn orders a certificate for it (issue #2642).
  Before, verification checked only that the hostname pointed at the
  ingress, so a tenant could claim a hostname another tenant left pointing
  there and get a certificate and routing for it. Each registration mints a
  new token, shown by `DnsInstructions::for_domain` as
  `_autumn-challenge.<hostname> TXT <token>`. `active` domains from before
  the upgrade keep serving and renewing; other stored domains go back to
  `pending_dns` until the tenant publishes the token. New setting:
  `[server.tls.acme.custom_domains] resolvers`. `autumn doctor --online` adds
  a `custom_domain_txt` check.
