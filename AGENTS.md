# agent-hop

Rust mux around Claude Code, Codex, OpenCode, Pi, and Grok. CLI is `ah`. Prefer `target/release/ah` after a local build. Do not push, bump the version, or publish npm unless the user asks.

## Cursor Cloud specific instructions

Cloud VMs are Ubuntu. `.cursor/install.sh` puts Rust (stable), Zig 0.15.2, clang, Node, and wrangler on disk. After a shell starts:

```bash
export PATH="$HOME/.local/bin:$HOME/.cargo/bin:$PATH"
export ZIG="$HOME/.local/bin/zig"
export CLOUDFLARE_ACCOUNT_ID="${CLOUDFLARE_ACCOUNT_ID:-f45021db74a97feaafae2e3e131d4d82}"
```

Build and test:

```bash
cargo test
cargo build --release
./target/release/ah --version
```

The live TUI (`ah`) needs a real terminal attached to a human. From a phone, do not try to "use" the mux. Use `cargo test`, `cargo build`, and D1 queries.

### Cloudflare / telemetry

Ingest is `https://telemetry.agent-hop.com`. D1 database `agent-hop-telemetry`, account `f45021db74a97feaafae2e3e131d4d82`.

Secrets (Cursor dashboard → Cloud Agents → Secrets — never commit):

- `CLOUDFLARE_API_TOKEN` — token with D1 read (and edit if you must run schema). Account-scoped.
- `CLOUDFLARE_ACCOUNT_ID` — `f45021db74a97feaafae2e3e131d4d82` (optional; install script defaults it).

Query:

```bash
cd telemetry
npx wrangler d1 execute agent-hop-telemetry --remote --json --command "SELECT COUNT(*) AS events, COUNT(DISTINCT device_id) AS devices FROM events;"
```

Do not treat `live-test%` / `probe%` / `observe%` device ids as users. Het's device is `38b7fdf0`.

If wrangler says unauthorized, the token is missing or wrong. Do not run interactive `wrangler login` on Cloud.
