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

    // Kitty's graphics protocol needs raster data (PNG/JPEG), not SVG --
    // some brands' first "icon" entry is a vector wordmark, so prefer a
    // raster icon explicitly rather than just taking the first icon match.
    let is_raster = |url: &str| {
        let lower = url.to_ascii_lowercase();
        lower.ends_with(".png") || lower.ends_with(".jpg") || lower.ends_with(".jpeg")
    };
    let icon = res
        .brand
        .logos
        .iter()
        .find(|l| l.kind == "icon" && is_raster(&l.url))
        .or_else(|| res.brand.logos.iter().find(|l| is_raster(&l.url)))
        .or_else(|| res.brand.logos.iter().find(|l| l.kind == "icon"))
        .or_else(|| res.brand.logos.first())
        .ok_or_else(|| anyhow::anyhow!("no logo returned for {}", tool.slug()))?;

    let bytes = client.get(&icon.url).send().await?.bytes().await?;
    // Kitty's f=100 format code means real PNG specifically -- some brands'
    // "raster icon" is actually a JPEG served with no reliable content-type,
    // so decode+re-encode to guarantee the cached file is genuinely PNG
    // regardless of what format the CDN actually served.
    let decoded = image::load_from_memory(&bytes)?;
    decoded.save_with_format(&path, image::ImageFormat::Png)?;
    Ok(path)
}

/// Stable per-tool Kitty image id -- lets a redraw reference an
/// already-transmitted image cheaply instead of re-sending its pixel data.
pub fn image_id_for(tool: ToolName) -> u32 {
    match tool {
        ToolName::Claude => 1,
        ToolName::Codex => 2,
        ToolName::OpenCode => 3,
        ToolName::Pi => 4,
        ToolName::Grok => 5,
    }
}

/// Transmits a PNG via the Kitty graphics protocol under a stable image id
/// (chunked base64) WITHOUT displaying it yet -- call once per tool per
/// process lifetime. Caller is responsible for having already confirmed
/// terminal support -- unsupported terminals should use `text_badge`
/// instead.
///
/// Splitting transmit from display matters a lot here: the toggle bar
/// redraws after *every* chunk of child output (streaming responses can
/// produce dozens of chunks a second), and re-sending the full base64
/// payload on each of those redraws was a real, confirmed bug -- it
/// flooded the escape-sequence stream with the entire image repeatedly and
/// visibly corrupted the screen (looked like garbled/repeating content).
/// Kitty lets you transmit pixel data once under an id, then reference it
/// with a cheap `put` command afterward.
pub fn transmit_kitty(path: &std::path::Path, image_id: u32, out: &mut impl Write) -> anyhow::Result<()> {
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
            // q=2 -- suppress the terminal's response entirely (including
            // errors). Without it, the terminal replies on its own with
            // `ESC _G i=<id>;OK ESC \` -- that response arrives on *our*
            // real stdin (we're the process actually attached to the real
            // terminal), and our stdin relay had no way to distinguish it
            // from a real keystroke, so it got forwarded straight into the
            // child agent, which echoed it back as visible garbage text.
            // Confirmed as a real, reproduced bug from a live terminal
            // capture, not theorized.
            write!(out, "\x1b_Ga=t,f=100,i={image_id},q=2,m={more};{part}\x1b\\")?;
            first = false;
        } else {
            write!(out, "\x1b_Gm={more};{part}\x1b\\")?;
        }
        i = end;
    }
    Ok(())
}

/// Displays an already-transmitted image (see `transmit_kitty`) at the
/// current cursor position, sized to `cols` columns by 1 row. Cheap -- no
/// pixel payload, just a placement reference by id. Needed on every redraw
/// because clearing the row's text (which the toggle bar does before every
/// redraw) also deletes any image placement occupying those cells.
pub fn put_kitty(image_id: u32, cols: u16, out: &mut impl Write) -> anyhow::Result<()> {
    // q=2 -- see the comment in transmit_kitty for why this is required,
    // not optional: without it the terminal's own response ends up
    // forwarded into the child agent as if it were typed input.
    write!(out, "\x1b_Ga=p,i={image_id},c={cols},r=1,q=2\x1b\\")?;
    Ok(())
}

/// Colored text badge fallback for terminals without Kitty/iTerm2/Sixel
/// graphics support.
pub fn text_badge(tool: ToolName) -> String {
    format!(" {} ", tool.slug())
}

/// Heuristic Kitty-graphics-protocol capability check. A live in-band query
/// (`CSI _Gi=1,a=q` + read the response) would be more precise but requires
/// synchronizing with the raw stdin relay thread before it starts reading;
/// env-based detection is what most Kitty-protocol-aware tools reach for
/// first and is enough for v1 -- covers Kitty itself, Ghostty (confirmed
/// working live), and WezTerm.
pub fn supports_kitty_graphics() -> bool {
    std::env::var("KITTY_WINDOW_ID").is_ok()
        || std::env::var("GHOSTTY_RESOURCES_DIR").is_ok()
        || std::env::var("TERM_PROGRAM").map(|v| v == "ghostty" || v == "WezTerm").unwrap_or(false)
        || std::env::var("TERM").map(|v| v == "xterm-kitty").unwrap_or(false)
}

/// Prefetches every agent's logo up front (before entering raw mode) so
/// per-hop toggle-bar redraws never block on network I/O. Failures are
/// tolerated per-tool -- a tool with no fetchable logo just falls back to
/// its text badge rather than failing the whole run.
pub async fn ensure_all_logos() -> std::collections::HashMap<&'static str, std::path::PathBuf> {
    let mut map = std::collections::HashMap::new();
    for tool in ToolName::ALL {
        match ensure_logo(tool).await {
            Ok(path) => {
                map.insert(tool.slug(), path);
            }
            Err(e) => {
                eprintln!("agent-hop: couldn't fetch logo for {} ({e}), using text badge", tool.slug());
            }
        }
    }
    map
}
