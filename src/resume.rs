//! Search-and-resume UI, shared by both `ah resume` (standalone, owns
//! stdin directly via crossterm) and the in-TUI Ctrl+R overlay (fed raw
//! bytes through a channel, since the TUI's persistent stdin relay thread
//! already owns the real stdin fd). One rendering/ranking loop, two
//! `KeySource` implementations.

use crate::adapters::SessionRef;
use crate::search::Ranker;
use crate::theme;
use crossterm::{cursor, queue, style::Print, terminal};
use std::io::Write;

pub enum SearchKey {
    Char(char),
    Backspace,
    Enter,
    Up,
    Down,
    /// Esc -- cancel the overlay, return to the agent conversation exactly
    /// where it was.
    Escape,
    /// Ctrl+C -- a real, hard quit of the whole program, same as pressing
    /// it anywhere else in a normal CLI tool. Deliberately distinct from
    /// `Escape`: the two used to be treated identically, which meant
    /// there was no way to actually exit agent-hop from inside this
    /// overlay -- confirmed confusing in real use.
    Quit,
}

/// What the overlay resolved to once it exits.
pub enum ResumeOutcome {
    /// Esc -- caller should resume the conversation exactly as it was
    /// before the overlay opened.
    Cancelled,
    /// Enter on a real result -- switch into this session.
    Resume(SessionRef),
    /// Ctrl+C -- exit the whole program, not just this overlay.
    Quit,
}

pub trait KeySource {
    fn next_key(&mut self) -> anyhow::Result<Option<SearchKey>>;
}

/// Standalone `ah resume` -- owns stdin directly, no relay thread exists
/// yet at this point in the program.
pub struct CrosstermKeys;

impl KeySource for CrosstermKeys {
    fn next_key(&mut self) -> anyhow::Result<Option<SearchKey>> {
        use crossterm::event::{self, Event, KeyCode, KeyModifiers};
        loop {
            if let Event::Key(key) = event::read()? {
                if key.kind != event::KeyEventKind::Press {
                    continue;
                }
                return Ok(Some(match key.code {
                    KeyCode::Up => SearchKey::Up,
                    KeyCode::Down => SearchKey::Down,
                    KeyCode::Enter => SearchKey::Enter,
                    KeyCode::Backspace => SearchKey::Backspace,
                    KeyCode::Esc => SearchKey::Escape,
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => SearchKey::Quit,
                    KeyCode::Char(c) => SearchKey::Char(c),
                    _ => continue,
                }));
            }
        }
    }
}

const ARROW_UP_LEGACY: &[u8] = b"\x1b[A";
const ARROW_DOWN_LEGACY: &[u8] = b"\x1b[B";
const ARROW_UP_KITTY_NOMOD: &[u8] = b"\x1b[57419u";
const ARROW_DOWN_KITTY_NOMOD: &[u8] = b"\x1b[57420u";
const ARROW_UP_KITTY_MOD1: &[u8] = b"\x1b[57419;1u";
const ARROW_DOWN_KITTY_MOD1: &[u8] = b"\x1b[57420;1u";

fn is_prefix_of_any_arrow(buf: &[u8]) -> bool {
    for seq in [ARROW_UP_LEGACY, ARROW_DOWN_LEGACY, ARROW_UP_KITTY_NOMOD, ARROW_DOWN_KITTY_NOMOD, ARROW_UP_KITTY_MOD1, ARROW_DOWN_KITTY_MOD1] {
        if seq.starts_with(buf) {
            return true;
        }
    }
    false
}

/// In-TUI overlay -- reads raw bytes forwarded through a channel by the
/// persistent stdin relay thread (see tui.rs), since that thread already
/// owns the real stdin fd for the whole program's lifetime. Hand-rolled
/// byte parsing rather than crossterm here for exactly that reason: this
/// isn't reading from a real fd crossterm can poll, it's reading whatever
/// the relay thread already pulled off stdin and handed over.
pub struct ChannelKeys {
    pub rx: std::sync::mpsc::Receiver<Vec<u8>>,
    pending: Vec<u8>,
}

impl ChannelKeys {
    pub fn new(rx: std::sync::mpsc::Receiver<Vec<u8>>) -> Self {
        Self { rx, pending: Vec::new() }
    }
}

