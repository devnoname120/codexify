//! Skill discovery, ported from `src/skills.ts`.
//!
//! A skill is a directory holding a `SKILL.md` whose YAML frontmatter names it
//! and says when to use it. Codex loads the catalogue at session start, puts the
//! names and descriptions in the prompt, and reads a body only once a skill is
//! chosen — the progressive disclosure that keeps a large library affordable.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use crate::codex_plugin_skills::discover_codex_plugin_skills;
use crate::project_doc::project_dirs;
use crate::types::AppConfig;
use crate::util::home_dir;

pub const SKILL_FILENAME: &str = "SKILL.md";
/// Codex's `MAX_NAME_LEN`, counted in bytes as the Rust does.
pub const MAX_SKILL_NAME_BYTES: usize = 64;
/// Files listed after a SKILL.md body so the model can reach the rest of the package.
pub const MAX_SKILL_PACKAGE_FILES: usize = 50;

/// The skill subdirectory pairs searched inside each project directory and
/// under the home directory: `.agents/skills`, `.codex/skills`, and optionally
/// `.claude/skills` when experimental Claude discovery is enabled.
const SKILL_DIR_NAMES: &[(&str, &str)] = &[
    (".agents", "skills"),
    (".codex", "skills"),
    (".claude", "skills"),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillScope {
    Repo,
    User,
    /// A skill bundled with an installed Codex or Claude Code plugin.
    Plugin,
}

impl SkillScope {
    pub fn as_str(&self) -> &'static str {
        match self {
            SkillScope::Repo => "repo",
            SkillScope::User => "user",
            SkillScope::Plugin => "plugin",
        }
    }
}

#[derive(Debug, Clone)]
pub struct SkillRoot {
    pub path: PathBuf,
    pub scope: SkillScope,
}

#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub short_description: Option<String>,
    pub allow_implicit_invocation: bool,
    /// Directory holding SKILL.md. Resources resolve against it, never work-dir.
    pub dir: PathBuf,
    /// Absolute path of the SKILL.md itself.
    pub path: PathBuf,
    pub scope: SkillScope,
}

#[derive(Debug, Clone)]
pub struct SkillWarning {
    pub path: PathBuf,
    pub message: String,
}

#[derive(Debug, Clone, Default)]
pub struct SkillCatalog {
    pub skills: Vec<Skill>,
    pub warnings: Vec<SkillWarning>,
    /// Roots searched, in precedence order, whether or not they exist.
    pub roots: Vec<SkillRoot>,
}

pub fn skills_enabled(config: &AppConfig) -> bool {
    config.skills.enabled.unwrap_or(true)
}

pub fn plugins_enabled(config: &AppConfig) -> bool {
    // Default on, but suppressed when `dirs` is set (an explicit "use exactly
    // these roots" override, which the test suite also relies on for isolation).
    // `includePlugins` explicitly wins either way.
    config
        .skills
        .include_plugins
        .unwrap_or_else(|| config.skills.dirs.is_none())
}

/// Roots to search, highest precedence first. Repo skills come before user
/// skills, so a project that ships a skill decides how that name behaves inside
/// it. Within the repo the walk runs outermost first.
pub fn skill_roots(config: &AppConfig) -> Vec<SkillRoot> {
    let mut roots: Vec<SkillRoot> = Vec::new();

    for dir in project_dirs(config) {
        for (a, b) in SKILL_DIR_NAMES {
            if *a == ".claude" && !config.experimental.claude_skills {
                continue;
            }
            roots.push(SkillRoot {
                path: dir.join(a).join(b),
                scope: SkillScope::Repo,
            });
        }
    }

    match &config.skills.dirs {
        Some(configured) => {
            // Configured directories replace the home-directory defaults.
            for dir in configured {
                let p = Path::new(dir);
                let path = if p.is_absolute() {
                    p.to_path_buf()
                } else {
                    config.work_dir.join(p)
                };
                roots.push(SkillRoot {
                    path,
                    scope: SkillScope::User,
                });
            }
        }
        None => {
            if let Some(home) = home_dir() {
                for (a, b) in SKILL_DIR_NAMES {
                    if *a == ".claude" && !config.experimental.claude_skills {
                        continue;
                    }
                    roots.push(SkillRoot {
                        path: home.join(a).join(b),
                        scope: SkillScope::User,
                    });
                }
            }
        }
    }

    // Auto-generated gateway skills (one per gateway-mode MCP server).
    if let Some(dir) = &config.generated_skills_dir {
        roots.push(SkillRoot {
            path: dir.clone(),
            scope: SkillScope::Plugin,
        });
    }

    let mut seen = std::collections::HashSet::new();
    roots.retain(|r| seen.insert(r.path.clone()));
    roots
}

