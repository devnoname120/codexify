use std::fs::{self, File};
use std::io::{self, BufReader, Read, Write};
use std::path::Path;

use anyhow::{Context, bail};
use serde_json::{Map, Value, json};
use tempfile::NamedTempFile;

use crate::util::home_dir;

const LEGACY_HOME_DIR: &str = ".codex-free";
const CURRENT_HOME_DIR: &str = ".codexify";
const LEGACY_CONFIG_FILE: &str = "codex.config.json";
const CURRENT_CONFIG_FILE: &str = "codexify.config.json";

#[derive(Debug, Default)]
pub struct LegacyMigrationOutcome {
    pub found: bool,
    pub config_fields_added: usize,
    pub config_conflicts: usize,
    pub moved_entries: usize,
    pub warnings: Vec<String>,
    pub legacy_root_remaining: bool,
}

pub fn migrate_default_home() -> anyhow::Result<LegacyMigrationOutcome> {
    let home =
        home_dir().context("cannot locate the user's home directory for legacy migration")?;
    migrate_legacy_state(&home)
}

pub fn migrate_legacy_state(home: &Path) -> anyhow::Result<LegacyMigrationOutcome> {
    let legacy_root = home.join(LEGACY_HOME_DIR);
    let current_root = home.join(CURRENT_HOME_DIR);
    let mut outcome = LegacyMigrationOutcome::default();

    match fs::symlink_metadata(&legacy_root) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                bail!(
                    "legacy state root is a symlink and will not be migrated automatically: {}",
                    legacy_root.display()
                );
            }
            if !metadata.is_dir() {
                bail!(
                    "legacy state root is not a directory: {}",
                    legacy_root.display()
                );
            }
            outcome.found = true;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(outcome),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("inspect legacy state root {}", legacy_root.display()));
        }
    }

    prepare_current_root(&current_root)?;

    migrate_config(&legacy_root, &current_root, &mut outcome)?;

    let entries = fs::read_dir(&legacy_root)
        .with_context(|| format!("read legacy state root {}", legacy_root.display()))?;
    for entry in entries {
        let entry = entry.with_context(|| format!("read entry in {}", legacy_root.display()))?;
        if entry.file_name() == LEGACY_CONFIG_FILE {
            continue;
        }
        let destination = current_root.join(entry.file_name());
        merge_path(&entry.path(), &destination, &mut outcome)?;
    }

    remove_legacy_binary(&current_root.join("bin").join("codex-free"), &mut outcome)?;
    remove_legacy_binary(
        &current_root.join("bin").join("codex-free.exe"),
        &mut outcome,
    )?;

    remove_dir_if_empty(&legacy_root)?;
    outcome.legacy_root_remaining = legacy_root.exists();
    if outcome.legacy_root_remaining {
        outcome.warnings.push(format!(
            "some legacy files remain under {} because Codexify already had conflicting files",
            legacy_root.display()
        ));
    }

    Ok(outcome)
}

