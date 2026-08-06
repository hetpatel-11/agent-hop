import { mkdirSync, writeFileSync, realpathSync } from "node:fs";
import { join } from "node:path";
import { homedir } from "node:os";
import { randomUUID } from "node:crypto";
import type { Adapter, SessionRef, Turn } from "../types.js";
import { readJsonlLines, findFiles, mtimeMs } from "../util.js";

const SESSIONS_DIR = join(homedir(), ".codex", "sessions");

function pad(n: number): string {
  return n.toString().padStart(2, "0");
}

async function listSessions(): Promise<SessionRef[]> {
  const files = findFiles(SESSIONS_DIR, (p) => p.endsWith(".jsonl"));
  const out: SessionRef[] = [];
  for (const file of files) {
    const lines = readJsonlLines(file);
    let sessionId: string | undefined;
    let cwd: string | undefined;
    let firstUserText = "";
    for (const obj of lines) {
      if (obj.type === "session_meta") {
        const payload = obj.payload as { id?: string; cwd?: string } | undefined;
        sessionId = payload?.id;
        cwd = payload?.cwd;
        continue;
      }
      if (!firstUserText && obj.type === "response_item") {
        const payload = obj.payload as { type?: string; role?: string; content?: unknown } | undefined;
        if (payload?.type === "message" && payload.role === "user" && Array.isArray(payload.content)) {
          const parts = payload.content
            .filter((b): b is { type: string; text: string } => typeof b === "object" && b !== null && ["input_text", "text"].includes((b as { type?: string }).type ?? ""))
            .map((b) => b.text);
          const text = parts.join("\n").trim();
          if (text && !text.startsWith("<environment_context>") && !text.startsWith("# Context from my IDE")) {
            firstUserText = text;
          }
        }
      }
      if (sessionId && cwd && firstUserText) break;
    }
    if (!sessionId || !cwd) continue;
    out.push({
      tool: "codex",
      sessionId,
      projectPath: cwd,
      title: firstUserText.slice(0, 80) || "(empty)",
      snippet: firstUserText.slice(0, 200),
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
    if (text.startsWith("<environment_context>") || text.startsWith("# Context from my IDE")) continue;
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
        cli_version: "0.45.0",
        instructions: null,
        source: "cli",
      },
    }),
  ];
  for (const turn of turns) {
    lines.push(
      JSON.stringify({
        timestamp: new Date().toISOString(),
        type: "response_item",
        payload: {
          type: "message",
          role: turn.role,
          content: [{ type: turn.role === "user" ? "input_text" : "output_text", text: turn.text }],
        },
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
