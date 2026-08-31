//! `ah cli`: banner → scope → type-to-search → resume-in → exec. No mux.

use crate::adapters::{self, SessionRef};
use crate::agents::ToolName;
use crate::picker;
use crate::resume::{self, SearchFrame};
use crate::search::{self, Ranker, SearchOptions};
use crate::theme;
use anyhow::bail;
use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute, queue,
    terminal::{self, ClearType},
};
use std::io::{stdout, IsTerminal, Write};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

const MAX_ITEMS: usize = 12;
const STAGE2_DEBOUNCE_MS: u64 = 80;
const SEARCH_TITLE: &str = "Type to search (e.g. oauth bug, video editor, auth migration):";

pub async fn run(
    query_arg: Option<String>,
    agent_arg: Option<String>,
    resume_in_arg: Option<String>,
) -> anyhow::Result<()> {
    let non_interactive = !stdio_is_tty();
    print_banner();
    println!(
        "{}",
        theme::grey("search every agent's sessions, resume any one in a different agent")
    );

    if non_interactive && query_arg.is_none() {
        eprintln!(
            "Cancelled. Running non-interactively (no TTY) -- a search query is required, e.g. `ah cli \"oauth bug\"`."
        );
        std::process::exit(1);
    }

    let scope = match &agent_arg {
        Some(raw) => vec![parse_agent(raw)?],
        None if non_interactive => ToolName::ALL.to_vec(),
        None => pick_scope()?,
    };

    print!(
        "{}",
        theme::grey(&format!(
            "Loading {} sessions...",
            scope.iter().map(|t| t.slug()).collect::<Vec<_>>().join(", ")
        ))
    );
    stdout().flush()?;
    let sessions = search::collect_sessions(&scope);
    // First keystroke used to block on ONNX + the 30MB index. Do that
    // here so live refine is just embed-query + cosine (~tens of ms).
    let _ = crate::embed::ensure_model(|_| {}).await;
    crate::vector_index::warmup();
    execute!(
        stdout(),
        crossterm::cursor::MoveToColumn(0),
        terminal::Clear(ClearType::CurrentLine)
    )?;
    println!(
        "Loaded {} session{}.",
        sessions.len(),
        if sessions.len() == 1 { "" } else { "s" }
    );
    if sessions.is_empty() {
        println!("No sessions found yet.");
        return Ok(());
    }

    let picked = if non_interactive {
        let query = query_arg.clone().unwrap_or_default();
        let result = search::search_sessions(sessions, &query, SearchOptions::default()).await;
        if result.results.is_empty() {
            println!("No sessions found. Try a different query or agent scope.");
            return Ok(());
        }
        let picked = result.results[0].clone();
        println!(
            "Non-interactive: auto-picked top match -- {} {}",
            theme::tool_tag(picked.tool),
            search::one_line(&picked.title)
        );
        picked
    } else {
        let index_pending = search::ensure_indexing_triggered(&sessions);
        if index_pending {
            println!(
                "{}",
                theme::grey(
                    "Semantic search is still learning some newer sessions in the background — results will get sharper on your next search."
                )
            );
        }
        live_search(sessions, query_arg.clone().unwrap_or_default(), index_pending).await?
    };

    let target = match &resume_in_arg {
        Some(raw) => parse_agent(raw)?,
        None if non_interactive => picked.tool,
        None => pick_resume_in(picked.tool)?,
    };
    if !target.is_installed() {
        bail!(
            "Cannot resume in {}: \"{}\" is not installed or not on PATH.",
            target.slug(),
            target.binary()
        );
    }

    crate::telemetry::capture(
        "resume",
        serde_json::json!({
            "from": picked.tool.slug(),
            "to": target.slug(),
            "same_agent": target == picked.tool,
            "via": "cli",
            "interactive": !non_interactive,
            "had_query": query_arg.is_some(),
            "scoped": agent_arg.is_some(),
        }),
    );

    let mut project_path = picked.project_path.clone();
    if !Path::new(&project_path).exists() {
        let home = dirs::home_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| ".".into());
        eprintln!(
            "Original project directory no longer exists: {project_path}\nResuming in {home} instead."
        );
        project_path = home;
    }

    let session_id = if target != picked.tool {
        print!(
            "Converting {} session for {}...",
            picked.tool.slug(),
            target.slug()
        );
        stdout().flush()?;
        let id = adapters::convert_session(&picked, target, &project_path)?;
        println!(" done.");
        id
    } else {
        picked.session_id.clone()
    };

    let cmd = adapters::adapter_for(target).resume_cmd(&session_id, &project_path);
    let argv = crate::agents::spawn_argv(&cmd);
    println!("Launching: {}", argv.join(" "));
    crate::telemetry::flush().await;
    let status = crate::agents::std_command(&cmd)
        .current_dir(&project_path)
        .status()?;
    match status.code() {
        Some(0) | None => Ok(()),
        Some(code) => std::process::exit(code),
    }
}