impl KeySource for ChannelKeys {
    fn next_key(&mut self) -> anyhow::Result<Option<SearchKey>> {
        loop {
            if self.pending.is_empty() {
                match self.rx.recv() {
                    Ok(bytes) => self.pending.extend(bytes),
                    Err(_) => return Ok(None),
                }
            }

            // A lone ESC byte is ambiguous with the start of an arrow
            // sequence -- if nothing else arrives within a short window,
            // treat it as a real Escape keypress (same heuristic terminal
            // libraries universally use, e.g. vim's timeoutlen).
            if self.pending == [0x1b] {
                match self.rx.recv_timeout(std::time::Duration::from_millis(50)) {
                    Ok(more) => {
                        self.pending.extend(more);
                    }
                    Err(_) => {
                        self.pending.clear();
                        return Ok(Some(SearchKey::Escape));
                    }
                }
            }

            if self.pending == ARROW_UP_LEGACY || self.pending == ARROW_UP_KITTY_NOMOD || self.pending == ARROW_UP_KITTY_MOD1 {
                self.pending.clear();
                return Ok(Some(SearchKey::Up));
            }
            if self.pending == ARROW_DOWN_LEGACY || self.pending == ARROW_DOWN_KITTY_NOMOD || self.pending == ARROW_DOWN_KITTY_MOD1 {
                self.pending.clear();
                return Ok(Some(SearchKey::Down));
            }
            if is_prefix_of_any_arrow(&self.pending) {
                continue; // wait for more bytes
            }

            // Not an arrow / not a prefix of one -- consume one logical
            // unit off the front: a control byte, or one UTF-8 char.
            let b0 = self.pending[0];
            if b0 == b'\r' || b0 == b'\n' {
                self.pending.remove(0);
                return Ok(Some(SearchKey::Enter));
            }
            if b0 == 0x7f || b0 == 0x08 {
                self.pending.remove(0);
                return Ok(Some(SearchKey::Backspace));
            }
            if b0 == 0x03 {
                self.pending.remove(0);
                return Ok(Some(SearchKey::Quit));
            }
            if b0 == 0x1b {
                // start of an unrecognized escape sequence -- drop it
                // entirely rather than feeding garbage into the query.
                self.pending.clear();
                continue;
            }
            // decode one UTF-8 scalar value off the front
            let width = utf8_width(b0);
            if self.pending.len() < width {
                match self.rx.recv() {
                    Ok(more) => {
                        self.pending.extend(more);
                        continue;
                    }
                    Err(_) => return Ok(None),
                }
            }
            let chunk: Vec<u8> = self.pending.drain(..width).collect();
            if let Ok(s) = std::str::from_utf8(&chunk) {
                if let Some(c) = s.chars().next() {
                    if !c.is_control() {
                        return Ok(Some(SearchKey::Char(c)));
                    }
                    continue;
                }
            }
        }
    }
}

fn utf8_width(first_byte: u8) -> usize {
    if first_byte & 0x80 == 0 {
        1
    } else if first_byte & 0xE0 == 0xC0 {
        2
    } else if first_byte & 0xF0 == 0xE0 {
        3
    } else if first_byte & 0xF8 == 0xF0 {
        4
    } else {
        1
    }
}

const MAX_VISIBLE_RESULTS: usize = 10;

