//! Local crontab-style scheduler.
//!
//! Cron jobs are a time-based source parallel to event triggers: the hook emits a normal
//! runtime [`Trigger`](pie_agent_core::Trigger) envelope, then the cron action hook maps
//! that accepted trigger into an `InjectAndRun` parent turn. Storage intentionally contains
//! only schedule/action text and never provider credentials.

use std::collections::BTreeSet;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Datelike, Local, Timelike, Utc};
use once_cell::sync::OnceCell;
use parking_lot::Mutex;
use pie_agent_core::{
    AgentHarness, AgentTool, AgentToolError, AgentToolResult, AgentToolUpdate,
    BeforeTriggerActionContext, BeforeTriggerActionHook, CredentialScope, HarnessEvent,
    HarnessListener, HookError, HookState, NotificationHook, NotificationHookStatus,
    PayloadVisibility, PromoteAction, ReplacementPolicy, SourceKind, ToolExecutionMode, Trigger,
    TriggerAction, TriggerAuthority, TriggerDelivery, TriggerSink, TriggerSource,
};
use pie_ai::{Tool, UserContentBlock};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::time::{Duration, MissedTickBehavior};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const CRON_SUBKIND: &str = "cron";
const TICK_SECS: u64 = 30;
const MAX_ACTION_PREVIEW_CHARS: usize = 120;
const MAX_ACTION_BYTES: usize = 4096;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CronJob {
    pub id: String,
    /// Standard 5-field cron expression: minute hour day-of-month month day-of-week.
    pub schedule: String,
    pub action: String,
    pub enabled: bool,
    #[serde(default)]
    pub running_trace_id: Option<String>,
    #[serde(default)]
    pub last_due_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub last_fired_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub last_completed_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub last_error: Option<String>,
    #[serde(default)]
    pub skipped_overlap_count: u64,
    pub created_at: DateTime<Utc>,
}

impl CronJob {
    pub fn next_run_after(&self, after: DateTime<Utc>) -> Option<DateTime<Utc>> {
        CronExpression::parse(&self.schedule)
            .ok()?
            .next_after(after)
    }
}

#[derive(Clone, Debug, Default)]
pub struct CronRegistry {
    inner: Arc<Mutex<CronRegistryState>>,
}

#[derive(Clone, Debug, Default)]
struct CronRegistryState {
    jobs: Vec<CronJob>,
    storage_path: Option<PathBuf>,
}

impl CronRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn load_from_path(&self, path: impl Into<PathBuf>) -> Result<(), CronStorageError> {
        let path = path.into();
        let mut jobs = read_jobs_file(&path)?;
        for job in &jobs {
            CronExpression::parse(&job.schedule)?;
        }
        let cleared_stale_running = clear_stale_running_state(&mut jobs);
        if cleared_stale_running {
            write_jobs_file(&path, &jobs)?;
        }
        let mut state = self.inner.lock();
        state.jobs = jobs;
        state.storage_path = Some(path);
        Ok(())
    }

    pub fn storage_path(&self) -> Option<PathBuf> {
        self.inner.lock().storage_path.clone()
    }

    pub fn list(&self) -> Vec<CronJob> {
        self.inner.lock().jobs.clone()
    }

    pub fn add_job(&self, schedule: &str, action: &str) -> Result<CronJob, AddCronJobError> {
        let schedule = schedule.trim();
        let action = action.trim();
        if action.is_empty() {
            return Err(AddCronJobError::EmptyAction);
        }
        if action.len() > MAX_ACTION_BYTES {
            return Err(AddCronJobError::ActionTooLarge {
                max_bytes: MAX_ACTION_BYTES,
            });
        }
        CronExpression::parse(schedule)?;
        let job = CronJob {
            id: format!("cron-{}", Uuid::new_v4().simple()),
            schedule: schedule.to_string(),
            action: action.to_string(),
            enabled: true,
            running_trace_id: None,
            last_due_at: None,
            last_fired_at: None,
            last_completed_at: None,
            last_error: None,
            skipped_overlap_count: 0,
            created_at: Utc::now(),
        };
        self.insert_job(job)
    }

    fn insert_job(&self, job: CronJob) -> Result<CronJob, AddCronJobError> {
        let mut state = self.inner.lock();
        let mut next = state.jobs.clone();
        next.push(job.clone());
        if let Some(path) = &state.storage_path {
            write_jobs_file(path, &next)?;
        }
        state.jobs = next;
        Ok(job)
    }

    pub fn remove_job(&self, id: &str) -> Result<Option<CronJob>, CronStorageError> {
        let id = id.trim();
        let mut state = self.inner.lock();
        let Some(pos) = state.jobs.iter().position(|job| job.id == id) else {
            return Ok(None);
        };
        let mut next = state.jobs.clone();
        let removed = next.remove(pos);
        if let Some(path) = &state.storage_path {
            write_jobs_file(path, &next)?;
        }
        state.jobs = next;
        Ok(Some(removed))
    }

    pub fn set_job_enabled(
        &self,
        id: &str,
        enabled: bool,
    ) -> Result<Option<CronJob>, CronStorageError> {
        let id = id.trim();
        let mut state = self.inner.lock();
        let Some(pos) = state.jobs.iter().position(|job| job.id == id) else {
            return Ok(None);
        };
        let mut next = state.jobs.clone();
        next[pos].enabled = enabled;
        if !enabled {
            next[pos].running_trace_id = None;
        }
        let updated = next[pos].clone();
        if let Some(path) = &state.storage_path {
            write_jobs_file(path, &next)?;
        }
        state.jobs = next;
        Ok(Some(updated))
    }

    fn due_jobs(&self, since: DateTime<Utc>, now: DateTime<Utc>) -> Vec<(CronJob, DateTime<Utc>)> {
        let mut state = self.inner.lock();
        let mut next = state.jobs.clone();
        let mut due = Vec::new();
        for job in &mut next {
            if !job.enabled {
                continue;
            }
            let Ok(expr) = CronExpression::parse(&job.schedule) else {
                job.last_error = Some("invalid schedule".into());
                continue;
            };
            let Some(due_at) = expr.next_after(since) else {
                job.last_error = Some("no next run within 5 years".into());
                continue;
            };
            if due_at > now {
                continue;
            }
            if job.running_trace_id.is_some() {
                job.skipped_overlap_count = job.skipped_overlap_count.saturating_add(1);
                job.last_due_at = Some(due_at);
                job.last_error = Some("skipped: previous run still active".into());
                continue;
            }
            let trace_id = format!("cron-{}", Uuid::new_v4().simple());
            job.running_trace_id = Some(trace_id.clone());
            job.last_due_at = Some(due_at);
            job.last_fired_at = Some(now);
            job.last_error = None;
            due.push((job.clone(), due_at));
        }
        // Ticks run every TICK_SECS for every session; only persist real state changes so
        // idle sessions don't accrete empty/rewritten sidecar files.
        if next != state.jobs {
            if let Some(path) = &state.storage_path {
                let _ = write_jobs_file(path, &next);
            }
            state.jobs = next;
        }
        due
    }

    pub fn mark_completed(&self, trace_id: &str, error: Option<String>) {
        let mut state = self.inner.lock();
        let Some(pos) = state
            .jobs
            .iter()
            .position(|job| job.running_trace_id.as_deref() == Some(trace_id))
        else {
            return;
        };
        let mut next = state.jobs.clone();
        next[pos].running_trace_id = None;
        next[pos].last_completed_at = Some(Utc::now());
        next[pos].last_error = error;
        if let Some(path) = &state.storage_path {
            let _ = write_jobs_file(path, &next);
        }
        state.jobs = next;
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub fn clear_for_tests(&self) {
        *self.inner.lock() = CronRegistryState::default();
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct CronJobsFile {
    #[serde(default)]
    jobs: Vec<CronJob>,
}

fn read_jobs_file(path: &Path) -> Result<Vec<CronJob>, CronStorageError> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let file: CronJobsFile =
                toml::from_str(&text).map_err(|err| CronStorageError::Parse(err.to_string()))?;
            Ok(file.jobs)
        }
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(Vec::new()),
        Err(err) => Err(CronStorageError::Io(err.to_string())),
    }
}

