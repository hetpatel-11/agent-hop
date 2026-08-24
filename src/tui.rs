use crate::adapters::{self, adapter_for};
use crate::agents::ToolName;
use crate::resume::{self, ChannelKeys, KeySource};
use crate::telemetry;
use crate::theme;
use crate::vt;
use crossterm::{cursor, execute, queue, style::Print, terminal};
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use ratatui::backend::CrosstermBackend;
use ratatui::buffer::Buffer;
use ratatui::style::{Color as RColor, Modifier as RModifier, Style};
use ratatui::text::{Line as RLine, Span};
use ratatui::Terminal as RTerminal;
use std::io::{stdout, Read, Stdout, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};

/// Resolves a `vt::Cell`'s already-libghostty-resolved color to the
/// ratatui color that reproduces it. `None` (the cell has no explicit
/// color) maps to `Color::Reset` -- letting the *real* terminal's own
/// theme apply is the correct, native behavior for a cell using the
/// default color, not a guess at what that default happens to be.
fn resolve_color(color: Option<vt::CellColor>) -> RColor {
    match color {
        Some(vt::CellColor::Rgb(rgb)) => RColor::Rgb(rgb.r, rgb.g, rgb.b),
        Some(vt::CellColor::Indexed(i)) => RColor::Indexed(i),
        None => RColor::Reset,
    }
}

fn cell_modifier(cell: &vt::Cell) -> RModifier {
    let mut m = RModifier::empty();
    if cell.bold {
        m.insert(RModifier::BOLD);
    }
    if cell.faint {
        m.insert(RModifier::DIM);
    }
    if cell.italic {
        m.insert(RModifier::ITALIC);
    }
    if cell.underline {
        m.insert(RModifier::UNDERLINED);
    }
    if cell.inverse {
        m.insert(RModifier::REVERSED);
    }
    if cell.hidden {
        m.insert(RModifier::HIDDEN);
    }
    if cell.strikethrough {
        m.insert(RModifier::CROSSED_OUT);
    }
    m
}

/// Forensic capture mode, opt-in via `AH_DEBUG_LOG=/path/to/file`. Every
/// bug in this rendering pipeline so far has been diagnosed by inspecting
/// exact real bytes -- and every synthetic reproduction I can build in a
/// sandbox (a scripted pty plus either raw byte inspection or the `pyte`
/// Python library) is a different program from a real terminal like
/// Ghostty: it doesn't answer terminal capability queries the way Ghostty
/// does, doesn't do real Kitty graphics/keyboard-protocol negotiation, and
/// isn't guaranteed to match Ghostty's exact VT parsing on edge cases.
/// That gap is *why* fixes that pass every synthetic test can still miss
/// the real bug. This logs every byte in every direction -- what the
/// child actually sent, what we actually wrote to the real terminal, and
/// crucially what the real terminal sent back to us on stdin (responses,
/// resize-related signals reflected in input, anything) -- so a real
/// session's exact behavior can be inspected directly instead of guessed
/// at through an imperfect stand-in.
fn debug_log_path() -> Option<std::path::PathBuf> {
    std::env::var_os("AH_DEBUG_LOG").map(std::path::PathBuf::from)
}

fn debug_log(tag: &str, data: &[u8]) {
    let Some(path) = debug_log_path() else { return };
    let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) else { return };
    let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    let escaped: String = data
        .iter()
        .map(|&b| {
            if b == b'\\' {
                "\\\\".to_string()
            } else if (0x20..0x7f).contains(&b) {
                (b as char).to_string()
            } else {
                format!("\\x{b:02x}")
            }
        })
        .collect();
    let _ = writeln!(f, "[{ts}] {tag} ({} bytes): {escaped}", data.len());
}

enum HopDirection {
    Next,
    Prev,
}

enum RunEvent {
    ChildExited(u64),
    Hop(HopDirection),
    SearchResume,
    /// A left-click landed on the bottom toggle bar (see
    /// `MouseDecode::OpenAgentPicker`) -- open the same agent list
    /// Alt+Up/Down cycles through, so a user can jump straight to a
    /// specific agent instead of only stepping next/prev.
    AgentPicker,
    /// `PREFIX_KEY` then `?` -- matches herdr's own `prefix+?` convention
    /// exactly (its docs: "Press prefix+? at any time to see every active
    /// binding"). Shows every ah shortcut in one place, since none of them
    /// are otherwise discoverable except by reading the toggle bar's own
    /// (necessarily terse) hint text.
    ShowHelp,
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
    /// The child exited on its own (user quit the agent normally). Skips
    /// the explicit `child.kill()` below on the way out -- it's already
    /// gone.
    Exited,
    /// Ctrl+C pressed inside the search-and-resume overlay -- a real,
    /// hard quit of the whole program (see `resume::SearchKey::Quit`).
    /// Deliberately a separate variant from `Exited`, not reusing it: the
    /// agent underneath the overlay is still very much alive at this
    /// point (the overlay only ever suppresses its output, never touches
    /// the process), so this must still go through the explicit
    /// `child.kill()` below, unlike a genuine self-exit.
    Quit,
    /// Alt+Up/Down was pressed -- translate the live conversation into the
    /// next/prev installed agent's format and continue there.
    Hop(HopDirection),
    /// A specific agent was picked from the agent-list overlay (see
    /// `RunEvent::AgentPicker`) -- same live-conversation translation as
    /// `Hop`, just jumping straight to a chosen tool instead of
    /// stepping next/prev through the installed list.
    HopTo(ToolName),
    /// The user picked a session from the in-TUI search overlay -- jump
    /// straight to it (that tool's own native resume, no translation).
    ResumeInto { tool: ToolName, session_id: String, project_path: String },
}

/// Single-pane TUI shell: one agent's real pty rendered full-pane, with a
/// persistent toggle strip (bottom row, owned by us, agent never draws into
/// it) for switching between installed agents via Alt+Up/Down, and a
/// search-and-resume overlay on Ctrl+R.
pub async fn run(initial: ToolName, initial_launch: Option<(String, String)>) -> anyhow::Result<()> {
    let sink: Arc<Mutex<InputSink>> = Arc::new(Mutex::new(InputSink::Idle));
    let mouse_sink: Arc<Mutex<Option<mpsc::Sender<ChildMsg>>>> = Arc::new(Mutex::new(None));
    // Set only while `run_agent_picker` is open (see its own doc comment
    // and `PickerEvent`'s) -- takes priority over `mouse_sink` in the
    // stdin relay so a click on the picker's own box is never mistaken
    // for a click meant for the child agent still running underneath it.
    let overlay_click_sink: Arc<Mutex<Option<mpsc::Sender<(u16, u16)>>>> = Arc::new(Mutex::new(None));
    let (tx, rx) = mpsc::channel::<RunEvent>();
    let generation = Arc::new(AtomicU64::new(0));

    spawn_stdin_relay(sink.clone(), mouse_sink.clone(), overlay_click_sink.clone(), tx.clone());
    spawn_resize_poller(tx.clone());
    // See its own doc comment: without this, the *first* hop into OpenCode
    // in any given `ah` process still pays ~1.4-1.8s (a real subprocess
    // call), even though every hop after that is ~2ms. Kicking it off here
    // means it's very likely already warm by the time the user actually
    // gets around to hopping, since starting and using their initial agent
    // takes far longer than this does in the background.
    crate::adapters::opencode::prewarm_export_template_cache();

    let mut current = initial;
    let (mut project_path, mut launch) = match initial_launch {
        Some((session_id, path)) => (resolve_project_path(path, true), Launch::Resume(session_id)),
        None => (
            std::env::current_dir().map(|p| p.to_string_lossy().to_string()).unwrap_or_else(|_| ".".to_string()),
            Launch::Fresh,
        ),
    };

    terminal::enable_raw_mode()?;
    // Every full-screen terminal app -- vim, htop, Codex/Claude/opencode
    // themselves, every ratatui example -- runs inside the alternate
    // screen buffer, and this one was the one exception: without entering
    // it, `render_frame`'s repeated full-screen redraws happen directly in
    // the *primary* buffer, the same one the real terminal's native
    // scrollback is built from. Confirmed live: scrolling up in the real
    // terminal during a session showed blank rows, not history -- each
    // redraw was overwriting the same on-screen rows in place, and rows
    // that came out blank in a given frame contributed blank lines to
    // scrollback rather than leaving whatever was there before untouched.
    // Entering the alternate screen here (and leaving it on the way out,
    // below) gives this program its own separate buffer to draw in,
    // exactly like every other full-screen app -- the user's actual shell
    // scrollback from before launch is left alone and reappears untouched
    // once this returns.
    let host_colors = query_host_colors(&sink);

    execute!(stdout(), terminal::EnterAlternateScreen).ok();
    let _ = stdout().write_all(MOUSE_CAPTURE_ENABLE);
    let _ = stdout().flush();
    let splash_start = std::time::Instant::now();
    let _ = draw_transition_splash(&mut stdout(), "Launching", current);
    if let Some(remaining) = SPLASH_MIN_DURATION.checked_sub(splash_start.elapsed()) {
        std::thread::sleep(remaining);
    }
    execute!(stdout(), cursor::Show).ok();

    let result: anyhow::Result<()> = loop {
        let generation_id = generation.fetch_add(1, Ordering::SeqCst) + 1;
        match run_one(current, &project_path, launch, &sink, &mouse_sink, &overlay_click_sink, &tx, &rx, generation_id, host_colors) {
            Ok(RunOutcome::Exited) | Ok(RunOutcome::Quit) => break Ok(()),
            Ok(RunOutcome::Hop(dir)) => {
                let via = match dir {
                    HopDirection::Next => "next",
                    HopDirection::Prev => "prev",
                };
                let next = match dir {
                    HopDirection::Next => next_installed(current, 1),
                    HopDirection::Prev => next_installed(current, -1),
                };
                launch = hop_to(current, next, &project_path, &rx);
                telemetry::capture(
                    "hop",
                    serde_json::json!({
                        "from": current.slug(),
                        "to": next.slug(),
                        "via": via,
                        "converted": matches!(launch, Launch::Resume(_)),
                    }),
                );
                current = next;
            }
            Ok(RunOutcome::HopTo(next)) => {
                launch = hop_to(current, next, &project_path, &rx);
                telemetry::capture(
                    "hop",
                    serde_json::json!({
                        "from": current.slug(),
                        "to": next.slug(),
                        "via": "picker",
                        "converted": matches!(launch, Launch::Resume(_)),
                    }),
                );
                current = next;
            }
            Ok(RunOutcome::ResumeInto { tool, session_id, project_path: new_path }) => {
                telemetry::capture(
                    "resume",
                    serde_json::json!({
                        "from": current.slug(),
                        "to": tool.slug(),
                        "same_agent": current == tool,
                        "via": "overlay",
                        "interactive": true,
                    }),
                );
                current = tool;
                // Mid-TUI (raw mode already active) -- no visible warning
                // here, just a silent, safe fallback plus a debug-log
                // breadcrumb. A plain eprintln! would corrupt the display
                // (raw mode doesn't do \n -> \r\n translation), and this is
                // exactly the same "don't crash on a missing directory"
                // safety net as the initial-launch case, just without
                // anywhere clean to print to right now.
                project_path = resolve_project_path(new_path, false);
                launch = Launch::Resume(session_id);
                let splash_start = std::time::Instant::now();
                let _ = draw_transition_splash(&mut stdout(), "Resuming in", current);
                if let Some(remaining) = SPLASH_MIN_DURATION.checked_sub(splash_start.elapsed()) {
                    std::thread::sleep(remaining);
                }
                execute!(stdout(), cursor::Show).ok();
            }
            Err(e) => break Err(e),
        }
    };

    let _ = stdout().write_all(MOUSE_CAPTURE_DISABLE);
    let _ = stdout().flush();
    execute!(stdout(), terminal::LeaveAlternateScreen).ok();
    terminal::disable_raw_mode()?;
    result
}

