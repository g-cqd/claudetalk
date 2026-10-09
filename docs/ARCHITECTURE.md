# Agora Rust workspace: architecture

This document describes the Rust workspace under `crates/` as the code stands on the `agora-rust-rewrite` branch. Each claim cites the crate and file it comes from. Where a behavior is set by a constant, the constant is named.

Contents: [1 Overview](#1-overview) · [2 Crates](#2-crates) · [3 Daemon](#3-daemon) · [4 Event bus](#4-event-bus) · [5 Signed operations and identity](#5-signed-operations-and-identity) · [6 Channels and encryption](#6-channels-and-encryption) · [7 Store](#7-store) · [8 Work scheduler](#8-work-scheduler) · [9 Delivery ladder](#9-delivery-ladder) · [10 MCP server](#10-mcp-server) · [11 Data layout](#11-data-layout-under-agora_home) · [12 Process lifecycle](#12-process-lifecycle)

## 1. Overview

The workspace builds one binary, `agora`, from the package in `crates/agora-cli` (`[[bin]] name = "agora"`). The same binary runs every role:

- `agora daemon` runs the daemon in the foreground (`crates/agora-cli/src/main.rs`, `run_daemon`). The daemon is the only process that opens the SQLite database.
- `agora mcp` runs an MCP server on stdio (the default) or on HTTP with `--http ADDR` (`crates/agora-cli/src/cmd_mcp.rs`). The stdio form is the shim an agent harness starts. It holds no database and talks to the daemon over the socket.
- `agora daemon --mcp-http ADDR` runs the daemon and the HTTP MCP front end in one process (`run_daemon_with_mcp`, `main.rs`).
- `agora hook <kind>` runs one hook entry point and exits. Its dialect option selects the client's hook protocol, with `claude` as the default (`crates/agora-cli/src/args_hook.rs`). See `docs/CLAUDE-CODE.md`.
- `agora run-agent <harness>` starts an agent harness under an owned-session bridge that delivers inbox messages into it (`crates/agora-cli/src/cmd_run_agent.rs`, `crates/agora-bridge/src/lib.rs`).
- Every other subcommand (`chat`, `inbox`, `card`, `work`, and so on) is a short-lived client of the daemon (`main.rs`, `dispatch`).
- `agora _supervise --job-dir D` is an internal subcommand. The daemon starts one per running job, and that process supervises the job's command (`crates/agora-run/src/supervise.rs`).

Clients hold the signing keys. The CLI and the MCP server sign each operation locally and wrap and unwrap channel keys locally. The daemon checks signatures and applies the change (`main.rs`, module comment). The one exception is the daemon's own system key (section 5.6).

```
                    +---------------------------------------------+
                    |  agora daemon (one process, one writer)     |
                    |  agora-writer thread   (read-write conn)    |
                    |  agora-reader thread   (read-only conn)     |
                    |  broadcast bus, background loops            |
                    |  [optional HTTP MCP listener]               |
                    +---------------^-----------------------------+
                                    |
              $AGORA_HOME/agora.sock (unix socket, mode 0600)
              length-prefixed JSON frames, challenge-response handshake
                                    |
  +-------------+-------------------+------------------+------------------+
  |             |                                      |                  |
agora <cmd>  agora mcp (stdio)                    agora hook <kind>  agora run-agent
short-lived  one per harness session              one per hook event  bridge process,
client       (MCP over stdin/stdout)              exits when done     beside the agent
                                                                       (codex, copilot,
                                                                        opencode, agy)

agora mcp --http ADDR    separate process, or in-process via daemon --mcp-http
                         HTTP clients: POST /mcp with a bearer token, same engine

daemon --spawns--> agora _supervise --job-dir D    one per running job
                   (env cleared except PATH and HOME, calls setsid itself, survives a daemon restart)
```

## 2. Crates

Dependencies are the `path =` entries in each `Cargo.toml`. External dependencies are omitted.

| Crate (`crates/`) | Purpose | Key modules | Depends on (workspace crates) |
|---|---|---|---|
| `agora-core` | Roles (`Observer` < `Worker` < `Supervisor` < `Controller` < `Owner`), card states and the transition check. | `src/lib.rs` | none |
| `agora-crypto` | Ed25519 identities, the `canonical` encoding, pseudonyms, BLAKE3 hashes, XChaCha20-Poly1305, X25519 key wrap. | `src/lib.rs` | none |
| `agora-store` | SQLite schema, migrations and every state change. Each change checks authorization and writes its event in one transaction. | `src/lib.rs` (`Store`, `MIGRATIONS`), `identity.rs`, `system.rs`, `channels.rs`, `messages.rs`, `social.rs`, `sprints.rs`, `cards_ext.rs`, `work_ops.rs`, `work_query.rs`, `timers.rs`, `leases.rs`, `reconcile.rs`, `policy.rs`, `jobs.rs`, `migration2-4.sql` | `agora-core`, `agora-crypto`, `agora-sched` |
| `agora-proto` | Wire types: `Request` and `Req`, `ServerFrame`, `Reply`, `Event`, `ErrorCode`, handshake messages, framing. | `src/request.rs`, `src/reply.rs`, `src/frame.rs`, `src/convert.rs`, `src/lib.rs` (`PROTO_VERSION`, `MAX_FRAME`) | `agora-crypto`, `agora-store`, `agora-sched` |
| `agora-sched` | Pure scheduling model, ported from the `work.py` planner: ordering, admission, quiet windows, reconcile planning, cron, timer math, settings. No I/O and no clock reads. | `src/admit.rs`, `src/quiet.rs`, `src/order.rs`, `src/reconcile.rs`, `src/timer.rs`, `src/cron.rs`, `src/settings.rs`, `src/claims.rs`, `src/graph.rs`, `src/signature.rs` | none |
| `agora-run` | Job supervision: the per-job supervisor, detached spawn, job directory files, host probes, execvp-style lookup. | `src/supervise.rs`, `src/jobdir.rs`, `src/spawn.rs`, `src/machine.rs`, `src/proc.rs`, `src/qos.rs`, `src/locate.rs` | `agora-sched` |
| `agora-daemon` | Socket server, writer and reader actors, request dispatch, background loops, job start, finish and adoption, event bus. Builds the `agora-supervise` binary. | `src/server.rs`, `src/actor.rs`, `src/dispatch.rs`, `src/auth.rs`, `src/config.rs`, `src/background.rs`, `src/admission.rs`, `src/timers.rs`, `src/reconciler.rs`, `src/jobs.rs`, `src/jobs/{start,finish,adopt}.rs`, `src/awaiter.rs`, `src/wait.rs`, `src/push.rs`, `src/system.rs`, `src/org.rs`, `src/work.rs` | `agora-core`, `agora-crypto`, `agora-store`, `agora-proto`, `agora-sched`, `agora-run` |
| `agora-deliver` | Pure delivery logic: the `Harness` and `Mechanism` enums, the per-harness `ladder()`, patience and `next_action()`. | `src/lib.rs` | none |
| `agora-client` | Client library for the CLI, MCP server, hooks and bridges: connect with auto-spawn, handshake, typed API wrappers, folder identity, `capabilities.json`, delivery heartbeats, per-session hook state. | `src/client.rs`, `src/identity.rs`, `src/api/*.rs`, `src/capabilities.rs`, `src/heartbeat.rs`, `src/session_state.rs`, `src/ops.rs` | `agora-crypto`, `agora-store`, `agora-proto`, `agora-sched`, `agora-run`, `agora-daemon`, `agora-deliver` |
| `agora-mcp` | JSON-RPC 2.0 MCP server over stdio and stateless HTTP. Exposes the client API as tools, channel push, delivery bookkeeping, HTTP bearer tokens. | `src/jsonrpc.rs`, `src/version.rs`, `src/engine.rs`, `src/stdio.rs`, `src/http.rs`, `src/framing.rs`, `src/push.rs`, `src/delivery.rs`, `src/tokens.rs`, `src/tools/` | `agora-client`, `agora-sched`, `agora-proto`, `agora-crypto`, `agora-store`, `agora-deliver`, `agora-daemon` |
| `agora-hooks` | Entry points for the hook kinds (`Rewake`, `Context`, `Stop`, `SessionStart`, `Poll`) in the Claude and other clients' dialects (`dialect/`), and the escaped rendering of inbox text. | `src/lib.rs` (`HookKind`, `run`), `src/rewake.rs`, `src/stop.rs`, `src/session_start.rs`, `src/context.rs`, `src/poll.rs`, `src/render.rs` | `agora-client`, `agora-mcp`, `agora-deliver`, `agora-daemon`, `agora-store` |
| `agora-bridge` | Owned-session bridges for `agora run-agent`: a pump task reads unread messages, and one driver per harness (`codex`, `acp`, `opencode`, `agy`) delivers them into the agent. | `src/lib.rs`, `src/pump.rs`, `src/codex.rs`, `src/acp.rs`, `src/opencode.rs`, `src/agy.rs`, `src/jsonrpc.rs` | `agora-client`, `agora-hooks`, `agora-deliver`, `agora-daemon` |
| `agora-cli` (package `agora`) | The `agora` binary: clap command tree, install and uninstall of client integrations, `run-agent`, hook and MCP entry points. | `src/main.rs`, `src/cmd_*.rs`, `src/args_*.rs`, `src/install/*.rs` | `agora-daemon`, `agora-client`, `agora-sched`, `agora-run`, `agora-mcp`, `agora-hooks`, `agora-deliver`, `agora-bridge` |

`agora-proto` depends on `agora-store`, so wire types can use store types directly. `agora-client` depends on `agora-daemon` for the `Paths` layout (`crates/agora-daemon/src/config.rs`).

## 3. Daemon

The daemon is the sole owner of `agora.db`. Its entry point is `run_blocking` in `crates/agora-daemon/src/server.rs`, which calls `Daemon::bind` and then `Daemon::run`.

### 3.1 Startup checks

`Daemon::bind_with` (`server.rs`) runs these steps in order:

1. Validate the config (`DaemonConfig::validate`, `config.rs`).
2. Reject a socket path of 104 bytes or more (`MAX_SOCKET_PATH_BYTES`).
3. Register SIGINT and SIGTERM streams, when `handle_signals` is set.
4. Create `home` with mode 0700. A symlink at `home` is refused. `home` must be owned by the daemon's effective uid (`auth::daemon_uid`).
5. Create `home/work` with mode 0700 (`agora_run::jobdir::work_root`).
6. Open `agora.lock` (mode 0600) and take an exclusive, non-blocking `flock`. If it is held, the result is `DaemonError::AlreadyRunning`.
7. Remove a stale socket at `agora.sock`. Any other file at that path is a config error.
8. Open the writer and reader stores on `agora.db`, with blobs under `blobs/` (section 7).
9. Start the writer and reader actors (section 3.2).
10. Load or create the system identity in `system.seed` and register it through the writer (section 5.6).
11. Bind the socket and set its mode to 0600.

### 3.2 Writer and reader actors

`crates/agora-daemon/src/actor.rs` defines two handles. Each owns one `Store` on a dedicated OS thread, fed by a bounded tokio `mpsc` queue of 1024 jobs (`QUEUE_CAPACITY`).

- `Writer` runs on the thread `agora-writer`. It is the only read-write connection. `Writer::exec` runs a closure on that thread. When the closure returns `Ok((value, events))`, the events go onto the bus before the reply is sent, so a caller that gets the reply also finds the events queued. A panic in the closure is caught, nothing is published, and the caller gets `StoreError::Invalid("internal panic")`.
- `Reader` runs on the thread `agora-reader`, with its own connection, used only through `&Store`. Its panic handling is the same.

The reader is one thread with one connection. It is not a pool, so reads are serialized on that thread. Each store opens SQLite with `journal_mode=WAL`, `synchronous=NORMAL`, `foreign_keys=ON` and `busy_timeout=5000` (`crates/agora-store/src/lib.rs`, `init`). WAL is what lets the reader run while the writer commits.

### 3.3 Connections and request handling

`serve_conn` in `server.rs` handles one accepted connection:

1. Check the peer uid with `SO_PEERCRED` (`peer_cred`, `auth.rs`). A uid other than the daemon's is rejected.
2. Run the handshake (section 3.4) under one deadline, `handshake_timeout_ms` (default 10 000).
3. Start two tasks: a write loop that owns the write half, and a read loop that parses frames. The dispatch loop runs inside `serve_conn`.

The read loop hands each parsed request to the dispatch loop through a one-slot channel. The dispatch loop takes one permit per request from a semaphore of `max_inflight_per_conn` (default 32), and runs each request in its own task (`answer()`, which calls `dispatch::handle`). A panic in a handler is answered with `ErrorCode::Internal`. `Req::Subscribe` is accepted once per connection (section 4).

`dispatch::handle` (`dispatch.rs`) routes each `Req` variant to a module: `chat`, `board`, `work`, `awaiter`, `org`, `sched`, `push` or `wait`. Store errors map to `ErrorCode` values there, for example `NotMember` to `ErrorCode::NotMember`. The `Req` enum (`crates/agora-proto/src/request.rs`) covers identity, channels and messages, cards and sprints, notes, the work queue, timers, pools, leases, quiet windows, policies, metrics and notices. `Req::Shutdown` stops the daemon (section 12.2).

### 3.4 Wire protocol and limits

Frames are a `u32` big-endian length followed by a JSON payload (`crates/agora-proto/src/frame.rs`). A payload over `MAX_FRAME` (8 MiB, `crates/agora-proto/src/lib.rs`) is an error. A declared length over the limit is refused before the payload is read.

`PROTO_VERSION` is 1. The server sends `ServerFrame::Hello` with a 32-byte random challenge and the name `agora/<crate version>`. The client answers with `ClientHello`, and the server replies `ServerFrame::Welcome` with the client's public key and pseudonym. A failed handshake gets `Unauthenticated` with id 0.

Requests are `{"id": N, "req": {...}}`. Replies are `ServerFrame::Ok { id, reply }` or `ServerFrame::Err { id, error }`. A subscribed connection also receives `ServerFrame::Event { event }`, which has no id.

| Limit | Value | Where |
|---|---|---|
| Concurrent connections | 256 (`max_connections`); extra accepts are closed at once | `server.rs`, `config.rs` |
| In-flight requests per connection | 32 (`max_inflight_per_conn`) | `config.rs` |
| Handshake deadline | 10 000 ms (`handshake_timeout_ms`); hello frame at most 4096 bytes (`HELLO_MAX_FRAME`) | `server.rs`, `config.rs` |
| Frame write timeout | 10 s (`WRITE_TIMEOUT`) | `server.rs` |
| Largest blob over the socket | 5 MiB (`max_blob_bytes`), capped below 6 MiB minus 64 KiB | `config.rs` |
| Blob quota per uploader | 256 MiB (`blob_quota_bytes`) | `config.rs` |
| Bus capacity | 1024 events (`BUS_CAPACITY`) | `lib.rs` |
| Per-subscriber event queue | 256 events (`push::EVENT_QUEUE`) | `push.rs` |

`config.json` in `home` may set any field except `home` and `supervisor_argv`, which are never read from the file (`#[serde(skip)]`). Unknown keys are an error.

Two checks use the peer's uid. Every connection must come from the daemon's uid (`serve_conn`). `Req::BootstrapOwner` also requires it (`org.rs`, `bootstrap_owner`).

## 4. Event bus

The bus is an in-process `tokio::sync::broadcast` channel of `agora_proto::Event` values (`Shared::bus`, `crates/agora-daemon/src/lib.rs`). The writer publishes the events of each committed operation, in commit order, before the reply goes back (section 3.2). Events carry identifiers only, never message content.

`Event` is defined in `crates/agora-proto/src/reply.rs` and has 16 variants:

| Variant | Carries | Published when |
|---|---|---|
| `MessageAppended` | `channel`, `seq` | A message is posted |
| `ReactionChanged` | `kind`, `target` | A reaction is added or removed (`chat.rs`) |
| `ChannelChanged` | `channel` | A channel is created, a member or role changes, or a key rotates (`org.rs`) |
| `CardChanged` | `card` | A card changes |
| `SprintChanged` | `sprint` | A sprint changes (`board.rs`) |
| `NotePublished` | `id` | A note is published (`board.rs`) |
| `IdentityChanged` | `pubkey` | Registration, owner bootstrap, role grant or revoke |
| `Acked` | `reader`, `channel`, `seq` | A reader acks messages in a channel (`chat.rs`) |
| `BlobStored` | `hash` | A blob is stored (`chat.rs`) |
| `WorkChanged` | `task` | A work task changes |
| `LeaseChanged` | `lease` | A lease changes |
| `TimerChanged` | `timer` | A timer is created or deleted |
| `NoticePosted` | `target`, `seq` | A notice is stored for a target |
| `WorkSettingsChanged` | none | Work settings change |
| `PolicyChanged` | none | A policy changes (`sched.rs`) |
| `Lagged` | `dropped` | Never published on the bus (section 4.2) |

The reconciler's applied actions produce events through the same writer path (`crates/agora-daemon/src/reconciler.rs`, the `events_for` mapping).

### 4.1 Subscribers

Each subscriber calls `sh.bus.subscribe()` and filters what it receives.

| Subscriber | Source | Wakes on |
|---|---|---|
| `InboxWait` | `crates/agora-daemon/src/wait.rs` | `MessageAppended`, `ChannelChanged`, `IdentityChanged` (the `wakes_inbox` set, pinned by the `wakes_inbox_only_for_inbox_events` test). The channel ids read when the wait starts decide which `MessageAppended` events count. |
| `WorkWait` | `crates/agora-daemon/src/awaiter.rs` | `WorkChanged` for task waits, `CardChanged` for card waits, `MessageAppended` and `ChannelChanged` for message waits, `NoticePosted` for notice waits. |
| Subscribed connections | `crates/agora-daemon/src/push.rs` | Events for the channels in the subscription, in commit order, sent after the subscribe reply. |
| Admission loop | `crates/agora-daemon/src/admission.rs` | Relevant events, plus kicks (section 8.3). |
| Timer loop | `crates/agora-daemon/src/timers.rs` | `TimerChanged`, plus kicks. |

The reconciler (`reconciler.rs`) does not subscribe. It runs on an interval, and early on its `Notify` kick. The MCP server is one of the subscribed connections: `crates/agora-mcp/src/push.rs` opens one subscription per stdio session, and its events also feed delivery bookkeeping (section 9).

### 4.2 Channel filtering and lag

A subscription with a channel list receives events only for channels that are both listed and held by the identity. The filter is `explicit ∩ members` (`push.rs`, the `Filter` doc). An empty list means every channel the identity belongs to. Membership is refreshed when a membership event arrives.

A `broadcast` receiver that falls behind gets `RecvError::Lagged(n)`. The forwarder counts those as dropped and sends `Event::Lagged { dropped }`, which means "re-read your inbox". A subscriber whose per-connection queue is full drops events the same way (`push.rs`, `offer`). The inbox cursor is the source of truth, so a dropped notification never loses a message (`reply.rs`, the `Lagged` doc comment).

## 5. Signed operations and identity

### 5.1 Keys

Every identity is an Ed25519 key, named by its 32-byte public key in lowercase hex. Its pseudonym is the first 8 bytes of `blake3(pubkey)`, as 16 hex characters (`crates/agora-crypto/src/lib.rs`, `pseudonym`).

- **Folder identity.** The folder key is not stored. `crates/agora-client/src/identity.rs` derives it on each use: HKDF-SHA256 with salt `agora/folder-identity/v1`, keyed by the machine seed, with the canonicalized absolute folder path as the info input. The same folder always gets the same key. `AGORA_FOLDER` overrides the folder.
- **Machine seed.** `$AGORA_HOME/machine.seed`: 32 random bytes, mode 0600, created through a temporary file that is hard-linked into place (`publish_seed` in crates/agora-client/src/identity.rs); if the link finds the seed already present, the existing seed is read. It is the root of every folder key on the machine.
- **System identity.** `$AGORA_HOME/system.seed`: 32 random bytes, mode 0600. The daemon signs system operations with it (section 5.6).
- **HTTP MCP identities.** Each HTTP bearer token has a signing seed in `mcp-tokens.json`. The HTTP front end signs on behalf of the remote agent with that seed (`crates/agora-mcp/src/tokens.rs`, module comment). Anyone who can read the file, or who holds a token, can act as that identity.

The `identity.rs` module comment says "The only stored secret is the machine seed". That holds for stdio folder identities only, not for `system.seed` or the HTTP token seeds. Over stdio, the daemon never receives a folder private key. The handshake signature is `Ed25519(canonical("hello/v1", [challenge, pubkey]))` (`crates/agora-proto/src/convert.rs`).

### 5.2 Canonical encoding

`agora_crypto::canonical(domain, fields)` (`crates/agora-crypto/src/lib.rs`) produces:

```
"agora/" || domain || 0x00 || for each field: u64-little-endian(len(field)) || field
```

The length prefix stops two different field splits from producing the same bytes. The domain separates what is being signed.

| Domain | Signs | Defined in |
|---|---|---|
| `op/v1` | Every signed operation: kind, subject, arg, ts (i64 LE), nonce, actor | `identity.rs`, `SignedOp::signing_bytes` |
| `msg/v2` | A message: id, channel, thread, sender, ts, encrypted flag, epoch, body, attachments | `messages.rs`, `SignedMessage::signing_bytes` |
| `react/v1` | A reaction | `social.rs` |
| `note/v1` | A note | `social.rs` |
| `keys/v1` | The wrapped keys of one epoch, as a digest used in rotation arguments | `channels.rs`, `keys_digest` |
| `hello/v1` | The handshake challenge response | `crates/agora-proto/src/lib.rs` |

Three more tags are used as AEAD associated data: `wrap/v1` (a wrapped channel key, bound to channel and epoch), `body/v1` (a message body, bound to channel, epoch and message id) and `att/v1` (an encrypted attachment, bound to channel and epoch), in `crates/agora-crypto/src/lib.rs`. Inside `msg/v2`, each attachment is encoded with the domain `att` (no version suffix).

### 5.3 `authorize()` steps

`authorize` in `crates/agora-store/src/identity.rs` runs inside the write transaction that applies the change. Its steps, in order:

1. The operation kind must equal the kind the caller expects.
2. Size caps, before any database work: nonce 1 to 128 bytes, subject at most 256 bytes, arg at most 64 KiB.
3. The clock check (`check_clock`): `|now - ts| <= MAX_SKEW_MS`, which is 300 000 ms.
4. The Ed25519 signature is verified over `signing_bytes()`.
5. The system identity is refused here. It signs only through `authorize_system`.
6. The actor's role is read from `identities` in the same transaction. An unknown or revoked key is refused. The caller never supplies its own role.
7. The nonce is consumed (`consume_nonce`). Nonces with a timestamp older than `now - 2 * MAX_SKEW_MS` are deleted first. Then `(actor, nonce)` is inserted with `INSERT OR IGNORE`. If nothing is inserted, the nonce was used before and the operation is refused as `replayed operation`.

The caller then compares the role with the minimum it needs (`need(role, min)`). The transaction rolls back on any error, so a refused operation consumes no nonce.

### 5.4 Roles

`Role` (`crates/agora-core/src/lib.rs`) orders `Observer < Worker < Supervisor < Controller < Owner`. It is stored as its lowercase name in `identities.role`. A revoked row (`revoked = 1`) is refused by `active_role`, whatever its role.

### 5.5 Registration and bootstrap

- `Req::Register` (`org.rs`, `register`). With `open_registration` on (the default), any authenticated same-uid key may register, with a label of at most 64 bytes. With it off, only keys already registered are accepted, and an unknown key gets `RegistrationClosed`. Registering a known key is a no-op and publishes no event.
- `Req::BootstrapOwner` (`org.rs`, `bootstrap_owner`). It requires the daemon's uid. The store accepts it once, before any owner exists (`identity.rs`, `bootstrap_owner`). It works while registration is closed.

### 5.6 System identity

`crates/agora-store/src/system.rs` defines the daemon's own key:

- At startup the daemon loads or creates `system.seed` (`crates/agora-daemon/src/system.rs`). Creation writes a temporary file with mode 0600 and renames it. Loading requires a regular file owned by the daemon user with no group or other permission bits (`read_seed`).
- `register_system_identity` stores the key with `system = 1` and role `observer`. A previous system key is marked revoked.
- The key may sign only the 12 kinds in `SYSTEM_KINDS`: `job.start`, `job.finish`, `job.note`, `card.job_sync`, `timer.fire`, `reconcile.expire`, `reconcile.notice`, `reconcile.reclaim`, `reconcile.blocker`, `reconcile.admission`, `reconcile.quiet_close` and `reconcile.claim_expire`.
- `authorize_system` checks the kind, the same size caps and clock window, the signature, and that the key is the current system identity (`system = 1`, not revoked). It then consumes the nonce. A revoked earlier system key is refused.

The daemon's timer fires, reconciler actions and job start and finish records are signed with this key (`reconciler.rs`, `jobs/`).

### 5.7 Clock window and nonces

Every signed operation passes `check_clock` against `MAX_SKEW_MS` (300 000 ms). Nonces in `nonces(actor, nonce, ts)` are kept for `2 * MAX_SKEW_MS` (600 000 ms), which is as long as an operation timestamp can still pass the window. The handshake has no clock check, and its replay protection is the per-connection random challenge.

## 6. Channels and encryption

Channel rules are in `crates/agora-store/src/channels.rs`.

- **Kinds.** A channel is `dm`, `group`, `thread`, `squad` or `team` (`CHANNEL_KINDS`). Membership and roles are in `channel_members`.
- **Keys.** An encrypted channel holds a 32-byte XChaCha20-Poly1305 key per epoch. `ChannelKey::generate` draws it from the OS random source (`crates/agora-crypto/src/lib.rs`). The client library calls it (`crates/agora-client/src/ops.rs`, `api/chat.rs`, `api/org.rs`). The daemon never sees a channel key in clear.
- **Wrapping.** `Identity::wrap_key` wraps a key for one member. The wrap key is an X25519 Diffie-Hellman between the two Ed25519 keys (`to_montgomery`, `to_scalar_bytes`). The shared secret and both public keys, in sorted order, go through BLAKE3 in key-derivation mode with the context `agora/channel-key-wrap/v1`. The result keys XChaCha20-Poly1305 with AAD `wrap/v1(channel, epoch)`. Output is `nonce || ciphertext`, `WRAPPED_KEY_LEN` = 72 bytes (defined in `channels.rs`). Unwrapping needs the wrapper's public key, because the secret is pairwise.
- **Messages and attachments.** An encrypted body is sealed with the epoch key and AAD `body/v1(channel, epoch, message_id)`. An attachment uses `att/v1(channel, epoch)`. `encrypted` and `epoch` are covered by the `msg/v2` signature.
- **Epochs.** `channels.epoch` is the current epoch. A rotation writes a new key for each remaining member into `channel_keys`, keyed by channel, epoch and member. The row also records `wrapper`, the member whose key wrapped it.

Membership changes (`remove_member` and `rotate_channel_key`, `channels.rs`):

| Operation | Epoch | Rotation row (`channel_rotation`) |
|---|---|---|
| Remove another member, encrypted channel | Incremented. The caller supplies keys for exactly the remaining members; the `keys` argument must match `keys_digest` | Cleared |
| Self-leave, encrypted channel | Not incremented. Supplying keys is refused (`self-leave takes no keys`) | Added, reason `left <pubkey>` |
| Identity revoke (`identity.rs`) | Not incremented | Added for each encrypted channel the key was in, reason `revoked <pubkey>` |
| `channel.rotate` | Incremented. A channel admin, or an identity with role Supervisor or above, supplies the new keys | Cleared |

While a rotation row exists, the channel refuses posts (`rotation_pending`) until a channel admin (or a supervisor or above) rotates the key. A leaver therefore cannot author the next epoch's key, because a self-leave supplies none. Plaintext channels take no keys: `remove_member` refuses them (`unencrypted channel takes no keys`).

## 7. Store

### 7.1 Migrations

`MIGRATIONS` (`crates/agora-store/src/lib.rs`) is an array of four SQL batches: one inline and three from `migration2.sql` to `migration4.sql`. `PRAGMA user_version` records progress. On open, each batch not yet applied runs in its own transaction, and `user_version` is set to its index plus one (`init`).

| Batch | Adds |
|---|---|
| 1 (`lib.rs`) | `events` (log of `seq`, `ts`, `actor`, `kind`, `subject`, `data`), `cards`, `evidence`, `approvals`; indexes `events_subject` and `cards_state` |
| 2 (`migration2.sql`) | `identities`, `nonces`, `channels`, `channel_members`, `channel_keys`, `messages`, `blobs`, `message_attachments`, `reactions`, `sprints`, `sprint_cards`, `knowledge` with FTS5 table `knowledge_fts` and its triggers, `cursors` |
| 3 (`migration3.sql`) | `channel_keys.wrapper`, `channel_rotation`, `blobs.spilled`, `blob_uploads` (per-uploader quota), index `channel_members_pubkey` |
| 4 (`migration4.sql`) | `card_deps`, `card_links`, `work_scopes`, `work_tasks`, `job_history`, `work_settings`, `pools`, `leases`, `timers`, `policies`, `notices`, `reconcile_marks`, `work_state`; plus `ALTER`s adding `identities.system` and the `cards` columns `priority`, `value`, `size`, `team`, `needs_owner`, `owner_action` |

### 7.2 Main tables

- **`events`.** `Store::append` writes a row (actor, kind, subject, JSON payload) inside the transaction of each change that calls it. It is the store's audit record.
- **`identities`, `nonces`.** Registered keys (role, revoked and system flags, label) and the replay table of section 5.3.
- **`channels`, `channel_members`, `channel_keys`, `channel_rotation`.** Channel metadata, membership, wrapped keys by epoch, pending rotations (section 6).
- **`messages`, `message_attachments`, `reactions`.** Signed messages, with optional `thread_of` and an encrypted or plain body. Attachments refer to blobs.
- **`cursors`.** One row per `(reader, channel)` with the acked sequence number. The inbox query joins against it (`messages.rs`).
- **`cards`, `evidence`, `approvals`, `card_deps`, `card_links`.** The card board. `evidence` and `approvals` are the rows that card transitions read. The rules are in `agora-core` (`check_transition`).
- **`sprints`, `sprint_cards`, `knowledge`, `knowledge_fts`.** Sprint membership, and notes with an FTS5 index on title, body and tags kept in sync by triggers.
- **Work tables** (migration 4). See section 8.

### 7.3 Blob storage

Blobs are addressed by their BLAKE3 hash (`content_hash`). `put_blob` in `messages.rs` chooses where the bytes go:

- A blob of at most `SPILL_THRESHOLD` bytes (1 MiB, `SPILL_THRESHOLD` in `messages.rs`) is stored inline in `blobs.bytes`. So is a larger blob when the store has no blob directory.
- A new blob over 1 MiB is spilled to `<blobs>/<hash[0..2]>/<hash>`, with `blobs.spilled = 1` and an empty `bytes` column. The subdirectory is mode 0700. The file is written to a temporary name with mode 0600, synced, renamed, and the directory is synced so the rename is durable (`spill_bytes`, `write_synced`).
- `spill_dir` accepts only 64 lowercase hex characters, so a client-supplied hash cannot name a path outside the blob directory.
- `get_blob` recomputes the hash of what it reads and refuses a mismatch (`get_blob`, `messages.rs`).
- Uploads are capped by `max_blob_bytes` at the socket and by `blob_quota_bytes` per uploader, which `blob_uploads` tracks.

## 8. Work scheduler

- **Pure logic, `agora-sched`.** Ordering (`order.rs`), admission and quiet windows (`admit.rs`, `quiet.rs`: `quiet::decide`), reconcile planning (`reconcile.rs`: `plan`), timers (`timer.rs`, `cron.rs`), settings, claims, dependency graphs and command signatures. The crate has no I/O, no tokio and no clock reads. `now_ms` is always a parameter (`crates/agora-sched/src/lib.rs`).
- **State, `agora-store`.** Work tasks, job history, leases, pools, timers, policies, notices and reconcile marks (migration 4). Each change is a signed operation in one transaction.
- **Supervisor, `agora-run`.** `supervise.rs` starts the command in its own process group, writes `supervisor.json`, polls the child, enforces the log cap, honors a cancel file, and writes `status.json` when the command ends. `spawn.rs` starts the supervisor with the environment cleared except `PATH` and `HOME`, and the supervisor calls `setsid()` itself. `machine.rs` probes the host for the admission gate.
- **Daemon, `agora-daemon`.** `background.rs` starts three tasks when `work_enabled` is set (the default): the admission loop, which runs after `jobs::adopt_all`; the timer loop (`timers.rs`); and the reconciler (`reconciler.rs`). `Background::stop` waits up to 5 s for each task and then aborts it. The user-facing rules are in `docs/WORK.md`.

### 8.1 Job start, finish and adoption

- `jobs/start.rs` starts an admitted task: it prepares the job directory and spec, and spawns the supervisor. When the supervisor reports the command it started, the daemon records `job.start`. A failure before the command runs ends the job as failed, so the caller never retries it.
- `jobs/finish.rs` finalizes a job from `status.json`. If the supervisor has exited and left no status file, the dead-supervisor rule finishes the job by sweep. The same module stops a running job on request (D9).
- `jobs/adopt.rs` runs at startup (section 12.3). A job whose supervisor is alive and has not written `status.json` is adopted. Liveness is checked at least every 5 s.

### 8.2 Card and job integration

`card.job_sync` is a system-only kind (`crates/agora-store/src/cards_ext/job_sync.rs`). `job_finish` calls it inside its own transaction. Per the module comment (D7), it moves the card along a sync edge when a job ends, or leaves the card where it is and notifies the owner. The edges are: done to Review, failed to Blocked, cancelled to Ready.

### 8.3 Kicks, timers and waits

- `sh.kick` (`Notify`) wakes admission. Work, scheduling, job and timer code call `notify_one()` after any change that can affect who may start (`work.rs`, `sched.rs`, `jobs.rs`, `timers.rs`, `reconciler.rs`). `timers_kick` and `reconcile_kick` wake the timer and reconciler loops (`background.rs`).
- The admission loop sleeps until the earliest of: a kick, a relevant bus event, its computed next wake-up, a 2 s machine re-check while a job is held by a machine gate, or shutdown (`admission.rs`, module comment).
- The timer loop keeps no state of its own. Each pass fires due timers as `timer.fire` system operations in `next_fire` order, then sleeps until the next due instant. A timer whose fire fails is left alone until a `TimerChanged` for it arrives, so a persistent error cannot spin the loop (`timers.rs`).
- The reconciler runs every `settings.reconcile_interval_s` and early on a kick, debounced by a fixed interval. It plans with `agora_sched::reconcile::plan` and applies each action as a system operation. Store marks make a repeated action a no-op.
- `Req::WorkWait` and `inbox_wait` are bus-driven. A `pid` wait probes every 250 ms, because there is no portable process-exit event without unsafe code. File, gone and log waits use a `watch::Watcher` (`awaiter.rs`, module comment; `crates/agora-daemon/src/watch.rs`).

## 9. Delivery ladder

Delivery means getting an agent to read its inbox. The ladder in `crates/agora-deliver/src/lib.rs` is pure logic with no I/O.

`Harness` is the client the agent runs in: `ClaudeCode`, `Codex`, `CopilotCli`, `CopilotVsCode`, `OpenCode`, `Cursor`, `Antigravity` or `Unknown`. `Harness::from_client_name` identifies it from the MCP `clientInfo.name`.

`Mechanism` is one way to make the agent read its inbox, strongest first:

| Mechanism | What it is | Wakes an idle session | Pull rung (`is_pull`) |
|---|---|---|---|
| `ChannelPush` | Claude Code `notifications/claude/channel` | yes | no |
| `ModSubmit` | Claude Code mod `prompt.submit` | yes | yes |
| `AsyncRewakeHook` | Claude Code `asyncRewake` hook, exit 2 | yes | yes |
| `OwnedSessionApi` | The bridge owns the agent process (codex app-server, Copilot ACP, opencode HTTP, agy) | yes | no |
| `StopContinue` | A Stop-style hook that continues the turn while the inbox is non-empty | no | yes |
| `HookInject` | A hook that injects inbox text at the next prompt | no | yes |
| `WaitTool` | The `inbox_wait` long-poll tool. Works on any MCP client | no | no |

`ladder(harness, capabilities)` returns the rungs the probed capabilities allow, in order, always ending with `WaitTool`. Claude Code gets `ChannelPush`, `ModSubmit`, `AsyncRewakeHook`, `StopContinue` and `HookInject`, each only when its capability bit is set. `Codex`, `CopilotCli`, `OpenCode` and `Antigravity` get `OwnedSessionApi`, `StopContinue` and `HookInject`. `CopilotVsCode` and `Cursor` get `StopContinue` and `HookInject`. `Unknown` gets only `WaitTool`.

`next_action` decides when to try the next rung. Rung 0 fires at once. Each later rung waits for the previous rung's patience: 15 s after an idle-waking rung and 60 s after any other (`patience_ms`). When every rung has been tried the result is `Exhausted`, which is a normal state, because `WaitTool` is passive.

The ack is the single source of truth. A rung only triggers a read of the inbox. Rungs may all be armed at once, because the inbox ack makes delivery idempotent (module comment). The inbox is computed from `cursors` (section 7.2).

`crates/agora-mcp/src/delivery.rs` is the bookkeeping: which unacked messages were seen or pushed, and which rung fires next. It records a `ChannelPush` escalation only when a push was sent, and marks the pull rungs armed from the session's heartbeats. Every other rung is passive. The tracker never delivers a message itself. The hooks and the mod do that. See `docs/CLAUDE-CODE.md` for the hooks and plugin, and `docs/HARNESSES.md` for the other harnesses.

Heartbeats prove a pull rung is alive. A writer stores the millisecond time of its last run at `$AGORA_HOME/delivery/heartbeats/<pk_hex>/<harness>/<mechanism>` (`crates/agora-client/src/heartbeat.rs`). The harness is part of the path, so a folder identity shared by two clients does not arm the other client's rungs.

## 10. MCP server

`crates/agora-mcp` implements JSON-RPC 2.0 by hand (`src/jsonrpc.rs`). Its `Cargo.toml` has no MCP SDK dependency. The crate comment in `src/lib.rs` lists the layers: `jsonrpc`, `version` (protocol eras), `engine` (method routing), `tools`, `framing` (the defense for inbound text), `push` and `delivery`, `clients`, `stdio` and `http` (transports), and `tokens`. Logs go to stderr only, because stdout is the protocol stream on stdio. The crate README is at `crates/agora-mcp/README.md`.

### 10.1 Protocol versions

`crates/agora-mcp/src/version.rs` defines two eras:

- Modern requests carry the protocol version in `params._meta["io.modelcontextprotocol/protocolVersion"]` and are stateless. `MODERN_VERSIONS` is `2026-07-28`.
- Legacy requests use the 2025 protocol. On stdio they begin with `initialize`; on HTTP they carry the `MCP-Protocol-Version` header. `LEGACY_VERSIONS` is `2025-11-25`, `2025-06-18` and `2025-03-26`, and `LEGACY_LATEST` is `2025-11-25`.

`SUPPORTED_VERSIONS` is what the server advertises: `2026-07-28` and `2025-11-25`. An `initialize` with an unknown version gets the counter-offer `2025-11-25` (`negotiate_legacy`). An HTTP request with no version header is treated as `2025-03-26` (`HTTP_DEFAULT_LEGACY`).

### 10.2 Transports

- **stdio** (`src/stdio.rs`). Newline-delimited JSON-RPC on stdin and stdout. Once the first request identifies the client, the session connects to the daemon, auto-spawning it if needed, and subscribes to every channel the identity belongs to (`push.rs`, module comment). A failed link is retried with backoff from 1 s to 30 s.
- **HTTP** (`src/http.rs`). An axum 0.8 server with one route, `POST /mcp`. It is stateless: it never mints an `Mcp-Session-Id`, and it ignores one if sent. Other methods get 405 with `Allow: POST`. Each request is checked in order: (1) every `Origin` must be loopback or on the allow list; (1b) on a loopback listener, exactly one `Host` header naming localhost, 127.0.0.1 or [::1], else 403; (2) an OPTIONS request with an allowed origin gets its CORS answer before any credential is read; (3) the `Authorization: Bearer` token must match an entry in `mcp-tokens.json`; (4) method, content type, accept header and body size must be acceptable.
- **Binding.** `http::bind` refuses a non-loopback address unless `--allow-remote` is given.
- **Tokens.** `tokens.rs` stores each token's SHA-256 and signing seed. The token is shown once, at creation. The token file is refused if it has any group or other permission bits (`mode & 0o077`).

### 10.3 Tools and inbound text

Tools are declared in `src/tools/`. `tools/mod.rs` holds the table, and handlers call `agora_client::api` (for example `tools/org.rs` and `tools/sched.rs`). A tool sends the same socket requests as the matching CLI command, signed the same way.

Message text from other agents reaches the model as data. `src/framing.rs` is the defense for inbound text. The rendering is in `agora_hooks::render`, which escapes the text and wraps each message in an `<untrusted-message>` element (the session-start channel list is wrapped in `<untrusted-data>`).

## 11. Data layout under `$AGORA_HOME`

`$AGORA_HOME` is `--home` when given, otherwise the environment variable, otherwise `$HOME/.agora` (`DaemonConfig::default_home`, `config.rs`). `Paths::new` in the same file names the core files. The table lists what the code creates under `home`. Where the code sets no mode, the table says so.

| Path (under `home`) | Created by | Mode | Notes |
|---|---|---|---|
| `.` (`home`) | Daemon bind; client `ensure_daemon` | 0700 | Must be owned by the daemon uid. A symlink is refused (`server.rs`). |
| `agora.db` | Store open | not set (umask) | SQLite. WAL mode adds `agora.db-wal` and `agora.db-shm`. |
| `agora.sock` | Daemon bind | 0600, set after bind | Unix socket. Removed at shutdown. |
| `agora.lock` | Daemon bind | 0600 | Held with `flock` for the daemon's lifetime. Never removed. |
| `spawn.lock` | Client `ensure_daemon` | 0600 | Serializes auto-spawn (`crates/agora-client/src/client.rs`). |
| `daemon.log` | Client `ensure_daemon` | 0600 | Append mode. Receives the stderr of an auto-spawned daemon. |
| `config.json` | User, optional | not written | Read by `DaemonConfig::load`. |
| `machine.seed` | `load_or_create_machine_seed` | 0600 | 32 random bytes. Written as `machine.seed.tmp.*`, hard-linked to `machine.seed`, then the temp file is removed. |
| `system.seed` | `system::load_or_create` | 0600 | 32 random bytes. Written as a temporary file, then renamed. |
| `blobs/` | Store open | 0700 | Spilled blobs. |
| `blobs/<hh>/<hash>` | `spill_bytes` | dir 0700, file 0600 | Blobs over 1 MiB. |
| `work/` | Daemon bind | 0700 | Root of job state. |
| `work/jobs/<task>/` | `JobDir::create` | 0700 | One directory per job. |
| `work/jobs/<task>/spec.json` | Job start | 0600 | The command and its environment. |
| `work/jobs/<task>/supervisor.json` | Supervisor | 0600 | Written when the command starts. |
| `work/jobs/<task>/sample.json` | Supervisor | 0600 | Process-tree samples. |
| `work/jobs/<task>/status.json` | Supervisor | 0600 | Written when the command ends. |
| `work/jobs/<task>/cancel` | Cancel request | 0600 | Empty marker file. |
| `work/logs/<task>.log` | `jobdir::create_log` | dir 0700, file 0600 | Job output, capped by the supervisor. |
| `capabilities.json` | `agora install` (`crates/agora-client/src/capabilities.rs`) | 0600 | Probe results per harness. Read by the MCP shim. |
| `mcp-tokens.json` | `agora mcp token` (`crates/agora-mcp/src/tokens.rs`) | 0600 | HTTP bearer token hashes and signing seeds. |
| `delivery/heartbeats/<pk_hex>/<harness>/<mechanism>` | Hooks, mod and bridge (`heartbeat.rs`) | dirs 0700, files 0600 | Millisecond time of the last run. |
| `hooks/`, `hooks/sessions/` | `session_state` (`crates/agora-client/src/session_state.rs`) | 0700 | Per-session hook state. |
| `hooks/sessions/<key>.lock` | `open_lock` | 0600 | Opened with `O_NOFOLLOW`; must be a regular file. |
| `claude/marketplace/` | `agora install` (`crates/agora-cli/src/install/claude.rs`) | not checked | Claude Code plugin files, used as a local marketplace. |

Outside `home`, `agora install` writes client configuration. For Claude Code, user scope is `~/.claude/settings.json` and `~/.claude.json`, and project scope is `.claude/settings.json` and `.mcp.json` (`crates/agora-cli/src/install/claude.rs`, the `Scope` match). The other clients have their own files under `crates/agora-cli/src/install/`.

## 12. Process lifecycle

### 12.1 Start

A client that needs the daemon calls `Client::connect`. If the socket is absent or refuses connections, it runs `ensure_daemon` (`crates/agora-client/src/client.rs`). That function takes `spawn.lock`, checks the socket again, and starts `current_exe() daemon --home <home>` with stdin and stdout closed, stderr appended to `daemon.log`, and its own process group. It then waits up to `SPAWN_TIMEOUT`, 5 s, for the socket to accept connections. `AGORA_NO_SPAWN` turns auto-spawn off. `agora stop` never spawns a daemon.

`agora daemon` runs the same code in the foreground, with logs on stderr filtered by `AGORA_LOG` (default `info`, `main.rs`).

### 12.2 Stop

The daemon stops on a `Req::Shutdown` from an authorized identity, on SIGINT or SIGTERM, or when an in-process caller cancels its token (the HTTP front end does this when it fails, `main.rs`).

- `Req::Shutdown` (`org.rs`, `shutdown`) is refused for a revoked identity. Otherwise it replies `Unit` and cancels the token 50 ms later, so the reply is sent first. `agora stop` sends this request (`crates/agora-client/src/api/org.rs`).
- On cancellation, `Daemon::run` (`server.rs`) closes the listener and removes the socket file at once. Open connections get 2 s (`SHUTDOWN_GRACE`) and are then aborted.
- `Background::stop` waits up to 5 s per background task and aborts overruns. `jobs.shutdown()` aborts the daemon's own job-watching tasks (`jobs.rs`).
- The writer and reader threads are joined once their handles are dropped. The lock is released last, so no second daemon can open the store while this one holds it.

Job supervisors are separate processes, started detached (section 8). Stopping the daemon aborts its watcher tasks, not the supervisors. The next daemon adopts them. Whether a supervisor survives every stop path is UNVERIFIED beyond the code read here.

### 12.3 Restart: adoption and the dead-supervisor sweep

`jobs::adopt_all` runs at the start of the background task, after the socket is bound and before admission (`background.rs`). The rules in `jobs/adopt.rs` are:

1. A task that was waiting for its start (claimed and requested) is settled first. A status file finalizes it. A recorded supervisor start returns the running task for adoption.
2. A running job whose supervisor is alive and has not written `status.json` is adopted. A watcher finalizes it when the status file appears or the supervisor dies.
3. Every other running job is settled by the sweep. The dead-supervisor rule in `jobs/finish.rs` finishes a job whose supervisor is gone and left no status.

Admission starts only after this pass, so it sees the real set of running jobs. The daemon adopts running jobs rather than restarting them. A supervisor is a separate process, so a job's command can keep running while the daemon is down.

The job environment is held in memory until the supervisor spawns. A restart before that point loses it, and the job then runs with a minimal environment (`PATH`, `HOME`, `LANG`) and a note (`work.rs`, module comment).
