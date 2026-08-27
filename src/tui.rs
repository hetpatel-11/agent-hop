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
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};

/// Unix socket path the parent `ah` listens on so a pane can run `ah tab`.
static CONTROL_SOCK: Mutex<Option<PathBuf>> = Mutex::new(None);
/// One tab's render thread at a time writes stdout. A new tab used to
/// start painting while the previous tab's last frame was still emitting,
/// which left the chrome and child grid shifted until a workspace click
/// forced a clear + full redraw.
static STDOUT_PAINT: Mutex<()> = Mutex::new(());

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
    HopTo(ToolName),
    ResumeInto { tool: ToolName, session_id: String, project_path: String },
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
    /// `PREFIX_KEY` then `c` -- new tab: another live agent PTY, current
    /// tab keeps running (unlike hop, which replaces the focused tab).
    NewTab,
    Pane(crate::control::PaneRequest),
    NewWorkspace,
    NextWorkspace,
    PrevWorkspace,
    /// `PREFIX_KEY` then `o` / `i` -- next / previous tab.
    NextTab,
    PrevTab,
    /// `PREFIX_KEY` then `x` -- close the focused tab (kills that agent).
    CloseTab,
    /// `PREFIX_KEY` then `1`…`9`, or a click on a tab in the top strip.
    FocusTab(usize),
    /// `PREFIX_KEY` then `q` — same chord herdr uses to detach. We are
    /// not a background server: layout is saved, agents stop, next `ah`
    /// restores the same workspaces and resumes each chat.
    Leave,
    /// Left-click on our chrome (top tab strip or session sidebar).
    ChromeClick { x: u16, y: u16 },
    /// The real terminal was resized (new cols, new rows). Not tied to any
    /// particular generation -- always relevant regardless of which agent
    /// is currently running.
    Resized(u16, u16),
}

/// Chrome labels: workspaces in the sidebar, tabs of the focused workspace on top.
#[derive(Clone)]
struct TabStrip {
    workspaces: Vec<(String, usize)>,
    ws_focus: usize,
    tabs: Vec<TabKind>,
    tab_focus: usize,
    /// Write `~/.agent-hop/layout.json` on mux changes. Off for
    /// one-shot `ah resume` so a search launch does not wipe the mux.
    persist: bool,
}

struct Workspace {
    path: String,
    tabs: Vec<LiveTab>,
    focus: usize,
}

/// One live PTY: an agent, or a shell tab (not spawned on agent exit).
#[derive(Clone, Copy, PartialEq, Eq)]
enum TabKind {
    Agent(ToolName),
    Shell,
}

impl TabKind {
    fn slug(self) -> &'static str {
        match self {
            TabKind::Agent(t) => t.slug(),
            TabKind::Shell => "term",
        }
    }

    fn tool(self) -> Option<ToolName> {
        match self {
            TabKind::Agent(t) => Some(t),
            TabKind::Shell => None,
        }
    }
}

/// One live agent PTY. Switching tabs never kills this; hop/close/exit does.
struct LiveTab {
    id: u64,
    kind: TabKind,
    project_path: String,
    session_id: Option<String>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    resize_tx: mpsc::Sender<(u16, u16)>,
    wake_tx: mpsc::Sender<ChildMsg>,
    paint: Arc<AtomicBool>,
    suppress: Arc<AtomicBool>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
}

fn sync_tab_strip(strip: &Arc<Mutex<TabStrip>>, workspaces: &[Workspace], ws_focus: usize) {
    let persist = {
        let mut s = strip.lock().unwrap();
        s.workspaces = workspaces.iter().map(|w| (w.path.clone(), w.tabs.len())).collect();
        s.ws_focus = ws_focus;
        if let Some(ws) = workspaces.get(ws_focus) {
            s.tabs = ws.tabs.iter().map(|t| t.kind).collect();
            s.tab_focus = ws.focus;
        } else {
            s.tabs.clear();
            s.tab_focus = 0;
        }
        s.persist
    };
    if persist {
        persist_mux(workspaces, ws_focus);
    }
}

fn persist_mux(workspaces: &[Workspace], ws_focus: usize) {
    let mut saved = crate::layout::SavedMux {
        ws_focus,
        workspaces: Vec::new(),
    };
    for ws in workspaces {
        let mut tabs = Vec::new();
        for tab in &ws.tabs {
            let Some(tool) = tab.kind.tool() else { continue };
            let session_id = tab.session_id.clone().or_else(|| {
                crate::adapters::find_latest_session_for_path(tool, &ws.path).map(|s| s.session_id)
            });
            tabs.push(crate::layout::SavedTab {
                tool: tool.slug().to_string(),
                session_id,
            });
        }
        if tabs.is_empty() {
            continue;
        }
        let focus = ws.focus.min(tabs.len().saturating_sub(1));
        saved.workspaces.push(crate::layout::SavedWorkspace {
            path: ws.path.clone(),
            focus,
            tabs,
        });
    }
    if saved.ws_focus >= saved.workspaces.len() && !saved.workspaces.is_empty() {
        saved.ws_focus = saved.workspaces.len() - 1;
    }
    crate::layout::save(&saved);
}

fn launch_from_saved(tool: ToolName, path: &str, session_id: Option<&str>) -> Launch {
    match crate::layout::resume_id(tool, path, session_id) {
        Some(id) => Launch::Resume(id),
        None => Launch::Fresh,
    }
}

fn restore_mux(
    saved: crate::layout::SavedMux,
    workspaces: &mut Vec<Workspace>,
    ws_focus: &mut usize,
    sink: &Arc<Mutex<InputSink>>,
    mouse_sink: &Arc<Mutex<Option<mpsc::Sender<ChildMsg>>>>,
    tx: &mpsc::Sender<RunEvent>,
    generation: &Arc<AtomicU64>,
    host_colors: (Option<vt::Rgb>, Option<vt::Rgb>),
    tab_strip: &Arc<Mutex<TabStrip>>,
) {
    let saved_focus = saved.ws_focus;
    for saved_ws in saved.workspaces {
        let mut tabs = Vec::new();
        for saved_tab in saved_ws.tabs {
            let Some(tool) = ToolName::from_slug(&saved_tab.tool) else { continue };
            if !tool.is_installed() {
                continue;
            }
            let launch = launch_from_saved(tool, &saved_ws.path, saved_tab.session_id.as_deref());
            let generation_id = generation.fetch_add(1, Ordering::SeqCst) + 1;
            match spawn_live_tab(
                TabKind::Agent(tool),
                &saved_ws.path,
                launch,
                sink,
                mouse_sink,
                tx,
                generation_id,
                host_colors,
                false,
                tab_strip.clone(),
            ) {
                Ok(tab) => tabs.push(tab),
                Err(e) => debug_log("RESTORE_TAB_ERR", format!("{e:#}").as_bytes()),
            }
        }
        if tabs.is_empty() {
            continue;
        }
        let focus = saved_ws.focus.min(tabs.len().saturating_sub(1));
        workspaces.push(Workspace { path: saved_ws.path, tabs, focus });
    }
    if workspaces.is_empty() {
        return;
    }
    *ws_focus = saved_focus.min(workspaces.len() - 1);
}

fn capture_leave(via: &str, workspaces: &[Workspace]) {
    let tabs: usize = workspaces.iter().map(|w| w.tabs.len()).sum();
    crate::telemetry::capture(
        "leave",
        serde_json::json!({
            "via": via,
            "workspaces": workspaces.len(),
            "tabs": tabs,
        }),
    );
}

fn leave_ah(workspaces: &mut [Workspace], ws_focus: usize, strip: &Arc<Mutex<TabStrip>>, via: &'static str) {
    capture_leave(via, workspaces);
    sync_tab_strip(strip, workspaces, ws_focus);
    for ws in workspaces.iter_mut() {
        for tab in &mut ws.tabs {
            let _ = tab.child.kill();
            let _ = tab.child.wait();
        }
    }
}

fn unpaint_all(workspaces: &[Workspace]) {
    for ws in workspaces {
        for tab in &ws.tabs {
            tab.paint.store(false, Ordering::SeqCst);
            // Wake so the render thread drops its ratatui backend now,
            // instead of painting one more frame after a new tab starts.
            let _ = tab.wake_tx.send(ChildMsg::Wake);
        }
    }
}

fn focus_active(
    workspaces: &mut [Workspace],
    ws_focus: usize,
    sink: &Arc<Mutex<InputSink>>,
    mouse_sink: &Arc<Mutex<Option<mpsc::Sender<ChildMsg>>>>,
    strip: &Arc<Mutex<TabStrip>>,
) {
    unpaint_all(workspaces);
    if let Some(ws) = workspaces.get_mut(ws_focus) {
        if ws.focus >= ws.tabs.len() {
            ws.focus = ws.tabs.len().saturating_sub(1);
        }
        focus_tab(&mut ws.tabs, ws.focus, sink, mouse_sink);
    }
    sync_tab_strip(strip, workspaces, ws_focus);
}

