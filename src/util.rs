//! Port of src/util.ts -- shared helpers used by every adapter.
//!
//! Faithful, literal port. Comments explaining *why* something is done a
//! particular way are carried over (adapted to Rust) from the TypeScript
//! source, since they document non-obvious behavior that was hit and fixed
//! against real generated sessions, not theorized.

use crate::adapters::{Turn, ToolCallRecord};
use serde_json::Value;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// Parses an entire JSONL file into memory. Returns an empty vec on any
/// read failure; malformed individual lines are skipped rather than
/// aborting the whole read.
pub fn read_jsonl_lines(path: &Path) -> Vec<Value> {
    let raw = match std::fs::read_to_string(path) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for line in raw.split('\n') {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str(trimmed) {
            out.push(v);
        }
    }
    out
}

/// True streaming line reader -- yields one parsed line at a time without
/// ever loading the whole file into memory. Matters because a single
/// session file can be hundreds of MB (long-running agentic sessions
/// accumulate a lot of tool output); reading the whole file up front would
/// pay that full I/O and decode cost even when a caller only needs the
/// first few KB before breaking early. `BufReader::lines()` reads
/// incrementally under the hood, so iterating (and breaking early) here
/// genuinely avoids reading the rest of the file, unlike the TS version's
/// async generator this doesn't need to be async in Rust -- a synchronous
/// iterator gives the same short-circuiting behavior.
pub fn read_jsonl_lines_lazy(path: &Path) -> Box<dyn Iterator<Item = Value>> {
    match std::fs::File::open(path) {
        Ok(f) => Box::new(BufReader::new(f).lines().filter_map(|line| {
            let line = line.ok()?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                return None;
            }
            serde_json::from_str(trimmed).ok()
        })),
        Err(_) => Box::new(std::iter::empty()),
    }
}

const TAIL_MAX_BYTES: u64 = 768 * 1024;

/// Tail-reads the last `TAIL_MAX_BYTES` of a (possibly huge) file instead of
/// the whole thing -- used to sample recent content (e.g. for the body
/// sampler's tail) without paying the cost of reading a multi-hundred-MB
/// session file end to end.
pub fn read_jsonl_tail_lines(path: &Path) -> Vec<Value> {
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };
    let size = match file.metadata() {
        Ok(m) => m.len(),
        Err(_) => return Vec::new(),
    };
    let start = size.saturating_sub(TAIL_MAX_BYTES);
    let length = size - start;
    if file.seek(SeekFrom::Start(start)).is_err() {
        return Vec::new();
    }
    let mut buf = vec![0u8; length as usize];
    if file.read_exact(&mut buf).is_err() {
        return Vec::new();
    }
    let mut raw = String::from_utf8_lossy(&buf).to_string();
    if start > 0 {
        match raw.find('\n') {
            Some(idx) => raw = raw[idx + 1..].to_string(),
            None => raw = String::new(),
        }
    }
    let mut out = Vec::new();
    for line in raw.split('\n') {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str(trimmed) {
            out.push(v);
        }
    }
    out
}

const FIND_FILES_MAX_DEPTH: usize = 8;

/// Recursively find files matching a predicate, without pulling in a glob
/// dependency.
pub fn find_files(root: &Path, matches: impl Fn(&Path) -> bool) -> Vec<PathBuf> {
    let mut out = Vec::new();
    walk(root, 0, &matches, &mut out);
    out
}

fn walk(dir: &Path, depth: usize, matches: &impl Fn(&Path) -> bool, out: &mut Vec<PathBuf>) {
    if depth > FIND_FILES_MAX_DEPTH {
        return;
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let full = entry.path();
        let meta = match std::fs::metadata(&full) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if meta.is_dir() {
            walk(&full, depth + 1, matches, out);
        } else if matches(&full) {
            out.push(full);
        }
    }
}

pub fn mtime_ms(path: &Path) -> i64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// A message under this length is almost always a greeting ("hi", "hey
/// claude") rather than something that actually describes the session --
/// bad material for a title. Used to skip past filler when picking a title
/// candidate; the raw first message is still kept as a fallback.
pub const MIN_TITLE_CHARS: usize = 15;

fn take_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

