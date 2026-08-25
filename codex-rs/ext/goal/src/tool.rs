use std::sync::Arc;

use codex_extension_api::FunctionCallError;
use codex_extension_api::JsonToolOutput;
use codex_extension_api::ToolCall;
use codex_extension_api::ToolExecutor;
use codex_extension_api::ToolName;
use codex_extension_api::ToolOutput;
use codex_extension_api::ToolSpec;
use codex_protocol::ThreadId;
use codex_protocol::protocol::ThreadGoal;
use codex_protocol::protocol::ThreadGoalStatus;
use codex_protocol::protocol::validate_thread_goal_objective;
use serde::Deserialize;
use serde::Serialize;

use crate::accounting::BlockedGoalDecision;
use crate::accounting::BlockedGoalReceipt;
use crate::accounting::BudgetLimitedGoalDisposition;
use crate::accounting::GoalAccountingState;
use crate::accounting::REQUIRED_CONSECUTIVE_BLOCKED_TURNS;
use crate::analytics::GoalAnalytics;
use crate::analytics::GoalEventAttribution;
use crate::events::GoalEventEmitter;
use crate::metrics::GoalMetrics;
use crate::spec::CREATE_GOAL_TOOL_NAME;
use crate::spec::GET_GOAL_TOOL_NAME;
use crate::spec::UPDATE_GOAL_TOOL_NAME;
use crate::spec::create_create_goal_tool;
use crate::spec::create_get_goal_tool;
use crate::spec::create_update_goal_tool;

const EXTERNALLY_REPLACED_GOAL_UPDATE_ERROR: &str = "cannot update goal because the active goal was set or replaced externally during this turn; continue working on the updated objective and let a later goal turn mark it complete or blocked";

#[derive(Clone)]
pub(crate) struct GoalToolExecutor {
    kind: GoalToolKind,
    thread_id: ThreadId,
    state_db: Arc<codex_state::StateRuntime>,
    accounting_state: Arc<GoalAccountingState>,
    analytics: GoalAnalytics,
    event_emitter: GoalEventEmitter,
    metrics: GoalMetrics,
}

#[derive(Clone, Copy)]
enum GoalToolKind {
    Get,
    Create,
    Update,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct CreateGoalRequest {
    pub objective: String,
    pub token_budget: Option<i64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
struct UpdateGoalArgs {
    status: ThreadGoalStatus,
    blocked_receipt: Option<BlockedGoalReceiptArgs>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
struct BlockedGoalReceiptArgs {
    blocker_fingerprint: String,
    summary: String,
    blocked_on: String,
    affected_resources: Vec<String>,
    evidence_fingerprint: String,
    evidence_summary: String,
    attempted_actions: Vec<String>,
    remaining_independent_work: Vec<String>,
    retry_condition: String,
}

#[derive(Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct GoalToolResponse {
    goal: Option<ThreadGoal>,
    remaining_tokens: Option<i64>,
    completion_budget_report: Option<String>,
}

#[derive(Clone, Copy)]
enum CompletionBudgetReport {
    Include,
    Omit,
}

impl GoalToolExecutor {
    pub(crate) fn get(
        thread_id: ThreadId,
        state_db: Arc<codex_state::StateRuntime>,
        accounting_state: Arc<GoalAccountingState>,
        analytics: GoalAnalytics,
        event_emitter: GoalEventEmitter,
        metrics: GoalMetrics,
    ) -> Self {
        Self {
            kind: GoalToolKind::Get,
            thread_id,
            state_db,
            accounting_state,
            analytics,
            event_emitter,
            metrics,
        }
    }

    pub(crate) fn create(
        thread_id: ThreadId,
        state_db: Arc<codex_state::StateRuntime>,
        accounting_state: Arc<GoalAccountingState>,
        analytics: GoalAnalytics,
        event_emitter: GoalEventEmitter,
        metrics: GoalMetrics,
    ) -> Self {
        Self {
            kind: GoalToolKind::Create,
            thread_id,
            state_db,
            accounting_state,
            analytics,
            event_emitter,
            metrics,
        }
    }

    pub(crate) fn update(
        thread_id: ThreadId,
        state_db: Arc<codex_state::StateRuntime>,
        accounting_state: Arc<GoalAccountingState>,
        analytics: GoalAnalytics,
        event_emitter: GoalEventEmitter,
        metrics: GoalMetrics,
    ) -> Self {
        Self {
            kind: GoalToolKind::Update,
            thread_id,
            state_db,
            accounting_state,
            analytics,
            event_emitter,
            metrics,
        }
    }
}

impl ToolExecutor<ToolCall> for GoalToolExecutor {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(match self.kind {
            GoalToolKind::Get => GET_GOAL_TOOL_NAME,
            GoalToolKind::Create => CREATE_GOAL_TOOL_NAME,
            GoalToolKind::Update => UPDATE_GOAL_TOOL_NAME,
        })
    }

    fn spec(&self) -> ToolSpec {
        match self.kind {
            GoalToolKind::Get => create_get_goal_tool(),
            GoalToolKind::Create => create_create_goal_tool(),
            GoalToolKind::Update => create_update_goal_tool(),
        }
    }

    fn handle(&self, invocation: ToolCall) -> codex_extension_api::ToolExecutorFuture<'_> {
        Box::pin(async move {
            match self.kind {
                GoalToolKind::Get => self.handle_get(invocation).await,
                GoalToolKind::Create => self.handle_create(invocation).await,
                GoalToolKind::Update => self.handle_update(invocation).await,
            }
        })
    }
}

