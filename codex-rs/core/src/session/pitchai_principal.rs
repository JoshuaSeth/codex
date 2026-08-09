use crate::config::Config;
use crate::session_rollout_init_error::InvalidSessionIdentityError;
use codex_config::PitchAiSkillPrincipal;
use codex_protocol::protocol::InitialHistory;
use codex_protocol::protocol::RolloutItem;

/// Resolve the immutable managed principal before any session skill warmup.
///
/// New and already-bound threads return their effective principal. A legacy
/// local resume binds once by replacing canonical rollout metadata before the
/// live writer opens. Forks from unbound legacy metadata fail closed so a
/// caller cannot choose the source thread's identity while creating a fork.
pub(super) async fn resolve_and_bind_pitchai_principal(
    config: &Config,
    initial_history: &mut InitialHistory,
) -> anyhow::Result<Option<PitchAiSkillPrincipal>> {
    let configured =
        codex_core_skills::pitchai_skill_principal_from_stack(&config.config_layer_stack)
            .map_err(InvalidSessionIdentityError)?;
    if let Some(principal) = configured.as_ref() {
        validate_principal(principal)?;
    }

    let persisted = pitchai_principal_from_initial_history(initial_history)
        .map_err(|message| InvalidSessionIdentityError(message.to_string()))?;
    if let Some(principal) = persisted.as_ref() {
        validate_principal(principal)?;
    }
    if persisted.is_some() && configured.is_none() {
        return Err(InvalidSessionIdentityError(
            "Resuming an identity-bound PitchAI thread requires an authoritative tenant/user principal."
                .to_string(),
        )
        .into());
    }
    if let (Some(configured), Some(persisted)) = (configured.as_ref(), persisted.as_ref())
        && configured != persisted
    {
        return Err(InvalidSessionIdentityError(
            "PitchAI skill principal does not match the identity already bound to this thread."
                .to_string(),
        )
        .into());
    }
    validate_catalog_principal_presence(
        codex_core_skills::managed_pitchai_catalog_enabled(),
        configured.as_ref(),
    )?;
    if matches!(&*initial_history, InitialHistory::Forked(items) if items.iter().any(|item| matches!(item, RolloutItem::SessionMeta(_))))
        && configured.is_some()
        && persisted.is_none()
    {
        return Err(InvalidSessionIdentityError(
            "Legacy managed source thread must be resumed once with its canonical principal before it can be forked."
                .to_string(),
        )
        .into());
    }

    let legacy_principal_needs_binding = persisted.is_none();
    let effective = persisted.or(configured);
    if legacy_principal_needs_binding
        && let (InitialHistory::Resumed(resumed), Some(principal)) =
            (&mut *initial_history, effective.as_ref())
    {
        let rollout_path = resumed.rollout_path.as_ref().ok_or_else(|| {
            InvalidSessionIdentityError(
                "Legacy managed thread identity cannot be persisted without canonical local thread storage."
                    .to_string(),
            )
        })?;
        let materialized_path = codex_rollout::bind_pitchai_principal_to_rollout_path(
            rollout_path,
            resumed.conversation_id,
            principal.clone(),
        )
        .await?;
        resumed.rollout_path = Some(materialized_path);
        let conversation_id = resumed.conversation_id;
        let Some(meta_line) = resumed.history.iter_mut().find_map(|item| match item {
            RolloutItem::SessionMeta(meta_line) if meta_line.meta.id == conversation_id => {
                Some(meta_line)
            }
            _ => None,
        }) else {
            return Err(InvalidSessionIdentityError(
                "Legacy managed thread does not contain canonical identity metadata.".to_string(),
            )
            .into());
        };
        meta_line.meta.pitchai_principal = Some(principal.clone());
    }
    Ok(effective)
}

fn pitchai_principal_from_initial_history(
    initial_history: &InitialHistory,
) -> Result<Option<PitchAiSkillPrincipal>, &'static str> {
    match initial_history {
        InitialHistory::New | InitialHistory::Cleared => Ok(None),
        InitialHistory::Resumed(resumed) => {
            codex_protocol::protocol::pitchai_skill_principal_from_rollout_items(
                resumed.history.as_slice(),
                resumed.conversation_id,
            )
        }
        InitialHistory::Forked(items) => {
            let Some(source_thread_id) = items.iter().find_map(|item| match item {
                RolloutItem::SessionMeta(meta_line) => Some(meta_line.meta.id),
                _ => None,
            }) else {
                return Ok(None);
            };
            codex_protocol::protocol::pitchai_skill_principal_from_rollout_items(
                items.as_slice(),
                source_thread_id,
            )
        }
    }
}

fn validate_principal(principal: &PitchAiSkillPrincipal) -> anyhow::Result<()> {
    codex_protocol::protocol::validate_pitchai_skill_principal(principal)
        .map_err(|message| anyhow::Error::new(InvalidSessionIdentityError(message.to_string())))
}

fn validate_catalog_principal_presence(
    managed_catalog_enabled: bool,
    configured: Option<&PitchAiSkillPrincipal>,
) -> anyhow::Result<()> {
    match (managed_catalog_enabled, configured.is_some()) {
        (false, false) | (true, true) => Ok(()),
        (false, true) => Err(InvalidSessionIdentityError(
            "PitchAI skill principal was supplied without PITCHAI_SKILL_CATALOG_RELEASE."
                .to_string(),
        )
        .into()),
        (true, false) => Err(InvalidSessionIdentityError(
            "Managed PitchAI skill resolution requires an authoritative tenant/user principal."
                .to_string(),
        )
        .into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn principal() -> PitchAiSkillPrincipal {
        PitchAiSkillPrincipal {
            schema_version: 1,
            tenant_id: "9bc52e7e-79df-5a9b-a4c7-d4eb29d24f12".to_string(),
            user_id: "baef2f0b-181b-571d-b66b-7a52d79eb963".to_string(),
        }
    }

    #[test]
    fn catalog_and_configured_principal_are_one_session_contract() {
        let principal = principal();
        assert!(validate_catalog_principal_presence(false, None).is_ok());
        assert!(validate_catalog_principal_presence(true, Some(&principal)).is_ok());

        let unmanaged_error = validate_catalog_principal_presence(false, Some(&principal))
            .expect_err("principal without managed catalog must fail")
            .to_string();
        assert_eq!(
            unmanaged_error,
            "PitchAI skill principal was supplied without PITCHAI_SKILL_CATALOG_RELEASE."
        );
        let unbound_error = validate_catalog_principal_presence(true, None)
            .expect_err("managed catalog without principal must fail")
            .to_string();
        assert_eq!(
            unbound_error,
            "Managed PitchAI skill resolution requires an authoritative tenant/user principal."
        );
        for message in [unmanaged_error, unbound_error] {
            assert!(!message.contains(principal.tenant_id.as_str()));
            assert!(!message.contains(principal.user_id.as_str()));
        }
    }
}