#[derive(Debug, Clone)]
pub struct SkillFrontmatter {
    pub name: String,
    pub description: String,
    pub short_description: Option<String>,
}

/// The YAML block between the leading `---` and the next one, or `None` when the
/// file does not open with one.
pub fn extract_frontmatter(contents: &str) -> Option<String> {
    let text = contents.strip_prefix('\u{feff}').unwrap_or(contents);
    let lines: Vec<&str> = text.split('\n').map(|l| l.trim_end_matches('\r')).collect();
    if lines.first().map(|l| l.trim()) != Some("---") {
        return None;
    }
    for i in 1..lines.len() {
        if lines[i].trim() == "---" {
            return Some(lines[1..i].join("\n"));
        }
    }
    None
}

/// Codex collapses runs of whitespace so a wrapped YAML scalar stays one line.
fn single_line(raw: Option<&serde_yaml::Value>) -> String {
    match raw.and_then(|v| v.as_str()) {
        Some(s) => s.split_whitespace().collect::<Vec<_>>().join(" "),
        None => String::new(),
    }
}

/// Parses and validates a SKILL.md frontmatter block. `name` falls back to the
/// directory name; `description` does not, because it is the only thing the
/// model sees before deciding whether to read the skill at all.
pub fn parse_skill_frontmatter(
    contents: &str,
    default_name: &str,
) -> Result<SkillFrontmatter, String> {
    let block = extract_frontmatter(contents)
        .ok_or_else(|| "missing YAML frontmatter delimited by ---".to_string())?;

    let parsed: serde_yaml::Value =
        serde_yaml::from_str(&block).map_err(|e| format!("invalid YAML: {e}"))?;

    let mapping = match &parsed {
        serde_yaml::Value::Mapping(_) => &parsed,
        _ => return Err("invalid YAML: frontmatter is not a mapping".to_string()),
    };

    let get = |key: &str| mapping.get(serde_yaml::Value::String(key.to_string()));
    let metadata = get("metadata");
    let short =
        metadata.and_then(|m| m.get(serde_yaml::Value::String("short-description".to_string())));

    let name = {
        let n = single_line(get("name"));
        if n.is_empty() {
            default_name.to_string()
        } else {
            n
        }
    };
    let description = single_line(get("description"));
    let short_description = single_line(short);

    if name.len() > MAX_SKILL_NAME_BYTES {
        return Err(format!(
            "invalid name: longer than {MAX_SKILL_NAME_BYTES} bytes"
        ));
    }
    if description.is_empty() {
        return Err("missing field `description`".to_string());
    }

    Ok(SkillFrontmatter {
        name,
        description,
        short_description: if short_description.is_empty() {
            None
        } else {
            Some(short_description)
        },
    })
}

#[derive(serde::Deserialize)]
struct SkillAgentMetadata {
    policy: Option<SkillInvocationPolicy>,
}

#[derive(serde::Deserialize)]
struct SkillInvocationPolicy {
    allow_implicit_invocation: Option<bool>,
}

