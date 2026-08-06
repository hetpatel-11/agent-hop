import type { Adapter, ToolName } from "../types.js";
import { claudeAdapter } from "./claude.js";
import { codexAdapter } from "./codex.js";
import { opencodeAdapter } from "./opencode.js";
import { piAdapter } from "./pi.js";
import { museAdapter } from "./muse.js";
import { grokAdapter } from "./grok.js";

export const ADAPTERS: Record<ToolName, Adapter> = {
  claude: claudeAdapter,
  codex: codexAdapter,
  opencode: opencodeAdapter,
  pi: piAdapter,
  muse: museAdapter,
  grok: grokAdapter,
};

export const TOOL_NAMES = Object.keys(ADAPTERS) as ToolName[];
