//! Integration tests for persistent cron scheduling engine, one-shot timers,
//! execution overlap policies, and restart recovery.

use chrono::{Duration, TimeZone, Utc};
use cortex_core::JobId;
use cortex_runtime::scheduler::{
    CronExpression, JobRunStatus, JobStatus, OverlapPolicy, Schedule, SchedulerEngine,
    TriggerResult,
};
use cortex_runtime::storage::RunStore;
use std::sync::Arc;

#[test]
fn test_cron_parsing_comprehensive() {
    // 5-field standard expressions
    let every_5_min = CronExpression::parse("*/5 * * * *").unwrap();
    assert!(every_5_min.minute.matches(0));
    assert!(every_5_min.minute.matches(5));
    assert!(!every_5_min.minute.matches(6));
    assert!(every_5_min.hour.is_wildcard());

    // Macros
    let hourly = CronExpression::parse("@hourly").unwrap();
    assert!(hourly.minute.matches(0));
    assert!(hourly.hour.is_wildcard());

    let midnight = CronExpression::parse("@midnight").unwrap();
    assert!(midnight.minute.matches(0));
    assert!(midnight.hour.matches(0));

    // Named months and days
    let complex = CronExpression::parse("30 8 15 JAN,JUN MON-FRI").unwrap();
    assert!(complex.minute.matches(30));
    assert!(complex.hour.matches(8));
    assert!(complex.day_of_month.matches(15));
    assert!(complex.month.matches(1));
    assert!(complex.month.matches(6));
    assert!(!complex.month.matches(2));
    assert!(complex.day_of_week.matches(1)); // Mon
    assert!(complex.day_of_week.matches(5)); // Fri
    assert!(!complex.day_of_week.matches(0)); // Sun
}

#[test]
fn test_next_execution_calculation_without_busy_polling() {
    let cron = CronExpression::parse("0 9 * * 1-5").unwrap(); // 9:00 AM on weekdays
    let friday = Utc.with_ymd_and_hms(2026, 10, 9, 17, 30, 0).unwrap(); // Friday 5:30 PM

    let next = cron.next_run_after(&friday).unwrap();
    // Should jump directly to next Monday Oct 12 at 09:00:00
    assert_eq!(next, Utc.with_ymd_and_hms(2026, 10, 12, 9, 0, 0).unwrap());
}

#[test]
fn test_one_shot_timer_schedule() {
    let target = Utc::now() + Duration::hours(2);
    let sched_str = format!("@once {}", target.to_rfc3339());
    let sched = Schedule::parse(&sched_str).unwrap();

    assert!(sched.is_one_shot());
    let before = Utc::now();
    assert_eq!(sched.next_run_after(&before), Some(target));

    let after = target + Duration::minutes(1);
    assert_eq!(sched.next_run_after(&after), None);
}

