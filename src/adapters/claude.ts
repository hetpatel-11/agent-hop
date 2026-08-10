import { mkdirSync, writeFileSync, realpathSync } from "node:fs";
import { join } from "node:path";
import { homedir } from "node:os";
import { randomUUID } from "node:crypto";
import { execFileSync } from "node:child_process";
import type { Adapter, SessionRef, Turn, ToolCallRecord, Attachment } from "../types.js";
import { readJsonlLines, readJsonlLinesLazy, readJsonlTailLines, findFiles, mtimeMs, MIN_TITLE_CHARS, cleanTitle, BodySampler, truncate, MAX_TOOL_OUTPUT_CHARS } from "../util.js";

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
    let stoppedEarly = false;
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
      if (cwd && titleText && body.hasHead()) {
        stoppedEarly = true;
        break;
      }
    }
    if (stoppedEarly) {
      body.markSampled();
      for (const obj of readJsonlTailLines(file)) {
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
        if (text) body.append(text);
      }
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

/** Claude's own API convention represents a tool call as `tool_use` inside
 * an assistant message, and its result as `tool_result` inside a
 * *following* message with role "user" -- that's API plumbing, not
 * something the human actually typed. Both get folded into the enclosing
 * assistant turn (matched by id/tool_use_id) instead of surfacing as a fake
 * human message; only content blocks the human genuinely sent (or a real
 * final assistant reply) become their own Turn.
 *
 * Separately: `@file` mentions don't appear in the message content array at
 * all -- Claude Code logs them as an entirely distinct top-level record,
 * `{type:"attachment", attachment:{type:"file", filename, content:{type:
 * "text"|"pdf", file:{...}}}}`, arriving *after* the user message it
 * belongs to (confirmed by generating real sessions with a real @file.pdf
 * and @file.txt reference and inspecting the raw output -- not documented
 * anywhere, found by diffing the raw file). That's why user turns are now
 * buffered (like assistant turns already were) instead of pushed
 * immediately: the attachment record needs to land in the same turn as the
 * message that referenced it, and it arrives on a later line. The same
 * top-level `type:"attachment"` record also carries unrelated session
 * bookkeeping (hook results, skill/agent listings, deferred-tool deltas) --
 * only `attachment.type === "file"` is a real user-turn attachment. */
async function read(ref: SessionRef): Promise<Turn[]> {
  const file = ref.raw?.file as string;
  const lines = readJsonlLines(file);
  const turns: Turn[] = [];

  let userTextParts: string[] = [];
  let userAttachments: Attachment[] = [];
  let assistantTextParts: string[] = [];
  let pendingToolCalls: ToolCallRecord[] = [];
  let pendingAttachments: Attachment[] = [];
  const callIndex = new Map<string, ToolCallRecord>();
  let lastRole: "user" | "assistant" | null = null;

  const flushUser = () => {
    const text = userTextParts.join("\n\n").trim();
    if (text || userAttachments.length) turns.push({ role: "user", text, attachments: userAttachments.length ? userAttachments : undefined });
    userTextParts = [];
    userAttachments = [];
  };
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
  const ensureRole = (role: "user" | "assistant") => {
    if (lastRole !== null && lastRole !== role) {
      if (lastRole === "user") flushUser();
      else flushAssistant();
    }
    lastRole = role;
  };

  // Shared shape for both `image` and `document` content blocks -- same
  // {type, source:{type:"base64", media_type, data}} wrapper, just a
  // different `type` and `media_type`. Confirmed `document` is real (not
  // guessed) by generating a session with a real @file.pdf reference.
  const extractAttachmentBlock = (b: Record<string, unknown>): Attachment | null => {
    const src = b.source as { type?: string; media_type?: string; data?: string } | undefined;
    if (src?.type === "base64" && typeof src.data === "string") {
      return { mimeType: src.media_type ?? "application/octet-stream", base64: src.data };
    }
    return null;
  };

  for (const obj of lines) {
    if (obj.type === "attachment") {
      const att = obj.attachment as { type?: string; filename?: string; content?: { type?: string; file?: { content?: string; base64?: string } } } | undefined;
      if (att?.type === "file") {
        ensureRole("user"); // @file mentions are always something the human referenced
        const filename = att.filename ?? "unnamed";
        if (att.content?.type === "text" && typeof att.content.file?.content === "string") {
          // A real text file -- genuinely readable content, inline as text
          // (consistent with how Codex/Pi/Grok already handle this) rather
          // than needlessly base64-wrapping something that's already plain
          // text.
          userTextParts.push(`<file name="${filename}">\n${att.content.file.content}\n</file>`);
        } else if (typeof att.content?.file?.base64 === "string") {
          const mime = att.content.type === "pdf" ? "application/pdf" : "application/octet-stream";
          userAttachments.push({ mimeType: mime, base64: att.content.file.base64, filename });
        }
      }
      continue; // every other attachment.type is session bookkeeping, not user content
    }

    if (obj.type !== "user" && obj.type !== "assistant") continue;
    const message = obj.message as { role?: string; content?: unknown } | undefined;
    const role = message?.role;
    if (role !== "user" && role !== "assistant") continue;
    const content = message?.content;

    if (role === "assistant") {
      ensureRole("assistant");
      if (typeof content === "string") {
        if (content.trim()) assistantTextParts.push(content.trim());
      } else if (Array.isArray(content)) {
        for (const b of content) {
          if (!b || typeof b !== "object") continue;
          const block = b as Record<string, unknown>;
          if (block.type === "text" && typeof block.text === "string") {
            const t = block.text.trim();
            if (t) assistantTextParts.push(t);
          } else if (block.type === "tool_use") {
            const rec: ToolCallRecord = { name: typeof block.name === "string" ? block.name : "unknown_tool", input: JSON.stringify(block.input ?? {}) };
            pendingToolCalls.push(rec);
            if (typeof block.id === "string") callIndex.set(block.id, rec);
          } else if (block.type === "image" || block.type === "document") {
            const att = extractAttachmentBlock(block);
            if (att) pendingAttachments.push(att);
          }
        }
      }
      continue;
    }

    // role === "user" -- may be a genuine human message, a tool_result
    // envelope, or (per the API) technically a mix of both. Compute the
    // real content first so a pure tool_result envelope can be skipped
    // *without* touching role state -- it's API plumbing mid-assistant-turn,
    // not a real turn boundary, and must not flush the assistant's
    // still-in-progress tool-call accumulation.
    let hadToolResult = false;
    const localText: string[] = [];
    const localAttachments: Attachment[] = [];
    if (typeof content === "string") {
      if (content.trim()) localText.push(content.trim());
    } else if (Array.isArray(content)) {
      for (const b of content) {
        if (!b || typeof b !== "object") continue;
        const block = b as Record<string, unknown>;
        if (block.type === "tool_result") {
          hadToolResult = true;
          const id = block.tool_use_id;
          const rec = typeof id === "string" ? callIndex.get(id) : undefined;
          if (rec) {
            if (typeof block.content === "string") {
              rec.output = truncate(block.content, MAX_TOOL_OUTPUT_CHARS);
            } else if (Array.isArray(block.content)) {
              // A tool_result's content can itself carry image/document
              // blocks -- both our own synthetic attachment tool_result and
              // a genuine real tool (e.g. a screenshot/file-read tool) can
              // return one this way. Extract those into real attachments
              // rather than flattening the whole array to escaped JSON text.
              const textParts: string[] = [];
              for (const item of block.content as unknown[]) {
                if (!item || typeof item !== "object") continue;
                const itemBlock = item as Record<string, unknown>;
                if (itemBlock.type === "text" && typeof itemBlock.text === "string") {
                  textParts.push(itemBlock.text);
                } else if (itemBlock.type === "image" || itemBlock.type === "document") {
                  const att = extractAttachmentBlock(itemBlock);
                  if (att) pendingAttachments.push(att);
                }
              }
              if (textParts.length) rec.output = truncate(textParts.join("\n"), MAX_TOOL_OUTPUT_CHARS);
            } else {
              rec.output = truncate(JSON.stringify(block.content ?? ""), MAX_TOOL_OUTPUT_CHARS);
            }
          }
        } else if (block.type === "text" && typeof block.text === "string") {
          const t = block.text.trim();
          if (t) localText.push(t);
        } else if (block.type === "image" || block.type === "document") {
          const att = extractAttachmentBlock(block);
          if (att) localAttachments.push(att);
        }
      }
    }
    if (hadToolResult && localText.length === 0 && localAttachments.length === 0) continue;

    ensureRole("user");
    userTextParts.push(...localText);
    userAttachments.push(...localAttachments);
  }
  flushUser();
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
    // Real tool_use/tool_result blocks, not inlined text -- confirmed safe
    // by actually forging one (including a tool name Claude Code never
    // registered, e.g. Codex's "exec_command") and resuming it for real:
    // it loaded and the model correctly recalled the exact fake result,
    // with zero schema/validation error. Claude Code's loader just replays
    // whatever's in the transcript; it doesn't validate tool names against
    // a whitelist. This is what makes a resumed tool call read as a
    // genuine native tool card instead of markdown text glued onto a
    // message.
    const attachments = turn.attachments ?? [];
    const otherNote = attachments
      .filter((a) => !a.mimeType.startsWith("image/") && a.mimeType !== "application/pdf")
      .map((a) => `[attached file: ${a.filename ?? "unnamed"} (${a.mimeType})]`)
      .join("\n");
    const combinedText = [turn.text, otherNote].filter(Boolean).join("\n\n");
    const attachmentBlocks = attachments
      .filter((a) => a.mimeType.startsWith("image/") || a.mimeType === "application/pdf")
      .map((a) => ({ type: a.mimeType === "application/pdf" ? "document" : "image", source: { type: "base64", media_type: a.mimeType, data: a.base64 } }));

    let entry: Record<string, unknown>;
    if (turn.role === "user") {
      entry = {
        parentUuid,
        isSidechain: false,
        promptId: randomUUID(),
        type: "user",
        message: { role: "user", content: attachmentBlocks.length ? [...attachmentBlocks, ...(combinedText ? [{ type: "text", text: combinedText }] : [])] : combinedText },
        uuid: myUuid,
        timestamp: ts,
        userType: "external",
        entrypoint: "cli",
        cwd: realCwd,
        sessionId: newId,
        version: cliVersion,
      };
      lines.push(JSON.stringify(entry));
      parentUuid = myUuid;
      lastUuid = myUuid;
      continue;
    }

    const toolCalls = turn.toolCalls ?? [];
    const toolUseIds = toolCalls.map(() => `toolu_${randomUUID().replace(/-/g, "").slice(0, 24)}`);
    const toolUseBlocks = toolCalls.map((tc, i) => {
      let input: unknown = tc.input;
      try {
        input = JSON.parse(tc.input);
      } catch {
        // not JSON -- keep the raw string, still a valid input value
      }
      return { type: "tool_use", id: toolUseIds[i], name: tc.name, input };
    });
    // 'image'/'document' blocks are only permitted in user turns -- confirmed
    // for real: a resumed session with an attachment block on an assistant
    // message fails with "API Error: 400 ... 'image' blocks are not
    // permitted within assistant turns" the moment the conversation is
    // continued. A tool_result's content, unlike an assistant message's own
    // content, *is* allowed to carry image/document blocks (this is exactly
    // how a real screenshot/file-reading tool returns one) -- so an
    // assistant-side attachment is carried as the result of a synthetic
    // tool_use instead, the same pattern already used for Grok's identical
    // "no assistant-message-level attachment slot" constraint.
    const attachmentToolUseId = attachmentBlocks.length ? `toolu_${randomUUID().replace(/-/g, "").slice(0, 24)}` : null;
    const allToolUseBlocks = attachmentToolUseId
      ? [...toolUseBlocks, { type: "tool_use", id: attachmentToolUseId, name: "imported_attachment", input: {} }]
      : toolUseBlocks;
    entry = {
      parentUuid,
      isSidechain: false,
      message: {
        model: "claude-sonnet-5",
        id: `msg_${randomUUID().replace(/-/g, "").slice(0, 24)}`,
        type: "message",
        role: "assistant",
        content: [...(combinedText ? [{ type: "text", text: combinedText }] : []), ...allToolUseBlocks],
        stop_reason: allToolUseBlocks.length ? "tool_use" : "end_turn",
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
    lines.push(JSON.stringify(entry));
    parentUuid = myUuid;
    lastUuid = myUuid;

    // Real tool calls (and a synthetic attachment tool_use, if any) need a
    // matching tool_result reply (as its own "user" role message, per
    // Claude's own API convention -- see read() above) immediately after,
    // or the transcript is structurally incomplete.
    if (allToolUseBlocks.length) {
      const resultUuid = randomUUID();
      const resultContent: Record<string, unknown>[] = toolCalls.map((tc, i) => ({ type: "tool_result", tool_use_id: toolUseIds[i], content: tc.output ?? "" }));
      if (attachmentToolUseId) resultContent.push({ type: "tool_result", tool_use_id: attachmentToolUseId, content: attachmentBlocks });
      const resultEntry = {
        parentUuid,
        isSidechain: false,
        promptId: randomUUID(),
        type: "user",
        message: {
          role: "user",
          content: resultContent,
        },
        uuid: resultUuid,
        timestamp: new Date().toISOString(),
        userType: "external",
        entrypoint: "cli",
        cwd: realCwd,
        sessionId: newId,
        version: cliVersion,
      };
      lines.push(JSON.stringify(resultEntry));
      parentUuid = resultUuid;
      lastUuid = resultUuid;
    }
  }

  lines.unshift(JSON.stringify({ type: "last-prompt", leafUuid: lastUuid, sessionId: newId }));
  writeFileSync(outPath, lines.join("\n") + "\n");
  return newId;
}

function resumeCmd(sessionId: string, _projectPath: string): string[] {
  return ["claude", "--resume", sessionId];
}

export const claudeAdapter: Adapter = { tool: "claude", listSessions, read, write, resumeCmd };
