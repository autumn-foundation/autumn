-- Dropping the table resets every generation to 1. A resource that stored a
-- higher token then rejects every new holder. Do not run this on a live
-- database that has fenced resources.
DROP TABLE IF EXISTS autumn_lease_locks;
