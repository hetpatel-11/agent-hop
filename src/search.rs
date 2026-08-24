//! Port of src/search.ts -- BM25 keyword ranking with fuzzy/prefix/compound
//! query expansion, blended with semantic (embedding) similarity when
//! available. Faithful, literal port -- comments carried over (adapted to
//! Rust) from the TS source since they document real, hard-won tuning
//! decisions (weights, thresholds) verified against real queries, not
//! guessed.

use crate::adapters::{adapter_for, SessionRef};
use crate::agents::ToolName;
use crate::fuzzy::{build_prefix_index, build_vocabulary_index, max_edit_distance, BKTree, PrefixIndex};
use crate::theme;
use crate::vector_index::{get_cached_semantic_scores, has_pending_work};
use regex::Regex;
use std::collections::{HashMap, HashSet};

pub fn collect_sessions(tools: &[ToolName]) -> Vec<SessionRef> {
    let mut all = Vec::new();
    for &tool in tools {
        if let Ok(sessions) = adapter_for(tool).list_sessions() {
            all.extend(sessions);
        }
    }
    deduplicate(all)
}

/// Drops near-duplicate sessions within the same tool (same opening
/// content, different session id -- e.g. from a fork, a resumed-then-
/// re-saved session, or a near-empty system-generated session that
/// repeats verbatim). Keeps the most recently updated copy. Cross-tool
/// duplicates are left alone -- if you asked the same question in Claude
/// and Pi, both are genuinely useful to see.
fn deduplicate(sessions: Vec<SessionRef>) -> Vec<SessionRef> {
    let mut groups: HashMap<String, Vec<SessionRef>> = HashMap::new();
    for s in sessions {
        let raw = s.body.clone().unwrap_or_else(|| s.snippet.clone());
        let normalized: String = raw.trim().to_lowercase().split_whitespace().collect::<Vec<_>>().join(" ").chars().take(200).collect();
        if normalized.is_empty() {
            let key = format!("{}:{}", s.tool.slug(), s.session_id);
            groups.entry(key).or_default().push(s);
            continue;
        }
        let key = format!("{}:{}", s.tool.slug(), normalized);
        groups.entry(key).or_default().push(s);
    }
    let mut out = Vec::new();
    for mut group in groups.into_values() {
        group.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        out.push(group.remove(0));
    }
    out
}

fn tokenize(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !(c.is_ascii_alphanumeric()))
        .filter(|t| t.chars().count() > 1)
        .map(|s| s.to_string())
        .collect()
}

struct WeightedTerm {
    term: String,
    weight: f64,
}

const FUZZY_MATCH_WEIGHT: f64 = 0.6;
// Higher than the fuzzy weight -- a prefix match is a deliberate, precise
// signal, not a guessed correction. Still below 1.0 so a genuine exact
// match always wins.
const PREFIX_MATCH_WEIGHT: f64 = 0.85;
// A concatenated query like "agenthop" should still match documents that
// tokenized it as "agent hop" or "agent-hop" -- decompounding an unknown
// token into known corpus terms.
const COMPOUND_MATCH_WEIGHT: f64 = 0.9;

/// Okapi BM25 -- the standard keyword-relevance algorithm (what
/// Elasticsearch/Lucene/Solr default to). Improves on plain term-presence
/// matching two ways: term-frequency saturation (a document can't
/// dominate just by repeating a word many times) and document-length
/// normalization.
struct Bm25 {
    k1: f64,
    b: f64,
    docs: Vec<Vec<String>>,
    doc_freq: HashMap<String, usize>,
    avg_doc_len: f64,
}

