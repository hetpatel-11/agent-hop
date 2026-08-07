import { existsSync, mkdirSync, readFileSync, writeFileSync, unlinkSync } from "node:fs";
import { join } from "node:path";
import { homedir } from "node:os";
import type { SessionRef } from "./types.js";
import { ensureModel, embedText } from "./embed.js";

const INDEX_DIR = join(homedir(), ".agent-hop");
const INDEX_PATH = join(INDEX_DIR, "index.json");
const CHUNK_CHARS = 2000; // MiniLM works best on focused text, not huge blobs
const CHUNK_OVERLAP = 200; // avoid splitting a relevant sentence exactly at a chunk boundary

interface ChunkEntry {
  vector: number[];
}

interface SessionEntry {
  key: string; // `${tool}:${sessionId}`
  sourceMtime: number;
  chunks: ChunkEntry[];
}

function loadIndex(): Map<string, SessionEntry> {
  if (!existsSync(INDEX_PATH)) return new Map();
  try {
    const entries: SessionEntry[] = JSON.parse(readFileSync(INDEX_PATH, "utf-8"));
    return new Map(entries.map((e) => [e.key, e]));
  } catch {
    return new Map();
  }
}

function saveIndex(index: Map<string, SessionEntry>): void {
  mkdirSync(INDEX_DIR, { recursive: true });
  writeFileSync(INDEX_PATH, JSON.stringify([...index.values()]));
}

export function chunkSession(session: SessionRef): string[] {
  const body = session.body ?? session.snippet;
  const full = `${session.title}\n${body}`;
  if (full.length <= CHUNK_CHARS) return [full];

  const chunks: string[] = [];
  let start = 0;
  while (start < full.length) {
    chunks.push(full.slice(start, start + CHUNK_CHARS));
    start += CHUNK_CHARS - CHUNK_OVERLAP;
  }
  return chunks;
}

/**
 * Reads whatever's already been embedded from disk -- no new embedding work,
 * no blocking. Sessions that haven't been indexed yet simply don't get a
 * score (caller treats that as "no semantic signal yet, BM25 still applies").
 * This is what live search calls, so a search is never gated on indexing.
 */
export function getCachedSemanticScores(
  sessions: SessionRef[],
  queryVec: Float32Array,
  cosineSimilarity: (a: Float32Array, b: Float32Array) => number
): Map<string, number> {
  const index = loadIndex();
  const scores = new Map<string, number>();
  for (const s of sessions) {
    const key = `${s.tool}:${s.sessionId}`;
    const entry = index.get(key);
    if (!entry) continue;
    let best = -1;
    for (const chunk of entry.chunks) {
      const sim = cosineSimilarity(queryVec, Float32Array.from(chunk.vector));
      if (sim > best) best = sim;
    }
    scores.set(key, best);
  }
  return scores;
}

// A session that's still being actively written to (e.g. the very
// conversation you're having with an agent right now) has its mtime change
// on nearly every turn. Without this, every single search would see it as
// "changed since last index" and kick off a fresh background embedding run
// -- which then goes stale again within seconds, so the next search kicks
// off *another* one, forever, for as long as you keep using that agent.
// Skipping anything modified in the last few minutes means we wait for it
// to settle instead of chasing a moving target.
const SETTLE_WINDOW_MS = 3 * 60 * 1000;

function needsEmbedding(s: SessionRef, index: Map<string, SessionEntry>): boolean {
  if (Date.now() - s.updatedAt < SETTLE_WINDOW_MS) return false;
  const existing = index.get(`${s.tool}:${s.sessionId}`);
  return !existing || existing.sourceMtime !== s.updatedAt;
}

/** True if any session needs (re-)embedding since the last index build. */
export function hasPendingWork(sessions: SessionRef[]): boolean {
  const index = loadIndex();
  return sessions.some((s) => needsEmbedding(s, index));
}

const LOCK_PATH = join(INDEX_DIR, "indexing.lock");

function isLockStale(): boolean {
  try {
    const pid = Number(readFileSync(LOCK_PATH, "utf-8").trim());
    process.kill(pid, 0); // throws if the process doesn't exist
    return false;
  } catch {
    return true; // no lock, unparseable, or process is gone
  }
}

/**
 * Actually does the embedding work -- sequential, in-process. Meant to run
 * inside the detached background process (see background-index.ts), never
 * inline in an interactive search. Writes progressively so a killed/crashed
 * run doesn't lose everything already completed.
 */
export async function buildIndex(sessions: SessionRef[], onProgress?: (done: number, total: number) => void): Promise<void> {
  mkdirSync(INDEX_DIR, { recursive: true });
  if (existsSync(LOCK_PATH) && !isLockStale()) return; // another run already in progress
  writeFileSync(LOCK_PATH, String(process.pid));

  try {
    const index = loadIndex();
    const toEmbed = sessions.filter((s) => needsEmbedding(s, index));
    if (toEmbed.length === 0) return;

    await ensureModel();
    for (let i = 0; i < toEmbed.length; i++) {
      const s = toEmbed[i];
      const key = `${s.tool}:${s.sessionId}`;
      const texts = chunkSession(s);
      const chunks: ChunkEntry[] = [];
      for (const text of texts) {
        const vec = await embedText(text);
        chunks.push({ vector: Array.from(vec) });
      }
      index.set(key, { key, sourceMtime: s.updatedAt, chunks });
      onProgress?.(i + 1, toEmbed.length);
      // save incrementally every few sessions so progress isn't lost if
      // this background process gets killed partway through
      if (i % 10 === 0) saveIndex(index);
    }

    const liveKeys = new Set(sessions.map((s) => `${s.tool}:${s.sessionId}`));
    for (const key of [...index.keys()]) {
      if (!liveKeys.has(key)) index.delete(key);
    }
    saveIndex(index);
  } finally {
    try {
      unlinkSync(LOCK_PATH);
    } catch {
      // already gone, fine
    }
  }
}
