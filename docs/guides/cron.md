# Cron guide

Cortex provides a persistent, autonomous background scheduler backed by SQLite. Jobs survive process restarts and track full execution histories.

## Managing scheduled jobs

### Create a job

Register recurring cron jobs or one-shot timers:

```bash
# Recurring job with 5-field cron syntax
cortex cron create --name "GitHub Monitor" --schedule "*/30 * * * *" --prompt "Check repository issues and triage" --overlap skip

# One-shot timer at a specific timestamp
cortex cron create --name "Deployment Check" --schedule "@once 2026-10-15T15:30:00Z" --prompt "Verify canary deployment status"
```

### List registered jobs

Display all scheduled jobs and their upcoming trigger timestamps:

```bash
cortex cron list
cortex cron list --json
```

### Inspect job details and statistics

View job configuration, target prompt, and run statistics (total runs, successes, failures):

```bash
# Query by full identifier or unique prefix
cortex cron inspect job_01j7abc...

# Output structured JSON
cortex cron inspect job_01j7abc... --json
```

Output:

```text
Job ID:       job_01j7abc...
Name:         GitHub Monitor
Schedule:     */30 * * * *
Overlap:      skip
Target Agent: default
Status:       active
Next Run:     2026-10-15T16:00:00Z
Last Run:     2026-10-15T15:30:00Z
Total Runs:   12
Successes:    11
Failures:     1
Prompt:       Check repository issues and triage
```

### View execution history

Display chronological execution records and failure error summaries for a job:

```bash
# View recent execution records
cortex cron history job_01j7abc...

# Limit records and output JSON
cortex cron history job_01j7abc... --limit 10 --json
```

Output:

```text
RUN ID                      TRIGGER TIME             DURATION   STATUS     ERROR SUMMARY
-----------------------------------------------------------------------------------------------
cronrun_1728599400_0001     2026-10-15T15:30:00Z     1420ms     completed  -
cronrun_1728597600_0001     2026-10-15T15:00:00Z     890ms      failed     Connection reset by peer
```

### Delete a job

Remove a job and its associated execution records:

```bash
cortex cron delete job_01j7abc...
```

## Overlap policies

When a scheduled trigger fires while a previous run is still in progress:

- `skip` (default): Drops the current trigger, records a skipped run, and advances directly to the next scheduled interval.
- `queue`: Queues triggers and runs them sequentially once the active execution finishes.
- `replace`: Cancels the active execution and starts the new run immediately.
