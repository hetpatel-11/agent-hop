https://github.com/user-attachments/assets/a508a303-5405-4420-aa8a-b86600e83cf1

*In demo: hopping Claude Code → Grok → Pi → OpenCode → Codex, in that order.*

**Search all your coding-agent chats, then continue any session in any agent.**

# agent-hop

Your best coding-agent context is probably trapped in the wrong tool.

`agent-hop` (`ah`) is a native Rust CLI. It wraps Claude Code, Codex, OpenCode, Pi, and Grok in one terminal, searches their local history from one picker, and hops a live conversation into another agent's native session format — tool calls and attachments included, not a summary.

Use it when you remember the topic, but not the tool, the project folder, or the exact session.

- **One search box for every agent** — hybrid search over local Claude Code, Codex, OpenCode, Pi, and Grok history.
- **Resume the real thread** — same-agent resume uses that tool's own session. No re-explaining.
- **Hop mid-conversation** — switch agents without starting over. The next tool gets the actual turns, including shell/file/MCP calls and file attachments.
- **The real agent, not a clone** — each tool still runs as itself in a pty. `ah` owns the chrome (toggle bar, hop, search), not the agent.
- **Interactive or scripted** — humans get a TUI; agents and CI call `ah resume` non-interactively.
- **Sessions stay on disk** — search and conversion read the same local files the agents write. Semantic search runs on your machine.

## Install

macOS or Linux, no Node required:

```bash
curl -fsSL https://raw.githubusercontent.com/hetpatel-11/agent-hop/main/install.sh | bash
```

That downloads the native `ah` binary into `~/.local/bin` (override with `AH_BIN_DIR`). Pin a version with `AH_VERSION=0.1.0`.

Or via npm / bun, if you already use them:

```bash
npm install -g agent-hop
# or
bun install -g agent-hop
```

npm is a binary CDN here, not a JavaScript app. The package is a tiny Node shim that execs the same Rust binary the curl installer puts on your PATH.

Supported prebuilds: macOS arm64/x64, Linux x64/arm64, Windows x64. Windows: use npm, or download `agent-hop-windows-x64` from the [npm registry](https://www.npmjs.com/package/agent-hop-windows-x64).

`ah` launches agents; it does not install them. The agent you hop into must already be on your `PATH`.

Anonymous usage telemetry is on by default (no queries, paths, or chat content). Turn it off with `ah telemetry off` or `AH_TELEMETRY=0`. Details: https://agent-hop.com/telemetry

## Usage

```bash
ah
```

Pick an agent, then work as usual. `ah` sits around that process:

| Shortcut | What it does |
|---|---|
| `Ctrl+B` then `n` / `p` | Hop to the next / previous installed agent |
| `Ctrl+B` then `a` | Open the agent picker (or click the bottom bar) |
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
```

## Architecture

`ah` is one binary with four jobs: wrap the real agent, search local history, translate a session into another tool's format, and get out of the way.

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

**Hop (`src/adapters/mod.rs`)** — On `Ctrl+B n/p/a` or `Alt+↑/↓`, `ah` finds the session the current agent just wrote for this project, reads it, trims it to a 200k-character budget (with a short synthetic summary of anything cut), writes it as the next agent's native session, and launches that agent's own resume command. Fast path: `find_latest_for_path` so a hop does not scan every session on disk.

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

**Distribution** — CI builds one `ah` per platform. npm `optionalDependencies` (`agent-hop-darwin-arm64`, …) are those binaries; `bin/ah.js` is only a resolver. `install.sh` pulls the same tarball from the npm registry and drops `ah` on your PATH. Two front doors, one artifact.

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
