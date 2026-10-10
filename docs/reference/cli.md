# CLI Reference

Cortex provides a unified command-line interface for running autonomous AI agents, inspecting execution traces, running benchmark evaluations, and launching the interactive terminal control plane.

```bash
cortex status
cortex check
cortex run "<prompt>" [--model <model>] [--api-key <key>] [--base-url <url>] [--workspace <path>] [--max-iterations <N>] [--json] [--quiet]

cortex runs list [--limit <N>]
cortex runs show <id> [--verbose]
cortex runs replay <id>

cortex bench run [--suite <suite>] [--json] [--report <path>] [--max-iterations <N>]

cortex agent list
cortex agent create --manifest <path>
cortex agent start <id>
cortex agent pause <id>
cortex agent stop <id>
cortex agent inspect <id> [--json]

cortex cron list [--json] [--db <path>]
cortex cron create --schedule "<expr>" --prompt "<prompt>" [--name <name>] [--overlap <policy>] [--db <path>]
cortex cron inspect <id> [--json] [--db <path>]
cortex cron history <id> [--limit <N>] [--json] [--db <path>]
cortex cron delete <id> [--db <path>]

cortex [--db <path>]
```

---

## `cortex run`

Execute an autonomous agent task against real model APIs or local LLM instances.

```bash
cortex run "<prompt>" [OPTIONS]
```

### Options

| Option | Environment Variable | Default | Description |
|---|---|---|---|
| `-m, --model <model>` | `CORTEX_MODEL` | `gpt-4o-mini` | Model identifier (e.g. `gpt-4o-mini`, `gpt-4o`, `claude-3-5-sonnet-20241022`, `deepseek-chat`, `ollama/<model>`). |
| `--api-key <key>` | `CORTEX_API_KEY` | None | API authentication key. Falls back to `OPENAI_API_KEY` or `ANTHROPIC_API_KEY`. |
| `--base-url <url>` | `CORTEX_BASE_URL` | None | Base API URL for OpenAI-compatible endpoints or local servers (`http://localhost:11434/v1`). |
| `-w, --workspace <path>` | None | `.` (current dir) | Confined workspace directory boundary for agent operations. |
| `-i, --max-iterations <N>`| None | `15` | Maximum autonomous model-tool iteration loops. |
| `--db <path>` | `CORTEX_DB_PATH` | `~/.cortex/cortex.db` | SQLite database file for recording execution runs and traces. |
| `-q, --quiet` | None | `false` | Suppress execution banners and print only the final answer. |
| `--json` | None | `false` | Output structured JSON with token counts and estimated USD cost. |

### Examples

#### 1. Local Google Gemma 4 (Offline, 100% Free, Zero API Key)
```bash
# Gemma 4 12B (recommended default)
cortex run "Inspect the git status and fix failing tests" --model ollama/gemma4:12b

# Gemma 4 26B (deep reasoning variant)
cortex run "Refactor configuration parsing to support environment variables" \
  --model ollama/gemma4:26b

# Explicit workspace sandboxing with Gemma 4
cortex run "Implement health check route" \
  --model ollama/gemma4:12b \
  --workspace /path/to/project
```

#### 2. Google AI Studio (Hosted Gemma 4)
```bash
export GEMINI_API_KEY="AIzaSy..."
cortex run "Summarize commit history and update CHANGELOG.md" \
  --model gemma-4-26b-it \
  --base-url https://generativelanguage.googleapis.com/v1beta/openai/
```

#### 3. OpenAI / OpenAI-Compatible
```bash
export OPENAI_API_KEY="sk-..."
cortex run "Refactor error handling in src/model.rs" --model gpt-4o-mini
```

#### 4. Anthropic Claude
```bash
export ANTHROPIC_API_KEY="sk-ant-..."
cortex run "Add comprehensive unit tests for tool registry" --model claude-3-5-sonnet-20241022
```

#### 5. JSON Output with Cost Tracking
```bash
cortex run "Fix calculator bug" --model ollama/gemma2:9b --json
```

Output:
```json
{
  "run_id": "run_01j7abc...",
  "task": "Fix calculator bug",
  "status": "completed",
  "final_answer": "Fixed bug in add function and verified all tests pass.",
  "iterations": 3,
  "duration_ms": 2840,
  "tokens": {
    "prompt": 1250,
    "completion": 310,
    "total": 1560
  },
  "estimated_cost_usd": 0.000373
}
```

---

## `cortex runs`

Inspect and replay historical execution traces recorded in SQLite.

