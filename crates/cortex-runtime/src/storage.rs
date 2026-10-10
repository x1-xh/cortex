//! SQLite-backed execution run and event stream persistence.
//!
//! Provides schema versioning, transaction-wrapped migrations, run lifecycle tracking,
//! and event stream storage for the Cortex runtime.

use crate::agent::{AgentMessage, AgentMessagePayload, RoutingKey};
use chrono::{DateTime, Utc};
pub use cortex_core::SecretRedactor;
use cortex_core::{AgentId, CortexError, EventRecord, JobId, Redactor, Result, RunId};
use std::str::FromStr;

use crate::scheduler::{
    CronJobStats, JobRunRecord, JobRunStatus, JobStatus, OverlapPolicy, ScheduledJob,
};
use rusqlite::{params, Connection, TransactionBehavior};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Mutex;

/// Current schema migration version.
pub const CURRENT_SCHEMA_VERSION: i32 = 4;
const SCHEMA_V1: &str = include_str!("../migrations/001_initial_schema.sql");
const SCHEMA_V2: &str = include_str!("../migrations/002_agents_and_checkpoints.sql");
const SCHEMA_V3: &str = include_str!("../migrations/003_cron_scheduler.sql");
const SCHEMA_V4: &str = include_str!("../migrations/004_multi_agent_tables.sql");

/// Persistent database record for an agent worker.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentRecord {
    /// Unique agent identifier.
    pub id: AgentId,
    /// Human-readable agent name.
    pub name: String,
    /// Declarative YAML manifest content.
    pub manifest_yaml: String,
    /// Current lifecycle status string (e.g., "created", "running", "paused", "stopped", "failed").
    pub status: String,
    /// Creation timestamp in ISO 8601 UTC.
    pub created_at: String,
    /// Last update timestamp in ISO 8601 UTC.
    pub updated_at: String,
}

/// Persistent execution checkpoint for an agent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentCheckpointRecord {
    /// Auto-incrementing database ID.
    pub id: Option<i64>,
    /// Associated agent ID.
    pub agent_id: AgentId,
    /// Execution step or sequence number.
    pub step: i64,
    /// Snapshot lifecycle state (e.g., "running", "paused", "stopped").
    pub state: String,
    /// Serialized context, memories, or checkpoint payload JSON.
    pub data_json: Option<String>,
    /// Checkpoint timestamp in ISO 8601 UTC.
    pub created_at: String,
}

/// Summary information for an execution run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunSummary {
    /// Unique run identifier.
    pub id: RunId,
    /// Task prompt assigned to the agent.
    pub task: String,
    /// Execution status (e.g., "running", "completed", "failed", "cancelled").
    pub status: String,
    /// ISO 8601 timestamp of run start.
    pub started_at: String,
    /// ISO 8601 timestamp of completion, if finished.
    pub finished_at: Option<String>,
    /// Elapsed execution time in milliseconds.
    pub duration_ms: Option<u64>,
    /// Number of prompt tokens consumed.
    pub tokens_prompt: u32,
    /// Number of completion tokens consumed.
    pub tokens_completion: u32,
    /// Total tokens consumed.
    pub tokens_total: u32,
    /// Estimated inference cost in USD.
    pub estimated_cost_usd: f64,
    /// Error message if the run terminated abnormally.
    pub error: Option<String>,
}

/// SQLite persistence manager for agent execution runs and structured events.
pub struct RunStore {
    conn: Mutex<Connection>,
    redactor: Redactor,
}

impl std::fmt::Debug for RunStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunStore").finish()
    }
}

