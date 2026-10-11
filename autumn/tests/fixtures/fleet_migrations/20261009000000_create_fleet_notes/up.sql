-- Tenant data for the SQLite fleet integration test (ADR 0019). Applied to
-- every fleet database when it is created or opened.
CREATE TABLE fleet_notes (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    body TEXT NOT NULL,
    tenant_id TEXT
);
