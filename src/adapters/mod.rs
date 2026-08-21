use serde::{Deserialize, Serialize};

/// Universal intermediate shape every adapter reads a source agent's native
/// session format into, and writes a target agent's native format back out
/// of. Ported 1:1 from the TypeScript `Turn`/`ToolCallRecord`/`Attachment`
/// types -- this is the actual cross-agent translation IP, not the pty/TUI
/// plumbing around it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Turn {
    pub role: Role,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCallRecord>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attachments: Option<Vec<Attachment>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallRecord {
    pub name: String,
    pub input: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attachment {
    pub mime_type: String,
    pub base64: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
}

pub mod claude;
pub mod codex;
pub mod opencode;
pub mod pi;
pub mod grok;

pub trait Adapter {
    fn read(&self, session_id: &str) -> anyhow::Result<Vec<Turn>>;
    fn write(&self, turns: &[Turn], project_path: &str) -> anyhow::Result<String>;
}
