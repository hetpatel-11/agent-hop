//! Port of src/adapters/opencode.ts -- OpenCode stores sessions in a
//! SQLite db (queried directly for listing) but only exposes full message
//! content via its own `opencode export`/`opencode import` subcommands, not
//! a readable file format. Faithful, literal port -- comments below are
//! carried over (adapted to Rust) from the TS source since they document
//! real, hard-won behavior.

use crate::adapters::{Adapter, Attachment, Role, SessionRef, Turn, ToolCallRecord};
use crate::agents::{which, ToolName};
use crate::util::{clean_title, truncate, to_tool_input_object, MAX_TOOL_OUTPUT_CHARS};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use uuid::Uuid;

fn db_path() -> PathBuf {
    dirs::home_dir().unwrap_or_default().join(".local").join("share").join("opencode").join("opencode.db")
}

/// The real installed opencode version, e.g. "1.18.15" -- a hardcoded guess
/// here goes stale the moment opencode updates itself. Falls back to a
/// generic placeholder if opencode isn't on PATH.
fn opencode_cli_version() -> String {
    match Command::new("opencode").arg("--version").output() {
        Ok(out) => {
            let text = String::from_utf8_lossy(&out.stdout).to_string();
            extract_semver(&text).unwrap_or_else(|| "0.0.0".to_string())
        }
        Err(_) => "0.0.0".to_string(),
    }
}

fn extract_semver(s: &str) -> Option<String> {
    for token in s.split(|c: char| !c.is_ascii_digit() && c != '.') {
        let parts: Vec<&str> = token.split('.').collect();
        if parts.len() >= 3
            && parts[0..3]
                .iter()
                .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
        {
            return Some(parts[0..3].join("."));
        }
    }
    None
}

fn has_opencode() -> bool {
    which("opencode").is_some()
}

const MAX_BODY_CHARS: usize = 40_000;

fn uid() -> String {
    Uuid::new_v4().simple().to_string()
}

/// Pull message text via a SQL join (not `opencode export` per session --
/// that would mean one subprocess spawn per session just to list, far too
/// slow at scale). Still pure SQL, so this stays fast.
fn list_sessions() -> anyhow::Result<Vec<SessionRef>> {
    if !db_path().exists() || !has_opencode() {
        return Ok(Vec::new());
    }

    let query = format!(
        "SELECT s.id, s.directory, s.title, s.time_updated, \
         SUBSTR(GROUP_CONCAT(json_extract(p.data, '$.text'), ' '), 1, {MAX_BODY_CHARS}) AS body \
         FROM session s \
         LEFT JOIN part p ON p.session_id = s.id AND json_extract(p.data, '$.type') = 'text' \
         WHERE s.directory IS NOT NULL \
         GROUP BY s.id \
         ORDER BY s.time_updated DESC \
         LIMIT 500"
    );
    let db_uri = format!("file:{}?mode=ro", db_path().to_string_lossy());
    let out = match Command::new("sqlite3").args(["-json", &db_uri, &query]).output() {
        Ok(o) if o.status.success() => o.stdout,
        _ => return Ok(Vec::new()),
    };
    let text = String::from_utf8_lossy(&out).to_string();
    if text.trim().is_empty() {
        return Ok(Vec::new());
    }
    let rows: Vec<Value> = match serde_json::from_str(&text) {
        Ok(r) => r,
        Err(_) => return Ok(Vec::new()),
    };

    let mut out_refs = Vec::new();
    for row in rows {
        let Some(id) = row.get("id").and_then(|v| v.as_str()) else { continue };
        let Some(directory) = row.get("directory").and_then(|v| v.as_str()) else { continue };
        let raw_title = row.get("title").and_then(|v| v.as_str());
        let body = row.get("body").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let time_updated = row.get("time_updated").and_then(|v| v.as_i64());

        // OpenCode's own placeholder before it auto-generates a real title --
        // useless for search/display, prefer real body content when we have it.
        let is_placeholder = raw_title.is_none() || is_new_session_placeholder(raw_title.unwrap());
        let body_first_line: String = body.trim().split_whitespace().take(20).collect::<Vec<_>>().join(" ");
        let title_source = if is_placeholder && !body_first_line.is_empty() {
            body_first_line
        } else {
            raw_title.unwrap_or("(untitled)").to_string()
        };
        let title = clean_title(&title_source);
        let title = if title.is_empty() { "(untitled)".to_string() } else { title };
        out_refs.push(SessionRef {
            tool: ToolName::OpenCode,
            session_id: id.to_string(),
            project_path: directory.to_string(),
            snippet: title.chars().take(200).collect(),
            title,
            body: Some(if body.is_empty() { raw_title.unwrap_or("").to_string() } else { body }),
            updated_at: time_updated.unwrap_or_else(|| chrono::Utc::now().timestamp_millis()),
            raw: Some(json!({})),
            match_snippet: None,
        });
    }
    Ok(out_refs)
}