impl RunStore {
    /// Open a persistent SQLite database at the given file path, applying migrations.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    CortexError::Internal(format!(
                        "failed to create database directory '{}': {}",
                        parent.display(),
                        e
                    ))
                })?;
            }
        }

        let conn = Connection::open(path).map_err(|e| {
            CortexError::Internal(format!(
                "failed to open sqlite database at '{}': {}",
                path.display(),
                e
            ))
        })?;

        let store = Self {
            conn: Mutex::new(conn),
            redactor: Redactor::default(),
        };
        store.migrate()?;
        Ok(store)
    }

    /// Open an in-memory SQLite database, applying migrations.
    pub fn in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().map_err(|e| {
            CortexError::Internal(format!("failed to open in-memory sqlite: {}", e))
        })?;

        let store = Self {
            conn: Mutex::new(conn),
            redactor: Redactor::default(),
        };
        store.migrate()?;
        Ok(store)
    }

    /// Attach a custom [`Redactor`] for scrubbing sensitive secrets from traces and payloads.
    pub fn with_redactor(mut self, redactor: Redactor) -> Self {
        self.redactor = redactor;
        self
    }

    /// Access the underlying [`Redactor`] used for secret scrubbing.
    pub fn redactor(&self) -> &Redactor {
        &self.redactor
    }

    /// Apply schema migrations sequentially within a transaction.
    pub fn migrate(&self) -> Result<()> {
        let mut conn = self
            .conn
            .lock()
            .map_err(|_| CortexError::Internal("failed to acquire store lock".to_string()))?;

        // Serialize schema inspection and updates across independent connections.
        let transaction = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| {
                CortexError::Internal(format!("failed to begin schema migration: {}", e))
            })?;

        // Check if schema_version table exists
        let table_exists: bool = transaction
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='schema_version'",
                [],
                |row| row.get(0),
            )
            .map_err(|e| {
                CortexError::Internal(format!("failed to query schema_version table: {}", e))
            })?;

        let current_version: i32 = if table_exists {
            transaction
                .query_row(
                    "SELECT COALESCE(MAX(version), 0) FROM schema_version",
                    [],
                    |row| row.get(0),
                )
                .map_err(|e| {
                    CortexError::Internal(format!("failed to read schema version: {}", e))
                })?
        } else {
            0
        };

        if current_version < 1 {
            transaction.execute_batch(SCHEMA_V1).map_err(|e| {
                CortexError::Internal(format!("failed to apply migration 001: {}", e))
            })?;

            let now = Utc::now().to_rfc3339();
            transaction
                .execute(
                    "INSERT INTO schema_version (version, applied_at) VALUES (?1, ?2)",
                    params![1, now],
                )
                .map_err(|e| {
                    CortexError::Internal(format!("failed to record schema_version 1: {}", e))
                })?;
        }

        if current_version < 2 {
            transaction.execute_batch(SCHEMA_V2).map_err(|e| {
                CortexError::Internal(format!("failed to apply migration 002: {}", e))
            })?;

            let now = Utc::now().to_rfc3339();
            transaction
                .execute(
                    "INSERT INTO schema_version (version, applied_at) VALUES (?1, ?2)",
                    params![2, now],
                )
                .map_err(|e| {
                    CortexError::Internal(format!("failed to record schema_version 2: {}", e))
                })?;
        }

        if current_version < 3 {
            transaction.execute_batch(SCHEMA_V3).map_err(|e| {
                CortexError::Internal(format!("failed to apply migration 003: {}", e))
            })?;

            let now = Utc::now().to_rfc3339();
            transaction
                .execute(
                    "INSERT INTO schema_version (version, applied_at) VALUES (?1, ?2)",
                    params![3, now],
                )
                .map_err(|e| {
                    CortexError::Internal(format!("failed to record schema_version 3: {}", e))
                })?;
        }

        if current_version < 4 {
            transaction.execute_batch(SCHEMA_V4).map_err(|e| {
                CortexError::Internal(format!("failed to apply migration 004: {}", e))
            })?;

            let now = Utc::now().to_rfc3339();
            transaction
                .execute(
                    "INSERT INTO schema_version (version, applied_at) VALUES (?1, ?2)",
                    params![4, now],
                )
                .map_err(|e| {
                    CortexError::Internal(format!("failed to record schema_version 4: {}", e))
                })?;
        }

        transaction
            .commit()
            .map_err(|e| CortexError::Internal(format!("failed to commit schema migration: {}", e)))
    }

    /// Return the currently applied schema migration version.
    pub fn schema_version(&self) -> Result<i32> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| CortexError::Internal("failed to acquire store lock".to_string()))?;

        let version: i32 = conn
            .query_row(
                "SELECT COALESCE(MAX(version), 0) FROM schema_version",
                [],
                |row| row.get(0),
            )
            .map_err(|e| CortexError::Internal(format!("failed to read schema version: {}", e)))?;

        Ok(version)
    }

    /// Record initial execution run state.
    pub fn record_run_start(&self, run_id: &RunId, task: &str, started_at: &str) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| CortexError::Internal("failed to acquire store lock".to_string()))?;

        conn.execute(
            "INSERT INTO runs (id, task, status, started_at) VALUES (?1, ?2, 'running', ?3)
             ON CONFLICT(id) DO UPDATE SET task = excluded.task, started_at = excluded.started_at",
            params![run_id.as_str(), task, started_at],
        )
        .map_err(|e| CortexError::Internal(format!("failed to record run start: {}", e)))?;

        Ok(())
    }

    /// Record completion or termination outcome of an execution run.
    #[allow(clippy::too_many_arguments)]
    pub fn record_run_completion(
        &self,
        run_id: &RunId,
        status: &str,
        finished_at: &str,
        duration_ms: u64,
        tokens_prompt: u32,
        tokens_completion: u32,
        estimated_cost_usd: f64,
        error: Option<&str>,
    ) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| CortexError::Internal("failed to acquire store lock".to_string()))?;

        let tokens_total = tokens_prompt + tokens_completion;

        conn.execute(
            "UPDATE runs SET
                status = ?1,
                finished_at = ?2,
                duration_ms = ?3,
                tokens_prompt = ?4,
                tokens_completion = ?5,
                tokens_total = ?6,
                estimated_cost_usd = ?7,
                error = ?8
             WHERE id = ?9",
            params![
                status,
                finished_at,
                duration_ms as i64,
                tokens_prompt as i64,
                tokens_completion as i64,
                tokens_total as i64,
                estimated_cost_usd,
                error,
                run_id.as_str(),
            ],
        )
        .map_err(|e| CortexError::Internal(format!("failed to record run completion: {}", e)))?;

        Ok(())
    }

    /// Reconcile runs left in 'running' state after an unhandled process termination or crash.
    pub fn reconcile_crashed_runs(&self, error_message: &str) -> Result<usize> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| CortexError::Internal("failed to acquire store lock".to_string()))?;

        let now = Utc::now().to_rfc3339();
        let updated = conn
            .execute(
                "UPDATE runs SET status = 'failed', finished_at = ?1, error = ?2 WHERE status = 'running'",
                params![now, error_message],
            )
            .map_err(|e| CortexError::Internal(format!("failed to reconcile crashed runs: {}", e)))?;

        Ok(updated)
    }

    /// Record a structured execution event in the event log.
    pub fn record_event(&self, record: &EventRecord) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| CortexError::Internal("failed to acquire store lock".to_string()))?;

        let payload_json = serde_json::to_string(record).map_err(|e| {
            CortexError::Internal(format!("failed to serialize event payload: {}", e))
        })?;

        conn.execute(
            "INSERT INTO events (run_id, sequence, timestamp, event_type, payload_json)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                record.run_id.as_str(),
                record.sequence as i64,
                record.timestamp.as_str(),
                record.event.event_type(),
                payload_json,
            ],
        )
        .map_err(|e| CortexError::Internal(format!("failed to insert event record: {}", e)))?;

        Ok(())
    }

    /// Retrieve summary details for a specific run.
    pub fn get_run(&self, run_id: &RunId) -> Result<Option<RunSummary>> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| CortexError::Internal("failed to acquire store lock".to_string()))?;

        let mut stmt = conn
            .prepare(
                "SELECT id, task, status, started_at, finished_at, duration_ms,
                        tokens_prompt, tokens_completion, tokens_total, estimated_cost_usd, error
                 FROM runs WHERE id = ?1",
            )
            .map_err(|e| CortexError::Internal(format!("failed to prepare run query: {}", e)))?;

        let mut rows = stmt
            .query(params![run_id.as_str()])
            .map_err(|e| CortexError::Internal(format!("failed to execute run query: {}", e)))?;

        if let Some(row) = rows
            .next()
            .map_err(|e| CortexError::Internal(format!("failed to read run row: {}", e)))?
        {
            let id_str: String = row.get(0).map_err(map_sql_err)?;
            let duration_ms: Option<i64> = row.get(5).map_err(map_sql_err)?;
            let tokens_prompt: i64 = row.get(6).map_err(map_sql_err)?;
            let tokens_completion: i64 = row.get(7).map_err(map_sql_err)?;
            let tokens_total: i64 = row.get(8).map_err(map_sql_err)?;

            Ok(Some(RunSummary {
                id: RunId::from(id_str),
                task: row.get(1).map_err(map_sql_err)?,
                status: row.get(2).map_err(map_sql_err)?,
                started_at: row.get(3).map_err(map_sql_err)?,
                finished_at: row.get(4).map_err(map_sql_err)?,
                duration_ms: duration_ms.map(|d| d as u64),
                tokens_prompt: tokens_prompt as u32,
                tokens_completion: tokens_completion as u32,
                tokens_total: tokens_total as u32,
                estimated_cost_usd: row.get(9).map_err(map_sql_err)?,
                error: row.get(10).map_err(map_sql_err)?,
            }))
        } else {
            Ok(None)
        }
    }

    /// List recent execution runs, sorted newest first.
    pub fn list_runs(&self, limit: usize) -> Result<Vec<RunSummary>> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| CortexError::Internal("failed to acquire store lock".to_string()))?;

        let mut stmt = conn
            .prepare(
                "SELECT id, task, status, started_at, finished_at, duration_ms,
                        tokens_prompt, tokens_completion, tokens_total, estimated_cost_usd, error
                 FROM runs ORDER BY started_at DESC LIMIT ?1",
            )
            .map_err(|e| CortexError::Internal(format!("failed to prepare list runs: {}", e)))?;

        let rows = stmt
            .query_map(params![limit as i64], |row| {
                let id_str: String = row.get(0)?;
                let duration_ms: Option<i64> = row.get(5)?;
                let tokens_prompt: i64 = row.get(6)?;
                let tokens_completion: i64 = row.get(7)?;
                let tokens_total: i64 = row.get(8)?;

                Ok(RunSummary {
                    id: RunId::from(id_str),
                    task: row.get(1)?,
                    status: row.get(2)?,
                    started_at: row.get(3)?,
                    finished_at: row.get(4)?,
                    duration_ms: duration_ms.map(|d| d as u64),
                    tokens_prompt: tokens_prompt as u32,
                    tokens_completion: tokens_completion as u32,
                    tokens_total: tokens_total as u32,
                    estimated_cost_usd: row.get(9)?,
                    error: row.get(10)?,
                })
            })
            .map_err(|e| CortexError::Internal(format!("failed to list runs: {}", e)))?;

        let mut result = Vec::new();
        for r in rows {
            result.push(r.map_err(map_sql_err)?);
        }
        Ok(result)
    }

    /// Retrieve all structured event records for a specific run, sorted by sequence number.
    pub fn get_events(&self, run_id: &RunId) -> Result<Vec<EventRecord>> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| CortexError::Internal("failed to acquire store lock".to_string()))?;

        let mut stmt = conn
            .prepare(
                "SELECT payload_json FROM events
                 WHERE run_id = ?1 ORDER BY sequence ASC",
            )
            .map_err(|e| CortexError::Internal(format!("failed to prepare get events: {}", e)))?;

        let rows = stmt
            .query_map(params![run_id.as_str()], |row| {
                let json_str: String = row.get(0)?;
                Ok(json_str)
            })
            .map_err(|e| CortexError::Internal(format!("failed to query events: {}", e)))?;

        let mut records = Vec::new();
        for r in rows {
            let json_str = r.map_err(map_sql_err)?;
            let record: EventRecord = serde_json::from_str(&json_str).map_err(|e| {
                CortexError::Internal(format!("failed to deserialize event record: {}", e))
            })?;
            records.push(record);
        }

        Ok(records)
    }

    /// Save or update an agent record in SQLite.
    pub fn save_agent(&self, agent: &AgentRecord) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| CortexError::Internal("failed to acquire store lock".to_string()))?;

        conn.execute(
            "INSERT INTO agents (id, name, manifest_yaml, status, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(id) DO UPDATE SET
                name = excluded.name,
                manifest_yaml = excluded.manifest_yaml,
                status = excluded.status,
                updated_at = excluded.updated_at",
            params![
                agent.id.as_str(),
                agent.name,
                agent.manifest_yaml,
                agent.status,
                agent.created_at,
                agent.updated_at,
            ],
        )
        .map_err(|e| CortexError::Internal(format!("failed to save agent: {}", e)))?;

        Ok(())
    }

    /// Retrieve an agent record by ID.
    pub fn get_agent(&self, id: &AgentId) -> Result<Option<AgentRecord>> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| CortexError::Internal("failed to acquire store lock".to_string()))?;

        let mut stmt = conn
            .prepare("SELECT id, name, manifest_yaml, status, created_at, updated_at FROM agents WHERE id = ?1")
            .map_err(|e| CortexError::Internal(format!("failed to prepare get_agent query: {}", e)))?;

        let mut rows = stmt
            .query(params![id.as_str()])
            .map_err(|e| CortexError::Internal(format!("failed to query agent: {}", e)))?;

        if let Some(row) = rows
            .next()
            .map_err(|e| CortexError::Internal(format!("failed to fetch agent row: {}", e)))?
        {
            let raw_id: String = row.get(0).map_err(map_sql_err)?;
            let name: String = row.get(1).map_err(map_sql_err)?;
            let manifest_yaml: String = row.get(2).map_err(map_sql_err)?;
            let status: String = row.get(3).map_err(map_sql_err)?;
            let created_at: String = row.get(4).map_err(map_sql_err)?;
            let updated_at: String = row.get(5).map_err(map_sql_err)?;

            Ok(Some(AgentRecord {
                id: AgentId::from(raw_id),
                name,
                manifest_yaml,
                status,
                created_at,
                updated_at,
            }))
        } else {
            Ok(None)
        }
    }

    /// List all agent records ordered by creation time.
    pub fn list_agents(&self) -> Result<Vec<AgentRecord>> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| CortexError::Internal("failed to acquire store lock".to_string()))?;

        let mut stmt = conn
            .prepare("SELECT id, name, manifest_yaml, status, created_at, updated_at FROM agents ORDER BY created_at ASC")
            .map_err(|e| CortexError::Internal(format!("failed to prepare list_agents query: {}", e)))?;

        let rows = stmt
            .query_map([], |row| {
                let raw_id: String = row.get(0)?;
                let name: String = row.get(1)?;
                let manifest_yaml: String = row.get(2)?;
                let status: String = row.get(3)?;
                let created_at: String = row.get(4)?;
                let updated_at: String = row.get(5)?;

                Ok(AgentRecord {
                    id: AgentId::from(raw_id),
                    name,
                    manifest_yaml,
                    status,
                    created_at,
                    updated_at,
                })
            })
            .map_err(|e| CortexError::Internal(format!("failed to query agents list: {}", e)))?;

        let mut agents = Vec::new();
        for agent in rows {
            agents.push(agent.map_err(map_sql_err)?);
        }

        Ok(agents)
    }

    /// Update the lifecycle status of an agent.
    pub fn update_agent_status(&self, id: &AgentId, status: &str) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| CortexError::Internal("failed to acquire store lock".to_string()))?;

        let now = Utc::now().to_rfc3339();
        let rows_affected = conn
            .execute(
                "UPDATE agents SET status = ?1, updated_at = ?2 WHERE id = ?3",
                params![status, now, id.as_str()],
            )
            .map_err(|e| CortexError::Internal(format!("failed to update agent status: {}", e)))?;

        if rows_affected == 0 {
            return Err(CortexError::NotFound(format!("agent '{}' not found", id)));
        }

        Ok(())
    }

    /// Delete an agent and its checkpoints by ID.
    pub fn delete_agent(&self, id: &AgentId) -> Result<bool> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| CortexError::Internal("failed to acquire store lock".to_string()))?;

        let rows = conn
            .execute("DELETE FROM agents WHERE id = ?1", params![id.as_str()])
            .map_err(|e| CortexError::Internal(format!("failed to delete agent: {}", e)))?;

        Ok(rows > 0)
    }

    /// Record an execution checkpoint for an agent.
    pub fn save_agent_checkpoint(&self, checkpoint: &AgentCheckpointRecord) -> Result<i64> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| CortexError::Internal("failed to acquire store lock".to_string()))?;

        conn.execute(
            "INSERT INTO agent_checkpoints (agent_id, step, state, data_json, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                checkpoint.agent_id.as_str(),
                checkpoint.step,
                checkpoint.state,
                checkpoint.data_json,
                checkpoint.created_at,
            ],
        )
        .map_err(|e| CortexError::Internal(format!("failed to save agent checkpoint: {}", e)))?;

        Ok(conn.last_insert_rowid())
    }

    /// List all checkpoints recorded for an agent.
    pub fn list_agent_checkpoints(&self, agent_id: &AgentId) -> Result<Vec<AgentCheckpointRecord>> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| CortexError::Internal("failed to acquire store lock".to_string()))?;

        let mut stmt = conn
            .prepare("SELECT id, agent_id, step, state, data_json, created_at FROM agent_checkpoints WHERE agent_id = ?1 ORDER BY step ASC, id ASC")
            .map_err(|e| CortexError::Internal(format!("failed to prepare list_agent_checkpoints query: {}", e)))?;

        let rows = stmt
            .query_map(params![agent_id.as_str()], |row| {
                let id: i64 = row.get(0)?;
                let raw_agent_id: String = row.get(1)?;
                let step: i64 = row.get(2)?;
                let state: String = row.get(3)?;
                let data_json: Option<String> = row.get(4)?;
                let created_at: String = row.get(5)?;

                Ok(AgentCheckpointRecord {
                    id: Some(id),
                    agent_id: AgentId::from(raw_agent_id),
                    step,
                    state,
                    data_json,
                    created_at,
                })
            })
            .map_err(|e| {
                CortexError::Internal(format!("failed to query agent checkpoints: {}", e))
            })?;

        let mut checkpoints = Vec::new();
        for cp in rows {
            checkpoints.push(cp.map_err(map_sql_err)?);
        }

        Ok(checkpoints)
    }

    /// Reconcile agents left in 'running' state after daemon shutdown or crash.
    ///
    /// Interrupted agents are cleanly marked as 'stopped', or remain 'running' if their ID
    /// is included in `auto_resume_ids`.
    pub fn reconcile_crashed_agents_in_store(&self, auto_resume_ids: &[AgentId]) -> Result<usize> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| CortexError::Internal("failed to acquire store lock".to_string()))?;

        let now = Utc::now().to_rfc3339();
        let mut stopped_count = 0;

        let mut stmt = conn
            .prepare("SELECT id FROM agents WHERE status = 'running'")
            .map_err(|e| CortexError::Internal(format!("failed to prepare query: {}", e)))?;

        let running_ids: Vec<String> = stmt
            .query_map([], |row| row.get(0))
            .map_err(|e| CortexError::Internal(format!("failed to query running agents: {}", e)))?
            .filter_map(|r| r.ok())
            .collect();

        for id_str in running_ids {
            let agent_id = AgentId::from(id_str.as_str());
            if !auto_resume_ids.contains(&agent_id) {
                conn.execute(
                    "UPDATE agents SET status = 'stopped', updated_at = ?1 WHERE id = ?2",
                    params![now, id_str],
                )
                .map_err(|e| CortexError::Internal(format!("failed to reconcile agent: {}", e)))?;
                stopped_count += 1;
            }
        }

        Ok(stopped_count)
    }

    /// Save or update a scheduled cron job.
    pub fn save_cron_job(&self, job: &ScheduledJob) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| CortexError::Internal("failed to acquire store lock".to_string()))?;

        let next_run_str = job.next_run_at.map(|dt| dt.to_rfc3339());
        let last_run_str = job.last_run_at.map(|dt| dt.to_rfc3339());
        let created_str = job.created_at.to_rfc3339();
        let updated_str = job.updated_at.to_rfc3339();

        conn.execute(
            r#"
            INSERT INTO cron_jobs (
                id, name, schedule, prompt, overlap_policy, status,
                next_run_at, last_run_at, created_at, updated_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
            ON CONFLICT(id) DO UPDATE SET
                name = excluded.name,
                schedule = excluded.schedule,
                prompt = excluded.prompt,
                overlap_policy = excluded.overlap_policy,
                status = excluded.status,
                next_run_at = excluded.next_run_at,
                last_run_at = excluded.last_run_at,
                updated_at = excluded.updated_at
            "#,
            params![
                job.id.as_str(),
                job.name,
                job.schedule,
                job.prompt,
                job.overlap_policy.as_str(),
                job.status.as_str(),
                next_run_str,
                last_run_str,
                created_str,
                updated_str,
            ],
        )
        .map_err(map_sql_err)?;

        Ok(())
    }

    /// Query a scheduled cron job by its identifier.
    pub fn get_cron_job(&self, id: &JobId) -> Result<Option<ScheduledJob>> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| CortexError::Internal("failed to acquire store lock".to_string()))?;

        let mut stmt = conn
            .prepare(
                r#"
                SELECT id, name, schedule, prompt, overlap_policy, status,
                       next_run_at, last_run_at, created_at, updated_at
                FROM cron_jobs
                WHERE id = ?1
                "#,
            )
            .map_err(map_sql_err)?;

        let mut rows = stmt
            .query_map(params![id.as_str()], |row| {
                let id_str: String = row.get(0)?;
                let name: String = row.get(1)?;
                let schedule: String = row.get(2)?;
                let prompt: String = row.get(3)?;
                let overlap_policy_str: String = row.get(4)?;
                let status_str: String = row.get(5)?;
                let next_run_str: Option<String> = row.get(6)?;
                let last_run_str: Option<String> = row.get(7)?;
                let created_str: String = row.get(8)?;
                let updated_str: String = row.get(9)?;

                Ok((
                    id_str,
                    name,
                    schedule,
                    prompt,
                    overlap_policy_str,
                    status_str,
                    next_run_str,
                    last_run_str,
                    created_str,
                    updated_str,
                ))
            })
            .map_err(map_sql_err)?;

        if let Some(r) = rows.next() {
            let (
                id_str,
                name,
                schedule,
                prompt,
                overlap_policy_str,
                status_str,
                next_run_str,
                last_run_str,
                created_str,
                updated_str,
            ) = r.map_err(map_sql_err)?;

            let overlap_policy =
                OverlapPolicy::from_str(&overlap_policy_str).unwrap_or(OverlapPolicy::Skip);
            let status = JobStatus::from_str(&status_str).unwrap_or(JobStatus::Active);

            let next_run_at = next_run_str
                .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
                .map(|dt| dt.with_timezone(&Utc));
            let last_run_at = last_run_str
                .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
                .map(|dt| dt.with_timezone(&Utc));
            let created_at = DateTime::parse_from_rfc3339(&created_str)
                .map(|dt| dt.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now());
            let updated_at = DateTime::parse_from_rfc3339(&updated_str)
                .map(|dt| dt.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now());

            Ok(Some(ScheduledJob {
                id: JobId::from(id_str),
                name,
                schedule,
                prompt,
                overlap_policy,
                status,
                next_run_at,
                last_run_at,
                created_at,
                updated_at,
            }))
        } else {
            Ok(None)
        }
    }

    /// List all registered scheduled cron jobs ordered by creation timestamp.
    pub fn list_cron_jobs(&self) -> Result<Vec<ScheduledJob>> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| CortexError::Internal("failed to acquire store lock".to_string()))?;

        let mut stmt = conn
            .prepare(
                r#"
                SELECT id, name, schedule, prompt, overlap_policy, status,
                       next_run_at, last_run_at, created_at, updated_at
                FROM cron_jobs
                ORDER BY created_at ASC
                "#,
            )
            .map_err(map_sql_err)?;

        let rows = stmt
            .query_map([], |row| {
                let id_str: String = row.get(0)?;
                let name: String = row.get(1)?;
                let schedule: String = row.get(2)?;
                let prompt: String = row.get(3)?;
                let overlap_policy_str: String = row.get(4)?;
                let status_str: String = row.get(5)?;
                let next_run_str: Option<String> = row.get(6)?;
                let last_run_str: Option<String> = row.get(7)?;
                let created_str: String = row.get(8)?;
                let updated_str: String = row.get(9)?;

                Ok((
                    id_str,
                    name,
                    schedule,
                    prompt,
                    overlap_policy_str,
                    status_str,
                    next_run_str,
                    last_run_str,
                    created_str,
                    updated_str,
                ))
            })
            .map_err(map_sql_err)?;

        let mut jobs = Vec::new();
        for r in rows {
            let (
                id_str,
                name,
                schedule,
                prompt,
                overlap_policy_str,
                status_str,
                next_run_str,
                last_run_str,
                created_str,
                updated_str,
            ) = r.map_err(map_sql_err)?;

            let overlap_policy =
                OverlapPolicy::from_str(&overlap_policy_str).unwrap_or(OverlapPolicy::Skip);
            let status = JobStatus::from_str(&status_str).unwrap_or(JobStatus::Active);

            let next_run_at = next_run_str
                .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
                .map(|dt| dt.with_timezone(&Utc));
            let last_run_at = last_run_str
                .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
                .map(|dt| dt.with_timezone(&Utc));
            let created_at = DateTime::parse_from_rfc3339(&created_str)
                .map(|dt| dt.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now());
            let updated_at = DateTime::parse_from_rfc3339(&updated_str)
                .map(|dt| dt.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now());

            jobs.push(ScheduledJob {
                id: JobId::from(id_str),
                name,
                schedule,
                prompt,
                overlap_policy,
                status,
                next_run_at,
                last_run_at,
                created_at,
                updated_at,
            });
        }

        Ok(jobs)
    }

    /// Delete a scheduled cron job and its execution history.
    pub fn delete_cron_job(&self, id: &JobId) -> Result<bool> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| CortexError::Internal("failed to acquire store lock".to_string()))?;

        let rows_affected = conn
            .execute("DELETE FROM cron_jobs WHERE id = ?1", params![id.as_str()])
            .map_err(map_sql_err)?;

        Ok(rows_affected > 0)
    }

    /// Update next run time, last run time, and status of a scheduled job.
    pub fn update_cron_job_schedule(
        &self,
        id: &JobId,
        next_run_at: Option<&str>,
        last_run_at: Option<&str>,
        status: &str,
    ) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| CortexError::Internal("failed to acquire store lock".to_string()))?;

        let now = Utc::now().to_rfc3339();
        conn.execute(
            r#"
            UPDATE cron_jobs
            SET next_run_at = ?1,
                last_run_at = COALESCE(?2, last_run_at),
                status = ?3,
                updated_at = ?4
            WHERE id = ?5
            "#,
            params![next_run_at, last_run_at, status, now, id.as_str()],
        )
        .map_err(map_sql_err)?;

        Ok(())
    }

    /// Record initial execution of a scheduled cron run.
    pub fn record_cron_run_start(
        &self,
        run_id: &str,
        job_id: &JobId,
        started_at: &str,
    ) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| CortexError::Internal("failed to acquire store lock".to_string()))?;

        conn.execute(
            r#"
            INSERT INTO cron_job_runs (id, job_id, status, started_at)
            VALUES (?1, ?2, 'running', ?3)
            "#,
            params![run_id, job_id.as_str(), started_at],
        )
        .map_err(map_sql_err)?;

        Ok(())
    }

    /// Record completion or termination of a scheduled cron run.
    pub fn record_cron_run_finish(
        &self,
        run_id: &str,
        status: &str,
        finished_at: &str,
        duration_ms: Option<u64>,
        output: Option<&str>,
        error: Option<&str>,
    ) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| CortexError::Internal("failed to acquire store lock".to_string()))?;

        conn.execute(
            r#"
            UPDATE cron_job_runs
            SET status = ?1,
                finished_at = ?2,
                duration_ms = ?3,
                output = ?4,
                error = ?5
            WHERE id = ?6
            "#,
            params![status, finished_at, duration_ms, output, error, run_id],
        )
        .map_err(map_sql_err)?;

        Ok(())
    }

    /// Cancel an active scheduled cron run.
    pub fn cancel_cron_run(&self, run_id: &str, finished_at: &str, reason: &str) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| CortexError::Internal("failed to acquire store lock".to_string()))?;

        conn.execute(
            r#"
            UPDATE cron_job_runs
            SET status = 'cancelled',
                finished_at = ?1,
                error = ?2
            WHERE id = ?3
            "#,
            params![finished_at, reason, run_id],
        )
        .map_err(map_sql_err)?;

        Ok(())
    }

    /// Query the currently active running execution for a scheduled job, if any.
    pub fn get_active_cron_run(&self, job_id: &JobId) -> Result<Option<JobRunRecord>> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| CortexError::Internal("failed to acquire store lock".to_string()))?;

        let mut stmt = conn
            .prepare(
                r#"
                SELECT id, job_id, status, started_at, finished_at, duration_ms, output, error
                FROM cron_job_runs
                WHERE job_id = ?1 AND status = 'running'
                ORDER BY started_at DESC
                LIMIT 1
                "#,
            )
            .map_err(map_sql_err)?;

        let mut rows = stmt
            .query_map(params![job_id.as_str()], |row| {
                let id: String = row.get(0)?;
                let job_id_str: String = row.get(1)?;
                let status_str: String = row.get(2)?;
                let started_str: String = row.get(3)?;
                let finished_str: Option<String> = row.get(4)?;
                let duration_ms: Option<u64> = row.get(5)?;
                let output: Option<String> = row.get(6)?;
                let error: Option<String> = row.get(7)?;

                Ok((
                    id,
                    job_id_str,
                    status_str,
                    started_str,
                    finished_str,
                    duration_ms,
                    output,
                    error,
                ))
            })
            .map_err(map_sql_err)?;

        if let Some(r) = rows.next() {
            let (id, job_id_str, status_str, started_str, finished_str, duration_ms, output, error) =
                r.map_err(map_sql_err)?;

            let status = JobRunStatus::from_str(&status_str).unwrap_or(JobRunStatus::Running);
            let started_at = DateTime::parse_from_rfc3339(&started_str)
                .map(|dt| dt.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now());
            let finished_at = finished_str
                .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
                .map(|dt| dt.with_timezone(&Utc));

            Ok(Some(JobRunRecord {
                id,
                job_id: JobId::from(job_id_str),
                status,
                started_at,
                finished_at,
                duration_ms,
                output,
                error,
            }))
        } else {
            Ok(None)
        }
    }

    /// Query recorded runs for a scheduled job.
    pub fn list_cron_job_runs(&self, job_id: &JobId, limit: usize) -> Result<Vec<JobRunRecord>> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| CortexError::Internal("failed to acquire store lock".to_string()))?;

        let mut stmt = conn
            .prepare(
                r#"
                SELECT id, job_id, status, started_at, finished_at, duration_ms, output, error
                FROM cron_job_runs
                WHERE job_id = ?1
                ORDER BY started_at DESC
                LIMIT ?2
                "#,
            )
            .map_err(map_sql_err)?;

        let rows = stmt
            .query_map(params![job_id.as_str(), limit as i64], |row| {
                let id: String = row.get(0)?;
                let job_id_str: String = row.get(1)?;
                let status_str: String = row.get(2)?;
                let started_str: String = row.get(3)?;
                let finished_str: Option<String> = row.get(4)?;
                let duration_ms: Option<u64> = row.get(5)?;
                let output: Option<String> = row.get(6)?;
                let error: Option<String> = row.get(7)?;

                Ok((
                    id,
                    job_id_str,
                    status_str,
                    started_str,
                    finished_str,
                    duration_ms,
                    output,
                    error,
                ))
            })
            .map_err(map_sql_err)?;

        let mut list = Vec::new();
        for r in rows {
            let (id, job_id_str, status_str, started_str, finished_str, duration_ms, output, error) =
                r.map_err(map_sql_err)?;

            let status = JobRunStatus::from_str(&status_str).unwrap_or(JobRunStatus::Completed);
            let started_at = DateTime::parse_from_rfc3339(&started_str)
                .map(|dt| dt.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now());
            let finished_at = finished_str
                .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
                .map(|dt| dt.with_timezone(&Utc));

            list.push(JobRunRecord {
                id,
                job_id: JobId::from(job_id_str),
                status,
                started_at,
                finished_at,
                duration_ms,
                output,
                error,
            });
        }

        Ok(list)
    }

    /// Retrieve execution run statistics (total, success, failure) for a scheduled cron job.
    pub fn get_cron_job_stats(&self, job_id: &JobId) -> Result<CronJobStats> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| CortexError::Internal("failed to acquire store lock".to_string()))?;

        let mut stmt = conn
            .prepare(
                r#"
                SELECT
                    COUNT(*),
                    COALESCE(SUM(CASE WHEN status = 'completed' THEN 1 ELSE 0 END), 0),
                    COALESCE(SUM(CASE WHEN status = 'failed' THEN 1 ELSE 0 END), 0)
                FROM cron_job_runs
                WHERE job_id = ?1
                "#,
            )
            .map_err(map_sql_err)?;

        let stats = stmt
            .query_row(params![job_id.as_str()], |row| {
                let total: i64 = row.get(0)?;
                let success: i64 = row.get(1)?;
                let failure: i64 = row.get(2)?;
                Ok(CronJobStats {
                    total_runs: total as usize,
                    success_runs: success as usize,
                    failure_runs: failure as usize,
                })
            })
            .map_err(map_sql_err)?;

        Ok(stats)
    }

    /// Find a scheduled cron job by exact ID or unique prefix.
    ///
    /// Returns `Ok(Some(job))` on exact match or unambiguous prefix match.
    /// Returns `Ok(None)` if no jobs match.
    /// Returns `Err(CortexError::Validation(...))` if the prefix matches multiple jobs.
    pub fn find_cron_job(&self, id_or_prefix: &str) -> Result<Option<ScheduledJob>> {
        let query = id_or_prefix.trim();
        if query.is_empty() {
            return Ok(None);
        }

        // Try exact match first
        let exact_id = JobId::from(query);
        if let Some(job) = self.get_cron_job(&exact_id)? {
            return Ok(Some(job));
        }

        // Search by prefix
        let conn = self
            .conn
            .lock()
            .map_err(|_| CortexError::Internal("failed to acquire store lock".to_string()))?;

        let mut stmt = conn
            .prepare(
                r#"
                SELECT id, name, schedule, prompt, overlap_policy, status,
                       next_run_at, last_run_at, created_at, updated_at
                FROM cron_jobs
                WHERE id LIKE ?1 || '%'
                ORDER BY created_at ASC
                "#,
            )
            .map_err(map_sql_err)?;

        let rows = stmt
            .query_map(params![query], |row| {
                let id_str: String = row.get(0)?;
                let name: String = row.get(1)?;
                let schedule: String = row.get(2)?;
                let prompt: String = row.get(3)?;
                let overlap_policy_str: String = row.get(4)?;
                let status_str: String = row.get(5)?;
                let next_run_str: Option<String> = row.get(6)?;
                let last_run_str: Option<String> = row.get(7)?;
                let created_str: String = row.get(8)?;
                let updated_str: String = row.get(9)?;

                Ok((
                    id_str,
                    name,
                    schedule,
                    prompt,
                    overlap_policy_str,
                    status_str,
                    next_run_str,
                    last_run_str,
                    created_str,
                    updated_str,
                ))
            })
            .map_err(map_sql_err)?;

        let mut matches = Vec::new();
        for r in rows {
            let (
                id_str,
                name,
                schedule,
                prompt,
                overlap_policy_str,
                status_str,
                next_run_str,
                last_run_str,
                created_str,
                updated_str,
            ) = r.map_err(map_sql_err)?;

            let overlap_policy =
                OverlapPolicy::from_str(&overlap_policy_str).unwrap_or(OverlapPolicy::Skip);
            let status = JobStatus::from_str(&status_str).unwrap_or(JobStatus::Active);

            let next_run_at = next_run_str
                .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
                .map(|dt| dt.with_timezone(&Utc));
            let last_run_at = last_run_str
                .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
                .map(|dt| dt.with_timezone(&Utc));
            let created_at = DateTime::parse_from_rfc3339(&created_str)
                .map(|dt| dt.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now());
            let updated_at = DateTime::parse_from_rfc3339(&updated_str)
                .map(|dt| dt.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now());

            matches.push(ScheduledJob {
                id: JobId::from(id_str),
                name,
                schedule,
                prompt,
                overlap_policy,
                status,
                next_run_at,
                last_run_at,
                created_at,
                updated_at,
            });
        }

        if matches.is_empty() {
            Ok(None)
        } else if matches.len() == 1 {
            Ok(Some(matches.remove(0)))
        } else {
            let ids: Vec<String> = matches.iter().map(|j| j.id.as_str().to_string()).collect();
            Err(CortexError::Validation(format!(
                "ambiguous cron job prefix '{}': matches multiple jobs ({})",
                query,
                ids.join(", ")
            )))
        }
    }

    /// Persist an inter-agent message transmission in SQLite with secret redaction.
    pub fn record_message(&self, msg: &AgentMessage) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| CortexError::Internal("failed to acquire store lock".to_string()))?;

        // Ensure run exists so foreign key constraints are preserved
        conn.execute(
            "INSERT OR IGNORE INTO runs (id, task, status, started_at) VALUES (?1, 'multi-agent execution', 'running', ?2)",
            params![msg.run_id.as_str(), msg.timestamp.as_str()],
        )
        .map_err(|e| CortexError::Internal(format!("failed to ensure run exists: {}", e)))?;

        let (task_id, message_type) = match &msg.payload {
            AgentMessagePayload::TaskRequest { task_id, .. } => {
                (Some(task_id.as_str()), "task_request")
            }
            AgentMessagePayload::TaskResult { task_id, .. } => {
                (Some(task_id.as_str()), "task_result")
            }
            AgentMessagePayload::TaskFailed { task_id, .. } => {
                (Some(task_id.as_str()), "task_failed")
            }
            AgentMessagePayload::Notification { .. } => (None, "notification"),
        };

        // Redact payload before writing to disk
        let raw_val = serde_json::to_value(&msg.payload).map_err(|e| {
            CortexError::Internal(format!("failed to serialize message payload: {}", e))
        })?;
        let redacted_val = self.redactor.redact_json(&raw_val);
        let payload_json = serde_json::to_string(&redacted_val).map_err(|e| {
            CortexError::Internal(format!("failed to serialize redacted payload: {}", e))
        })?;

        conn.execute(
            "INSERT INTO inter_agent_messages (
                id, run_id, task_id, sender, recipient, routing_key, message_type, payload, timestamp
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(id) DO UPDATE SET
                run_id = excluded.run_id,
                task_id = excluded.task_id,
                sender = excluded.sender,
                recipient = excluded.recipient,
                routing_key = excluded.routing_key,
                message_type = excluded.message_type,
                payload = excluded.payload,
                timestamp = excluded.timestamp",
            params![
                msg.id.as_str(),
                msg.run_id.as_str(),
                task_id,
                msg.sender.as_str(),
                msg.recipient.as_str(),
                msg.routing_key.as_str(),
                message_type,
                payload_json,
                msg.timestamp.as_str(),
            ],
        )
        .map_err(|e| CortexError::Internal(format!("failed to insert inter-agent message: {}", e)))?;

        Ok(())
    }

    /// Persist a slice of inter-agent messages in SQLite.
    pub fn record_messages(&self, msgs: &[AgentMessage]) -> Result<()> {
        for msg in msgs {
            self.record_message(msg)?;
        }
        Ok(())
    }

    /// Retrieve all inter-agent messages recorded for a specific run, sorted chronologically.
    pub fn get_messages_for_run(&self, run_id: &RunId) -> Result<Vec<AgentMessage>> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| CortexError::Internal("failed to acquire store lock".to_string()))?;

        let mut stmt = conn
            .prepare(
                "SELECT id, run_id, sender, recipient, routing_key, timestamp, message_type, payload
                 FROM inter_agent_messages
                 WHERE run_id = ?1
                 ORDER BY timestamp ASC, id ASC",
            )
            .map_err(|e| CortexError::Internal(format!("failed to prepare get_messages_for_run query: {}", e)))?;

        let rows = stmt
            .query_map(params![run_id.as_str()], Self::row_to_agent_message)
            .map_err(|e| {
                CortexError::Internal(format!("failed to query messages for run: {}", e))
            })?;

        let mut messages = Vec::new();
        for r in rows {
            messages.push(r.map_err(map_sql_err)?);
        }

        Ok(messages)
    }

    /// Retrieve all inter-agent messages exchanged between two agents across all runs, sorted chronologically.
    pub fn get_messages_between(
        &self,
        agent_a: &AgentId,
        agent_b: &AgentId,
    ) -> Result<Vec<AgentMessage>> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| CortexError::Internal("failed to acquire store lock".to_string()))?;

        let mut stmt = conn
            .prepare(
                "SELECT id, run_id, sender, recipient, routing_key, timestamp, message_type, payload
                 FROM inter_agent_messages
                 WHERE (sender = ?1 AND recipient = ?2) OR (sender = ?2 AND recipient = ?1)
                 ORDER BY timestamp ASC, id ASC",
            )
            .map_err(|e| CortexError::Internal(format!("failed to prepare get_messages_between query: {}", e)))?;

        let rows = stmt
            .query_map(
                params![agent_a.as_str(), agent_b.as_str()],
                Self::row_to_agent_message,
            )
            .map_err(|e| {
                CortexError::Internal(format!("failed to query messages between agents: {}", e))
            })?;

        let mut messages = Vec::new();
        for r in rows {
            messages.push(r.map_err(map_sql_err)?);
        }

        Ok(messages)
    }

    /// Retrieve all inter-agent messages correlated with a specific task identifier.
    pub fn get_messages_for_task(&self, task_id: &str) -> Result<Vec<AgentMessage>> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| CortexError::Internal("failed to acquire store lock".to_string()))?;

        let mut stmt = conn
            .prepare(
                "SELECT id, run_id, sender, recipient, routing_key, timestamp, message_type, payload
                 FROM inter_agent_messages
                 WHERE task_id = ?1
                 ORDER BY timestamp ASC, id ASC",
            )
            .map_err(|e| CortexError::Internal(format!("failed to prepare get_messages_for_task query: {}", e)))?;

        let rows = stmt
            .query_map(params![task_id], Self::row_to_agent_message)
            .map_err(|e| {
                CortexError::Internal(format!("failed to query messages for task: {}", e))
            })?;

        let mut messages = Vec::new();
        for r in rows {
            messages.push(r.map_err(map_sql_err)?);
        }

        Ok(messages)
    }

    fn row_to_agent_message(row: &rusqlite::Row<'_>) -> rusqlite::Result<AgentMessage> {
        let id: String = row.get(0)?;
        let run_id_str: String = row.get(1)?;
        let sender_str: String = row.get(2)?;
        let recipient_str: String = row.get(3)?;
        let routing_key_str: String = row.get(4)?;
        let timestamp: String = row.get(5)?;
        let message_type: String = row.get(6)?;
        let payload_json: String = row.get(7)?;

        let routing_key = RoutingKey::new(routing_key_str).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(4, rusqlite::types::Type::Text, Box::new(e))
        })?;

        let payload_val: serde_json::Value = serde_json::from_str(&payload_json).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(7, rusqlite::types::Type::Text, Box::new(e))
        })?;

        let payload: AgentMessagePayload = serde_json::from_value(payload_val.clone())
            .or_else(|_| {
                serde_json::from_value(serde_json::json!({
                    "type": message_type,
                    "payload": payload_val
                }))
            })
            .map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    7,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })?;

        Ok(AgentMessage {
            id,
            run_id: RunId::from(run_id_str),
            sender: AgentId::from(sender_str),
            recipient: AgentId::from(recipient_str),
            routing_key,
            timestamp,
            payload,
        })
    }
}

