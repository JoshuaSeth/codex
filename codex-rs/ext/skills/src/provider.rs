use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use codex_extension_api::ContextualUserFragment;

mod executor;
mod host;
mod orchestrator;

use crate::HostSkillsSnapshot;
use crate::SkillLoadOutcome;
use crate::render::SkillCatalogRenderPolicy;
use crate::render::render_available_skills;
use crate::render::skill_metadata_budget;
use codex_exec_server::ExecutorCapabilityDiscoverySnapshot;
use codex_exec_server::FileSystemSandboxContext;
use codex_exec_server::ResolvedSelectedCapabilityRoot;
use codex_mcp::McpResourceClient;
use codex_protocol::capabilities::SelectedCapabilityRoot;

use crate::catalog::SkillAuthority;
use crate::catalog::SkillCatalog;
use crate::catalog::SkillPackageId;
use crate::catalog::SkillProviderResult;
use crate::catalog::SkillReadResult;
use crate::catalog::SkillResourceId;
use crate::catalog::SkillSearchResult;

pub use executor::ExecutorSkillProvider;
pub use host::HostSkillProvider;
pub use orchestrator::OrchestratorSkillProvider;

pub(crate) const MAX_SKILL_RESOURCE_CONTENT_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug)]
pub struct SkillListQuery {
    pub turn_id: String,
    pub executor_roots: Vec<SelectedCapabilityRoot>,
    pub resolved_executor_roots: Vec<ResolvedSelectedCapabilityRoot>,
    pub host_snapshot: Option<Arc<HostSkillsSnapshot>>,
    pub include_host_skills: bool,
    pub include_bundled_skills: bool,
    pub include_orchestrator_skills: bool,
    pub mcp_resources: Option<Arc<McpResourceClient>>,
    /// Present only when the opt-in high-level executor discovery path is selected.
    pub executor_capability_discovery: Option<ExecutorCapabilityDiscoverySnapshot>,
}

#[derive(Clone, Debug)]
pub struct SkillReadRequest {
    pub authority: SkillAuthority,
    pub package: SkillPackageId,
    pub resource: SkillResourceId,
    pub resolved_executor_roots: Vec<ResolvedSelectedCapabilityRoot>,
    pub sandbox: Option<FileSystemSandboxContext>,
    pub host_snapshot: Option<Arc<HostSkillsSnapshot>>,
    pub mcp_resources: Option<Arc<McpResourceClient>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkillSearchRequest {
    pub authority: SkillAuthority,
    pub package: SkillPackageId,
    pub query: String,
}

pub type SkillProviderFuture<'a, T> =
    Pin<Box<dyn Future<Output = SkillProviderResult<T>> + Send + 'a>>;

/// Source-specific skill catalog and resource access.
///
/// Implementations must preserve authority boundaries: a resource listed by a
/// provider must be read or searched through the same provider/authority rather
/// than converted into an ambient local path.
pub trait SkillProvider: Send + Sync {
    fn list(&self, query: SkillListQuery) -> SkillProviderFuture<'_, SkillCatalog>;

    fn read(&self, request: SkillReadRequest) -> SkillProviderFuture<'_, SkillReadResult>;

    fn search(&self, request: SkillSearchRequest) -> SkillProviderFuture<'_, SkillSearchResult>;
}

/// A rendered host-catalog fragment together with the renderer's own warning.
#[derive(Clone, Debug)]
pub struct RenderedHostSkillsInstructions {
    /// The complete fragment text, markers included.
    pub fragment: String,
    /// Budget or omission warning the renderer produced, if any.
    pub warning: Option<String>,
}

/// Render the host catalog of a load outcome the caller already holds.
///
/// This is the same single-catalog path the extension's own world-state section
/// renders with (`CoreCompatible`, the policy the host catalog has always been
/// ordered and described by), exposed for callers that keep the catalog in
/// their own thread state instead of a world-state section: `codex-core`'s
/// managed PitchAI skills context records the fragment so a catalog change
/// revokes historical skill access. `None` means the catalog has no
/// model-visible skill at all.
pub fn render_host_skills_instructions(
    outcome: &SkillLoadOutcome,
    context_window: Option<i64>,
    include_skills_usage_instructions: bool,
) -> Option<RenderedHostSkillsInstructions> {
    let catalog = host::catalog_from_outcome(outcome);
    let rendered = render_available_skills(
        &catalog,
        SkillCatalogRenderPolicy::CoreCompatible,
        skill_metadata_budget(context_window),
        include_skills_usage_instructions,
    )?;
    let warning = rendered.report.warning_message();
    let fragment = rendered
        .into_fragment(include_skills_usage_instructions)?
        .render();
    Some(RenderedHostSkillsInstructions { fragment, warning })
}

/// The empty catalog fragment: what a managed context renders when a turn has
/// no available skill, or when skill instructions are disabled.
pub fn empty_skills_instructions_fragment() -> String {
    crate::fragments::AvailableSkillsInstructions::from_skill_lines(
        crate::catalog_prompt::SkillPromptKind::Unaliased,
        Vec::new(),
        Vec::new(),
        false,
    )
    .render()
}
