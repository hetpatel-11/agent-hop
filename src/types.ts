export type Role = "user" | "assistant";

export interface Turn {
  role: Role;
  text: string;
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
