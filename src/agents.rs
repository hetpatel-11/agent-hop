use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ToolName {
    Claude,
    Codex,
    OpenCode,
    Pi,
    Grok,
}

impl ToolName {
    pub const ALL: [ToolName; 5] = [
        ToolName::Claude,
        ToolName::Codex,
        ToolName::OpenCode,
        ToolName::Pi,
        ToolName::Grok,
    ];

    pub fn slug(&self) -> &'static str {
        match self {
            ToolName::Claude => "claude",
            ToolName::Codex => "codex",
            ToolName::OpenCode => "opencode",
            ToolName::Pi => "pi",
            ToolName::Grok => "grok",
        }
    }

    /// Full product name, for anywhere reading as prose matters more than
    /// a compact tag (e.g. the transition splash's "Switching to Claude
    /// Code..." -- `slug()` alone reads flat/technical there).
    pub fn display_name(&self) -> &'static str {
        match self {
            ToolName::Claude => "Claude Code",
            ToolName::Codex => "Codex",
            ToolName::OpenCode => "OpenCode",
            ToolName::Pi => "Pi",
            ToolName::Grok => "Grok",
        }
    }

    pub fn binary(&self) -> &'static str {
        match self {
            ToolName::Claude => "claude",
            ToolName::Codex => "codex",
            ToolName::OpenCode => "opencode",
            ToolName::Pi => "pi",
            ToolName::Grok => "grok",
        }
    }

    pub fn install_command(&self) -> (&'static str, &'static [&'static str]) {
        match self {
            ToolName::Claude => ("npm", &["install", "-g", "@anthropic-ai/claude-code"]),
            ToolName::Codex => ("npm", &["install", "-g", "@openai/codex"]),
            ToolName::OpenCode => ("npm", &["install", "-g", "opencode-ai"]),
            ToolName::Pi => ("npm", &["install", "-g", "@heypi/cli"]),
            ToolName::Grok => ("npm", &["install", "-g", "@vibe-kit/grok-cli"]),
        }
    }

    pub fn from_slug(s: &str) -> Option<ToolName> {
        ToolName::ALL.into_iter().find(|t| t.slug() == s)
    }

    pub fn is_installed(&self) -> bool {
        which(self.binary()).is_some()
    }
}

impl fmt::Display for ToolName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.slug())
    }
}

/// Cross-platform "is this on PATH" check without shelling out to `which`
/// (which doesn't exist on Windows) -- searches PATH directly, trying
/// `.exe`/`.cmd` suffixes on Windows.
pub fn which(bin: &str) -> Option<std::path::PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    let exts: Vec<String> = if cfg!(windows) {
        vec![".exe".into(), ".cmd".into(), ".bat".into(), "".into()]
    } else {
        vec!["".into()]
    };
    for dir in std::env::split_paths(&path_var) {
        for ext in &exts {
            let candidate = dir.join(format!("{bin}{ext}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn which_finds_installed_agents() {
        for tool in ToolName::ALL {
            let found = which(tool.binary());
            assert!(found.is_some(), "expected {} on PATH for this smoke test", tool.slug());
        }
    }
}
