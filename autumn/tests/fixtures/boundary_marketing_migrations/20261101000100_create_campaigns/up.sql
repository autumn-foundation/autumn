-- No schema here: the migration set runs in schema `marketing`.
CREATE TABLE campaigns (id BIGSERIAL PRIMARY KEY, name TEXT NOT NULL);
