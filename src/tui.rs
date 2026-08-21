use crate::adapters::{self, adapter_for};
use crate::agents::ToolName;
use crate::logos;
use crate::resume::{self, ChannelKeys};
use crossterm::{cursor, execute, queue, style::Print, terminal};
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use std::collections::{HashMap, HashSet};
use std::io::{stdout, Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};

struct BarAssets {
    logos: HashMap<&'static str, PathBuf>,
    use_graphics: bool,
    // Which tools' images have already been transmitted to the terminal
    // this process lifetime -- see logos::transmit_kitty's doc comment for
    // why re-sending pixel data on every redraw was a real, confirmed bug.
    transmitted: Mutex<HashSet<ToolName>>,
}

enum HopDirection {
    Next,
    Prev,
}

enum RunEvent {
    ChildExited(u64),
    Hop(HopDirection),
    SearchResume,
}

/// What to do with the pty we're about to spawn: launch the agent fresh,
/// or resume a specific existing session (its own native resume command --
/// no translation involved, since it's the agent's own session).
enum Launch {
    Fresh,
    Resume(String),
}

/// Outcome of running one agent to completion.
enum RunOutcome {
    /// The child exited on its own (user quit the agent normally).
    Exited,
    /// Alt+Up/Down was pressed -- translate the live conversation into the
    /// next/prev installed agent's format and continue there.
    Hop(HopDirection),
    /// The user picked a session from the in-TUI search overlay -- jump
    /// straight to it (that tool's own native resume, no translation).
    ResumeInto { tool: ToolName, session_id: String, project_path: String },
}

/// Single-pane TUI shell: one agent's real pty rendered full-pane, with a
/// persistent toggle strip (bottom row, owned by us, agent never draws into
/// it) for switching between installed agents via Alt+Up/Down, and a
/// search-and-resume overlay on Ctrl+R.
pub async fn run(initial: ToolName, initial_launch: Option<(String, String)>) -> anyhow::Result<()> {
    // Prefetch before entering raw mode so any network hiccup prints
    // normally instead of getting mangled by an active pty relay.
    let assets = Arc::new(BarAssets {
        logos: logos::ensure_all_logos().await,
        use_graphics: logos::supports_kitty_graphics(),
        transmitted: Mutex::new(HashSet::new()),
    });

    let sink: Arc<Mutex<InputSink>> = Arc::new(Mutex::new(InputSink::Idle));
    let (tx, rx) = mpsc::channel::<RunEvent>();
    let generation = Arc::new(AtomicU64::new(0));

    spawn_stdin_relay(sink.clone(), tx.clone());

    let mut current = initial;
    let (mut project_path, mut launch) = match initial_launch {
        Some((session_id, path)) => (path, Launch::Resume(session_id)),
        None => (
            std::env::current_dir().map(|p| p.to_string_lossy().to_string()).unwrap_or_else(|_| ".".to_string()),
            Launch::Fresh,
        ),
    };

    terminal::enable_raw_mode()?;
    execute!(stdout(), terminal::Clear(terminal::ClearType::All))?;

    let result: anyhow::Result<()> = loop {
        let generation_id = generation.fetch_add(1, Ordering::SeqCst) + 1;
        match run_one(current, &project_path, launch, &sink, &tx, &rx, generation_id, &assets) {
            Ok(RunOutcome::Exited) => break Ok(()),
            Ok(RunOutcome::Hop(dir)) => {
                let next = match dir {
                    HopDirection::Next => next_installed(current, 1),
                    HopDirection::Prev => next_installed(current, -1),
                };
                // Translate the just-active agent's live conversation into
                // the next tool's format -- this is the actual point of
                // hopping mid-conversation, not just relaunching fresh.
                launch = translate_session(current, next, &project_path).unwrap_or(Launch::Fresh);
                current = next;
            }
            Ok(RunOutcome::ResumeInto { tool, session_id, project_path: new_path }) => {
                current = tool;
                project_path = new_path;
                launch = Launch::Resume(session_id);
            }
            Err(e) => break Err(e),
        }
    };

    // Reset the scroll region back to the full screen -- otherwise the
    // user's real shell would stay confined to rows 1..rows-1 after ah
    // exits, which would look like the terminal is permanently missing its
    // last row until something else resets it.
    write!(stdout(), "\x1b[r")?;
    stdout().flush()?;
    terminal::disable_raw_mode()?;
    result
}