fn last_chars(s: &str, n: usize) -> String {
    let total = s.chars().count();
    if total <= n {
        return s.to_string();
    }
    s.chars().skip(total - n).collect()
}

pub struct BodySampler {
    first: String,
    head: String,
    tail: String,
    total: usize,
    sampled: bool,
    max_chars: usize,
    head_chars: usize,
    tail_chars: usize,
}

impl BodySampler {
    pub fn new(max_chars: usize) -> Self {
        Self::with_params(max_chars, 20_000, 20_000)
    }

    pub fn with_params(max_chars: usize, head_chars: usize, tail_chars: usize) -> Self {
        Self {
            first: String::new(),
            head: String::new(),
            tail: String::new(),
            total: 0,
            sampled: false,
            max_chars,
            head_chars,
            tail_chars,
        }
    }

    pub fn append(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let segment = format!("{text} ");
        self.total += segment.chars().count();
        if self.first.chars().count() < self.max_chars {
            let remaining = self.max_chars - self.first.chars().count();
            self.first.push_str(&take_chars(&segment, remaining));
        }
        if self.head.chars().count() < self.head_chars {
            let remaining = self.head_chars - self.head.chars().count();
            self.head.push_str(&take_chars(&segment, remaining));
        }
        let combined = format!("{}{}", self.tail, segment);
        self.tail = last_chars(&combined, self.tail_chars);
    }

    pub fn has_head(&self) -> bool {
        self.head.chars().count() >= self.head_chars
    }

    pub fn mark_sampled(&mut self) {
        self.sampled = true;
    }

    pub fn value(&self) -> String {
        if !self.sampled && self.total <= self.max_chars {
            self.first.clone()
        } else {
            format!("{} \u{2026} {}", self.head, self.tail)
        }
    }
}

const MAX_TITLE_CHARS: usize = 80;

