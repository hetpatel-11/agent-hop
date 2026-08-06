#!/usr/bin/env node
import { Command } from "commander";
import * as p from "@clack/prompts";
import { spawn } from "node:child_process";
import { collectSessions, searchSessions, TOOL_NAMES } from "./search.js";
import { ADAPTERS } from "./adapters/index.js";
import type { ToolName, SessionRef } from "./types.js";

const program = new Command();
program
  .name("handoff")
  .description("Search across every local coding-agent session and resume any one in any agent.")
  .argument("[query]", "search query (omitted = interactive prompt)")
  .option("-a, --agent <tool>", "restrict search to one agent (claude|codex|opencode|pi|muse|grok)")
  .option("-r, --resume-in <tool>", "resume the picked session in this agent (default: same tool)")
  .action(main);

program.parse();

async function main(queryArg: string | undefined, opts: { agent?: string; resumeIn?: string }) {
  p.intro("handoff — cross-agent session search & resume");

  // Step 1: which agent(s) to search
  let scope: ToolName[];
  if (opts.agent) {
    if (!TOOL_NAMES.includes(opts.agent as ToolName)) {
      p.cancel(`Unknown agent "${opts.agent}". Valid: ${TOOL_NAMES.join(", ")}`);
      process.exit(1);
    }
    scope = [opts.agent as ToolName];
  } else {
    const choice = await p.select({
      message: "Which agent(s) to search?",
      options: [
        { value: "all", label: "All agents" },
        ...TOOL_NAMES.map((t) => ({ value: t, label: t })),
      ],
    });
    if (p.isCancel(choice)) return cancelExit();
    scope = choice === "all" ? TOOL_NAMES : [choice as ToolName];
  }

  // Step 2: search query
  let query = queryArg;
  if (query === undefined) {
    const q = await p.text({ message: "Search for:", placeholder: "e.g. auth migration, oauth bug" });
    if (p.isCancel(q)) return cancelExit();
    query = q;
  }

  const spinner = p.spinner();
  spinner.start(`Searching ${scope.join(", ")}...`);
  const sessions = await collectSessions(scope);
  const results = searchSessions(sessions, query ?? "");
  spinner.stop(`Found ${results.length} match${results.length === 1 ? "" : "es"}.`);

  if (results.length === 0) {
    p.outro("No sessions found. Try a different query or agent scope.");
    return;
  }

  // Step 3 + 4: pick a session
  const picked = await p.select<SessionRef>({
    message: "Pick a session:",
    options: results.map((r) => ({
      value: r,
      label: `[${r.tool}] ${r.title}`,
      hint: `${r.projectPath} · ${new Date(r.updatedAt).toLocaleString()}`,
    })),
  });
  if (p.isCancel(picked)) return cancelExit();

  // Step 5: which agent to resume in
  let targetTool: ToolName;
  if (opts.resumeIn) {
    if (!TOOL_NAMES.includes(opts.resumeIn as ToolName)) {
      p.cancel(`Unknown agent "${opts.resumeIn}". Valid: ${TOOL_NAMES.join(", ")}`);
      process.exit(1);
    }
    targetTool = opts.resumeIn as ToolName;
  } else {
    const choice = await p.select({
      message: `Resume in which agent? (session is from ${picked.tool})`,
      options: TOOL_NAMES.map((t) => ({
        value: t,
        label: t === picked.tool ? `${t} (same tool — native resume)` : t,
      })),
      initialValue: picked.tool,
    });
    if (p.isCancel(choice)) return cancelExit();
    targetTool = choice as ToolName;
  }

  // Step 6: convert (if needed) + launch
  const sourceAdapter = ADAPTERS[picked.tool];
  const targetAdapter = ADAPTERS[targetTool];

  let sessionId = picked.sessionId;
  let projectPath = picked.projectPath;

  if (targetTool !== picked.tool) {
    const convertSpinner = p.spinner();
    convertSpinner.start(`Converting ${picked.tool} session for ${targetTool}...`);
    try {
      const turns = await sourceAdapter.read(picked);
      if (turns.length === 0) {
        convertSpinner.stop("No readable turns in that session.");
        p.outro("Nothing to resume.");
        return;
      }
      sessionId = await targetAdapter.write(turns, projectPath);
      convertSpinner.stop(`Converted (${turns.length} turns).`);
    } catch (err) {
      convertSpinner.stop("Conversion failed.");
      p.cancel(err instanceof Error ? err.message : String(err));
      process.exit(1);
    }
  }

  const cmd = targetAdapter.resumeCmd(sessionId, projectPath);
  p.outro(`Launching: ${cmd.join(" ")}`);

  const child = spawn(cmd[0], cmd.slice(1), {
    cwd: projectPath,
    stdio: "inherit",
  });
  child.on("exit", (code) => process.exit(code ?? 0));
}

function cancelExit(): void {
  p.cancel("Cancelled.");
  process.exit(0);
}