fn write_jobs_file(path: &Path, jobs: &[CronJob]) -> Result<(), CronStorageError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|err| CronStorageError::Io(err.to_string()))?;
    }
    let file = CronJobsFile {
        jobs: jobs.to_vec(),
    };
    let text = toml::to_string_pretty(&file)
        .map_err(|err| CronStorageError::Serialize(err.to_string()))?;
    std::fs::write(path, text).map_err(|err| CronStorageError::Io(err.to_string()))
}

fn clear_stale_running_state(jobs: &mut [CronJob]) -> bool {
    let mut changed = false;
    for job in jobs {
        if job.running_trace_id.is_some() {
            job.running_trace_id = None;
            job.last_error = Some("cleared stale running state on startup".into());
            changed = true;
        }
    }
    changed
}

pub fn global_cron_registry() -> &'static CronRegistry {
    static CELL: once_cell::sync::OnceCell<CronRegistry> = once_cell::sync::OnceCell::new();
    CELL.get_or_init(CronRegistry::new)
}

type HarnessCell = Arc<OnceCell<Arc<AgentHarness>>>;

pub struct NewCronJobTool {
    harness: Option<HarnessCell>,
}

pub struct ListCronJobsTool;

pub struct RemoveCronJobTool {
    harness: Option<HarnessCell>,
}

pub struct SetCronJobStateTool {
    harness: Option<HarnessCell>,
}

impl NewCronJobTool {
    pub fn new(harness: Option<HarnessCell>) -> Self {
        Self { harness }
    }
}

impl RemoveCronJobTool {
    pub fn new(harness: Option<HarnessCell>) -> Self {
        Self { harness }
    }
}

impl SetCronJobStateTool {
    pub fn new(harness: Option<HarnessCell>) -> Self {
        Self { harness }
    }
}

#[async_trait]
impl AgentTool for NewCronJobTool {
    fn definition(&self) -> &Tool {
        &NEW_CRON_JOB_TOOL
    }

    fn label(&self) -> &str {
        "NewCronJob"
    }

    fn execution_mode(&self) -> Option<ToolExecutionMode> {
        Some(ToolExecutionMode::Sequential)
    }

