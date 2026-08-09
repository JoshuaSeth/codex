use codex_config::ConfigLayerStack;
use codex_config::PitchAiSkillPrincipal;
use codex_config::SkillsConfig;
use codex_utils_absolute_path::AbsolutePathBuf;
use std::path::Path;

const CATALOG_RELEASE_ENV: &str = "PITCHAI_SKILL_CATALOG_RELEASE";
const CATALOG_MARKER: &str = ".pitchai-principal-catalog-v1";
const CATALOG_MARKER_TEXT: &str = "pitchai-codex-home-skills/principal-v1";
const PRINCIPAL_SCHEMA_VERSION: u8 = 1;

pub(crate) fn managed_pitchai_catalog_enabled() -> bool {
    std::env::var_os(CATALOG_RELEASE_ENV).is_some()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PitchAiSkillCatalogProfile {
    pub catalog_root: AbsolutePathBuf,
    pub system_root: AbsolutePathBuf,
    pub tenant_root: AbsolutePathBuf,
    pub user_root: AbsolutePathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PitchAiSkillResolution {
    Legacy,
    Managed(PitchAiSkillCatalogProfile),
    Invalid {
        path: AbsolutePathBuf,
        message: String,
    },
}

pub(crate) fn resolve_pitchai_skill_profile(
    config_layer_stack: &ConfigLayerStack,
    explicit_principal: Option<&PitchAiSkillPrincipal>,
    cwd: &AbsolutePathBuf,
) -> PitchAiSkillResolution {
    let configured_principal = match principal_from_stack(config_layer_stack) {
        Ok(principal) => principal,
        Err(message) => {
            return PitchAiSkillResolution::Invalid {
                path: cwd.clone(),
                message,
            };
        }
    };
    if let (Some(explicit), Some(configured)) = (explicit_principal, configured_principal.as_ref())
        && explicit != configured
    {
        return PitchAiSkillResolution::Invalid {
            path: cwd.clone(),
            message: "PitchAI skill principal conflicts with the effective thread config."
                .to_string(),
        };
    }
    let principal = explicit_principal.or(configured_principal.as_ref());
    let catalog_release = std::env::var_os(CATALOG_RELEASE_ENV);
    match (catalog_release, principal) {
        (None, None) => PitchAiSkillResolution::Legacy,
        (None, Some(_)) => PitchAiSkillResolution::Invalid {
            path: cwd.clone(),
            message: format!("PitchAI skill principal was supplied without {CATALOG_RELEASE_ENV}."),
        },
        (Some(_raw_release), None) => PitchAiSkillResolution::Invalid {
            path: cwd.clone(),
            message:
                "Managed PitchAI skill resolution requires an authoritative tenant/user principal."
                    .to_string(),
        },
        (Some(raw_release), Some(principal)) => {
            profile_from_release(Path::new(&raw_release), principal, cwd)
        }
    }
}

fn principal_from_stack(
    config_layer_stack: &ConfigLayerStack,
) -> Result<Option<PitchAiSkillPrincipal>, String> {
    let effective_config = config_layer_stack.effective_config();
    let Some(skills_value) = effective_config
        .as_table()
        .and_then(|table| table.get("skills"))
    else {
        return Ok(None);
    };
    let skills: SkillsConfig = skills_value
        .clone()
        .try_into()
        .map_err(|error| format!("Invalid managed skills config: {error}"))?;
    Ok(skills.pitchai_principal)
}

fn profile_from_release(
    release_path: &Path,
    principal: &PitchAiSkillPrincipal,
    error_path: &AbsolutePathBuf,
) -> PitchAiSkillResolution {
    if principal.schema_version != PRINCIPAL_SCHEMA_VERSION
        || !is_canonical_uuid(&principal.tenant_id)
        || !is_canonical_uuid(&principal.user_id)
    {
        return PitchAiSkillResolution::Invalid {
            path: error_path.clone(),
            message: "Managed PitchAI skill principal is malformed or uses an unsupported schema version."
                .to_string(),
        };
    }
    if !release_path.is_absolute() {
        return PitchAiSkillResolution::Invalid {
            path: error_path.clone(),
            message: format!("{CATALOG_RELEASE_ENV} must name an absolute immutable release."),
        };
    }
    let canonical_release = match dunce::canonicalize(release_path)
        .ok()
        .and_then(|path| AbsolutePathBuf::from_absolute_path_checked(path).ok())
    {
        Some(path) => path,
        None => {
            return PitchAiSkillResolution::Invalid {
                path: error_path.clone(),
                message: format!("{CATALOG_RELEASE_ENV} does not resolve to a readable release."),
            };
        }
    };
    let marker = canonical_release.join(CATALOG_MARKER);
    if std::fs::read_to_string(&marker)
        .ok()
        .map(|value| value.trim().to_string())
        .as_deref()
        != Some(CATALOG_MARKER_TEXT)
    {
        return PitchAiSkillResolution::Invalid {
            path: marker,
            message: "Managed PitchAI skill catalog marker is missing or invalid.".to_string(),
        };
    }
    let system_root = canonical_release.join(".system");
    let tenant_root = canonical_release
        .join("profiles")
        .join("tenants")
        .join(&principal.tenant_id)
        .join("skills");
    let user_root = canonical_release
        .join("profiles")
        .join("users")
        .join(&principal.user_id)
        .join("skills");
    for (category, path) in [
        ("system", &system_root),
        ("tenant", &tenant_root),
        ("user", &user_root),
    ] {
        if !path.is_dir() {
            return PitchAiSkillResolution::Invalid {
                path: path.clone(),
                message: format!("Managed PitchAI {category} skill profile is missing."),
            };
        }
    }
    PitchAiSkillResolution::Managed(PitchAiSkillCatalogProfile {
        catalog_root: canonical_release,
        system_root,
        tenant_root,
        user_root,
    })
}

fn is_canonical_uuid(value: &str) -> bool {
    if value.len() != 36 {
        return false;
    }
    value.bytes().enumerate().all(|(index, byte)| match index {
        8 | 13 | 18 | 23 => byte == b'-',
        _ => byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const TENANT_ID: &str = "9bc52e7e-79df-5a9b-a4c7-d4eb29d24f12";
    const USER_ID: &str = "baef2f0b-181b-571d-b66b-7a52d79eb963";

    fn error_path() -> AbsolutePathBuf {
        AbsolutePathBuf::from_absolute_path(std::env::temp_dir()).expect("absolute temp path")
    }

    fn valid_principal() -> PitchAiSkillPrincipal {
        PitchAiSkillPrincipal {
            schema_version: PRINCIPAL_SCHEMA_VERSION,
            tenant_id: TENANT_ID.to_string(),
            user_id: USER_ID.to_string(),
        }
    }

    #[test]
    fn resolves_only_complete_immutable_principal_profiles() {
        let release = tempfile::tempdir().expect("catalog tempdir");
        std::fs::write(
            release.path().join(CATALOG_MARKER),
            format!("{CATALOG_MARKER_TEXT}\n"),
        )
        .expect("write marker");
        for path in [
            release.path().join(".system"),
            release
                .path()
                .join("profiles/tenants")
                .join(TENANT_ID)
                .join("skills"),
            release
                .path()
                .join("profiles/users")
                .join(USER_ID)
                .join("skills"),
        ] {
            std::fs::create_dir_all(path).expect("create profile");
        }

        let resolution = profile_from_release(release.path(), &valid_principal(), &error_path());
        let PitchAiSkillResolution::Managed(profile) = resolution else {
            panic!("expected managed profile: {resolution:?}");
        };
        assert_eq!(
            profile.tenant_root,
            AbsolutePathBuf::from_absolute_path(
                release
                    .path()
                    .join("profiles/tenants")
                    .join(TENANT_ID)
                    .join("skills")
            )
            .expect("absolute tenant path")
        );
        assert_eq!(
            profile.user_root,
            AbsolutePathBuf::from_absolute_path(
                release
                    .path()
                    .join("profiles/users")
                    .join(USER_ID)
                    .join("skills")
            )
            .expect("absolute user path")
        );
    }

    #[test]
    fn rejects_malformed_principal_without_echoing_identity_values() {
        let release = tempfile::tempdir().expect("catalog tempdir");
        let secret_like_value = "not-a-uuid-secret-identity-value";
        let principal = PitchAiSkillPrincipal {
            schema_version: PRINCIPAL_SCHEMA_VERSION,
            tenant_id: TENANT_ID.to_string(),
            user_id: secret_like_value.to_string(),
        };

        let resolution = profile_from_release(release.path(), &principal, &error_path());
        let PitchAiSkillResolution::Invalid { message, .. } = resolution else {
            panic!("expected invalid principal: {resolution:?}");
        };
        assert!(!message.contains(secret_like_value));
        assert!(message.contains("malformed"));
    }
}
