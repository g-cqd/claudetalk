-- M2: who wrapped each channel key (needed to unwrap: X25519 is pairwise).
-- NULL only for rows written before M2.
ALTER TABLE channel_keys ADD COLUMN wrapper BLOB;

-- Encrypted channels that lost a member outside remove_member (identity revoke)
-- and must get a new key epoch before anyone may post again.
CREATE TABLE channel_rotation (
    channel TEXT PRIMARY KEY REFERENCES channels(id),
    since   INTEGER NOT NULL,
    reason  TEXT NOT NULL
);

-- 1 = bytes live in a content-addressed file under the blob dir and `bytes` is empty.
ALTER TABLE blobs ADD COLUMN spilled INTEGER NOT NULL DEFAULT 0;

-- Per-uploader accounting for the daemon's blob quota.
CREATE TABLE blob_uploads (
    uploader BLOB NOT NULL,
    hash     TEXT NOT NULL REFERENCES blobs(hash),
    size     INTEGER NOT NULL,
    ts       INTEGER NOT NULL,
    PRIMARY KEY (uploader, hash)
);

CREATE INDEX channel_members_pubkey ON channel_members(pubkey);