fn is_new_session_placeholder(title: &str) -> bool {
    // /^New session - \d{4}-\d{2}-\d{2}/
    let Some(rest) = title.strip_prefix("New session - ") else { return false };
    let bytes = rest.as_bytes();
    bytes.len() >= 10
        && bytes[0..4].iter().all(|b| b.is_ascii_digit())
        && bytes[4] == b'-'
        && bytes[5..7].iter().all(|b| b.is_ascii_digit())
        && bytes[7] == b'-'
        && bytes[8..10].iter().all(|b| b.is_ascii_digit())
}

/// Captures `opencode export`'s stdout via a real temp file, not a piped
/// output capture -- confirmed as a real, silent bug: for a genuinely large
/// export, a piped stdout capture cut off deterministically at exactly the
/// same byte offset on every run (not a race), while the exact same command
/// with shell `>` redirection to a file got the full output. Writing
/// directly to a file descriptor (like shell redirection does) instead of
/// through a pipe avoids whatever pipe-specific limit caused this.
fn export_session(session_id: &str) -> Option<Value> {
    let tmp_path = std::env::temp_dir().join(format!("agent-hop-opencode-export-{}.json", Uuid::new_v4()));
    let result = (|| -> anyhow::Result<Value> {
        let file = std::fs::File::create(&tmp_path)?;
        let status = Command::new("opencode")
            .args(["export", session_id])
            .stdin(Stdio::null())
            .stdout(Stdio::from(file))
            .stderr(Stdio::null())
            .status()?;
        if !status.success() {
            anyhow::bail!("opencode export failed");
        }
        let out = std::fs::read_to_string(&tmp_path)?;
        let brace = out.find('{').ok_or_else(|| anyhow::anyhow!("no JSON in export output"))?;
        Ok(serde_json::from_str(&out[brace..])?)
    })();
    let _ = std::fs::remove_file(&tmp_path);
    result.ok()
}

/// Reads a FilePart's bytes -- OpenCode's `url` field is a data: URI for
/// pasted attachments, but can also be a local file path/file: URL for
/// attachments referenced on disk. Since OpenCode is local-first, a local
/// path is just as readable as an inline blob -- try both instead of only
/// handling the inline case. Generic across mime types.
fn read_opencode_attachment(part: &Value) -> Option<Attachment> {
    let url = part.get("url").and_then(|v| v.as_str())?;
    let mime = part.get("mime").and_then(|v| v.as_str())?;
    let filename = part.get("filename").and_then(|v| v.as_str()).map(|s| s.to_string());
    if let Some(rest) = url.strip_prefix("data:") {
        if let Some((_, b64)) = rest.split_once(";base64,") {
            return Some(Attachment { mime_type: mime.to_string(), base64: b64.to_string(), filename });
        }
    }
    let file_path = url.strip_prefix("file://").unwrap_or(url);
    let bytes = std::fs::read(file_path).ok()?; // moved/deleted since recording -- skip, don't crash
    Some(Attachment {
        mime_type: mime.to_string(),
        base64: base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &bytes),
        filename,
    })
}

/// OpenCode's tool calls (ToolPart) and image attachments (FilePart) are
/// confirmed against opencode's own source, not empirically verified
/// against a real local session with a tool call -- read defensively so an
/// unexpected shape just means that part is skipped, never a thrown error.
fn read_impl(session_ref: &SessionRef) -> anyhow::Result<Vec<Turn>> {
    let Some(data) = export_session(&session_ref.session_id) else {
        return Ok(Vec::new());
    };
    let mut turns = Vec::new();
    let Some(messages) = data.get("messages").and_then(|v| v.as_array()) else {
        return Ok(turns);
    };
    for m in messages {
        let role = m.get("info").and_then(|i| i.get("role")).and_then(|v| v.as_str());
        let role = match role {
            Some("user") => Role::User,
            Some("assistant") => Role::Assistant,
            _ => continue,
        };
        let Some(parts) = m.get("parts").and_then(|v| v.as_array()) else { continue };

        let text_parts: Vec<String> = parts
            .iter()
            .filter(|p| p.get("type").and_then(|v| v.as_str()) == Some("text"))
            .filter_map(|p| p.get("text").and_then(|v| v.as_str()))
            .map(|s| s.to_string())
            .collect();
        let text = text_parts.join("\n").trim().to_string();

        let tool_calls: Vec<ToolCallRecord> = parts
            .iter()
            .filter(|p| p.get("type").and_then(|v| v.as_str()) == Some("tool"))
            .map(|p| {
                let state = p.get("state");
                let input_val = state.and_then(|s| s.get("input"));
                let input = match input_val {
                    Some(Value::String(s)) => s.clone(),
                    Some(v) => v.to_string(),
                    None => "{}".to_string(),
                };
                let name = p.get("tool").and_then(|v| v.as_str()).unwrap_or("unknown_tool").to_string();
                let mut rec = ToolCallRecord { name, input, output: None };
                if let Some(output_val) = state.and_then(|s| s.get("output")) {
                    let out = match output_val {
                        Value::String(s) => s.clone(),
                        v => v.to_string(),
                    };
                    rec.output = Some(truncate(&out, MAX_TOOL_OUTPUT_CHARS));
                }
                rec
            })
            .collect();

        let attachments: Vec<Attachment> = parts
            .iter()
            .filter(|p| p.get("type").and_then(|v| v.as_str()) == Some("file"))
            .filter_map(read_opencode_attachment)
            .collect();

        if !text.is_empty() || !tool_calls.is_empty() || !attachments.is_empty() {
            turns.push(Turn {
                role,
                text,
                tool_calls: if tool_calls.is_empty() { None } else { Some(tool_calls) },
                attachments: if attachments.is_empty() { None } else { Some(attachments) },
            });
        }
    }
    Ok(turns)
}