fn migrate_config(
    legacy_root: &Path,
    current_root: &Path,
    outcome: &mut LegacyMigrationOutcome,
) -> anyhow::Result<()> {
    let legacy_path = legacy_root.join(LEGACY_CONFIG_FILE);
    let current_path = current_root.join(CURRENT_CONFIG_FILE);
    let legacy_permissions = match fs::symlink_metadata(&legacy_path) {
        Ok(_) => regular_file_permissions(&legacy_path, "legacy Codex Free config")?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| {
                format!("inspect legacy Codex Free config {}", legacy_path.display())
            });
        }
    };
    let mut legacy = read_json_object(&legacy_path, "legacy Codex Free config")?;
    let defaults = legacy_defaults();
    legacy.retain(|key, _| defaults.get(key).is_some());
    let mut migrated = strip_defaults(&Value::Object(legacy), Some(&defaults))
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    strip_effective_defaults(&mut migrated);
    // `allowedCommands` existed only for the removed run_command tool. Never
    // carry a customized legacy value into Codexify now that the field has no
    // runtime meaning.
    migrated.remove("allowedCommands");

    if let Some(review) = migrated.remove("review") {
        match migrated.entry("diff".to_string()) {
            serde_json::map::Entry::Vacant(entry) => {
                entry.insert(review);
            }
            serde_json::map::Entry::Occupied(mut entry) => {
                if let (Some(current), Some(legacy)) =
                    (entry.get_mut().as_object_mut(), review.as_object())
                {
                    let mut ignored_conflicts = 0;
                    merge_missing(current, legacy.clone(), &mut ignored_conflicts);
                }
            }
        }
    }
    let mut migrated_value = Value::Object(migrated);
    rewrite_legacy_paths(&mut migrated_value, legacy_root, current_root);
    migrated = migrated_value.as_object().cloned().unwrap_or_default();

    let current_permissions = match fs::symlink_metadata(&current_path) {
        Ok(_) => Some(regular_file_permissions(
            &current_path,
            "existing Codexify config",
        )?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "inspect existing Codexify config {}",
                    current_path.display()
                )
            });
        }
    };
    let mut current = if current_permissions.is_some() {
        read_json_object(&current_path, "existing Codexify config")?
    } else {
        Map::new()
    };

    let before = current.clone();
    merge_missing(&mut current, migrated, &mut outcome.config_conflicts);
    outcome.config_fields_added = count_added_leaves(&before, &current);

    if current != before || (!current_path.exists() && !current.is_empty()) {
        write_json_object(
            &current_path,
            &current,
            current_permissions.or(Some(legacy_permissions)),
        )?;
    }

    fs::remove_file(&legacy_path)
        .with_context(|| format!("remove migrated legacy config {}", legacy_path.display()))?;
    Ok(())
}

fn prepare_current_root(path: &Path) -> anyhow::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => bail!(
            "Codexify state root is a symlink and will not be modified automatically: {}",
            path.display()
        ),
        Ok(metadata) if metadata.is_dir() => Ok(()),
        Ok(_) => bail!("Codexify state root is not a directory: {}", path.display()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => fs::create_dir(path)
            .with_context(|| format!("create Codexify state root {}", path.display())),
        Err(error) => {
            Err(error).with_context(|| format!("inspect Codexify state root {}", path.display()))
        }
    }
}

fn regular_file_permissions(path: &Path, label: &str) -> anyhow::Result<fs::Permissions> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect {label} {}", path.display()))?;
    if metadata.file_type().is_symlink() {
        bail!(
            "{label} is a symlink and will not be migrated: {}",
            path.display()
        );
    }
    if !metadata.is_file() {
        bail!("{label} is not a regular file: {}", path.display());
    }
    Ok(metadata.permissions())
}

fn legacy_defaults() -> Value {
    // This is the effective config-default snapshot from the final pre-rename
    // Codex Free main commit (cb487c744e2220909548ec112d3fe2a5e26cb2f1).
    json!({
        "apiKey": null,
        "conversationAuthToken": null,
        "port": 3000,
        "multiProject": false,
        "projectCloneDir": null,
        "worktrees": {
            "mode": "auto",
            "root": null,
            "upstreamRefreshMode": "never",
            "autoCleanupEnabled": true,
            "keepCount": 15,
            "allowSetupScript": false
        },
        "allowedCommands": ["bun", "npm", "npx", "node", "git", "python", "pip", "cargo", "make"],
        "tree": {
            "defaultDepth": 3,
            "ignore": ["node_modules", ".git", "dist", ".next", "__pycache__"]
        },
        "command": {
            "defaultTimeout": 30000,
            "maxTimeout": 120000
        },
        "exec": {
            "mode": "allowlist",
            "extraAllowedCommands": ["ls", "cat", "grep", "find", "head", "tail", "wc", "echo", "pwd", "which", "rg", "sed", "awk", "sort", "uniq", "diff", "true", "false"],
            "maxSessions": 8,
            "defaultShell": null,
            "idleTimeoutMs": 300000
        },
        "projectDoc": {
            "maxBytes": null,
            "fallbackFilenames": null,
            "rootMarkers": null
        },
        "output": {
            "maxToolOutputTokens": null,
            "maxFileLines": null,
            "maxFileBytes": null,
            "maxEntries": null,
            "maxTreeNodes": null
        },
        "review": {
            "maxPatchBytes": 4194304
        },
        "artifactIngress": {
            "enabled": true,
            "maxFileBytes": 104857600,
            "requestTimeoutMs": 120000,
            "idleTimeoutMs": 30000,
            "maxRedirects": 3,
            "maxConcurrentDownloads": 2,
            "allowedHosts": ["*"]
        },
        "artifactEgress": {
            "enabled": true,
            "maxFileBytes": 104857600,
            "maxCachedBytes": 268435456,
            "maxReferences": 64,
            "referenceTtlMs": 300000
        },
        "memory": {
            "enabled": null,
            "dir": null,
            "maxBytes": null
        },
        "skills": {
            "enabled": null,
            "dirs": null,
            "includePlugins": null
        },
        "ignore": {
            "useGitignore": null,
            "useDefaultPatterns": null,
            "customPatterns": null
        },
        "toolLogging": {
            "mode": "off",
            "level": "info",
            "maxRequestBytes": 2048,
            "maxResponseBytes": 4096,
            "redactEnv": []
        },
        "audit": {
            "logFile": null,
            "includeCommandPreview": false,
            "commandPreviewMaxBytes": 512,
            "redactEnv": []
        },
        "codexMcp": {
            "enabled": true,
            "useCli": true,
            "cliPath": null
        },
        "projectCatalog": {
            "codexConfig": {
                "enabled": true,
                "trustedOnly": true
            },
            "entries": []
        },
        "allowedHosts": [],
        "openaiTunnel": null,
        "mcpServers": {}
    })
}

