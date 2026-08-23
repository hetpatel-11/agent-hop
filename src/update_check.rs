//! Port of src/update-check.ts -- cached, network-optional version check
//! against the npm registry. Never blocks a launch meaningfully: bounded by
//! a short timeout, and skipped entirely when the caller isn't interactive.

use std::path::PathBuf;
use std::time::Duration;

const CACHE_FILE: &str = "update-check.json";
const CHECK_INTERVAL_SECS: u64 = 24 * 60 * 60; // once a day
const FETCH_TIMEOUT: Duration = Duration::from_millis(1500);

#[derive(serde::Serialize, serde::Deserialize)]
struct Cache {
    latest: String,
    checked_at_secs: u64,
}

fn cache_path() -> PathBuf {
    dirs::home_dir().unwrap_or_default().join(".agent-hop").join(CACHE_FILE)
}

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn read_cache() -> Option<Cache> {
    let text = std::fs::read_to_string(cache_path()).ok()?;
    serde_json::from_str(&text).ok()
}

fn write_cache(cache: &Cache) {
    let path = cache_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(text) = serde_json::to_string(cache) {
        let _ = std::fs::write(path, text);
    }
}

async fn fetch_latest_version() -> Option<String> {
    let client = reqwest::Client::builder().timeout(FETCH_TIMEOUT).build().ok()?;
    let res = client.get("https://registry.npmjs.org/agent-hop/latest").send().await.ok()?;
    if !res.status().is_success() {
        return None;
    }
    let data: serde_json::Value = res.json().await.ok()?;
    data.get("version").and_then(|v| v.as_str()).map(|s| s.to_string())
}

/// Simple semver-ish compare, good enough for "is latest newer than current".
fn is_newer(latest: &str, current: &str) -> bool {
    let parse = |s: &str| -> Vec<u64> { s.split('.').map(|p| p.parse().unwrap_or(0)).collect() };
    let a = parse(latest);
    let b = parse(current);
    for i in 0..a.len().max(b.len()) {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        if x != y {
            return x > y;
        }
    }
    false
}

pub struct UpdateInfo {
    pub current: String,
    pub latest: String,
    pub update_available: bool,
}

/// Cached, network-optional version check. Never panics, never blocks
/// longer than `FETCH_TIMEOUT`, and only hits the registry once per
/// `CHECK_INTERVAL_SECS`. Simplified from the original TS version's
/// interactive "update now or later?" prompt to a plain informational
/// message -- printing one doesn't need a whole new interactive UI
/// component the way an inline select would.
pub async fn check_for_update() -> UpdateInfo {
    let current = env!("CARGO_PKG_VERSION").to_string();
    let cache = read_cache();
    let cache_fresh = cache.as_ref().is_some_and(|c| now_secs().saturating_sub(c.checked_at_secs) < CHECK_INTERVAL_SECS);

    let mut latest = cache.as_ref().map(|c| c.latest.clone()).unwrap_or_else(|| current.clone());
    if !cache_fresh {
        if let Some(fetched) = fetch_latest_version().await {
            write_cache(&Cache { latest: fetched.clone(), checked_at_secs: now_secs() });
            latest = fetched;
        }
        // else: offline/timeout/registry hiccup -- keep whatever we had
        // (stale cache, or just current if there was never a cache at all).
    }

    let update_available = is_newer(&latest, &current);
    UpdateInfo { current, latest, update_available }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_newer_detects_patch_and_minor_bumps() {
        assert!(is_newer("0.1.1", "0.1.0"));
        assert!(is_newer("0.2.0", "0.1.9"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.2.0"));
    }

    #[test]
    fn is_newer_handles_uneven_segment_counts() {
        assert!(is_newer("1.0", "0.9.9"));
        assert!(!is_newer("0.9", "0.9.1"));
    }
}