impl Bm25 {
    fn new(documents: &[String]) -> Self {
        let docs: Vec<Vec<String>> = documents.iter().map(|d| tokenize(d)).collect();
        let avg_doc_len = if docs.is_empty() {
            0.0
        } else {
            docs.iter().map(|d| d.len()).sum::<usize>() as f64 / docs.len() as f64
        };
        let mut doc_freq: HashMap<String, usize> = HashMap::new();
        for doc in &docs {
            let unique: HashSet<&String> = doc.iter().collect();
            for term in unique {
                *doc_freq.entry(term.clone()).or_insert(0) += 1;
            }
        }
        Self { k1: 1.5, b: 0.75, docs, doc_freq, avg_doc_len }
    }

    fn idf(&self, term: &str) -> f64 {
        let n = *self.doc_freq.get(term).unwrap_or(&0) as f64;
        let total = self.docs.len() as f64;
        ((total - n + 0.5) / (n + 0.5) + 1.0).ln()
    }

    /// True if this exact token appears anywhere in the corpus -- used to
    /// decide whether a query token needs fuzzy substitution at all.
    fn has_term(&self, term: &str) -> bool {
        self.doc_freq.contains_key(term)
    }

    fn score(&self, doc_index: usize, weighted_terms: &[WeightedTerm]) -> f64 {
        let doc = &self.docs[doc_index];
        let doc_len = doc.len() as f64;
        let mut score = 0.0;
        for wt in weighted_terms {
            let freq = doc.iter().filter(|t| *t == &wt.term).count() as f64;
            if freq == 0.0 {
                continue;
            }
            let idf = self.idf(&wt.term);
            let numerator = freq * (self.k1 + 1.0);
            let denominator = freq + self.k1 * (1.0 - self.b + (self.b * doc_len) / self.avg_doc_len);
            score += wt.weight * idf * (numerator / denominator);
        }
        score
    }
}

/// For each query token, in priority order: keep it as-is if it appears
/// verbatim in the corpus; otherwise check whether it's a genuine prefix of
/// a longer real word; otherwise fall back to edit-distance typo
/// tolerance. A token matching none of these contributes nothing.
fn split_compound_token(term: &str, bm25: &Bm25) -> Option<Vec<String>> {
    // Avoid turning short/noisy tokens into accidental two-letter fragments.
    if term.chars().count() < 6 {
        return None;
    }
    let chars: Vec<char> = term.chars().collect();
    let len = chars.len();
    let mut memo: HashMap<usize, Option<Vec<String>>> = HashMap::new();

    fn solve(start: usize, chars: &[char], len: usize, bm25: &Bm25, memo: &mut HashMap<usize, Option<Vec<String>>>) -> Option<Vec<String>> {
        if start == len {
            return Some(Vec::new());
        }
        if let Some(cached) = memo.get(&start) {
            return cached.clone();
        }
        let mut end = len;
        while end >= start + 2 {
            let part: String = chars[start..end].iter().collect();
            if bm25.has_term(&part) {
                if let Some(rest) = solve(end, chars, len, bm25, memo) {
                    let mut result = vec![part];
                    result.extend(rest);
                    memo.insert(start, Some(result.clone()));
                    return Some(result);
                }
            }
            if end == 0 {
                break;
            }
            end -= 1;
        }
        memo.insert(start, None);
        None
    }

    let parts = solve(0, &chars, len, bm25, &mut memo);
    // Require at least two real words. This keeps normal exact-vocab terms
    // untouched and only handles true concatenations.
    parts.filter(|p| p.len() >= 2)
}

fn expand_query_terms(query_terms: &[String], bm25: &Bm25, vocab_tree: &BKTree, prefix_index: &PrefixIndex) -> Vec<WeightedTerm> {
    let mut expanded = Vec::new();
    for term in query_terms {
        if bm25.has_term(term) {
            expanded.push(WeightedTerm { term: term.clone(), weight: 1.0 });
            continue;
        }
        if let Some(parts) = split_compound_token(term, bm25) {
            for part in parts {
                expanded.push(WeightedTerm { term: part, weight: COMPOUND_MATCH_WEIGHT });
            }
            continue;
        }
        let prefix_matches = prefix_index.search(term, 5);
        if let Some(first) = prefix_matches.into_iter().next() {
            expanded.push(WeightedTerm { term: first, weight: PREFIX_MATCH_WEIGHT });
            continue;
        }
        let max_dist = max_edit_distance(term.chars().count());
        if max_dist == 0 {
            continue; // too short to fuzzy-match safely, and no prefix hit either
        }
        let matches = vocab_tree.search(term, max_dist);
        if let Some(first) = matches.into_iter().next() {
            expanded.push(WeightedTerm { term: first, weight: FUZZY_MATCH_WEIGHT });
        }
    }
    expanded
}

