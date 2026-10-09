/**
 * Phase N0: machine.json generation. Sticky UUID per machine, written
 * with mode 600.
 */
import { afterEach, beforeEach, expect, test } from "bun:test";
import { existsSync, statSync } from "node:fs";
import { join } from "node:path";
import { getOrCreateMachineId, readMachineFile } from "../../src/machine-id.ts";
import { getNetworkConfig, isNetworkConfigured } from "../../src/network-config.ts";
import { writeFileSync } from "node:fs";
import { isolatedHome } from "../helpers/tmp.ts";

let home: { home: string; cleanup: () => void };

beforeEach(() => {
  home = isolatedHome();
});

afterEach(() => {
  home.cleanup();
});

test("getOrCreateMachineId generates a UUID + machine.json on first call", () => {
  const id = getOrCreateMachineId();
  expect(id).toMatch(/^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/);
  expect(existsSync(join(home.home, "machine.json"))).toBe(true);
});

test("getOrCreateMachineId is idempotent — second call returns the same id", () => {
  const a = getOrCreateMachineId();
  const b = getOrCreateMachineId();
  expect(a).toBe(b);
});

test("machine.json is written with mode 600", () => {
  getOrCreateMachineId();
  const path = join(home.home, "machine.json");
  const mode = statSync(path).mode & 0o777;
  expect(mode).toBe(0o600);
});

test("readMachineFile returns the persisted shape", () => {
  const id = getOrCreateMachineId();
  const file = readMachineFile();
  expect(file).not.toBeNull();
  expect(file!.machine_id).toBe(id);
  expect(typeof file!.hostname).toBe("string");
  expect(file!.created_at).toMatch(/^\d{4}-\d{2}-\d{2}T/);
});

test("readMachineFile returns null before generation", () => {
  expect(readMachineFile()).toBeNull();
});

test("getNetworkConfig returns null when ~/.claudetalk/network.json absent", () => {
  expect(getNetworkConfig()).toBeNull();
  expect(isNetworkConfigured()).toBe(false);
});

test("getNetworkConfig parses a valid file", () => {
  writeFileSync(
    join(home.home, "network.json"),
    JSON.stringify({
      relay_url: "wss://relay.example.com",
      shared_secret: "deadbeef",
    }),
  );
  const cfg = getNetworkConfig();
  expect(cfg).not.toBeNull();
  expect(cfg!.relay_url).toBe("wss://relay.example.com");
  expect(cfg!.encrypt).toBe(false);
  expect(isNetworkConfigured()).toBe(true);
});

test("getNetworkConfig rejects files missing required fields", () => {
  writeFileSync(join(home.home, "network.json"), JSON.stringify({ relay_url: "wss://x" }));
  expect(getNetworkConfig()).toBeNull();
});
