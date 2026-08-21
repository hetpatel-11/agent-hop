mod agents;
mod logos;
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

use agents::ToolName;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "ah", version, about = "Search all your coding-agent chats, then continue any session in any agent.")]
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
    Resume,
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

    let initial_agent = match cli.command {
        Some(Commands::Claude) => Some(ToolName::Claude),
        Some(Commands::Codex) => Some(ToolName::Codex),
        Some(Commands::Opencode) => Some(ToolName::OpenCode),
        Some(Commands::Pi) => Some(ToolName::Pi),
        Some(Commands::Grok) => Some(ToolName::Grok),
        Some(Commands::Resume) => {
            search::run_standalone_resume().await?;
            return Ok(());
        }
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

    tui::run(agent, None).await
}


