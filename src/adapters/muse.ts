import { mkdirSync, existsSync } from "node:fs";
import { join } from "node:path";
import { homedir } from "node:os";
import { randomUUID } from "node:crypto";
import { execFileSync } from "node:child_process";
import type { Adapter, SessionRef, Turn } from "../types.js";
import { readJsonlLines, readJsonlLinesLazy, findFiles, mtimeMs } from "../util.js";
import { readFileSync, writeFileSync } from "node:fs";

const SESSIONS_DIR = join(homedir(), ".local", "share", "muse", "sessions");

function hasMuse(): boolean {
  try {
    execFileSync("which", ["muse"], { stdio: "ignore" });
    return true;
  } catch {
    return false;
  }
}

const MAX_BODY_CHARS = 40000;

async function listSessions(): Promise<SessionRef[]> {
  const files = findFiles(SESSIONS_DIR, (p) => p.endsWith("session.jsonl") && !p.includes("/subagent/"));
  const out: SessionRef[] = [];
  for (const file of files) {
    const sessionId = file.split("/").slice(-2, -1)[0];
    let cwd: string | undefined;
    let firstPrompt = "";
    let body = "";
    for await (const d of readJsonlLinesLazy(file)) {
      const payload = d.payload as { kind?: string; record?: Record<string, unknown>; event?: Record<string, unknown> } | undefined;
      if (payload?.kind === "route_facts") {
        cwd = (payload.record as { cwd?: string } | undefined)?.cwd;
      }
      const rec = payload?.record as { kind?: string; command?: { kind?: string; prompt?: string } } | undefined;
      if (rec?.kind === "received" && rec.command?.kind === "turn_submit" && rec.command.prompt) {
        if (!firstPrompt) firstPrompt = rec.command.prompt;
        if (body.length < MAX_BODY_CHARS) body += rec.command.prompt + " ";
      }
      const ev = payload?.event as { kind?: string; text?: string } | undefined;
      if (ev?.kind === "assistant_message_committed" && ev.text) {
        if (body.length < MAX_BODY_CHARS) body += ev.text + " ";
      }
      if (cwd && firstPrompt && body.length >= MAX_BODY_CHARS) break;
    }
    if (!cwd) continue;
    out.push({
      tool: "muse",
      sessionId,
      projectPath: cwd,
      title: firstPrompt.slice(0, 80) || "(empty)",
      snippet: firstPrompt.slice(0, 200),
      body: body.slice(0, MAX_BODY_CHARS),
      updatedAt: mtimeMs(file),
      raw: { file },
    });
  }
  return out;
}

async function read(ref: SessionRef): Promise<Turn[]> {
  const file = ref.raw?.file as string;
  const turns: Turn[] = [];
  for (const d of readJsonlLines(file)) {
    const payload = d.payload as { record?: Record<string, unknown>; event?: Record<string, unknown> } | undefined;
    const rec = payload?.record as { kind?: string; command?: { kind?: string; prompt?: string } } | undefined;
    if (rec?.kind === "received" && rec.command?.kind === "turn_submit" && rec.command.prompt) {
      turns.push({ role: "user", text: rec.command.prompt });
    }
    const ev = payload?.event as { kind?: string; text?: string } | undefined;
    if (ev?.kind === "assistant_message_committed" && ev.text) {
      turns.push({ role: "assistant", text: ev.text });
    }
  }
  return turns;
}

/**
 * Muse's format is event-sourced -- there's no officially documented way to
 * hand-author a session file. `write` generates a real skeleton via
 * `muse exec --provider echo` (a genuinely valid, muse-authored event log,
 * zero model cost) then substitutes the placeholder prompt/response text with
 * real content. Slower than the other adapters (shells out once per user
 * turn) but every event stays structurally authentic instead of guessed.
 */
async function write(turns: Turn[], projectPath: string): Promise<string> {
  if (!hasMuse()) throw new Error("muse CLI not found on PATH");
  mkdirSync(projectPath, { recursive: true });

  const newId = randomUUID();
  const userTurns = turns.filter((t) => t.role === "user");
  if (userTurns.length === 0) throw new Error("muse: need at least one user turn to seed a session");

  for (let i = 0; i < userTurns.length; i++) {
    execFileSync(
      "muse",
      ["exec", "--provider", "echo", "--session-id", newId, "--workspace", projectPath, `handoff placeholder ${i}`],
      { stdio: "pipe", timeout: 60_000 }
    );
  }

  const matches = findFiles(SESSIONS_DIR, (p) => p.endsWith(`/${newId}/session.jsonl`));
  if (matches.length === 0) throw new Error(`muse: could not locate generated session ${newId}`);
  const sessionFile = matches[0];

  const lines = readJsonlLines(sessionFile);
  const userReplacements = turns.filter((t) => t.role === "user").map((t) => t.text);
  const assistantReplacements = turns.filter((t) => t.role === "assistant").map((t) => t.text);
  let ui = 0;
  let ai = 0;

  for (const d of lines) {
    const payload = d.payload as { record?: Record<string, unknown>; event?: Record<string, unknown> } | undefined;
    const rec = payload?.record as { kind?: string; command?: { kind?: string; prompt?: string } } | undefined;
    if (rec?.kind === "received" && rec.command?.kind === "turn_submit" && rec.command.prompt?.startsWith("handoff placeholder") && ui < userReplacements.length) {
      rec.command.prompt = userReplacements[ui];
      ui++;
    }
    const ev = payload?.event as { kind?: string; prompt?: unknown; text?: string } | undefined;
    if (ev?.kind === "started" && typeof ev.prompt === "string" && ev.prompt.startsWith("handoff placeholder")) {
      const idx = parseInt(ev.prompt.split(" ").pop()!, 10);
      if (idx < userReplacements.length) ev.prompt = userReplacements[idx];
    }
    if (ev?.kind === "assistant_message_committed" && ai < assistantReplacements.length) {
      ev.text = assistantReplacements[ai];
      ai++;
    }
  }

  writeFileSync(sessionFile, lines.map((d) => JSON.stringify(d)).join("\n") + "\n");
  return newId;
}

function resumeCmd(sessionId: string, projectPath: string): string[] {
  // muse's `resume` command is interactive/TUI-only; headless continuation uses --session-id.
  return ["muse", "exec", "--session-id", sessionId, "--workspace", projectPath];
}

export const museAdapter: Adapter = { tool: "muse", listSessions, read, write, resumeCmd };