/// Grab one real user+assistant message pair from any existing session to
/// use as a field-complete template -- opencode's import schema requires
/// many fields (mode, path, tokens, cost, parentID chains...) not worth
/// hand-guessing when a real example already satisfies them.
fn real_export_template() -> Option<(Value, Value)> {
    let db_uri = format!("file:{}?mode=ro", db_path().to_string_lossy());
    let out = Command::new("sqlite3")
        .args(["-json", &db_uri, "SELECT id FROM session LIMIT 20"])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    if text.trim().is_empty() {
        return None;
    }
    let rows: Vec<Value> = serde_json::from_str(&text).ok()?;
    for row in rows {
        let id = row.get("id").and_then(|v| v.as_str())?;
        let Some(data) = export_session(id) else { continue };
        let messages = data.get("messages").and_then(|v| v.as_array())?;
        let user_info = messages
            .iter()
            .find(|m| m.get("info").and_then(|i| i.get("role")).and_then(|v| v.as_str()) == Some("user"))
            .and_then(|m| m.get("info"));
        let asst_info = messages
            .iter()
            .find(|m| m.get("info").and_then(|i| i.get("role")).and_then(|v| v.as_str()) == Some("assistant"))
            .and_then(|m| m.get("info"));
        if let (Some(u), Some(a)) = (user_info, asst_info) {
            return Some((u.clone(), a.clone()));
        }
    }
    None
}

