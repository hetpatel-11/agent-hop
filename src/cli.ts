#!/usr/bin/env node
import { Command } from "commander";
import * as p from "@clack/prompts";
import figlet from "figlet";
import { spawn, execFileSync } from "node:child_process";
import { existsSync } from "node:fs";
import { homedir } from "node:os";
import { toolTag, highlightDate, color } from "./theme.js";
import { collectSessions, searchSessions, buildRanker, ensureIndexingTriggered, TOOL_NAMES } from "./search.js";
import { ADAPTERS } from "./adapters/index.js";
import type { ToolName, SessionRef } from "./types.js";

const program = new Command();
program
  .name("agentresume")
  .description("Search across every local coding-agent session and resume any one in any agent.")
  .argument("[query]", "search query (omitted = interactive prompt)")
  .option("-a, --agent <tool>", "restrict search to one agent (claude|codex|opencode|pi|grok)")
  .option("-r, --resume-in <tool>", "resume the picked session in this agent (default: same tool)")
  .action(main);

program.parse();

/** A full block-letter logo is the whole point of a "massive" banner, but it's
 * ~71 columns wide -- unusable if it wraps. Fall back to the compact wordmark
 * in a narrow terminal or when stdout isn't a real TTY (piped output). */
function printBanner(): void {
  const big = figlet.textSync("agentresume", { font: "ANSI Shadow" });
  const bigWidth = Math.max(...big.split("\n").map((l) => l.length));
  const columns = process.stdout.columns ?? 0;
  if (process.stdout.isTTY && columns >= bigWidth) {
    console.log(color.bold(color.cyan(big)));
  } else {
    console.log(`\n  ${color.bold(color.cyan("agent"))}${color.bold(color.green("resume"))}\n`);
  }
}

