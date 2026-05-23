/**
 * `claudetalk auth` subcommands — onboarding helpers for the
 * cross-machine relay. Phase N3.
 *
 * Subcommands:
 *   auth init --relay-url <url>            create network.json + secret
 *   auth status                            print current network.json (no secret)
 *   auth add-machine                       print one-liner for machine B
 *   auth reset                             delete network.json (local-only)
 */
import {
  chmodSync,
  existsSync,
  mkdirSync,
  unlinkSync,
  writeFileSync,
} from "node:fs";
import { join } from "node:path";
import { getNetworkConfig, type NetworkConfig } from "./network-config.ts";

function rootDir(): string {
  return process.env.CLAUDETALK_HOME ?? `${process.env.HOME}/.claudetalk`;
}

function configPath(): string {
  return join(rootDir(), "network.json");
}

function randomSecret(bytes = 32): string {
  const u = new Uint8Array(bytes);
  crypto.getRandomValues(u);
  return Buffer.from(u).toString("base64url");
}

export interface AuthInitOptions {
  relayUrl: string;
  /** Skip the "this overwrites existing config" prompt. */
  yes: boolean;
}

export function runAuthInit(opts: AuthInitOptions): { ok: boolean; output: string } {
  if (!opts.relayUrl.startsWith("ws://") && !opts.relayUrl.startsWith("wss://")) {
    return {
      ok: false,
      output: `relay-url must start with ws:// or wss:// (got '${opts.relayUrl}').`,
    };
  }
  const path = configPath();
  if (existsSync(path) && !opts.yes) {
    return {
      ok: false,
      output:
        `${path} already exists. Re-run with --yes to overwrite ` +
        `(this generates a NEW shared secret; other machines need to rerun add-machine).`,
    };
  }
  mkdirSync(rootDir(), { recursive: true });
  const config: NetworkConfig = {
    relay_url: opts.relayUrl,
    shared_secret: randomSecret(32),
    encrypt: true,
  };
  writeFileSync(path, JSON.stringify(config, null, 2) + "\n", { mode: 0o600 });
  try {
    chmodSync(path, 0o600);
  } catch {}
  return {
    ok: true,
    output: [
      `✓ wrote ${path} (mode 0o600)`,
      `  relay_url:     ${config.relay_url}`,
      `  shared_secret: <hidden — see \`claudetalk auth add-machine\` to share>`,
      `  encrypt:       ${config.encrypt}`,
      "",
      "Restart your Claude Code sessions. To enroll other machines, run",
      "`claudetalk auth add-machine` and follow the one-liner.",
    ].join("\n"),
  };
}

export function runAuthStatus(): { ok: boolean; output: string } {
  const cfg = getNetworkConfig();
  if (!cfg) {
    return {
      ok: true,
      output:
        "Network mode is OFF (no network.json present). Run `claudetalk auth init` to enable.",
    };
  }
  return {
    ok: true,
    output: [
      "Network mode: ON",
      `  relay_url:     ${cfg.relay_url}`,
      `  encrypt:       ${cfg.encrypt ? "on" : "off"}`,
      `  shared_secret: ${cfg.shared_secret.slice(0, 4)}…${cfg.shared_secret.slice(-4)} (${cfg.shared_secret.length} chars)`,
    ].join("\n"),
  };
}

/** Print a one-liner the user can paste on machine B's terminal to
 *  enroll it in this namespace. The line embeds the relay URL + the
 *  shared secret in a bash heredoc. Treat the output like a secret —
 *  anyone with this line can join the network. */
export function runAddMachine(): { ok: boolean; output: string } {
  const cfg = getNetworkConfig();
  if (!cfg) {
    return {
      ok: false,
      output:
        "No network.json on this machine. Run `claudetalk auth init --relay-url wss://...` first.",
    };
  }
  // Pretty-print the config as a bash heredoc the user can paste.
  const payload = JSON.stringify(
    { relay_url: cfg.relay_url, shared_secret: cfg.shared_secret, encrypt: cfg.encrypt ?? true },
    null,
    2,
  );
  // Quote-safe heredoc.
  const oneLiner =
    `mkdir -p ~/.claudetalk && ` +
    `cat > ~/.claudetalk/network.json <<'CLAUDETALK_CONFIG_EOF'\n${payload}\nCLAUDETALK_CONFIG_EOF\n` +
    `chmod 600 ~/.claudetalk/network.json && ` +
    `echo "✓ enrolled — restart your Claude Code sessions"`;
  return {
    ok: true,
    output: [
      "Paste THIS on the OTHER machine's terminal (run as the same user):",
      "",
      oneLiner,
      "",
      "⚠ Treat that block as a secret. Anyone who has it can join the namespace,",
      "  decrypt all messages, and post as any new pseudonym they bring.",
      "  Send it via 1Password / iCloud Keychain / scp — not Slack / email plaintext.",
    ].join("\n"),
  };
}

export function runAuthReset(): { ok: boolean; output: string } {
  const path = configPath();
  if (!existsSync(path)) {
    return { ok: true, output: "Network mode already OFF (no network.json)." };
  }
  unlinkSync(path);
  return {
    ok: true,
    output: `✓ removed ${path}. Restart your Claude Code sessions to disable cross-machine sync.`,
  };
}