/// Shared search-and-select loop: renders a query line + ranked results,
/// re-ranks (fast, BM25-only -- no embedding call per keystroke) on every
/// query edit, arrow keys move the selection, Enter confirms, Escape
/// cancels back to the conversation, Ctrl+C quits the whole program (see
/// `SearchKey::Quit`'s doc comment for why these are no longer the same
/// thing).
pub fn run_resume_ui(
    sessions: Vec<SessionRef>,
    initial_query: &str,
    keys: &mut impl KeySource,
    out: &mut impl Write,
) -> anyhow::Result<ResumeOutcome> {
    // Every search keeps the persistent semantic index fresh, even though
    // this interactive overlay itself only uses the fast, sync BM25 rank
    // per keystroke -- a per-keystroke embedding model call would be
    // real, felt latency in an interactive list.
    crate::search::ensure_indexing_triggered(&sessions);
    let ranker = Ranker::new(sessions);
    // `ah resume "query"` pre-fills this (see `run_standalone_resume`) so a
    // query given as a CLI argument narrows the list immediately instead of
    // starting from an empty search -- the user can still keep typing to
    // refine it, or just hit Enter on the top match.
    let mut query = initial_query.to_string();
    let mut selected = 0usize;
    let mut results = ranker.rank(&query, MAX_VISIBLE_RESULTS);

    // The real terminal cursor is hidden for the whole overlay: `render`
    // draws its own visual cursor (the solid block after the query text),
    // and the real cursor otherwise ends up parked wherever the last
    // `Print` call left it (after the footer line) -- a second, unrelated
    // blinking cursor on screen at the same time as our drawn one,
    // confirmed confusing in real use. Restored below before returning,
    // on every exit path, via the single `break`-driven loop.
    queue!(out, cursor::Hide)?;

    paint_mux(out, &query, &results, selected)?;

    let outcome = loop {
        match keys.next_key()? {
            None => break ResumeOutcome::Cancelled,
            Some(SearchKey::Escape) => break ResumeOutcome::Cancelled,
            Some(SearchKey::Quit) => break ResumeOutcome::Quit,
            Some(SearchKey::Enter) => {
                break match results.into_iter().nth(selected) {
                    Some(r) => ResumeOutcome::Resume(r),
                    None => ResumeOutcome::Cancelled,
                };
            }
            Some(SearchKey::Up) => {
                if selected > 0 {
                    selected -= 1;
                }
                paint_mux(out, &query, &results, selected)?;
            }
            Some(SearchKey::Down) => {
                if selected + 1 < results.len() {
                    selected += 1;
                }
                paint_mux(out, &query, &results, selected)?;
            }
            Some(SearchKey::Backspace) => {
                query.pop();
                results = ranker.rank(&query, MAX_VISIBLE_RESULTS);
                selected = 0;
                paint_mux(out, &query, &results, selected)?;
            }
            Some(SearchKey::Char(c)) => {
                query.push(c);
                results = ranker.rank(&query, MAX_VISIBLE_RESULTS);
                selected = 0;
                paint_mux(out, &query, &results, selected)?;
            }
        }
    };

    queue!(out, cursor::Show)?;
    out.flush()?;
    Ok(outcome)
}

pub struct SearchFrame<'a> {
    pub title: &'a str,
    pub query_prefix: &'a str,
    pub query: &'a str,
    pub results: &'a [SessionRef],
    pub selected: usize,
    /// Stage-2 semantic refine is still in flight for this query.
    pub searching: bool,
    /// Background embedder still has unindexed sessions. Used so an empty
    /// BM25 list is not announced as "no results" while vectors are building.
    pub index_pending: bool,
}

fn paint_mux(
    out: &mut impl Write,
    query: &str,
    results: &[SessionRef],
    selected: usize,
) -> anyhow::Result<()> {
    queue!(out, terminal::Clear(terminal::ClearType::All), cursor::MoveTo(0, 0))?;
    write_search_frame(
        out,
        SearchFrame {
            title: "Resume a session",
            query_prefix: "Search  ",
            query,
            results,
            selected,
            searching: false,
            index_pending: false,
        },
    )?;
    out.flush()?;
    Ok(())
}

/// One Clack-style search frame: diamond title, typed query, color-coded
/// `[agent]` tags, cyan date + highlighted match on the focused row.
/// Shared by the mux overlay and `ah cli`. Returns the number of lines
/// written so an inline (non-fullscreen) caller can MoveUp and redraw.
fn visible_len(s: &str) -> usize {
    let mut n = 0;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for c2 in chars.by_ref() {
                    if c2.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        n += 1;
    }
    n
}