/// Shared by both hop paths -- Alt+Up/Down stepping next/prev, and picking
/// a specific agent from the click-to-open list (see `RunOutcome::HopTo`).
/// Draws the transition splash, translates the live conversation into
/// `next`'s format on a background thread while still servicing `rx` (see
/// the long-standing comment this used to carry inline, preserved in spirit
/// here: translation can take anywhere from milliseconds to several
/// seconds, and not draining `rx` during that window is what let a second
/// hop keypress queue up and fire late, looking like a phantom extra hop),
/// then returns the `Launch` the caller should hand to the next `run_one`.
fn hop_to(current: ToolName, next: ToolName, project_path: &str, rx: &mpsc::Receiver<RunEvent>) -> Launch {
    let splash_start = std::time::Instant::now();
    let _ = draw_transition_splash(&mut stdout(), "Switching to", next);

    let (result_tx, result_rx) = mpsc::channel();
    let path_for_thread = project_path.to_string();
    std::thread::spawn(move || {
        let result = translate_session(current, next, &path_for_thread).unwrap_or(Launch::Fresh);
        let _ = result_tx.send(result);
    });
    let launch = loop {
        match result_rx.try_recv() {
            Ok(l) => break l,
            Err(mpsc::TryRecvError::Disconnected) => break Launch::Fresh,
            Err(mpsc::TryRecvError::Empty) => {}
        }
        // Draining `rx` here (rather than leaving it unread) is the actual
        // point -- a resize during this window doesn't need to be applied
        // to anything right now (there's no live child pty until the next
        // run_one starts, and that call queries the real terminal size
        // fresh at that point anyway), it just needs to not sit queued
        // until this hop lands.
        let _ = rx.recv_timeout(std::time::Duration::from_millis(50));
    };
    if let Some(remaining) = SPLASH_MIN_DURATION.checked_sub(splash_start.elapsed()) {
        std::thread::sleep(remaining);
    }
    execute!(stdout(), cursor::Show).ok();
    launch
}

/// Reads whatever `from` was just running (the most recent session for
/// this project path), and writes it into `to`'s own format. Returns
/// `None` (caller falls back to a fresh launch) if there's no matching
/// session, or if the read/write translation itself fails -- resilience
/// matters more than perfection here; a failed hop should degrade to
/// "start fresh in the next agent," not crash the whole switcher.
fn translate_session(from: ToolName, to: ToolName, project_path: &str) -> Option<Launch> {
    let t0 = std::time::Instant::now();
    // The source agent writes its own session file asynchronously -- we've
    // confirmed directly (both through this tool and by scripting the raw
    // agent CLI on its own, no wrapper involved) that at least one agent
    // (codex) can still have nothing on disk for a brand-new session
    // several seconds after a completed exchange. A short bounded retry
    // closes the narrow race where that write is already in flight at the
    // moment we look for it, without meaningfully slowing down the common
    // case where it's already there (the first attempt succeeds
    // immediately, so this costs nothing then). It can't do anything about
    // an agent that simply hasn't started persisting yet by design -- that
    // case still degrades to a fresh start in the target agent, same as
    // before, rather than hopping forever.
    let mut session_ref = adapters::find_latest_session_for_path(from, project_path);
    for _ in 0..8 {
        if session_ref.is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(120));
        session_ref = adapters::find_latest_session_for_path(from, project_path);
    }
    let session_ref = session_ref?;
    debug_log("TIMING_FIND_SESSION", format!("{:?}", t0.elapsed()).as_bytes());
    let t1 = std::time::Instant::now();
    let new_id = adapters::convert_session(&session_ref, to, project_path).ok()?;
    debug_log("TIMING_CONVERT", format!("{:?}", t1.elapsed()).as_bytes());
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

/// Falls back to the user's home directory if a session's originally
/// recorded project path no longer exists (moved or deleted since) --
/// spawning a child process with a nonexistent cwd fails with a raw ENOENT
/// that reads like "command not found," not "directory missing," a real,
/// confirmed source of a confusing crash. `warn_visibly` should only be
/// true when called before raw mode is active (a plain eprintln! would
/// corrupt the display once the TUI has taken over the screen, since raw
/// mode doesn't do \n -> \r\n translation) -- see call sites.
fn resolve_project_path(path: String, warn_visibly: bool) -> String {
    if std::path::Path::new(&path).exists() {
        return path;
    }
    let home = dirs::home_dir().map(|p| p.to_string_lossy().to_string()).unwrap_or_else(|| ".".to_string());
    if warn_visibly {
        eprintln!("agent-hop: original project directory no longer exists: {path}\nResuming in {home} instead.");
    }
    debug_log("PROJECT_PATH_FALLBACK", format!("{path} -> {home}").as_bytes());
    home
}

/// Minimum time the transition splash stays visible -- long enough to
/// actually register as a deliberate branding moment, not just a flash.
/// Previously a hop went straight from the old agent's screen to a blank
/// clear to the new agent's own splash, with nothing of ours in between --
/// confirmed in practice as "not apparent which agent you're switching
/// to." This runs concurrently with whatever translate_session/spawn work
/// is already happening, not serially after it -- see call sites -- so it
/// only ever adds latency on the (common) case where that work finishes
/// faster than this floor.
const SPLASH_MIN_DURATION: std::time::Duration = std::time::Duration::from_millis(550);

/// Rows of `ah`'s own chrome around the child agent's screen: a 3-row
/// block-letter "AH" logo strip pinned to the very top (see
/// `write_top_bar`) plus the existing toggle bar pinned to the bottom (see
/// `write_toggle_bar`). Both are drawn by us, never by the child -- the
/// child's own pty is sized `rows - CHROME_ROWS` tall and its content is
/// offset down by `TOP_BAR_ROWS` when composited into the real frame, so
/// there's no coordinate a misbehaving child could send that reaches either
/// row (see `child_screen_model_cannot_exceed_its_own_row_count`). A
/// multi-row top strip (not one line of text) is what actually reads as a
/// logo mark rather than a caption -- the bottom bar already carries the
/// tool name and keybinding hints, so the top strip carries nothing but the
/// brand itself.
const TOP_BAR_ROWS: u16 = 3;
const CHROME_ROWS: u16 = TOP_BAR_ROWS + 1;

/// ah's own tmux/herdr-style prefix key -- Ctrl+B (0x02), matching herdr's
/// own default exactly. Alt+Up/Down and clicking the toggle bar both
/// already switch agents, but neither is universal: Alt+Up/Down depends on
/// the terminal transmitting a CSI modifier code for the held Option/Alt
/// key, which macOS Terminal.app's default configuration simply doesn't do
/// (confirmed live: Option+Up there sends the exact same bytes as a plain,
/// unmodified Up arrow, silently dropping the fact that Option was ever
/// held), and mouse clicks require SGR mouse support, which isn't
/// guaranteed either (headless/restricted terminals, some SSH setups). A
/// raw single-byte ASCII control code is the one thing every terminal on
/// every OS is guaranteed to transmit identically -- no CSI negotiation,
/// no modifier-key ambiguity, no mouse dependency. Checked directly against
/// all five target agents' own keybindings (Codex's own `/keymap`, Claude
/// Code's and OpenCode's official docs, and live-tested in Pi and Grok)
/// before picking this: Ctrl+B itself does collide with something in each
/// of Codex/Claude Code/OpenCode, but as the *prefix* key that only matters
/// once -- everything chorded after it (see the `n`/`p`/`a` match in
/// `spawn_stdin_relay`) needs no further collision-checking at all, since a
/// two-key sequence starting with an arbitrary prefix byte is never going
/// to collide with anything a child agent binds on its own.
const PREFIX_KEY: u8 = 0x02;

/// How long to wait for a chord key after `PREFIX_KEY`, before giving up
/// and treating it as a bare, unbound keypress (silently absorbed -- see
/// its own doc comment at the call site for why not forwarded). Generous
/// relative to the 50ms used to disambiguate a lone ESC, since a user
/// consciously reaching for a two-key chord is deliberately pausing between
/// the two presses far more than the "did more bytes arrive as part of the
/// same escape sequence" question ESC disambiguation is answering.
const PREFIX_CHORD_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(600);

/// Deliberately hand-written instead of crossterm's own `EnableMouseCapture`
/// -- that command unconditionally bundles mode 1003 (`?1003h`, "any-event"
/// tracking) in with the modes this actually needs. Mode 1003 makes the
/// real terminal report *every* mouse movement over the window, not just
/// clicks and scroll-wheel presses, as its own SGR sequence -- confirmed
/// live as the actual root cause of choppy-feeling scroll: those motion
/// reports flow through the exact same single-threaded channel as the
/// child's own real content updates (see `ChildMsg`), so a burst of
/// mouse-move noise (trivial cursor motion during or around a scroll
/// gesture) competed with and delayed the genuine scroll-wheel events and
/// redraws for visible content. Nothing this program does needs continuous
/// motion tracking -- click-to-select (the agent picker) and scroll
/// forwarding both only ever care about button presses, which mode 1000
/// (`?1000h`, click tracking) already reports on its own, including scroll
/// wheel presses (buttons 4/5 use the same encoding as buttons 1-3 per
/// xterm's own mouse-tracking spec). `?1006h` is SGR extended-coordinate
/// mode, needed regardless of which tracking mode is active so
/// coordinates past 223 columns/rows don't wrap.
const MOUSE_CAPTURE_ENABLE: &[u8] = b"\x1b[?1000h\x1b[?1006h";
const MOUSE_CAPTURE_DISABLE: &[u8] = b"\x1b[?1006l\x1b[?1000l";

