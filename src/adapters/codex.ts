import { mkdirSync, writeFileSync, realpathSync } from "node:fs";
import { join } from "node:path";
import { homedir } from "node:os";
import { randomUUID } from "node:crypto";
import { execFileSync } from "node:child_process";
import type { Adapter, SessionRef, Turn } from "../types.js";
import { readJsonlLines, readJsonlLinesLazy, readJsonlTailLines, findFiles, mtimeMs, MIN_TITLE_CHARS, cleanTitle, BodySampler } from "../util.js";

const SESSIONS_DIR = join(homedir(), ".codex", "sessions");

function pad(n: number): string {
  return n.toString().padStart(2, "0");
}

/** The real codex CLI's version, e.g. "0.146.1" -- codex's own `resume`
 * bumps its schema/behavior across releases, so a hardcoded guess here
 * inevitably goes stale the moment codex updates itself. Falls back to a
 * generic placeholder if codex isn't on PATH (write() would fail on the
 * missing binary long before this matters in practice). */
function codexCliVersion(): string {
  try {
    const out = execFileSync("codex", ["--version"], { encoding: "utf-8" });
    const match = out.match(/(\d+\.\d+\.\d+)/);
    return match ? match[1] : "0.0.0";
  } catch {
    return "0.0.0";
  }
}

const MAX_BODY_CHARS = 40000;

