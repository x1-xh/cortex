//! # cortex-runtime
//!
//! Runtime abstractions, tool execution contracts, model interfaces, workspace boundaries,
//! and sandboxing boundaries for the Cortex agent harness.
//!
//! Cortex strictly separates model generation from runtime execution authority:
//! models propose actions, and the runtime validates, authorizes, and executes
//! them within bounded workspaces.

#![deny(missing_docs)]

pub mod agent;
pub mod mcp;
pub mod model;
pub mod providers;
pub mod sandbox;
pub mod scheduler;
pub mod storage;
pub mod tool;
pub mod tools;
pub mod workflow;
pub mod workspace;

pub use agent::{
    Agent, AgentCapability, AgentContext, AgentDescriptor, AgentEndpoint, AgentLifecycleEvent,
    AgentLoop, AgentManager, AgentManifest, AgentMessage, AgentMessagePayload, AgentModelConfig,
    AgentPermissions, AgentRegistry, AgentRunResult, AgentState, AgentYamlManifest, AgentYamlModel,
    AgentYamlPermissions, BusMessage, CancellationToken, ChatMessage, DeadLetterEnvelope,
    DeadLetterReason, MessageBus, MessageType, PermissionState, RoutingKey, SessionMetadata,
    TaskPriority, TaskQueue, TaskStatus, TodoItem, WorkflowCoordinator, WorkflowTask,
    YamlPermissionValue, CODING_AGENT_POLICY,
};
pub use mcp::{CortexConfig, McpClient, McpConfig, McpManager, McpServerConfig, McpTool};
pub use model::{
    MockModelProvider, ModelDescriptor, ModelOutput, ModelProvider, ModelUsage,
    ReplayModelProvider, ToolCall,
};
pub use providers::{
    create_model_provider, estimate_cost, AnthropicProvider, OpenAiCompatibleProvider,
};
pub use sandbox::{
    DockerSandbox, DockerSandboxConfig, HostSandbox, NetworkIsolationPolicy, Sandbox,
    SandboxExecutionResult, SandboxMode,
};
pub use scheduler::{
    CronExpression, CronField, CronJobStats, CronParseError, JobRunRecord, JobRunStatus, JobStatus,
    OverlapPolicy, Schedule, ScheduledJob, SchedulerEngine, TriggerResult,
};
pub use storage::{AgentCheckpointRecord, AgentRecord, RunStore, RunSummary, SecretRedactor};
pub use tool::{validate_schema, PermissionLevel, Tool, ToolDefinition, ToolRegistry, ToolResult};
pub use workflow::{
    WorkflowAgentRef, WorkflowManifest, WorkflowManifestExt, WorkflowStage, WorkflowYamlParser,
};
pub use workspace::Workspace;

/// Current semantic version of the Cortex runtime crate.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    use super::*;

    struct DummyTool;
    impl Tool for DummyTool {
        fn definition(&self) -> &ToolDefinition {
            static DEF: std::sync::OnceLock<ToolDefinition> = std::sync::OnceLock::new();
            DEF.get_or_init(|| {
                ToolDefinition::new(
                    "dummy",
                    "A dummy test tool",
                    serde_json::json!({ "type": "object" }),
                )
            })
        }

        fn execute(&self, _input: &serde_json::Value) -> cortex_core::Result<ToolResult> {
            Ok(ToolResult::success("dummy output"))
        }
    }

    #[test]
    fn test_runtime_version() {
        assert!(!VERSION.is_empty());
    }

    #[test]
    fn test_dummy_tool_contract() {
        let tool = DummyTool;
        assert_eq!(tool.name(), "dummy");
        assert!(tool.is_available().unwrap());
        let res = tool.execute(&serde_json::json!({})).unwrap();
        assert_eq!(res.output, "dummy output");
    }

    #[test]
    fn test_tool_definition() {
        let def = ToolDefinition::new(
            "read_file",
            "Reads file contents from disk",
            serde_json::json!({ "type": "object" }),
        );
        assert_eq!(def.name, "read_file");
        assert_eq!(def.description, "Reads file contents from disk");
    }

    #[test]
    fn test_model_descriptor() {
        let desc = ModelDescriptor::new("google", "gemma-2-9b");
        assert_eq!(desc.provider, "google");
        assert_eq!(desc.name, "gemma-2-9b");
    }
}