impl GoalToolExecutor {
    async fn handle_get(
        &self,
        invocation: ToolCall,
    ) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        let _ = invocation.function_arguments()?;
        let goal = self
            .state_db
            .thread_goals()
            .get_thread_goal(self.thread_id)
            .await
            .map(|goal| goal.map(protocol_goal_from_state))
            .map_err(|err| {
                FunctionCallError::RespondToModel(format!("failed to read goal: {err}"))
            })?;
        goal_response(goal, CompletionBudgetReport::Omit)
    }

    async fn handle_create(
        &self,
        invocation: ToolCall,
    ) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        let mut request: CreateGoalRequest = parse_arguments(invocation.function_arguments()?)?;
        request.objective = request.objective.trim().to_string();
        validate_thread_goal_objective(&request.objective)
            .map_err(FunctionCallError::RespondToModel)?;
        validate_goal_budget(request.token_budget).map_err(FunctionCallError::RespondToModel)?;
        let _goal_state_permit =
            self.accounting_state
                .goal_state_permit()
                .await
                .map_err(|err| {
                    FunctionCallError::Fatal(format!("goal state semaphore closed: {err}"))
                })?;

        let goal = self
            .state_db
            .thread_goals()
            .insert_thread_goal(
                self.thread_id,
                request.objective.as_str(),
                codex_state::ThreadGoalStatus::Active,
                request.token_budget,
            )
            .await
            .map_err(|err| FunctionCallError::RespondToModel(format!("failed to create goal: {err}")))?
            .ok_or_else(|| {
                FunctionCallError::RespondToModel(
                    "cannot create a new goal because this thread has an unfinished goal; complete the existing goal first"
                        .to_string(),
                )
            })?;
        fill_empty_thread_preview_if_possible(self.state_db.as_ref(), self.thread_id, &goal).await;
        let turn_id = self
            .accounting_state
            .mark_current_turn_goal_active(goal.goal_id.clone());
        self.metrics.record_created();
        self.analytics.created(
            &goal,
            GoalEventAttribution::Turn(invocation.turn_id.as_str()),
        );
        let goal = protocol_goal_from_state(goal);
        self.emit_goal_updated_from_tool_call(&invocation, turn_id, goal.clone());
        goal_response(Some(goal), CompletionBudgetReport::Omit)
    }

    async fn handle_update(
        &self,
        invocation: ToolCall,
    ) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        let args: UpdateGoalArgs = parse_arguments(invocation.function_arguments()?)?;
        if !matches!(
            args.status,
            ThreadGoalStatus::Complete | ThreadGoalStatus::Blocked
        ) {
            return Err(FunctionCallError::RespondToModel(
                "update_goal can only mark the existing goal complete or blocked; pause, resume, budget-limited, and usage-limited status changes are controlled by the user or system"
                    .to_string(),
            ));
        }
        let _goal_state_permit =
            self.accounting_state
                .goal_state_permit()
                .await
                .map_err(|err| {
                    FunctionCallError::Fatal(format!("goal state semaphore closed: {err}"))
                })?;
        let expected_goal_id = self
            .accounting_state
            .terminal_update_goal_id_for_turn(invocation.turn_id.as_str())
            .ok_or_else(|| {
                FunctionCallError::RespondToModel(EXTERNALLY_REPLACED_GOAL_UPDATE_ERROR.to_string())
            })?;
        if args.status == ThreadGoalStatus::Blocked {
            let receipt = args
                .blocked_receipt
                .as_ref()
                .ok_or_else(|| {
                    FunctionCallError::RespondToModel(
                        "cannot mark this goal blocked without blocked_receipt: provide a scoped blocker fingerprint, fresh evidence fingerprint and summary, meaningful attempted_actions, affected_resources, blocked_on, retry_condition, and an empty remaining_independent_work list"
                            .to_string(),
                    )
                })?;
            validate_blocked_goal_receipt(receipt).map_err(FunctionCallError::RespondToModel)?;
            let accounting_receipt = BlockedGoalReceipt {
                condition_fingerprint: receipt.blocker_fingerprint.clone(),
                evidence_fingerprint: receipt.evidence_fingerprint.clone(),
            };
            let decision = self
                .accounting_state
                .record_blocked_goal_attempt(
                    invocation.turn_id.as_str(),
                    expected_goal_id.as_str(),
                    &accounting_receipt,
                )
                .ok_or_else(|| {
                    FunctionCallError::RespondToModel(
                        EXTERNALLY_REPLACED_GOAL_UPDATE_ERROR.to_string(),
                    )
                })?;
            match decision {
                BlockedGoalDecision::Continue {
                    blocked_turns,
                    audit_restarted,
                } => {
                    let restart_notice = if audit_restarted {
                        " The blocker fingerprint changed, so the audit restarted at 1/3."
                    } else {
                        ""
                    };
                    return Err(FunctionCallError::RespondToModel(format!(
                        "cannot mark this goal blocked yet: blocked audit {blocked_turns}/{REQUIRED_CONSECUTIVE_BLOCKED_TURNS}.{restart_notice} Keep the goal active and continue authorized independent work. On a later goal turn, re-check the same scoped external condition and submit a new evidence_fingerprint from that fresh observation after a meaningful attempt. Do not wait for symbolic permission or repeat stale evidence."
                    )));
                }
                BlockedGoalDecision::AlreadyRecorded { blocked_turns } => {
                    return Err(FunctionCallError::RespondToModel(format!(
                        "cannot advance the blocked audit twice in one turn: it remains {blocked_turns}/{REQUIRED_CONSECUTIVE_BLOCKED_TURNS}. Continue working; a repeated or rewritten receipt in this turn does not count."
                    )));
                }
                BlockedGoalDecision::StaleEvidence { blocked_turns } => {
                    return Err(FunctionCallError::RespondToModel(format!(
                        "cannot advance the blocked audit with stale evidence: it remains {blocked_turns}/{REQUIRED_CONSECUTIVE_BLOCKED_TURNS}. Re-check the real external condition, try another authorized approach, and use a new evidence_fingerprint only for genuinely fresh evidence."
                    )));
                }
                BlockedGoalDecision::Allow => {}
            }
        }

        self.account_active_goal_progress(
            match args.status {
                ThreadGoalStatus::Complete => codex_state::GoalAccountingMode::ActiveOrComplete,
                ThreadGoalStatus::Blocked => codex_state::GoalAccountingMode::ActiveOrStopped,
                ThreadGoalStatus::Active
                | ThreadGoalStatus::Paused
                | ThreadGoalStatus::UsageLimited
                | ThreadGoalStatus::BudgetLimited => unreachable!("status validated above"),
            },
            invocation.call_id.as_str(),
            BudgetLimitedGoalDisposition::ClearActive,
        )
        .await?;
        let previous_status = self
            .current_goal_status_for_metrics(Some(expected_goal_id.as_str()))
            .await?;
        let goal = self
            .state_db
            .thread_goals()
            .update_thread_goal(
                self.thread_id,
                codex_state::GoalUpdate {
                    objective: None,
                    status: Some(state_status_from_protocol(args.status)),
                    token_budget: None,
                    expected_goal_id: Some(expected_goal_id),
                },
            )
            .await
            .map_err(|err| {
                FunctionCallError::RespondToModel(format!("failed to update goal: {err}"))
            })?
            .ok_or_else(|| {
                FunctionCallError::RespondToModel(EXTERNALLY_REPLACED_GOAL_UPDATE_ERROR.to_string())
            })?;
        self.metrics
            .record_terminal_if_status_changed(previous_status, &goal);
        self.analytics.status_changed(
            &goal,
            previous_status,
            GoalEventAttribution::Turn(invocation.turn_id.as_str()),
        );
        let goal = protocol_goal_from_state(goal);
        let turn_id = self.accounting_state.clear_current_turn_goal();
        self.emit_goal_updated_from_tool_call(&invocation, turn_id, goal.clone());
        goal_response(
            Some(goal),
            if args.status == ThreadGoalStatus::Complete {
                CompletionBudgetReport::Include
            } else {
                CompletionBudgetReport::Omit
            },
        )
    }

    fn emit_goal_updated_from_tool_call(
        &self,
        invocation: &ToolCall,
        turn_id: Option<String>,
        goal: ThreadGoal,
    ) {
        self.event_emitter
            .thread_goal_updated(invocation.call_id.clone(), turn_id, goal);
    }

    async fn account_active_goal_progress(
        &self,
        mode: codex_state::GoalAccountingMode,
        event_id: &str,
        budget_limited_goal_disposition: BudgetLimitedGoalDisposition,
    ) -> Result<Option<ThreadGoal>, FunctionCallError> {
        let Some(turn_id) = self.accounting_state.current_turn_id() else {
            return Ok(None);
        };
        let _accounting_permit = self
            .accounting_state
            .progress_accounting_permit()
            .await
            .map_err(|err| {
                FunctionCallError::Fatal(format!(
                    "goal progress accounting semaphore closed: {err}"
                ))
            })?;
        let Some(snapshot) = self.accounting_state.progress_snapshot(turn_id.as_str()) else {
            return Ok(None);
        };
        let previous_status = self
            .current_goal_status_for_metrics(Some(snapshot.expected_goal_id.as_str()))
            .await?;
        let outcome = self
            .state_db
            .thread_goals()
            .account_thread_goal_usage(
                self.thread_id,
                snapshot.time_delta_seconds,
                snapshot.token_delta,
                mode,
                Some(snapshot.expected_goal_id.as_str()),
            )
            .await
            .map_err(|err| {
                FunctionCallError::RespondToModel(format!("failed to account goal progress: {err}"))
            })?;
        Ok(match outcome {
            codex_state::GoalAccountingOutcome::Updated(goal) => {
                self.metrics
                    .record_terminal_if_status_changed(previous_status, &goal);
                self.analytics
                    .usage_accounted(&goal, GoalEventAttribution::Turn(turn_id.as_str()));
                self.analytics.status_changed(
                    &goal,
                    previous_status,
                    GoalEventAttribution::Turn(turn_id.as_str()),
                );
                self.accounting_state.mark_progress_accounted_for_status(
                    turn_id.as_str(),
                    &snapshot,
                    goal.status,
                    budget_limited_goal_disposition,
                );
                let goal = protocol_goal_from_state(goal);
                self.event_emitter.thread_goal_updated(
                    event_id.to_string(),
                    Some(turn_id),
                    goal.clone(),
                );
                Some(goal)
            }
            codex_state::GoalAccountingOutcome::Unchanged(_) => None,
        })
    }

    async fn current_goal_status_for_metrics(
        &self,
        expected_goal_id: Option<&str>,
    ) -> Result<Option<codex_state::ThreadGoalStatus>, FunctionCallError> {
        let goal = self
            .state_db
            .thread_goals()
            .get_thread_goal(self.thread_id)
            .await
            .map_err(|err| {
                FunctionCallError::RespondToModel(format!(
                    "failed to read goal metrics status: {err}"
                ))
            })?;
        Ok(goal.and_then(|goal| {
            expected_goal_id
                .is_none_or(|expected_goal_id| goal.goal_id == expected_goal_id)
                .then_some(goal.status)
        }))
    }
}

