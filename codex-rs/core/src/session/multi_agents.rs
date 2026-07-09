use crate::context::MultiAgentMode;
use crate::session::turn_context::TurnContext;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;

pub(super) fn usage_hint_text<'a>(
    turn_context: &'a TurnContext,
    session_source: &SessionSource,
) -> Option<&'a str> {
    if turn_context.multi_agent_version != MultiAgentVersion::V2 {
        return None;
    }

    let multi_agent_v2 = &turn_context.config.multi_agent_v2;
    if !multi_agent_v2.usage_hint_enabled {
        return None;
    }

    match session_source {
        SessionSource::SubAgent(SubAgentSource::ThreadSpawn { .. }) => {
            multi_agent_v2.subagent_usage_hint_text.as_deref()
        }
        SessionSource::Cli
        | SessionSource::VSCode
        | SessionSource::Exec
        | SessionSource::Mcp
        | SessionSource::Custom(_)
        | SessionSource::Unknown => multi_agent_v2.root_agent_usage_hint_text.as_deref(),
        SessionSource::Internal(_) | SessionSource::SubAgent(_) => None,
    }
}

pub(crate) fn mode_for_reasoning_effort(
    multi_agent_version: MultiAgentVersion,
    reasoning_effort: Option<&ReasoningEffort>,
) -> Option<MultiAgentMode> {
    if multi_agent_version != MultiAgentVersion::V2 {
        return None;
    }

    let multi_agent_mode = match reasoning_effort {
        Some(ReasoningEffort::Ultra) => MultiAgentMode::Proactive,
        _ => MultiAgentMode::ExplicitRequestOnly,
    };
    Some(multi_agent_mode)
}

pub(crate) fn effective_multi_agent_mode(turn_context: &TurnContext) -> Option<MultiAgentMode> {
    let multi_agent_mode = mode_for_reasoning_effort(
        turn_context.multi_agent_version,
        turn_context.effective_reasoning_effort().as_ref(),
    );

    match &turn_context.session_source {
        SessionSource::SubAgent(SubAgentSource::ThreadSpawn { .. })
        | SessionSource::Cli
        | SessionSource::VSCode
        | SessionSource::Exec
        | SessionSource::Mcp
        | SessionSource::Custom(_)
        | SessionSource::Unknown => multi_agent_mode,
        SessionSource::Internal(_) | SessionSource::SubAgent(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ultra_enables_proactive_multi_agent_behavior() {
        let mode = mode_for_reasoning_effort(MultiAgentVersion::V2, Some(&ReasoningEffort::Ultra));

        assert_eq!(mode, Some(MultiAgentMode::Proactive));
    }

    #[test]
    fn max_keeps_explicit_request_only_behavior() {
        let mode = mode_for_reasoning_effort(MultiAgentVersion::V2, Some(&ReasoningEffort::Max));

        assert_eq!(mode, Some(MultiAgentMode::ExplicitRequestOnly));
    }
}
