//! Screen-matching agent status, same shape as herdr's per-agent manifests:
//! look at the live VT snapshot (visible lines + OSC title/progress) and
//! decide idle / working / blocked / done / unknown. Not a silence timer.
//!
//! User TOML under `~/.agent-hop/detect/*.toml` (and herdr-style
//! `~/.local/state/agent-hop/agent-detection/remote/`) runs first.

use crate::agents::ToolName;
use serde::Deserialize;
use std::sync::OnceLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentStatus {
    Idle,
    Working,
    Blocked,
    Done,
    Unknown,
}

impl AgentStatus {
    pub fn label(self) -> &'static str {
        match self {
            AgentStatus::Idle => "idle",
            AgentStatus::Working => "working",
            AgentStatus::Blocked => "blocked",
            AgentStatus::Done => "done",
            AgentStatus::Unknown => "unknown",
        }
    }

    pub fn from_label(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "idle" => Some(Self::Idle),
            "working" => Some(Self::Working),
            "blocked" => Some(Self::Blocked),
            "done" => Some(Self::Done),
            "unknown" => Some(Self::Unknown),
            _ => None,
        }
    }

    pub fn icon(self) -> char {
        match self {
            AgentStatus::Idle => '✓',
            AgentStatus::Working => '…',
            AgentStatus::Blocked => '∗',
            AgentStatus::Done => '■',
            AgentStatus::Unknown => '?',
        }
    }

    pub fn as_u8(self) -> u8 {
        match self {
            AgentStatus::Idle => 0,
            AgentStatus::Working => 1,
            AgentStatus::Blocked => 2,
            AgentStatus::Done => 3,
            AgentStatus::Unknown => 4,
        }
    }

    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => AgentStatus::Working,
            2 => AgentStatus::Blocked,
            3 => AgentStatus::Done,
            4 => AgentStatus::Unknown,
            _ => AgentStatus::Idle,
        }
    }
}

/// Visible child screen plus OSC extras herdr's rules also read.
#[derive(Clone, Debug, Default)]
pub struct Screen {
    pub lines: Vec<String>,
    pub osc_title: Option<String>,
    pub osc_progress: Option<String>,
}

impl Screen {
    #[cfg(test)]
    pub fn from_lines(lines: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self { lines: lines.into_iter().map(Into::into).collect(), osc_title: None, osc_progress: None }
    }

    fn joined(&self) -> String {
        self.lines.join("\n")
    }

    fn lower(&self) -> String {
        self.joined().to_ascii_lowercase()
    }

    fn contains_ci(&self, needle: &str) -> bool {
        self.lower().contains(&needle.to_ascii_lowercase())
    }

    fn bottom(&self, n: usize) -> String {
        let start = self.lines.len().saturating_sub(n);
        self.lines[start..].join("\n")
    }

    fn bottom_ci(&self, n: usize) -> String {
        self.bottom(n).to_ascii_lowercase()
    }

    fn any_line(&self, pred: impl Fn(&str) -> bool) -> bool {
        self.lines.iter().any(|l| pred(l))
    }
}

/// Pull OSC 0/1/2 titles and OSC 9;4 progress out of a raw PTY stream.
#[derive(Default)]
pub struct OscProbe {
    title: Option<String>,
    progress: Option<String>,
    pending: Vec<u8>,
}

impl OscProbe {
    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    pub fn progress(&self) -> Option<&str> {
        self.progress.as_deref()
    }

    pub fn feed(&mut self, data: &[u8]) {
        self.pending.extend_from_slice(data);
        let mut i = 0;
        while i < self.pending.len() {
            if self.pending[i] != 0x1b {
                i += 1;
                continue;
            }
            if i + 1 >= self.pending.len() {
                break;
            }
            if self.pending[i + 1] != b']' {
                i += 1;
                continue;
            }
            let body_start = i + 2;
            let mut j = body_start;
            let mut ended = None;
            while j < self.pending.len() {
                if self.pending[j] == 0x07 {
                    ended = Some((j, j + 1));
                    break;
                }
                if self.pending[j] == 0x1b && j + 1 < self.pending.len() && self.pending[j + 1] == b'\\' {
                    ended = Some((j, j + 2));
                    break;
                }
                j += 1;
            }
            let Some((end, consume)) = ended else { break };
            if let Ok(payload) = std::str::from_utf8(&self.pending[body_start..end]) {
                apply_osc(&mut self.title, &mut self.progress, payload);
            }
            i = consume;
        }
        self.pending.drain(..i);
        const CAP: usize = 4096;
        if self.pending.len() > CAP {
            let keep = CAP / 2;
            self.pending.drain(..self.pending.len() - keep);
        }
    }
}

