use std::collections::HashSet;
use std::io;

use codex_exec_server::ExecutorFileSystem;
use codex_exec_server::WalkEntryKind;
use codex_exec_server::WalkOptions;
use codex_utils_path_uri::PathUri;
use codex_utils_plugins::DISCOVERABLE_PLUGIN_MANIFEST_PATHS;
use codex_utils_plugins::SkillDiscoveryMode;

use super::MAX_SCAN_DEPTH;
use super::MAX_SKILLS_DIRS_PER_ROOT;
use super::SKILLS_FILENAME;
use super::SKILLS_METADATA_DIR;
use super::SKILLS_METADATA_FILENAME;

const MAX_SKILLS_ENTRIES_PER_ROOT: usize = 20_000;
pub(super) const MAX_CONCURRENT_SKILL_LOADS: usize = 64;

pub(super) enum DirectorySymlinkPolicy {
    Follow,
    Ignore,
}

pub(super) enum HiddenDirectoryPolicy {
    Include,
    Skip,
}

pub(super) struct SkillDiscoveryOptions {
    pub directory_symlinks: DirectorySymlinkPolicy,
    pub hidden_directories: HiddenDirectoryPolicy,
    pub mode: SkillDiscoveryMode,
}

pub(super) struct SkillDiscovery {
    pub skills: Vec<DiscoveredSkill>,
    pub plugin_roots: HashSet<PathUri>,
    pub namespace_roots: HashSet<PathUri>,
    pub warnings: Vec<String>,
    /// PitchAI local: every directory this discovery refused because it resolved outside
    /// `source_boundary`, in the order the walk reported them.
    pub boundary_skips: Vec<SkillBoundarySkip>,
}

/// PitchAI local: one directory the source-boundary check refused to walk, and whether the
/// refusal was a link leaving the catalog or a plain directory resolving elsewhere.
pub(super) struct SkillBoundarySkip {
    pub path: PathUri,
    pub symlinked: bool,
}

pub(super) struct DiscoveredSkill {
    pub path: PathUri,
    pub metadata: SkillMetadataDiscovery,
}

pub(super) enum SkillMetadataDiscovery {
    Present(PathUri),
    Absent,
    Probe(PathUri),
}

pub(super) async fn discover_skills(
    file_system: &dyn ExecutorFileSystem,
    root: &PathUri,
    options: SkillDiscoveryOptions,
) -> SkillDiscovery {
    discover_skills_within_boundary(file_system, root, options, /*source_boundary*/ None).await
}

