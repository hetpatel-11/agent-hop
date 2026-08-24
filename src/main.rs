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
mod telemetry;
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
    if !is_background_index {
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

    // Init telemetry for real user sessions only (never the detached
    // background indexer). This shows the one-time notice when enabled.
    if !is_background_index {
        telemetry::init();
        let entry = match &cli.command {
            Some(Commands::Claude) => "claude",
            Some(Commands::Codex) => "codex",
            Some(Commands::Opencode) => "opencode",
            Some(Commands::Pi) => "pi",
            Some(Commands::Grok) => "grok",
            Some(Commands::Resume { .. }) => "resume",
            _ => "picker",
        };
        telemetry::capture(
            "app_launched",
            serde_json::json!({
                "entry": entry,
                "installed": ToolName::installed_slugs(),
            }),
        );
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
        Some(Commands::Telemetry { .. }) => unreachable!(),
        Some(Commands::BackgroundIndex) => {
            let sessions = search::collect_sessions(&ToolName::ALL);
            vector_index::build_index(&sessions).await?;
            return Ok(());
        }
        None => None,
    };

    let agent = match initial_agent {
        Some(a) => a,
        None => picker::pick_agent().await?,
    };

    let res = tui::run(agent, None).await;
    telemetry::flush().await;
    res
}