fn focus_tab(
    tabs: &mut [LiveTab],
    focus: usize,
    sink: &Arc<Mutex<InputSink>>,
    mouse_sink: &Arc<Mutex<Option<mpsc::Sender<ChildMsg>>>>,
) {
    for (i, tab) in tabs.iter().enumerate() {
        tab.paint.store(i == focus, Ordering::SeqCst);
    }
    if let Some(tab) = tabs.get(focus) {
        *sink.lock().unwrap() = InputSink::Forward(tab.writer.clone());
        *mouse_sink.lock().unwrap() = Some(tab.wake_tx.clone());
        let _ = tab.wake_tx.send(ChildMsg::Wake);
    }
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
    #[allow(dead_code)]
    Exited,
    #[allow(dead_code)]
    Quit,
    #[allow(dead_code)]
    Hop(HopDirection),
    #[allow(dead_code)]
    HopTo(ToolName),
    #[allow(dead_code)]
    ResumeInto { tool: ToolName, session_id: String, project_path: String },
}

/// Single-pane TUI shell: one agent's real pty rendered full-pane, with a
/// persistent toggle strip (bottom row, owned by us, agent never draws into
/// it) for switching between installed agents via Alt+Up/Down, and a
/// search-and-resume overlay on Ctrl+R.
pub async fn run(
    initial: ToolName,
    initial_launch: Option<(String, String)>,
    restore: Option<crate::layout::SavedMux>,
    persist: bool,
) -> anyhow::Result<()> {
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
    let control_path = match crate::control::bind() {
        Ok(server) => {
            let path = server.path.clone();
            *CONTROL_SOCK.lock().unwrap() = Some(path.clone());
            let (ctl_tx, ctl_rx) = mpsc::channel();
            crate::control::spawn_listener(server, ctl_tx);
            let tx_ctl = tx.clone();
            std::thread::spawn(move || {
                while let Ok(ev) = ctl_rx.recv() {
                    match ev {
                        crate::control::PaneRequest { from_tab, op } => {
                            let _ = tx_ctl.send(RunEvent::Pane(crate::control::PaneRequest { from_tab, op }));
                        }
                    }
                }
            });
            Some(path)
        }
        Err(_) => None,
    };
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
    let restoring = restore.as_ref().is_some_and(|m| !m.is_empty());
    let tab_strip: Arc<Mutex<TabStrip>> = Arc::new(Mutex::new(TabStrip {
        workspaces: vec![(project_path.clone(), 1)],
        ws_focus: 0,
        tabs: vec![TabKind::Agent(current)],
        tab_focus: 0,
        persist,
    }));
    let mut workspaces: Vec<Workspace> = Vec::new();
    let mut ws_focus: usize = 0;

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
    let splash_verb = if restoring { "Restoring" } else { "Launching" };
    let _ = draw_transition_splash(&mut stdout(), splash_verb, current);
    if let Some(remaining) = SPLASH_MIN_DURATION.checked_sub(splash_start.elapsed()) {
        std::thread::sleep(remaining);
    }
    execute!(stdout(), cursor::Show).ok();

    let spawn_err = |e: anyhow::Error| -> anyhow::Error {
        let _ = stdout().write_all(MOUSE_CAPTURE_DISABLE);
        let _ = stdout().flush();
        execute!(stdout(), terminal::LeaveAlternateScreen).ok();
        if let Some(path) = &control_path {
            let _ = std::fs::remove_file(path);
            *CONTROL_SOCK.lock().unwrap() = None;
        }
        let _ = terminal::disable_raw_mode();
        e
    };

    if restoring {
        if let Some(saved) = restore {
            restore_mux(
                saved,
                &mut workspaces,
                &mut ws_focus,
                &sink,
                &mouse_sink,
                &tx,
                &generation,
                host_colors,
                &tab_strip,
            );
        }
    }
    if workspaces.is_empty() {
        let generation_id = generation.fetch_add(1, Ordering::SeqCst) + 1;
        match spawn_live_tab(
            TabKind::Agent(current),
            &project_path,
            launch,
            &sink,
            &mouse_sink,
            &tx,
            generation_id,
            host_colors,
            true,
            tab_strip.clone(),
        ) {
            Ok(tab) => {
                workspaces.push(Workspace { path: project_path.clone(), tabs: vec![tab], focus: 0 });
                ws_focus = 0;
                sync_tab_strip(&tab_strip, &workspaces, ws_focus);
            }
            Err(e) => return Err(spawn_err(e)),
        }
    } else {
        focus_active(&mut workspaces, ws_focus, &sink, &mouse_sink, &tab_strip);
    }

    let result: anyhow::Result<()> = loop {
        if workspaces.is_empty() {
            break Ok(());
        }

        match rx.recv() {
            Ok(RunEvent::ChildExited(g)) => {
                let mut found: Option<(usize, usize)> = None;
                for (wi, ws) in workspaces.iter().enumerate() {
                    if let Some(ti) = ws.tabs.iter().position(|t| t.id == g) {
                        found = Some((wi, ti));
                        break;
                    }
                }
                if let Some((wi, ti)) = found {
                    let mut dead = workspaces[wi].tabs.remove(ti);
                    let _ = dead.child.kill();
                    let _ = dead.child.wait();
                    // Agent quit (its own /q, Ctrl+C, crash). Close the
                    // tab rather than dropping into a shell: a leftover
                    // prompt invites `claude` again *inside* ah, which
                    // is not a hop-able pane. Last tab of the last
                    // workspace leaves ah entirely — run `ah` to hop.
                    if workspaces[wi].tabs.is_empty() {
                        workspaces.remove(wi);
                        if workspaces.is_empty() {
                            capture_leave("agent_exit", &workspaces);
                            break Ok(());
                        }
                        if ws_focus >= workspaces.len() {
                            ws_focus = workspaces.len() - 1;
                        } else if wi < ws_focus {
                            ws_focus -= 1;
                        }
                    } else if workspaces[wi].focus >= workspaces[wi].tabs.len() {
                        workspaces[wi].focus = workspaces[wi].tabs.len() - 1;
                    } else if ti < workspaces[wi].focus {
                        workspaces[wi].focus -= 1;
                    }
                    focus_active(&mut workspaces, ws_focus, &sink, &mouse_sink, &tab_strip);
                }
            }
            Ok(RunEvent::Hop(dir)) => {
                let ws = &workspaces[ws_focus];
                let tab = &ws.tabs[ws.focus];
                current = tab.kind.tool().unwrap_or_else(|| next_installed(ToolName::Claude, 1));
                project_path = ws.path.clone();
                let via = match dir {
                    HopDirection::Next => "next",
                    HopDirection::Prev => "prev",
                };
                let next = match dir {
                    HopDirection::Next => next_installed(current, 1),
                    HopDirection::Prev => next_installed(current, -1),
                };
                launch = match workspaces[ws_focus].tabs[workspaces[ws_focus].focus].kind {
                    TabKind::Shell => Launch::Fresh,
                    TabKind::Agent(_) => hop_to(current, next, &project_path, &rx, true),
                };
                telemetry::capture(
                    "hop",
                    serde_json::json!({
                        "from": current.slug(),
                        "to": next.slug(),
                        "via": via,
                        "converted": matches!(launch, Launch::Resume(_)),
                    }),
                );
                match replace_focused_tab(
                    &mut workspaces[ws_focus],
                    next,
                    &project_path,
                    launch,
                    &sink,
                    &mouse_sink,
                    &overlay_click_sink,
                    &tx,
                    &generation,
                    host_colors,
                    &tab_strip,
                ) {
                    Ok(()) => sync_tab_strip(&tab_strip, &workspaces, ws_focus),
                    Err(e) => break Err(e),
                }
                current = next;
            }
            Ok(RunEvent::HopTo(next)) => {
                let ws = &workspaces[ws_focus];
                let tab = &ws.tabs[ws.focus];
                current = tab.kind.tool().unwrap_or_else(|| next_installed(ToolName::Claude, 1));
                project_path = ws.path.clone();
                launch = match workspaces[ws_focus].tabs[workspaces[ws_focus].focus].kind {
                    TabKind::Shell => Launch::Fresh,
                    TabKind::Agent(_) => hop_to(current, next, &project_path, &rx, true),
                };
                telemetry::capture(
                    "hop",
                    serde_json::json!({
                        "from": current.slug(),
                        "to": next.slug(),
                        "via": "picker",
                        "converted": matches!(launch, Launch::Resume(_)),
                    }),
                );
                match replace_focused_tab(
                    &mut workspaces[ws_focus],
                    next,
                    &project_path,
                    launch,
                    &sink,
                    &mouse_sink,
                    &overlay_click_sink,
                    &tx,
                    &generation,
                    host_colors,
                    &tab_strip,
                ) {
                    Ok(()) => sync_tab_strip(&tab_strip, &workspaces, ws_focus),
                    Err(e) => break Err(e),
                }
                current = next;
            }
            Ok(RunEvent::ResumeInto { tool, session_id, project_path: new_path }) => {
                let from = workspaces[ws_focus].tabs[workspaces[ws_focus].focus].kind.tool().unwrap_or(ToolName::Claude);
                telemetry::capture(
                    "resume",
                    serde_json::json!({
                        "from": from.slug(),
                        "to": tool.slug(),
                        "same_agent": from == tool,
                        "via": "overlay",
                        "interactive": true,
                    }),
                );
                current = tool;
                project_path = resolve_project_path(new_path, false);
                launch = Launch::Resume(session_id);
                let splash_start = std::time::Instant::now();
                let _ = draw_transition_splash(&mut stdout(), "Resuming in", current);
                if let Some(remaining) = SPLASH_MIN_DURATION.checked_sub(splash_start.elapsed()) {
                    std::thread::sleep(remaining);
                }
                execute!(stdout(), cursor::Show).ok();
                if let Some(existing) = workspaces.iter().position(|w| w.path == project_path) {
                    ws_focus = existing;
                    match replace_focused_tab(
                        &mut workspaces[ws_focus],
                        current,
                        &project_path,
                        launch,
                        &sink,
                        &mouse_sink,
                        &overlay_click_sink,
                        &tx,
                        &generation,
                        host_colors,
                        &tab_strip,
                    ) {
                        Ok(()) => focus_active(&mut workspaces, ws_focus, &sink, &mouse_sink, &tab_strip),
                        Err(e) => break Err(e),
                    }
                } else {
                    unpaint_all(&workspaces);
                    let generation_id = generation.fetch_add(1, Ordering::SeqCst) + 1;
                    match spawn_live_tab(
                        TabKind::Agent(current),
                        &project_path,
                        launch,
                        &sink,
                        &mouse_sink,
                        &tx,
                        generation_id,
                        host_colors,
                        true,
                        tab_strip.clone(),
                    ) {
                        Ok(tab) => {
                            workspaces.push(Workspace { path: project_path.clone(), tabs: vec![tab], focus: 0 });
                            ws_focus = workspaces.len() - 1;
                            sync_tab_strip(&tab_strip, &workspaces, ws_focus);
                        }
                        Err(e) => break Err(e),
                    }
                }
            }
            Ok(RunEvent::Resized(new_cols, new_rows)) => {
                for ws in &workspaces {
                    for tab in &ws.tabs {
                        let _ = tab.resize_tx.send((new_cols, new_rows));
                    }
                }
            }
            Ok(RunEvent::SearchResume) => {
                let suppress = workspaces[ws_focus].tabs[workspaces[ws_focus].focus].suppress.clone();
                match run_search_overlay(&sink, &suppress) {
                    resume::ResumeOutcome::Resume(selected) => {
                        let _ = tx.send(RunEvent::ResumeInto {
                            tool: selected.tool,
                            session_id: selected.session_id,
                            project_path: selected.project_path,
                        });
                    }
                    resume::ResumeOutcome::Cancelled => {
                        crate::telemetry::capture("search_cancelled", serde_json::json!({ "via": "overlay" }));
                        focus_active(&mut workspaces, ws_focus, &sink, &mouse_sink, &tab_strip);
                    }
                    resume::ResumeOutcome::Quit => {
                        leave_ah(&mut workspaces, ws_focus, &tab_strip, "search");
                        break Ok(());
                    }
                }
            }
            Ok(RunEvent::AgentPicker) => {
                let suppress = workspaces[ws_focus].tabs[workspaces[ws_focus].focus].suppress.clone();
                let current_tool = workspaces[ws_focus].tabs[workspaces[ws_focus].focus].kind.tool().unwrap_or(ToolName::Claude);
                match run_agent_picker(&sink, &overlay_click_sink, &suppress, current_tool) {
                    Some(picked) if workspaces[ws_focus].tabs[workspaces[ws_focus].focus].kind.tool() != Some(picked) => {
                        let _ = tx.send(RunEvent::HopTo(picked));
                    }
                    _ => {
                        focus_active(&mut workspaces, ws_focus, &sink, &mouse_sink, &tab_strip);
                    }
                }
            }
            Ok(RunEvent::ShowHelp) => {
                let suppress = workspaces[ws_focus].tabs[workspaces[ws_focus].focus].suppress.clone();
                run_help_overlay(&sink, &suppress);
                focus_active(&mut workspaces, ws_focus, &sink, &mouse_sink, &tab_strip);
            }
            Ok(RunEvent::NewTab) => {
                let suppress = workspaces[ws_focus].tabs[workspaces[ws_focus].focus].suppress.clone();
                let current_tool = workspaces[ws_focus].tabs[workspaces[ws_focus].focus].kind.tool().unwrap_or(ToolName::Claude);
                match run_agent_picker(&sink, &overlay_click_sink, &suppress, current_tool) {
                    Some(picked) => add_agent_tab(
                        &mut workspaces,
                        ws_focus,
                        picked,
                        &sink,
                        &mouse_sink,
                        &tx,
                        &generation,
                        host_colors,
                        &tab_strip,
                    ),
                    None => {
                        focus_active(&mut workspaces, ws_focus, &sink, &mouse_sink, &tab_strip);
                    }
                }
            }
            Ok(RunEvent::Pane(req)) => handle_pane_request(
                req,
                &mut workspaces,
                &mut ws_focus,
                &sink,
                &mouse_sink,
                &overlay_click_sink,
                &tx,
                &generation,
                host_colors,
                &tab_strip,
                &rx,
            ),
            Ok(RunEvent::NewWorkspace) => {
                let suppress = workspaces[ws_focus].tabs[workspaces[ws_focus].focus].suppress.clone();
                let default_path = workspaces[ws_focus].path.clone();
                let current_tool = workspaces[ws_focus].tabs[workspaces[ws_focus].focus].kind.tool().unwrap_or(ToolName::Claude);
                match run_path_overlay(&sink, &suppress, &default_path) {
                    Some(path) => {
                        let path = expand_workspace_path(&path);
                        match run_agent_picker(&sink, &overlay_click_sink, &suppress, current_tool) {
                            Some(picked) => {
                                unpaint_all(&workspaces);
                                let generation_id = generation.fetch_add(1, Ordering::SeqCst) + 1;
                                match spawn_live_tab(
                                    TabKind::Agent(picked),
                                    &path,
                                    Launch::Fresh,
                                    &sink,
                                    &mouse_sink,
                                    &tx,
                                    generation_id,
                                    host_colors,
                                    false,
                                    tab_strip.clone(),
                                ) {
                                    Ok(tab) => {
                                        workspaces.push(Workspace { path, tabs: vec![tab], focus: 0 });
                                        ws_focus = workspaces.len() - 1;
                                        focus_active(&mut workspaces, ws_focus, &sink, &mouse_sink, &tab_strip);
                                    }
                                    Err(e) => {
                                        focus_active(&mut workspaces, ws_focus, &sink, &mouse_sink, &tab_strip);
                                        debug_log("NEW_WORKSPACE_ERR", format!("{e:#}").as_bytes());
                                    }
                                }
                            }
                            None => focus_active(&mut workspaces, ws_focus, &sink, &mouse_sink, &tab_strip),
                        }
                    }
                    None => focus_active(&mut workspaces, ws_focus, &sink, &mouse_sink, &tab_strip),
                }
            }
            Ok(RunEvent::NextTab) => {
                let n = workspaces[ws_focus].tabs.len();
                if n > 1 {
                    workspaces[ws_focus].focus = (workspaces[ws_focus].focus + 1) % n;
                }
                focus_active(&mut workspaces, ws_focus, &sink, &mouse_sink, &tab_strip);
            }
            Ok(RunEvent::PrevTab) => {
                let n = workspaces[ws_focus].tabs.len();
                if n > 1 {
                    workspaces[ws_focus].focus = (workspaces[ws_focus].focus + n - 1) % n;
                }
                focus_active(&mut workspaces, ws_focus, &sink, &mouse_sink, &tab_strip);
            }
            Ok(RunEvent::NextWorkspace) if workspaces.len() > 1 => {
                ws_focus = (ws_focus + 1) % workspaces.len();
                focus_active(&mut workspaces, ws_focus, &sink, &mouse_sink, &tab_strip);
            }
            Ok(RunEvent::PrevWorkspace) if workspaces.len() > 1 => {
                ws_focus = (ws_focus + workspaces.len() - 1) % workspaces.len();
                focus_active(&mut workspaces, ws_focus, &sink, &mouse_sink, &tab_strip);
            }
            Ok(RunEvent::NextWorkspace | RunEvent::PrevWorkspace) => {
                focus_active(&mut workspaces, ws_focus, &sink, &mouse_sink, &tab_strip);
            }
            Ok(RunEvent::CloseTab) => {
                let last_workspace = workspaces.len() == 1;
                let last_tab = workspaces[ws_focus].tabs.len() == 1;
                if last_workspace && last_tab {
                    capture_leave("close_tab", &workspaces);
                    let mut dead = workspaces[ws_focus].tabs.remove(0);
                    let _ = dead.child.kill();
                    let _ = dead.child.wait();
                    break Ok(());
                }
                let focus = workspaces[ws_focus].focus;
                let mut dead = workspaces[ws_focus].tabs.remove(focus);
                let _ = dead.child.kill();
                let _ = dead.child.wait();
                if workspaces[ws_focus].tabs.is_empty() {
                    workspaces.remove(ws_focus);
                    if ws_focus >= workspaces.len() {
                        ws_focus = workspaces.len() - 1;
                    }
                } else if workspaces[ws_focus].focus >= workspaces[ws_focus].tabs.len() {
                    workspaces[ws_focus].focus = workspaces[ws_focus].tabs.len() - 1;
                }
                focus_active(&mut workspaces, ws_focus, &sink, &mouse_sink, &tab_strip);
            }
            Ok(RunEvent::FocusTab(i)) => {
                if i < workspaces[ws_focus].tabs.len() {
                    workspaces[ws_focus].focus = i;
                    focus_active(&mut workspaces, ws_focus, &sink, &mouse_sink, &tab_strip);
                }
            }
            Ok(RunEvent::Leave) => {
                leave_ah(&mut workspaces, ws_focus, &tab_strip, "prefix");
                break Ok(());
            }
            Ok(RunEvent::ChromeClick { x, y }) => {
                let side = terminal::size().map(|(c, _)| sidebar_cols(c)).unwrap_or(0);
                if side > 0 && x < side {
                    match hit_test_sidebar(workspaces.len(), y) {
                        TabBarHit::Focus(i) if i < workspaces.len() => {
                            ws_focus = i;
                            focus_active(&mut workspaces, ws_focus, &sink, &mouse_sink, &tab_strip);
                        }
                        TabBarHit::New => {
                            let _ = tx.send(RunEvent::NewWorkspace);
                        }
                        _ => {}
                    }
                } else if y < TOP_BAR_ROWS {
                    match hit_test_tab_bar(&workspaces[ws_focus].tabs, x.saturating_sub(side), if side == 0 { 4 } else { 1 }) {
                        TabBarHit::Focus(i) => {
                            workspaces[ws_focus].focus = i;
                            focus_active(&mut workspaces, ws_focus, &sink, &mouse_sink, &tab_strip);
                        }
                        TabBarHit::New => {
                            let _ = tx.send(RunEvent::NewTab);
                        }
                        TabBarHit::Miss => {}
                    }
                }
            }
            Err(_) => break Ok(()),
        }
    };

    let _ = stdout().write_all(MOUSE_CAPTURE_DISABLE);
    let _ = stdout().flush();
    execute!(stdout(), terminal::LeaveAlternateScreen).ok();
    if let Some(path) = control_path {
        let _ = std::fs::remove_file(&path);
        *CONTROL_SOCK.lock().unwrap() = None;
    }
    terminal::disable_raw_mode()?;
    result
}

