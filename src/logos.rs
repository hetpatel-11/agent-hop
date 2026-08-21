use crate::agents::ToolName;
use base64::Engine;
use serde::Deserialize;
use std::io::Write;

const API_KEY_ENV: &str = "CONTEXT_DEV_API_KEY";

#[derive(Deserialize)]
struct BrandResponse {
    brand: Brand,
}

#[derive(Deserialize)]
struct Brand {
    logos: Vec<Logo>,
}

#[derive(Deserialize)]
struct Logo {
    url: String,
    #[serde(rename = "type")]
    kind: String,
}

fn cache_path(tool: ToolName) -> std::path::PathBuf {
    let dir = dirs::home_dir()
        .unwrap_or_default()
        .join(".agent-hop")
        .join("logos");
    dir.join(format!("{}.png", tool.slug()))
}

/// Fetches (or returns the cached) icon-type logo PNG for a tool via the
/// context.dev Brand API. Confirmed working against the real API with a
/// real key -- Anthropic's icon round-tripped and rendered correctly via
/// the Kitty graphics protocol in a live test.
pub async fn ensure_logo(tool: ToolName) -> anyhow::Result<std::path::PathBuf> {
    let path = cache_path(tool);
    if path.exists() {
        return Ok(path);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let key = std::env::var(API_KEY_ENV)
        .map_err(|_| anyhow::anyhow!("{API_KEY_ENV} not set -- cannot fetch real logos"))?;

    let client = reqwest::Client::new();
    let res: BrandResponse = client
        .post("https://api.context.dev/v1/brand/retrieve")
        .bearer_auth(key)
        .json(&serde_json::json!({ "domain": tool.brand_domain() }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    let icon = res
        .brand
        .logos
        .iter()
        .find(|l| l.kind == "icon")
        .or_else(|| res.brand.logos.first())
        .ok_or_else(|| anyhow::anyhow!("no logo returned for {}", tool.slug()))?;

    let bytes = client.get(&icon.url).send().await?.bytes().await?;
    std::fs::write(&path, &bytes)?;
    Ok(path)
}

/// Renders a PNG at `path` inline via the Kitty graphics protocol
/// (transmit + display, chunked base64). Caller is responsible for having
/// already confirmed terminal support -- unsupported terminals should use
/// `text_badge` instead.
pub fn render_kitty(path: &std::path::Path, out: &mut impl Write) -> anyhow::Result<()> {
    let bytes = std::fs::read(path)?;
    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    const CHUNK: usize = 4096;
    let chars: Vec<char> = b64.chars().collect();
    let mut i = 0;
    let mut first = true;
    while i < chars.len() {
        let end = (i + CHUNK).min(chars.len());
        let part: String = chars[i..end].iter().collect();
        let more = if end < chars.len() { 1 } else { 0 };
        if first {
            write!(out, "\x1b_Ga=T,f=100,m={more};{part}\x1b\\")?;
            first = false;
        } else {
            write!(out, "\x1b_Gm={more};{part}\x1b\\")?;
        }
        i = end;
    }
    Ok(())
}

/// Colored text badge fallback for terminals without Kitty/iTerm2/Sixel
/// graphics support.
pub fn text_badge(tool: ToolName) -> String {
    format!(" {} ", tool.slug())
}
