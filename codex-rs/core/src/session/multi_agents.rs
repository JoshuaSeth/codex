use crate::config::MultiAgentV2Config;
use crate::session::turn_context::TurnContext;
#[cfg(test)]
use codex_protocol::ThreadId;
use codex_protocol::config_types::MultiAgentMode;
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
    configured_usage_hint_text_for_source(multi_agent_v2, session_source)
}

fn configured_usage_hint_text_for_source<'a>(
    multi_agent_v2: &'a MultiAgentV2Config,
    session_source: &SessionSource,
) -> Option<&'a str> {
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

pub(crate) fn effective_multi_agent_mode(turn_context: &TurnContext) -> Option<MultiAgentMode> {
    let reasoning_effort = turn_context.effective_reasoning_effort();
    effective_multi_agent_mode_for(
        turn_context.multi_agent_version,
        &turn_context.session_source,
        reasoning_effort.as_ref(),
    )
}

fn effective_multi_agent_mode_for(
    multi_agent_version: MultiAgentVersion,
    session_source: &SessionSource,
    reasoning_effort: Option<&ReasoningEffort>,
) -> Option<MultiAgentMode> {
    if multi_agent_version != MultiAgentVersion::V2 {
        return None;
    }

    match session_source {
        // Ultra is a root-only orchestration mode. A spawned child always
        // receives explicit-request-only instructions, even when its inherited
        // reasoning effort remains Ultra.
        SessionSource::SubAgent(SubAgentSource::ThreadSpawn { .. }) => {
            Some(MultiAgentMode::ExplicitRequestOnly)
        }
        SessionSource::Cli
        | SessionSource::VSCode
        | SessionSource::Exec
        | SessionSource::Mcp
        | SessionSource::Custom(_)
        | SessionSource::Unknown => Some(if reasoning_effort == Some(&ReasoningEffort::Ultra) {
            MultiAgentMode::Proactive
        } else {
            MultiAgentMode::ExplicitRequestOnly
        }),
        SessionSource::Internal(_) | SessionSource::SubAgent(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ultra_enables_proactive_root_behavior() {
        assert_eq!(
            effective_multi_agent_mode_for(
                MultiAgentVersion::V2,
                &SessionSource::Exec,
                Some(&ReasoningEffort::Ultra),
            ),
            Some(MultiAgentMode::Proactive)
        );
    }

    #[test]
    fn non_ultra_roots_are_explicit_request_only() {
        assert_eq!(
            effective_multi_agent_mode_for(
                MultiAgentVersion::V2,
                &SessionSource::Exec,
                Some(&ReasoningEffort::Max),
            ),
            Some(MultiAgentMode::ExplicitRequestOnly)
        );
    }

    #[test]
    fn thread_spawn_children_are_always_explicit_request_only() {
        let child_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id: ThreadId::new(),
            depth: 1,
            agent_path: None,
            agent_nickname: None,
            agent_role: None,
        });

        assert_eq!(
            effective_multi_agent_mode_for(
                MultiAgentVersion::V2,
                &child_source,
                Some(&ReasoningEffort::Ultra),
            ),
            Some(MultiAgentMode::ExplicitRequestOnly)
        );
    }

    #[test]
    fn unsupported_versions_and_sources_receive_no_mode() {
        assert_eq!(
            effective_multi_agent_mode_for(
                MultiAgentVersion::V1,
                &SessionSource::Exec,
                Some(&ReasoningEffort::Ultra),
            ),
            None
        );
        assert_eq!(
            effective_multi_agent_mode_for(
                MultiAgentVersion::V2,
                &SessionSource::Internal(
                    codex_protocol::protocol::InternalSessionSource::MemoryConsolidation,
                ),
                Some(&ReasoningEffort::Ultra),
            ),
            None
        );
    }
}
