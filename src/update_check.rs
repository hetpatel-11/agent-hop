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
/// `CHECK_INTERVAL_SECS`. The interactive "update now or later?" prompt
/// lives in `prompt_and_maybe_update`.
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

/// Interactive `Update now` / `Later` picker -- same two options the
/// original TS CLI showed via clack `p.select`. Returns `true` if the
/// user chose to update (caller should exit so they re-run the new
/// binary); `false` if they picked Later, cancelled, or the picker
/// itself failed (never block a launch on this).
pub fn prompt_and_maybe_update(info: &UpdateInfo) -> bool {
    match pick_update_now_or_later(info) {
        Ok(true) => {
            run_update();
            println!("Updated. Run `ah` again to use the new version.");
            true
        }
        Ok(false) | Err(_) => false,
    }
}

fn pick_update_now_or_later(info: &UpdateInfo) -> anyhow::Result<bool> {
    use crate::theme;
    use crossterm::{
        cursor,
        event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
        execute, queue,
        style::{Color, Print, ResetColor, SetForegroundColor},
        terminal::{self, ClearType},
    };
    use std::io::{stdout, Write};

    let options: [&str; 2] = ["Update now (recommended)", "Later"];
    let hints: [&str; 2] = ["npm/bun install -g, then re-run ah", ""];
    let mut selected: usize = 0;

    let mut out = stdout();
    terminal::enable_raw_mode()?;
    execute!(out, cursor::Hide)?;

    let result = (|| -> anyhow::Result<bool> {
        let render = |out: &mut std::io::Stdout, selected: usize| -> anyhow::Result<()> {
            queue!(out, terminal::Clear(ClearType::FromCursorDown))?;
            queue!(
                out,
                Print(format!(
                    "{} {}\r\n",
                    theme::bold(&theme::magenta("◆")),
                    theme::bold(&format!(
                        "A new version of agent-hop is available ({} → {}).",
                        info.current, info.latest
                    ))
                ))
            )?;
            for (i, label) in options.iter().enumerate() {
                let marker = if i == selected { "❯" } else { " " };
                let hint = if hints[i].is_empty() {
                    String::new()
                } else {
                    format!("  {}", theme::grey(hints[i]))
                };
                if i == selected {
                    queue!(out, SetForegroundColor(Color::Cyan))?;
                    queue!(out, Print(format!("{marker} {label}{hint}\r\n")))?;
                    queue!(out, ResetColor)?;
                } else {
                    queue!(out, Print(format!("{} {}\r\n", marker, theme::grey(label))))?;
                }
            }
            queue!(
                out,
                Print(format!("{}\r\n", theme::grey("  ↑/↓ move · enter select · esc later")))
            )?;
            queue!(out, cursor::MoveUp((options.len() + 2) as u16))?;
            out.flush()?;
            Ok(())
        };

        render(&mut out, selected)?;

        loop {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                match key.code {
                    KeyCode::Up | KeyCode::Down => {
                        selected = 1 - selected;
                        render(&mut out, selected)?;
                    }
                    KeyCode::Enter => return Ok(selected == 0),
                    KeyCode::Esc => return Ok(false),
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        return Ok(false);
                    }
                    _ => {}
                }
            }
        }
    })();

    let _ = queue!(out, terminal::Clear(ClearType::FromCursorDown), cursor::Show);
    let _ = out.flush();
    let _ = terminal::disable_raw_mode();
    result
}

/// Runs the global reinstall with visible output. Tries npm first (the
/// documented default install path); if that fails outright (e.g. this was
/// actually installed via bun and npm's global root doesn't have it), falls
/// back to bun rather than leaving the user stuck. Never panics -- worst
/// case is "update now" silently does nothing and they're no worse off
/// than before. Ported from the TS `runUpdate`.
fn run_update() {
    let npm_ok = std::process::Command::new("npm")
        .args(["install", "-g", "agent-hop@latest"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !npm_ok {
        let _ = std::process::Command::new("bun")
            .args(["install", "-g", "agent-hop@latest"])
            .status();
    }
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
