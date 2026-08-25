//! `ah feedback` — send a short note to the same Cloudflare Worker / D1
//! that holds telemetry. Explicit: we send what they typed, even if
//! telemetry is off.

use std::io::{IsTerminal, Read, Write};
use std::time::Duration;

const DEFAULT_ENDPOINT: &str = "https://telemetry.agent-hop.com/feedback";
const ENDPOINT_ENV: &str = "AH_FEEDBACK_ENDPOINT";
const MAX_CHARS: usize = 4000;

pub fn collect_message(parts: Vec<String>) -> anyhow::Result<String> {
    if !parts.is_empty() {
        return Ok(parts.join(" "));
    }
    let mut stdin = std::io::stdin();
    if !stdin.is_terminal() {
        let mut buf = String::new();
        stdin.read_to_string(&mut buf)?;
        return Ok(buf);
    }
    let mut out = std::io::stderr();
    writeln!(out, "What's on your mind? (empty line cancels)")?;
    write!(out, "> ")?;
    out.flush()?;
    let mut line = String::new();
    stdin.read_line(&mut line)?;
    Ok(line)
}

pub async fn submit(raw: &str) -> anyhow::Result<()> {
    let message: String = raw.trim().chars().take(MAX_CHARS).collect();
    if message.is_empty() {
        anyhow::bail!("no message");
    }
    let endpoint = std::env::var(ENDPOINT_ENV)
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_ENDPOINT.to_string());
    let payload = serde_json::json!({
        "message": message,
        "device_id": crate::telemetry::install_id(),
        "app_version": env!("CARGO_PKG_VERSION"),
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
    });
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(8))
        .build()?;
    let res = client.post(&endpoint).json(&payload).send().await?;
    let status = res.status();
    if status.is_success() || status.as_u16() == 204 {
        return Ok(());
    }
    anyhow::bail!("could not send feedback ({status})");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collect_message_joins_argv_parts() {
        let s = collect_message(vec!["the hop".into(), "bar".into(), "is hard".into()]).unwrap();
        assert_eq!(s, "the hop bar is hard");
    }
}
