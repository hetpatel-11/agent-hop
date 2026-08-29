//! Stub adapter for harnesses we can spawn but do not yet translate.
//! Hop / search degrade to a fresh launch instead of failing the mux.

use super::{Adapter, SessionRef, Turn};
use crate::agents::ToolName;

pub struct GenericAdapter {
    pub tool: ToolName,
}

impl Adapter for GenericAdapter {
    fn list_sessions(&self) -> anyhow::Result<Vec<SessionRef>> {
        Ok(Vec::new())
    }

    fn read(&self, _session_ref: &SessionRef) -> anyhow::Result<Vec<Turn>> {
        anyhow::bail!("{} has no hop/search session format yet", self.tool.slug())
    }

    fn write(&self, _turns: &[Turn], _project_path: &str) -> anyhow::Result<String> {
        anyhow::bail!("cannot hop into {} yet — launching a fresh session", self.tool.slug())
    }

    fn resume_cmd(&self, _session_id: &str, _project_path: &str) -> Vec<String> {
        vec![self.tool.binary().to_string()]
    }
}
