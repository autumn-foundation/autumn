-- An app migration that names its schema itself.
CREATE SCHEMA IF NOT EXISTS core;
CREATE TABLE core.contacts (id BIGSERIAL PRIMARY KEY, name TEXT NOT NULL);
CREATE TABLE core.deals (id BIGSERIAL PRIMARY KEY, title TEXT NOT NULL);
CREATE TABLE core.notes (id BIGSERIAL PRIMARY KEY, body TEXT NOT NULL);
