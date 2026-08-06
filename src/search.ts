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

function tokenize(text: string): string[] {
  return text
    .toLowerCase()
    .split(/[^a-z0-9]+/)
    .filter((t) => t.length > 1);
}

/**
 * Token-based relevance scoring instead of character-fuzzy matching.
 * fuzzysort (contiguous-character fuzzy matching, built for short identifiers
 * like file names) scores natural-language queries against paragraphs badly:
 * word order and non-adjacent matches are normal in prose but get penalized
 * as if they were typos. This scores by how many distinct query words
 * actually appear in the session's full conversation text (not just its
 * opening message -- see SessionRef.body), with bonuses for an exact phrase
 * match and for matches in the title specifically.
 */
function score(session: SessionRef, queryTokens: string[], queryLower: string): { s: number; matched: number } {
  const title = session.title.toLowerCase();
  const body = (session.body ?? session.snippet).toLowerCase();
  const project = session.projectPath.toLowerCase();
  const haystack = `${title} ${body} ${project}`;

  if (queryTokens.length === 0) return { s: 0, matched: 0 };

  let s = 0;
  let matched = 0;
  for (const token of queryTokens) {
    const hit = haystack.includes(token);
    if (hit) {
      s += 1;
      matched += 1;
    }
    if (title.includes(token)) s += 0.5; // title matches are more likely relevant
  }
  if (queryLower.length > 2 && haystack.includes(queryLower)) s += queryTokens.length; // exact phrase bonus
  return { s, matched };
}

export function searchSessions(sessions: SessionRef[], query: string, limit = 15): SessionRef[] {
  const trimmed = query.trim();
  if (!trimmed) {
    return [...sessions].sort((a, b) => b.updatedAt - a.updatedAt).slice(0, limit);
  }

  const queryLower = trimmed.toLowerCase();
  const queryTokens = tokenize(trimmed);
  // Short queries (the common case) require every word to match -- AND
  // semantics, like most simple search tools. Longer queries relax to a
  // majority, since requiring every word in a long natural-language query is
  // too strict (filler words shouldn't gate the match).
  const minMatched = queryTokens.length <= 3 ? queryTokens.length : Math.ceil(queryTokens.length * 0.6);

  const scored = sessions
    .map((session) => ({ session, ...score(session, queryTokens, queryLower) }))
    .filter((r) => r.matched >= minMatched);

  scored.sort((a, b) => b.s - a.s || b.session.updatedAt - a.session.updatedAt);

  return scored.slice(0, limit).map((r) => r.session);
}

export { TOOL_NAMES };
