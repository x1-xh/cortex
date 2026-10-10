//! Autonomous background job scheduling engine and persistent triggers.
//!
//! Supports standard 5-field cron schedules, one-shot timers, SQLite-backed
//! persistence across restarts, and execution overlap policies (`skip`, `queue`, `replace`).

pub mod cron;

pub use cron::{CronExpression, CronField, CronParseError, Schedule};

use chrono::{DateTime, Utc};
use cortex_core::{CortexError, JobId, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::storage::RunStore;

/// Execution overlap policy when a scheduled trigger fires while a previous run is still active.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OverlapPolicy {
    /// Skip the current trigger if a previous run is still in progress.
    #[default]
    Skip,
    /// Queue the trigger to execute immediately after the active run completes.
    Queue,
    /// Cancel/terminate the currently active run and start the new execution.
    Replace,
}

impl OverlapPolicy {
    /// Return the string representation of the policy.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Skip => "skip",
            Self::Queue => "queue",
            Self::Replace => "replace",
        }
    }
}

impl fmt::Display for OverlapPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl FromStr for OverlapPolicy {
    type Err = CortexError;

    fn from_str(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "skip" => Ok(Self::Skip),
            "queue" => Ok(Self::Queue),
            "replace" => Ok(Self::Replace),
            other => Err(CortexError::Validation(format!(
                "invalid overlap policy '{}': expected 'skip', 'queue', or 'replace'",
                other
            ))),
        }
    }
}

/// Lifecycle status of a scheduled job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JobStatus {
    /// Job is active and will fire at scheduled intervals.
    #[default]
    Active,
    /// Job execution is paused.
    Paused,
    /// Job was a one-shot timer and has completed execution.
    Completed,
}

impl JobStatus {
    /// Return the string representation of the job status.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Paused => "paused",
            Self::Completed => "completed",
        }
    }
}

impl fmt::Display for JobStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl FromStr for JobStatus {
    type Err = CortexError;

    fn from_str(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "active" => Ok(Self::Active),
            "paused" => Ok(Self::Paused),
            "completed" => Ok(Self::Completed),
            other => Err(CortexError::Validation(format!(
                "invalid job status '{}': expected 'active', 'paused', or 'completed'",
                other
            ))),
        }
    }
}

/// Execution status of an individual job run record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JobRunStatus {
    /// Run is currently executing.
    Running,
    /// Run finished successfully.
    Completed,
    /// Run failed with an error.
    Failed,
    /// Run was skipped due to overlap policy.
    Skipped,
    /// Run was cancelled or replaced by a newer trigger.
    Cancelled,
}

impl JobRunStatus {
    /// Return the string representation of the run status.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
            Self::Cancelled => "cancelled",
        }
    }
}

impl fmt::Display for JobRunStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl FromStr for JobRunStatus {
    type Err = CortexError;

    fn from_str(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "running" => Ok(Self::Running),
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
            "skipped" => Ok(Self::Skipped),
            "cancelled" => Ok(Self::Cancelled),
            other => Err(CortexError::Validation(format!(
                "invalid job run status '{}'",
                other
            ))),
        }
    }
}

/// Persistent definition of a registered scheduled job.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScheduledJob {
    /// Unique identifier for the job.
    pub id: JobId,
    /// Human-readable job name or description.
    pub name: String,
    /// Cron expression or one-shot timestamp.
    pub schedule: String,
    /// Task prompt to execute when triggered.
    pub prompt: String,
    /// Overlap policy handling concurrent triggers.
    pub overlap_policy: OverlapPolicy,
    /// Current job lifecycle status.
    pub status: JobStatus,
    /// Next scheduled execution timestamp.
    pub next_run_at: Option<DateTime<Utc>>,
    /// Timestamp of most recent execution start.
    pub last_run_at: Option<DateTime<Utc>>,
    /// Creation timestamp.
    pub created_at: DateTime<Utc>,
    /// Last updated timestamp.
    pub updated_at: DateTime<Utc>,
}

