use super::JSONRPCErrorError;
use super::invalid_request;
use codex_config::PitchAiSkillPrincipal;
use codex_core::CodexThread;
use serde_json::Value;
use std::collections::HashMap;
use uuid::Uuid;

const CATALOG_RELEASE_ENV: &str = "PITCHAI_SKILL_CATALOG_RELEASE";
const PRINCIPAL_SCHEMA_VERSION: u8 = 1;

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

pub(super) async fn bind_required_pitchai_principal(
    thread: &CodexThread,
    principal: Option<PitchAiSkillPrincipal>,
) -> Result<(), JSONRPCErrorError> {
    if let Some(principal) = principal {
        thread
            .bind_pitchai_skill_principal(principal)
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
    if principal.schema_version != PRINCIPAL_SCHEMA_VERSION
        || !is_canonical_uuid(&principal.tenant_id)
        || !is_canonical_uuid(&principal.user_id)
    {
        return Err(invalid_request(
            "Managed PitchAI skill principal is malformed or uses an unsupported schema version.",
        ));
    }
    Ok(Some(principal))
}

fn is_canonical_uuid(value: &str) -> bool {
    Uuid::parse_str(value).is_ok_and(|parsed| parsed.to_string() == value)
}