/// Full-screen branded transition splash: the block-letter "agent hop"
/// wordmark (falls back to a compact form below its width) plus which
/// agent is about to take over, in that agent's own color -- the same
/// "you're being handed off to something with its own identity" moment
/// opencode/Claude Code themselves open with, but establishing *our*
/// brand as the thing doing the handing-off.
fn draw_transition_splash(out: &mut impl Write, verb: &str, tool: ToolName) -> anyhow::Result<()> {
    let (cols, rows) = terminal::size()?;
    // `cursor::MoveTo(0, 0)` in the same batch as `Hide` is deliberate, not
    // just belt-and-suspenders: `Hide` stops the cursor from *blinking*,
    // but doesn't relocate it -- it stays wherever the just-exited agent
    // last left it. Confirmed live as a real, visible glitch: a hidden
    // cursor can still cause a one-frame flash of misplaced/deformed
    // content right as the terminal repaints, if the real terminal's own
    // redraw happens to race with `Hide` taking effect. Parking it at the
    // origin removes any chance of that flash landing somewhere that looks
    // wrong, regardless of timing.
    execute!(out, terminal::Clear(terminal::ClearType::All), cursor::Hide, cursor::MoveTo(0, 0))?;

    // A brief brand-cyan bar sweeping top to bottom before the wordmark
    // settles -- gives the transition actual motion instead of one static
    // frame appearing and holding, which is what "switching agents" used
    // to look like. ~100ms total, so it reads as a deliberate flourish, not
    // a delay.
    let sweep_frames = 6u16.min(rows.max(1));
    for i in 0..sweep_frames {
        let row = (u32::from(i) * u32::from(rows) / u32::from(sweep_frames)) as u16;
        queue!(out, cursor::MoveTo(0, row), Print(theme::brand_cyan(&"\u{2588}".repeat(cols as usize))))?;
        out.flush()?;
        std::thread::sleep(std::time::Duration::from_millis(18));
        queue!(out, cursor::MoveTo(0, row), Print(" ".repeat(cols as usize)))?;
    }

    let message = format!("{verb} {}...", tool.display_name());
    let use_big = cols >= theme::BRAND_WORDMARK_WIDTH;

    let wordmark_lines: Vec<&str> = if use_big { theme::BRAND_WORDMARK.trim_matches('\n').lines().collect() } else { Vec::new() };
    let content_height = if use_big { wordmark_lines.len() as u16 + 2 } else { 2 };
    let mut row = rows.saturating_sub(content_height) / 2;

    if use_big {
        // One shared column for every row, computed from the *widest* row --
        // not `line.chars().count()` recomputed per row. The ANSI-Shadow
        // letterforms are ragged on the right (e.g. "P"'s foot has no
        // stroke on its bottom two rows), so several rows are a few
        // characters "shorter" than the others purely because their
        // trailing blank columns aren't part of the string at all, not
        // because the artwork itself is narrower there. Centering each row
        // independently by its own (different) length shifted those
        // shorter rows rightward relative to the rest -- confirmed live as
        // the actual cause of the wordmark looking like its last couple of
        // rows were shifted right during the splash. Every row starts at
        // the same left edge; the raggedness is purely on the right, where
        // it belongs.
        let wordmark_width = wordmark_lines.iter().map(|l| l.chars().count()).max().unwrap_or(0) as u16;
        let wordmark_col = cols.saturating_sub(wordmark_width) / 2;
        for line in &wordmark_lines {
            queue!(out, cursor::MoveTo(wordmark_col, row), Print(theme::bold(&theme::brand_cyan(line))))?;
            row += 1;
        }
    } else {
        let compact = "agent-hop";
        let col = cols.saturating_sub(compact.chars().count() as u16) / 2;
        queue!(out, cursor::MoveTo(col, row), Print(theme::brand()))?;
        row += 1;
    }
    row += 1;
    let col = cols.saturating_sub(message.chars().count() as u16) / 2;
    queue!(out, cursor::MoveTo(col, row), Print(theme::tool_color(tool, &message)))?;
    out.flush()?;
    Ok(())
}
#[allow(clippy::too_many_arguments)]
fn run_one(
    tool: ToolName,
    project_path: &str,
    launch: Launch,
    sink: &Arc<Mutex<InputSink>>,
    mouse_sink: &Arc<Mutex<Option<mpsc::Sender<ChildMsg>>>>,
    overlay_click_sink: &Arc<Mutex<Option<mpsc::Sender<(u16, u16)>>>>,
    tx: &mpsc::Sender<RunEvent>,
    rx: &mpsc::Receiver<RunEvent>,
    generation_id: u64,
    host_colors: (Option<vt::Rgb>, Option<vt::Rgb>),
) -> anyhow::Result<RunOutcome> {
    // Checked here, not just left to fail at spawn time: a resumed-from-
    // search session (`ResumeInto`) can name a tool that was installed
    // when that session was originally recorded but isn't anymore (or
    // never was on this machine -- syncing session history across
    // machines, for instance). Alt+Up/Down hopping is already immune to
    // this (`next_installed` only ever cycles through installed tools),
    // but that guard doesn't cover a session picked directly by name via
    // search. Failing here gives a clean, actionable message instead of a
    // raw spawn ENOENT that looks like "command not found" with no
    // context about which agent or why.
    if !tool.is_installed() {
        anyhow::bail!("Cannot resume in {}: \"{}\" is not installed or not on PATH.", tool.slug(), tool.binary());
    }
    let t_run_one_start = std::time::Instant::now();
    let pty_system = native_pty_system();
    let (cols, rows) = terminal::size()?;
    let pair = pty_system.openpty(PtySize {
        rows: rows.saturating_sub(CHROME_ROWS),
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
    debug_log("TIMING_RUN_ONE_TO_SPAWN", format!("{:?}", t_run_one_start.elapsed()).as_bytes());

    // Shared because both the user's own keystrokes (`forward`, driven by
    // the stdin relay thread) and `EventProxy::send_event` (driven by the
    // render thread below, answering a query the terminal model generated
    // on the child's behalf) write to the same pty -- see `InputSink`'s
    // doc comment for why this can't just be two separate writers.
    let writer: Arc<Mutex<Box<dyn Write + Send>>> = Arc::new(Mutex::new(pair.master.take_writer()?));
    *sink.lock().unwrap() = InputSink::Forward(writer.clone());

    let suppress = Arc::new(AtomicBool::new(false));
    // Resize requests are forwarded here rather than applied where they're
    // received (the main loop below, on the caller's thread). The pty
    // resize (which is what actually sends the child a real SIGWINCH) and
    // the vt100 model's own resize+redraw used to happen on *different*
    // threads -- the main thread resized the pty and stashed the new size
    // in a shared `Mutex`, while the parser thread below polled that same
    // Mutex on its own schedule. Two threads independently deciding when
    // to act on "the terminal resized" is exactly what let a size slip
    // through uncoordinated: the child could receive SIGWINCH, redraw for
    // the new size, and have those bytes reach the parser thread *before*
    // that thread had polled and applied the matching model resize --
    // parsing content addressed for one grid against a model still
    // configured for another. Confirmed live as the actual cause of a
    // corrupted, duplicated-looking box during a hop into Codex timed
    // against a rapid resize. The fix, same pattern production PTY
    // multiplexers use (see shpool's `spawn_shell_to_client`, which
    // resizes its own vt100-backed "spool" and the pty from the same
    // single-threaded event loop): one thread -- the parser thread below,
    // via `master` moved into it just after this point -- owns applying a
    // resize to *both* the pty and the model, back to back, so nothing else
    // can ever process a chunk of child output in between.
    let (resize_tx, resize_rx) = mpsc::channel::<(u16, u16)>();

    let mut reader = pair.master.try_clone_reader()?;
    let master = pair.master;
    let tx_out = tx.clone();
    let suppress_thread = suppress.clone();

    // The blocking read is moved onto its own thread and fed through a
    // channel so the processing loop below can use `recv_timeout` instead
    // of a direct blocking `reader.read()`. This matters specifically for
    // resize handling: applying a resize to the vt100 model and redrawing
    // the toggle bar both used to happen *only* as a side effect of
    // processing a chunk of the child's own output -- if the child went
    // idle right after a real resize (confirmed live: codex sitting at an
    // idle prompt), neither ever happened, leaving the model the wrong
    // size and the toggle bar undrawn indefinitely, until the child
    // happened to print something on its own. Waking up periodically even
    // with no child data lets a pending resize get applied promptly
    // regardless of what the child is doing.
    let (byte_tx, byte_rx) = mpsc::channel::<ChildMsg>();
    let child_byte_tx = byte_tx.clone();
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => {
                    // Explicit EOF signal, not a bare `break` relying on
                    // `child_byte_tx` dropping to disconnect `byte_rx` --
                    // confirmed live as a real deadlock: `byte_rx` is a
                    // *shared* channel (see `mouse_sink`'s doc comment,
                    // just below), and `mouse_sink` holds its own
                    // long-lived clone of `byte_tx` that isn't cleared
                    // until *after* the render loop below has already
                    // broken and sent `RunEvent::ChildExited`. Waiting for
                    // the channel to fully disconnect therefore never
                    // happens on its own: this thread's sender dropping
                    // isn't enough while that other clone is still alive,
                    // and that other clone doesn't go away until the very
                    // event this was supposed to produce. Sending an
                    // explicit message through the channel instead of
                    // relying on its lifecycle sidesteps that circular
                    // wait entirely, regardless of how many other senders
                    // exist.
                    let _ = child_byte_tx.send(ChildMsg::Eof);
                    break;
                }
                Ok(n) => {
                    if child_byte_tx.send(ChildMsg::Bytes(buf[..n].to_vec())).is_err() {
                        break;
                    }
                }
            }
        }
    });

    // Routes decoded mouse events from the stdin relay thread (see
    // `parse_sgr_mouse`) to this specific child's render thread, which owns
    // the only `vt::Terminal` that knows this child's own negotiated mouse
    // tracking mode/format -- swapped in lockstep with `sink` in
    // `MouseSink`, mirroring exactly how `InputSink` already routes raw
    // keystrokes to whichever child is currently active.
    *mouse_sink.lock().unwrap() = Some(byte_tx.clone());

    let vt_writer = writer.clone();
    let writer_for_mouse = writer.clone();
    let (host_fg, host_bg) = host_colors;
    std::thread::spawn(move || {
        // The child's raw bytes are never written to the real terminal
        // directly -- they're parsed into a real terminal-emulator model
        // (`vt::Terminal`, wrapping Ghostty's own embeddable engine,
        // libghostty-vt) and *that* model is what gets rendered, via
        // `ratatui`, every frame. This is deliberately not a hand-rolled
        // byte-diffing layer: neither the original `vt100`-based design
        // nor an earlier `alacritty_terminal`-based one could fully
        // reproduce what a real terminal shows -- gaps between a
        // reimplementation's coverage and what a real terminal actually
        // implements (synchronized-update mode, OSC 133 semantic prompt
        // marking, full color/attribute semantics, wide-character
        // handling) were each confirmed live as real corruption/fidelity
        // bugs. Using Ghostty's own engine means anything Ghostty itself
        // understands, this understands too -- the same reasoning herdr
        // (a real, shipping AI-agent terminal multiplexer) documented in
        // its own CHANGELOG when it migrated off a `vt100` backend for the
        // same class of bug. `ratatui`'s own `Terminal` owns the actual
        // diff-and-emit step (the same battle-tested code path used by
        // every ratatui app), so nothing in this file re-implements
        // terminal *rendering* by hand -- only the read side, from
        // libghostty-vt's own render-state API.
        let mut applied_dims = (cols, rows);
        let mut term = vt::Terminal::with_host_colors(cols, rows.saturating_sub(CHROME_ROWS), vt_writer, host_fg, host_bg);

        let Ok(mut rterm) = RTerminal::new(CrosstermBackend::new(stdout())) else {
            let _ = tx_out.send(RunEvent::ChildExited(generation_id));
            return;
        };
        let _ = rterm.clear();
        if let Err(e) = render_frame(&mut rterm, &mut term, tool) {
            debug_log("RENDER_FRAME_ERR", format!("{e:#}").as_bytes());
        }

        // How often the loop wakes up even with no child data, purely to
        // check for a pending resize -- without this, a resize applied
        // right as the child goes idle (confirmed live: Codex sitting at
        // an idle prompt) wouldn't be reflected on screen until the child
        // happened to print something on its own.
        const IDLE_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(120);

        let apply_resize_if_pending = |applied_dims: &mut (u16, u16), term: &mut vt::Terminal| {
            // Drain the whole backlog and keep only the newest -- if
            // several sizes queued up while this thread was busy, every
            // one but the last is already stale by definition.
            let mut current_dims = *applied_dims;
            while let Ok(size) = resize_rx.try_recv() {
                current_dims = size;
            }
            if current_dims != *applied_dims {
                debug_log("RESIZE_APPLIED_TO_MODEL", format!("{applied_dims:?} -> {current_dims:?}").as_bytes());
                *applied_dims = current_dims;
                let new_child_rows = current_dims.1.saturating_sub(CHROME_ROWS);
                // The pty resize (which is what actually delivers SIGWINCH
                // to the child) and the model resize below happen back to
                // back on this one thread, with nothing else able to run
                // in between -- see the comment where `resize_tx` is
                // created for why that ordering is the actual fix. Ratatui
                // itself doesn't need a matching manual resize call here:
                // `Terminal::draw` re-queries the real terminal size and
                // resizes its own buffers automatically before every frame.
                let _ = master.resize(PtySize {
                    rows: new_child_rows,
                    cols: current_dims.0,
                    pixel_width: 0,
                    pixel_height: 0,
                });
                term.resize(current_dims.0, new_child_rows);
            }
            current_dims
        };

        // Tracks whether the search-and-resume overlay owned the screen on
        // the *previous* iteration, so a true -> false transition can be
        // detected below.
        let mut was_suppressed = false;

        loop {
            // The overlay draws directly over the whole real terminal (see
            // `run_search_overlay`) without going through `rterm` at all,
            // so ratatui's own internal "what does the real terminal
            // currently show" record is stale by the time it hands the
            // screen back. `Terminal::clear()` is ratatui's own API for
            // exactly this situation: it clears the real terminal *and*
            // resets ratatui's back buffer, guaranteeing the next `draw`
            // is a full repaint rather than a diff against a screen that
            // no longer reflects reality.
            let is_suppressed = suppress_thread.load(Ordering::SeqCst);
            if was_suppressed && !is_suppressed {
                let _ = rterm.clear();
                if let Err(e) = render_frame(&mut rterm, &mut term, tool) {
                    debug_log("RENDER_FRAME_ERR", format!("{e:#}").as_bytes());
                }
            }
            was_suppressed = is_suppressed;

            match byte_rx.recv_timeout(IDLE_POLL_INTERVAL) {
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    let before = applied_dims;
                    apply_resize_if_pending(&mut applied_dims, &mut term);
                    if applied_dims != before && !suppress_thread.load(Ordering::SeqCst) {
                        if let Err(e) = render_frame(&mut rterm, &mut term, tool) {
                            debug_log("RENDER_FRAME_ERR", format!("{e:#}").as_bytes());
                        }
                    }
                }
                Ok(ChildMsg::Bytes(data)) => {
                    debug_log("CHILD_RAW", &data);
                    apply_resize_if_pending(&mut applied_dims, &mut term);
                    term.write(&data);
                    // While the search overlay owns the screen, still drain
                    // the child's output (so its pty buffer never fills and
                    // blocks it) but don't paint over the overlay with it.
                    if !suppress_thread.load(Ordering::SeqCst) {
                        if let Err(e) = render_frame(&mut rterm, &mut term, tool) {
                            debug_log("RENDER_FRAME_ERR", format!("{e:#}").as_bytes());
                        }
                    }
                }
                // Mouse events pass through only if this specific child
                // negotiated some mouse tracking mode -- `encode_mouse`
                // returns `None` (nothing written) for an agent that never
                // asked, same as a real terminal would never send one.
                Ok(ChildMsg::Mouse { x, y, input, mods }) => {
                    if let Some(bytes) = term.encode_mouse(x, y, input, mods) {
                        let mut w = writer_for_mouse.lock().unwrap();
                        let _ = w.write_all(&bytes);
                        let _ = w.flush();
                    }
                }
                Ok(ChildMsg::Eof) => break,
            }
        }
        let _ = tx_out.send(RunEvent::ChildExited(generation_id));
    });

    // No initial render call here: the render thread above owns the one
    // and only `ratatui::Terminal` instance writing to stdout, and draws
    // the first frame (child content plus the toggle bar) itself before
    // this function ever reaches this point. A second, independent write
    // to stdout from here would be exactly the kind of unsynchronized
    // cross-thread write that corrupted the bar in the old design -- see
    // `render_frame`'s doc comment for why that whole class of bug is now
    // structurally impossible instead of just avoided.

    let outcome = loop {
        match rx.recv() {
            Ok(RunEvent::ChildExited(g)) if g == generation_id => break RunOutcome::Exited,
            Ok(RunEvent::ChildExited(_)) => continue, // stale event from a prior killed child
            Ok(RunEvent::Hop(dir)) => break RunOutcome::Hop(dir),
            Ok(RunEvent::Resized(new_cols, new_rows)) => {
                debug_log("MAIN_THREAD_RESIZE_RECEIVED", format!("{new_cols}x{new_rows}").as_bytes());
                // Only forwarded here, never applied -- the parser thread
                // is the sole owner of both the pty resize and the model
                // resize (see the comment on `resize_tx`'s creation above).
                let _ = resize_tx.send((new_cols, new_rows));
                continue;
            }
            Ok(RunEvent::SearchResume) => {
                match run_search_overlay(sink, &suppress) {
                    resume::ResumeOutcome::Resume(selected) => {
                        break RunOutcome::ResumeInto {
                            tool: selected.tool,
                            session_id: selected.session_id,
                            project_path: selected.project_path,
                        }
                    }
                    resume::ResumeOutcome::Cancelled => {
                        crate::telemetry::capture("search_cancelled", serde_json::json!({ "via": "overlay" }));
                        // Every overlay leaves `sink` parked on
                        // `InputSink::Capture` (see its own doc comment on
                        // why it can't restore this itself). Whenever we're
                        // about to keep running the *same* child rather
                        // than replacing it, that capture state has to be
                        // explicitly swapped back to `Forward` here --
                        // confirmed live as a real bug: without this,
                        // every keystroke typed after cancelling an
                        // overlay silently vanished (routed into a
                        // channel whose receiver had already been dropped
                        // along with the overlay's own local state)
                        // instead of ever reaching the child again.
                        *sink.lock().unwrap() = InputSink::Forward(writer.clone());
                        continue;
                    }
                    resume::ResumeOutcome::Quit => break RunOutcome::Quit,
                }
            }
            Ok(RunEvent::AgentPicker) => match run_agent_picker(sink, overlay_click_sink, &suppress, tool) {
                Some(picked) if picked != tool => break RunOutcome::HopTo(picked),
                _ => {
                    // See the matching comment on `SearchResume`'s
                    // `Cancelled` arm above -- same restoration, same
                    // reason, for the "cancelled or re-picked the agent
                    // already running" case here.
                    *sink.lock().unwrap() = InputSink::Forward(writer.clone());
                    continue;
                }
            },
            Ok(RunEvent::ShowHelp) => {
                run_help_overlay(sink, &suppress);
                *sink.lock().unwrap() = InputSink::Forward(writer.clone());
                continue; // always returns to this same running child
            }
            Err(_) => break RunOutcome::Exited,
        }
    };

    *sink.lock().unwrap() = InputSink::Idle;
    *mouse_sink.lock().unwrap() = None;

    let t_kill = std::time::Instant::now();
    if !matches!(outcome, RunOutcome::Exited) {
        let _ = child.kill();
    }
    let _ = child.wait();
    debug_log("TIMING_CHILD_KILL_WAIT", format!("{:?}", t_kill.elapsed()).as_bytes());

    Ok(outcome)
}

