//! Port of src/adapters/pi.ts -- Pi's JSONL message log with a distinct
//! `toolResult`-role message convention and flat (non-nested) image content
//! blocks. Faithful, literal port -- comments below are carried over
//! (adapted to Rust) from the TS source since they document real, hard-won
//! behavior.

use crate::adapters::{Adapter, Attachment, Role, SessionRef, Turn, ToolCallRecord};
use crate::agents::ToolName;
use crate::util::{
    clean_title, find_files, mtime_ms, read_jsonl_lines, read_jsonl_lines_lazy,
    read_jsonl_tail_lines, sanitize_tool_name, to_tool_input_object, truncate, BodySampler,
    MAX_TOOL_OUTPUT_CHARS, MIN_TITLE_CHARS,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// PI_CODING_AGENT_DIR (confirmed real: Pi itself exports/reads it,
/// pointing at the ~/.pi/agent leaf directory, not just ~/.pi) lets a user
/// relocate their whole agent directory -- respecting it here means we
/// still find their real sessions instead of only ever looking in the
/// default location. A truthy check, not just "is the var set": an empty
/// string would resolve to the relative path "sessions", silently
/// redirecting reads/writes to whatever the current working directory
/// happens to be.
fn sessions_dir() -> PathBuf {
    match std::env::var("PI_CODING_AGENT_DIR") {
        Ok(v) if !v.is_empty() => Path::new(&v).join("sessions"),
        _ => dirs::home_dir().unwrap_or_default().join(".pi").join("agent").join("sessions"),
    }
}

fn encode_dir(cwd: &str) -> String {
    let real = std::fs::canonicalize(cwd).map(|p| p.to_string_lossy().to_string()).unwrap_or_else(|_| cwd.to_string());
    // Splitting on a literal "/" left a Windows path (e.g. C:\Users\test\src)
    // as one untouched component, including the ":" and "\" -- both illegal
    // in a Windows directory name. Strip one leading separator (either
    // style), then flatten every remaining "/", "\", and ":" to "-" so the
    // result is a single safe component on both platforms.
    let stripped = real.strip_prefix('/').or_else(|| real.strip_prefix('\\')).unwrap_or(&real);
    let flattened: String = stripped.chars().map(|c| if c == '/' || c == '\\' || c == ':' { '-' } else { c }).collect();
    format!("--{flattened}--")
}

const MAX_BODY_CHARS: usize = 40_000;

fn message_texts(content: &[Value]) -> Vec<String> {
    content
        .iter()
        .filter_map(|b| {
            let obj = b.as_object()?;
            if obj.get("type")?.as_str()? == "text" {
                obj.get("text")?.as_str().map(|s| s.to_string())
            } else {
                None
            }
        })
        .collect()
}

fn list_sessions() -> anyhow::Result<Vec<SessionRef>> {
    let files = find_files(&sessions_dir(), |p| p.extension().map(|e| e == "jsonl").unwrap_or(false));
    let mut out = Vec::new();
    for file in files {
        let mut session_id: Option<String> = None;
        let mut cwd: Option<String> = None;
        let mut first_user_text = String::new();
        let mut title_text = String::new();
        let mut body = BodySampler::new(MAX_BODY_CHARS);
        let mut stopped_early = false;

        for obj in read_jsonl_lines_lazy(&file) {
            let obj_type = obj.get("type").and_then(|v| v.as_str()).unwrap_or("");
            if obj_type == "session" {
                session_id = obj.get("id").and_then(|v| v.as_str()).map(|s| s.to_string());
                cwd = obj.get("cwd").and_then(|v| v.as_str()).map(|s| s.to_string());
                continue;
            }
            if obj_type != "message" {
                continue;
            }
            let Some(message) = obj.get("message").and_then(|v| v.as_object()) else { continue };
            let role = message.get("role").and_then(|v| v.as_str());
            if role != Some("user") && role != Some("assistant") {
                continue;
            }
            let Some(content) = message.get("content").and_then(|v| v.as_array()) else { continue };
            let text = message_texts(content).join("\n").trim().to_string();
            if text.is_empty() {
                continue;
            }
            if role == Some("user") {
                if first_user_text.is_empty() {
                    first_user_text = text.clone();
                }
                if title_text.is_empty() && text.chars().count() >= MIN_TITLE_CHARS {
                    title_text = text.clone();
                }
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
                if obj.get("type").and_then(|v| v.as_str()) != Some("message") {
                    continue;
                }
                let Some(message) = obj.get("message").and_then(|v| v.as_object()) else { continue };
                let role = message.get("role").and_then(|v| v.as_str());
                if role != Some("user") && role != Some("assistant") {
                    continue;
                }
                let Some(content) = message.get("content").and_then(|v| v.as_array()) else { continue };
                let text = message_texts(content).join("\n").trim().to_string();
                if !text.is_empty() {
                    body.append(&text);
                }
            }
        }
        let (Some(session_id), Some(cwd)) = (session_id, cwd) else { continue };
        let title = clean_title(if !title_text.is_empty() { &title_text } else { &first_user_text });
        let title = if title.is_empty() { "(empty)".to_string() } else { title };
        out.push(SessionRef {
            tool: ToolName::Pi,
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

/// Pi declares `api: "anthropic-messages"` (see write() below) but its own
/// real image content block is flat -- {type:"image", mimeType, data} --
/// NOT Claude's nested {type:"image", source:{type:"base64", media_type,
/// data}}. Confirmed by generating a real image through the actual `pi`
/// CLI and inspecting the real session file.
fn extract_pi_attachments(content: &[Value]) -> Vec<Attachment> {
    content
        .iter()
        .filter_map(|b| {
            let obj = b.as_object()?;
            if obj.get("type")?.as_str()? != "image" {
                return None;
            }
            let data = obj.get("data")?.as_str()?.to_string();
            let mime_type = obj.get("mimeType").and_then(|v| v.as_str()).unwrap_or("image/png").to_string();
            Some(Attachment { mime_type, base64: data, filename: None })
        })
        .collect()
}

/// Pi's own message shape declares `api: "anthropic-messages"`, but tool
/// calls/results aren't nested the way Claude's are -- a toolCall is a
/// content block inside an assistant message, while its result is an
/// entirely separate message with its own role: `{role:"toolResult",
/// toolCallId, toolName, content}`. Both get folded into the enclosing
/// assistant turn, matched by id, same as the Codex/Claude adapters.
fn read_impl(session_ref: &SessionRef) -> anyhow::Result<Vec<Turn>> {
    let file = session_ref
        .raw
        .as_ref()
        .and_then(|r| r.get("file"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("pi: session ref missing raw.file"))?;
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
        if obj.get("type").and_then(|v| v.as_str()) != Some("message") {
            continue;
        }
        let Some(message) = obj.get("message").and_then(|v| v.as_object()) else { continue };
        let role = message.get("role").and_then(|v| v.as_str());

        if role == Some("toolResult") {
            let call_id = message.get("toolCallId").and_then(|v| v.as_str());
            let idx = call_id.and_then(|id| call_index.get(id).copied());
            if let Some(idx) = idx {
                let mut out = String::new();
                match message.get("content") {
                    Some(Value::Array(arr)) => {
                        out = message_texts(arr).join("\n");
                        // A toolResult's content can carry an image block too
                        // (same flat shape as everywhere else in Pi) -- e.g.
                        // our own synthetic attachment tool result, or a real
                        // tool that returns an image directly.
                        pending_attachments.extend(extract_pi_attachments(arr));
                    }
                    Some(Value::String(s)) => out = s.clone(),
                    _ => {}
                }
                pending_tool_calls[idx].output = Some(truncate(&out, MAX_TOOL_OUTPUT_CHARS));
            }
            continue;
        }

        if role != Some("user") && role != Some("assistant") {
            continue;
        }
        let Some(content) = message.get("content").and_then(|v| v.as_array()) else { continue };

        if role == Some("assistant") {
            for b in content {
                let Some(block) = b.as_object() else { continue };
                let btype = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
                if btype == "text" {
                    if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                        let t = t.trim();
                        if !t.is_empty() {
                            assistant_text_parts.push(t.to_string());
                        }
                    }
                } else if btype == "toolCall" {
                    let name = block.get("name").and_then(|v| v.as_str()).unwrap_or("unknown_tool").to_string();
                    let input = block.get("arguments").cloned().unwrap_or_else(|| json!({})).to_string();
                    pending_tool_calls.push(ToolCallRecord { name, input, output: None });
                    if let Some(id) = block.get("id").and_then(|v| v.as_str()) {
                        call_index.insert(id.to_string(), pending_tool_calls.len() - 1);
                    }
                }
            }
            pending_attachments.extend(extract_pi_attachments(content));
            continue;
        }

        // role === "user" -- a genuine human turn, flush whatever assistant
        // activity (text + tool calls) accumulated before it.
        flush_assistant(&mut turns, &mut assistant_text_parts, &mut pending_tool_calls, &mut pending_attachments, &mut call_index);
        let text = message_texts(content).join("\n").trim().to_string();
        let attachments = extract_pi_attachments(content);
        if !text.is_empty() || !attachments.is_empty() {
            turns.push(Turn {
                role: Role::User,
                text,
                tool_calls: None,
                attachments: if attachments.is_empty() { None } else { Some(attachments) },
            });
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

fn short_hex(n: usize) -> String {
    Uuid::new_v4().simple().to_string().chars().take(n).collect()
}

/// Matches JS's `now.toISOString().replace(/:/g,"-").replace(/\.\d+Z$/,"")
/// + "-" + millis.pad(3) + "Z"` -- e.g. "2024-05-01T12-34-56-789Z".
fn fname_timestamp(now: chrono::DateTime<chrono::Utc>) -> String {
    let base = now.format("%Y-%m-%dT%H-%M-%S").to_string();
    format!("{base}-{:03}Z", now.timestamp_subsec_millis())
}

fn write_impl(turns: &[Turn], project_path: &str) -> anyhow::Result<String> {
    let real_cwd_str = real_cwd(project_path)?;
    let encoded = encode_dir(&real_cwd_str);
    let session_dir = sessions_dir().join(encoded);
    std::fs::create_dir_all(&session_dir)?;

    let new_id = Uuid::new_v4().to_string();
    let now = chrono::Utc::now();
    let out_path = session_dir.join(format!("{}_{}.jsonl", fname_timestamp(now), new_id));

    let mut lines: Vec<String> = vec![json!({
        "type": "session",
        "version": 3,
        "id": new_id,
        "timestamp": now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        "cwd": real_cwd_str,
    })
    .to_string()];

    let mut parent_id: Option<String> = None;
    for turn in turns {
        let my_id = short_hex(8);
        let ts = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        // Pi's own @file mechanism inlines non-image attachments as readable
        // text, extracted by pi itself -- we only have base64 bytes, not
        // extracted text, so non-image attachments get a placeholder note
        // instead of forged binary-as-text.
        let attachments = turn.attachments.clone().unwrap_or_default();
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
        // Real shape confirmed by generating an actual image through the pi
        // CLI: flat {type, mimeType, data}, not Claude's nested `source`.
        let image_blocks: Vec<Value> = attachments
            .iter()
            .filter(|a| a.mime_type.starts_with("image/"))
            .map(|img| json!({ "type": "image", "mimeType": img.mime_type, "data": img.base64 }))
            .collect();
        let tool_calls = turn.tool_calls.clone().unwrap_or_default();
        let tool_call_ids: Vec<String> = tool_calls.iter().map(|_| format!("call-{}-0", Uuid::new_v4())).collect();
        let tool_call_blocks: Vec<Value> = tool_calls
            .iter()
            .zip(tool_call_ids.iter())
            .map(|(tc, id)| json!({ "type": "toolCall", "id": id, "name": sanitize_tool_name(&tc.name), "arguments": to_tool_input_object(&tc.input) }))
            .collect();
        // Pi declares `api: "anthropic-messages"` -- the same backing API as
        // claude.ts, confirmed for real to reject image blocks on assistant
        // messages. Rather than assume Pi shares that restriction, an
        // assistant-side image is carried as a synthetic toolCall's result
        // instead, matching the same pattern proven safe for Claude/Grok.
        let attachment_tool_call_id = if turn.role == Role::Assistant && !image_blocks.is_empty() {
            Some(format!("call-{}-attachment", Uuid::new_v4()))
        } else {
            None
        };
        let mut all_tool_call_blocks = tool_call_blocks.clone();
        if let Some(id) = &attachment_tool_call_id {
            all_tool_call_blocks.push(json!({ "type": "toolCall", "id": id, "name": "imported_attachment", "arguments": {} }));
        }

        let mut content: Vec<Value> = Vec::new();
        if turn.role == Role::User {
            content.extend(image_blocks.clone());
        }
        if !combined_text.is_empty() {
            content.push(json!({ "type": "text", "text": combined_text }));
        }
        content.extend(all_tool_call_blocks.clone());

        let mut message = json!({
            "role": turn.role,
            "content": content,
            "timestamp": chrono::Utc::now().timestamp_millis(),
        });
        if turn.role == Role::Assistant {
            if let Some(obj) = message.as_object_mut() {
                obj.insert("api".to_string(), json!("anthropic-messages"));
                obj.insert("provider".to_string(), json!("anthropic"));
                obj.insert("model".to_string(), json!("claude-sonnet-5"));
                obj.insert(
                    "usage".to_string(),
                    json!({
                        "input": 1, "output": 1, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 2,
                        "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0 },
                    }),
                );
                obj.insert("stopReason".to_string(), json!("stop"));
                obj.insert("responseId".to_string(), json!(format!("msg_{}", short_hex(24))));
                obj.insert("rawStopReason".to_string(), json!("end_turn"));
            }
        }
        lines.push(json!({ "type": "message", "id": my_id, "parentId": parent_id, "timestamp": ts, "message": message }).to_string());
        parent_id = Some(my_id);

        // toolResult is its own message with a dedicated role, per Pi's own
        // convention -- one per tool call, matched by id.
        for (i, tc) in tool_calls.iter().enumerate() {
            let result_id = short_hex(8);
            lines.push(json!({
                "type": "message",
                "id": result_id,
                "parentId": parent_id,
                "timestamp": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                "message": {
                    "role": "toolResult",
                    "toolCallId": tool_call_ids[i],
                    "toolName": sanitize_tool_name(&tc.name),
                    "content": [{ "type": "text", "text": tc.output.clone().unwrap_or_default() }],
                    "timestamp": chrono::Utc::now().timestamp_millis(),
                },
            })
            .to_string());
            parent_id = Some(result_id);
        }
        if let Some(id) = &attachment_tool_call_id {
            let result_id = short_hex(8);
            lines.push(json!({
                "type": "message",
                "id": result_id,
                "parentId": parent_id,
                "timestamp": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                "message": {
                    "role": "toolResult",
                    "toolCallId": id,
                    "toolName": "imported_attachment",
                    "content": image_blocks,
                    "timestamp": chrono::Utc::now().timestamp_millis(),
                },
            })
            .to_string());
            parent_id = Some(result_id);
        }
    }

    std::fs::write(&out_path, lines.join("\n") + "\n")?;
    Ok(new_id)
}

fn resume_cmd_impl(session_id: &str, _project_path: &str) -> Vec<String> {
    vec!["pi".to_string(), "--session".to_string(), session_id.to_string()]
}

pub struct PiAdapter;

impl Adapter for PiAdapter {
    fn tool(&self) -> ToolName {
        ToolName::Pi
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
