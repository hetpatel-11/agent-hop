import { mkdirSync, writeFileSync, realpathSync } from "node:fs";
import { join } from "node:path";
import { homedir } from "node:os";
import { randomUUID } from "node:crypto";
import type { Adapter, SessionRef, Turn, ToolCallRecord, Attachment } from "../types.js";
import { readJsonlLines, readJsonlLinesLazy, readJsonlTailLines, findFiles, mtimeMs, MIN_TITLE_CHARS, cleanTitle, BodySampler, truncate, MAX_TOOL_OUTPUT_CHARS, sanitizeToolName, toToolInputObject } from "../util.js";

const SESSIONS_DIR = process.env.PI_CODING_AGENT_DIR !== undefined ? join(process.env.PI_CODING_AGENT_DIR, "sessions") : join(homedir(), ".pi", "agent", "sessions");

function encodeDir(cwd: string): string {
  let real = cwd;
  try {
    real = realpathSync(cwd);
  } catch {
    // ignore
  }
  return `--${real.replace(/^[/\\]/, "").replace(/[/\\:]/g, "-")}--`;
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
    let stoppedEarly = false;
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
      if (sessionId && cwd && titleText && body.hasHead()) {
        stoppedEarly = true;
        break;
      }
    }
    if (stoppedEarly) {
      body.markSampled();
      for (const obj of readJsonlTailLines(file)) {
        if (obj.type !== "message") continue;
        const message = obj.message as { role?: string; content?: unknown } | undefined;
        if (message?.role !== "user" && message?.role !== "assistant") continue;
        if (!Array.isArray(message.content)) continue;
        const parts = message.content
          .filter((b): b is { type: string; text: string } => typeof b === "object" && b !== null && (b as { type?: string }).type === "text")
          .map((b) => b.text);
        const text = parts.join("\n").trim();
        if (text) body.append(text);
      }
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

/** Pi declares `api: "anthropic-messages"` (see write() below) but its own
 * real image content block is flat -- {type:"image", mimeType, data} --
 * NOT Claude's nested {type:"image", source:{type:"base64", media_type,
 * data}}. Wrongly assumed they matched; caught by generating a real image
 * through the actual `pi` CLI (`pi -p @file.png "..."`) and inspecting the
 * real session file, which returned zero images under the nested-shape
 * assumption despite the raw file clearly having image content. */
function extractPiAttachments(content: unknown[]): Attachment[] {
  return content
    .filter((b): b is Record<string, unknown> => typeof b === "object" && b !== null && (b as { type?: string }).type === "image")
    .map((b) => (typeof b.data === "string" ? { mimeType: typeof b.mimeType === "string" ? b.mimeType : "image/png", base64: b.data } : null))
    .filter((x): x is Attachment => x !== null);
}

/** Pi's own message shape declares `api: "anthropic-messages"`, but tool
 * calls/results aren't nested the way Claude's are -- a toolCall is a
 * content block inside an assistant message (`{type:"toolCall", id, name,
 * arguments}`), while its result is an entirely separate message with its
 * own role: `{role:"toolResult", toolCallId, toolName, content}`. Both get
 * folded into the enclosing assistant turn, matched by id, same as the
 * Codex/Claude adapters. */
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
    if (obj.type !== "message") continue;
    const message = obj.message as { role?: string; content?: unknown; toolCallId?: string } | undefined;
    const role = message?.role;

    if (role === "toolResult") {
      const rec = message?.toolCallId ? callIndex.get(message.toolCallId) : undefined;
      if (rec) {
        let out = "";
        if (Array.isArray(message?.content)) {
          const contentArr = message!.content as unknown[];
          out = contentArr
            .filter((b): b is { type: string; text: string } => typeof b === "object" && b !== null && (b as { type?: string }).type === "text")
            .map((b) => b.text)
            .join("\n");
          // A toolResult's content can carry an image block too (same flat
          // shape as everywhere else in Pi) -- e.g. our own synthetic
          // attachment tool result, or a real tool that returns an image
          // directly. Extract it into a real attachment instead of losing it.
          pendingAttachments.push(...extractPiAttachments(contentArr));
        } else if (typeof message?.content === "string") {
          out = message.content;
        }
        rec.output = truncate(out, MAX_TOOL_OUTPUT_CHARS);
      }
      continue;
    }

    if (role !== "user" && role !== "assistant") continue;
    if (!Array.isArray(message?.content)) continue;
    const content = message.content as unknown[];

    if (role === "assistant") {
      for (const b of content) {
        if (!b || typeof b !== "object") continue;
        const block = b as Record<string, unknown>;
        if (block.type === "text" && typeof block.text === "string") {
          const t = block.text.trim();
          if (t) assistantTextParts.push(t);
        } else if (block.type === "toolCall") {
          const rec: ToolCallRecord = { name: typeof block.name === "string" ? block.name : "unknown_tool", input: JSON.stringify(block.arguments ?? {}) };
          pendingToolCalls.push(rec);
          if (typeof block.id === "string") callIndex.set(block.id, rec);
        }
      }
      pendingAttachments.push(...extractPiAttachments(content));
      continue;
    }

    // role === "user" -- a genuine human turn, flush whatever assistant
    // activity (text + tool calls) accumulated before it.
    flushAssistant();
    const parts = content
      .filter((b): b is { type: string; text: string } => typeof b === "object" && b !== null && (b as { type?: string }).type === "text")
      .map((b) => b.text);
    const text = parts.join("\n").trim();
    const attachments = extractPiAttachments(content);
    if (text || attachments.length) turns.push({ role: "user", text, attachments: attachments.length ? attachments : undefined });
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
    // Real toolCall content blocks + a separate toolResult message, not
    // inlined text -- confirmed safe by actually forging one (with a tool
    // name pi never registered, e.g. Codex's "exec_command") and resuming
    // it for real: it loaded with zero schema/validation error (the one
    // real error hit during testing was from an incomplete hand-rolled
    // usage object, not from the tool call shape itself -- fixed by
    // reusing this exact real assistant-message template). Pi's loader
    // just replays whatever's in the transcript.
    // Pi's own @file mechanism inlines non-image attachments as readable
    // text (<file name="...">...content...</file>), extracted by pi itself
    // -- we only have base64 bytes, not extracted text, so non-image
    // attachments get a placeholder note instead of forged binary-as-text.
    const nonImageNote = (turn.attachments ?? [])
      .filter((a) => !a.mimeType.startsWith("image/"))
      .map((a) => `[attached file: ${a.filename ?? "unnamed"} (${a.mimeType})]`)
      .join("\n");
    const combinedText = [turn.text, nonImageNote].filter(Boolean).join("\n\n");
    // Real shape confirmed by generating an actual image through the pi
    // CLI: flat {type, mimeType, data}, not Claude's nested `source` object.
    const imageBlocks = (turn.attachments ?? []).filter((a) => a.mimeType.startsWith("image/")).map((img) => ({ type: "image", mimeType: img.mimeType, data: img.base64 }));
    const toolCalls = turn.toolCalls ?? [];
    const toolCallIds = toolCalls.map(() => `call-${randomUUID()}-0`);
    const toolCallBlocks = toolCalls.map((tc, i) => ({ type: "toolCall", id: toolCallIds[i], name: sanitizeToolName(tc.name), arguments: toToolInputObject(tc.input) }));
    // Pi declares `api: "anthropic-messages"` (see below) -- the same
    // backing API as claude.ts, which was confirmed for real to reject
    // image blocks on assistant messages ("'image' blocks are not permitted
    // within assistant turns"). Rather than assume Pi shares that
    // restriction, an assistant-side image is carried as a synthetic
    // toolCall's result instead, matching the same pattern already proven
    // safe for Claude/Grok -- correct either way, and avoids relying on an
    // untested assumption about a restriction we can't currently verify
    // live (Pi testing here is blocked by an unrelated account usage limit).
    const attachmentToolCallId = turn.role === "assistant" && imageBlocks.length ? `call-${randomUUID()}-attachment` : null;
    const allToolCallBlocks = attachmentToolCallId
      ? [...toolCallBlocks, { type: "toolCall", id: attachmentToolCallId, name: "imported_attachment", arguments: {} }]
      : toolCallBlocks;
    const message: Record<string, unknown> = {
      role: turn.role,
      content: [
        ...(turn.role === "user" ? imageBlocks : []),
        ...(combinedText ? [{ type: "text", text: combinedText }] : []),
        ...allToolCallBlocks,
      ],
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

    // toolResult is its own message with a dedicated role, per Pi's own
    // convention -- see read() above -- one per tool call, matched by id.
    for (let i = 0; i < toolCalls.length; i++) {
      const resultId = randomUUID().replace(/-/g, "").slice(0, 8);
      lines.push(
        JSON.stringify({
          type: "message",
          id: resultId,
          parentId,
          timestamp: new Date().toISOString(),
          message: {
            role: "toolResult",
            toolCallId: toolCallIds[i],
            toolName: sanitizeToolName(toolCalls[i].name),
            content: [{ type: "text", text: toolCalls[i].output ?? "" }],
            timestamp: Date.now(),
          },
        })
      );
      parentId = resultId;
    }
    if (attachmentToolCallId) {
      const resultId = randomUUID().replace(/-/g, "").slice(0, 8);
      lines.push(
        JSON.stringify({
          type: "message",
          id: resultId,
          parentId,
          timestamp: new Date().toISOString(),
          message: {
            role: "toolResult",
            toolCallId: attachmentToolCallId,
            toolName: "imported_attachment",
            content: imageBlocks,
            timestamp: Date.now(),
          },
        })
      );
      parentId = resultId;
    }
  }

  writeFileSync(outPath, lines.join("\n") + "\n");
  return newId;
}

function resumeCmd(sessionId: string, _projectPath: string): string[] {
  return ["pi", "--session", sessionId];
}

export const piAdapter: Adapter = { tool: "pi", listSessions, read, write, resumeCmd };