/// Pauses the current child's output, takes over the screen with the
/// search-and-resume UI (fed keys via the same persistent stdin relay
/// thread, switched into Capture mode), then hands the screen back. The
/// child process itself is never touched here -- only its *output* is
/// suppressed -- so cancelling returns to exactly where the conversation
/// was.
fn run_search_overlay(sink: &Arc<Mutex<InputSink>>, suppress: &Arc<AtomicBool>) -> resume::ResumeOutcome {
    suppress.store(true, Ordering::SeqCst);
    let (key_tx, key_rx) = mpsc::channel::<Vec<u8>>();
    *sink.lock().unwrap() = InputSink::Capture(key_tx);

    let sessions = crate::search::collect_sessions(&ToolName::ALL);
    let mut keys = ChannelKeys::new(key_rx);
    let mut out = stdout();
    let result = resume::run_resume_ui(sessions, "", &mut keys, &mut out);

    suppress.store(false, Ordering::SeqCst);
    // The render thread's own `was_suppressed && !is_suppressed` check
    // (see `run_one`) is what actually repaints the screen once this
    // returns, via `ratatui::Terminal::clear()` -- nothing to do here.
    // Deliberately never killed or paused the child process, only its
    // screen output, so there's nothing to resume either.

    result.unwrap_or(resume::ResumeOutcome::Cancelled)
}

/// Screen geometry of the agent-picker's centered box, shared by the
/// drawing code and the click hit-test below so the two can never drift
/// apart -- if they were computed separately, a click could land one row
/// off from what's actually drawn there.
struct PickerGeometry {
    start_col: u16,
    start_row: u16,
    box_width: u16,
    box_height: u16,
}

fn picker_geometry(installed: &[ToolName], cols: u16, rows: u16) -> PickerGeometry {
    let box_width: u16 = installed.iter().map(|t| t.display_name().chars().count() as u16).max().unwrap_or(10) + 8;
    let box_height = installed.len() as u16 + 2;
    PickerGeometry { start_col: cols.saturating_sub(box_width) / 2, start_row: rows.saturating_sub(box_height) / 2, box_width, box_height }
}

/// Opened by a click on the bottom toggle bar (see `RunEvent::AgentPicker`)
/// -- the same installed-agent list Alt+Up/Down cycles through, but as a
/// pick-directly list instead of only stepping next/prev. Returns the
/// chosen tool, or `None` if cancelled (Esc/Ctrl+C) or nothing is
/// installed. Reuses the search overlay's `InputSink::Capture` +
/// `ChannelKeys` plumbing (see `run_search_overlay`) since it's the same
/// "temporarily steal keyboard input, suppress the agent's own screen"
/// pattern, just with a much simpler list instead of a fuzzy search.
///
/// Also accepts a direct click on any row -- registers itself as
/// `overlay_click_sink` for its duration (see that field's own doc
/// comment) so a click lands here instead of being routed toward whatever
/// child agent happens to still be running underneath this overlay. One
/// click both selects *and* confirms that row, matching how every mouse-
/// driven menu works -- there's no reason to make a user click once to
/// highlight and again to confirm when they've already pointed at exactly
/// the row they want.
fn run_agent_picker(sink: &Arc<Mutex<InputSink>>, overlay_click_sink: &Arc<Mutex<Option<mpsc::Sender<(u16, u16)>>>>, suppress: &Arc<AtomicBool>, current: ToolName) -> Option<ToolName> {
    let installed: Vec<ToolName> = ToolName::ALL.into_iter().filter(|t| t.is_installed()).collect();
    // The tool currently running is necessarily installed (it's running),
    // so `installed` can never truly be empty in practice -- but the real
    // version of this edge case, a fresh machine with only one agent ever
    // installed, is entirely plausible: there's nothing else to switch to.
    // Silently doing nothing on a click or `Ctrl+B a` in that case used to
    // be indistinguishable from the feature being broken; a brief message
    // makes it clear this is expected, not a bug.
    if installed.iter().all(|t| *t == current) {
        run_help_style_message(sink, suppress, "No other agents installed", "install another agent to switch to it");
        return None;
    }

    suppress.store(true, Ordering::SeqCst);
    let (key_tx, key_rx) = mpsc::channel::<Vec<u8>>();
    *sink.lock().unwrap() = InputSink::Capture(key_tx);

    // Both event sources -- decoded keystrokes and raw click positions --
    // feed into one shared channel (see `PickerEvent`'s own doc comment
    // for why this is necessary: Rust's std `mpsc` has no native
    // multi-channel select, the same reason `ChildMsg` unifies the
    // render thread's own two input sources).
    let (picker_tx, picker_rx) = mpsc::channel::<PickerEvent>();

    let key_picker_tx = picker_tx.clone();
    std::thread::spawn(move || {
        let mut keys = ChannelKeys::new(key_rx);
        loop {
            match keys.next_key() {
                Ok(Some(k)) => {
                    if key_picker_tx.send(PickerEvent::Key(k)).is_err() {
                        break;
                    }
                }
                Ok(None) | Err(_) => break,
            }
        }
    });

    let (click_tx, click_rx) = mpsc::channel::<(u16, u16)>();
    *overlay_click_sink.lock().unwrap() = Some(click_tx);
    std::thread::spawn(move || {
        while let Ok((x, y)) = click_rx.recv() {
            if picker_tx.send(PickerEvent::Click(x, y)).is_err() {
                break;
            }
        }
    });

    let mut out = stdout();
    let mut selected = installed.iter().position(|t| *t == current).unwrap_or(0);

    let picked = loop {
        if draw_agent_picker(&mut out, &installed, selected).is_err() {
            break None;
        }
        match picker_rx.recv() {
            Ok(PickerEvent::Key(resume::SearchKey::Up)) => {
                selected = if selected == 0 { installed.len() - 1 } else { selected - 1 };
            }
            Ok(PickerEvent::Key(resume::SearchKey::Down)) => {
                selected = (selected + 1) % installed.len();
            }
            Ok(PickerEvent::Key(resume::SearchKey::Enter)) => break installed.get(selected).copied(),
            Ok(PickerEvent::Key(resume::SearchKey::Escape)) | Ok(PickerEvent::Key(resume::SearchKey::Quit)) => break None,
            Ok(PickerEvent::Key(_)) => {}
            Ok(PickerEvent::Click(x, y)) => {
                let Ok((cols, rows)) = terminal::size() else { continue };
                let geo = picker_geometry(&installed, cols, rows);
                let in_box_cols = x > geo.start_col && x < geo.start_col + geo.box_width - 1;
                let row_idx = (y as i32) - (geo.start_row as i32) - 1;
                if in_box_cols && row_idx >= 0 && (row_idx as usize) < installed.len() {
                    break installed.get(row_idx as usize).copied();
                }
                // Click landed outside every row (on the border, or
                // entirely off the box) -- same as clicking outside a
                // real dropdown menu, dismiss without picking anything.
                break None;
            }
            Err(_) => break None,
        }
    };

    *overlay_click_sink.lock().unwrap() = None;
    suppress.store(false, Ordering::SeqCst);
    // Same as `run_search_overlay`: the render thread's own
    // `was_suppressed && !is_suppressed` check repaints the real screen
    // (agent content plus both chrome bars) the moment `suppress` flips
    // back, so there's nothing left to clean up here.
    picked
}