fn validate_blocked_goal_receipt(receipt: &BlockedGoalReceiptArgs) -> Result<(), String> {
    validate_fingerprint("blocker_fingerprint", &receipt.blocker_fingerprint)?;
    validate_fingerprint("evidence_fingerprint", &receipt.evidence_fingerprint)?;
    validate_receipt_text("summary", &receipt.summary, 12, 512)?;
    validate_receipt_text("blocked_on", &receipt.blocked_on, 3, 256)?;
    validate_receipt_text("evidence_summary", &receipt.evidence_summary, 8, 512)?;
    validate_receipt_text("retry_condition", &receipt.retry_condition, 8, 512)?;
    validate_receipt_list("affected_resources", &receipt.affected_resources, 1, 8, 256)?;
    validate_receipt_list("attempted_actions", &receipt.attempted_actions, 1, 8, 512)?;
    if !receipt.remaining_independent_work.is_empty() {
        return Err(
            "cannot mark this goal blocked while remaining_independent_work is non-empty; complete that authorized work first"
                .to_string(),
        );
    }
    Ok(())
}

fn validate_fingerprint(field: &str, value: &str) -> Result<(), String> {
    let value = value.trim();
    if !(8..=160).contains(&value.len()) {
        return Err(format!(
            "blocked_receipt.{field} must contain 8-160 characters"
        ));
    }
    if !value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b':' | b'.'))
    {
        return Err(format!(
            "blocked_receipt.{field} may contain only ASCII letters, digits, hyphen, underscore, colon, and period"
        ));
    }
    Ok(())
}

