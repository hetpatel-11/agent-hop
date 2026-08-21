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
    /// The real terminal was resized (new cols, new rows). Not tied to any
    /// particular generation -- always relevant regardless of which agent
    /// is currently running.
    Resized(u16, u16),
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
    spawn_resize_poller(tx.clone());

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

    let suppress = Arc::new(AtomicBool::new(false));
    // Shared "what does the real terminal look like right now" -- updated
    // by the main loop below when a resize is detected, read by the reader
    // thread so its vt100 model and the pty stay in sync with reality. See
    // spawn_resize_poller's doc comment for why this exists at all: without
    // it, resizing your terminal window mid-session (an entirely ordinary
    // thing to do) desyncs the child's content model from where our status
    // row thinks the bottom of the screen is, and the two start colliding.
    let dims: Arc<Mutex<(u16, u16)>> = Arc::new(Mutex::new((cols, rows)));

    let mut reader = pair.master.try_clone_reader()?;
    let tx_out = tx.clone();
    let assets_thread = assets.clone();
    let suppress_thread = suppress.clone();
    let dims_thread = dims.clone();
    std::thread::spawn(move || {
        let mut out = stdout();
        let mut buf = [0u8; 8192];
        // The child's raw bytes are never written to the real terminal
        // directly -- they're parsed into a real virtual screen model
        // instead. This is the actual fix for the whole family of bugs
        // from tonight (escape-sequence splicing, Kitty response leakage,
        // scroll-region stacking): there's no longer a shared raw byte
        // stream for our own status row to collide with, because the
        // child's bytes are consumed and modeled, not passed through.
        // `vt100::Parser` internally handles sequences split across
        // multiple reads correctly (it's a real incremental VT parser),
        // so there's no need for our own "wait for a complete sequence"
        // bookkeeping either.
        let mut applied_dims = (cols, rows);
        let mut parser = vt100::Parser::new(rows.saturating_sub(1), cols, 0);
        let mut prev_screen = parser.screen().clone();
        execute!(out, terminal::Clear(terminal::ClearType::All)).ok();
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let current_dims = *dims_thread.lock().unwrap();
                    if current_dims != applied_dims {
                        applied_dims = current_dims;
                        let new_child_rows = current_dims.1.saturating_sub(1);
                        parser.set_size(new_child_rows, current_dims.0);
                        // The old prev_screen is the wrong size to diff
                        // against now -- force a full redraw at the new
                        // size instead of an invalid partial one.
                        prev_screen = vt100::Parser::new(new_child_rows, current_dims.0, 0).screen().clone();
                        execute!(out, terminal::Clear(terminal::ClearType::All)).ok();
                    }
                    parser.process(&buf[..n]);
                    // While the search overlay owns the screen, still drain
                    // the child's output (so its pty buffer never fills and
                    // blocks it) but don't paint over the overlay with it.
                    if !suppress_thread.load(Ordering::SeqCst) {
                        let current_screen = parser.screen().clone();
                        // A byte stream sufficient to turn what was
                        // previously on screen into what's on screen now --
                        // well-formed by construction, including correctly
                        // repositioning the cursor to where the child
                        // expects it. This is what makes the status-row
                        // draw afterward safe: it's appended after a
                        // complete, self-contained update, never spliced
                        // into the middle of one.
                        let diff = current_screen.contents_diff(&prev_screen);
                        let _ = out.write_all(&diff);
                        let _ = draw_toggle_bar(&mut out, tool, &assets_thread);
                        let _ = out.flush();
                        prev_screen = current_screen;
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
            Ok(RunEvent::Resized(new_cols, new_rows)) => {
                let _ = pair.master.resize(PtySize {
                    rows: new_rows.saturating_sub(1),
                    cols: new_cols,
                    pixel_width: 0,
                    pixel_height: 0,
                });
                *dims.lock().unwrap() = (new_cols, new_rows);
                continue;
            }
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

/// Polls the real terminal's size and reports changes as `RunEvent::Resized`.
/// A real terminal resize (SIGWINCH) doesn't otherwise reach us anywhere in
/// this design -- the pty and the vt100 model are both sized once at spawn
/// time and never revisited, so an entirely ordinary thing (the user
/// resizing their terminal window mid-session) silently desyncs our status
/// row's position from where the child's own (still old-sized) content
/// model believes its last row is, producing exactly the interleaved/
/// overlapping corruption this was built to fix. Polling instead of a real
/// SIGWINCH handler because it's portable (Windows has no SIGWINCH) and
/// the ~250ms latency is imperceptible for a terminal resize.
fn spawn_resize_poller(tx: mpsc::Sender<RunEvent>) {
    std::thread::spawn(move || {
        let mut last = terminal::size().unwrap_or((80, 24));
        loop {
            std::thread::sleep(std::time::Duration::from_millis(250));
            match terminal::size() {
                Ok(current) if current != last => {
                    last = current;
                    if tx.send(RunEvent::Resized(current.0, current.1)).is_err() {
                        break; // receiver gone -- program is shutting down
                    }
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
    });
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

#[cfg(test)]
mod vt100_model_tests {
    /// The structural guarantee the whole rewrite depends on: a child's
    /// pty is sized rows-1 tall, so its vt100 screen model can *only* ever
    /// contain content in rows 0..rows-2. There's no escape sequence a
    /// child could send that makes contents_diff emit anything touching
    /// our reserved status row, because that row doesn't exist in the
    /// child's model at all -- not "we filtered it out," structurally
    /// absent.
    #[test]
    fn child_screen_model_cannot_exceed_its_own_row_count() {
        let child_rows = 29u16; // matches rows.saturating_sub(1) for a 30-row terminal
        let cols = 100u16;
        let mut parser = vt100::Parser::new(child_rows, cols, 0);

        // Try to get the child to scroll far past its own screen size, and
        // to explicitly move its cursor to an absurd row -- both are
        // things a real misbehaving or confused child could send.
        for _ in 0..500 {
            parser.process(b"some line of output\r\n");
        }
        parser.process(b"\x1b[9999;1H"); // absolute move to a huge row
        parser.process(b"X");

        let screen = parser.screen();
        let (screen_rows, _) = screen.size();
        assert_eq!(screen_rows, child_rows, "screen model's own size never changes on its own");

        let (cursor_row, _) = screen.cursor_position();
        assert!(
            (cursor_row as usize) < child_rows as usize,
            "cursor position must be clamped inside the model's own bounds, got row {cursor_row}"
        );

        // The diff this screen would produce is only ever addressed within
        // the model's own dimensions -- there is no possible content at
        // child_rows or beyond for it to emit.
        let blank = vt100::Parser::new(child_rows, cols, 0).screen().clone();
        let diff = screen.contents_diff(&blank);
        assert!(!diff.is_empty());
    }
}