// Only a single per-length snippet-highlight window, rebuilt per call
// since query terms vary -- no persistent-regex-cache concern in Rust the
// way there might be constructing `new RegExp` in a hot loop in JS.
const SNIPPET_WINDOW_CHARS: usize = 130;

/// Excerpt around wherever a query term first appears in the body, with
/// matches highlighted -- this is what actually answers "is this the chat
/// I meant," which a truncated opening line alone can't.
fn build_snippet(body: &str, query_terms: &[String]) -> Option<String> {
    if body.is_empty() || query_terms.is_empty() {
        return None;
    }
    let lower = body.to_lowercase();
    let mut best_index: Option<usize> = None;
    for term in query_terms {
        let Ok(re) = Regex::new(&format!(r"\b{}\b", regex::escape(term))) else { continue };
        if let Some(m) = re.find(&lower) {
            if best_index.is_none() || m.start() < best_index.unwrap() {
                best_index = Some(m.start());
            }
        }
    }
    let best_index = best_index?;

    // byte-offset based windowing (body may contain multi-byte UTF-8, but
    // regex match offsets are byte offsets already, consistent here).
    let start = best_index.saturating_sub(SNIPPET_WINDOW_CHARS);
    let end = (best_index + SNIPPET_WINDOW_CHARS).min(body.len());
    let start = floor_char_boundary(body, start);
    let end = ceil_char_boundary(body, end);
    let mut snippet: String = body[start..end].split_whitespace().collect::<Vec<_>>().join(" ");

    for term in query_terms {
        if term.chars().count() < 2 {
            continue;
        }
        let Ok(re) = Regex::new(&format!(r"(?i)\b({})\b", regex::escape(term))) else { continue };
        snippet = re.replace_all(&snippet, |caps: &regex::Captures| theme::bold(&theme::yellow(&caps[1]))).to_string();
    }

    let prefix = if start > 0 { "\u{2026}" } else { "" };
    let suffix = if end < body.len() { "\u{2026}" } else { "" };
    Some(format!("{prefix}{snippet}{suffix}"))
}

fn floor_char_boundary(s: &str, mut idx: usize) -> usize {
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}
fn ceil_char_boundary(s: &str, mut idx: usize) -> usize {
    while idx < s.len() && !s.is_char_boundary(idx) {
        idx += 1;
    }
    idx
}

// Half-life decay: a session updated "today" scores ~1.0, two weeks old
// scores ~0.5, a month old ~0.25, and so on.
const RECENCY_HALF_LIFE_DAYS: f64 = 14.0;

fn recency_score(updated_at: i64) -> f64 {
    let age_days = (chrono::Utc::now().timestamp_millis() - updated_at) as f64 / (1000.0 * 60.0 * 60.0 * 24.0);
    0.5f64.powf(age_days / RECENCY_HALF_LIFE_DAYS)
}

fn min_max_normalize(values: &[f64]) -> Vec<f64> {
    let max = values.iter().cloned().fold(0.0, f64::max);
    let min = values.iter().cloned().fold(0.0, f64::min);
    let range = if max - min == 0.0 { 1.0 } else { max - min };
    values.iter().map(|v| (v - min) / range).collect()
}

pub struct SearchOptions {
    pub limit: usize,
}
impl Default for SearchOptions {
    fn default() -> Self {
        Self { limit: 25 }
    }
}

