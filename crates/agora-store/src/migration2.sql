CREATE TABLE identities (
    pubkey     BLOB PRIMARY KEY,
    label      TEXT NOT NULL DEFAULT '',
    role       TEXT NOT NULL DEFAULT 'worker',
    revoked    INTEGER NOT NULL DEFAULT 0,
    first_seen INTEGER NOT NULL
);
CREATE TABLE nonces (
    actor BLOB NOT NULL,
    nonce TEXT NOT NULL,
    ts    INTEGER NOT NULL,
    PRIMARY KEY (actor, nonce)
);
CREATE INDEX nonces_ts ON nonces(ts);

CREATE TABLE channels (
    id      TEXT PRIMARY KEY,
    kind    TEXT NOT NULL CHECK (kind IN ('dm','group','thread','squad','team')),
    name    TEXT NOT NULL DEFAULT '',
    parent  TEXT REFERENCES channels(id),
    epoch   INTEGER NOT NULL DEFAULT 0,
    created INTEGER NOT NULL
);
CREATE TABLE channel_members (
    channel TEXT NOT NULL REFERENCES channels(id),
    pubkey  BLOB NOT NULL,
    role    TEXT NOT NULL DEFAULT 'member' CHECK (role IN ('admin','member')),
    PRIMARY KEY (channel, pubkey)
);
-- Wrapped channel key per (epoch, member). Presence of rows == channel is encrypted.
CREATE TABLE channel_keys (
    channel TEXT NOT NULL REFERENCES channels(id),
    epoch   INTEGER NOT NULL,
    member  BLOB NOT NULL,
    wrapped BLOB NOT NULL,
    PRIMARY KEY (channel, epoch, member)
);
CREATE TABLE messages (
    id        TEXT PRIMARY KEY,
    seq       INTEGER NOT NULL UNIQUE,
    channel   TEXT NOT NULL REFERENCES channels(id),
    thread_of TEXT REFERENCES messages(id),
    sender    BLOB NOT NULL,
    ts        INTEGER NOT NULL,
    encrypted INTEGER NOT NULL,
    epoch     INTEGER NOT NULL DEFAULT 0,
    body      BLOB NOT NULL,
    sig       BLOB NOT NULL
);
CREATE INDEX messages_channel ON messages(channel, seq);
CREATE INDEX messages_thread ON messages(thread_of, seq);
CREATE TABLE blobs (
    hash  TEXT PRIMARY KEY,
    size  INTEGER NOT NULL,
    bytes BLOB NOT NULL
);
CREATE TABLE message_attachments (
    message TEXT NOT NULL REFERENCES messages(id),
    hash    TEXT NOT NULL REFERENCES blobs(hash),
    name    TEXT NOT NULL,
    mime    TEXT NOT NULL,
    PRIMARY KEY (message, hash)
);
CREATE TABLE reactions (
    kind   TEXT NOT NULL CHECK (kind IN ('message','file','card')),
    target TEXT NOT NULL,
    by     BLOB NOT NULL,
    emoji  TEXT NOT NULL,
    ts     INTEGER NOT NULL,
    sig    BLOB NOT NULL,
    PRIMARY KEY (kind, target, by, emoji)
);
CREATE TABLE sprints (
    id     TEXT PRIMARY KEY,
    name   TEXT NOT NULL,
    goal   TEXT NOT NULL DEFAULT '',
    starts INTEGER NOT NULL,
    ends   INTEGER NOT NULL,
    state  TEXT NOT NULL DEFAULT 'planned' CHECK (state IN ('planned','active','closed'))
);
CREATE TABLE sprint_cards (
    sprint TEXT NOT NULL REFERENCES sprints(id),
    card   TEXT NOT NULL REFERENCES cards(id),
    PRIMARY KEY (sprint, card)
);
CREATE TABLE knowledge (
    id         TEXT PRIMARY KEY,
    author     BLOB NOT NULL,
    ts         INTEGER NOT NULL,
    title      TEXT NOT NULL,
    body       TEXT NOT NULL,
    tags       TEXT NOT NULL DEFAULT '',
    supersedes TEXT REFERENCES knowledge(id),
    sig        BLOB NOT NULL
);
CREATE VIRTUAL TABLE knowledge_fts USING fts5(title, body, tags, content='knowledge', content_rowid='rowid');
CREATE TRIGGER knowledge_ai AFTER INSERT ON knowledge BEGIN
    INSERT INTO knowledge_fts(rowid, title, body, tags) VALUES (new.rowid, new.title, new.body, new.tags);
END;
CREATE TABLE cursors (
    reader  BLOB NOT NULL,
    channel TEXT NOT NULL REFERENCES channels(id),
    acked   INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (reader, channel)
);
