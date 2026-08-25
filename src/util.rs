//! Port of src/util.ts -- shared helpers used by every adapter.
//!
//! Faithful, literal port. Comments explaining *why* something is done a
//! particular way are carried over (adapted to Rust) from the TypeScript
//! source, since they document non-obvious behavior that was hit and fixed
//! against real generated sessions, not theorized.

use crate::adapters::{Role, Turn};
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
/// Live hops use `trim_turns_with_summary` (same cut, plus a native compact
/// when the source harness stored one, else a digest of what was dropped).
/// This is the cut-only helper, compiled in tests so the budget math can
/// be asserted without going through the summary path.
#[cfg(test)]
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

/// Extracts path-looking values from a tool call's raw JSON input string --
/// deliberately just a regex over common argument keys rather than a full
/// JSON parse, since the exact schema varies per tool and this only needs
/// to be good enough for a human-readable digest, not a faithful replay.
fn extract_touched_paths(tool_input: &str, into: &mut Vec<String>) {
    static PATH_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = PATH_RE.get_or_init(|| {
        regex::Regex::new(r#"(?:file_path|notebook_path|path|filename)"\s*:\s*"([^"]+)"#).unwrap()
    });
    for cap in re.captures_iter(tool_input) {
        let path = cap[1].to_string();
        if !into.contains(&path) {
            into.push(path);
        }
    }
}

fn truncate_chars(s: &str, max: usize) -> String {
    let mut out: String = s.chars().take(max).collect();
    if s.chars().count() > max {
        out.push('\u{2026}');
    }
    out
}

/// Builds a short, fast, purely-local digest of the turns `trim_turns_to_budget`
/// is about to drop -- no model call involved (a hop needs to stay quick), just
/// heuristics over what's already in memory. Returns `None` if nothing was
/// dropped. Used so a hop into a different agent reads as "keep going from
/// here, here's roughly what came before," not a cold, silent truncation --
/// several of the agents this crate targets do something like this
/// themselves internally when their own context fills up (an
/// auto-compact-style summary turn), so a receiving agent seeing one in its
/// history is a familiar shape, not something unusual.
pub fn summarize_dropped_turns(dropped: &[Turn]) -> Option<String> {
    if dropped.is_empty() {
        return None;
    }
    let first_user_text = dropped.iter().find(|t| t.role == Role::User).map(|t| truncate_chars(&t.text, 240));

    let mut tool_names: Vec<String> = Vec::new();
    let mut files: Vec<String> = Vec::new();
    for t in dropped {
        if let Some(tcs) = &t.tool_calls {
            for tc in tcs {
                if !tool_names.contains(&tc.name) {
                    tool_names.push(tc.name.clone());
                }
                extract_touched_paths(&tc.input, &mut files);
            }
        }
    }

    let mut summary = format!(
        "[agent-hop summary of earlier context]\nThis conversation continued from an earlier session; {} earlier turn(s) were trimmed to stay within the context budget.",
        dropped.len()
    );
    if let Some(task) = first_user_text {
        if !task.is_empty() {
            summary.push_str(&format!("\nOriginal task: \"{task}\""));
        }
    }
    if !tool_names.is_empty() {
        summary.push_str(&format!("\nTools used earlier: {}", tool_names.join(", ")));
    }
    if !files.is_empty() {
        let shown: Vec<&String> = files.iter().take(12).collect();
        let shown_str = shown.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ");
        let more = files.len().saturating_sub(shown.len());
        if more > 0 {
            summary.push_str(&format!("\nFiles touched earlier: {shown_str} (+{more} more)"));
        } else {
            summary.push_str(&format!("\nFiles touched earlier: {shown_str}"));
        }
    }
    Some(summary)
}

/// Prefix Grok (and any other sidecar recap) uses when we inject the
/// harness's own compact into the hop stream. Claude's compact is already
/// a user turn starting with `CLAUDE_COMPACT_PREFIX`.
pub const NATIVE_COMPACT_MARKER: &str = "[agent-hop native compact]";
const CLAUDE_COMPACT_PREFIX: &str =
    "This session is being continued from a previous conversation that ran out of context";

pub fn is_native_compact_turn(t: &Turn) -> bool {
    t.text.starts_with(NATIVE_COMPACT_MARKER) || t.text.contains(CLAUDE_COMPACT_PREFIX)
}

/// Compact/recap (and the local digest that stands in when none exists)
/// belong in the model context of a hop, not as a transcript bubble.
/// Writers should keep the text in the API log and omit the TUI replay
/// event (Codex `event_msg`, Grok `user_message_chunk`) or mark it the
/// way Claude marks its own compact (`isCompactSummary`).
pub fn is_hop_context_only(t: &Turn) -> bool {
    is_native_compact_turn(t) || t.text.starts_with("[agent-hop summary of earlier context]")
}

/// Trims to the character budget (see `trim_turns_to_budget`). If the
/// source harness stored a model compact/recap, that text is reserved
/// first (and turns already summarized by it are dropped). Anything else
/// that still doesn't fit gets the local heuristic digest.
pub fn trim_turns_with_summary(turns: Vec<Turn>, budget: usize) -> Vec<Turn> {
    let last_compact = turns.iter().rposition(is_native_compact_turn);
    let (native, rest): (Option<Turn>, Vec<Turn>) = if let Some(idx) = last_compact {
        let native = turns[idx].clone();
        // Claude-style: compact sits mid-log and replaces everything
        // before it. Grok-style: we inject the recap at index 0, so
        // "after compact" is the full remaining conversation.
        let rest = turns.into_iter().skip(idx + 1).collect();
        (Some(native), rest)
    } else {
        (None, turns)
    };

    let last_needed = rest.last().map(turn_char_count).unwrap_or(0).min(budget);
    let mut native = native.map(|mut t| {
        let cap = budget.saturating_sub(last_needed);
        if turn_char_count(&t) > cap {
            t.text = truncate_chars(&t.text, cap);
        }
        t
    });
    let reserved = native.as_ref().map(turn_char_count).unwrap_or(0);
    let rest_budget = budget.saturating_sub(reserved);

    let cut_index = {
        let mut total = 0usize;
        let mut idx = rest.len();
        for i in (0..rest.len()).rev() {
            total += turn_char_count(&rest[i]);
            if total > rest_budget {
                idx = i + 1;
                break;
            }
            idx = i;
        }
        idx
    };
    if cut_index == 0 && native.is_none() {
        return rest;
    }
    let (dropped, kept) = rest.split_at(cut_index);
    let heuristic = if native.is_none() {
        summarize_dropped_turns(dropped)
    } else if !dropped.is_empty() {
        summarize_dropped_turns(dropped)
    } else {
        None
    };
    let mut result = Vec::with_capacity(kept.len() + 2);
    if let Some(t) = native.take() {
        result.push(t);
    }
    if let Some(text) = heuristic {
        result.push(Turn { role: Role::User, text, tool_calls: None, attachments: None });
    }
    result.extend_from_slice(kept);
    result
}

/// Matches JS `new Date().toISOString()`: always millisecond precision,
/// always a "Z" (UTC) suffix.
pub fn iso_string_now() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::{Role, ToolCallRecord};

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
    fn hop_context_only_is_compact_or_digest_not_normal_turns() {
        let digest = Turn {
            role: Role::User,
            text: "[agent-hop summary of earlier context]\ncut 2 turns".into(),
            tool_calls: None,
            attachments: None,
        };
        let grok = Turn {
            role: Role::User,
            text: format!("{NATIVE_COMPACT_MARKER}\nrecap"),
            tool_calls: None,
            attachments: None,
        };
        let claude = Turn {
            role: Role::User,
            text: format!("{CLAUDE_COMPACT_PREFIX}. Summary: done."),
            tool_calls: None,
            attachments: None,
        };
        let normal = Turn {
            role: Role::User,
            text: "please fix the login bug".into(),
            tool_calls: None,
            attachments: None,
        };
        assert!(is_hop_context_only(&digest));
        assert!(is_hop_context_only(&grok));
        assert!(is_hop_context_only(&claude));
        assert!(!is_hop_context_only(&normal));
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
    fn trim_turns_with_summary_prepends_digest_when_something_dropped() {
        let turns = vec![
            Turn {
                role: Role::User,
                text: "please fix the login bug".to_string(),
                tool_calls: Some(vec![ToolCallRecord {
                    name: "Edit".to_string(),
                    input: r#"{"file_path": "src/auth.rs", "old_string": "x", "new_string": "y"}"#.to_string(),
                    output: None,
                }]),
                attachments: None,
            },
            Turn { role: Role::Assistant, text: "a".repeat(100), tool_calls: None, attachments: None },
            Turn { role: Role::User, text: "c".repeat(100), tool_calls: None, attachments: None },
        ];
        let result = trim_turns_with_summary(turns, 150);
        // the last (most recent) turn always survives, plus a synthetic
        // summary turn describing what was cut
        assert_eq!(result.len(), 2);
        assert!(result[0].text.contains("agent-hop summary"));
        assert!(result[0].text.contains("login bug"));
        assert!(result[0].text.contains("Edit"));
        assert!(result[0].text.contains("src/auth.rs"));
        assert!(result[1].text.starts_with('c'));
    }

    #[test]
    fn trim_turns_with_summary_is_a_noop_when_nothing_dropped() {
        let turns = vec![
            Turn { role: Role::User, text: "a".repeat(10), tool_calls: None, attachments: None },
            Turn { role: Role::Assistant, text: "b".repeat(10), tool_calls: None, attachments: None },
        ];
        let result = trim_turns_with_summary(turns, 1000);
        assert_eq!(result.len(), 2);
        assert!(!result[0].text.contains("agent-hop summary"));
    }

    #[test]
    fn trim_prefers_native_compact_and_drops_pre_compact_turns() {
        let compact = format!(
            "{CLAUDE_COMPACT_PREFIX}. The summary below covers the earlier portion of the conversation.\nSummary: built the hop budget."
        );
        let turns = vec![
            Turn { role: Role::User, text: "old task that was compacted".into(), tool_calls: None, attachments: None },
            Turn { role: Role::Assistant, text: "old reply".into(), tool_calls: None, attachments: None },
            Turn { role: Role::User, text: compact.clone(), tool_calls: None, attachments: None },
            Turn { role: Role::User, text: "keep going from here".into(), tool_calls: None, attachments: None },
        ];
        let result = trim_turns_with_summary(turns, 10_000);
        assert_eq!(result.len(), 2);
        assert!(result[0].text.contains(CLAUDE_COMPACT_PREFIX));
        assert_eq!(result[1].text, "keep going from here");
    }

    #[test]
    fn trim_keeps_sidecar_native_compact_without_dropping_later_turns() {
        let recap = format!("{NATIVE_COMPACT_MARKER}\nGrok recap of the session so far.");
        let turns = vec![
            Turn { role: Role::User, text: recap.clone(), tool_calls: None, attachments: None },
            Turn { role: Role::User, text: "first question".into(), tool_calls: None, attachments: None },
            Turn { role: Role::Assistant, text: "an answer".into(), tool_calls: None, attachments: None },
        ];
        let result = trim_turns_with_summary(turns, 10_000);
        assert_eq!(result.len(), 3);
        assert!(result[0].text.starts_with(NATIVE_COMPACT_MARKER));
        assert_eq!(result[1].text, "first question");
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