    async fn execute(
        &self,
        _id: &str,
        params: Value,
        _cancel: CancellationToken,
        _on_update: Option<AgentToolUpdate>,
    ) -> Result<AgentToolResult, AgentToolError> {
        let schedule = params
            .get("schedule")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AgentToolError::from("missing required arg: schedule"))?;
        let schedule = normalize_schedule(schedule)?;
        let action = params
            .get("action")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AgentToolError::from("missing required arg: action"))?;
        let job = global_cron_registry()
            .add_job(&schedule, action)
            .map_err(|e| AgentToolError::Message(e.to_string()))?;

        let audit_entry_id =
            write_tool_cron_control_audit(&self.harness, "add", None, Some(&job)).await;

        Ok(AgentToolResult {
            content: vec![UserContentBlock::text(format!(
                "created cron job {}\nschedule: {}\naction: {}",
                job.id,
                job.schedule,
                preview_redacted(&job.action, MAX_ACTION_PREVIEW_CHARS)
            ))],
            details: json!({
                "id": job.id,
                "schedule": job.schedule,
                "action": job.action,
                "enabled": job.enabled,
                "scope": "session",
                "audit_entry_id": audit_entry_id,
            }),
            terminate: None,
        })
    }
}

#[async_trait]
impl AgentTool for ListCronJobsTool {
    fn definition(&self) -> &Tool {
        &LIST_CRON_JOBS_TOOL
    }

    fn label(&self) -> &str {
        "ListCronJobs"
    }

    fn execution_mode(&self) -> Option<ToolExecutionMode> {
        Some(ToolExecutionMode::Parallel)
    }

    async fn execute(
        &self,
        _id: &str,
        _params: Value,
        _cancel: CancellationToken,
        _on_update: Option<AgentToolUpdate>,
    ) -> Result<AgentToolResult, AgentToolError> {
        let jobs = global_cron_registry().list();
        let storage_path = global_cron_registry()
            .storage_path()
            .map(|path| path.display().to_string());
        Ok(AgentToolResult {
            content: vec![UserContentBlock::text(render_cron_jobs_for_tool(&jobs))],
            details: json!({
                "count": jobs.len(),
                "scope": "session",
                "storage_path": storage_path,
                "jobs": jobs.iter().map(cron_job_details_for_model).collect::<Vec<_>>(),
            }),
            terminate: None,
        })
    }
}

#[async_trait]
impl AgentTool for RemoveCronJobTool {
    fn definition(&self) -> &Tool {
        &REMOVE_CRON_JOB_TOOL
    }

    fn label(&self) -> &str {
        "RemoveCronJob"
    }

    fn execution_mode(&self) -> Option<ToolExecutionMode> {
        Some(ToolExecutionMode::Sequential)
    }

    async fn execute(
        &self,
        _id: &str,
        params: Value,
        _cancel: CancellationToken,
        _on_update: Option<AgentToolUpdate>,
    ) -> Result<AgentToolResult, AgentToolError> {
        let id = params
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AgentToolError::from("missing required arg: id"))?;
        let job = global_cron_registry()
            .list()
            .into_iter()
            .find(|job| job.id == id);
        let Some(job) = job else {
            return Err(AgentToolError::Message(format!(
                "no cron job with id '{id}'"
            )));
        };
        let confirm = params
            .get("confirm")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if !confirm {
            return Ok(AgentToolResult {
                content: vec![UserContentBlock::text(format!(
                    "remove cron job {} requires confirmation\nschedule: {}\naction: {}\ncall RemoveCronJob again with confirm=true only after the user confirms",
                    job.id,
                    job.schedule,
                    preview_redacted(&job.action, MAX_ACTION_PREVIEW_CHARS)
                ))],
                details: json!({
                    "id": job.id,
                    "removed_count": 0,
                    "confirmation_required": true,
                    "scope": "session",
                    "action_preview": preview_redacted(&job.action, MAX_ACTION_PREVIEW_CHARS),
                }),
                terminate: None,
            });
        }

        let removed = global_cron_registry()
            .remove_job(id)
            .map_err(|e| AgentToolError::Message(e.to_string()))?;
        let Some(job) = removed else {
            return Err(AgentToolError::Message(format!(
                "no cron job with id '{id}'"
            )));
        };

        let audit_entry_id =
            write_tool_cron_control_audit(&self.harness, "remove", Some(&job), None).await;
        Ok(AgentToolResult {
            content: vec![UserContentBlock::text(format!(
                "removed cron job {}\nschedule: {}\naction: {}",
                job.id,
                job.schedule,
                preview_redacted(&job.action, MAX_ACTION_PREVIEW_CHARS)
            ))],
            details: json!({
                "id": job.id,
                "removed_count": 1,
                "scope": "session",
                "audit_entry_id": audit_entry_id,
            }),
            terminate: None,
        })
    }
}

#[async_trait]
impl AgentTool for SetCronJobStateTool {
    fn definition(&self) -> &Tool {
        &SET_CRON_JOB_STATE_TOOL
    }

    fn label(&self) -> &str {
        "SetCronJobState"
    }

    fn execution_mode(&self) -> Option<ToolExecutionMode> {
        Some(ToolExecutionMode::Sequential)
    }

    async fn execute(
        &self,
        _id: &str,
        params: Value,
        _cancel: CancellationToken,
        _on_update: Option<AgentToolUpdate>,
    ) -> Result<AgentToolResult, AgentToolError> {
        let id = params
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AgentToolError::from("missing required arg: id"))?;
        let enabled = params
            .get("enabled")
            .and_then(|v| v.as_bool())
            .ok_or_else(|| AgentToolError::from("missing required arg: enabled"))?;
        if enabled {
            return Err(AgentToolError::Message(
                "enabling cron jobs from model-facing tools requires user confirmation; use /cron enable <id>"
                    .into(),
            ));
        }
        let before = global_cron_registry()
            .list()
            .into_iter()
            .find(|job| job.id == id);
        let updated = global_cron_registry()
            .set_job_enabled(id, enabled)
            .map_err(|e| AgentToolError::Message(e.to_string()))?;
        let Some(job) = updated else {
            return Err(AgentToolError::Message(format!(
                "no cron job with id '{id}'"
            )));
        };