fn validate_receipt_text(
    field: &str,
    value: &str,
    min_len: usize,
    max_len: usize,
) -> Result<(), String> {
    let value = value.trim();
    if !(min_len..=max_len).contains(&value.len()) {
        return Err(format!(
            "blocked_receipt.{field} must contain {min_len}-{max_len} characters"
        ));
    }
    Ok(())
}

fn validate_receipt_list(
    field: &str,
    values: &[String],
    min_items: usize,
    max_items: usize,
    max_item_len: usize,
) -> Result<(), String> {
    if !(min_items..=max_items).contains(&values.len()) {
        return Err(format!(
            "blocked_receipt.{field} must contain {min_items}-{max_items} items"
        ));
    }
    for value in values {
        if value.trim().is_empty() || value.trim().len() > max_item_len {
            return Err(format!(
                "each blocked_receipt.{field} item must contain 1-{max_item_len} characters"
            ));
        }
    }
    Ok(())
}

fn parse_arguments<T>(arguments: &str) -> Result<T, FunctionCallError>
where
    T: for<'de> Deserialize<'de>,
{
    serde_json::from_str(arguments)
        .map_err(|err| FunctionCallError::RespondToModel(err.to_string()))
}

pub(crate) fn validate_goal_budget(value: Option<i64>) -> Result<(), String> {
    if let Some(value) = value
        && value <= 0
    {
        return Err("goal budgets must be positive when provided".to_string());
    }
    Ok(())
}

