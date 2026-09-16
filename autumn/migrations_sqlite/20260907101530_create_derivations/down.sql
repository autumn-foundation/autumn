-- Dropping this table loses every backfill checkpoint. Nothing derived is lost:
-- the next boot re-enqueues each derivation and the backfill rebuilds the
-- maintained columns from the source of truth.

DROP TABLE IF EXISTS _autumn_derivations;