async function main(queryArg: string | undefined, opts: { agent?: string; resumeIn?: string }) {
  // When something else is on the other end of stdin/stdout -- another
  // agent shelling out to this as a command, output piped to a file, a CI
  // job -- there's no one to answer an interactive prompt, so it would just
  // hang forever. Fall back to sane defaults (all agents, top match, same
  // tool) instead of blocking, and require the query up front since there's
  // no way to prompt for it.
  const nonInteractive = !process.stdin.isTTY || !process.stdout.isTTY;

  printBanner();
  p.intro(color.dim("search every agent's sessions, resume any one in a different agent"));

  if (nonInteractive && queryArg === undefined) {
    p.cancel("Running non-interactively (no TTY) -- a search query is required, e.g. `agentresume \"oauth bug\"`.");
    process.exit(1);
  }

  // Step 1: which agent(s) to search
  let scope: ToolName[];
  if (opts.agent) {
    if (!TOOL_NAMES.includes(opts.agent as ToolName)) {
      p.cancel(`Unknown agent "${opts.agent}". Valid: ${TOOL_NAMES.join(", ")}`);
      process.exit(1);
    }
    scope = [opts.agent as ToolName];
  } else if (nonInteractive) {
    scope = TOOL_NAMES;
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

  const spinner = p.spinner();
  spinner.start(`Loading ${scope.join(", ")} sessions...`);
  const sessions = await collectSessions(scope);
  spinner.stop(`Loaded ${sessions.length} session${sessions.length === 1 ? "" : "s"}.`);

  let picked: SessionRef;

  if (queryArg !== undefined || nonInteractive) {
    // A query was already supplied (CLI argument, or forced by non-interactive
    // mode) -- nothing to type live, so this is a one-shot full hybrid search
    // (BM25 + semantic) rather than the live-typing ranker.
    const query = queryArg ?? "";
    const { results, indexingInBackground } = await searchSessions(sessions, query);
    if (indexingInBackground) {
      p.log.info("Semantic search is still learning some newer sessions in the background — results will get sharper on your next search.");
    }
    if (results.length === 0) {
      p.outro("No sessions found. Try a different query or agent scope.");
      return;
    }
    if (nonInteractive) {
      picked = results[0];
      p.log.info(`Non-interactive: auto-picked top match -- ${toolTag(picked.tool)} ${picked.title}`);
    } else {
      const choice = await p.select<SessionRef>({
        message: "Pick a session:",
        options: results.map((r) => sessionOption(r)),
      });
      if (p.isCancel(choice)) return cancelExit();
      picked = choice;
    }
  } else {
    // No query yet -- search and pick are one step: a live-filtering prompt
    // that re-ranks as you type, so you see the effect of refining your
    // keywords immediately instead of committing to a query, seeing a static
    // list, and having to back up if it's not what you meant.
    const indexingInBackground = ensureIndexingTriggered(sessions);
    if (indexingInBackground) {
      p.log.info("Semantic search is still learning some newer sessions in the background — results will get sharper on your next search.");
    }
    const ranker = buildRanker(sessions);

    // Stage 2: after typing pauses, upgrade the instant BM25+fuzzy list to
    // include semantic similarity. `options` must be synchronous (clack
    // calls it inline on every keystroke), so the async refine happens
    // entirely outside it -- we stash the live prompt instance (`this`,
    // captured below) and later mutate its `filteredOptions` directly and
    // call its `render()` ourselves. Neither is documented public API, but
    // both are plain instance properties/methods, not internals we'd need
    // a private field to reach.
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    let promptInstance: any;
    let generation = 0;
    let debounceTimer: NodeJS.Timeout | undefined;
    const STAGE2_DEBOUNCE_MS = 200;

    const choice = await p.autocomplete<SessionRef>({
      message: "Type to search (e.g. oauth bug, video editor, auth migration):",
      // clack's autocomplete hides its own cursor block whenever the input
      // is empty, no matter what -- there's no placeholder-plus-cursor
      // rendering path here the way p.text has (checked its source: the
      // empty-input branch always renders `hidden`, unconditionally). A
      // single leading space forces it down the non-empty branch instead,
      // which does show a real cursor block. Harmless for search itself
      // (trimmed away before ranking) -- the only cost is one visible
      // leading space until the first real keystroke, which is a fair
      // trade for "the input previously looked inert/un-typeable."
      initialUserInput: " ",
      maxItems: 12,
      // clack's own `autocomplete()` wrapper supplies a default `filter`
      // even when none is passed explicitly (checked its source: it's
      // `t.filter ?? defaultSubstringMatcher`, not left unset) -- a literal
      // case-insensitive substring check against label/hint/value, applied
      // on top of whatever `options()` already returned, every time the
      // input is non-empty. That silently discarded the "semantically
      // searching…" status row (the typed text is never a substring of
      // that label) and any real result that matched via BM25/fuzzy/semantic
      // scoring without the exact typed text literally appearing in its
      // title -- both showing up as "No matches found" despite `options()`
      // having legitimately returned results. `options()` already does all
      // real filtering and ranking itself, so this must be a pure pass-through.
      filter: () => true,
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      options: function (this: any) {
        promptInstance = this;
        const query: string = (this.userInput ?? "").trim();
        generation++;
        const myGeneration = generation;

        if (debounceTimer) clearTimeout(debounceTimer);
        debounceTimer = setTimeout(() => {
          ranker
            .refineWithSemantic(query, 12)
            .then((refined) => {
              // A newer keystroke happened while this was in flight --
              // discard rather than let a stale result render over it.
              if (myGeneration !== generation || !promptInstance) return;
              const refinedOptions = refined.length > 0 ? refined.map((r) => sessionOption(r)) : [statusOption("no results found — try a different phrase")];

              // clack only shows a row's hint (date + conversation snippet)
              // when that row's `value` is the *exact same object reference*
              // as `focusedValue` -- checked its source directly. Every call
              // to sessionOption() builds a brand-new object, so blindly
              // resetting focus to index 0's freshly-created value is fine
              // for the first render, but the moment semantic reordering
              // moves the session you were actually looking at to a
              // different position (routine once stage 2 kicks in), that
              // reference stops matching anything on screen and the hint
              // -- date included -- goes blank for every row, not just a
              // content change. Re-find the same session by identity
              // (tool+sessionId) in the new array instead of assuming it's
              // still first, so focus (and its hint) follows the actual
              // session you were looking at rather than jumping to whatever
              // is now ranked highest.
              const previouslyFocused = promptInstance.focusedValue as SessionRef | undefined;
              const stillFocused = previouslyFocused
                ? refinedOptions.find((o) => o.value.tool === previouslyFocused.tool && o.value.sessionId === previouslyFocused.sessionId)
                : undefined;

              promptInstance.filteredOptions = refinedOptions;
              promptInstance.focusedValue = (stillFocused ?? refinedOptions[0])?.value;
              promptInstance.render();
            })
            .catch(() => {
              // offline / model unavailable -- stage-1 results are already
              // shown, nothing to fall back to
            });
        }, STAGE2_DEBOUNCE_MS);

        const stage1 = ranker.rank(query, 12);
        // Do NOT show "no results found" here just because stage 1 (fast
        // keyword+fuzzy matching) came up empty -- semantic search is
        // *already* running in the background regardless (the debounce
        // timer above was just armed unconditionally), and its entire
        // reason to exist is finding sessions that share no literal
        // vocabulary with the query. Jumping to "no results" before it even
        // gets a chance defeats the point. "no results" is only ever shown
        // once stage 2 has genuinely completed and confirmed there's
        // nothing (see the .then() above) -- until then this always stays
        // "semantically searching…". Placed *after* the real results (not
        // before) -- clack renders every entry in this array with the same
        // bulleted-row template regardless of content, so there's no way to
        // make this structurally different from a real result; last-in-list
        // at least reads as a trailing status note rather than looking like
        // the top-ranked "result" when it's the very first thing shown.
        return [...stage1.map((r) => sessionOption(r)), statusOption("(still semantically searching for more…)")];
      },
    });
    if (p.isCancel(choice)) return cancelExit();
    if (choice.sessionId === STATUS_SENTINEL_ID) return cancelExit(); // defensive -- disabled rows shouldn't be selectable at all
    picked = choice;
  }

  // Step 5: which agent to resume in
  let targetTool: ToolName;
  if (opts.resumeIn) {
    if (!TOOL_NAMES.includes(opts.resumeIn as ToolName)) {
      p.cancel(`Unknown agent "${opts.resumeIn}". Valid: ${TOOL_NAMES.join(", ")}`);
      process.exit(1);
    }
    targetTool = opts.resumeIn as ToolName;
  } else if (nonInteractive) {
    targetTool = picked.tool;
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

  // The original project directory may no longer exist (moved/deleted since
  // the source session was recorded) -- spawning a child process with a
  // nonexistent cwd throws a misleading ENOENT that looks like "command not
  // found." Fall back to homedir() and say so, rather than crash cryptically.
  if (!existsSync(projectPath)) {
    p.log.warn(`Original project directory no longer exists: ${projectPath}\nResuming in ${homedir()} instead.`);
    projectPath = homedir();
  }

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

  // @clack/prompts leaves stdin in raw mode with its own listener attached
  // after the prompts above finish. If we spawn the target TUI with
  // stdio: "inherit" while that's still attached, both this process and the
  // child end up reading the same tty fd at once -- terminal capability
  // replies and mouse-motion escape codes get split unpredictably between
  // the two, which is what shows up as garbled control-sequence text once
  // the child starts. Fully release stdin so the child is the sole reader.
  if (process.stdin.isTTY) process.stdin.setRawMode(false);
  process.stdin.removeAllListeners();
  process.stdin.pause();

  // spawn()+wait keeps *this* process alive for as long as the resumed
  // agent runs -- and even with stdin released above, Node's own tty handle
  // can retain some residual low-level involvement with the shared fd for
  // as long as this process's event loop is alive. That's the likely cause
  // of the sluggish-typing reports for opencode/pi specifically (both
  // exhibited it despite being totally different runtimes -- a native Bun
  // binary and a plain Node script -- which points at something on *our*
  // side, not theirs). The structurally correct fix is true process
  // replacement (execve): once our process image is gone, there's no
  // parent left to have any relationship with the tty at all.
  //
  // execve() failures are NOT catchable JS exceptions -- confirmed directly:
  // an invalid path crashes the process with a native ENOENT trace that a
  // surrounding try/catch does not intercept. So this is only attempted
  // once the resolved path is independently verified to exist; any
  // uncertainty (unsupported platform, resolution failure, missing API)
  // falls through to the always-safe spawn()+wait path below instead of
  // ever risking that call.
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  const execve = typeof (process as any).execve === "function" ? ((process as any).execve as (path: string, args: string[], env: Record<string, string>) => never) : undefined;
  const resolvedExecPath = process.platform !== "win32" && execve ? resolveExecutable(cmd[0]) : null;
  if (resolvedExecPath && existsSync(resolvedExecPath) && execve) {
    process.chdir(projectPath); // execve has no cwd option -- must chdir first
    execve(resolvedExecPath, cmd, process.env as Record<string, string>); // never returns on success
  }

  const child = spawn(cmd[0], cmd.slice(1), {
    cwd: projectPath,
    stdio: "inherit",
  });
  child.on("exit", (code) => process.exit(code ?? 0));
}

/** Resolves a bare command name (e.g. "opencode") to its full path via the
 * OS's own lookup (`which`/`where`) -- execve needs a real path, it doesn't
 * do PATH search the way spawn() does. Returns null on any failure rather
 * than throwing, so the caller can safely fall back to spawn(). */
function resolveExecutable(cmd: string): string | null {
  try {
    const finder = process.platform === "win32" ? "where" : "which";
    const out = execFileSync(finder, [cmd], { encoding: "utf-8" });
    const resolved = out.split("\n")[0].trim();
    return resolved || null;
  } catch {
    return null;
  }
}

function cancelExit(): void {
  p.cancel("Cancelled.");
  process.exit(0);
}

// Adapters are expected to hand back single-line titles/snippets already,
// but a raw multi-line value slipping through (e.g. a native title from an
// agent's own summary field) breaks clack's rendering -- it has no newline
// handling in a label or hint, so extra lines show up as bare, bullet-less
// rows that look like separate broken list items. Collapse defensively here
// regardless of whether the upstream source was supposed to be clean.
function oneLine(s: string): string {
  return s.replace(/\s+/g, " ").trim();
}

function sessionOption(r: SessionRef): { value: SessionRef; label: string; hint: string } {
  const date = highlightDate(new Date(r.updatedAt).toLocaleDateString(undefined, { month: "short", day: "numeric", year: "numeric" }));
  const folder = r.projectPath.split("/").filter(Boolean).pop() ?? r.projectPath;
  // Prefer the matched-content snippet (why this result showed up). When
  // there's no literal query match to excerpt (browsing unfiltered, or a
  // semantic-only match), fall back to real conversation body text -- NOT
  // r.snippet, which several adapters (claude/codex/opencode/pi) set to a
  // truncated copy of r.title, telling the user nothing beyond what the
  // label above already shows. r.body carries the actual accumulated
  // turns, so it's the only fallback that answers "what was this chat".
  const bodyPreview = (r.body ?? "").trim();
  const fallback = bodyPreview && bodyPreview !== r.title ? bodyPreview : r.snippet;
  const context = oneLine(r.matchSnippet ?? (fallback || r.projectPath)).slice(0, 160);
  return {
    value: r,
    label: `${toolTag(r.tool)} ${oneLine(r.title)}`,
    hint: `${date} · ${folder} · ${context}`,
  };
}

// A synthetic, disabled list row used to show search status ("semantically
// searching…", "no results found…") using clack's own list rendering instead
// of relying on its hardcoded internal empty-state text, which we can't
// customize. sessionId is a marker, never a real session -- deliberately
// NOT disabled:true, since clack renders disabled options with a strikethrough
// (semantically "unavailable option"), which reads as broken/wrong for a
// plain status message. The caller checks this id defensively after the
// prompt resolves instead, in case it's ever selected.
const STATUS_SENTINEL_ID = "__agentresume_status__";
function statusOption(text: string): { value: SessionRef; label: string } {
  const sentinel: SessionRef = {
    tool: "claude",
    sessionId: STATUS_SENTINEL_ID,
    projectPath: "",
    title: "",
    snippet: "",
    updatedAt: 0,
  };
  return { value: sentinel, label: color.dim(text) };
}