fn strip_defaults(value: &Value, default: Option<&Value>) -> Option<Value> {
    match value {
        Value::Object(object) => {
            let defaults = default.and_then(Value::as_object);
            let mut retained = Map::new();
            for (key, value) in object {
                let default = defaults.and_then(|defaults| defaults.get(key));
                if let Some(value) = strip_defaults(value, default) {
                    retained.insert(key.clone(), value);
                }
            }
            (!retained.is_empty()).then_some(Value::Object(retained))
        }
        _ if default == Some(value) => None,
        _ => Some(value.clone()),
    }
}

fn strip_effective_defaults(config: &mut Map<String, Value>) {
    for (path, value) in [
        (&["projectDoc", "maxBytes"][..], json!(32_768)),
        (&["projectDoc", "fallbackFilenames"][..], json!([])),
        (&["projectDoc", "rootMarkers"][..], json!([".git"])),
        (&["output", "maxToolOutputTokens"][..], json!(10_000)),
        (&["output", "maxFileLines"][..], json!(1_000)),
        (&["output", "maxFileBytes"][..], json!(131_072)),
        (&["output", "maxEntries"][..], json!(500)),
        (&["output", "maxTreeNodes"][..], json!(1_000)),
        (&["memory", "enabled"][..], json!(true)),
        (&["memory", "maxBytes"][..], json!(16_384)),
        (&["skills", "enabled"][..], json!(true)),
        (&["ignore", "useGitignore"][..], json!(true)),
        (&["ignore", "useDefaultPatterns"][..], json!(true)),
        (&["ignore", "customPatterns"][..], json!([])),
    ] {
        remove_nested_default(config, path, &value);
    }

    let configured_skill_dirs = config
        .get("skills")
        .and_then(Value::as_object)
        .is_some_and(|skills| skills.contains_key("dirs"));
    remove_nested_default(
        config,
        &["skills", "includePlugins"],
        &Value::Bool(!configured_skill_dirs),
    );
}

fn remove_nested_default(object: &mut Map<String, Value>, path: &[&str], expected: &Value) {
    let Some((head, tail)) = path.split_first() else {
        return;
    };
    if tail.is_empty() {
        if object.get(*head) == Some(expected) {
            object.remove(*head);
        }
        return;
    }

    let remove_parent = object
        .get_mut(*head)
        .and_then(Value::as_object_mut)
        .is_some_and(|child| {
            remove_nested_default(child, tail, expected);
            child.is_empty()
        });
    if remove_parent {
        object.remove(*head);
    }
}

