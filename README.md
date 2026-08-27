https://github.com/user-attachments/assets/f37e5ffa-fec3-46bd-8354-2353e2a2d8ad

demo

**A runtime for coding-agent harnesses.** Run Claude Code, Codex, OpenCode, Pi, and Grok inside one terminal, hop a live session between them, and search or resume any local chat.

# agent-hop

`ah` is the process you start. The harness runs inside it.

Claude Code, Codex, OpenCode, Pi, and Grok each have their own CLI, their own session files, and their own idea of "resume." None of them can take over a conversation another one just had. `agent-hop` is the runtime around those harnesses: a native Rust binary that opens the real agent in a pty, keeps a strip of chrome `ah` owns, and can move the live thread into the next harness without you leaving the terminal or starting over.

You work the way you already do. `claude` is still `claude`. Codex is still Codex. `ah` does not reimplement them, proxy their APIs, or invent a new agent. It is the runtime around them — PTY, Ghostty's VT engine, hop, search.

When you hop Claude Code → Grok → Pi → OpenCode → Codex, the runtime finds the session the current harness just wrote for this project, translates the actual turns into the next harness's native format, and launches that harness's own resume command. Tool calls (shell, file edits, MCP) and attachments (images, PDFs) go with the thread. It is not a pasted summary.

Search and resume are part of the same runtime, not a separate product. Every harness writes history to disk in its own shape. `ah` indexes all of it locally — BM25 as you type, MiniLM when you pause — so you can pull up a thread you remember by topic, not by which tool or folder held it. `Ctrl+R` resumes in the same harness. A hop, or `ah resume -r`, continues it in a different one.

- **Runtime, not a wrapper UI** — you launch `ah`; it spawns the real harness in a pty and renders it with Ghostty's terminal engine. The agent is unmodified.
- **Live hop between harnesses** — `Ctrl+B n/p/a`, `Alt+↑/↓`, or click the bottom bar. The next tool gets the real conversation in its own session format. Native compact/recap (and the local digest when a thread is cut) stay in model context, not as a chat bubble.
- **Tabs and workspaces** — several agents in one `ah` process. Prefix chords (`Ctrl+B c/w/o/i/[ /]/1–9/x`) or click the sidebar and tab strip. `Ctrl+B q` leaves `ah`; the next `ah` restores those workspaces and resumes each chat. When an agent exits, that tab closes. Last tab also leaves `ah`.
- **Pane CLI** — from inside a live tab, `ah tab`, `ah hop`, `ah close`, `ah focus`, and `ah workspace` talk to the parent mux (never your own pane for hop/close).
- **Search every local chat** — one picker over Claude Code, Codex, OpenCode, Pi, and Grok. Hybrid lexical + semantic search, all on your machine.
- **Resume the real session** — same-harness resume uses that tool's own files and resume command. Cross-harness resume writes a native session the target would have written itself.
- **Interactive or scripted** — humans stay in the TUI. Agents and CI call `ah resume` without a picker.
- **Sessions never leave disk** — hop and search read the same files the harnesses already write. Telemetry, if left on, is aggregate usage only.

## Install

macOS or Linux, no Node required:

```bash
curl -fsSL https://raw.githubusercontent.com/hetpatel-11/agent-hop/main/install.sh | bash
```

