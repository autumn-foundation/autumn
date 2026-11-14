# SES via SNS: `SigningCertURL` user info replaced the signing key

- **Surface:** `inbound_mail::sns_verify::verify` and the SES route's
  `SubscriptionConfirmation` fetch (`inbound-ses` feature).
- **Threat model:** a caller on the Internet who knows the SES path and the
  `TopicArn`. They serve a PEM from an AWS host they control and send
  `SigningCertURL: https://sns.us-east-1.amazonaws.com@<their host>/cert.pem`.
  The raw-text check saw `sns.…amazonaws.com`. The URL parser and the fetch
  used `<their host>`. Their key then verified their signature, and the
  `TopicArn` (inside the signed document) matched. Result: forged inbound mail
  dispatch, or a GET to any `SubscribeURL` (blind SSRF).
- **Reproduction:** `cargo test -p autumn-web --features "inbound-mail,inbound-mailgun,inbound-ses,mail" --lib inbound_mail::tests::sns`
  and `cargo test -p autumn-web --features "inbound-mail,inbound-mailgun,inbound-ses,mail,test-support" --test integration_tests inbound_mail_integration::ses`.
  Trunk output: `trunk-failure.txt` (`502` = a TLS connection to the other
  host). Fixed: `after.txt`.
- **Root cause:** `is_valid_sns_cert_url` split the raw text at `/` and
  checked a prefix and a suffix. It did not parse the authority. The
  `SubscribeURL` had no check.
- **Fix:** one parsed-origin policy (`sns_origin`) for both URLs: `https`, a
  DNS host `sns.<region>.amazonaws.com` (or `.com.cn` for `cn-` regions), no
  user info, no port, and raw authority text equal to the parsed host. The
  fetch uses the parsed `Url`, through the `SnsCertUrl` and `SnsSubscribeUrl`
  types. `SigningCertURL` must be one canonical `/<name>.pem`. `SubscribeURL`
  must be `/?Action=ConfirmSubscription` with the configured `TopicArn`. The
  SNS client does not follow redirects and has a 10 s timeout. The
  certificate body needs `Content-Length` of 16 KiB or less. The `TopicArn`
  check now runs before the fetch.
- **Variant sweep:** other raw `https://` host checks in `autumn/src`.
  `plugin_sandbox::capability::outbound::host_of` refuses `@` and checks the
  host after the authority split. `mail.rs` list-unsubscribe parses with
  `Url` and refuses user info. No other host allow-list was found.
- **Compatibility:** URLs that real SNS sends still pass. A `SubscribeURL`
  outside SNS now gets `400`, not a fetch. Changelog fragment added.
