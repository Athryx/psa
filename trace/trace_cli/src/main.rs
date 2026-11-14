//! `trace`: import sessions from other harnesses and inspect traces.

use agent_trace::{
    AgentTrace, EventKind, SessionTrace,
    import::{self, Harness},
    render::{RenderOptions, format_duration, render_event},
    stats::AgentStats,
    store,
};
use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use regex::RegexBuilder;
use std::{
    io::Write,
    path::{Path, PathBuf},
};
use time::format_description::well_known::Rfc3339;

/// `println!` that exits quietly when stdout is closed, e.g. piped into `head`.
macro_rules! out {
    ($($arg:tt)*) => {
        write_line(format_args!($($arg)*))
    };
}

fn write_line(args: std::fmt::Arguments<'_>) {
    let mut stdout = std::io::stdout().lock();
    if let Err(error) = stdout
        .write_fmt(args)
        .and_then(|()| stdout.write_all(b"\n"))
    {
        if error.kind() == std::io::ErrorKind::BrokenPipe {
            std::process::exit(0);
        }
        eprintln!("trace: writing output: {error}");
        std::process::exit(1);
    }
}

#[derive(Parser)]
#[command(
    name = "trace",
    about = "Import, inspect, and search agent session traces"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Convert a Claude Code, Codex, or pi session into a trace directory.
    Import {
        /// Source harness: claude-code, codex, or pi.
        harness: Harness,
        /// Session log to import; defaults to the most recently modified session.
        source: Option<PathBuf>,
        /// Output directory; defaults to ./traces/<harness>-<session id>.
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Replace an existing trace in the output directory.
        #[arg(long)]
        force: bool,
    },
    /// List trace directories under a root.
    List {
        #[arg(default_value = ".")]
        root: PathBuf,
    },
    /// Session metadata, agent tree, and totals.
    Info { trace: PathBuf },
    /// Render an agent's events as text.
    Show {
        trace: PathBuf,
        /// Agent id or name; defaults to the root agent.
        #[arg(short, long)]
        agent: Option<String>,
        /// First event id to show.
        #[arg(long)]
        from: Option<u64>,
        /// Last event id to show (inclusive).
        #[arg(long)]
        to: Option<u64>,
        /// Show only the last N matching events.
        #[arg(long)]
        last: Option<usize>,
        /// Comma-separated event types to include.
        #[arg(short, long, value_delimiter = ',')]
        types: Vec<String>,
        /// Comma-separated event types to exclude.
        #[arg(short = 'x', long, value_delimiter = ',')]
        exclude: Vec<String>,
        /// Maximum characters per event body before middle truncation.
        #[arg(long, default_value_t = 2000)]
        max_chars: usize,
        /// Never truncate.
        #[arg(long)]
        full: bool,
        /// Show event timestamps.
        #[arg(long)]
        time: bool,
    },
    /// Print one event in full; ids are unique across the session.
    Event {
        trace: PathBuf,
        id: u64,
        /// Print the stored JSON instead of rendered text.
        #[arg(long)]
        json: bool,
    },
    /// Search event text with a regular expression.
    Grep {
        trace: PathBuf,
        pattern: String,
        #[arg(short, long)]
        agent: Option<String>,
        #[arg(short, long, value_delimiter = ',')]
        types: Vec<String>,
        #[arg(short = 'i', long)]
        ignore_case: bool,
        /// Characters of context around each match.
        #[arg(long, default_value_t = 80)]
        context: usize,
        /// Stop after this many matching events.
        #[arg(long, default_value_t = 200)]
        limit: usize,
    },
    /// Tool definitions with call and error counts.
    Tools {
        trace: PathBuf,
        /// Show one tool's full definition.
        #[arg(short, long)]
        name: Option<String>,
    },
    /// Token usage, cost, finishes, and tool counts per agent.
    Stats { trace: PathBuf },
}

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Import {
            harness,
            source,
            output,
            force,
        } => run_import(harness, source, output, force).await,
        Command::List { root } => run_list(&root).await,
        Command::Info { trace } => run_info(&load(&trace).await?),
        Command::Show {
            trace,
            agent,
            from,
            to,
            last,
            types,
            exclude,
            max_chars,
            full,
            time,
        } => {
            let trace = load(&trace).await?;
            let agent = select_agent(&trace, agent.as_deref())?;
            validate_types(types.iter().chain(&exclude))?;
            let options = RenderOptions {
                max_chars: (!full).then_some(max_chars),
                show_time: time,
            };
            let mut selected: Vec<_> = agent
                .events
                .iter()
                .filter(|event| from.is_none_or(|from| event.id >= from))
                .filter(|event| to.is_none_or(|to| event.id <= to))
                .filter(|event| matches_types(&types, event.kind.type_name()))
                .filter(|event| !exclude.iter().any(|t| t == event.kind.type_name()))
                .collect();
            if let Some(last) = last {
                selected.drain(..selected.len().saturating_sub(last));
            }
            for event in selected {
                out!("{}", render_event(event, options));
            }
            Ok(())
        }
        Command::Event { trace, id, json } => {
            let trace = load(&trace).await?;
            let (agent, event) = trace
                .find_event(id)
                .with_context(|| format!("no event with id {id}"))?;
            if json {
                out!("{}", serde_json::to_string_pretty(event)?);
            } else {
                let options = RenderOptions {
                    max_chars: None,
                    show_time: true,
                };
                out!("agent {}", agent.meta.id);
                out!("{}", render_event(event, options));
            }
            Ok(())
        }
        Command::Grep {
            trace,
            pattern,
            agent,
            types,
            ignore_case,
            context,
            limit,
        } => {
            let trace = load(&trace).await?;
            validate_types(&types)?;
            let regex = RegexBuilder::new(&pattern)
                .case_insensitive(ignore_case)
                .build()?;
            let agents: Vec<&AgentTrace> = match agent {
                Some(agent) => vec![select_agent(&trace, Some(&agent))?],
                None => trace.agents.iter().collect(),
            };
            run_grep(&agents, &regex, &types, context, limit);
            Ok(())
        }
        Command::Tools { trace, name } => run_tools(&load(&trace).await?, name.as_deref()),
        Command::Stats { trace } => {
            run_stats(&load(&trace).await?);
            Ok(())
        }
    }
}

