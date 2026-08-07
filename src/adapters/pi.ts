import { mkdirSync, writeFileSync, realpathSync } from "node:fs";
import { join } from "node:path";
import { homedir } from "node:os";
import { randomUUID } from "node:crypto";
import type { Adapter, SessionRef, Turn } from "../types.js";
import { readJsonlLines, readJsonlLinesLazy, findFiles, mtimeMs, MIN_TITLE_CHARS, cleanTitle, BodySampler } from "../util.js";

const SESSIONS_DIR = join(homedir(), ".pi", "agent", "sessions");

function encodeDir(cwd: string): string {
  let real = cwd;
  try {
    real = realpathSync(cwd);
  } catch {
    // ignore
  }
  const components = real.split("/").filter(Boolean);
  return "--" + components.join("-") + "--";
}

const MAX_BODY_CHARS = 40000;

async function listSessions(): Promise<SessionRef[]> {
  const files = findFiles(SESSIONS_DIR, (p) => p.endsWith(".jsonl"));
  const out: SessionRef[] = [];
  for (const file of files) {
    let sessionId: string | undefined;
    let cwd: string | undefined;
    let firstUserText = "";
    let titleText = "";
    const body = new BodySampler(MAX_BODY_CHARS);
    for await (const obj of readJsonlLinesLazy(file)) {
      if (obj.type === "session") {
        sessionId = obj.id as string | undefined;
        cwd = obj.cwd as string | undefined;
        continue;
      }
      if (obj.type !== "message") continue;
      const message = obj.message as { role?: string; content?: unknown } | undefined;
      if (message?.role !== "user" && message?.role !== "assistant") continue;
      if (!Array.isArray(message.content)) continue;
      const parts = message.content
        .filter((b): b is { type: string; text: string } => typeof b === "object" && b !== null && (b as { type?: string }).type === "text")
        .map((b) => b.text);
      const text = parts.join("\n").trim();
      if (!text) continue;
      if (message.role === "user") {
        if (!firstUserText) firstUserText = text;
        if (!titleText && text.length >= MIN_TITLE_CHARS) titleText = text;
      }
      body.append(text);
    }
    if (!sessionId || !cwd) continue;
    const title = cleanTitle(titleText || firstUserText);
    out.push({
      tool: "pi",
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
    if (obj.type !== "message") continue;
    const message = obj.message as { role?: string; content?: unknown } | undefined;
    const role = message?.role;
    if (role !== "user" && role !== "assistant") continue;
    if (!Array.isArray(message?.content)) continue;
    const parts = message.content
      .filter((b): b is { type: string; text: string } => typeof b === "object" && b !== null && (b as { type?: string }).type === "text")
      .map((b) => b.text);
    const text = parts.join("\n").trim();
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
  const encoded = encodeDir(realCwd);
  const sessionDir = join(SESSIONS_DIR, encoded);
  mkdirSync(sessionDir, { recursive: true });

  const newId = randomUUID();
  const now = new Date();
  const fnameTs = now.toISOString().replace(/:/g, "-").replace(/\.\d+Z$/, "") + "-" + now.getUTCMilliseconds().toString().padStart(3, "0") + "Z";
  const outPath = join(sessionDir, `${fnameTs}_${newId}.jsonl`);

  const lines: string[] = [
    JSON.stringify({
      type: "session",
      version: 3,
      id: newId,
      timestamp: now.toISOString(),
      cwd: realCwd,
    }),
  ];

  let parentId: string | null = null;
  for (const turn of turns) {
    const myId = randomUUID().replace(/-/g, "").slice(0, 8);
    const ts = new Date().toISOString();
    const message: Record<string, unknown> = {
      role: turn.role,
      content: [{ type: "text", text: turn.text }],
      timestamp: Date.now(),
    };
    if (turn.role === "assistant") {
      Object.assign(message, {
        api: "anthropic-messages",
        provider: "anthropic",
        model: "claude-sonnet-5",
        usage: {
          input: 1,
          output: 1,
          cacheRead: 0,
          cacheWrite: 0,
          totalTokens: 2,
          cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
        },
        stopReason: "stop",
        responseId: `msg_${randomUUID().replace(/-/g, "").slice(0, 24)}`,
        rawStopReason: "end_turn",
      });
    }
    lines.push(
      JSON.stringify({
        type: "message",
        id: myId,
        parentId,
        timestamp: ts,
        message,
      })
    );
    parentId = myId;
  }

  writeFileSync(outPath, lines.join("\n") + "\n");
  return newId;
}

function resumeCmd(sessionId: string, _projectPath: string): string[] {
  return ["pi", "--session", sessionId];
}

export const piAdapter: Adapter = { tool: "pi", listSessions, read, write, resumeCmd };