/// Draws the agent-picker overlay: a centered box listing every installed
/// agent, the currently-selected row highlighted in brand cyan with a
/// leading `\u{203a}` marker -- deliberately plain (no fuzzy search, no
/// extra metadata) since its whole job is "glance, pick, done" for a list
/// that's rarely more than four or five entries long.
fn draw_agent_picker(out: &mut impl Write, installed: &[ToolName], selected: usize) -> anyhow::Result<()> {
    let (cols, rows) = terminal::size()?;
    let geo = picker_geometry(installed, cols, rows);
    let (start_col, start_row, box_width, box_height) = (geo.start_col, geo.start_row, geo.box_width, geo.box_height);

    execute!(out, terminal::Clear(terminal::ClearType::All), cursor::Hide, cursor::MoveTo(0, 0))?;

    queue!(out, cursor::MoveTo(start_col, start_row), Print(theme::brand_cyan(&format!("\u{256d}{}\u{256e}", "\u{2500}".repeat(box_width as usize - 2)))))?;
    for (i, tool) in installed.iter().enumerate() {
        let row = start_row + 1 + i as u16;
        let label = format!("{:<width$}", tool.display_name(), width = box_width as usize - 4);
        let line = if i == selected { format!("\u{203a} {label}") } else { format!("  {label}") };
        queue!(out, cursor::MoveTo(start_col, row), Print(theme::brand_cyan("\u{2502}")))?;
        if i == selected {
            queue!(out, Print(theme::bold(&theme::tool_color(*tool, &line))))?;
        } else {
            queue!(out, Print(theme::grey(&line)))?;
        }
        queue!(out, Print(theme::brand_cyan("\u{2502}")))?;
    }
    queue!(out, cursor::MoveTo(start_col, start_row + box_height - 1), Print(theme::brand_cyan(&format!("\u{2570}{}\u{256f}", "\u{2500}".repeat(box_width as usize - 2)))))?;
    out.flush()?;
    Ok(())
}

/// Second step after `Ctrl+R` picks a session: `Resume in which agent?
/// (session is from X)` — lets a session recorded in one tool be resumed
/// in any other installed tool via `adapters::convert_session`, mirroring
/// Every shortcut ah itself binds, in the order shown by the help overlay.
/// `Ctrl+B ?` (matching herdr's own `prefix+?` exactly -- its docs:
/// "Press prefix+? at any time to see every active binding") is the one
/// discoverable way to see this list; the toggle bar's own hint text is
/// necessarily terse and can't fit all of it.
const HELP_LINES: &[(&str, &str)] = &[
    ("Ctrl+B then n", "Switch to next installed agent"),
    ("Ctrl+B then p", "Switch to previous installed agent"),
    ("Ctrl+B then a", "Open the agent picker (click or arrow keys + Enter)"),
    ("Ctrl+B then ?", "Show this help"),
    ("Alt+\u{2191} / Alt+\u{2193}", "Switch agent (where the terminal supports it)"),
    ("Click the bottom bar", "Open the agent picker"),
    ("Ctrl+R", "Search and resume a past session"),
];

/// Opened by `Ctrl+B ?` (see `RunEvent::ShowHelp`) -- a plain list of every
/// ah-level shortcut, dismissed by any keypress. Deliberately simpler than
/// `run_agent_picker`: nothing to select, no click targets, just read and
/// dismiss, so this only needs the keystroke half of that function's
/// machinery.
fn run_help_overlay(sink: &Arc<Mutex<InputSink>>, suppress: &Arc<AtomicBool>) {
    suppress.store(true, Ordering::SeqCst);
    let (key_tx, key_rx) = mpsc::channel::<Vec<u8>>();
    *sink.lock().unwrap() = InputSink::Capture(key_tx);

    let mut keys = ChannelKeys::new(key_rx);
    let mut out = stdout();
    if draw_help_overlay(&mut out).is_ok() {
        let _ = keys.next_key();
    }

    suppress.store(false, Ordering::SeqCst);
    // Same as `run_search_overlay`/`run_agent_picker`: the render thread's
    // own `was_suppressed && !is_suppressed` check repaints the real
    // screen the moment `suppress` flips back.
}

/// A single centered message, dismissed by any keypress -- the same
/// "temporarily steal input, show something, give it back" shape as
/// `run_help_overlay`, just for a one-line notice instead of a shortcut
/// list. Doesn't touch `sink` on the way out (see `run_help_overlay`'s own
/// doc comment on why): the caller already restores it right after this
/// returns, same as every other overlay.
fn run_help_style_message(sink: &Arc<Mutex<InputSink>>, suppress: &Arc<AtomicBool>, title: &str, subtitle: &str) {
    suppress.store(true, Ordering::SeqCst);
    let (key_tx, key_rx) = mpsc::channel::<Vec<u8>>();
    *sink.lock().unwrap() = InputSink::Capture(key_tx);

    let mut keys = ChannelKeys::new(key_rx);
    let mut out = stdout();
    if draw_centered_message(&mut out, title, subtitle).is_ok() {
        let _ = keys.next_key();
    }

    suppress.store(false, Ordering::SeqCst);
}

fn draw_centered_message(out: &mut impl Write, title: &str, subtitle: &str) -> anyhow::Result<()> {
    let (cols, rows) = terminal::size()?;
    let content_width = title.chars().count().max(subtitle.chars().count());
    let box_width = (content_width + 4) as u16;
    let box_height: u16 = 4;
    let start_col = cols.saturating_sub(box_width) / 2;
    let start_row = rows.saturating_sub(box_height) / 2;

    execute!(out, terminal::Clear(terminal::ClearType::All), cursor::Hide, cursor::MoveTo(0, 0))?;
    queue!(out, cursor::MoveTo(start_col, start_row), Print(theme::brand_cyan(&format!("\u{256d}{}\u{256e}", "\u{2500}".repeat(box_width as usize - 2)))))?;

    let title_padded = format!("{title:^width$}", width = box_width as usize - 2);
    queue!(out, cursor::MoveTo(start_col, start_row + 1), Print(theme::brand_cyan("\u{2502}")))?;
    queue!(out, Print(theme::bold(&title_padded)))?;
    queue!(out, Print(theme::brand_cyan("\u{2502}")))?;

    let subtitle_padded = format!("{subtitle:^width$}", width = box_width as usize - 2);
    queue!(out, cursor::MoveTo(start_col, start_row + 2), Print(theme::brand_cyan("\u{2502}")))?;
    queue!(out, Print(theme::grey(&subtitle_padded)))?;
    queue!(out, Print(theme::brand_cyan("\u{2502}")))?;

    queue!(out, cursor::MoveTo(start_col, start_row + box_height - 1), Print(theme::brand_cyan(&format!("\u{2570}{}\u{256f}", "\u{2500}".repeat(box_width as usize - 2)))))?;
    out.flush()?;
    Ok(())
}

fn draw_help_overlay(out: &mut impl Write) -> anyhow::Result<()> {
    let (cols, rows) = terminal::size()?;
    let key_width = HELP_LINES.iter().map(|(k, _)| k.chars().count()).max().unwrap_or(0);
    let max_desc_width = HELP_LINES.iter().map(|(_, d)| d.chars().count()).max().unwrap_or(0);
    // Every row's key gets padded to `key_width` (see the `line` format
    // below), so the box must be sized off that padded width, not each
    // row's own (pre-padding) key length -- using the per-row length
    // undercounted whichever row had a short key next to a long
    // description, letting that description overflow the right border.
    let content_width = 1 + key_width + 2 + max_desc_width;
    let box_width = (content_width + 2) as u16;
    let box_height = HELP_LINES.len() as u16 + 3;
    let start_col = cols.saturating_sub(box_width) / 2;
    let start_row = rows.saturating_sub(box_height) / 2;

    execute!(out, terminal::Clear(terminal::ClearType::All), cursor::Hide, cursor::MoveTo(0, 0))?;

    queue!(out, cursor::MoveTo(start_col, start_row), Print(theme::brand_cyan(&format!("\u{256d}{}\u{256e}", "\u{2500}".repeat(box_width as usize - 2)))))?;
    let title = " agent-hop shortcuts ";
    let title_col = start_col + (box_width.saturating_sub(title.chars().count() as u16)) / 2;
    queue!(out, cursor::MoveTo(title_col, start_row), Print(theme::bold(&theme::brand_cyan(title))))?;

    for (i, (key, desc)) in HELP_LINES.iter().enumerate() {
        let row = start_row + 1 + i as u16;
        let line = format!(" {key:<key_width$}  {desc}");
        let padded = format!("{line:<width$}", width = box_width as usize - 2);
        queue!(out, cursor::MoveTo(start_col, row), Print(theme::brand_cyan("\u{2502}")))?;
        queue!(out, Print(theme::grey(&padded)))?;
        queue!(out, Print(theme::brand_cyan("\u{2502}")))?;
    }
    let footer_row = start_row + 1 + HELP_LINES.len() as u16;
    let footer = "press any key to close";
    let footer_padded = format!("{footer:^width$}", width = box_width as usize - 2);
    queue!(out, cursor::MoveTo(start_col, footer_row), Print(theme::brand_cyan("\u{2502}")))?;
    queue!(out, Print(theme::grey(&footer_padded)))?;
    queue!(out, Print(theme::brand_cyan("\u{2502}")))?;
    queue!(out, cursor::MoveTo(start_col, start_row + box_height - 1), Print(theme::brand_cyan(&format!("\u{2570}{}\u{256f}", "\u{2500}".repeat(box_width as usize - 2)))))?;
    out.flush()?;
    Ok(())
}

/// Renders one full frame: the agent's own content (from
/// `term.renderable_content()`) plus the toggle bar composed into the same
/// `ratatui::buffer::Buffer`, then handed to `ratatui::Terminal::draw` --
/// which does the actual work of turning that buffer into the minimal set
/// of ANSI writes needed to make the real terminal match it. This is the
/// one and only place anything gets written to the real terminal for a
/// running agent; nothing else in this file holds a competing stdout
/// handle for it, which is what makes the whole "something else clobbered
/// the status row" family of bugs from the previous design structurally
/// unreachable now rather than merely mitigated.
fn render_frame(rterm: &mut RTerminal<CrosstermBackend<Stdout>>, term: &mut vt::Terminal, tool: ToolName) -> anyhow::Result<()> {
    // `cursor()` is what actually triggers libghostty-vt's render-state
    // update for this frame (see its doc comment in vt.rs) -- must run
    // before `for_each_cell`, which reuses that same snapshot rather than
    // updating it again itself.
    let cursor = term.cursor();
    rterm.draw(|frame| {
        let area = frame.area();
        let bar_row = area.height.saturating_sub(1);
        let buf = frame.buffer_mut();

        term.for_each_cell(|x, y, cell| {
            // The child's own coordinate space starts at 0, but its content
            // is composited starting one row down from the real top --
            // `TOP_BAR_ROWS` is ours, drawn separately below, never the
            // child's to touch (its own pty is sized `rows - CHROME_ROWS`
            // tall, so no coordinate it sends can even reach this row).
            let real_y = y + TOP_BAR_ROWS;
            if x >= area.width || real_y >= bar_row {
                return;
            }
            let Some(rc) = buf.cell_mut((x, real_y)) else { return };
            if cell.wide_spacer {
                rc.set_symbol("");
                rc.skip = true;
                return;
            }
            if cell.text.is_empty() {
                rc.set_char(' ');
            } else {
                rc.set_symbol(&cell.text);
            }
            rc.fg = resolve_color(cell.fg);
            rc.bg = resolve_color(cell.bg);
            rc.modifier = cell_modifier(&cell);
            rc.skip = false;
        });

        write_top_bar(buf, area.width);
        write_toggle_bar(buf, area.width, bar_row, tool);

        let cursor_real_y = cursor.y + TOP_BAR_ROWS;
        if cursor.visible && cursor.x < area.width && cursor_real_y < bar_row {
            frame.set_cursor_position((cursor.x, cursor_real_y));
        }
    })?;
    Ok(())
}

