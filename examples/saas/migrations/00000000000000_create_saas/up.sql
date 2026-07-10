-- Accounts. `tenant_id` is the organisation a user belongs to; it is the value
-- every tenant-scoped query is filtered by. Users are looked up by email at
-- login (across tenants — you don't know the tenant until after authentication),
-- so this table is intentionally NOT tenant-scoped.
CREATE TABLE users (
    id            BIGSERIAL PRIMARY KEY,
    email         TEXT      NOT NULL UNIQUE,
    password_hash TEXT      NOT NULL,
    tenant_id     TEXT      NOT NULL,
    created_at    TIMESTAMP NOT NULL DEFAULT NOW()
);
CREATE INDEX idx_users_tenant ON users (tenant_id);

-- The tenant-scoped domain table. Every row carries `tenant_id`; the
-- `#[repository(Project, tenant_scoped)]` macro fills it in on insert and
-- filters every read by the current tenant, so one organisation can never see
-- another's projects.
CREATE TABLE projects (
    id         BIGSERIAL PRIMARY KEY,
    tenant_id  TEXT      NOT NULL,
    name       TEXT      NOT NULL,
    created_at TIMESTAMP NOT NULL DEFAULT NOW()
);
CREATE INDEX idx_projects_tenant ON projects (tenant_id);

-- Persistent "remember-me" login chains (issue #1397). One row per device
-- login-chain, keyed by the stable opaque `series`; `token_hash` rotates on
-- every use so replaying a rotated-out cookie is detected as theft. Only the
-- SHA-256 hash of the current token is stored — never the raw token — so a
-- database leak cannot be replayed as a remember cookie. `ON DELETE CASCADE`
-- ties a chain's lifetime to its account.
CREATE TABLE remember_tokens (
    series       TEXT        PRIMARY KEY,
    user_id      BIGINT      NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    tenant_id    TEXT        NOT NULL,
    token_hash   TEXT        NOT NULL,
    expires_at   TIMESTAMPTZ NOT NULL,
    ip           TEXT        NOT NULL DEFAULT '',
    user_agent   TEXT        NOT NULL DEFAULT '',
    ua_family    TEXT        NOT NULL DEFAULT '',
    ua_os        TEXT        NOT NULL DEFAULT '',
    ua_device    TEXT        NOT NULL DEFAULT '',
    label        TEXT,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_used_at TIMESTAMPTZ
);
CREATE INDEX idx_remember_tokens_user ON remember_tokens (user_id);