```bash
# List recent executions
cortex runs list --limit 10

# Inspect run details and full event history
cortex runs show run_01j7abc... --verbose

# Replay a past run deterministically using recorded model outputs
cortex runs replay run_01j7abc...
```

---

## `cortex bench`

Execute deterministic benchmark suites to evaluate autonomous coding capabilities.

```bash
# Run coding benchmark suite
cortex bench run --suite coding

# Output JSON metrics and save Markdown report
cortex bench run --suite coding --json --report reports/coding.md
```

---

## `cortex agent`

Manage persistent agent worker lifecycles and manifest configurations.

```bash
# List all configured agents and their current status
cortex agent list

# Create an agent from a YAML manifest
cortex agent create --manifest ./agents/reviewer.yaml

# Start or resume an agent
cortex agent start <agent-id>

# Pause a running agent
cortex agent pause <agent-id>

# Stop a running or paused agent
cortex agent stop <agent-id>

# Inspect details, configuration, and state for an agent
cortex agent inspect <agent-id>
cortex agent inspect <agent-id> --json
```

### Subcommands

| Subcommand | Description | Arguments / Flags |
|---|---|---|
| `list` | List all configured agents in table format | None |
| `create` | Create an agent from a manifest file | `-m, --manifest <path>` (YAML, JSON, or TOML) |
| `start` | Start a created agent or resume a paused agent | `<agent-id>` |
| `pause` | Pause a running agent | `<agent-id>` |
| `stop` | Stop a running or paused agent | `<agent-id>` |
| `inspect` | Inspect detailed configuration and state | `<agent-id>`, `--json` (optional) |

---

## `cortex cron`

Manage persistent background scheduled cron jobs and one-shot execution timers.

```bash
# List all registered scheduled jobs
cortex cron list
cortex cron list --json

# Create and register a scheduled cron job
cortex cron create --name "Nightly Backup" --schedule "0 2 * * *" --prompt "Backup workspace databases" --overlap queue

# Inspect job details, configuration, and execution statistics
cortex cron inspect <job-id>
cortex cron inspect <job-id> --json

# View chronological execution history and failure summaries
cortex cron history <job-id>
cortex cron history <job-id> --limit 10 --json

# Delete a registered scheduled job
cortex cron delete <job-id>
```

### Subcommands

| Subcommand | Description | Arguments / Flags |
|---|---|---|
| `list` | List all registered scheduled jobs | `--json`, `--db <path>` |
| `create` | Register a new scheduled cron job | `-s, --schedule <expr>`, `-p, --prompt <text>`, `-n, --name <name>`, `-o, --overlap <policy>`, `--db <path>` |
| `inspect` | Inspect job details and run statistics | `<job-id>`, `--json`, `--db <path>` |
| `history` | View chronological execution history | `<job-id>`, `-l, --limit <N>`, `--json`, `--db <path>` |
| `delete` | Delete a scheduled cron job | `<job-id>`, `--db <path>` |

---

## Interactive Control Plane (`cortex`)

Launch the interactive terminal control plane (Ratatui) to navigate live and historical runs, inspect traces, chat with agents, and monitor token usage. Simply run `cortex`:

```bash
# Default database
cortex

# Custom database
cortex --db /path/to/cortex.db
```

See the [TUI Control Plane Guide](../guides/tui.md) for keyboard shortcuts and views.

---

## `cortex workflow`

Manage and execute declarative multi-agent workflows defined in `workflow.yaml`.

```bash
# Execute a multi-agent workflow file
cortex workflow run ./workflow.yaml

# Execute with task override and JSON output
cortex workflow run ./workflow.yaml --task "Fix security vulnerabilities" --json

# Query stage progression and status
cortex workflow status <workflow-id> [--json]
```

### Subcommands

| Subcommand | Description | Arguments / Flags |
|---|---|---|
| `run` | Execute a multi-agent workflow manifest | `<path>`, `-t, --task <prompt>`, `-q, --quiet`, `--json` |
| `status` | Query status and stage progression | `<workflow_id>`, `--json` |

---

## `cortex team`

Inspect multi-agent team composition and inter-agent coordination messages.

```bash
# List configured team members and capability roles
cortex team list
cortex team list --json

# Inspect and filter inter-agent message logs for a run
cortex team messages <run-id>
cortex team messages <run-id> --sender manager --recipient coder --limit 20 --json
```

### Subcommands

| Subcommand | Description | Arguments / Flags |
|---|---|---|
| `list` | List team agents and capability roles | `--json` (optional) |
| `messages` | Filter and tail inter-agent message history | `<run_id>`, `-s, --sender <id>`, `-r, --recipient <id>`, `-l, --limit <N>`, `--json` |
