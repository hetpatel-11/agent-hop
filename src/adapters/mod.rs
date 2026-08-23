use crate::agents::ToolName;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Universal intermediate shape every adapter reads a source agent's native
/// session format into, and writes a target agent's native format back out
/// of. Ported 1:1 from the TypeScript `Turn`/`ToolCallRecord`/`Attachment`
/// types -- this is the actual cross-agent translation IP, not the pty/TUI
/// plumbing around it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Turn {
    pub role: Role,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCallRecord>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attachments: Option<Vec<Attachment>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallRecord {
    pub name: String,
    pub input: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attachment {
    pub mime_type: String,
    pub base64: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
}

pub mod claude;
pub mod codex;
pub mod opencode;
pub mod pi;
pub mod grok;

/// Port of the TS `SessionRef` -- what `listSessions()` returns, and what
/// `read()` is given back so it can get at adapter-specific extra data
/// (e.g. the source file path) without every adapter re-deriving it from a
/// bare id.
#[derive(Debug, Clone)]
pub struct SessionRef {
    pub tool: ToolName,
    pub session_id: String,
    pub project_path: String,
    pub title: String,
    pub snippet: String,
    /// Full (length-capped) conversation text for search -- not just the
    /// opening message. Falls back to `snippet` when an adapter can't
    /// cheaply capture more (e.g. OpenCode, which would need a subprocess
    /// export call per session just to list).
    pub body: Option<String>,
    /// unix ms, for recency sorting
    pub updated_at: i64,
    /// adapter-specific extra data (e.g. file path)
    pub raw: Option<Value>,
    /// Excerpt around the matched query terms (ANSI-highlighted), set by
    /// searchSessions() so the picker can show *why* a result matched
    /// instead of just its opening line. Absent for a no-query (recency)
    /// listing.
    pub match_snippet: Option<String>,
}

pub trait Adapter {
    fn list_sessions(&self) -> anyhow::Result<Vec<SessionRef>>;
    fn read(&self, session_ref: &SessionRef) -> anyhow::Result<Vec<Turn>>;
    fn write(&self, turns: &[Turn], project_path: &str) -> anyhow::Result<String>;
    fn resume_cmd(&self, session_id: &str, project_path: &str) -> Vec<String>;

    /// Fast path for "the single most recent session for this exact
    /// project path" -- what a hop actually needs. The default falls back
    /// to the full `list_sessions()` scan (title/snippet/body extraction
    /// for *every* session on disk, needed for the interactive search UI
    /// but wasted work here), which measured at 650ms-1.2s+ against a
    /// real, months-old session history, entirely on the critical path of
    /// every single hop. Adapters override this with something that reads
    /// only the small amount of metadata actually needed to identify a
    /// match, skipping sessions that don't match well before paying for
    /// full extraction (or, for Claude, skipping straight to the one
    /// directory that can possibly contain a match at all, since project
    /// path is already encoded into the directory name).
    fn find_latest_for_path(&self, project_path: &str) -> Option<SessionRef> {
        let target = canonical_or_self(project_path);
        self.list_sessions().ok()?.into_iter().filter(|s| canonical_or_self(&s.project_path) == target).max_by_key(|s| s.updated_at)
    }
}

pub fn adapter_for(tool: ToolName) -> Box<dyn Adapter> {
    match tool {
        ToolName::Claude => Box::new(claude::ClaudeAdapter),
        ToolName::Codex => Box::new(codex::CodexAdapter),
        ToolName::OpenCode => Box::new(opencode::OpenCodeAdapter),
        ToolName::Pi => Box::new(pi::PiAdapter),
        ToolName::Grok => Box::new(grok::GrokAdapter),
    }
}

fn canonical_or_self(path: &str) -> String {
    std::fs::canonicalize(path).map(|p| p.to_string_lossy().to_string()).unwrap_or_else(|_| path.to_string())
}

/// Finds the most-recently-updated session a given tool has for a given
/// project path -- this is what makes hopping mid-conversation possible:
/// after killing the just-active agent, this is how we find *which*
/// session it was just writing to, so it can be read and translated into
/// the next tool's format. Paths are canonicalized on both sides before
/// comparing since adapters store cwd in whatever form they wrote it
/// (usually already canonical, but not guaranteed identical string
/// representations across platforms).
pub fn find_latest_session_for_path(tool: ToolName, project_path: &str) -> Option<SessionRef> {
    adapter_for(tool).find_latest_for_path(project_path)
}

/// Converts one already-known session into a target tool's native format --
/// shared by the live in-TUI hop path (which first has to *find* the
/// relevant session for a project path -- see `find_latest_session_for_path`
/// and its caller) and the standalone `ah resume -r <tool>` path (which
/// already has an exact `SessionRef` in hand from search, nothing to find).
/// Trims to the target's context budget and folds anything cut into a short
/// synthetic summary turn (see `trim_turns_with_summary`) rather than
/// silently dropping it, the same shape several of these agents already use
/// themselves when their own context fills up.
pub fn convert_session(session_ref: &SessionRef, to: ToolName, project_path: &str) -> anyhow::Result<String> {
    let turns = adapter_for(session_ref.tool).read(session_ref)?;
    let turns = crate::util::trim_turns_with_summary(turns, crate::util::CONVERSION_CHAR_BUDGET);
    adapter_for(to).write(&turns, project_path)
}

#[cfg(test)]
mod roundtrip_tests {
    use super::*;
    use crate::adapters::claude::ClaudeAdapter;
    use crate::adapters::codex::CodexAdapter;
    use crate::adapters::pi::PiAdapter;

    fn sample_turns() -> Vec<Turn> {
        vec![
            Turn { role: Role::User, text: "can you list the files in this repo".to_string(), tool_calls: None, attachments: None },
            Turn {
                role: Role::Assistant,
                text: "Sure, here's what's in the repo.".to_string(),
                tool_calls: Some(vec![ToolCallRecord {
                    name: "Bash".to_string(),
                    input: r#"{"command":"ls"}"#.to_string(),
                    output: Some("Cargo.toml\nsrc/".to_string()),
                }]),
                attachments: None,
            },
        ]
    }

    /// A write() that doesn't produce something read() can parse back
    /// isn't a real round-trip -- this is what actually proves the adapter
    /// works, not just that it compiles. Writes land in the real
    /// ~/.claude/~/.codex/~/.pi session storage (there's no sandboxed
    /// alternative -- that's genuinely where each agent looks), so the
    /// written file is deleted afterward rather than left behind as test
    /// litter in the user's real session history.
    fn assert_roundtrips(adapter: &dyn Adapter, project_dir: &std::path::Path) {
        let turns = sample_turns();
        let new_id = adapter.write(&turns, &project_dir.to_string_lossy()).expect("write should succeed");
        assert!(!new_id.is_empty());

        let refs = adapter.list_sessions().expect("list_sessions should succeed");
        let session_ref = refs
            .iter()
            .find(|r| r.session_id == new_id)
            .unwrap_or_else(|| panic!("written session {new_id} not found by list_sessions"));

        let read_back = adapter.read(session_ref).expect("read should succeed");

        if let Some(file) = session_ref.raw.as_ref().and_then(|r| r.get("file")).and_then(|v| v.as_str()) {
            let path = std::path::Path::new(file);
            let parent = path.parent().map(|p| p.to_path_buf());
            let _ = std::fs::remove_file(path);
            // Only removes if empty -- codex's date directories are shared
            // with real sessions, so this is a safe no-op there and a real
            // cleanup for claude/pi's per-project directories, which are
            // unique to this test's random tmp cwd.
            if let Some(parent) = parent {
                let _ = std::fs::remove_dir(parent);
            }
        }

        assert_eq!(read_back.len(), 2, "expected both turns to survive the round-trip");
        assert_eq!(read_back[0].role, Role::User);
        assert_eq!(read_back[0].text, turns[0].text);
        assert_eq!(read_back[1].role, Role::Assistant);
        assert_eq!(read_back[1].text, turns[1].text);
        let tool_calls = read_back[1].tool_calls.as_ref().expect("tool call should survive round-trip");
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].name, "Bash");
        assert_eq!(tool_calls[0].output.as_deref(), Some("Cargo.toml\nsrc/"));
    }

    #[test]
    fn claude_write_then_read_roundtrips() {
        let dir = std::env::temp_dir().join(format!("agent-hop-test-claude-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_roundtrips(&ClaudeAdapter, &dir);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn codex_write_then_read_roundtrips() {
        let dir = std::env::temp_dir().join(format!("agent-hop-test-codex-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_roundtrips(&CodexAdapter, &dir);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pi_write_then_read_roundtrips() {
        let dir = std::env::temp_dir().join(format!("agent-hop-test-pi-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_roundtrips(&PiAdapter, &dir);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// This is the exact sequence `tui.rs`'s `translate_session` runs on
    /// every hop: write a session as one tool, find it by project path,
    /// read it back, write it into a *different* tool's format, then read
    /// that back too -- proving a live conversation genuinely survives a
    /// cross-agent hop, not just a same-tool round-trip.
    #[test]
    fn cross_agent_hop_translation_preserves_content() {
        let dir = std::env::temp_dir().join(format!("agent-hop-test-hop-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let project_path = dir.to_string_lossy().to_string();
        let marker = "xyzzy-hop-translation-marker";

        let turns = vec![
            Turn { role: Role::User, text: format!("say the phrase '{marker}' and nothing else"), tool_calls: None, attachments: None },
            Turn { role: Role::Assistant, text: marker.to_string(), tool_calls: None, attachments: None },
        ];

        // Simulates "claude was just live and wrote this" without actually
        // spawning a real claude process (recursion inside an already-live
        // Claude Code session interferes with a real spawned instance's
        // own session bookkeeping -- this isolates the translation logic
        // itself from that unrelated testing artifact).
        let claude_id = ClaudeAdapter.write(&turns, &project_path).expect("claude write should succeed");

        let found = find_latest_session_for_path(ToolName::Claude, &project_path).expect("should find the just-written claude session by project path");
        assert_eq!(found.session_id, claude_id);

        let read_back = ClaudeAdapter.read(&found).expect("claude read should succeed");
        assert_eq!(read_back.len(), 2);
        assert!(read_back[1].text.contains(marker));

        let codex_id = CodexAdapter.write(&read_back, &project_path).expect("codex write should succeed");
        let codex_ref = find_latest_session_for_path(ToolName::Codex, &project_path).expect("should find the newly-written codex session");
        assert_eq!(codex_ref.session_id, codex_id);

        let codex_turns = CodexAdapter.read(&codex_ref).expect("codex read should succeed");
        assert_eq!(codex_turns.len(), 2);
        assert!(codex_turns[1].text.contains(marker), "marker phrase should survive claude -> codex translation, got: {:?}", codex_turns[1].text);

        // Cleanup: remove both written sessions so this test leaves no
        // litter in real ~/.claude or ~/.codex session storage.
        if let Some(file) = found.raw.as_ref().and_then(|r| r.get("file")).and_then(|v| v.as_str()) {
            let _ = std::fs::remove_file(file);
            if let Some(parent) = std::path::Path::new(file).parent() {
                let _ = std::fs::remove_dir(parent);
            }
        }
        if let Some(file) = codex_ref.raw.as_ref().and_then(|r| r.get("file")).and_then(|v| v.as_str()) {
            let _ = std::fs::remove_file(file);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    use uuid::Uuid;
}