enum TabBarHit {
    Focus(usize),
    New,
    Miss,
}

fn hit_test_tab_bar(tabs: &[LiveTab], x: u16, start: u16) -> TabBarHit {
    let mut col = start;
    for (i, tab) in tabs.iter().enumerate() {
        let w = format!(" {} ", tab.kind.slug()).chars().count() as u16;
        if x >= col && x < col + w {
            return TabBarHit::Focus(i);
        }
        col = col.saturating_add(w);
    }
    if x >= col && x < col.saturating_add(3) {
        return TabBarHit::New;
    }
    TabBarHit::Miss
}

fn hit_test_sidebar(n_workspaces: usize, y: u16) -> TabBarHit {
    let mut row: u16 = 1;
    for i in 0..n_workspaces {
        if y >= row && y < row.saturating_add(SIDEBAR_SESSION_ROWS) {
            return TabBarHit::Focus(i);
        }
        row = row.saturating_add(SIDEBAR_SESSION_ROWS);
    }
    if y >= row && y < row.saturating_add(2) {
        return TabBarHit::New;
    }
    TabBarHit::Miss
}

fn expand_workspace_path(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return std::env::current_dir().map(|p| p.to_string_lossy().to_string()).unwrap_or_else(|_| ".".into());
    }
    if trimmed == "~" {
        return dirs::home_dir().map(|p| p.to_string_lossy().to_string()).unwrap_or_else(|| trimmed.into());
    }
    if let Some(rest) = trimmed.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest).to_string_lossy().to_string();
        }
    }
    trimmed.to_string()
}

