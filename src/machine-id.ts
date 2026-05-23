/**
 * Machine identity for Phase N0. Generated once per machine, stored in
 * `~/.claudetalk/machine.json`, surfaces in `discover` output and (later)
 * in cross-machine routing metadata. Sticky across runs; deterministic
 * per machine, not per folder.
 */
import { existsSync, writeFileSync } from "node:fs";
import { hostname } from "node:os";
import { join } from "node:path";
import { readJsonBounded } from "./safe-json.ts";

interface MachineFile {
  machine_id: string;
  hostname: string;
  created_at: string;
}

function machineFilePath(home: string): string {
  return join(home, "machine.json");
}

function rootDir(): string {
  return process.env.CLAUDETALK_HOME ?? `${process.env.HOME}/.claudetalk`;
}

/** Return the machine id, generating + persisting it on first call. */
export function getOrCreateMachineId(): string {
  const path = machineFilePath(rootDir());
  if (existsSync(path)) {
    const parsed = readJsonBounded<MachineFile>(path);
    if (parsed && typeof parsed.machine_id === "string" && parsed.machine_id.length > 0) {
      return parsed.machine_id;
    }
    // corrupted / oversized file — fall through and regenerate
  }
  const file: MachineFile = {
    machine_id: crypto.randomUUID(),
    hostname: hostname(),
    created_at: new Date().toISOString(),
  };
  writeFileSync(path, JSON.stringify(file, null, 2) + "\n", { mode: 0o600 });
  return file.machine_id;
}

/** Read the persisted machine file. Returns null if not yet generated. */
export function readMachineFile(): MachineFile | null {
  const path = machineFilePath(rootDir());
  if (!existsSync(path)) return null;
  return readJsonBounded<MachineFile>(path);
}
