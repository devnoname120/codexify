//! Discovery of skills contributed by installed OpenAI Codex plugins.
//!
//! Codex treats plugin skills as ordinary host skills, but only for plugins that
//! are enabled in its effective plugin configuration. This module mirrors the
//! local user-config/cache path: `[plugins."name@marketplace"]` selects an
//! installed cache entry, the active version is resolved using Codex's version
//! ordering, and the plugin manifest supplies the namespace and skill roots.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};

use semver::Version;
use serde::Deserialize;
use serde_json::Value as JsonValue;
use walkdir::WalkDir;

use crate::codex_config::{codex_config_path, load_codex_config};
use crate::skills::{
    MAX_SKILL_NAME_BYTES, SKILL_FILENAME, Skill, SkillScope, SkillWarning, parse_skill_frontmatter,
};

const PLUGINS_CACHE_DIR: &str = "plugins/cache";
const LOCAL_PLUGIN_VERSION: &str = "local";
const DEFAULT_SKILLS_DIR: &str = "skills";
const MIGRATED_COMMAND_SKILLS_DIR: &str = ".codex-plugin/migrated-command-skills";
const AGENT_PLUGIN_MANIFEST: &str = "plugin.json";
const AGENT_PLUGIN_SCHEMA_URI: &str = "https://agent-plugins.org/schemas/1.0.0/plugin.schema.json";
const AGENT_PLUGIN_SCHEMA_PREFIX: &str = "https://agent-plugins.org/schemas/";
const LEGACY_PLUGIN_MANIFESTS: &[&str] = &[
    ".codex-plugin/plugin.json",
    ".claude-plugin/plugin.json",
    ".cursor-plugin/plugin.json",
];
const MAX_PLUGIN_SCAN_DEPTH: usize = 6;
const MAX_PLUGIN_SCAN_ENTRIES: usize = 20_000;
const MAX_QUALIFIED_SKILL_NAME_BYTES: usize = MAX_SKILL_NAME_BYTES * 2 + 1;

#[derive(Debug, Clone, PartialEq, Eq)]
struct PluginId {
    plugin_name: String,
    marketplace_name: String,
}

impl PluginId {
    fn parse(key: &str) -> Result<Self, String> {
        let Some((plugin_name, marketplace_name)) = key.rsplit_once('@') else {
            return Err(format!(
                "invalid plugin key `{key}`; expected <plugin>@<marketplace>"
            ));
        };
        validate_plugin_segment(plugin_name, "plugin name", true)?;
        validate_plugin_segment(marketplace_name, "marketplace name", false)?;
        Ok(Self {
            plugin_name: plugin_name.to_string(),
            marketplace_name: marketplace_name.to_string(),
        })
    }
}

fn validate_plugin_segment(segment: &str, kind: &str, allow_dots: bool) -> Result<(), String> {
    if segment.is_empty() {
        return Err(format!("invalid {kind}: must not be empty"));
    }
    if allow_dots && matches!(segment, "." | "..") {
        return Err(format!("invalid {kind}: path traversal is not allowed"));
    }
    if allow_dots && (segment.starts_with('.') || segment.ends_with('.') || segment.contains(".."))
    {
        return Err(format!(
            "invalid {kind}: dots must separate non-empty name segments"
        ));
    }
    if !segment
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') || allow_dots && ch == '.')
    {
        return Err(format!("invalid {kind}: contains unsupported characters"));
    }
    Ok(())
}

fn valid_plugin_version_segment(version: &str) -> bool {
    !version.is_empty()
        && !matches!(version, "." | "..")
        && version
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | '+'))
}

fn compare_plugin_versions(left: &str, right: &str) -> Ordering {
    match (Version::parse(left), Version::parse(right)) {
        (Ok(left), Ok(right)) => left.cmp(&right),
        _ => left.cmp(right),
    }
}

