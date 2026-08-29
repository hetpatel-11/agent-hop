//! Control socket so a pane's agent can drive the parent `ah` mux
//! (new tab, hop/close another tab, workspaces) without touching its
//! own PTY. The child sends one JSON line to `$AH_SOCK` and includes
//! `$AH_TAB_ID` so the parent can refuse self-targeting ops.

use crate::agents::ToolName;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::sync::Mutex;

pub const SOCK_ENV: &str = "AH_SOCK";
pub const TAB_ENV: &str = "AH_TAB_ID";

static LIVE_LOCK: Mutex<()> = Mutex::new(());

#[derive(Debug, Deserialize)]
struct Request {
    op: String,
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    tab: Option<u32>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    from_tab: Option<u64>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    keys: Option<String>,
    #[serde(default)]
    new_name: Option<String>,
}

#[derive(Serialize)]
struct Reply {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaneOp {
    NewTab { tool: Option<ToolName> },
    Hop { tab: Option<usize>, tool: ToolName },
    Close { tab: Option<usize> },
    Focus { tab: usize },
    WorkspaceNew { path: Option<String>, tool: Option<ToolName> },
    WorkspaceNext,
    WorkspacePrev,
    Prompt { tab: Option<usize>, name: Option<String>, text: String },
    SendKeys { tab: Option<usize>, name: Option<String>, keys: String },
    Rename { tab: Option<usize>, name: Option<String>, new_name: String },
    Stop,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneRequest {
    pub from_tab: u64,
    pub op: PaneOp,
}

pub struct ControlServer {
    pub path: PathBuf,
    #[cfg(unix)]
    listener: std::os::unix::net::UnixListener,
}

pub fn mux_sock_path() -> PathBuf {
    dirs::home_dir().unwrap_or_default().join(".agent-hop").join("mux.sock")
}

pub fn live_path() -> PathBuf {
    dirs::home_dir().unwrap_or_default().join(".agent-hop").join("live.json")
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LiveMux {
    #[serde(default)]
    pub agents: Vec<LiveAgent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LiveAgent {
    pub id: u64,
    pub name: String,
    pub tool: String,
    pub status: String,
    pub workspace: String,
    pub ws: usize,
    pub tab: usize,
    pub index: usize,
    #[serde(default)]
    pub focused: bool,
    #[serde(default)]
    pub lines: Vec<String>,
}

pub fn publish_live(agents: Vec<LiveAgent>) {
    let _g = LIVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    write_live(&LiveMux { agents });
}

pub fn touch_live(id: u64, status: &str, lines: Vec<String>) {
    let _g = LIVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut live = read_live_unlocked().unwrap_or_default();
    if let Some(a) = live.agents.iter_mut().find(|a| a.id == id) {
        if a.status == status && a.lines == lines {
            return;
        }
        a.status = status.to_string();
        a.lines = lines;
        write_live(&live);
    }
}

pub fn read_live() -> Option<LiveMux> {
    let _g = LIVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    read_live_unlocked()
}

pub fn clear_live() {
    let _g = LIVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _ = std::fs::remove_file(live_path());
}

fn read_live_unlocked() -> Option<LiveMux> {
    let text = std::fs::read_to_string(live_path()).ok()?;
    serde_json::from_str(&text).ok()
}

fn write_live(live: &LiveMux) {
    let path = live_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let tmp = path.with_extension("json.tmp");
    if let Ok(text) = serde_json::to_string_pretty(live) {
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, path);
        }
    }
}

#[cfg(unix)]
pub fn bind() -> anyhow::Result<ControlServer> {
    use std::os::unix::net::{UnixListener, UnixStream};
    let preferred = mux_sock_path();
    if let Some(parent) = preferred.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let path = if UnixStream::connect(&preferred).is_ok() {
        std::env::temp_dir().join(format!("ah-{}.sock", std::process::id()))
    } else {
        let _ = std::fs::remove_file(&preferred);
        preferred
    };
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    listener.set_nonblocking(true)?;
    Ok(ControlServer { path, listener })
}

#[cfg(not(unix))]
pub fn bind() -> anyhow::Result<ControlServer> {
    anyhow::bail!("pane control socket is not available on Windows")
}

#[cfg(unix)]
pub fn spawn_listener(server: ControlServer, tx: Sender<PaneRequest>) {
    std::thread::spawn(move || {
        let listener = server.listener;
        loop {
            match listener.accept() {
                Ok((stream, _)) => handle_client(stream, &tx),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                Err(_) => break,
            }
        }
        let _ = std::fs::remove_file(&server.path);
    });
}

#[cfg(not(unix))]
pub fn spawn_listener(_server: ControlServer, _tx: Sender<PaneRequest>) {}

fn parse_tool(slug: Option<&str>) -> Result<Option<ToolName>, &'static str> {
    match slug {
        None | Some("") => Ok(None),
        Some(s) => ToolName::from_slug(s)
            .map(Some)
            .ok_or("unknown agent (use claude, codex, opencode, pi, or grok)"),
    }
}

#[cfg(unix)]
fn handle_client(stream: std::os::unix::net::UnixStream, tx: &Sender<PaneRequest>) {
    use std::io::{BufRead, BufReader};
    let mut writer = match stream.try_clone() {
        Ok(w) => w,
        Err(_) => return,
    };
    let mut line = String::new();
    let mut reader = BufReader::new(stream);
    if reader.read_line(&mut line).is_err() {
        let _ = write_reply(&mut writer, false, Some("could not read request"));
        return;
    }
    let req: Request = match serde_json::from_str(line.trim()) {
        Ok(r) => r,
        Err(_) => {
            let _ = write_reply(&mut writer, false, Some("invalid json"));
            return;
        }
    };
    match parse_request(req) {
        Ok(parsed) => {
            if tx.send(parsed).is_err() {
                let _ = write_reply(&mut writer, false, Some("parent ah is gone"));
                return;
            }
            let _ = write_reply(&mut writer, true, None);
        }
        Err(e) => {
            let _ = write_reply(&mut writer, false, Some(e));
        }
    }
}

fn parse_request(req: Request) -> Result<PaneRequest, &'static str> {
    let from_tab = req.from_tab.unwrap_or(0);
    let op = match req.op.as_str() {
        "tab" => PaneOp::NewTab { tool: parse_tool(req.agent.as_deref())? },
        "hop" => {
            let tool = parse_tool(req.agent.as_deref())?.ok_or("ah hop needs an agent")?;
            PaneOp::Hop { tab: req.tab.map(|n| n as usize), tool }
        }
        "close" => PaneOp::Close { tab: req.tab.map(|n| n as usize) },
        "focus" => match req.tab {
            Some(n) if n >= 1 => PaneOp::Focus { tab: n as usize },
            _ => return Err("ah focus needs a 1-based tab number"),
        },
        "workspace" => PaneOp::WorkspaceNew {
            path: req.path,
            tool: parse_tool(req.agent.as_deref()).ok().flatten(),
        },
        "workspace-next" => PaneOp::WorkspaceNext,
        "workspace-prev" => PaneOp::WorkspacePrev,
        "prompt" => PaneOp::Prompt {
            tab: req.tab.map(|n| n as usize),
            name: req.name,
            text: req.text.unwrap_or_default(),
        },
        "send-keys" => PaneOp::SendKeys {
            tab: req.tab.map(|n| n as usize),
            name: req.name,
            keys: req.keys.or(req.text).unwrap_or_default(),
        },
        "stop" => PaneOp::Stop,
        "rename" => {
            let new_name = req.new_name.filter(|s| !s.trim().is_empty()).ok_or("ah rename needs a name")?;
            PaneOp::Rename {
                tab: req.tab.map(|n| n as usize),
                name: req.name,
                new_name,
            }
        }
        _ => return Err("unknown op"),
    };
    Ok(PaneRequest { from_tab, op })
}

