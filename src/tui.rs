use crate::agents::ToolName;

/// Single-pane TUI shell: one agent's real pty rendered full-pane, with a
/// persistent toggle strip (top/bottom, owned by us, agent never draws into
/// it) for switching between installed agents via Alt+Up/Down or a click.
pub async fn run(initial: ToolName) -> anyhow::Result<()> {
    todo!("task #3: spawn {initial:?} via portable-pty, render pane + toggle bar, wire hop logic")
}
