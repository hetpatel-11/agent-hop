import { mkdirSync, writeFileSync, realpathSync } from "node:fs";
import { join } from "node:path";
import { homedir } from "node:os";
import { randomUUID } from "node:crypto";
import type { Adapter, SessionRef, Turn } from "../types.js";
import { readJsonlLines, findFiles, mtimeMs } from "../util.js";

const PROJECTS_DIR = join(homedir(), ".claude", "projects");

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

async function listSessions(): Promise<SessionRef[]> {
  const files = findFiles(PROJECTS_DIR, (p) => p.endsWith(".jsonl"));
  const out: SessionRef[] = [];
  for (const file of files) {
    const lines = readJsonlLines(file);
    let cwd: string | undefined;
    let firstUserText = "";
    for (const obj of lines) {
      if (typeof obj.cwd === "string") cwd = obj.cwd;
      if (!firstUserText && obj.type === "user") {
        const message = obj.message as { content?: unknown } | undefined;
        if (typeof message?.content === "string") firstUserText = message.content;
      }
      if (cwd && firstUserText) break;
    }
    if (!cwd) continue;
    const sessionId = file.split("/").pop()!.replace(/\.jsonl$/, "");
    out.push({
      tool: "claude",
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
        version: "2.1.198",
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
        version: "2.1.198",
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
