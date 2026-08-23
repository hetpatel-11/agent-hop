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

    render(out, &query, &results, selected)?;

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
                render(out, &query, &results, selected)?;
            }
            Some(SearchKey::Down) => {
                if selected + 1 < results.len() {
                    selected += 1;
                }
                render(out, &query, &results, selected)?;
            }
            Some(SearchKey::Backspace) => {
                query.pop();
                results = ranker.rank(&query, MAX_VISIBLE_RESULTS);
                selected = 0;
                render(out, &query, &results, selected)?;
            }
            Some(SearchKey::Char(c)) => {
                query.push(c);
                results = ranker.rank(&query, MAX_VISIBLE_RESULTS);
                selected = 0;
                render(out, &query, &results, selected)?;
            }
        }
    };

    queue!(out, cursor::Show)?;
    out.flush()?;
    Ok(outcome)
}

/// Renders in Clack's visual language (the `@clack/prompts` look:
/// diamond-headed step, a connecting spine down the left margin, hollow/
/// filled circles for unselected/selected items) -- reimplemented directly
/// rather than pulling in the `cliclack` crate, since that crate wants to
/// own its own event loop and raw-mode session for a fixed set of
/// discrete prompts, whereas this overlay is a continuously-reactive
/// fuzzy search fed through our own custom key-routing (`ChannelKeys`)
/// while a backgrounded agent's pty keeps draining -- the visual style
/// carries over cleanly, the input model doesn't.
fn render(out: &mut impl Write, query: &str, results: &[SessionRef], selected: usize) -> anyhow::Result<()> {
    queue!(out, terminal::Clear(terminal::ClearType::All), cursor::MoveTo(0, 0))?;
    let spine = theme::grey("\u{2502}");
    queue!(
        out,
        Print(format!("{}  {}\r\n", theme::bold(&theme::magenta("\u{25c6}")), theme::bold("Resume a session")))
    )?;
    queue!(out, Print(format!("{spine}\r\n")))?;
    queue!(out, Print(format!("{spine}  {}{query}\u{2588}\r\n", theme::grey("Search  "))))?;
    queue!(out, Print(format!("{spine}\r\n")))?;
    if results.is_empty() {
        queue!(out, Print(format!("{spine}  {}\r\n", theme::grey("(no results)"))))?;
    }
    for (i, r) in results.iter().enumerate() {
        let tag = theme::tool_tag(r.tool);
        let title: String = r.title.chars().take(70).collect();
        if i == selected {
            let marker = theme::bold(&theme::magenta("\u{25cf}"));
            queue!(out, Print(format!("{spine}  {marker} {tag} {}\r\n", theme::bold(&title))))?;
            if let Some(snippet) = &r.match_snippet {
                queue!(out, Print(format!("{spine}     {}\r\n", theme::grey(snippet))))?;
            }
        } else {
            let marker = theme::grey("\u{25cb}");
            queue!(out, Print(format!("{spine}  {marker} {tag} {title}\r\n")))?;
        }
    }
    queue!(out, Print(format!("{spine}\r\n")))?;
    queue!(
        out,
        Print(format!(
            "{}  {}\r\n",
            theme::grey("\u{2514}"),
            theme::grey("\u{2191}/\u{2193} move \u{00b7} enter resume \u{00b7} esc cancel")
        ))
    )?;
    out.flush()?;
    Ok(())
}