fn goal_response(
    goal: Option<ThreadGoal>,
    completion_budget_report: CompletionBudgetReport,
) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
    let value = serde_json::to_value(GoalToolResponse::new(goal, completion_budget_report))
        .map_err(|err| FunctionCallError::Fatal(err.to_string()))?;
    Ok(Box::new(JsonToolOutput::new(value)))
}

impl GoalToolResponse {
    fn new(goal: Option<ThreadGoal>, report_mode: CompletionBudgetReport) -> Self {
        let remaining_tokens = goal.as_ref().and_then(|goal| {
            goal.token_budget
                .map(|budget| (budget - goal.tokens_used).max(0))
        });
        let completion_budget_report = match report_mode {
            CompletionBudgetReport::Include => goal
                .as_ref()
                .filter(|goal| goal.status == ThreadGoalStatus::Complete)
                .and_then(completion_budget_report),
            CompletionBudgetReport::Omit => None,
        };
        Self {
            goal,
            remaining_tokens,
            completion_budget_report,
        }
    }
}

pub(crate) async fn fill_empty_thread_preview_if_possible(
    state_db: &codex_state::StateRuntime,
    thread_id: ThreadId,
    goal: &codex_state::ThreadGoal,
) {
    if let Err(err) = state_db
        .set_thread_preview_if_empty(thread_id, goal.objective.as_str())
        .await
    {
        tracing::warn!(
            "failed to set empty thread preview from goal objective for {thread_id}: {err}"
        );
    }
}