fn rewrite_legacy_paths(value: &mut Value, legacy_root: &Path, current_root: &Path) {
    match value {
        Value::Object(object) => {
            for value in object.values_mut() {
                rewrite_legacy_paths(value, legacy_root, current_root);
            }
        }
        Value::Array(array) => {
            for value in array {
                rewrite_legacy_paths(value, legacy_root, current_root);
            }
        }
        Value::String(string) => {
            if let Some(rewritten) = rewrite_path_string(string, legacy_root, current_root) {
                *string = rewritten;
            }
        }
        _ => {}
    }
}

fn rewrite_path_string(value: &str, legacy_root: &Path, current_root: &Path) -> Option<String> {
    if let Some(path) = value.strip_prefix("file:") {
        return rewrite_plain_path(path, legacy_root, current_root)
            .map(|path| format!("file:{path}"));
    }
    rewrite_plain_path(value, legacy_root, current_root)
}

fn rewrite_plain_path(value: &str, legacy_root: &Path, current_root: &Path) -> Option<String> {
    let suffix = Path::new(value).strip_prefix(legacy_root).ok()?;
    Some(current_root.join(suffix).to_string_lossy().into_owned())
}

fn read_json_object(path: &Path, label: &str) -> anyhow::Result<Map<String, Value>> {
    let text =
        fs::read_to_string(path).with_context(|| format!("read {label} {}", path.display()))?;
    serde_json::from_str::<Value>(&text)
        .with_context(|| format!("parse {label} {}", path.display()))?
        .as_object()
        .cloned()
        .with_context(|| format!("{label} must contain a JSON object: {}", path.display()))
}

fn write_json_object(
    path: &Path,
    object: &Map<String, Value>,
    permissions: Option<fs::Permissions>,
) -> anyhow::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)
        .with_context(|| format!("create config directory {}", parent.display()))?;
    let mut bytes = serde_json::to_vec_pretty(&Value::Object(object.clone()))
        .context("serialize migrated Codexify config")?;
    bytes.push(b'\n');
    let mut temp = NamedTempFile::new_in(parent)
        .with_context(|| format!("create temporary config beside {}", path.display()))?;
    temp.write_all(&bytes)
        .with_context(|| format!("write temporary migrated config for {}", path.display()))?;
    temp.as_file()
        .sync_all()
        .with_context(|| format!("sync temporary migrated config for {}", path.display()))?;
    if let Some(permissions) = permissions {
        fs::set_permissions(temp.path(), permissions).with_context(|| {
            format!(
                "preserve config permissions before replacing {}",
                path.display()
            )
        })?;
    }
    persist_replacing(temp, path)
        .with_context(|| format!("replace migrated config {}", path.display()))?;
    Ok(())
}

fn persist_replacing(temp: NamedTempFile, path: &Path) -> io::Result<()> {
    match temp.persist(path) {
        Ok(_) => Ok(()),
        Err(error) => {
            #[cfg(windows)]
            {
                let temp = error.file;
                if fs::symlink_metadata(path).is_ok() {
                    fs::remove_file(path)?;
                    return temp.persist(path).map(|_| ()).map_err(|error| error.error);
                }
                Err(error.error)
            }
            #[cfg(not(windows))]
            {
                Err(error.error)
            }
        }
    }
}

fn merge_missing(
    current: &mut Map<String, Value>,
    legacy: Map<String, Value>,
    conflicts: &mut usize,
) {
    for (key, legacy_value) in legacy {
        match current.get_mut(&key) {
            None => {
                current.insert(key, legacy_value);
            }
            Some(current_value) => {
                if let (Some(current_object), Some(legacy_object)) =
                    (current_value.as_object_mut(), legacy_value.as_object())
                {
                    merge_missing(current_object, legacy_object.clone(), conflicts);
                } else if current_value != &legacy_value {
                    *conflicts += 1;
                }
            }
        }
    }
}

fn count_added_leaves(before: &Map<String, Value>, after: &Map<String, Value>) -> usize {
    after
        .iter()
        .map(|(key, value)| match before.get(key) {
            None => leaf_count(value),
            Some(before) => count_added_value(before, value),
        })
        .sum()
}

fn count_added_value(before: &Value, after: &Value) -> usize {
    match (before.as_object(), after.as_object()) {
        (Some(before), Some(after)) => count_added_leaves(before, after),
        _ => 0,
    }
}