fn apply_osc(title: &mut Option<String>, progress: &mut Option<String>, payload: &str) {
    let (ps, pt) = match payload.split_once(';') {
        Some((ps, pt)) => (ps, pt),
        None => (payload, ""),
    };
    match ps {
        "0" | "1" | "2" => *title = Some(pt.to_string()),
        "9" => {
            if pt.starts_with("4;") || pt == "4" {
                *progress = Some(pt.to_string());
            }
        }
        _ => {}
    }
}

/// Classify `screen` for `tool`. Overlay UIs (transcript viewer) keep
/// `previous` so a scrollback pane does not flip the sidebar.
pub fn detect(tool: ToolName, screen: &Screen, previous: AgentStatus) -> AgentStatus {
    if is_overlay(tool, screen) {
        return previous;
    }
    if let Some(from_toml) = match_manifests(tool, screen) {
        return from_toml;
    }
    if let Some(status) = detect_done(screen) {
        return status;
    }
    let built_in = match tool {
        ToolName::Claude => detect_claude(screen),
        ToolName::Codex => detect_codex(screen),
        ToolName::Grok => detect_grok(screen),
        ToolName::OpenCode => detect_opencode(screen),
        ToolName::Pi => detect_pi(screen),
        _ => detect_generic(screen),
    };
    if let Some(status) = built_in {
        return status;
    }
    match tool {
        ToolName::OpenCode | ToolName::Pi => AgentStatus::Idle,
        ToolName::Claude | ToolName::Codex | ToolName::Grok => previous,
        _ => {
            if previous == AgentStatus::Unknown {
                AgentStatus::Unknown
            } else {
                previous
            }
        }
    }
}

fn detect_done(s: &Screen) -> Option<AgentStatus> {
    if s.contains_ci("no conversation found")
        || s.contains_ci("session ended")
        || s.contains_ci("conversation ended")
        || s.contains_ci("agent has finished")
    {
        return Some(AgentStatus::Done);
    }
    None
}

fn detect_generic(s: &Screen) -> Option<AgentStatus> {
    if detect_done(s).is_some() {
        return Some(AgentStatus::Done);
    }
    if s.contains_ci("esc to interrupt")
        || s.contains_ci("ctrl+c to interrupt")
        || s.contains_ci("working...")
        || s.contains_ci("thinking...")
    {
        return Some(AgentStatus::Working);
    }
    if s.contains_ci("allow command")
        || s.contains_ci("permission required")
        || s.contains_ci("do you want to")
        || s.contains_ci("[y/n]")
    {
        return Some(AgentStatus::Blocked);
    }
    if s.any_line(|l| {
        let t = l.trim();
        t == ">" || t == "❯" || t.starts_with("> ") || t.starts_with("❯ ")
    }) {
        return Some(AgentStatus::Idle);
    }
    None
}

#[derive(Debug, Deserialize)]
struct ManifestFile {
    #[serde(default)]
    agent: String,
    #[serde(default)]
    rule: Vec<ManifestRule>,
}

#[derive(Debug, Deserialize, Clone)]
struct ManifestRule {
    status: String,
    #[serde(default)]
    contains: Option<String>,
    #[serde(default)]
    title_contains: Option<String>,
}

#[derive(Default)]
struct ManifestSet {
    rules: Vec<(String, ManifestRule)>,
}

fn manifests() -> &'static ManifestSet {
    static SET: OnceLock<ManifestSet> = OnceLock::new();
    SET.get_or_init(load_manifests)
}

fn detect_dirs() -> Vec<std::path::PathBuf> {
    if let Ok(dir) = std::env::var("AH_DETECT_DIR") {
        return vec![std::path::PathBuf::from(dir)];
    }
    let mut dirs = Vec::new();
    if let Some(home) = dirs::home_dir() {
        dirs.push(home.join(".agent-hop").join("detect"));
        dirs.push(home.join(".local").join("state").join("agent-hop").join("agent-detection").join("remote"));
        dirs.push(home.join(".agent-hop").join("plugins"));
    }
    dirs
}

fn load_manifests() -> ManifestSet {
    let mut set = ManifestSet::default();
    for dir in detect_dirs() {
        load_manifest_dir(&dir, &mut set);
    }
    set
}

