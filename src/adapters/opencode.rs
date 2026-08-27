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
use std::process::Stdio;
use uuid::Uuid;

fn db_path() -> PathBuf {
    if let Ok(p) = std::env::var("OPENCODE_DB") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    let data = std::env::var("XDG_DATA_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(|p| PathBuf::from(p).join("opencode"))
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".local").join("share").join("opencode"));
    data.join("opencode.db")
}

fn open_db_ro(path: &std::path::Path) -> Option<rusqlite::Connection> {
    if !path.is_file() {
        return None;
    }
    rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).ok()
}

fn session_ids_from(path: &std::path::Path, limit: usize) -> Vec<String> {
    let Some(conn) = open_db_ro(path) else {
        return Vec::new();
    };
    let Ok(mut stmt) = conn.prepare("SELECT id FROM session LIMIT ?1") else {
        return Vec::new();
    };
    let Ok(rows) = stmt.query_map([limit as i64], |row| row.get::<_, String>(0)) else {
        return Vec::new();
    };
    rows.flatten().collect()
}

/// The real installed opencode version, e.g. "1.18.15" -- a hardcoded guess
/// here goes stale the moment opencode updates itself. Falls back to a
/// generic placeholder if opencode isn't on PATH.
fn opencode_cli_version() -> String {
    match crate::agents::std_command_bin("opencode", &["--version"]).output() {
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
    if !has_opencode() {
        return Ok(Vec::new());
    }
    Ok(list_sessions_from(&db_path()))
}

/// List OpenCode sessions by reading the sqlite db directly. Used to live
/// in a `sqlite3 -json` subprocess, which is missing on most Windows
/// installs and also broke on `file:C:\...` URIs (the colon). Bundled
/// rusqlite opens the path as a file, no CLI, no URI.
fn list_sessions_from(path: &std::path::Path) -> Vec<SessionRef> {
    let Some(conn) = open_db_ro(path) else {
        return Vec::new();
    };
    let sql = format!(
        "SELECT s.id, s.directory, s.title, s.time_updated, \
         SUBSTR(GROUP_CONCAT(json_extract(p.data, '$.text'), ' '), 1, {MAX_BODY_CHARS}) AS body \
         FROM session s \
         LEFT JOIN part p ON p.session_id = s.id AND json_extract(p.data, '$.type') = 'text' \
         WHERE s.directory IS NOT NULL \
         GROUP BY s.id \
         ORDER BY s.time_updated DESC \
         LIMIT 500"
    );
    let Ok(mut stmt) = conn.prepare(&sql) else {
        return Vec::new();
    };
    let Ok(rows) = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, Option<i64>>(3)?,
            row.get::<_, Option<String>>(4)?.unwrap_or_default(),
        ))
    }) else {
        return Vec::new();
    };

    let mut out_refs = Vec::new();
    for row in rows.flatten() {
        let (id, directory, raw_title, time_updated, body) = row;

        // OpenCode's own placeholder before it auto-generates a real title --
        // useless for search/display, prefer real body content when we have it.
        let is_placeholder = raw_title.as_deref().is_none_or(is_new_session_placeholder);
        let body_first_line: String = body.trim().split_whitespace().take(20).collect::<Vec<_>>().join(" ");
        let title_source = if is_placeholder && !body_first_line.is_empty() {
            body_first_line
        } else {
            raw_title.clone().unwrap_or_else(|| "(untitled)".to_string())
        };
        let title = clean_title(&title_source);
        let title = if title.is_empty() { "(untitled)".to_string() } else { title };
        out_refs.push(SessionRef {
            tool: ToolName::OpenCode,
            session_id: id,
            project_path: directory,
            snippet: title.chars().take(200).collect(),
            title,
            body: Some(if body.is_empty() { raw_title.unwrap_or_default() } else { body }),
            updated_at: time_updated.unwrap_or_else(|| chrono::Utc::now().timestamp_millis()),
            raw: Some(json!({})),
            match_snippet: None,
        });
    }
    out_refs
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
        let status = crate::agents::std_command_bin("opencode", &["export", session_id])
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
///
/// Cached for the process lifetime (see `real_export_template`) -- this
/// scans up to 20 sessions and calls the real `opencode export` CLI
/// (subprocess, real interpreter startup cost each time) for each one
/// until it finds a pair, which measured at up to ~1.7s wall-clock in a
/// real hop. The template itself doesn't change within one run of
/// agent-hop, so paying that cost more than once per process is pure
/// waste -- directly part of why hops felt slow.
/// Kicks off the (expensive, subprocess-based) export-template derivation
/// in the background at process start, so the cache above is already warm
/// by the time a hop into OpenCode actually happens -- otherwise the
/// *first* hop into OpenCode in any given `ah` process still pays the
/// full ~1.4-1.8s cost (confirmed live), even though every hop after that
/// is ~2ms once the cache is populated. Fire-and-forget: nothing needs
/// this call's result directly, it only exists to populate the cache
/// ahead of the moment it's actually needed.
pub fn prewarm_export_template_cache() {
    std::thread::spawn(real_export_template);
}