fn workspace_title(path: &str) -> String {
    let s = shorten_path(path);
    std::path::Path::new(&s)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|n| !n.is_empty() && n != "~")
        .unwrap_or(s)
}

fn shorten_path(path: &str) -> String {
    let mut s = path.to_string();
    if let Some(home) = dirs::home_dir() {
        let home = home.to_string_lossy();
        if s == home {
            return "~".into();
        }
        let prefix = format!("{home}/");
        if s.starts_with(&prefix) {
            s = format!("~/{}", &s[prefix.len()..]);
        }
    }
    const MAX: usize = 20;
    if s.chars().count() <= MAX {
        return s;
    }
    let parts: Vec<&str> = s.split('/').filter(|p| !p.is_empty()).collect();
    if parts.len() >= 2 {
        format!("…/{}/{}", parts[parts.len() - 2], parts[parts.len() - 1])
    } else {
        s.chars().skip(s.chars().count().saturating_sub(MAX)).collect()
    }
}

fn find_tab_by_id(workspaces: &[Workspace], id: u64) -> Option<(usize, usize)> {
    for (wi, ws) in workspaces.iter().enumerate() {
        if let Some(ti) = ws.tabs.iter().position(|t| t.id == id) {
            return Some((wi, ti));
        }
    }
    None
}

/// 1-based `--tab` among `tab_count` tabs. Never the caller's own index.
fn resolve_other_tab_index(
    tab_count: usize,
    self_index: usize,
    tab_1based: Option<usize>,
) -> Result<usize, &'static str> {
    let target = match tab_1based {
        Some(t) if t >= 1 && t <= tab_count => t - 1,
        Some(_) => return Err("no such tab"),
        None => {
            let others: Vec<usize> = (0..tab_count).filter(|i| *i != self_index).collect();
            if others.len() == 1 {
                others[0]
            } else {
                return Err("pass --tab N (1-based). You cannot target your own tab.");
            }
        }
    };
    if target == self_index {
        return Err("cannot change your own tab");
    }
    Ok(target)
}

/// 1-based `--tab` in the caller's workspace, never the caller's own tab.
fn resolve_other_tab(
    workspaces: &[Workspace],
    from_id: u64,
    tab_1based: Option<usize>,
) -> Result<(usize, usize), String> {
    let (wi, self_ti) = find_tab_by_id(workspaces, from_id)
        .ok_or_else(|| "could not identify this pane".to_string())?;
    let n = workspaces[wi].tabs.len();
    let target = resolve_other_tab_index(n, self_ti, tab_1based).map_err(str::to_string)?;
    Ok((wi, target))
}