        let op = if enabled { "enable" } else { "disable" };
        let audit_entry_id =
            write_tool_cron_control_audit(&self.harness, op, before.as_ref(), Some(&job)).await;
        let state = if job.enabled { "enabled" } else { "disabled" };
        Ok(AgentToolResult {
            content: vec![UserContentBlock::text(format!(
                "updated cron job {}\nstate: {}\nschedule: {}\naction: {}",
                job.id,
                state,
                job.schedule,
                preview_redacted(&job.action, MAX_ACTION_PREVIEW_CHARS)
            ))],
            details: json!({
                "id": job.id,
                "schedule": job.schedule,
                "enabled": job.enabled,
                "scope": "session",
                "audit_entry_id": audit_entry_id,
            }),
            terminate: None,
        })
    }
}

pub struct CronNotificationHook {
    registry: CronRegistry,
    status: Arc<Mutex<NotificationHookStatus>>,
}

impl CronNotificationHook {
    pub fn new(registry: CronRegistry) -> Self {
        let mut status = NotificationHookStatus::pending();
        status.subscription_labels = vec!["local crontab".into()];
        Self {
            registry,
            status: Arc::new(Mutex::new(status)),
        }
    }
}

#[async_trait]
impl NotificationHook for CronNotificationHook {
    fn label(&self) -> &str {
        "cron"
    }

    async fn run(&self, sink: TriggerSink) -> Result<(), HookError> {
        {
            let mut status = self.status.lock();
            status.state = HookState::Connected;
            status.last_error = None;
        }
        let mut last_scan = Utc::now();
        let mut interval = tokio::time::interval(Duration::from_secs(TICK_SECS));
        interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
        interval.tick().await;
        loop {
            interval.tick().await;
            let now = Utc::now();
            for (job, due_at) in self.registry.due_jobs(last_scan, now) {
                let Some(trace_id) = job.running_trace_id.clone() else {
                    continue;
                };
                let trigger = cron_trigger_for_job(&job, due_at, trace_id);
                if sink.send(trigger).is_err() {
                    let mut status = self.status.lock();
                    status.state = HookState::Disconnected {
                        reason: "sink closed".into(),
                    };
                    status.last_error = Some("sink closed".into());
                    return Err(HookError::SinkClosed);
                }
                let mut status = self.status.lock();
                status.last_event_at = Some(now);
            }
            last_scan = now;
        }
    }

    fn status(&self) -> NotificationHookStatus {
        let mut status = self.status.lock().clone();
        let jobs = self.registry.list();
        status.queued_count = jobs
            .iter()
            .filter(|job| job.running_trace_id.is_some())
            .count() as u64;
        status.subscription_labels = if jobs.is_empty() {
            vec!["local crontab: 0 jobs".into()]
        } else {
            vec![format!(
                "local crontab: {} job(s), {} enabled",
                jobs.len(),
                jobs.iter().filter(|job| job.enabled).count()
            )]
        };
        status
    }
}

fn cron_trigger_for_job(job: &CronJob, due_at: DateTime<Utc>, trace_id: String) -> Trigger {
    Trigger {
        source: TriggerSource::Local {
            subkind: CRON_SUBKIND.into(),
        },
        source_kind: SourceKind::Local,
        source_label: "Cron".into(),
        event_label: job.id.clone(),
        payload_visibility: PayloadVisibility::Local,
        payload_summary: Some(format!(
            "cron `{}` due at {}: {}",
            job.id,
            due_at.to_rfc3339(),
            preview_redacted(&job.action, MAX_ACTION_PREVIEW_CHARS)
        )),
        payload: Some(json!({
            "job_id": job.id,
            "due_at": due_at.to_rfc3339(),
        })),
        idempotency_key: format!("cron:{}:{}", job.id, due_at.to_rfc3339()),
        replacement_policy: ReplacementPolicy::Drop,
        trace_id,
        authority: TriggerAuthority {
            principal_id: "local-cron".into(),
            principal_label: "local cron".into(),
            credential_scope: CredentialScope::None,
            allowed_source_actions: Vec::new(),
            expires_at: None,
        },
        received_at: Utc::now(),
    }
}

pub fn cron_action_hook(
    registry: CronRegistry,
    inner: BeforeTriggerActionHook,
) -> BeforeTriggerActionHook {
    Arc::new(
        move |ctx: BeforeTriggerActionContext, cancel: CancellationToken| {
            let registry = registry.clone();
            let is_cron = matches!(
                &ctx.trigger.source,
                TriggerSource::Local { subkind } if subkind == CRON_SUBKIND
            );
            if !is_cron {
                return inner(ctx, cancel);
            }
            let job_id = ctx
                .trigger
                .payload
                .as_ref()
                .and_then(|payload| payload.get("job_id"))
                .and_then(|v| v.as_str())
                .map(str::to_string);
            Box::pin(async move {
                let Some(job_id) = job_id else {
                    return TriggerAction::default_for(&ctx.trigger);
                };
                let Some(job) = registry.list().into_iter().find(|job| job.id == job_id) else {
                    return TriggerAction::default_for(&ctx.trigger);
                };
                TriggerAction {
                    prompt: job.action,
                    promote: PromoteAction::None,
                    promote_requires_approval: false,
                    delivery: TriggerDelivery::InjectAndRun,
                }
            })
        },
    )
}

