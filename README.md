<p align="center">
  <img src="assets/screenshot.png" alt="agentresume searching local agent sessions" width="100%">
</p>

# agentresume

If you use more than one coding agent — Claude Code, Codex, OpenCode, Pi,
Grok Build — you've hit this: a great conversation happens in one of them,
and then it's stuck there. Switching agents (or just losing track of which
one you used) means starting over: re-explaining the bug, the codebase, the
decisions you already made.

`agentresume` finds any past conversation across all of them from one search
box, and drops you back into it — in the *same* agent, or a completely
different one — with the real conversation history intact, not a summary.
The target agent picks up exactly where you left off, because as far as it
can tell, it's the one that had the conversation.

**What it saves you:** the 10-15 minutes of re-explaining context every time
you want to pick a thread back up, plus never having to remember "which
tool was I even using for that."

## Install

```bash
npm install -g agentresume
```

## Usage

```bash
agentresume
```

Walks you through:

1. **Which agent(s) to search?** — all five, or restrict to one
2. **Search for:** — fuzzy match across session titles, snippets, and project paths
3. **Pick a session** — numbered list with tool, title, project, and recency
4. **Resume in which agent?** — defaults to the same tool (native resume), or
   pick a different one to convert into that tool's format first
5. Launches you directly into the resumed session

Non-interactive / scriptable form:

```bash
agentresume "auth migration" --agent claude --resume-in codex
```

| Flag | Description |
|---|---|
| `-a, --agent <tool>` | Restrict search to one agent |
| `-r, --resume-in <tool>` | Resume the picked session in this agent (default: same tool) |

## Supported agents

| Agent | Same-tool resume | Cross-tool resume (write into this format) |
|---|---|---|
| Claude Code | ✅ | ✅ |
| Codex | ✅ | ✅ |
| OpenCode | ✅ | ✅ |
| Pi | ✅ | ✅ |
| Grok Build | ✅ | ✅ |

Every adapter has been verified with a real live model call actually
recalling injected content across a resume.

Muse Code support was removed for now — the write/resume path needs live
Muse API access to verify correctly, which isn't available here. May come
back once that's testable end-to-end.

## How it works

Each tool stores sessions on disk in its own format — some as flat JSONL
files, some behind an official export/import CLI, one as an event-sourced
trace log. `agentresume` normalizes all of them to one shape:

```ts
interface Turn { role: "user" | "assistant"; text: string }
```

Each adapter (`src/adapters/<tool>.ts`) implements:

- `listSessions()` — cheap metadata scan for search
- `read(ref)` — full conversation → `Turn[]`
- `write(turns, projectPath)` — `Turn[]` → a new session in that tool's
  native format, indistinguishable from one the tool created itself
- `resumeCmd(sessionId, projectPath)` — the actual command to exec into

Adding a new agent means writing one new adapter file; nothing else changes.

### Per-tool notes

- **Claude Code**: raw JSONL under `~/.claude/projects/<encoded-cwd>/`. The
  directory name replaces *every* non-alphanumeric character with `-` (not
  just `/`) — a real gotcha if you don't match it exactly.
- **Codex**: raw JSONL rollout files under `~/.codex/sessions/YYYY/MM/DD/`.
- **OpenCode**: uses the official `opencode export`/`opencode import`
  commands rather than writing to its SQLite store directly. Message/part IDs
  must be genuinely unique per session — OpenCode's schema uses them as
  primary keys with `onConflictDoNothing()`, so a repeated ID silently no-ops
  the insert with zero visible error.
- **Pi**: JSONL under `~/.pi/agent/sessions/--<encoded-cwd>--/`. Unlike
  Claude, Pi only replaces `/`, leaving `_` and `.` in path components intact.
- **Grok Build**: `chat_history.jsonl` + `summary.json` per session, directory
  keyed by URL-encoded cwd.

## Development

```bash
git clone https://github.com/hetpatel-11/agentresume.git
cd agentresume
npm install
npm run build
npm link   # makes `agentresume` available globally, pointing at your local build
```

`npm run dev` runs the CLI directly via `tsx`, no build step needed.
