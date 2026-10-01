pub mod config_rules;
pub mod injection;
pub(crate) mod invocation_utils;
pub mod loader;
pub mod model;
mod pitchai_principal;
pub mod remote;
mod root_loader;
mod skill_instructions;

/// Hard byte limit for one model-visible skill instruction body.
///
/// Both the legacy explicit-injection path and the skills extension use this
/// limit so a skill cannot bypass context bounds by changing how it is loaded.
pub const MAX_SKILL_PROMPT_BYTES: usize = 8_000;

pub use codex_skills::ImplicitSkillLookup;
pub use codex_skills::build_skill_name_counts;
pub use codex_skills::detect_implicit_skill_invocation_for_command;
pub(crate) use invocation_utils::build_implicit_skill_path_indexes;
pub use model::SkillError;
pub use model::SkillLoadOutcome;
pub use model::SkillMetadata;
pub use model::SkillPolicy;
pub use model::filter_skill_load_outcome_for_product;
pub use pitchai_principal::managed_pitchai_catalog_enabled;
pub use pitchai_principal::pitchai_skill_principal_from_stack;

// The PitchAI seam the skills extension reaches. Ext/skills owns the host
// skills service from 267 on, but the managed-catalog resolution and the
// tenant name precedence stay in this crate; these three names are the
// contract between them.
pub use loader::SkillRootsResolution as PitchaiSkillRootsResolution;
pub use loader::load_skills_from_roots_with_name_precedence_and_pool as pitchai_load_skills_from_roots_with_name_precedence;
pub use loader::skill_roots_with_diagnostics as pitchai_skill_roots_with_diagnostics;
pub use root_loader::PluginSkillSnapshots;
pub use skill_instructions::SkillInstructions;
