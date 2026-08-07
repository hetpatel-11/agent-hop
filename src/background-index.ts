#!/usr/bin/env node
// Standalone entry point, spawned detached from the interactive CLI so
// indexing survives after the parent process exits (e.g. once you've picked
// a session and resumed into an agent). Never invoked directly by a user --
// see search.ts, which spawns this whenever there's unindexed content.
import { collectSessions } from "./search.js";
import { buildIndex } from "./vector-index.js";
import { TOOL_NAMES } from "./adapters/index.js";

async function main() {
  const sessions = await collectSessions(TOOL_NAMES);
  await buildIndex(sessions);
}

main().catch(() => process.exit(1));
