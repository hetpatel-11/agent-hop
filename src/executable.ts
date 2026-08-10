import { execFileSync } from "node:child_process";

/** Resolves a bare command name (e.g. "opencode") to its full path via the
 * OS's own lookup (`which`/`where`). Returns null on any failure so callers
 * can report a concise missing-client error instead of leaking a spawn stack. */
export function resolveExecutable(command: string): string | null {
  try {
    const finder = process.platform === "win32" ? "where" : "which";
    const out = execFileSync(finder, [command], { encoding: "utf-8" });
    const resolved = out.split("\n")[0].trim();
    return resolved || null;
  } catch {
    return null;
  }
}
