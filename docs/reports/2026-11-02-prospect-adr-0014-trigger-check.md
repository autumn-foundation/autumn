# ⛏️ Prospect: has ADR 0014's revisit trigger fired? (prior-art close: no, 0 of 3 triggers vs ≥1 line)

## 🎯 Question

**Falsifiable:** as of 2026-11-02, has any revisit trigger in
`docs/adr/0014-defer-custom-domain-issuance-budget-coordination.md` (fleet-wide
`IssuanceLimiter` coordination, durable-state option 3 of #2644) fired?
1. Multi-replica custom domains become a supported topology (HTTP-01 token map, certificate
   store and `CustomDomainRegistry` externalized or synchronized).
2. An operator reports ≥2 replicas concurrently issuing custom-domain certificates.
3. A deployment is documented approaching Let's Encrypt's 300 orders / 3 h account limit.

**Decision fed:** whether Keystone reopens the deferral. **Decider:** Keystone / repo maintainer.

## ⚖️ Pre-registration

Criteria are the ADR's own "Trigger to revisit" text (committed 2026-09-28). No apparatus, so no
separate plan commit. Pursue (reopen) = any trigger true; kill (deferral stands) = all false.
Time box: one session.

## 🔍 Prior art

ADR 0014; `git log origin/trunk-dev --since=2026-09-28` (50 commits, one author) filtered by
`multi-replica|replicas|http-01|token map|2644|issuance`; history of `docs/guide/tls.md`,
`autumn/src/custom_domain.rs`, `autumn/src/tenant_domains.rs`. Assay ledger has no prior 0014 entry.

## 🧪 Apparatus

None. `git log`/`grep` only. **Stubs/limits:** git and docs cannot show an operator report or a
private deployment (triggers 2 and 3); the GitHub issue tracker was not re-queried this run.

## 📊 Assay

| Trigger | Evidence | Result |
|---|---|---|
| 1. Multi-replica supported | `docs/guide/tls.md` still says "Run custom domains on a single host" (lines ~449, 927-930). Commits touching these files since 09-28 are #3489 (retention-sweep write gate), #3499 (slow DNS no longer delays other tenants), #3520 (partial load refuses registrations). None externalize the token map, cert store or registry. | false |
| 2. Operator reports ≥2 replicas | No commit, doc or example references it. Single human author on trunk. | false (in repo record) |
| 3. Approaching 300 orders / 3 h | No documented deployment. | false (in repo record) |

Context: the ADR's default path (durable per-domain and global attempt windows) has landed:
`IssuanceLimiter::hydrate` reads a persisted ledger back after restart. That was the part the ADR
said is due regardless, and it is not a trigger.

## 🏁 Verdict

**Kill (deferral stands), prior-art close.** 0 of 3 triggers fired against a line of ≥1.
Caveat: triggers 2 and 3 are judged only from the repo record.

## 💰 Cost to productionize

N/A (not a pursue verdict).

## 🔬 Reproduce

```bash
git fetch origin trunk-dev
git log origin/trunk-dev --since=2026-09-28 --format='%h %ad %s' --date=short -- \
  docs/guide/tls.md autumn/src/custom_domain.rs autumn/src/tenant_domains.rs
grep -n "single host\|single-host" docs/guide/tls.md
grep -n "fn hydrate" autumn/src/custom_domain.rs
```
