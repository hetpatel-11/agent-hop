import { readFileSync, statSync, readdirSync, createReadStream, openSync, readSync, closeSync } from "node:fs";
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

export function readJsonlTailLines(path: string, maxBytes = 768 * 1024): Record<string, unknown>[] {
  let fd: number | undefined;
  try {
    const size = statSync(path).size;
    const start = Math.max(0, size - maxBytes);
    const length = size - start;
    const buffer = Buffer.allocUnsafe(length);
    fd = openSync(path, "r");
    readSync(fd, buffer, 0, length, start);

    let raw = buffer.toString("utf-8");
    if (start > 0) {
      const firstNewline = raw.indexOf("\n");
      raw = firstNewline === -1 ? "" : raw.slice(firstNewline + 1);
    }

    const out: Record<string, unknown>[] = [];
    for (const line of raw.split("\n")) {
      const trimmed = line.trim();
      if (!trimmed) continue;
      try {
        out.push(JSON.parse(trimmed));
      } catch {
        // skip partial or malformed lines
      }
    }
    return out;
  } catch {
    return [];
  } finally {
    if (fd !== undefined) {
      try {
        closeSync(fd);
      } catch {
        // ignore
      }
    }
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

/** A message under this length is almost always a greeting ("hi", "hey
 * claude") rather than something that actually describes the session --
 * bad material for a title. Used to skip past filler when picking a title
 * candidate; the raw first message is still kept as a fallback. */
export const MIN_TITLE_CHARS = 15;

export class BodySampler {
  private first = "";
  private head = "";
  private tail = "";
  private total = 0;
  private sampled = false;

  constructor(
    private readonly maxChars = 40000,
    private readonly headChars = 20000,
    private readonly tailChars = 20000
  ) {}

  append(text: string): void {
    if (!text) return;
    const segment = text + " ";
    this.total += segment.length;
    if (this.first.length < this.maxChars) this.first += segment.slice(0, this.maxChars - this.first.length);
    if (this.head.length < this.headChars) this.head += segment.slice(0, this.headChars - this.head.length);
    this.tail = (this.tail + segment).slice(-this.tailChars);
  }

  hasHead(): boolean {
    return this.head.length >= this.headChars;
  }

  markSampled(): void {
    this.sampled = true;
  }

  value(): string {
    return !this.sampled && this.total <= this.maxChars ? this.first : `${this.head} … ${this.tail}`;
  }
}

const MAX_TITLE_CHARS = 80;

/**
 * Turns a raw first-message string into a display title: drops a leading
 * bare URL (common when a session opens with a pasted link -- the URL alone
 * is a useless title, the sentence after it is what the session is about),
 * collapses whitespace, and truncates at a word boundary instead of
 * mid-word so titles don't end like "...can you use the adobe p".
 */
export function cleanTitle(raw: string): string {
  let text = raw.trim().replace(/\s+/g, " ");
  const leadingUrl = text.match(/^https?:\/\/\S+\s*/);
  if (leadingUrl) {
    const rest = text.slice(leadingUrl[0].length).trim();
    // only drop the URL if something substantive follows it -- a
    // URL-only message still needs *a* title, so keep the URL as a
    // last resort rather than producing an empty string.
    if (rest.length >= MIN_TITLE_CHARS) text = rest;
  }
  if (text.length <= MAX_TITLE_CHARS) return text;
  const cut = text.slice(0, MAX_TITLE_CHARS);
  const lastSpace = cut.lastIndexOf(" ");
  const trimmed = lastSpace > MAX_TITLE_CHARS * 0.6 ? cut.slice(0, lastSpace) : cut;
  return trimmed.trimEnd() + "…";
}

export function isoNow(): string {
  return new Date().toISOString().replace("Z", "").slice(0, -3) + "Z";
}