That pulls `ah` from the [GitHub Release](https://github.com/hetpatel-11/agent-hop/releases/latest) (npm tarball as fallback) into `~/.local/bin`. Override with `AH_BIN_DIR`. Pin a version with `AH_VERSION=0.1.4`.

Or download a binary from the [releases page](https://github.com/hetpatel-11/agent-hop/releases/latest): `ah-darwin-arm64`, `ah-darwin-x64`, `ah-linux-x64`, `ah-linux-arm64`, `ah-windows-x64.exe`.

Or via npm / bun, if you already use them:

```bash
npm install -g agent-hop
# or
bun install -g agent-hop
```

npm is a binary CDN here, not a JavaScript app. The package is a tiny Node shim that execs the same Rust binary GitHub Releases and the curl installer ship.

Supported prebuilds: macOS arm64/x64, Linux x64/arm64, Windows x64.

`ah` is the runtime, not an installer. The harness you hop into must already be on your `PATH`.

Anonymous usage telemetry is on by default (no queries, paths, or chat content). Turn it off with `ah telemetry off` or `AH_TELEMETRY=0`. Details: https://agent-hop.com/telemetry

## Usage

```bash
ah
```

Reopens the workspaces and tabs from last time, each on that harness's own resume of the chat. First run — or after you start with `ah claude` / `ah resume` — you pick a harness. That harness is a child process; `ah` wraps it:

| Shortcut | What it does |
|---|---|
| `Ctrl+B` then `n` / `p` | Hop **this** tab to the next / previous installed agent |
| `Ctrl+B` then `a` | Open the agent picker (or click the bottom bar) |
| `Ctrl+B` then `c` | New tab in this workspace |
| `Ctrl+B` then `w` | New workspace (folder), then pick an agent |
| `Ctrl+B` then `o` / `i` | Next / previous tab |
| `Ctrl+B` then `[` / `]` | Previous / next workspace |
| `Ctrl+B` then `1`–`9` | Focus that tab |
| `Ctrl+B` then `x` | Close this tab |
| `Ctrl+B` then `q` | Leave `ah`. Next `ah` restores these workspaces and chats |
| `Ctrl+B` then `?` | Show every `ah` shortcut |
| `Alt+↑` / `Alt+↓` | Hop next / previous (where the terminal sends those keys) |
| `Ctrl+R` | Search local history and resume a session in the **same** agent |

Launch straight into a tool:

```bash
ah claude
ah codex
ah opencode
ah pi
ah grok
```

Search and resume outside the live TUI:

```bash
ah resume
ah resume "auth migration"
ah resume "auth migration" --agent claude
ah resume "auth migration" --agent claude --resume-in codex
```

| Flag | Description |
|---|---|
| `-a, --agent <tool>` | Only search this agent (`claude`, `codex`, `opencode`, `pi`, `grok`) |
| `-r, --resume-in <tool>` | Convert the picked session into this agent's format, then launch it |

`Ctrl+R` and bare `ah resume` stay on the same agent. Cross-agent is the hop (`Ctrl+B` / `Alt+↑↓`) or `-r` for scripts.

Without a TTY (another agent or a script), a query is required and the top match is auto-picked:

```bash
ah resume "adobe premiere mcp setup" --agent codex --resume-in opencode
```

Be specific. A vague query like `"adobe"` can resume the wrong chat.

```bash
ah telemetry          # status
ah telemetry off
ah telemetry on
ah feedback "the hop bar is hard to see"
```

From inside a live pane (`AH_SOCK` is set; macOS and Linux), agents and you can drive the mux without targeting the pane that issued the command:

```bash
ah tab                  # new tab (picker)
ah tab codex            # new tab, skip picker
ah hop grok             # hop the other tab (omit --tab if there is exactly one other)
ah hop grok --tab 2
ah close --tab 2        # never closes the calling pane
ah focus 2
ah workspace            # new workspace
ah workspace next
ah workspace prev
```

`Ctrl+B q` leaves `ah` the way herdr's `prefix+q` does. Agents are not kept running in the background: the layout is saved, and the next `ah` reopens those folders and resumes each chat. When the last agent in a tab exits, that tab closes. If it was the last tab, `ah` exits. `Ctrl+B x` closes a tab the same way.

## Architecture

`ah` is a runtime: run the real harness, search the history those harnesses already wrote, translate a live session into another harness's format, and stay out of the way.

```
you ──► ah (picker / TUI chrome)
          │
          ├─ portable-pty ──► claude | codex | opencode | pi | grok
          │                      ▲
          │                      │ native resume command
          │
          ├─ libghostty-vt ──► render the agent's own terminal
          │
          ├─ adapters ──► read/write each tool's files on disk
          │      │
          │      └─ Turn[] (shared IR) ──► hop / ah resume -r
          │
          └─ search ──► BM25 + MiniLM (local ONNX) over ~/.agent-hop
```

### What each piece is for

**TUI shell (`src/tui.rs`, `src/picker.rs`)** — Starts the agent you picked inside a pty (`portable-pty`) and keeps a toggle bar `ah` owns. The agent never draws into that bar. Hop, search, and help are overlays on top of the still-running agent, not a separate app.

**Terminal engine (`src/vt.rs`, `libghostty-vt`)** — Ghostty's embeddable VT parser, linked at build time. The agent is a real TUI (Kitty keyboard protocol, OSC 133 prompt marks, truecolor). We render what it would draw in Ghostty, instead of reimplementing a half-compatible terminal. End users never need Zig; only people building from source do.

**Hop (`src/adapters/mod.rs`)** — On `Ctrl+B n/p/a` or `Alt+↑/↓`, `ah` finds the session the current agent just wrote for this project, reads it, trims it to a 200k-character budget, writes it as the next agent's native session, and launches that agent's own resume command. If the source already stored a compact/recap (Claude user compact, Grok `session_recap`), that text is reserved first and hidden from the target TUI as a bubble; Codex compact is encrypted and unused. Anything else that still doesn't fit gets a local digest, also hidden. Fast path: `find_latest_for_path` so a hop does not scan every session on disk.

**Mux (`src/tui.rs`, `src/control.rs`)** — Tabs live in workspaces (folders). Chrome is a full-height sidebar plus a tab strip on the pane. Pane commands go over a unix socket (`$AH_SOCK` / `$AH_TAB_ID`) so a child cannot hop or close itself. `ah feedback` posts to the same worker as telemetry.

**Adapters (`src/adapters/<tool>.rs`)** — One module per agent. Each implements:

- `list_sessions()` — cheap metadata for search
- `read()` — that tool's files → `Turn[]`
- `write()` — `Turn[]` → a new session in that tool's format
- `resume_cmd()` — the argv to exec (`claude --resume …`, `codex resume …`, …)

Adding an agent is one new adapter. The TUI and search do not change.

**Shared IR (`Turn`)** — Every hop goes through one shape: role, text, structured tool calls (name / input / output), attachments (mime, base64, filename). Tool calls are not flattened into prose. Each writer emits the target's real blocks (`tool_use` / `function_call` / `ToolPart` / …), the same records that agent would have written itself.

**Search (`src/search.rs`, `src/fuzzy.rs`, `src/resume.rs`)** — Two stages, same index:

1. **Lexical, every keystroke** — BM25 over local sessions. Exact phrases first, then fuzzy (BK-tree), prefix (`ux` → `uxp`), and compound tokens (`agenthop` → `agent hop`), with a recency boost.
2. **Semantic, after you pause** — `all-MiniLM-L6-v2` (384-d, mean-pooled) refines ranking. First use downloads the quantized ONNX model plus ONNX Runtime into `~/.agent-hop/` and keeps them. Later searches are local.

The incremental vector index lives under `~/.agent-hop/`. New sessions are indexed by a detached `ah __background-index` so the picker never blocks on a full rebuild.

**Standalone resume (`ah resume`)** — Same ranker and picker as `Ctrl+R`, without wrapping a live agent first. `-r` is the scripted hop. Non-interactive mode skips the TUI entirely and execs the target agent's resume command with inherited stdio.

**Telemetry (`src/telemetry.rs`)** — Opt-out, self-hosted (`telemetry.agent-hop.com`). Sends aggregate usage (command, version, OS). Never queries, paths, project names, session ids, or chat content. Disabled by `AH_TELEMETRY=0`, `DO_NOT_TRACK=1`, or `ah telemetry off`.

**Distribution** — CI builds one `ah` per platform and ships it twice: a [GitHub Release](https://github.com/hetpatel-11/agent-hop/releases) (what `install.sh` prefers) and npm `optionalDependencies` (`agent-hop-darwin-arm64`, …). `bin/ah.js` is only a resolver. Same artifact, two front doors.

### Per-agent session stores

`ah` reads and writes what each tool already uses:

| Agent | On disk | Notes |
|---|---|---|
| Claude Code | `~/.claude/projects/<encoded-cwd>/*.jsonl` | Directory name replaces every non-alphanumeric with `-`, not just `/`. |
| Codex | `~/.codex/sessions/YYYY/MM/DD/*.jsonl` | `response_item` continues the model; `event_msg` is what the TUI replays. |
| OpenCode | official `opencode export` / `opencode import` | No raw SQLite writes. Part IDs must be unique or OpenCode silently drops the insert. |
| Pi | `~/.pi/agent/sessions/--<encoded-cwd>--/` | Only `/` is encoded; `_` and `.` stay. |
| Grok | `chat_history.jsonl` + `summary.json` per session | `updates.jsonl` is also written so the TUI can replay, not only continue. |

Long threads are cut to the most recent slice that fits (`CONVERSION_CHAR_BUDGET`, 200k characters) rather than handed to a target that cannot load them.

## Supported agents

| Agent | Same-tool resume | Hop / write into this format |
|---|---|---|
| Claude Code | yes | yes |
| Codex | yes | yes |
| OpenCode | yes | yes |
| Pi | yes | yes |
| Grok | yes | yes |

## Development

Prebuilt installs do not need this. Building from source does: a current Rust toolchain, Zig 0.15.2 (for `libghostty-vt`), and libclang (for bindgen).

```bash
git clone https://github.com/hetpatel-11/agent-hop.git
cd agent-hop
cargo build --release
# binary: target/release/ah
```

`npm run build` is the same `cargo build --release`. Do not copy the binary over a Homebrew/npm shim on macOS — Gatekeeper will kill a detached copy; symlink `~/.cargo/bin/ah` or `target/release/ah` instead.

```bash
cargo test --release
```
