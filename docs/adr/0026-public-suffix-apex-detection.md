# ADR 0026: Custom-Domain Apex Detection Uses the Public Suffix List

- Status: Accepted
- Date: 2026-10-26
- Deciders: Autumn maintainers
- Tags: custom-domains, dns, dependencies

## Terms

- **Apex**: the top name of a DNS zone. It has SOA and NS records, so
  RFC 1034 does not let it have a CNAME.
- **Public suffix**: a name under which the public registers domains, for
  example `com`, `co.uk` or `eu.org`.
- **PSL**: the Public Suffix List, <https://publicsuffix.org>.

## Context

`custom_domain::is_apex` selects the record that the tenant DNS instructions
show: a CNAME for a subdomain, A/AAAA for an apex. The function used the rule
"two labels or fewer". That rule is incorrect for an apex under a multi-label
suffix, for example `clientco.co.uk`. The tenant got a CNAME instruction that
a DNS provider refuses (issue #2643).

The grader accepts both record shapes, so a tenant can still verify with A
records. But a deployment that sets only `ingress_hostname` gave such an apex
a CNAME instruction, not the "set `ingress_ipv4` / `ingress_ipv6`" error. The
operator did not get a signal.

## Options

1. The `psl` crate. It compiles the PSL into code.
2. The `publicsuffix` crate (already in `Cargo.lock`) and a copy of the PSL
   data file in this repository.
3. A list of multi-label suffixes that we keep by hand.
4. A caller override: `addresses_for(hostname, ingress)`.
5. Show the CNAME and the A/AAAA records, and let the tenant select.

## Decision

Option 1. An apex is a name that is its own registrable domain under the
PSL, or a name that is a public suffix. The lookup uses the ICANN and the
private sections, because a private suffix such as `eu.org` also delegates
zones. A TLD that is not on the list keeps the two-label rule.

`autumn doctor` calls `RoutingRecord::for_hostname`, as the tenant view does.
It warns when a registered domain gets no DNS instructions.

- Option 1: the license is MIT or Apache-2.0. It shares `psl-types` with the
  current graph. A PSL change gets a new patch release, and Dependabot
  updates it. A debug build of the crate takes approximately 12 s.
- Option 2: the data file is MPL-2.0 inside an MIT/Apache crate. Nothing
  updates it.
- Option 3: the list becomes old, and nothing tells us.
- Option 4: each app must then find the apex itself. This function exists to
  prevent that.
- Option 5: more text for each tenant, and an instruction that can still be
  incorrect.

## Consequences

- `autumn-web` has a new dependency in all feature sets.
- A suffix that is not in the compiled list falls back to the two-label rule,
  as before. A `psl` update corrects it.
- A delegated sub-zone (`eu.clientco.com` with its own SOA) is not an apex
  under the PSL. Only DNS can find it. The grader accepts A/AAAA records, so
  the tenant can still verify.
- Options 4 and 5 stay open if a deployment needs an override.