fn leaf_count(value: &Value) -> usize {
    match value {
        Value::Object(object) => object.values().map(leaf_count).sum(),
        _ => 1,
    }
}

fn merge_path(
    source: &Path,
    destination: &Path,
    outcome: &mut LegacyMigrationOutcome,
) -> anyhow::Result<()> {
    let source_meta = fs::symlink_metadata(source)
        .with_context(|| format!("inspect legacy state {}", source.display()))?;
    if source_meta.file_type().is_symlink() {
        outcome.warnings.push(format!(
            "kept legacy symlink {} instead of importing it into Codexify state",
            source.display()
        ));
        return Ok(());
    }

    let destination_meta = match fs::symlink_metadata(destination) {
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(error)
                .with_context(|| format!("inspect Codexify state {}", destination.display()));
        }
    };
    if destination_meta.is_none() {
        fs::rename(source, destination).with_context(|| {
            format!(
                "move legacy state {} to {}",
                source.display(),
                destination.display()
            )
        })?;
        outcome.moved_entries += 1;
        return Ok(());
    }

    let destination_meta = destination_meta.unwrap();

    if source_meta.is_dir()
        && destination_meta.is_dir()
        && !source_meta.file_type().is_symlink()
        && !destination_meta.file_type().is_symlink()
    {
        for entry in fs::read_dir(source)
            .with_context(|| format!("read legacy state directory {}", source.display()))?
        {
            let entry = entry.with_context(|| format!("read entry in {}", source.display()))?;
            merge_path(&entry.path(), &destination.join(entry.file_name()), outcome)?;
        }
        remove_dir_if_empty(source)?;
        return Ok(());
    }

    if source_meta.is_file() && destination_meta.is_file() && files_equal(source, destination)? {
        fs::remove_file(source)
            .with_context(|| format!("remove duplicate legacy state {}", source.display()))?;
        return Ok(());
    }

    outcome.warnings.push(format!(
        "kept legacy path {} because {} already exists with different contents",
        source.display(),
        destination.display()
    ));
    Ok(())
}

fn files_equal(left: &Path, right: &Path) -> anyhow::Result<bool> {
    let left_meta = fs::metadata(left).with_context(|| format!("inspect {}", left.display()))?;
    let right_meta = fs::metadata(right).with_context(|| format!("inspect {}", right.display()))?;
    if left_meta.len() != right_meta.len() {
        return Ok(false);
    }

    let mut left =
        BufReader::new(File::open(left).with_context(|| format!("open {}", left.display()))?);
    let mut right =
        BufReader::new(File::open(right).with_context(|| format!("open {}", right.display()))?);
    let mut left_buffer = [0_u8; 8192];
    let mut right_buffer = [0_u8; 8192];
    loop {
        let left_read = left.read(&mut left_buffer)?;
        let right_read = right.read(&mut right_buffer)?;
        if left_read != right_read || left_buffer[..left_read] != right_buffer[..right_read] {
            return Ok(false);
        }
        if left_read == 0 {
            return Ok(true);
        }
    }
}

fn remove_legacy_binary(path: &Path, outcome: &mut LegacyMigrationOutcome) -> anyhow::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() || metadata.file_type().is_symlink() => {
            fs::remove_file(path)
                .with_context(|| format!("remove obsolete legacy binary {}", path.display()))?;
            if let Some(parent) = path.parent() {
                remove_dir_if_empty(parent)?;
            }
        }
        Ok(_) => outcome.warnings.push(format!(
            "did not remove legacy binary path {} because it is not a file",
            path.display()
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("inspect legacy binary {}", path.display()));
        }
    }
    Ok(())
}

