import { mkdirSync, writeFileSync, realpathSync } from "node:fs";
import { join } from "node:path";
import { homedir } from "node:os";
import { randomUUID } from "node:crypto";
import type { Adapter, SessionRef, Turn, ToolCallRecord, Attachment } from "../types.js";
import { readJsonlLines, readJsonlLinesLazy, readJsonlTailLines, findFiles, mtimeMs, cleanTitle, BodySampler, truncate, MAX_TOOL_OUTPUT_CHARS } from "../util.js";

const SESSIONS_DIR = join(homedir(), ".grok", "sessions");

function safeJsonParse(s: string): unknown {
  try {
    return JSON.parse(s);
  } catch {
    return { raw: s };
  }
}

function extractUserQuery(text: string): string | null {
  const start = text.indexOf("<user_query>");
  const end = text.indexOf("</user_query>");
  if (start === -1 || end === -1) return null;
  return text.slice(start + "<user_query>".length, end).trim();
}

const MAX_BODY_CHARS = 40000;

async function listSessions(): Promise<SessionRef[]> {
  const summaryFiles = findFiles(SESSIONS_DIR, (p) => p.endsWith("summary.json"));
  const out: SessionRef[] = [];
  for (const summaryFile of summaryFiles) {
    const sessionDir = summaryFile.replace(/\/summary\.json$/, "");
    const chatFile = join(sessionDir, "chat_history.jsonl");
    let summary: Record<string, unknown>;
    try {
      const { readFileSync } = await import("node:fs");
      summary = JSON.parse(readFileSync(summaryFile, "utf-8"));
    } catch {
      continue;
    }
    const info = summary.info as { id?: string; cwd?: string } | undefined;
    const sessionId = info?.id;
    const cwd = info?.cwd;
    if (!sessionId || !cwd) continue;

    let firstUserText = "";
    const body = new BodySampler(MAX_BODY_CHARS);
    let stoppedEarly = false;
    for await (const obj of readJsonlLinesLazy(chatFile)) {
      if (obj.type === "user" && Array.isArray(obj.content)) {
        for (const b of obj.content as { text?: string }[]) {
          const q = extractUserQuery(b.text ?? "");
          if (q) {
            if (!firstUserText) firstUserText = q;
            body.append(q);
            break;
          }
        }
      } else if (obj.type === "assistant" && typeof obj.content === "string" && obj.content.trim()) {
        body.append(obj.content.trim());
      }
      if (firstUserText && body.hasHead()) {
        stoppedEarly = true;
        break;
      }
    }
    if (stoppedEarly) {
      body.markSampled();
      for (const obj of readJsonlTailLines(chatFile)) {
        if (obj.type === "user" && Array.isArray(obj.content)) {
          for (const b of obj.content as { text?: string }[]) {
            const q = extractUserQuery(b.text ?? "");
            if (q) {
              body.append(q);
              break;
            }
          }
        } else if (obj.type === "assistant" && typeof obj.content === "string" && obj.content.trim()) {
          body.append(obj.content.trim());
        }
      }
    }

    out.push({
      tool: "grok",
      sessionId,
      projectPath: cwd,
      title: cleanTitle((summary.generated_title as string) || firstUserText) || "(empty)",
      snippet: firstUserText.slice(0, 200),
      body: body.value(),
      updatedAt: mtimeMs(chatFile),
      raw: { file: chatFile },
    });
  }
  return out;
}

/** chat_history.jsonl (used by listSessions() above) has no tool-call data
 * at all -- checked every local session, genuinely absent. It turns out
 * that was looking in the wrong file: updates.jsonl (the ACP `session/
 * update` display stream, already written for the TUI) DOES carry real
 * tool_call/tool_call_update events with actual input/output, confirmed by
 * generating real sessions with `grok -p ... --always-approve` and
 * inspecting the output directly. It's also a complete substitute for
 * chat_history.jsonl's text turns (one user_message_chunk/
 * agent_message_chunk per real turn, by construction -- see write()
 * below), so read() sources from this single richer file instead of two. */
