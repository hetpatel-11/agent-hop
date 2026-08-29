//! Persist mux workspaces/tabs across `ah` process exits.
//!
//! Agents are not kept running in the background. The next `ah` reopens
//! the same folders and resumes each tab's native session from disk.

use crate::adapters;
use crate::agents::ToolName;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

const FILE: &str = "layout.json";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SavedTab {
    pub tool: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SavedWorkspace {
    pub path: String,
    #[serde(default)]
    pub focus: usize,
    pub tabs: Vec<SavedTab>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct SavedMux {
    #[serde(default)]
    pub ws_focus: usize,
    pub workspaces: Vec<SavedWorkspace>,
}

impl SavedMux {
    pub fn is_empty(&self) -> bool {
        self.workspaces.iter().all(|w| w.tabs.is_empty())
    }

    pub fn first_tool(&self) -> Option<ToolName> {
        let ws = self.workspaces.get(self.ws_focus).or(self.workspaces.first())?;
        let tab = ws.tabs.get(ws.focus).or(ws.tabs.first())?;
        ToolName::from_slug(&tab.tool)
    }

    pub fn tab_count(&self) -> usize {
        self.workspaces.iter().map(|w| w.tabs.len()).sum()
    }
}

fn path() -> PathBuf {
    dirs::home_dir().unwrap_or_default().join(".agent-hop").join(FILE)
}

pub fn load() -> Option<SavedMux> {
    let text = std::fs::read_to_string(path()).ok()?;
    let mut mux: SavedMux = serde_json::from_str(&text).ok()?;
    mux.workspaces.retain(|w| !w.tabs.is_empty());
    if mux.workspaces.is_empty() {
        return None;
    }
    if mux.ws_focus >= mux.workspaces.len() {
        mux.ws_focus = mux.workspaces.len() - 1;
    }
    for ws in &mut mux.workspaces {
        if ws.focus >= ws.tabs.len() {
            ws.focus = ws.tabs.len().saturating_sub(1);
        }
    }
    Some(mux)
}

pub fn save(mux: &SavedMux) {
    if mux.is_empty() {
        return;
    }
    let path = path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(text) = serde_json::to_string_pretty(mux) {
        let _ = std::fs::write(path, text);
    }
}

/// Resume the stored session if we have an id; otherwise the latest
/// session that agent wrote for this folder.
pub fn resume_id(tool: ToolName, project_path: &str, session_id: Option<&str>) -> Option<String> {
    if let Some(id) = session_id.filter(|s| !s.is_empty()) {
        return Some(id.to_string());
    }
    adapters::find_latest_session_for_path(tool, project_path).map(|s| s.session_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_layout_json() {
        let mux = SavedMux {
            ws_focus: 1,
            workspaces: vec![
                SavedWorkspace {
                    path: "/tmp/a".into(),
                    focus: 0,
                    tabs: vec![SavedTab { tool: "claude".into(), session_id: Some("s1".into()), name: None }],
                },
                SavedWorkspace {
                    path: "/tmp/b".into(),
                    focus: 1,
                    tabs: vec![
                        SavedTab { tool: "codex".into(), session_id: None, name: None },
                        SavedTab { tool: "grok".into(), session_id: Some("g".into()), name: Some("security-droid".into()) },
                    ],
                },
            ],
        };
        let text = serde_json::to_string(&mux).unwrap();
        let back: SavedMux = serde_json::from_str(&text).unwrap();
        assert_eq!(back, mux);
        assert_eq!(back.first_tool().unwrap().slug(), "grok");
    }

    #[test]
    fn empty_mux_is_empty() {
        assert!(SavedMux::default().is_empty());
    }
}