/// Pinned to the very top of the real screen, always -- the "you're inside
/// agent-hop, not looking at the raw agent" frame this exists for needs to
/// be visible the instant you look at the screen, not just discoverable by
/// noticing a footnote at the bottom. A filled background (not just colored
/// text on the default background) is what makes it read as a distinct
/// strip of chrome rather than another line of the agent's own output --
/// same reasoning a real GUI app's title bar has a background fill, not
/// just a label floating over whatever's behind it.
/// Pinned to the very top of the real screen, always -- the "you're inside
/// agent-hop, not looking at the raw agent" frame this exists for needs to
/// be visible the instant you look at the screen, not just discoverable by
/// noticing a footnote at the bottom. A solid-filled 3-row badge (not a
/// single line of text) is what actually reads as a *logo mark*, the same
/// way a real app's title bar carries a mark, not a caption -- the tool
/// name and keybinding hints already live in the bottom toggle bar (see
/// `write_toggle_bar`), so this carries nothing else, deliberately.
///
/// A hand-drawn block-letter bitmap "AH" was tried first, built from
/// nothing but the full-block character to render identically across
/// fonts -- but confirmed live, it didn't work anyway: terminal cells are
/// noticeably taller than they are wide, so a letterform designed assuming
/// square pixels comes out squashed and unrecognizable (the "A" read as a
/// goblet, not a letter) regardless of which block characters compose it.
/// Plain bold text centered in a tall solid-color badge sidesteps that
/// entirely -- the text renders at the terminal's own normal, correct
/// glyph shapes, and the height comes from the badge's background fill,
/// not from trying to draw the letters' own pixels.
///
/// Deliberately *not* a real bitmap image via Kitty graphics, which is
/// exactly what the old per-tool logo row used and which caused three
/// separate real, confirmed bugs (retransmission flooding, the placement
/// ACK leaking into the child agent as typed input, and an intermittent
/// real terminal scroll) -- see `write_toggle_bar`'s own "Logo history" doc
/// comment.
fn write_top_bar(buf: &mut Buffer, width: u16) {
    let brand_rgb = RColor::Rgb(theme::BRAND_RGB.0, theme::BRAND_RGB.1, theme::BRAND_RGB.2);
    for row in 0..TOP_BAR_ROWS {
        for x in 0..width {
            let Some(c) = buf.cell_mut((x, row)) else { continue };
            c.set_char(' ');
            c.fg = RColor::Reset;
            c.bg = brand_rgb;
            c.modifier = RModifier::empty();
            c.skip = false;
        }
    }
    let mid_row = TOP_BAR_ROWS / 2;
    let text = "AGENT-HOP";
    let span = Span::styled(text, Style::default().fg(RColor::Black).bg(brand_rgb).add_modifier(RModifier::BOLD));
    let start_col = width.saturating_sub(text.chars().count() as u16) / 2;
    buf.set_span(start_col, mid_row, &span, text.chars().count() as u16);
}

/// Composes the status row directly into the frame buffer: agent-hop's own
/// brand mark leads (bold brand cyan, distinct from any per-tool color --
/// see `theme::brand_cyan`), then the currently-running tool's colored tag,
/// then the keybinding hints, dimmed so they read as secondary to the
/// brand+tag. This used to lead with the tool's own tag and no brand color
/// at all, which read as "you're using claude" with some plumbing text
/// after it, not "you're using agent-hop, which is currently running
/// claude" -- the whole point of the tool.
///
/// Logo history: this row used to render a per-tool image via Kitty
/// graphics, which caused three separate real, confirmed bugs over one
/// session (retransmission flooding, the terminal's placement ACK leaking
/// into the child agent as typed input, and -- worst -- repeatedly
/// re-issuing the placement command was itself enough to intermittently
/// trigger a real terminal scroll for at least one agent, reproduced
/// live). A Braille-art rendering of the same logos was tried next, but at
/// the ~4x1-character size the status row actually has room for, it read
/// as a distorted blob of dots, not a recognizable icon -- there simply
/// aren't enough pixels for it to work. Plain colored text has neither
/// problem, and composing it into the same buffer the agent's own content
/// renders from means it can never be a separate, racing write.
fn write_toggle_bar(buf: &mut Buffer, width: u16, row: u16, tool: ToolName) {
    // Blank the row first: `set_line` below only writes as many cells as
    // the rendered text needs, and ratatui hands back whichever of its two
    // internal buffers held the frame from *two* draws ago, not a blank
    // one -- any cell past the end of the text would otherwise still show
    // whatever was there that far back.
    for x in 0..width {
        let Some(c) = buf.cell_mut((x, row)) else { continue };
        c.set_char(' ');
        c.fg = RColor::Reset;
        c.bg = RColor::Reset;
        c.modifier = RModifier::empty();
        c.skip = false;
    }
    let brand_style = Style::default().fg(RColor::Rgb(theme::BRAND_RGB.0, theme::BRAND_RGB.1, theme::BRAND_RGB.2)).add_modifier(RModifier::BOLD);
    let tag_style = Style::default().fg(theme::tool_ratatui_color(tool)).add_modifier(RModifier::BOLD);
    let hint_style = Style::default().fg(theme::GREY_RATATUI);
    let line = RLine::from(vec![
        Span::raw(" "),
        Span::styled("agent-hop", brand_style),
        Span::raw(" "),
        Span::styled("\u{25b8}", hint_style),
        Span::raw(" "),
        Span::styled(format!("[{}]", tool.slug()), tag_style),
        Span::raw("  "),
        Span::styled("Ctrl+B ? for shortcuts \u{00b7} Alt+\u{2191}/\u{2193} switch agent \u{00b7} Ctrl+R resume", hint_style),
    ]);
    buf.set_line(0, row, &line, width);
}

/// Where raw stdin bytes currently go: forwarded straight to the active
/// child's pty (normal operation), captured into a channel for the search
/// overlay to parse, or dropped (no child spawned yet).
///
/// `Forward` holds a *shared* writer (not an owned one) because the pty's
/// writer can only ever be taken once (`take_writer()` panics/errors on a
/// second call), yet two independent things need to write to it: the user's
/// own keystrokes (here) and `EventProxy::send_event` answering a query the
/// terminal model generated on the child's behalf (a DA/DSR reply, for
/// instance) -- both are just "bytes going to the child," so one shared
/// handle serves both.
enum InputSink {
    Forward(Arc<Mutex<Box<dyn Write + Send>>>),
    Capture(mpsc::Sender<Vec<u8>>),
    Idle,
}

/// One channel carries both the active child's raw output bytes *and*
/// decoded mouse events -- Rust's std `mpsc` has no native multi-channel
/// select, so this is how the render thread's single blocking
/// `recv_timeout` loop can react to either without a second polling loop.
/// The child-output reader thread sends `Bytes`; the stdin relay thread
/// (via `mouse_sink`, updated in lockstep with `sink`) sends `Mouse`.
enum ChildMsg {
    Bytes(Vec<u8>),
    Mouse { x: u16, y: u16, input: vt::MouseInput, mods: vt::MouseMods },
    /// Sent explicitly by the child-output reader thread on EOF/read
    /// error, instead of relying on `byte_rx` disconnecting -- see that
    /// send site's own doc comment for the deadlock this replaces.
    Eof,
}

/// One event stream for `run_agent_picker`'s own loop, merging its two
/// input sources -- decoded keystrokes (via the same `ChannelKeys`
/// plumbing the search overlay already uses) and raw left-click positions
/// (via `overlay_click_sink`, since a click on the picker's own box has
/// nothing to do with any child agent's screen and needs absolute
/// terminal coordinates, not the chrome-relative ones `parse_sgr_mouse`
/// normally produces for a running child).
enum PickerEvent {
    Key(resume::SearchKey),
    Click(u16, u16),
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

/// Decodes one complete SGR mouse report -- `ESC [ < Cb ; Px ; Py (M|m)` --
/// into a `ChildMsg::Mouse`, or `None` if `seq` isn't one (a plain CSI
/// sequence, or an SGR report landing on `ah`'s own reserved toggle-bar
/// row, which doesn't exist in the child's own screen model at all -- see
/// `child_screen_model_cannot_exceed_its_own_row_count`).
///
/// Bit layout of `Cb` (xterm's own encoding, which is what `EnableMouseCapture`
/// negotiates): bits 0-1 are the button number, bit 2 (4) is Shift, bit 3
/// (8) is Alt, bit 4 (16) is Ctrl, bit 5 (32) marks motion/drag, and bit 6
/// (64) combined with bits 0-1 marks the scroll wheel (0 = up, 1 = down).
/// What an SGR mouse report should turn into, one layer up from raw bytes.
enum MouseDecode {
    /// Forward to whichever child currently owns `mouse_sink`.
    Forward(ChildMsg),
    /// A left-button press landed on `ah`'s own bottom toggle bar --
    /// nothing to forward (that row isn't part of any child's screen
    /// model), but it's a deliberate click on the thing that already shows
    /// the current agent's name, so it opens the same agent list Alt+Up/Down
    /// cycles through, rather than just being silently dropped.
    OpenAgentPicker,
    /// A valid SGR report, but not one that means anything here (e.g. a
    /// click on the top brand bar, or a release/motion/right-click on the
    /// bottom bar).
    Ignore,
    /// Not an SGR mouse report at all -- caller should fall through to
    /// other CSI handling (Alt+Up/Down, Ctrl+R, or plain forwarding).
    NotMouse,
}

/// Decodes a raw SGR left-button-press position in absolute real-terminal
/// coordinates (0-indexed), with none of `parse_sgr_mouse`'s chrome-row or
/// child-relative adjustments -- for overlays like the agent picker, which
/// draw directly in real screen coordinates and have no concept of
/// `TOP_BAR_ROWS`/`CHROME_ROWS` at all. Returns `None` for anything that
/// isn't specifically a left-button press (releases, drags, scrolls, other
/// buttons) -- an overlay's click-to-select only ever cares about presses.
fn parse_sgr_left_click_absolute(seq: &[u8]) -> Option<(u16, u16)> {
    if seq.len() < 4 || seq[2] != b'<' || *seq.last()? != b'M' {
        return None;
    }
    let body = std::str::from_utf8(&seq[3..seq.len() - 1]).ok()?;
    let mut parts = body.split(';');
    let cb: u32 = parts.next()?.parse().ok()?;
    let px: u16 = parts.next()?.parse().ok()?;
    let py: u16 = parts.next()?.parse().ok()?;
    // Left button, no motion/drag bit, no scroll bit.
    if cb & 0b11 != 0 || cb & 0b0110_0000 != 0 {
        return None;
    }
    Some((px.saturating_sub(1), py.saturating_sub(1)))
}

fn parse_sgr_mouse(seq: &[u8]) -> MouseDecode {
    if seq.len() < 4 || seq[2] != b'<' {
        return MouseDecode::NotMouse;
    }
    let Some(&final_byte) = seq.last() else { return MouseDecode::NotMouse };
    if final_byte != b'M' && final_byte != b'm' {
        return MouseDecode::NotMouse;
    }
    let Ok(body) = std::str::from_utf8(&seq[3..seq.len() - 1]) else { return MouseDecode::Ignore };
    let mut parts = body.split(';');
    let (Some(cb), Some(px), Some(py)) = (
        parts.next().and_then(|s| s.parse::<u32>().ok()),
        parts.next().and_then(|s| s.parse::<u16>().ok()),
        parts.next().and_then(|s| s.parse::<u16>().ok()),
    ) else {
        return MouseDecode::Ignore;
    };

    // `real_y` is 0-indexed on the *real* terminal. Rows `0..TOP_BAR_ROWS`
    // (the top brand bar) and `real_rows - 1` (the bottom toggle bar) are
    // `ah`'s own chrome, not part of the child's own screen model at all --
    // see `CHROME_ROWS` -- so a click landing on either doesn't have a
    // meaningful child-relative coordinate to forward.
    let real_y = py.saturating_sub(1);
    if let Ok((_, real_rows)) = terminal::size() {
        let on_bottom_bar = real_y + 1 == real_rows;
        if real_y < TOP_BAR_ROWS || on_bottom_bar {
            let is_left_press = final_byte == b'M' && cb & 0b11 == 0 && cb & 0b0110_0000 == 0;
            return if on_bottom_bar && is_left_press { MouseDecode::OpenAgentPicker } else { MouseDecode::Ignore };
        }
    }
    let x = px.saturating_sub(1);
    let y = real_y.saturating_sub(TOP_BAR_ROWS);
    let mods = vt::MouseMods { shift: cb & 4 != 0, alt: cb & 8 != 0, ctrl: cb & 16 != 0 };

    if cb & 64 != 0 {
        let input = if cb & 1 == 0 { vt::MouseInput::ScrollUp } else { vt::MouseInput::ScrollDown };
        return MouseDecode::Forward(ChildMsg::Mouse { x, y, input, mods });
    }

    let is_motion = cb & 32 != 0;
    let btn_bits = cb & 0b11;
    let input = if is_motion && btn_bits == 3 {
        vt::MouseInput::Motion(None)
    } else {
        let button = match btn_bits {
            0 => vt::MouseButtonKind::Left,
            1 => vt::MouseButtonKind::Middle,
            2 => vt::MouseButtonKind::Right,
            _ => return MouseDecode::Ignore,
        };
        if is_motion {
            vt::MouseInput::Motion(Some(button))
        } else if final_byte == b'M' {
            vt::MouseInput::Press(button)
        } else {
            vt::MouseInput::Release(button)
        }
    };
    MouseDecode::Forward(ChildMsg::Mouse { x, y, input, mods })
}

/// Asks the *real* host terminal what its own default foreground/background
/// colors are, via the standard OSC 10/11 query-reply protocol, so those
/// real colors can be handed to every `vt::Terminal` this process spawns
/// (see `vt::Terminal::with_host_colors`'s doc comment for why this
/// matters: without a known default background, libghostty-vt can't answer
/// a *child's own* OSC 10/11 query, and at least one real agent (Codex)
/// treats that unanswered query as "assume no truecolor background
/// support" and disables an adaptive-background UI element as a result).
///
/// Uses the same `InputSink::Capture` mechanism as the search overlay to
/// borrow the stdin relay thread for the ~100ms this takes, since a
/// terminal that doesn't support the query at all (rare, but real) would
/// otherwise hang this forever with nothing to unblock it.
fn query_host_colors(sink: &Arc<Mutex<InputSink>>) -> (Option<vt::Rgb>, Option<vt::Rgb>) {
    let (key_tx, key_rx) = mpsc::channel::<Vec<u8>>();
    *sink.lock().unwrap() = InputSink::Capture(key_tx);

    let mut out = stdout();
    let _ = out.write_all(b"\x1b]10;?\x1b\\\x1b]11;?\x1b\\");
    let _ = out.flush();

    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(200);
    let mut buf = Vec::new();
    while std::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        match key_rx.recv_timeout(remaining) {
            Ok(bytes) => {
                buf.extend_from_slice(&bytes);
                if parse_osc_color(&buf, 10).is_some() && parse_osc_color(&buf, 11).is_some() {
                    break;
                }
            }
            Err(_) => break,
        }
    }