pub struct SearchResult {
    pub results: Vec<SessionRef>,
    /// true if some sessions haven't been semantically indexed yet.
    pub indexing_in_background: bool,
}

struct LexicalIndex {
    bm25: Bm25,
    vocab_tree: BKTree,
    prefix_index: PrefixIndex,
}

// Matches the adapters' own MAX_BODY_CHARS cap.
const BM25_DOC_CHAR_CAP: usize = 40_000;

fn build_lexical_index(sessions: &[SessionRef]) -> LexicalIndex {
    let documents: Vec<String> = sessions
        .iter()
        .map(|s| {
            let body_full = s.body.clone().unwrap_or_else(|| s.snippet.clone());
            let body: String = body_full.chars().take(BM25_DOC_CHAR_CAP).collect();
            // title repeated to weight it naturally in BM25's term-frequency signal
            format!("{} {} {} {}", s.title, s.title, body, s.project_path)
        })
        .collect();
    let bm25 = Bm25::new(&documents);
    let token_lists: Vec<Vec<String>> = documents.iter().map(|d| tokenize(d)).collect();
    let vocab_tree = build_vocabulary_index(&token_lists);
    let prefix_index = build_prefix_index(&token_lists);
    LexicalIndex { bm25, vocab_tree, prefix_index }
}

fn lexical_scores(index: &LexicalIndex, sessions: &[SessionRef], trimmed_query: &str) -> (Vec<String>, Vec<String>, Vec<f64>) {
    let query_terms = tokenize(trimmed_query);
    let weighted = expand_query_terms(&query_terms, &index.bm25, &index.vocab_tree, &index.prefix_index);
    let mut match_terms: Vec<String> = query_terms.clone();
    for w in &weighted {
        if !match_terms.contains(&w.term) {
            match_terms.push(w.term.clone());
        }
    }
    let bm25_scores: Vec<f64> = (0..sessions.len()).map(|i| index.bm25.score(i, &weighted)).collect();
    (query_terms, match_terms, min_max_normalize(&bm25_scores))
}

/// Exact-match tier, recency multiplier, meaningful-score cutoff, and
/// snippet attachment -- shared by both the sync stage-1 ranker and the
/// async semantic-refined stage-2.
fn apply_ranking_layers(
    sessions: &[SessionRef],
    query_terms: &[String],
    trimmed_query: &str,
    relevance_scores: &[f64],
    limit: usize,
    meaningful_threshold: f64,
) -> Vec<SessionRef> {
    let lower_query = trimmed_query.to_lowercase();
    // Title + opening of the conversation only -- not the full body. A
    // query phrase appearing once, deep in a long unrelated session, isn't
    // the same signal as it being what the chat opens with or is titled
    // around.
    let is_exact_match = |s: &SessionRef| -> bool {
        let body = s.body.clone().unwrap_or_else(|| s.snippet.clone());
        let head: String = body.chars().take(1000).collect();
        format!("{} {}", s.title, head).to_lowercase().contains(&lower_query)
    };

    let mut combined: Vec<(SessionRef, f64, bool)> = sessions
        .iter()
        .enumerate()
        .map(|(i, s)| {
            // Multiplicative, not additive -- recency should amplify an
            // already-relevant result, not stand in for relevance on its
            // own.
            let score = relevance_scores[i] * (1.0 + 0.5 * recency_score(s.updated_at));
            (s.clone(), score, is_exact_match(s))
        })
        .collect();

    combined.sort_by(|a, b| {
        if a.2 != b.2 {
            return if a.2 { std::cmp::Ordering::Less } else { std::cmp::Ordering::Greater };
        }
        if a.2 && b.2 {
            return b.0.updated_at.cmp(&a.0.updated_at);
        }
        b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal).then(b.0.updated_at.cmp(&a.0.updated_at))
    });

    // keep only results with a non-trivial combined score -- otherwise, on
    // a query that matches almost nothing, we'd still show `limit` results
    // that are really just "least irrelevant". Exact matches always count
    // as meaningful regardless of blended score. Deliberately no "show top
    // 3 anyway" fallback: let genuine emptiness propagate.
    combined
        .into_iter()
        .filter(|(_, score, exact)| *score > meaningful_threshold || *exact)
        .take(limit)
        .map(|(s, _, _)| {
            let body = s.body.clone().unwrap_or_else(|| s.snippet.clone());
            match build_snippet(&body, query_terms) {
                Some(snippet) => SessionRef { match_snippet: Some(snippet), ..s },
                None => s,
            }
        })
        .collect()
}

