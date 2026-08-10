import { readFileSync, statSync, readdirSync, createReadStream, openSync, readSync, closeSync } from "node:fs";
import { createInterface } from "node:readline";
import { join } from "node:path";
import type { ToolCallRecord, Turn } from "./types.js";

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

// A tool call's output can legitimately be a whole file dump or command log
// -- capped per-call so one giant `cat` doesn't blow the entire turn budget,
// while still keeping enough to be useful (unlike dropping tool calls
// entirely, which was the previous behavior).
export const MAX_TOOL_OUTPUT_CHARS = 3000;

export function truncate(s: string, max: number): string {
  return s.length > max ? s.slice(0, max) + `\n…(truncated, ${s.length - max} more chars)` : s;
}

/** OpenAI-style function-calling backends (Codex's API, and any
 * OpenAI/Azure-backed model Pi can route to via `--model auto`) validate a
 * function name against /^[a-zA-Z0-9_-]+$/ when a conversation is
 * continued -- confirmed for real on both: a cross-agent tool label with
 * spaces/punctuation (e.g. a display name like "Web search:") loads and
 * resumes fine, then fails with a 400 the moment the conversation actually
 * continues. Anthropic's API does not enforce this on historical tool_use
 * names (also confirmed live), but sanitizing unconditionally is harmless
 * there and safer than assuming which backend a target will use. */
export function sanitizeToolName(name: string): string {
  const sanitized = name
    .replace(/^[^a-zA-Z0-9_-]+/, "")
    .replace(/[^a-zA-Z0-9_-]+$/, "")
    .replace(/[^a-zA-Z0-9_-]+/g, "_");
  return sanitized || "unknown_tool";
}

/** Renders tool calls as a plain-text block -- the shared fallback shape for
 * cross-agent conversion (native write() paths) and for --print's output,
 * since no structured tool_use/tool_result schema is portable across all
 * five agents' formats. Deliberately terse and consistent regardless of
 * which agent originally made the call. */
/** Every real coding-agent tool call whose input is a shell/exec-style
 * invocation is keyed the same handful of ways across agents (Codex's
 * exec_command uses "cmd", Claude's Bash tool uses "command", etc.) --
 * pulling the actual command out and rendering it as a real shell block is
 * what makes it read like a native tool call instead of a JSON envelope
 * dump. `description` is common alongside it (a human-readable one-liner
 * of intent) and reads naturally as a comment above the command, matching
 * how agents already narrate "why" before "what". */
function extractShellCommand(parsed: unknown): { command: string; description?: string } | null {
  if (typeof parsed !== "object" || parsed === null) return null;
  const obj = parsed as Record<string, unknown>;
  for (const key of ["command", "cmd", "script"]) {
    if (typeof obj[key] === "string") {
      const description = typeof obj.description === "string" ? obj.description : undefined;
      return { command: obj[key] as string, description };
    }
  }
  return null;
}

/** Every target TUI renders markdown in assistant messages (that's how
 * they render their own tool output), so real code fences read as a
 * proper formatted block instead of a raw inline JSON dump -- which is
 * what the plain "[tool call: x]\ninput: {...}" version produced, and
 * looked like an unstyled wall of text next to everything else the TUI
 * renders normally. The JSON itself also needs pretty-printing: a raw
 * single-line stringified arguments blob (escaped quotes, embedded
 * newlines as literal \n) reads as an unreadable wall of text even inside
 * a fence -- confirmed genuinely bad by looking at a real screenshot of a
 * resumed tool call, not just theorized. */
export function renderToolCalls(toolCalls?: ToolCallRecord[]): string {
  if (!toolCalls || toolCalls.length === 0) return "";
  return toolCalls
    .map((tc) => {
      let parsed: unknown;
      try {
        parsed = JSON.parse(tc.input);
      } catch {
        parsed = undefined;
      }

      let input: string;
      const shell = parsed !== undefined ? extractShellCommand(parsed) : null;
      if (shell) {
        const comment = shell.description ? `# ${shell.description}\n` : "";
        input = `\`\`\`bash\n${comment}${shell.command}\n\`\`\``;
      } else if (parsed !== undefined) {
        input = `\`\`\`json\n${JSON.stringify(parsed, null, 2)}\n\`\`\``;
      } else {
        input = `\`\`\`\n${tc.input}\n\`\`\``;
      }

      const output = tc.output ? `\nOutput:\n\`\`\`\n${tc.output}\n\`\`\`` : "";
      return `**Tool call: \`${tc.name}\`**\n${input}${output}`;
    })
    .join("\n\n");
}

// Full tool-call I/O (now preserved in full, not just narration text) can
// make a real long-running session's converted size enormous -- a real
// 547-turn session measured at ~3.3M characters (~820k estimated tokens),
// which silently produced a converted session no target agent's context
// window could actually load (confirmed for real: Codex errored with "ran
// out of room in the model's context window" on resume). There was never
// any size cap on full-session conversion, even before tool-call fidelity
// existed -- it just wasn't survivable to notice until output got this
// much bigger. 200k chars (~50k tokens) leaves real headroom for a target
// agent's own system prompt/skills/tools (observed as large as ~40-50k
// chars on their own in a real session) while still keeping a
// substantial, useful slice of recent conversation.
export const CONVERSION_CHAR_BUDGET = 200_000;

function turnCharCount(t: Turn): number {
  let n = t.text.length;
  for (const tc of t.toolCalls ?? []) n += tc.input.length + (tc.output?.length ?? 0);
  return n;
}

/** Keeps the most recent turns that fit under a total character budget --
 * trimming from the oldest end, since "resume" almost always means
 * "continue from where things left off," not "replay the entire history
 * from months ago." Attachment bytes (images/PDFs) aren't counted toward
 * the budget -- they're usually tokenized far more efficiently than raw
 * text per byte, and excluding them keeps this from over-trimming a
 * conversation just because it happened to have a couple of screenshots. */
export function trimTurnsToBudget(turns: Turn[], budget = CONVERSION_CHAR_BUDGET): { turns: Turn[]; droppedCount: number } {
  let total = 0;
  let cutIndex = turns.length;
  for (let i = turns.length - 1; i >= 0; i--) {
    total += turnCharCount(turns[i]);
    if (total > budget) {
      cutIndex = i + 1;
      break;
    }
    cutIndex = i;
  }
  return { turns: turns.slice(cutIndex), droppedCount: cutIndex };
}

export function isoNow(): string {
  return new Date().toISOString().replace("Z", "").slice(0, -3) + "Z";
}
