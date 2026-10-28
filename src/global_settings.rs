//! Validated owner-only editing and maintenance of the active global JSON file.
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use clap::Parser;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::types::AppConfig;

const MAX_CONFIG_BYTES: usize = 1024 * 1024;
static EDIT_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

pub(crate) fn extras(config: &AppConfig) -> Result<crate::markdown_chat::ExtrasConfig, String> {
    let Some(path) = &config.loaded_config_path else {
        return Ok(config.markdown_chat.extras.clone());
    };
    let document: Value = serde_json::from_str(&read_raw(path)?).map_err(|e| e.to_string())?;
    document
        .pointer("/agentChat/extras")
        .filter(|v| !v.is_null())
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .map(|v| v.unwrap_or_default())
        .map_err(|e| e.to_string())
}

/// Only presentation hints are hot-reloaded. Other schema changes are refreshed
/// after the existing Restart flow has applied their runtime configuration.
pub(crate) fn advertising_config(config: &AppConfig) -> AppConfig {
    let mut effective = config.clone();
    let document = config
        .loaded_config_path
        .as_ref()
        .and_then(|p| read_raw(p).ok())
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok());
    if let Some(document) = document {
        let value = |key: &str| {
            document
                .get("experimental")
                .and_then(|e| e.get(key))
                .and_then(Value::as_bool)
        };
        effective.experimental.force_read_only_tool_annotations =
            value("forceReadOnlyToolAnnotations").unwrap_or(false);
        effective.experimental.force_destructive_tool_annotations =
            Some(value("forceDestructiveToolAnnotations").unwrap_or(false));
        effective.experimental.force_idempotent_tool_annotations =
            value("forceIdempotentToolAnnotations");
        effective.experimental.force_open_world_tool_annotations =
            Some(value("forceOpenWorldToolAnnotations").unwrap_or(false));
    }
    effective
}
fn schema_projection(document: &Value, defaults: &Value) -> Value {
    let pointers = [
        "/debug",
        "/uiWidgets",
        "/experimental/agentTickets",
        "/experimental/forceReadOnlyToolAnnotations",
        "/experimental/forceDestructiveToolAnnotations",
        "/experimental/forceIdempotentToolAnnotations",
        "/experimental/forceOpenWorldToolAnnotations",
        "/agentChat/enabled",
        "/artifactIngress/enabled",
        "/artifactIngress/maxConcurrentDownloads",
        "/artifactEgress/enabled",
        "/mcpServers",
        "/codexMcp",
    ];
    Value::Object(
        pointers
            .into_iter()
            .map(|pointer| {
                (
                    pointer.into(),
                    document
                        .pointer(pointer)
                        .filter(|v| !v.is_null())
                        .or_else(|| defaults.pointer(pointer))
                        .cloned()
                        .unwrap_or(Value::Null),
                )
            })
            .collect(),
    )
}
pub(crate) fn schema_versions(config: &AppConfig) -> Result<(String, String), String> {
    let value = snapshot(config)?;
    let mut projection = schema_projection(&value["document"], &value["defaults"]);
    let all = revision(
        serde_json::to_string(&projection)
            .map_err(|e| e.to_string())?
            .as_bytes(),
    );
    for key in [
        "/experimental/forceReadOnlyToolAnnotations",
        "/experimental/forceDestructiveToolAnnotations",
        "/experimental/forceIdempotentToolAnnotations",
        "/experimental/forceOpenWorldToolAnnotations",
    ] {
        projection.as_object_mut().unwrap().remove(key);
    }
    let structural = revision(
        serde_json::to_string(&projection)
            .map_err(|e| e.to_string())?
            .as_bytes(),
    );
    Ok((all, structural))
}
pub(crate) fn schema_revision(config: &AppConfig) -> Result<String, String> {
    schema_versions(config).map(|v| v.0)
}

/// A replacement must be created after discovery has caught up, including the
/// debounce window between saving settings and starting the browser refresh.
pub(crate) fn automation_schema_refresh_ready(config: &AppConfig) -> bool {
    if config.loaded_config_path.is_none() {
        return true;
    }
    if read_refresh_intent().is_some_and(|intent| {
        intent["targetVersion"] == env!("CARGO_PKG_VERSION") && intent["phase"] != "completed"
    }) {
        return false;
    }
    let Some(intent) = read_schema_intent(config) else {
        return true;
    };
    matches!(
        intent["phase"].as_str(),
        Some("idle" | "completed" | "cancelled")
    ) && schema_revision(config).is_ok_and(|revision| intent["observedRevision"] == revision)
}
pub(crate) fn schema_intent_path(config: &AppConfig) -> Result<PathBuf, String> {
    Ok(config
        .loaded_config_path
        .as_ref()
        .and_then(|p| p.parent())
        .ok_or("No global settings path")?
        .join("schema-refresh.json"))
}
pub(crate) fn read_schema_intent(config: &AppConfig) -> Option<Value> {
    let path = schema_intent_path(config).ok()?;
    let raw = read_raw(&path).ok()?;
    serde_json::from_str(&raw).ok()
}
static SCHEMA_INTENT_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
fn write_schema_intent_unlocked(config: &AppConfig, value: &Value) -> Result<(), String> {
    let path = schema_intent_path(config)?;
    let mut temp = tempfile::NamedTempFile::new_in(path.parent().ok_or("No state directory")?)
        .map_err(|e| e.to_string())?;
    temp.write_all(&serde_json::to_vec(value).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    if std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink() || !m.is_file()) {
        return Err("Unsafe schema refresh file".into());
    }
    temp.persist(path).map_err(|e| e.to_string())?;
    Ok(())
}
pub(crate) fn write_schema_intent(config: &AppConfig, value: &Value) -> Result<(), String> {
    let _lock = SCHEMA_INTENT_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .map_err(|_| "Schema refresh lock unavailable")?;
    write_schema_intent_unlocked(config, value)
}
pub(crate) fn update_schema_intent(
    config: &AppConfig,
    expected: &str,
    value: &Value,
) -> Result<(), String> {
    let _lock = SCHEMA_INTENT_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .map_err(|_| "Schema refresh lock unavailable")?;
    if read_schema_intent(config)
        .as_ref()
        .and_then(|v| v["refreshId"].as_str())
        == Some(expected)
    {
        write_schema_intent_unlocked(config, value)?;
    }
    Ok(())
}