/// Reads whatever `from` was just running (the most recent session for
/// this project path), and writes it into `to`'s own format. Returns
/// `None` (caller falls back to a fresh launch) if there's no matching
/// session, or if the read/write translation itself fails -- resilience
/// matters more than perfection here; a failed hop should degrade to
/// "start fresh in the next agent," not crash the whole switcher.
fn translate_session(from: ToolName, to: ToolName, project_path: &str) -> Option<Launch> {
    let session_ref = adapters::find_latest_session_for_path(from, project_path)?;
    let turns = adapter_for(from).read(&session_ref).ok()?;
    let new_id = adapter_for(to).write(&turns, project_path).ok()?;
    Some(Launch::Resume(new_id))
}

fn next_installed(current: ToolName, dir: i32) -> ToolName {
    let installed: Vec<ToolName> = ToolName::ALL.into_iter().filter(|t| t.is_installed()).collect();
    if installed.is_empty() {
        return current;
    }
    let idx = installed.iter().position(|t| *t == current).unwrap_or(0) as i32;
    let len = installed.len() as i32;
    let new_idx = (idx + dir).rem_euclid(len) as usize;
    installed[new_idx]
}

/// Runs one agent to completion, or until a hop/search-resume is
/// triggered.
#[allow(clippy::too_many_arguments)]
fn run_one(
    tool: ToolName,
    project_path: &str,
    launch: Launch,
    sink: &Arc<Mutex<InputSink>>,
    tx: &mpsc::Sender<RunEvent>,
    rx: &mpsc::Receiver<RunEvent>,
    generation_id: u64,
    assets: &Arc<BarAssets>,
) -> anyhow::Result<RunOutcome> {
    let pty_system = native_pty_system();
    let (cols, rows) = terminal::size()?;
    let pair = pty_system.openpty(PtySize {
        rows: rows.saturating_sub(1),
        cols,
        pixel_width: 0,
        pixel_height: 0,
    })?;

    let mut cmd = match &launch {
        Launch::Fresh => CommandBuilder::new(tool.binary()),
        Launch::Resume(session_id) => {
            let argv = adapter_for(tool).resume_cmd(session_id, project_path);
            let mut cmd = CommandBuilder::new(&argv[0]);
            cmd.args(&argv[1..]);
            cmd
        }
    };
    cmd.cwd(project_path);
    // If agent-hop itself is being run from inside a live Claude Code
    // session (e.g. testing agent-hop from within Claude Code, or any
    // other nested-agent scenario), these env vars leak into the spawned
    // child and make it detect itself as a nested sub-session -- which
    // makes real Claude Code instances silently skip writing a session
    // transcript file at all. Confirmed as the actual root cause of a
    // "session carryover isn't working" investigation: the hop mechanism
    // itself was fine, but the spawned Claude Code never wrote a .jsonl to
    // read from in the first place, only a project-level memory/ directory.
    for (key, _) in std::env::vars() {
        if key == "CLAUDECODE"
            || key.starts_with("CLAUDE_")
            || key.starts_with("CMUX_")
            || key == "AI_AGENT"
            || key == "NODE_OPTIONS"
        {
            cmd.env_remove(key);
        }
    }
    let mut child = pair.slave.spawn_command(cmd)?;
    drop(pair.slave);

    let writer = pair.master.take_writer()?;
    *sink.lock().unwrap() = InputSink::Forward(writer);

    // Confine the real terminal's own scrolling to rows 1..=rows-1 (DECSTBM),
    // leaving the last row as a fixed margin the terminal itself will never
    // scroll. Without this, a child that scrolls its own content (many CLIs
    // print startup banners inline rather than staying in the alternate
    // screen buffer) scrolls the *entire physical terminal* -- dragging our
    // previously-drawn status line up into scrollback while a fresh copy
    // gets redrawn at the bottom every time, which is exactly what produced
    // repeated stacked "agent-hop ..." lines. This is the same primitive
    // tmux/screen use for their own status lines -- not something a
    // rendering framework like ratatui would provide either, since it's a
    // terminal-level scrolling behavior, not something rendered content
    // controls.
    write!(stdout(), "\x1b[1;{}r", rows.saturating_sub(1))?;
    // DECSTBM moves the cursor to the scrolling region's home position as a
    // side effect -- clear first so nothing stale from a previous agent's
    // screen lingers until this one's first redraw arrives.
    execute!(stdout(), terminal::Clear(terminal::ClearType::All))?;
    stdout().flush()?;

    let suppress = Arc::new(AtomicBool::new(false));

    let mut reader = pair.master.try_clone_reader()?;
    let tx_out = tx.clone();
    let assets_thread = assets.clone();
    let suppress_thread = suppress.clone();
    std::thread::spawn(move || {
        let mut out = stdout();
        let mut buf = [0u8; 8192];
        let mut esc_state = EscState::None;
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    esc_state = scan_escape_state(esc_state, &buf[..n]);
                    // While the search overlay owns the screen, still drain
                    // the child's output (so its pty buffer never fills and
                    // blocks it) but don't paint over the overlay with it.
                    if !suppress_thread.load(Ordering::SeqCst) {
                        let _ = out.write_all(&buf[..n]);
                        // Splicing our own escape sequences in right after a
                        // chunk that ends mid-sequence corrupts the child's
                        // sequence -- the terminal aborts parsing it and the
                        // orphaned tail bytes print as literal garbage (this
                        // was a real, confirmed bug: stray letters/fragments
                        // showing up on screen). Skip this redraw when we're
                        // still inside an incomplete sequence; the next
                        // chunk that completes it will trigger a clean one.
                        if esc_state == EscState::None {
                            let _ = draw_toggle_bar(&mut out, tool, &assets_thread);
                        }
                        let _ = out.flush();
                    }
                }
            }
        }
        let _ = tx_out.send(RunEvent::ChildExited(generation_id));
    });

    draw_toggle_bar(&mut stdout(), tool, assets)?;
    stdout().flush()?;

    let outcome = loop {
        match rx.recv() {
            Ok(RunEvent::ChildExited(g)) if g == generation_id => break RunOutcome::Exited,
            Ok(RunEvent::ChildExited(_)) => continue, // stale event from a prior killed child
            Ok(RunEvent::Hop(dir)) => break RunOutcome::Hop(dir),
            Ok(RunEvent::SearchResume) => {
                match run_search_overlay(sink, &suppress) {
                    Some(selected) => {
                        break RunOutcome::ResumeInto {
                            tool: selected.tool,
                            session_id: selected.session_id,
                            project_path: selected.project_path,
                        }
                    }
                    None => continue, // cancelled -- keep running this same child
                }
            }
            Err(_) => break RunOutcome::Exited,
        }
    };

    *sink.lock().unwrap() = InputSink::Idle;

    if !matches!(outcome, RunOutcome::Exited) {
        let _ = child.kill();
    }
    let _ = child.wait();

    Ok(outcome)
}

