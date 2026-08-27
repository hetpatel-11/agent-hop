//! Port of src/adapters/grok.ts -- Grok sessions split across three files
//! per session directory (chat_history.jsonl for API continuation,
//! updates.jsonl -- an ACP session/update event stream -- for what the TUI
//! actually replays, and summary.json for listing metadata). Faithful,
//! literal port -- comments below are carried over (adapted to Rust) from
//! the TS source since they document real, hard-won behavior.

use crate::adapters::{Adapter, Attachment, Role, SessionRef, Turn, ToolCallRecord};
use crate::agents::ToolName;
use crate::util::{clean_title, find_files, mtime_ms, read_jsonl_lines, read_jsonl_lines_lazy, read_jsonl_tail_lines, truncate, BodySampler, MAX_TOOL_OUTPUT_CHARS};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use uuid::Uuid;

fn sessions_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_default().join(".grok").join("sessions")
}

fn safe_json_parse(s: &str) -> Value {
    serde_json::from_str(s).unwrap_or_else(|_| json!({ "raw": s }))
}

fn extract_user_query(text: &str) -> Option<String> {
    let start = text.find("<user_query>")?;
    let end = text.find("</user_query>")?;
    let inner_start = start + "<user_query>".len();
    if inner_start > end {
        return None;
    }
    Some(text[inner_start..end].trim().to_string())
}

