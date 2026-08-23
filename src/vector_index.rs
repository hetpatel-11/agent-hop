//! Port of src/vector-index.ts -- the persistent semantic-search index:
//! chunking, cached-score lookup (never blocks search), and the actual
//! (re-)embedding work meant to run in a detached background process.

use crate::adapters::SessionRef;
use crate::embed;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

fn index_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_default().join(".agent-hop")
}
fn index_path() -> PathBuf {
    index_dir().join("index.json")
}
fn lock_path() -> PathBuf {
    index_dir().join("indexing.lock")
}

const CHUNK_CHARS: usize = 2000; // MiniLM works best on focused text, not huge blobs
const CHUNK_OVERLAP: usize = 200; // avoid splitting a relevant sentence exactly at a chunk boundary

#[derive(Serialize, Deserialize, Clone)]
struct ChunkEntry {
    vector: Vec<f32>,
}

#[derive(Serialize, Deserialize, Clone)]
struct SessionEntry {
    key: String, // "{tool}:{sessionId}"
    #[serde(rename = "sourceMtime")]
    source_mtime: i64,
    chunks: Vec<ChunkEntry>,
}

fn session_key(s: &SessionRef) -> String {
    format!("{}:{}", s.tool.slug(), s.session_id)
}

fn load_index() -> HashMap<String, SessionEntry> {
    let Ok(text) = std::fs::read_to_string(index_path()) else { return HashMap::new() };
    let Ok(entries) = serde_json::from_str::<Vec<SessionEntry>>(&text) else { return HashMap::new() };
    entries.into_iter().map(|e| (e.key.clone(), e)).collect()
}

fn save_index(index: &HashMap<String, SessionEntry>) -> anyhow::Result<()> {
    std::fs::create_dir_all(index_dir())?;
    let entries: Vec<&SessionEntry> = index.values().collect();
    std::fs::write(index_path(), serde_json::to_string(&entries)?)?;
    Ok(())
}

fn char_len(s: &str) -> usize {
    s.chars().count()
}

pub fn chunk_session(session: &SessionRef) -> Vec<String> {
    let body = session.body.clone().unwrap_or_else(|| session.snippet.clone());
    let full = format!("{}\n{}", session.title, body);
    if char_len(&full) <= CHUNK_CHARS {
        return vec![full];
    }
    let chars: Vec<char> = full.chars().collect();
    let mut chunks = Vec::new();
    let mut start = 0usize;
    while start < chars.len() {
        let end = (start + CHUNK_CHARS).min(chars.len());
        chunks.push(chars[start..end].iter().collect());
        start += CHUNK_CHARS - CHUNK_OVERLAP;
    }
    chunks
}

/// Reads whatever's already been embedded from disk -- no new embedding
/// work, no blocking. Sessions that haven't been indexed yet simply don't
/// get a score (caller treats that as "no semantic signal yet, BM25 still
/// applies"). This is what live search calls, so a search is never gated
/// on indexing.
pub fn get_cached_semantic_scores(sessions: &[SessionRef], query_vec: &[f32]) -> HashMap<String, f32> {
    let index = load_index();
    let mut scores = HashMap::new();
    for s in sessions {
        let key = session_key(s);
        let Some(entry) = index.get(&key) else { continue };
        let mut best = -1.0f32;
        for chunk in &entry.chunks {
            let sim = embed::cosine_similarity(query_vec, &chunk.vector);
            if sim > best {
                best = sim;
            }
        }
        scores.insert(key, best);
    }
    scores
}

// A session still being actively written to has its mtime change on
// nearly every turn. Without this, every search would see it as "changed
// since last index" and kick off a fresh background embedding run that
// goes stale again within seconds. Skipping anything modified in the last
// few minutes means we wait for it to settle instead of chasing a moving
// target.
const SETTLE_WINDOW_MS: i64 = 3 * 60 * 1000;

fn needs_embedding(s: &SessionRef, index: &HashMap<String, SessionEntry>) -> bool {
    let now_ms = chrono::Utc::now().timestamp_millis();
    if now_ms - s.updated_at < SETTLE_WINDOW_MS {
        return false;
    }
    match index.get(&session_key(s)) {
        Some(existing) => existing.source_mtime != s.updated_at,
        None => true,
    }
}

/// True if any session needs (re-)embedding since the last index build.
pub fn has_pending_work(sessions: &[SessionRef]) -> bool {
    let index = load_index();
    sessions.iter().any(|s| needs_embedding(s, &index))
}

fn is_lock_stale() -> bool {
    let Ok(text) = std::fs::read_to_string(lock_path()) else { return true };
    let Ok(pid) = text.trim().parse::<u32>() else { return true };
    process_exists(pid)
        .map(|exists| !exists)
        .unwrap_or(true)
}

#[cfg(unix)]
fn process_exists(pid: u32) -> anyhow::Result<bool> {
    // kill(pid, 0) -- signal 0 checks existence without actually signaling.
    let result = unsafe { libc::kill(pid as i32, 0) };
    Ok(result == 0)
}

#[cfg(not(unix))]
fn process_exists(_pid: u32) -> anyhow::Result<bool> {
    Ok(false)
}

/// Actually does the embedding work -- sequential, in-process. Meant to run
/// inside a detached background process, never inline in an interactive
/// search. Writes progressively so a killed/crashed run doesn't lose
/// everything already completed.
pub async fn build_index(sessions: &[SessionRef]) -> anyhow::Result<()> {
    std::fs::create_dir_all(index_dir())?;
    if lock_path().exists() && !is_lock_stale() {
        return Ok(()); // another run already in progress
    }
    std::fs::write(lock_path(), std::process::id().to_string())?;

    let result = (|| async {
        let mut index = load_index();
        let to_embed: Vec<&SessionRef> = sessions.iter().filter(|s| needs_embedding(s, &index)).collect();
        if to_embed.is_empty() {
            return Ok(());
        }

        embed::ensure_model(|_msg| {}).await?;
        for (i, s) in to_embed.iter().enumerate() {
            let key = session_key(s);
            let texts = chunk_session(s);
            let mut chunks = Vec::new();
            for text in &texts {
                let vec = embed::embed_text(text)?;
                chunks.push(ChunkEntry { vector: vec });
            }
            index.insert(key.clone(), SessionEntry { key, source_mtime: s.updated_at, chunks });
            // save incrementally every few sessions so progress isn't lost
            // if this background process gets killed partway through
            if i % 10 == 0 {
                save_index(&index)?;
            }
        }

        let live_keys: std::collections::HashSet<String> = sessions.iter().map(session_key).collect();
        index.retain(|k, _| live_keys.contains(k));
        save_index(&index)?;
        Ok::<(), anyhow::Error>(())
    })()
    .await;

    let _ = std::fs::remove_file(lock_path());
    result
}