/// Pauses the current child's output, takes over the screen with the
/// search-and-resume UI (fed keys via the same persistent stdin relay
/// thread, switched into Capture mode), then hands the screen back. The
/// child process itself is never touched here -- only its *output* is
/// suppressed -- so cancelling returns to exactly where the conversation
/// was.
fn run_search_overlay(sink: &Arc<Mutex<InputSink>>, suppress: &Arc<AtomicBool>) -> Option<adapters::SessionRef> {
    suppress.store(true, Ordering::SeqCst);
    let (key_tx, key_rx) = mpsc::channel::<Vec<u8>>();
    *sink.lock().unwrap() = InputSink::Capture(key_tx);

    let sessions = crate::search::collect_sessions(&ToolName::ALL);
    let mut keys = ChannelKeys::new(key_rx);
    let mut out = stdout();
    let result = resume::run_resume_ui(sessions, &mut keys, &mut out);

    suppress.store(false, Ordering::SeqCst);
    // The child's own next redraw will repaint the screen; nothing else to
    // do here if the search was cancelled -- we deliberately never killed
    // or paused the child process, only its screen output.
    let _ = execute!(stdout(), terminal::Clear(terminal::ClearType::All));

    match result {
        Ok(Some(session_ref)) => Some(session_ref),
        _ => None,
    }
}