async fn run_import(
    harness: Harness,
    source: Option<PathBuf>,
    output: Option<PathBuf>,
    force: bool,
) -> Result<()> {
    let source = match source {
        Some(source) => source,
        None => import::find_latest_session(harness).await?,
    };
    let mut trace = import::import_session(harness, &source)
        .await
        .with_context(|| format!("importing {}", source.display()))?;
    let output = output.unwrap_or_else(|| {
        let id: String = trace
            .meta
            .id
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
            .collect();
        PathBuf::from("traces").join(format!("{}-{id}", harness.as_str()))
    });
    trace.save(&output, force).await?;

    let events: usize = trace.agents.iter().map(|agent| agent.events.len()).sum();
    out!(
        "Imported {} ({} agents, {events} events, {} images) to {}",
        source.display(),
        trace.agents.len(),
        trace.blobs.len(),
        output.display()
    );
    Ok(())
}

async fn run_list(root: &Path) -> Result<()> {
    let mut dirs = Vec::new();
    let mut pending = vec![(root.to_owned(), 0)];
    while let Some((dir, depth)) = pending.pop() {
        if store::is_trace_dir(&dir).await {
            dirs.push(dir);
            continue;
        }
        if depth >= 4 {
            continue;
        }
        let Ok(mut entries) = tokio::fs::read_dir(&dir).await else {
            continue;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            if entry.file_type().await.is_ok_and(|kind| kind.is_dir()) {
                pending.push((entry.path(), depth + 1));
            }
        }
    }
    dirs.sort();

    for dir in dirs {
        let trace = match SessionTrace::load(&dir).await {
            Ok(trace) => trace,
            Err(error) => {
                out!("{}  (unreadable: {error})", dir.display());
                continue;
            }
        };
        let events: usize = trace.agents.iter().map(|agent| agent.events.len()).sum();
        let started = trace
            .meta
            .started_at
            .map(|t| t.date().to_string())
            .unwrap_or_else(|| "-".into());
        out!(
            "{}  {}  {started}  agents={} events={}  {}",
            dir.display(),
            trace.meta.source.harness,
            trace.agents.len(),
            events,
            session_label(&trace)
        );
    }
    Ok(())
}

fn run_info(trace: &SessionTrace) -> Result<()> {
    let meta = &trace.meta;
    out!("id:       {}", meta.id);
    out!("title:    {}", session_label(trace));
    match &meta.source.version {
        Some(version) => out!("source:   {} {version}", meta.source.harness),
        None => out!("source:   {}", meta.source.harness),
    }
    if let Some(path) = &meta.source.path {
        out!("from:     {path}");
    }
    if let Some(cwd) = &meta.cwd {
        out!("cwd:      {cwd}");
    }
    if let (Some(start), Some(end)) = (meta.started_at, meta.ended_at) {
        let ms = (end - start).whole_milliseconds().max(0) as u64;
        let start = start.format(&Rfc3339).unwrap_or_default();
        out!("time:     {start} ({})", format_duration(ms));
    }

    out!("\nagents:");
    print_agent_tree(trace, None, 0);

    let mut total = AgentStats::default();
    for agent in &trace.agents {
        total.merge(&AgentStats::compute(agent));
    }
    out!("\ntotals:   {}", format_usage(&total));
    let calls: usize = total.tools.values().map(|tool| tool.calls).sum();
    let errors: usize = total.tools.values().map(|tool| tool.errors).sum();
    out!(
        "tools:    {calls} calls, {errors} errors, {} defined",
        meta.tools.len()
    );
    Ok(())
}