/// Recorded history of a single scheduled job execution.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobRunRecord {
    /// Unique execution run identifier.
    pub id: String,
    /// Associated scheduled job identifier.
    pub job_id: JobId,
    /// Run execution status.
    pub status: JobRunStatus,
    /// Start timestamp.
    pub started_at: DateTime<Utc>,
    /// Finished timestamp, if completed.
    pub finished_at: Option<DateTime<Utc>>,
    /// Execution duration in milliseconds.
    pub duration_ms: Option<u64>,
    /// Run output summary.
    pub output: Option<String>,
    /// Error message if run failed or was cancelled.
    pub error: Option<String>,
}

/// Execution statistics for a scheduled cron job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct CronJobStats {
    /// Total number of recorded execution attempts.
    pub total_runs: usize,
    /// Number of runs completed successfully.
    pub success_runs: usize,
    /// Number of runs that failed with an error.
    pub failure_runs: usize,
}

/// Outcome of triggering a scheduled job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TriggerResult {
    /// New execution run was started.
    Started {
        /// Generated run identifier.
        run_id: String,
        /// Triggered job identifier.
        job_id: JobId,
    },
    /// Execution was skipped due to an active run (`OverlapPolicy::Skip`).
    Skipped {
        /// Skipped job identifier.
        job_id: JobId,
        /// Reason for skip.
        reason: String,
    },
    /// Trigger was queued because an active run is in progress (`OverlapPolicy::Queue`).
    Queued {
        /// Queued job identifier.
        job_id: JobId,
    },
    /// Active run was cancelled and replaced by a new run (`OverlapPolicy::Replace`).
    Replaced {
        /// ID of run that was cancelled.
        cancelled_run_id: String,
        /// ID of new run that was started.
        new_run_id: String,
        /// Job identifier.
        job_id: JobId,
    },
}

static CRON_RUN_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn generate_cron_run_id() -> String {
    let ts = Utc::now().timestamp_micros();
    let cnt = CRON_RUN_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("cronrun_{}_{:04x}", ts, cnt & 0xffff)
}

/// Autonomous background scheduler engine managing jobs, triggers, and overlap policies.
pub struct SchedulerEngine {
    store: Arc<RunStore>,
    queued_triggers: Mutex<HashMap<JobId, usize>>,
}

impl SchedulerEngine {
    /// Create a new [`SchedulerEngine`] backed by the provided [`RunStore`].
    pub fn new(store: Arc<RunStore>) -> Self {
        Self {
            store,
            queued_triggers: Mutex::new(HashMap::new()),
        }
    }

    /// Register and persist a new scheduled job.
    pub fn register_job(
        &self,
        name: impl Into<String>,
        schedule_str: &str,
        prompt: impl Into<String>,
        overlap_policy: OverlapPolicy,
    ) -> Result<ScheduledJob> {
        let name = name.into();
        let prompt = prompt.into();

        // Validate schedule syntax
        let parsed_schedule = Schedule::parse(schedule_str).map_err(|e| {
            CortexError::Validation(format!("invalid schedule '{}': {}", schedule_str, e))
        })?;

        let now = Utc::now();
        let next_run_at = parsed_schedule.next_run_after(&now);

        let job = ScheduledJob {
            id: JobId::generate(),
            name,
            schedule: schedule_str.trim().to_string(),
            prompt,
            overlap_policy,
            status: JobStatus::Active,
            next_run_at,
            last_run_at: None,
            created_at: now,
            updated_at: now,
        };

        self.store.save_cron_job(&job)?;
        Ok(job)
    }

    /// Retrieve a registered job by its identifier.
    pub fn get_job(&self, id: &JobId) -> Result<Option<ScheduledJob>> {
        self.store.get_cron_job(id)
    }

    /// List all registered scheduled jobs.
    pub fn list_jobs(&self) -> Result<Vec<ScheduledJob>> {
        self.store.list_cron_jobs()
    }

    /// Delete a registered scheduled job and its execution history.
    pub fn delete_job(&self, id: &JobId) -> Result<bool> {
        let mut queued = self.queued_triggers.lock().unwrap();
        queued.remove(id);
        self.store.delete_cron_job(id)
    }