const LOGO_COLS: u16 = 2;

fn draw_toggle_bar(out: &mut impl Write, tool: ToolName, assets: &BarAssets) -> anyhow::Result<()> {
    let (_, rows) = terminal::size()?;
    queue!(out, cursor::SavePosition)?;
    queue!(out, cursor::MoveTo(0, rows.saturating_sub(1)))?;
    queue!(out, terminal::Clear(terminal::ClearType::CurrentLine))?;

    let logo_path = assets.logos.get(tool.slug());
    if assets.use_graphics {
        if let Some(path) = logo_path {
            let image_id = logos::image_id_for(tool);
            // Transmit the pixel data once per tool per process lifetime;
            // every other redraw just references it by id. Re-sending the
            // full base64 payload after every single output chunk was a
            // real, confirmed bug (found by capturing and inspecting raw
            // session bytes) -- it flooded the escape sequence stream and
            // visibly corrupted the screen.
            let already_sent = {
                let mut sent = assets.transmitted.lock().unwrap();
                let was_sent = sent.contains(&tool);
                sent.insert(tool);
                was_sent
            };
            if !already_sent {
                logos::transmit_kitty(path, image_id, out)?;
            }
            logos::put_kitty(image_id, LOGO_COLS, out)?;
        } else {
            queue!(out, Print(logos::text_badge(tool)))?;
        }
    } else {
        queue!(out, Print(logos::text_badge(tool)))?;
    }

    queue!(
        out,
        Print(format!(
            " agent-hop \u{25cf} {} | Alt+\u{2191}/\u{2193} switch agent \u{00b7} Ctrl+R resume",
            tool.slug()
        ))
    )?;
    queue!(out, cursor::RestorePosition)?;
    Ok(())
}

/// Where raw stdin bytes currently go: forwarded straight to the active
/// child's pty (normal operation), captured into a channel for the search
/// overlay to parse, or dropped (no child spawned yet).
enum InputSink {
    Forward(Box<dyn Write + Send>),
    Capture(mpsc::Sender<Vec<u8>>),
    Idle,
}

enum ParsedTrigger {
    AltUp,
    AltDown,
    CtrlR,
}

/// Parses a *complete* CSI sequence (`ESC [ ... <final byte 0x40-0x7e>`)
/// generically -- extracting the numeric modifier parameter and checking
/// the relevant bit, rather than requiring an exact byte-for-byte match
/// against one specific encoding.
///
/// This matters a lot: which exact bytes a terminal sends for e.g.
/// Alt+Down depends on which Kitty keyboard-protocol enhancement flags
/// the *currently active* application has pushed. Different agents can
/// (and do) push different flag sets, so an encoding that worked while
/// Claude was running is not guaranteed to still be what the terminal
/// sends once a different agent -- say, Codex -- becomes the active
/// child. Hardcoding exact constants for "the" Kitty encoding was the
/// real, confirmed root cause of "hopping works once but never again":
/// it only ever matched whatever encoding the *first* agent happened to
/// negotiate.
fn parse_csi_trigger(seq: &[u8]) -> Option<ParsedTrigger> {
    if seq.len() < 3 || seq[0] != 0x1b || seq[1] != b'[' {
        return None;
    }
    let final_byte = *seq.last()?;
    let body = std::str::from_utf8(&seq[2..seq.len() - 1]).ok()?;
    let parts: Vec<&str> = body.split(';').collect();

    match final_byte {
        b'A' | b'B' => {
            // Legacy xterm modifyOtherKeys form: "1;<modifier>". modifier
            // = 1 + shift(1) + alt(2) + ctrl(4).
            if parts.len() == 2 && parts[0] == "1" {
                let modifier: u32 = parts[1].parse().ok()?;
                if modifier.wrapping_sub(1) & 2 != 0 {
                    return Some(if final_byte == b'A' { ParsedTrigger::AltUp } else { ParsedTrigger::AltDown });
                }
            }
            None
        }
        b'u' => {
            // Kitty CSI-u form: "<codepoint>" or "<codepoint>;<modifier>"
            // (the modifier field is omitted entirely when it would be 1,
            // i.e. no modifiers held).
            let code: u32 = parts.first()?.parse().ok()?;
            let modifier: u32 = if parts.len() >= 2 { parts[1].parse().unwrap_or(1) } else { 1 };
            let bits = modifier.wrapping_sub(1);
            if code == 57419 && bits & 2 != 0 {
                return Some(ParsedTrigger::AltUp);
            }
            if code == 57420 && bits & 2 != 0 {
                return Some(ParsedTrigger::AltDown);
            }
            if code == 114 && bits & 4 != 0 {
                return Some(ParsedTrigger::CtrlR);
            }
            None
        }
        _ => None,
    }
}