fn active_plugin_version(plugin_base: &Path) -> Option<String> {
    let mut versions = std::fs::read_dir(plugin_base)
        .ok()?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let file_type = entry.file_type().ok()?;
            if !file_type.is_dir() {
                return None;
            }
            let version = entry.file_name().into_string().ok()?;
            valid_plugin_version_segment(&version).then_some(version)
        })
        .collect::<Vec<_>>();
    if versions
        .iter()
        .any(|version| version == LOCAL_PLUGIN_VERSION)
    {
        return Some(LOCAL_PLUGIN_VERSION.to_string());
    }
    versions.sort_unstable_by(|left, right| compare_plugin_versions(left, right));
    versions.pop()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ManifestFormat {
    Legacy,
    AgentPlugin,
}

#[derive(Debug)]
struct LoadedManifest {
    namespace: String,
    skill_roots: Vec<PathBuf>,
    format: ManifestFormat,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LegacyManifest {
    #[serde(default)]
    name: String,
    #[serde(default)]
    skills: Option<RawManifestPaths>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawManifestPaths {
    Path(String),
    Paths(Vec<String>),
    Invalid(JsonValue),
}

#[derive(Debug, Deserialize)]
struct AgentPluginManifest {
    #[serde(rename = "$schema")]
    schema: String,
    name: String,
}

#[derive(Debug, Deserialize)]
struct CodexPluginConfig {
    #[serde(default = "default_true")]
    enabled: bool,
}

fn default_true() -> bool {
    true
}

fn agent_plugin_schema_status(contents: &str) -> Option<&'static str> {
    let value: JsonValue = serde_json::from_str(contents).ok()?;
    let schema = value.get("$schema")?.as_str()?;
    if schema == AGENT_PLUGIN_SCHEMA_URI {
        Some("supported")
    } else if schema.starts_with(AGENT_PLUGIN_SCHEMA_PREFIX) {
        Some("unsupported")
    } else {
        Some("unrelated")
    }
}

fn regular_file(path: &Path) -> Result<Option<()>, String> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.file_type().is_file() => {
            Err(format!("{} is not a regular file", path.display()))
        }
        Ok(_) => Ok(Some(())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("cannot inspect {}: {error}", path.display())),
    }
}

fn find_plugin_manifest(plugin_root: &Path) -> Result<Option<(PathBuf, ManifestFormat)>, String> {
    let agent_manifest = plugin_root.join(AGENT_PLUGIN_MANIFEST);
    if regular_file(&agent_manifest)?.is_some() {
        let contents = std::fs::read_to_string(&agent_manifest)
            .map_err(|error| format!("cannot read {}: {error}", agent_manifest.display()))?;
        if matches!(
            agent_plugin_schema_status(&contents),
            Some("supported" | "unsupported")
        ) {
            return Ok(Some((agent_manifest, ManifestFormat::AgentPlugin)));
        }
    }

    for relative in LEGACY_PLUGIN_MANIFESTS {
        let manifest = plugin_root.join(relative);
        let Some(parent) = manifest.parent() else {
            continue;
        };
        match std::fs::symlink_metadata(parent) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() => {
                return Err(format!("{} is not a regular directory", parent.display()));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(format!("cannot inspect {}: {error}", parent.display())),
        }
        if regular_file(&manifest)?.is_some() {
            return Ok(Some((manifest, ManifestFormat::Legacy)));
        }
    }
    Ok(None)
}

fn validate_agent_plugin_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_SKILL_NAME_BYTES
        && !name.contains("--")
        && !name.contains("..")
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || b".-".contains(&byte))
        && name
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        && name
            .as_bytes()
            .last()
            .is_some_and(u8::is_ascii_alphanumeric)
}

fn resolve_manifest_path(plugin_root: &Path, raw: &str) -> Option<PathBuf> {
    let relative = raw.strip_prefix("./")?;
    if relative.is_empty() {
        return None;
    }
    let relative_path = Path::new(relative);
    if relative_path.is_absolute()
        || relative_path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return None;
    }
    Some(plugin_root.join(relative_path))
}