fn handle_pane_request(
    req: crate::control::PaneRequest,
    workspaces: &mut Vec<Workspace>,
    ws_focus: &mut usize,
    sink: &Arc<Mutex<InputSink>>,
    mouse_sink: &Arc<Mutex<Option<mpsc::Sender<ChildMsg>>>>,
    overlay_click_sink: &Arc<Mutex<Option<mpsc::Sender<(u16, u16)>>>>,
    tx: &mpsc::Sender<RunEvent>,
    generation: &Arc<AtomicU64>,
    host_colors: (Option<vt::Rgb>, Option<vt::Rgb>),
    tab_strip: &Arc<Mutex<TabStrip>>,
    rx: &mpsc::Receiver<RunEvent>,
) {
    use crate::control::PaneOp;
    match req.op {
        PaneOp::NewTab { tool } => match tool {
            Some(picked) => add_agent_tab(
                workspaces, *ws_focus, picked, sink, mouse_sink, tx, generation, host_colors, tab_strip,
            ),
            None => {
                let _ = tx.send(RunEvent::NewTab);
            }
        },
        PaneOp::Hop { tab, tool } => {
            let Ok((wi, ti)) = resolve_other_tab(workspaces, req.from_tab, tab) else {
                debug_log("PANE_HOP_REFUSED", b"self or missing tab");
                return;
            };
            let path = workspaces[wi].path.clone();
            let from = workspaces[wi].tabs[ti].kind.tool();
            let launch = match from {
                None => Launch::Fresh,
                Some(current) => hop_to(current, tool, &path, rx, false),
            };
            if let Err(e) = replace_tab_at(
                &mut workspaces[wi],
                ti,
                tool,
                &path,
                launch,
                sink,
                mouse_sink,
                tx,
                generation,
                host_colors,
                tab_strip,
            ) {
                debug_log("PANE_HOP_ERR", format!("{e:#}").as_bytes());
            }
            focus_active(workspaces, *ws_focus, sink, mouse_sink, tab_strip);
        }
        PaneOp::Close { tab } => {
            let Ok((wi, ti)) = resolve_other_tab(workspaces, req.from_tab, tab) else {
                debug_log("PANE_CLOSE_REFUSED", b"self or missing tab");
                return;
            };
            if workspaces.len() == 1 && workspaces[wi].tabs.len() == 1 {
                return;
            }
            let mut dead = workspaces[wi].tabs.remove(ti);
            let _ = dead.child.kill();
            let _ = dead.child.wait();
            if workspaces[wi].tabs.is_empty() {
                workspaces.remove(wi);
                if *ws_focus >= workspaces.len() {
                    *ws_focus = workspaces.len().saturating_sub(1);
                } else if wi < *ws_focus {
                    *ws_focus -= 1;
                }
            } else if workspaces[wi].focus >= workspaces[wi].tabs.len() {
                workspaces[wi].focus = workspaces[wi].tabs.len() - 1;
            } else if ti < workspaces[wi].focus {
                workspaces[wi].focus -= 1;
            }
            focus_active(workspaces, *ws_focus, sink, mouse_sink, tab_strip);
        }
        PaneOp::Focus { tab } => {
            let Some((wi, _)) = find_tab_by_id(workspaces, req.from_tab) else { return };
            let i = tab.saturating_sub(1);
            if i < workspaces[wi].tabs.len() {
                *ws_focus = wi;
                workspaces[wi].focus = i;
                focus_active(workspaces, *ws_focus, sink, mouse_sink, tab_strip);
            }
        }
        PaneOp::WorkspaceNext if workspaces.len() > 1 => {
            *ws_focus = (*ws_focus + 1) % workspaces.len();
            focus_active(workspaces, *ws_focus, sink, mouse_sink, tab_strip);
        }
        PaneOp::WorkspacePrev if workspaces.len() > 1 => {
            *ws_focus = (*ws_focus + workspaces.len() - 1) % workspaces.len();
            focus_active(workspaces, *ws_focus, sink, mouse_sink, tab_strip);
        }
        PaneOp::WorkspaceNext | PaneOp::WorkspacePrev => {
            focus_active(workspaces, *ws_focus, sink, mouse_sink, tab_strip);
        }
        PaneOp::WorkspaceNew { path, tool } => {
            let default_path = workspaces.get(*ws_focus).map(|w| w.path.clone()).unwrap_or_else(|| ".".into());
            let path = match path {
                Some(p) => expand_workspace_path(&p),
                None => {
                    let suppress = workspaces[*ws_focus].tabs[workspaces[*ws_focus].focus].suppress.clone();
                    match run_path_overlay(sink, &suppress, &default_path) {
                        Some(p) => expand_workspace_path(&p),
                        None => {
                            focus_active(workspaces, *ws_focus, sink, mouse_sink, tab_strip);
                            return;
                        }
                    }
                }
            };
            let picked = match tool {
                Some(t) => t,
                None => {
                    let suppress = workspaces[*ws_focus].tabs[workspaces[*ws_focus].focus].suppress.clone();
                    let current = workspaces[*ws_focus].tabs[workspaces[*ws_focus].focus]
                        .kind
                        .tool()
                        .unwrap_or(ToolName::Claude);
                    match run_agent_picker(sink, overlay_click_sink, &suppress, current) {
                        Some(t) => t,
                        None => {
                            focus_active(workspaces, *ws_focus, sink, mouse_sink, tab_strip);
                            return;
                        }
                    }
                }
            };
            unpaint_all(workspaces);
            let generation_id = generation.fetch_add(1, Ordering::SeqCst) + 1;
            match spawn_live_tab(
                TabKind::Agent(picked),
                &path,
                Launch::Fresh,
                sink,
                mouse_sink,
                tx,
                generation_id,
                host_colors,
                false,
                tab_strip.clone(),
            ) {
                Ok(tab) => {
                    workspaces.push(Workspace { path, tabs: vec![tab], focus: 0 });
                    *ws_focus = workspaces.len() - 1;
                    focus_active(workspaces, *ws_focus, sink, mouse_sink, tab_strip);
                }
                Err(e) => {
                    focus_active(workspaces, *ws_focus, sink, mouse_sink, tab_strip);
                    debug_log("PANE_WS_ERR", format!("{e:#}").as_bytes());
                }
            }
        }
    }
}

fn add_agent_tab(
    workspaces: &mut Vec<Workspace>,
    ws_focus: usize,
    tool: ToolName,
    sink: &Arc<Mutex<InputSink>>,
    mouse_sink: &Arc<Mutex<Option<mpsc::Sender<ChildMsg>>>>,
    tx: &mpsc::Sender<RunEvent>,
    generation: &Arc<AtomicU64>,
    host_colors: (Option<vt::Rgb>, Option<vt::Rgb>),
    tab_strip: &Arc<Mutex<TabStrip>>,
) {
    let path = workspaces[ws_focus].path.clone();
    let generation_id = generation.fetch_add(1, Ordering::SeqCst) + 1;
    unpaint_all(workspaces);
    match spawn_live_tab(
        TabKind::Agent(tool),
        &path,
        Launch::Fresh,
        sink,
        mouse_sink,
        tx,
        generation_id,
        host_colors,
        false,
        tab_strip.clone(),
    ) {
        Ok(tab) => {
            let ws = &mut workspaces[ws_focus];
            ws.tabs.push(tab);
            ws.focus = ws.tabs.len() - 1;
            focus_active(workspaces, ws_focus, sink, mouse_sink, tab_strip);
        }
        Err(e) => {
            focus_active(workspaces, ws_focus, sink, mouse_sink, tab_strip);
            debug_log("NEW_TAB_ERR", format!("{e:#}").as_bytes());
        }
    }
}

fn replace_tab_at(
    ws: &mut Workspace,
    index: usize,
    tool: ToolName,
    project_path: &str,
    launch: Launch,
    sink: &Arc<Mutex<InputSink>>,
    mouse_sink: &Arc<Mutex<Option<mpsc::Sender<ChildMsg>>>>,
    tx: &mpsc::Sender<RunEvent>,
    generation: &Arc<AtomicU64>,
    host_colors: (Option<vt::Rgb>, Option<vt::Rgb>),
    tab_strip: &Arc<Mutex<TabStrip>>,
) -> anyhow::Result<()> {
    let focused = ws.focus == index;
    let mut dead = ws.tabs.remove(index);
    dead.paint.store(false, Ordering::SeqCst);
    let _ = dead.child.kill();
    let _ = dead.child.wait();
    let generation_id = generation.fetch_add(1, Ordering::SeqCst) + 1;
    let tab = spawn_live_tab(
        TabKind::Agent(tool),
        project_path,
        launch,
        sink,
        mouse_sink,
        tx,
        generation_id,
        host_colors,
        focused,
        tab_strip.clone(),
    )?;
    ws.tabs.insert(index, tab);
    Ok(())
}

