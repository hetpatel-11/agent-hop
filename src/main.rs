mod agents;
mod picker;
mod tui;
mod search;
mod adapters;
mod util;
mod embed;
mod fuzzy;
mod theme;
mod vector_index;
mod resume;
mod layout;
mod telemetry;
mod feedback;
mod control;
mod update_check;
mod vt;

use agents::ToolName;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "ah", version, about = "Runtime for coding-agent harnesses. Run Claude Code, Codex, OpenCode, Pi, and Grok in one terminal; hop live between them; search and resume any local session.")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Launch straight into Claude Code
    Claude,
    /// Launch straight into Codex
    Codex,
    /// Launch straight into OpenCode
    Opencode,
    /// Launch straight into Pi
    Pi,
    /// Launch straight into Grok
    Grok,
    /// Search and resume a past session (standalone, outside the TUI)
    Resume {
        /// Search query (omitted = interactive prompt)
        query: Option<String>,
        /// Restrict search to one agent (claude|codex|opencode|pi|grok)
        #[arg(short, long)]
        agent: Option<String>,
        /// Resume the picked session in this agent (default: same tool)
        #[arg(short = 'r', long = "resume-in")]
        resume_in: Option<String>,
    },
    /// Show or change anonymous usage telemetry (on by default).
    Telemetry {
        /// `status` (default), `on`, or `off`
        action: Option<String>,
    },
    /// Send us a short note. Lands in the same D1 database as telemetry.
    Feedback {
        /// Your message. Omit to type it on the next line.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        message: Vec<String>,
    },
    /// Open a new tab in the parent ah session. Only works from inside a pane
    /// (`AH_SOCK` is set). Same idea as cmux: the agent runs a command, the
    /// multiplexer opens the panel. `ah tab codex` skips the picker.
    Tab {
        /// Agent slug (claude|codex|opencode|pi|grok). Omit to use the picker.
        agent: Option<String>,
    },
    Hop {
        /// Target agent (claude|codex|opencode|pi|grok). Hops another tab, never this pane.
        agent: String,
        /// 1-based tab in this workspace. Omit if there is exactly one other tab.
        #[arg(long)]
        tab: Option<u32>,
    },
    /// Close another tab (never this pane).
    Close {
        #[arg(long)]
        tab: Option<u32>,
    },
    /// Focus a tab (1-based). This pane keeps running.
    Focus {
        tab: u32,
    },
    /// New workspace, or `next`/`prev`.
    Workspace {
        action: Option<String>,
        #[arg(long)]
        path: Option<String>,
        #[arg(long)]
        agent: Option<String>,
    },
    /// Hidden: runs the semantic-index build in-process. Never invoked
    /// directly by a user -- search.rs spawns this detached from the
    /// interactive CLI whenever there's unindexed content, so indexing
    /// survives after the parent process exits.
    #[command(hide = true, name = "__background-index")]
    BackgroundIndex,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // Skipped for the hidden background-index command (runs silently,
    // detached, with no one to read a message anyway) and whenever
    // stdin/stdout aren't both real terminals (a script/another agent
    // invoking this shouldn't ever stop to mention an update). Bounded to
    // ~1.5s by the check itself even when it does run, so this never
    // meaningfully delays a launch.
    let is_background_index = matches!(cli.command, Some(Commands::BackgroundIndex));
    let skip_update = is_background_index
        || matches!(
            cli.command,
            Some(
                Commands::Telemetry { .. }
                    | Commands::Feedback { .. }
                    | Commands::Tab { .. }
                    | Commands::Hop { .. }
                    | Commands::Close { .. }
                    | Commands::Focus { .. }
                    | Commands::Workspace { .. }
            )
        );
    if !skip_update {
        use std::io::IsTerminal;
        if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
            let info = update_check::check_for_update().await;
            if info.update_available && update_check::prompt_and_maybe_update(&info) {
                return Ok(());
            }
        }
    }

    // The telemetry control command isn't a session -- handle and exit
    // before we init telemetry or emit a launch event.
    if let Some(Commands::Telemetry { action }) = &cli.command {
        match action.as_deref() {
            Some("on") => {
                telemetry::set_enabled(true).ok();
                println!("Telemetry enabled.");
            }
            Some("off") => {
                telemetry::set_enabled(false).ok();
                println!("Telemetry disabled.");
            }
            _ => println!("{}", telemetry::status_line()),
        }
        return Ok(());
    }

    if let Some(Commands::Tab { agent }) = &cli.command {
        control::request("tab", agent.as_deref(), None, None)?;
        println!("Opening a new tab.");
        return Ok(());
    }
    if let Some(Commands::Hop { agent, tab }) = &cli.command {
        control::request("hop", Some(agent), *tab, None)?;
        println!("Hopping the other tab to {agent}.");
        return Ok(());
    }
    if let Some(Commands::Close { tab }) = &cli.command {
        control::request("close", None, *tab, None)?;
        println!("Closing the other tab.");
        return Ok(());
    }
    if let Some(Commands::Focus { tab }) = &cli.command {
        control::request("focus", None, Some(*tab), None)?;
        println!("Focusing tab {tab}.");
        return Ok(());
    }
    if let Some(Commands::Workspace { action, path, agent }) = &cli.command {
        let op = match action.as_deref() {
            Some("next") => "workspace-next",
            Some("prev") => "workspace-prev",
            None => "workspace",
            Some(other) => anyhow::bail!("unknown workspace action '{other}' (use next, prev, or omit)"),
        };
        control::request(op, agent.as_deref(), None, path.as_deref())?;
        println!("Workspace command sent.");
        return Ok(());
    }

    if let Some(Commands::Feedback { message }) = cli.command {
        match feedback::collect_message(message) {
            Ok(text) if text.trim().is_empty() => {
                println!("Cancelled.");
                return Ok(());
            }
            Ok(text) => match feedback::submit(&text).await {
                Ok(()) => println!("Thanks — we got it."),
                Err(e) => anyhow::bail!("Could not send feedback: {e}"),
            },
            Err(e) => anyhow::bail!(e),
        }
        return Ok(());
    }

    // Init telemetry for real user sessions only (never the detached
    // background indexer). This shows the one-time notice when enabled.
    // Layout is loaded first so `app_launched.entry` can be `restore`.
    let restore = if !is_background_index && cli.command.is_none() {
        layout::load()
    } else {
        None
    };
    if !is_background_index {
        telemetry::init();
        let restoring = restore.as_ref().is_some_and(|m| !m.is_empty());
        let entry = match &cli.command {
            Some(Commands::Claude) => "claude",
            Some(Commands::Codex) => "codex",
            Some(Commands::Opencode) => "opencode",
            Some(Commands::Pi) => "pi",
            Some(Commands::Grok) => "grok",
            Some(Commands::Resume { .. }) => "resume",
            Some(Commands::Telemetry { .. } | Commands::Feedback { .. } | Commands::Tab { .. } | Commands::Hop { .. } | Commands::Close { .. } | Commands::Focus { .. } | Commands::Workspace { .. } | Commands::BackgroundIndex) => "picker",
            None if restoring => "restore",
            None => "picker",
        };
        let mut props = serde_json::json!({
            "entry": entry,
            "installed": ToolName::installed_slugs(),
        });
        if restoring {
            if let Some(mux) = restore.as_ref() {
                props["workspaces"] = mux.workspaces.len().into();
                props["tabs"] = mux.tab_count().into();
            }
        }
        telemetry::capture("app_launched", props);
    }

    let initial_agent = match cli.command {
        Some(Commands::Claude) => Some(ToolName::Claude),
        Some(Commands::Codex) => Some(ToolName::Codex),
        Some(Commands::Opencode) => Some(ToolName::OpenCode),
        Some(Commands::Pi) => Some(ToolName::Pi),
        Some(Commands::Grok) => Some(ToolName::Grok),
        Some(Commands::Resume { query, agent, resume_in }) => {
            let res = search::run_standalone_resume(query, agent, resume_in).await;
            telemetry::flush().await;
            res?;
            return Ok(());
        }
        // Handled and returned above; kept for match exhaustiveness.
        Some(Commands::Telemetry { .. } | Commands::Feedback { .. } | Commands::Tab { .. } | Commands::Hop { .. } | Commands::Close { .. } | Commands::Focus { .. } | Commands::Workspace { .. }) => unreachable!(),
        Some(Commands::BackgroundIndex) => {
            let sessions = search::collect_sessions(&ToolName::ALL);
            vector_index::build_index(&sessions).await?;
            return Ok(());
        }
        None => None,
    };
    let agent = match initial_agent {
        Some(a) => a,
        None if restore.as_ref().is_some_and(|m| !m.is_empty()) => restore
            .as_ref()
            .and_then(|m| m.first_tool())
            .unwrap_or(ToolName::Claude),
        None => picker::pick_agent().await?,
    };

    let res = tui::run(agent, None, restore, true).await;
    telemetry::flush().await;
    res
}


