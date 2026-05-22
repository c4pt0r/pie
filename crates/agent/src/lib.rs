//! pie-agent-core — Rust port of `@earendil-works/pie-agent-core`. Layered on top of `pie-ai`.
//! 1:1 file mapping with the TypeScript source at `packages/agent/src/`.

pub mod agent;
pub mod agent_loop;
pub mod node;
pub mod proxy;
pub mod types;

#[cfg(feature = "harness")]
pub mod harness;

// Public surface — mirrors `packages/agent/src/index.ts`.
pub use agent::{Agent, AgentListener, AgentOptions, AgentRunError};
pub use types::{
    AfterToolCallContext, AfterToolCallHook, AfterToolCallResult, AgentContext, AgentEvent,
    AgentLoopConfig, AgentLoopTurnUpdate, AgentMessage, AgentState, AgentTool, AgentToolCall,
    AgentToolError, AgentToolResult, AgentToolUpdate, BeforeToolCallContext, BeforeToolCallHook,
    BeforeToolCallResult, ConvertToLlm, CustomMessage, GetApiKey, MessageQueueProvider,
    PrepareNextTurnContext, PrepareNextTurnHook, QueueMode, ShouldStopAfterTurnContext,
    ShouldStopHook, StreamFn, ThinkingLevel, ToolExecutionMode, TransformContext,
    default_convert_to_llm,
};

#[cfg(feature = "harness")]
pub use harness::{
    agent_harness::{
        AgentHarness, AgentHarnessOptions, HarnessEvent, HarnessListener,
        NotificationStatusSnapshot,
    },
    compaction::{
        branch_summarization::{BranchSummaryResult, summarize_branch},
        compaction::{
            CompactionPreparation, CompactionResult, CompactionSettings, ContextUsageEstimate,
            CutPointResult, DEFAULT_COMPACTION_SETTINGS, GenerateSummaryOutput,
            GenerateSummaryRequest, SUMMARIZATION_SYSTEM_PROMPT, SummarizeError,
            calculate_context_tokens, compact, estimate_context_tokens, estimate_tokens,
            find_cut_point, find_turn_start_index, generate_summary, get_last_assistant_usage,
            prepare_compaction, serialize_conversation, should_compact,
        },
    },
    cost::{
        CostSnapshot, CostTracker, full_breakdown as cost_full_breakdown,
        one_line_summary as cost_one_line_summary,
    },
    messages,
    notification_hook::{
        DynNotificationHook, HookError, HookFuture, HookState, NotificationHook,
        NotificationHookStatus, TriggerSink,
    },
    permission::{PermissionDecision, PermissionPolicy},
    prompt_templates::{LoadTemplatesOutput, PromptTemplateRegistry, load_templates},
    session::{
        jsonl_repo::JsonlSessionRepo,
        jsonl_storage::JsonlSessionStorage,
        memory_repo::MemorySessionRepo,
        memory_storage::MemorySessionStorage,
        repo_utils::{
            ForkOptions, ForkPosition, create_session_id, create_timestamp, get_entries_to_fork,
            to_session,
        },
        session::{
            BranchSummaryInput, JsonlSessionMetadata, Session, SessionContext, SessionContextModel,
            SessionMetadata, SessionStorage, SessionTreeEntry, build_session_context,
        },
        uuid::uuidv7,
    },
    skills::{LoadSkillsOutput, format_skill_invocation, load_skills, load_sourced_skills},
    system_prompt::format_skills_for_system_prompt,
    trigger::{
        CredentialScope, PayloadVisibility, ReplacementPolicy, SourceKind, Trigger,
        TriggerAuthority, TriggerRecord, TriggerSource, TriggerState,
    },
    trigger_runtime::{
        EvaluationOutcome, TriggerRuntime, TriggerRuntimeConfig, TriggerRuntimeSnapshot,
    },
    types::{
        ExecOptions, ExecOutput, ExecResult, ExecutionEnv, ExecutionError, ExecutionErrorCode,
        FileError, FileErrorCode, FileInfo, FileKind, FsResult, PromptTemplate, SessionError,
        SessionErrorCode, Skill, SkillDiagnostic, SkillDiagnosticCode, SkillFrontmatter,
    },
};

#[cfg(all(feature = "harness", feature = "native-env"))]
pub use harness::env::native::NativeEnv;
