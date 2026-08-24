//! Read-only project discovery for multi-project mode.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::codex_config::{codex_config_path, load_codex_config};
use crate::types::{AppConfig, ProjectCatalogConfig, ProjectCatalogEntryConfig};

pub const DEFAULT_PROJECT_LIMIT: usize = 50;
pub const MAX_PROJECT_LIMIT: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectTrustLevel {
    Trusted,
    Untrusted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectSource {
    CodexConfig,
    ExplicitMetadata,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProjectCandidate {
    #[serde(skip)]
    pub canonical_path: PathBuf,
    pub selector: String,
    pub name: String,
    pub aliases: Vec<String>,
    pub description: Option<String>,
    pub trust_level: Option<ProjectTrustLevel>,
    pub sources: Vec<ProjectSource>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectDiagnosticReason {
    InvalidEntry,
    Untrusted,
    Missing,
    Inaccessible,
    NotDirectory,
    OutsideAccessRoot,
    DuplicateCanonicalPath,
    DuplicateAlias,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProjectCatalogDiagnostic {
    pub reason: ProjectDiagnosticReason,
    pub configured_path: Option<String>,
    pub detail: String,
}

impl ProjectCatalogDiagnostic {
    pub fn render_local(&self) -> String {
        match self.configured_path.as_deref() {
            Some(path) => format!("{path}: {}", self.detail),
            None => self.detail.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProjectCatalog {
    pub access_root: PathBuf,
    pub projects: Vec<ProjectCandidate>,
    pub diagnostics: Vec<ProjectCatalogDiagnostic>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProjectListOutput {
    pub access_root: String,
    pub projects: Vec<ProjectCandidate>,
    pub total: usize,
    pub warnings: Vec<String>,
}

impl ProjectCatalog {
    pub fn list(&self, query: Option<&str>, limit: usize) -> ProjectListOutput {
        let query = query.map(str::trim).filter(|value| !value.is_empty());
        let mut matches = self
            .projects
            .iter()
            .filter_map(|project| {
                query
                    .map(|query| match_rank(project, query).map(|rank| (rank, project)))
                    .unwrap_or(Some((0, project)))
            })
            .collect::<Vec<_>>();
        matches.sort_by(|(left_rank, left), (right_rank, right)| {
            left_rank
                .cmp(right_rank)
                .then_with(|| sortable(&left.name).cmp(&sortable(&right.name)))
                .then_with(|| left.selector.cmp(&right.selector))
        });
        let total = matches.len();
        let projects = matches
            .into_iter()
            .take(limit.min(MAX_PROJECT_LIMIT))
            .map(|(_, project)| project.clone())
            .collect();

        ProjectListOutput {
            access_root: self.access_root.to_string_lossy().into_owned(),
            projects,
            total,
            warnings: self.warnings(),
        }
    }

    pub fn warnings(&self) -> Vec<String> {
        let mut counts = BTreeMap::<ProjectDiagnosticReason, usize>::new();
        for diagnostic in &self.diagnostics {
            *counts.entry(diagnostic.reason).or_default() += 1;
        }

        let mut warnings = Vec::new();
        for (reason, count) in counts {
            let warning = match reason {
                ProjectDiagnosticReason::InvalidEntry => {
                    format!(
                        "Skipped {count} malformed project catalogue entr{}.",
                        plural_y(count)
                    )
                }
                ProjectDiagnosticReason::Untrusted => format!(
                    "Filtered {count} untrusted native Codex project entr{} by operator policy.",
                    plural_y(count)
                ),
                ProjectDiagnosticReason::Missing => {
                    format!(
                        "Skipped {count} project path{} that no longer exist{}.",
                        plural_s(count),
                        singular_s(count)
                    )
                }
                ProjectDiagnosticReason::Inaccessible => format!(
                    "Skipped {count} project path{} that could not be accessed.",
                    plural_s(count)
                ),
                ProjectDiagnosticReason::NotDirectory => format!(
                    "Skipped {count} project path{} that {} not directories.",
                    plural_s(count),
                    if count == 1 { "is" } else { "are" }
                ),
                ProjectDiagnosticReason::OutsideAccessRoot => format!(
                    "Skipped {count} project path{} outside the configured access root.",
                    plural_s(count)
                ),
                ProjectDiagnosticReason::DuplicateCanonicalPath => format!(
                    "Merged {count} duplicate project path{} that resolve to an existing catalogue entry.",
                    plural_s(count)
                ),
                ProjectDiagnosticReason::DuplicateAlias => format!(
                    "Found {count} alias{} shared by multiple projects; intent matching may be ambiguous.",
                    plural_s(count)
                ),
            };
            warnings.push(warning);
        }
        warnings
    }
}

pub fn discover_project_catalog(config: &AppConfig) -> Result<ProjectCatalog, String> {
    let codex_path = if config.project_catalog.codex_config.enabled {
        Some(codex_config_path()?)
    } else {
        None
    };
    discover_project_catalog_at(
        &config.work_dir,
        &config.project_catalog,
        codex_path.as_deref(),
    )
}

pub fn discover_project_catalog_at(
    access_root: &Path,
    config: &ProjectCatalogConfig,
    codex_path: Option<&Path>,
) -> Result<ProjectCatalog, String> {
    let canonical_access_root = std::fs::canonicalize(access_root).map_err(|error| {
        format!(
            "Could not resolve project access root {}: {error}",
            access_root.display()
        )
    })?;
    if !canonical_access_root.is_dir() {
        return Err(format!(
            "Project access root is not a directory: {}",
            canonical_access_root.display()
        ));
    }

    let mut diagnostics = Vec::new();
    let mut raw_entries = Vec::new();

    if config.codex_config.enabled
        && let Some(path) = codex_path
        && let Some(root) = load_codex_config(path)?
    {
        raw_entries.extend(parse_native_entries(
            &root,
            config.codex_config.trusted_only,
            &mut diagnostics,
        )?);
    }

    raw_entries.extend(config.entries.iter().cloned().map(RawProject::explicit));

    let mut candidates = BTreeMap::<PathBuf, CandidateBuilder>::new();
    for raw in raw_entries {
        let Some(path) = raw
            .path
            .as_deref()
            .map(str::trim)
            .filter(|path| !path.is_empty())
        else {
            diagnostics.push(ProjectCatalogDiagnostic {
                reason: ProjectDiagnosticReason::InvalidEntry,
                configured_path: None,
                detail: "project entry is missing a non-empty `path`".to_string(),
            });
            continue;
        };
        let configured_path = path.to_string();
        let candidate_path = match raw.source {
            ProjectSource::CodexConfig => {
                let path = PathBuf::from(path);
                if !path.is_absolute() {
                    diagnostics.push(ProjectCatalogDiagnostic {
                        reason: ProjectDiagnosticReason::InvalidEntry,
                        configured_path: Some(configured_path),
                        detail: "native Codex project paths must be absolute".to_string(),
                    });
                    continue;
                }
                path
            }
            ProjectSource::ExplicitMetadata => {
                let path = PathBuf::from(path);
                if path.is_absolute() {
                    path
                } else {
                    canonical_access_root.join(path)
                }
            }
        };

        let metadata = match std::fs::metadata(&candidate_path) {
            Ok(metadata) => metadata,
            Err(error) => {
                let reason = if error.kind() == std::io::ErrorKind::NotFound {
                    ProjectDiagnosticReason::Missing
                } else {
                    ProjectDiagnosticReason::Inaccessible
                };
                diagnostics.push(ProjectCatalogDiagnostic {
                    reason,
                    configured_path: Some(configured_path),
                    detail: if reason == ProjectDiagnosticReason::Missing {
                        "path does not exist".to_string()
                    } else {
                        "path cannot be accessed".to_string()
                    },
                });
                continue;
            }
        };
        if !metadata.is_dir() {
            diagnostics.push(ProjectCatalogDiagnostic {
                reason: ProjectDiagnosticReason::NotDirectory,
                configured_path: Some(configured_path),
                detail: "path is not a directory".to_string(),
            });
            continue;
        }

        let canonical_path = match std::fs::canonicalize(&candidate_path) {
            Ok(path) => path,
            Err(_) => {
                diagnostics.push(ProjectCatalogDiagnostic {
                    reason: ProjectDiagnosticReason::Inaccessible,
                    configured_path: Some(configured_path),
                    detail: "path cannot be canonicalized".to_string(),
                });
                continue;
            }
        };
        if canonical_path != canonical_access_root
            && !canonical_path.starts_with(&canonical_access_root)
        {
            diagnostics.push(ProjectCatalogDiagnostic {
                reason: ProjectDiagnosticReason::OutsideAccessRoot,
                configured_path: Some(configured_path),
                detail: "canonical path is outside the configured access root".to_string(),
            });
            continue;
        }

        let selector = match selector_for(&canonical_path, &canonical_access_root) {
            Some(selector) => selector,
            None => {
                diagnostics.push(ProjectCatalogDiagnostic {
                    reason: ProjectDiagnosticReason::InvalidEntry,
                    configured_path: Some(configured_path),
                    detail: "path cannot be represented as an MCP selector".to_string(),
                });
                continue;
            }
        };

        if let Some(existing) = candidates.get_mut(&canonical_path) {
            let repeats_source = existing.sources.contains(&raw.source);
            existing.merge(&raw);
            if repeats_source {
                diagnostics.push(ProjectCatalogDiagnostic {
                    reason: ProjectDiagnosticReason::DuplicateCanonicalPath,
                    configured_path: Some(configured_path),
                    detail: format!("resolves to the existing selector `{selector}`"),
                });
            }
            continue;
        }

        candidates.insert(
            canonical_path.clone(),
            CandidateBuilder::new(canonical_path, selector, &raw),
        );
    }

    let mut projects = candidates
        .into_values()
        .map(CandidateBuilder::finish)
        .collect::<Vec<_>>();
    projects.sort_by(|left, right| {
        sortable(&left.name)
            .cmp(&sortable(&right.name))
            .then_with(|| left.selector.cmp(&right.selector))
    });
    find_duplicate_aliases(&projects, &mut diagnostics);

    Ok(ProjectCatalog {
        access_root: canonical_access_root,
        projects,
        diagnostics,
    })
}

#[derive(Debug, Clone)]
struct RawProject {
    path: Option<String>,
    name: Option<String>,
    aliases: Vec<String>,
    description: Option<String>,
    trust_level: Option<ProjectTrustLevel>,
    source: ProjectSource,
}

impl RawProject {
    fn explicit(entry: ProjectCatalogEntryConfig) -> Self {
        Self {
            path: entry.path,
            name: entry.name,
            aliases: entry.aliases,
            description: entry.description,
            trust_level: None,
            source: ProjectSource::ExplicitMetadata,
        }
    }
}

fn parse_native_entries(
    root: &toml::Table,
    trusted_only: bool,
    diagnostics: &mut Vec<ProjectCatalogDiagnostic>,
) -> Result<Vec<RawProject>, String> {
    let Some(value) = root.get("projects") else {
        return Ok(Vec::new());
    };
    let Some(projects) = value.as_table() else {
        return Err("Codex `projects` must be a TOML table".to_string());
    };

    let mut paths = projects.keys().collect::<Vec<_>>();
    paths.sort();
    let mut entries = Vec::new();
    for path in paths {
        let Some(table) = projects[path].as_table() else {
            diagnostics.push(ProjectCatalogDiagnostic {
                reason: ProjectDiagnosticReason::InvalidEntry,
                configured_path: Some(path.clone()),
                detail: "native Codex project entry must be a table".to_string(),
            });
            continue;
        };
        let trust_level = match table.get("trust_level").and_then(toml::Value::as_str) {
            Some("trusted") => ProjectTrustLevel::Trusted,
            Some("untrusted") => ProjectTrustLevel::Untrusted,
            Some(_) => {
                diagnostics.push(ProjectCatalogDiagnostic {
                    reason: ProjectDiagnosticReason::InvalidEntry,
                    configured_path: Some(path.clone()),
                    detail: "native Codex `trust_level` must be `trusted` or `untrusted`"
                        .to_string(),
                });
                continue;
            }
            None => {
                diagnostics.push(ProjectCatalogDiagnostic {
                    reason: ProjectDiagnosticReason::InvalidEntry,
                    configured_path: Some(path.clone()),
                    detail: "native Codex project entry is missing a string `trust_level`"
                        .to_string(),
                });
                continue;
            }
        };
        if trusted_only && trust_level == ProjectTrustLevel::Untrusted {
            diagnostics.push(ProjectCatalogDiagnostic {
                reason: ProjectDiagnosticReason::Untrusted,
                configured_path: Some(path.clone()),
                detail: "native Codex project is untrusted and trustedOnly is enabled".to_string(),
            });
            continue;
        }
        entries.push(RawProject {
            path: Some(path.clone()),
            name: None,
            aliases: Vec::new(),
            description: None,
            trust_level: Some(trust_level),
            source: ProjectSource::CodexConfig,
        });
    }
    Ok(entries)
}

struct CandidateBuilder {
    canonical_path: PathBuf,
    selector: String,
    name: Option<String>,
    aliases: BTreeMap<String, String>,
    description: Option<String>,
    trust_level: Option<ProjectTrustLevel>,
    sources: BTreeSet<ProjectSource>,
}

impl CandidateBuilder {
    fn new(canonical_path: PathBuf, selector: String, raw: &RawProject) -> Self {
        let mut builder = Self {
            canonical_path,
            selector,
            name: None,
            aliases: BTreeMap::new(),
            description: None,
            trust_level: None,
            sources: BTreeSet::new(),
        };
        builder.merge(raw);
        builder
    }

    fn merge(&mut self, raw: &RawProject) {
        self.sources.insert(raw.source);
        if let Some(trust_level) = raw.trust_level {
            self.trust_level = match (self.trust_level, trust_level) {
                (Some(ProjectTrustLevel::Trusted), _) | (_, ProjectTrustLevel::Trusted) => {
                    Some(ProjectTrustLevel::Trusted)
                }
                _ => Some(ProjectTrustLevel::Untrusted),
            };
        }
        if raw.source == ProjectSource::ExplicitMetadata {
            if let Some(name) = normalized_text(raw.name.as_deref()) {
                self.name = Some(name);
            }
            if let Some(description) = normalized_text(raw.description.as_deref()) {
                self.description = Some(description);
            }
            for alias in &raw.aliases {
                if let Some(alias) = normalized_text(Some(alias)) {
                    self.aliases.entry(sortable(&alias)).or_insert(alias);
                }
            }
        }
    }

    fn finish(self) -> ProjectCandidate {
        let default_name = self
            .canonical_path
            .file_name()
            .and_then(|value| value.to_str())
            .filter(|value| !value.is_empty())
            .unwrap_or(&self.selector)
            .to_string();
        ProjectCandidate {
            canonical_path: self.canonical_path,
            selector: self.selector,
            name: self.name.unwrap_or(default_name),
            aliases: self.aliases.into_values().collect(),
            description: self.description,
            trust_level: self.trust_level,
            sources: self.sources.into_iter().collect(),
        }
    }
}

fn selector_for(path: &Path, access_root: &Path) -> Option<String> {
    let relative = path.strip_prefix(access_root).ok()?;
    if relative.as_os_str().is_empty() {
        return Some(".".to_string());
    }
    let text = relative.to_str()?;
    Some(text.replace(std::path::MAIN_SEPARATOR, "/"))
}

fn normalized_text(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn sortable(value: &str) -> String {
    value.to_lowercase()
}

fn match_rank(project: &ProjectCandidate, query: &str) -> Option<u8> {
    let query = sortable(query);
    let name = sortable(&project.name);
    let aliases = project.aliases.iter().map(|alias| sortable(alias));
    if name == query || aliases.clone().any(|alias| alias == query) {
        return Some(0);
    }
    if name.starts_with(&query) || aliases.clone().any(|alias| alias.starts_with(&query)) {
        return Some(1);
    }
    if name.contains(&query)
        || aliases.clone().any(|alias| alias.contains(&query))
        || project
            .description
            .as_deref()
            .map(sortable)
            .is_some_and(|description| description.contains(&query))
    {
        return Some(2);
    }
    sortable(&project.selector).contains(&query).then_some(3)
}

fn find_duplicate_aliases(
    projects: &[ProjectCandidate],
    diagnostics: &mut Vec<ProjectCatalogDiagnostic>,
) {
    let mut owners = HashMap::<String, Vec<&ProjectCandidate>>::new();
    for project in projects {
        for alias in &project.aliases {
            owners.entry(sortable(alias)).or_default().push(project);
        }
    }
    let mut aliases = owners.into_iter().collect::<Vec<_>>();
    aliases.sort_by(|left, right| left.0.cmp(&right.0));
    for (alias_key, projects) in aliases {
        if projects.len() < 2 {
            continue;
        }
        let alias = projects[0]
            .aliases
            .iter()
            .find(|alias| sortable(alias) == alias_key)
            .cloned()
            .unwrap_or_else(|| "<alias>".to_string());
        let selectors = projects
            .iter()
            .map(|project| format!("`{}`", project.selector))
            .collect::<Vec<_>>()
            .join(", ");
        diagnostics.push(ProjectCatalogDiagnostic {
            reason: ProjectDiagnosticReason::DuplicateAlias,
            configured_path: None,
            detail: format!("alias `{alias}` is shared by {selectors}"),
        });
    }
}

fn plural_s(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

fn singular_s(count: usize) -> &'static str {
    if count == 1 { "s" } else { "" }
}

fn plural_y(count: usize) -> &'static str {
    if count == 1 { "y" } else { "ies" }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quote(value: &str) -> String {
        serde_json::to_string(value).unwrap()
    }

    fn write_codex_config(root: &Path, contents: &str) -> PathBuf {
        let path = root.join("config.toml");
        std::fs::write(&path, contents).unwrap();
        path
    }

    fn explicit(path: impl Into<String>) -> ProjectCatalogEntryConfig {
        ProjectCatalogEntryConfig {
            path: Some(path.into()),
            ..Default::default()
        }
    }

    #[test]
    fn imports_trusted_native_project_and_overlays_metadata() {
        let root = tempfile::tempdir().unwrap();
        let access = root.path().join("projects");
        let project = access.join("codexify");
        std::fs::create_dir_all(&project).unwrap();
        let codex_path = write_codex_config(
            root.path(),
            &format!(
                "[projects.{}]\ntrust_level = \"trusted\"\n",
                quote(project.to_str().unwrap())
            ),
        );
        let mut config = ProjectCatalogConfig::default();
        config.entries.push(ProjectCatalogEntryConfig {
            path: Some(project.to_string_lossy().into_owned()),
            name: Some("Codexify".to_string()),
            aliases: vec!["ChatGPT bridge".to_string(), "chatgpt bridge".to_string()],
            description: Some("Rust MCP bridge".to_string()),
        });

        let catalog = discover_project_catalog_at(&access, &config, Some(&codex_path)).unwrap();
        assert_eq!(catalog.projects.len(), 1);
        let project = &catalog.projects[0];
        assert_eq!(project.selector, "codexify");
        assert_eq!(project.name, "Codexify");
        assert_eq!(project.aliases, ["ChatGPT bridge"]);
        assert_eq!(project.description.as_deref(), Some("Rust MCP bridge"));
        assert_eq!(project.trust_level, Some(ProjectTrustLevel::Trusted));
        assert_eq!(
            project.sources,
            [ProjectSource::CodexConfig, ProjectSource::ExplicitMetadata]
        );
        assert!(catalog.warnings().is_empty());
    }

    #[test]
    fn filters_untrusted_missing_and_outside_entries_without_path_disclosure() {
        let root = tempfile::tempdir().unwrap();
        let access = root.path().join("projects");
        let trusted = access.join("trusted");
        let untrusted = access.join("untrusted");
        let missing = access.join("private-missing-name");
        let outside = root.path().join("private-outside-name");
        for path in [&trusted, &untrusted, &outside] {
            std::fs::create_dir_all(path).unwrap();
        }
        let codex_path = write_codex_config(
            root.path(),
            &format!(
                concat!(
                    "[projects.{}]\ntrust_level = \"trusted\"\n",
                    "[projects.{}]\ntrust_level = \"untrusted\"\n",
                    "[projects.{}]\ntrust_level = \"trusted\"\n",
                    "[projects.{}]\ntrust_level = \"trusted\"\n"
                ),
                quote(trusted.to_str().unwrap()),
                quote(untrusted.to_str().unwrap()),
                quote(missing.to_str().unwrap()),
                quote(outside.to_str().unwrap())
            ),
        );

        let catalog = discover_project_catalog_at(
            &access,
            &ProjectCatalogConfig::default(),
            Some(&codex_path),
        )
        .unwrap();
        assert_eq!(
            catalog
                .projects
                .iter()
                .map(|project| project.selector.as_str())
                .collect::<Vec<_>>(),
            ["trusted"]
        );
        let warnings = catalog.warnings().join("\n");
        assert!(warnings.contains("untrusted"));
        assert!(warnings.contains("no longer exists"));
        assert!(warnings.contains("outside the configured access root"));
        assert!(!warnings.contains("private-missing-name"));
        assert!(!warnings.contains("private-outside-name"));
    }

    #[test]
    fn operator_can_include_untrusted_native_projects() {
        let root = tempfile::tempdir().unwrap();
        let access = root.path().join("projects");
        let project = access.join("untrusted");
        std::fs::create_dir_all(&project).unwrap();
        let codex_path = write_codex_config(
            root.path(),
            &format!(
                "[projects.{}]\ntrust_level = \"untrusted\"\n",
                quote(project.to_str().unwrap())
            ),
        );
        let mut config = ProjectCatalogConfig::default();
        config.codex_config.trusted_only = false;

        let catalog = discover_project_catalog_at(&access, &config, Some(&codex_path)).unwrap();
        assert_eq!(catalog.projects.len(), 1);
        assert_eq!(
            catalog.projects[0].trust_level,
            Some(ProjectTrustLevel::Untrusted)
        );
    }

    #[test]
    fn explicit_relative_entry_does_not_require_native_codex_config() {
        let root = tempfile::tempdir().unwrap();
        let access = root.path().join("projects");
        std::fs::create_dir_all(access.join("manual")).unwrap();
        let mut config = ProjectCatalogConfig::default();
        config.codex_config.enabled = false;
        config.entries.push(ProjectCatalogEntryConfig {
            path: Some("manual".to_string()),
            name: Some("  Manual Project  ".to_string()),
            aliases: vec![" local ".to_string(), "LOCAL".to_string()],
            description: Some("  Explicit catalogue entry  ".to_string()),
        });

        let catalog = discover_project_catalog_at(&access, &config, None).unwrap();
        assert_eq!(catalog.projects.len(), 1);
        let project = &catalog.projects[0];
        assert_eq!(project.name, "Manual Project");
        assert_eq!(project.aliases, ["local"]);
        assert_eq!(
            project.description.as_deref(),
            Some("Explicit catalogue entry")
        );
        assert_eq!(project.trust_level, None);
        assert_eq!(project.sources, [ProjectSource::ExplicitMetadata]);
    }

    #[test]
    fn query_ranking_is_deterministic() {
        let root = tempfile::tempdir().unwrap();
        let access = root.path().join("projects");
        for name in ["alpha", "beta", "gamma", "delta"] {
            std::fs::create_dir_all(access.join(name)).unwrap();
        }
        let mut config = ProjectCatalogConfig::default();
        config.codex_config.enabled = false;
        config.entries = vec![
            ProjectCatalogEntryConfig {
                path: Some("alpha".to_string()),
                name: Some("Alpha".to_string()),
                aliases: vec!["bridge".to_string()],
                ..Default::default()
            },
            ProjectCatalogEntryConfig {
                path: Some("beta".to_string()),
                name: Some("Bridge Tools".to_string()),
                ..Default::default()
            },
            ProjectCatalogEntryConfig {
                path: Some("gamma".to_string()),
                name: Some("Gamma".to_string()),
                description: Some("A bridge runtime".to_string()),
                ..Default::default()
            },
            ProjectCatalogEntryConfig {
                path: Some("delta".to_string()),
                name: Some("Delta".to_string()),
                ..Default::default()
            },
        ];
        let catalog = discover_project_catalog_at(&access, &config, None).unwrap();

        let output = catalog.list(Some("BRIDGE"), 2);
        assert_eq!(output.total, 3);
        assert_eq!(
            output
                .projects
                .iter()
                .map(|project| project.selector.as_str())
                .collect::<Vec<_>>(),
            ["alpha", "beta"]
        );
    }

    #[test]
    fn malformed_native_entry_does_not_hide_valid_sibling() {
        let root = tempfile::tempdir().unwrap();
        let access = root.path().join("projects");
        let valid = access.join("valid");
        std::fs::create_dir_all(&valid).unwrap();
        let invalid = access.join("invalid");
        let codex_path = write_codex_config(
            root.path(),
            &format!(
                concat!(
                    "[projects]\n{} = 42\n",
                    "[projects.{}]\ntrust_level = \"trusted\"\n"
                ),
                quote(invalid.to_str().unwrap()),
                quote(valid.to_str().unwrap())
            ),
        );

        let catalog = discover_project_catalog_at(
            &access,
            &ProjectCatalogConfig::default(),
            Some(&codex_path),
        )
        .unwrap();
        assert_eq!(catalog.projects.len(), 1);
        assert_eq!(catalog.projects[0].selector, "valid");
        assert!(
            catalog
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.reason == ProjectDiagnosticReason::InvalidEntry)
        );
    }

    #[test]
    fn invalid_and_missing_trust_levels_do_not_hide_valid_unicode_path() {
        let root = tempfile::tempdir().unwrap();
        let access = root.path().join("projects");
        let valid = access.join("café project.with.dots");
        let invalid = access.join("invalid");
        let missing = access.join("missing-trust");
        std::fs::create_dir_all(&valid).unwrap();
        std::fs::create_dir_all(&invalid).unwrap();
        std::fs::create_dir_all(&missing).unwrap();
        std::fs::write(valid.join(".git"), "gitdir: ../worktrees/example").unwrap();
        let codex_path = write_codex_config(
            root.path(),
            &format!(
                concat!(
                    "[projects.{}]\ntrust_level = \"trusted\"\n",
                    "[projects.{}]\ntrust_level = \"owner\"\n",
                    "[projects.{}]\nname = \"ignored\"\n"
                ),
                quote(valid.to_str().unwrap()),
                quote(invalid.to_str().unwrap()),
                quote(missing.to_str().unwrap())
            ),
        );

        let catalog = discover_project_catalog_at(
            &access,
            &ProjectCatalogConfig::default(),
            Some(&codex_path),
        )
        .unwrap();
        assert_eq!(catalog.projects.len(), 1);
        assert_eq!(catalog.projects[0].selector, "café project.with.dots");
        assert_eq!(
            catalog
                .diagnostics
                .iter()
                .filter(|diagnostic| diagnostic.reason == ProjectDiagnosticReason::InvalidEntry)
                .count(),
            2
        );
    }

    #[test]
    fn native_mcp_secrets_never_enter_catalogue_output_or_diagnostics() {
        let root = tempfile::tempdir().unwrap();
        let access = root.path().join("projects");
        let project = access.join("project");
        std::fs::create_dir_all(&project).unwrap();
        let secret = "catalogue-secret-must-not-leak";
        let codex_path = write_codex_config(
            root.path(),
            &format!(
                concat!(
                    "[projects.{}]\ntrust_level = \"trusted\"\n",
                    "[mcp_servers.private]\ncommand = \"server\"\n",
                    "[mcp_servers.private.env]\nTOKEN = \"{}\"\n"
                ),
                quote(project.to_str().unwrap()),
                secret
            ),
        );

        let catalog = discover_project_catalog_at(
            &access,
            &ProjectCatalogConfig::default(),
            Some(&codex_path),
        )
        .unwrap();
        let output = serde_json::to_string(&catalog.list(None, MAX_PROJECT_LIMIT)).unwrap();
        let diagnostics = catalog
            .diagnostics
            .iter()
            .map(ProjectCatalogDiagnostic::render_local)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!output.contains(secret));
        assert!(!diagnostics.contains(secret));
    }

    #[test]
    fn regular_files_are_not_selectable() {
        let root = tempfile::tempdir().unwrap();
        let access = root.path().join("projects");
        std::fs::create_dir_all(&access).unwrap();
        std::fs::write(access.join("file"), "not a project").unwrap();
        let mut config = ProjectCatalogConfig::default();
        config.codex_config.enabled = false;
        config.entries.push(explicit("file"));

        let catalog = discover_project_catalog_at(&access, &config, None).unwrap();
        assert!(catalog.projects.is_empty());
        assert!(
            catalog
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.reason == ProjectDiagnosticReason::NotDirectory)
        );
    }

    #[test]
    fn duplicate_aliases_are_reported() {
        let root = tempfile::tempdir().unwrap();
        let access = root.path().join("projects");
        for name in ["alpha", "beta"] {
            std::fs::create_dir_all(access.join(name)).unwrap();
        }
        let mut config = ProjectCatalogConfig::default();
        config.codex_config.enabled = false;
        config.entries = vec![
            ProjectCatalogEntryConfig {
                path: Some("alpha".to_string()),
                aliases: vec!["Shared".to_string()],
                ..Default::default()
            },
            ProjectCatalogEntryConfig {
                path: Some("beta".to_string()),
                aliases: vec!["shared".to_string()],
                ..Default::default()
            },
        ];

        let catalog = discover_project_catalog_at(&access, &config, None).unwrap();
        assert!(catalog.warnings().join("\n").contains("shared by multiple"));
        assert!(
            catalog
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.detail.contains("`alpha`")
                    && diagnostic.detail.contains("`beta`"))
        );
    }

    #[test]
    fn access_root_itself_uses_dot_selector() {
        let root = tempfile::tempdir().unwrap();
        let access = root.path().join("projects");
        std::fs::create_dir_all(&access).unwrap();
        let mut config = ProjectCatalogConfig::default();
        config.codex_config.enabled = false;
        config.entries.push(explicit("."));

        let catalog = discover_project_catalog_at(&access, &config, None).unwrap();
        assert_eq!(catalog.projects[0].selector, ".");
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escape_is_rejected_and_aliases_are_deduplicated() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let access = root.path().join("projects");
        let real = access.join("real");
        let outside = root.path().join("outside");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        symlink(&real, access.join("alias")).unwrap();
        symlink(&outside, access.join("escape")).unwrap();
        let mut config = ProjectCatalogConfig::default();
        config.codex_config.enabled = false;
        config.entries = vec![explicit("real"), explicit("alias"), explicit("escape")];

        let catalog = discover_project_catalog_at(&access, &config, None).unwrap();
        assert_eq!(catalog.projects.len(), 1);
        assert!(
            catalog
                .warnings()
                .join("\n")
                .contains("duplicate project path")
        );
        assert!(
            catalog
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.reason == ProjectDiagnosticReason::OutsideAccessRoot)
        );
    }
}
