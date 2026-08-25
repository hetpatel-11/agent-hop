//! Control socket so a pane's agent can drive the parent `ah` mux
//! (new tab, hop/close another tab, workspaces) without touching its
//! own PTY. The child sends one JSON line to `$AH_SOCK` and includes
//! `$AH_TAB_ID` so the parent can refuse self-targeting ops.

use crate::agents::ToolName;
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;

pub const SOCK_ENV: &str = "AH_SOCK";
pub const TAB_ENV: &str = "AH_TAB_ID";

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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneRequest {
    pub from_tab: u64,
    pub op: PaneOp,
}

pub struct ControlServer {
    pub path: PathBuf,
    listener: UnixListener,
}

pub fn bind() -> anyhow::Result<ControlServer> {
    let path = std::env::temp_dir().join(format!("ah-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    listener.set_nonblocking(true)?;
    Ok(ControlServer { path, listener })
}

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

fn parse_tool(slug: Option<&str>) -> Result<Option<ToolName>, &'static str> {
    match slug {
        None | Some("") => Ok(None),
        Some(s) => ToolName::from_slug(s)
            .map(Some)
            .ok_or("unknown agent (use claude, codex, opencode, pi, or grok)"),
    }
}

fn handle_client(stream: UnixStream, tx: &Sender<PaneRequest>) {
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
        _ => return Err("unknown op"),
    };
    Ok(PaneRequest { from_tab, op })
}

fn parse_line(line: &str) -> Result<PaneRequest, &'static str> {
    let req: Request = serde_json::from_str(line.trim()).map_err(|_| "invalid json")?;
    parse_request(req)
}

fn write_reply(stream: &mut UnixStream, ok: bool, error: Option<&str>) -> std::io::Result<()> {
    let body = serde_json::to_string(&Reply {
        ok,
        error: error.map(|s| s.to_string()),
    })
    .unwrap_or_else(|_| r#"{"ok":false}"#.to_string());
    writeln!(stream, "{body}")?;
    stream.flush()
}

pub fn request(op: &str, agent: Option<&str>, tab: Option<u32>, path: Option<&str>) -> anyhow::Result<()> {
    let sock = std::env::var(SOCK_ENV).map_err(|_| {
        anyhow::anyhow!("this command only works inside a live ah pane")
    })?;
    let from_tab: Option<u64> = std::env::var(TAB_ENV).ok().and_then(|s| s.parse().ok());
    let mut stream = UnixStream::connect(Path::new(&sock))
        .map_err(|_| anyhow::anyhow!("could not reach the parent ah session"))?;
    let req = serde_json::json!({
        "op": op,
        "agent": agent,
        "tab": tab,
        "path": path,
        "from_tab": from_tab,
    });
    writeln!(stream, "{req}")?;
    stream.flush()?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    let v: serde_json::Value = serde_json::from_str(line.trim()).unwrap_or(serde_json::json!({}));
    if v.get("ok").and_then(|x| x.as_bool()) == Some(true) {
        Ok(())
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
}