fn write_impl(turns: &[Turn], project_path: &str) -> anyhow::Result<String> {
    let Some((user_template, assistant_template)) = real_export_template() else {
        anyhow::bail!(
            "opencode: no existing session found to use as a field template. Start one real opencode session first (`opencode run \"hi\"`), then retry."
        );
    };

    let now_ms = chrono::Utc::now().timestamp_millis();
    // IDs must be genuinely unique across every write() call, not just
    // within one call -- opencode's message/part tables use `id` as a
    // primary key and `import` does onConflictDoNothing(), so a repeated id
    // silently no-ops the insert instead of erroring.
    let new_session_id = format!("ses_{}", uid());

    let mut messages: Vec<Value> = Vec::new();
    let mut prev_msg_id: Option<String> = None;
    for (i, turn) in turns.iter().enumerate() {
        let i = i as i64;
        let msg_id = format!("msg_{}", uid());
        let template = if turn.role == Role::User { &user_template } else { &assistant_template };
        let mut info = template.clone();
        if let Some(obj) = info.as_object_mut() {
            obj.insert("id".to_string(), json!(msg_id));
            obj.insert("sessionID".to_string(), json!(new_session_id));
            obj.insert("time".to_string(), json!({ "created": now_ms + i }));
            if turn.role == Role::Assistant {
                if let Some(Value::Object(time_obj)) = obj.get_mut("time") {
                    time_obj.insert("completed".to_string(), json!(now_ms + i + 1));
                }
                obj.insert("parentID".to_string(), json!(prev_msg_id.clone().unwrap_or_else(|| msg_id.clone())));
                if let Some(Value::Object(path_obj)) = obj.get_mut("path") {
                    path_obj.insert("cwd".to_string(), json!(project_path));
                }
            }
        }

        // Real ToolPart/FilePart shapes, generated and confirmed against a
        // real opencode install via a live `opencode run` with an actual
        // tool call and file attachment, then `opencode export` to inspect
        // the true field-for-field shape.
        let tool_parts: Vec<Value> = turn
            .tool_calls
            .iter()
            .flatten()
            .map(|tc| {
                let input = to_tool_input_object(&tc.input);
                json!({
                    "type": "tool",
                    "tool": tc.name,
                    "callID": format!("call_{}", uid()),
                    "state": { "status": "completed", "input": input, "output": tc.output.clone().unwrap_or_default(), "title": tc.name, "metadata": {}, "time": { "start": now_ms + i, "end": now_ms + i } },
                    "id": format!("prt_{}", uid()),
                    "sessionID": new_session_id,
                    "messageID": msg_id,
                })
            })
            .collect();
        // Generic across mime types -- same FilePart shape works for a PDF
        // as for an image, confirmed against a real opencode session.
        let file_parts: Vec<Value> = turn
            .attachments
            .iter()
            .flatten()
            .map(|att| {
                let ext = att.mime_type.split('/').nth(1).unwrap_or("bin");
                json!({
                    "type": "file",
                    "mime": att.mime_type,
                    "url": format!("data:{};base64,{}", att.mime_type, att.base64),
                    "synthetic": true,
                    "filename": att.filename.clone().unwrap_or_else(|| format!("attachment.{ext}")),
                    "id": format!("prt_{}", uid()),
                    "sessionID": new_session_id,
                    "messageID": msg_id,
                })
            })
            .collect();

        let mut parts = vec![json!({ "type": "text", "text": turn.text, "id": format!("prt_{}", uid()), "sessionID": new_session_id, "messageID": msg_id })];
        parts.extend(tool_parts);
        parts.extend(file_parts);

        messages.push(json!({ "info": info, "parts": parts }));
        prev_msg_id = Some(msg_id);
    }

    let first_user_text = turns.iter().find(|t| t.role == Role::User).map(|t| t.text.clone()).unwrap_or_else(|| "Resumed via agent-hop".to_string());
    let title: String = first_user_text.chars().take(80).collect();
    let stripped_path = project_path.strip_prefix('/').unwrap_or(project_path);

    let export_shape = json!({
        "info": {
            "id": new_session_id,
            // no parentID -- a real top-level session doesn't have one.
            // Setting it (even to its own new id) makes opencode treat the
            // session as a child/subagent session instead of a normal
            // top-level chat, which is why a resumed session was rendering
            // as if it were "thinking" as a subagent.
            "slug": "resumed-via-handoff",
            "projectID": "global",
            "directory": project_path,
            "path": stripped_path,
            "title": title,
            "agent": "build",
            "model": { "id": "big-pickle", "providerID": "opencode" },
            "version": opencode_cli_version(),
            "time": { "created": now_ms, "updated": now_ms },
        },
        "messages": messages,
    });

    let tmp_dir = std::env::temp_dir().join(format!("handoff-opencode-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&tmp_dir)?;
    let tmp_file = tmp_dir.join("session.json");
    std::fs::write(&tmp_file, export_shape.to_string())?;

    // must run with cwd=project_path -- opencode ties the imported session
    // to whatever directory the `import` process was actually run from,
    // not the "directory" field inside the JSON payload. If the original
    // project dir no longer exists on disk, fall back to homedir().
    let import_cwd = if std::path::Path::new(project_path).exists() {
        project_path.to_string()
    } else {
        dirs::home_dir().unwrap_or_default().to_string_lossy().to_string()
    };
    let result = Command::new("opencode")
        .args(["import", &tmp_file.to_string_lossy()])
        .current_dir(&import_cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .status();
    let _ = std::fs::remove_dir_all(&tmp_dir);
    let status = result?;
    if !status.success() {
        anyhow::bail!("opencode import failed");
    }

    Ok(new_session_id)
}

fn resume_cmd_impl(session_id: &str, _project_path: &str) -> Vec<String> {
    // `opencode run` is for one-shot non-interactive messages -- it errors
    // if given only --session with no message, even though the session id
    // is valid. The default top-level command (no subcommand) is what
    // actually opens the interactive TUI resumed at a given session.
    vec!["opencode".to_string(), "--session".to_string(), session_id.to_string()]
}

pub struct OpenCodeAdapter;

impl Adapter for OpenCodeAdapter {
    fn tool(&self) -> ToolName {
        ToolName::OpenCode
    }
    fn list_sessions(&self) -> anyhow::Result<Vec<SessionRef>> {
        list_sessions()
    }
    fn read(&self, session_ref: &SessionRef) -> anyhow::Result<Vec<Turn>> {
        read_impl(session_ref)
    }
    fn write(&self, turns: &[Turn], project_path: &str) -> anyhow::Result<String> {
        write_impl(turns, project_path)
    }
    fn resume_cmd(&self, session_id: &str, project_path: &str) -> Vec<String> {
        resume_cmd_impl(session_id, project_path)
    }
}