#[test]
fn test_sqlite_persistence_across_restarts() {
    let tmp_dir =
        std::env::temp_dir().join(format!("cortex_cron_persist_test_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp_dir);
    let db_path = tmp_dir.join("cortex_cron.db");

    let job_id: JobId;

    // Phase 1: Create store, register job, start a run, and drop engine/store
    {
        let store = Arc::new(RunStore::open(&db_path).unwrap());
        let engine = SchedulerEngine::new(store);

        let job = engine
            .register_job(
                "Nightly Cleanup",
                "0 2 * * *",
                "Clean up temporary build artifacts",
                OverlapPolicy::Queue,
            )
            .unwrap();

        job_id = job.id.clone();

        let trigger = engine.trigger_job(&job_id, Utc::now()).unwrap();
        match trigger {
            TriggerResult::Started { run_id, .. } => {
                engine
                    .finish_job_run(
                        &run_id,
                        &job_id,
                        JobRunStatus::Completed,
                        Some("Cleaned 45 files".into()),
                        None,
                    )
                    .unwrap();
            }
            _ => panic!("expected started"),
        }
    }

    // Phase 2: Reopen from disk, verify job and run history persist completely
    {
        let store = Arc::new(RunStore::open(&db_path).unwrap());
        let engine = SchedulerEngine::new(store);

        let job = engine.get_job(&job_id).unwrap().expect("job must persist");
        assert_eq!(job.name, "Nightly Cleanup");
        assert_eq!(job.schedule, "0 2 * * *");
        assert_eq!(job.prompt, "Clean up temporary build artifacts");
        assert_eq!(job.overlap_policy, OverlapPolicy::Queue);
        assert_eq!(job.status, JobStatus::Active);

        let runs = engine.list_job_runs(&job_id, 10).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, JobRunStatus::Completed);
        assert_eq!(runs[0].output.as_deref(), Some("Cleaned 45 files"));
    }

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

#[test]
fn test_overlap_policy_skip_behavior() {
    let store = Arc::new(RunStore::in_memory().unwrap());
    let engine = SchedulerEngine::new(store);

    let job = engine
        .register_job("Skip Job", "*/5 * * * *", "Prompt", OverlapPolicy::Skip)
        .unwrap();

    let now = Utc::now();
    let res1 = engine.trigger_job(&job.id, now).unwrap();
    let run_id = match res1 {
        TriggerResult::Started { run_id, .. } => run_id,
        _ => panic!("expected started"),
    };

    // Second trigger while first is running -> Skipped
    let res2 = engine.trigger_job(&job.id, now).unwrap();
    assert!(matches!(res2, TriggerResult::Skipped { .. }));

    // Complete run
    engine
        .finish_job_run(&run_id, &job.id, JobRunStatus::Completed, None, None)
        .unwrap();

    let runs = engine.list_job_runs(&job.id, 10).unwrap();
    assert_eq!(runs.len(), 2);
    assert!(runs.iter().any(|r| r.status == JobRunStatus::Skipped));
    assert!(runs.iter().any(|r| r.status == JobRunStatus::Completed));
}

#[test]
fn test_overlap_policy_queue_behavior() {
    let store = Arc::new(RunStore::in_memory().unwrap());
    let engine = SchedulerEngine::new(store);

    let job = engine
        .register_job("Queue Job", "*/5 * * * *", "Prompt", OverlapPolicy::Queue)
        .unwrap();

    let now = Utc::now();
    let res1 = engine.trigger_job(&job.id, now).unwrap();
    let first_run_id = match res1 {
        TriggerResult::Started { run_id, .. } => run_id,
        _ => panic!("expected started"),
    };

    // Second trigger while first is running -> Queued
    let res2 = engine.trigger_job(&job.id, now).unwrap();
    assert!(matches!(res2, TriggerResult::Queued { .. }));

    // Finish first run -> should automatically trigger queued execution!
    let next_run = engine
        .finish_job_run(&first_run_id, &job.id, JobRunStatus::Completed, None, None)
        .unwrap();

    assert!(matches!(next_run, Some(TriggerResult::Started { .. })));
}

#[test]
fn test_overlap_policy_replace_behavior() {
    let store = Arc::new(RunStore::in_memory().unwrap());
    let engine = SchedulerEngine::new(store);

    let job = engine
        .register_job(
            "Replace Job",
            "*/5 * * * *",
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

    // Second trigger while running -> Replaces active run
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
fn test_time_until_next_job_and_tick() {
    let store = Arc::new(RunStore::in_memory().unwrap());
    let engine = SchedulerEngine::new(store);

    // No jobs yet
    assert_eq!(engine.time_until_next_job(Utc::now()).unwrap(), None);

    let target = Utc::now() + Duration::seconds(30);
    let sched_str = format!("@once {}", target.to_rfc3339());
    let job = engine
        .register_job("One Shot", &sched_str, "Execute once", OverlapPolicy::Skip)
        .unwrap();

    let duration = engine.time_until_next_job(Utc::now()).unwrap().unwrap();
    assert!(duration.as_secs() <= 30);

    // Tick before target time: nothing triggered
    let triggered_early = engine.tick(Utc::now()).unwrap();
    assert!(triggered_early.is_empty());

    // Tick at/after target time: triggers job
    let triggered = engine.tick(target + Duration::seconds(1)).unwrap();
    assert_eq!(triggered.len(), 1);
    assert!(matches!(triggered[0], TriggerResult::Started { .. }));

    // After triggering one-shot, status is Completed and next_run_at is None
    let updated_job = engine.get_job(&job.id).unwrap().unwrap();
    assert_eq!(updated_job.status, JobStatus::Completed);
    assert_eq!(updated_job.next_run_at, None);
}

#[test]
fn test_find_job_prefix_and_stats() {
    let store = Arc::new(RunStore::in_memory().unwrap());
    let engine = SchedulerEngine::new(store);

    let job = engine
        .register_job("Prefix Job", "0 12 * * *", "Prompt", OverlapPolicy::Skip)
        .unwrap();

    let job_id_str = job.id.as_str();
    let prefix = &job_id_str[..job_id_str.len().min(8)];

    // Exact lookup
    let found_exact = engine.find_job(job_id_str).unwrap().unwrap();
    assert_eq!(found_exact.id, job.id);

    // Prefix lookup
    let found_prefix = engine.find_job(prefix).unwrap().unwrap();
    assert_eq!(found_prefix.id, job.id);

    // Nonexistent lookup
    assert_eq!(engine.find_job("nonexistent").unwrap(), None);

    // Empty stats initially
    let stats = engine.get_job_stats(&job.id).unwrap();
    assert_eq!(stats.total_runs, 0);
    assert_eq!(stats.success_runs, 0);
    assert_eq!(stats.failure_runs, 0);

    // Add runs: 1 completed, 1 failed
    let now = Utc::now();
    let res1 = engine.trigger_job(&job.id, now).unwrap();
    if let TriggerResult::Started { run_id, .. } = res1 {
        engine
            .finish_job_run(&run_id, &job.id, JobRunStatus::Completed, None, None)
            .unwrap();
    }

    let res2 = engine.trigger_job(&job.id, now).unwrap();
    if let TriggerResult::Started { run_id, .. } = res2 {
        engine
            .finish_job_run(
                &run_id,
                &job.id,
                JobRunStatus::Failed,
                None,
                Some("Error message".into()),
            )
            .unwrap();
    }

    let updated_stats = engine.get_job_stats(&job.id).unwrap();
    assert_eq!(updated_stats.total_runs, 2);
    assert_eq!(updated_stats.success_runs, 1);
    assert_eq!(updated_stats.failure_runs, 1);
}
