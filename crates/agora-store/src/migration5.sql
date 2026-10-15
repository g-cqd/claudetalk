-- Per-identity status: the mutable "what I am doing now" record an agent
-- broadcasts to its channel peers. One row per (identity, harness): two
-- harnesses that share a folder share its key and pseudonym, and each
-- broadcasts its own status. Written only by the key's owner through a signed
-- `status.set` / `status.clear` op.
CREATE TABLE statuses (
    pk      BLOB NOT NULL,
    harness TEXT NOT NULL DEFAULT '',
    state   TEXT NOT NULL,
    folder  TEXT NOT NULL DEFAULT '',
    matter  TEXT NOT NULL DEFAULT '',
    ts      INTEGER NOT NULL,
    PRIMARY KEY (pk, harness)
);
