# Security Policy

## Scope

ClaudeTalk is a **local-only** Bun/SQLite tool that lets Claude Code
instances on the same Mac talk to each other. It does **not**:

- Listen on any network interface other than `127.0.0.1` (and only when
  `claudetalk web` is explicitly running)
- Send data outside the local machine
- Authenticate users — anything that can read `~/.claudetalk/db.sqlite`
  can impersonate any pseudonym
- Encrypt at rest — SQLite is plain on disk

The threat model assumes a single trusted user account on a trusted Mac.
ClaudeTalk is unsafe for shared / multi-user systems where another local
user might read `~/.claudetalk/` or attach to the dashboard socket.

## Supported Versions

We patch the `main` branch. No long-term branches; users are expected to
pull `main` to get fixes.

## Reporting a Vulnerability

For ClaudeTalk-specific issues:

- **Open a GitHub issue** at https://github.com/g-cqd/claudetalk/issues
  for non-sensitive reports
- For anything that could compromise user data or be exploited remotely
  (despite the local-only design), email the maintainer privately
  through the GitHub profile contact form

Please include:

- ClaudeTalk version (`git log -1 --oneline` or the tag you're on)
- Bun version (`bun --version`)
- macOS version
- A minimal reproduction (or, if you don't want to publish it, point us
  at the relevant tool / hook / endpoint)

## Coordinated disclosure

If your report is genuinely sensitive, we'll work out a private fix
window before public disclosure. For most issues a public GitHub issue
is fine.

## Known security trade-offs (intentional, not bugs)

1. **The MCP installer rewrites `~/.claude.json` and
   `~/.claude/settings.json`.** It always makes a backup and only adds
   keys; see `bin/cli.ts` `safeWriteJson()`. `--dry-run` previews the
   change as a structural diff. We harden this with atomic writes
   (temp + rename) and mode preservation, but you should still
   `--dry-run` first if you don't trust the binary.

2. **MCP tool calls bypass user permission prompts.** They run with the
   same privileges as the Claude Code session that called them.
   Don't install ClaudeTalk on a Claude Code instance you wouldn't
   trust to read your SQLite store.

3. **Audit log captures tool args and result text** (truncated to
   1–2 KB each). Sensitive content in chat messages or asks lands in
   `~/.claudetalk/db.sqlite`. Use `claudetalk gc` to prune older audit
   rows (Phase 4.2).

4. **`claudetalk web`** binds to `127.0.0.1` by default but accepts
   `--host 0.0.0.0`. **Don't** use the LAN-visible flag on shared
   networks; the dashboard is read-only but exposes message bodies.
