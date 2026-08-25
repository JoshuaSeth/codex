//! Responses API tool definitions for persisted thread goals.

use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolSpec;
use serde_json::json;
use std::collections::BTreeMap;

pub const GET_GOAL_TOOL_NAME: &str = "get_goal";
pub const CREATE_GOAL_TOOL_NAME: &str = "create_goal";
pub const UPDATE_GOAL_TOOL_NAME: &str = "update_goal";

pub fn create_get_goal_tool() -> ToolSpec {
    ToolSpec::Function(ResponsesApiTool {
        name: GET_GOAL_TOOL_NAME.to_string(),
        description: "Get the current goal for this thread, including status, budgets, token and elapsed-time usage, and remaining token budget."
            .to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(BTreeMap::new(), Some(Vec::new()), Some(false.into())),
        output_schema: None,
    })
}

pub fn create_create_goal_tool() -> ToolSpec {
    let properties = BTreeMap::from([
        (
            "objective".to_string(),
            JsonSchema::string(Some(
                "Required. The concrete objective to start pursuing. This starts a new active goal when no goal exists or replaces the current goal when it is complete."
                    .to_string(),
            )),
        ),
        (
            "token_budget".to_string(),
            JsonSchema::integer(Some(
                "Positive token budget for the new goal. Omit unless explicitly requested."
                    .to_string(),
            )),
        ),
    ]);

    ToolSpec::Function(ResponsesApiTool {
        name: CREATE_GOAL_TOOL_NAME.to_string(),
        description: format!(
            r#"Create a goal only when explicitly requested by the user or system/developer instructions; do not infer goals from ordinary tasks.
Set token_budget only when an explicit token budget is requested. Fails if an unfinished goal exists; use {UPDATE_GOAL_TOOL_NAME} only for status."#
        ),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            properties,
            /*required*/ Some(vec!["objective".to_string()]),
            Some(false.into()),
        ),
        output_schema: None,
    })
}

pub fn create_update_goal_tool() -> ToolSpec {
    let blocked_receipt_properties = BTreeMap::from([
        (
            "blocker_fingerprint".to_string(),
            JsonSchema::string(Some(
                "Stable opaque identifier for this exact external condition and scope. Reuse it only while the same blocker remains; changing it restarts the audit. Do not include secrets."
                    .to_string(),
            )),
        ),
        (
            "summary".to_string(),
            JsonSchema::string(Some(
                "Concise explanation of the specific external condition that makes all remaining goal progress impossible."
                    .to_string(),
            )),
        ),
        (
            "blocked_on".to_string(),
            JsonSchema::string(Some(
                "External actor, service, event, or immutable policy boundary that must change. Symbolic approval is not a blocker when the objective already authorizes the action."
                    .to_string(),
            )),
        ),
        (
            "affected_resources".to_string(),
            JsonSchema::array(
                JsonSchema::string(None),
                Some(
                    "One to eight concrete resources or work lanes blocked by this condition. Keep unrelated work out of scope."
                        .to_string(),
                ),
            ),
        ),
        (
            "evidence_fingerprint".to_string(),
            JsonSchema::string(Some(
                "Opaque fingerprint for fresh evidence observed in this turn, such as a tool-call/result digest or external event receipt. Reusing the previous fingerprint does not advance the audit. Do not include secrets."
                    .to_string(),
            )),
        ),
        (
            "evidence_summary".to_string(),
            JsonSchema::string(Some(
                "Short, non-secret summary of the fresh observation proving the blocker still exists."
                    .to_string(),
            )),
        ),
        (
            "attempted_actions".to_string(),
            JsonSchema::array(
                JsonSchema::string(None),
                Some(
                    "One to eight meaningful, authorized actions or safe checks performed in this turn to resolve or route around the blocker."
                        .to_string(),
                ),
            ),
        ),
        (
            "remaining_independent_work".to_string(),
            JsonSchema::array(
                JsonSchema::string(None),
                Some(
                    "Every still-actionable part of the objective not blocked by this condition. This list must be empty before blocked is valid."
                        .to_string(),
                ),
            ),
        ),
        (
            "retry_condition".to_string(),
            JsonSchema::string(Some(
                "Observable event or state change that would make another attempt useful."
                    .to_string(),
            )),
        ),
    ]);
    let blocked_receipt = JsonSchema::object(
        blocked_receipt_properties,
        Some(vec![
            "blocker_fingerprint".to_string(),
            "summary".to_string(),
            "blocked_on".to_string(),
            "affected_resources".to_string(),
            "evidence_fingerprint".to_string(),
            "evidence_summary".to_string(),
            "attempted_actions".to_string(),
            "remaining_independent_work".to_string(),
            "retry_condition".to_string(),
        ]),
        Some(false.into()),
    );
    let properties = BTreeMap::from([
        (
            "status".to_string(),
            JsonSchema::string_enum(
                vec![json!("complete"), json!("blocked")],
                Some(
                    "Required. Set to `complete` only when the objective is achieved and no required work remains. Set to `blocked` only after the same scoped external condition has passed the semantic blocked audit."
                        .to_string(),
                ),
            ),
        ),
        (
            "blocked_receipt".to_string(),
            blocked_receipt,
        ),
    ]);

    ToolSpec::Function(ResponsesApiTool {
        name: UPDATE_GOAL_TOOL_NAME.to_string(),
        description: r#"Update the existing goal.
Use this tool only to mark the goal achieved or genuinely blocked.
Set status to `complete` only when the objective has actually been achieved and no required work remains.
Set status to `blocked` only when the same scoped external condition has repeated for at least three qualifying goal turns, the agent cannot make meaningful progress without user input or an external-state change, and no authorized independent work remains.
For `blocked`, blocked_receipt is required. Each qualifying turn needs the same blocker_fingerprint, a genuinely fresh evidence_fingerprint, fresh evidence, and at least one meaningful attempted action. A changed blocker fingerprint restarts the audit; stale evidence and repeated calls in one turn do not advance it.
If the user resumes a goal that was previously marked `blocked`, treat the resumed run as a fresh blocked audit. If the same blocking condition then repeats for at least three qualifying resumed goal turns, set status to `blocked` again.
Once the blocked threshold is satisfied, do not keep reporting that you are still blocked while leaving the goal active; set status to `blocked`.
Do not use `blocked` merely because the work is hard, slow, uncertain, incomplete, or would benefit from clarification.
Do not use `blocked` for symbolic permission, deployment discomfort, a verification ritual, production fear, or stale instructions when the current objective already authorizes safe action. Continue safely: inspect current state, take reversible or scoped implementation steps, validate, and use the normal review/deployment gates.
Do not mark a goal complete merely because its budget is nearly exhausted or because you are stopping work.
You cannot use this tool to pause, resume, budget-limit, or usage-limit a goal; those status changes are controlled by the user or system.
When marking a budgeted goal achieved with status `complete`, report the final token usage from the tool result to the user."#
            .to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            properties,
            /*required*/ Some(vec!["status".to_string()]),
            Some(false.into()),
        ),
        output_schema: None,
    })
}