fn load_plugin_manifest(plugin_root: &Path) -> Result<LoadedManifest, String> {
    let Some((manifest_path, format)) = find_plugin_manifest(plugin_root)? else {
        return Err("missing plugin manifest".to_string());
    };
    let contents = std::fs::read_to_string(&manifest_path)
        .map_err(|error| format!("cannot read {}: {error}", manifest_path.display()))?;

    match format {
        ManifestFormat::AgentPlugin => {
            let manifest: AgentPluginManifest = serde_json::from_str(&contents)
                .map_err(|error| format!("invalid Agent Plugin manifest: {error}"))?;
            if manifest.schema != AGENT_PLUGIN_SCHEMA_URI {
                return Err(format!(
                    "unsupported Agent Plugins schema `{}`",
                    manifest.schema
                ));
            }
            if !validate_agent_plugin_name(&manifest.name) {
                return Err(format!("invalid Agent Plugins name `{}`", manifest.name));
            }
            Ok(LoadedManifest {
                namespace: manifest.name,
                skill_roots: vec![plugin_root.join(DEFAULT_SKILLS_DIR)],
                format,
            })
        }
        ManifestFormat::Legacy => {
            let manifest: LegacyManifest = serde_json::from_str(&contents)
                .map_err(|error| format!("invalid plugin manifest: {error}"))?;
            let namespace = if manifest.name.trim().is_empty() {
                plugin_root
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or_default()
                    .to_string()
            } else {
                manifest.name
            };
            if namespace.is_empty() {
                return Err("invalid plugin namespace".to_string());
            }

            let mut skill_roots = match manifest.skills {
                Some(RawManifestPaths::Path(path)) => resolve_manifest_path(plugin_root, &path)
                    .into_iter()
                    .collect(),
                Some(RawManifestPaths::Paths(paths)) => paths
                    .iter()
                    .filter_map(|path| resolve_manifest_path(plugin_root, path))
                    .collect(),
                Some(RawManifestPaths::Invalid(value)) => {
                    let _ = value;
                    Vec::new()
                }
                None => Vec::new(),
            };
            if skill_roots.is_empty() {
                let default_root = plugin_root.join(DEFAULT_SKILLS_DIR);
                if default_root.is_dir() {
                    skill_roots.push(default_root);
                }
            }
            let migrated_commands = plugin_root.join(MIGRATED_COMMAND_SKILLS_DIR);
            if migrated_commands.is_dir() {
                skill_roots.push(migrated_commands);
            }
            skill_roots.sort_unstable();
            skill_roots.dedup();
            Ok(LoadedManifest {
                namespace,
                skill_roots,
                format,
            })
        }
    }
}

#[derive(Debug, Clone)]
enum SkillRuleSelector {
    Name(String),
    Path(PathBuf),
}

#[derive(Debug, Clone)]
struct SkillRule {
    selector: SkillRuleSelector,
    enabled: bool,
}

fn configured_skill_rules(root: &toml::Table) -> Vec<SkillRule> {
    let Some(entries) = root
        .get("skills")
        .and_then(toml::Value::as_table)
        .and_then(|skills| skills.get("config"))
        .and_then(toml::Value::as_array)
    else {
        return Vec::new();
    };

    let mut rules = Vec::new();
    for value in entries {
        let Some(table) = value.as_table() else {
            continue;
        };
        let Some(enabled) = table.get("enabled").and_then(toml::Value::as_bool) else {
            continue;
        };
        let path = table.get("path").and_then(toml::Value::as_str);
        let name = table
            .get("name")
            .and_then(toml::Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty());
        let selector = match (path, name) {
            (Some(path), None) => {
                let path = PathBuf::from(path);
                if !path.is_absolute() {
                    continue;
                }
                SkillRuleSelector::Path(path.canonicalize().unwrap_or(path))
            }
            (None, Some(name)) => SkillRuleSelector::Name(name.to_string()),
            _ => continue,
        };
        rules.push(SkillRule { selector, enabled });
    }
    rules
}

fn skill_enabled(skill: &Skill, rules: &[SkillRule]) -> bool {
    let canonical_path = skill
        .path
        .canonicalize()
        .unwrap_or_else(|_| skill.path.clone());
    let mut enabled = true;
    for rule in rules {
        let matches = match &rule.selector {
            SkillRuleSelector::Name(name) => name == &skill.name,
            SkillRuleSelector::Path(path) => path == &canonical_path,
        };
        if matches {
            enabled = rule.enabled;
        }
    }
    enabled
}