pub(crate) fn skill_allows_implicit_invocation(
    dir: &Path,
    warnings: &mut Vec<SkillWarning>,
) -> bool {
    let path = dir.join("agents/openai.yaml");
    let contents = match std::fs::read_to_string(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return true,
        result => result.map_err(|error| error.to_string()),
    };
    let metadata = contents.and_then(|contents| {
        serde_yaml::from_str::<Option<SkillAgentMetadata>>(&contents)
            .map_err(|error| error.to_string())
    });
    match metadata {
        Ok(metadata) => metadata
            .and_then(|metadata| metadata.policy)
            .and_then(|policy| policy.allow_implicit_invocation)
            .unwrap_or(true),
        Err(error) => {
            warnings.push(SkillWarning {
                path,
                message: format!("invalid skill metadata: {error}; implicit invocation disabled"),
            });
            false
        }
    }
}

fn is_directory(path: &Path) -> bool {
    path.is_dir()
}

/// Sorted directory entry names, or empty when the directory cannot be read.
fn sorted_entries(dir: &Path) -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = rd
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// Every skill reachable from the configured roots, sorted by name.
pub fn discover_skills(config: &AppConfig) -> SkillCatalog {
    if !skills_enabled(config) {
        return SkillCatalog::default();
    }

    let roots = skill_roots(config);
    let mut skills: Vec<Skill> = Vec::new();
    let mut warnings: Vec<SkillWarning> = Vec::new();
    let mut by_name: HashMap<String, Skill> = HashMap::new();

    for root in &roots {
        for entry in sorted_entries(&root.path) {
            let dir = root.path.join(&entry);
            if !is_directory(&dir) {
                continue;
            }
            let path = dir.join(SKILL_FILENAME);
            let Ok(contents) = std::fs::read_to_string(&path) else {
                continue;
            };

            let parsed = match parse_skill_frontmatter(&contents, &entry) {
                Ok(p) => p,
                Err(message) => {
                    warnings.push(SkillWarning { path, message });
                    continue;
                }
            };

            let key = parsed.name.to_lowercase();
            if let Some(shadowed) = by_name.get(&key) {
                warnings.push(SkillWarning {
                    path,
                    message: format!(
                        "shadowed by the {} skill at {}",
                        shadowed.scope.as_str(),
                        shadowed.path.display()
                    ),
                });
                continue;
            }

            let skill = Skill {
                name: parsed.name,
                description: parsed.description,
                short_description: parsed.short_description,
                allow_implicit_invocation: skill_allows_implicit_invocation(&dir, &mut warnings),
                dir,
                path,
                scope: root.scope,
            };
            by_name.insert(key, skill.clone());
            skills.push(skill);
        }
    }

    // Native Codex plugins retain precedence over the opt-in Claude source.
    if plugins_enabled(config) {
        let (codex_plugin_skills, codex_plugin_warnings) = discover_codex_plugin_skills();
        let (claude_plugin_skills, claude_plugin_warnings) = discover_claude_plugin_skills(config);
        for skill in codex_plugin_skills.into_iter().chain(claude_plugin_skills) {
            let key = skill.name.to_lowercase();
            if let Some(shadowed) = by_name.get(&key) {
                warnings.push(SkillWarning {
                    path: skill.path.clone(),
                    message: format!(
                        "shadowed by the {} skill at {}",
                        shadowed.scope.as_str(),
                        shadowed.path.display()
                    ),
                });
                continue;
            }
            by_name.insert(key, skill.clone());
            skills.push(skill);
        }
        warnings.extend(codex_plugin_warnings);
        warnings.extend(claude_plugin_warnings);
    }

    skills.sort_by(|a, b| a.name.cmp(&b.name));
    SkillCatalog {
        skills,
        warnings,
        roots,
    }
}

