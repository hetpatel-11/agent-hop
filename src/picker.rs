use crate::agents::ToolName;
use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEventKind},
    execute, queue,
    style::{Color, Print, ResetColor, SetForegroundColor},
    terminal::{self, ClearType},
};
use std::io::{stdout, Write};

/// Prompt-time agent picker. Uninstalled agents show `(not installed)`;
/// picking one runs its install command before launching. This flow only
/// happens here, at startup -- once inside the TUI toggle bar, only
/// already-installed agents are selectable.
pub async fn pick_agent() -> anyhow::Result<ToolName> {
    let agents = ToolName::ALL;
    let mut selected: usize = 0;
    let mut installed: Vec<bool> = agents.iter().map(|t| t.is_installed()).collect();

    let mut out = stdout();
    terminal::enable_raw_mode()?;
    execute!(out, cursor::Hide)?;

    let render = |out: &mut std::io::Stdout, selected: usize, installed: &[bool]| -> anyhow::Result<()> {
        queue!(out, terminal::Clear(ClearType::FromCursorDown))?;
        for (i, tool) in agents.iter().enumerate() {
            let marker = if i == selected { "❯" } else { " " };
            if i == selected {
                queue!(out, SetForegroundColor(Color::Cyan))?;
            }
            let status = if installed[i] { "".to_string() } else { " (not installed)".to_string() };
            queue!(out, Print(format!("{marker} {}{status}\r\n", tool.slug())))?;
            if i == selected {
                queue!(out, ResetColor)?;
            }
        }
        queue!(out, Print("\r\n  ↑/↓ to move, enter to select, ctrl+c to quit\r\n"))?;
        queue!(out, cursor::MoveUp((agents.len() + 2) as u16))?;
        out.flush()?;
        Ok(())
    };

    render(&mut out, selected, &installed)?;

    let result = loop {
        if let Event::Key(key) = event::read()? {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            match key.code {
                KeyCode::Up => {
                    selected = if selected == 0 { agents.len() - 1 } else { selected - 1 };
                    render(&mut out, selected, &installed)?;
                }
                KeyCode::Down => {
                    selected = (selected + 1) % agents.len();
                    render(&mut out, selected, &installed)?;
                }
                KeyCode::Char('c') if key.modifiers.contains(event::KeyModifiers::CONTROL) => {
                    break Err(anyhow::anyhow!("cancelled"));
                }
                KeyCode::Enter => {
                    break Ok(agents[selected]);
                }
                _ => {}
            }
        }
    };

    queue!(out, terminal::Clear(ClearType::FromCursorDown), cursor::Show)?;
    out.flush()?;
    terminal::disable_raw_mode()?;

    let tool = result?;

    if !installed[ToolName::ALL.iter().position(|t| *t == tool).unwrap()] {
        install(tool)?;
        installed[ToolName::ALL.iter().position(|t| *t == tool).unwrap()] = true;
    }

    Ok(tool)
}

fn install(tool: ToolName) -> anyhow::Result<()> {
    let (cmd, args) = tool.install_command();
    println!("Installing {}...", tool.slug());
    let status = std::process::Command::new(cmd).args(args).status()?;
    if !status.success() {
        anyhow::bail!("failed to install {} (exit {status})", tool.slug());
    }
    Ok(())
}