fn load_manifest_dir(dir: &std::path::Path, set: &mut ManifestSet) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let nested = path.join("detect.toml");
            if nested.is_file() {
                load_manifest_file(&nested, set);
            }
            continue;
        }
        if path.extension().and_then(|s| s.to_str()) == Some("toml") {
            load_manifest_file(&path, set);
        }
    }
}

fn load_manifest_file(path: &std::path::Path, set: &mut ManifestSet) {
    let Ok(text) = std::fs::read_to_string(path) else { return };
    ingest_manifest(&text, set);
}

fn ingest_manifest(text: &str, set: &mut ManifestSet) {
    let Ok(file) = toml::from_str::<ManifestFile>(text) else { return };
    let agent = file.agent.trim().to_ascii_lowercase();
    if agent.is_empty() {
        return;
    }
    for rule in file.rule {
        set.rules.push((agent.clone(), rule));
    }
}

fn match_manifests(tool: ToolName, screen: &Screen) -> Option<AgentStatus> {
    let slug = tool.slug();
    for (agent, rule) in &manifests().rules {
        if agent != slug {
            continue;
        }
        let status = AgentStatus::from_label(&rule.status)?;
        let mut hit = false;
        if let Some(needle) = rule.contains.as_deref() {
            if screen.contains_ci(needle) {
                hit = true;
            }
        }
        if let Some(needle) = rule.title_contains.as_deref() {
            if screen.osc_title.as_deref().is_some_and(|t| t.to_ascii_lowercase().contains(&needle.to_ascii_lowercase())) {
                hit = true;
            }
        }
        if rule.contains.is_none() && rule.title_contains.is_none() {
            continue;
        }
        if hit {
            return Some(status);
        }
    }
    None
}

/// Parse a single TOML snippet (tests / `ah` docs). Not used at runtime.
#[cfg(test)]
pub fn parse_manifest_rules(text: &str) -> Option<(String, Vec<(AgentStatus, String)>)> {
    let file: ManifestFile = toml::from_str(text).ok()?;
    let mut out = Vec::new();
    for r in file.rule {
        let status = AgentStatus::from_label(&r.status)?;
        let needle = r.contains.or(r.title_contains)?;
        out.push((status, needle));
    }
    Some((file.agent, out))
}

fn is_overlay(tool: ToolName, s: &Screen) -> bool {
    match tool {
        ToolName::Claude => s.contains_ci("showing detailed transcript"),
        ToolName::Codex => {
            s.contains_ci("q to quit")
                && (s.contains_ci("↑/↓ to scroll") || s.contains_ci("pgup/pgdn"))
        }
        _ => false,
    }
}

fn detect_claude(s: &Screen) -> Option<AgentStatus> {
    if claude_working(s) {
        return Some(AgentStatus::Working);
    }
    if claude_strong_blocked(s) {
        return Some(AgentStatus::Blocked);
    }
    if claude_idle_prompt(s) {
        return Some(AgentStatus::Idle);
    }
    if claude_weak_blocked(s) {
        return Some(AgentStatus::Blocked);
    }
    if s.osc_title.as_deref().is_some_and(|t| t.starts_with('\u{2733}')) {
        return Some(AgentStatus::Idle);
    }
    if s.osc_progress.as_deref().is_some_and(|p| p.starts_with("4;0")) {
        return Some(AgentStatus::Idle);
    }
    None
}

fn claude_working(s: &Screen) -> bool {
    if title_has_working_spinner(s.osc_title.as_deref()) {
        return true;
    }
    if s.contains_ci("esc to interrupt") {
        return true;
    }
    if s.contains_ci("waiting for") && s.contains_ci("background agent") {
        return true;
    }
    if s.contains_ci("mcp") && s.contains_ci("still running") {
        return true;
    }
    // Herdr `live_turn_working`: a status mark + ellipsis, even when the
    // prompt box (❯) is still drawn and "esc to interrupt" is absent.
    s.any_line(claude_turn_line)
}

fn claude_turn_line(line: &str) -> bool {
    let t = line.trim_start();
    let Some(mark) = t.chars().next() else { return false };
    if !(claude_activity_mark(mark) || mark == '⏸' || mark == '⏵') {
        return false;
    }
    t.contains('…') || t.contains("...")
}