#[derive(serde::Deserialize)]
struct ClaudePluginRegistry {
    plugins: BTreeMap<String, serde_json::Value>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClaudePluginInstallation {
    scope: Option<String>,
    project_path: Option<PathBuf>,
    install_path: PathBuf,
}

fn claude_installation_priority(
    installation: &ClaudePluginInstallation,
    work_dir: &Path,
) -> Option<(u8, usize)> {
    let priority = match installation.scope.as_deref().unwrap_or("user") {
        "user" => return Some((0, 0)),
        "managed" => return Some((3, 0)),
        "project" => 1,
        "local" => 2,
        _ => return None,
    };
    let project = installation.project_path.as_ref()?;
    if !project.is_absolute() {
        return None;
    }
    let project = project.canonicalize().unwrap_or_else(|_| project.clone());
    work_dir
        .starts_with(&project)
        .then(|| (priority, project.components().count()))
}

/// Skills bundled with installed Claude Code plugins.
fn discover_claude_plugin_skills(config: &AppConfig) -> (Vec<Skill>, Vec<SkillWarning>) {
    if !config.experimental.claude_skills {
        return (Vec::new(), Vec::new());
    }
    let Some(home) = home_dir() else {
        return (Vec::new(), Vec::new());
    };
    discover_claude_plugin_skills_from(&home, config)
}

fn discover_claude_plugin_skills_from(
    home: &Path,
    config: &AppConfig,
) -> (Vec<Skill>, Vec<SkillWarning>) {
    if !config.experimental.claude_skills {
        return (Vec::new(), Vec::new());
    }
    let mut skills: Vec<Skill> = Vec::new();
    let mut warnings: Vec<SkillWarning> = Vec::new();
    let registry_path = home.join(".claude/plugins/installed_plugins.json");
    let contents = match std::fs::read_to_string(&registry_path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return (skills, warnings),
        result => result.map_err(|error| error.to_string()),
    };
    let registry = contents.and_then(|contents| {
        serde_json::from_str::<ClaudePluginRegistry>(&contents).map_err(|error| error.to_string())
    });
    let registry = match registry {
        Ok(registry) => registry,
        Err(error) => {
            warnings.push(SkillWarning {
                path: registry_path,
                message: format!("cannot read Claude plugin registry: {error}"),
            });
            return (skills, warnings);
        }
    };
    let work_dir = config
        .work_dir
        .canonicalize()
        .unwrap_or_else(|_| config.work_dir.clone());

    for (key, value) in registry.plugins {
        let Some((plugin, marketplace)) = key.rsplit_once('@') else {
            continue;
        };
        if plugin.is_empty() || marketplace.is_empty() {
            continue;
        }
        let installations: Result<Vec<ClaudePluginInstallation>, _> = if value.is_array() {
            serde_json::from_value(value)
        } else {
            serde_json::from_value(value).map(|installation| vec![installation])
        };
        let installations = match installations {
            Ok(installations) => installations,
            Err(error) => {
                warnings.push(SkillWarning {
                    path: registry_path.clone(),
                    message: format!("invalid Claude plugin `{key}` installation: {error}"),
                });
                continue;
            }
        };
        let installation = installations
            .into_iter()
            .filter_map(|installation| {
                claude_installation_priority(&installation, &work_dir)
                    .map(|priority| (priority, installation))
            })
            .max_by_key(|(priority, _)| *priority);
        let Some((_, installation)) = installation else {
            continue;
        };
        if !installation.install_path.is_absolute() {
            warnings.push(SkillWarning {
                path: registry_path.clone(),
                message: format!("Claude plugin `{key}` installPath must be absolute"),
            });
            continue;
        }
        let skills_dir = installation.install_path.join("skills");
        for entry in sorted_entries(&skills_dir) {
            let dir = skills_dir.join(&entry);
            if !is_directory(&dir) {
                continue;
            }
            let path = dir.join(SKILL_FILENAME);
            let Ok(contents) = std::fs::read_to_string(&path) else {
                continue;
            };
            match parse_skill_frontmatter(&contents, &entry) {
                Ok(parsed) => skills.push(Skill {
                    name: format!("{plugin}:{}", parsed.name),
                    description: parsed.description,
                    short_description: parsed.short_description,
                    allow_implicit_invocation: skill_allows_implicit_invocation(
                        &dir,
                        &mut warnings,
                    ),
                    dir,
                    path,
                    scope: SkillScope::Plugin,
                }),
                Err(message) => warnings.push(SkillWarning { path, message }),
            }
        }
    }

    (skills, warnings)
}

/// Lookup by name, exact first and case-insensitively after.
pub fn find_skill<'a>(catalog: &'a SkillCatalog, name: &str) -> Option<&'a Skill> {
    let wanted = name.trim();
    // Case-insensitive fallback uses full-Unicode lowercasing, matching the TS
    // `toLowerCase()` and the shadow-map key built in `discover_skills`.
    let wanted_lower = wanted.to_lowercase();
    catalog
        .skills
        .iter()
        .find(|s| s.name == wanted)
        .or_else(|| {
            catalog
                .skills
                .iter()
                .find(|s| s.name.to_lowercase() == wanted_lower)
        })
}