    /// Pause an active scheduled job.
    pub fn pause_job(&self, id: &JobId) -> Result<bool> {
        if let Some(mut job) = self.store.get_cron_job(id)? {
            job.status = JobStatus::Paused;
            job.updated_at = Utc::now();
            self.store.save_cron_job(&job)?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Resume a paused scheduled job, recalculating its next run time.
    pub fn resume_job(&self, id: &JobId) -> Result<bool> {
        if let Some(mut job) = self.store.get_cron_job(id)? {
            let sched = Schedule::parse(&job.schedule)
                .map_err(|e| CortexError::Validation(format!("invalid schedule: {}", e)))?;
            let now = Utc::now();
            job.status = JobStatus::Active;
            job.next_run_at = sched.next_run_after(&now);
            job.updated_at = now;
            self.store.save_cron_job(&job)?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Calculate wall-clock duration until the next active job trigger fires.
    ///
    /// Returns `None` if no active jobs are registered with a pending `next_run_at`.
    /// Used by schedulers to sleep precisely without busy polling loops.
    pub fn time_until_next_job(&self, now: DateTime<Utc>) -> Result<Option<Duration>> {
        let jobs = self.store.list_cron_jobs()?;
        let earliest = jobs
            .into_iter()
            .filter(|j| j.status == JobStatus::Active)
            .filter_map(|j| j.next_run_at)
            .min();

        match earliest {
            Some(target) if target > now => {
                let diff_ms = (target - now).num_milliseconds().max(0) as u64;
                Ok(Some(Duration::from_millis(diff_ms)))
            }
            Some(_) => Ok(Some(Duration::from_millis(0))),
            None => Ok(None),
        }
    }

    /// Trigger a scheduled job immediately, applying its configured overlap policy.
    pub fn trigger_job(&self, id: &JobId, now: DateTime<Utc>) -> Result<TriggerResult> {
        let job = self
            .store
            .get_cron_job(id)?
            .ok_or_else(|| CortexError::NotFound(format!("scheduled job '{}'", id)))?;

        let sched = Schedule::parse(&job.schedule)
            .map_err(|e| CortexError::Validation(format!("invalid schedule: {}", e)))?;

        let active_run = self.store.get_active_cron_run(id)?;

        if let Some(running) = active_run {
            match job.overlap_policy {
                OverlapPolicy::Skip => {
                    // Update next run time and record skipped execution
                    let next = sched.next_run_after(&now);
                    let skip_run_id = generate_cron_run_id();
                    let now_str = now.to_rfc3339();

                    self.store
                        .record_cron_run_start(&skip_run_id, id, &now_str)?;
                    self.store.record_cron_run_finish(
                        &skip_run_id,
                        JobRunStatus::Skipped.as_str(),
                        &now_str,
                        Some(0),
                        None,
                        Some("Execution skipped: previous run is still active"),
                    )?;

                    let next_str = next.as_ref().map(|dt| dt.to_rfc3339());
                    self.store.update_cron_job_schedule(
                        id,
                        next_str.as_deref(),
                        Some(&job.last_run_at.unwrap_or(now).to_rfc3339()),
                        job.status.as_str(),
                    )?;

                    return Ok(TriggerResult::Skipped {
                        job_id: id.clone(),
                        reason: "Active run in progress".to_string(),
                    });
                }
                OverlapPolicy::Queue => {
                    // Increment queued count for this job
                    let mut queued = self.queued_triggers.lock().unwrap();
                    let count = queued.entry(id.clone()).or_insert(0);
                    *count += 1;

                    // Update next run time
                    let next = sched.next_run_after(&now);
                    let next_str = next.as_ref().map(|dt| dt.to_rfc3339());
                    self.store.update_cron_job_schedule(
                        id,
                        next_str.as_deref(),
                        Some(&job.last_run_at.unwrap_or(now).to_rfc3339()),
                        job.status.as_str(),
                    )?;

                    return Ok(TriggerResult::Queued { job_id: id.clone() });
                }
                OverlapPolicy::Replace => {
                    // Cancel active run
                    let now_str = now.to_rfc3339();
                    self.store.cancel_cron_run(
                        &running.id,
                        &now_str,
                        "Replaced by newer scheduled execution",
                    )?;

                    // Start new run immediately
                    let new_run_id = generate_cron_run_id();
                    self.store
                        .record_cron_run_start(&new_run_id, id, &now_str)?;

                    let next = sched.next_run_after(&now);
                    let next_status = if sched.is_one_shot() && next.is_none() {
                        JobStatus::Completed
                    } else {
                        job.status
                    };

                    let next_str = next.as_ref().map(|dt| dt.to_rfc3339());
                    self.store.update_cron_job_schedule(
                        id,
                        next_str.as_deref(),
                        Some(&now_str),
                        next_status.as_str(),
                    )?;

                    return Ok(TriggerResult::Replaced {
                        cancelled_run_id: running.id,
                        new_run_id,
                        job_id: id.clone(),
                    });
                }
            }
        }

        // No active run: start execution normally
        let run_id = generate_cron_run_id();
        let now_str = now.to_rfc3339();
        self.store.record_cron_run_start(&run_id, id, &now_str)?;

        let next = sched.next_run_after(&now);
        let next_status = if sched.is_one_shot() && next.is_none() {
            JobStatus::Completed
        } else {
            job.status
        };

        let next_str = next.as_ref().map(|dt| dt.to_rfc3339());
        self.store.update_cron_job_schedule(
            id,
            next_str.as_deref(),
            Some(&now_str),
            next_status.as_str(),
        )?;

        Ok(TriggerResult::Started {
            run_id,
            job_id: id.clone(),
        })
    }

    /// Complete an active run, and if triggers were queued for this job, start the queued execution.
    pub fn finish_job_run(
        &self,
        run_id: &str,
        job_id: &JobId,
        status: JobRunStatus,
        output: Option<String>,
        error: Option<String>,
    ) -> Result<Option<TriggerResult>> {
        let now = Utc::now();
        let now_str = now.to_rfc3339();

        self.store.record_cron_run_finish(
            run_id,
            status.as_str(),
            &now_str,
            None,
            output.as_deref(),
            error.as_deref(),
        )?;

        // Check if there are queued runs for this job
        let mut queued = self.queued_triggers.lock().unwrap();
        if let Some(count) = queued.get_mut(job_id) {
            if *count > 0 {
                *count -= 1;
                drop(queued);
                let next_trigger = self.trigger_job(job_id, Utc::now())?;
                return Ok(Some(next_trigger));
            }
        }

        Ok(None)
    }

    /// Process all jobs whose scheduled trigger time is due (`next_run_at <= now`).
    pub fn tick(&self, now: DateTime<Utc>) -> Result<Vec<TriggerResult>> {
        let jobs = self.store.list_cron_jobs()?;
        let mut results = Vec::new();

        for job in jobs {
            if job.status == JobStatus::Active {
                if let Some(next_run) = job.next_run_at {
                    if next_run <= now {
                        let res = self.trigger_job(&job.id, now)?;
                        results.push(res);
                    }
                }
            }
        }

        Ok(results)
    }

    /// Retrieve recorded run executions for a scheduled job.
    pub fn list_job_runs(&self, job_id: &JobId, limit: usize) -> Result<Vec<JobRunRecord>> {
        self.store.list_cron_job_runs(job_id, limit)
    }

    /// Find a registered scheduled job by identifier or unique prefix.
    pub fn find_job(&self, id_or_prefix: &str) -> Result<Option<ScheduledJob>> {
        self.store.find_cron_job(id_or_prefix)
    }

    /// Retrieve execution statistics (total, success, failure) for a scheduled job.
    pub fn get_job_stats(&self, job_id: &JobId) -> Result<CronJobStats> {
        self.store.get_cron_job_stats(job_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_overlap_policy_from_str() {
        assert_eq!(
            OverlapPolicy::from_str("skip").unwrap(),
            OverlapPolicy::Skip
        );
        assert_eq!(
            OverlapPolicy::from_str("QUEUE").unwrap(),
            OverlapPolicy::Queue
        );
        assert_eq!(
            OverlapPolicy::from_str("replace").unwrap(),
            OverlapPolicy::Replace
        );
        assert!(OverlapPolicy::from_str("invalid").is_err());
    }

    #[test]
    fn test_job_lifecycle_and_triggers() {
        let store = Arc::new(RunStore::in_memory().unwrap());
        let engine = SchedulerEngine::new(store);

        let job = engine
            .register_job(
                "Test Job",
                "*/5 * * * *",
                "Run diagnostics",
                OverlapPolicy::Skip,
            )
            .unwrap();

        assert_eq!(job.name, "Test Job");
        assert_eq!(job.status, JobStatus::Active);
        assert!(job.next_run_at.is_some());

        let retrieved = engine.get_job(&job.id).unwrap().unwrap();
        assert_eq!(retrieved.id, job.id);

        let listed = engine.list_jobs().unwrap();
        assert_eq!(listed.len(), 1);

        // Trigger job first time
        let now = Utc::now();
        let trigger1 = engine.trigger_job(&job.id, now).unwrap();
        let run_id = match trigger1 {
            TriggerResult::Started { run_id, .. } => run_id,
            _ => panic!("expected started run"),
        };

        // OverlapPolicy::Skip when active run exists
        let trigger2 = engine.trigger_job(&job.id, now).unwrap();
        assert!(matches!(trigger2, TriggerResult::Skipped { .. }));

        // Finish first run
        engine
            .finish_job_run(
                &run_id,
                &job.id,
                JobRunStatus::Completed,
                Some("ok".into()),
                None,
            )
            .unwrap();

        let runs = engine.list_job_runs(&job.id, 10).unwrap();
        assert_eq!(runs.len(), 2); // 1 completed, 1 skipped

        // Delete job
        assert!(engine.delete_job(&job.id).unwrap());
        assert_eq!(engine.list_jobs().unwrap().len(), 0);
    }

    #[test]
    fn test_overlap_policy_replace() {
        let store = Arc::new(RunStore::in_memory().unwrap());
        let engine = SchedulerEngine::new(store);

        let job = engine
            .register_job(
                "Replace Job",
                "*/10 * * * *",
                "Prompt",
                OverlapPolicy::Replace,
            )
            .unwrap();

        let now = Utc::now();
        let res1 = engine.trigger_job(&job.id, now).unwrap();
        let first_run_id = match res1 {
            TriggerResult::Started { run_id, .. } => run_id,
            _ => panic!("expected started"),
        };

        // Trigger while first is running -> should replace
        let res2 = engine.trigger_job(&job.id, now).unwrap();
        match res2 {
            TriggerResult::Replaced {
                cancelled_run_id,
                new_run_id,
                ..
            } => {
                assert_eq!(cancelled_run_id, first_run_id);
                assert_ne!(new_run_id, first_run_id);
            }
            _ => panic!("expected replaced"),
        }

        let runs = engine.list_job_runs(&job.id, 10).unwrap();
        let cancelled = runs.iter().find(|r| r.id == first_run_id).unwrap();
        assert_eq!(cancelled.status, JobRunStatus::Cancelled);
    }

    #[test]
    fn test_overlap_policy_queue() {
        let store = Arc::new(RunStore::in_memory().unwrap());
        let engine = SchedulerEngine::new(store);

        let job = engine
            .register_job("Queue Job", "*/10 * * * *", "Prompt", OverlapPolicy::Queue)
            .unwrap();

        let now = Utc::now();
        let res1 = engine.trigger_job(&job.id, now).unwrap();
        let first_run_id = match res1 {
            TriggerResult::Started { run_id, .. } => run_id,
            _ => panic!("expected started"),
        };

        // Trigger while running -> Queued
        let res2 = engine.trigger_job(&job.id, now).unwrap();
        assert!(matches!(res2, TriggerResult::Queued { .. }));

        // Finishing first run should automatically trigger the queued run!
        let next = engine
            .finish_job_run(&first_run_id, &job.id, JobRunStatus::Completed, None, None)
            .unwrap();

        assert!(matches!(next, Some(TriggerResult::Started { .. })));
    }
}