// Even split: verified empirically across several real queries -- raising
// semantic's share from 30% to 50% fixed real cases (a focused single
// mention in a long, on-topic session losing to incidental repetition in a
// long, unrelated one) with no regression on queries that were already
// ranking well. Both halves are min-max normalized to comparable [0,1]
// ranges before blending.
const BM25_WEIGHT: f64 = 0.5;
const SEMANTIC_WEIGHT: f64 = 0.5;

pub struct Ranker {
    index: LexicalIndex,
    sessions: Vec<SessionRef>,
}

impl Ranker {
    pub fn new(sessions: Vec<SessionRef>) -> Self {
        let index = build_lexical_index(&sessions);
        Self { index, sessions }
    }

    /// Synchronous re-rank for a new query -- cheap enough to call on every
    /// keystroke (BM25 with fuzzy typo tolerance + exact-match tier +
    /// recency, no semantic component). This is stage 1: instant feedback
    /// while typing.
    pub fn rank(&self, query: &str, limit: usize) -> Vec<SessionRef> {
        let trimmed = query.trim();
        if trimmed.is_empty() {
            return most_recent(&self.sessions, limit);
        }
        let (query_terms, match_terms, bm25_normalized) = lexical_scores(&self.index, &self.sessions, trimmed);
        let _ = query_terms;
        apply_ranking_layers(&self.sessions, &match_terms, trimmed, &bm25_normalized, limit, 0.1)
    }

    /// Async upgrade over the same query -- adds semantic similarity into
    /// the blend, reusing the same lexical index (no BM25 rebuild). This is
    /// stage 2: call it debounced after typing pauses.
    pub async fn refine_with_semantic(&self, query: &str, limit: usize) -> Vec<SessionRef> {
        let trimmed = query.trim();
        if trimmed.is_empty() {
            return most_recent(&self.sessions, limit);
        }
        let (query_terms, match_terms, bm25_normalized) = lexical_scores(&self.index, &self.sessions, trimmed);
        let _ = query_terms;

        let mut semantic_normalized = vec![0.0; self.sessions.len()];
        if let Ok(()) = crate::embed::ensure_model(|_| {}).await {
            if let Ok(query_vec) = crate::embed::embed_text(trimmed) {
                let score_map = get_cached_semantic_scores(&self.sessions, &query_vec);
                let semantic_raw: Vec<f64> = self
                    .sessions
                    .iter()
                    .map(|s| *score_map.get(&format!("{}:{}", s.tool.slug(), s.session_id)).unwrap_or(&0.0) as f64)
                    .collect();
                semantic_normalized = min_max_normalize(&semantic_raw);
            }
        }
        // offline on first run, disk issue, unsupported platform -- falls
        // back to lexical-only (semantic_normalized stays all-zero) rather
        // than failing the refine.

        let relevance: Vec<f64> = (0..self.sessions.len()).map(|i| BM25_WEIGHT * bm25_normalized[i] + SEMANTIC_WEIGHT * semantic_normalized[i]).collect();
        apply_ranking_layers(&self.sessions, &match_terms, trimmed, &relevance, limit, 0.15)
    }
}

fn most_recent(sessions: &[SessionRef], limit: usize) -> Vec<SessionRef> {
    let mut sorted: Vec<SessionRef> = sessions.to_vec();
    sorted.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    sorted.truncate(limit);
    sorted
}

