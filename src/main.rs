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
mod detect;
mod attach;
mod server;
mod worktree;
mod plugin;
mod remote;

use agents::ToolName;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "ah", version, about = "Runtime for coding-agent harnesses. Run Claude Code, Codex, OpenCode, Pi, Grok, Cursor, Copilot, Gemini, and Droid in one terminal; hop live between them; search and resume any local session.")]
struct Cli {
    /// Thin remote: `ssh -t HOST -- ah` (same as `ah remote HOST`).
    #[arg(long)]
    remote: Option<String>,
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
    /// Launch straight into Cursor Agent (`cursor-agent` / `agent`)
    Cursor,
    /// Launch straight into GitHub Copilot CLI
    Copilot,
    /// Launch straight into Gemini CLI
    Gemini,
    /// Launch straight into Droid
    Droid,
    /// Search and resume a past session (standalone, outside the TUI)
    Resume {
        /// Search query (omitted = interactive prompt)
        query: Option<String>,
        /// Restrict search to one agent (claude|codex|opencode|pi|grok|cursor|copilot|gemini|droid)
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
        /// Agent slug. Omit to use the picker.
        agent: Option<String>,
    },
    Hop {
        /// Target agent. Hops another tab, never this pane.
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
    /// Drive a live agent (list / wait / prompt / read / send-keys / rename).
    /// Works from any terminal while `ah` is running.
    Agent {
        #[command(subcommand)]
        command: AgentCmd,
    },
    /// Rename the focused agent (or `--name` / `--tab`) in the live mux.
    Rename {
        /// New display name, herdr-style (`security-droid`).
        name: String,
        #[arg(long)]
        tab: Option<u32>,
        /// Current name of the agent to rename.
        #[arg(long)]
        current: Option<String>,
    },
    /// Start, stop, or show the background mux (`status` default).
    Server {
        action: Option<String>,
    },
    /// Hidden: the background mux. Started by `ah`, not by hand.
    #[command(hide = true, name = "__daemon")]
    Daemon {
        #[arg(long)]
        tool: Option<String>,
    },
    /// Hidden: runs the semantic-index build in-process. Never invoked
    /// directly by a user -- search.rs spawns this detached from the
    /// interactive CLI whenever there's unindexed content, so indexing
    /// survives after the parent process exits.
    #[command(hide = true, name = "__background-index")]
    BackgroundIndex,
    /// SSH to a host and attach to `ah` there (thin remote).
    Remote {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        ssh: Vec<String>,
    },
    /// Git worktrees as first-class workspace folders.
    Worktree {
        #[command(subcommand)]
        command: WorktreeCmd,
    },
    /// List local plugins (`~/.agent-hop/plugins`).
    Plugin {
        #[command(subcommand)]
        command: PluginCmd,
    },
}

#[derive(Subcommand)]
enum WorktreeCmd {
    /// `git worktree list` for this repo (or `--path`).
    List {
        #[arg(long)]
        path: Option<String>,
    },
    /// `git worktree add` a sibling folder named `<repo>-<name>`.
    Add {
        name: String,
        #[arg(long)]
        branch: Option<String>,
        #[arg(long)]
        dest: Option<String>,
        #[arg(long)]
        path: Option<String>,
    },
    /// Remove a worktree by path (or name under the default sibling layout).
    Remove {
        target: String,
        #[arg(long)]
        path: Option<String>,
    },
}

#[derive(Subcommand)]
enum PluginCmd {
    /// Show installed plugins and their Ctrl+B chords.
    List,
}

#[derive(Subcommand)]
enum AgentCmd {
    /// List live agents and their idle/working/blocked status.
    List,
    /// Block until an agent reaches a status (`idle`, `working`, `blocked`, `done`, `unknown`).
    Wait {
        #[arg(long, default_value = "idle,blocked")]
        until: String,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        tab: Option<u32>,
        /// Seconds to wait (default: no limit).
        #[arg(long)]
        timeout: Option<u64>,
    },
    /// Type a prompt into an agent and submit it.
    Prompt {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        text: Vec<String>,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        tab: Option<u32>,
    },
    /// Print the agent's visible screen.
    Read {
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        tab: Option<u32>,
    },
    /// Send raw keys (`y\\r`, `\\t`, …).
    SendKeys {
        keys: String,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        tab: Option<u32>,
    },
    /// Rename an agent and persist it in layout.json.
    Rename {
        name: String,
        #[arg(long)]
        tab: Option<u32>,
        #[arg(long)]
        current: Option<String>,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    if let Some(host) = cli.remote.as_deref() {
        return remote::run(&[host.to_string()]);
    }

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
                    | Commands::Agent { .. }
                    | Commands::Rename { .. }
                    | Commands::Server { .. }
                    | Commands::Daemon { .. }
                    | Commands::Remote { .. }
                    | Commands::Worktree { .. }
                    | Commands::Plugin { .. }
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

    if let Some(Commands::Rename { name, tab, current }) = &cli.command {
        control::request_ex("rename", None, *tab, None, current.as_deref(), None, None, Some(name))?;
        println!("Renamed to {name}.");
        return Ok(());
    }

    if let Some(Commands::Agent { command }) = &cli.command {
        return run_agent_cmd(command);
    }

    if let Some(Commands::Remote { ssh }) = &cli.command {
        return remote::run(ssh);
    }

    if let Some(Commands::Worktree { command }) = &cli.command {
        return run_worktree_cmd(command);
    }

    if let Some(Commands::Plugin { command }) = &cli.command {
        match command {
            PluginCmd::List => {
                plugin::print_list();
                return Ok(());
            }
        }
    }

    if let Some(Commands::Server { action }) = &cli.command {
        match action.as_deref() {
            Some("stop") | Some("kill") => return server::stop(),
            _ => {
                server::status();
                return Ok(());
            }
        }
    }

    if let Some(Commands::Daemon { tool }) = cli.command {
        let restore = layout::load();
        let agent = tool
            .as_deref()
            .and_then(ToolName::from_slug)
            .or_else(|| restore.as_ref().and_then(|m| m.first_tool()))
            .unwrap_or(ToolName::Claude);
        return tui::run_daemon(agent, restore);
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
            Some(Commands::Cursor) => "cursor",
            Some(Commands::Copilot) => "copilot",
            Some(Commands::Gemini) => "gemini",
            Some(Commands::Droid) => "droid",
            Some(Commands::Resume { .. }) => "resume",
            Some(Commands::Telemetry { .. } | Commands::Feedback { .. } | Commands::Tab { .. } | Commands::Hop { .. } | Commands::Close { .. } | Commands::Focus { .. } | Commands::Workspace { .. } | Commands::Agent { .. } | Commands::Rename { .. } | Commands::Server { .. } | Commands::Daemon { .. } | Commands::BackgroundIndex | Commands::Remote { .. } | Commands::Worktree { .. } | Commands::Plugin { .. }) => "picker",
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
        Some(Commands::Cursor) => Some(ToolName::Cursor),
        Some(Commands::Copilot) => Some(ToolName::Copilot),
        Some(Commands::Gemini) => Some(ToolName::Gemini),
        Some(Commands::Droid) => Some(ToolName::Droid),
        Some(Commands::Resume { query, agent, resume_in }) => {
            let res = search::run_standalone_resume(query, agent, resume_in).await;
            telemetry::flush().await;
            res?;
            return Ok(());
        }
        // Handled and returned above; kept for match exhaustiveness.
        Some(Commands::Telemetry { .. } | Commands::Feedback { .. } | Commands::Tab { .. } | Commands::Hop { .. } | Commands::Close { .. } | Commands::Focus { .. } | Commands::Workspace { .. } | Commands::Agent { .. } | Commands::Rename { .. } | Commands::Server { .. } | Commands::Daemon { .. } | Commands::Remote { .. } | Commands::Worktree { .. } | Commands::Plugin { .. }) => unreachable!(),
        Some(Commands::BackgroundIndex) => {
            let sessions = search::collect_sessions(&ToolName::ALL);
            vector_index::build_index(&sessions).await?;
            return Ok(());
        }
        None => None,
    };

    if server::is_running() {
        attach::run_client()?;
        telemetry::flush().await;
        return Ok(());
    }

    let agent = match initial_agent {
        Some(a) => a,
        None if restore.as_ref().is_some_and(|m| !m.is_empty()) => restore
            .as_ref()
            .and_then(|m| m.first_tool())
            .unwrap_or(ToolName::Claude),
        None => picker::pick_agent().await?,
    };

    server::spawn_daemon(agent)?;
    attach::run_client()?;
    telemetry::flush().await;
    Ok(())
}

fn run_agent_cmd(cmd: &AgentCmd) -> anyhow::Result<()> {
    match cmd {
        AgentCmd::List => {
            let live = live_mux()?;
            if live.agents.is_empty() {
                println!("No live agents.");
                return Ok(());
            }
            for a in &live.agents {
                let mark = if a.focused { '*' } else { ' ' };
                println!("{mark} {:>2}  {:<22} {:<8} {}", a.index, a.name, a.status, a.tool);
            }
            Ok(())
        }
        AgentCmd::Wait { until, name, tab, timeout } => {
            let wanted: Vec<String> = until
                .split(',')
                .map(|s| s.trim().to_ascii_lowercase())
                .filter(|s| !s.is_empty())
                .collect();
            if wanted.is_empty() {
                anyhow::bail!("--until needs a status (idle, working, blocked, done, unknown)");
            }
            let deadline = timeout.map(|s| std::time::Instant::now() + std::time::Duration::from_secs(s));
            loop {
                let live = live_mux()?;
                let agent = pick_live_agent(&live, name.as_deref(), *tab)?;
                if wanted.iter().any(|w| agent.status.eq_ignore_ascii_case(w)) {
                    println!("{} {}", agent.name, agent.status);
                    return Ok(());
                }
                if deadline.is_some_and(|d| std::time::Instant::now() >= d) {
                    anyhow::bail!("{} still {}", agent.name, agent.status);
                }
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
        }
        AgentCmd::Prompt { text, name, tab } => {
            let text = text.join(" ");
            if text.trim().is_empty() {
                anyhow::bail!("ah agent prompt needs text");
            }
            control::request_ex("prompt", None, *tab, None, name.as_deref(), Some(&text), None, None)?;
            println!("Prompt sent.");
            Ok(())
        }
        AgentCmd::Read { name, tab } => {
            let live = live_mux()?;
            let agent = pick_live_agent(&live, name.as_deref(), *tab)?;
            for line in &agent.lines {
                println!("{line}");
            }
            Ok(())
        }
        AgentCmd::SendKeys { keys, name, tab } => {
            control::request_ex("send-keys", None, *tab, None, name.as_deref(), None, Some(keys), None)?;
            println!("Keys sent.");
            Ok(())
        }
        AgentCmd::Rename { name, tab, current } => {
            control::request_ex("rename", None, *tab, None, current.as_deref(), None, None, Some(name))?;
            println!("Renamed to {name}.");
            Ok(())
        }
    }
}

fn repo_cwd(path: Option<&str>) -> String {
    path.filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .or_else(|| std::env::current_dir().ok().map(|p| p.to_string_lossy().into_owned()))
        .unwrap_or_else(|| ".".into())
}

fn run_worktree_cmd(cmd: &WorktreeCmd) -> anyhow::Result<()> {
    match cmd {
        WorktreeCmd::List { path } => worktree::print_list(&repo_cwd(path.as_deref())),
        WorktreeCmd::Add { name, branch, dest, path } => {
            let dest = worktree::add(&repo_cwd(path.as_deref()), name, branch.as_deref(), dest.as_deref())?;
            println!("{}", dest.display());
            eprintln!("Open it with: ah workspace --path {}", dest.display());
            Ok(())
        }
        WorktreeCmd::Remove { target, path } => {
            let repo = repo_cwd(path.as_deref());
            let resolved = if std::path::Path::new(target).exists() {
                target.clone()
            } else {
                worktree::list(&repo)?
                    .into_iter()
                    .find(|t| t.path.ends_with(target) || t.branch.as_deref() == Some(target))
                    .map(|t| t.path)
                    .unwrap_or_else(|| target.clone())
            };
            worktree::remove(&repo, &resolved)?;
            println!("Removed {resolved}");
            Ok(())
        }
    }
}

fn live_mux() -> anyhow::Result<control::LiveMux> {
    control::read_live().filter(|m| !m.agents.is_empty()).ok_or_else(|| {
        anyhow::anyhow!("no live ah session (start `ah` first)")
    })
}

fn pick_live_agent<'a>(
    live: &'a control::LiveMux,
    name: Option<&str>,
    tab: Option<u32>,
) -> anyhow::Result<&'a control::LiveAgent> {
    if let Some(n) = name.filter(|s| !s.is_empty()) {
        return live
            .agents
            .iter()
            .find(|a| a.name == n)
            .ok_or_else(|| anyhow::anyhow!("no agent named {n}"));
    }
    if let Some(t) = tab {
        return live
            .agents
            .iter()
            .find(|a| a.index == t as usize)
            .ok_or_else(|| anyhow::anyhow!("no such tab"));
    }
    live.agents
        .iter()
        .find(|a| a.focused)
        .or_else(|| live.agents.first())
        .ok_or_else(|| anyhow::anyhow!("no live agents"))
}


