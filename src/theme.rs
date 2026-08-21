//! Port of src/theme.ts -- tiny hand-rolled ANSI helpers, deliberately not
//! a dependency (matches the TS rationale: pulling in chalk/picocolors for
//! a handful of escape codes is an unnecessary supply-chain surface).

use crate::agents::ToolName;

fn wrap(open: &str, close: &str, s: &str) -> String {
    format!("\x1b[{open}m{s}\x1b[{close}m")
}

pub fn bold(s: &str) -> String {
    wrap("1", "22", s)
}
pub fn yellow(s: &str) -> String {
    wrap("33", "39", s)
}
pub fn cyan(s: &str) -> String {
    wrap("36", "39", s)
}
pub fn orange(s: &str) -> String {
    wrap("38;5;208", "39", s)
}
pub fn dark_blue(s: &str) -> String {
    wrap("38;5;25", "39", s)
}
pub fn grey(s: &str) -> String {
    wrap("38;5;244", "39", s)
}
pub fn white(s: &str) -> String {
    wrap("97", "39", s)
}

/// One distinct color per agent so a listing reads at a glance.
pub fn tool_tag(tool: ToolName) -> String {
    let tag = format!("[{}]", tool.slug());
    let colored = match tool {
        ToolName::Claude => orange(&tag),
        ToolName::Codex => dark_blue(&tag),
        ToolName::Pi => yellow(&tag),
        ToolName::OpenCode => grey(&tag),
        ToolName::Grok => white(&tag),
    };
    bold(&colored)
}

pub fn highlight_date(date_str: &str) -> String {
    bold(&cyan(date_str))
}
