-- Accounts. A user is NOT tied to a single organization (unlike
-- `examples/saas`'s one-tenant-per-user shortcut): membership in zero or more
-- organizations is expressed entirely through the `memberships` join table
-- below. Users are looked up by email at login, so this table is intentionally
-- not tenant-scoped.
CREATE TABLE users (
    id            BIGSERIAL PRIMARY KEY,
    email         TEXT      NOT NULL UNIQUE,
    password_hash TEXT      NOT NULL,
    created_at    TIMESTAMP NOT NULL DEFAULT NOW()
);

-- An organization (tenant). Creating one makes the creator an `owner` member
-- (see `routes/organizations.rs`).
CREATE TABLE organizations (
    id         BIGSERIAL PRIMARY KEY,
    name       TEXT      NOT NULL,
    created_at TIMESTAMP NOT NULL DEFAULT NOW()
);

-- The user <-> organization join row, carrying the closed `role` enum
-- (`owner` | `admin` | `member`, see `src/role.rs`). `tenant_scoped` on
-- `#[repository(Membership, ...)]` filters every read/write by the active
-- organization at the SQL level (issue #695's row-level multi-tenancy seam,
-- not a second isolation mechanism) — but the macro's generated queries
-- unconditionally filter on a column literally named `tenant_id` (`TEXT`,
-- mirroring `examples/saas`'s `Project.tenant_id`), so this holds the
-- organization's id in its string form rather than a typed FK; application
-- code parses it back to `i64` where it needs to look up the `Organization`
-- row itself (see `org_repo.find_by_id` call sites). The UNIQUE constraint is
-- the idempotency backstop for a double-clicked invitation accept: a second
-- INSERT for the same (tenant_id, user_id) pair fails closed instead of
-- creating a duplicate membership.
CREATE TABLE memberships (
    id         BIGSERIAL PRIMARY KEY,
    tenant_id  TEXT      NOT NULL,
    user_id    BIGINT    NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    role       TEXT      NOT NULL CHECK (role IN ('owner', 'admin', 'member')),
    created_at TIMESTAMP NOT NULL DEFAULT NOW(),
    UNIQUE (tenant_id, user_id)
);
CREATE INDEX idx_memberships_tenant ON memberships (tenant_id);
CREATE INDEX idx_memberships_user ON memberships (user_id);

-- A single-use, expiring, cryptographically-random email invitation. Only the
-- SHA-256 hash of the token is stored (`hash_api_token`, mirroring
-- `examples/saas`'s remember-token pattern) — never the raw token — so a
-- database leak cannot be replayed as an accept link. `status` starts
-- `pending`; accepting sets it to `accepted`, revoking sets it to `revoked`.
-- Both are terminal: the accept handler checks `status = 'pending'` AND
-- `expires_at > now()` before creating a membership, so an
-- expired/revoked/already-accepted token renders a clear error instead of a
-- second membership row or a panic.
CREATE TABLE invitations (
    id                 BIGSERIAL PRIMARY KEY,
    tenant_id          TEXT      NOT NULL,
    email              TEXT      NOT NULL,
    role               TEXT      NOT NULL CHECK (role IN ('owner', 'admin', 'member')),
    token_hash         TEXT      NOT NULL UNIQUE,
    status             TEXT      NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'accepted', 'revoked')),
    invited_by_user_id BIGINT    NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    expires_at         TIMESTAMP NOT NULL,
    created_at         TIMESTAMP NOT NULL DEFAULT NOW()
);
CREATE INDEX idx_invitations_tenant ON invitations (tenant_id);
CREATE INDEX idx_invitations_email ON invitations (email);
