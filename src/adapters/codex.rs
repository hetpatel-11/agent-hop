//! Port of src/adapters/codex.ts -- Codex's response_item/event_msg JSONL
//! rollout format, function_call/function_call_output tool calls matched by
//! call_id, input_image data-URI attachments, and the session_meta header
//! codex's own `resume` requires. Faithful, literal port -- comments below
//! are carried over (adapted to Rust) from the TS source since they
//! document real, hard-won behavior.

use crate::adapters::{Adapter, Attachment, Role, SessionRef, Turn, ToolCallRecord};
use crate::agents::ToolName;
use crate::util::{
    clean_title, find_files, mtime_ms, read_jsonl_lines, read_jsonl_lines_lazy,
    read_jsonl_tail_lines, sanitize_tool_name, truncate, BodySampler, MAX_TOOL_OUTPUT_CHARS,
    MIN_TITLE_CHARS,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use uuid::Uuid;

fn sessions_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_default().join(".codex").join("sessions")
}

/// The real codex CLI's version, e.g. "0.146.1" -- codex's own `resume`
/// bumps its schema/behavior across releases, so a hardcoded guess here
/// inevitably goes stale the moment codex updates itself. Falls back to a
/// generic placeholder if codex isn't on PATH (write() would fail on the
/// missing binary long before this matters in practice).
fn codex_cli_version() -> String {
    match std::process::Command::new("codex").arg("--version").output() {
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

fn pad(n: u32) -> String {
    format!("{n:02}")
}

fn codex_image_blocks(attachments: &[Attachment]) -> Vec<Value> {
    attachments
        .iter()
        .filter(|a| a.mime_type.starts_with("image/"))
        .map(|img| json!({ "type": "input_image", "image_url": format!("data:{};base64,{}", img.mime_type, img.base64) }))
        .collect()
}

/// input_image is valid in user messages, but assistant messages only
/// accept output_text/refusal. Assistant-side images are emitted as tool
/// outputs by build_codex_assistant_image_payloads() instead.
fn build_codex_message_content(role: Role, text: &str, attachments: &[Attachment]) -> Vec<Value> {
    let mut content = Vec::new();
    if !text.is_empty() {
        content.push(json!({ "type": if role == Role::User { "input_text" } else { "output_text" }, "text": text }));
    }
    if role == Role::User {
        content.extend(codex_image_blocks(attachments));
    }
    content
}

fn build_codex_assistant_image_payloads(attachments: &[Attachment], call_id: &str) -> Vec<Value> {
    let images = codex_image_blocks(attachments);
    if images.is_empty() {
        return Vec::new();
    }
    vec![
        json!({ "type": "function_call", "name": "imported_image", "arguments": "{}", "call_id": call_id }),
        json!({ "type": "function_call_output", "call_id": call_id, "output": images }),
    ]
}

const MAX_BODY_CHARS: usize = 40_000;

const ENV_PREFIXES: [&str; 4] = [
    "<environment_context>",
    "# Context from my IDE",
    "# AGENTS.md instructions",
    "<recommended_plugins>",
];

fn has_env_prefix(text: &str) -> bool {
    ENV_PREFIXES.iter().any(|p| text.starts_with(p))
}

fn content_text(content: &[Value]) -> String {
    content
        .iter()
        .filter_map(|b| {
            let obj = b.as_object()?;
            let t = obj.get("type")?.as_str()?;
            if t == "input_text" || t == "output_text" || t == "text" {
                obj.get("text")?.as_str().map(|s| s.to_string())
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Codex has exactly one binary content type -- input_image. Non-image
/// attachments (PDFs, text files via -i) get flattened into plain
/// input_text by codex itself, already captured by the text extraction
/// above -- confirmed by generating a real session with a PDF attached.
fn extract_codex_attachments(content: &[Value]) -> Vec<Attachment> {
    content
        .iter()
        .filter_map(|b| {
            let obj = b.as_object()?;
            if obj.get("type")?.as_str()? != "input_image" {
                return None;
            }
            let image_url = obj.get("image_url")?.as_str()?;
            let (mime, b64) = parse_data_uri(image_url)?;
            Some(Attachment { mime_type: mime, base64: b64, filename: None })
        })
        .collect()
}

fn parse_data_uri(s: &str) -> Option<(String, String)> {
    let rest = s.strip_prefix("data:")?;
    let (mime, b64) = rest.split_once(";base64,")?;
    Some((mime.to_string(), b64.to_string()))
}

fn is_shell_paste(text: &str) -> bool {
    // /^\S+@\S+\s.*[%$#]\s/ -- a pasted terminal prompt line
    let Some(at_idx) = text.find('@') else { return false };
    if text[..at_idx].contains(char::is_whitespace) || at_idx == 0 {
        return false;
    }
    let after_at = &text[at_idx + 1..];
    let Some(ws_idx) = after_at.find(char::is_whitespace) else { return false };
    if after_at[..ws_idx].is_empty() {
        return false;
    }
    let rest = &after_at[ws_idx..];
    rest.chars().enumerate().any(|(i, c)| {
        (c == '%' || c == '$' || c == '#') && rest[i + c.len_utf8()..].starts_with(char::is_whitespace)
    })
}

fn list_sessions() -> anyhow::Result<Vec<SessionRef>> {
    let dir = sessions_dir();
    let files = find_files(&dir, |p| p.extension().map(|e| e == "jsonl").unwrap_or(false));
    let mut out = Vec::new();
    for file in files {
        let mut session_id: Option<String> = None;
        let mut cwd: Option<String> = None;
        let mut first_user_text = String::new();
        let mut first_assistant_text = String::new();
        let mut title_text = String::new();
        let mut body = BodySampler::new(MAX_BODY_CHARS);
        let mut stopped_early = false;

        for obj in read_jsonl_lines_lazy(&file) {
            let obj_type = obj.get("type").and_then(|v| v.as_str()).unwrap_or("");
            if obj_type == "session_meta" {
                if let Some(payload) = obj.get("payload").and_then(|v| v.as_object()) {
                    session_id = payload.get("id").and_then(|v| v.as_str()).map(|s| s.to_string());
                    cwd = payload.get("cwd").and_then(|v| v.as_str()).map(|s| s.to_string());
                }
                continue;
            }
            if obj_type != "response_item" {
                continue;
            }
            let Some(payload) = obj.get("payload").and_then(|v| v.as_object()) else { continue };
            if payload.get("type").and_then(|v| v.as_str()) != Some("message") {
                continue;
            }
            let Some(content) = payload.get("content").and_then(|v| v.as_array()) else { continue };
            let role = payload.get("role").and_then(|v| v.as_str());
            if role != Some("user") && role != Some("assistant") {
                continue;
            }
            let text = content_text(content).trim().to_string();
            if text.is_empty() || has_env_prefix(&text) {
                continue;
            }
            // a pasted terminal prompt is real content but a bad title --
            // it's what the user *ran*, not what they *asked*. Skip it as a
            // title candidate entirely, but body/search still sees it.
            let shell_paste = is_shell_paste(&text);
            if role == Some("user") {
                if first_user_text.is_empty() && !shell_paste {
                    first_user_text = text.clone();
                }
                if title_text.is_empty() && text.chars().count() >= MIN_TITLE_CHARS && !shell_paste {
                    title_text = text.clone();
                }
            } else if first_assistant_text.is_empty() {
                first_assistant_text = text.clone();
            }
            body.append(&text);
            if session_id.is_some() && cwd.is_some() && !title_text.is_empty() && body.has_head() {
                stopped_early = true;
                break;
            }
        }
        if stopped_early {
            body.mark_sampled();
            for obj in read_jsonl_tail_lines(&file) {
                if obj.get("type").and_then(|v| v.as_str()) != Some("response_item") {
                    continue;
                }
                let Some(payload) = obj.get("payload").and_then(|v| v.as_object()) else { continue };
                if payload.get("type").and_then(|v| v.as_str()) != Some("message") {
                    continue;
                }
                let Some(content) = payload.get("content").and_then(|v| v.as_array()) else { continue };
                let role = payload.get("role").and_then(|v| v.as_str());
                if role != Some("user") && role != Some("assistant") {
                    continue;
                }
                let text = content_text(content).trim().to_string();
                if text.is_empty() || has_env_prefix(&text) {
                    continue;
                }
                body.append(&text);
            }
        }
        let (Some(session_id), Some(cwd)) = (session_id, cwd) else { continue };
        let fallback = format!(
            "({}, no readable content)",
            Path::new(&cwd).file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default()
        );
        let title_source = if !title_text.is_empty() {
            &title_text
        } else if !first_user_text.is_empty() {
            &first_user_text
        } else if !first_assistant_text.is_empty() {
            &first_assistant_text
        } else {
            &fallback
        };
        let title = clean_title(title_source);
        out.push(SessionRef {
            tool: ToolName::Codex,
            session_id,
            project_path: cwd,
            snippet: title.chars().take(200).collect(),
            title,
            body: Some(body.value()),
            updated_at: mtime_ms(&file),
            raw: Some(json!({ "file": file.to_string_lossy() })),
            match_snippet: None,
        });
    }
    Ok(out)
}

/// Codex emits tool calls (function_call/function_call_output, and
/// custom_tool_call/custom_tool_call_output for things like apply_patch) as
/// separate top-level stream events, matched by call_id, interleaved with
/// possibly several assistant text messages before the next user turn. All
/// of that -- narration, tool calls, and their real output/diffs -- belongs
/// to one logical assistant turn, so it's accumulated and flushed as a
/// single Turn each time a user message appears.
fn read_impl(session_ref: &SessionRef) -> anyhow::Result<Vec<Turn>> {
    let file = session_ref
        .raw
        .as_ref()
        .and_then(|r| r.get("file"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("codex: session ref missing raw.file"))?;
    let lines = read_jsonl_lines(Path::new(file));
    let mut turns: Vec<Turn> = Vec::new();

    let mut assistant_text_parts: Vec<String> = Vec::new();
    let mut pending_tool_calls: Vec<ToolCallRecord> = Vec::new();
    let mut pending_attachments: Vec<Attachment> = Vec::new();
    let mut call_index: HashMap<String, usize> = HashMap::new();

    fn flush_assistant(
        turns: &mut Vec<Turn>,
        assistant_text_parts: &mut Vec<String>,
        pending_tool_calls: &mut Vec<ToolCallRecord>,
        pending_attachments: &mut Vec<Attachment>,
        call_index: &mut HashMap<String, usize>,
    ) {
        let text = assistant_text_parts.join("\n\n").trim().to_string();
        if !text.is_empty() || !pending_tool_calls.is_empty() || !pending_attachments.is_empty() {
            turns.push(Turn {
                role: Role::Assistant,
                text,
                tool_calls: if pending_tool_calls.is_empty() { None } else { Some(std::mem::take(pending_tool_calls)) },
                attachments: if pending_attachments.is_empty() { None } else { Some(std::mem::take(pending_attachments)) },
            });
        }
        assistant_text_parts.clear();
        pending_tool_calls.clear();
        pending_attachments.clear();
        call_index.clear();
    }

    for obj in lines {
        if obj.get("type").and_then(|v| v.as_str()) != Some("response_item") {
            continue;
        }
        let Some(payload) = obj.get("payload").and_then(|v| v.as_object()) else { continue };
        let ptype = payload.get("type").and_then(|v| v.as_str()).unwrap_or("");

        if ptype == "function_call" || ptype == "custom_tool_call" {
            let name = payload.get("name").and_then(|v| v.as_str()).unwrap_or("unknown_tool").to_string();
            let input = if ptype == "function_call" {
                payload.get("arguments").and_then(|v| v.as_str()).unwrap_or("").to_string()
            } else {
                payload.get("input").and_then(|v| v.as_str()).unwrap_or("").to_string()
            };
            pending_tool_calls.push(ToolCallRecord { name, input, output: None });
            if let Some(call_id) = payload.get("call_id").and_then(|v| v.as_str()) {
                call_index.insert(call_id.to_string(), pending_tool_calls.len() - 1);
            }
            continue;
        }
        if ptype == "function_call_output" || ptype == "custom_tool_call_output" {
            if let Some(call_id) = payload.get("call_id").and_then(|v| v.as_str()) {
                if let Some(&idx) = call_index.get(call_id) {
                    let output_val = payload.get("output");
                    let out = match output_val {
                        Some(Value::String(s)) => s.clone(),
                        Some(other) => other.to_string(),
                        None => String::new(),
                    };
                    pending_tool_calls[idx].output = Some(truncate(&out, MAX_TOOL_OUTPUT_CHARS));
                }
            }
            continue;
        }
        if ptype != "message" {
            continue;
        }
        let role = payload.get("role").and_then(|v| v.as_str());
        if role != Some("user") && role != Some("assistant") {
            continue;
        }
        let Some(content) = payload.get("content").and_then(|v| v.as_array()) else { continue };
        let text = content_text(content).trim().to_string();
        if has_env_prefix(&text) {
            continue;
        }
        let attachments = extract_codex_attachments(content);

        if role == Some("user") {
            flush_assistant(&mut turns, &mut assistant_text_parts, &mut pending_tool_calls, &mut pending_attachments, &mut call_index);
            if !text.is_empty() || !attachments.is_empty() {
                turns.push(Turn {
                    role: Role::User,
                    text,
                    tool_calls: None,
                    attachments: if attachments.is_empty() { None } else { Some(attachments) },
                });
            }
        } else {
            if !text.is_empty() {
                assistant_text_parts.push(text);
            }
            pending_attachments.extend(attachments);
        }
    }
    flush_assistant(&mut turns, &mut assistant_text_parts, &mut pending_tool_calls, &mut pending_attachments, &mut call_index);
    Ok(turns)
}

fn real_cwd(project_path: &str) -> anyhow::Result<String> {
    match std::fs::canonicalize(project_path) {
        Ok(p) => Ok(p.to_string_lossy().to_string()),
        Err(_) => {
            std::fs::create_dir_all(project_path)?;
            Ok(std::fs::canonicalize(project_path)?.to_string_lossy().to_string())
        }
    }
}

fn call_id() -> String {
    format!("call_{}", Uuid::new_v4().simple().to_string().chars().take(24).collect::<String>())
}

fn write_impl(turns: &[Turn], project_path: &str) -> anyhow::Result<String> {
    let real_cwd_str = real_cwd(project_path)?;
    let new_id = Uuid::new_v4().to_string();
    let now = chrono::Utc::now();
    let date_dir = sessions_dir()
        .join(format!("{}", now.format("%Y")))
        .join(format!("{}", now.format("%m")))
        .join(format!("{}", now.format("%d")));
    std::fs::create_dir_all(&date_dir)?;
    let fname_ts = now.format("%Y-%m-%dT%H-%M-%S").to_string();
    let out_path = date_dir.join(format!("rollout-{fname_ts}-{new_id}.jsonl"));

    let iso_now = now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let mut lines: Vec<String> = vec![json!({
        "timestamp": iso_now,
        "type": "session_meta",
        "payload": {
            "id": new_id,
            "timestamp": iso_now,
            "cwd": real_cwd_str,
            "originator": "codex_cli_rs",
            "cli_version": codex_cli_version(),
            "instructions": Value::Null,
            "source": "cli",
            // Missing entirely before this fix -- codex's own `resume`
            // command reads this field and, when absent, apparently
            // defaults it to an empty string internally rather than
            // erroring at write time, surfacing later as a cryptic 'Model
            // provider `` not found' failure only when you actually try to
            // resume. Confirmed by comparing against a real session's
            // session_meta, which always has this set.
            "model_provider": "openai",
        },
    })
    .to_string()];

    // response_item alone lets `codex resume` load and continue the
    // session (it's the API-level conversation log) but the TUI doesn't
    // display prior turns from it -- a real rollout file also has
    // event_msg records (user_message/agent_message), which is what the
    // TUI actually replays to render history. Same class of bug as Grok's
    // missing updates.jsonl, found the same way: diffing real vs synthetic
    // event types and inspecting the real payload shape.
    for turn in turns {
        let ts = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let attachments = turn.attachments.clone().unwrap_or_default();
        // Codex's own -i flag flattens non-image attachments (PDFs, text
        // files) into plain input_text rather than a distinct binary block
        // -- but that only works because codex extracts real readable text
        // from the file itself. We only have base64 bytes, not extracted
        // text, so a non-image attachment gets a placeholder note instead
        // of forged (and likely garbled) inline binary-as-text.
        let non_image_note = attachments
            .iter()
            .filter(|a| !a.mime_type.starts_with("image/"))
            .map(|a| format!("[attached file: {} ({})]", a.filename.clone().unwrap_or_else(|| "unnamed".into()), a.mime_type))
            .collect::<Vec<_>>()
            .join("\n");
        let combined_text: String = [turn.text.clone(), non_image_note]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n");
        let content = build_codex_message_content(turn.role, &combined_text, &attachments);
        if !content.is_empty() {
            lines.push(
                json!({ "timestamp": ts, "type": "response_item", "payload": { "type": "message", "role": turn.role, "content": content } })
                    .to_string(),
            );
        }
        for tc in turn.tool_calls.iter().flatten() {
            let cid = call_id();
            lines.push(
                json!({ "timestamp": ts, "type": "response_item", "payload": { "type": "function_call", "name": sanitize_tool_name(&tc.name), "arguments": tc.input, "call_id": cid } })
                    .to_string(),
            );
            lines.push(
                json!({ "timestamp": ts, "type": "response_item", "payload": { "type": "function_call_output", "call_id": cid, "output": tc.output.clone().unwrap_or_default() } })
                    .to_string(),
            );
        }
        // Images returned by source-agent tools belong to assistant turns,
        // but Codex rejects input_image inside an assistant message. A
        // native Codex function_call_output does accept image blocks, so
        // preserve the bytes in a synthetic imported_image result instead
        // of dropping or re-roleing it.
        if turn.role == Role::Assistant {
            let image_call_id = call_id();
            for payload in build_codex_assistant_image_payloads(&attachments, &image_call_id) {
                lines.push(json!({ "timestamp": ts, "type": "response_item", "payload": payload }).to_string());
            }
        }
        if content.is_empty() && turn.tool_calls.as_ref().map(|t| t.is_empty()).unwrap_or(true) && attachments.is_empty() {
            continue;
        }
        let event_payload = if turn.role == Role::User {
            json!({ "type": "user_message", "message": if combined_text.is_empty() { "[image attached]".to_string() } else { combined_text.clone() } })
        } else {
            json!({ "type": "agent_message", "message": if combined_text.is_empty() { "[tool call]".to_string() } else { combined_text.clone() }, "phase": "commentary" })
        };
        lines.push(json!({ "timestamp": ts, "type": "event_msg", "payload": event_payload }).to_string());
    }
    std::fs::write(&out_path, lines.join("\n") + "\n")?;
    Ok(new_id)
}

fn resume_cmd_impl(session_id: &str, _project_path: &str) -> Vec<String> {
    vec!["codex".to_string(), "resume".to_string(), session_id.to_string()]
}

pub struct CodexAdapter;

impl Adapter for CodexAdapter {
    fn tool(&self) -> ToolName {
        ToolName::Codex
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