/// Absolute path of a file inside a skill package. Containment is checked
/// against the skill's own directory, since skills live outside work-dir.
pub fn resolve_skill_resource(skill: &Skill, resource: &str) -> Result<PathBuf, String> {
    let relative = resource.replace('\\', "/");
    let relative = relative.strip_prefix("./").unwrap_or(&relative).to_string();

    let is_drive = {
        let b = resource.as_bytes();
        b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':'
    };
    if relative.is_empty() || Path::new(resource).is_absolute() || is_drive {
        return Err(format!(
            "Resource must be a path inside the skill: {resource}"
        ));
    }
    if relative
        .split('/')
        .any(|seg| seg.is_empty() || seg == "." || seg == "..")
    {
        return Err(format!(
            "Resource must be a path inside the skill: {resource}"
        ));
    }

    let full = skill.dir.join(&relative);
    if full != skill.dir && full.strip_prefix(&skill.dir).is_err() {
        return Err(format!(
            "Resource must be a path inside the skill: {resource}"
        ));
    }
    Ok(full)
}

/// Package files other than SKILL.md, as paths to pass back as `resource`.
pub fn skill_package_files(skill: &Skill, max: usize) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();

    fn walk(dir: &Path, prefix: &str, found: &mut Vec<String>, max: usize) {
        if found.len() >= max {
            return;
        }
        for entry in sorted_entries(dir) {
            if found.len() >= max {
                return;
            }
            let child = dir.join(&entry);
            let relative = if prefix.is_empty() {
                entry.clone()
            } else {
                format!("{prefix}/{entry}")
            };
            if is_directory(&child) {
                walk(&child, &relative, found, max);
            } else if relative != SKILL_FILENAME {
                found.push(relative);
            }
        }
    }

    walk(&skill.dir, "", &mut found, max);
    found
}

/// The catalogue as the model should see it. `None` when there is nothing to
/// offer, so callers can leave the whole section out.
pub fn render_skill_catalog(catalog: &SkillCatalog) -> Option<String> {
    let entries: Vec<_> = catalog
        .skills
        .iter()
        .filter(|skill| skill.allow_implicit_invocation)
        .map(|s| format!("- {} ({}) — {}", s.name, s.scope.as_str(), s.description))
        .collect();
    (!entries.is_empty()).then(|| entries.join("\n"))
}

#[cfg(test)]
mod plugin_tests {
    use super::*;
    use crate::config::default_config;

    fn claude_config(work_dir: PathBuf) -> AppConfig {
        let mut config = default_config(work_dir);
        config.experimental.claude_skills = true;
        config
    }

    fn write_claude_skill(home: &Path, version: &str, name: &str) -> PathBuf {
        let plugin = home
            .join(".claude/plugins/cache/market/sample")
            .join(version);
        let skill = plugin.join("skills").join(name);
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(
            skill.join(SKILL_FILENAME),
            format!("---\nname: {name}\ndescription: Use {name}\n---\nBody\n"),
        )
        .unwrap();
        plugin
    }

