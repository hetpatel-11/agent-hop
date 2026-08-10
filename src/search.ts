import { spawn } from "node:child_process";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { setPriority, constants as osConstants } from "node:os";
import type { SessionRef, ToolName } from "./types.js";
import { ADAPTERS, TOOL_NAMES } from "./adapters/index.js";
import { getCachedSemanticScores, hasPendingWork } from "./vector-index.js";
import { embedText, ensureModel, cosineSimilarity } from "./embed.js";
import { color } from "./theme.js";
import { BKTree, PrefixIndex, buildVocabularyIndex, buildPrefixIndex, maxEditDistance } from "./fuzzy.js";

const __dirname = dirname(fileURLToPath(import.meta.url));

/** Fire-and-forget: spawns the background indexer detached from this
 * process, so it keeps running (and writing progress to the persistent
 * index) even after this CLI invocation exits -- e.g. once you've picked a
 * result and resumed into an agent. Never awaited, never blocks search. */
/** Checks for unindexed sessions and kicks off the background embedder if
 * needed -- shared by both the full hybrid search and the live-typing
 * ranker, which otherwise has no reason to touch indexing at all. */
export function ensureIndexingTriggered(sessions: SessionRef[]): boolean {
  const pending = hasPendingWork(sessions);
  if (pending) triggerBackgroundIndexing();
  return pending;
}

function triggerBackgroundIndexing(): void {
  const child = spawn(process.execPath, [join(__dirname, "background-index.js")], {
    detached: true,
    stdio: "ignore",
  });
  // onnxruntime-web's WASM backend can use its own internal thread pool
  // during inference -- it doesn't make embedding faster (measured earlier),
  // but it can still transiently saturate most/all CPU cores per call, which
  // starves whatever foreground process you're actually looking at (e.g. an
  // agent's TUI you just resumed into) of scheduling time and shows up as
  // input lag with no obvious cause. This is a true background task, so tell
  // the OS to schedule it at low priority -- best-effort, not fatal if the
  // platform doesn't allow it.
  if (child.pid !== undefined) {
    try {
      setPriority(child.pid, osConstants.priority.PRIORITY_LOW);
    } catch {
      // not fatal -- indexing still runs, just at normal priority
    }
  }
  child.unref();
}

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
  return deduplicate(results.flat());
}

/**
 * Drops near-duplicate sessions within the same tool (same opening content,
 * different session id -- e.g. from a fork, a resumed-then-re-saved session,
 * or a near-empty system-generated session that repeats verbatim). Keeps the
 * most recently updated copy. Cross-tool duplicates are left alone -- if you
 * asked the same question in Claude and Pi, both are genuinely useful to see.
 */
function deduplicate(sessions: SessionRef[]): SessionRef[] {
  const groups = new Map<string, SessionRef[]>();
  for (const s of sessions) {
    const normalized = (s.body ?? s.snippet).trim().toLowerCase().replace(/\s+/g, " ").slice(0, 200);
    const key = `${s.tool}:${normalized}`;
    if (!normalized) {
      // nothing to dedupe on -- keep as its own group
      groups.set(`${s.tool}:${s.sessionId}`, [s]);
      continue;
    }
    if (!groups.has(key)) groups.set(key, []);
    groups.get(key)!.push(s);
  }
  const out: SessionRef[] = [];
  for (const group of groups.values()) {
    group.sort((a, b) => b.updatedAt - a.updatedAt);
    out.push(group[0]);
  }
  return out;
}

function tokenize(text: string): string[] {
  return text
    .toLowerCase()
    .split(/[^a-z0-9]+/)
    .filter((t) => t.length > 1);
}

/**
 * Okapi BM25 -- the standard keyword-relevance algorithm (what Elasticsearch/
 * Lucene/Solr default to). Improves on plain term-presence matching two ways
 * that directly mattered here: term-frequency saturation (a document can't
 * dominate just by repeating a word many times) and document-length
 * normalization (a huge document that happens to contain a query word once
 * gets correctly discounted relative to a short, genuinely-on-topic one).
 */
/** A query term to score against, with a weight -- 1.0 for a term that
 * appears verbatim in the corpus, less for a fuzzy-matched substitute
 * (typo tolerance), so a guessed correction never outweighs a real hit. */
interface WeightedTerm {
  term: string;
  weight: number;
}