fn print_agent_tree(trace: &SessionTrace, parent: Option<&str>, depth: usize) {
    let known = |id: &str| trace.agents.iter().any(|agent| agent.meta.id == id);
    for agent in &trace.agents {
        // Agents whose parent is missing from the trace are shown at the top level.
        let agent_parent = agent.meta.parent.as_deref().filter(|id| known(id));
        if agent_parent != parent {
            continue;
        }
        let meta = &agent.meta;
        let mut line = format!("{}- {}", "  ".repeat(depth), meta.id);
        if meta.name != meta.id {
            line.push_str(&format!(" \"{}\"", meta.name));
        }
        if let Some(role) = &meta.role {
            line.push_str(&format!(" [{role}]"));
        }
        if let Some(model) = &meta.model {
            line.push_str(&format!(" {model}"));
        }
        line.push_str(&format!(" events={}", agent.events.len()));
        if let Some(spawn) = &meta.spawned_by {
            line.push_str(&format!(" spawned_by=#{spawn}"));
        }
        out!("{line}");
        print_agent_tree(trace, Some(&meta.id), depth + 1);
    }
}

fn run_grep(
    agents: &[&AgentTrace],
    regex: &regex::Regex,
    types: &[String],
    context: usize,
    limit: usize,
) {
    let mut matches = 0;
    for agent in agents {
        for event in &agent.events {
            if !matches_types(types, event.kind.type_name()) {
                continue;
            }
            let text = searchable_text(&event.kind);
            let Some(found) = regex.find(&text) else {
                continue;
            };
            let start = floor_char_boundary(&text, found.start().saturating_sub(context));
            let end = ceil_char_boundary(&text, (found.end() + context).min(text.len()));
            let snippet = text[start..end].replace('\n', " ");
            let count = regex.find_iter(&text).count();
            let more = if count > 1 {
                format!(" (+{} more)", count - 1)
            } else {
                String::new()
            };
            out!(
                "{}#{} {}{more}: {snippet}",
                agent.meta.id,
                event.id,
                event.kind.type_name()
            );

            matches += 1;
            if matches >= limit {
                out!("... stopped after {limit} matching events (use --limit)");
                return;
            }
        }
    }
}

/// Text a search should see: bodies plus names and identifiers.
fn searchable_text(kind: &EventKind) -> String {
    match kind {
        EventKind::System { text }
        | EventKind::Assistant { text }
        | EventKind::Thinking { text }
        | EventKind::User { text, .. } => text.clone(),
        EventKind::ToolCall {
            name, args, result, ..
        } => match result {
            Some(result) => format!("{name} {args}\n{}", result.output),
            None => format!("{name} {args}"),
        },
        EventKind::Context { kind, text } => format!("{kind} {text}"),
        EventKind::AgentMessage { from, to, text } => format!("{from} -> {to} {text}"),
        EventKind::Compaction { summary, .. } => summary.clone(),
        EventKind::Inference(inference) => {
            let model = inference.model.as_deref().unwrap_or_default();
            let error = inference.error.as_deref().unwrap_or_default();
            format!("{model} {error}")
        }
        EventKind::ModelChange { model, effort } => {
            format!(
                "{} {}",
                model.as_deref().unwrap_or_default(),
                effort.as_deref().unwrap_or_default()
            )
        }
        EventKind::AgentFinish { reason, .. } => reason.clone().unwrap_or_default(),
    }
}

fn run_tools(trace: &SessionTrace, name: Option<&str>) -> Result<()> {
    let mut total = AgentStats::default();
    for agent in &trace.agents {
        total.merge(&AgentStats::compute(agent));
    }

    if let Some(name) = name {
        let definition = trace
            .meta
            .tools
            .get(name)
            .with_context(|| format!("no tool named {name:?}"))?;
        out!("{name}");
        if let Some(description) = &definition.description {
            out!("\n{description}");
        }
        if let Some(schema) = &definition.input_schema {
            out!("\ninput schema:\n{}", serde_json::to_string_pretty(schema)?);
        }
        if let Some(format) = &definition.input_format {
            out!("\ninput format:\n{}", serde_json::to_string_pretty(format)?);
        }
        let agents: Vec<_> = trace
            .agents
            .iter()
            .filter(|agent| agent.meta.tools.iter().any(|tool| tool == name))
            .map(|agent| agent.meta.id.as_str())
            .collect();
        out!("\nagents: {}", agents.join(", "));
        return Ok(());
    }

    let mut names: Vec<&String> = trace.meta.tools.keys().collect();
    for used in total.tools.keys() {
        if !trace.meta.tools.contains_key(used) {
            names.push(used);
        }
    }
    for name in names {
        let usage = total.tools.get(name).cloned().unwrap_or_default();
        let description = trace
            .meta
            .tools
            .get(name)
            .and_then(|definition| definition.description.as_deref())
            .and_then(|description| description.lines().next())
            .unwrap_or("");
        let description = truncate_end(description, 100);
        out!(
            "{name}  calls={} errors={}  {description}",
            usage.calls,
            usage.errors
        );
    }
    Ok(())
}