/// Index of a CSI sequence's final byte (0x40-0x7e), if the sequence
/// starting at `buf[0]` (`ESC [ ...`) is complete yet.
fn find_csi_final_byte(buf: &[u8]) -> Option<usize> {
    (2..buf.len()).find(|&i| (0x40..=0x7e).contains(&buf[i]))
}

fn forward(sink: &Arc<Mutex<InputSink>>, bytes: &[u8]) {
    match &mut *sink.lock().unwrap() {
        InputSink::Forward(w) => {
            let _ = w.write_all(bytes);
            let _ = w.flush();
        }
        InputSink::Capture(s) => {
            let _ = s.send(bytes.to_vec());
        }
        InputSink::Idle => {}
    }
}

/// One persistent stdin-reading thread for the whole program lifetime.
/// Detects Alt+Up/Alt+Down and Ctrl+R (legacy CSI and Kitty CSI-u
/// encodings, decoded generically -- see `parse_csi_trigger`) and signals
/// the corresponding event; forwards everything else to whatever
/// `InputSink` is currently active. A single long-lived reader avoids the
/// correctness bug of two threads racing to read the same stdin fd
/// across hops.
fn spawn_stdin_relay(sink: Arc<Mutex<InputSink>>, tx: mpsc::Sender<RunEvent>) {
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin();
        let mut buf = [0u8; 1024];
        let mut pending: Vec<u8> = Vec::new();
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    pending.extend_from_slice(&buf[..n]);
                    'inner: loop {
                        if pending.is_empty() {
                            break;
                        }
                        // Ctrl+R, legacy single control-byte form.
                        if pending[0] == 0x12 {
                            let _ = tx.send(RunEvent::SearchResume);
                            pending.remove(0);
                            continue;
                        }
                        if pending[0] != 0x1b {
                            // Run of plain bytes -- forward as one chunk
                            // instead of byte-by-byte.
                            let end = pending.iter().position(|&b| b == 0x1b || b == 0x12).unwrap_or(pending.len());
                            let chunk: Vec<u8> = pending.drain(..end).collect();
                            forward(&sink, &chunk);
                            continue;
                        }
                        if pending.len() == 1 {
                            break; // lone ESC -- wait to see what follows
                        }
                        // A Kitty graphics protocol response (`ESC _G
                        // ... ESC \`) -- the terminal replying to a
                        // transmit/put command we sent. This arrives on
                        // *our* real stdin (we're the process actually
                        // attached to the real terminal), never as
                        // genuine keyboard input. `q=2` on our own Kitty
                        // commands should suppress these at the source
                        // (see logos.rs), but this is a defensive second
                        // layer: forwarding one into the child agent
                        // makes it echo the raw sequence back as visible
                        // garbage text -- a real, confirmed bug.
                        if pending.starts_with(b"\x1b_G") {
                            if let Some(end) = find_st_terminator(&pending) {
                                pending.drain(..end);
                                continue;
                            }
                            break; // wait for the ST terminator
                        }
                        if pending[1] == b'[' {
                            match find_csi_final_byte(&pending) {
                                Some(final_idx) => {
                                    let seq: Vec<u8> = pending.drain(..=final_idx).collect();
                                    match parse_csi_trigger(&seq) {
                                        Some(ParsedTrigger::AltUp) => {
                                            let _ = tx.send(RunEvent::Hop(HopDirection::Prev));
                                        }
                                        Some(ParsedTrigger::AltDown) => {
                                            let _ = tx.send(RunEvent::Hop(HopDirection::Next));
                                        }
                                        Some(ParsedTrigger::CtrlR) => {
                                            let _ = tx.send(RunEvent::SearchResume);
                                        }
                                        None => forward(&sink, &seq),
                                    }
                                    continue;
                                }
                                None => break 'inner, // wait for more bytes to complete the CSI sequence
                            }
                        }
                        // Some other short (Fe/Fp-class) escape, e.g.
                        // DECSC/DECRC (`ESC 7`/`ESC 8`) -- not one of
                        // ours, forward the two bytes whole.
                        let seq: Vec<u8> = pending.drain(..2).collect();
                        forward(&sink, &seq);
                    }
                }
            }
        }
    });
}