async function read(ref: SessionRef): Promise<Turn[]> {
  const chatFile = ref.raw?.file as string;
  const updatesFile = chatFile.replace(/chat_history\.jsonl$/, "updates.jsonl");
  const lines = readJsonlLines(updatesFile);
  const turns: Turn[] = [];

  // Images arrive as their OWN user_message_chunk event, separate from the
  // text chunk for the same turn (confirmed by generating a real session
  // with `grok --prompt-json` and a real image) -- both need buffering
  // until the turn actually changes, not pushed as a Turn immediately.
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
    const text = assistantTextParts.join("").trim();
    if (text || pendingToolCalls.length > 0 || pendingAttachments.length > 0) {
      turns.push({ role: "assistant", text, toolCalls: pendingToolCalls.length ? pendingToolCalls : undefined, attachments: pendingAttachments.length ? pendingAttachments : undefined });
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

  for (const obj of lines) {
    const params = obj.params as { update?: Record<string, unknown> } | undefined;
    const update = params?.update;
    if (!update) continue;
    const kind = update.sessionUpdate;

    if (kind === "user_message_chunk") {
      ensureRole("user");
      const content = update.content as { type?: string; text?: string; data?: string; mimeType?: string } | undefined;
      if (content?.type === "image" && typeof content.data === "string") {
        userAttachments.push({ mimeType: content.mimeType ?? "image/png", base64: content.data });
      } else if (typeof content?.text === "string" && content.text.trim()) {
        userTextParts.push(content.text.trim());
      }
      continue;
    }
    if (kind === "agent_message_chunk") {
      ensureRole("assistant");
      // Chunks stream incrementally during generation -- concatenate every
      // chunk for this turn, don't just take the first/last one.
      const text = (update.content as { text?: string } | undefined)?.text ?? "";
      if (text) assistantTextParts.push(text);
      continue;
    }
    if (kind === "tool_call") {
      ensureRole("assistant");
      const meta = (update._meta as Record<string, unknown> | undefined)?.["x.ai/tool"] as { name?: string } | undefined;
      const name = meta?.name ?? (typeof update.title === "string" ? update.title : "unknown_tool");
      const rec: ToolCallRecord = { name, input: JSON.stringify(update.rawInput ?? {}) };
      pendingToolCalls.push(rec);
      if (typeof update.toolCallId === "string") callIndex.set(update.toolCallId, rec);
      continue;
    }
    if (kind === "tool_call_update") {
      const rec = typeof update.toolCallId === "string" ? callIndex.get(update.toolCallId) : undefined;
      if (!rec) continue;
      // Multiple updates can arrive per call (in_progress, then completed)
      // -- each overwrite just keeps the latest, which ends up being the
      // final state once the stream finishes. A tool result can carry a
      // real image too (e.g. a "read image file" tool after extracting an
      // embedded image from a PDF -- confirmed by generating a real session
      // where Codex/Pi both silently failed to capture an embedded PDF
      // image at all, but Grok's own image-reading tool returned the
      // actual bytes right here in the tool result). Same flat {type,
      // data, mimeType} shape as the user_message_chunk image case above.
      let out = "";
      if (Array.isArray(update.content)) {
        const items = update.content as { type?: string; content?: { type?: string; text?: string; data?: string; mimeType?: string } }[];
        out = items
          .map((c) => c.content?.text ?? "")
          .filter(Boolean)
          .join("\n");
        for (const c of items) {
          if (c.content?.type === "image" && typeof c.content.data === "string") {
            pendingAttachments.push({ mimeType: c.content.mimeType ?? "image/png", base64: c.content.data });
          }
        }
      }
      if (!out && update.rawOutput !== undefined) out = JSON.stringify(update.rawOutput);
      if (out) rec.output = truncate(out, MAX_TOOL_OUTPUT_CHARS);
      continue;
    }
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
  const encodedCwd = encodeURIComponent(realCwd);

  const newId = randomUUID();
  const sessionDir = join(SESSIONS_DIR, encodedCwd, newId);
  mkdirSync(sessionDir, { recursive: true });

  const now = new Date();
  const lines: string[] = [
    JSON.stringify({
      type: "system",
      content: "You are Grok, an interactive CLI tool that helps users with software engineering tasks.",
    }),
    JSON.stringify({
      type: "user",
      content: [
        {
          type: "text",
          text: `<user_info>\nOS Version: macos\nShell: /bin/zsh\nWorkspace Path: ${realCwd}\nToday's date: ${now.toISOString().slice(0, 10)}\n</user_info>`,
        },
      ],
    }),
  ];

  let promptIdx = 0;
  for (const turn of turns) {
    if (turn.role === "user") {
      // Real chat_history.jsonl image shape confirmed by generating an
      // actual grok session with `--prompt-json` and a real image: a
      // `{type:"image", url:"data:...;base64,..."}` block in the same
      // message's content array as the text block (Codex-style url field,
      // not Claude/Pi's nested `source` object).
      const imageBlocks = (turn.attachments ?? []).filter((a) => a.mimeType.startsWith("image/")).map((img) => ({ type: "image", url: `data:${img.mimeType};base64,${img.base64}` }));
      // Grok's own @file mechanism inlines non-image attachments as
      // readable text (<attached_files><file_contents>...</file_contents>),
      // extracted by grok itself -- we only have base64 bytes, not
      // extracted text, so non-image attachments get a placeholder note.
      const nonImageNote = (turn.attachments ?? [])
        .filter((a) => !a.mimeType.startsWith("image/"))
        .map((a) => `[attached file: ${a.filename ?? "unnamed"} (${a.mimeType})]`)
        .join("\n");
      const queryText = [turn.text, nonImageNote].filter(Boolean).join("\n\n");
      lines.push(
        JSON.stringify({
          type: "user",
          content: [{ type: "text", text: `<user_query>\n${queryText}\n</user_query>` }, ...imageBlocks],
          prompt_index: promptIdx,
        })
      );
      promptIdx++;
    } else {
      // Real tool_calls shape confirmed by generating an actual grok
      // session and inspecting chat_history.jsonl directly: a flat
      // {id, name, arguments} array on the assistant entry (not
      // OpenAI-style {type:"function", function:{...}}), each followed by
      // its own separate tool_result entry -- the same native structure
      // updates.jsonl already used below, now applied here too so the API
      // continuation path (what chat_history.jsonl backs) carries real
      // tool-call fidelity instead of a text rendering.
      const realToolCalls = (turn.toolCalls ?? []).map((tc) => ({ id: `call-${randomUUID()}-0`, name: tc.name, arguments: tc.input }));
      const assistantImages = (turn.attachments ?? []).filter((a) => a.mimeType.startsWith("image/"));
      const nonImageNote = (turn.attachments ?? [])
        .filter((a) => !a.mimeType.startsWith("image/"))
        .map((a) => `[attached file: ${a.filename ?? "unnamed"} (${a.mimeType})]`)
        .join("\n");
      // An assistant-side image (e.g. a tool that read/extracted one) has
      // no assistant-message-level slot in this format -- every real
      // example found one only inside a tool_result's own `images` array,
      // never attached to the assistant message directly (confirmed by
      // generating a real session where Grok's own image-reading tool
      // returned a real embedded-PDF image this way). Reproduced with a
      // synthetic tool_call/tool_result pair rather than forcing it
      // somewhere Grok's own sessions never actually put one.
      const imgToolCallId = assistantImages.length ? `call-${randomUUID()}-img` : null;
      const allToolCalls = [...realToolCalls, ...(imgToolCallId ? [{ id: imgToolCallId, name: "read_image", arguments: "{}" }] : [])];
      lines.push(
        JSON.stringify({
          type: "assistant",
          content: [turn.text, nonImageNote].filter(Boolean).join("\n\n"),
          model_id: "grok-4.5-build",
          model_fingerprint: "fp_handoff",
          reasoning_effort: "low",
          ...(allToolCalls.length ? { tool_calls: allToolCalls } : {}),
        })
      );
      (turn.toolCalls ?? []).forEach((tc, i) => {
        lines.push(
          JSON.stringify({
            type: "tool_result",
            tool_call_id: realToolCalls[i].id,
            content: tc.output ?? "",
          })
        );
      });
      if (imgToolCallId) {
        lines.push(
          JSON.stringify({
            type: "tool_result",
            tool_call_id: imgToolCallId,
            content: "Read image file",
            images: assistantImages.map((img) => ({ type: "image", url: `data:${img.mimeType};base64,${img.base64}` })),
          })
        );
      }
    }
  }

  writeFileSync(join(sessionDir, "chat_history.jsonl"), lines.join("\n") + "\n");

  // chat_history.jsonl alone launches `grok --resume` fine (it's what backs
  // API continuation) but the TUI doesn't show prior turns from it -- a real
  // session directory also has updates.jsonl, an Agent Client Protocol
  // (ACP) `session/update` event stream that's what the TUI actually
  // replays to render history. Without it, resume "works" (loads, no
  // crash, continues fine) but the conversation looks empty. Confirmed by
  // diffing a real session directory's file list against what this adapter
  // was writing (missing this file entirely) and inspecting its real event
  // schema directly.
  const sessionStartSec = Math.floor(now.getTime() / 1000);
  const updates: string[] = [];
  let updatePromptIdx = 0;
  turns.forEach((turn, i) => {
    const eventNum = i + 1;
    const ts = sessionStartSec + i;
    if (turn.role === "user") {
      updates.push(
        JSON.stringify({
          timestamp: ts,
          method: "session/update",
          params: {
            sessionId: newId,
            update: {
              sessionUpdate: "user_message_chunk",
              content: { type: "text", text: turn.text },
              _meta: { modelId: "grok-4.5", promptIndex: updatePromptIdx },
            },
            _meta: { eventId: `${newId}-${eventNum}`, agentTimestampMs: ts * 1000 },
          },
        })
      );
      // Real shape confirmed the same way as the tool-call events: images
      // arrive as their OWN user_message_chunk (flat {type, data,
      // mimeType}, not nested like Claude/Pi's `source` object), sharing
      // the same promptIndex as the text chunk for that turn.
      (turn.attachments ?? []).filter((a) => a.mimeType.startsWith("image/")).forEach((img, imgI) => {
        updates.push(
          JSON.stringify({
            timestamp: ts,
            method: "session/update",
            params: {
              sessionId: newId,
              update: {
                sessionUpdate: "user_message_chunk",
                content: { type: "image", data: img.base64, mimeType: img.mimeType },
                _meta: { modelId: "grok-4.5", promptIndex: updatePromptIdx },
              },
              _meta: { eventId: `${newId}-${eventNum}-img${imgI}`, agentTimestampMs: ts * 1000 },
            },
          })
        );
      });
      updatePromptIdx++;
    } else {
      // Real tool_call/tool_call_update shape, confirmed by generating an
      // actual grok session (`grok -p ... --always-approve`) and inspecting
      // its updates.jsonl directly -- not guessed. One tool_call (has the
      // input) plus one tool_call_update with status "completed" (has the
      // output) per call, matched by toolCallId, emitted before the
      // narration text that follows it (matches real ordering).
      for (const tc of turn.toolCalls ?? []) {
        const toolCallId = `call-${randomUUID()}-0`;
        updates.push(
          JSON.stringify({
            timestamp: ts,
            method: "session/update",
            params: {
              sessionId: newId,
              update: {
                sessionUpdate: "tool_call",
                toolCallId,
                title: tc.name,
                rawInput: safeJsonParse(tc.input),
                _meta: { "x.ai/tool": { version: 1, name: tc.name, kind: "execute", namespace: "grok_build", label: tc.name, read_only: false } },
              },
              _meta: { eventId: `${newId}-${eventNum}-tool`, agentTimestampMs: ts * 1000 },
            },
          })
        );
        updates.push(
          JSON.stringify({
            timestamp: ts,
            method: "session/update",
            params: {
              sessionId: newId,
              update: {
                sessionUpdate: "tool_call_update",
                toolCallId,
                status: "completed",
                content: [{ type: "content", content: { type: "text", text: tc.output ?? "" } }],
                rawOutput: tc.output ?? "",
              },
              _meta: { eventId: `${newId}-${eventNum}-tool-done`, agentTimestampMs: ts * 1000 },
            },
          })
        );
      }
      // Assistant-side images have no message-level slot in this format --
      // same synthetic tool_call/tool_call_update reproduction as
      // chat_history.jsonl above, matching where a real one actually
      // showed up (a tool result's content array), read back correctly by
      // this adapter's own tool_call_update image handling in read().
      for (const img of (turn.attachments ?? []).filter((a) => a.mimeType.startsWith("image/"))) {
        const toolCallId = `call-${randomUUID()}-img`;
        updates.push(
          JSON.stringify({
            timestamp: ts,
            method: "session/update",
            params: {
              sessionId: newId,
              update: { sessionUpdate: "tool_call", toolCallId, title: "read_image", rawInput: {} },
              _meta: { eventId: `${newId}-${eventNum}-imgtool`, agentTimestampMs: ts * 1000 },
            },
          })
        );
        updates.push(
          JSON.stringify({
            timestamp: ts,
            method: "session/update",
            params: {
              sessionId: newId,
              update: {
                sessionUpdate: "tool_call_update",
                toolCallId,
                status: "completed",
                content: [{ type: "content", content: { type: "image", data: img.base64, mimeType: img.mimeType } }],
              },
              _meta: { eventId: `${newId}-${eventNum}-imgtool-done`, agentTimestampMs: ts * 1000 },
            },
          })
        );
      }
      updates.push(
        JSON.stringify({
          timestamp: ts,
          method: "session/update",
          params: {
            sessionId: newId,
            update: {
              sessionUpdate: "agent_message_chunk",
              content: { type: "text", text: turn.text },
            },
            _meta: {
              totalTokens: 0,
              eventId: `${newId}-${eventNum}`,
              agentTimestampMs: ts * 1000,
              promptId: newId,
              streamStartMs: ts * 1000,
              turnStartMs: ts * 1000,
              updateType: "AgentMessageChunk",
              chunkId: eventNum,
            },
          },
        })
      );
    }
  });
  writeFileSync(join(sessionDir, "updates.jsonl"), updates.join("\n") + "\n");

  const nowIso = now.toISOString();
  const realTitle = (turns.find((t) => t.role === "user")?.text ?? "Resumed via agent-hop").slice(0, 80);
  const summary = {
    info: { id: newId, cwd: realCwd },
    session_summary: realTitle,
    created_at: nowIso,
    updated_at: nowIso,
    num_messages: turns.length,
    num_chat_messages: lines.length,
    current_model_id: "grok-4.5",
    next_trace_turn: 1,
    chat_format_version: 1,
    request_id: randomUUID(),
    grok_home: join(homedir(), ".grok"),
    last_active_at: nowIso,
    generated_title: realTitle,
    agent_name: "grok-build-plan",
    sandbox_profile: "off",
    reasoning_effort: "low",
  };
  writeFileSync(join(sessionDir, "summary.json"), JSON.stringify(summary, null, 2));

  return newId;
}

function resumeCmd(sessionId: string, _projectPath: string): string[] {
  return ["grok", "--resume", sessionId];
}

export const grokAdapter: Adapter = { tool: "grok", listSessions, read, write, resumeCmd };