pub fn turn_renewal_settings(
    path: Option<&Path>,
    fallback: &crate::agent_automation::turn_renewal::TurnRenewalConfig,
) -> Result<crate::agent_automation::turn_renewal::TurnRenewalConfig, String> {
    let Some(path) = path else {
        return Ok(fallback.clone());
    };
    let document: Value =
        serde_json::from_str(&read_raw(path)?).map_err(|_| "Cannot read turn renewal settings")?;
    let config = match document
        .pointer("/agentChat/turnRenewal")
        .filter(|v| !v.is_null())
    {
        Some(value) => serde_json::from_value(value.clone())
            .map_err(|e| format!("Invalid turn renewal settings: {e}"))?,
        None => crate::agent_automation::turn_renewal::TurnRenewalConfig::default(),
    };
    config.validate()?;
    Ok(config)
}
pub(crate) fn automatic_retries(path: Option<&Path>, fallback: bool) -> Result<bool, String> {
    let Some(path) = path else {
        return Ok(fallback);
    };
    let document: Value = serde_json::from_str(&read_raw(path)?)
        .map_err(|_| "Cannot read automatic retry settings")?;
    match document
        .pointer("/agentChat/automation/automaticRetries")
        .filter(|v| !v.is_null())
    {
        Some(value) => value
            .as_bool()
            .ok_or_else(|| "Automatic retries must be a boolean".into()),
        None => Ok(true),
    }
}

pub fn rollout_enabled(config: &AppConfig) -> Result<bool, String> {
    let Some(path) = &config.loaded_config_path else {
        return Ok(config.markdown_chat.rollout_enabled);
    };
    let document: Value = serde_json::from_str(&read_raw(path)?)
        .map_err(|_| "Cannot read progress journal settings")?;
    match document
        .pointer("/agentChat/rolloutEnabled")
        .filter(|v| !v.is_null())
    {
        Some(value) => value
            .as_bool()
            .ok_or_else(|| "Progress journal enabled must be a boolean".into()),
        None => Ok(true),
    }
}

pub fn rollout_reminder_interval(config: &AppConfig) -> Result<u64, String> {
    let Some(path) = &config.loaded_config_path else {
        return Ok(config.markdown_chat.rollout_reminder_interval);
    };
    let document: Value = serde_json::from_str(&read_raw(path)?)
        .map_err(|_| "Cannot read progress report settings")?;
    let value = document.pointer("/agentChat/rolloutReminderInterval").filter(|v|!v.is_null()).cloned()
        .unwrap_or_else(||crate::config::settings_defaults(config, &document)["agentChat"]["rolloutReminderInterval"].clone());
    value
        .as_u64()
        .filter(|v| (1..=10000).contains(v))
        .ok_or_else(|| "Progress report interval must be between 1 and 10000".into())
}

pub fn default_message_mode(
    config: &AppConfig,
) -> Result<crate::markdown_chat::MessageMode, String> {
    let Some(path) = &config.loaded_config_path else {
        return Ok(config.markdown_chat.default_message_mode);
    };
    let document: Value = serde_json::from_str(&read_raw(path)?)
        .map_err(|_| "Cannot read the default message mode from configuration")?;
    match document
        .pointer("/agentChat/defaultMessageMode")
        .filter(|value| !value.is_null())
    {
        Some(value) => serde_json::from_value(value.clone())
            .map_err(|_| "Default message mode must be steer or queue".into()),
        None => {
            let defaults = crate::config::settings_defaults(config, &document);
            serde_json::from_value(defaults["agentChat"]["defaultMessageMode"].clone())
                .map_err(|_| "Default message mode is unavailable".into())
        }
    }
}

pub fn snapshot(config: &AppConfig) -> Result<Value, String> {
    let path = config
        .loaded_config_path
        .as_ref()
        .ok_or("This instance has no global configuration file")?;
    let raw = read_raw(path)?;
    let document: Value = serde_json::from_str(&raw)
        .map_err(|error| format!("Invalid configuration JSON: {error}"))?;
    if !document.is_object() {
        return Err("Configuration must contain a JSON object".into());
    }
    let defaults = crate::config::settings_defaults(config, &document);
    Ok(
        json!({"path":path,"raw":raw,"document":document,"revision":revision(raw.as_bytes()),
        "defaults":defaults,
        "fieldSchema":serde_json::from_str::<Value>(include_str!("../codexify.config.example.json")).expect("bundled field schema"),
        "defaultLabels":{"workDir":"Required","skills.dirs":"Automatic discovery",
        "experimental.forceIdempotentToolAnnotations":"Keep tool hints"},
        "restartRequired":false}),
    )
}

fn read_raw(path: &Path) -> Result<String, String> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() || !meta.is_file() => {
            return Err("Configuration must be a regular, non-symlink file".into());
        }
        Ok(meta) if meta.len() > MAX_CONFIG_BYTES as u64 => {
            return Err("Configuration exceeds 1 MiB".into());
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok("{}\n".into()),
        Err(error) => return Err(format!("Cannot read configuration: {error}")),
    }
    std::fs::read_to_string(path).map_err(|error| format!("Cannot read configuration: {error}"))
}