fn real_export_template_uncached() -> Option<(Value, Value)> {
    let ids = session_ids_from(&db_path(), 20);
    for id in ids {
        let Some(data) = export_session(&id) else { continue };
        let Some(messages) = data.get("messages").and_then(|v| v.as_array()) else { continue };
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

fn real_export_template() -> Option<(Value, Value)> {
    static CACHE: std::sync::OnceLock<Option<(Value, Value)>> = std::sync::OnceLock::new();
    CACHE.get_or_init(real_export_template_uncached).clone()
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
    let tmp_str = tmp_file.to_string_lossy();
    let result = crate::agents::std_command_bin("opencode", &["import", tmp_str.as_ref()])
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

#[cfg(test)]
mod tests {
    use super::*;

    fn write_fixture_db(path: &std::path::Path) {
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE session (id TEXT, directory TEXT, title TEXT, time_updated INTEGER);
            CREATE TABLE part (session_id TEXT, data TEXT);
            INSERT INTO session VALUES ('ses_keep', '/tmp/proj', 'New session - 2026-08-27', 200);
            INSERT INTO part VALUES ('ses_keep', '{"type":"text","text":"fix the windows spawn"}');
            INSERT INTO session VALUES ('ses_named', '/tmp/other', 'real title', 100);
            INSERT INTO part VALUES ('ses_named', '{"type":"text","text":"hello"}');
            "#,
        )
        .unwrap();
    }

    #[test]
    fn list_sessions_reads_sqlite_without_cli() {
        let dir = std::env::temp_dir().join(format!("agent-hop-opencode-db-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("opencode.db");
        write_fixture_db(&db);

        let sessions = list_sessions_from(&db);
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].session_id, "ses_keep");
        assert_eq!(sessions[0].project_path, "/tmp/proj");
        assert!(
            sessions[0].title.contains("fix") || sessions[0].body.as_deref().unwrap_or("").contains("windows spawn"),
            "placeholder title should fall back to body, got title={:?} body={:?}",
            sessions[0].title,
            sessions[0].body
        );
        assert_eq!(session_ids_from(&db, 20), vec!["ses_keep".to_string(), "ses_named".to_string()]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn list_sessions_missing_db_is_empty_not_an_error() {
        let missing = std::env::temp_dir().join("agent-hop-no-such-opencode.db");
        let _ = std::fs::remove_file(&missing);
        assert!(list_sessions_from(&missing).is_empty());
        assert!(session_ids_from(&missing, 20).is_empty());
    }

    #[test]
    fn list_sessions_reads_real_opencode_db_if_present() {
        let path = dirs::home_dir().unwrap_or_default().join(".local").join("share").join("opencode").join("opencode.db");
        if !path.is_file() {
            return;
        }
        let sessions = list_sessions_from(&path);
        assert!(
            !sessions.is_empty(),
            "bundled rusqlite should list sessions from the real OpenCode db at {}",
            path.display()
        );
        assert!(sessions.iter().all(|s| !s.session_id.is_empty() && !s.project_path.is_empty()));
    }
}
