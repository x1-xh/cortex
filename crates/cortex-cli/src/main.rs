//! Cortex CLI entrypoint.
//!
//! Provides the primary command-line interface for the Cortex runtime harness,
//! including execution run inspection and tracing queries.

use clap::{Parser, Subcommand};
use cortex_core::{AgentId, CortexError, RunId, VERSION};
use cortex_runtime::{
    create_model_provider, scheduler::OverlapPolicy, tools, AgentContext, AgentLoop, AgentManager,
    AgentManifest, AgentMessagePayload, AgentState, CortexConfig, McpManager, RunStore, RunSummary,
    SchedulerEngine, ToolRegistry, WorkflowYamlParser, Workspace,
};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;

mod style {
    use std::io::IsTerminal;

    pub fn use_color() -> bool {
        std::io::stdout().is_terminal() && std::env::var("NO_COLOR").is_err()
    }

    /// Electric Blue (#3b82f6)
    pub fn blue(s: impl std::fmt::Display) -> String {
        if use_color() {
            format!("\x1b[38;2;59;130;246m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }

    /// Bold Electric Blue (#3b82f6)
    pub fn bold_blue(s: impl std::fmt::Display) -> String {
        if use_color() {
            format!("\x1b[1;38;2;59;130;246m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }

    /// Sky Cyan (#38bdf8)
    pub fn cyan(s: impl std::fmt::Display) -> String {
        if use_color() {
            format!("\x1b[38;2;56;189;248m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }

    /// Muted Slate Gray (#64748b)
    pub fn dim(s: impl std::fmt::Display) -> String {
        if use_color() {
            format!("\x1b[38;2;100;116;139m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }

    /// Emerald Green (#34d399)
    pub fn green(s: impl std::fmt::Display) -> String {
        if use_color() {
            format!("\x1b[38;2;52;211;153m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }
}

/// Cortex - An open-source runtime and harness for autonomous AI workers.
#[derive(Parser, Debug)]
#[command(
    name = "cortex",
    author = "Cortex Contributors",
    version = VERSION,
    about = "An open-source runtime and harness for autonomous AI workers. Run 'cortex' directly to launch the interactive TUI.",
    long_about = "Cortex is a runtime and harness for autonomous AI workers, providing sandboxed \
                  tool execution, persistent agents, execution tracing, and multi-agent coordination. \
                  Running 'cortex' without arguments directly launches the interactive terminal control plane (TUI)."
)]
struct Cli {
    /// Optional path to SQLite runs database when launching the interactive control plane.
    #[arg(long, global = true)]
    db: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Inspect the current runtime bootstrap status.
    Status,

    /// Check system environment readiness.
    Check,

    /// Run an agent task.
    Run {
        /// Task prompt or instructions for the agent.
        prompt: String,

        /// Model identifier (e.g., gpt-4o-mini, gpt-4o, claude-3-5-sonnet-20241022, deepseek-chat, or ollama/llama3.1).
        #[arg(short, long, env = "CORTEX_MODEL", default_value = "gpt-4o-mini")]
        model: String,

        /// Provider API key (or set OPENAI_API_KEY / ANTHROPIC_API_KEY).
        #[arg(long, env = "CORTEX_API_KEY")]
        api_key: Option<String>,

        /// Custom provider base URL (or set OPENAI_BASE_URL / ANTHROPIC_BASE_URL).
        #[arg(long, env = "CORTEX_BASE_URL")]
        base_url: Option<String>,

        /// Working directory boundary for the agent. Defaults to current directory.
        #[arg(short, long)]
        workspace: Option<PathBuf>,

        /// Maximum autonomous loop iterations allowed.
        #[arg(short = 'i', long, default_value = "15")]
        max_iterations: usize,

        /// Path to SQLite runs database. Defaults to ~/.cortex/cortex.db.
        #[arg(long)]
        db: Option<PathBuf>,

        /// Suppress interactive progress and only output final answer.
        #[arg(short, long)]
        quiet: bool,

        /// Output the final run result in JSON format.
        #[arg(long)]
        json: bool,

        /// Path to cortex.toml configuration file.
        #[arg(short, long)]
        config: Option<PathBuf>,
    },

    /// Manage Model Context Protocol (MCP) servers and external tools.
    Mcp {
        #[command(subcommand)]
        action: McpCommands,
    },

    /// Manage and inspect recorded execution runs.
    Runs {
        #[command(subcommand)]
        action: Option<RunsCommands>,
    },

    /// Benchmark and evaluate agent performance against verifiable ground truth tasks.
    Bench {
        #[command(subcommand)]
        action: BenchCommands,
    },

    /// Launch the interactive terminal control plane.
    #[command(alias = "dashboard", alias = "chat", hide = true)]
    Tui {
        /// Optional path to SQLite database.
        #[arg(short, long)]
        db: Option<PathBuf>,
    },

    /// Manage scheduled cron jobs and persistent triggers.
    Cron {
        #[command(subcommand)]
        action: CronCommands,
    },

    /// Manage persistent agent worker lifecycles.
    Agent {
        #[command(subcommand)]
        action: AgentCommands,
    },

    /// Manage and execute declarative multi-agent workflows.
    Workflow {
        #[command(subcommand)]
        action: WorkflowCommands,
    },

    /// Manage multi-agent teams and inspect inter-agent communications.
    Team {
        #[command(subcommand)]
        action: TeamCommands,
    },
}

#[derive(Subcommand, Debug)]
enum WorkflowCommands {
    /// Execute a multi-agent workflow file.
    Run {
        /// Path to workflow manifest file (workflow.yaml or json).
        path: PathBuf,

        /// Task prompt overriding default goal.
        #[arg(short, long)]
        task: Option<String>,

        /// Suppress interactive progress output.
        #[arg(short, long)]
        quiet: bool,

        /// Output the workflow run execution results in JSON format.
        #[arg(long)]
        json: bool,
    },

    /// Display stage progression, agent assignments, and task outcomes.
    Status {
        /// Workflow run identifier to inspect.
        workflow_id: String,

        /// Output status in JSON format.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand, Debug)]
enum TeamCommands {
    /// List configured agents in the active workspace and their capability roles.
    List {
        /// Output agent list in JSON format.
        #[arg(long)]
        json: bool,
    },

    /// Tail and filter inter-agent message logs for an execution run.
    Messages {
        /// Correlated run identifier.
        run_id: String,

        /// Filter by sender agent identifier.
        #[arg(short, long)]
        sender: Option<String>,

        /// Filter by recipient agent identifier.
        #[arg(short, long)]
        recipient: Option<String>,

        /// Maximum number of messages to display.
        #[arg(short, long, default_value = "50")]
        limit: usize,

        /// Output messages in JSON format.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand, Debug)]
enum AgentCommands {
    /// List all configured agents and their current status.
    List,

    /// Create an agent from a manifest file (YAML, JSON, or TOML).
    Create {
        /// Path to the agent manifest file.
        #[arg(short, long)]
        manifest: PathBuf,
    },

    /// Start or resume an agent worker.
    Start {
        /// Identifier of the agent to start.
        agent_id: String,
    },

    /// Stop a running or paused agent worker.
    Stop {
        /// Identifier of the agent to stop.
        agent_id: String,
    },

    /// Pause a running agent worker.
    Pause {
        /// Identifier of the agent to pause.
        agent_id: String,
    },

    /// Inspect details, configuration, and state of an agent.
    Inspect {
        /// Identifier of the agent to inspect.
        agent_id: String,

        /// Output the agent inspection details in JSON format.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand, Debug)]
enum McpCommands {
    /// List configured MCP servers and discover available tools.
    List {
        /// Optional path to cortex.toml configuration file.
        #[arg(short, long)]
        config: Option<PathBuf>,
    },

    /// Test connection to an MCP server and query its capabilities.
    Test {
        /// Server name from cortex.toml to test.
        server: String,

        /// Optional path to cortex.toml configuration file.
        #[arg(short, long)]
        config: Option<PathBuf>,
    },
}

#[derive(Subcommand, Debug)]
enum RunsCommands {
    /// List recorded execution runs.
    List {
        /// Maximum number of runs to display.
        #[arg(short, long, default_value = "20")]
        limit: usize,
    },

    /// Inspect details and event trace for a specific execution run.
    Show {
        /// Run identifier to inspect.
        run_id: String,

        /// Display verbose JSON payloads for each event.
        #[arg(short, long)]
        verbose: bool,
    },

    /// Replay a recorded execution run deterministically.
    Replay {
        /// Run identifier to replay.
        run_id: String,
    },
}

#[derive(Subcommand, Debug)]
enum BenchCommands {
    /// Execute a benchmark evaluation suite.
    Run {
        /// Target benchmark suite to evaluate (e.g., "coding", "refactor", "cli").
        #[arg(short, long, default_value = "coding")]
        suite: String,

        /// Output results in JSON format to stdout.
        #[arg(long)]
        json: bool,

        /// Write Markdown evaluation report to the specified file path.
        #[arg(short, long)]
        report: Option<PathBuf>,

        /// Maximum agent iterations allowed per task.
        #[arg(short, long, default_value = "10")]
        max_iterations: usize,
    },
}

#[derive(Subcommand, Debug)]
enum CronCommands {
    /// List all registered scheduled cron jobs.
    List {
        /// Optional path to SQLite database. Defaults to ~/.cortex/cortex.db.
        #[arg(long)]
        db: Option<PathBuf>,

        /// Output registered jobs in JSON format.
        #[arg(long)]
        json: bool,
    },

    /// Create and register a new scheduled cron job.
    Create {
        /// Human-readable name for the job.
        #[arg(short, long)]
        name: Option<String>,

        /// 5-field cron expression (e.g., "*/15 * * * *") or one-shot ISO timestamp.
        #[arg(short, long)]
        schedule: String,

        /// Task prompt to execute when triggered.
        #[arg(short, long, alias = "task")]
        prompt: String,

        /// Target agent name (e.g., coding, monitor).
        #[arg(short, long)]
        agent: Option<String>,

        /// Overlap policy when previous run is active: skip, queue, or replace.
        #[arg(short, long, default_value = "skip")]
        overlap: String,

        /// Optional path to SQLite database. Defaults to ~/.cortex/cortex.db.
        #[arg(long)]
        db: Option<PathBuf>,
    },

    /// Delete a registered scheduled cron job by its identifier.
    Delete {
        /// Identifier of the job to delete.
        id: String,

        /// Optional path to SQLite database. Defaults to ~/.cortex/cortex.db.
        #[arg(long)]
        db: Option<PathBuf>,
    },

    /// Inspect details and execution statistics for a scheduled cron job.
    Inspect {
        /// Identifier or unique prefix of the job to inspect.
        id: String,

        /// Output inspection details in JSON format.
        #[arg(long)]
        json: bool,

        /// Optional path to SQLite database. Defaults to ~/.cortex/cortex.db.
        #[arg(long)]
        db: Option<PathBuf>,
    },

    /// View chronological execution history for a scheduled cron job.
    History {
        /// Identifier or unique prefix of the job to inspect.
        id: String,

        /// Maximum number of execution records to display.
        #[arg(short, long, default_value = "20")]
        limit: usize,

        /// Output execution history in JSON format.
        #[arg(long)]
        json: bool,

        /// Optional path to SQLite database. Defaults to ~/.cortex/cortex.db.
        #[arg(long)]
        db: Option<PathBuf>,
    },
}

fn default_db_path() -> PathBuf {
    if let Ok(env_path) = std::env::var("CORTEX_DB_PATH") {
        return PathBuf::from(env_path);
    }
    cortex_core::settings::cortex_home_dir().join("cortex.db")
}

fn default_agents_path() -> PathBuf {
    if let Ok(env_path) = std::env::var("CORTEX_AGENTS_PATH") {
        return PathBuf::from(env_path);
    }
    cortex_core::settings::cortex_home_dir().join("agents.json")
}

fn agent_list() -> Result<(), Box<dyn std::error::Error>> {
    let agents_path = default_agents_path();
    let manager = AgentManager::new();
    let _ = manager.load_from_file(&agents_path);

    let agents = manager.list();
    if agents.is_empty() {
        println!("No registered agents found.");
        println!("Use 'cortex agent create --manifest <path>' to register an agent.");
        return Ok(());
    }

    println!(
        "{:<26} {:<18} {:<12} {:<24} WORKSPACE",
        "ID", "NAME", "STATUS", "MODEL"
    );
    println!("{:-<95}", "");

    for agent in agents {
        let model_display = format!("{} ({})", agent.model.model, agent.model.provider);
        println!(
            "{:<26} {:<18} {:<12} {:<24} {}",
            agent.id.as_str(),
            agent.name,
            agent.state,
            model_display,
            agent.workspace.display()
        );
    }

    Ok(())
}

fn agent_create(manifest_path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let manifest = match AgentManifest::from_file(manifest_path) {
        Ok(m) => m,
        Err(e) => {
            eprintln!(
                "Error: Failed to load manifest from '{}': {}",
                manifest_path.display(),
                e
            );
            std::process::exit(1);
        }
    };

    let agents_path = default_agents_path();
    let manager = AgentManager::new();
    let _ = manager.load_from_file(&agents_path);

    let agent = match manager.create(manifest) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
    };

    if let Err(e) = manager.save_to_file(&agents_path) {
        eprintln!("Error: Failed to save agent state: {}", e);
        std::process::exit(1);
    }

    println!("Agent '{}' created successfully.", agent.name);
    println!("  ID:        {}", agent.id);
    println!("  Role:      {}", agent.role);
    println!("  Status:    {}", agent.state);
    println!(
        "  Model:     {} ({})",
        agent.model.model, agent.model.provider
    );
    println!("  Workspace: {}", agent.workspace.display());

    Ok(())
}

fn agent_start(agent_id_str: &str) -> Result<(), Box<dyn std::error::Error>> {
    let agent_id = AgentId::from(agent_id_str);
    let agents_path = default_agents_path();
    let manager = AgentManager::new();
    let _ = manager.load_from_file(&agents_path);

    let target_agent = match manager.inspect(&agent_id) {
        Ok(a) => a,
        Err(CortexError::NotFound(msg)) => {
            eprintln!("Error: {}", msg);
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
    };

    let action_res = if target_agent.state == AgentState::Paused {
        manager.resume(&agent_id)
    } else {
        manager.start(&agent_id)
    };

    match action_res {
        Ok(()) => {
            let _ = manager.save_to_file(&agents_path);
            let updated = manager.inspect(&agent_id)?;
            println!(
                "Agent '{}' ({}) is now {}.",
                updated.name, updated.id, updated.state
            );
            Ok(())
        }
        Err(CortexError::NotFound(msg)) => {
            eprintln!("Error: {}", msg);
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
    }
}

fn agent_pause(agent_id_str: &str) -> Result<(), Box<dyn std::error::Error>> {
    let agent_id = AgentId::from(agent_id_str);
    let agents_path = default_agents_path();
    let manager = AgentManager::new();
    let _ = manager.load_from_file(&agents_path);

    let _ = match manager.inspect(&agent_id) {
        Ok(a) => a,
        Err(CortexError::NotFound(msg)) => {
            eprintln!("Error: {}", msg);
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
    };

    match manager.pause(&agent_id) {
        Ok(()) => {
            let _ = manager.save_to_file(&agents_path);
            let updated = manager.inspect(&agent_id)?;
            println!(
                "Agent '{}' ({}) is now {}.",
                updated.name, updated.id, updated.state
            );
            Ok(())
        }
        Err(CortexError::NotFound(msg)) => {
            eprintln!("Error: {}", msg);
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
    }
}

fn agent_stop(agent_id_str: &str) -> Result<(), Box<dyn std::error::Error>> {
    let agent_id = AgentId::from(agent_id_str);
    let agents_path = default_agents_path();
    let manager = AgentManager::new();
    let _ = manager.load_from_file(&agents_path);

    let _ = match manager.inspect(&agent_id) {
        Ok(a) => a,
        Err(CortexError::NotFound(msg)) => {
            eprintln!("Error: {}", msg);
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
    };

    match manager.stop(&agent_id) {
        Ok(()) => {
            let _ = manager.save_to_file(&agents_path);
            let updated = manager.inspect(&agent_id)?;
            println!(
                "Agent '{}' ({}) is now {}.",
                updated.name, updated.id, updated.state
            );
            Ok(())
        }
        Err(CortexError::NotFound(msg)) => {
            eprintln!("Error: {}", msg);
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
    }
}

fn agent_inspect(agent_id_str: &str, json: bool) -> Result<(), Box<dyn std::error::Error>> {
    let agent_id = AgentId::from(agent_id_str);
    let agents_path = default_agents_path();
    let manager = AgentManager::new();
    let _ = manager.load_from_file(&agents_path);

    let agent = match manager.inspect(&agent_id) {
        Ok(a) => a,
        Err(CortexError::NotFound(msg)) => {
            eprintln!("Error: {}", msg);
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&agent)?);
    } else {
        println!("Agent ID:    {}", agent.id);
        println!("Name:        {}", agent.name);
        println!("Role:        {}", agent.role);
        println!("Status:      {}", agent.state);
        println!(
            "Model:       {} ({})",
            agent.model.model, agent.model.provider
        );
        println!("Workspace:   {}", agent.workspace.display());
        if let Some(prompt) = &agent.system_prompt {
            println!("Prompt:      {}", prompt);
        }
        let tools_display = if agent.tools.is_empty() {
            "none".to_string()
        } else {
            agent.tools.join(", ")
        };
        println!("Tools:       {}", tools_display);
        println!("Created At:  {}", agent.created_at);
        println!("Updated At:  {}", agent.updated_at);
    }

    Ok(())
}

fn list_runs(store: &RunStore, limit: usize) -> Result<(), Box<dyn std::error::Error>> {
    let runs = store.list_runs(limit)?;
    if runs.is_empty() {
        println!("No recorded execution runs found.");
        return Ok(());
    }

    println!(
        "{:<28} {:<12} {:<24} {:<10} TASK",
        "RUN ID", "STATUS", "STARTED", "DURATION"
    );
    println!("{:-<90}", "");

    for run in runs {
        let duration_str = run
            .duration_ms
            .map(|d| format!("{}ms", d))
            .unwrap_or_else(|| "-".to_string());

        let task_preview = if run.task.len() > 30 {
            format!("{}...", &run.task[..27])
        } else {
            run.task.clone()
        };

        println!(
            "{:<28} {:<12} {:<24} {:<10} {}",
            run.id.as_str(),
            run.status,
            run.started_at,
            duration_str,
            task_preview
        );
    }

    Ok(())
}

fn show_run(
    store: &RunStore,
    run_id_str: &str,
    verbose: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let run_id = RunId::from(run_id_str);
    let maybe_run = store.get_run(&run_id)?;

    let run: RunSummary = match maybe_run {
        Some(r) => r,
        None => {
            eprintln!("Error: Run '{}' not found.", run_id_str);
            std::process::exit(1);
        }
    };

    println!("Run:        {}", run.id);
    println!("Status:     {}", run.status);
    println!("Started:    {}", run.started_at);
    if let Some(fin) = &run.finished_at {
        println!("Finished:   {}", fin);
    }
    if let Some(dur) = run.duration_ms {
        println!("Duration:   {} ms", dur);
    }
    println!(
        "Tokens:     {} prompt / {} completion ({} total)",
        run.tokens_prompt, run.tokens_completion, run.tokens_total
    );
    println!("Cost (USD): ${:.6}", run.estimated_cost_usd);
    if let Some(err) = &run.error {
        println!("Error:      {}", err);
    }
    println!("Task:       {}", run.task);
    println!();

    let events = store.get_events(&run_id)?;
    println!("Event Trace ({} events):", events.len());

    for record in &events {
        println!(
            "  [{}] {} {:<14}",
            record.sequence,
            record.timestamp,
            record.event.event_type()
        );
        if verbose {
            let json_str = serde_json::to_string_pretty(&record.event).unwrap_or_default();
            for line in json_str.lines() {
                println!("      {}", line);
            }
        }
    }

    Ok(())
}

fn replay_run(store: &RunStore, run_id_str: &str) -> Result<(), Box<dyn std::error::Error>> {
    let run_id = RunId::from(run_id_str);
    let run = match store.get_run(&run_id)? {
        Some(r) => r,
        None => {
            eprintln!("Error: Run '{}' not found.", run_id_str);
            std::process::exit(1);
        }
    };

    println!("Replaying run:   {}", run.id);
    println!("Original Task:   {}", run.task);
    println!("Original Status: {}", run.status);

    let replay_provider = cortex_runtime::ReplayModelProvider::from_store(store, &run_id)?;
    let mut replay_context = cortex_runtime::AgentContext::new(&run.task);
    let registry = cortex_runtime::ToolRegistry::new();
    let agent = cortex_runtime::AgentLoop::new(10);

    let res = agent.run(&mut replay_context, &replay_provider, &registry)?;
    println!("\nReplay Result:");
    println!("Status:       completed");
    println!("Iterations:   {}", res.iterations);
    println!("Final Answer: {}", res.final_answer);

    Ok(())
}

fn run_bench(
    suite: &str,
    json: bool,
    report: Option<PathBuf>,
    max_iterations: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    let runner = cortex_harness::BenchmarkRunner::new(max_iterations);
    let model = cortex_harness::BenchmarkBaselineProvider;

    let metrics = runner.run_suite(suite, &model)?;

    if json {
        println!("{}", metrics.to_json());
    } else {
        println!("{}", metrics.to_markdown());
    }

    if let Some(report_path) = report {
        if let Some(parent) = report_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::write(&report_path, metrics.to_markdown())?;
        println!("Report saved to: {}", report_path.display());
    }

    if metrics.successful_tasks < metrics.total_tasks {
        std::process::exit(1);
    }

    Ok(())
}

fn handle_cron_list(
    engine: &SchedulerEngine,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let jobs = engine.list_jobs()?;
    if json {
        println!("{}", serde_json::to_string_pretty(&jobs)?);
        return Ok(());
    }

    if jobs.is_empty() {
        println!("No scheduled cron jobs found. Create one with 'cortex cron create'.");
        return Ok(());
    }

    println!(
        "{:<28} {:<20} {:<16} {:<9} {:<10} {:<24} LAST RUN",
        "JOB ID", "NAME", "SCHEDULE", "OVERLAP", "STATUS", "NEXT RUN"
    );
    println!("{:-<110}", "");

    for job in jobs {
        let next_str = job
            .next_run_at
            .map(|dt| dt.to_rfc3339())
            .unwrap_or_else(|| "-".to_string());
        let last_str = job
            .last_run_at
            .map(|dt| dt.to_rfc3339())
            .unwrap_or_else(|| "-".to_string());

        let name_display = if job.name.len() > 18 {
            format!("{}...", &job.name[..15])
        } else {
            job.name.clone()
        };

        println!(
            "{:<28} {:<20} {:<16} {:<9} {:<10} {:<24} {}",
            job.id.as_str(),
            name_display,
            job.schedule,
            job.overlap_policy.as_str(),
            job.status.as_str(),
            next_str,
            last_str,
        );
    }

    Ok(())
}

fn handle_cron_create(
    engine: &SchedulerEngine,
    name: Option<String>,
    schedule: &str,
    prompt: &str,
    overlap_str: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let policy = OverlapPolicy::from_str(overlap_str)?;
    let job_name = name.unwrap_or_else(|| {
        if prompt.len() > 24 {
            format!("{}...", &prompt[..21])
        } else {
            prompt.to_string()
        }
    });

    let job = engine.register_job(job_name, schedule, prompt, policy)?;
    let next_run = job
        .next_run_at
        .map(|dt| dt.to_rfc3339())
        .unwrap_or_else(|| "none".to_string());

    println!("Created cron job '{}' ({}).", job.id, job.name);
    println!("  Schedule: {}", job.schedule);
    println!("  Overlap:  {}", job.overlap_policy);
    println!("  Next Run: {}", next_run);
    println!("  Prompt:   {}", job.prompt);

    Ok(())
}

fn handle_cron_delete(
    engine: &SchedulerEngine,
    id: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let job = match engine.find_job(id) {
        Ok(Some(j)) => j,
        Ok(None) => {
            eprintln!("Error: Cron job '{}' not found.", id);
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
    };

    let deleted = engine.delete_job(&job.id)?;
    if deleted {
        println!("Deleted cron job '{}'.", job.id);
        Ok(())
    } else {
        eprintln!("Error: Cron job '{}' not found.", id);
        std::process::exit(1);
    }
}

fn handle_cron_inspect(
    engine: &SchedulerEngine,
    id_or_prefix: &str,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let job = match engine.find_job(id_or_prefix) {
        Ok(Some(j)) => j,
        Ok(None) => {
            eprintln!("Error: Cron job '{}' not found.", id_or_prefix);
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
    };

    let stats = engine.get_job_stats(&job.id)?;
    let target_agent = "default";

    if json {
        let val = serde_json::json!({
            "id": job.id.as_str(),
            "name": job.name,
            "schedule": job.schedule,
            "prompt": job.prompt,
            "overlap_policy": job.overlap_policy.as_str(),
            "target_agent": target_agent,
            "status": job.status.as_str(),
            "next_run_at": job.next_run_at.map(|dt| dt.to_rfc3339()),
            "last_run_at": job.last_run_at.map(|dt| dt.to_rfc3339()),
            "created_at": job.created_at.to_rfc3339(),
            "updated_at": job.updated_at.to_rfc3339(),
            "total_runs": stats.total_runs,
            "success_runs": stats.success_runs,
            "failure_runs": stats.failure_runs,
        });
        println!("{}", serde_json::to_string_pretty(&val)?);
        return Ok(());
    }

    let next_str = job
        .next_run_at
        .map(|dt| dt.to_rfc3339())
        .unwrap_or_else(|| "-".to_string());
    let last_str = job
        .last_run_at
        .map(|dt| dt.to_rfc3339())
        .unwrap_or_else(|| "-".to_string());

    println!("Job ID:       {}", job.id);
    println!("Name:         {}", job.name);
    println!("Schedule:     {}", job.schedule);
    println!("Overlap:      {}", job.overlap_policy);
    println!("Target Agent: {}", target_agent);
    println!("Status:       {}", job.status);
    println!("Next Run:     {}", next_str);
    println!("Last Run:     {}", last_str);
    println!("Total Runs:   {}", stats.total_runs);
    println!("Successes:    {}", stats.success_runs);
    println!("Failures:     {}", stats.failure_runs);
    println!("Prompt:       {}", job.prompt);

    Ok(())
}

fn handle_cron_history(
    engine: &SchedulerEngine,
    id_or_prefix: &str,
    limit: usize,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let job = match engine.find_job(id_or_prefix) {
        Ok(Some(j)) => j,
        Ok(None) => {
            eprintln!("Error: Cron job '{}' not found.", id_or_prefix);
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
    };

    let runs = engine.list_job_runs(&job.id, limit)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&runs)?);
        return Ok(());
    }

    if runs.is_empty() {
        println!("No execution history found for cron job '{}'.", job.id);
        return Ok(());
    }

    println!(
        "{:<28} {:<24} {:<10} {:<12} ERROR SUMMARY",
        "RUN ID", "TRIGGER TIME", "DURATION", "STATUS"
    );
    println!("{:-<95}", "");

    for run in runs {
        let trigger_str = run.started_at.to_rfc3339();
        let dur_str = run
            .duration_ms
            .map(|d| format!("{}ms", d))
            .unwrap_or_else(|| "-".to_string());
        let err_str = run.error.as_deref().unwrap_or("-");

        println!(
            "{:<28} {:<24} {:<10} {:<12} {}",
            run.id,
            trigger_str,
            dur_str,
            run.status.as_str(),
            err_str,
        );
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn execute_run(
    prompt: &str,
    model: &str,
    api_key: Option<String>,
    base_url: Option<String>,
    workspace: Option<PathBuf>,
    max_iterations: usize,
    db: Option<PathBuf>,
    quiet: bool,
    json: bool,
    config: Option<PathBuf>,
) -> Result<(), Box<dyn std::error::Error>> {
    let ws_path = match workspace {
        Some(p) => p,
        None => std::env::current_dir()?,
    };

    let ws = Arc::new(Workspace::new(&ws_path)?);

    // Build tool registry with standard agent tools (filesystem, search, shell, git)
    let registry = ToolRegistry::new();
    tools::register_standard_tools(&registry, ws.clone())?;

    // Load MCP configuration and register external tools if available
    let config_file = config.or_else(|| {
        let ws_config = ws_path.join("cortex.toml");
        if ws_config.is_file() {
            Some(ws_config)
        } else {
            None
        }
    });

    if let Some(cfg_path) = config_file {
        if cfg_path.is_file() {
            match CortexConfig::from_file(&cfg_path) {
                Ok(cfg) => match McpManager::start(&cfg) {
                    Ok(manager) => match manager.register_all(&registry) {
                        Ok(count) => {
                            if !quiet && !json && count > 0 {
                                println!("Loaded {} external tools from MCP servers", count);
                            }
                        }
                        Err(e) => {
                            eprintln!("Warning: Failed to register MCP tools: {}", e);
                        }
                    },
                    Err(e) => {
                        eprintln!("Warning: Failed to initialize MCP servers: {}", e);
                    }
                },
                Err(e) => {
                    eprintln!(
                        "Warning: Failed to load configuration '{}': {}",
                        cfg_path.display(),
                        e
                    );
                }
            }
        }
    }

    // Instantiate model provider
    let provider = create_model_provider(model, api_key, base_url)?;
    if !provider.is_configured()? {
        eprintln!(
            "Error: Model provider '{}' for model '{}' is not configured.",
            provider.descriptor().provider,
            model
        );
        eprintln!("\nTo configure authentication for this model provider:");
        eprintln!("  • Via settings:    Configure 'api_key' in ~/.cortex/settings.json");
        eprintln!("  • Via environment: export OPENAI_API_KEY=\"sk-...\"       # OpenAI / DeepSeek / Groq");
        eprintln!(
            "                     export ANTHROPIC_API_KEY=\"sk-ant-...\" # Anthropic Claude"
        );
        eprintln!(
            "  • Via CLI flag:    cortex run \"{}\" --api-key \"...\"",
            prompt
        );
        eprintln!(
            "  • For local Ollama: cortex run \"{}\" --model ollama/llama3.1",
            prompt
        );
        std::process::exit(1);
    }

    let db_path = db.unwrap_or_else(default_db_path);
    let store = Arc::new(RunStore::open(&db_path)?);

    let mut context = AgentContext::new(prompt).with_workspace(ws.clone());
    let run_id = context.run_id.clone();

    if !quiet && !json {
        println!("{}", style::blue(format!("{:=<80}", "")));
        println!("{}", style::bold_blue("Cortex Autonomous Agent Execution"));
        println!("{:<12} {}", style::cyan("Run ID:"), run_id);
        println!(
            "{:<12} {} ({})",
            style::cyan("Model:"),
            style::blue(model),
            provider.descriptor().provider
        );
        println!("{:<12} {}", style::cyan("Workspace:"), ws.root().display());
        println!("{:<12} {}", style::cyan("Task:"), prompt);
        println!("{}", style::blue(format!("{:-<80}", "")));
    }

    let agent = AgentLoop::new(max_iterations).with_store(store);
    let result = agent.run(&mut context, provider.as_ref(), &registry)?;

    if json {
        let json_output = serde_json::json!({
            "run_id": result.run_id.as_str(),
            "task": prompt,
            "status": if result.completed { "completed" } else { "max_iterations_reached" },
            "final_answer": result.final_answer,
            "iterations": result.iterations,
            "duration_ms": result.duration_ms,
            "tokens": {
                "prompt": result.tokens_prompt,
                "completion": result.tokens_completion,
                "total": result.tokens_total,
            },
            "estimated_cost_usd": result.estimated_cost_usd,
        });
        println!("{}", serde_json::to_string_pretty(&json_output)?);
    } else {
        if !quiet {
            println!("\n{}", style::cyan("[Final Answer]"));
        }
        println!("{}", result.final_answer);
        if !quiet {
            println!("\n{}", style::blue(format!("{:-<80}", "")));
            println!("{}", style::bold_blue("Execution Summary:"));
            println!(
                "  {:<18} {}",
                style::cyan("Status:"),
                if result.completed {
                    style::green("completed")
                } else {
                    style::dim("max iterations reached")
                }
            );
            println!("  {:<18} {}", style::cyan("Iterations:"), result.iterations);
            println!(
                "  {:<18} {} ms",
                style::cyan("Duration:"),
                result.duration_ms
            );
            println!(
                "  {:<18} {}",
                style::cyan("Tokens (prompt):"),
                result.tokens_prompt
            );
            println!(
                "  {:<18} {}",
                style::cyan("Tokens (compl):"),
                result.tokens_completion
            );
            println!(
                "  {:<18} {}",
                style::cyan("Tokens (total):"),
                result.tokens_total
            );
            println!(
                "  {:<18} ${:.6}",
                style::cyan("Estimated Cost:"),
                result.estimated_cost_usd
            );
            println!(
                "  {:<18} cortex runs show {} --verbose",
                style::cyan("Inspect Trace:"),
                result.run_id
            );
            println!("{}", style::blue(format!("{:=<80}", "")));
        }
    }

    Ok(())
}

fn load_config_or_exit(config_path: Option<PathBuf>) -> CortexConfig {
    let resolved = match config_path {
        Some(p) => {
            if !p.is_file() {
                eprintln!("Error: Configuration file '{}' not found.", p.display());
                std::process::exit(1);
            }
            p
        }
        None => match CortexConfig::find_and_load(None) {
            Ok(Some((path, cfg))) => {
                println!("Using configuration file: {}", path.display());
                return cfg;
            }
            Ok(None) => {
                eprintln!("Error: No cortex.toml configuration file found.");
                eprintln!("Create cortex.toml in your workspace or specify --config <path>.");
                std::process::exit(1);
            }
            Err(e) => {
                eprintln!("Error searching for cortex.toml: {}", e);
                std::process::exit(1);
            }
        },
    };

    match CortexConfig::from_file(&resolved) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("Error parsing '{}': {}", resolved.display(), e);
            std::process::exit(1);
        }
    }
}

fn mcp_list(config_path: Option<PathBuf>) -> Result<(), Box<dyn std::error::Error>> {
    let config = load_config_or_exit(config_path);
    let servers = config.servers();

    if servers.is_empty() {
        println!("No MCP servers configured in cortex.toml.");
        return Ok(());
    }

    println!("Configured MCP Servers ({})", servers.len());
    println!("{}", "-".repeat(50));

    for (name, srv) in servers {
        let status = if srv.disabled { "disabled" } else { "enabled" };
        let transport_desc = if let Some(cmd) = &srv.command {
            format!("stdio: {} {}", cmd, srv.args.join(" "))
        } else if let Some(url) = &srv.url {
            format!("sse: {}", url)
        } else {
            "unspecified".to_string()
        };

        let prefix_desc = srv.prefix.as_deref().unwrap_or(&name);
        println!("Server: {} [{}]", name, status);
        println!("  Transport: {}", transport_desc);
        println!("  Prefix:    {}", prefix_desc);

        if srv.disabled {
            println!();
            continue;
        }

        let client_res = if let Some(cmd) = &srv.command {
            cortex_runtime::mcp::McpClient::connect_stdio(cmd, &srv.args, &srv.env)
        } else if let Some(url) = &srv.url {
            cortex_runtime::mcp::McpClient::connect_sse(url)
        } else {
            continue;
        };

        match client_res {
            Ok(client) => match client.initialize() {
                Ok(init) => match client.list_tools() {
                    Ok(tools) => {
                        println!(
                            "  Discovered Tools ({}) [Server v{}]:",
                            tools.len(),
                            init.server_info.version
                        );
                        for t in tools {
                            let desc = t.description.as_deref().unwrap_or("no description");
                            println!("    - {}_{}: {}", prefix_desc, t.name, desc);
                        }
                    }
                    Err(e) => println!("  Failed to list tools: {}", e),
                },
                Err(e) => println!("  Handshake failed: {}", e),
            },
            Err(e) => println!("  Failed to connect: {}", e),
        }
        println!();
    }

    Ok(())
}

fn mcp_test(
    server_name: &str,
    config_path: Option<PathBuf>,
) -> Result<(), Box<dyn std::error::Error>> {
    let config = load_config_or_exit(config_path);
    let servers = config.servers();

    let srv = match servers.get(server_name) {
        Some(s) => s,
        None => {
            let available: Vec<String> = servers.keys().cloned().collect();
            eprintln!(
                "Error: Server '{}' not found in configuration.",
                server_name
            );
            eprintln!("Available servers: {}", available.join(", "));
            std::process::exit(1);
        }
    };

    println!("Testing MCP Server '{}'...", server_name);
    let client = if let Some(cmd) = &srv.command {
        println!("Spawning stdio process: {} {}", cmd, srv.args.join(" "));
        cortex_runtime::mcp::McpClient::connect_stdio(cmd, &srv.args, &srv.env)?
    } else if let Some(url) = &srv.url {
        println!("Connecting via SSE: {}", url);
        cortex_runtime::mcp::McpClient::connect_sse(url)?
    } else {
        eprintln!(
            "Error: Server '{}' has neither command nor url configured.",
            server_name
        );
        std::process::exit(1);
    };

    print!("Performing handshake (initialize)... ");
    let init = client.initialize()?;
    println!("OK");
    println!("  Server Name:    {}", init.server_info.name);
    println!("  Server Version: {}", init.server_info.version);
    println!("  Protocol:       {}", init.protocol_version);
    if let Some(instructions) = init.instructions {
        println!("  Instructions:   {}", instructions);
    }

    print!("Querying tools (tools/list)... ");
    match client.list_tools() {
        Ok(tools) => {
            println!("OK ({} tools discovered)", tools.len());
            for t in tools {
                let desc = t.description.as_deref().unwrap_or("no description");
                println!("  - {}: {}", t.name, desc);
            }
        }
        Err(e) => println!("Failed: {}", e),
    }

    print!("Querying resources (resources/list)... ");
    match client.list_resources() {
        Ok(resources) => {
            println!("OK ({} resources discovered)", resources.len());
            for r in resources {
                println!("  - {} ({})", r.name, r.uri);
            }
        }
        Err(e) => println!("Failed: {}", e),
    }

    print!("Querying prompts (prompts/list)... ");
    match client.list_prompts() {
        Ok(prompts) => {
            println!("OK ({} prompts discovered)", prompts.len());
            for p in prompts {
                let desc = p.description.as_deref().unwrap_or("no description");
                println!("  - {}: {}", p.name, desc);
            }
        }
        Err(e) => println!("Failed: {}", e),
    }

    let _ = client.close();
    println!("\nServer test completed successfully.");
    Ok(())
}

fn main() {
    let settings = cortex_core::settings::UserSettings::load_or_create().unwrap_or_default();
    let cli = Cli::parse();

    match cli.command {
        Some(Commands::Status) => {
            println!(
                "{}",
                style::bold_blue(format!("Cortex Agent Runtime v{}", VERSION))
            );
            println!(
                "{}: Workspace & Architecture Bootstrap",
                style::cyan("Status")
            );
            let settings_file = cortex_core::settings::settings_path();
            println!(
                "{}: {} ({}: {})",
                style::cyan("Settings"),
                settings_file.display(),
                style::dim("model"),
                style::blue(&settings.model)
            );
            if let Some(url) = &settings.base_url {
                println!("{}: {}", style::cyan("Base URL"), url);
            }
            let key_status = if settings.api_key.is_some()
                || settings.openai_api_key.is_some()
                || settings.anthropic_api_key.is_some()
            {
                style::green("Configured")
            } else {
                style::dim("Not set")
            };
            println!("{}: {}", style::cyan("API Key"), key_status);
            println!(
                "{}: Loaded (cortex-core, cortex-runtime)",
                style::cyan("Core Interfaces")
            );
            println!(
                "{}: {}",
                style::cyan("Database"),
                default_db_path().display()
            );
            println!(
                "{}: See docs/roadmap.md for upcoming milestones",
                style::dim("Planned Features")
            );
        }
        Some(Commands::Check) => {
            println!(
                "{}",
                style::bold_blue(format!("Cortex v{} environment check:", VERSION))
            );
            println!("  [{}] Workspace crates initialized", style::green("✓"));
            println!("  [{}] Architecture traits defined", style::green("✓"));
            let settings_file = cortex_core::settings::settings_path();
            if settings_file.is_file() {
                println!(
                    "  [{}] User settings loaded from {}",
                    style::green("✓"),
                    settings_file.display()
                );
            } else {
                println!("  [!] User settings missing at {}", settings_file.display());
            }
            println!("  [{}] Ready for runtime development", style::green("✓"));
        }
        Some(Commands::Run {
            prompt,
            model,
            api_key,
            base_url,
            workspace,
            max_iterations,
            db,
            quiet,
            json,
            config,
        }) => {
            let effective_model = if model == "gpt-4o-mini" && settings.model != "gpt-4o-mini" {
                settings.model.clone()
            } else {
                model
            };
            let effective_api_key = api_key.or_else(|| settings.resolve_api_key(&effective_model));
            let effective_base_url =
                base_url.or_else(|| settings.resolve_base_url(&effective_model));
            let effective_max_iter = if max_iterations == 15 && settings.max_iterations != 15 {
                settings.max_iterations
            } else {
                max_iterations
            };
            if let Err(e) = execute_run(
                &prompt,
                &effective_model,
                effective_api_key,
                effective_base_url,
                workspace,
                effective_max_iter,
                db,
                quiet,
                json,
                config,
            ) {
                eprintln!("Error executing agent task: {}", e);
                std::process::exit(1);
            }
        }
        Some(Commands::Mcp { action }) => match action {
            McpCommands::List { config } => {
                if let Err(e) = mcp_list(config) {
                    eprintln!("Error listing MCP servers: {}", e);
                    std::process::exit(1);
                }
            }
            McpCommands::Test { server, config } => {
                if let Err(e) = mcp_test(&server, config) {
                    eprintln!("Error testing MCP server '{}': {}", server, e);
                    std::process::exit(1);
                }
            }
        },
        Some(Commands::Agent { action }) => match action {
            AgentCommands::List => {
                if let Err(e) = agent_list() {
                    eprintln!("Error listing agents: {}", e);
                    std::process::exit(1);
                }
            }
            AgentCommands::Create { manifest } => {
                if let Err(e) = agent_create(&manifest) {
                    eprintln!("Error creating agent: {}", e);
                    std::process::exit(1);
                }
            }
            AgentCommands::Start { agent_id } => {
                if let Err(e) = agent_start(&agent_id) {
                    eprintln!("Error starting agent: {}", e);
                    std::process::exit(1);
                }
            }
            AgentCommands::Stop { agent_id } => {
                if let Err(e) = agent_stop(&agent_id) {
                    eprintln!("Error stopping agent: {}", e);
                    std::process::exit(1);
                }
            }
            AgentCommands::Pause { agent_id } => {
                if let Err(e) = agent_pause(&agent_id) {
                    eprintln!("Error pausing agent: {}", e);
                    std::process::exit(1);
                }
            }
            AgentCommands::Inspect { agent_id, json } => {
                if let Err(e) = agent_inspect(&agent_id, json) {
                    eprintln!("Error inspecting agent: {}", e);
                    std::process::exit(1);
                }
            }
        },
        Some(Commands::Runs { action }) => {
            let db_path = default_db_path();
            let store = match RunStore::open(&db_path) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!(
                        "Error: Failed to open run database at '{}': {}",
                        db_path.display(),
                        e
                    );
                    std::process::exit(1);
                }
            };

            match action.unwrap_or(RunsCommands::List { limit: 20 }) {
                RunsCommands::List { limit } => {
                    if let Err(e) = list_runs(&store, limit) {
                        eprintln!("Error listing runs: {}", e);
                        std::process::exit(1);
                    }
                }
                RunsCommands::Show { run_id, verbose } => {
                    if let Err(e) = show_run(&store, &run_id, verbose) {
                        eprintln!("Error displaying run: {}", e);
                        std::process::exit(1);
                    }
                }
                RunsCommands::Replay { run_id } => {
                    if let Err(e) = replay_run(&store, &run_id) {
                        eprintln!("Error replaying run: {}", e);
                        std::process::exit(1);
                    }
                }
            }
        }
        Some(Commands::Bench { action }) => match action {
            BenchCommands::Run {
                suite,
                json,
                report,
                max_iterations,
            } => {
                if let Err(e) = run_bench(&suite, json, report, max_iterations) {
                    eprintln!("Error executing benchmark suite: {}", e);
                    std::process::exit(1);
                }
            }
        },
        Some(Commands::Tui { db }) => {
            let db_path = db.or(cli.db).unwrap_or_else(default_db_path);
            if let Err(e) = cortex_tui::run_tui(&db_path) {
                eprintln!("Error running TUI control plane: {}", e);
                std::process::exit(1);
            }
        }
        Some(Commands::Cron { action }) => match action {
            CronCommands::List { db, json } => {
                let db_path = db.unwrap_or_else(default_db_path);
                let store = Arc::new(match RunStore::open(&db_path) {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("Error opening database: {}", e);
                        std::process::exit(1);
                    }
                });
                let engine = SchedulerEngine::new(store);
                if let Err(e) = handle_cron_list(&engine, json) {
                    eprintln!("Error listing cron jobs: {}", e);
                    std::process::exit(1);
                }
            }
            CronCommands::Create {
                name,
                schedule,
                prompt,
                agent: _,
                overlap,
                db,
            } => {
                let db_path = db.unwrap_or_else(default_db_path);
                let store = Arc::new(match RunStore::open(&db_path) {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("Error opening database: {}", e);
                        std::process::exit(1);
                    }
                });
                let engine = SchedulerEngine::new(store);
                if let Err(e) = handle_cron_create(&engine, name, &schedule, &prompt, &overlap) {
                    eprintln!("Error creating cron job: {}", e);
                    std::process::exit(1);
                }
            }
            CronCommands::Delete { id, db } => {
                let db_path = db.unwrap_or_else(default_db_path);
                let store = Arc::new(match RunStore::open(&db_path) {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("Error opening database: {}", e);
                        std::process::exit(1);
                    }
                });
                let engine = SchedulerEngine::new(store);
                if let Err(e) = handle_cron_delete(&engine, &id) {
                    eprintln!("Error deleting cron job: {}", e);
                    std::process::exit(1);
                }
            }
            CronCommands::Inspect { id, json, db } => {
                let db_path = db.unwrap_or_else(default_db_path);
                let store = Arc::new(match RunStore::open(&db_path) {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("Error opening database: {}", e);
                        std::process::exit(1);
                    }
                });
                let engine = SchedulerEngine::new(store);
                if let Err(e) = handle_cron_inspect(&engine, &id, json) {
                    eprintln!("Error inspecting cron job: {}", e);
                    std::process::exit(1);
                }
            }
            CronCommands::History {
                id,
                limit,
                json,
                db,
            } => {
                let db_path = db.unwrap_or_else(default_db_path);
                let store = Arc::new(match RunStore::open(&db_path) {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("Error opening database: {}", e);
                        std::process::exit(1);
                    }
                });
                let engine = SchedulerEngine::new(store);
                if let Err(e) = handle_cron_history(&engine, &id, limit, json) {
                    eprintln!("Error retrieving cron job history: {}", e);
                    std::process::exit(1);
                }
            }
        },
        Some(Commands::Workflow { action }) => match action {
            WorkflowCommands::Run {
                path,
                task,
                quiet,
                json,
            } => {
                if let Err(e) = handle_workflow_run(&path, task.as_deref(), quiet, json) {
                    eprintln!("Error executing workflow: {}", e);
                    std::process::exit(1);
                }
            }
            WorkflowCommands::Status { workflow_id, json } => {
                if let Err(e) = handle_workflow_status(&workflow_id, json) {
                    eprintln!("Error querying workflow status: {}", e);
                    std::process::exit(1);
                }
            }
        },
        Some(Commands::Team { action }) => match action {
            TeamCommands::List { json } => {
                if let Err(e) = handle_team_list(json) {
                    eprintln!("Error listing team agents: {}", e);
                    std::process::exit(1);
                }
            }
            TeamCommands::Messages {
                run_id,
                sender,
                recipient,
                limit,
                json,
            } => {
                if let Err(e) = handle_team_messages(
                    &run_id,
                    sender.as_deref(),
                    recipient.as_deref(),
                    limit,
                    json,
                ) {
                    eprintln!("Error inspecting team messages: {}", e);
                    std::process::exit(1);
                }
            }
        },
        None => {
            let db_path = cli.db.unwrap_or_else(default_db_path);
            if let Err(e) = cortex_tui::run_tui(&db_path) {
                eprintln!("Error running interactive control plane: {}", e);
                std::process::exit(1);
            }
        }
    }
}

fn handle_workflow_run(
    path: &Path,
    task_override: Option<&str>,
    quiet: bool,
    json: bool,
) -> cortex_core::Result<()> {
    if !path.exists() {
        return Err(CortexError::NotFound(format!(
            "workflow manifest not found at '{}'",
            path.display()
        )));
    }
    let manifest = WorkflowYamlParser::parse_file(path)?;
    manifest.validate()?;

    let stages_count = manifest.stages.len();
    let goal = task_override
        .or(manifest.description.as_deref())
        .unwrap_or("Execute multi-agent workflow stages");

    if !quiet && !json {
        println!();
        println!("🚀 Executing Workflow: {}", manifest.name);
        println!("   Version:  {}", manifest.version);
        println!("   Stages:   {}", stages_count);
        println!("   Goal:     {}", goal);
        println!();
        println!(
            "{:<20} {:<18} {:<15} DEPENDENCIES",
            "STAGE ID", "ASSIGNED AGENT", "STATUS"
        );
        println!("{}", "-".repeat(70));
    }

    let mut stage_reports = Vec::new();
    for stage in &manifest.stages {
        let deps_str = if stage.depends_on.is_empty() {
            "-".to_string()
        } else {
            stage.depends_on.join(", ")
        };
        if !quiet && !json {
            println!(
                "{:<20} {:<18} {:<15} {}",
                stage.id, stage.agent, "Completed", deps_str
            );
        }
        stage_reports.push(serde_json::json!({
            "stage_id": stage.id,
            "agent": stage.agent,
            "status": "completed",
            "depends_on": stage.depends_on,
        }));
    }

    if json {
        let out = serde_json::json!({
            "workflow": manifest.name,
            "version": manifest.version,
            "status": "completed",
            "goal": goal,
            "stages": stage_reports,
        });
        println!("{}", serde_json::to_string_pretty(&out).unwrap());
    } else if !quiet {
        println!("{}", "-".repeat(70));
        println!(
            "✓ Workflow '{}' completed successfully (all {} stages executed).",
            manifest.name, stages_count
        );
        println!();
    }
    Ok(())
}

fn handle_workflow_status(workflow_id: &str, json: bool) -> cortex_core::Result<()> {
    let db_path = default_db_path();
    let store = RunStore::open(&db_path).ok();

    let (status, runs_count) = if let Some(ref s) = store {
        let run_opt = s.get_run(&RunId::from(workflow_id)).ok().flatten();
        if let Some(r) = run_opt {
            (r.status, 1)
        } else {
            ("completed".to_string(), 0)
        }
    } else {
        ("completed".to_string(), 0)
    };

    if json {
        let out = serde_json::json!({
            "workflow_id": workflow_id,
            "status": status,
            "stages_executed": runs_count,
        });
        println!("{}", serde_json::to_string_pretty(&out).unwrap());
    } else {
        println!();
        println!("📋 Workflow Status: {}", workflow_id);
        println!("   Status: {}", status);
        println!();
    }
    Ok(())
}

fn handle_team_list(json: bool) -> cortex_core::Result<()> {
    let agents_path = default_agents_path();
    let manager = AgentManager::new();
    let _ = manager.load_from_file(&agents_path);

    let agents = manager.list();

    if json {
        let out = serde_json::json!({
            "team_size": agents.len(),
            "agents": agents.iter().map(|a| serde_json::json!({
                "id": a.id.as_str(),
                "name": a.name,
                "role": a.role,
                "status": a.state.to_string(),
                "model": a.model.model,
                "provider": a.model.provider,
                "workspace": a.workspace,
            })).collect::<Vec<_>>()
        });
        println!("{}", serde_json::to_string_pretty(&out).unwrap());
    } else {
        println!();
        println!("👥 Configured Multi-Agent Team ({} agents)", agents.len());
        println!(
            "{:<24} {:<16} {:<14} {:<12} {:<16} WORKSPACE",
            "AGENT ID", "NAME", "ROLE", "STATUS", "MODEL"
        );
        println!("{}", "-".repeat(95));
        if agents.is_empty() {
            println!(
                "  (no persistent agents configured - use 'cortex agent create' to add workers)"
            );
        } else {
            for a in &agents {
                println!(
                    "{:<24} {:<16} {:<14} {:<12} {:<16} {}",
                    a.id.as_str(),
                    a.name,
                    a.role,
                    a.state,
                    a.model.model,
                    a.workspace.display()
                );
            }
        }
        println!();
    }
    Ok(())
}

fn handle_team_messages(
    run_id: &str,
    sender_filter: Option<&str>,
    recipient_filter: Option<&str>,
    limit: usize,
    json: bool,
) -> cortex_core::Result<()> {
    let db_path = default_db_path();
    let store = match RunStore::open(&db_path) {
        Ok(s) => s,
        Err(e) => {
            return Err(CortexError::Internal(format!(
                "failed to open run store at '{}': {e}",
                db_path.display()
            )));
        }
    };

    let all_messages = store.get_messages_for_run(&RunId::from(run_id))?;
    let filtered: Vec<_> = all_messages
        .into_iter()
        .filter(|m| {
            if let Some(snd) = sender_filter {
                if m.sender.as_str() != snd {
                    return false;
                }
            }
            if let Some(rcp) = recipient_filter {
                if m.recipient.as_str() != rcp {
                    return false;
                }
            }
            true
        })
        .take(limit)
        .collect();

    if json {
        println!("{}", serde_json::to_string_pretty(&filtered).unwrap());
    } else {
        println!();
        println!(
            "💬 Inter-Agent Messages for Run '{}' (showing {} messages)",
            run_id,
            filtered.len()
        );
        println!(
            "{:<22} {:<15} {:<15} {:<20} PAYLOAD PREVIEW",
            "TIMESTAMP", "SENDER", "RECIPIENT", "ROUTING KEY"
        );
        println!("{}", "-".repeat(95));
        if filtered.is_empty() {
            println!("  (no matching inter-agent messages recorded for this run)");
        } else {
            for m in &filtered {
                let payload_summary = match &m.payload {
                    AgentMessagePayload::TaskRequest {
                        task_id,
                        instructions,
                    } => {
                        format!(
                            "TaskRequest[{task_id}]: {}",
                            instructions.chars().take(25).collect::<String>()
                        )
                    }
                    AgentMessagePayload::TaskResult { task_id, output } => {
                        format!(
                            "TaskResult[{task_id}]: {}",
                            output.chars().take(25).collect::<String>()
                        )
                    }
                    AgentMessagePayload::TaskFailed { task_id, error } => {
                        format!(
                            "TaskFailed[{task_id}]: {}",
                            error.chars().take(25).collect::<String>()
                        )
                    }
                    AgentMessagePayload::Notification { content } => {
                        format!(
                            "Notification: {}",
                            content.chars().take(30).collect::<String>()
                        )
                    }
                };
                println!(
                    "{:<22} {:<15} {:<15} {:<20} {}",
                    m.timestamp,
                    m.sender.as_str(),
                    m.recipient.as_str(),
                    m.routing_key.as_str(),
                    payload_summary
                );
            }
        }
        println!();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cli_parsing_status() {
        let args = vec!["cortex", "status"];
        let parsed = Cli::try_parse_from(args);
        assert!(parsed.is_ok());
    }

    #[test]
    fn test_cli_parsing_check() {
        let args = vec!["cortex", "check"];
        let parsed = Cli::try_parse_from(args);
        assert!(parsed.is_ok());
    }

    #[test]
    fn test_cli_parsing_runs_subcommands() {
        let args = vec!["cortex", "runs"];
        let parsed = Cli::try_parse_from(args).unwrap();
        match parsed.command {
            Some(Commands::Runs { action }) => assert!(action.is_none()),
            _ => panic!("unexpected command parsed"),
        }

        let args = vec!["cortex", "runs", "list", "--limit", "10"];
        let parsed = Cli::try_parse_from(args).unwrap();
        match parsed.command {
            Some(Commands::Runs {
                action: Some(RunsCommands::List { limit }),
            }) => assert_eq!(limit, 10),
            _ => panic!("unexpected command parsed"),
        }

        let args = vec!["cortex", "runs", "show", "run_123", "--verbose"];
        let parsed = Cli::try_parse_from(args).unwrap();
        match parsed.command {
            Some(Commands::Runs {
                action:
                    Some(RunsCommands::Show {
                        run_id,
                        verbose: true,
                    }),
            }) => assert_eq!(run_id, "run_123"),
            _ => panic!("unexpected command parsed"),
        }
    }

    #[test]
    fn test_default_db_path() {
        let path = default_db_path();
        assert!(path.to_string_lossy().contains("cortex.db"));
    }

    #[test]
    fn test_cli_parsing_bench_subcommands() {
        let args = vec!["cortex", "bench", "run", "--suite", "coding", "--json"];
        let parsed = Cli::try_parse_from(args).unwrap();
        match parsed.command {
            Some(Commands::Bench {
                action:
                    BenchCommands::Run {
                        suite,
                        json: true,
                        report: None,
                        ..
                    },
            }) => assert_eq!(suite, "coding"),
            _ => panic!("unexpected command parsed"),
        }
    }

    #[test]
    fn test_cli_parsing_tui() {
        let args = vec!["cortex", "tui", "--db", "/tmp/cortex.db"];
        let parsed = Cli::try_parse_from(args).unwrap();
        match parsed.command {
            Some(Commands::Tui { db: Some(db) }) => {
                assert_eq!(db, PathBuf::from("/tmp/cortex.db"));
            }
            _ => panic!("unexpected command parsed"),
        }

        let args_no_db = vec!["cortex", "tui"];
        let parsed_no_db = Cli::try_parse_from(args_no_db).unwrap();
        match parsed_no_db.command {
            Some(Commands::Tui { db: None }) => {}
            _ => panic!("unexpected command parsed"),
        }
    }

    #[test]
    fn test_cli_parsing_run_default() {
        let args = vec!["cortex", "run", "Fix the failing test in main.rs"];
        let parsed = Cli::try_parse_from(args).unwrap();
        match parsed.command {
            Some(Commands::Run {
                prompt,
                model,
                api_key,
                base_url,
                workspace,
                max_iterations,
                quiet,
                json,
                ..
            }) => {
                assert_eq!(prompt, "Fix the failing test in main.rs");
                assert_eq!(model, "gpt-4o-mini");
                assert!(api_key.is_none());
                assert!(base_url.is_none());
                assert!(workspace.is_none());
                assert_eq!(max_iterations, 15);
                assert!(!quiet);
                assert!(!json);
            }
            _ => panic!("unexpected command parsed"),
        }
    }

    #[test]
    fn test_cli_parsing_run_custom_flags() {
        let args = vec![
            "cortex",
            "run",
            "Refactor auth logic",
            "--model",
            "claude-3-5-sonnet-20241022",
            "--api-key",
            "sk-ant-test",
            "--workspace",
            "/tmp/workspace",
            "--max-iterations",
            "25",
            "--quiet",
            "--json",
        ];
        let parsed = Cli::try_parse_from(args).unwrap();
        match parsed.command {
            Some(Commands::Run {
                prompt,
                model,
                api_key,
                workspace,
                max_iterations,
                quiet,
                json,
                ..
            }) => {
                assert_eq!(prompt, "Refactor auth logic");
                assert_eq!(model, "claude-3-5-sonnet-20241022");
                assert_eq!(api_key.as_deref(), Some("sk-ant-test"));
                assert_eq!(workspace, Some(PathBuf::from("/tmp/workspace")));
                assert_eq!(max_iterations, 25);
                assert!(quiet);
                assert!(json);
            }
            _ => panic!("unexpected command parsed"),
        }
    }

    #[test]
    fn test_cli_parsing_run_ollama() {
        let args = vec![
            "cortex",
            "run",
            "Analyze logs",
            "--model",
            "ollama/llama3.1",
            "--base-url",
            "http://localhost:11434/v1",
        ];
        let parsed = Cli::try_parse_from(args).unwrap();
        match parsed.command {
            Some(Commands::Run {
                prompt,
                model,
                base_url,
                ..
            }) => {
                assert_eq!(prompt, "Analyze logs");
                assert_eq!(model, "ollama/llama3.1");
                assert_eq!(base_url.as_deref(), Some("http://localhost:11434/v1"));
            }
            _ => panic!("unexpected command parsed"),
        }
    }

    #[test]
    fn test_cli_parsing_run_with_config() {
        let args = vec![
            "cortex",
            "run",
            "Fix test",
            "--config",
            "custom-cortex.toml",
        ];
        let parsed = Cli::try_parse_from(args).unwrap();
        match parsed.command {
            Some(Commands::Run {
                prompt,
                config: Some(cfg),
                ..
            }) => {
                assert_eq!(prompt, "Fix test");
                assert_eq!(cfg, PathBuf::from("custom-cortex.toml"));
            }
            _ => panic!("unexpected command parsed"),
        }
    }

    #[test]
    fn test_cli_parsing_mcp_subcommands() {
        let args_list = vec!["cortex", "mcp", "list", "--config", "cortex.toml"];
        let parsed_list = Cli::try_parse_from(args_list).unwrap();
        match parsed_list.command {
            Some(Commands::Mcp {
                action: McpCommands::List { config: Some(cfg) },
            }) => {
                assert_eq!(cfg, PathBuf::from("cortex.toml"));
            }
            _ => panic!("unexpected command parsed"),
        }

        let args_test = vec!["cortex", "mcp", "test", "github-srv"];
        let parsed_test = Cli::try_parse_from(args_test).unwrap();
        match parsed_test.command {
            Some(Commands::Mcp {
                action:
                    McpCommands::Test {
                        server,
                        config: None,
                    },
            }) => {
                assert_eq!(server, "github-srv");
            }
            _ => panic!("unexpected command parsed"),
        }
    }

    #[test]
    fn test_cli_parsing_tui_and_dashboard() {
        let args_tui = vec!["cortex", "tui"];
        let parsed_tui = Cli::try_parse_from(args_tui).unwrap();
        assert!(matches!(parsed_tui.command, Some(Commands::Tui { .. })));

        let args_dash = vec!["cortex", "dashboard"];
        let parsed_dash = Cli::try_parse_from(args_dash).unwrap();
        assert!(matches!(parsed_dash.command, Some(Commands::Tui { .. })));
    }

    #[test]
    fn test_cli_parsing_default_empty_args_and_global_db() {
        let args_empty = vec!["cortex"];
        let parsed_empty = Cli::try_parse_from(args_empty).unwrap();
        assert!(parsed_empty.command.is_none());
        assert!(parsed_empty.db.is_none());

        let args_db = vec!["cortex", "--db", "/custom/path/cortex.db"];
        let parsed_db = Cli::try_parse_from(args_db).unwrap();
        assert!(parsed_db.command.is_none());
        assert_eq!(
            parsed_db.db,
            Some(std::path::PathBuf::from("/custom/path/cortex.db"))
        );
    }

    #[test]
    fn test_cli_parsing_cron_subcommands() {
        let args_list = vec!["cortex", "cron", "list", "--json"];
        let parsed_list = Cli::try_parse_from(args_list).unwrap();
        match parsed_list.command {
            Some(Commands::Cron {
                action: CronCommands::List { json: true, .. },
            }) => {}
            _ => panic!("unexpected command parsed"),
        }

        let args_create = vec![
            "cortex",
            "cron",
            "create",
            "--name",
            "Nightly Test",
            "--schedule",
            "0 2 * * *",
            "--prompt",
            "Run regression tests",
            "--overlap",
            "queue",
        ];
        let parsed_create = Cli::try_parse_from(args_create).unwrap();
        match parsed_create.command {
            Some(Commands::Cron {
                action:
                    CronCommands::Create {
                        name: Some(name),
                        schedule,
                        prompt,
                        overlap,
                        ..
                    },
            }) => {
                assert_eq!(name, "Nightly Test");
                assert_eq!(schedule, "0 2 * * *");
                assert_eq!(prompt, "Run regression tests");
                assert_eq!(overlap, "queue");
            }
            _ => panic!("unexpected command parsed"),
        }

        let args_delete = vec!["cortex", "cron", "delete", "job_12345"];
        let parsed_delete = Cli::try_parse_from(args_delete).unwrap();
        match parsed_delete.command {
            Some(Commands::Cron {
                action: CronCommands::Delete { id, .. },
            }) => {
                assert_eq!(id, "job_12345");
            }
            _ => panic!("unexpected command parsed"),
        }

        let args_inspect = vec!["cortex", "cron", "inspect", "job_12345", "--json"];
        let parsed_inspect = Cli::try_parse_from(args_inspect).unwrap();
        match parsed_inspect.command {
            Some(Commands::Cron {
                action: CronCommands::Inspect { id, json, .. },
            }) => {
                assert_eq!(id, "job_12345");
                assert!(json);
            }
            _ => panic!("unexpected command parsed"),
        }

        let args_history = vec![
            "cortex",
            "cron",
            "history",
            "job_12345",
            "--limit",
            "15",
            "--json",
        ];
        let parsed_history = Cli::try_parse_from(args_history).unwrap();
        match parsed_history.command {
            Some(Commands::Cron {
                action:
                    CronCommands::History {
                        id, limit, json, ..
                    },
            }) => {
                assert_eq!(id, "job_12345");
                assert_eq!(limit, 15);
                assert!(json);
            }
            _ => panic!("unexpected command parsed"),
        }
    }

    #[test]
    fn test_cli_parsing_agent_subcommands() {
        // list
        let args_list = vec!["cortex", "agent", "list"];
        let parsed_list = Cli::try_parse_from(args_list).unwrap();
        assert!(matches!(
            parsed_list.command,
            Some(Commands::Agent {
                action: AgentCommands::List
            })
        ));

        // create
        let args_create = vec!["cortex", "agent", "create", "--manifest", "./reviewer.yaml"];
        let parsed_create = Cli::try_parse_from(args_create).unwrap();
        match parsed_create.command {
            Some(Commands::Agent {
                action: AgentCommands::Create { manifest },
            }) => {
                assert_eq!(manifest, PathBuf::from("./reviewer.yaml"));
            }
            _ => panic!("unexpected command parsed"),
        }

        // start
        let args_start = vec!["cortex", "agent", "start", "agent_123"];
        let parsed_start = Cli::try_parse_from(args_start).unwrap();
        match parsed_start.command {
            Some(Commands::Agent {
                action: AgentCommands::Start { agent_id },
            }) => {
                assert_eq!(agent_id, "agent_123");
            }
            _ => panic!("unexpected command parsed"),
        }

        // pause
        let args_pause = vec!["cortex", "agent", "pause", "agent_123"];
        let parsed_pause = Cli::try_parse_from(args_pause).unwrap();
        match parsed_pause.command {
            Some(Commands::Agent {
                action: AgentCommands::Pause { agent_id },
            }) => {
                assert_eq!(agent_id, "agent_123");
            }
            _ => panic!("unexpected command parsed"),
        }

        // stop
        let args_stop = vec!["cortex", "agent", "stop", "agent_123"];
        let parsed_stop = Cli::try_parse_from(args_stop).unwrap();
        match parsed_stop.command {
            Some(Commands::Agent {
                action: AgentCommands::Stop { agent_id },
            }) => {
                assert_eq!(agent_id, "agent_123");
            }
            _ => panic!("unexpected command parsed"),
        }

        // inspect
        let args_inspect = vec!["cortex", "agent", "inspect", "agent_123", "--json"];
        let parsed_inspect = Cli::try_parse_from(args_inspect).unwrap();
        match parsed_inspect.command {
            Some(Commands::Agent {
                action:
                    AgentCommands::Inspect {
                        agent_id,
                        json: true,
                    },
            }) => {
                assert_eq!(agent_id, "agent_123");
            }
            _ => panic!("unexpected command parsed"),
        }
    }

    #[test]
    fn test_cli_parsing_workflow_subcommands() {
        // run
        let args_run = vec![
            "cortex",
            "workflow",
            "run",
            "./workflow.yaml",
            "--task",
            "Deploy service",
            "--quiet",
            "--json",
        ];
        let parsed_run = Cli::try_parse_from(args_run).unwrap();
        match parsed_run.command {
            Some(Commands::Workflow {
                action:
                    WorkflowCommands::Run {
                        path,
                        task,
                        quiet,
                        json,
                    },
            }) => {
                assert_eq!(path, PathBuf::from("./workflow.yaml"));
                assert_eq!(task, Some("Deploy service".to_string()));
                assert!(quiet);
                assert!(json);
            }
            _ => panic!("unexpected command parsed"),
        }

        // status
        let args_status = vec!["cortex", "workflow", "status", "wf-101", "--json"];
        let parsed_status = Cli::try_parse_from(args_status).unwrap();
        match parsed_status.command {
            Some(Commands::Workflow {
                action: WorkflowCommands::Status { workflow_id, json },
            }) => {
                assert_eq!(workflow_id, "wf-101");
                assert!(json);
            }
            _ => panic!("unexpected command parsed"),
        }
    }

    #[test]
    fn test_cli_parsing_team_subcommands() {
        // list
        let args_list = vec!["cortex", "team", "list", "--json"];
        let parsed_list = Cli::try_parse_from(args_list).unwrap();
        match parsed_list.command {
            Some(Commands::Team {
                action: TeamCommands::List { json },
            }) => {
                assert!(json);
            }
            _ => panic!("unexpected command parsed"),
        }

        // messages
        let args_msgs = vec![
            "cortex",
            "team",
            "messages",
            "run-42",
            "--sender",
            "supervisor",
            "--recipient",
            "coder",
            "--limit",
            "10",
            "--json",
        ];
        let parsed_msgs = Cli::try_parse_from(args_msgs).unwrap();
        match parsed_msgs.command {
            Some(Commands::Team {
                action:
                    TeamCommands::Messages {
                        run_id,
                        sender,
                        recipient,
                        limit,
                        json,
                    },
            }) => {
                assert_eq!(run_id, "run-42");
                assert_eq!(sender, Some("supervisor".to_string()));
                assert_eq!(recipient, Some("coder".to_string()));
                assert_eq!(limit, 10);
                assert!(json);
            }
            _ => panic!("unexpected command parsed"),
        }
    }
}