/// PitchAI local: the same walk, refusing to leave `source_boundary` when one is given - a
/// directory that resolves outside it is reported through `boundary_skips` and never walked, so a
/// link out of an approved catalog is an error the caller surfaces rather than content that
/// loads. `None` is the shared discovery: every caller that has no catalog to hold to.
pub(super) async fn discover_skills_within_boundary(
    file_system: &dyn ExecutorFileSystem,
    root: &PathUri,
    options: SkillDiscoveryOptions,
    source_boundary: Option<&PathUri>,
) -> SkillDiscovery {
    let empty_discovery = || SkillDiscovery {
        skills: Vec::new(),
        plugin_roots: HashSet::new(),
        namespace_roots: HashSet::new(),
        warnings: Vec::new(),
        boundary_skips: Vec::new(),
    };
    let walk = match file_system
        .walk(
            root,
            WalkOptions {
                max_depth: match options.mode {
                    SkillDiscoveryMode::Recursive => MAX_SCAN_DEPTH,
                    SkillDiscoveryMode::DirectChildren => 2,
                },
                max_directories: MAX_SKILLS_DIRS_PER_ROOT,
                max_entries: MAX_SKILLS_ENTRIES_PER_ROOT,
                follow_directory_symlinks: matches!(
                    options.directory_symlinks,
                    DirectorySymlinkPolicy::Follow
                ),
                prune_hidden_directories: matches!(
                    options.hidden_directories,
                    HiddenDirectoryPolicy::Skip
                ),
            },
            /*sandbox*/ None,
        )
        .await
    {
        Ok(walk) => walk,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return empty_discovery(),
        Err(error) => {
            let mut discovery = empty_discovery();
            discovery
                .warnings
                .push(format!("failed to walk skills root {root}: {error:#}"));
            return discovery;
        }
    };

    let inventory_complete = !walk.truncated && walk.errors.is_empty();
    let mut warnings = walk
        .errors
        .into_iter()
        .map(|error| {
            format!(
                "failed to scan skill path {}: {}",
                error.path, error.message
            )
        })
        .collect::<Vec<_>>();
    if walk.truncated {
        warnings.push(format!(
            "skills scan reached its traversal limit (root: {root})"
        ));
    }

    let skip_hidden = matches!(options.hidden_directories, HiddenDirectoryPolicy::Skip);
    let mut skill_files = Vec::new();
    let mut file_paths = HashSet::new();
    let mut metadata_directory_parents = HashSet::new();
    let mut plugin_roots = HashSet::new();
    let mut boundary_skips = Vec::new();
    // Directories that resolved outside the boundary. The walk reports a directory before the
    // entries inside it, so recording the escape is enough to keep everything below it out of
    // the inventory as well as the walk that already refused to descend.
    let mut escaped_directories: Vec<PathUri> = Vec::new();
    for entry in walk.entries {
        if skip_hidden && has_hidden_ancestor_below_root(&entry.path, root) {
            continue;
        }
        if escaped_directories
            .iter()
            .any(|escaped| entry.path.starts_with(escaped))
        {
            continue;
        }
        if let Some(boundary) = source_boundary
            && entry.kind == WalkEntryKind::Directory
        {
            let resolved = file_system
                .canonicalize(&entry.path, /*sandbox*/ None)
                .await
                .unwrap_or_else(|_| entry.path.clone());
            if !resolved.starts_with(boundary) {
                boundary_skips.push(SkillBoundarySkip {
                    symlinked: resolved != entry.path,
                    path: entry.path.clone(),
                });
                escaped_directories.push(entry.path.clone());
                continue;
            }
        }
        match entry.kind {
            WalkEntryKind::Directory => {
                if entry
                    .path
                    .basename()
                    .is_some_and(|name| name.eq_ignore_ascii_case(SKILLS_METADATA_DIR))
                    && let Some(skill_dir) = entry.path.parent()
                {
                    metadata_directory_parents.insert(skill_dir);
                }
                if DISCOVERABLE_PLUGIN_MANIFEST_PATHS
                    .iter()
                    .any(|path| path.split('/').next() == entry.path.basename().as_deref())
                    && let Some(plugin_root) = entry.path.parent()
                {
                    plugin_roots.insert(plugin_root);
                }
            }
            WalkEntryKind::File => {
                file_paths.insert(entry.path.clone());
                if entry.path.basename().as_deref() == Some(SKILLS_FILENAME)
                    && (options.mode == SkillDiscoveryMode::Recursive
                        || is_direct_child_skill_path(&entry.path, root))
                {
                    skill_files.push(entry.path);
                }
            }
        }
    }
    let skills = skill_files
        .into_iter()
        .map(|path| DiscoveredSkill {
            metadata: discover_skill_metadata(
                &path,
                &file_paths,
                &metadata_directory_parents,
                inventory_complete,
            ),
            path,
        })
        .collect();

    SkillDiscovery {
        skills,
        plugin_roots,
        namespace_roots: HashSet::from([root.clone()]),
        warnings,
        boundary_skips,
    }
}

fn is_direct_child_skill_path(path: &PathUri, root: &PathUri) -> bool {
    path.parent().and_then(|parent| parent.parent()).as_ref() == Some(root)
}

fn has_hidden_ancestor_below_root(path: &PathUri, root: &PathUri) -> bool {
    let mut ancestor = path.parent();
    while let Some(current) = ancestor {
        if &current == root {
            return false;
        }
        if current.basename().is_some_and(|name| name.starts_with('.')) {
            return true;
        }
        ancestor = current.parent();
    }
    false
}

fn discover_skill_metadata(
    skill_path: &PathUri,
    file_paths: &HashSet<PathUri>,
    metadata_directory_parents: &HashSet<PathUri>,
    inventory_complete: bool,
) -> SkillMetadataDiscovery {
    let Some(skill_dir) = skill_path.parent() else {
        return SkillMetadataDiscovery::Absent;
    };
    let Ok(metadata_dir) = skill_dir.join(SKILLS_METADATA_DIR) else {
        return SkillMetadataDiscovery::Absent;
    };
    let Ok(metadata_path) = metadata_dir.join(SKILLS_METADATA_FILENAME) else {
        return SkillMetadataDiscovery::Absent;
    };
    if file_paths.contains(&metadata_path) {
        return SkillMetadataDiscovery::Present(metadata_path);
    }

    if inventory_complete && !metadata_directory_parents.contains(&skill_dir) {
        SkillMetadataDiscovery::Absent
    } else {
        // A complete walk proves ordinary absence, but keep a filesystem probe for case aliases,
        // file symlinks omitted by the walk, and incomplete inventories.
        SkillMetadataDiscovery::Probe(metadata_path)
    }
}