const FUZZY_MATCH_WEIGHT = 0.6;
// Higher than the fuzzy weight -- a prefix match is a deliberate, precise
// signal ("ux" typed as the start of "uxp"), not a guessed correction the
// way a typo-fix is. Still below 1.0 so a genuine exact match always wins.
const PREFIX_MATCH_WEIGHT = 0.85;
// A concatenated query like "agenthop" should still match documents that
// tokenized it as "agent hop" or "agent-hop". This is neither a typo nor a
// prefix; it is decompounding an unknown token into known corpus terms.
const COMPOUND_MATCH_WEIGHT = 0.9;

class BM25 {
  private readonly k1 = 1.5;
  private readonly b = 0.75;
  private docs: string[][] = [];
  private docFreq = new Map<string, number>(); // term -> number of docs containing it
  private avgDocLen = 0;

  constructor(documents: string[]) {
    this.docs = documents.map(tokenize);
    this.avgDocLen = this.docs.reduce((sum, d) => sum + d.length, 0) / (this.docs.length || 1);
    for (const doc of this.docs) {
      for (const term of new Set(doc)) {
        this.docFreq.set(term, (this.docFreq.get(term) ?? 0) + 1);
      }
    }
  }

  private idf(term: string): number {
    const n = this.docFreq.get(term) ?? 0;
    const N = this.docs.length;
    return Math.log((N - n + 0.5) / (n + 0.5) + 1);
  }

  /** True if this exact token appears anywhere in the corpus -- used to
   * decide whether a query token needs fuzzy substitution at all. */
  hasTerm(term: string): boolean {
    return this.docFreq.has(term);
  }

  score(docIndex: number, weightedTerms: WeightedTerm[]): number {
    const doc = this.docs[docIndex];
    const docLen = doc.length;
    let score = 0;
    for (const { term, weight } of weightedTerms) {
      const freq = doc.filter((t) => t === term).length;
      if (freq === 0) continue;
      const idf = this.idf(term);
      const numerator = freq * (this.k1 + 1);
      const denominator = freq + this.k1 * (1 - this.b + (this.b * docLen) / this.avgDocLen);
      score += weight * idf * (numerator / denominator);
    }
    return score;
  }
}

/** For each query token, in priority order: keep it as-is if it appears
 * verbatim in the corpus; otherwise check whether it's a genuine prefix of
 * a longer real word ("ux" -> "uxp": edit distance 1, but the *length*-based
 * fuzzy tier for a 2-char term is 0, so fuzzy alone would silently drop it
 * entirely -- prefix matching is what actually catches this, since it's a
 * different phenomenon than a typo and doesn't degrade with length the way
 * edit distance does: "auth" -> "authentication" is 10 edits apart but a
 * 4-character exact prefix); otherwise fall back to edit-distance typo
 * tolerance. A token matching none of these contributes nothing. */
function splitCompoundToken(term: string, bm25: BM25): string[] | undefined {
  // Avoid turning short/noisy tokens into accidental two-letter fragments.
  if (term.length < 6) return undefined;

  const memo = new Map<number, string[] | undefined>();
  const solve = (start: number): string[] | undefined => {
    if (start === term.length) return [];
    if (memo.has(start)) return memo.get(start);

    for (let end = term.length; end >= start + 2; end--) {
      const part = term.slice(start, end);
      if (!bm25.hasTerm(part)) continue;
      const rest = solve(end);
      if (rest) {
        const result = [part, ...rest];
        memo.set(start, result);
        return result;
      }
    }

    memo.set(start, undefined);
    return undefined;
  };

  const parts = solve(0);
  // Require at least two real words. This keeps normal exact-vocab terms
  // untouched and only handles true concatenations.
  return parts && parts.length >= 2 ? parts : undefined;
}

function expandQueryTerms(queryTerms: string[], bm25: BM25, vocabTree: BKTree, prefixIndex: PrefixIndex): WeightedTerm[] {
  const expanded: WeightedTerm[] = [];
  for (const term of queryTerms) {
    if (bm25.hasTerm(term)) {
      expanded.push({ term, weight: 1 });
      continue;
    }
    const compoundParts = splitCompoundToken(term, bm25);
    if (compoundParts) {
      for (const part of compoundParts) expanded.push({ term: part, weight: COMPOUND_MATCH_WEIGHT });
      continue;
    }
    const prefixMatches = prefixIndex.search(term);
    if (prefixMatches.length > 0) {
      expanded.push({ term: prefixMatches[0], weight: PREFIX_MATCH_WEIGHT });
      continue;
    }
    const maxDist = maxEditDistance(term.length);
    if (maxDist === 0) continue; // too short to fuzzy-match safely, and no prefix hit either
    const matches = vocabTree.search(term, maxDist);
    if (matches.length > 0) expanded.push({ term: matches[0], weight: FUZZY_MATCH_WEIGHT });
  }
  return expanded;
}