fn stdio_is_tty() -> bool {
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

fn parse_agent(raw: &str) -> anyhow::Result<ToolName> {
    ToolName::from_slug(raw).ok_or_else(|| {
        anyhow::anyhow!(
            "Unknown agent \"{raw}\". Valid: {}",
            ToolName::ALL.iter().map(|t| t.slug()).collect::<Vec<_>>().join(", ")
        )
    })
}

fn print_banner() {
    let cols = if std::io::stdout().is_terminal() {
        terminal::size().map(|(c, _)| c).unwrap_or(0)
    } else {
        0
    };
    if std::io::stdout().is_terminal() && cols >= theme::BRAND_WORDMARK_WIDTH {
        println!(
            "{}",
            theme::brand_cyan(theme::BRAND_WORDMARK.trim_start_matches('\n'))
        );
    } else {
        println!(
            "\n  {} {}\n",
            theme::bold(&theme::brand_cyan("agent")),
            theme::bold(&theme::grey("hop"))
        );
    }
}

fn pick_scope() -> anyhow::Result<Vec<ToolName>> {
    let mut labels = vec!["All agents".to_string()];
    labels.extend(ToolName::ALL.iter().map(|t| theme::tool_tag(*t)));
    let title = "Which agent(s) to search?";
    let i = picker::pick_list(Some(title), &labels, 0)?;
    println!("{title}");
    println!("  {}\n", labels[i]);
    match i {
        0 => Ok(ToolName::ALL.to_vec()),
        i => Ok(vec![ToolName::ALL[i - 1]]),
    }
}

fn pick_resume_in(from: ToolName) -> anyhow::Result<ToolName> {
    let labels: Vec<String> = ToolName::ALL
        .iter()
        .map(|t| {
            let tag = theme::tool_tag(*t);
            if *t == from {
                format!("{tag} {}", theme::grey("(same tool — native resume)"))
            } else {
                tag
            }
        })
        .collect();
    let start = ToolName::ALL.iter().position(|t| *t == from).unwrap_or(0);
    let title = format!("Resume in which agent? (session is from {})", from.slug());
    let i = picker::pick_list(Some(&title), &labels, start)?;
    println!("{title}");
    println!("  {}\n", labels[i]);
    Ok(ToolName::ALL[i])
}

async fn live_search(
    sessions: Vec<SessionRef>,
    initial: String,
    index_pending: bool,
) -> anyhow::Result<SessionRef> {
    let ranker = Arc::new(Ranker::new(sessions));
    let mut query = initial;
    let mut selected = 0usize;
    let mut generation: u64 = 0;
    let mut semantic_pending = false;
    let mut results = ranker.rank(&query, MAX_ITEMS);
    // Query already known (typed as an argv, or we just warmed the model):
    // run stage 2 now so the first frame is hybrid, not a 200ms BM25 stall.
    if !query.trim().is_empty() {
        let (refined, _) = ranker.refine_with_semantic_status(&query, MAX_ITEMS).await;
        results = refined;
    }
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(u64, Vec<SessionRef>, bool)>();

    let mut out = stdout();
    terminal::enable_raw_mode()?;
    // Alternate screen + full redraw. In-place MoveUp floods the terminal
    // the moment a wrapped hint line makes the row count lie.
    execute!(out, terminal::EnterAlternateScreen, cursor::Hide)?;
    let _restore = TerminalRestore;

    let paint = |out: &mut std::io::Stdout,
                 query: &str,
                 results: &[SessionRef],
                 selected: usize,
                 semantic_pending: bool,
                 index_pending: bool|
     -> anyhow::Result<()> {
        queue!(
            out,
            terminal::Clear(ClearType::All),
            cursor::MoveTo(0, 0)
        )?;
        resume::write_search_frame(
            out,
            SearchFrame {
                title: SEARCH_TITLE,
                query_prefix: "",
                query,
                results,
                selected,
                searching: semantic_pending,
                index_pending,
            },
        )?;
        out.flush()?;
        Ok(())
    };

    paint(&mut out, &query, &results, selected, semantic_pending, index_pending)?;

    let picked = loop {
        while let Ok((g, refined, used_semantic)) = rx.try_recv() {
            if g == generation {
                let keep = results.get(selected).map(|s| (s.tool, s.session_id.clone()));
                results = refined;
                selected = keep
                    .and_then(|(tool, id)| {
                        results.iter().position(|r| r.tool == tool && r.session_id == id)
                    })
                    .unwrap_or(0);
                semantic_pending = false;
                let wait_on_index = index_pending || (results.is_empty() && !used_semantic);
                paint(&mut out, &query, &results, selected, semantic_pending, wait_on_index)?;
            }
        }

        if !event::poll(Duration::from_millis(50))? {
            // The loop is otherwise fully sync; without a yield the
            // refine task (tokio::spawn) can sit unpolled.
            tokio::task::yield_now().await;
            continue;
        }
        let Event::Key(key) = event::read()? else { continue };
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            continue;
        }
        match key.code {
            KeyCode::Esc => bail!("Cancelled."),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                bail!("Cancelled.");
            }
            KeyCode::Enter => {
                if let Some(r) = results.get(selected).cloned() {
                    break r;
                }
            }
            KeyCode::Up => {
                if selected > 0 {
                    selected -= 1;
                    paint(&mut out, &query, &results, selected, semantic_pending, index_pending)?;
                }
            }
            KeyCode::Down => {
                if selected + 1 < results.len() {
                    selected += 1;
                    paint(&mut out, &query, &results, selected, semantic_pending, index_pending)?;
                }
            }
            KeyCode::Backspace => {
                if query.pop().is_some() {
                    generation += 1;
                    results = ranker.rank(&query, MAX_ITEMS);
                    selected = 0;
                    semantic_pending = !query.trim().is_empty();
                    paint(&mut out, &query, &results, selected, semantic_pending, index_pending)?;
                    if semantic_pending {
                        spawn_refine(ranker.clone(), query.clone(), generation, tx.clone());
                    }
                }
            }
            KeyCode::Char(c)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                query.push(c);
                generation += 1;
                results = ranker.rank(&query, MAX_ITEMS);
                selected = 0;
                semantic_pending = true;
                paint(&mut out, &query, &results, selected, semantic_pending, index_pending)?;
                spawn_refine(ranker.clone(), query.clone(), generation, tx.clone());
            }
            _ => {}
        }
    };

    drop(_restore);
    println!("{SEARCH_TITLE}");
    println!(
        "  {} {}\n",
        theme::tool_tag(picked.tool),
        search::one_line(&picked.title)
    );
    Ok(picked)
}

/// Always leave alt-screen + raw mode, including on `bail!`.
struct TerminalRestore;
impl Drop for TerminalRestore {
    fn drop(&mut self) {
        let mut out = stdout();
        let _ = execute!(out, cursor::Show, terminal::LeaveAlternateScreen);
        let _ = terminal::disable_raw_mode();
    }
}

fn spawn_refine(
    ranker: Arc<Ranker>,
    query: String,
    generation: u64,
    tx: tokio::sync::mpsc::UnboundedSender<(u64, Vec<SessionRef>, bool)>,
) {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(STAGE2_DEBOUNCE_MS)).await;
        let (refined, used_semantic) = ranker.refine_with_semantic_status(&query, MAX_ITEMS).await;
        let _ = tx.send((generation, refined, used_semantic));
    });
}
