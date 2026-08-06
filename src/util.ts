import { readFileSync, statSync, readdirSync } from "node:fs";
import { join } from "node:path";

export function readJsonlLines(path: string): Record<string, unknown>[] {
  let raw: string;
  try {
    raw = readFileSync(path, "utf-8");
  } catch {
    return [];
  }
  const out: Record<string, unknown>[] = [];
  for (const line of raw.split("\n")) {
    const trimmed = line.trim();
    if (!trimmed) continue;
    try {
      out.push(JSON.parse(trimmed));
    } catch {
      // skip malformed lines
    }
  }
  return out;
}

/** Recursively find files matching a predicate, without pulling in a glob dependency. */
export function findFiles(root: string, matches: (path: string) => boolean, maxDepth = 8): string[] {
  const out: string[] = [];
  function walk(dir: string, depth: number) {
    if (depth > maxDepth) return;
    let entries: string[];
    try {
      entries = readdirSync(dir);
    } catch {
      return;
    }
    for (const entry of entries) {
      const full = join(dir, entry);
      let st;
      try {
        st = statSync(full);
      } catch {
        continue;
      }
      if (st.isDirectory()) {
        walk(full, depth + 1);
      } else if (matches(full)) {
        out.push(full);
      }
    }
  }
  walk(root, 0);
  return out;
}

export function mtimeMs(path: string): number {
  try {
    return statSync(path).mtimeMs;
  } catch {
    return 0;
  }
}

export function isoNow(): string {
  return new Date().toISOString().replace("Z", "").slice(0, -3) + "Z";
}