pub(crate) fn protocol_goal_from_state(goal: codex_state::ThreadGoal) -> ThreadGoal {
    ThreadGoal {
        thread_id: goal.thread_id,
        objective: goal.objective,
        status: protocol_status_from_state(goal.status),
        token_budget: goal.token_budget,
        tokens_used: goal.tokens_used,
        time_used_seconds: goal.time_used_seconds,
        created_at: goal.created_at.timestamp(),
        updated_at: goal.updated_at.timestamp(),
    }
}

fn protocol_status_from_state(status: codex_state::ThreadGoalStatus) -> ThreadGoalStatus {
    match status {
        codex_state::ThreadGoalStatus::Active => ThreadGoalStatus::Active,
        codex_state::ThreadGoalStatus::Paused => ThreadGoalStatus::Paused,
        codex_state::ThreadGoalStatus::Blocked => ThreadGoalStatus::Blocked,
        codex_state::ThreadGoalStatus::UsageLimited => ThreadGoalStatus::UsageLimited,
        codex_state::ThreadGoalStatus::BudgetLimited => ThreadGoalStatus::BudgetLimited,
        codex_state::ThreadGoalStatus::Complete => ThreadGoalStatus::Complete,
    }
}

pub(crate) fn state_status_from_protocol(
    status: ThreadGoalStatus,
) -> codex_state::ThreadGoalStatus {
    match status {
        ThreadGoalStatus::Active => codex_state::ThreadGoalStatus::Active,
        ThreadGoalStatus::Paused => codex_state::ThreadGoalStatus::Paused,
        ThreadGoalStatus::Blocked => codex_state::ThreadGoalStatus::Blocked,
        ThreadGoalStatus::UsageLimited => codex_state::ThreadGoalStatus::UsageLimited,
        ThreadGoalStatus::BudgetLimited => codex_state::ThreadGoalStatus::BudgetLimited,
        ThreadGoalStatus::Complete => codex_state::ThreadGoalStatus::Complete,
    }
}

fn completion_budget_report(goal: &ThreadGoal) -> Option<String> {
    if goal.token_budget.is_none() && goal.time_used_seconds <= 0 {
        None
    } else {
        Some(
            "Goal achieved. Report final usage from this tool result's structured goal fields. If `goal.tokenBudget` is present, include token usage from `goal.tokensUsed` and `goal.tokenBudget`. If `goal.timeUsedSeconds` is greater than 0, summarize elapsed time in a concise, human-friendly form appropriate to the response language."
                .to_string(),
        )
    }
}
