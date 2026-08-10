export type Role = "user" | "assistant";

/** A tool call + its result, inlined as text -- this is the only shape that
 * survives translation across all five agents' wildly different tool-call
 * schemas (different field names, different argument encodings, and tools
 * that flat-out don't exist in the target agent, e.g. an MCP server only
 * installed for the source agent). Text is the universal denominator; a
 * real structured tool_use/tool_result block is not, and forging one in a
 * format the target agent didn't actually produce risks it rejecting the
 * session outright. */
export interface ToolCallRecord {
  name: string;
  input: string;
  output?: string;
}

/** Binary attachments are the other exception to "text is the only portable
 * shape" -- every agent embeds files (images, PDFs, arbitrary attachments)
 * as inline base64 somewhere in its own format (different wrapper shape,
 * same underlying bytes), so they genuinely round-trip regardless of mime
 * type. `filename` is optional context for adapters that track it (e.g.
 * OpenCode's FilePart, Claude's attachment records) -- not load-bearing
 * for round-tripping the actual bytes. */
export interface Attachment {
  mimeType: string;
  base64: string;
  filename?: string;
}

export interface Turn {
  role: Role;
  text: string;
  toolCalls?: ToolCallRecord[];
  attachments?: Attachment[];
}

// "muse" removed for now -- resumeCmd was using the wrong muse subcommand
// (exec --session-id, headless-only) instead of the real interactive
// `muse resume <uuid>`, and there's no way to verify the fix without muse
// API access. Re-add once that can actually be tested end-to-end.
export type ToolName = "claude" | "codex" | "opencode" | "pi" | "grok";

export interface SessionRef {
  tool: ToolName;
  sessionId: string;
  projectPath: string;
  title: string;
  snippet: string;
  /** Full (length-capped) conversation text for search -- not just the
   * opening message. Falls back to `snippet` when an adapter can't cheaply
   * capture more (e.g. OpenCode, which would need a subprocess export call
   * per session just to list). */
  body?: string;
  updatedAt: number; // unix ms, for recency sorting
  raw?: Record<string, unknown>; // adapter-specific extra data (e.g. file path)
  /** Excerpt around the matched query terms (ANSI-highlighted), set by
   * searchSessions() so the picker can show *why* a result matched instead
   * of just its opening line. Absent for a no-query (recency) listing. */
  matchSnippet?: string;
}

export interface Adapter {
  tool: ToolName;
  listSessions(): Promise<SessionRef[]>;
  read(ref: SessionRef): Promise<Turn[]>;
  write(turns: Turn[], projectPath: string): Promise<string>; // returns new session id
  resumeCmd(sessionId: string, projectPath: string): string[];
}