    *sink.lock().unwrap() = InputSink::Idle;
    (parse_osc_color(&buf, 10), parse_osc_color(&buf, 11))
}

/// Parses one `OSC <code> ; rgb:RRRR/GGGG/BBBB (BEL|ST)` reply out of a
/// buffer that may contain other bytes before/after/between it -- terminals
/// reply to a batched query with two separate OSC replies back to back, in
/// whatever order they please.
fn parse_osc_color(buf: &[u8], code: u32) -> Option<vt::Rgb> {
    let prefix = format!("\x1b]{code};rgb:");
    let text = String::from_utf8_lossy(buf);
    let start = text.find(&prefix)? + prefix.len();
    let rest = &text[start..];
    let end = rest.find(['\x07', '\x1b']).unwrap_or(rest.len());
    let spec = &rest[..end];
    let mut parts = spec.split('/');
    let r = u16::from_str_radix(parts.next()?, 16).ok()?;
    let g = u16::from_str_radix(parts.next()?, 16).ok()?;
    let b = u16::from_str_radix(parts.next()?, 16).ok()?;
    Some(vt::Rgb { r: (r >> 8) as u8, g: (g >> 8) as u8, b: (b >> 8) as u8 })
}

fn forward(sink: &Arc<Mutex<InputSink>>, bytes: &[u8]) {
    match &mut *sink.lock().unwrap() {
        InputSink::Forward(w) => {
            let mut w = w.lock().unwrap();
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
/// this design -- the pty and the terminal model are both sized once at
/// spawn time and never revisited, so an entirely ordinary thing (the user
/// resizing their terminal window mid-session) silently desyncs our status
/// row's position from where the child's own (still old-sized) content
/// model believes its last row is, producing exactly the interleaved/
/// overlapping corruption this was built to fix. Polling instead of a real
/// SIGWINCH handler because it's portable (Windows has no SIGWINCH).
///
/// Settle-debounced rather than reporting every observed size: dragging a
/// window edge (or, worse, a terminal tab animating into place right after
/// a hop) sweeps through several distinct intermediate sizes over tens to
/// hundreds of milliseconds. Reporting each one used to forward every one
/// of them straight to `pair.master.resize()`, which sends the child a
/// real SIGWINCH per size -- so a freshly-spawned agent (Codex, confirmed
/// live) would redraw itself multiple times for genuinely different
/// transient sizes in a matter of milliseconds, while the terminal model
/// on the render thread caught up to a resize asynchronously by polling a
/// last-write-wins value -- with several distinct sizes flying by that
/// fast, it could adopt one size while bytes the child already emitted for
/// a *different* size were still in flight, and parse them against the
/// wrong grid. That dimension mismatch is what produced the corrupted,
/// duplicated-looking box seen live during a hop into Codex: not a single
/// missing clear, but content addressed for one size being interpreted at
/// another. Only emitting once the size has held steady for
/// `SETTLE_DURATION` means a hop or a window drag always converges on
/// exactly one final `Resized` event, so the child (and our own model)
/// only ever redraw for the size that's actually still true by the time
/// they see it.
fn spawn_resize_poller(tx: mpsc::Sender<RunEvent>) {
    const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(60);
    const SETTLE_DURATION: std::time::Duration = std::time::Duration::from_millis(150);
    std::thread::spawn(move || {
        let mut applied = terminal::size().unwrap_or((80, 24));
        let mut candidate = applied;
        let mut candidate_since = std::time::Instant::now();
        loop {
            std::thread::sleep(POLL_INTERVAL);
            let current = match terminal::size() {
                Ok(c) => c,
                Err(_) => break,
            };
            if current != candidate {
                // Still moving -- reset the settle timer against this new
                // candidate rather than the one we were previously tracking.
                candidate = current;
                candidate_since = std::time::Instant::now();
                continue;
            }
            if candidate != applied && candidate_since.elapsed() >= SETTLE_DURATION {
                debug_log("RESIZE_POLLER_DETECTED", format!("{applied:?} -> {candidate:?}").as_bytes());
                applied = candidate;
                if tx.send(RunEvent::Resized(candidate.0, candidate.1)).is_err() {
                    break; // receiver gone -- program is shutting down
                }
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
fn spawn_stdin_relay(
    sink: Arc<Mutex<InputSink>>,
    mouse_sink: Arc<Mutex<Option<mpsc::Sender<ChildMsg>>>>,
    overlay_click_sink: Arc<Mutex<Option<mpsc::Sender<(u16, u16)>>>>,
    tx: mpsc::Sender<RunEvent>,
) {
    // The blocking read lives on its own thread, feeding the processing
    // loop below through a channel, so that loop can use `recv_timeout`
    // instead of an indefinite blocking read -- needed specifically to
    // disambiguate a lone ESC byte. A genuine standalone Escape keypress
    // (no follow-up bytes, ever) used to just sit in `pending` forever
    // waiting to see if it was the start of a CSI/trigger sequence
    // (Alt+Up/Down, Ctrl+R), since the old code could only ever resolve
    // that ambiguity by reading more bytes -- a real, confirmed bug: Esc
    // silently doing nothing in the search-and-resume overlay, which
    // depends on this relay ever forwarding it a Quit/cancel keystroke.
    let (byte_tx, byte_rx) = mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin();
        let mut buf = [0u8; 1024];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if byte_tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });

    std::thread::spawn(move || {
        let mut pending: Vec<u8> = Vec::new();
        loop {
            let is_lone_esc = pending == [0x1b];
            // `0x02` is Ctrl+B -- ah's own tmux/herdr-style prefix key (see
            // `PREFIX_CHORD_TIMEOUT`'s doc comment for why this exists at
            // all). Unlike ESC, a lone Ctrl+B is never the start of a
            // legitimate multi-byte escape sequence -- it's a single,
            // self-contained control byte -- so there's no ambiguity to
            // resolve here the way there is for ESC. This still needs the
            // same bounded wait, though: the very next byte the user types
            // determines which chord fired (or that none did), and without
            // waiting, a follow-up keystroke that hasn't arrived yet would
            // just look identical to "no chord key came."
            let is_lone_prefix = pending == [PREFIX_KEY];
            let received = if is_lone_esc {
                match byte_rx.recv_timeout(std::time::Duration::from_millis(50)) {
                    Ok(data) => Some(data),
                    Err(mpsc::RecvTimeoutError::Timeout) => None,
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            } else if is_lone_prefix {
                match byte_rx.recv_timeout(PREFIX_CHORD_TIMEOUT) {
                    Ok(data) => Some(data),
                    Err(mpsc::RecvTimeoutError::Timeout) => None,
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            } else {
                match byte_rx.recv() {
                    Ok(data) => Some(data),
                    Err(_) => break,
                }
            };
            match received {
                None if is_lone_prefix => {
                    // No chord key followed within the timeout -- same as
                    // tmux's own prefix-key UX, this is silently absorbed
                    // rather than forwarded. A user who pressed Ctrl+B was
                    // reaching for ah's prefix, not for whatever the child
                    // agent might otherwise bind that byte to; forwarding
                    // it now would just be confusing.
                    pending.clear();
                    continue;
                }
                None => {
                    // Timed out waiting for a continuation on a lone ESC
                    // -- resolve it as a real, standalone Escape keypress.
                    pending.clear();
                    forward(&sink, b"\x1b");
                    continue;
                }
                Some(data) => {
                    // The most important log line in the whole capture:
                    // this is every byte the *real* terminal ever sends us
                    // -- actual keystrokes, but also anything Ghostty sends
                    // back on its own (query responses, etc.) that no
                    // synthetic pty test can produce, since nothing answers
                    // those queries in a scripted test.
                    debug_log("STDIN_RAW", &data);
                    pending.extend_from_slice(&data);
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
                        // ah's own prefix chord: Ctrl+B (0x02) then a
                        // single plain letter, matching herdr's own default
                        // prefix key and its N/P next/previous-tab
                        // convention exactly -- picked specifically because
                        // it's the only kind of signal guaranteed to
                        // transmit identically on every terminal, every OS
                        // (a raw single-byte ASCII control code, unlike
                        // Alt+Up/Down's CSI-modifier encoding, which
                        // Terminal.app's default config silently drops --
                        // confirmed live, not guessed: Option+Up there just
                        // sends the same bytes as a plain Up arrow).
                        //
                        // A second-key press that isn't one of these is
                        // silently absorbed, not forwarded -- same as
                        // tmux's own prefix-key convention for an unbound
                        // chord: better than guessing what a user reaching
                        // for ah's prefix key actually meant.
                        if pending[0] == PREFIX_KEY && pending.len() == 1 {
                            break; // lone prefix key -- wait to see what chord follows
                        }
                        if pending[0] == PREFIX_KEY {
                            let chord = pending[1];
                            match chord {
                                b'n' | b'N' => {
                                    debug_log("TRIGGER_PREFIX_NEXT", &pending[..2]);
                                    let _ = tx.send(RunEvent::Hop(HopDirection::Next));
                                }
                                b'p' | b'P' => {
                                    debug_log("TRIGGER_PREFIX_PREV", &pending[..2]);
                                    let _ = tx.send(RunEvent::Hop(HopDirection::Prev));
                                }
                                b'a' | b'A' => {
                                    debug_log("TRIGGER_PREFIX_AGENT_PICKER", &pending[..2]);
                                    let _ = tx.send(RunEvent::AgentPicker);
                                }
                                b'?' => {
                                    debug_log("TRIGGER_PREFIX_HELP", &pending[..2]);
                                    let _ = tx.send(RunEvent::ShowHelp);
                                }
                                _ => {}
                            }
                            pending.drain(..2);
                            continue;
                        }
                        if pending[0] != 0x1b {
                            // Run of plain bytes -- forward as one chunk
                            // instead of byte-by-byte. Also stops at
                            // `PREFIX_KEY` (Ctrl+B) -- without that, a
                            // prefix chord arriving right after plain text
                            // in the same read() batch would get scooped up
                            // as "plain bytes" and forwarded to the child
                            // instead of ever reaching the chord-matching
                            // branch above.
                            let end = pending.iter().position(|&b| b == 0x1b || b == 0x12 || b == PREFIX_KEY).unwrap_or(pending.len());
                            let chunk: Vec<u8> = pending.drain(..end).collect();
                            forward(&sink, &chunk);
                            continue;
                        }
                        if pending.len() == 1 {
                            break; // lone ESC -- wait to see what follows
                        }
                        // A Kitty graphics protocol response (`ESC _G
                        // ... ESC \`). We don't send Kitty commands
                        // ourselves anymore (see write_toggle_bar's doc
                        // comment for why), but this arrives on *our* real
                        // stdin regardless of who triggered it, never as
                        // genuine keyboard input -- kept as a defensive
                        // filter in case any child agent's own Kitty usage
                        // ever produces one here. Forwarding one into the
                        // child agent makes it echo the raw sequence back
                        // as visible garbage text -- a real, confirmed bug
                        // from when we did send our own Kitty commands.
                        if pending.starts_with(b"\x1b_G") {
                            if let Some(end) = find_st_terminator(&pending) {
                                debug_log("KITTY_RESPONSE_DROPPED", &pending[..end]);
                                pending.drain(..end);
                                continue;
                            }
                            break; // wait for the ST terminator
                        }
                        if pending[1] == b'[' {
                            match find_csi_final_byte(&pending) {
                                Some(final_idx) => {
                                    let seq: Vec<u8> = pending.drain(..=final_idx).collect();
                                    // SGR mouse report (`ESC [ < Cb;Px;Py M/m`)
                                    // -- real terminal mouse capture only
                                    // sends these once `EnableMouseCapture`
                                    // negotiated SGR encoding, so this is
                                    // unambiguous. Decoded and re-encoded
                                    // per-child (see `ChildMsg::Mouse`'s doc
                                    // comment) rather than forwarded
                                    // raw, since the child may have
                                    // negotiated a different tracking
                                    // mode/format than what the real
                                    // terminal used to report it to us.
                                    // While an overlay like the agent
                                    // picker is open (see
                                    // `overlay_click_sink`'s own doc
                                    // comment), a click belongs to *it*,
                                    // not to whatever child agent happens
                                    // to still be running underneath --
                                    // decoded in absolute terminal
                                    // coordinates here, bypassing
                                    // `parse_sgr_mouse`'s chrome-relative
                                    // logic entirely (that logic assumes
                                    // its target is always a child's own
                                    // sub-screen, which doesn't apply to an
                                    // overlay that draws directly in real
                                    // screen coordinates).
                                    let is_sgr_mouse_report = seq.len() >= 3 && seq[2] == b'<';
                                    if is_sgr_mouse_report {
                                        if let Some(click_tx) = overlay_click_sink.lock().unwrap().as_ref() {
                                            if let Some((x, y)) = parse_sgr_left_click_absolute(&seq) {
                                                let _ = click_tx.send((x, y));
                                            }
                                            continue;
                                        }
                                    }
                                    match parse_sgr_mouse(&seq) {
                                        MouseDecode::Forward(decoded) => {
                                            if let Some(mtx) = mouse_sink.lock().unwrap().as_ref() {
                                                let _ = mtx.send(decoded);
                                            }
                                            continue;
                                        }
                                        MouseDecode::OpenAgentPicker => {
                                            let _ = tx.send(RunEvent::AgentPicker);
                                            continue;
                                        }
                                        MouseDecode::Ignore => continue,
                                        MouseDecode::NotMouse => {}
                                    }
                                    match parse_csi_trigger(&seq) {
                                        Some(ParsedTrigger::AltUp) => {
                                            debug_log("TRIGGER_ALT_UP", &seq);
                                            let _ = tx.send(RunEvent::Hop(HopDirection::Prev));
                                        }
                                        Some(ParsedTrigger::AltDown) => {
                                            debug_log("TRIGGER_ALT_DOWN", &seq);
                                            let _ = tx.send(RunEvent::Hop(HopDirection::Next));
                                        }
                                        Some(ParsedTrigger::CtrlR) => {
                                            debug_log("TRIGGER_CTRL_R", &seq);
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
mod sgr_mouse_parser_tests {
    use super::*;

    // `py = 10` throughout: comfortably past both of `ah`'s own reserved
    // chrome rows (the top brand bar and the bottom toggle bar), so these
    // decode assertions hold regardless of whether `terminal::size()`
    // happens to succeed in the test harness's own stdout (the row-clamp
    // it feeds only ever *excludes* events near the real top/bottom edge,
    // never changes the coordinate math for one safely in the middle).

    #[test]
    fn left_click_press_and_release() {
        match parse_sgr_mouse(b"\x1b[<0;10;10M") {
            MouseDecode::Forward(ChildMsg::Mouse { x, y, input: vt::MouseInput::Press(vt::MouseButtonKind::Left), .. }) => {
                // 1-indexed on the wire; 0-indexed here, then shifted up by
                // `TOP_BAR_ROWS` to land in the child's own coordinate space.
                assert_eq!((x, y), (9, 10 - 1 - TOP_BAR_ROWS));
            }
            _ => panic!("expected a left press"),
        }
        assert!(matches!(
            parse_sgr_mouse(b"\x1b[<0;10;10m"),
            MouseDecode::Forward(ChildMsg::Mouse { input: vt::MouseInput::Release(vt::MouseButtonKind::Left), .. })
        ));
    }

    #[test]
    fn scroll_wheel_up_and_down() {
        assert!(matches!(parse_sgr_mouse(b"\x1b[<64;5;10M"), MouseDecode::Forward(ChildMsg::Mouse { input: vt::MouseInput::ScrollUp, .. })));
        assert!(matches!(parse_sgr_mouse(b"\x1b[<65;5;10M"), MouseDecode::Forward(ChildMsg::Mouse { input: vt::MouseInput::ScrollDown, .. })));
    }

    #[test]
    fn drag_reports_as_motion_with_button_held() {
        assert!(matches!(
            parse_sgr_mouse(b"\x1b[<32;10;10M"),
            MouseDecode::Forward(ChildMsg::Mouse { input: vt::MouseInput::Motion(Some(vt::MouseButtonKind::Left)), .. })
        ));
    }

    #[test]
    fn plain_hover_motion_has_no_button() {
        assert!(matches!(parse_sgr_mouse(b"\x1b[<35;10;10M"), MouseDecode::Forward(ChildMsg::Mouse { input: vt::MouseInput::Motion(None), .. })));
    }

    #[test]
    fn modifiers_decode_from_cb_bits() {
        // 0 (left) | 4 (shift) | 8 (alt) | 16 (ctrl) = 28
        match parse_sgr_mouse(b"\x1b[<28;1;10M") {
            MouseDecode::Forward(ChildMsg::Mouse { mods, .. }) => {
                assert!(mods.shift && mods.alt && mods.ctrl);
            }
            _ => panic!("expected a decoded mouse event"),
        }
    }

    #[test]
    fn non_sgr_csi_sequences_are_not_mouse_reports() {
        assert!(matches!(parse_sgr_mouse(b"\x1b[2K"), MouseDecode::NotMouse));
        assert!(matches!(parse_sgr_mouse(b"\x1b[1;3B"), MouseDecode::NotMouse));
    }

    #[test]
    fn left_click_on_bottom_bar_opens_agent_picker() {
        // Row 10 with a 12-row terminal puts this squarely on the last
        // (bottom toggle bar) row -- only meaningful when `terminal::size()`
        // succeeds in the test harness, same caveat as elsewhere in this
        // module; skip the assertion rather than fail on a headless runner
        // that can't report a real terminal size.
        if terminal::size().is_err() {
            return;
        }
        let (_, rows) = terminal::size().unwrap();
        let seq = format!("\x1b[<0;5;{rows}M");
        assert!(matches!(parse_sgr_mouse(seq.as_bytes()), MouseDecode::OpenAgentPicker));
    }

    #[test]
    fn absolute_left_click_decodes_zero_indexed_position() {
        assert_eq!(parse_sgr_left_click_absolute(b"\x1b[<0;10;5M"), Some((9, 4)));
    }

    #[test]
    fn absolute_left_click_rejects_release_drag_and_scroll() {
        assert_eq!(parse_sgr_left_click_absolute(b"\x1b[<0;10;5m"), None); // release
        assert_eq!(parse_sgr_left_click_absolute(b"\x1b[<32;10;5M"), None); // drag
        assert_eq!(parse_sgr_left_click_absolute(b"\x1b[<64;10;5M"), None); // scroll
        assert_eq!(parse_sgr_left_click_absolute(b"\x1b[<2;10;5M"), None); // right click
    }
}

#[cfg(test)]
mod terminal_model_tests {
    use super::*;

    /// The structural guarantee the whole rendering design depends on: a
    /// child's pty is sized `rows - CHROME_ROWS` tall, so its terminal
    /// model can *only* ever contain content within its own rows. There's
    /// no escape sequence a child could send that makes `for_each_cell`
    /// yield anything touching either of our reserved chrome rows, because
    /// those rows don't exist in the child's model at all -- not "we
    /// filtered it out," structurally absent.
    #[test]
    fn child_screen_model_cannot_exceed_its_own_row_count() {
        let child_rows = 26u16; // matches rows.saturating_sub(CHROME_ROWS) for a 30-row terminal
        let cols = 100u16;
        let sink: Arc<Mutex<Box<dyn Write + Send>>> = Arc::new(Mutex::new(Box::new(std::io::sink())));
        let mut term = vt::Terminal::new(cols, child_rows, sink);

        // Try to get the child to scroll far past its own screen size, and
        // to explicitly move its cursor to an absurd row -- both are
        // things a real misbehaving or confused child could send.
        for _ in 0..500 {
            term.write(b"some line of output\r\n");
        }
        term.write(b"\x1b[9999;1H"); // absolute move to a huge row
        term.write(b"X");

        let cursor = term.cursor();
        if cursor.visible {
            assert!(
                (cursor.y as usize) < child_rows as usize,
                "cursor position must be clamped inside the model's own bounds, got row {}",
                cursor.y
            );
        }

        // There is no possible content at child_rows or beyond for
        // `for_each_cell` to yield -- the model is structurally that size.
        let mut saw_any = false;
        let mut max_row_seen = 0u16;
        term.for_each_cell(|_x, y, cell| {
            if !cell.text.is_empty() {
                saw_any = true;
            }
            max_row_seen = max_row_seen.max(y);
        });
        assert!((max_row_seen as usize) < child_rows as usize, "for_each_cell yielded a row outside the model's own bounds: {max_row_seen}");
        assert!(saw_any, "expected the 500 written lines to produce visible content");
    }
}
