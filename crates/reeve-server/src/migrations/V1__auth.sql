-- V1: identity/auth tables (docs/decisions/auth.md D1, agent.md D4).
-- Schema law (storage.md D16): explicit PRIMARY KEY on every table.
-- Timestamps are integer unix seconds.

-- Local human users (REEVE_AUTH=password). password_hash is a PHC-format
-- argon2id string.
CREATE TABLE users (
    username      TEXT PRIMARY KEY,
    password_hash TEXT NOT NULL,
    role          TEXT NOT NULL CHECK (role IN ('admin', 'operator', 'viewer')),
    created_at    INTEGER NOT NULL
);

-- SQLite-backed session cookies, sliding expiry (D1). token_hash is hex
-- sha256 of the random session token; the raw token lives only in the
-- cookie.
CREATE TABLE sessions (
    token_hash TEXT PRIMARY KEY,
    username   TEXT NOT NULL REFERENCES users(username) ON DELETE CASCADE,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL
);
CREATE INDEX sessions_expires_idx ON sessions (expires_at);

-- Enrolled devices. Minimal shape for the auth seam; enrollment (C2, D4)
-- extends this table in a later migration rather than recreating it.
CREATE TABLE devices (
    device_id     TEXT PRIMARY KEY,
    hostname      TEXT NOT NULL DEFAULT '',
    arch          TEXT NOT NULL DEFAULT '',
    agent_version TEXT NOT NULL DEFAULT '',
    enrolled_at   INTEGER NOT NULL
);

-- Enrollment-issued device bearer tokens (D1/D4): ONE credential for every
-- device-facing surface. Stored hashed (hex sha256 — sufficient for random
-- 256-bit tokens). Revocation = set revoked_at; one revocation is full
-- site cutoff.
CREATE TABLE device_tokens (
    token_hash TEXT PRIMARY KEY,
    device_id  TEXT NOT NULL REFERENCES devices(device_id) ON DELETE CASCADE,
    created_at INTEGER NOT NULL,
    revoked_at INTEGER
);
CREATE INDEX device_tokens_device_idx ON device_tokens (device_id);