/// Finds the end (exclusive) of a `ESC \` (String Terminator) sequence in
/// `buf`, if it's complete yet. Used to know how many bytes of a Kitty
/// graphics protocol response to discard.
fn find_st_terminator(buf: &[u8]) -> Option<usize> {
    let mut i = 0;
    while i + 1 < buf.len() {
        if buf[i] == 0x1b && buf[i + 1] == b'\\' {
            return Some(i + 2);
        }
        i += 1;
    }
    None
}

/// Tracks whether a byte stream is currently in the middle of an ANSI
/// escape sequence -- not a full terminal emulator, just enough of a
/// state machine to know "is it safe to inject bytes of our own right
/// now." Persisted across chunks (a real sequence can be split across
/// multiple pty reads), since judging completeness from a single chunk in
/// isolation would misparse a sequence that started in a previous chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EscState {
    /// Not inside any escape sequence -- plain text, safe to interject.
    None,
    /// Just saw ESC, haven't seen the byte that decides what kind yet.
    Esc,
    /// CSI (`ESC [ ... <final byte 0x40-0x7e>`), e.g. cursor moves, SGR,
    /// mode toggles.
    Csi,
    /// OSC (`ESC ] ... BEL` or `... ST`), e.g. window title.
    Osc,
    /// APC/DCS/PM/SOS (`ESC _/P/^/X ... ST`), e.g. Kitty graphics.
    ApcDcs,
}

fn scan_escape_state(mut state: EscState, buf: &[u8]) -> EscState {
    let mut i = 0;
    while i < buf.len() {
        let b = buf[i];
        state = match state {
            EscState::None => {
                if b == 0x1b {
                    EscState::Esc
                } else {
                    EscState::None
                }
            }
            EscState::Esc => match b {
                b'[' => EscState::Csi,
                b']' => EscState::Osc,
                b'_' | b'P' | b'^' | b'X' => EscState::ApcDcs,
                // Any other byte completes a short (Fe/Fp-class) sequence
                // like DECSC/DECRC (`ESC 7` / `ESC 8`) immediately.
                _ => EscState::None,
            },
            EscState::Csi => {
                if (0x40..=0x7e).contains(&b) {
                    EscState::None
                } else {
                    EscState::Csi
                }
            }
            EscState::Osc => {
                if b == 0x07 {
                    EscState::None
                } else if b == 0x1b && buf.get(i + 1) == Some(&b'\\') {
                    i += 1;
                    EscState::None
                } else {
                    EscState::Osc
                }
            }
            EscState::ApcDcs => {
                if b == 0x1b && buf.get(i + 1) == Some(&b'\\') {
                    i += 1;
                    EscState::None
                } else {
                    EscState::ApcDcs
                }
            }
        };
        i += 1;
    }
    state
}

#[cfg(test)]
mod escape_state_tests {
    use super::*;

    #[test]
    fn plain_text_stays_none() {
        assert_eq!(scan_escape_state(EscState::None, b"hello world"), EscState::None);
    }

    #[test]
    fn complete_csi_sequence_returns_to_none() {
        // cursor move, e.g. ESC [ 1 ; 2 H
        assert_eq!(scan_escape_state(EscState::None, b"\x1b[1;2H"), EscState::None);
    }

    #[test]
    fn split_csi_sequence_is_detected_as_incomplete() {
        // "\x1b[?2004" with the terminating "l" not yet arrived
        let state = scan_escape_state(EscState::None, b"text\x1b[?2004");
        assert_eq!(state, EscState::Csi);
        // completing it in a later chunk brings it back to None
        assert_eq!(scan_escape_state(state, b"l"), EscState::None);
    }