async function listSessions(): Promise<SessionRef[]> {
  const files = findFiles(SESSIONS_DIR, (p) => p.endsWith(".jsonl"));
  const out: SessionRef[] = [];
  for (const file of files) {
    let sessionId: string | undefined;
    let cwd: string | undefined;
    let firstUserText = "";
    let firstAssistantText = "";
    let titleText = "";
    const body = new BodySampler(MAX_BODY_CHARS);
    let stoppedEarly = false;
    for await (const obj of readJsonlLinesLazy(file)) {
      if (obj.type === "session_meta") {
        const payload = obj.payload as { id?: string; cwd?: string } | undefined;
        sessionId = payload?.id;
        cwd = payload?.cwd;
        continue;
      }
      if (obj.type !== "response_item") continue;
      const payload = obj.payload as { type?: string; role?: string; content?: unknown } | undefined;
      if (payload?.type !== "message" || !Array.isArray(payload.content)) continue;
      if (payload.role !== "user" && payload.role !== "assistant") continue;
      const parts = payload.content
        .filter((b): b is { type: string; text: string } => typeof b === "object" && b !== null && ["input_text", "output_text", "text"].includes((b as { type?: string }).type ?? ""))
        .map((b) => b.text);
      const text = parts.join("\n").trim();
      if (!text || text.startsWith("<environment_context>") || text.startsWith("# Context from my IDE") || text.startsWith("# AGENTS.md instructions") || text.startsWith("<recommended_plugins>")) continue;
      // a pasted terminal prompt ("user@host dir % some-command ...") is
      // real content but a bad title -- it's what the user *ran*, not what
      // they *asked*. Skip it as a title candidate entirely (both the
      // preferred titleText and the last-resort firstUserText), but
      // body/search still sees it via `body` below.
      const isShellPaste = /^\S+@\S+\s.*[%$#]\s/.test(text);
      if (payload.role === "user") {
        if (!firstUserText && !isShellPaste) firstUserText = text;
        if (!titleText && text.length >= MIN_TITLE_CHARS && !isShellPaste) titleText = text;
      } else if (!firstAssistantText) {
        firstAssistantText = text;
      }
      body.append(text);
      if (sessionId && cwd && titleText && body.hasHead()) {
        stoppedEarly = true;
        break;
      }
    }
    if (stoppedEarly) {
      body.markSampled();
      for (const obj of readJsonlTailLines(file)) {
        if (obj.type !== "response_item") continue;
        const payload = obj.payload as { type?: string; role?: string; content?: unknown } | undefined;
        if (payload?.type !== "message" || !Array.isArray(payload.content)) continue;
        if (payload.role !== "user" && payload.role !== "assistant") continue;
        const parts = payload.content
          .filter((b): b is { type: string; text: string } => typeof b === "object" && b !== null && ["input_text", "output_text", "text"].includes((b as { type?: string }).type ?? ""))
          .map((b) => b.text);
        const text = parts.join("\n").trim();
        if (!text || text.startsWith("<environment_context>") || text.startsWith("# Context from my IDE") || text.startsWith("# AGENTS.md instructions") || text.startsWith("<recommended_plugins>")) continue;
        body.append(text);
      }
    }
    if (!sessionId || !cwd) continue;
    const title = cleanTitle(titleText || firstUserText || firstAssistantText || `(${cwd.split("/").pop()}, no readable content)`);
    out.push({
      tool: "codex",
      sessionId,
      projectPath: cwd,
      title,
      snippet: title.slice(0, 200),
      body: body.value(),
      updatedAt: mtimeMs(file),
      raw: { file },
    });
  }
  return out;
}

async function read(ref: SessionRef): Promise<Turn[]> {
  const file = ref.raw?.file as string;
  const lines = readJsonlLines(file);
  const turns: Turn[] = [];
  for (const obj of lines) {
    if (obj.type !== "response_item") continue;
    const payload = obj.payload as { type?: string; role?: string; content?: unknown } | undefined;
    if (payload?.type !== "message") continue;
    const role = payload.role;
    if (role !== "user" && role !== "assistant") continue;
    if (!Array.isArray(payload.content)) continue;
    const parts = payload.content
      .filter((b): b is { type: string; text: string } => typeof b === "object" && b !== null && ["input_text", "output_text", "text"].includes((b as { type?: string }).type ?? ""))
      .map((b) => b.text);
    const text = parts.join("\n").trim();
    if (text.startsWith("<environment_context>") || text.startsWith("# Context from my IDE") || text.startsWith("# AGENTS.md instructions") || text.startsWith("<recommended_plugins>")) continue;
    if (text) turns.push({ role, text });
  }
  return turns;
}

async function write(turns: Turn[], projectPath: string): Promise<string> {
  let realCwd = projectPath;
  try {
    realCwd = realpathSync(projectPath);
  } catch {
    mkdirSync(projectPath, { recursive: true });
    realCwd = realpathSync(projectPath);
  }

  const newId = randomUUID();
  const now = new Date();
  const dateDir = join(SESSIONS_DIR, `${now.getUTCFullYear()}`, pad(now.getUTCMonth() + 1), pad(now.getUTCDate()));
  mkdirSync(dateDir, { recursive: true });
  const fnameTs = `${now.getUTCFullYear()}-${pad(now.getUTCMonth() + 1)}-${pad(now.getUTCDate())}T${pad(now.getUTCHours())}-${pad(now.getUTCMinutes())}-${pad(now.getUTCSeconds())}`;
  const outPath = join(dateDir, `rollout-${fnameTs}-${newId}.jsonl`);

  const lines: string[] = [
    JSON.stringify({
      timestamp: now.toISOString(),
      type: "session_meta",
      payload: {
        id: newId,
        timestamp: now.toISOString(),
        cwd: realCwd,
        originator: "codex_cli_rs",
        cli_version: codexCliVersion(),
        instructions: null,
        source: "cli",
        // Missing entirely before this fix -- codex's own `resume` command
        // reads this field and, when absent, apparently defaults it to an
        // empty string internally rather than erroring at write time,
        // surfacing later as a cryptic 'Model provider `` not found'
        // failure only when you actually try to resume. Confirmed by
        // comparing against a real session's session_meta, which always
        // has this set.
        model_provider: "openai",
      },
    }),
  ];
  // response_item alone lets `codex resume` load and continue the session
  // (it's the API-level conversation log) but the TUI doesn't display prior
  // turns from it -- a real rollout file also has event_msg records
  // (`user_message`/`agent_message`), which is what the TUI actually
  // replays to render history. Same class of bug as Grok's missing
  // updates.jsonl, found the same way: diffing real vs. synthetic event
  // types (`response_item`: 1140, `event_msg`: 819 in a real session --
  // not a minor/optional category) and inspecting the real payload shape.
  for (const turn of turns) {
    const ts = new Date().toISOString();
    lines.push(
      JSON.stringify({
        timestamp: ts,
        type: "response_item",
        payload: {
          type: "message",
          role: turn.role,
          content: [{ type: turn.role === "user" ? "input_text" : "output_text", text: turn.text }],
        },
      })
    );
    lines.push(
      JSON.stringify({
        timestamp: ts,
        type: "event_msg",
        payload:
          turn.role === "user"
            ? { type: "user_message", message: turn.text }
            : { type: "agent_message", message: turn.text, phase: "commentary" },
      })
    );
  }
  writeFileSync(outPath, lines.join("\n") + "\n");
  return newId;
}

function resumeCmd(sessionId: string, _projectPath: string): string[] {
  return ["codex", "resume", sessionId];
}

export const codexAdapter: Adapter = { tool: "codex", listSessions, read, write, resumeCmd };
