/**
 * Optional network mode config. When `~/.claudetalk/network.json` exists
 * with at least `relay_url`, claudetalk activates cross-machine sync.
 * When absent, behaviour is byte-identical to local-only mode.
 *
 * Phase N0 ships this loader only — the RelayClient that consumes it
 * lands in Phase N1. The point of shipping the scaffold first is so
 * the file-write contract is stable before any networking code starts
 * depending on it.
 */
import { existsSync, readFileSync } from "node:fs";
import { join } from "node:path";

export interface NetworkConfig {
  /** wss:// URL of the relay this machine connects to. */
  relay_url: string;
  /** Base64-encoded 32-byte shared secret. All machines under the same
   *  "account" share this value; used as a bearer credential and (Phase N2)
   *  to derive an encryption key. */
  shared_secret: string;
  /** Phase N2: encrypt message bodies before sending to the relay. */
  encrypt?: boolean;
  /** Optional override for debugging — defaults to the relay's view of
   *  this machine's machine_id. */
  display_name?: string;
}

function configPath(home: string): string {
  return join(home, "network.json");
}

function rootDir(): string {
  return process.env.CLAUDETALK_HOME ?? `${process.env.HOME}/.claudetalk`;
}

/** Return the parsed network config, or null if the file is absent /
 *  invalid / missing required fields. Caller treats null as "local-only mode". */
export function getNetworkConfig(): NetworkConfig | null {
  const path = configPath(rootDir());
  if (!existsSync(path)) return null;
  try {
    const parsed = JSON.parse(readFileSync(path, "utf8")) as Partial<NetworkConfig>;
    if (typeof parsed.relay_url !== "string" || parsed.relay_url.length === 0) return null;
    if (typeof parsed.shared_secret !== "string" || parsed.shared_secret.length === 0) {
      return null;
    }
    return {
      relay_url: parsed.relay_url,
      shared_secret: parsed.shared_secret,
      encrypt: parsed.encrypt ?? false,
      ...(parsed.display_name ? { display_name: parsed.display_name } : {}),
    };
  } catch {
    return null;
  }
}

/** Pure check used in startup logging + doctor without parsing further. */
export function isNetworkConfigured(): boolean {
  return getNetworkConfig() !== null;
}