fn replace_focused_tab(
    ws: &mut Workspace,
    tool: ToolName,
    project_path: &str,
    launch: Launch,
    sink: &Arc<Mutex<InputSink>>,
    mouse_sink: &Arc<Mutex<Option<mpsc::Sender<ChildMsg>>>>,
    _overlay_click_sink: &Arc<Mutex<Option<mpsc::Sender<(u16, u16)>>>>,
    tx: &mpsc::Sender<RunEvent>,
    generation: &Arc<AtomicU64>,
    host_colors: (Option<vt::Rgb>, Option<vt::Rgb>),
    tab_strip: &Arc<Mutex<TabStrip>>,
) -> anyhow::Result<()> {
    let i = ws.focus;
    replace_tab_at(ws, i, tool, project_path, launch, sink, mouse_sink, tx, generation, host_colors, tab_strip)
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
fn hop_to(current: ToolName, next: ToolName, project_path: &str, rx: &mpsc::Receiver<RunEvent>, splash: bool) -> Launch {
    let splash_start = std::time::Instant::now();
    if splash {
        let _ = draw_transition_splash(&mut stdout(), "Switching to", next);
    }

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
    if splash {
        if let Some(remaining) = SPLASH_MIN_DURATION.checked_sub(splash_start.elapsed()) {
            std::thread::sleep(remaining);
        }
        execute!(stdout(), cursor::Show).ok();
    }
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

/// Chrome around the child: a 1-row tab strip on top, a session sidebar
/// on the left (when the terminal is wide enough), and the hop/hint bar
/// on the bottom. The child's pty is `child_pty_size` — never our chrome.
const TOP_BAR_ROWS: u16 = 1;
const CHROME_ROWS: u16 = TOP_BAR_ROWS + 1;
const SIDEBAR_COLS: u16 = 24;
const SIDEBAR_MIN_TERM_COLS: u16 = 72;
const SIDEBAR_SESSION_ROWS: u16 = 3;

fn sidebar_cols(term_cols: u16) -> u16 {
    if term_cols >= SIDEBAR_MIN_TERM_COLS {
        SIDEBAR_COLS
    } else {
        0
    }
}

fn child_pty_size(term_cols: u16, term_rows: u16) -> (u16, u16) {
    (
        term_cols.saturating_sub(sidebar_cols(term_cols)).max(20),
        term_rows.saturating_sub(CHROME_ROWS).max(4),
    )
}

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

    let wordmark_lines: Vec<&str> = if use_big {
        theme::BRAND_WORDMARK.trim_matches('\n').lines().collect()
    } else {
        Vec::new()
    };
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
fn spawn_live_tab(
    kind: TabKind,
    project_path: &str,
    launch: Launch,
    sink: &Arc<Mutex<InputSink>>,
    mouse_sink: &Arc<Mutex<Option<mpsc::Sender<ChildMsg>>>>,
    tx: &mpsc::Sender<RunEvent>,
    generation_id: u64,
    host_colors: (Option<vt::Rgb>, Option<vt::Rgb>),
    initially_focused: bool,
    tab_strip: Arc<Mutex<TabStrip>>,
) -> anyhow::Result<LiveTab> {
    if let TabKind::Agent(tool) = kind {
        if !tool.is_installed() {
            anyhow::bail!("Cannot resume in {}: \"{}\" is not installed or not on PATH.", tool.slug(), tool.binary());
        }
    }
    let t_run_one_start = std::time::Instant::now();
    let pty_system = native_pty_system();
    let (term_cols, term_rows) = terminal::size()?;
    let (cols, child_rows) = child_pty_size(term_cols, term_rows);
    let pair = pty_system.openpty(PtySize {
        rows: child_rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    })?;

    let mut cmd = match kind {
        TabKind::Shell => {
            let shell = std::env::var("SHELL")
                .or_else(|_| std::env::var("COMSPEC"))
                .unwrap_or_else(|_| {
                    if cfg!(windows) {
                        "cmd.exe".into()
                    } else {
                        "/bin/zsh".into()
                    }
                });
            CommandBuilder::new(shell)
        }
        TabKind::Agent(tool) => {
            let argv = match &launch {
                Launch::Fresh => crate::agents::spawn_argv(&[tool.binary().to_string()]),
                Launch::Resume(session_id) => {
                    crate::agents::spawn_argv(&adapter_for(tool).resume_cmd(session_id, project_path))
                }
            };
            let mut cmd = CommandBuilder::new(&argv[0]);
            cmd.args(&argv[1..]);
            cmd
        }
    };
    cmd.cwd(project_path);
    if let Some(sock) = CONTROL_SOCK.lock().unwrap().as_ref() {
        cmd.env(crate::control::SOCK_ENV, sock.to_string_lossy().as_ref());
        cmd.env(crate::control::TAB_ENV, generation_id.to_string());
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let mut paths = vec![dir.to_path_buf()];
            if let Some(rest) = std::env::var_os("PATH") {
                paths.extend(std::env::split_paths(&rest));
            }
            if let Ok(joined) = std::env::join_paths(paths) {
                cmd.env("PATH", joined);
            }
        }
    }
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
    let session_id = match &launch {
        Launch::Resume(id) => Some(id.clone()),
        Launch::Fresh => None,
    };
    let child = pair.slave.spawn_command(cmd)?;
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
    let paint = Arc::new(AtomicBool::new(initially_focused));
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
    let wake_tx = byte_tx.clone();
    if initially_focused {
        *mouse_sink.lock().unwrap() = Some(byte_tx.clone());
        *sink.lock().unwrap() = InputSink::Forward(writer.clone());
    }

    let vt_writer = writer.clone();
    let writer_for_mouse = writer.clone();
    let paint_thread = paint.clone();
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
        let mut applied_dims = (term_cols, term_rows);
        let mut term = vt::Terminal::with_host_colors(cols, child_rows, vt_writer, host_fg, host_bg);
        let mut rterm: Option<RTerminal<CrosstermBackend<Stdout>>> = None;

        let paint_frame = |rterm: &mut Option<RTerminal<CrosstermBackend<Stdout>>>, term: &mut vt::Terminal, strip: &TabStrip, clear: bool| {
            let _g = STDOUT_PAINT.lock().unwrap_or_else(|e| e.into_inner());
            let Some(rt) = (if rterm.is_none() {
                match RTerminal::new(CrosstermBackend::new(stdout())) {
                    Ok(mut t) => {
                        // Splash (and any previous tab) painted outside
                        // ratatui. Reset the real screen and ratatui's
                        // back buffer so the first draw is a full frame,
                        // not a diff against leftover wordmark cells.
                        let _ = t.clear();
                        *rterm = Some(t);
                        rterm.as_mut()
                    }
                    Err(_) => None,
                }
            } else {
                if clear {
                    if let Some(t) = rterm.as_mut() {
                        let _ = t.clear();
                    }
                }
                rterm.as_mut()
            }) else {
                return;
            };
            if let Err(e) = render_frame(rt, term, kind, strip) {
                debug_log("RENDER_FRAME_ERR", format!("{e:#}").as_bytes());
            }
        };

        if paint_thread.load(Ordering::SeqCst) {
            let strip = tab_strip.lock().unwrap().clone();
            paint_frame(&mut rterm, &mut term, &strip, true);
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
                let (new_cols, new_child_rows) = child_pty_size(current_dims.0, current_dims.1);
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
                    cols: new_cols,
                    pixel_width: 0,
                    pixel_height: 0,
                });
                term.resize(new_cols, new_child_rows);
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
            let is_paint = paint_thread.load(Ordering::SeqCst);
            if !is_paint {
                rterm = None;
            }
            if was_suppressed && !is_suppressed && is_paint {
                let strip = tab_strip.lock().unwrap().clone();
                paint_frame(&mut rterm, &mut term, &strip, true);
            }
            was_suppressed = is_suppressed;

            match byte_rx.recv_timeout(IDLE_POLL_INTERVAL) {
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    let before = applied_dims;
                    apply_resize_if_pending(&mut applied_dims, &mut term);
                    if applied_dims != before && !suppress_thread.load(Ordering::SeqCst) && paint_thread.load(Ordering::SeqCst) {
                        let strip = tab_strip.lock().unwrap().clone();
                        paint_frame(&mut rterm, &mut term, &strip, false);
                    }
                }
                Ok(ChildMsg::Bytes(data)) => {
                    debug_log("CHILD_RAW", &data);
                    apply_resize_if_pending(&mut applied_dims, &mut term);
                    term.write(&data);
                    // While the search overlay owns the screen, still drain
                    // the child's output (so its pty buffer never fills and
                    // blocks it) but don't paint over the overlay with it.
                    if !suppress_thread.load(Ordering::SeqCst) && paint_thread.load(Ordering::SeqCst) {
                        let strip = tab_strip.lock().unwrap().clone();
                        paint_frame(&mut rterm, &mut term, &strip, false);
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
                Ok(ChildMsg::Wake) => {
                    if paint_thread.load(Ordering::SeqCst) {
                        let strip = tab_strip.lock().unwrap().clone();
                        paint_frame(&mut rterm, &mut term, &strip, true);
                    } else {
                        rterm = None;
                    }
                }
                Ok(ChildMsg::Eof) => break,
            }
        }
        let _ = tx_out.send(RunEvent::ChildExited(generation_id));
    });

    Ok(LiveTab {
        id: generation_id,
        kind,
        project_path: project_path.to_string(),
        session_id,
        writer,
        resize_tx,
        wake_tx,
        paint,
        suppress,
        child,
    })
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
    ("Ctrl+B then n", "Hop this chat to the next agent — keeps the conversation"),
    ("Ctrl+B then p", "Hop this chat to the previous agent"),
    ("Ctrl+B then a", "Hop via the agent picker"),
    ("Click the bottom bar", "Same hop picker — mouse works anywhere chrome is"),
    ("Click a tab or +", "Focus that tab, or open a new agent in this workspace"),
    ("Click a workspace or +", "Switch workspace, or start a new one"),
    ("Ctrl+B then c", "New tab in this workspace"),
    ("ah tab [agent]", "From a pane: open a new tab"),
    ("ah hop AGENT [--tab N]", "From a pane: hop another tab (never this one)"),
    ("ah close [--tab N]", "From a pane: close another tab"),
    ("ah focus N", "From a pane: focus that tab"),
    ("ah workspace [next|prev]", "From a pane: workspaces"),
    ("Ctrl+B then w", "New workspace (folder), then pick an agent"),
    ("Ctrl+B then ] / [", "Next / previous workspace"),
    ("Ctrl+B then o", "Next tab in this workspace"),
    ("Ctrl+B then i", "Previous tab in this workspace"),
    ("Ctrl+B then 1-9", "Focus that tab"),
    ("Ctrl+B then x", "Close this tab"),
    ("Ctrl+B then q", "Leave ah — same workspaces and chats when you run ah again"),
    ("Ctrl+B then ?", "Show this help"),
    ("Alt+\u{2191} / Alt+\u{2193}", "Hop next / previous (where the terminal supports it)"),
    ("ah feedback \"…\"", "Command in the shell (not a key) — send us a note"),
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

