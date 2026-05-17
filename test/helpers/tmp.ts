import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

/** Allocate an isolated CLAUDETALK_HOME and return cleanup. */
export function isolatedHome(): { home: string; cleanup: () => void } {
  const home = mkdtempSync(join(tmpdir(), "claudetalk-test-"));
  const prev = process.env.CLAUDETALK_HOME;
  process.env.CLAUDETALK_HOME = home;
  return {
    home,
    cleanup: () => {
      if (prev === undefined) delete process.env.CLAUDETALK_HOME;
      else process.env.CLAUDETALK_HOME = prev;
      try {
        rmSync(home, { recursive: true, force: true });
      } catch {}
    },
  };
}
