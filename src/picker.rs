use crate::agents::ToolName;
use crate::theme;
use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute, queue,
    style::Print,
    terminal::{self, ClearType},
};
use std::io::{stdout, Write};

/// Shared arrow-key list. Used by the mux startup agent picker and by
/// `ah cli` (scope + resume-in). Labels may already contain ANSI
/// (per-agent colors); we only bold the selected row.
pub fn pick_list(title: Option<&str>, labels: &[String], start: usize) -> anyhow::Result<usize> {
    anyhow::ensure!(!labels.is_empty(), "nothing to pick");
    let mut selected = start.min(labels.len() - 1);
    let mut out = stdout();
    terminal::enable_raw_mode()?;
    execute!(out, cursor::Hide)?;

    let extra = if title.is_some() { 1 } else { 0 };
    let height = (labels.len() + extra + 2) as u16;

    let render = |out: &mut std::io::Stdout, selected: usize| -> anyhow::Result<()> {
        queue!(out, terminal::Clear(ClearType::FromCursorDown))?;
        if let Some(title) = title {
            queue!(out, Print(format!("{title}\r\n")))?;
        }
        for (i, label) in labels.iter().enumerate() {
            let marker = if i == selected { "❯" } else { " " };
            let line = if i == selected {
                theme::bold(&format!("{marker} {label}"))
            } else {
                format!("{marker} {label}")
            };
            queue!(out, Print(format!("{line}\r\n")))?;
        }
        queue!(out, Print("\r\n  ↑/↓ move · enter select · ctrl+c cancel\r\n"))?;
        queue!(out, cursor::MoveUp(height))?;
        out.flush()?;
        Ok(())
    };

    render(&mut out, selected)?;
    let result = loop {
        if let Event::Key(key) = event::read()? {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            match key.code {
                KeyCode::Up => {
                    selected = if selected == 0 { labels.len() - 1 } else { selected - 1 };
                    render(&mut out, selected)?;
                }
                KeyCode::Down => {
                    selected = (selected + 1) % labels.len();
                    render(&mut out, selected)?;
                }
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    break Err(anyhow::anyhow!("Cancelled."));
                }
                KeyCode::Esc => break Err(anyhow::anyhow!("Cancelled.")),
                KeyCode::Enter => break Ok(selected),
                _ => {}
            }
        }
    };

    queue!(out, terminal::Clear(ClearType::FromCursorDown), cursor::Show)?;
    out.flush()?;
    terminal::disable_raw_mode()?;
    result
}

/// Prompt-time agent picker. Uninstalled agents show `(not installed)`;
/// picking one runs its install command before launching. This flow only
/// happens here, at startup -- once inside the TUI toggle bar, only
/// already-installed agents are selectable.
pub async fn pick_agent() -> anyhow::Result<ToolName> {
    let agents = ToolName::ALL;
    let installed: Vec<bool> = agents.iter().map(|t| t.is_installed()).collect();
    let labels: Vec<String> = agents
        .iter()
        .enumerate()
        .map(|(i, tool)| {
            let mut label = theme::tool_tag(*tool);
            if !installed[i] {
                label.push_str(&theme::grey(" (not installed)"));
            }
            label
        })
        .collect();

    let selected = pick_list(None, &labels, 0)?;
    let tool = agents[selected];
    let already_installed = installed[selected];

    if !already_installed {
        install(tool)?;
    }

    crate::telemetry::capture(
        "agent_selected",
        serde_json::json!({
            "agent": tool.slug(),
            "was_installed": already_installed,
        }),
    );

    Ok(tool)
}

fn install(tool: ToolName) -> anyhow::Result<()> {
    let (cmd, args) = tool.install_command();
    println!("Installing {}...", tool.slug());
    let mut argv = vec![cmd.to_string()];
    argv.extend(args.iter().map(|s| (*s).to_string()));
    let status = crate::agents::std_command(&argv).status()?;
    if !status.success() {
        anyhow::bail!("failed to install {} (exit {status})", tool.slug());
    }
    Ok(())
}
