/**
 * Typo tolerance for search: given a query token with no exact vocabulary
 * match, find nearby real words (by edit distance) so a misspelling like
 * "permiere" still surfaces "premiere". A BK-tree makes "find all words
 * within distance N" fast without scanning the whole vocabulary per token.
 */

/** Classic dynamic-programming Levenshtein distance. */
export function levenshtein(a: string, b: string): number {
  const m = a.length;
  const n = b.length;
  if (m === 0) return n;
  if (n === 0) return m;

  let prev = new Array(n + 1);
  let curr = new Array(n + 1);
  for (let j = 0; j <= n; j++) prev[j] = j;

  for (let i = 1; i <= m; i++) {
    curr[0] = i;
    for (let j = 1; j <= n; j++) {
      const cost = a[i - 1] === b[j - 1] ? 0 : 1;
      curr[j] = Math.min(
        prev[j] + 1, // deletion
        curr[j - 1] + 1, // insertion
        prev[j - 1] + cost // substitution
      );
    }
    [prev, curr] = [curr, prev];
  }
  return prev[n];
}

/** How permissive fuzzy matching is, scaled to word length -- matches
 * Elasticsearch's `fuzziness: AUTO` default. Short words are exact-only
 * (edit distance 1 on a 3-letter word can mean almost any other 3-letter
 * word); longer words can tolerate more typos before becoming ambiguous. */
export function maxEditDistance(tokenLength: number): number {
  if (tokenLength <= 3) return 0;
  if (tokenLength <= 5) return 1;
  return 2;
}

interface BKNode {
  word: string;
  children: Map<number, BKNode>;
}

/** BK-tree: a metric tree over edit distance. Insert is O(depth) per word;
 * querying "all words within distance N" prunes almost the whole tree via
 * the triangle inequality instead of comparing against every vocabulary
 * word. Built once per corpus, queried once per query token. */
export class BKTree {
  private root: BKNode | undefined;

  insert(word: string): void {
    if (!this.root) {
      this.root = { word, children: new Map() };
      return;
    }
    let node = this.root;
    for (;;) {
      const dist = levenshtein(word, node.word);
      if (dist === 0) return; // already present
      const child = node.children.get(dist);
      if (!child) {
        node.children.set(dist, { word, children: new Map() });
        return;
      }
      node = child;
    }
  }

  /** All vocabulary words within `maxDist` edits of `word`, closest first. */
  search(word: string, maxDist: number): string[] {
    if (!this.root || maxDist <= 0) return [];
    const results: { word: string; dist: number }[] = [];
    const stack: BKNode[] = [this.root];
    while (stack.length > 0) {
      const node = stack.pop()!;
      const dist = levenshtein(word, node.word);
      if (dist > 0 && dist <= maxDist) results.push({ word: node.word, dist });
      // triangle inequality: only children whose edge-distance is within
      // [dist - maxDist, dist + maxDist] can possibly be close enough
      for (const [edgeDist, child] of node.children) {
        if (edgeDist >= dist - maxDist && edgeDist <= dist + maxDist) stack.push(child);
      }
    }
    results.sort((a, b) => a.dist - b.dist);
    return results.map((r) => r.word);
  }
}

/**
 * Prefix matching -- a different problem from typo tolerance, and one edit
 * distance can't solve: a short query term like "ux" and its intended full
 * word "uxp" often differ by more edits than the query term is even long
 * (irrelevant here, but a query of "auth" against "authentication" differs
 * by 10 edits -- there's no reasonable edit-distance threshold that catches
 * both real typos and genuine partial-word searches without also matching
 * everything). This is the classic "search-as-you-type" case: the query is
 * a deliberate prefix of a longer real word, not a misspelling of it.
 * A sorted array + binary search finds the whole prefix range in
 * O(log n + k) without scanning the vocabulary.
 */
export class PrefixIndex {
  private sorted: string[];

  constructor(words: Iterable<string>) {
    this.sorted = [...new Set(words)].sort();
  }

  /** All vocabulary words starting with `prefix` (excluding an exact match,
   * which is handled separately), shortest -- closest to what was typed --
   * first. */
  search(prefix: string, maxResults = 5): string[] {
    if (!prefix) return [];
    const n = this.sorted.length;
    let lo = 0;
    let hi = n;
    while (lo < hi) {
      const mid = (lo + hi) >>> 1;
      if (this.sorted[mid] < prefix) lo = mid + 1;
      else hi = mid;
    }
    const results: string[] = [];
    for (let i = lo; i < n; i++) {
      if (!this.sorted[i].startsWith(prefix)) break;
      if (this.sorted[i] !== prefix) results.push(this.sorted[i]);
    }
    results.sort((a, b) => a.length - b.length);
    return results.slice(0, maxResults);
  }
}

export function buildVocabularyIndex(tokenLists: string[][]): BKTree {
  const tree = new BKTree();
  const seen = new Set<string>();
  for (const tokens of tokenLists) {
    for (const t of tokens) {
      if (!seen.has(t)) {
        seen.add(t);
        tree.insert(t);
      }
    }
  }
  return tree;
}

export function buildPrefixIndex(tokenLists: string[][]): PrefixIndex {
  const words = new Set<string>();
  for (const tokens of tokenLists) for (const t of tokens) words.add(t);
  return new PrefixIndex(words);
}
