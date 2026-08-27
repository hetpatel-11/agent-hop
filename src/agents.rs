use std::fmt;
use std::path::PathBuf;
use std::process::Command;

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

/// Normalize `["codex", "resume", id]` into an argv `CreateProcessW` /
/// `portable_pty` can actually spawn.
///
/// POSIX: returned unchanged.
///
/// Windows: npm puts three launchers on PATH — an extensionless POSIX
/// shim, a `*.cmd` batch wrapper, and a `*.ps1`. `CreateProcessW` cannot
/// run the shim (os error 193: not a valid Win32 application) or a batch
/// file by itself. Resolve via [`which`] (which prefers `.cmd` over the
/// shim) and route `*.cmd`/`*.bat` through `cmd.exe /d /s /c`. Reported
/// and patched in that form by @UnknownUser03393 in
/// https://github.com/hetpatel-11/agent-hop/pull/3 — 0.1.4 applies the
/// same wrap at every harness spawn, not only the live pty.
pub fn spawn_argv(command: &[String]) -> Vec<String> {
    spawn_argv_for(command, cfg!(windows), which)
}

fn spawn_argv_for(
    command: &[String],
    windows: bool,
    resolve: impl Fn(&str) -> Option<PathBuf>,
) -> Vec<String> {
    let Some((program, args)) = command.split_first() else {
        return command.to_vec();
    };
    if !windows {
        return command.to_vec();
    }
    let target = resolve(program)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| program.clone());
    let lower = target.to_ascii_lowercase();
    if lower.ends_with(".cmd") || lower.ends_with(".bat") {
        let mut argv = vec!["cmd.exe".into(), "/d".into(), "/s".into(), "/c".into(), target];
        argv.extend(args.iter().cloned());
        argv
    } else {
        let mut argv = vec![target];
        argv.extend(args.iter().cloned());
        argv
    }
}

/// `std::process::Command` pointed at [`spawn_argv`] so adapter version
/// probes, OpenCode export/import, picker install, and `ah resume` all
/// hit the same Windows shim path as the live pty.
pub fn std_command(command: &[String]) -> Command {
    let argv = spawn_argv(command);
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    cmd
}

pub fn std_command_bin(bin: &str, args: &[&str]) -> Command {
    let mut argv = Vec::with_capacity(args.len() + 1);
    argv.push(bin.to_string());
    argv.extend(args.iter().map(|s| s.to_string()));
    std_command(&argv)
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
        let argv = spawn_argv_for(
            &["codex".into(), "resume".into(), "sess-123".into()],
            true,
            |_| Some(PathBuf::from(r"C:\Users\a\AppData\Roaming\npm\codex.cmd")),
        );
        assert_eq!(
            argv,
            vec![
                "cmd.exe",
                "/d",
                "/s",
                "/c",
                r"C:\Users\a\AppData\Roaming\npm\codex.cmd",
                "resume",
                "sess-123",
            ]
        );
    }

    #[test]
    fn spawn_argv_runs_exe_directly_on_windows() {
        let argv = spawn_argv_for(
            &["codex".into(), "--version".into()],
            true,
            |_| Some(PathBuf::from(r"C:\Program Files\codex\codex.exe")),
        );
        assert_eq!(argv, vec![r"C:\Program Files\codex\codex.exe", "--version"]);
    }

    #[test]
    fn spawn_argv_is_passthrough_on_unix() {
        let argv = spawn_argv_for(&["codex".into(), "resume".into(), "sess".into()], false, |_| None);
        assert_eq!(argv, vec!["codex", "resume", "sess"]);
    }
}
