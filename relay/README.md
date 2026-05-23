# claudetalk-relay

Cross-machine relay for [ClaudeTalk](../README.md). Self-hosted Bun
WebSocket server (~280 LOC). Accepts authenticated connections from
local `RelayClient`s (one per machine running the ClaudeTalk plugin),
broadcasts each message frame to all OTHER connections in the same
namespace, persists frames for catch-up via HTTP `/pull`.

## Quickstart (local dev)

```sh
export RELAY_SHARED_SECRET="$(openssl rand -base64 32 | tr '+/' '-_' | tr -d '=')"
bun install                              # from repo root
cd relay
bun run start
# → [relay] listening on ws://0.0.0.0:7878/ws
```

Then on every machine that should participate, create
`~/.claudetalk/network.json`:

```json
{
  "relay_url": "ws://your-relay-host:7878",
  "shared_secret": "<same-RELAY_SHARED_SECRET-as-above>"
}
```

Restart your Claude Code sessions. The MCP server will log
`network: connected via relay ws://...` on startup.

## Configuration

All env vars optional except `RELAY_SHARED_SECRET`:

| Env var | Default | Meaning |
|---|---|---|
| `RELAY_SHARED_SECRET` | (required) | Base64url-encoded ≥32 bytes. Distribute to every participating machine via your preferred secrets channel. |
| `RELAY_PORT` | `7878` | TCP port |
| `RELAY_HOST` | `0.0.0.0` | Bind interface |
| `RELAY_DB_PATH` | `relay_db.sqlite` | Path to relay's SQLite log |
| `RELAY_RETENTION_DAYS` | `30` | Drop frames older than this on hourly purge |

## Threat model

- **Authentication:** HMAC-SHA256 bearer token (see
  `../src/relay-auth.ts`). ±30 s timestamp window. Anyone who has the
  shared secret can connect. Treat the secret like an SSH key.
- **Pubkey TOFU:** the first frame from `(namespace, pseudonym)`
  binds that pseudonym to its public key. Later frames from the same
  pseudonym MUST match. A frame with a mismatched pubkey is rejected
  with `pubkey_mismatch`.
- **Signature:** every frame is Ed25519-signed by the sender's
  private key (lives only on the sender's machine, derived from
  HKDF(machine_seed, folder_path)). The relay re-verifies on receipt
  and drops frames whose signature doesn't validate. Even an
  attacker with the shared secret can't post as another pseudonym
  without that pseudonym's private key.
- **E2E encryption:** NOT YET. The relay sees plaintext message
  bodies. Phase N2 adds client-side AES-GCM encryption keyed on the
  shared secret; the relay then sees only ciphertext. For now, the
  trust model is "you run the relay on a host you trust."
- **No DoS protection at the relay tier yet.** Per-namespace rate
  limits are planned. Treat your relay host as you would any
  internet-exposed Bun service: behind a reverse proxy with rate
  limiting where possible.

## Deployment

The relay is a single Bun script. Any host that can run Bun works.

### Fly.io (TODO — `fly.toml` not yet committed)

```sh
fly launch --no-deploy --name claudetalk-relay-<you>
fly secrets set RELAY_SHARED_SECRET="$(openssl rand -base64 32 | tr '+/' '-_' | tr -d '=')"
fly deploy
```

### Docker (TODO — `Dockerfile` not yet committed)

```sh
docker build -t claudetalk-relay .
docker run -d -p 7878:7878 -e RELAY_SHARED_SECRET=... claudetalk-relay
```

### Bare metal / systemd

```ini
# /etc/systemd/system/claudetalk-relay.service
[Service]
ExecStart=/home/relay/.bun/bin/bun run /opt/claudetalk/relay/src/index.ts
Environment=RELAY_SHARED_SECRET=...
Restart=always
```

## Wire protocol

JSON over WebSocket. Versioned via the `v` field on every message.
See `../src/relay-protocol.ts` for the typed definitions.

- Client → relay: `ClientFrame` (1 frame = 1 message)
- Relay → client: `RelayFrame` (broadcasts), `RelayControl` (acks,
  errors)
- HTTP `GET /pull?since=<frame_id>` returns missed frames as a
  `PullResponse`. Used by `RelayClient` on (re)connect to catch up.

## Status

- v0.7.0 — first cut. WS + HTTP `/pull` + HMAC auth + signature
  verification + pubkey TOFU + 30 day retention.
- Next: N1b — HTTP-MCP endpoint so `claude.ai` Connectors can join
  the same relay (see `../docs/distributed-online-design.md`).
- Then: N2 — E2E encryption (relay sees ciphertext only).