fn revision(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// Read the explicitly saved connector name so owner setup takes effect immediately.
pub(crate) fn connector_name(
    path: Option<&Path>,
    fallback: &str,
) -> Result<Option<String>, String> {
    let name = match path {
        Some(path) => {
            let document: Value =
                serde_json::from_str(&read_raw(path)?).map_err(|e| e.to_string())?;
            document
                .pointer("/agentChat/automation/connectorName")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        }
        None => fallback.to_string(),
    };
    if name.len() > 128 || name.chars().any(char::is_control) {
        return Err("Connector name must contain at most 128 visible characters".into());
    }
    Ok((!name.trim().is_empty()).then_some(name))
}

pub fn save(
    config: &AppConfig,
    expected_revision: &str,
    raw: Option<String>,
    changes: Option<Map<String, Value>>,
) -> Result<Value, String> {
    let _guard = EDIT_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .map_err(|_| "Configuration editor lock is unavailable")?;
    let path = config
        .loaded_config_path
        .as_ref()
        .ok_or("This instance has no global configuration file")?;
    let before = read_raw(path)?;
    if revision(before.as_bytes()) != expected_revision {
        return Err("Configuration changed on disk; reload it before saving".into());
    }
    let mut document = match (raw, changes) {
        (Some(raw), None) => {
            if raw.len() > MAX_CONFIG_BYTES {
                return Err("Configuration exceeds 1 MiB".into());
            }
            serde_json::from_str::<Value>(&raw)
                .map_err(|error| format!("Invalid configuration JSON: {error}"))?
        }
        (None, Some(changes)) => {
            let mut document: Value =
                serde_json::from_str(&before).map_err(|error| error.to_string())?;
            for (path, value) in changes {
                set_path(&mut document, &path, value)?;
            }
            document
        }
        _ => return Err("Provide raw JSON or setting changes, not both".into()),
    };
    let object = document
        .as_object_mut()
        .ok_or("Configuration must contain a JSON object")?;
    object.insert(
        "schemaVersion".into(),
        json!(crate::config_migration::CONFIG_SCHEMA_VERSION),
    );
    if let Some(chat) = object.get_mut("agentChat").and_then(Value::as_object_mut) {
        chat.remove("analyticsEnabled");
    }
    validate(path, object)?;
    if read_raw(path)? != before {
        return Err("Configuration changed on disk; reload it before saving".into());
    }
    crate::config_cli::write_document(path, object)
        .map_err(|error| format!("Cannot save configuration: {error:#}"))?;
    let mut saved = snapshot(config)?;
    let mut previous: Value = serde_json::from_str(&before).map_err(|error| error.to_string())?;
    let mut next = saved["document"].clone();
    for value in [&mut previous, &mut next] {
        if let Some(chat) = value.get_mut("agentChat").and_then(Value::as_object_mut) {
            chat.remove("defaultMessageMode");
            chat.remove("rolloutReminderInterval");
            chat.remove("rolloutEnabled");
            chat.remove("analyticsEnabled");
            chat.remove("turnRenewal");
            chat.remove("extras");
            if let Some(automation) = chat.get_mut("automation").and_then(Value::as_object_mut) {
                automation.remove("connectorName");
                automation.remove("automaticRetries");
                if automation.is_empty() {
                    chat.remove("automation");
                }
            }
        }
        if let Some(experimental) = value.get_mut("experimental").and_then(Value::as_object_mut) {
            for key in [
                "forceReadOnlyToolAnnotations",
                "forceDestructiveToolAnnotations",
                "forceIdempotentToolAnnotations",
                "forceOpenWorldToolAnnotations",
            ] {
                experimental.remove(key);
            }
        }
        if value
            .get("experimental")
            .and_then(Value::as_object)
            .is_some_and(|v| v.is_empty())
        {
            value.as_object_mut().unwrap().remove("experimental");
        }
        if value
            .get("agentChat")
            .and_then(Value::as_object)
            .is_some_and(|chat| chat.is_empty())
        {
            value.as_object_mut().unwrap().remove("agentChat");
        }
        value.as_object_mut().unwrap().remove("schemaVersion");
    }
    saved["restartRequired"] = json!(previous != next);
    let before_document: Value = serde_json::from_str(&before).map_err(|e| e.to_string())?;
    let old_projection = schema_projection(&before_document, &saved["defaults"]);
    let new_projection = schema_projection(&saved["document"], &saved["defaults"]);
    saved["schemaRefreshRequired"] = json!(old_projection != new_projection);
    let mut structural_before = old_projection;
    let mut structural_after = new_projection;
    for value in [&mut structural_before, &mut structural_after] {
        for key in [
            "/experimental/forceReadOnlyToolAnnotations",
            "/experimental/forceDestructiveToolAnnotations",
            "/experimental/forceIdempotentToolAnnotations",
            "/experimental/forceOpenWorldToolAnnotations",
        ] {
            value.as_object_mut().unwrap().remove(key);
        }
    }
    saved["schemaRestartRequired"] = json!(structural_before != structural_after);
    Ok(saved)
}

fn set_path(document: &mut Value, path: &str, value: Value) -> Result<(), String> {
    let segments: Vec<_> = path.split('.').collect();
    if segments.is_empty()
        || segments
            .iter()
            .any(|key| key.is_empty() || matches!(*key, "__proto__" | "constructor" | "prototype"))
    {
        return Err("Invalid settings key".into());
    }
    let mut target = document;
    for key in &segments[..segments.len() - 1] {
        let map = target
            .as_object_mut()
            .ok_or("A settings group must be an object")?;
        target = map.entry((*key).to_string()).or_insert_with(|| json!({}));
        if target.is_null() {
            *target = json!({});
        }
    }
    target
        .as_object_mut()
        .ok_or("A settings group must be an object")?
        .insert(segments.last().unwrap().to_string(), value);
    Ok(())
}

fn validate(path: &Path, document: &Map<String, Value>) -> Result<(), String> {
    let known = [
        "schemaVersion",
        "instanceName",
        "workDir",
        "debug",
        "uiWidgets",
        "experimental",
        "forceReadOnlyToolAnnotations",
        "apiKey",
        "conversationAuthToken",
        "port",
        "multiProject",
        "projectCloneDir",
        "worktrees",
        "tree",
        "command",
        "exec",
        "projectDoc",
        "output",
        "filesystem",
        "diff",
        "review",
        "artifactIngress",
        "artifactEgress",
        "memory",
        "agentChat",
        "skills",
        "ignore",
        "toolLogging",
        "audit",
        "codexMcp",
        "projectCatalog",
        "allowedHosts",
        "openaiTunnel",
        "openaiTunnels",
        "mcpServers",
    ];
    if let Some(key) = document.keys().find(|key| !known.contains(&key.as_str())) {
        return Err(format!("Unknown global setting: {key}"));
    }
    let parent = path
        .parent()
        .ok_or("Configuration has no parent directory")?;
    std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    let mut staging = tempfile::Builder::new()
        .prefix(".settings-validate-")
        .suffix(".json")
        .tempfile_in(parent)
        .map_err(|error| error.to_string())?;
    let bytes = serde_json::to_vec(document).map_err(|error| error.to_string())?;
    if bytes.len() > MAX_CONFIG_BYTES {
        return Err("Configuration exceeds 1 MiB".into());
    }
    staging
        .write_all(&bytes)
        .and_then(|_| staging.as_file().sync_all())
        .map_err(|error| error.to_string())?;
    let cli = crate::config::Cli::try_parse_from([
        std::ffi::OsString::from("codexify"),
        std::ffi::OsString::from("--config"),
        staging.path().as_os_str().to_owned(),
    ])
    .map_err(|error| error.to_string())?;
    crate::config::validate_config_quiet(&cli)
}

pub fn schedule_restart() -> Result<(), String> {
    if std::env::var_os(crate::process_env::SERVICE_SUPERVISED_ENV).as_deref()
        != Some(std::ffi::OsStr::new("1"))
    {
        return Err("This instance is not running under the Codexify service".into());
    }
    if !crate::service::is_installed().map_err(|error| error.to_string())? {
        return Err("Install the Codexify service before using Restart".into());
    }
    let home = crate::util::home_dir().ok_or("Cannot locate the configuration directory")?;
    let directory = home.join(".codexify/maintenance");
    std::fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
    let id = format!("restart-{}", crate::markdown_chat::now_ms());
    let worker = directory.join(format!("{id}.{}", if cfg!(windows) { "ps1" } else { "sh" }));
    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
    let script = if cfg!(windows) {
        format!(
            "Start-Sleep -Seconds 2\n& '{}' service restart\nif ($LASTEXITCODE -ne 0) {{ exit $LASTEXITCODE }}\n",
            executable.to_string_lossy().replace('\'', "''")
        )
    } else {
        unix_restart_script(&executable, &directory.join(format!("{id}.claimed")), &id)
    };
    std::fs::write(&worker, script).map_err(|error| error.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))
            .map_err(|error| error.to_string())?;
        std::fs::set_permissions(&worker, std::fs::Permissions::from_mode(0o700))
            .map_err(|error| error.to_string())?;
    }
    crate::self_update::schedule_worker(&worker, &id, true).map_err(|error| error.to_string())
}