/// Checks for unindexed sessions and kicks off the background embedder if
/// needed -- shared by both the full hybrid search and any future
/// live-typing ranker.
pub fn ensure_indexing_triggered(sessions: &[SessionRef]) -> bool {
    let pending = has_pending_work(sessions);
    if pending {
        trigger_background_indexing();
    }
    pending
}

/// Fire-and-forget: spawns the current binary's hidden background-index
/// subcommand, detached from this process, so it keeps running (and
/// writing progress to the persistent index) even after this CLI
/// invocation exits.
fn trigger_background_indexing() {
    let Ok(exe) = std::env::current_exe() else { return };
    let child = std::process::Command::new(exe)
        .arg("__background-index")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    // Detached: the child's own process group/session separation from
    // a spawned (non-shell) Command on Unix already avoids being killed
    // by this process's own exit -- there's no `.unref()` equivalent
    // needed since Rust's Child isn't awaited here at all.

    // The embedding model's inference (native ONNX Runtime here, same
    // underlying concern the original TS version hit with onnxruntime's
    // WASM backend) can use its own thread pool across every core during
    // a call -- doesn't make embedding faster, but it can transiently
    // saturate the machine and starve whatever foreground process you're
    // actually looking at (e.g. an agent's TUI you just resumed into) of
    // scheduling time, showing up as input lag with no obvious cause.
    // This is a true background task, so ask the OS to schedule it at the
    // lowest niceness -- best-effort, not fatal if the platform disallows
    // it (e.g. no permission to renice, which requires nothing special
    // for *lowering* your own child's priority on Unix, but the syscall
    // can still fail). `setpriority`/`PRIO_PROCESS` are POSIX-only --
    // `libc` doesn't define them on Windows at all, so this has to be
    // behind `cfg(unix)` or it won't compile there; a real Windows
    // equivalent (`SetPriorityClass`) could be added later, but this is
    // a nice-to-have, not worth blocking a Windows build over.
    #[cfg(unix)]
    if let Ok(child) = child {
        unsafe {
            let _ = libc::setpriority(libc::PRIO_PROCESS, child.id() as libc::id_t, 19);
        }
    }
    #[cfg(not(unix))]
    let _ = child;
}

/// One-shot hybrid search (BM25 + fuzzy + semantic, blended) for when a
/// query is already fully known. Never blocks on embedding the corpus:
/// semantic scoring only uses whatever is already in the persistent index.
pub async fn search_sessions(sessions: Vec<SessionRef>, query: &str, opts: SearchOptions) -> SearchResult {
    let trimmed = query.trim();
    let pending = ensure_indexing_triggered(&sessions);

    if trimmed.is_empty() {
        return SearchResult { results: most_recent(&sessions, opts.limit), indexing_in_background: pending };
    }
    if sessions.is_empty() {
        return SearchResult { results: Vec::new(), indexing_in_background: pending };
    }

    let ranker = Ranker::new(sessions);
    let results = ranker.refine_with_semantic(trimmed, opts.limit).await;
    SearchResult { results, indexing_in_background: pending }
}

fn parse_agent_arg(raw: &str) -> ToolName {
    match ToolName::from_slug(raw) {
        Some(t) => t,
        None => {
            eprintln!(
                "agent-hop: unknown agent \"{raw}\". Valid: {}",
                ToolName::ALL.iter().map(|t| t.slug()).collect::<Vec<_>>().join(", ")
            );
            std::process::exit(1);
        }
    }
}

