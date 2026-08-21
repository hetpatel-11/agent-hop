import { existsSync, mkdtempSync, writeFileSync, rmSync, readFileSync, openSync, closeSync, unlinkSync } from "node:fs";
import { join } from "node:path";
import { homedir, tmpdir } from "node:os";
import { execFileSync } from "node:child_process";
import { randomUUID } from "node:crypto";
import { fileURLToPath } from "node:url";
import type { Adapter, SessionRef, Turn, ToolCallRecord, Attachment } from "../types.js";
import { cleanTitle, truncate, MAX_TOOL_OUTPUT_CHARS, toToolInputObject } from "../util.js";
import { resolveExecutable } from "../executable.js";

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

// Was hardcoded to `which`, which doesn't exist on Windows (it's `where`) --
// every check silently failed there, making opencode look uninstalled even
// when it was. resolveExecutable() already handles this split correctly
// (see executable.ts) and is used by every other adapter's own install
// check; this one just hadn't been switched over.
function hasOpencode(): boolean {
  return resolveExecutable("opencode") !== null;
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

interface OpencodePart {
  type: string;
  text?: string;
  // ToolPart -- confirmed against opencode's own source (message-v2.ts),
  // no local session with an actual tool call was available to verify
  // empirically, so this is read defensively (missing/renamed fields just
  // mean that tool call is skipped, not a crash).
  tool?: string;
  callID?: string;
  state?: { status?: string; input?: unknown; output?: unknown };
  // FilePart -- generic, not image-specific (confirmed by attaching a real
  // PDF: same shape, mime just says application/pdf instead of image/*).
  url?: string;
  mime?: string;
  filename?: string;
}

/** Captures `opencode export`'s stdout via a real temp file, not a piped
 * execFileSync capture -- confirmed as a real, silent bug: for a genuinely
 * large export (~1.9MB here), execFileSync's piped stdout capture cut off
 * deterministically at exactly the same byte offset on every run (not a
 * race -- reproduced 3x, identical truncation point each time), while the
 * exact same command with shell `>` redirection to a file got the full
 * output. The truncated JSON then failed to parse and was silently
 * swallowed by the catch below, returning null -- meaning read() silently
 * returned zero turns for a large real session, no error surfaced anywhere.
 * Writing directly to a file descriptor (like shell redirection does)
 * instead of through a pipe avoids whatever pipe-specific limit caused this. */
function exportSession(sessionId: string): { info: Record<string, unknown>; messages: { info: Record<string, unknown>; parts: OpencodePart[] }[] } | null {
  const tmpPath = join(tmpdir(), `agent-hop-opencode-export-${randomUUID()}.json`);
  let fd: number | undefined;
  try {
    fd = openSync(tmpPath, "w");
    execFileSync("opencode", ["export", sessionId], { stdio: ["ignore", fd, "ignore"] });
    closeSync(fd);
    fd = undefined;
    const out = readFileSync(tmpPath, "utf-8");
    const brace = out.indexOf("{");
    if (brace === -1) return null;
    return JSON.parse(out.slice(brace));
  } catch {
    return null;
  } finally {
    if (fd !== undefined) {
      try {
        closeSync(fd);
      } catch {
        // already closed or never opened
      }
    }
    try {
      unlinkSync(tmpPath);
    } catch {
      // best-effort cleanup -- not fatal if it was never created
    }
  }
}

/** Reads a FilePart's bytes -- OpenCode's `url` field is a data: URI for
 * pasted attachments, but can also be a local file path/file: URL for
 * attachments referenced on disk. Since OpenCode is local-first (this
 * process runs on the same machine that recorded the session), a local
 * path is just as readable as an inline blob -- try both instead of only
 * handling the inline case. Generic across mime types -- confirmed by
 * attaching a real PDF through the actual opencode CLI (`-f file.pdf`):
 * identical FilePart shape, just `mime: "application/pdf"` instead of
 * `image/*`, so there's no reason to filter to images only. */
function readOpencodeAttachment(part: OpencodePart): Attachment | null {
  if (!part.url || !part.mime) return null;
  const dataMatch = /^data:[^;]+;base64,(.*)$/s.exec(part.url);
  if (dataMatch) return { mimeType: part.mime, base64: dataMatch[1], filename: part.filename };
  try {
    const filePath = part.url.startsWith("file://") ? fileURLToPath(part.url) : part.url;
    return { mimeType: part.mime, base64: readFileSync(filePath).toString("base64"), filename: part.filename };
  } catch {
    return null; // moved/deleted since the session was recorded -- skip, don't crash
  }
}

/** OpenCode's tool calls (`ToolPart`) and image attachments (`FilePart`)
 * are confirmed against opencode's own source, not empirically verified
 * against a real local session (none in this install happened to use a
 * tool) -- read defensively so an unexpected shape just means that part is
 * skipped, never a thrown error. */
async function read(ref: SessionRef): Promise<Turn[]> {
  const data = exportSession(ref.sessionId);
  if (!data) return [];
  const turns: Turn[] = [];
  for (const m of data.messages) {
    const role = m.info.role as string;
    if (role !== "user" && role !== "assistant") continue;

    const textParts = m.parts.filter((p) => p.type === "text" && p.text).map((p) => p.text as string);
    const text = textParts.join("\n").trim();

    const toolCalls: ToolCallRecord[] = m.parts
      .filter((p) => p.type === "tool")
      .map((p) => {
        const input = typeof p.state?.input === "string" ? p.state.input : JSON.stringify(p.state?.input ?? {});
        const rec: ToolCallRecord = { name: p.tool ?? "unknown_tool", input };
        if (p.state?.output !== undefined) {
          const out = typeof p.state.output === "string" ? p.state.output : JSON.stringify(p.state.output);
          rec.output = truncate(out, MAX_TOOL_OUTPUT_CHARS);
        }
        return rec;
      });

    const attachments = m.parts
      .filter((p) => p.type === "file")
      .map(readOpencodeAttachment)
      .filter((x): x is Attachment => x !== null);

    if (text || toolCalls.length || attachments.length) {
      turns.push({ role, text, toolCalls: toolCalls.length ? toolCalls : undefined, attachments: attachments.length ? attachments : undefined });
    }
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
    // Real ToolPart/FilePart shapes, generated and confirmed against this
    // exact opencode install (not guessed from docs/source) via a live
    // `opencode run` with an actual tool call and file attachment, then
    // `opencode export` to inspect the true field-for-field shape --
    // avoids the "guessing wrong silently drops the message" risk that
    // applies to every other field in this schema (see the id-uniqueness
    // comment above).
    const toolParts: Record<string, unknown>[] = (turn.toolCalls ?? []).map((tc) => {
      const input = toToolInputObject(tc.input);
      return {
        type: "tool",
        tool: tc.name,
        callID: `call_${uid()}`,
        state: { status: "completed", input, output: tc.output ?? "", title: tc.name, metadata: {}, time: { start: nowMs + i, end: nowMs + i } },
        id: `prt_${uid()}`,
        sessionID: newSessionId,
        messageID: msgId,
      };
    });
    // Generic across mime types -- same FilePart shape works for a PDF as
    // for an image, confirmed against a real opencode session.
    const fileParts: Record<string, unknown>[] = (turn.attachments ?? []).map((att) => ({
      type: "file",
      mime: att.mimeType,
      url: `data:${att.mimeType};base64,${att.base64}`,
      synthetic: true,
      filename: att.filename ?? `attachment.${att.mimeType.split("/")[1] ?? "bin"}`,
      id: `prt_${uid()}`,
      sessionID: newSessionId,
      messageID: msgId,
    }));
    messages.push({
      info,
      parts: [
        { type: "text", text: turn.text, id: `prt_${uid()}`, sessionID: newSessionId, messageID: msgId },
        ...toolParts,
        ...fileParts,
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
      title: (turns.find((t) => t.role === "user")?.text ?? "Resumed via agent-hop").slice(0, 80),
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