fn map_sql_err(err: rusqlite::Error) -> CortexError {
    CortexError::Internal(format!("sqlite error: {}", err))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cortex_core::ExecutionEvent;

    #[test]
    fn test_store_in_memory_migrations() {
        let store = RunStore::in_memory().unwrap();
        assert_eq!(store.schema_version().unwrap(), CURRENT_SCHEMA_VERSION);
    }

    #[test]
    fn test_concurrent_store_initialization() {
        let path = std::env::temp_dir().join(format!(
            "cortex_concurrent_store_{}_{}.db",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap()
        ));
        let barrier = std::sync::Barrier::new(8);
        let results = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|_| {
                    scope.spawn(|| {
                        barrier.wait();
                        RunStore::open(&path).and_then(|store| store.schema_version())
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect::<Vec<_>>()
        });
        std::fs::remove_file(&path).unwrap();
        for result in results {
            assert_eq!(result.unwrap(), CURRENT_SCHEMA_VERSION);
        }
    }

    #[test]
    fn test_migration_failure_rolls_back_schema_and_versions() {
        let conn = Connection::open_in_memory().unwrap();
        // An incompatible preexisting table makes migration 002 fail.
        conn.execute_batch("CREATE TABLE agents (id TEXT PRIMARY KEY)")
            .unwrap();
        let store = RunStore {
            conn: Mutex::new(conn),
            redactor: Redactor::default(),
        };
        let error = store.migrate().unwrap_err().to_string();
        assert!(error.contains("failed to apply migration 002"), "{error}");
        let conn = store.conn.lock().unwrap();
        let created_tables: i32 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name IN ('schema_version', 'runs', 'events', 'agent_checkpoints')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(created_tables, 0);
        // The failure leaves the existing database intact.
        conn.execute("INSERT INTO agents (id) VALUES ('existing-agent')", [])
            .unwrap();
    }

    #[test]
    fn test_record_and_query_run_lifecycle() {
        let store = RunStore::in_memory().unwrap();
        let run_id = RunId::from("run_test_01");

        store
            .record_run_start(&run_id, "Inspect repository", "2026-10-08T00:00:00Z")
            .unwrap();

        let run = store.get_run(&run_id).unwrap().unwrap();
        assert_eq!(run.id, run_id);
        assert_eq!(run.status, "running");
        assert_eq!(run.task, "Inspect repository");

        store
            .record_run_completion(
                &run_id,
                "completed",
                "2026-10-08T00:00:05Z",
                5000,
                150,
                50,
                0.0003,
                None,
            )
            .unwrap();

        let finished = store.get_run(&run_id).unwrap().unwrap();
        assert_eq!(finished.status, "completed");
        assert_eq!(finished.duration_ms, Some(5000));
        assert_eq!(finished.tokens_total, 200);
        assert_eq!(finished.tokens_prompt, 150);
        assert_eq!(finished.tokens_completion, 50);
        assert!(finished.error.is_none());
    }

    #[test]
    fn test_record_and_query_events_ordering() {
        let store = RunStore::in_memory().unwrap();
        let run_id = RunId::from("run_test_seq");

        store
            .record_run_start(&run_id, "Test sequence", "2026-10-08T00:00:00Z")
            .unwrap();

        let ev1 = ExecutionEvent::RunStarted {
            run_id: run_id.clone(),
            task: "Test sequence".to_string(),
            workspace_root: None,
        };
        let ev2 = ExecutionEvent::ModelRequest {
            run_id: run_id.clone(),
            prompt_preview: "Prompt content".to_string(),
        };
        let ev3 = ExecutionEvent::RunCompleted {
            run_id: run_id.clone(),
            final_answer: "Done".to_string(),
            iterations: 1,
            duration_ms: 120,
        };

        store.record_event(&EventRecord::new(1, ev1)).unwrap();
        store.record_event(&EventRecord::new(2, ev2)).unwrap();
        store.record_event(&EventRecord::new(3, ev3)).unwrap();

        let events = store.get_events(&run_id).unwrap();
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].sequence, 1);
        assert_eq!(events[0].event.event_type(), "RunStarted");
        assert_eq!(events[1].sequence, 2);
        assert_eq!(events[1].event.event_type(), "ModelRequest");
        assert_eq!(events[2].sequence, 3);
        assert_eq!(events[2].event.event_type(), "RunCompleted");
    }

    #[test]
    fn test_list_runs() {
        let store = RunStore::in_memory().unwrap();
        for i in 1..=3 {
            let rid = RunId::from(format!("run_{}", i));
            store
                .record_run_start(
                    &rid,
                    &format!("task {}", i),
                    &format!("2026-10-08T00:00:0{}Z", i),
                )
                .unwrap();
        }

        let runs = store.list_runs(10).unwrap();
        assert_eq!(runs.len(), 3);
        // Latest first
        assert_eq!(runs[0].id.as_str(), "run_3");
    }

    #[test]
    fn test_inter_agent_message_persistence_and_query_by_run() {
        let store = RunStore::in_memory().unwrap();
        let run_id = RunId::from("run_multi_agent_01");
        store
            .record_run_start(&run_id, "Coordinated task", "2026-10-08T00:00:00Z")
            .unwrap();

        let supervisor = AgentId::from("supervisor");
        let worker = AgentId::from("worker_coder");
        let route = RoutingKey::new("task.delegate").unwrap();

        let msg1 = AgentMessage {
            id: "msg_1".into(),
            run_id: run_id.clone(),
            sender: supervisor.clone(),
            recipient: worker.clone(),
            routing_key: route.clone(),
            timestamp: "2026-10-08T00:00:01Z".into(),
            payload: AgentMessagePayload::TaskRequest {
                task_id: "task-001".into(),
                instructions: "Implement binary search".into(),
            },
        };

        let msg2 = AgentMessage {
            id: "msg_2".into(),
            run_id: run_id.clone(),
            sender: worker.clone(),
            recipient: supervisor.clone(),
            routing_key: route.clone(),
            timestamp: "2026-10-08T00:00:02Z".into(),
            payload: AgentMessagePayload::TaskResult {
                task_id: "task-001".into(),
                output: "Binary search implemented".into(),
            },
        };

        store.record_message(&msg1).unwrap();
        store.record_message(&msg2).unwrap();

        let messages = store.get_messages_for_run(&run_id).unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].id, "msg_1");
        assert_eq!(messages[0].sender, supervisor);
        assert_eq!(messages[0].recipient, worker);
        assert_eq!(
            messages[0].payload,
            AgentMessagePayload::TaskRequest {
                task_id: "task-001".into(),
                instructions: "Implement binary search".into(),
            }
        );

        assert_eq!(messages[1].id, "msg_2");
        assert_eq!(messages[1].sender, worker);
        assert_eq!(messages[1].recipient, supervisor);
        assert_eq!(
            messages[1].payload,
            AgentMessagePayload::TaskResult {
                task_id: "task-001".into(),
                output: "Binary search implemented".into(),
            }
        );
    }

    #[test]
    fn test_inter_agent_message_query_between_agents() {
        let store = RunStore::in_memory().unwrap();
        let run_1 = RunId::from("run_coord_1");
        let run_2 = RunId::from("run_coord_2");

        let agent_a = AgentId::from("agent_a");
        let agent_b = AgentId::from("agent_b");
        let agent_c = AgentId::from("agent_c");
        let route = RoutingKey::new("direct.channel").unwrap();

        let msg_ab_1 = AgentMessage {
            id: "m1".into(),
            run_id: run_1.clone(),
            sender: agent_a.clone(),
            recipient: agent_b.clone(),
            routing_key: route.clone(),
            timestamp: "2026-10-08T00:00:01Z".into(),
            payload: AgentMessagePayload::Notification {
                content: "Hello B from A".into(),
            },
        };

        let msg_ba_1 = AgentMessage {
            id: "m2".into(),
            run_id: run_1.clone(),
            sender: agent_b.clone(),
            recipient: agent_a.clone(),
            routing_key: route.clone(),
            timestamp: "2026-10-08T00:00:02Z".into(),
            payload: AgentMessagePayload::Notification {
                content: "Hello A from B".into(),
            },
        };

        let msg_ac_1 = AgentMessage {
            id: "m3".into(),
            run_id: run_2.clone(),
            sender: agent_a.clone(),
            recipient: agent_c.clone(),
            routing_key: route.clone(),
            timestamp: "2026-10-08T00:00:03Z".into(),
            payload: AgentMessagePayload::Notification {
                content: "Hello C from A".into(),
            },
        };

        store
            .record_messages(&[msg_ab_1, msg_ba_1, msg_ac_1])
            .unwrap();

        let conversation_ab = store.get_messages_between(&agent_a, &agent_b).unwrap();
        assert_eq!(conversation_ab.len(), 2);
        assert_eq!(conversation_ab[0].id, "m1");
        assert_eq!(conversation_ab[1].id, "m2");

        let conversation_ac = store.get_messages_between(&agent_a, &agent_c).unwrap();
        assert_eq!(conversation_ac.len(), 1);
        assert_eq!(conversation_ac[0].id, "m3");

        let conversation_bc = store.get_messages_between(&agent_b, &agent_c).unwrap();
        assert!(conversation_bc.is_empty());
    }

    #[test]
    fn test_inter_agent_message_secret_redaction() {
        let store = RunStore::in_memory().unwrap();
        let run_id = RunId::from("run_secret_leak_prevention");

        let secret_key = "sk-1234567890abcdef1234567890";
        let aws_key = "AKIAIOSFODNN7EXAMPLE";

        let msg = AgentMessage {
            id: "msg_sensitive".into(),
            run_id: run_id.clone(),
            sender: AgentId::from("supervisor"),
            recipient: AgentId::from("worker"),
            routing_key: RoutingKey::new("confidential.dispatch").unwrap(),
            timestamp: "2026-10-08T00:00:01Z".into(),
            payload: AgentMessagePayload::TaskRequest {
                task_id: "task-auth".into(),
                instructions: format!("Connect with key {} and AWS {}", secret_key, aws_key),
            },
        };

        store.record_message(&msg).unwrap();

        // 1. Verify that retrieved message payload is redacted
        let retrieved = store.get_messages_for_run(&run_id).unwrap();
        assert_eq!(retrieved.len(), 1);

        if let AgentMessagePayload::TaskRequest { instructions, .. } = &retrieved[0].payload {
            assert!(!instructions.contains(secret_key));
            assert!(!instructions.contains(aws_key));
            assert!(instructions.contains("[REDACTED]"));
        } else {
            panic!("expected TaskRequest payload");
        }

        // 2. Inspect raw SQLite storage to verify no cleartext secrets exist on disk
        let conn = store.conn.lock().unwrap();
        let raw_payload: String = conn
            .query_row(
                "SELECT payload FROM inter_agent_messages WHERE id = 'msg_sensitive'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!raw_payload.contains(secret_key));
        assert!(!raw_payload.contains(aws_key));
        assert!(raw_payload.contains("[REDACTED]"));
    }

    #[test]
    fn test_inter_agent_message_task_correlation() {
        let store = RunStore::in_memory().unwrap();
        let run_id = RunId::from("run_tasks_workflow");

        let msg_t1_req = AgentMessage {
            id: "m_t1_1".into(),
            run_id: run_id.clone(),
            sender: AgentId::from("lead"),
            recipient: AgentId::from("dev"),
            routing_key: RoutingKey::new("workflow.tasks").unwrap(),
            timestamp: "2026-10-08T00:00:01Z".into(),
            payload: AgentMessagePayload::TaskRequest {
                task_id: "task-xyz".into(),
                instructions: "Write parser".into(),
            },
        };

        let msg_t1_res = AgentMessage {
            id: "m_t1_2".into(),
            run_id: run_id.clone(),
            sender: AgentId::from("dev"),
            recipient: AgentId::from("lead"),
            routing_key: RoutingKey::new("workflow.tasks").unwrap(),
            timestamp: "2026-10-08T00:00:02Z".into(),
            payload: AgentMessagePayload::TaskResult {
                task_id: "task-xyz".into(),
                output: "Parser finished".into(),
            },
        };

        let msg_t2_req = AgentMessage {
            id: "m_t2_1".into(),
            run_id: run_id.clone(),
            sender: AgentId::from("lead"),
            recipient: AgentId::from("dev"),
            routing_key: RoutingKey::new("workflow.tasks").unwrap(),
            timestamp: "2026-10-08T00:00:03Z".into(),
            payload: AgentMessagePayload::TaskRequest {
                task_id: "task-abc".into(),
                instructions: "Write serializer".into(),
            },
        };

        store
            .record_messages(&[msg_t1_req, msg_t1_res, msg_t2_req])
            .unwrap();

        let task_xyz_msgs = store.get_messages_for_task("task-xyz").unwrap();
        assert_eq!(task_xyz_msgs.len(), 2);
        assert_eq!(task_xyz_msgs[0].id, "m_t1_1");
        assert_eq!(task_xyz_msgs[1].id, "m_t1_2");

        let task_abc_msgs = store.get_messages_for_task("task-abc").unwrap();
        assert_eq!(task_abc_msgs.len(), 1);
        assert_eq!(task_abc_msgs[0].id, "m_t2_1");
    }

    #[test]
    fn test_deterministic_replay_message_history() {
        let store = RunStore::in_memory().unwrap();
        let run_id = RunId::from("run_replay_audit");

        let sup = AgentId::from("supervisor");
        let worker = AgentId::from("worker");
        let route = RoutingKey::new("audit.exec").unwrap();

        let messages = vec![
            AgentMessage {
                id: "step_1".into(),
                run_id: run_id.clone(),
                sender: sup.clone(),
                recipient: worker.clone(),
                routing_key: route.clone(),
                timestamp: "2026-10-08T00:00:10Z".into(),
                payload: AgentMessagePayload::TaskRequest {
                    task_id: "t1".into(),
                    instructions: "Analyze vulnerability in dependencies".into(),
                },
            },
            AgentMessage {
                id: "step_2".into(),
                run_id: run_id.clone(),
                sender: worker.clone(),
                recipient: sup.clone(),
                routing_key: route.clone(),
                timestamp: "2026-10-08T00:00:15Z".into(),
                payload: AgentMessagePayload::TaskResult {
                    task_id: "t1".into(),
                    output: "No CVEs discovered".into(),
                },
            },
            AgentMessage {
                id: "step_3".into(),
                run_id: run_id.clone(),
                sender: sup.clone(),
                recipient: worker.clone(),
                routing_key: route.clone(),
                timestamp: "2026-10-08T00:00:20Z".into(),
                payload: AgentMessagePayload::Notification {
                    content: "Execution verified and completed".into(),
                },
            },
        ];

        store.record_messages(&messages).unwrap();

        // Reconstruct from store for deterministic replay
        let replayed = store.get_messages_for_run(&run_id).unwrap();
        assert_eq!(replayed.len(), 3);

        // Verify deterministic timeline and states
        for (original, reconstructed) in messages.iter().zip(replayed.iter()) {
            assert_eq!(original.id, reconstructed.id);
            assert_eq!(original.run_id, reconstructed.run_id);
            assert_eq!(original.sender, reconstructed.sender);
            assert_eq!(original.recipient, reconstructed.recipient);
            assert_eq!(original.routing_key, reconstructed.routing_key);
            assert_eq!(original.timestamp, reconstructed.timestamp);
            assert_eq!(original.payload, reconstructed.payload);
        }
    }
}
