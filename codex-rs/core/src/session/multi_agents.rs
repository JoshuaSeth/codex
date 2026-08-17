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

pub(crate) fn effective_multi_agent_mode(
    multi_agent_version: MultiAgentVersion,
    session_source: &SessionSource,
    selected_multi_agent_mode: Option<MultiAgentMode>,
    reasoning_effort: Option<&ReasoningEffort>,
) -> Option<MultiAgentMode> {
    if multi_agent_version != MultiAgentVersion::V2 {
        return None;
    }

    match session_source {
        // A child may inherit its parent's selected mode, but it must never
        // become another proactive manager.
        SessionSource::SubAgent(SubAgentSource::ThreadSpawn { .. }) => {
            Some(MultiAgentMode::ExplicitRequestOnly)
        }
        SessionSource::Cli
        | SessionSource::VSCode
        | SessionSource::Exec
        | SessionSource::Mcp
        | SessionSource::Custom(_)
        | SessionSource::Unknown => Some(selected_multi_agent_mode.unwrap_or_else(|| {
            if reasoning_effort == Some(&ReasoningEffort::Ultra) {
                MultiAgentMode::Proactive
            } else {
                MultiAgentMode::ExplicitRequestOnly
            }
        })),
        SessionSource::Internal(_) | SessionSource::SubAgent(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn omitted_ultra_enables_proactive_root_behavior() {
        assert_eq!(
            effective_multi_agent_mode(
                MultiAgentVersion::V2,
                &SessionSource::Exec,
                /*selected_multi_agent_mode*/ None,
                Some(&ReasoningEffort::Ultra),
            ),
            Some(MultiAgentMode::Proactive)
        );
    }

    #[test]
    fn explicit_root_modes_win_over_ultra_derivation() {
        for mode in [
            MultiAgentMode::None,
            MultiAgentMode::ExplicitRequestOnly,
            MultiAgentMode::Proactive,
        ] {
            assert_eq!(
                effective_multi_agent_mode(
                    MultiAgentVersion::V2,
                    &SessionSource::Exec,
                    Some(mode),
                    Some(&ReasoningEffort::Ultra),
                ),
                Some(mode)
            );
        }
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

        for mode in [MultiAgentMode::None, MultiAgentMode::Proactive] {
            assert_eq!(
                effective_multi_agent_mode(
                    MultiAgentVersion::V2,
                    &child_source,
                    Some(mode),
                    Some(&ReasoningEffort::Ultra),
                ),
                Some(MultiAgentMode::ExplicitRequestOnly)
            );
        }
    }

    #[test]
    fn unsupported_versions_and_sources_receive_no_mode() {
        assert_eq!(
            effective_multi_agent_mode(
                MultiAgentVersion::V1,
                &SessionSource::Exec,
                None,
                Some(&ReasoningEffort::Ultra),
            ),
            None
        );
        assert_eq!(
            effective_multi_agent_mode(
                MultiAgentVersion::V2,
                &SessionSource::Internal(
                    codex_protocol::protocol::InternalSessionSource::MemoryConsolidation,
                ),
                Some(MultiAgentMode::Proactive),
                Some(&ReasoningEffort::Ultra),
            ),
            None
        );
    }
}
