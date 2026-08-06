export type Role = "user" | "assistant";

export interface Turn {
  role: Role;
  text: string;
}

export type ToolName = "claude" | "codex" | "opencode" | "pi" | "muse" | "grok";

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
}

export interface Adapter {
  tool: ToolName;
  listSessions(): Promise<SessionRef[]>;
  read(ref: SessionRef): Promise<Turn[]>;
  write(turns: Turn[], projectPath: string): Promise<string>; // returns new session id
  resumeCmd(sessionId: string, projectPath: string): string[];
}
