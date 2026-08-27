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

    /// Slugs of harnesses currently on PATH. Aggregate-only — used by
    /// telemetry so we know which tools a given install can even hop to.
    pub fn installed_slugs() -> Vec<&'static str> {
        ToolName::ALL.into_iter().filter(ToolName::is_installed).map(|t| t.slug()).collect()
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

/// Normalize a full agent command (program + trailing args, e.g.
/// `["codex", "resume", "<session-id>"]`) into an argv we can actually
/// spawn on this platform.
///
/// POSIX/Unix: returned unchanged -- the harness binary npm linked onto
/// `PATH` is a real executable.
///
/// Windows: this is where npm's shim layout bites. Each CLI ships three
/// launchers in `AppData\Roaming\npm`: an *extensionless* POSIX shell script
/// (`codex`), a `codex.cmd` batch wrapper, and a `codex.ps1`. `CreateProcessW`
/// -- what `portable_pty` and `std::process` use to start the pty child --
/// can execute none of them directly: the extensionless file isn't a Win32
/// image ("%1 is not a valid Win32 application", os error 193) and a `.cmd`
/// must go through the command interpreter. So on Windows we resolve the real
/// `*.cmd` via [`which`] and route it through `cmd.exe /d /s /c`.
pub fn spawn_argv(command: &[String]) -> Vec<String> {
    let Some((program, args)) = command.split_first() else {
        return command.to_vec();
    };
    if !cfg!(windows) {
        return command.to_vec();
    }
    let target = which(program)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| program.clone());
    // A resolved native binary (a real `.exe` on PATH) can be spawned
    // directly. The npm shim layout, though, is a `.cmd` batch -- and
    // `CreateProcessW` (what `portable_pty` and `std::process` use to start
    // the pty child) cannot execute a batch on its own; it has to go through
    // the command interpreter. Route `.cmd`/`.bat` via `cmd.exe /d /s /c`.
    if target.to_ascii_lowercase().ends_with(".cmd") || target.to_ascii_lowercase().ends_with(".bat") {
        let mut argv: Vec<String> = vec!["cmd.exe".into(), "/d".into(), "/s".into(), "/c".into(), target];
        argv.extend(args.iter().cloned());
        argv
    } else {
        let mut argv: Vec<String> = vec![target];
        argv.extend(args.iter().cloned());
        argv
    }
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

    #[test]
    fn spawn_argv_wraps_batch_shims_through_cmd_on_windows() {
        let argv = spawn_argv(&["codex".to_string(), "resume".to_string(), "sess-123".to_string()]);
        if cfg!(windows) {
            // `argv[4]` is whatever `which("codex")` resolved (a `.cmd` path
            // or the bare name); the trailing faithful args must survive.
            assert_eq!(argv[0], "cmd.exe");
            assert_eq!(argv[1], "/d");
            assert_eq!(argv[2], "/s");
            assert_eq!(argv[3], "/c");
            assert_eq!(argv[5], "resume");
            assert_eq!(argv[6], "sess-123");
        } else {
            assert_eq!(argv, vec!["codex".to_string(), "resume".to_string(), "sess-123".to_string()]);
        }
    }
}
