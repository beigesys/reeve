-- V6: secrets vault (C7; spec/reeve/10-secrets.md §12.2,
-- docs/decisions/secrets.md D15). Schema law (storage.md D16):
-- explicit PRIMARY KEY on every table; timestamps integer unix seconds.

-- One row per (name, scope): the AEAD-encrypted secret value.
--   scope      — where on the layer chain the secret is defined:
--                fleet | class.<n> | region.<n> | site.<n> |
--                device.<id>, plus the reserved `reeve-internal`
--                scope for the server's own operational secrets
--                (zot upstream creds, S3 keys, tier tokens — §12.2).
--                Resolution walks the device's chain deepest-first;
--                `reeve-internal` is never on any device's chain.
--   version    — rotation counter (u64), starts at 1, bumped on every
--                rotate; feeds the per-app manifest `secrets_version`
--                hash (§12.4). Stored plaintext: versions are audit
--                metadata, never values (§12.6).
--   ciphertext — XChaCha20-Poly1305 envelope `nonce(24) || ct+tag`
--                under the D15 external keyfile (REEVE_DATA/
--                secret.key). Fresh random nonce per write, so a
--                rotated row never reuses one. Snapshots therefore
--                ship ciphertext only (§12.2).
--   rotated_at — NULL until the first rotation.
CREATE TABLE secrets (
    name       TEXT NOT NULL,
    scope      TEXT NOT NULL,
    version    INTEGER NOT NULL,
    ciphertext BLOB NOT NULL,
    created_at INTEGER NOT NULL,
    rotated_at INTEGER,
    PRIMARY KEY (name, scope)
);

-- Render change detection for secrets (§12.4): the fold of this
-- device's per-app secrets_version map at its last manifest write.
-- NULL when no rendered app references a secret. A change here with
-- content_digest UNCHANGED is the secrets-only bump: manifestVersion
-- moves, the previous bundle (and its digest) is reused verbatim, so
-- agents re-resolve without a re-pull.
ALTER TABLE device_manifests ADD COLUMN secrets_digest TEXT;