/// Standalone `ah resume [query] [-a agent] [-r resume-in]` -- search
/// outside the TUI (crossterm owns stdin directly here, no relay thread
/// exists yet), then jump straight into the TUI already resumed at
/// whatever session was picked.
///
/// Ported from the original CLI's non-interactive/scriptable design (see
/// the v0.0.5 tag): a query can be given directly as an argument for a
/// one-shot search, `-a` restricts which agent(s) to search, and `-r`
/// resumes the picked session in a *different* agent than it was recorded
/// in without needing to enter the live TUI and hop -- e.g. for scripting
/// or another agent shelling out to this as a command. When stdin/stdout
/// aren't both real terminals, there's no one to answer an interactive
/// prompt (it would just hang), so a query is required up front and the
/// top match is auto-picked rather than blocking forever.
pub async fn run_standalone_resume(
    query_arg: Option<String>,
    agent_arg: Option<String>,
    resume_in_arg: Option<String>,
) -> anyhow::Result<()> {
    use std::io::IsTerminal;
    let non_interactive = !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal();

    if non_interactive && query_arg.is_none() {
        eprintln!("agent-hop: running non-interactively (no TTY) -- a search query is required, e.g. `ah resume \"oauth bug\"`.");
        std::process::exit(1);
    }

    let scope: Vec<ToolName> = match &agent_arg {
        Some(a) => vec![parse_agent_arg(a)],
        None => ToolName::ALL.to_vec(),
    };

    let sessions = collect_sessions(&scope);
    if sessions.is_empty() {
        println!("No sessions found yet.");
        return Ok(());
    }

    let session_ref = if let Some(query) = &query_arg {
        // A query was already supplied -- one-shot hybrid search (BM25 +
        // semantic), not the live-typing ranker, but still shown through
        // the same interactive picker (pre-filled) so the user can keep
        // refining or just confirm the top match. Non-interactive mode
        // (no TTY) skips the picker entirely and takes the top result.
        let result = search_sessions(sessions, query, SearchOptions::default()).await;
        if result.indexing_in_background {
            println!("Semantic search is still learning some newer sessions in the background — results will get sharper on your next search.");
        }
        if result.results.is_empty() {
            println!("No sessions found. Try a different query or agent scope.");
            return Ok(());
        }
        if non_interactive {
            let picked = result.results[0].clone();
            println!("Non-interactive: auto-picked top match -- [{}] {}", picked.tool.slug(), picked.title);
            picked
        } else {
            crossterm::terminal::enable_raw_mode()?;
            let mut keys = crate::resume::CrosstermKeys;
            let mut out = std::io::stdout();
            let outcome = crate::resume::run_resume_ui(result.results, query, &mut keys, &mut out);
            crossterm::terminal::disable_raw_mode()?;
            match outcome? {
                crate::resume::ResumeOutcome::Resume(r) => r,
                crate::resume::ResumeOutcome::Cancelled => {
                    crate::telemetry::capture("search_cancelled", serde_json::json!({ "via": "cli" }));
                    println!("Cancelled.");
                    return Ok(());
                }
                crate::resume::ResumeOutcome::Quit => return Ok(()),
            }
        }
    } else {
        // No query yet -- the familiar live-typing overlay, re-ranking as
        // you type.
        crossterm::terminal::enable_raw_mode()?;
        let mut keys = crate::resume::CrosstermKeys;
        let mut out = std::io::stdout();
        let outcome = crate::resume::run_resume_ui(sessions, "", &mut keys, &mut out);
        crossterm::terminal::disable_raw_mode()?;
        match outcome? {
            crate::resume::ResumeOutcome::Resume(r) => r,
            crate::resume::ResumeOutcome::Cancelled => {
                crate::telemetry::capture("search_cancelled", serde_json::json!({ "via": "cli" }));
                println!("Cancelled.");
                return Ok(());
            }
            crate::resume::ResumeOutcome::Quit => return Ok(()),
        }
    };

    let target_tool = match &resume_in_arg {
        Some(a) => parse_agent_arg(a),
        None => session_ref.tool,
    };
    if !target_tool.is_installed() {
        eprintln!("agent-hop: cannot resume in {}: \"{}\" is not installed or not on PATH.", target_tool.slug(), target_tool.binary());
        std::process::exit(1);
    }

    crate::telemetry::capture(
        "resume",
        serde_json::json!({
            "from": session_ref.tool.slug(),
            "to": target_tool.slug(),
            "same_agent": target_tool == session_ref.tool,
            "via": "cli",
            "interactive": !non_interactive,
            "had_query": query_arg.is_some(),
            "scoped": agent_arg.is_some(),
        }),
    );

    // tui::run's own initial-launch handling already falls back to the
    // home directory (with a visible warning) if this path no longer
    // exists, so there's nothing extra to check here (interactive path
    // only -- see below for why the non-interactive path checks it
    // directly instead).
    let project_path = session_ref.project_path.clone();

    let session_id = if target_tool != session_ref.tool {
        println!("Converting {} session for {}...", session_ref.tool.slug(), target_tool.slug());
        crate::adapters::convert_session(&session_ref, target_tool, &project_path)?
    } else {
        session_ref.session_id
    };

    if non_interactive {
        // The persistent switcher TUI (`tui::run`) unconditionally needs a
        // real controlling terminal (it puts the terminal in raw mode and
        // manages a pty directly) -- fundamentally incompatible with "no
        // TTY," not just an inconvenience to work around. The original
        // (pre-switcher) design didn't have this problem because it never
        // wrapped the target agent in anything: it directly spawned (or
        // execve'd) the target's own native resume command with inherited
        // stdio. Do the same here for the non-interactive path specifically
        // -- if the target agent *itself* also needs a real terminal for
        // its own interactive UI, that's now its own error to report, not
        // an artifact of our own code requiring one.
        let project_dir = if std::path::Path::new(&project_path).exists() {
            project_path
        } else {
            let home = dirs::home_dir().map(|p| p.to_string_lossy().to_string()).unwrap_or_else(|| ".".to_string());
            eprintln!("agent-hop: original project directory no longer exists: {project_path}\nResuming in {home} instead.");
            home
        };
        let cmd = crate::adapters::adapter_for(target_tool).resume_cmd(&session_id, &project_dir);
        println!("Launching: {}", cmd.join(" "));
        let status = std::process::Command::new(&cmd[0]).args(&cmd[1..]).current_dir(&project_dir).status();
        // Drain queued telemetry before the hard exit swallows it (process::exit
        // skips the flush the caller runs after this fn returns).
        crate::telemetry::flush().await;
        match status {
            Ok(s) => std::process::exit(s.code().unwrap_or(0)),
            Err(e) => {
                eprintln!("agent-hop: failed to launch {}: {e}", target_tool.slug());
                std::process::exit(1);
            }
        }
    }

    crate::tui::run(target_tool, Some((session_id, project_path))).await
}

#[cfg(test)]
mod ranker_tests {
    use super::*;

    #[test]
    fn real_session_corpus_ranks_sensibly() {
        let sessions = collect_sessions(&ToolName::ALL);
        if sessions.len() < 5 {
            eprintln!("skipping: fewer than 5 real sessions on this machine ({})", sessions.len());
            return;
        }
        let ranker = Ranker::new(sessions.clone());

        // Empty query -> most-recent ordering, not empty.
        let recent = ranker.rank("", 10);
        assert!(!recent.is_empty());
        for w in recent.windows(2) {
            assert!(w[0].updated_at >= w[1].updated_at);
        }

        // A query built from a real, real session's own title should surface
        // that session at or near the top -- this is what actually proves
        // BM25 + exact-match tiering works against real data, not synthetic
        // fixtures.
        let sample = sessions.iter().max_by_key(|s| s.title.split_whitespace().count()).unwrap();
        let query_words: Vec<&str> = sample.title.split_whitespace().take(4).collect();
        if query_words.len() >= 2 {
            let query = query_words.join(" ");
            let results = ranker.rank(&query, 10);
            assert!(!results.is_empty(), "expected at least one result for query derived from a real session's own title");
        }
    }
}
