use super::JSONRPCErrorError;
use super::invalid_request;
use codex_config::PitchAiSkillPrincipal;
use codex_core::CodexThread;
use codex_protocol::ThreadId;
use codex_protocol::protocol::RolloutItem;
use serde_json::Value;
use std::collections::HashMap;

const CATALOG_RELEASE_ENV: &str = "PITCHAI_SKILL_CATALOG_RELEASE";

pub(super) fn managed_pitchai_catalog_enabled() -> bool {
    std::env::var_os(CATALOG_RELEASE_ENV).is_some()
}

pub(super) fn required_pitchai_principal_from_config(
    config: Option<&HashMap<String, Value>>,
) -> Result<Option<PitchAiSkillPrincipal>, JSONRPCErrorError> {
    let principal = pitchai_principal_from_config(config)?;
    match (managed_pitchai_catalog_enabled(), principal) {
        (false, None) => Ok(None),
        (false, Some(_)) => Err(invalid_request(format!(
            "PitchAI skill principal was supplied without {CATALOG_RELEASE_ENV}."
        ))),
        (true, None) => Err(invalid_request(
            "Managed PitchAI skill resolution requires an authoritative tenant/user principal.",
        )),
        (true, Some(principal)) => Ok(Some(principal)),
    }
}

pub(super) async fn require_matching_pitchai_principal(
    thread: &CodexThread,
    principal: Option<PitchAiSkillPrincipal>,
) -> Result<(), JSONRPCErrorError> {
    if let Some(principal) = principal {
        thread
            .require_pitchai_skill_principal(principal)
            .await
            .map_err(|error| invalid_request(error.to_string()))?;
    }
    Ok(())
}

pub(super) async fn require_bound_pitchai_principal(
    thread: &CodexThread,
) -> Result<(), JSONRPCErrorError> {
    if managed_pitchai_catalog_enabled() && thread.pitchai_skill_principal().await.is_none() {
        return Err(invalid_request(
            "Managed PitchAI thread is not bound to an authoritative tenant/user principal; resume it with canonical managed config before starting work.",
        ));
    }
    Ok(())
}

pub(super) fn validate_requested_principal_against_rollout(
    items: &[RolloutItem],
    thread_id: ThreadId,
    requested: Option<&PitchAiSkillPrincipal>,
    require_persisted: bool,
) -> Result<(), JSONRPCErrorError> {
    let persisted =
        codex_protocol::protocol::pitchai_skill_principal_from_rollout_items(items, thread_id)
            .map_err(invalid_request)?;
    if let Some(persisted) = persisted.as_ref() {
        codex_protocol::protocol::validate_pitchai_skill_principal(persisted)
            .map_err(invalid_request)?;
        if requested.is_some_and(|requested| requested != persisted) {
            return Err(invalid_request(
                "PitchAI skill principal does not match the identity already bound to this thread.",
            ));
        }
    } else if require_persisted && managed_pitchai_catalog_enabled() {
        return Err(invalid_request(
            "Legacy managed source thread must be resumed once with its canonical principal before it can be forked.",
        ));
    }
    Ok(())
}

fn pitchai_principal_from_config(
    config: Option<&HashMap<String, Value>>,
) -> Result<Option<PitchAiSkillPrincipal>, JSONRPCErrorError> {
    let Some(skills) = config.and_then(|config| config.get("skills")) else {
        return Ok(None);
    };
    let skills = skills.as_object().ok_or_else(|| {
        invalid_request("Managed skills config must be an object containing pitchai_principal.")
    })?;
    let Some(principal) = skills.get("pitchai_principal") else {
        return Ok(None);
    };
    let principal: PitchAiSkillPrincipal = serde_json::from_value(principal.clone()).map_err(|_| {
        invalid_request(
            "Managed PitchAI skill principal is malformed or uses an unsupported schema version.",
        )
    })?;
    if codex_protocol::protocol::validate_pitchai_skill_principal(&principal).is_err() {
        return Err(invalid_request(
            "Managed PitchAI skill principal is malformed or uses an unsupported schema version.",
        ));
    }
    Ok(Some(principal))
}
