/**
 * Phase N3: `claudetalk auth` onboarding subcommands.
 */
import { afterEach, beforeEach, expect, test } from "bun:test";
import { existsSync, readFileSync, statSync } from "node:fs";
import { join } from "node:path";
import {
  runAddMachine,
  runAuthInit,
  runAuthReset,
  runAuthStatus,
} from "../../src/cli-auth.ts";
import { isolatedHome } from "../helpers/tmp.ts";

let home: { home: string; cleanup: () => void };

beforeEach(() => {
  home = isolatedHome();
});

afterEach(() => {
  home.cleanup();
});

test("auth status reports OFF when network.json absent", () => {
  const r = runAuthStatus();
  expect(r.ok).toBe(true);
  expect(r.output).toContain("OFF");
});

test("auth init creates network.json mode 0o600 with a fresh secret", () => {
  const r = runAuthInit({ relayUrl: "wss://relay.example.com", yes: false });
  expect(r.ok).toBe(true);
  const path = join(home.home, "network.json");
  expect(existsSync(path)).toBe(true);
  const mode = statSync(path).mode & 0o777;
  expect(mode).toBe(0o600);
  const cfg = JSON.parse(readFileSync(path, "utf8"));
  expect(cfg.relay_url).toBe("wss://relay.example.com");
  expect(typeof cfg.shared_secret).toBe("string");
  expect(cfg.shared_secret.length).toBeGreaterThan(40);
  expect(cfg.encrypt).toBe(true);
});

test("auth init refuses non-ws URL", () => {
  const r = runAuthInit({ relayUrl: "http://relay.example.com", yes: false });
  expect(r.ok).toBe(false);
  expect(r.output).toContain("ws://");
});

test("auth init refuses to overwrite without --yes", () => {
  runAuthInit({ relayUrl: "wss://r1", yes: false });
  const r = runAuthInit({ relayUrl: "wss://r2", yes: false });
  expect(r.ok).toBe(false);
  expect(r.output).toContain("already exists");
});

test("auth init rotates the secret with --yes", () => {
  runAuthInit({ relayUrl: "wss://r1", yes: false });
  const before = JSON.parse(readFileSync(join(home.home, "network.json"), "utf8"));
  runAuthInit({ relayUrl: "wss://r2", yes: true });
  const after = JSON.parse(readFileSync(join(home.home, "network.json"), "utf8"));
  expect(after.relay_url).toBe("wss://r2");
  expect(after.shared_secret).not.toBe(before.shared_secret);
});

test("auth status (post-init) prints a redacted secret", () => {
  runAuthInit({ relayUrl: "wss://r1", yes: false });
  const r = runAuthStatus();
  expect(r.output).toContain("Network mode: ON");
  expect(r.output).toContain("relay_url");
  // Secret should be partially hidden — just first 4 + last 4 chars.
  const cfg = JSON.parse(readFileSync(join(home.home, "network.json"), "utf8"));
  expect(r.output).not.toContain(cfg.shared_secret);
  expect(r.output).toContain(cfg.shared_secret.slice(0, 4));
});

test("auth add-machine emits a heredoc-bearing one-liner", () => {
  runAuthInit({ relayUrl: "wss://r1", yes: false });
  const r = runAddMachine();
  expect(r.ok).toBe(true);
  expect(r.output).toContain("mkdir -p ~/.claudetalk");
  expect(r.output).toContain("CLAUDETALK_CONFIG_EOF");
  expect(r.output).toContain("chmod 600");
  expect(r.output).toContain("Treat that block as a secret");
});

test("auth add-machine fails when no network.json", () => {
  const r = runAddMachine();
  expect(r.ok).toBe(false);
  expect(r.output).toContain("auth init");
});

test("auth reset removes network.json", () => {
  runAuthInit({ relayUrl: "wss://r1", yes: false });
  expect(existsSync(join(home.home, "network.json"))).toBe(true);
  const r = runAuthReset();
  expect(r.ok).toBe(true);
  expect(existsSync(join(home.home, "network.json"))).toBe(false);
});

test("auth reset is idempotent", () => {
  const r = runAuthReset();
  expect(r.ok).toBe(true);
  expect(r.output).toContain("already OFF");
});