fn unix_restart_script(executable: &Path, claim: &Path, id: &str) -> String {
    let quote = |value: &str| format!("'{}'", value.replace('\'', "'\\''"));
    let cleanup = if cfg!(target_os = "macos") {
        format!(
            "launchctl remove {} >/dev/null 2>&1 || true",
            quote(&format!("dev.codexify.update.{id}"))
        )
    } else {
        ":".into()
    };
    format!(
        "#!/bin/sh\nset -eu\numask 077\nfinish() {{\n  worker_exit_code=$?\n  trap - 0\n  {cleanup}\n  exit \"$worker_exit_code\"\n}}\ntrap finish 0\nif ! mkdir {claim} 2>/dev/null; then\n  [ -d {claim} ] && exit 0\n  exit 1\nfi\nsleep 2\n{executable} service restart\n",
        claim = quote(&claim.to_string_lossy()),
        executable = quote(&executable.to_string_lossy()),
    )
}

pub fn refresh_intent_path() -> Result<PathBuf, String> {
    Ok(crate::util::home_dir()
        .ok_or("Cannot locate update state")?
        .join(".codexify/connector-refresh.json"))
}
pub fn record_refresh_intent(update_id: &str, target: &str) -> Result<(), String> {
    write_refresh_intent(
        &json!({"updateId":update_id,"targetVersion":target,"phase":"waiting_for_restart"}),
    )
}
pub fn read_refresh_intent() -> Option<Value> {
    let path = refresh_intent_path().ok()?;
    let metadata = std::fs::symlink_metadata(&path).ok()?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > 32768 {
        return None;
    }
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}
pub fn write_refresh_intent(value: &Value) -> Result<(), String> {
    let path = refresh_intent_path()?;
    if std::fs::symlink_metadata(&path)
        .is_ok_and(|meta| !meta.is_file() || meta.file_type().is_symlink())
    {
        return Err("Unsafe connector refresh state file".into());
    }
    let mut temp = tempfile::NamedTempFile::new_in(path.parent().unwrap())
        .map_err(|error| error.to_string())?;
    serde_json::to_writer(&mut temp, value).map_err(|error| error.to_string())?;
    temp.as_file()
        .sync_all()
        .map_err(|error| error.to_string())?;
    temp.persist(path).map_err(|error| error.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn offline_timeout_is_validated_saved_and_requires_restart() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("config.json");
        let mut config = crate::config::default_config(root.path().into());
        config.loaded_config_path = Some(path.clone());
        std::fs::write(
            &path,
            json!({"workDir":root.path(),"codexMcp":{"enabled":false}}).to_string(),
        )
        .unwrap();
        let before = snapshot(&config).unwrap();
        assert_eq!(before["defaults"]["agentChat"]["offlineTimeoutMinutes"], 5);
        let saved = save(
            &config,
            before["revision"].as_str().unwrap(),
            None,
            Some(Map::from_iter([(
                "agentChat.offlineTimeoutMinutes".into(),
                json!(10),
            )])),
        )
        .unwrap();
        assert_eq!(saved["document"]["agentChat"]["offlineTimeoutMinutes"], 10);
        assert_eq!(saved["restartRequired"], true);
        assert_eq!(saved["schemaRestartRequired"], false);
        let persisted = std::fs::read(&path).unwrap();
        for value in [json!(0), json!(1441), json!(1.5), json!("10")] {
            assert!(
                save(
                    &config,
                    saved["revision"].as_str().unwrap(),
                    None,
                    Some(Map::from_iter([(
                        "agentChat.offlineTimeoutMinutes".into(),
                        value
                    )]))
                )
                .is_err()
            );
            assert_eq!(std::fs::read(&path).unwrap(), persisted);
        }
    }

    #[test]
    fn optional_instance_name_can_be_saved_reset_and_kept_during_other_edits() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("config.json");
        let mut config = crate::config::default_config(root.path().into());
        config.loaded_config_path = Some(path.clone());
        std::fs::write(
            &path,
            json!({"workDir":root.path(),"codexMcp":{"enabled":false}}).to_string(),
        )
        .unwrap();
        let initial = snapshot(&config).unwrap();
        assert_eq!(initial["defaults"]["instanceName"], Value::Null);
        assert!(initial["fieldSchema"].get("instanceName").is_some());
        assert!(initial["document"].get("instanceName").is_none());
        let named = save(
            &config,
            initial["revision"].as_str().unwrap(),
            None,
            Some(Map::from_iter([(
                "instanceName".into(),
                json!("Workstation"),
            )])),
        )
        .unwrap();
        let edited = save(
            &config,
            named["revision"].as_str().unwrap(),
            None,
            Some(Map::from_iter([("debug".into(), json!(false))])),
        )
        .unwrap();
        assert_eq!(edited["document"]["instanceName"], "Workstation");
        let reset = save(
            &config,
            edited["revision"].as_str().unwrap(),
            None,
            Some(Map::from_iter([("instanceName".into(), Value::Null)])),
        )
        .unwrap();
        let cli =
            crate::config::Cli::try_parse_from(["codexify", "--config", path.to_str().unwrap()])
                .unwrap();
        assert!(
            crate::config::load_config_quiet_read_only(cli)
                .unwrap()
                .instance_name
                .is_none()
        );
        assert!(reset["document"]["instanceName"].is_null());
    }

    #[test]
    fn snapshots_show_runtime_defaults_without_materializing_overrides() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("config.json");
        let raw = json!({"workDir":root.path(),"codexMcp":{"enabled":false}}).to_string();
        std::fs::write(&path, &raw).unwrap();
        let cli =
            crate::config::Cli::try_parse_from(["codexify", "--config", path.to_str().unwrap()])
                .unwrap();
        let config = crate::config::load_config_quiet_read_only(cli).unwrap();
        let shown = snapshot(&config).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), raw);
        assert!(shown["document"].get("exec").is_none());
        assert_eq!(
            shown["defaults"]["exec"]["maxSessions"],
            config.exec.max_sessions
        );
        assert_eq!(
            shown["defaults"]["exec"]["idleTimeoutMs"],
            config.exec.idle_timeout_ms
        );
        assert_eq!(
            shown["defaults"]["exec"]["defaultShell"],
            crate::exec_sessions::resolve_shell(None)[0]
        );
        assert_eq!(
            shown["defaults"]["filesystem"],
            serde_json::to_value(&config.filesystem).unwrap()
        );
        assert_eq!(
            shown["defaults"]["worktrees"]["root"],
            json!(config.worktrees.root)
        );
        assert_eq!(
            shown["defaults"]["worktrees"]["upstreamRefreshMode"],
            config.worktrees.upstream_refresh_mode.as_str()
        );
        assert_eq!(
            shown["defaults"]["output"]["maxFileBytes"],
            crate::output_budget::DEFAULT_MAX_FILE_BYTES
        );
        assert!(shown["defaults"]["multiProject"].is_null());
        assert!(shown["fieldSchema"]["multiProject"].is_null());
        assert_eq!(shown["defaults"]["mcpServers"], json!({}));
        for key in [
            "forceDestructiveToolAnnotations",
            "forceIdempotentToolAnnotations",
            "forceOpenWorldToolAnnotations",
        ] {
            if key == "forceIdempotentToolAnnotations" {
                assert!(shown["defaults"]["experimental"][key].is_null());
                assert_eq!(
                    shown["defaultLabels"][format!("experimental.{key}")],
                    "Keep tool hints"
                );
            } else {
                assert_eq!(shown["defaults"]["experimental"][key], false);
                assert_eq!(shown["fieldSchema"]["experimental"][key], false);
            }
        }
        assert_eq!(shown["defaults"]["openaiTunnels"], json!([]));
        assert_eq!(
            shown["defaults"]["agentChat"]["notifications"]["urls"],
            json!([])
        );
        let saved = save(
            &config,
            shown["revision"].as_str().unwrap(),
            None,
            Some(Map::from_iter([("exec.maxSessions".into(), json!(12))])),
        )
        .unwrap();
        assert_eq!(saved["document"]["exec"], json!({"maxSessions":12}));
        assert!(saved["document"].get("filesystem").is_none());
        assert_eq!(
            saved["defaults"]["exec"]["maxSessions"],
            config.exec.max_sessions
        );
    }

    #[test]
    fn displayed_defaults_follow_dependent_settings_without_writing_them() {
        let root = tempfile::tempdir().unwrap();
        let config = crate::config::default_config(root.path().to_owned());
        let document = json!({"workDir":"/another/project","skills":{"dirs":[]}});
        let defaults = crate::config::settings_defaults(&config, &document);
        assert_eq!(defaults["projectCloneDir"], "/another/project");
        assert_eq!(defaults["skills"]["includePlugins"], false);
        assert!(
            defaults["memory"]["dir"]
                .as_str()
                .unwrap()
                .contains("project-")
        );
        assert_eq!(
            document,
            json!({"workDir":"/another/project","skills":{"dirs":[]}})
        );
    }
    #[cfg(unix)]
    #[test]
    fn restart_worker_runs_only_once_when_the_scheduler_relaunches_it() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let executable = root.path().join("restart 'fixture.sh");
        let calls = root.path().join("calls");
        std::fs::write(&executable, "#!/bin/sh\n[ \"$1 $2\" = 'service restart' ] || exit 1\nprintf 'restart\\n' >> \"$RESTART_TEST_CALLS\"\n").unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let launchctl = root.path().join("launchctl");
        std::fs::write(&launchctl, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&launchctl, std::fs::Permissions::from_mode(0o700)).unwrap();
        let worker = root.path().join("worker.sh");
        std::fs::write(
            &worker,
            unix_restart_script(&executable, &root.path().join("once"), "fixture"),
        )
        .unwrap();
        for _ in 0..2 {
            assert!(
                std::process::Command::new("/bin/sh")
                    .arg(&worker)
                    .env("RESTART_TEST_CALLS", &calls)
                    .env("PATH", format!("{}:/usr/bin:/bin", root.path().display()))
                    .status()
                    .unwrap()
                    .success()
            );
        }
        assert_eq!(std::fs::read_to_string(calls).unwrap(), "restart\n");
    }
    #[test]
    fn settings_validate_before_replacement_and_reject_stale_edits() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("config.json");
        std::fs::write(
            &path,
            format!(
                "{{\"workDir\":{},\"codexMcp\":{{\"enabled\":false}},\"debug\":false}}",
                serde_json::to_string(root.path()).unwrap()
            ),
        )
        .unwrap();
        let mut config = crate::config::default_config(root.path().to_owned());
        config.loaded_config_path = Some(path.clone());
        let initial = snapshot(&config).unwrap();
        let etag = initial["revision"].as_str().unwrap();
        let before = std::fs::read(&path).unwrap();
        for raw in [
            "{",
            "{\"port\":99999}",
            "{\"notASetting\":true}",
            "{\"agentChat\":{\"automation\":{\"enabled\":true},\"enabled\":false}}",
        ] {
            assert!(save(&config, etag, Some(raw.into()), None).is_err());
            assert_eq!(std::fs::read(&path).unwrap(), before);
        }
        let saved = save(
            &config,
            etag,
            None,
            Some(Map::from_iter([("debug".into(), json!(true))])),
        )
        .unwrap();
        assert_eq!(saved["document"]["debug"], true);
        assert!(
            save(&config, etag, None, Some(Map::new()))
                .unwrap_err()
                .contains("changed on disk")
        );
        assert_eq!(saved["document"]["workDir"], initial["document"]["workDir"]);
    }

    #[test]
    fn settings_allow_disabled_audit_without_a_destination_and_publish_its_default() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("config.json");
        std::fs::write(
            &path,
            json!({"workDir":root.path(),"codexMcp":{"enabled":false}}).to_string(),
        )
        .unwrap();
        let mut config = crate::config::default_config(root.path().to_owned());
        config.loaded_config_path = Some(path);
        let initial = snapshot(&config).unwrap();
        let saved = save(
            &config,
            initial["revision"].as_str().unwrap(),
            None,
            Some(Map::from_iter([("audit.enabled".into(), json!(false))])),
        )
        .unwrap();
        assert_eq!(saved["document"]["audit"]["enabled"], false);
        assert!(saved["document"]["audit"].get("logFile").is_none());
        assert_eq!(
            saved["defaults"]["audit"]["logFile"],
            json!(config.audit.log_file)
        );
        assert!(
            saved["defaults"]["audit"]["logFile"]
                .as_str()
                .unwrap()
                .ends_with("audit.jsonl")
        );
    }
    #[test]
    fn message_mode_changes_apply_immediately_and_do_not_require_restart() {
        use crate::markdown_chat::MessageMode;
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("config.json");
        std::fs::write(
            &path,
            serde_json::json!({"workDir":root.path(),"codexMcp":{"enabled":false}}).to_string(),
        )
        .unwrap();
        let mut config = crate::config::default_config(root.path().to_owned());
        config.loaded_config_path = Some(path);
        assert_eq!(default_message_mode(&config).unwrap(), MessageMode::Queue);
        let mut current = snapshot(&config).unwrap();
        for mode in [json!("queue"), json!("steer")] {
            current = save(
                &config,
                current["revision"].as_str().unwrap(),
                None,
                Some(Map::from_iter([(
                    "agentChat.defaultMessageMode".into(),
                    mode.clone(),
                )])),
            )
            .unwrap();
            assert_eq!(current["restartRequired"], false);
            assert_eq!(
                default_message_mode(&config).unwrap(),
                if mode == "queue" {
                    MessageMode::Queue
                } else {
                    MessageMode::Steer
                }
            );
        }
        let mut reset = current["document"].clone();
        reset["agentChat"]
            .as_object_mut()
            .unwrap()
            .remove("defaultMessageMode");
        current = save(
            &config,
            current["revision"].as_str().unwrap(),
            Some(reset.to_string()),
            None,
        )
        .unwrap();
        assert_eq!(current["restartRequired"], false);
        assert_eq!(default_message_mode(&config).unwrap(), MessageMode::Queue);
        let bytes = std::fs::read(config.loaded_config_path.as_ref().unwrap()).unwrap();
        assert!(
            save(
                &config,
                current["revision"].as_str().unwrap(),
                None,
                Some(Map::from_iter([(
                    "agentChat.defaultMessageMode".into(),
                    json!("invalid")
                )]))
            )
            .is_err()
        );
        assert_eq!(
            std::fs::read(config.loaded_config_path.as_ref().unwrap()).unwrap(),
            bytes
        );
    }
    #[test]
    fn progress_interval_is_validated_and_applies_without_restart() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("config.json");
        std::fs::write(
            &path,
            json!({"workDir":root.path(),"codexMcp":{"enabled":false}}).to_string(),
        )
        .unwrap();
        let mut config = crate::config::default_config(root.path().into());
        config.loaded_config_path = Some(path.clone());
        assert_eq!(rollout_reminder_interval(&config).unwrap(), 25);
        let mut current = snapshot(&config).unwrap();
        for interval in [1, 50, 10000] {
            current = save(
                &config,
                current["revision"].as_str().unwrap(),
                None,
                Some(Map::from_iter([(
                    "agentChat.rolloutReminderInterval".into(),
                    json!(interval),
                )])),
            )
            .unwrap();
            assert_eq!(current["restartRequired"], false);
            assert_eq!(rollout_reminder_interval(&config).unwrap(), interval);
        }
        let before = std::fs::read(&path).unwrap();
        for invalid in [json!(0), json!(10001), json!(-1), json!(1.5), json!("20")] {
            assert!(
                save(
                    &config,
                    current["revision"].as_str().unwrap(),
                    None,
                    Some(Map::from_iter([(
                        "agentChat.rolloutReminderInterval".into(),
                        invalid
                    )]))
                )
                .is_err()
            );
            assert_eq!(std::fs::read(&path).unwrap(), before);
        }
        let mut document = current["document"].clone();
        document["agentChat"]
            .as_object_mut()
            .unwrap()
            .remove("rolloutReminderInterval");
        let reset = save(
            &config,
            current["revision"].as_str().unwrap(),
            Some(document.to_string()),
            None,
        )
        .unwrap();
        assert_eq!(reset["restartRequired"], false);
        assert_eq!(rollout_reminder_interval(&config).unwrap(), 25);
    }
    #[test]
    fn retry_timings_save_require_restart_without_connector_schema_refresh() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("config.json");
        std::fs::write(
            &path,
            json!({"workDir":root.path(),"codexMcp":{"enabled":false},"multiProject":false})
                .to_string(),
        )
        .unwrap();
        let mut config = crate::config::default_config(root.path().to_owned());
        config.loaded_config_path = Some(path.clone());
        let shown = snapshot(&config).unwrap();
        assert_eq!(
            shown["defaults"]["agentChat"]["automation"]["retryTimings"]["chatgptAccessDelaysSeconds"],
            json!([180, 300, 600, 1200, 1800, 3600])
        );
        let saved = save(
            &config,
            shown["revision"].as_str().unwrap(),
            None,
            Some(Map::from_iter([
                (
                    "agentChat.automation.retryTimings.chatgptAccessDelaysSeconds".into(),
                    json!([25, 95]),
                ),
                (
                    "agentChat.automation.retryTimings.conversationLoadIntervalSeconds".into(),
                    json!(60),
                ),
                (
                    "agentChat.automation.retryTimings.jitterPercent".into(),
                    json!(0),
                ),
            ])),
        )
        .unwrap();
        assert_eq!(saved["restartRequired"], true);
        assert_eq!(saved["schemaRefreshRequired"], false);
        assert_eq!(saved["schemaRestartRequired"], false);
        assert_eq!(
            saved["document"]["agentChat"]["automation"]["retryTimings"]["chatgptAccessDelaysSeconds"],
            json!([25, 95])
        );
        let before = std::fs::read(&path).unwrap();
        assert!(
            save(
                &config,
                saved["revision"].as_str().unwrap(),
                None,
                Some(Map::from_iter([(
                    "agentChat.automation.retryTimings.chatgptAccessDelaysSeconds".into(),
                    json!([0])
                )]))
            )
            .is_err()
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn automatic_retries_default_on_and_hot_reload_without_restart_or_schema_refresh() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("config.json");
        std::fs::write(
            &path,
            json!({"workDir":root.path(),"codexMcp":{"enabled":false},"multiProject":false})
                .to_string(),
        )
        .unwrap();
        let mut config = crate::config::default_config(root.path().to_owned());
        config.loaded_config_path = Some(path.clone());
        assert!(automatic_retries(Some(&path), false).unwrap());
        let mut shown = snapshot(&config).unwrap();
        assert_eq!(
            shown["defaults"]["agentChat"]["automation"]["automaticRetries"],
            true
        );
        for enabled in [false, true] {
            shown = save(
                &config,
                shown["revision"].as_str().unwrap(),
                None,
                Some(Map::from_iter([(
                    "agentChat.automation.automaticRetries".into(),
                    json!(enabled),
                )])),
            )
            .unwrap();
            assert!(!shown["restartRequired"].as_bool().unwrap());
            assert!(!shown["schemaRefreshRequired"].as_bool().unwrap());
            assert_eq!(automatic_retries(Some(&path), true).unwrap(), enabled);
        }
        assert!(automatic_retries(None, false).is_ok_and(|v| !v));
    }

    #[test]
    fn new_defaults_and_schema_refresh_projection_are_consistent() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("config.json");
        std::fs::write(
            &path,
            json!({"workDir":root.path(),"codexMcp":{"enabled":false},"multiProject":false})
                .to_string(),
        )
        .unwrap();
        let mut config = crate::config::default_config(root.path().to_owned());
        config.loaded_config_path = Some(path);
        let shown = snapshot(&config).unwrap();
        let defaults = &shown["defaults"];
        for pointer in [
            "/debug",
            "/agentChat/enabled",
            "/agentChat/automation/enabled",
            "/agentChat/automation/autoApproveToolCalls",
            "/agentChat/rolloutEnabled",
            "/audit/enabled",
            "/audit/includeCommandPreview",
        ] {
            assert_eq!(defaults.pointer(pointer), Some(&json!(true)), "{pointer}");
        }
        assert_eq!(defaults["filesystem"]["maxConcurrentWorkers"], 8);
        assert_eq!(defaults["artifactIngress"]["maxConcurrentDownloads"], 8);
        assert_eq!(defaults["artifactEgress"]["enabled"], false);
        assert_eq!(defaults["agentChat"]["rolloutReminderInterval"], 25);
        assert_eq!(defaults["agentChat"]["defaultMessageMode"], "queue");
        assert_eq!(defaults["toolLogging"]["level"], "info");
        assert_eq!(defaults["toolLogging"]["mode"], "all");
        assert_eq!(defaults["worktrees"]["upstreamRefreshMode"], "best-effort");
        assert_eq!(
            defaults["experimental"]["forceDestructiveToolAnnotations"],
            false
        );
        assert_eq!(
            defaults["experimental"]["forceOpenWorldToolAnnotations"],
            false
        );
        assert_eq!(
            defaults["agentChat"]["automation"]["requestBlockingEnabled"],
            false
        );
        assert_eq!(
            defaults["agentChat"]["automation"]["requestBlockList"]
                .as_array()
                .unwrap()
                .len(),
            5
        );
        assert!(defaults.get("multiProject").is_none());
        assert!(
            crate::registry::load_tools_for_config(&config)
                .iter()
                .any(|tool| tool.name() == "set_project_root")
        );
        let saved = save(
            &config,
            shown["revision"].as_str().unwrap(),
            None,
            Some(Map::from_iter([(
                "agentChat.extras.animations".into(),
                json!(false),
            )])),
        )
        .unwrap();
        assert_eq!(saved["schemaRefreshRequired"], false);
        let saved = save(
            &config,
            saved["revision"].as_str().unwrap(),
            None,
            Some(Map::from_iter([("uiWidgets".into(), json!(false))])),
        )
        .unwrap();
        assert_eq!(saved["schemaRefreshRequired"], true);
        assert_eq!(saved["schemaRestartRequired"], true);
        assert_eq!(saved["restartRequired"], true);
        write_schema_intent(&config, &json!({"refreshId":"new","phase":"queued"})).unwrap();
        update_schema_intent(
            &config,
            "old",
            &json!({"refreshId":"old","phase":"completed"}),
        )
        .unwrap();
        assert_eq!(read_schema_intent(&config).unwrap()["refreshId"], "new");
    }

    #[test]
    fn file_materialization_settings_preserve_opt_in_and_require_schema_restart() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("config.json");
        std::fs::write(
            &path,
            json!({"workDir":root.path(),"codexMcp":{"enabled":false}}).to_string(),
        )
        .unwrap();
        let cli = || {
            crate::config::Cli::try_parse_from(["codexify", "--config", path.to_str().unwrap()])
                .unwrap()
        };
        let config = crate::config::load_config_quiet_read_only(cli()).unwrap();
        let mut shown = snapshot(&config).unwrap();
        assert_eq!(shown["defaults"]["artifactEgress"]["enabled"], false);
        assert!(!config.artifact_egress.enabled);
        for enabled in [true, false] {
            shown = save(
                &config,
                shown["revision"].as_str().unwrap(),
                None,
                Some(Map::from_iter([(
                    "artifactEgress.enabled".into(),
                    json!(enabled),
                )])),
            )
            .unwrap();
            assert_eq!(shown["schemaRefreshRequired"], true);
            assert_eq!(shown["schemaRestartRequired"], true);
            assert_eq!(shown["restartRequired"], true);
            let loaded = crate::config::load_config_quiet_read_only(cli()).unwrap();
            assert_eq!(loaded.artifact_egress.enabled, enabled);
            assert!(loaded.artifact_ingress.enabled);
            assert_eq!(shown["document"]["artifactEgress"]["enabled"], enabled);
        }
    }
}

