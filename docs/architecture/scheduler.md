# Scheduler architecture

The scheduler engine manages autonomous background job triggers and recurring tasks in Cortex. It supports standard 5-field cron schedules, one-shot timers, SQLite persistence across process restarts, and execution overlap policies.

## Architecture and lifecycle

The scheduler operates as a stateful coordinator backed by SQLite storage (`RunStore`):

```text
[Cron Schedule or One-Shot Timestamp]
                  |
                  v
       [Scheduler Engine: tick()]
                  |
         +--------+--------+
         |                 |
  (No Active Run)   (Active Run Present)
         |                 |
         |                 +--> Overlap Policy:
         |                      - Skip: records skipped entry, updates next trigger
         |                      - Queue: increments backlog, runs on completion
         |                      - Replace: cancels active run, starts new run
         v
[Persistent SQLite Store (cron_jobs, cron_job_runs)]
         |
         v
[Agent Loop Execution Run (RunId)]
```

## Schedule definitions

Jobs declare their execution timing through the `Schedule` model in `crates/cortex-runtime/src/scheduler/cron.rs`:

1. Standard 5-field cron expressions:
   - Fields: `minute` (0-59), `hour` (0-23), `day_of_month` (1-31), `month` (1-12), and `day_of_week` (0-7, where both 0 and 7 represent Sunday).
   - Wildcards (`*`), steps (`*/15`), lists (`1,15,30`), ranges (`1-5`), and named tokens (`MON-FRI`, `JAN-MAR`).
   - Macros: `@yearly`, `@monthly`, `@weekly`, `@daily`, `@midnight`, `@hourly`.
2. One-shot timers:
   - Prefix syntax: `@once <ISO-8601-TIMESTAMP>` or `@at <ISO-8601-TIMESTAMP>`.
   - Fires once at the designated instant, transitions job status to `Completed`, and clears `next_run_at`.

## Overlap policies and recovery semantics

When a scheduled trigger fires while a previous execution run is still active, the engine applies the configured `OverlapPolicy`:

- `Skip` (default): Drops the current trigger and records a skipped run record in `cron_job_runs`. Advances `next_run_at` to the next future scheduled interval, preventing overlapping runs without building up backlogs.
- `Queue`: Increments the job's queued execution backlog. When the currently running execution finishes via `finish_job_run`, the engine automatically consumes the queue and starts the next execution.
- `Replace`: Cancels the active run (`cancel_cron_run`), records its status as `Cancelled` with an explanation, and immediately starts a new execution run.

## Downtime recovery behavior

When the Cortex process is offline across multiple scheduled intervals, the engine avoids thundering-herd cascades upon restart:

1. Catch-up evaluation: During startup or the first `engine.tick(now)` after downtime, any active job where `next_run_at <= now` triggers immediately.
2. Next run calculation: The next execution time is computed strictly relative to the current wall-clock time (`sched.next_run_after(now)`), rather than replaying every missed historical point in time.
3. Skip policy behavior: Jobs configured with `OverlapPolicy::Skip` trigger once at recovery time and advance directly to the next future interval. Missed intervals in the downtime window do not accumulate.
4. Queue policy catch-up: Jobs configured with `OverlapPolicy::Queue` queue successive triggers and drain them sequentially upon each run completion.

## Cancellation and deletion behavior

- Deletion during active runs: When `engine.delete_job(id)` is called, the job is removed from `cron_jobs`, all associated run records in `cron_job_runs` are removed via database cascade, and any queued trigger counters are evicted from memory. Orphaned runs that complete after job deletion are handled gracefully without panics.
- Replacement cancellation: When `OverlapPolicy::Replace` cancels an active execution, the cancelled run is recorded with `JobRunStatus::Cancelled` and an explicit error explanation in SQLite before the replacement run starts.

## Timezone and calendar boundary transitions

All internal scheduling calculations operate strictly in `DateTime<Utc>`:

- Timezone offsets: Timestamps with arbitrary UTC offsets (such as `+05:30` or `-08:00`) are normalized to UTC upon parsing.
- Month-end transitions: Transitions across month boundaries (such as May 31 to June 1) accurately account for months with 28, 29, 30, or 31 days.
- Leap years: Daily schedules correctly advance from February 28 to February 29 in leap years (such as 2028), and from February 28 to March 1 in non-leap years (such as 2027). Schedules targeting February 29 specifically (`0 0 29 2 *`) skip non-leap years and resolve to the next leap year.
- Daylight saving shifts: Because expressions evaluate against UTC timestamps without local timezone ambiguity, transitions across DST shifts occur predictably without duplicate or skipped hours.