fn run_stats(trace: &SessionTrace) {
    let mut total = AgentStats::default();
    for agent in &trace.agents {
        let stats = AgentStats::compute(agent);
        out!("{}", agent.meta.id);
        print_stats(&stats);
        total.merge(&stats);
    }
    if trace.agents.len() > 1 {
        out!("total");
        print_stats(&total);
    }
}

fn print_stats(stats: &AgentStats) {
    out!("  {}", format_usage(stats));
    out!(
        "  events={} user={} assistant={} agent_messages={} finishes={} (not completed {}) compactions={}",
        stats.events,
        stats.user_messages,
        stats.assistant_messages,
        stats.agent_messages,
        stats.finishes,
        stats.aborted_finishes,
        stats.compactions
    );
    let mut tools: Vec<_> = stats.tools.iter().collect();
    tools.sort_by(|a, b| b.1.calls.cmp(&a.1.calls).then_with(|| a.0.cmp(b.0)));
    for (name, usage) in tools {
        let mut line = format!(
            "  tool {name}: calls={} errors={}",
            usage.calls, usage.errors
        );
        if usage.total_ms > 0 {
            line.push_str(&format!(" time={}", format_duration(usage.total_ms)));
        }
        out!("{line}");
    }
}

fn format_usage(stats: &AgentStats) -> String {
    let usage = &stats.usage;
    let mut line = format!(
        "inferences={} input={} (cached {}, cache writes {}) output={} (reasoning {})",
        usage.inferences,
        usage.input_tokens,
        usage.cached_input_tokens,
        usage.cache_write_tokens,
        usage.output_tokens,
        usage.reasoning_tokens
    );
    if let Some(cost) = usage.cost_usd {
        line.push_str(&format!(" cost=${cost:.4}"));
    }
    if usage.failed_inferences > 0 {
        line.push_str(&format!(" failed={}", usage.failed_inferences));
    }
    line
}

/// Title, or the first user message when the source had no title.
fn session_label(trace: &SessionTrace) -> String {
    if let Some(title) = &trace.meta.title {
        return title.clone();
    }
    let first_prompt = trace.root_agent().and_then(|agent| {
        agent.events.iter().find_map(|event| match &event.kind {
            EventKind::User { text, .. } => Some(text.as_str()),
            _ => None,
        })
    });
    let prompt = first_prompt.unwrap_or("(untitled)").trim();
    let line = prompt.lines().next().unwrap_or_default();
    truncate_end(line, 80)
}

async fn load(dir: &Path) -> Result<SessionTrace> {
    SessionTrace::load(dir)
        .await
        .with_context(|| format!("loading trace {}", dir.display()))
}

fn select_agent<'a>(trace: &'a SessionTrace, selector: Option<&str>) -> Result<&'a AgentTrace> {
    let Some(selector) = selector else {
        return trace.root_agent().context("trace has no agents");
    };
    trace
        .agents
        .iter()
        .find(|agent| agent.meta.id == selector)
        .or_else(|| {
            trace
                .agents
                .iter()
                .find(|agent| agent.meta.name == selector)
        })
        .with_context(|| {
            let ids: Vec<_> = trace
                .agents
                .iter()
                .map(|agent| agent.meta.id.as_str())
                .collect();
            format!("no agent {selector:?}; agents: {}", ids.join(", "))
        })
}

fn truncate_end(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(max_chars.saturating_sub(3)).collect();
    out.push_str("...");
    out
}

fn matches_types(types: &[String], name: &str) -> bool {
    types.is_empty() || types.iter().any(|t| t == name)
}

fn validate_types<'a>(types: impl IntoIterator<Item = &'a String>) -> Result<()> {
    for name in types {
        if !EventKind::TYPE_NAMES.contains(&name.as_str()) {
            bail!(
                "unknown event type {name:?}; expected one of {}",
                EventKind::TYPE_NAMES.join(", ")
            );
        }
    }
    Ok(())
}

fn floor_char_boundary(text: &str, mut index: usize) -> usize {
    while !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn ceil_char_boundary(text: &str, mut index: usize) -> usize {
    while !text.is_char_boundary(index) {
        index += 1;
    }
    index
}
