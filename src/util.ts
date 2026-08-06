import { readFileSync, statSync, readdirSync, createReadStream } from "node:fs";
import { createInterface } from "node:readline";
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

/**
 * True streaming line reader -- yields one parsed line at a time without
 * ever loading the whole file into memory. Matters because a single session
 * file can be hundreds of MB (long-running agentic sessions accumulate a lot
 * of tool output); readFileSync-then-split would pay that full I/O and
 * decode cost even when a caller only needs the first few KB before
 * breaking early.
 */
export async function* readJsonlLinesLazy(path: string): AsyncGenerator<Record<string, unknown>> {
  let stream;
  try {
    stream = createReadStream(path, { encoding: "utf-8" });
  } catch {
    return;
  }
  const rl = createInterface({ input: stream, crlfDelay: Infinity });
  try {
    for await (const line of rl) {
      const trimmed = line.trim();
      if (!trimmed) continue;
      try {
        yield JSON.parse(trimmed);
      } catch {
        // skip malformed lines
      }
    }
  } finally {
    rl.close();
    stream.destroy();
  }
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
