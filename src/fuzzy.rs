//! Port of src/fuzzy.ts -- typo tolerance for search via a BK-tree (edit
//! distance) plus a sorted-array prefix index, faithful literal port.

use std::collections::HashMap;

/// Classic dynamic-programming Levenshtein distance.
pub fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let m = a.len();
    let n = b.len();
    if m == 0 {
        return n;
    }
    if n == 0 {
        return m;
    }
    let mut prev: Vec<usize> = (0..=n).collect();
    let mut curr = vec![0usize; n + 1];
    for i in 1..=m {
        curr[0] = i;
        for j in 1..=n {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            curr[j] = (prev[j] + 1).min(curr[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[n]
}

/// How permissive fuzzy matching is, scaled to word length -- matches
/// Elasticsearch's `fuzziness: AUTO` default. Short words are exact-only;
/// longer words can tolerate more typos before becoming ambiguous.
pub fn max_edit_distance(token_length: usize) -> usize {
    if token_length <= 3 {
        0
    } else if token_length <= 5 {
        1
    } else {
        2
    }
}

struct BkNode {
    word: String,
    children: HashMap<usize, BkNode>,
}

/// BK-tree: a metric tree over edit distance. Insert is O(depth) per word;
/// querying "all words within distance N" prunes almost the whole tree via
/// the triangle inequality instead of comparing against every vocabulary
/// word. Built once per corpus, queried once per query token.
pub struct BKTree {
    root: Option<BkNode>,
}

impl BKTree {
    pub fn new() -> Self {
        Self { root: None }
    }

    pub fn insert(&mut self, word: &str) {
        let Some(root) = &mut self.root else {
            self.root = Some(BkNode { word: word.to_string(), children: HashMap::new() });
            return;
        };
        let mut node = root;
        loop {
            let dist = levenshtein(word, &node.word);
            if dist == 0 {
                return; // already present
            }
            if node.children.contains_key(&dist) {
                node = node.children.get_mut(&dist).unwrap();
            } else {
                node.children.insert(dist, BkNode { word: word.to_string(), children: HashMap::new() });
                return;
            }
        }
    }

    /// All vocabulary words within `max_dist` edits of `word`, closest first.
    pub fn search(&self, word: &str, max_dist: usize) -> Vec<String> {
        let Some(root) = &self.root else { return Vec::new() };
        if max_dist == 0 {
            return Vec::new();
        }
        let mut results: Vec<(String, usize)> = Vec::new();
        let mut stack: Vec<&BkNode> = vec![root];
        while let Some(node) = stack.pop() {
            let dist = levenshtein(word, &node.word);
            if dist > 0 && dist <= max_dist {
                results.push((node.word.clone(), dist));
            }
            // triangle inequality: only children whose edge-distance is
            // within [dist - maxDist, dist + maxDist] can possibly be close
            // enough
            for (&edge_dist, child) in &node.children {
                let lower = dist.saturating_sub(max_dist);
                if edge_dist >= lower && edge_dist <= dist + max_dist {
                    stack.push(child);
                }
            }
        }
        results.sort_by_key(|(_, d)| *d);
        results.into_iter().map(|(w, _)| w).collect()
    }
}

impl Default for BKTree {
    fn default() -> Self {
        Self::new()
    }
}

/// Prefix matching -- a different problem from typo tolerance, and one
/// edit distance can't solve (a query of "auth" against "authentication"
/// differs by 10 edits, but is a genuine 4-character prefix match). A
/// sorted array + binary search finds the whole prefix range in
/// O(log n + k) without scanning the vocabulary.
pub struct PrefixIndex {
    sorted: Vec<String>,
}

impl PrefixIndex {
    pub fn new(words: impl IntoIterator<Item = String>) -> Self {
        let mut set: std::collections::BTreeSet<String> = words.into_iter().collect();
        let sorted: Vec<String> = std::mem::take(&mut set).into_iter().collect();
        Self { sorted }
    }

    /// All vocabulary words starting with `prefix` (excluding an exact
    /// match, which is handled separately), shortest -- closest to what was
    /// typed -- first.
    pub fn search(&self, prefix: &str, max_results: usize) -> Vec<String> {
        if prefix.is_empty() {
            return Vec::new();
        }
        let n = self.sorted.len();
        let mut lo = 0usize;
        let mut hi = n;
        while lo < hi {
            let mid = (lo + hi) / 2;
            if self.sorted[mid].as_str() < prefix {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        let mut results: Vec<String> = Vec::new();
        for item in &self.sorted[lo..] {
            if !item.starts_with(prefix) {
                break;
            }
            if item != prefix {
                results.push(item.clone());
            }
        }
        results.sort_by_key(|s| s.chars().count());
        results.truncate(max_results);
        results
    }
}

pub fn build_vocabulary_index(token_lists: &[Vec<String>]) -> BKTree {
    let mut tree = BKTree::new();
    let mut seen = std::collections::HashSet::new();
    for tokens in token_lists {
        for t in tokens {
            if seen.insert(t.clone()) {
                tree.insert(t);
            }
        }
    }
    tree
}

pub fn build_prefix_index(token_lists: &[Vec<String>]) -> PrefixIndex {
    let mut words = std::collections::HashSet::new();
    for tokens in token_lists {
        for t in tokens {
            words.insert(t.clone());
        }
    }
    PrefixIndex::new(words)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levenshtein_basic() {
        assert_eq!(levenshtein("kitten", "sitting"), 3);
        assert_eq!(levenshtein("", "abc"), 3);
        assert_eq!(levenshtein("abc", "abc"), 0);
    }

    #[test]
    fn bktree_finds_typo() {
        let mut tree = BKTree::new();
        for w in ["premiere", "project", "product"] {
            tree.insert(w);
        }
        let results = tree.search("permiere", max_edit_distance("permiere".len()));
        assert!(results.contains(&"premiere".to_string()));
    }

    #[test]
    fn prefix_index_finds_shortest_first() {
        let idx = PrefixIndex::new(vec!["uxp".to_string(), "uxplugin".to_string(), "unrelated".to_string()]);
        let results = idx.search("ux", 5);
        assert_eq!(results, vec!["uxp".to_string(), "uxplugin".to_string()]);
    }
}
