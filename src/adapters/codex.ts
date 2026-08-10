import { mkdirSync, writeFileSync, realpathSync } from "node:fs";
import { join } from "node:path";
import { homedir } from "node:os";
import { randomUUID } from "node:crypto";
import { execFileSync } from "node:child_process";
import type { Adapter, SessionRef, Turn, ToolCallRecord, Attachment } from "../types.js";
import { readJsonlLines, readJsonlLinesLazy, readJsonlTailLines, findFiles, mtimeMs, MIN_TITLE_CHARS, cleanTitle, BodySampler, truncate, MAX_TOOL_OUTPUT_CHARS } from "../util.js";

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

const ENV_PREFIXES = ["<environment_context>", "# Context from my IDE", "# AGENTS.md instructions", "<recommended_plugins>"];

// Codex has exactly one binary content type -- input_image. Non-image
// attachments (PDFs, text files via -i) get flattened into plain
// input_text by codex itself, already captured by the text extraction
// below -- confirmed by generating a real session with a PDF attached.
function extractCodexAttachments(content: unknown[]): Attachment[] {
  return content
    .filter((b): b is { type: string; image_url: string } => typeof b === "object" && b !== null && (b as { type?: string }).type === "input_image" && typeof (b as { image_url?: unknown }).image_url === "string")
    .map((b) => {
      const m = /^data:([^;]+);base64,(.*)$/s.exec(b.image_url);
      return m ? { mimeType: m[1], base64: m[2] } : null;
    })
    .filter((x): x is Attachment => x !== null);
}

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

/** Codex emits tool calls (function_call/function_call_output, and
 * custom_tool_call/custom_tool_call_output for things like apply_patch) as
 * separate top-level stream events, matched by call_id, interleaved with
 * possibly several assistant text messages before the next user turn. All
 * of that -- narration, tool calls, and their real output/diffs -- belongs
 * to one logical assistant turn, so it's accumulated and flushed as a
 * single Turn each time a user message appears. */
async function read(ref: SessionRef): Promise<Turn[]> {
  const file = ref.raw?.file as string;
  const lines = readJsonlLines(file);
  const turns: Turn[] = [];

  let assistantTextParts: string[] = [];
  let pendingToolCalls: ToolCallRecord[] = [];
  let pendingAttachments: Attachment[] = [];
  const callIndex = new Map<string, ToolCallRecord>();

  const flushAssistant = () => {
    const text = assistantTextParts.join("\n\n").trim();
    if (text || pendingToolCalls.length > 0 || pendingAttachments.length > 0) {
      turns.push({
        role: "assistant",
        text,
        toolCalls: pendingToolCalls.length ? pendingToolCalls : undefined,
        attachments: pendingAttachments.length ? pendingAttachments : undefined,
      });
    }
    assistantTextParts = [];
    pendingToolCalls = [];
    pendingAttachments = [];
    callIndex.clear();
  };

  for (const obj of lines) {
    if (obj.type !== "response_item") continue;
    const payload = obj.payload as { type?: string; role?: string; content?: unknown; name?: string; arguments?: string; input?: string; output?: unknown; call_id?: string } | undefined;
    if (!payload) continue;

    if (payload.type === "function_call" || payload.type === "custom_tool_call") {
      const rec: ToolCallRecord = { name: payload.name ?? "unknown_tool", input: (payload.type === "function_call" ? payload.arguments : payload.input) ?? "" };
      pendingToolCalls.push(rec);
      if (payload.call_id) callIndex.set(payload.call_id, rec);
      continue;
    }
    if (payload.type === "function_call_output" || payload.type === "custom_tool_call_output") {
      const rec = payload.call_id ? callIndex.get(payload.call_id) : undefined;
      if (rec) {
        const out = typeof payload.output === "string" ? payload.output : JSON.stringify(payload.output);
        rec.output = truncate(out, MAX_TOOL_OUTPUT_CHARS);
      }
      continue;
    }
    if (payload.type !== "message") continue;
    const role = payload.role;
    if (role !== "user" && role !== "assistant") continue;
    if (!Array.isArray(payload.content)) continue;

    const parts = payload.content
      .filter((b): b is { type: string; text: string } => typeof b === "object" && b !== null && ["input_text", "output_text", "text"].includes((b as { type?: string }).type ?? ""))
      .map((b) => b.text);
    const text = parts.join("\n").trim();
    if (ENV_PREFIXES.some((p) => text.startsWith(p))) continue;
    const attachments = extractCodexAttachments(payload.content);

    if (role === "user") {
      flushAssistant();
      if (text || attachments.length) turns.push({ role: "user", text, attachments: attachments.length ? attachments : undefined });
    } else {
      if (text) assistantTextParts.push(text);
      if (attachments.length) pendingAttachments.push(...attachments);
    }
  }
  flushAssistant();
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
    // Real function_call/function_call_output response_items, not inlined
    // text -- confirmed safe by actually forging one (with a tool name
    // codex never registered, e.g. Claude's "Bash") and resuming it for
    // real: it loaded and the model correctly recalled the exact fake
    // result, zero schema/validation error. Codex's loader just replays
    // whatever's in the rollout; it doesn't validate tool names.
    // Codex's own -i flag flattens non-image attachments (PDFs, text files)
    // into plain input_text rather than a distinct binary block -- but that
    // only works because codex extracts real readable text from the file
    // itself. We only have base64 bytes, not extracted text, so a non-image
    // attachment gets a placeholder note instead of forged (and likely
    // garbled) inline binary-as-text.
    const nonImageNote = (turn.attachments ?? [])
      .filter((a) => !a.mimeType.startsWith("image/"))
      .map((a) => `[attached file: ${a.filename ?? "unnamed"} (${a.mimeType})]`)
      .join("\n");
    const combinedText = [turn.text, nonImageNote].filter(Boolean).join("\n\n");
    const content: Record<string, unknown>[] = [];
    if (combinedText) content.push({ type: turn.role === "user" ? "input_text" : "output_text", text: combinedText });
    // Images DO have a real portable shape (both Claude and Codex embed them
    // as inline base64, just different wrapper fields), so these round-trip
    // as genuine input_image blocks, not a text placeholder.
    for (const img of (turn.attachments ?? []).filter((a) => a.mimeType.startsWith("image/"))) {
      content.push({ type: "input_image", image_url: `data:${img.mimeType};base64,${img.base64}` });
    }
    if (content.length > 0) {
      lines.push(
        JSON.stringify({
          timestamp: ts,
          type: "response_item",
          payload: { type: "message", role: turn.role, content },
        })
      );
    }
    for (const tc of turn.toolCalls ?? []) {
      const callId = `call_${randomUUID().replace(/-/g, "").slice(0, 24)}`;
      lines.push(
        JSON.stringify({
          timestamp: ts,
          type: "response_item",
          payload: { type: "function_call", name: tc.name, arguments: tc.input, call_id: callId },
        })
      );
      lines.push(
        JSON.stringify({
          timestamp: ts,
          type: "response_item",
          payload: { type: "function_call_output", call_id: callId, output: tc.output ?? "" },
        })
      );
    }
    if (content.length === 0 && !(turn.toolCalls ?? []).length) continue;
    lines.push(
      JSON.stringify({
        timestamp: ts,
        type: "event_msg",
        payload:
          turn.role === "user"
            ? { type: "user_message", message: combinedText || "[image attached]" }
            : { type: "agent_message", message: combinedText || "[tool call]", phase: "commentary" },
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
