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

struct TempDirGuard(std::path::PathBuf);

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

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
    let _guard = TempDirGuard(tmp_dir.clone());
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
fn test_downtime_recovery_skip_policy() {
    let tmp_dir =
        std::env::temp_dir().join(format!("cortex_cron_downtime_skip_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp_dir);
    let _guard = TempDirGuard(tmp_dir.clone());
    let db_path = tmp_dir.join("cortex_downtime.db");

    let job_id: JobId;
    let t0 = Utc.with_ymd_and_hms(2026, 10, 10, 10, 0, 0).unwrap();

    // Phase 1: Register job and trigger run at T0
    {
        let store = Arc::new(RunStore::open(&db_path).unwrap());
        let engine = SchedulerEngine::new(store);

        let job = engine
            .register_job(
                "Downtime Skip Job",
                "*/10 * * * *",
                "Prompt",
                OverlapPolicy::Skip,
            )
            .unwrap();
        job_id = job.id.clone();

        let trigger = engine.trigger_job(&job_id, t0).unwrap();
        let run_id = match trigger {
            TriggerResult::Started { run_id, .. } => run_id,
            _ => panic!("expected started"),
        };

        engine
            .finish_job_run(&run_id, &job_id, JobRunStatus::Completed, None, None)
            .unwrap();
    }

    // Phase 2: Engine restarts after downtime at 10:45:00 (spanned 10:10, 10:20, 10:30, 10:40)
    let t_resume = Utc.with_ymd_and_hms(2026, 10, 10, 10, 45, 0).unwrap();
    {
        let store = Arc::new(RunStore::open(&db_path).unwrap());
        let engine = SchedulerEngine::new(store);

        let job_before = engine.get_job(&job_id).unwrap().unwrap();
        assert!(job_before.next_run_at.unwrap() <= t_resume);

        // Tick triggers missed job once at resume time
        let results = engine.tick(t_resume).unwrap();
        assert_eq!(results.len(), 1);
        let active_run_id = match &results[0] {
            TriggerResult::Started { run_id, .. } => run_id.clone(),
            _ => panic!("expected started"),
        };

        // Next scheduled run time advances to next future interval (10:50:00)
        let job_after = engine.get_job(&job_id).unwrap().unwrap();
        assert_eq!(
            job_after.next_run_at,
            Some(Utc.with_ymd_and_hms(2026, 10, 10, 10, 50, 0).unwrap())
        );

        // Immediate tick while active run is in flight records skipped notice under Skip policy
        let second_tick = engine.tick(t_resume + Duration::seconds(30)).unwrap();
        assert_eq!(second_tick.len(), 0); // next_run_at is 10:50, not due at 10:45:30

        engine
            .finish_job_run(&active_run_id, &job_id, JobRunStatus::Completed, None, None)
            .unwrap();

        let runs = engine.list_job_runs(&job_id, 10).unwrap();
        assert_eq!(runs.len(), 2);
        assert!(runs.iter().all(|r| r.status == JobRunStatus::Completed));
    }
}

#[test]
fn test_queue_policy_multi_trigger_drain_with_failure_recovery() {
    let store = Arc::new(RunStore::in_memory().unwrap());
    let engine = SchedulerEngine::new(store);

    let t0 = Utc.with_ymd_and_hms(2026, 10, 10, 12, 0, 0).unwrap();
    let job = engine
        .register_job(
            "Queue Catchup",
            "*/15 * * * *",
            "Prompt",
            OverlapPolicy::Queue,
        )
        .unwrap();

    // Start initial run
    let res1 = engine.trigger_job(&job.id, t0).unwrap();
    let run_1 = match res1 {
        TriggerResult::Started { run_id, .. } => run_id,
        _ => panic!("expected started"),
    };

    // Two ticks arrive while run_1 is in progress
    let res2 = engine
        .trigger_job(&job.id, t0 + Duration::minutes(15))
        .unwrap();
    assert!(matches!(res2, TriggerResult::Queued { .. }));

    let res3 = engine
        .trigger_job(&job.id, t0 + Duration::minutes(30))
        .unwrap();
    assert!(matches!(res3, TriggerResult::Queued { .. }));

    // Finish run 1: starts run 2 automatically
    let run_2_res = engine
        .finish_job_run(&run_1, &job.id, JobRunStatus::Completed, None, None)
        .unwrap()
        .expect("must start queued run 2");

    let run_2 = match run_2_res {
        TriggerResult::Started { run_id, .. } => run_id,
        _ => panic!("expected started run 2"),
    };

    // Finish run 2 as Failed: error message is recorded and starts run 3 automatically
    let run_3_res = engine
        .finish_job_run(
            &run_2,
            &job.id,
            JobRunStatus::Failed,
            None,
            Some("Task process crashed with exit code 1".into()),
        )
        .unwrap()
        .expect("must start queued run 3 even when previous run failed");

    let run_3 = match run_3_res {
        TriggerResult::Started { run_id, .. } => run_id,
        _ => panic!("expected started run 3"),
    };

    // Finish run 3: queue is drained
    let no_more = engine
        .finish_job_run(&run_3, &job.id, JobRunStatus::Completed, None, None)
        .unwrap();
    assert_eq!(no_more, None);

    let runs = engine.list_job_runs(&job.id, 10).unwrap();
    assert_eq!(runs.len(), 3);

    let rec_1 = runs.iter().find(|r| r.id == run_1).unwrap();
    assert_eq!(rec_1.status, JobRunStatus::Completed);

    let rec_2 = runs.iter().find(|r| r.id == run_2).unwrap();
    assert_eq!(rec_2.status, JobRunStatus::Failed);
    assert_eq!(
        rec_2.error.as_deref(),
        Some("Task process crashed with exit code 1")
    );

    let rec_3 = runs.iter().find(|r| r.id == run_3).unwrap();
    assert_eq!(rec_3.status, JobRunStatus::Completed);
}

#[test]
fn test_pause_and_resume_job_lifecycle_and_restart() {
    let tmp_dir =
        std::env::temp_dir().join(format!("cortex_cron_pause_resume_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp_dir);
    let _guard = TempDirGuard(tmp_dir.clone());
    let db_path = tmp_dir.join("cortex_pause.db");

    let job_id: JobId;
    let t0 = Utc.with_ymd_and_hms(2026, 10, 10, 10, 0, 0).unwrap();

    // Phase 1: Register job, pause it, verify tick does not trigger
    {
        let store = Arc::new(RunStore::open(&db_path).unwrap());
        let engine = SchedulerEngine::new(store);

        let job = engine
            .register_job("Pausable Job", "*/5 * * * *", "Prompt", OverlapPolicy::Skip)
            .unwrap();
        job_id = job.id.clone();

        assert!(engine.pause_job(&job_id).unwrap());
        let paused_job = engine.get_job(&job_id).unwrap().unwrap();
        assert_eq!(paused_job.status, JobStatus::Paused);

        // Tick at due time does NOT trigger paused job
        let triggered = engine.tick(t0 + Duration::minutes(10)).unwrap();
        assert!(triggered.is_empty());
    }

    // Phase 2: Reopen store after restart, verify status persists as Paused, then resume
    {
        let store = Arc::new(RunStore::open(&db_path).unwrap());
        let engine = SchedulerEngine::new(store);

        let job = engine.get_job(&job_id).unwrap().unwrap();
        assert_eq!(job.status, JobStatus::Paused);

        let triggered = engine.tick(t0 + Duration::minutes(20)).unwrap();
        assert!(triggered.is_empty());

        assert!(engine.resume_job(&job_id).unwrap());
        let resumed = engine.get_job(&job_id).unwrap().unwrap();
        assert_eq!(resumed.status, JobStatus::Active);
        assert!(resumed.next_run_at.is_some());

        // Now tick triggers active job
        let due_time = resumed.next_run_at.unwrap();
        let triggered_after = engine.tick(due_time).unwrap();
        assert_eq!(triggered_after.len(), 1);
        assert!(matches!(triggered_after[0], TriggerResult::Started { .. }));
    }
}

#[test]
fn test_cancellation_and_deletion_of_active_jobs() {
    let store = Arc::new(RunStore::in_memory().unwrap());
    let engine = SchedulerEngine::new(store);

    let job = engine
        .register_job(
            "Active Delete Job",
            "*/5 * * * *",
            "Prompt",
            OverlapPolicy::Queue,
        )
        .unwrap();

    let now = Utc::now();
    let res = engine.trigger_job(&job.id, now).unwrap();
    let run_id = match res {
        TriggerResult::Started { run_id, .. } => run_id,
        _ => panic!("expected started"),
    };

    // Queue another trigger
    let queued = engine.trigger_job(&job.id, now).unwrap();
    assert!(matches!(queued, TriggerResult::Queued { .. }));

    // Delete job while run is active
    let deleted = engine.delete_job(&job.id).unwrap();
    assert!(deleted);

    // Job is removed
    assert_eq!(engine.get_job(&job.id).unwrap(), None);

    // Associated runs are deleted by cascade
    let runs = engine.list_job_runs(&job.id, 10).unwrap();
    assert!(runs.is_empty());

    // Completing orphaned run does not panic and returns no next trigger
    let result = engine
        .finish_job_run(&run_id, &job.id, JobRunStatus::Completed, None, None)
        .unwrap();
    assert_eq!(result, None);
}

#[test]
fn test_cancellation_and_replacement_graceful_transition() {
    let store = Arc::new(RunStore::in_memory().unwrap());
    let engine = SchedulerEngine::new(store);

    let job = engine
        .register_job(
            "Replace Cascade",
            "*/5 * * * *",
            "Prompt",
            OverlapPolicy::Replace,
        )
        .unwrap();

    let now = Utc::now();
    let res1 = engine.trigger_job(&job.id, now).unwrap();
    let run_a = match res1 {
        TriggerResult::Started { run_id, .. } => run_id,
        _ => panic!("expected started"),
    };

    // Second trigger cancels Run A and starts Run B
    let res2 = engine.trigger_job(&job.id, now).unwrap();
    let run_b = match res2 {
        TriggerResult::Replaced {
            cancelled_run_id,
            new_run_id,
            ..
        } => {
            assert_eq!(cancelled_run_id, run_a);
            new_run_id
        }
        _ => panic!("expected replaced"),
    };

    // Third trigger cancels Run B and starts Run C
    let res3 = engine.trigger_job(&job.id, now).unwrap();
    let run_c = match res3 {
        TriggerResult::Replaced {
            cancelled_run_id,
            new_run_id,
            ..
        } => {
            assert_eq!(cancelled_run_id, run_b);
            new_run_id
        }
        _ => panic!("expected replaced"),
    };

    // Finish Run C cleanly
    engine
        .finish_job_run(&run_c, &job.id, JobRunStatus::Completed, None, None)
        .unwrap();

    let runs = engine.list_job_runs(&job.id, 10).unwrap();
    assert_eq!(runs.len(), 3);

    let a_rec = runs.iter().find(|r| r.id == run_a).unwrap();
    assert_eq!(a_rec.status, JobRunStatus::Cancelled);
    assert!(a_rec.error.as_deref().unwrap().contains("Replaced"));

    let b_rec = runs.iter().find(|r| r.id == run_b).unwrap();
    assert_eq!(b_rec.status, JobRunStatus::Cancelled);
    assert!(b_rec.error.as_deref().unwrap().contains("Replaced"));

    let c_rec = runs.iter().find(|r| r.id == run_c).unwrap();
    assert_eq!(c_rec.status, JobRunStatus::Completed);
}

#[test]
fn test_timezone_offsets_and_calendar_boundary_transitions() {
    // 1. Month-end boundary: May 31 (31 days) to June 1 (30 days)
    let daily_midnight = CronExpression::parse("0 0 * * *").unwrap();
    let may_31 = Utc.with_ymd_and_hms(2026, 5, 31, 23, 50, 0).unwrap();
    let june_1 = daily_midnight.next_run_after(&may_31).unwrap();
    assert_eq!(june_1, Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0).unwrap());

    // 2. Leap year boundary: Feb 28, 2028 (leap year) -> Feb 29, 2028
    let feb_28_leap = Utc.with_ymd_and_hms(2028, 2, 28, 23, 0, 0).unwrap();
    let leap_next = daily_midnight.next_run_after(&feb_28_leap).unwrap();
    assert_eq!(
        leap_next,
        Utc.with_ymd_and_hms(2028, 2, 29, 0, 0, 0).unwrap()
    );

    // 3. Non-leap year boundary: Feb 28, 2027 (non-leap year) -> March 1, 2027
    let feb_28_non_leap = Utc.with_ymd_and_hms(2027, 2, 28, 23, 0, 0).unwrap();
    let non_leap_next = daily_midnight.next_run_after(&feb_28_non_leap).unwrap();
    assert_eq!(
        non_leap_next,
        Utc.with_ymd_and_hms(2027, 3, 1, 0, 0, 0).unwrap()
    );

    // 4. Leap-day specific target: 0 12 29 2 * (Feb 29 at 12:00)
    let leap_only = CronExpression::parse("0 12 29 2 *").unwrap();
    let from_2026 = Utc.with_ymd_and_hms(2026, 10, 10, 0, 0, 0).unwrap();
    let next_leap_day = leap_only.next_run_after(&from_2026).unwrap();
    assert_eq!(
        next_leap_day,
        Utc.with_ymd_and_hms(2028, 2, 29, 12, 0, 0).unwrap()
    );

    // 5. Year-end transition: Dec 31 23:55 -> Jan 1 00:00
    let hourly = CronExpression::parse("0 * * * *").unwrap();
    let dec_31 = Utc.with_ymd_and_hms(2026, 12, 31, 23, 55, 0).unwrap();
    let jan_1 = hourly.next_run_after(&dec_31).unwrap();
    assert_eq!(jan_1, Utc.with_ymd_and_hms(2027, 1, 1, 0, 0, 0).unwrap());

    // 6. Timezone offset normalization: @once with +05:30 offset
    let sched_offset = Schedule::parse("@once 2026-10-15T15:30:00+05:30").unwrap();
    let expected_utc = Utc.with_ymd_and_hms(2026, 10, 15, 10, 0, 0).unwrap();
    assert_eq!(
        sched_offset.next_run_after(&Utc.with_ymd_and_hms(2026, 10, 15, 9, 0, 0).unwrap()),
        Some(expected_utc)
    );
    assert_eq!(
        sched_offset.next_run_after(&Utc.with_ymd_and_hms(2026, 10, 15, 10, 1, 0).unwrap()),
        None
    );
}