/// Matches JS's `encodeURIComponent` -- percent-encodes everything except
/// the unreserved set (letters, digits, and `- _ . ! ~ * ' ( )`).
fn encode_uri_component(s: &str) -> String {
    let mut out = String::new();
    for byte in s.bytes() {
        let c = byte as char;
        if c.is_ascii_alphanumeric() || "-_.!~*'()".contains(c) {
            out.push(c);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

const MAX_BODY_CHARS: usize = 40_000;

fn list_sessions() -> anyhow::Result<Vec<SessionRef>> {
    let summary_files = find_files(&sessions_dir(), |p| p.file_name().map(|n| n == "summary.json").unwrap_or(false));
    let mut out = Vec::new();
    for summary_file in summary_files {
        let Some(session_dir) = summary_file.parent() else { continue };
        let chat_file = session_dir.join("chat_history.jsonl");
        let Ok(summary_text) = std::fs::read_to_string(&summary_file) else { continue };
        let Ok(summary) = serde_json::from_str::<Value>(&summary_text) else { continue };
        let info = summary.get("info");
        let Some(session_id) = info.and_then(|i| i.get("id")).and_then(|v| v.as_str()) else { continue };
        let Some(cwd) = info.and_then(|i| i.get("cwd")).and_then(|v| v.as_str()) else { continue };

        let mut first_user_text = String::new();
        let mut body = BodySampler::new(MAX_BODY_CHARS);
        let mut stopped_early = false;
        for obj in read_jsonl_lines_lazy(&chat_file) {
            let obj_type = obj.get("type").and_then(|v| v.as_str()).unwrap_or("");
            if obj_type == "user" {
                if let Some(content) = obj.get("content").and_then(|v| v.as_array()) {
                    for b in content {
                        let text = b.get("text").and_then(|v| v.as_str()).unwrap_or("");
                        if let Some(q) = extract_user_query(text) {
                            if first_user_text.is_empty() {
                                first_user_text = q.clone();
                            }
                            body.append(&q);
                            break;
                        }
                    }
                }
            } else if obj_type == "assistant" {
                if let Some(c) = obj.get("content").and_then(|v| v.as_str()) {
                    let t = c.trim();
                    if !t.is_empty() {
                        body.append(t);
                    }
                }
            }
            if !first_user_text.is_empty() && body.has_head() {
                stopped_early = true;
                break;
            }
        }
        if stopped_early {
            body.mark_sampled();
            for obj in read_jsonl_tail_lines(&chat_file) {
                let obj_type = obj.get("type").and_then(|v| v.as_str()).unwrap_or("");
                if obj_type == "user" {
                    if let Some(content) = obj.get("content").and_then(|v| v.as_array()) {
                        for b in content {
                            let text = b.get("text").and_then(|v| v.as_str()).unwrap_or("");
                            if let Some(q) = extract_user_query(text) {
                                body.append(&q);
                                break;
                            }
                        }
                    }
                } else if obj_type == "assistant" {
                    if let Some(c) = obj.get("content").and_then(|v| v.as_str()) {
                        let t = c.trim();
                        if !t.is_empty() {
                            body.append(t);
                        }
                    }
                }
            }
        }

        let generated_title = summary.get("generated_title").and_then(|v| v.as_str()).unwrap_or("");
        let title_source = if !generated_title.is_empty() { generated_title } else { &first_user_text };
        let title = clean_title(title_source);
        let title = if title.is_empty() { "(empty)".to_string() } else { title };
        out.push(SessionRef {
            tool: ToolName::Grok,
            session_id: session_id.to_string(),
            project_path: cwd.to_string(),
            snippet: first_user_text.chars().take(200).collect(),
            title,
            body: Some(body.value()),
            updated_at: mtime_ms(&chat_file),
            raw: Some(json!({ "file": chat_file.to_string_lossy() })),
            match_snippet: None,
        });
    }
    Ok(out)
}

/// chat_history.jsonl (used by list_sessions() above) has no tool-call data
/// at all -- it turns out that was looking in the wrong file: updates.jsonl
/// (the ACP `session/update` display stream, already written for the TUI)
/// DOES carry real tool_call/tool_call_update events with actual
/// input/output, confirmed by generating real sessions with `grok -p ...
/// --always-approve` and inspecting the output directly. It's also a
/// complete substitute for chat_history.jsonl's text turns, so read()
/// sources from this single richer file instead of two.
fn read_impl(session_ref: &SessionRef) -> anyhow::Result<Vec<Turn>> {
    let chat_file = session_ref
        .raw
        .as_ref()
        .and_then(|r| r.get("file"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("grok: session ref missing raw.file"))?;
    let updates_file = if chat_file.ends_with("chat_history.jsonl") {
        format!("{}updates.jsonl", &chat_file[..chat_file.len() - "chat_history.jsonl".len()])
    } else {
        chat_file.to_string()
    };
    let lines = read_jsonl_lines(Path::new(&updates_file));
    let mut turns: Vec<Turn> = Vec::new();
    let mut last_recap: Option<String> = None;

    // Images arrive as their OWN user_message_chunk event, separate from
    // the text chunk for the same turn -- both need buffering until the
    // turn actually changes, not pushed as a Turn immediately.
    let mut user_text_parts: Vec<String> = Vec::new();
    let mut user_attachments: Vec<Attachment> = Vec::new();
    let mut assistant_text_parts: Vec<String> = Vec::new();
    let mut pending_tool_calls: Vec<ToolCallRecord> = Vec::new();
    let mut pending_attachments: Vec<Attachment> = Vec::new();
    let mut call_index: HashMap<String, usize> = HashMap::new();
    #[derive(PartialEq, Clone, Copy)]
    enum LastRole {
        User,
        Assistant,
    }
    let mut last_role: Option<LastRole> = None;

    fn flush_user(turns: &mut Vec<Turn>, user_text_parts: &mut Vec<String>, user_attachments: &mut Vec<Attachment>) {
        let text = user_text_parts.join("\n\n").trim().to_string();
        if !text.is_empty() || !user_attachments.is_empty() {
            turns.push(Turn {
                role: Role::User,
                text,
                tool_calls: None,
                attachments: if user_attachments.is_empty() { None } else { Some(std::mem::take(user_attachments)) },
            });
        }
        user_text_parts.clear();
        user_attachments.clear();
    }
    fn flush_assistant(
        turns: &mut Vec<Turn>,
        assistant_text_parts: &mut Vec<String>,
        pending_tool_calls: &mut Vec<ToolCallRecord>,
        pending_attachments: &mut Vec<Attachment>,
        call_index: &mut HashMap<String, usize>,
    ) {
        // NOTE: joined with "" not "\n\n" -- chunks stream incrementally
        // during generation and are already segmented naturally.
        let text = assistant_text_parts.join("").trim().to_string();
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
    fn ensure_role(
        role: LastRole,
        last_role: &mut Option<LastRole>,
        turns: &mut Vec<Turn>,
        user_text_parts: &mut Vec<String>,
        user_attachments: &mut Vec<Attachment>,
        assistant_text_parts: &mut Vec<String>,
        pending_tool_calls: &mut Vec<ToolCallRecord>,
        pending_attachments: &mut Vec<Attachment>,
        call_index: &mut HashMap<String, usize>,
    ) {
        if let Some(lr) = *last_role {
            if lr != role {
                if lr == LastRole::User {
                    flush_user(turns, user_text_parts, user_attachments);
                } else {
                    flush_assistant(turns, assistant_text_parts, pending_tool_calls, pending_attachments, call_index);
                }
            }
        }
        *last_role = Some(role);
    }

    for obj in lines {
        let Some(update) = obj.get("params").and_then(|p| p.get("update")) else { continue };
        let kind = update.get("sessionUpdate").and_then(|v| v.as_str()).unwrap_or("");

        if kind == "session_recap" {
            if let Some(summary) = update.get("summary").and_then(|v| v.as_str()) {
                let summary = summary.trim();
                if !summary.is_empty() {
                    last_recap = Some(summary.to_string());
                }
            }
            continue;
        }
        if kind == "user_message_chunk" {
            ensure_role(LastRole::User, &mut last_role, &mut turns, &mut user_text_parts, &mut user_attachments, &mut assistant_text_parts, &mut pending_tool_calls, &mut pending_attachments, &mut call_index);
            let content = update.get("content");
            let ctype = content.and_then(|c| c.get("type")).and_then(|v| v.as_str());
            if ctype == Some("image") {
                if let Some(data) = content.and_then(|c| c.get("data")).and_then(|v| v.as_str()) {
                    let mime = content.and_then(|c| c.get("mimeType")).and_then(|v| v.as_str()).unwrap_or("image/png");
                    user_attachments.push(Attachment { mime_type: mime.to_string(), base64: data.to_string(), filename: None });
                }
            } else if let Some(text) = content.and_then(|c| c.get("text")).and_then(|v| v.as_str()) {
                let t = text.trim();
                if !t.is_empty() {
                    user_text_parts.push(t.to_string());
                }
            }
            continue;
        }
        if kind == "agent_message_chunk" {
            ensure_role(LastRole::Assistant, &mut last_role, &mut turns, &mut user_text_parts, &mut user_attachments, &mut assistant_text_parts, &mut pending_tool_calls, &mut pending_attachments, &mut call_index);
            // Chunks stream incrementally during generation -- concatenate
            // every chunk for this turn, don't just take the first/last one.
            if let Some(text) = update.get("content").and_then(|c| c.get("text")).and_then(|v| v.as_str()) {
                if !text.is_empty() {
                    assistant_text_parts.push(text.to_string());
                }
            }
            continue;
        }
        if kind == "tool_call" {
            ensure_role(LastRole::Assistant, &mut last_role, &mut turns, &mut user_text_parts, &mut user_attachments, &mut assistant_text_parts, &mut pending_tool_calls, &mut pending_attachments, &mut call_index);
            let meta_name = update.get("_meta").and_then(|m| m.get("x.ai/tool")).and_then(|t| t.get("name")).and_then(|v| v.as_str());
            let name = meta_name
                .or_else(|| update.get("title").and_then(|v| v.as_str()))
                .unwrap_or("unknown_tool")
                .to_string();
            let input = update.get("rawInput").cloned().unwrap_or_else(|| json!({})).to_string();
            pending_tool_calls.push(ToolCallRecord { name, input, output: None });
            if let Some(id) = update.get("toolCallId").and_then(|v| v.as_str()) {
                call_index.insert(id.to_string(), pending_tool_calls.len() - 1);
            }
            continue;
        }
        if kind == "tool_call_update" {
            let idx = update.get("toolCallId").and_then(|v| v.as_str()).and_then(|id| call_index.get(id).copied());
            let Some(idx) = idx else { continue };
            // Multiple updates can arrive per call (in_progress, then
            // completed) -- each overwrite just keeps the latest, which
            // ends up being the final state once the stream finishes. A
            // tool result can carry a real image too -- same flat {type,
            // data, mimeType} shape as the user_message_chunk image case.
            let mut out = String::new();
            if let Some(items) = update.get("content").and_then(|v| v.as_array()) {
                let texts: Vec<String> = items
                    .iter()
                    .filter_map(|c| c.get("content").and_then(|cc| cc.get("text")).and_then(|v| v.as_str()))
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string())
                    .collect();
                out = texts.join("\n");
                for c in items {
                    let cc = c.get("content");
                    if cc.and_then(|v| v.get("type")).and_then(|v| v.as_str()) == Some("image") {
                        if let Some(data) = cc.and_then(|v| v.get("data")).and_then(|v| v.as_str()) {
                            let mime = cc.and_then(|v| v.get("mimeType")).and_then(|v| v.as_str()).unwrap_or("image/png");
                            pending_attachments.push(Attachment { mime_type: mime.to_string(), base64: data.to_string(), filename: None });
                        }
                    }
                }
            }
            if out.is_empty() {
                if let Some(raw_output) = update.get("rawOutput") {
                    out = match raw_output {
                        Value::String(s) => s.clone(),
                        v => v.to_string(),
                    };
                }
            }
            if !out.is_empty() {
                pending_tool_calls[idx].output = Some(truncate(&out, MAX_TOOL_OUTPUT_CHARS));
            }
            continue;
        }
    }
    flush_user(&mut turns, &mut user_text_parts, &mut user_attachments);
    flush_assistant(&mut turns, &mut assistant_text_parts, &mut pending_tool_calls, &mut pending_attachments, &mut call_index);
    if let Some(summary) = last_recap {
        turns.insert(
            0,
            Turn {
                role: Role::User,
                text: format!("{}\n{summary}", crate::util::NATIVE_COMPACT_MARKER),
                tool_calls: None,
                attachments: None,
            },
        );
    }
    Ok(turns)
}

fn real_cwd(project_path: &str) -> anyhow::Result<String> {
    Ok(crate::util::canonicalize_create(project_path)?)
}

fn write_impl(turns: &[Turn], project_path: &str) -> anyhow::Result<String> {
    let real_cwd_str = real_cwd(project_path)?;
    let encoded_cwd = encode_uri_component(&real_cwd_str);

    let new_id = Uuid::new_v4().to_string();
    let session_dir = sessions_dir().join(&encoded_cwd).join(&new_id);
    std::fs::create_dir_all(&session_dir)?;

    let now = chrono::Utc::now();
    let mut lines: Vec<String> = vec![
        json!({ "type": "system", "content": "You are Grok, an interactive CLI tool that helps users with software engineering tasks." }).to_string(),
        json!({
            "type": "user",
            "content": [{
                "type": "text",
                "text": format!("<user_info>\nOS Version: macos\nShell: /bin/zsh\nWorkspace Path: {}\nToday's date: {}\n</user_info>", real_cwd_str, now.format("%Y-%m-%d")),
            }],
        })
        .to_string(),
    ];

    let mut prompt_idx = 0i64;
    for turn in turns {
        if turn.role == Role::User {
            let attachments = turn.attachments.clone().unwrap_or_default();
            // Real chat_history.jsonl image shape: a {type:"image",
            // url:"data:...;base64,..."} block in the same message's
            // content array as the text block (Codex-style url field, not
            // Claude/Pi's nested `source` object).
            let image_blocks: Vec<Value> = attachments
                .iter()
                .filter(|a| a.mime_type.starts_with("image/"))
                .map(|img| json!({ "type": "image", "url": format!("data:{};base64,{}", img.mime_type, img.base64) }))
                .collect();
            // Grok's own @file mechanism inlines non-image attachments as
            // readable text, extracted by grok itself -- we only have
            // base64 bytes, not extracted text, so non-image attachments
            // get a placeholder note.
            let non_image_note = attachments
                .iter()
                .filter(|a| !a.mime_type.starts_with("image/"))
                .map(|a| format!("[attached file: {} ({})]", a.filename.clone().unwrap_or_else(|| "unnamed".into()), a.mime_type))
                .collect::<Vec<_>>()
                .join("\n");
            let query_text: String = [turn.text.clone(), non_image_note].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join("\n\n");
            let mut content = vec![json!({ "type": "text", "text": format!("<user_query>\n{query_text}\n</user_query>") })];
            content.extend(image_blocks);
            lines.push(json!({ "type": "user", "content": content, "prompt_index": prompt_idx }).to_string());
            prompt_idx += 1;
        } else {
            // Real tool_calls shape: a flat {id, name, arguments} array on
            // the assistant entry (not OpenAI-style {type:"function",
            // function:{...}}), each followed by its own separate
            // tool_result entry.
            let tool_calls = turn.tool_calls.clone().unwrap_or_default();
            let real_tool_calls: Vec<(String, Value)> = tool_calls
                .iter()
                .map(|tc| {
                    let id = format!("call-{}-0", Uuid::new_v4());
                    (id.clone(), json!({ "id": id, "name": tc.name, "arguments": tc.input }))
                })
                .collect();
            let attachments = turn.attachments.clone().unwrap_or_default();
            let assistant_images: Vec<&Attachment> = attachments.iter().filter(|a| a.mime_type.starts_with("image/")).collect();
            let non_image_note = attachments
                .iter()
                .filter(|a| !a.mime_type.starts_with("image/"))
                .map(|a| format!("[attached file: {} ({})]", a.filename.clone().unwrap_or_else(|| "unnamed".into()), a.mime_type))
                .collect::<Vec<_>>()
                .join("\n");
            // An assistant-side image has no assistant-message-level slot
            // in this format -- every real example found one only inside a
            // tool_result's own `images` array. Reproduced with a synthetic
            // tool_call/tool_result pair rather than forcing it somewhere
            // Grok's own sessions never actually put one.
            let img_tool_call_id = if !assistant_images.is_empty() { Some(format!("call-{}-img", Uuid::new_v4())) } else { None };
            let mut all_tool_calls: Vec<Value> = real_tool_calls.iter().map(|(_, v)| v.clone()).collect();
            if let Some(id) = &img_tool_call_id {
                all_tool_calls.push(json!({ "id": id, "name": "read_image", "arguments": "{}" }));
            }
            let combined_text: String = [turn.text.clone(), non_image_note].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join("\n\n");
            let mut assistant_entry = json!({
                "type": "assistant",
                "content": combined_text,
                "model_id": "grok-4.5-build",
                "model_fingerprint": "fp_handoff",
                "reasoning_effort": "low",
            });
            if !all_tool_calls.is_empty() {
                if let Some(obj) = assistant_entry.as_object_mut() {
                    obj.insert("tool_calls".to_string(), Value::Array(all_tool_calls));
                }
            }
            lines.push(assistant_entry.to_string());

            for (i, tc) in tool_calls.iter().enumerate() {
                lines.push(json!({ "type": "tool_result", "tool_call_id": real_tool_calls[i].0, "content": tc.output.clone().unwrap_or_default() }).to_string());
            }
            if let Some(id) = &img_tool_call_id {
                let images: Vec<Value> = assistant_images
                    .iter()
                    .map(|img| json!({ "type": "image", "url": format!("data:{};base64,{}", img.mime_type, img.base64) }))
                    .collect();
                lines.push(json!({ "type": "tool_result", "tool_call_id": id, "content": "Read image file", "images": images }).to_string());
            }
        }
    }
    std::fs::write(session_dir.join("chat_history.jsonl"), lines.join("\n") + "\n")?;

    // chat_history.jsonl alone launches `grok --resume` fine (it's what
    // backs API continuation) but the TUI doesn't show prior turns from it
    // -- a real session directory also has updates.jsonl, an Agent Client
    // Protocol (ACP) `session/update` event stream that's what the TUI
    // actually replays to render history. Without it, resume "works" but
    // the conversation looks empty.
    let session_start_sec = now.timestamp();
    let mut updates: Vec<String> = Vec::new();
    let mut update_prompt_idx = 0i64;
    for (i, turn) in turns.iter().enumerate() {
        let event_num = i as i64 + 1;
        let ts = session_start_sec + i as i64;
        if crate::util::is_hop_context_only(turn) {
            continue;
        }
        if turn.role == Role::User {
            updates.push(json!({
                "timestamp": ts,
                "method": "session/update",
                "params": {
                    "sessionId": new_id,
                    "update": {
                        "sessionUpdate": "user_message_chunk",
                        "content": { "type": "text", "text": turn.text },
                        "_meta": { "modelId": "grok-4.5", "promptIndex": update_prompt_idx },
                    },
                    "_meta": { "eventId": format!("{new_id}-{event_num}"), "agentTimestampMs": ts * 1000 },
                },
            })
            .to_string());
            // Real shape: images arrive as their OWN user_message_chunk
            // (flat {type, data, mimeType}), sharing the same promptIndex
            // as the text chunk for that turn.
            for (img_i, img) in turn.attachments.iter().flatten().filter(|a| a.mime_type.starts_with("image/")).enumerate() {
                updates.push(json!({
                    "timestamp": ts,
                    "method": "session/update",
                    "params": {
                        "sessionId": new_id,
                        "update": {
                            "sessionUpdate": "user_message_chunk",
                            "content": { "type": "image", "data": img.base64, "mimeType": img.mime_type },
                            "_meta": { "modelId": "grok-4.5", "promptIndex": update_prompt_idx },
                        },
                        "_meta": { "eventId": format!("{new_id}-{event_num}-img{img_i}"), "agentTimestampMs": ts * 1000 },
                    },
                })
                .to_string());
            }
            update_prompt_idx += 1;
        } else {
            // Real tool_call/tool_call_update shape: one tool_call (has the
            // input) plus one tool_call_update with status "completed" (has
            // the output) per call, matched by toolCallId, emitted before
            // the narration text that follows it (matches real ordering).
            for tc in turn.tool_calls.iter().flatten() {
                let tool_call_id = format!("call-{}-0", Uuid::new_v4());
                updates.push(json!({
                    "timestamp": ts,
                    "method": "session/update",
                    "params": {
                        "sessionId": new_id,
                        "update": {
                            "sessionUpdate": "tool_call",
                            "toolCallId": tool_call_id,
                            "title": tc.name,
                            "rawInput": safe_json_parse(&tc.input),
                            "_meta": { "x.ai/tool": { "version": 1, "name": tc.name, "kind": "execute", "namespace": "grok_build", "label": tc.name, "read_only": false } },
                        },
                        "_meta": { "eventId": format!("{new_id}-{event_num}-tool"), "agentTimestampMs": ts * 1000 },
                    },
                })
                .to_string());
                updates.push(json!({
                    "timestamp": ts,
                    "method": "session/update",
                    "params": {
                        "sessionId": new_id,
                        "update": {
                            "sessionUpdate": "tool_call_update",
                            "toolCallId": tool_call_id,
                            "status": "completed",
                            "content": [{ "type": "content", "content": { "type": "text", "text": tc.output.clone().unwrap_or_default() } }],
                            "rawOutput": tc.output.clone().unwrap_or_default(),
                        },
                        "_meta": { "eventId": format!("{new_id}-{event_num}-tool-done"), "agentTimestampMs": ts * 1000 },
                    },
                })
                .to_string());
            }
            // Assistant-side images have no message-level slot in this
            // format -- same synthetic tool_call/tool_call_update
            // reproduction as chat_history.jsonl above, matching where a
            // real one actually showed up, read back correctly by this
            // adapter's own tool_call_update image handling in read().
            for img in turn.attachments.iter().flatten().filter(|a| a.mime_type.starts_with("image/")) {
                let tool_call_id = format!("call-{}-img", Uuid::new_v4());
                updates.push(json!({
                    "timestamp": ts,
                    "method": "session/update",
                    "params": {
                        "sessionId": new_id,
                        "update": { "sessionUpdate": "tool_call", "toolCallId": tool_call_id, "title": "read_image", "rawInput": {} },
                        "_meta": { "eventId": format!("{new_id}-{event_num}-imgtool"), "agentTimestampMs": ts * 1000 },
                    },
                })
                .to_string());
                updates.push(json!({
                    "timestamp": ts,
                    "method": "session/update",
                    "params": {
                        "sessionId": new_id,
                        "update": {
                            "sessionUpdate": "tool_call_update",
                            "toolCallId": tool_call_id,
                            "status": "completed",
                            "content": [{ "type": "content", "content": { "type": "image", "data": img.base64, "mimeType": img.mime_type } }],
                        },
                        "_meta": { "eventId": format!("{new_id}-{event_num}-imgtool-done"), "agentTimestampMs": ts * 1000 },
                    },
                })
                .to_string());
            }
            updates.push(json!({
                "timestamp": ts,
                "method": "session/update",
                "params": {
                    "sessionId": new_id,
                    "update": { "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": turn.text } },
                    "_meta": {
                        "totalTokens": 0,
                        "eventId": format!("{new_id}-{event_num}"),
                        "agentTimestampMs": ts * 1000,
                        "promptId": new_id,
                        "streamStartMs": ts * 1000,
                        "turnStartMs": ts * 1000,
                        "updateType": "AgentMessageChunk",
                        "chunkId": event_num,
                    },
                },
            })
            .to_string());
        }
    }
    std::fs::write(session_dir.join("updates.jsonl"), updates.join("\n") + "\n")?;

    let now_iso = now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let real_title: String = turns
        .iter()
        .find(|t| t.role == Role::User)
        .map(|t| t.text.clone())
        .unwrap_or_else(|| "Resumed via agent-hop".to_string())
        .chars()
        .take(80)
        .collect();
    let summary = json!({
        "info": { "id": new_id, "cwd": real_cwd_str },
        "session_summary": real_title,
        "created_at": now_iso,
        "updated_at": now_iso,
        "num_messages": turns.len(),
        "num_chat_messages": lines.len(),
        "current_model_id": "grok-4.5",
        "next_trace_turn": 1,
        "chat_format_version": 1,
        "request_id": Uuid::new_v4().to_string(),
        "grok_home": dirs::home_dir().unwrap_or_default().join(".grok").to_string_lossy(),
        "last_active_at": now_iso,
        "generated_title": real_title,
        "agent_name": "grok-build-plan",
        "sandbox_profile": "off",
        "reasoning_effort": "low",
    });
    std::fs::write(session_dir.join("summary.json"), serde_json::to_string_pretty(&summary)?)?;

    Ok(new_id)
}

fn resume_cmd_impl(session_id: &str, _project_path: &str) -> Vec<String> {
    vec!["grok".to_string(), "--resume".to_string(), session_id.to_string()]
}

pub struct GrokAdapter;

impl Adapter for GrokAdapter {
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
    fn find_latest_for_path(&self, project_path: &str) -> Option<SessionRef> {
        find_latest_for_path_impl(project_path)
    }
}

/// Fast path for hop lookups -- same rationale as Claude/pi: Grok's own
/// directory layout already encodes the project path (`write_impl`'s
/// `encode_uri_component`), one subdirectory per session underneath that,
/// so the newest session for a path is a directory listing + mtime
/// comparison, no file content needs reading.
fn find_latest_for_path_impl(project_path: &str) -> Option<SessionRef> {
    let real_cwd_str = real_cwd(project_path).ok()?;
    let dir = sessions_dir().join(encode_uri_component(&real_cwd_str));
    let entries = std::fs::read_dir(&dir).ok()?;
    let latest_dir = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .max_by_key(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok())?;
    let session_id = latest_dir.file_name()?.to_string_lossy().to_string();
    let chat_file = latest_dir.join("chat_history.jsonl");
    if !chat_file.exists() {
        return None;
    }
    Some(SessionRef {
        tool: ToolName::Grok,
        session_id,
        project_path: real_cwd_str,
        title: String::new(),
        snippet: String::new(),
        body: None,
        updated_at: 0,
        raw: Some(json!({ "file": chat_file.to_string_lossy() })),
        match_snippet: None,
    })
}