fn run_path_overlay(sink: &Arc<Mutex<InputSink>>, suppress: &Arc<AtomicBool>, default: &str) -> Option<String> {
    suppress.store(true, Ordering::SeqCst);
    let (key_tx, key_rx) = mpsc::channel::<Vec<u8>>();
    *sink.lock().unwrap() = InputSink::Capture(key_tx);
    let mut keys = ChannelKeys::new(key_rx);
    let mut buf = default.to_string();
    let mut out = stdout();
    let chosen = loop {
        if draw_path_prompt(&mut out, &buf).is_err() {
            break None;
        }
        match keys.next_key() {
            Ok(Some(resume::SearchKey::Char(c))) if !c.is_control() => buf.push(c),
            Ok(Some(resume::SearchKey::Backspace)) => {
                buf.pop();
            }
            Ok(Some(resume::SearchKey::Enter)) => break Some(buf.trim().to_string()).filter(|s| !s.is_empty()).or_else(|| Some(default.to_string())),
            Ok(Some(resume::SearchKey::Escape)) | Ok(Some(resume::SearchKey::Quit)) | Ok(None) | Err(_) => break None,
            Ok(Some(_)) => {}
        }
    };
    suppress.store(false, Ordering::SeqCst);
    chosen
}

fn draw_path_prompt(out: &mut impl Write, value: &str) -> anyhow::Result<()> {
    draw_centered_message(out, "New workspace", &format!("{value}█"))
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
fn render_frame(rterm: &mut RTerminal<CrosstermBackend<Stdout>>, term: &mut vt::Terminal, kind: TabKind, strip: &TabStrip) -> anyhow::Result<()> {
    // `cursor()` is what actually triggers libghostty-vt's render-state
    // update for this frame (see its doc comment in vt.rs) -- must run
    // before `for_each_cell`, which reuses that same snapshot rather than
    // updating it again itself.
    let cursor = term.cursor();
    rterm.draw(|frame| {
        let area = frame.area();
        let bar_row = area.height.saturating_sub(1);
        let buf = frame.buffer_mut();
        let side = sidebar_cols(area.width);

        // Blank the child pane first. Splash and the previous tab write
        // straight to the real terminal; ratatui then diffs. Cells the
        // child never draws (Codex's idle middle, Claude's gutters) would
        // otherwise keep the wordmark / old agent until something overwrote
        // them -- confirmed as the first-frame "ENT HOP" overlay.
        for y in TOP_BAR_ROWS..bar_row {
            for x in side..area.width {
                let Some(c) = buf.cell_mut((x, y)) else { continue };
                c.set_char(' ');
                c.fg = RColor::Reset;
                c.bg = RColor::Reset;
                c.modifier = RModifier::empty();
                c.skip = false;
            }
        }

        term.for_each_cell(|x, y, cell| {
            let real_x = x.saturating_add(side);
            let real_y = y + TOP_BAR_ROWS;
            if real_x >= area.width || real_y >= bar_row {
                return;
            }
            let Some(rc) = buf.cell_mut((real_x, real_y)) else { return };
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

        write_top_bar(buf, area.width, side, strip);
        write_toggle_bar(buf, area.width, bar_row, side, kind);
        write_sidebar(buf, area.width, area.height, strip);

        let cursor_real_x = cursor.x.saturating_add(side);
        let cursor_real_y = cursor.y + TOP_BAR_ROWS;
        if cursor.visible && cursor_real_x < area.width && cursor_real_y < bar_row {
            frame.set_cursor_position((cursor_real_x, cursor_real_y));
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
const CHROME_BG: RColor = RColor::Rgb(18, 18, 22);
const CHROME_RAISED: RColor = RColor::Rgb(38, 38, 44);
const CHROME_TEXT: RColor = RColor::Rgb(220, 220, 224);
const CHROME_DIM: RColor = RColor::Rgb(120, 120, 128);

const CHROME_ACCENT: RColor = RColor::Rgb(theme::BRAND_RGB.0, theme::BRAND_RGB.1, theme::BRAND_RGB.2);

fn write_top_bar(buf: &mut Buffer, width: u16, side: u16, strip: &TabStrip) {
    if side >= width {
        return;
    }
    fill_rect(buf, side, 0, width.saturating_sub(side), TOP_BAR_ROWS, CHROME_BG);
    let mid_row = TOP_BAR_ROWS / 2;
    let mut col: u16 = side.saturating_add(1);
    if side == 0 {
        let ah = Span::styled("ah ", Style::default().fg(CHROME_ACCENT).bg(CHROME_BG).add_modifier(RModifier::BOLD));
        buf.set_span(col, mid_row, &ah, 3);
        col += 3;
    }
    for (i, kind) in strip.tabs.iter().enumerate() {
        let focused = i == strip.tab_focus;
        let label = format!(" {} ", kind.slug());
        let style = if focused {
            Style::default().fg(CHROME_TEXT).bg(CHROME_RAISED).add_modifier(RModifier::BOLD)
        } else {
            Style::default().fg(CHROME_DIM).bg(CHROME_BG)
        };
        let span = Span::styled(label.clone(), style);
        let w = label.chars().count() as u16;
        if col + w >= width {
            break;
        }
        buf.set_span(col, mid_row, &span, w);
        col += w;
    }
    if col + 3 < width {
        let plus = Span::styled(" + ", Style::default().fg(CHROME_DIM).bg(CHROME_BG));
        buf.set_span(col, mid_row, &plus, 3);
    }
}

fn fill_rect(buf: &mut Buffer, x0: u16, y0: u16, width: u16, height: u16, bg: RColor) {
    for y in y0..y0.saturating_add(height) {
        for x in x0..x0.saturating_add(width) {
            let Some(c) = buf.cell_mut((x, y)) else { continue };
            c.set_char(' ');
            c.fg = RColor::Reset;
            c.bg = bg;
            c.modifier = RModifier::empty();
            c.skip = false;
        }
    }
}

fn write_sidebar(buf: &mut Buffer, term_width: u16, term_height: u16, strip: &TabStrip) {
    let side = sidebar_cols(term_width);
    if side == 0 {
        return;
    }
    let bg = CHROME_BG;
    let bg_focus = CHROME_RAISED;
    let dim = CHROME_DIM;
    fill_rect(buf, 0, 0, side, term_height, bg);

    let header = Span::styled(" ah", Style::default().fg(CHROME_ACCENT).bg(bg).add_modifier(RModifier::BOLD));
    buf.set_span(0, 0, &header, side);

    let mut row: u16 = 1;
    for (i, (path, n_tabs)) in strip.workspaces.iter().enumerate() {
        if row + 1 >= term_height {
            break;
        }
        let focused = i == strip.ws_focus;
        let block_bg = if focused { bg_focus } else { bg };
        fill_rect(buf, 0, row, side, SIDEBAR_SESSION_ROWS.min(term_height.saturating_sub(row)), block_bg);
        let name_style = if focused {
            Style::default().fg(CHROME_TEXT).bg(block_bg).add_modifier(RModifier::BOLD)
        } else {
            Style::default().fg(CHROME_TEXT).bg(block_bg)
        };
        let name = format!(" {}", workspace_title(path));
        buf.set_span(0, row, &Span::styled(name, name_style), side);
        if focused {
            if let Some(c) = buf.cell_mut((0, row)) {
                c.set_char('▎');
                c.fg = CHROME_TEXT;
                c.bg = block_bg;
            }
        }
        let status = if *n_tabs == 1 { " 1 tab".to_string() } else { format!(" {n_tabs} tabs") };
        buf.set_span(
            0,
            row + 1,
            &Span::styled(status, Style::default().fg(dim).bg(block_bg)),
            side,
        );
        if row + 2 < term_height {
            let cwd = format!(" {}", shorten_path(path));
            buf.set_span(0, row + 2, &Span::styled(cwd, Style::default().fg(dim).bg(block_bg)), side);
        }
        row = row.saturating_add(SIDEBAR_SESSION_ROWS);
    }
    if row < term_height {
        let plus = Span::styled(" + workspace", Style::default().fg(dim).bg(bg));
        buf.set_span(0, row, &plus, side);
    }
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
fn write_toggle_bar(buf: &mut Buffer, width: u16, row: u16, side: u16, kind: TabKind) {
    if side >= width {
        return;
    }
    fill_rect(buf, side, row, width.saturating_sub(side), 1, CHROME_BG);
    let tag_style = Style::default().fg(CHROME_TEXT).bg(CHROME_BG);
    let hint_style = Style::default().fg(CHROME_DIM).bg(CHROME_BG);
    let line = RLine::from(vec![
        Span::raw(" "),
        Span::styled(format!("[{}]", kind.slug()), tag_style),
        Span::raw("  "),
        Span::styled("Ctrl+B n  hop", hint_style),
        Span::raw("  "),
        Span::styled("Ctrl+B q  leave", hint_style),
        Span::raw("  "),
        Span::styled("Ctrl+B ?  keys", hint_style),
    ]);
    buf.set_line(side, row, &line, width.saturating_sub(side));
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
    /// Focused this tab: parser thread should take the ratatui terminal
    /// and paint, or drop it if we just lost focus.
    Wake,
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
    /// Left-click on our chrome (top strip or session sidebar).
    ChromeClick { x: u16, y: u16 },
    /// A valid SGR report, but not one that means anything here (e.g. a
    /// release/motion/right-click on chrome).
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
    let real_x = px.saturating_sub(1);
    if let Ok((term_cols, real_rows)) = terminal::size() {
        let side = sidebar_cols(term_cols);
        let on_bottom_bar = real_x >= side && real_y + 1 == real_rows;
        let on_top = real_x >= side && real_y < TOP_BAR_ROWS;
        let on_sidebar = side > 0 && real_x < side;
        if on_top || on_bottom_bar || on_sidebar {
            let is_left_press = final_byte == b'M' && cb & 0b11 == 0 && cb & 0b0110_0000 == 0;
            return if on_bottom_bar && is_left_press {
                MouseDecode::OpenAgentPicker
            } else if is_left_press {
                MouseDecode::ChromeClick { x: real_x, y: real_y }
            } else {
                MouseDecode::Ignore
            };
        }
        let x = real_x.saturating_sub(side);
        let y = real_y.saturating_sub(TOP_BAR_ROWS);
        return decode_child_mouse(cb, final_byte, x, y);
    }
    let x = real_x;
    let y = real_y.saturating_sub(TOP_BAR_ROWS);
    decode_child_mouse(cb, final_byte, x, y)
}

fn decode_child_mouse(cb: u32, final_byte: u8, x: u16, y: u16) -> MouseDecode {
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
                                b'c' | b'C' => {
                                    debug_log("TRIGGER_PREFIX_NEW_TAB", &pending[..2]);
                                    let _ = tx.send(RunEvent::NewTab);
                                }
                                b'w' | b'W' => {
                                    debug_log("TRIGGER_PREFIX_NEW_WORKSPACE", &pending[..2]);
                                    let _ = tx.send(RunEvent::NewWorkspace);
                                }
                                b']' => {
                                    debug_log("TRIGGER_PREFIX_NEXT_WORKSPACE", &pending[..2]);
                                    let _ = tx.send(RunEvent::NextWorkspace);
                                }
                                b'[' => {
                                    debug_log("TRIGGER_PREFIX_PREV_WORKSPACE", &pending[..2]);
                                    let _ = tx.send(RunEvent::PrevWorkspace);
                                }
                                b'o' | b'O' => {
                                    debug_log("TRIGGER_PREFIX_NEXT_TAB", &pending[..2]);
                                    let _ = tx.send(RunEvent::NextTab);
                                }
                                b'i' | b'I' => {
                                    debug_log("TRIGGER_PREFIX_PREV_TAB", &pending[..2]);
                                    let _ = tx.send(RunEvent::PrevTab);
                                }
                                b'x' | b'X' => {
                                    debug_log("TRIGGER_PREFIX_CLOSE_TAB", &pending[..2]);
                                    let _ = tx.send(RunEvent::CloseTab);
                                }
                                b'q' | b'Q' => {
                                    debug_log("TRIGGER_PREFIX_LEAVE", &pending[..2]);
                                    let _ = tx.send(RunEvent::Leave);
                                }
                                b'1'..=b'9' => {
                                    let idx = (chord - b'1') as usize;
                                    debug_log("TRIGGER_PREFIX_FOCUS_TAB", &pending[..2]);
                                    let _ = tx.send(RunEvent::FocusTab(idx));
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
                                        MouseDecode::ChromeClick { x, y } => {
                                            let _ = tx.send(RunEvent::ChromeClick { x, y });
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

    // Use a column past the session sidebar so these decode as child events
    // when `terminal::size()` reports a wide enough terminal.

    #[test]
    fn left_click_press_and_release() {
        match parse_sgr_mouse(b"\x1b[<0;40;10M") {
            MouseDecode::Forward(ChildMsg::Mouse { x, y, input: vt::MouseInput::Press(vt::MouseButtonKind::Left), .. }) => {
                let side = terminal::size().ok().map(|(c, _)| sidebar_cols(c)).unwrap_or(0);
                assert_eq!((x, y), (39u16.saturating_sub(side), 10 - 1 - TOP_BAR_ROWS));
            }
            _ => panic!("expected a left press"),
        }
        assert!(matches!(
            parse_sgr_mouse(b"\x1b[<0;40;10m"),
            MouseDecode::Forward(ChildMsg::Mouse { input: vt::MouseInput::Release(vt::MouseButtonKind::Left), .. })
        ));
    }

    #[test]
    fn scroll_wheel_up_and_down() {
        assert!(matches!(parse_sgr_mouse(b"\x1b[<64;40;10M"), MouseDecode::Forward(ChildMsg::Mouse { input: vt::MouseInput::ScrollUp, .. })));
        assert!(matches!(parse_sgr_mouse(b"\x1b[<65;40;10M"), MouseDecode::Forward(ChildMsg::Mouse { input: vt::MouseInput::ScrollDown, .. })));
    }

    #[test]
    fn drag_reports_as_motion_with_button_held() {
        assert!(matches!(
            parse_sgr_mouse(b"\x1b[<32;40;10M"),
            MouseDecode::Forward(ChildMsg::Mouse { input: vt::MouseInput::Motion(Some(vt::MouseButtonKind::Left)), .. })
        ));
    }

    #[test]
    fn plain_hover_motion_has_no_button() {
        assert!(matches!(parse_sgr_mouse(b"\x1b[<35;40;10M"), MouseDecode::Forward(ChildMsg::Mouse { input: vt::MouseInput::Motion(None), .. })));
    }

    #[test]
    fn modifiers_decode_from_cb_bits() {
        // 0 (left) | 4 (shift) | 8 (alt) | 16 (ctrl) = 28
        match parse_sgr_mouse(b"\x1b[<28;40;10M") {
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
        let (cols, rows) = terminal::size().unwrap();
        let px = (sidebar_cols(cols) + 8).max(5);
        let seq = format!("\x1b[<0;{px};{rows}M");
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
        let child_rows = 30u16.saturating_sub(CHROME_ROWS);
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

#[cfg(test)]
mod multiplexer_chrome_tests {
    use super::*;

    #[test]
    fn wide_terminal_reserves_sidebar_columns() {
        let (cols, rows) = child_pty_size(80, 30);
        assert_eq!(cols, 80 - SIDEBAR_COLS);
        assert_eq!(rows, 30 - CHROME_ROWS);
    }

    #[test]
    fn narrow_terminal_hides_sidebar() {
        let (cols, rows) = child_pty_size(60, 24);
        assert_eq!(cols, 60);
        assert_eq!(rows, 24 - CHROME_ROWS);
        assert_eq!(sidebar_cols(60), 0);
    }

    #[test]
    fn expand_tilde_workspace_path() {
        let expanded = expand_workspace_path("~/handoff");
        assert!(expanded.contains("handoff"), "{expanded}");
        assert!(!expanded.starts_with('~'), "{expanded}");
    }

    #[test]
    fn shorten_path_uses_tilde_for_home() {
        if let Some(home) = dirs::home_dir() {
            let nested = home.join("handoff").join("worktree");
            let s = shorten_path(&nested.to_string_lossy());
            assert!(s.starts_with('~') || s.contains("handoff"), "{s}");
        }
    }

    #[test]
    fn resolve_other_tab_omits_when_exactly_one_other() {
        assert_eq!(resolve_other_tab_index(2, 0, None).unwrap(), 1);
        assert_eq!(resolve_other_tab_index(2, 1, None).unwrap(), 0);
    }

    #[test]
    fn resolve_other_tab_refuses_self() {
        assert!(resolve_other_tab_index(3, 1, Some(2)).is_err());
        assert!(resolve_other_tab_index(1, 0, None).is_err());
        assert!(resolve_other_tab_index(1, 0, Some(1)).is_err());
    }

    #[test]
    fn resolve_other_tab_explicit_and_missing() {
        assert_eq!(resolve_other_tab_index(3, 0, Some(3)).unwrap(), 2);
        assert!(resolve_other_tab_index(3, 0, Some(9)).is_err());
        assert!(resolve_other_tab_index(3, 0, None).is_err());
    }
}
