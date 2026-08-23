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
pub fn magenta(s: &str) -> String {
    wrap("35", "39", s)
}

/// agent-hop.com's own brand accent (#22d3ee, Tailwind cyan-400) -- the
/// dominant color across the marketing site (20 of its ~26 non-grayscale
/// color declarations), used for its own ASCII wordmark. Matched here via
/// 24-bit truecolor rather than the 16-color `wrap()` palette so it's the
/// exact hex, not an approximation.
pub fn brand_cyan(s: &str) -> String {
    format!("\x1b[38;2;34;211;238m{s}\x1b[39m")
}

/// agent-hop's own brand mark, distinct from any per-tool color (see
/// `tool_tag`) so the toggle bar reads as "agent-hop, currently running
/// claude" rather than just "claude" with some plumbing text after it.
pub fn brand() -> String {
    bold(&brand_cyan("agent-hop"))
}

/// Applies the one distinct color assigned to each agent (see `tool_tag`)
/// to arbitrary text, not just a bracketed `[tag]` -- shared by the
/// toggle bar's tag and the transition splash's "Switching to X..."
/// message, so the same tool always reads in the same color everywhere.
pub fn tool_color(tool: ToolName, s: &str) -> String {
    match tool {
        ToolName::Claude => orange(s),
        ToolName::Codex => dark_blue(s),
        ToolName::Pi => yellow(s),
        ToolName::OpenCode => grey(s),
        ToolName::Grok => white(s),
    }
}

/// One distinct color per agent so a listing reads at a glance.
pub fn tool_tag(tool: ToolName) -> String {
    bold(&tool_color(tool, &format!("[{}]", tool.slug())))
}

/// ratatui equivalent of `tool_color`'s palette, for the toggle bar now
/// that it's composed into a `ratatui::buffer::Buffer` frame instead of
/// written as raw ANSI -- same underlying color codes (indexed 208/25/244
/// match the `38;5;N` sequences `orange`/`dark_blue`/`grey` emit above),
/// kept in one place so the two rendering paths can't drift apart.
pub fn tool_ratatui_color(tool: ToolName) -> ratatui::style::Color {
    use ratatui::style::Color;
    match tool {
        ToolName::Claude => Color::Indexed(208),
        ToolName::Codex => Color::Indexed(25),
        ToolName::Pi => Color::Yellow,
        ToolName::OpenCode => Color::Indexed(244),
        ToolName::Grok => Color::White,
    }
}

/// ratatui equivalent of `grey()`, for the same reason as `tool_ratatui_color`.
pub const GREY_RATATUI: ratatui::style::Color = ratatui::style::Color::Indexed(244);

/// agent-hop.com's brand cyan (#22d3ee) as RGB, for `ratatui::style::Color::Rgb`.
pub const BRAND_RGB: (u8, u8, u8) = (34, 211, 238);

/// "agent hop" in the same block-letter style (ANSI Shadow) the original
/// TypeScript version rendered via `figlet`, generated once with the real
/// font and embedded as a plain string rather than adding a figlet
/// dependency at runtime -- the wordmark text itself never changes, so
/// there's nothing to render dynamically. 74 columns wide; callers should
/// fall back to `BRAND_WORDMARK_COMPACT` below the terminal width.
pub const BRAND_WORDMARK: &str = r"
 █████╗  ██████╗ ███████╗███╗   ██╗████████╗    ██╗  ██╗ ██████╗ ██████╗
██╔══██╗██╔════╝ ██╔════╝████╗  ██║╚══██╔══╝    ██║  ██║██╔═══██╗██╔══██╗
███████║██║  ███╗█████╗  ██╔██╗ ██║   ██║       ███████║██║   ██║██████╔╝
██╔══██║██║   ██║██╔══╝  ██║╚██╗██║   ██║       ██╔══██║██║   ██║██╔═══╝
██║  ██║╚██████╔╝███████╗██║ ╚████║   ██║       ██║  ██║╚██████╔╝██║
╚═╝  ╚═╝ ╚═════╝ ╚══════╝╚═╝  ╚═══╝   ╚═╝       ╚═╝  ╚═╝ ╚═════╝ ╚═╝";

pub const BRAND_WORDMARK_WIDTH: u16 = 74;
