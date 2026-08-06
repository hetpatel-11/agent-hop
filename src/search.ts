import fuzzysort from "fuzzysort";
import type { SessionRef, ToolName } from "./types.js";
import { ADAPTERS, TOOL_NAMES } from "./adapters/index.js";

export async function collectSessions(tools: ToolName[]): Promise<SessionRef[]> {
  const results = await Promise.all(
    tools.map(async (tool) => {
      try {
        return await ADAPTERS[tool].listSessions();
      } catch {
        return [] as SessionRef[];
      }
    })
  );
  return results.flat();
}

export function searchSessions(sessions: SessionRef[], query: string, limit = 15): SessionRef[] {
  if (!query.trim()) {
    return [...sessions].sort((a, b) => b.updatedAt - a.updatedAt).slice(0, limit);
  }
  const targets = sessions.map((s) => ({
    session: s,
    haystack: `${s.title} ${s.snippet} ${s.projectPath}`,
  }));
  const results = fuzzysort.go(query, targets, { key: "haystack", limit, threshold: -10000 });
  return results.map((r) => r.obj.session);
}

export { TOOL_NAMES };
