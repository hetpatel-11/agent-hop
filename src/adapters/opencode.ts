import { existsSync, mkdtempSync, writeFileSync, rmSync } from "node:fs";
import { join } from "node:path";
import { homedir, tmpdir } from "node:os";
import { execFileSync } from "node:child_process";
import { randomUUID } from "node:crypto";
import type { Adapter, SessionRef, Turn } from "../types.js";

const DB_PATH = join(homedir(), ".local", "share", "opencode", "opencode.db");

function hasOpencode(): boolean {
  try {
    execFileSync("which", ["opencode"], { stdio: "ignore" });
    return true;
  } catch {
    return false;
  }
}

async function listSessions(): Promise<SessionRef[]> {
  if (!existsSync(DB_PATH) || !hasOpencode()) return [];

  let out: string;
  try {
    out = execFileSync(
      "sqlite3",
      ["-json", `file:${DB_PATH}?mode=ro`,
       "SELECT id, directory, title, time_updated FROM session WHERE directory IS NOT NULL ORDER BY time_updated DESC LIMIT 500"],
      { encoding: "utf-8" }
    );
  } catch {
    return [];
  }
  if (!out.trim()) return [];

  let rows: { id: string; directory: string; title?: string; time_updated?: number }[];
  try {
    rows = JSON.parse(out);
  } catch {
    return [];
  }

  return rows.map((row) => ({
    tool: "opencode" as const,
    sessionId: row.id,
    projectPath: row.directory,
    title: (row.title ?? "(untitled)").slice(0, 80),
    snippet: row.title ?? "",
    updatedAt: row.time_updated ?? Date.now(),
    raw: {},
  }));
}

function exportSession(sessionId: string): { info: Record<string, unknown>; messages: { info: Record<string, unknown>; parts: { type: string; text?: string }[] }[] } | null {
  let out: string;
  try {
    out = execFileSync("opencode", ["export", sessionId], { encoding: "utf-8" });
  } catch {
    return null;
  }
  const brace = out.indexOf("{");
  if (brace === -1) return null;
  try {
    return JSON.parse(out.slice(brace));
  } catch {
    return null;
  }
}

async function read(ref: SessionRef): Promise<Turn[]> {
  const data = exportSession(ref.sessionId);
  if (!data) return [];
  const turns: Turn[] = [];
  for (const m of data.messages) {
    const role = m.info.role as string;
    if (role !== "user" && role !== "assistant") continue;
    const text = m.parts
      .filter((p) => p.type === "text" && p.text)
      .map((p) => p.text)
      .join("\n")
      .trim();
    if (text) turns.push({ role, text });
  }
  return turns;
}

/** Grab one real user+assistant message pair from any existing session to use as
 * a field-complete template -- opencode's import schema requires many fields
 * (mode, path, tokens, cost, parentID chains...) not worth hand-guessing when
 * a real example already satisfies them. */
function realExportTemplate(): { user: Record<string, unknown>; assistant: Record<string, unknown> } | null {
  let sessions: SessionRef[] = [];
  try {
    const out = execFileSync(
      "sqlite3",
      ["-json", `file:${DB_PATH}?mode=ro`, "SELECT id FROM session LIMIT 20"],
      { encoding: "utf-8" }
    );
    const rows: { id: string }[] = out.trim() ? JSON.parse(out) : [];
    sessions = rows.map((r) => ({ tool: "opencode", sessionId: r.id, projectPath: "", title: "", snippet: "", updatedAt: 0 }));
  } catch {
    return null;
  }
  for (const ref of sessions) {
    const data = exportSession(ref.sessionId);
    if (!data) continue;
    const userT = data.messages.find((m) => m.info.role === "user")?.info;
    const asstT = data.messages.find((m) => m.info.role === "assistant")?.info;
    if (userT && asstT) return { user: userT, assistant: asstT };
  }
  return null;
}

async function write(turns: Turn[], projectPath: string): Promise<string> {
  const template = realExportTemplate();
  if (!template) {
    throw new Error(
      "opencode: no existing session found to use as a field template. Start one real opencode session first (`opencode run \"hi\"`), then retry."
    );
  }

  const nowMs = Date.now();
  // IDs must be genuinely unique across every write() call, not just within
  // one call -- opencode's message/part tables use `id` as a primary key and
  // `import` does onConflictDoNothing(), so a repeated id silently no-ops the
  // insert instead of erroring. A deterministic id (e.g. index-based) means
  // every session after the first one to use that id loses its messages with
  // zero visible error.
  const uid = () => randomUUID().replace(/-/g, "");
  const newSessionId = "ses_" + uid();

  const messages: { info: Record<string, unknown>; parts: Record<string, unknown>[] }[] = [];
  let prevMsgId: string | null = null;
  turns.forEach((turn, i) => {
    const msgId = `msg_${uid()}`;
    const template_ = turn.role === "user" ? template.user : template.assistant;
    const info: Record<string, unknown> = JSON.parse(JSON.stringify(template_));
    info.id = msgId;
    info.sessionID = newSessionId;
    info.time = { created: nowMs + i };
    if (turn.role === "assistant") {
      (info.time as Record<string, unknown>).completed = nowMs + i + 1;
      info.parentID = prevMsgId ?? msgId;
      if (info.path) (info.path as Record<string, unknown>).cwd = projectPath;
    }
    messages.push({
      info,
      parts: [
        {
          type: "text",
          text: turn.text,
          id: `prt_${uid()}`,
          sessionID: newSessionId,
          messageID: msgId,
        },
      ],
    });
    prevMsgId = msgId;
  });

  const exportShape = {
    info: {
      id: newSessionId,
      parentID: newSessionId,
      slug: "resumed-via-handoff",
      projectID: "global",
      directory: projectPath,
      path: projectPath.replace(/^\//, ""),
      title: "Resumed via handoff",
      agent: "build",
      model: { id: "big-pickle", providerID: "opencode" },
      version: "1.17.7",
      time: { created: nowMs, updated: nowMs },
    },
    messages,
  };

  const tmpDir = mkdtempSync(join(tmpdir(), "handoff-opencode-"));
  const tmpFile = join(tmpDir, "session.json");
  writeFileSync(tmpFile, JSON.stringify(exportShape));
  try {
    // must run with cwd=projectPath -- opencode ties the imported session to
    // whatever directory the `import` process was actually run from, not the
    // "directory" field inside the JSON payload.
    execFileSync("opencode", ["import", tmpFile], { stdio: "pipe", cwd: projectPath });
  } finally {
    rmSync(tmpDir, { recursive: true, force: true });
  }

  return newSessionId;
}

function resumeCmd(sessionId: string, _projectPath: string): string[] {
  return ["opencode", "run", "--session", sessionId];
}

export const opencodeAdapter: Adapter = { tool: "opencode", listSessions, read, write, resumeCmd };