fn collapse_whitespace(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Matches a leading `^https?://\S+\s*` and returns (full_match_incl_trailing_ws, rest).
fn strip_leading_url(text: &str) -> Option<(String, String)> {
    if !(text.starts_with("http://") || text.starts_with("https://")) {
        return None;
    }
    let mut end = text.len();
    for (i, c) in text.char_indices() {
        if c.is_whitespace() {
            end = i;
            break;
        }
    }
    let after = &text[end..];
    let ws_len: usize = after
        .chars()
        .take_while(|c| c.is_whitespace())
        .map(|c| c.len_utf8())
        .sum();
    let rest_start = end + ws_len;
    Some((text[..rest_start].to_string(), text[rest_start..].to_string()))
}

/// Turns a raw first-message string into a display title: drops a leading
/// bare URL (common when a session opens with a pasted link -- the URL
/// alone is a useless title, the sentence after it is what the session is
/// about), collapses whitespace, and truncates at a word boundary instead
/// of mid-word so titles don't end like "...can you use the adobe p".
pub fn clean_title(raw: &str) -> String {
    let mut text = collapse_whitespace(raw.trim());
    if let Some((_full, rest)) = strip_leading_url(&text) {
        let rest_trimmed = rest.trim().to_string();
        // only drop the URL if something substantive follows it -- a
        // URL-only message still needs *a* title, so keep the URL as a
        // last resort rather than producing an empty string.
        if rest_trimmed.chars().count() >= MIN_TITLE_CHARS {
            text = rest_trimmed;
        }
    }
    if text.chars().count() <= MAX_TITLE_CHARS {
        return text;
    }
    let cut_chars: Vec<char> = text.chars().take(MAX_TITLE_CHARS).collect();
    let last_space = cut_chars.iter().rposition(|&c| c == ' ');
    let trimmed: String = match last_space {
        Some(idx) if (idx as f64) > (MAX_TITLE_CHARS as f64) * 0.6 => {
            cut_chars[..idx].iter().collect()
        }
        _ => cut_chars.iter().collect(),
    };
    format!("{}\u{2026}", trimmed.trim_end())
}

// A tool call's output can legitimately be a whole file dump or command log
// -- capped per-call so one giant `cat` doesn't blow the entire turn
// budget, while still keeping enough to be useful (unlike dropping tool
// calls entirely, which was the previous behavior).
pub const MAX_TOOL_OUTPUT_CHARS: usize = 3000;

pub fn truncate(s: &str, max: usize) -> String {
    let len = s.chars().count();
    if len > max {
        let head: String = s.chars().take(max).collect();
        format!("{}\n\u{2026}(truncated, {} more chars)", head, len - max)
    } else {
        s.to_string()
    }
}

/// OpenAI-style function-calling backends (Codex's API, and any
/// OpenAI/Azure-backed model Pi can route to via `--model auto`) validate a
/// function name against /^[a-zA-Z0-9_-]+$/ when a conversation is
/// continued -- confirmed for real on both: a cross-agent tool label with
/// spaces/punctuation (e.g. a display name like "Web search:") loads and
/// resumes fine, then fails with a 400 the moment the conversation actually
/// continues. Anthropic's API does not enforce this on historical tool_use
/// names (also confirmed live), but sanitizing unconditionally is harmless
/// there and safer than assuming which backend a target will use.
pub fn sanitize_tool_name(name: &str) -> String {
    fn is_ok(c: char) -> bool {
        c.is_ascii_alphanumeric() || c == '_' || c == '-'
    }
    let chars: Vec<char> = name.chars().collect();
    let mut start = 0;
    while start < chars.len() && !is_ok(chars[start]) {
        start += 1;
    }
    let mut end = chars.len();
    while end > start && !is_ok(chars[end - 1]) {
        end -= 1;
    }
    let middle = &chars[start..end];
    let mut result = String::new();
    let mut in_run = false;
    for &c in middle {
        if is_ok(c) {
            result.push(c);
            in_run = false;
        } else if !in_run {
            result.push('_');
            in_run = true;
        }
    }
    if result.is_empty() {
        "unknown_tool".to_string()
    } else {
        result
    }
}

/// Claude, OpenCode, and Pi all require a tool call's structured input to be
/// a JSON *object*, not just valid JSON -- confirmed for real: a resumed
/// session failed with "400 ... tool_use.input: Input should be an object"
/// because a source tool call's raw input string parsed to (or, on parse
/// failure, was kept as) a non-object value -- a bare string, array,
/// number, or the raw unparsed text itself. Wraps any non-object result so
/// the output is always a legal object, without losing the original data.
pub fn to_tool_input_object(input: &str) -> Value {
    match serde_json::from_str::<Value>(input) {
        Ok(Value::Object(map)) => Value::Object(map),
        Ok(other) => {
            let mut m = serde_json::Map::new();
            m.insert("value".to_string(), other);
            Value::Object(m)
        }
        Err(_) => {
            let mut m = serde_json::Map::new();
            m.insert("value".to_string(), Value::String(input.to_string()));
            Value::Object(m)
        }
    }
}

/// Every real coding-agent tool call whose input is a shell/exec-style
/// invocation is keyed the same handful of ways across agents (Codex's
/// exec_command uses "cmd", Claude's Bash tool uses "command", etc.) --
/// pulling the actual command out and rendering it as a real shell block is
/// what makes it read like a native tool call instead of a JSON envelope
/// dump. `description` is common alongside it (a human-readable one-liner
/// of intent) and reads naturally as a comment above the command, matching
/// how agents already narrate "why" before "what".
fn extract_shell_command(parsed: &Value) -> Option<(String, Option<String>)> {
    let obj = parsed.as_object()?;
    for key in ["command", "cmd", "script"] {
        if let Some(Value::String(cmd)) = obj.get(key) {
            let description = obj
                .get("description")
                .and_then(|d| d.as_str())
                .map(|s| s.to_string());
            return Some((cmd.clone(), description));
        }
    }
    None
}

/// Renders tool calls as a plain-text block -- the shared fallback shape
/// for cross-agent conversion (native write() paths) and for --print's
/// output, since no structured tool_use/tool_result schema is portable
/// across all five agents' formats. Deliberately terse and consistent
/// regardless of which agent originally made the call.
///
/// Every target TUI renders markdown in assistant messages (that's how
/// they render their own tool output), so real code fences read as a
/// proper formatted block instead of a raw inline JSON dump -- which is
/// what the plain "[tool call: x]\ninput: {...}" version produced, and
/// looked like an unstyled wall of text next to everything else the TUI
/// renders normally. The JSON itself also needs pretty-printing: a raw
/// single-line stringified arguments blob (escaped quotes, embedded
/// newlines as literal \n) reads as an unreadable wall of text even inside
/// a fence -- confirmed genuinely bad by looking at a real screenshot of a
/// resumed tool call, not just theorized.
pub fn render_tool_calls(tool_calls: Option<&[ToolCallRecord]>) -> String {
    let Some(tcs) = tool_calls else {
        return String::new();
    };
    if tcs.is_empty() {
        return String::new();
    }
    tcs.iter()
        .map(|tc| {
            let parsed: Option<Value> = serde_json::from_str(&tc.input).ok();
            let input = if let Some(p) = &parsed {
                if let Some((cmd, desc)) = extract_shell_command(p) {
                    let comment = desc.map(|d| format!("# {d}\n")).unwrap_or_default();
                    format!("```bash\n{comment}{cmd}\n```")
                } else {
                    format!(
                        "```json\n{}\n```",
                        serde_json::to_string_pretty(p).unwrap_or_default()
                    )
                }
            } else {
                format!("```\n{}\n```", tc.input)
            };
            let output = tc
                .output
                .as_ref()
                .map(|o| format!("\nOutput:\n```\n{o}\n```"))
                .unwrap_or_default();
            format!("**Tool call: `{}`**\n{input}{output}", tc.name)
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

// Full tool-call I/O (now preserved in full, not just narration text) can
// make a real long-running session's converted size enormous -- a real
// 547-turn session measured at ~3.3M characters (~820k estimated tokens),
// which silently produced a converted session no target agent's context
// window could actually load (confirmed for real: Codex errored with "ran
// out of room in the model's context window" on resume). There was never
// any size cap on full-session conversion, even before tool-call fidelity
// existed -- it just wasn't survivable to notice until output got this
// much bigger. 200k chars (~50k tokens) leaves real headroom for a target
// agent's own system prompt/skills/tools (observed as large as ~40-50k
// chars on their own in a real session) while still keeping a
// substantial, useful slice of recent conversation.
pub const CONVERSION_CHAR_BUDGET: usize = 200_000;

fn turn_char_count(t: &Turn) -> usize {
    let mut n = t.text.chars().count();
    if let Some(tcs) = &t.tool_calls {
        for tc in tcs {
            n += tc.input.chars().count() + tc.output.as_ref().map(|o| o.chars().count()).unwrap_or(0);
        }
    }
    n
}

/// Keeps the most recent turns that fit under a total character budget --
/// trimming from the oldest end, since "resume" almost always means
/// "continue from where things left off," not "replay the entire history
/// from months ago." Attachment bytes (images/PDFs) aren't counted toward
/// the budget -- they're usually tokenized far more efficiently than raw
/// text per byte, and excluding them keeps this from over-trimming a
/// conversation just because it happened to have a couple of screenshots.
pub fn trim_turns_to_budget(turns: Vec<Turn>, budget: usize) -> (Vec<Turn>, usize) {
    let mut total = 0usize;
    let mut cut_index = turns.len();
    for i in (0..turns.len()).rev() {
        total += turn_char_count(&turns[i]);
        if total > budget {
            cut_index = i + 1;
            break;
        }
        cut_index = i;
    }
    let dropped = cut_index;
    let kept: Vec<Turn> = turns.into_iter().skip(cut_index).collect();
    (kept, dropped)
}

/// Matches JS `new Date().toISOString()`: always millisecond precision,
/// always a "Z" (UTC) suffix.
pub fn iso_string_now() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

/// Port of the TS `isoNow()` -- note this reproduces its exact (slightly
/// unusual) truncation: `toISOString()` then drop the trailing "Z", drop
/// the last 3 characters (the milliseconds digits), then re-append "Z",
/// leaving a trailing ".Z" with no milliseconds. Ported literally rather
/// than "fixed" since a target agent's on-disk format may depend on the
/// exact string shape this has always produced.
pub fn iso_now() -> String {
    let s = iso_string_now();
    let without_z = s.trim_end_matches('Z');
    let truncated = &without_z[..without_z.len().saturating_sub(3)];
    format!("{truncated}Z")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::Role;

    #[test]
    fn sanitize_tool_name_strips_leading_trailing_punctuation() {
        assert_eq!(sanitize_tool_name("Web search:"), "Web_search");
        assert_eq!(sanitize_tool_name("...tool!!!"), "tool");
    }

    #[test]
    fn sanitize_tool_name_replaces_runs_of_special_chars_with_underscore() {
        assert_eq!(sanitize_tool_name("foo   bar"), "foo_bar");
        assert_eq!(sanitize_tool_name("a/b\\c:d"), "a_b_c_d");
    }

    #[test]
    fn sanitize_tool_name_falls_back_for_all_punctuation() {
        assert_eq!(sanitize_tool_name("!!!"), "unknown_tool");
        assert_eq!(sanitize_tool_name(""), "unknown_tool");
    }

    #[test]
    fn sanitize_tool_name_leaves_valid_names_alone() {
        assert_eq!(sanitize_tool_name("Bash"), "Bash");
        assert_eq!(sanitize_tool_name("exec_command-1"), "exec_command-1");
    }

    #[test]
    fn to_tool_input_object_wraps_non_object_json() {
        assert_eq!(to_tool_input_object("\"hello\""), serde_json::json!({"value": "hello"}));
        assert_eq!(to_tool_input_object("42"), serde_json::json!({"value": 42}));
        assert_eq!(to_tool_input_object("[1,2,3]"), serde_json::json!({"value": [1,2,3]}));
    }

    #[test]
    fn to_tool_input_object_passes_through_objects() {
        assert_eq!(
            to_tool_input_object(r#"{"a":1,"b":"c"}"#),
            serde_json::json!({"a": 1, "b": "c"})
        );
    }

    #[test]
    fn to_tool_input_object_wraps_unparsable_input() {
        assert_eq!(to_tool_input_object("not json{"), serde_json::json!({"value": "not json{"}));
    }

    #[test]
    fn clean_title_strips_leading_url_when_substantive_text_follows() {
        let title = clean_title("https://example.com/some/path this is a real substantive question about something");
        assert_eq!(title, "this is a real substantive question about something");
    }

    #[test]
    fn clean_title_keeps_url_only_message_as_last_resort() {
        let title = clean_title("https://example.com/some/path short");
        assert!(title.starts_with("https://"));
    }

    #[test]
    fn clean_title_truncates_at_word_boundary() {
        let long = "can you please go ahead and use the adobe premier pro application to edit this video file for me";
        let title = clean_title(long);
        assert!(title.ends_with('\u{2026}'));
        assert!(!title.contains("premie\u{2026}"), "should not cut mid-word");
    }

    #[test]
    fn clean_title_collapses_whitespace() {
        assert_eq!(clean_title("hello   \n\n  world"), "hello world");
    }

    #[test]
    fn trim_turns_to_budget_keeps_most_recent_turns() {
        let turns = vec![
            Turn { role: Role::User, text: "a".repeat(100), tool_calls: None, attachments: None },
            Turn { role: Role::Assistant, text: "b".repeat(100), tool_calls: None, attachments: None },
            Turn { role: Role::User, text: "c".repeat(100), tool_calls: None, attachments: None },
        ];
        let (kept, dropped) = trim_turns_to_budget(turns, 150);
        assert_eq!(dropped, 2);
        assert_eq!(kept.len(), 1);
        assert!(kept[0].text.starts_with('c'));
    }

    #[test]
    fn trim_turns_to_budget_keeps_everything_under_budget() {
        let turns = vec![
            Turn { role: Role::User, text: "a".repeat(10), tool_calls: None, attachments: None },
            Turn { role: Role::Assistant, text: "b".repeat(10), tool_calls: None, attachments: None },
        ];
        let (kept, dropped) = trim_turns_to_budget(turns, 1000);
        assert_eq!(dropped, 0);
        assert_eq!(kept.len(), 2);
    }

    #[test]
    fn body_sampler_stays_first_when_under_budget() {
        let mut s = BodySampler::new(1000);
        s.append("hello");
        s.append("world");
        assert_eq!(s.value(), "hello world ");
        assert!(!s.has_head());
    }
}
