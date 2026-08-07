import { mkdirSync, writeFileSync, realpathSync } from "node:fs";
import { join } from "node:path";
import { homedir } from "node:os";
import { randomUUID } from "node:crypto";
import { execFileSync } from "node:child_process";
import type { Adapter, SessionRef, Turn } from "../types.js";
import { readJsonlLines, readJsonlLinesLazy, findFiles, mtimeMs, MIN_TITLE_CHARS, cleanTitle, BodySampler } from "../util.js";

const PROJECTS_DIR = join(homedir(), ".claude", "projects");

/** The real installed Claude Code version, e.g. "2.1.198" -- a hardcoded
 * guess here inevitably goes stale the moment Claude Code updates itself
 * (confirmed happening for real with opencode's equivalent field while
 * auditing this). Falls back to a generic placeholder if claude isn't on
 * PATH, which would fail write() well before this matters in practice. */
function claudeCliVersion(): string {
  try {
    const out = execFileSync("claude", ["--version"], { encoding: "utf-8" });
    const match = out.match(/(\d+\.\d+\.\d+)/);
    return match ? match[1] : "0.0.0";
  } catch {
    return "0.0.0";
  }
}

function encodeDir(cwd: string): string {
  let real = cwd;
  try {
    real = realpathSync(cwd);
  } catch {
    // path may not exist yet -- fall back to the given value
  }
  // Claude Code replaces every non-alphanumeric character with "-", not just "/".
  return real.replace(/[^a-zA-Z0-9]/g, "-");
}

const MAX_BODY_CHARS = 40000;

async function listSessions(): Promise<SessionRef[]> {
  const files = findFiles(PROJECTS_DIR, (p) => p.endsWith(".jsonl"));
  const out: SessionRef[] = [];
  for (const file of files) {
    let cwd: string | undefined;
    let firstUserText = ""; // any length -- fallback
    let titleText = ""; // first *substantive* user message -- preferred
    const body = new BodySampler(MAX_BODY_CHARS);
    for await (const obj of readJsonlLinesLazy(file)) {
      if (typeof obj.cwd === "string") cwd = obj.cwd;
      if (obj.type !== "user" && obj.type !== "assistant") continue;
      const message = obj.message as { role?: string; content?: unknown } | undefined;
      let text = "";
      if (typeof message?.content === "string") {
        text = message.content;
      } else if (Array.isArray(message?.content)) {
        text = message.content
          .filter((b): b is { type: string; text: string } => typeof b === "object" && b !== null && (b as { type?: string }).type === "text")
          .map((b) => b.text)
          .join(" ");
      }
      if (!text) continue;
      if (obj.type === "user") {
        if (!firstUserText) firstUserText = text;
        if (!titleText && text.length >= MIN_TITLE_CHARS) titleText = text;
      }
      body.append(text);
    }
    if (!cwd) continue;
    const sessionId = file.split("/").pop()!.replace(/\.jsonl$/, "");
    const title = cleanTitle(titleText || firstUserText);
    out.push({
      tool: "claude",
      sessionId,
      projectPath: cwd,
      title: title || "(empty)",
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
    if (obj.type !== "user" && obj.type !== "assistant") continue;
    const message = obj.message as { role?: string; content?: unknown } | undefined;
    const role = message?.role;
    if (role !== "user" && role !== "assistant") continue;
    let text = "";
    const content = message?.content;
    if (typeof content === "string") {
      text = content;
    } else if (Array.isArray(content)) {
      text = content
        .filter((b): b is { type: string; text: string } => typeof b === "object" && b !== null && (b as { type?: string }).type === "text")
        .map((b) => b.text)
        .join("\n");
    }
    text = text.trim();
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
  const cliVersion = claudeCliVersion(); // computed once, reused per turn below
  const encoded = encodeDir(realCwd);
  const sessionDir = join(PROJECTS_DIR, encoded);
  mkdirSync(sessionDir, { recursive: true });

  const newId = randomUUID();
  const outPath = join(sessionDir, `${newId}.jsonl`);

  const lines: string[] = [];
  let parentUuid: string | null = null;
  let lastUuid: string | null = null;

  for (const turn of turns) {
    const myUuid = randomUUID();
    const ts = new Date().toISOString();
    let entry: Record<string, unknown>;
    if (turn.role === "user") {
      entry = {
        parentUuid,
        isSidechain: false,
        promptId: randomUUID(),
        type: "user",
        message: { role: "user", content: turn.text },
        uuid: myUuid,
        timestamp: ts,
        userType: "external",
        entrypoint: "cli",
        cwd: realCwd,
        sessionId: newId,
        version: cliVersion,
      };
    } else {
      entry = {
        parentUuid,
        isSidechain: false,
        message: {
          model: "claude-sonnet-5",
          id: `msg_${randomUUID().replace(/-/g, "").slice(0, 24)}`,
          type: "message",
          role: "assistant",
          content: [{ type: "text", text: turn.text }],
          stop_reason: "end_turn",
          stop_sequence: null,
        },
        type: "assistant",
        uuid: myUuid,
        timestamp: ts,
        userType: "external",
        entrypoint: "cli",
        cwd: realCwd,
        sessionId: newId,
        version: cliVersion,
      };
    }
    lines.push(JSON.stringify(entry));
    parentUuid = myUuid;
    lastUuid = myUuid;
  }

  lines.unshift(JSON.stringify({ type: "last-prompt", leafUuid: lastUuid, sessionId: newId }));
  writeFileSync(outPath, lines.join("\n") + "\n");
  return newId;
}

function resumeCmd(sessionId: string, _projectPath: string): string[] {
  return ["claude", "--resume", sessionId];
}

export const claudeAdapter: Adapter = { tool: "claude", listSessions, read, write, resumeCmd };