fn claude_activity_mark(c: char) -> bool {
    matches!(c, '*' | '·' | '✢' | '✱' | '✻' | '✼' | '✶' | '✳' | '❋') || is_braille(c)
}

fn claude_strong_blocked(s: &Screen) -> bool {
    let text = s.lower();
    text.contains("waiting for permission")
        || text.contains("do you want to allow this connection?")
        || text.contains("do you want to proceed?")
        || (text.contains("esc to cancel") && (text.contains("enter to confirm") || text.contains("enter to select")))
}

fn claude_weak_blocked(s: &Screen) -> bool {
    let text = s.lower();
    if text.contains("do you want to") || text.contains("would you like to") {
        return text.contains("yes") || s.any_line(|l| l.contains('❯'));
    }
    false
}

fn claude_idle_prompt(s: &Screen) -> bool {
    if !s.any_line(|l| l.trim_start().starts_with('❯')) {
        return false;
    }
    let t = s.lower();
    !t.contains("esc to cancel") && !t.contains("enter to select") && !t.contains("do you want to proceed?")
}

fn detect_codex(s: &Screen) -> Option<AgentStatus> {
    let title = s.osc_title.as_deref().unwrap_or("");
    if title.contains("Action Required") {
        return Some(AgentStatus::Blocked);
    }
    if title_has_braille_dots(title) {
        return Some(AgentStatus::Working);
    }
    if s.contains_ci("allow command?")
        || s.contains_ci("press enter to confirm or esc to cancel")
        || s.contains_ci("do you trust the contents of this directory")
        || s.contains_ci("[y/n]")
    {
        return Some(AgentStatus::Blocked);
    }
    if s.any_line(|l| l.contains("Working") && l.to_ascii_lowercase().contains("esc to interrupt")) {
        return Some(AgentStatus::Working);
    }
    if !title.is_empty() {
        return Some(AgentStatus::Idle);
    }
    None
}

fn detect_grok(s: &Screen) -> Option<AgentStatus> {
    let title = s.osc_title.as_deref().unwrap_or("");
    if title.contains("Action Required") {
        return Some(AgentStatus::Blocked);
    }
    if s.any_line(|l| l.contains('┃') && (l.contains("(○)") || l.contains("(●)")))
        || s.bottom_ci(2).contains(":select") && s.bottom_ci(2).contains("ctrl+o:yolo")
        || s.bottom_ci(2).contains("shift+x:dismiss")
    {
        return Some(AgentStatus::Blocked);
    }
    if s.osc_progress.as_deref() == Some("4;1;-1") {
        return Some(AgentStatus::Working);
    }
    if grok_idle_title(title) {
        return Some(AgentStatus::Idle);
    }
    if !title.is_empty() && !grok_idle_title(title) {
        return Some(AgentStatus::Working);
    }
    if s.osc_progress.as_deref() == Some("4;0;0") || s.osc_progress.as_deref().is_some_and(|p| p.starts_with("4;0")) {
        return Some(AgentStatus::Idle);
    }
    if s.any_line(|l| l.contains("[stop]")) || s.bottom_ci(2).contains("esc:cancel") {
        return Some(AgentStatus::Working);
    }
    if s.bottom_ci(2).contains("ctrl+.:shortcuts") && !s.bottom_ci(2).contains("esc:cancel") {
        return Some(AgentStatus::Idle);
    }
    None
}

fn grok_idle_title(title: &str) -> bool {
    let t = title.trim();
    (t == "grok" || t.ends_with(" - grok")) && !t.chars().any(is_braille)
}

fn detect_opencode(s: &Screen) -> Option<AgentStatus> {
    if s.contains_ci("permission required")
        || s.contains_ci("esc dismiss") && (s.contains_ci("enter confirm") || s.contains_ci("enter submit"))
    {
        return Some(AgentStatus::Blocked);
    }
    if s.contains_ci("esc to interrupt") || s.contains_ci("ctrl+c to interrupt") || s.contains_ci("press esc to interrupt")
    {
        return Some(AgentStatus::Working);
    }
    Some(AgentStatus::Idle)
}

fn detect_pi(s: &Screen) -> Option<AgentStatus> {
    if s.contains_ci("working...") {
        Some(AgentStatus::Working)
    } else {
        Some(AgentStatus::Idle)
    }
}

fn title_has_working_spinner(title: Option<&str>) -> bool {
    let Some(t) = title.filter(|t| !t.is_empty()) else { return false };
    t.chars().next().is_some_and(|c| is_braille(c) || ('\u{25D0}'..='\u{25D3}').contains(&c))
}

