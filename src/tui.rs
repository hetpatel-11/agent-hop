use crate::agents::ToolName;
use crate::logos;
use crossterm::{cursor, execute, queue, style::Print, terminal};
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use std::collections::HashMap;
use std::io::{stdout, Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};

struct BarAssets {
    logos: HashMap<&'static str, PathBuf>,
    use_graphics: bool,
}

const ALT_UP_LEGACY: &[u8] = b"\x1b[1;3A";
const ALT_DOWN_LEGACY: &[u8] = b"\x1b[1;3B";
const ALT_UP_KITTY: &[u8] = b"\x1b[57419;3u";
const ALT_DOWN_KITTY: &[u8] = b"\x1b[57420;3u";

enum HopDirection {
    Next,
    Prev,
}

enum RunEvent {
    ChildExited(u64),
    Hop(HopDirection),
}

/// Single-pane TUI shell: one agent's real pty rendered full-pane, with a
/// persistent toggle strip (bottom row, owned by us, agent never draws into
/// it) for switching between installed agents via Alt+Up/Down.
pub async fn run(initial: ToolName) -> anyhow::Result<()> {
    // Prefetch before entering raw mode so any network hiccup prints
    // normally instead of getting mangled by an active pty relay.
    let assets = Arc::new(BarAssets {
        logos: logos::ensure_all_logos().await,
        use_graphics: logos::supports_kitty_graphics(),
    });

    let current_writer: Arc<Mutex<Option<Box<dyn Write + Send>>>> = Arc::new(Mutex::new(None));
    let (tx, rx) = mpsc::channel::<RunEvent>();
    let generation = Arc::new(AtomicU64::new(0));

    spawn_stdin_relay(current_writer.clone(), tx.clone());

    let mut current = initial;
    terminal::enable_raw_mode()?;
    execute!(stdout(), terminal::Clear(terminal::ClearType::All))?;

    let result: anyhow::Result<()> = loop {
        let generation_id = generation.fetch_add(1, Ordering::SeqCst) + 1;
        match run_one(current, &current_writer, &tx, &rx, generation_id, &assets) {
            Ok(Some(HopDirection::Next)) => current = next_installed(current, 1),
            Ok(Some(HopDirection::Prev)) => current = next_installed(current, -1),
            Ok(None) => break Ok(()),
            Err(e) => break Err(e),
        }
    };

    terminal::disable_raw_mode()?;
    result
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

/// Runs one agent to completion (or until a hop is triggered). Returns
/// `Some(direction)` if the user triggered a hop, `None` if the child
/// exited on its own (user quit the agent normally).
fn run_one(
    tool: ToolName,
    current_writer: &Arc<Mutex<Option<Box<dyn Write + Send>>>>,
    tx: &mpsc::Sender<RunEvent>,
    rx: &mpsc::Receiver<RunEvent>,
    generation_id: u64,
    assets: &Arc<BarAssets>,
) -> anyhow::Result<Option<HopDirection>> {
    let pty_system = native_pty_system();
    let (cols, rows) = terminal::size()?;
    let pair = pty_system.openpty(PtySize {
        rows: rows.saturating_sub(1),
        cols,
        pixel_width: 0,
        pixel_height: 0,
    })?;

    let cmd = CommandBuilder::new(tool.binary());
    let mut child = pair.slave.spawn_command(cmd)?;
    drop(pair.slave);

    let writer = pair.master.take_writer()?;
    *current_writer.lock().unwrap() = Some(writer);

    let mut reader = pair.master.try_clone_reader()?;
    let tx_out = tx.clone();
    let assets_thread = assets.clone();
    std::thread::spawn(move || {
        let mut out = stdout();
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let _ = out.write_all(&buf[..n]);
                    let _ = draw_toggle_bar(&mut out, tool, &assets_thread);
                    let _ = out.flush();
                }
            }
        }
        let _ = tx_out.send(RunEvent::ChildExited(generation_id));
    });

    draw_toggle_bar(&mut stdout(), tool, assets)?;
    stdout().flush()?;

    let hop = loop {
        match rx.recv() {
            Ok(RunEvent::ChildExited(g)) if g == generation_id => break None,
            Ok(RunEvent::ChildExited(_)) => continue, // stale event from a prior killed child
            Ok(RunEvent::Hop(dir)) => break Some(dir),
            Err(_) => break None,
        }
    };

    *current_writer.lock().unwrap() = None;

    if hop.is_some() {
        let _ = child.kill();
    }
    let _ = child.wait();

    Ok(hop)
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
            logos::render_kitty(path, LOGO_COLS, out)?;
        } else {
            queue!(out, Print(logos::text_badge(tool)))?;
        }
    } else {
        queue!(out, Print(logos::text_badge(tool)))?;
    }

    queue!(
        out,
        Print(format!(" agent-hop \u{25cf} {} | Alt+\u{2191}/\u{2193} to switch agent", tool.slug()))
    )?;
    queue!(out, cursor::RestorePosition)?;
    Ok(())
}

/// One persistent stdin-reading thread for the whole program lifetime.
/// Detects Alt+Up/Alt+Down (legacy CSI and Kitty CSI-u encodings) and
/// signals a hop; forwards everything else raw to whichever agent's pty is
/// currently active. A single long-lived reader avoids the correctness bug
/// of two threads racing to read the same stdin fd across hops.
fn spawn_stdin_relay(current_writer: Arc<Mutex<Option<Box<dyn Write + Send>>>>, tx: mpsc::Sender<RunEvent>) {
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
                        if is_prefix_of_any(&pending) {
                            break; // wait for more bytes
                        }
                        // Not a trigger and not a prefix of one -- forward as-is.
                        if let Some(w) = current_writer.lock().unwrap().as_mut() {
                            let _ = w.write_all(&pending);
                            let _ = w.flush();
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
    for seq in [ALT_UP_LEGACY, ALT_DOWN_LEGACY, ALT_UP_KITTY, ALT_DOWN_KITTY] {
        if seq.starts_with(buf) {
            return true;
        }
    }
    false
}
