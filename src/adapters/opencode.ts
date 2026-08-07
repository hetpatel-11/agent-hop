import { existsSync, mkdtempSync, writeFileSync, rmSync } from "node:fs";
import { join } from "node:path";
import { homedir, tmpdir } from "node:os";
import { execFileSync } from "node:child_process";
import { randomUUID } from "node:crypto";
import type { Adapter, SessionRef, Turn } from "../types.js";
import { cleanTitle } from "../util.js";

const DB_PATH = join(homedir(), ".local", "share", "opencode", "opencode.db");

/** The real installed opencode version, e.g. "1.18.15" -- a hardcoded guess
 * here goes stale the moment opencode updates itself (this was already
 * stuck at "1.17.7" against a real 1.18.15 install by the time this was
 * caught). Falls back to a generic placeholder if opencode isn't on PATH. */
function opencodeCliVersion(): string {
  try {
    const out = execFileSync("opencode", ["--version"], { encoding: "utf-8" });
    const match = out.match(/(\d+\.\d+\.\d+)/);
    return match ? match[1] : "0.0.0";
  } catch {
    return "0.0.0";
  }
}

function hasOpencode(): boolean {
  try {
    execFileSync("which", ["opencode"], { stdio: "ignore" });
    return true;
  } catch {
    return false;
  }
}

const MAX_BODY_CHARS = 40000;

async function listSessions(): Promise<SessionRef[]> {
  if (!existsSync(DB_PATH) || !hasOpencode()) return [];

  // Pull message text via a SQL join (not `opencode export` per session --
  // that would mean one subprocess spawn per session just to list, far too
  // slow at scale). Still pure SQL, so this stays fast.
  let out: string;
  try {
    out = execFileSync(
      "sqlite3",
      ["-json", `file:${DB_PATH}?mode=ro`,
       `SELECT s.id, s.directory, s.title, s.time_updated,
               SUBSTR(GROUP_CONCAT(json_extract(p.data, '$.text'), ' '), 1, ${MAX_BODY_CHARS}) AS body
        FROM session s
        LEFT JOIN part p ON p.session_id = s.id AND json_extract(p.data, '$.type') = 'text'
        WHERE s.directory IS NOT NULL
        GROUP BY s.id
        ORDER BY s.time_updated DESC
        LIMIT 500`],
      { encoding: "utf-8" }
    );
  } catch {
    return [];
  }
  if (!out.trim()) return [];

  let rows: { id: string; directory: string; title?: string; time_updated?: number; body?: string }[];
  try {
    rows = JSON.parse(out);
  } catch {
    return [];
  }

  return rows.map((row) => {
    // OpenCode's own placeholder before it auto-generates a real title --
    // useless for search/display, prefer real body content when we have it.
    const isPlaceholder = !row.title || /^New session - \d{4}-\d{2}-\d{2}/.test(row.title);
    const bodyFirstLine = (row.body ?? "").trim().split(/\s+/).slice(0, 20).join(" ");
    const title = cleanTitle(isPlaceholder && bodyFirstLine ? bodyFirstLine : (row.title ?? "(untitled)"));
    return {
      tool: "opencode" as const,
      sessionId: row.id,
      projectPath: row.directory,
      title: title || "(untitled)",
      snippet: title.slice(0, 200),
      body: row.body ?? row.title ?? "",
      updatedAt: row.time_updated ?? Date.now(),
      raw: {},
    };
  });
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
      // no parentID -- a real top-level session doesn't have one (confirmed
      // against a live `opencode export`). Setting it (even to its own new
      // id) makes opencode treat the session as a child/subagent session
      // instead of a normal top-level chat, which is why the resumed
      // session was rendering as if it were "thinking" as a subagent.
      slug: "resumed-via-handoff",
      projectID: "global",
      directory: projectPath,
      path: projectPath.replace(/^\//, ""),
      title: (turns.find((t) => t.role === "user")?.text ?? "Resumed via agentresume").slice(0, 80),
      agent: "build",
      model: { id: "big-pickle", providerID: "opencode" },
      version: opencodeCliVersion(),
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
    // "directory" field inside the JSON payload. If the original project dir
    // no longer exists on disk (moved/deleted since the source session was
    // created), fall back to homedir() -- otherwise spawnSync throws a
    // misleading ENOENT that looks like "opencode not found" when the real
    // problem is the missing cwd.
    const importCwd = existsSync(projectPath) ? projectPath : homedir();
    execFileSync("opencode", ["import", tmpFile], { stdio: "pipe", cwd: importCwd });
  } finally {
    rmSync(tmpDir, { recursive: true, force: true });
  }

  return newSessionId;
}

function resumeCmd(sessionId: string, _projectPath: string): string[] {
  // `opencode run` is for one-shot non-interactive messages -- it errors
  // ("You must provide a message or a command") if given only --session with
  // no message, even though the session id is valid. The default top-level
  // command (no subcommand) is what actually opens the interactive TUI
  // resumed at a given session.
  return ["opencode", "--session", sessionId];
}

export const opencodeAdapter: Adapter = { tool: "opencode", listSessions, read, write, resumeCmd };
