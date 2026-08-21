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

const ALT_UP_LEGACY: &[u8] = b"\x1b[1;3A";
const ALT_DOWN_LEGACY: &[u8] = b"\x1b[1;3B";
const ALT_UP_KITTY: &[u8] = b"\x1b[57419;3u";
const ALT_DOWN_KITTY: &[u8] = b"\x1b[57420;3u";
// Ctrl+R -- opens the search-and-resume overlay from inside a running
// agent. Legacy terminals send it as the single control byte 0x12; Kitty-
// protocol-aware terminals may send the disambiguated CSI-u form instead.
const CTRL_R_LEGACY: &[u8] = b"\x12";
const CTRL_R_KITTY: &[u8] = b"\x1b[114;5u";

fn all_triggers() -> [&'static [u8]; 6] {
    [ALT_UP_LEGACY, ALT_DOWN_LEGACY, ALT_UP_KITTY, ALT_DOWN_KITTY, CTRL_R_LEGACY, CTRL_R_KITTY]
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

    let mut reader = pair.master.try_clone_reader()?;
    let tx_out = tx.clone();
    let assets_thread = assets.clone();
    let suppress_thread = suppress.clone();
    std::thread::spawn(move || {
        let mut out = stdout();
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    // While the search overlay owns the screen, still drain
                    // the child's output (so its pty buffer never fills and
                    // blocks it) but don't paint over the overlay with it.
                    if !suppress_thread.load(Ordering::SeqCst) {
                        let _ = out.write_all(&buf[..n]);
                        let _ = draw_toggle_bar(&mut out, tool, &assets_thread);
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

/// One persistent stdin-reading thread for the whole program lifetime.
/// Detects Alt+Up/Alt+Down and Ctrl+R (legacy CSI and Kitty CSI-u
/// encodings) and signals the corresponding event; forwards everything
/// else to whatever `InputSink` is currently active. A single long-lived
/// reader avoids the correctness bug of two threads racing to read the
/// same stdin fd across hops.
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
                    loop {
                        if pending == ALT_UP_LEGACY || pending == ALT_UP_KITTY {
                            let _ = tx.send(RunEvent::Hop(HopDirection::Prev));
                            pending.clear();
                            break;
                        }
                        if pending == ALT_DOWN_LEGACY || pending == ALT_DOWN_KITTY {
                            let _ = tx.send(RunEvent::Hop(HopDirection::Next));
                            pending.clear();
                            break;
                        }
                        if pending == CTRL_R_LEGACY || pending == CTRL_R_KITTY {
                            let _ = tx.send(RunEvent::SearchResume);
                            pending.clear();
                            break;
                        }
                        if is_prefix_of_any(&pending) {
                            break; // wait for more bytes
                        }
                        // Not a trigger and not a prefix of one -- hand off
                        // to whichever sink is currently active.
                        match &mut *sink.lock().unwrap() {
                            InputSink::Forward(w) => {
                                let _ = w.write_all(&pending);
                                let _ = w.flush();
                            }
                            InputSink::Capture(s) => {
                                let _ = s.send(pending.clone());
                            }
                            InputSink::Idle => {}
                        }
                        pending.clear();
                        break;
                    }
                }
            }
        }
    });
}

fn is_prefix_of_any(buf: &[u8]) -> bool {
    for seq in all_triggers() {
        if seq.starts_with(buf) {
            return true;
        }
    }
    false
}
