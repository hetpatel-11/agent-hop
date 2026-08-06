# handoff

Search across every local coding-agent session on your machine — Claude Code,
Codex, OpenCode, Pi, Muse, and Grok Build — and resume any one of them in any
other agent. Not a summary, not a briefing: a genuine native resume, so the
target agent picks up with the real conversation history.

## Install

```bash
npm install -g handoff
```

(Not published yet — for now, clone and build locally, see [Development](#development).)

## Usage

```bash
handoff
```

Walks you through:

1. **Which agent(s) to search?** — all six, or restrict to one
2. **Search for:** — fuzzy match across session titles, snippets, and project paths
3. **Pick a session** — numbered list with tool, title, project, and recency
4. **Resume in which agent?** — defaults to the same tool (native resume), or
   pick a different one to convert into that tool's format first
5. Launches you directly into the resumed session

Non-interactive / scriptable form:

```bash
handoff "auth migration" --agent claude --resume-in codex
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
| Muse Code | ✅ | ✅ (echo-provider skeleton + content splice — see below) |
| Grok Build | ✅ | ✅ |

Every adapter has been verified with a real live model call actually
recalling injected content across a resume, except Muse, which is verified
structurally (zero-error load, correct content in the trace) without spending
against a real model.

## How it works

Each tool stores sessions on disk in its own format — some as flat JSONL
files, some behind an official export/import CLI, one as an event-sourced
trace log. `handoff` normalizes all of them to one shape:

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
- **Muse Code**: event-sourced trace log, no documented way to hand-author a
  session. `write` generates a real skeleton via `muse exec --provider echo`
  (genuinely muse-authored, zero model cost), then splices real content into
  the placeholder prompt/response fields.
- **Grok Build**: `chat_history.jsonl` + `summary.json` per session, directory
  keyed by URL-encoded cwd.

## Development

```bash
git clone <this-repo>
cd handoff
npm install
npm run build
npm link   # makes `handoff` available globally, pointing at your local build
```

`npm run dev` runs the CLI directly via `tsx`, no build step needed.

## Known limitations

- The Muse and OpenCode adapters shell out to their respective CLIs at
  write-time, so cross-tool conversion *into* those formats only works on
  machines that have them installed.
- Not yet published to npm.