    fn write_claude_registry(home: &Path, plugins: serde_json::Value) {
        let path = home.join(".claude/plugins/installed_plugins.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            path,
            serde_json::json!({"version": 2, "plugins": plugins}).to_string(),
        )
        .unwrap();
    }

    #[test]
    fn experimental_claude_discovery_is_opt_in() {
        let home = tempfile::tempdir().unwrap();
        let registered = write_claude_skill(home.path(), "1.0.0", "registered");
        write_claude_registry(
            home.path(),
            serde_json::json!({
                "sample@market": [{"scope":"user", "installPath":registered}]
            }),
        );
        let mut config = default_config(home.path().to_path_buf());
        assert!(
            !skill_roots(&config)
                .iter()
                .any(|root| root.path.ends_with(".claude/skills"))
        );
        let (skills, warnings) = discover_claude_plugin_skills_from(home.path(), &config);
        assert!(skills.is_empty());
        assert!(warnings.is_empty());
        config.experimental.claude_skills = true;
        assert!(
            skill_roots(&config)
                .iter()
                .any(|root| root.path.ends_with(".claude/skills"))
        );
        assert_eq!(
            discover_claude_plugin_skills_from(home.path(), &config)
                .0
                .len(),
            1
        );
    }

    #[test]
    fn claude_cached_skills_require_an_installed_registry_entry() {
        let home = tempfile::tempdir().unwrap();
        write_claude_skill(home.path(), "1.0.0", "stale");
        let config = claude_config(home.path().to_path_buf());
        let (skills, warnings) = discover_claude_plugin_skills_from(home.path(), &config);
        assert!(skills.is_empty());
        assert!(warnings.is_empty());
        write_claude_registry(home.path(), serde_json::json!({}));
        let (skills, warnings) = discover_claude_plugin_skills_from(home.path(), &config);
        assert!(skills.is_empty());
        assert!(warnings.is_empty());
    }

