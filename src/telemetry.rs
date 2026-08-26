//! Anonymous, opt-out usage telemetry.
//!
//! Modeled on browser-use's approach (github.com/browser-use/browser-use):
//! on by default, disabled via an env var, keyed by an anonymous persistent
//! device id, with a one-time first-run disclosure. It is adapted to
//! agent-hop's core promise ("100% local -- your sessions never leave your
//! disk"): we send only *aggregate usage* events -- which command ran, which
//! agents were detected, counts, version, OS. We deliberately never send
//! search queries, file paths, project names, session ids, or anything
//! derived from the content of a user's chats.
//!
//! Design constraints, in priority order:
//! Events (all aggregate; never queries, paths, chat, or agent session ids):
//!   - `app_launched` — `entry` (picker/restore/resume/claude/…) and
//!     `installed` (harness slugs on PATH). Restore also sends
//!     `workspaces` / `tabs` counts, never paths or session ids.
//!   - `leave` — `via` (prefix / search / close_tab / agent_exit) and
//!     workspace/tab counts. Fired when the user leaves the mux.
//!   - `hop` — `from`/`to` slugs, `via` (next/prev/picker), `converted`.
//!   - `resume` — `from`/`to`, `same_agent`, `via` (cli/overlay), whether
//!     they had a query / restricted `-a` / were interactive.
//!   - `search_cancelled` — `via` (cli/overlay).
//!   - `agent_selected` — startup picker: which slug, and whether it was
//!     already installed.
//!
//! Control surface (any one disables it):
//!   - `AH_TELEMETRY=0` (also accepts `false`/`off`/`no`)
//!   - `DO_NOT_TRACK=1`
//!   - `ah telemetry off`  (persists an opt-out marker file)

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use serde_json::{json, Value};

/// Primary opt-out env var. `AH_TELEMETRY=0|false|off|no` disables.
const ENABLE_ENV: &str = "AH_TELEMETRY";
/// Lets a user (or CI) pin a stable anonymous id, matching browser-use's
/// `BROWSER_USE_DEVICE_ID`. Mostly useful for testing and for machines that
/// can't persist a file.
const DEVICE_ID_ENV: &str = "AH_DEVICE_ID";
/// Override the ingest endpoint (handy for local testing / self-hosting).
const ENDPOINT_ENV: &str = "AH_TELEMETRY_ENDPOINT";

/// Self-hosted ingest. Replace with your deployed endpoint (see
/// `telemetry/worker.js` for a minimal Cloudflare Worker that accepts this
/// payload). Kept as a const so there's a single place to point it.
const DEFAULT_ENDPOINT: &str = "https://telemetry.agent-hop.com/e";

const DEVICE_ID_FILE: &str = "device_id";
const OPTOUT_FILE: &str = "telemetry-optout";
const NOTICE_FILE: &str = "telemetry-notice-shown";

/// Total budget for draining the queue at exit. Telemetry must never make a
/// user wait on shutdown longer than this.
const FLUSH_TIMEOUT: Duration = Duration::from_millis(1500);

const DOCS_URL: &str = "https://agent-hop.com/telemetry";

/// The directory agent-hop already uses for cached state (see update_check).
fn dir() -> PathBuf {
    dirs::home_dir().unwrap_or_default().join(".agent-hop")
}

fn is_falsey(v: &str) -> bool {
    matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "false" | "off" | "no")
}

fn is_truthy(v: &str) -> bool {
    matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "on" | "yes")
}

fn env_says_off(name: &str) -> bool {
    std::env::var(name).ok().is_some_and(|v| is_falsey(&v))
}

fn env_says_on(name: &str) -> bool {
    std::env::var(name).ok().is_some_and(|v| is_truthy(&v))
}

fn optout_marker() -> PathBuf {
    dir().join(OPTOUT_FILE)
}

/// Whether telemetry is currently enabled, consulting (in order): the
/// `DO_NOT_TRACK` standard, our own env var, and the persisted opt-out
/// marker. On by default.
pub fn is_enabled() -> bool {
    if env_says_on("DO_NOT_TRACK") {
        return false;
    }
    if env_says_off(ENABLE_ENV) {
        return false;
    }
    if optout_marker().exists() {
        return false;
    }
    true
}

/// Persist the user's choice so it survives across runs. Used by the
/// `ah telemetry on|off` command. Returns Ok even if nothing changed.
pub fn set_enabled(enabled: bool) -> std::io::Result<()> {
    let marker = optout_marker();
    if enabled {
        match std::fs::remove_file(&marker) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    } else {
        if let Some(parent) = marker.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&marker, b"")
    }
}

/// Stable anonymous id for counting distinct installs. Fallback chain:
/// env override -> persisted random UUID -> in-memory random UUID (if the
/// filesystem is unwritable). We intentionally do NOT hash MAC/hostname the
/// way browser-use does -- a random UUID counts uniques just as well without
/// fingerprinting the machine.
fn device_id() -> String {
    if let Ok(id) = std::env::var(DEVICE_ID_ENV) {
        let id = id.trim();
        if !id.is_empty() {
            return id.to_string();
        }
    }

    let path = dir().join(DEVICE_ID_FILE);
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let existing = existing.trim();
        if !existing.is_empty() {
            return existing.to_string();
        }
    }

    let fresh = uuid::Uuid::new_v4().to_string();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, &fresh);
    fresh
}

