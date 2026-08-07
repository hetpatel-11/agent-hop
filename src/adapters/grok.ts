import { mkdirSync, writeFileSync, realpathSync } from "node:fs";
import { join } from "node:path";
import { homedir } from "node:os";
import { randomUUID } from "node:crypto";
import type { Adapter, SessionRef, Turn } from "../types.js";
import { readJsonlLines, readJsonlLinesLazy, findFiles, mtimeMs, cleanTitle, BodySampler } from "../util.js";

const SESSIONS_DIR = join(homedir(), ".grok", "sessions");

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

async function read(ref: SessionRef): Promise<Turn[]> {
  const file = ref.raw?.file as string;
  const turns: Turn[] = [];
  for (const obj of readJsonlLines(file)) {
    if (obj.type === "user" && Array.isArray(obj.content)) {
      for (const b of obj.content as { text?: string }[]) {
        const q = extractUserQuery(b.text ?? "");
        if (q) {
          turns.push({ role: "user", text: q });
          break;
        }
      }
    } else if (obj.type === "assistant" && typeof obj.content === "string" && obj.content.trim()) {
      turns.push({ role: "assistant", text: obj.content.trim() });
    }
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
      lines.push(
        JSON.stringify({
          type: "user",
          content: [{ type: "text", text: `<user_query>\n${turn.text}\n</user_query>` }],
          prompt_index: promptIdx,
        })
      );
      promptIdx++;
    } else {
      lines.push(
        JSON.stringify({
          type: "assistant",
          content: turn.text,
          model_id: "grok-4.5-build",
          model_fingerprint: "fp_handoff",
          reasoning_effort: "low",
        })
      );
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
      updatePromptIdx++;
    } else {
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