    #[test]
    fn claude_uses_the_registered_path_not_the_highest_cached_version() {
        let home = tempfile::tempdir().unwrap();
        let registered = write_claude_skill(home.path(), "1.0.0", "registered");
        write_claude_skill(home.path(), "2.0.0", "stale");
        write_claude_registry(
            home.path(),
            serde_json::json!({
                "sample@market": [{"scope": "user", "installPath": registered}]
            }),
        );
        let config = claude_config(home.path().to_path_buf());
        let (skills, warnings) = discover_claude_plugin_skills_from(home.path(), &config);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "sample:registered");
        assert!(skills[0].path.starts_with(registered));
    }

    #[test]
    fn claude_invalid_registry_does_not_fall_back_to_cached_skills() {
        let home = tempfile::tempdir().unwrap();
        write_claude_skill(home.path(), "1.0.0", "stale");
        let path = home.path().join(".claude/plugins/installed_plugins.json");
        let config = claude_config(home.path().to_path_buf());
        for contents in ["{", "{}", "{\"plugins\": []}"] {
            std::fs::write(&path, contents).unwrap();
            let (skills, warnings) = discover_claude_plugin_skills_from(home.path(), &config);
            assert!(skills.is_empty(), "{contents}");
            assert_eq!(warnings.len(), 1, "{contents}");
            assert_eq!(warnings[0].path, path);
        }
    }

    #[test]
    fn claude_project_installations_do_not_leak_into_other_projects() {
        let home = tempfile::tempdir().unwrap();
        let user = write_claude_skill(home.path(), "1.0.0", "user");
        let local = write_claude_skill(home.path(), "2.0.0", "local");
        let project = home.path().join("project");
        std::fs::create_dir_all(project.join("src")).unwrap();
        write_claude_registry(
            home.path(),
            serde_json::json!({
                "sample@market": [
                    {"scope": "user", "installPath": user},
                    {"scope": "local", "projectPath": project, "installPath": local}
                ]
            }),
        );
        for (cwd, expected) in [
            (project.join("src"), "sample:local"),
            (home.path().join("project-other"), "sample:user"),
        ] {
            let config = claude_config(cwd);
            let (skills, warnings) = discover_claude_plugin_skills_from(home.path(), &config);
            assert!(warnings.is_empty(), "{warnings:?}");
            assert_eq!(skills.len(), 1);
            assert_eq!(skills[0].name, expected);
        }
    }

    #[test]
    fn claude_legacy_registration_preserves_explicit_only_skills() {
        let home = tempfile::tempdir().unwrap();
        let original = write_claude_skill(home.path(), "1.0.0", "manual");
        let installed = home.path().join("custom-install");
        std::fs::rename(original, &installed).unwrap();
        std::fs::create_dir_all(installed.join("skills/manual/agents")).unwrap();
        std::fs::write(
            installed.join("skills/manual/agents/openai.yaml"),
            "policy:\n  allow_implicit_invocation: false\n",
        )
        .unwrap();
        write_claude_registry(
            home.path(),
            serde_json::json!({
                "sample@market": {"installPath": installed}
            }),
        );
        let config = claude_config(home.path().to_path_buf());
        let (skills, warnings) = discover_claude_plugin_skills_from(home.path(), &config);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "sample:manual");
        assert!(!skills[0].allow_implicit_invocation);
    }

    #[test]
    fn claude_broken_installation_does_not_hide_other_plugins_or_use_cache() {
        let home = tempfile::tempdir().unwrap();
        let registered = write_claude_skill(home.path(), "1.0.0", "registered");
        write_claude_skill(home.path(), "2.0.0", "stale");
        let config = claude_config(home.path().to_path_buf());
        for broken in [
            serde_json::json!({}),
            serde_json::json!({"installPath": "relative"}),
        ] {
            write_claude_registry(
                home.path(),
                serde_json::json!({
                    "sample@market": broken,
                    "working@market": [{"scope": "user", "installPath": registered}]
                }),
            );
            let (skills, warnings) = discover_claude_plugin_skills_from(home.path(), &config);
            assert_eq!(skills.len(), 1);
            assert_eq!(skills[0].name, "working:registered");
            assert_eq!(warnings.len(), 1);
        }
        write_claude_registry(
            home.path(),
            serde_json::json!({
                "sample@market": [{"scope": "user", "installPath": home.path().join("missing")}]
            }),
        );
        let (skills, _) = discover_claude_plugin_skills_from(home.path(), &config);
        assert!(skills.is_empty());
    }

    #[test]
    fn claude_scoped_installation_requires_an_applicable_project_path() {
        let home = tempfile::tempdir().unwrap();
        let registered = write_claude_skill(home.path(), "1.0.0", "registered");
        let config = claude_config(home.path().to_path_buf());
        for scope in ["project", "local"] {
            for project in [
                None,
                Some(PathBuf::from("relative")),
                Some(home.path().join("other")),
            ] {
                write_claude_registry(
                    home.path(),
                    serde_json::json!({
                        "sample@market": [{"scope": scope, "projectPath": project, "installPath": registered}]
                    }),
                );
                let (skills, _) = discover_claude_plugin_skills_from(home.path(), &config);
                assert!(skills.is_empty());
            }
        }
    }

    #[test]
    fn plugins_default_on_but_off_when_dirs_overridden() {
        let mut config = claude_config(std::env::temp_dir());
        assert!(
            plugins_enabled(&config),
            "default (no dirs) should scan plugins"
        );

        config.skills.dirs = Some(vec!["/some/dir".into()]);
        assert!(
            !plugins_enabled(&config),
            "explicit dirs suppress plugin scan"
        );

        config.skills.include_plugins = Some(true);
        assert!(plugins_enabled(&config), "includePlugins=true always wins");

        config.skills.include_plugins = Some(false);
        config.skills.dirs = None;
        assert!(
            !plugins_enabled(&config),
            "includePlugins=false always wins"
        );
    }
}
