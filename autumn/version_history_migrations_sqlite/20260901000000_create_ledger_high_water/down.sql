-- WARNING: this destroys the ledger's out-of-band high-water marks.
--
-- Without them a post-truncation append re-uses the deleted sequence number
-- again, so deleting the newest revision and letting ordinary traffic land
-- becomes undetectable from inside the database once more (issue #2323).
-- Revisions themselves are untouched; the evidence that no revision is missing
-- from the end is what goes.

DROP TABLE IF EXISTS _autumn_ledger_high_water;