pub fn cron_harness_listener(registry: CronRegistry) -> HarnessListener {
    Arc::new(move |event| match event {
        HarnessEvent::TriggerCompleted { trace_id, .. } => {
            registry.mark_completed(&trace_id, None);
        }
        HarnessEvent::TriggerFailed { trace_id, reason } => {
            registry.mark_completed(&trace_id, Some(reason.clone()));
        }
        _ => {}
    })
}

#[derive(Clone, Debug, thiserror::Error, PartialEq, Eq)]
pub enum AddCronJobError {
    #[error("cron action cannot be empty")]
    EmptyAction,
    #[error("cron action exceeds {max_bytes} bytes")]
    ActionTooLarge { max_bytes: usize },
    #[error("{0}")]
    Schedule(#[from] CronScheduleError),
    #[error("{0}")]
    Storage(#[from] CronStorageError),
}

#[derive(Clone, Debug, thiserror::Error, PartialEq, Eq)]
pub enum CronStorageError {
    #[error("cron storage io: {0}")]
    Io(String),
    #[error("parse cron storage: {0}")]
    Parse(String),
    #[error("serialize cron storage: {0}")]
    Serialize(String),
    #[error("{0}")]
    Schedule(#[from] CronScheduleError),
}

#[derive(Clone, Debug, thiserror::Error, PartialEq, Eq)]
pub enum CronScheduleError {
    #[error("cron schedule must have 5 fields: minute hour day-of-month month day-of-week")]
    WrongFieldCount,
    #[error("invalid cron field `{field}`: {reason}")]
    InvalidField { field: String, reason: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CronExpression {
    minutes: BTreeSet<u32>,
    hours: BTreeSet<u32>,
    days_of_month: BTreeSet<u32>,
    months: BTreeSet<u32>,
    days_of_week: BTreeSet<u32>,
}

impl CronExpression {
    fn parse(input: &str) -> Result<Self, CronScheduleError> {
        let parts: Vec<_> = input.split_whitespace().collect();
        if parts.len() != 5 {
            return Err(CronScheduleError::WrongFieldCount);
        }
        Ok(Self {
            minutes: parse_field(parts[0], 0, 59)?,
            hours: parse_field(parts[1], 0, 23)?,
            days_of_month: parse_field(parts[2], 1, 31)?,
            months: parse_field(parts[3], 1, 12)?,
            days_of_week: parse_day_of_week(parts[4])?,
        })
    }

    fn matches(&self, dt: DateTime<Utc>) -> bool {
        let local = dt.with_timezone(&Local);
        self.minutes.contains(&local.minute())
            && self.hours.contains(&local.hour())
            && self.days_of_month.contains(&local.day())
            && self.months.contains(&local.month())
            && self
                .days_of_week
                .contains(&local.weekday().num_days_from_sunday())
    }

    fn next_after(&self, after: DateTime<Utc>) -> Option<DateTime<Utc>> {
        let mut candidate = after + chrono::Duration::minutes(1);
        candidate = candidate.with_second(0)?.with_nanosecond(0)?;
        let limit = after + chrono::Duration::days(366 * 5);
        while candidate <= limit {
            if self.matches(candidate) {
                return Some(candidate);
            }
            candidate += chrono::Duration::minutes(1);
        }
        None
    }
}

fn parse_day_of_week(field: &str) -> Result<BTreeSet<u32>, CronScheduleError> {
    let mut set = parse_field(field, 0, 7)?;
    if set.remove(&7) {
        set.insert(0);
    }
    Ok(set)
}

fn parse_field(field: &str, min: u32, max: u32) -> Result<BTreeSet<u32>, CronScheduleError> {
    let mut out = BTreeSet::new();
    for part in field.split(',') {
        let part = part.trim();
        if part.is_empty() {
            return Err(CronScheduleError::InvalidField {
                field: field.into(),
                reason: "empty item".into(),
            });
        }
        let (range_part, step) = match part.split_once('/') {
            Some((range, step)) => {
                let step = step
                    .parse::<u32>()
                    .map_err(|_| CronScheduleError::InvalidField {
                        field: field.into(),
                        reason: "step must be a positive integer".into(),
                    })?;
                if step == 0 {
                    return Err(CronScheduleError::InvalidField {
                        field: field.into(),
                        reason: "step must be at least 1".into(),
                    });
                }
                (range, step)
            }
            None => (part, 1),
        };
        let (start, end) = if range_part == "*" {
            (min, max)
        } else if let Some((start, end)) = range_part.split_once('-') {
            (
                parse_number(field, start, min, max)?,
                parse_number(field, end, min, max)?,
            )
        } else {
            let value = parse_number(field, range_part, min, max)?;
            (value, value)
        };
        if start > end {
            return Err(CronScheduleError::InvalidField {
                field: field.into(),
                reason: "range start must be <= range end".into(),
            });
        }
        for value in (start..=end).step_by(step as usize) {
            out.insert(value);
        }
    }
    Ok(out)
}

fn parse_number(field: &str, raw: &str, min: u32, max: u32) -> Result<u32, CronScheduleError> {
    let value = raw
        .parse::<u32>()
        .map_err(|_| CronScheduleError::InvalidField {
            field: field.into(),
            reason: format!("`{raw}` is not a number"),
        })?;
    if !(min..=max).contains(&value) {
        return Err(CronScheduleError::InvalidField {
            field: field.into(),
            reason: format!("value {value} outside {min}-{max}"),
        });
    }
    Ok(value)
}

fn normalize_schedule(input: &str) -> Result<String, AgentToolError> {
    let trimmed = input.trim();
    if CronExpression::parse(trimmed).is_ok() {
        return Ok(trimmed.to_string());
    }

    let normalized = trimmed.to_lowercase();
    let alias = match normalized.as_str() {
        "hourly" | "every hour" | "once an hour" => Some("0 * * * *"),
        "daily" | "every day" | "once a day" => Some("0 9 * * *"),
        "weekly" | "every week" | "once a week" => Some("0 9 * * 1"),
        _ => {
            if trimmed.contains("每小时") || trimmed.contains("每個小時") {
                Some("0 * * * *")
            } else if trimmed.contains("每天") || trimmed.contains("每日") {
                Some("0 9 * * *")
            } else if trimmed.contains("每周") || trimmed.contains("每週") {
                Some("0 9 * * 1")
            } else {
                None
            }
        }
    };
    alias.map(str::to_string).ok_or_else(|| {
        AgentToolError::Message(
            "invalid schedule: provide a 5-field cron expression, or a supported alias such as hourly / every hour / 每小时"
                .into(),
        )
    })
}

pub fn cron_control_plane_audit(
    op: &str,
    actor: &str,
    before: Option<&CronJob>,
    after: Option<&CronJob>,
) -> Value {
    let job = after.or(before);
    let now = Utc::now();
    json!({
        "op": op,
        "actor": actor,
        "job_id": job.map(|job| job.id.as_str()),
        "schedule": job.map(|job| job.schedule.as_str()),
        "action_preview": job.map(|job| preview_redacted(&job.action, MAX_ACTION_PREVIEW_CHARS)),
        "before_enabled": before.map(|job| job.enabled),
        "after_enabled": after.map(|job| job.enabled),
        "next_run": after
            .filter(|job| job.enabled)
            .and_then(|job| job.next_run_after(now))
            .map(|dt| dt.to_rfc3339()),
        "removed": before.is_some() && after.is_none(),
    })
}

async fn write_tool_cron_control_audit(
    harness: &Option<HarnessCell>,
    op: &str,
    before: Option<&CronJob>,
    after: Option<&CronJob>,
) -> Option<String> {
    let harness = harness.as_ref().and_then(|cell| cell.get())?;
    let audit = cron_control_plane_audit(op, "tool", before, after);
    match harness
        .session()
        .append_custom("cron_control_plane", Some(audit))
        .await
    {
        Ok(id) => Some(id),
        Err(e) => {
            let job = after.or(before);
            tracing::warn!(
                op,
                actor = "tool",
                job_id = job.map(|job| job.id.as_str()),
                error = %e,
                "cron_control_plane audit write failed; tool cron change itself succeeded"
            );
            None
        }
    }
}

fn render_cron_jobs_for_tool(jobs: &[CronJob]) -> String {
    if jobs.is_empty() {
        return "session cron jobs: none".into();
    }

    let now = Utc::now();
    let mut lines = vec![format!("session cron jobs: {}", jobs.len())];
    for job in jobs {
        let state = if job.enabled { "enabled" } else { "disabled" };
        lines.push(format!(
            "- {} [{}] schedule: {} action: {}",
            job.id,
            state,
            job.schedule,
            preview_redacted(&job.action, MAX_ACTION_PREVIEW_CHARS)
        ));
        if let Some(next_run) = job
            .enabled
            .then(|| job.next_run_after(now))
            .flatten()
            .map(|dt| dt.to_rfc3339())
        {
            lines.push(format!("  next_run: {next_run}"));
        }
        if let Some(trace_id) = &job.running_trace_id {
            lines.push(format!("  running_trace_id: {trace_id}"));
        }
        if let Some(last_error) = &job.last_error {
            lines.push(format!(
                "  last_error: {}",
                preview_redacted(last_error, MAX_ACTION_PREVIEW_CHARS)
            ));
        }
        if job.skipped_overlap_count > 0 {
            lines.push(format!(
                "  skipped_overlap_count: {}",
                job.skipped_overlap_count
            ));
        }
    }
    lines.join("\n")
}

fn cron_job_details_for_model(job: &CronJob) -> Value {
    let now = Utc::now();
    json!({
        "id": job.id,
        "schedule": job.schedule,
        "action_preview": preview_redacted(&job.action, MAX_ACTION_PREVIEW_CHARS),
        "enabled": job.enabled,
        "scope": "session",
        "running_trace_id": job.running_trace_id,
        "last_due_at": job.last_due_at.map(|dt| dt.to_rfc3339()),
        "last_fired_at": job.last_fired_at.map(|dt| dt.to_rfc3339()),
        "last_completed_at": job.last_completed_at.map(|dt| dt.to_rfc3339()),
        "last_error": job
            .last_error
            .as_ref()
            .map(|err| preview_redacted(err, MAX_ACTION_PREVIEW_CHARS)),
        "skipped_overlap_count": job.skipped_overlap_count,
        "next_run": job
            .enabled
            .then(|| job.next_run_after(now))
            .flatten()
            .map(|dt| dt.to_rfc3339()),
        "created_at": job.created_at.to_rfc3339(),
    })
}

fn preview_redacted(input: &str, max_chars: usize) -> String {
    preview(&crate::bug_report::redact(input), max_chars)
}

fn preview(input: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (idx, ch) in input.chars().enumerate() {
        if idx == max_chars {
            out.push('…');
            return out;
        }
        out.push(ch);
    }
    out
}

static NEW_CRON_JOB_TOOL: once_cell::sync::Lazy<Tool> = once_cell::sync::Lazy::new(|| Tool {
    name: "NewCronJob".into(),
    description: "Create a session-scoped cron scheduled job. Use this when the user asks for a \
         fixed time, recurring, scheduled, hourly, daily, weekly, crontab, 定时任务, 每小时, \
         每天, or similar time-based job. Do not use NewTrigger for these scheduled jobs. \
         Cron jobs are scoped to the current chat session by default."
        .into(),
    parameters: json!({
        "type": "object",
        "properties": {
            "schedule": {
                "type": "string",
                "description": "A 5-field cron expression in local time (minute hour day-of-month month day-of-week), or a supported alias such as hourly / every hour / 每小时."
            },
            "action": {
                "type": "string",
                "description": "Natural-language instruction to run when the schedule is due."
            }
        },
        "required": ["schedule", "action"],
        "additionalProperties": false,
    }),
});

static LIST_CRON_JOBS_TOOL: once_cell::sync::Lazy<Tool> = once_cell::sync::Lazy::new(|| Tool {
    name: "ListCronJobs".into(),
    description: "List the session-scoped cron scheduled jobs. Use this when the user asks to \
         view, list, inspect, or find scheduled jobs, cron jobs, crontab entries, 定时任务, \
         or recurring jobs."
        .into(),
    parameters: json!({
        "type": "object",
        "properties": {},
        "additionalProperties": false,
    }),
});

static REMOVE_CRON_JOB_TOOL: once_cell::sync::Lazy<Tool> = once_cell::sync::Lazy::new(|| Tool {
    name: "RemoveCronJob".into(),
    description: "Preview or confirm removal of a session-scoped cron scheduled job by exact id. \
         Use confirm=false first when the user asks to delete, remove, or clear a scheduled job, \
         cron job, crontab entry, or 定时任务. Call confirm=true only after the user explicitly \
         confirms removal."
        .into(),
    parameters: json!({
        "type": "object",
        "properties": {
            "id": {
                "type": "string",
                "description": "Exact cron job id, for example cron-abc123."
            },
            "confirm": {
                "type": "boolean",
                "description": "false to preview the removal; true only after explicit user confirmation."
            }
        },
        "required": ["id"],
        "additionalProperties": false,
    }),
});

static SET_CRON_JOB_STATE_TOOL: once_cell::sync::Lazy<Tool> = once_cell::sync::Lazy::new(|| Tool {
    name: "SetCronJobState".into(),
    description: "Disable a session-scoped cron scheduled job by exact id. Model-facing \
             enable/resume is refused until control-plane confirmation is wired; use \
             /cron enable <id> for enabling."
        .into(),
    parameters: json!({
        "type": "object",
        "properties": {
            "id": {
                "type": "string",
                "description": "Exact cron job id, for example cron-abc123."
            },
            "enabled": {
                "type": "boolean",
                "description": "true to enable/resume the cron job; false to disable/pause it."
            }
        },
        "required": ["id", "enabled"],
        "additionalProperties": false,
    }),
});

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use pie_agent_core::TriggerRecord;
    use tempfile::tempdir;

    #[test]
    fn cron_parser_supports_steps_ranges_and_sunday_alias() {
        let expr = CronExpression::parse("*/15 9-17 * * 1,7").unwrap();
        assert!(expr.minutes.contains(&0));
        assert!(expr.minutes.contains(&45));
        assert!(expr.hours.contains(&9));
        assert!(expr.hours.contains(&17));
        assert!(expr.days_of_week.contains(&0));
        assert!(expr.days_of_week.contains(&1));
    }

    #[test]
    fn cron_parser_rejects_invalid_schedule() {
        assert!(CronExpression::parse("* * * *").is_err());
        assert!(CronExpression::parse("60 * * * *").is_err());
        assert!(CronExpression::parse("*/0 * * * *").is_err());
    }

    #[test]
    fn next_after_uses_local_time_and_does_not_return_current_minute() {
        let expr = CronExpression::parse("5 * * * *").unwrap();
        let base = Utc.with_ymd_and_hms(2026, 5, 26, 22, 5, 0).unwrap();
        let next = expr.next_after(base).unwrap();
        let local = next.with_timezone(&Local);
        assert_eq!(local.minute(), 5);
        assert!(next > base);
    }

    #[test]
    fn registry_round_trips_storage_and_enable_state() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("cron.toml");
        let registry = CronRegistry::new();
        registry.load_from_path(&path).unwrap();
        let job = registry.add_job("*/10 * * * *", "say hello").unwrap();
        registry.set_job_enabled(&job.id, false).unwrap();

        let reloaded = CronRegistry::new();
        reloaded.load_from_path(&path).unwrap();
        let jobs = reloaded.list();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].schedule, "*/10 * * * *");
        assert_eq!(jobs[0].action, "say hello");
        assert!(!jobs[0].enabled);
    }

    #[test]
    fn due_jobs_tick_writes_sidecar_only_when_state_changed() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("cron.toml");
        let registry = CronRegistry::new();
        registry.load_from_path(&path).unwrap();
        let since = Utc.with_ymd_and_hms(2026, 5, 26, 22, 0, 0).unwrap();
        let now = Utc.with_ymd_and_hms(2026, 5, 26, 22, 1, 5).unwrap();

        // Empty registry: an idle tick must not create the sidecar.
        assert!(registry.due_jobs(since, now).is_empty());
        assert!(!path.exists(), "idle tick created an empty sidecar");

        // Job exists but is not due: tick must not rewrite the file.
        registry.add_job("0 0 1 1 *", "yearly job").unwrap();
        std::fs::remove_file(&path).unwrap();
        assert!(registry.due_jobs(since, now).is_empty());
        assert!(!path.exists(), "no-op tick rewrote the sidecar");

        // A due job is a real state change and must persist.
        registry.add_job("* * * * *", "every minute").unwrap();
        assert_eq!(registry.due_jobs(since, now).len(), 1);
        assert!(path.exists(), "firing tick must persist job state");
    }