#[cfg(test)]
fn parse_line(line: &str) -> Result<PaneRequest, &'static str> {
    let req: Request = serde_json::from_str(line.trim()).map_err(|_| "invalid json")?;
    parse_request(req)
}

#[cfg(unix)]
fn write_reply(
    stream: &mut std::os::unix::net::UnixStream,
    ok: bool,
    error: Option<&str>,
) -> std::io::Result<()> {
    use std::io::Write;
    let body = serde_json::to_string(&Reply {
        ok,
        error: error.map(|s| s.to_string()),
    })
    .unwrap_or_else(|_| r#"{"ok":false}"#.to_string());
    writeln!(stream, "{body}")?;
    stream.flush()
}

#[cfg(not(unix))]
pub fn request(
    _op: &str,
    _agent: Option<&str>,
    _tab: Option<u32>,
    _path: Option<&str>,
) -> anyhow::Result<()> {
    anyhow::bail!("ah tab/hop/close/focus/workspace need macOS or Linux")
}

#[cfg(not(unix))]
pub fn request_ex(
    _op: &str,
    _agent: Option<&str>,
    _tab: Option<u32>,
    _path: Option<&str>,
    _name: Option<&str>,
    _text: Option<&str>,
    _keys: Option<&str>,
    _new_name: Option<&str>,
) -> anyhow::Result<serde_json::Value> {
    anyhow::bail!("ah agent commands need macOS or Linux")
}

#[cfg(unix)]
pub fn request(op: &str, agent: Option<&str>, tab: Option<u32>, path: Option<&str>) -> anyhow::Result<()> {
    request_ex(op, agent, tab, path, None, None, None, None).map(|_| ())
}

#[cfg(unix)]
fn connect_sock() -> anyhow::Result<std::os::unix::net::UnixStream> {
    use std::os::unix::net::UnixStream;
    use std::path::Path;
    if let Ok(sock) = std::env::var(SOCK_ENV) {
        if let Ok(stream) = UnixStream::connect(Path::new(&sock)) {
            return Ok(stream);
        }
    }
    UnixStream::connect(mux_sock_path())
        .map_err(|_| anyhow::anyhow!("no live ah session (start `ah` first)"))
}

#[cfg(unix)]
pub fn request_ex(
    op: &str,
    agent: Option<&str>,
    tab: Option<u32>,
    path: Option<&str>,
    name: Option<&str>,
    text: Option<&str>,
    keys: Option<&str>,
    new_name: Option<&str>,
) -> anyhow::Result<serde_json::Value> {
    use std::io::{BufRead, BufReader, Write};
    let from_tab: Option<u64> = std::env::var(TAB_ENV).ok().and_then(|s| s.parse().ok());
    let mut stream = connect_sock()?;
    let req = serde_json::json!({
        "op": op,
        "agent": agent,
        "tab": tab,
        "path": path,
        "from_tab": from_tab,
        "name": name,
        "text": text,
        "keys": keys,
        "new_name": new_name,
    });
    writeln!(stream, "{req}")?;
    stream.flush()?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    let v: serde_json::Value = serde_json::from_str(line.trim()).unwrap_or(serde_json::json!({}));
    if v.get("ok").and_then(|x| x.as_bool()) == Some(true) {
        Ok(v)
    } else {
        let err = v.get("error").and_then(|x| x.as_str()).unwrap_or("request failed");
        anyhow::bail!("{err}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::ToolName;

    #[test]
    fn parse_tab_with_and_without_agent() {
        let r = parse_line(r#"{"op":"tab","from_tab":3}"#).unwrap();
        assert_eq!(r.from_tab, 3);
        assert_eq!(r.op, PaneOp::NewTab { tool: None });
        let r = parse_line(r#"{"op":"tab","agent":"codex","from_tab":1}"#).unwrap();
        assert_eq!(r.op, PaneOp::NewTab { tool: Some(ToolName::Codex) });
    }

    #[test]
    fn parse_hop_requires_agent() {
        assert!(parse_line(r#"{"op":"hop","from_tab":1}"#).is_err());
        let r = parse_line(r#"{"op":"hop","agent":"grok","tab":2,"from_tab":1}"#).unwrap();
        assert_eq!(
            r.op,
            PaneOp::Hop {
                tab: Some(2),
                tool: ToolName::Grok
            }
        );
    }

    #[test]
    fn parse_close_focus_workspace() {
        let r = parse_line(r#"{"op":"close","from_tab":1}"#).unwrap();
        assert_eq!(r.op, PaneOp::Close { tab: None });
        let r = parse_line(r#"{"op":"focus","tab":3,"from_tab":1}"#).unwrap();
        assert_eq!(r.op, PaneOp::Focus { tab: 3 });
        assert!(parse_line(r#"{"op":"focus","from_tab":1}"#).is_err());
        assert_eq!(
            parse_line(r#"{"op":"workspace-next"}"#).unwrap().op,
            PaneOp::WorkspaceNext
        );
        assert_eq!(
            parse_line(r#"{"op":"workspace-prev"}"#).unwrap().op,
            PaneOp::WorkspacePrev
        );
        let r = parse_line(r#"{"op":"workspace","path":"/tmp/p","agent":"pi"}"#).unwrap();
        assert_eq!(
            r.op,
            PaneOp::WorkspaceNew {
                path: Some("/tmp/p".into()),
                tool: Some(ToolName::Pi)
            }
        );
    }

    #[test]
    fn parse_rejects_unknown_op_and_agent() {
        assert!(parse_line(r#"{"op":"explode"}"#).is_err());
        assert!(parse_line(r#"{"op":"tab","agent":"not-an-agent"}"#).is_err());
        assert!(parse_line("not json").is_err());
    }

    #[test]
    fn parse_prompt_send_rename() {
        let r = parse_line(r#"{"op":"prompt","text":"hello","from_tab":1}"#).unwrap();
        assert_eq!(
            r.op,
            PaneOp::Prompt {
                tab: None,
                name: None,
                text: "hello".into()
            }
        );
        let r = parse_line(r#"{"op":"send-keys","keys":"y\\r","tab":2}"#).unwrap();
        assert_eq!(
            r.op,
            PaneOp::SendKeys {
                tab: Some(2),
                name: None,
                keys: "y\\r".into()
            }
        );
        let r = parse_line(r#"{"op":"rename","new_name":"security-droid","name":"handoff-claude"}"#).unwrap();
        assert_eq!(
            r.op,
            PaneOp::Rename {
                tab: None,
                name: Some("handoff-claude".into()),
                new_name: "security-droid".into()
            }
        );
        assert!(parse_line(r#"{"op":"rename"}"#).is_err());
    }
}