fn plugin_feature_enabled(root: &toml::Table) -> bool {
    root.get("features")
        .and_then(toml::Value::as_table)
        .and_then(|features| features.get("plugins"))
        .and_then(toml::Value::as_bool)
        .unwrap_or(true)
}

fn configured_enabled_plugins(root: &toml::Table) -> Vec<(String, PluginId)> {
    let Some(plugins) = root.get("plugins") else {
        return Vec::new();
    };
    let Ok(plugins) = plugins
        .clone()
        .try_into::<HashMap<String, CodexPluginConfig>>()
    else {
        return Vec::new();
    };
    let mut configured = Vec::new();
    for (key, value) in plugins {
        if !value.enabled {
            continue;
        }
        if let Ok(plugin_id) = PluginId::parse(&key) {
            configured.push((key, plugin_id));
        }
    }
    configured.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    configured
}

fn is_hidden_directory(entry: &walkdir::DirEntry) -> bool {
    entry.depth() > 0
        && entry.file_type().is_dir()
        && entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with('.'))
}

fn discover_root_skills(
    root: &Path,
    namespace: &str,
    format: ManifestFormat,
    warnings: &mut Vec<SkillWarning>,
) -> Vec<Skill> {
    let max_depth = match format {
        ManifestFormat::Legacy => MAX_PLUGIN_SCAN_DEPTH,
        ManifestFormat::AgentPlugin => 2,
    };
    let mut skills = Vec::new();
    let walker = WalkDir::new(root)
        .follow_links(true)
        .max_depth(max_depth)
        .into_iter()
        .filter_entry(|entry| !is_hidden_directory(entry));

    for entry in walker.take(MAX_PLUGIN_SCAN_ENTRIES) {
        let Ok(entry) = entry else {
            continue;
        };
        if !entry.file_type().is_file() || entry.file_name() != SKILL_FILENAME {
            continue;
        }
        let path = entry.into_path();
        if format == ManifestFormat::AgentPlugin
            && path
                .parent()
                .and_then(Path::parent)
                .is_none_or(|parent| parent != root)
        {
            continue;
        }
        let Some(dir) = path.parent().map(Path::to_path_buf) else {
            continue;
        };
        let Some(default_name) = dir.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let contents = match std::fs::read_to_string(&path) {
            Ok(contents) => contents,
            Err(_) => continue,
        };
        match parse_skill_frontmatter(&contents, default_name) {
            Ok(parsed) => {
                let name = format!("{namespace}:{}", parsed.name);
                if name.len() > MAX_QUALIFIED_SKILL_NAME_BYTES {
                    warnings.push(SkillWarning {
                        path,
                        message: format!(
                            "invalid qualified name: longer than {MAX_QUALIFIED_SKILL_NAME_BYTES} bytes"
                        ),
                    });
                    continue;
                }
                skills.push(Skill {
                    name,
                    description: parsed.description,
                    short_description: parsed.short_description,
                    dir,
                    path,
                    scope: SkillScope::Plugin,
                });
            }
            Err(message) => warnings.push(SkillWarning { path, message }),
        }
    }
    skills
}

pub(crate) fn discover_codex_plugin_skills() -> (Vec<Skill>, Vec<SkillWarning>) {
    let config_path = match codex_config_path() {
        Ok(path) => path,
        Err(_) => return (Vec::new(), Vec::new()),
    };
    let Some(codex_home) = config_path.parent() else {
        return (Vec::new(), Vec::new());
    };
    let config = match load_codex_config(&config_path) {
        Ok(Some(config)) => config,
        Ok(None) => return (Vec::new(), Vec::new()),
        Err(message) => {
            return (
                Vec::new(),
                vec![SkillWarning {
                    path: config_path,
                    message,
                }],
            );
        }
    };
    discover_codex_plugin_skills_from(codex_home, &config)
}