    #[test]
    fn short_fe_sequence_completes_immediately() {
        // DECSC / DECRC -- exactly the sequences that were leaking as
        // literal "7"/"8" characters when spliced mid-stream.
        assert_eq!(scan_escape_state(EscState::None, b"\x1b7"), EscState::None);
        assert_eq!(scan_escape_state(EscState::None, b"\x1b8"), EscState::None);
    }

    #[test]
    fn split_kitty_graphics_apc_sequence_is_detected_as_incomplete() {
        let state = scan_escape_state(EscState::None, b"\x1b_Ga=T,f=100;AAAA");
        assert_eq!(state, EscState::ApcDcs);
        assert_eq!(scan_escape_state(state, b"BBBB\x1b\\"), EscState::None);
    }

    #[test]
    fn osc_sequence_terminated_by_bel() {
        assert_eq!(scan_escape_state(EscState::None, b"\x1b]0;title\x07"), EscState::None);
    }
}

#[cfg(test)]
mod kitty_response_filter_tests {
    use super::*;

    #[test]
    fn find_st_terminator_locates_end() {
        assert_eq!(find_st_terminator(b"\x1b_Gi=1;OK\x1b\\"), Some(11));
        assert_eq!(find_st_terminator(b"\x1b_Gi=1;OK"), None); // not terminated yet
    }

    #[test]
    fn find_st_terminator_with_trailing_bytes_after() {
        // e.g. the response arrived in the same read as a real keystroke
        let buf = b"\x1b_Gi=1;OK\x1b\\a";
        let end = find_st_terminator(buf).unwrap();
        assert_eq!(&buf[end..], b"a");
    }
}

#[cfg(test)]
mod csi_trigger_parser_tests {
    use super::*;

    #[test]
    fn kitty_csi_u_alt_arrows_with_modifier() {
        assert!(matches!(parse_csi_trigger(b"\x1b[57419;3u"), Some(ParsedTrigger::AltUp)));
        assert!(matches!(parse_csi_trigger(b"\x1b[57420;3u"), Some(ParsedTrigger::AltDown)));
    }

    #[test]
    fn legacy_xterm_alt_arrows() {
        assert!(matches!(parse_csi_trigger(b"\x1b[1;3A"), Some(ParsedTrigger::AltUp)));
        assert!(matches!(parse_csi_trigger(b"\x1b[1;3B"), Some(ParsedTrigger::AltDown)));
    }

    #[test]
    fn ctrl_r_kitty_csi_u() {
        assert!(matches!(parse_csi_trigger(b"\x1b[114;5u"), Some(ParsedTrigger::CtrlR)));
    }

    #[test]
    fn modifier_combined_with_other_bits_still_detected() {
        // shift+alt+arrow (modifier = 1+1+2 = 4) still has the alt bit set
        assert!(matches!(parse_csi_trigger(b"\x1b[57420;4u"), Some(ParsedTrigger::AltDown)));
        // ctrl+alt+arrow (modifier = 1+2+4 = 7) also still has the alt bit set
        assert!(matches!(parse_csi_trigger(b"\x1b[1;7B"), Some(ParsedTrigger::AltDown)));
    }

    #[test]
    fn plain_arrow_without_alt_is_not_a_trigger() {
        // modifier absent entirely (bare arrow, no modifiers held)
        assert!(parse_csi_trigger(b"\x1b[57420u").is_none());
        // modifier = 1 (no modifiers)
        assert!(parse_csi_trigger(b"\x1b[1;1B").is_none());
    }

    #[test]
    fn unrelated_csi_sequences_are_not_triggers() {
        assert!(parse_csi_trigger(b"\x1b[2K").is_none()); // erase line
        assert!(parse_csi_trigger(b"\x1b[?2004h").is_none()); // bracketed paste enable
        assert!(parse_csi_trigger(b"\x1b[10;20H").is_none()); // cursor position
    }

    #[test]
    fn find_csi_final_byte_detects_completion() {
        assert_eq!(find_csi_final_byte(b"\x1b[1;3B"), Some(5));
        assert_eq!(find_csi_final_byte(b"\x1b[1;3"), None); // no final byte yet
    }
}
