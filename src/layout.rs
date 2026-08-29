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

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SplitDir {
    Vertical,
    Horizontal,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct SavedSplit {
    pub dir: SplitDir,
    #[serde(default = "default_ratio")]
    pub ratio: u16,
    pub a: usize,
    pub b: usize,
}

fn default_ratio() -> u16 {
    50
}

impl SavedSplit {
    pub fn contains(self, i: usize) -> bool {
        self.a == i || self.b == i
    }

    pub fn other(self, i: usize) -> Option<usize> {
        if self.a == i {
            Some(self.b)
        } else if self.b == i {
            Some(self.a)
        } else {
            None
        }
    }

    pub fn drop_tab(&mut self, ti: usize) -> bool {
        if self.contains(ti) {
            return false;
        }
        if self.a > ti {
            self.a -= 1;
        }
        if self.b > ti {
            self.b -= 1;
        }
        true
    }
}

/// Body-relative pane rects `(x, y, w, h)` plus a 1-cell divider.
pub fn pane_rects(width: u16, height: u16, dir: SplitDir, ratio: u16) -> ((u16, u16, u16, u16), (u16, u16, u16, u16)) {
    let ratio = ratio.clamp(20, 80) as u32;
    match dir {
        SplitDir::Vertical => {
            let inner = width.saturating_sub(1);
            let left = ((inner as u32) * ratio / 100) as u16;
            let right = inner.saturating_sub(left);
            ((0, 0, left.max(8), height), (left.saturating_add(1), 0, right.max(8), height))
        }
        SplitDir::Horizontal => {
            let inner = height.saturating_sub(1);
            let top = ((inner as u32) * ratio / 100) as u16;
            let bottom = inner.saturating_sub(top);
            ((0, 0, width, top.max(3)), (0, top.saturating_add(1), width, bottom.max(3)))
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SavedWorkspace {
    pub path: String,
    #[serde(default)]
    pub focus: usize,
    pub tabs: Vec<SavedTab>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub split: Option<SavedSplit>,
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
                    split: None,
                },
                SavedWorkspace {
                    path: "/tmp/b".into(),
                    focus: 1,
                    tabs: vec![
                        SavedTab { tool: "codex".into(), session_id: None, name: None },
                        SavedTab { tool: "grok".into(), session_id: Some("g".into()), name: Some("security-droid".into()) },
                    ],
                    split: Some(SavedSplit { dir: SplitDir::Vertical, ratio: 50, a: 0, b: 1 }),
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

    #[test]
    fn pane_rects_leave_a_divider() {
        let (a, b) = pane_rects(81, 24, SplitDir::Vertical, 50);
        assert_eq!(a, (0, 0, 40, 24));
        assert_eq!(b, (41, 0, 40, 24));
        let (a, b) = pane_rects(80, 21, SplitDir::Horizontal, 50);
        assert_eq!(a.3 + b.3 + 1, 21);
        assert_eq!(a.1, 0);
        assert_eq!(b.1, a.3 + 1);
    }
}
