//! Local plugins: extra prefix chords and detection files.
//!
//! Drop a folder under `~/.agent-hop/plugins/<name>/` with `plugin.toml`:
//!
//! ```toml
//! name = "splits"
//!
//! [[bind]]
//! chord = "t"
//! action = "split-vertical"
//!
//! [[bind]]
//! chord = "e"
//! shell = "notify-send agent-hop plugin"
//! ```

use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginAction {
    SplitVertical,
    SplitHorizontal,
    NextPane,
    Zoom,
    Shell(String),
}

#[derive(Debug, Clone)]
pub struct Plugin {
    pub name: String,
    pub dir: PathBuf,
    pub binds: Vec<PluginBind>,
}

#[derive(Debug, Clone)]
pub struct PluginBind {
    pub chord: u8,
    pub action: PluginAction,
}

#[derive(Debug, Deserialize)]
struct Manifest {
    #[serde(default)]
    name: String,
    #[serde(default)]
    bind: Vec<BindToml>,
}

#[derive(Debug, Deserialize)]
struct BindToml {
    chord: String,
    #[serde(default)]
    action: Option<String>,
    #[serde(default)]
    shell: Option<String>,
}

pub fn plugins_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_default().join(".agent-hop").join("plugins")
}

pub fn load_all() -> Vec<Plugin> {
    load_from(&plugins_dir())
}

pub fn load_from(root: &std::path::Path) -> Vec<Plugin> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return out;
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let path = dir.join("plugin.toml");
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        if let Some(plugin) = parse_plugin(&text, &dir) {
            out.push(plugin);
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

pub fn parse_plugin(text: &str, dir: &std::path::Path) -> Option<Plugin> {
    let man: Manifest = toml::from_str(text).ok()?;
    let name = if man.name.is_empty() {
        dir.file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "plugin".into())
    } else {
        man.name
    };
    let mut binds = Vec::new();
    for b in man.bind {
        let Some(chord) = parse_chord(&b.chord) else { continue };
        let action = if let Some(shell) = b.shell.filter(|s| !s.trim().is_empty()) {
            PluginAction::Shell(shell)
        } else {
            match b.action.as_deref().unwrap_or("") {
                "split-vertical" | "split_vertical" | "%" => PluginAction::SplitVertical,
                "split-horizontal" | "split_horizontal" | "\"" => PluginAction::SplitHorizontal,
                "next-pane" | "next_pane" => PluginAction::NextPane,
                "zoom" => PluginAction::Zoom,
                _ => continue,
            }
        };
        binds.push(PluginBind { chord, action });
    }
    Some(Plugin { name, dir: dir.to_path_buf(), binds })
}

fn parse_chord(raw: &str) -> Option<u8> {
    let s = raw.trim();
    if s.len() == 1 {
        return Some(s.as_bytes()[0]);
    }
    if s.eq_ignore_ascii_case("percent") {
        return Some(b'%');
    }
    None
}

/// Built-in prefix letters win. First plugin that claims an unbound chord wins.
pub fn action_for(plugins: &[Plugin], chord: u8, reserved: &[u8]) -> Option<PluginAction> {
    let lower = chord.to_ascii_lowercase();
    if reserved.iter().any(|r| r.to_ascii_lowercase() == lower) {
        return None;
    }
    for p in plugins {
        if let Some(b) = p.binds.iter().find(|b| b.chord.to_ascii_lowercase() == lower) {
            return Some(b.action.clone());
        }
    }
    None
}

pub fn reserved_chords() -> &'static [u8] {
    b"nNpPaAcCwWoOiIxXqQrR?%\"hHjJkKlLzZ[]123456789"
}

pub fn print_list() {
    let plugins = load_all();
    if plugins.is_empty() {
        println!("No plugins in {}.", plugins_dir().display());
        println!("Add ~/.agent-hop/plugins/<name>/plugin.toml — see docs/PLUGINS.md.");
        return;
    }
    for p in plugins {
        println!("{}  ({})", p.name, p.dir.display());
        for b in &p.binds {
            let chord = (b.chord as char).to_string();
            let action = match &b.action {
                PluginAction::SplitVertical => "split-vertical".into(),
                PluginAction::SplitHorizontal => "split-horizontal".into(),
                PluginAction::NextPane => "next-pane".into(),
                PluginAction::Zoom => "zoom".into(),
                PluginAction::Shell(s) => format!("shell: {s}"),
            };
            println!("  Ctrl+B {chord}  {action}");
        }
    }
}

pub fn run_shell(cmd: &str) {
    if cmd.trim().is_empty() {
        return;
    }
    let _ = std::process::Command::new("sh").arg("-c").arg(cmd).spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_binds_and_shell() {
        let text = r#"
name = "demo"
[[bind]]
chord = "t"
action = "split-vertical"
[[bind]]
chord = "e"
shell = "echo hi"
"#;
        let p = parse_plugin(text, std::path::Path::new("/tmp/demo")).unwrap();
        assert_eq!(p.name, "demo");
        assert_eq!(p.binds.len(), 2);
        assert!(matches!(p.binds[0].action, PluginAction::SplitVertical));
        assert!(matches!(p.binds[1].action, PluginAction::Shell(ref s) if s == "echo hi"));
    }

    #[test]
    fn reserved_chords_block_plugins() {
        let p = parse_plugin(
            "name=\"x\"\n[[bind]]\nchord=\"n\"\naction=\"zoom\"\n",
            std::path::Path::new("/tmp/x"),
        )
        .unwrap();
        assert!(action_for(&[p], b'n', reserved_chords()).is_none());
    }
}