    #[test]
    fn load_clears_stale_running_state_from_previous_process() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("cron.toml");
        let registry = CronRegistry::new();
        registry.load_from_path(&path).unwrap();
        let job = registry.add_job("* * * * *", "say hello").unwrap();
        let since = Utc.with_ymd_and_hms(2026, 5, 26, 22, 0, 0).unwrap();
        let now = Utc.with_ymd_and_hms(2026, 5, 26, 22, 1, 5).unwrap();
        assert_eq!(registry.due_jobs(since, now).len(), 1);
        assert!(registry.list()[0].running_trace_id.is_some());

        let reloaded = CronRegistry::new();
        reloaded.load_from_path(&path).unwrap();
        let jobs = reloaded.list();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].id, job.id);
        assert!(jobs[0].running_trace_id.is_none());
        assert_eq!(
            jobs[0].last_error.as_deref(),
            Some("cleared stale running state on startup")
        );

        let persisted = read_jobs_file(&path).unwrap();
        assert!(persisted[0].running_trace_id.is_none());
    }

    #[test]
    fn registry_rejects_oversized_action() {
        let registry = CronRegistry::new();
        let err = registry
            .add_job("* * * * *", &"x".repeat(MAX_ACTION_BYTES + 1))
            .unwrap_err();
        assert!(matches!(err, AddCronJobError::ActionTooLarge { .. }));
    }

    #[test]
    fn trigger_summary_redacts_secret_like_action_text() {
        let registry = CronRegistry::new();
        let secret = "sk-abcdefghijklmnopqrstuvwxyz123456";
        let bearer = "Bearer abcdefghijklmnopqrstuvwxyz";
        let job = registry
            .add_job("* * * * *", &format!("use token {secret} and {bearer}"))
            .unwrap();
        let trigger = cron_trigger_for_job(&job, Utc::now(), "trace-cron".into());
        let record = TriggerRecord::received_from(&trigger);
        let summary = record.payload_summary.unwrap();
        assert!(!summary.contains(secret), "{summary}");
        assert!(!summary.contains(bearer), "{summary}");
        assert!(summary.contains("[REDACTED:"), "{summary}");
    }

    #[test]
    fn due_jobs_marks_running_and_skips_overlap() {
        let registry = CronRegistry::new();
        let job = registry.add_job("* * * * *", "do work").unwrap();
        let since = Utc.with_ymd_and_hms(2026, 5, 26, 22, 0, 0).unwrap();
        let now = Utc.with_ymd_and_hms(2026, 5, 26, 22, 1, 5).unwrap();
        let due = registry.due_jobs(since, now);
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].0.id, job.id);
        assert!(registry.list()[0].running_trace_id.is_some());

        let later = Utc.with_ymd_and_hms(2026, 5, 26, 22, 2, 5).unwrap();
        let skipped = registry.due_jobs(now, later);
        assert!(skipped.is_empty());
        assert_eq!(registry.list()[0].skipped_overlap_count, 1);
    }

    #[test]
    fn listener_clears_running_job_by_trace_id() {
        let registry = CronRegistry::new();
        registry.add_job("* * * * *", "do work").unwrap();
        let since = Utc.with_ymd_and_hms(2026, 5, 26, 22, 0, 0).unwrap();
        let now = Utc.with_ymd_and_hms(2026, 5, 26, 22, 1, 5).unwrap();
        let trace_id = registry.due_jobs(since, now)[0]
            .0
            .running_trace_id
            .clone()
            .unwrap();
        let listener = cron_harness_listener(registry.clone());
        listener(HarnessEvent::TriggerCompleted {
            trace_id,
            summary: None,
            cost_usd: None,
            details: serde_json::Value::Null,
        });
        let job = registry.list().remove(0);
        assert!(job.running_trace_id.is_none());
        assert!(job.last_completed_at.is_some());
    }

    #[tokio::test]
    async fn cron_action_hook_maps_cron_trigger_to_inject_and_run() {
        let registry = CronRegistry::new();
        let job = registry.add_job("* * * * *", "run tests").unwrap();
        let trigger = cron_trigger_for_job(&job, Utc::now(), "trace-cron".into());
        let inner: BeforeTriggerActionHook = Arc::new(|ctx, _cancel| {
            Box::pin(async move { TriggerAction::default_for(&ctx.trigger) })
        });
        let hook = cron_action_hook(registry, inner);
        let action = hook(
            BeforeTriggerActionContext {
                trigger,
                runtime: pie_agent_core::TriggerRuntimeSnapshot {
                    dedup_entries: 0,
                    active_traces: 0,
                    accepted_total: 0,
                    deduped_total: 0,
                    cycle_suppressed_total: 0,
                },
            },
            CancellationToken::new(),
        )
        .await;
        assert_eq!(action.prompt, "run tests");
        assert_eq!(action.delivery, TriggerDelivery::InjectAndRun);
    }
}