fn title_has_braille_dots(title: &str) -> bool {
    const DOTS: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
    title.chars().any(|c| DOTS.contains(&c))
}

fn is_braille(c: char) -> bool {
    ('\u{2800}'..='\u{28FF}').contains(&c)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(lines: &[&str]) -> Screen {
        Screen::from_lines(lines.iter().copied())
    }

    fn titled(lines: &[&str], title: &str) -> Screen {
        let mut s = screen(lines);
        s.osc_title = Some(title.into());
        s
    }

    #[test]
    fn claude_interrupt_is_working() {
        let s = screen(&["* Crystallizing… (3s)", "esc to interrupt"]);
        assert_eq!(detect(ToolName::Claude, &s, AgentStatus::Idle), AgentStatus::Working);
    }

    #[test]
    fn claude_prompt_is_idle() {
        let s = screen(&["How can I help?", "❯ "]);
        assert_eq!(detect(ToolName::Claude, &s, AgentStatus::Working), AgentStatus::Idle);
    }

    #[test]
    fn claude_spinner_ellipsis_is_working_even_with_prompt() {
        // Claude keeps the ❯ box on screen during a turn. Herdr ranks the
        // spinner+ellipsis line above the prompt-box idle rule; we used to
        // miss that line (no "esc to interrupt") and snap to idle.
        let s = screen(&["* Crystallizing…", "❯ "]);
        assert_eq!(detect(ToolName::Claude, &s, AgentStatus::Idle), AgentStatus::Working);
        let s = screen(&["✶ Wrangling… (3s · esc to interrupt)", "❯ "]);
        assert_eq!(detect(ToolName::Claude, &s, AgentStatus::Idle), AgentStatus::Working);
        let s = screen(&["⏸ Wrangling · esc to interrupt"]);
        assert_eq!(detect(ToolName::Claude, &s, AgentStatus::Idle), AgentStatus::Working);
    }

    #[test]
    fn unmatched_claude_screen_holds_previous() {
        let s = screen(&["some streaming token output"]);
        assert_eq!(detect(ToolName::Claude, &s, AgentStatus::Working), AgentStatus::Working);
        assert_eq!(detect(ToolName::Claude, &s, AgentStatus::Blocked), AgentStatus::Blocked);
    }

    #[test]
    fn claude_permission_is_blocked() {
        let s = screen(&["Bash(ls)", "Do you want to proceed?", "  1. Yes", "  2. No", "esc to cancel"]);
        assert_eq!(detect(ToolName::Claude, &s, AgentStatus::Working), AgentStatus::Blocked);
    }

    #[test]
    fn claude_title_spinner_is_working() {
        let s = titled(&[" "], "⣿ working");
        assert_eq!(detect(ToolName::Claude, &s, AgentStatus::Idle), AgentStatus::Working);
    }

    #[test]
    fn claude_title_star_is_idle() {
        let s = titled(&[" "], "✳ claude");
        assert_eq!(detect(ToolName::Claude, &s, AgentStatus::Working), AgentStatus::Idle);
    }

    #[test]
    fn claude_transcript_holds_previous() {
        let s = screen(&["showing detailed transcript", "ctrl+o to toggle"]);
        assert_eq!(detect(ToolName::Claude, &s, AgentStatus::Working), AgentStatus::Working);
        assert_eq!(detect(ToolName::Claude, &s, AgentStatus::Blocked), AgentStatus::Blocked);
    }

    #[test]
    fn codex_working_line() {
        let s = screen(&["• Working (3s · esc to interrupt)"]);
        assert_eq!(detect(ToolName::Codex, &s, AgentStatus::Idle), AgentStatus::Working);
    }

    #[test]
    fn codex_action_required_title_is_blocked() {
        let s = titled(&["allow command?"], "Action Required");
        assert_eq!(detect(ToolName::Codex, &s, AgentStatus::Working), AgentStatus::Blocked);
    }

    #[test]
    fn codex_quiet_title_is_idle() {
        let s = titled(&["ready"], "codex");
        assert_eq!(detect(ToolName::Codex, &s, AgentStatus::Working), AgentStatus::Idle);
    }

    #[test]
    fn grok_stop_chip_is_working() {
        let s = screen(&["⠧ Waiting on subagent… 2.8s   13s ⇣29.7k [stop]"]);
        assert_eq!(detect(ToolName::Grok, &s, AgentStatus::Idle), AgentStatus::Working);
    }

    #[test]
    fn grok_permission_dialog_is_blocked() {
        let s = screen(&["┃  2 (○) Yes, proceed", "1/3:select │ Ctrl+o:yolo │ Ctrl+c:cancel"]);
        assert_eq!(detect(ToolName::Grok, &s, AgentStatus::Working), AgentStatus::Blocked);
    }

    #[test]
    fn grok_idle_footer() {
        let s = screen(&["> ", "Ctrl+.:shortcuts"]);
        assert_eq!(detect(ToolName::Grok, &s, AgentStatus::Working), AgentStatus::Idle);
    }

    #[test]
    fn grok_progress_osc() {
        let mut s = screen(&[" "]);
        s.osc_progress = Some("4;1;-1".into());
        assert_eq!(detect(ToolName::Grok, &s, AgentStatus::Idle), AgentStatus::Working);
        s.osc_progress = Some("4;0;0".into());
        s.osc_title = Some("grok".into());
        assert_eq!(detect(ToolName::Grok, &s, AgentStatus::Working), AgentStatus::Idle);
    }

    #[test]
    fn opencode_permission_and_interrupt() {
        let blocked = screen(&["△ Permission required", "enter confirm"]);
        assert_eq!(detect(ToolName::OpenCode, &blocked, AgentStatus::Idle), AgentStatus::Blocked);
        let working = screen(&["esc to interrupt"]);
        assert_eq!(detect(ToolName::OpenCode, &working, AgentStatus::Idle), AgentStatus::Working);
        let idle = screen(&["ready"]);
        assert_eq!(detect(ToolName::OpenCode, &idle, AgentStatus::Working), AgentStatus::Idle);
    }

    #[test]
    fn pi_working_literal() {
        assert_eq!(
            detect(ToolName::Pi, &screen(&["Working..."]), AgentStatus::Idle),
            AgentStatus::Working
        );
        assert_eq!(detect(ToolName::Pi, &screen(&["> "]), AgentStatus::Working), AgentStatus::Idle);
    }

    #[test]
    fn osc_probe_title_and_progress_across_chunks() {
        let mut p = OscProbe::default();
        p.feed(b"\x1b]0;cla");
        assert!(p.title().is_none());
        p.feed(b"ude\x07");
        assert_eq!(p.title(), Some("claude"));
        p.feed(b"\x1b]9;4;1;-1\x1b\\");
        assert_eq!(p.progress(), Some("4;1;-1"));
    }

    #[test]
    fn five_status_labels_roundtrip() {
        for s in [AgentStatus::Idle, AgentStatus::Working, AgentStatus::Blocked, AgentStatus::Done, AgentStatus::Unknown] {
            assert_eq!(AgentStatus::from_u8(s.as_u8()), s);
            assert_eq!(AgentStatus::from_label(s.label()), Some(s));
        }
    }

    #[test]
    fn session_ended_is_done() {
        let s = screen(&["No conversation found"]);
        assert_eq!(detect(ToolName::Claude, &s, AgentStatus::Working), AgentStatus::Done);
    }

    #[test]
    fn generic_cursor_prompt_is_idle() {
        let s = screen(&["> "]);
        assert_eq!(detect(ToolName::Cursor, &s, AgentStatus::Unknown), AgentStatus::Idle);
        let s = screen(&["esc to interrupt"]);
        assert_eq!(detect(ToolName::Gemini, &s, AgentStatus::Idle), AgentStatus::Working);
    }

    #[test]
    fn unmatched_new_harness_holds_previous() {
        let s = screen(&["streaming tokens"]);
        assert_eq!(detect(ToolName::Droid, &s, AgentStatus::Working), AgentStatus::Working);
        assert_eq!(detect(ToolName::Copilot, &s, AgentStatus::Unknown), AgentStatus::Unknown);
    }

    #[test]
    fn toml_manifest_parses_herdr_style_rules() {
        let text = r#"
agent = "cursor"
version = 1
[[rule]]
status = "working"
contains = "Generating"
[[rule]]
status = "done"
contains = "conversation ended"
"#;
        let (agent, rules) = parse_manifest_rules(text).unwrap();
        assert_eq!(agent, "cursor");
        assert_eq!(rules[0].0, AgentStatus::Working);
        assert_eq!(rules[1].0, AgentStatus::Done);
    }
}
