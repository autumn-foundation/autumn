### Security

- **inbound-mail:** the SES endpoint now parses `SigningCertURL` and
  `SubscribeURL` and accepts only an SNS origin
  (`https://sns.<region>.amazonaws.com[.cn]`, no user info, no port). Before,
  a raw-text check let `https://sns.<region>.amazonaws.com@<other AWS host>/…`
  through. A caller could then sign forged mail with their own key, or make
  Autumn GET any URL. The certificate fetch now refuses redirects, a missing
  `Content-Length` and a body over 16 KiB. The `TopicArn` check now runs
  before any fetch.
