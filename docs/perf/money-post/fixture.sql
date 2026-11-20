-- Production-shaped money ledger (autumn/src/money/ledger.rs) for the post
-- harness. Seeded and deterministic. psql -v txns=N.
--
-- A marketplace: 200,000 customer wallets, 20,000 seller accounts, three
-- house accounts. History is :txns transactions over 365 days:
--   95% a 3-leg charge: customer wallet credited, seller debited the net,
--       platform:fees debited the 10% fee (every charge touches it).
--    5% a 20-leg payout: payout:pool credited, 19 sellers debited.
-- Customer choice skews (random()^2), seller choice skews harder
-- (random()^3), so a few sellers carry most postings.
-- Keys are 64-hex sha256 like IdempotencyKey::derive; ids are uuids like
-- post's Uuid::new_v4 (psql -v idfn='gen_random_uuid()::text'), or v7 with
-- idfn='uuid_v7()', a deployment that only ever ran a v7 post.
-- Postings go in posted_at order, as an append-only table fills.
-- Balances are the stored sums, as the trigger keeps them, except 1% of
-- customer wallets dormant since the stored-balance migration (NULL).
\set ON_ERROR_STOP 1
SELECT setseed(0.3741);

ALTER TABLE _autumn_money_postings DISABLE TRIGGER ALL;
ALTER TABLE _autumn_money_transactions DISABLE TRIGGER ALL;

INSERT INTO _autumn_money_accounts (id, currency, allow_negative, created_at, balance_minor)
SELECT 'customer:' || g, 'USD', TRUE, TIMESTAMPTZ '2025-11-01' + g * INTERVAL '1 second', 0
  FROM generate_series(1, 200000) g;
INSERT INTO _autumn_money_accounts (id, currency, allow_negative, created_at, balance_minor)
SELECT 'seller:' || g, 'USD', TRUE, TIMESTAMPTZ '2025-11-01' + g * INTERVAL '1 second', 0
  FROM generate_series(1, 20000) g;
INSERT INTO _autumn_money_accounts (id, currency, allow_negative, created_at, balance_minor)
VALUES ('platform:fees', 'USD', TRUE, '2025-11-01', 0),
       ('platform:cash', 'USD', TRUE, '2025-11-01', 0),
       ('payout:pool',   'USD', TRUE, '2025-11-01', 0);

CREATE TEMP TABLE t AS
SELECT g,
       :idfn AS id,
       encode(sha256(('order:' || g)::bytea), 'hex') AS key,
       random() < 0.05 AS payout,
       1 + floor(200000 * random() ^ 2)::int AS customer,
       1 + floor(20000 * random() ^ 3)::int AS seller,
       (500 + floor(random() * 20000))::bigint AS amount,
       TIMESTAMPTZ '2025-11-16' + (g::float8 / :txns) * INTERVAL '365 days' AS at
  FROM generate_series(1, :txns) g;

INSERT INTO _autumn_money_transactions (id, idempotency_key, request_hash, currency, memo, posted_at)
SELECT id, key, encode(sha256(('req:' || g)::bytea), 'hex'), 'USD',
       CASE WHEN payout THEN 'payout run ' || g ELSE 'order ' || g END, at
  FROM t ORDER BY g;

INSERT INTO _autumn_money_postings (transaction_id, seq, account_id, amount_minor, currency, posted_at)
SELECT id, s.seq, s.account_id, s.amount, 'USD', at
  FROM t, LATERAL (
        VALUES (0, 'customer:' || customer, -amount),
               (1, 'seller:' || seller, amount - amount / 10),
               (2, 'platform:fees', amount / 10)
       ) AS s(seq, account_id, amount)
 WHERE NOT payout
UNION ALL
SELECT id, s.seq, s.account_id, s.amount, 'USD', at
  FROM t, LATERAL (
        SELECT 0::int, 'payout:pool', -amount * 19
        UNION ALL
        SELECT k, 'seller:' || (1 + (seller + k * 997) % 20000), amount
          FROM generate_series(1, 19) k
       ) AS s(seq, account_id, amount)
 WHERE payout
 ORDER BY 6, 2;

ALTER TABLE _autumn_money_postings ENABLE TRIGGER ALL;
ALTER TABLE _autumn_money_transactions ENABLE TRIGGER ALL;

UPDATE _autumn_money_accounts a SET balance_minor = s.total
  FROM (SELECT account_id, SUM(amount_minor)::bigint AS total
          FROM _autumn_money_postings GROUP BY account_id) s
 WHERE a.id = s.account_id;
UPDATE _autumn_money_accounts SET balance_minor = NULL
 WHERE id LIKE 'customer:%' AND hashtext(id) % 100 = 0;

VACUUM (ANALYZE) _autumn_money_accounts;
VACUUM (ANALYZE) _autumn_money_transactions;
VACUUM (ANALYZE) _autumn_money_postings;
