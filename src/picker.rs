use crate::agents::ToolName;

/// Prompt-time agent picker. Uninstalled agents show `(not installed)`;
/// picking one runs its install command before launching. This flow only
/// happens here, at startup -- once inside the TUI toggle bar, only
/// already-installed agents are selectable.
pub async fn pick_agent() -> anyhow::Result<ToolName> {
    todo!("task #2: render picker listing ToolName::ALL with install-state, handle selection + install-on-pick")
}