fn remove_dir_if_empty(path: &Path) -> anyhow::Result<()> {
    match fs::remove_dir(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::DirectoryNotEmpty => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("remove empty directory {}", path.display()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    fn write_json(path: &Path, value: Value) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
    }

    fn read_json(path: &Path) -> Value {
        serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
    }

    #[test]
    fn no_legacy_state_is_a_noop() {
        let home = TempDir::new().unwrap();
        let outcome = migrate_legacy_state(home.path()).unwrap();
        assert!(!outcome.found);
        assert!(!home.path().join(CURRENT_HOME_DIR).exists());
    }

    #[test]
    fn migrates_state_and_rebases_config_on_codexify_defaults() {
        let home = TempDir::new().unwrap();
        let legacy = home.path().join(LEGACY_HOME_DIR);
        let current = home.path().join(CURRENT_HOME_DIR);
        let credential = legacy.join("openai-tunnel/credentials/tunnel.key");
        fs::create_dir_all(credential.parent().unwrap()).unwrap();
        fs::write(&credential, "secret").unwrap();
        fs::create_dir_all(legacy.join("projects/demo")).unwrap();
        fs::write(legacy.join("projects/demo/memory.json"), "memory").unwrap();
        fs::create_dir_all(legacy.join("bin")).unwrap();
        fs::write(legacy.join("bin/codex-free"), "old binary").unwrap();

        write_json(
            &legacy.join(LEGACY_CONFIG_FILE),
            json!({
                "port": 4242,
                "multiProject": false,
                "allowedCommands": ["bun", "npm", "npx", "node", "git", "python", "pip", "cargo", "make"],
                "tree": {
                    "defaultDepth": 3,
                    "ignore": ["node_modules", ".git", "dist", ".next", "__pycache__", "vendor"]
                },
                "exec": {
                    "mode": "allowlist",
                    "maxSessions": 12,
                    "idleTimeoutMs": 300000
                },
                "review": { "maxPatchBytes": 8388608 },
                "openaiTunnel": {
                    "tunnelId": "tunnel_0123456789abcdefghijklmnopqrstuv",
                    "apiKeyRef": format!("file:{}", credential.display())
                },
                "codexMcp": { "enabled": true, "useCli": true }
            }),
        );

        let outcome = migrate_legacy_state(home.path()).unwrap();
        assert!(outcome.found);
        assert!(!outcome.legacy_root_remaining);
        assert!(current.join("projects/demo/memory.json").is_file());
        assert!(
            current
                .join("openai-tunnel/credentials/tunnel.key")
                .is_file()
        );
        assert!(!current.join("bin/codex-free").exists());

        let config = read_json(&current.join(CURRENT_CONFIG_FILE));
        assert_eq!(config["port"], 4242);
        assert_eq!(
            config["tree"],
            json!({ "ignore": ["node_modules", ".git", "dist", ".next", "__pycache__", "vendor"] })
        );
        assert_eq!(config["exec"], json!({ "maxSessions": 12 }));
        assert_eq!(config["diff"], json!({ "maxPatchBytes": 8388608 }));
        assert!(config.get("multiProject").is_none());
        assert!(config.get("allowedCommands").is_none());
        assert!(config.get("codexMcp").is_none());
        assert_eq!(
            config["openaiTunnel"]["apiKeyRef"],
            format!(
                "file:{}",
                current
                    .join("openai-tunnel/credentials/tunnel.key")
                    .display()
            )
        );
    }

    #[test]
    fn existing_codexify_config_wins_only_where_it_is_explicit() {
        let home = TempDir::new().unwrap();
        let legacy = home.path().join(LEGACY_HOME_DIR);
        let current = home.path().join(CURRENT_HOME_DIR);
        write_json(
            &legacy.join(LEGACY_CONFIG_FILE),
            json!({
                "port": 4567,
                "exec": { "maxSessions": 16, "defaultShell": "/bin/zsh" },
                "review": { "maxPatchBytes": 9000000 }
            }),
        );
        write_json(
            &current.join(CURRENT_CONFIG_FILE),
            json!({
                "port": 7777,
                "exec": { "maxSessions": 4 }
            }),
        );

        let outcome = migrate_legacy_state(home.path()).unwrap();
        assert_eq!(outcome.config_conflicts, 2);
        let config = read_json(&current.join(CURRENT_CONFIG_FILE));
        assert_eq!(config["port"], 7777);
        assert_eq!(config["exec"]["maxSessions"], 4);
        assert_eq!(config["exec"]["defaultShell"], "/bin/zsh");
        assert_eq!(config["diff"]["maxPatchBytes"], 9000000);
        assert!(!legacy.exists());
    }

    #[test]
    fn conflicting_state_is_not_overwritten_or_deleted() {
        let home = TempDir::new().unwrap();
        let legacy = home.path().join(LEGACY_HOME_DIR);
        let current = home.path().join(CURRENT_HOME_DIR);
        fs::create_dir_all(legacy.join("projects/demo")).unwrap();
        fs::create_dir_all(current.join("projects/demo")).unwrap();
        fs::write(legacy.join("projects/demo/memory.json"), "legacy").unwrap();
        fs::write(current.join("projects/demo/memory.json"), "current").unwrap();

        let outcome = migrate_legacy_state(home.path()).unwrap();
        assert!(outcome.legacy_root_remaining);
        assert_eq!(
            fs::read_to_string(current.join("projects/demo/memory.json")).unwrap(),
            "current"
        );
        assert_eq!(
            fs::read_to_string(legacy.join("projects/demo/memory.json")).unwrap(),
            "legacy"
        );
        assert!(!outcome.warnings.is_empty());
    }

    #[test]
    fn ignores_config_keys_unknown_to_the_legacy_release() {
        let home = TempDir::new().unwrap();
        let legacy = home.path().join(LEGACY_HOME_DIR);
        let current = home.path().join(CURRENT_HOME_DIR);
        write_json(
            &legacy.join(LEGACY_CONFIG_FILE),
            json!({
                "workDir": "/would-be-meaningful-only-in-codexify",
                "port": 4567
            }),
        );

        migrate_legacy_state(home.path()).unwrap();
        let config = read_json(&current.join(CURRENT_CONFIG_FILE));
        assert!(config.get("workDir").is_none());
        assert_eq!(config["port"], 4567);
    }

    #[test]
    fn strips_explicit_values_equal_to_effective_legacy_defaults() {
        let home = TempDir::new().unwrap();
        let legacy = home.path().join(LEGACY_HOME_DIR);
        let current = home.path().join(CURRENT_HOME_DIR);
        write_json(
            &legacy.join(LEGACY_CONFIG_FILE),
            json!({
                "projectDoc": {
                    "maxBytes": 32768,
                    "fallbackFilenames": [],
                    "rootMarkers": [".git"]
                },
                "output": {
                    "maxToolOutputTokens": 10000,
                    "maxFileLines": 1000,
                    "maxFileBytes": 131072,
                    "maxEntries": 500,
                    "maxTreeNodes": 1000
                },
                "memory": { "enabled": true, "maxBytes": 16384 },
                "skills": { "enabled": true, "includePlugins": true },
                "ignore": {
                    "useGitignore": true,
                    "useDefaultPatterns": true,
                    "customPatterns": []
                },
                "port": 4567
            }),
        );

        migrate_legacy_state(home.path()).unwrap();
        let config = read_json(&current.join(CURRENT_CONFIG_FILE));
        assert_eq!(config, json!({ "port": 4567 }));
    }

    #[cfg(unix)]
    #[test]
    fn refuses_symlinked_current_state_root() {
        use std::os::unix::fs::symlink;

        let home = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        fs::create_dir_all(home.path().join(LEGACY_HOME_DIR)).unwrap();
        symlink(outside.path(), home.path().join(CURRENT_HOME_DIR)).unwrap();

        let error = migrate_legacy_state(home.path()).unwrap_err().to_string();
        assert!(error.contains("state root is a symlink"));
        assert!(fs::read_dir(outside.path()).unwrap().next().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn refuses_symlinked_current_config() {
        use std::os::unix::fs::symlink;

        let home = TempDir::new().unwrap();
        let legacy = home.path().join(LEGACY_HOME_DIR);
        let current = home.path().join(CURRENT_HOME_DIR);
        let outside = home.path().join("outside.json");
        write_json(&legacy.join(LEGACY_CONFIG_FILE), json!({ "port": 4567 }));
        fs::create_dir_all(&current).unwrap();
        fs::write(&outside, "{\"port\":9999}\n").unwrap();
        symlink(&outside, current.join(CURRENT_CONFIG_FILE)).unwrap();

        let error = migrate_legacy_state(home.path()).unwrap_err().to_string();
        assert!(error.contains("existing Codexify config is a symlink"));
        assert_eq!(fs::read_to_string(&outside).unwrap(), "{\"port\":9999}\n");
    }
}
