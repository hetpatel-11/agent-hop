//! Port of src/adapters/claude.ts -- JSONL-based, Claude's tool_use/
//! tool_result API convention, a separate top-level "attachment" record
//! type for @file mentions, image/document content blocks, tool-call ID
//! matching, char-budget-aware body sampling for listing, and a
//! Windows-path-safe `encodeDir`/`basename` handling. Faithful, literal
//! port -- comments below are carried over (adapted to Rust) from the TS
//! source since they document real, hard-won behavior.

use crate::adapters::{Adapter, Attachment, Role, SessionRef, Turn, ToolCallRecord};
use crate::agents::ToolName;
use crate::util::{
    clean_title, find_files, mtime_ms, read_jsonl_lines, read_jsonl_lines_lazy,
    read_jsonl_tail_lines, truncate, to_tool_input_object, BodySampler, MAX_TOOL_OUTPUT_CHARS,
    MIN_TITLE_CHARS,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use uuid::Uuid;

fn projects_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_default().join(".claude").join("projects")
}

/// The real installed Claude Code version, e.g. "2.1.198" -- a hardcoded
/// guess here inevitably goes stale the moment Claude Code updates itself
/// (confirmed happening for real with opencode's equivalent field while
/// auditing this). Falls back to a generic placeholder if claude isn't on
/// PATH, which would fail write() well before this matters in practice.
fn claude_cli_version() -> String {
    match crate::agents::std_command_bin("claude", &["--version"]).output() {
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

fn encode_dir(cwd: &str) -> String {
    let real = crate::util::canonicalize_display(cwd);
    // Claude Code replaces every non-alphanumeric character with "-", not just "/".
    real.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

const MAX_BODY_CHARS: usize = 40_000;

fn message_text_join_space(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(arr) => arr
            .iter()
            .filter_map(|b| {
                let obj = b.as_object()?;
                if obj.get("type")?.as_str()? == "text" {
                    obj.get("text")?.as_str().map(|s| s.to_string())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    }
}

fn list_sessions() -> anyhow::Result<Vec<SessionRef>> {
    let files = find_files(&projects_dir(), |p| {
        p.extension().map(|e| e == "jsonl").unwrap_or(false)
    });
    let mut out = Vec::new();
    for file in files {
        let mut cwd: Option<String> = None;
        let mut first_user_text = String::new(); // any length -- fallback
        let mut title_text = String::new(); // first *substantive* user message -- preferred
        let mut body = BodySampler::new(MAX_BODY_CHARS);
        let mut stopped_early = false;

        for obj in read_jsonl_lines_lazy(&file) {
            if let Some(c) = obj.get("cwd").and_then(|v| v.as_str()) {
                cwd = Some(c.to_string());
            }
            let obj_type = obj.get("type").and_then(|v| v.as_str()).unwrap_or("");
            if obj_type != "user" && obj_type != "assistant" {
                continue;
            }
            let message = obj.get("message");
            let content = message.and_then(|m| m.get("content"));
            let text = content.map(message_text_join_space).unwrap_or_default();
            if text.is_empty() {
                continue;
            }
            if obj_type == "user" {
                if first_user_text.is_empty() {
                    first_user_text = text.clone();
                }
                if title_text.is_empty() && text.chars().count() >= MIN_TITLE_CHARS {
                    title_text = text.clone();
                }
            }
            body.append(&text);
            if cwd.is_some() && !title_text.is_empty() && body.has_head() {
                stopped_early = true;
                break;
            }
        }
        if stopped_early {
            body.mark_sampled();
            for obj in read_jsonl_tail_lines(&file) {
                let obj_type = obj.get("type").and_then(|v| v.as_str()).unwrap_or("");
                if obj_type != "user" && obj_type != "assistant" {
                    continue;
                }
                let content = obj.get("message").and_then(|m| m.get("content"));
                let text = content.map(message_text_join_space).unwrap_or_default();
                if !text.is_empty() {
                    body.append(&text);
                }
            }
        }
        let Some(cwd) = cwd else { continue };
        // `file` comes from find_files(), which is platform-aware already
        // (PathBuf); using file_stem() is the Rust equivalent of Node's
        // basename() + stripping ".jsonl" -- correct regardless of
        // separator style, avoiding the historical bug where splitting on a
        // literal "/" corrupted sessionId on Windows.
        let session_id = file
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let title = clean_title(if !title_text.is_empty() { &title_text } else { &first_user_text });
        let title = if title.is_empty() { "(empty)".to_string() } else { title };
        out.push(SessionRef {
            tool: ToolName::Claude,
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

fn extract_attachment_block(block: &serde_json::Map<String, Value>) -> Option<Attachment> {
    let src = block.get("source")?.as_object()?;
    if src.get("type")?.as_str()? == "base64" {
        let data = src.get("data")?.as_str()?.to_string();
        let mime_type = src
            .get("media_type")
            .and_then(|v| v.as_str())
            .unwrap_or("application/octet-stream")
            .to_string();
        return Some(Attachment { mime_type, base64: data, filename: None });
    }
    None
}

/// Claude's own API convention represents a tool call as `tool_use` inside
/// an assistant message, and its result as `tool_result` inside a
/// *following* message with role "user" -- that's API plumbing, not
/// something the human actually typed. Both get folded into the enclosing
/// assistant turn (matched by id/tool_use_id) instead of surfacing as a
/// fake human message; only content blocks the human genuinely sent (or a
/// real final assistant reply) become their own Turn.
///
/// Separately: `@file` mentions don't appear in the message content array
/// at all -- Claude Code logs them as an entirely distinct top-level
/// record, `{type:"attachment", attachment:{type:"file", filename,
/// content:{type:"text"|"pdf", file:{...}}}}`, arriving *after* the user
/// message it belongs to (confirmed by generating real sessions with a
/// real @file.pdf and @file.txt reference and inspecting the raw output --
/// not documented anywhere, found by diffing the raw file). That's why
/// user turns are now buffered (like assistant turns already were) instead
/// of pushed immediately: the attachment record needs to land in the same
/// turn as the message that referenced it, and it arrives on a later line.
/// The same top-level `type:"attachment"` record also carries unrelated
/// session bookkeeping (hook results, skill/agent listings, deferred-tool
/// deltas) -- only `attachment.type === "file"` is a real user-turn
/// attachment.
fn read_impl(session_ref: &SessionRef) -> anyhow::Result<Vec<Turn>> {
    let file = session_ref
        .raw
        .as_ref()
        .and_then(|r| r.get("file"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("claude: session ref missing raw.file"))?;
    let lines = read_jsonl_lines(Path::new(file));
    let mut turns: Vec<Turn> = Vec::new();

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

    fn flush_user(
        turns: &mut Vec<Turn>,
        user_text_parts: &mut Vec<String>,
        user_attachments: &mut Vec<Attachment>,
    ) {
        let text = user_text_parts.join("\n\n").trim().to_string();
        if !text.is_empty() || !user_attachments.is_empty() {
            turns.push(Turn {
                role: Role::User,
                text,
                tool_calls: None,
                attachments: if user_attachments.is_empty() {
                    None
                } else {
                    Some(std::mem::take(user_attachments))
                },
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
        let text = assistant_text_parts.join("\n\n").trim().to_string();
        if !text.is_empty() || !pending_tool_calls.is_empty() || !pending_attachments.is_empty() {
            turns.push(Turn {
                role: Role::Assistant,
                text,
                tool_calls: if pending_tool_calls.is_empty() {
                    None
                } else {
                    Some(std::mem::take(pending_tool_calls))
                },
                attachments: if pending_attachments.is_empty() {
                    None
                } else {
                    Some(std::mem::take(pending_attachments))
                },
            });
        }
        assistant_text_parts.clear();
        pending_tool_calls.clear();
        pending_attachments.clear();
        call_index.clear();
    }

    for obj in lines {
        let obj_type = obj.get("type").and_then(|v| v.as_str()).unwrap_or("");

        if obj_type == "attachment" {
            if let Some(att) = obj.get("attachment").and_then(|v| v.as_object()) {
                if att.get("type").and_then(|v| v.as_str()) == Some("file") {
                    // @file mentions are always something the human referenced
                    if last_role != Some(LastRole::User) {
                        if last_role == Some(LastRole::Assistant) {
                            flush_assistant(
                                &mut turns,
                                &mut assistant_text_parts,
                                &mut pending_tool_calls,
                                &mut pending_attachments,
                                &mut call_index,
                            );
                        }
                        last_role = Some(LastRole::User);
                    }
                    let filename = att
                        .get("filename")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unnamed")
                        .to_string();
                    let content = att.get("content").and_then(|v| v.as_object());
                    let content_type = content.and_then(|c| c.get("type")).and_then(|v| v.as_str());
                    let file_obj = content.and_then(|c| c.get("file")).and_then(|v| v.as_object());
                    if content_type == Some("text") {
                        if let Some(text_content) =
                            file_obj.and_then(|f| f.get("content")).and_then(|v| v.as_str())
                        {
                            // A real text file -- genuinely readable content,
                            // inline as text (consistent with how
                            // Codex/Pi/Grok already handle this) rather than
                            // needlessly base64-wrapping something that's
                            // already plain text.
                            user_text_parts.push(format!(
                                "<file name=\"{filename}\">\n{text_content}\n</file>"
                            ));
                        }
                    } else if let Some(b64) =
                        file_obj.and_then(|f| f.get("base64")).and_then(|v| v.as_str())
                    {
                        let mime = if content_type == Some("pdf") {
                            "application/pdf"
                        } else {
                            "application/octet-stream"
                        };
                        user_attachments.push(Attachment {
                            mime_type: mime.to_string(),
                            base64: b64.to_string(),
                            filename: Some(filename),
                        });
                    }
                }
            }
            continue; // every other attachment.type is session bookkeeping, not user content
        }

        if obj_type != "user" && obj_type != "assistant" {
            continue;
        }
        let message = obj.get("message").and_then(|v| v.as_object());
        let role = message.and_then(|m| m.get("role")).and_then(|v| v.as_str());
        if role != Some("user") && role != Some("assistant") {
            continue;
        }
        let content = message.and_then(|m| m.get("content"));

        if role == Some("assistant") {
            if last_role != Some(LastRole::Assistant) {
                if last_role == Some(LastRole::User) {
                    flush_user(&mut turns, &mut user_text_parts, &mut user_attachments);
                }
                last_role = Some(LastRole::Assistant);
            }
            match content {
                Some(Value::String(s)) => {
                    if !s.trim().is_empty() {
                        assistant_text_parts.push(s.trim().to_string());
                    }
                }
                Some(Value::Array(arr)) => {
                    for b in arr {
                        let Some(block) = b.as_object() else { continue };
                        let btype = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
                        if btype == "text" {
                            if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                                let t = t.trim();
                                if !t.is_empty() {
                                    assistant_text_parts.push(t.to_string());
                                }
                            }
                        } else if btype == "tool_use" {
                            let name = block
                                .get("name")
                                .and_then(|v| v.as_str())
                                .unwrap_or("unknown_tool")
                                .to_string();
                            let input = block
                                .get("input")
                                .cloned()
                                .unwrap_or_else(|| json!({}));
                            let rec = ToolCallRecord {
                                name,
                                input: input.to_string(),
                                output: None,
                            };
                            pending_tool_calls.push(rec);
                            if let Some(id) = block.get("id").and_then(|v| v.as_str()) {
                                call_index.insert(id.to_string(), pending_tool_calls.len() - 1);
                            }
                        } else if btype == "image" || btype == "document" {
                            if let Some(att) = extract_attachment_block(block) {
                                pending_attachments.push(att);
                            }
                        }
                    }
                }
                _ => {}
            }
            continue;
        }

        // role === "user" -- may be a genuine human message, a tool_result
        // envelope, or (per the API) technically a mix of both. Compute the
        // real content first so a pure tool_result envelope can be skipped
        // *without* touching role state -- it's API plumbing mid-assistant-
        // turn, not a real turn boundary, and must not flush the
        // assistant's still-in-progress tool-call accumulation.
        let mut had_tool_result = false;
        let mut local_text: Vec<String> = Vec::new();
        let mut local_attachments: Vec<Attachment> = Vec::new();
        match content {
            Some(Value::String(s)) => {
                if !s.trim().is_empty() {
                    local_text.push(s.trim().to_string());
                }
            }
            Some(Value::Array(arr)) => {
                for b in arr {
                    let Some(block) = b.as_object() else { continue };
                    let btype = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
                    if btype == "tool_result" {
                        had_tool_result = true;
                        let id = block.get("tool_use_id").and_then(|v| v.as_str());
                        let idx = id.and_then(|i| call_index.get(i).copied());
                        if let Some(idx) = idx {
                            let block_content = block.get("content");
                            match block_content {
                                Some(Value::String(s)) => {
                                    pending_tool_calls[idx].output =
                                        Some(truncate(s, MAX_TOOL_OUTPUT_CHARS));
                                }
                                Some(Value::Array(items)) => {
                                    // A tool_result's content can itself carry
                                    // image/document blocks -- both our own
                                    // synthetic attachment tool_result and a
                                    // genuine real tool (e.g. a
                                    // screenshot/file-read tool) can return
                                    // one this way. Extract those into real
                                    // attachments rather than flattening the
                                    // whole array to escaped JSON text.
                                    let mut text_parts: Vec<String> = Vec::new();
                                    for item in items {
                                        let Some(item_block) = item.as_object() else { continue };
                                        let itype =
                                            item_block.get("type").and_then(|v| v.as_str()).unwrap_or("");
                                        if itype == "text" {
                                            if let Some(t) = item_block.get("text").and_then(|v| v.as_str()) {
                                                text_parts.push(t.to_string());
                                            }
                                        } else if itype == "image" || itype == "document" {
                                            if let Some(att) = extract_attachment_block(item_block) {
                                                pending_attachments.push(att);
                                            }
                                        }
                                    }
                                    if !text_parts.is_empty() {
                                        pending_tool_calls[idx].output = Some(truncate(
                                            &text_parts.join("\n"),
                                            MAX_TOOL_OUTPUT_CHARS,
                                        ));
                                    }
                                }
                                other => {
                                    let s = other
                                        .map(|v| v.to_string())
                                        .unwrap_or_else(|| "\"\"".to_string());
                                    pending_tool_calls[idx].output =
                                        Some(truncate(&s, MAX_TOOL_OUTPUT_CHARS));
                                }
                            }
                        }
                    } else if btype == "text" {
                        if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                            let t = t.trim();
                            if !t.is_empty() {
                                local_text.push(t.to_string());
                            }
                        }
                    } else if btype == "image" || btype == "document" {
                        if let Some(att) = extract_attachment_block(block) {
                            local_attachments.push(att);
                        }
                    }
                }
            }
            _ => {}
        }
        if had_tool_result && local_text.is_empty() && local_attachments.is_empty() {
            continue;
        }

        if last_role != Some(LastRole::User) {
            if last_role == Some(LastRole::Assistant) {
                flush_assistant(
                    &mut turns,
                    &mut assistant_text_parts,
                    &mut pending_tool_calls,
                    &mut pending_attachments,
                    &mut call_index,
                );
            }
            last_role = Some(LastRole::User);
        }
        user_text_parts.extend(local_text);
        user_attachments.extend(local_attachments);
    }
    flush_user(&mut turns, &mut user_text_parts, &mut user_attachments);
    flush_assistant(
        &mut turns,
        &mut assistant_text_parts,
        &mut pending_tool_calls,
        &mut pending_attachments,
        &mut call_index,
    );
    Ok(turns)
}

fn real_cwd(project_path: &str) -> anyhow::Result<String> {
    Ok(crate::util::canonicalize_create(project_path)?)
}

fn short_id(prefix: &str) -> String {
    format!("{prefix}{}", Uuid::new_v4().simple().to_string().chars().take(24).collect::<String>())
}

fn write_impl(turns: &[Turn], project_path: &str) -> anyhow::Result<String> {
    let real_cwd_str = real_cwd(project_path)?;
    let cli_version = claude_cli_version(); // computed once, reused per turn below
    let encoded = encode_dir(&real_cwd_str);
    let session_dir = projects_dir().join(encoded);
    std::fs::create_dir_all(&session_dir)?;

    let new_id = Uuid::new_v4().to_string();
    let out_path = session_dir.join(format!("{new_id}.jsonl"));

    let mut lines: Vec<String> = Vec::new();
    let mut parent_uuid: Option<String> = None;
    let mut last_uuid: Option<String> = None;

    for turn in turns {
        let my_uuid = Uuid::new_v4().to_string();
        let ts = crate::util::iso_string_now();
        // Real tool_use/tool_result blocks, not inlined text -- confirmed
        // safe by actually forging one (including a tool name Claude Code
        // never registered, e.g. Codex's "exec_command") and resuming it
        // for real: it loaded and the model correctly recalled the exact
        // fake result, with zero schema/validation error. Claude Code's
        // loader just replays whatever's in the transcript; it doesn't
        // validate tool names against a whitelist. This is what makes a
        // resumed tool call read as a genuine native tool card instead of
        // markdown text glued onto a message.
        let attachments = turn.attachments.clone().unwrap_or_default();
        let other_note = attachments
            .iter()
            .filter(|a| !a.mime_type.starts_with("image/") && a.mime_type != "application/pdf")
            .map(|a| format!("[attached file: {} ({})]", a.filename.clone().unwrap_or_else(|| "unnamed".into()), a.mime_type))
            .collect::<Vec<_>>()
            .join("\n");
        let combined_text: String = [turn.text.clone(), other_note]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n");
        let attachment_blocks: Vec<Value> = attachments
            .iter()
            .filter(|a| a.mime_type.starts_with("image/") || a.mime_type == "application/pdf")
            .map(|a| {
                json!({
                    "type": if a.mime_type == "application/pdf" { "document" } else { "image" },
                    "source": { "type": "base64", "media_type": a.mime_type, "data": a.base64 }
                })
            })
            .collect();

        if turn.role == Role::User {
            let content: Value = if !attachment_blocks.is_empty() {
                let mut arr = attachment_blocks.clone();
                if !combined_text.is_empty() {
                    arr.push(json!({ "type": "text", "text": combined_text }));
                }
                Value::Array(arr)
            } else {
                Value::String(combined_text.clone())
            };
            let mut entry = json!({
                "parentUuid": parent_uuid,
                "isSidechain": false,
                "promptId": Uuid::new_v4().to_string(),
                "type": "user",
                "message": { "role": "user", "content": content },
                "uuid": my_uuid,
                "timestamp": ts,
                "userType": "external",
                "entrypoint": "cli",
                "cwd": real_cwd_str,
                "sessionId": new_id,
                "version": cli_version,
            });
            // Same flags Claude writes for its own auto-compact: the model
            // still sees the text; the TUI does not paint it as a user bubble.
            if crate::util::is_hop_context_only(turn) {
                if let Some(obj) = entry.as_object_mut() {
                    obj.insert("isCompactSummary".to_string(), json!(true));
                    obj.insert("isVisibleInTranscriptOnly".to_string(), json!(true));
                }
            }
            lines.push(entry.to_string());
            parent_uuid = Some(my_uuid.clone());
            last_uuid = Some(my_uuid);
            continue;
        }

        let tool_calls = turn.tool_calls.clone().unwrap_or_default();
        let tool_use_ids: Vec<String> = tool_calls.iter().map(|_| short_id("toolu_")).collect();
        let tool_use_blocks: Vec<Value> = tool_calls
            .iter()
            .zip(tool_use_ids.iter())
            .map(|(tc, id)| json!({ "type": "tool_use", "id": id, "name": tc.name, "input": to_tool_input_object(&tc.input) }))
            .collect();
        // 'image'/'document' blocks are only permitted in user turns --
        // confirmed for real: a resumed session with an attachment block on
        // an assistant message fails with "API Error: 400 ... 'image'
        // blocks are not permitted within assistant turns" the moment the
        // conversation is continued. A tool_result's content, unlike an
        // assistant message's own content, *is* allowed to carry
        // image/document blocks (this is exactly how a real
        // screenshot/file-reading tool returns one) -- so an assistant-side
        // attachment is carried as the result of a synthetic tool_use
        // instead, the same pattern already used for Grok's identical "no
        // assistant-message-level attachment slot" constraint.
        let attachment_tool_use_id = if !attachment_blocks.is_empty() {
            Some(short_id("toolu_"))
        } else {
            None
        };
        let mut all_tool_use_blocks = tool_use_blocks.clone();
        if let Some(id) = &attachment_tool_use_id {
            all_tool_use_blocks.push(json!({ "type": "tool_use", "id": id, "name": "imported_attachment", "input": {} }));
        }

        let mut content_arr: Vec<Value> = Vec::new();
        if !combined_text.is_empty() {
            content_arr.push(json!({ "type": "text", "text": combined_text }));
        }
        content_arr.extend(all_tool_use_blocks.clone());

        let entry = json!({
            "parentUuid": parent_uuid,
            "isSidechain": false,
            "message": {
                "model": "claude-sonnet-5",
                "id": short_id("msg_"),
                "type": "message",
                "role": "assistant",
                "content": content_arr,
                "stop_reason": if all_tool_use_blocks.is_empty() { "end_turn" } else { "tool_use" },
                "stop_sequence": Value::Null,
            },
            "type": "assistant",
            "uuid": my_uuid,
            "timestamp": ts,
            "userType": "external",
            "entrypoint": "cli",
            "cwd": real_cwd_str,
            "sessionId": new_id,
            "version": cli_version,
        });
        lines.push(entry.to_string());
        parent_uuid = Some(my_uuid.clone());
        last_uuid = Some(my_uuid);

        // Real tool calls (and a synthetic attachment tool_use, if any)
        // need a matching tool_result reply (as its own "user" role
        // message, per Claude's own API convention -- see read() above)
        // immediately after, or the transcript is structurally incomplete.
        if !all_tool_use_blocks.is_empty() {
            let result_uuid = Uuid::new_v4().to_string();
            let mut result_content: Vec<Value> = tool_calls
                .iter()
                .zip(tool_use_ids.iter())
                .map(|(tc, id)| json!({ "type": "tool_result", "tool_use_id": id, "content": tc.output.clone().unwrap_or_default() }))
                .collect();
            if let Some(id) = &attachment_tool_use_id {
                result_content.push(json!({ "type": "tool_result", "tool_use_id": id, "content": attachment_blocks }));
            }
            let result_entry = json!({
                "parentUuid": parent_uuid,
                "isSidechain": false,
                "promptId": Uuid::new_v4().to_string(),
                "type": "user",
                "message": { "role": "user", "content": result_content },
                "uuid": result_uuid,
                "timestamp": crate::util::iso_string_now(),
                "userType": "external",
                "entrypoint": "cli",
                "cwd": real_cwd_str,
                "sessionId": new_id,
                "version": cli_version,
            });
            lines.push(result_entry.to_string());
            parent_uuid = Some(result_uuid.clone());
            last_uuid = Some(result_uuid);
        }
    }

    lines.insert(0, json!({ "type": "last-prompt", "leafUuid": last_uuid, "sessionId": new_id }).to_string());
    std::fs::write(&out_path, lines.join("\n") + "\n")?;
    Ok(new_id)
}

fn resume_cmd_impl(session_id: &str, _project_path: &str) -> Vec<String> {
    vec!["claude".to_string(), "--resume".to_string(), session_id.to_string()]
}

pub struct ClaudeAdapter;

impl Adapter for ClaudeAdapter {
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

/// Fast path for hop lookups: Claude Code's own directory layout already
/// encodes the project path into the directory name (see `write_impl`'s
/// use of `encode_dir`), so which session is the right one is answered by
/// `std::fs::read_dir` and file mtimes alone -- no need to open or parse a
/// single file's contents, let alone every session on disk the way
/// `list_sessions()` does for the search UI. `read_impl` only needs
/// `raw.file` from the `SessionRef`, so the rest of the fields here are
/// harmless placeholders.
fn find_latest_for_path_impl(project_path: &str) -> Option<SessionRef> {
    let real_cwd_str = real_cwd(project_path).ok()?;
    let dir = projects_dir().join(encode_dir(&real_cwd_str));
    let files = find_files(&dir, |p| p.extension().map(|e| e == "jsonl").unwrap_or(false));
    let latest = files.into_iter().max_by_key(|f| std::fs::metadata(f).and_then(|m| m.modified()).ok())?;
    let session_id = latest.file_stem()?.to_string_lossy().to_string();
    Some(SessionRef {
        tool: ToolName::Claude,
        session_id,
        project_path: real_cwd_str,
        title: String::new(),
        snippet: String::new(),
        body: None,
        updated_at: 0,
        raw: Some(json!({ "file": latest.to_string_lossy() })),
        match_snippet: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_dir_strips_windows_verbatim_prefix() {
        let from_verbatim = encode_dir(r"\\?\C:\Users\test\src");
        let from_plain = encode_dir(r"C:\Users\test\src");
        assert_eq!(from_verbatim, from_plain);
        assert_eq!(from_plain, "C--Users-test-src");
        assert!(!from_verbatim.contains('?'), "verbatim prefix leaked: {from_verbatim}");
    }
}