/// Anonymous per-install id. Used by `ah feedback` so a note can be tied
/// to the same install as telemetry without requiring telemetry to be on.
pub fn install_id() -> String {
    device_id()
}

/// Print the one-time disclosure the first time telemetry runs for a user,
/// then record that we've shown it so it never repeats. Only prints when
/// stderr is a real terminal -- scripts and other agents shouldn't get a
/// surprise line on their stream. This is the piece that keeps an opt-out
/// default honest.
fn maybe_show_notice() {
    use std::io::IsTerminal;
    if !std::io::stderr().is_terminal() {
        return;
    }
    let marker = dir().join(NOTICE_FILE);
    if marker.exists() {
        return;
    }
    eprintln!(
        "agent-hop collects anonymous, aggregate usage stats (no queries, paths, or chat content).\n\
         It helps us improve the tool. Turn it off any time with `ah telemetry off` or AH_TELEMETRY=0.\n\
         Details: {DOCS_URL}"
    );
    if let Some(parent) = marker.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&marker, b"");
}

struct Telemetry {
    enabled: bool,
    device_id: String,
    /// Random per-process id, so we can group events from one invocation
    /// without any cross-run linkage.
    session_id: String,
    endpoint: String,
    queue: Mutex<Vec<Value>>,
}

static INSTANCE: OnceLock<Telemetry> = OnceLock::new();

/// Initialize the global telemetry client. Safe to call once near startup.
/// When disabled, this is nearly free and every later `capture`/`flush`
/// becomes a no-op. Shows the first-run notice when enabled.
pub fn init() {
    let enabled = is_enabled();
    if enabled {
        maybe_show_notice();
    }
    let endpoint = std::env::var(ENDPOINT_ENV)
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_ENDPOINT.to_string());

    let _ = INSTANCE.set(Telemetry {
        enabled,
        device_id: if enabled { device_id() } else { String::new() },
        session_id: uuid::Uuid::new_v4().to_string(),
        endpoint,
        queue: Mutex::new(Vec::new()),
    });
}

/// Record an event. Cheap and non-blocking -- it only appends to an
/// in-memory queue that is sent once at `flush()`. `props` must contain only
/// aggregate, non-identifying values (no queries, paths, or chat content).
/// A no-op if telemetry is disabled or uninitialized.
pub fn capture(event: &str, props: Value) {
    let Some(t) = INSTANCE.get() else { return };
    if !t.enabled {
        return;
    }
    let mut obj = match props {
        Value::Object(m) => m,
        _ => serde_json::Map::new(),
    };
    obj.insert("event".into(), json!(event));
    obj.insert("time".into(), json!(chrono::Utc::now().to_rfc3339()));
    if let Ok(mut q) = t.queue.lock() {
        q.push(Value::Object(obj));
    }
}

/// Send everything captured this run in a single request, bounded by
/// `FLUSH_TIMEOUT`. Call once just before the process exits. Silent on every
/// failure -- offline, timeout, or a 500 all look the same to the user
/// (nothing). A no-op if telemetry is disabled or there's nothing queued.
pub async fn flush() {
    let Some(t) = INSTANCE.get() else { return };
    if !t.enabled {
        return;
    }
    let events = match t.queue.lock() {
        Ok(mut q) => std::mem::take(&mut *q),
        Err(_) => return,
    };
    if events.is_empty() {
        return;
    }

    let payload = json!({
        "device_id": t.device_id,
        "session_id": t.session_id,
        "app_version": env!("CARGO_PKG_VERSION"),
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "events": events,
    });

    let send = async {
        let client = match reqwest::Client::builder().timeout(FLUSH_TIMEOUT).build() {
            Ok(c) => c,
            Err(_) => return,
        };
        let _ = client.post(&t.endpoint).json(&payload).send().await;
    };

    // Double-bound: the client has its own timeout, but we also cap the whole
    // flush so a hung TLS handshake can't outlive the budget on shutdown.
    let _ = tokio::time::timeout(FLUSH_TIMEOUT, send).await;
}

/// One-line human-readable status for `ah telemetry status`.
pub fn status_line() -> String {
    if is_enabled() {
        format!(
            "Telemetry: ON (anonymous, aggregate usage only). Disable with `ah telemetry off`.\nDevice id: {}\nDetails: {DOCS_URL}",
            device_id()
        )
    } else {
        "Telemetry: OFF. Enable with `ah telemetry on`.".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_parsing_accepts_common_spellings() {
        for v in ["0", "false", "OFF", " no ", "No"] {
            assert!(is_falsey(v), "{v:?} should be falsey");
        }
        for v in ["1", "true", "ON", " yes ", "Yes"] {
            assert!(is_truthy(v), "{v:?} should be truthy");
        }
        // Cross terms don't leak between the two.
        assert!(!is_falsey("true"));
        assert!(!is_truthy("off"));
        assert!(!is_falsey("maybe"));
        assert!(!is_truthy("maybe"));
    }

    #[test]
    fn capture_before_init_is_a_noop() {
        // No INSTANCE set in a fresh test binary path -> must not panic.
        capture("noop_event", json!({ "k": "v" }));
    }
}