/// Cut a line to `max` visible columns without breaking ANSI, so a long
/// hint cannot wrap and throw off redraw.
fn truncate_ansi(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if visible_len(s) <= max {
        return s.to_string();
    }
    let keep = max.saturating_sub(1);
    let mut out = String::new();
    let mut visible = 0;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            out.push(c);
            if chars.peek() == Some(&'[') {
                out.push(chars.next().unwrap());
                for c2 in chars.by_ref() {
                    out.push(c2);
                    if c2.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        if visible >= keep {
            break;
        }
        out.push(c);
        visible += 1;
    }
    out.push('…');
    out.push_str("\x1b[0m");
    out
}

/// None = do not show an empty-state row (either there are hits, or
/// semantic refine is still running — v0.0.6 never said "no results" then).
pub fn empty_status(
    query: &str,
    results_empty: bool,
    semantic_pending: bool,
    index_pending: bool,
) -> Option<&'static str> {
    if !results_empty || query.trim().is_empty() {
        return None;
    }
    if semantic_pending {
        return None;
    }
    if index_pending {
        return Some(
            "Semantic search is still indexing sessions — try this phrase again in a moment.",
        );
    }
    Some("no results found — try a different phrase")
}

fn put_line(out: &mut impl Write, s: &str, lines: &mut u16) -> anyhow::Result<()> {
    let cols = terminal::size().map(|(c, _)| c as usize).unwrap_or(80);
    let s = truncate_ansi(s, cols.saturating_sub(1));
    queue!(out, Print(s), Print("\r\n"))?;
    *lines += 1;
    Ok(())
}

pub fn write_search_frame(out: &mut impl Write, frame: SearchFrame<'_>) -> anyhow::Result<u16> {
    let spine = theme::grey("│");
    let mut lines = 0u16;

    put_line(
        out,
        &format!("{}  {}", theme::bold(&theme::magenta("◆")), theme::bold(frame.title)),
        &mut lines,
    )?;
    put_line(out, &spine, &mut lines)?;
    put_line(
        out,
        &format!(
            "{spine}  {}{}{}",
            theme::grey(frame.query_prefix),
            frame.query,
            theme::bold("█")
        ),
        &mut lines,
    )?;
    put_line(out, &spine, &mut lines)?;

    if let Some(msg) = empty_status(
        frame.query,
        frame.results.is_empty(),
        frame.searching,
        frame.index_pending,
    ) {
        put_line(out, &format!("{spine}  {}", theme::grey(msg)), &mut lines)?;
    }

    for (i, r) in frame.results.iter().enumerate() {
        let tag = theme::tool_tag(r.tool);
        let title = crate::search::session_title(r, 70);
        if i == frame.selected {
            let marker = theme::bold(&theme::magenta("●"));
            put_line(
                out,
                &format!("{spine}  {marker} {tag} {}", theme::bold(&title)),
                &mut lines,
            )?;
            put_line(
                out,
                &format!("{spine}     {}", crate::search::session_hint(r)),
                &mut lines,
            )?;
        } else {
            let marker = theme::grey("○");
            put_line(out, &format!("{spine}  {marker} {tag} {title}"), &mut lines)?;
        }
    }

    if frame.searching {
        put_line(
            out,
            &format!(
                "{spine}  {}",
                theme::grey("(still semantically searching for more…)")
            ),
            &mut lines,
        )?;
    }

    put_line(out, &spine, &mut lines)?;
    put_line(
        out,
        &format!(
            "{}  {}",
            theme::grey("└"),
            theme::grey("↑/↓ move · enter pick · esc cancel")
        ),
        &mut lines,
    )?;
    Ok(lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_ansi_counts_visible_only() {
        let colored = format!("{}hello{}", "\x1b[38;5;208m", "\x1b[39m");
        assert_eq!(visible_len(&colored), 5);
        let cut = truncate_ansi(&colored, 3);
        assert_eq!(visible_len(&cut), 3);
        assert!(cut.contains('…'));
    }

    #[test]
    fn truncate_ansi_leaves_short_lines_alone() {
        assert_eq!(truncate_ansi("hi", 10), "hi");
    }

    #[test]
    fn no_results_waits_for_semantic() {
        assert_eq!(
            empty_status("oauth", true, true, false),
            None,
            "stage 2 in flight must not say no results"
        );
        assert_eq!(
            empty_status("oauth", true, false, true),
            Some("Semantic search is still indexing sessions — try this phrase again in a moment.")
        );
        assert_eq!(
            empty_status("oauth", true, false, false),
            Some("no results found — try a different phrase")
        );
        assert_eq!(empty_status("oauth", false, false, false), None);
        assert_eq!(empty_status("", true, false, false), None);
    }
}