function escapeRegExp(s: string): string {
  return s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

// clack's single-line inline hint is the only per-item context it can show
// as you move through the list (no side panel is possible), so give it more
// room than a typical search-result snippet -- natural terminal-width
// wrapping is harmless (only literal newlines inside the string break
// clack's rendering), so a longer window just means more real conversation
// text is visible without needing a second pane.
const SNIPPET_WINDOW_CHARS = 130;

/**
 * Excerpt around wherever a query term first appears in the body, with
 * matches highlighted -- this is what actually answers "is this the chat I
 * meant," which a truncated opening line alone can't (the opening line is
 * often boilerplate or a greeting, not the part that matched).
 */
function buildSnippet(body: string, queryTerms: string[]): string | undefined {
  if (!body || queryTerms.length === 0) return undefined;
  const lower = body.toLowerCase();
  // \b word-boundary matches only -- a plain substring search would let a
  // short term like "pro" match inside "project" or "provide", both for
  // picking the excerpt location and for highlighting.
  let bestIndex = -1;
  for (const term of queryTerms) {
    const m = lower.match(new RegExp(`\\b${escapeRegExp(term)}\\b`));
    if (m?.index !== undefined && (bestIndex === -1 || m.index < bestIndex)) bestIndex = m.index;
  }
  if (bestIndex === -1) return undefined;

  const start = Math.max(0, bestIndex - SNIPPET_WINDOW_CHARS);
  const end = Math.min(body.length, bestIndex + SNIPPET_WINDOW_CHARS);
  let snippet = body.slice(start, end).replace(/\s+/g, " ").trim();

  for (const term of queryTerms) {
    if (term.length < 2) continue;
    const re = new RegExp(`\\b(${escapeRegExp(term)})\\b`, "gi");
    snippet = snippet.replace(re, (m) => color.bold(color.yellow(m)));
  }

  return (start > 0 ? "…" : "") + snippet + (end < body.length ? "…" : "");
}

// Half-life decay: a session updated "today" scores ~1.0, two weeks old
// scores ~0.5, a month old ~0.25, and so on -- recent sessions get a real
// nudge without a genuinely better-matching old session getting buried
// under a barely-relevant recent one (it's 20% of the combined score, not
// the deciding factor).
const RECENCY_HALF_LIFE_DAYS = 14;

function recencyScore(updatedAt: number): number {
  const ageDays = (Date.now() - updatedAt) / (1000 * 60 * 60 * 24);
  return Math.pow(0.5, ageDays / RECENCY_HALF_LIFE_DAYS);
}

function minMaxNormalize(values: number[]): number[] {
  const max = Math.max(...values, 0);
  const min = Math.min(...values, 0);
  const range = max - min || 1;
  return values.map((v) => (v - min) / range);
}

export interface SearchOptions {
  limit?: number;
}

export interface SearchResult {
  results: SessionRef[];
  /** true if some sessions haven't been semantically indexed yet -- search
   * still ran (BM25 + whatever's cached), and background indexing was
   * kicked off so the *next* search has fuller semantic coverage. */
  indexingInBackground: boolean;
}

interface LexicalIndex {
  bm25: BM25;
  vocabTree: BKTree;
  prefixIndex: PrefixIndex;
  sessions: SessionRef[];
}

// Matches the adapters' own MAX_BODY_CHARS cap -- raising this further would
// be pointless, `body` never has more text than that regardless. Measured
// safe at this size: ~15ms one-time index build, ~15ms per-keystroke scoring
// (383 real sessions) -- both well under perceptible-lag thresholds.
const BM25_DOC_CHAR_CAP = 40000;

function buildLexicalIndex(sessions: SessionRef[]): LexicalIndex {
  const documents = sessions.map((s) => {
    const body = (s.body ?? s.snippet).slice(0, BM25_DOC_CHAR_CAP);
    // title repeated to weight it naturally in BM25's term-frequency signal
    return `${s.title} ${s.title} ${body} ${s.projectPath}`;
  });
  const bm25 = new BM25(documents);
  const tokenLists = documents.map(tokenize);
  const vocabTree = buildVocabularyIndex(tokenLists);
  const prefixIndex = buildPrefixIndex(tokenLists);
  return { bm25, vocabTree, prefixIndex, sessions };
}

function lexicalScores(index: LexicalIndex, trimmedQuery: string): { queryTerms: string[]; bm25Normalized: number[] } {
  const queryTerms = tokenize(trimmedQuery);
  // expandQueryTerms's fuzzy/prefix/compound substitutions (e.g. "uxp" ->
  // some unrelated edit-distance-close vocab word) feed BM25 scoring only
  // -- they must never reach buildSnippet's highlighting, or a result can
  // show a bolded "match" for a word the user never typed and that doesn't
  // actually appear near their real query (confirmed as a real, reported
  // bug: searching "uxp plugin" highlighted a result with no mention of
  // either word anywhere in it). Snippet highlighting always uses the
  // literal queryTerms below, kept separate from this expansion.
  const weighted = expandQueryTerms(queryTerms, index.bm25, index.vocabTree, index.prefixIndex);
  const bm25Scores = index.sessions.map((_, i) => index.bm25.score(i, weighted));
  return { queryTerms, bm25Normalized: minMaxNormalize(bm25Scores) };
}

/** Exact-match tier, recency multiplier, meaningful-score cutoff, and
 * snippet attachment -- shared by both the sync stage-1 ranker and the
 * async semantic-refined stage-2, so the two only ever differ in what
 * `relevanceScores` they were handed. */
function applyRankingLayers(
  sessions: SessionRef[],
  snippetTerms: string[],
  trimmedQuery: string,
  relevanceScores: number[],
  limit: number,
  meaningfulThreshold: number
): SessionRef[] {
  // If your exact query phrase literally appears in a session, that's a much
  // stronger signal than blended relevance -- and among exact matches, the
  // most recent one is very likely what you're after (you're searching for
  // a specific chat you remember having, not doing topic research). These
  // go first, most-recent-first, ahead of the blended-score ranking below.
  const lowerQuery = trimmedQuery.toLowerCase();
  // Title + opening of the conversation only -- not the full (up to 40k
  // char) body. A query phrase appearing once, deep in a long unrelated
  // session, isn't the same signal as it being what the chat opens with or
  // is titled around; treating any incidental mention as an "exact match"
  // let long, tangential sessions dominate purely on recency.
  const isExactMatch = (s: SessionRef) => `${s.title} ${(s.body ?? s.snippet).slice(0, 1000)}`.toLowerCase().includes(lowerQuery);

  const combined = sessions.map((session, i) => ({
    session,
    // Multiplicative, not additive -- recency should amplify an already-
    // relevant result, not stand in for relevance on its own. Additive
    // scoring let a barely-relevant-but-recent session outrank a clearly
    // on-topic older one, since min-max normalization gives even irrelevant
    // docs some nonzero score to work with. A 0-relevance doc times any
    // recency boost is still 0.
    score: relevanceScores[i] * (1 + 0.5 * recencyScore(session.updatedAt)),
  }));

  combined.sort((a, b) => {
    const aExact = isExactMatch(a.session);
    const bExact = isExactMatch(b.session);
    if (aExact !== bExact) return aExact ? -1 : 1;
    if (aExact && bExact) return b.session.updatedAt - a.session.updatedAt;
    return b.score - a.score || b.session.updatedAt - a.session.updatedAt;
  });

  // keep only results with a non-trivial combined score -- otherwise, on a
  // query that matches almost nothing, we'd still show `limit` results that
  // are really just "least irrelevant". Exact matches always count as
  // meaningful regardless of blended score.
  //
  // Deliberately no "show top 3 anyway" fallback when nothing meets the
  // threshold: that used to paper over genuine emptiness with low-relevance
  // junk, which meant a real "no results" state could never actually reach
  // the caller (a non-empty session corpus made this function return
  // *something* for literally any query, however irrelevant) -- so a
  // "no results found" message built on top of this could never fire, and
  // "results.length === 0" checks elsewhere were silently dead code. Let
  // genuine emptiness propagate; the caller decides what to show for it.
  const meaningful = combined.filter((r) => r.score > meaningfulThreshold || isExactMatch(r.session));
  return meaningful.slice(0, limit).map((r) => {
    const matchSnippet = buildSnippet(r.session.body ?? r.session.snippet, snippetTerms);
    return matchSnippet ? { ...r.session, matchSnippet } : r.session;
  });
}

// Repeated/backspaced-back-to queries during live typing shouldn't re-embed
// -- the model call is the one genuinely slow part of a refine pass.
const queryEmbedCache = new Map<string, Float32Array>();

async function embedQueryCached(query: string): Promise<Float32Array> {
  const cached = queryEmbedCache.get(query);
  if (cached) return cached;
  const vec = await embedText(query);
  queryEmbedCache.set(query, vec);
  return vec;
}

// Even split: BM25 alone was measurably losing real, genuinely-relevant
// sessions to unrelated ones that happened to repeat a query word many times
// (a whole conversation is one BM25 "document," so a focused single mention
// in a long, on-topic session can lose to incidental repetition in a long,
// unrelated one -- see the "personal website" / "ux plugin" investigation).
// Verified empirically across several real queries before changing this:
// raising semantic's share from 30% to 50% fixed both broken cases with no
// regression on queries that were already ranking well (e.g. "adobe
// premiere pro" stayed rank-stable at every weight tested from 30-70%).
// Both halves are min-max normalized to comparable [0,1] ranges before
// blending -- raw BM25 and raw cosine similarity live on different scales,
// so blending them unnormalized isn't actually an even split, it's whatever
// their raw magnitudes happen to be.
const BM25_WEIGHT = 0.5;
const SEMANTIC_WEIGHT = 0.5;

export interface Ranker {
  /** Synchronous re-rank for a new query -- cheap enough to call on every
   * keystroke (BM25 with fuzzy typo tolerance + exact-match tier + recency,
   * no semantic component -- that needs an async model call, which can't
   * fit a synchronous per-keystroke callback). This is stage 1: instant
   * feedback while typing. */
  rank(query: string, limit?: number): SessionRef[];
  /** Async upgrade over the same query -- adds semantic similarity into the
   * blend, reusing the same lexical index (no BM25 rebuild). This is stage
   * 2: call it debounced after typing pauses, and discard the result if a
   * newer query has since been entered (generation check is the caller's
   * responsibility, not this function's). */
  refineWithSemantic(query: string, limit?: number): Promise<SessionRef[]>;
}

export function buildRanker(sessions: SessionRef[]): Ranker {
  const index = buildLexicalIndex(sessions);

  return {
    rank(query: string, limit = 15): SessionRef[] {
      const trimmed = query.trim();
      if (!trimmed) return [...sessions].sort((a, b) => b.updatedAt - a.updatedAt).slice(0, limit);
      const { queryTerms, bm25Normalized } = lexicalScores(index, trimmed);
      return applyRankingLayers(sessions, queryTerms, trimmed, bm25Normalized, limit, 0.1);
    },

    async refineWithSemantic(query: string, limit = 15): Promise<SessionRef[]> {
      const trimmed = query.trim();
      if (!trimmed) return [...sessions].sort((a, b) => b.updatedAt - a.updatedAt).slice(0, limit);

      const { queryTerms, bm25Normalized } = lexicalScores(index, trimmed);

      let semanticNormalized = new Array(sessions.length).fill(0);
      try {
        await ensureModel();
        const queryVec = await embedQueryCached(trimmed);
        const scoreMap = getCachedSemanticScores(sessions, queryVec, cosineSimilarity);
        const semanticRaw = sessions.map((s) => scoreMap.get(`${s.tool}:${s.sessionId}`) ?? 0);
        semanticNormalized = minMaxNormalize(semanticRaw);
      } catch {
        // offline on first run, disk issue, unsupported platform -- fall
        // back to lexical-only rather than failing the refine
      }

      const relevance = sessions.map((_, i) => BM25_WEIGHT * bm25Normalized[i] + SEMANTIC_WEIGHT * semanticNormalized[i]);
      return applyRankingLayers(sessions, queryTerms, trimmed, relevance, limit, 0.15);
    },
  };
}

/**
 * One-shot hybrid search (BM25 + fuzzy + semantic, blended) for when a query
 * is already fully known -- a CLI argument, or non-interactive mode -- so
 * there's no live typing to stage. Built on top of buildRanker() rather than
 * duplicating its logic.
 *
 * Never blocks on embedding the corpus: semantic scoring only uses whatever
 * is already in the persistent index (instant, disk read). Anything not yet
 * indexed just contributes 0 to the semantic half for this search, and a
 * detached background process is kicked off to catch it up for next time.
 */
export async function searchSessions(
  sessions: SessionRef[],
  query: string,
  opts: SearchOptions = {}
): Promise<SearchResult> {
  const limit = opts.limit ?? 25;
  const trimmed = query.trim();

  const pending = ensureIndexingTriggered(sessions);

  if (!trimmed) {
    const results = [...sessions].sort((a, b) => b.updatedAt - a.updatedAt).slice(0, limit);
    return { results, indexingInBackground: pending };
  }
  if (sessions.length === 0) return { results: [], indexingInBackground: pending };

  const ranker = buildRanker(sessions);
  const results = await ranker.refineWithSemantic(trimmed, limit);
  return { results, indexingInBackground: pending };
}

export { TOOL_NAMES };
