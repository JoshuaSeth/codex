use super::Session;
use super::TurnContext;
use crate::build_available_skills;
use crate::context::AvailableSkillsInstructions;
use crate::context::ContextualUserFragment;
use crate::context_manager::remove_matching_skills_instructions;
use crate::context_manager::updates::build_developer_update_item;
use crate::default_skill_metadata_budget;
use crate::skills::SkillRenderSideEffects;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::WarningEvent;

const EMPTY_MANAGED_SKILLS_WARNING: &str =
    "Managed PitchAI skill context has no available skills; historical skill access was revoked.";
const DISABLED_MANAGED_SKILLS_WARNING: &str =
    "Managed PitchAI skill instructions are disabled; historical skill access was revoked.";

impl Session {
    /// Append the authoritative managed skill catalog only when it differs from the latest
    /// catalog stored in the thread. Prompt projection removes older catalog fragments without
    /// rewriting the append-only rollout.
    pub(crate) async fn sync_managed_skills_context(&self, turn_context: &TurnContext) {
        if self.pitchai_skill_principal().await.is_none() {
            return;
        }

        let (current, warning) = if turn_context.config.include_skill_instructions {
            match build_available_skills(
                &turn_context.turn_skills.outcome,
                default_skill_metadata_budget(turn_context.model_info.context_window),
                SkillRenderSideEffects::None,
            ) {
                Some(available_skills) => {
                    let warning = available_skills.warning_message.clone();
                    (
                        AvailableSkillsInstructions::from(available_skills).render(),
                        warning,
                    )
                }
                None => (
                    AvailableSkillsInstructions::from_skill_lines(Vec::new()).render(),
                    Some(EMPTY_MANAGED_SKILLS_WARNING.to_string()),
                ),
            }
        } else {
            (
                AvailableSkillsInstructions::from_skill_lines(Vec::new()).render(),
                Some(DISABLED_MANAGED_SKILLS_WARNING.to_string()),
            )
        };

        let (latest, has_reference_context) = {
            let state = self.state.lock().await;
            let latest = state
                .history
                .latest_skills_instructions()
                .map(str::to_owned);
            (latest, state.reference_context_item().is_some())
        };
        if latest.as_deref() == Some(current.as_str()) {
            return;
        }
        // A new thread with no reference baseline will record its full initial context after the
        // pre-compaction check. Keep that normal layout rather than pre-seeding a separate catalog.
        if latest.is_none() && !has_reference_context {
            return;
        }

        if let Some(message) = warning {
            self.send_event_raw(Event {
                id: String::new(),
                msg: EventMsg::Warning(WarningEvent { message }),
            })
            .await;
        }
        let current_item = build_developer_update_item(vec![current])
            .expect("a rendered managed skills context is non-empty");
        self.record_conversation_items(turn_context, std::slice::from_ref(&current_item))
            .await;
    }

    /// Full-context reinjection can follow a pre-compaction catalog refresh when a resumed
    /// rollout has no usable TurnContext baseline. Remove only an identical catalog fragment
    /// from the new bundle so the refreshed append-only item is not recorded twice.
    pub(crate) async fn remove_duplicate_managed_skills_context(
        &self,
        context_items: &mut Vec<codex_protocol::models::ResponseItem>,
    ) {
        if self.pitchai_skill_principal().await.is_none() {
            return;
        }
        let latest = {
            let state = self.state.lock().await;
            state
                .history
                .latest_skills_instructions()
                .map(str::to_owned)
        };
        if let Some(latest) = latest {
            remove_matching_skills_instructions(context_items, &latest);
        }
    }
}