fn discover_codex_plugin_skills_from(
    codex_home: &Path,
    config: &toml::Table,
) -> (Vec<Skill>, Vec<SkillWarning>) {
    if !plugin_feature_enabled(config) {
        return (Vec::new(), Vec::new());
    }

    let rules = configured_skill_rules(config);
    let cache = codex_home.join(PLUGINS_CACHE_DIR);
    let mut skills = Vec::new();
    let mut warnings = Vec::new();
    let mut seen_paths = HashSet::new();

    for (plugin_key, plugin_id) in configured_enabled_plugins(config) {
        let plugin_base = cache
            .join(&plugin_id.marketplace_name)
            .join(&plugin_id.plugin_name);
        let Some(version) = active_plugin_version(&plugin_base) else {
            continue;
        };
        let plugin_root = plugin_base.join(version);
        let manifest = match load_plugin_manifest(&plugin_root) {
            Ok(manifest) => manifest,
            Err(message) => {
                warnings.push(SkillWarning {
                    path: plugin_root,
                    message: format!("failed to load Codex plugin `{plugin_key}`: {message}"),
                });
                continue;
            }
        };
        for root in manifest.skill_roots {
            let root_key = root.canonicalize().unwrap_or_else(|_| root.clone());
            if !seen_paths.insert((plugin_key.clone(), root_key)) {
                continue;
            }
            for skill in
                discover_root_skills(&root, &manifest.namespace, manifest.format, &mut warnings)
            {
                if skill_enabled(&skill, &rules) {
                    skills.push(skill);
                }
            }
        }
    }

    skills.sort_by(|left, right| {
        left.name
            .cmp(&right.name)
            .then_with(|| left.path.cmp(&right.path))
    });
    (skills, warnings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn parse_config(contents: &str) -> toml::Table {
        let value: toml::Value = toml::from_str(contents).unwrap();
        value.as_table().unwrap().clone()
    }

    fn write_skill(path: &Path, name: &str, description: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            path,
            format!("---\nname: {name}\ndescription: {description}\n---\n\n# Body\n"),
        )
        .unwrap();
    }

    fn write_legacy_plugin(
        home: &Path,
        marketplace: &str,
        plugin: &str,
        version: &str,
        manifest: &str,
    ) -> PathBuf {
        let root = home
            .join(PLUGINS_CACHE_DIR)
            .join(marketplace)
            .join(plugin)
            .join(version);
        fs::create_dir_all(root.join(".codex-plugin")).unwrap();
        fs::write(root.join(".codex-plugin/plugin.json"), manifest).unwrap();
        root
    }

    #[test]
    fn active_version_matches_codex_semver_and_local_precedence() {
        let temp = tempfile::tempdir().unwrap();
        for version in ["1.2.0", "1.10.0", "1.9.9"] {
            fs::create_dir_all(temp.path().join(version)).unwrap();
        }
        assert_eq!(
            active_plugin_version(temp.path()).as_deref(),
            Some("1.10.0")
        );
        fs::create_dir_all(temp.path().join("local")).unwrap();
        assert_eq!(active_plugin_version(temp.path()).as_deref(), Some("local"));
    }

    #[test]
    fn discovers_enabled_legacy_plugin_custom_roots_recursively() {
        let home = tempfile::tempdir().unwrap();
        let root = write_legacy_plugin(
            home.path(),
            "market",
            "sample",
            "1.0.0",
            r#"{"name":"acme","skills":"./custom-skills"}"#,
        );
        write_skill(
            &root.join("custom-skills/nested/search/SKILL.md"),
            "search",
            "Search the project",
        );
        let config = parse_config(
            r#"
[plugins."sample@market"]
enabled = true
"#,
        );
        let (skills, warnings) = discover_codex_plugin_skills_from(home.path(), &config);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "acme:search");
    }

    #[test]
    fn agent_plugin_uses_conventional_root_and_direct_children_only() {
        let home = tempfile::tempdir().unwrap();
        let root = home
            .path()
            .join(PLUGINS_CACHE_DIR)
            .join("market")
            .join("portable")
            .join("1.0.0");
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("plugin.json"),
            format!(r#"{{"$schema":"{AGENT_PLUGIN_SCHEMA_URI}","name":"portable"}}"#),
        )
        .unwrap();
        write_skill(
            &root.join("skills/direct/SKILL.md"),
            "direct",
            "Direct child",
        );
        write_skill(
            &root.join("skills/nested/deep/SKILL.md"),
            "deep",
            "Nested child",
        );
        let config = parse_config(
            r#"
[plugins."portable@market"]
enabled = true
"#,
        );
        let (skills, warnings) = discover_codex_plugin_skills_from(home.path(), &config);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(
            skills
                .iter()
                .map(|skill| skill.name.as_str())
                .collect::<Vec<_>>(),
            ["portable:direct"]
        );
    }

    #[test]
    fn disabled_plugins_and_disabled_feature_are_not_exposed() {
        let home = tempfile::tempdir().unwrap();
        let root = write_legacy_plugin(
            home.path(),
            "market",
            "sample",
            "1.0.0",
            r#"{"name":"sample"}"#,
        );
        write_skill(&root.join("skills/demo/SKILL.md"), "demo", "Demo");

        let disabled_plugin = parse_config(
            r#"
[plugins."sample@market"]
enabled = false
"#,
        );
        assert!(
            discover_codex_plugin_skills_from(home.path(), &disabled_plugin)
                .0
                .is_empty()
        );

        let disabled_feature = parse_config(
            r#"
[features]
plugins = false
[plugins."sample@market"]
enabled = true
"#,
        );
        assert!(
            discover_codex_plugin_skills_from(home.path(), &disabled_feature)
                .0
                .is_empty()
        );
    }

    #[test]
    fn skill_config_rules_match_qualified_name_and_canonical_path() {
        let home = tempfile::tempdir().unwrap();
        let root = write_legacy_plugin(
            home.path(),
            "market",
            "sample",
            "1.0.0",
            r#"{"name":"sample"}"#,
        );
        let one = root.join("skills/one/SKILL.md");
        let two = root.join("skills/two/SKILL.md");
        write_skill(&one, "one", "One");
        write_skill(&two, "two", "Two");
        let config = parse_config(&format!(
            r#"
[plugins."sample@market"]
enabled = true

[[skills.config]]
name = "sample:one"
enabled = false

[[skills.config]]
path = {two:?}
enabled = false
"#
        ));
        let (skills, warnings) = discover_codex_plugin_skills_from(home.path(), &config);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(skills.is_empty());
    }

    #[test]
    fn legacy_default_and_migrated_command_roots_are_both_loaded() {
        let home = tempfile::tempdir().unwrap();
        let root = write_legacy_plugin(
            home.path(),
            "market",
            "sample",
            "1.0.0",
            r#"{"name":"sample"}"#,
        );
        write_skill(&root.join("skills/normal/SKILL.md"), "normal", "Normal");
        write_skill(
            &root
                .join(MIGRATED_COMMAND_SKILLS_DIR)
                .join("command/SKILL.md"),
            "command",
            "Migrated command",
        );
        let config = parse_config(
            r#"
[plugins."sample@market"]
enabled = true
"#,
        );
        let (skills, warnings) = discover_codex_plugin_skills_from(home.path(), &config);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(
            skills
                .iter()
                .map(|skill| skill.name.as_str())
                .collect::<Vec<_>>(),
            ["sample:command", "sample:normal"]
        );
    }

    #[test]
    fn manifest_namespace_not_config_key_namespaces_skills() {
        let home = tempfile::tempdir().unwrap();
        let root = write_legacy_plugin(
            home.path(),
            "market",
            "cache-name",
            "1.0.0",
            r#"{"name":"manifest-name"}"#,
        );
        write_skill(&root.join("skills/demo/SKILL.md"), "demo", "Demo");
        let config = parse_config(
            r#"
[plugins."cache-name@market"]
enabled = true
"#,
        );
        let (skills, _) = discover_codex_plugin_skills_from(home.path(), &config);
        assert_eq!(skills[0].name, "manifest-name:demo");
    }
}