#[cfg(test)]
mod connector_name_tests {
    use super::*;

    #[test]
    fn missing_name_is_valid_configuration_but_not_ready_for_automation() {
        let chat = crate::markdown_chat::MarkdownChatConfig {
            enabled: true,
            automation: crate::markdown_chat::AgentAutomationConfig {
                enabled: true,
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(chat.automation.connector_name.is_empty());
        chat.validate().unwrap();
        assert_eq!(connector_name(None, "").unwrap(), None);
        assert_eq!(connector_name(None, "   ").unwrap(), None);
        assert!(connector_name(None, "invalid\nname").is_err());
        assert!(connector_name(None, &"x".repeat(129)).is_err());
    }

    #[test]
    fn saved_names_are_explicit_and_read_live_instead_of_using_the_startup_default() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("config.json");
        std::fs::write(&path, "{}").unwrap();
        assert_eq!(connector_name(Some(&path), "Codexify").unwrap(), None);
        std::fs::write(
            &path,
            r#"{"agentChat":{"automation":{"connectorName":"My exact connector"}}}"#,
        )
        .unwrap();
        assert_eq!(
            connector_name(Some(&path), "Old name").unwrap().as_deref(),
            Some("My exact connector")
        );
        std::fs::write(
            &path,
            r#"{"agentChat":{"automation":{"connectorName":"Renamed connector"}}}"#,
        )
        .unwrap();
        assert_eq!(
            connector_name(Some(&path), "Old name").unwrap().as_deref(),
            Some("Renamed connector")
        );
    }

    #[test]
    fn saving_a_connector_preserves_other_settings_and_does_not_require_restart() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("config.json");
        let document =
            json!({"workDir":root.path(),"debug":false,"port":37652,"codexMcp":{"enabled":false}});
        std::fs::write(&path, serde_json::to_vec(&document).unwrap()).unwrap();
        let mut config = crate::config::default_config(root.path().to_owned());
        config.loaded_config_path = Some(path.clone());
        let before = snapshot(&config).unwrap();
        let changes = Map::from_iter([(
            "agentChat.automation.connectorName".into(),
            json!("My exact connector"),
        )]);
        let saved = save(
            &config,
            before["revision"].as_str().unwrap(),
            None,
            Some(changes),
        )
        .unwrap();
        assert_eq!(saved["restartRequired"], false);
        assert_eq!(saved["document"]["debug"], false);
        assert_eq!(saved["document"]["port"], 37652);
        assert_eq!(
            connector_name(Some(&path), "").unwrap().as_deref(),
            Some("My exact connector")
        );
        assert!(config.markdown_chat.automation.connector_name.is_empty());
    }
}
