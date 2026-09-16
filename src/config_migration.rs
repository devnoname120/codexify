//! Versioned, recoverable migrations for `codexify.config.json`.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use serde_json::{Map, Value};
use tempfile::NamedTempFile;

pub const CONFIG_SCHEMA_VERSION: u64 = 1;

pub struct PreparedConfig {
    pub document: Map<String, Value>,
    pub migration: Option<PendingConfigMigration>,
}

pub struct PendingConfigMigration {
    path: PathBuf,
    original: Vec<u8>,
    migrated: Vec<u8>,
    from_version: u64,
}

pub struct ConfigMigrationOutcome {
    pub from_version: u64,
    pub to_version: u64,
    pub backup_path: PathBuf,
}

pub fn prepare(path: &Path, source: &[u8]) -> anyhow::Result<PreparedConfig> {
    let value: Value = serde_json::from_slice(source)
        .with_context(|| format!("parse config file {}", path.display()))?;
    let mut document = value.as_object().cloned().with_context(|| {
        format!(
            "configuration must contain a JSON object: {}",
            path.display()
        )
    })?;
    let version = schema_version(&document, path)?;
    if version > CONFIG_SCHEMA_VERSION {
        bail!(
            "config file {} uses schemaVersion {version}, but this Codexify build supports up to {CONFIG_SCHEMA_VERSION}; upgrade Codexify instead of starting with a newer config",
            path.display()
        );
    }
    if version == CONFIG_SCHEMA_VERSION {
        return Ok(PreparedConfig {
            document,
            migration: None,
        });
    }

    let from_version = version;
    let mut next = version;
    while next < CONFIG_SCHEMA_VERSION {
        match next {
            0 => migrate_v0_to_v1(&mut document)?,
            unsupported => bail!(
                "config file {} uses unsupported schemaVersion {unsupported}",
                path.display()
            ),
        }
        next += 1;
    }
    document.insert(
        "schemaVersion".to_string(),
        Value::Number(CONFIG_SCHEMA_VERSION.into()),
    );

    let mut migrated = serde_json::to_vec_pretty(&Value::Object(document.clone()))
        .context("serialize migrated configuration")?;
    migrated.push(b'\n');
    Ok(PreparedConfig {
        document,
        migration: Some(PendingConfigMigration {
            path: path.to_path_buf(),
            original: source.to_vec(),
            migrated,
            from_version,
        }),
    })
}

impl PendingConfigMigration {
    pub fn commit(self) -> anyhow::Result<ConfigMigrationOutcome> {
        refuse_unsafe_target(&self.path)?;
        match current_contents(&self.path, &self.original, &self.migrated)? {
            CurrentContents::AlreadyMigrated => {
                let backup_path = find_existing_backup(
                    &self.path,
                    CONFIG_SCHEMA_VERSION,
                    &self.original,
                )?
                .with_context(|| {
                    format!(
                        "config file {} was migrated concurrently, but its backup is missing",
                        self.path.display()
                    )
                })?;
                return Ok(ConfigMigrationOutcome {
                    from_version: self.from_version,
                    to_version: CONFIG_SCHEMA_VERSION,
                    backup_path,
                });
            }
            CurrentContents::Original => {}
        }

        let backup_path = self.backup_original()?;
        match current_contents(&self.path, &self.original, &self.migrated)? {
            CurrentContents::AlreadyMigrated => {}
            CurrentContents::Original => {
                replace_file(&self.path, &self.migrated)?;
            }
        }
        Ok(ConfigMigrationOutcome {
            from_version: self.from_version,
            to_version: CONFIG_SCHEMA_VERSION,
            backup_path,
        })
    }

    pub fn backup_original(&self) -> anyhow::Result<PathBuf> {
        refuse_unsafe_target(&self.path)?;
        let current = current_contents(&self.path, &self.original, &self.migrated)?;
        if let Some(existing) =
            find_existing_backup(&self.path, CONFIG_SCHEMA_VERSION, &self.original)?
        {
            return Ok(existing);
        }
        match current {
            CurrentContents::AlreadyMigrated => bail!(
                "config file {} was migrated concurrently, but its backup is missing",
                self.path.display()
            ),
            CurrentContents::Original => {}
        }

        let parent = config_parent(&self.path);
        let permissions = fs::metadata(&self.path)
            .with_context(|| format!("inspect config file {}", self.path.display()))?
            .permissions();
        for index in 0_u32.. {
            let target = backup_path(&self.path, CONFIG_SCHEMA_VERSION, index);
            let mut temp = NamedTempFile::new_in(parent).with_context(|| {
                format!(
                    "create temporary config backup beside {}",
                    self.path.display()
                )
            })?;
            temp.write_all(&self.original)
                .with_context(|| format!("write config backup {}", target.display()))?;
            temp.as_file()
                .sync_all()
                .with_context(|| format!("sync config backup {}", target.display()))?;
            fs::set_permissions(temp.path(), permissions.clone()).with_context(|| {
                format!("preserve config permissions on backup {}", target.display())
            })?;
            match temp.persist_noclobber(&target) {
                Ok(_) => {
                    sync_parent(parent)?;
                    return Ok(target);
                }
                Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {
                    if fs::read(&target).ok().as_deref() == Some(self.original.as_slice()) {
                        return Ok(target);
                    }
                }
                Err(error) => {
                    return Err(error.error)
                        .with_context(|| format!("create config backup {}", target.display()));
                }
            }
        }
        unreachable!("the backup suffix space is not finite")
    }
}

fn schema_version(document: &Map<String, Value>, path: &Path) -> anyhow::Result<u64> {
    match document.get("schemaVersion") {
        None => Ok(0),
        Some(Value::Number(number)) => number.as_u64().with_context(|| {
            format!(
                "schemaVersion in {} must be a non-negative integer",
                path.display()
            )
        }),
        Some(_) => bail!(
            "schemaVersion in {} must be a non-negative integer",
            path.display()
        ),
    }
}

fn migrate_v0_to_v1(document: &mut Map<String, Value>) -> anyhow::Result<()> {
    rename_field(document, "review", "diff")?;
    document.remove("allowedCommands");

    let remove_exec = document
        .get_mut("exec")
        .and_then(Value::as_object_mut)
        .is_some_and(|exec| {
            exec.remove("mode");
            exec.remove("extraAllowedCommands");
            exec.is_empty()
        });
    if remove_exec {
        document.remove("exec");
    }

    if let Some(artifact_egress) = document
        .get_mut("artifactEgress")
        .and_then(Value::as_object_mut)
    {
        artifact_egress.remove("maxCachedBytes");
    }

    rename_field(document, "markdownChat", "agentChat")?;
    if let Some(chat) = document.get_mut("agentChat") {
        migrate_chat_notifications(chat)?;
    }
    Ok(())
}

fn rename_field(
    document: &mut Map<String, Value>,
    old_name: &str,
    new_name: &str,
) -> anyhow::Result<()> {
    let Some(old_value) = document.remove(old_name) else {
        return Ok(());
    };
    if document.contains_key(new_name) {
        document.insert(old_name.to_string(), old_value);
        bail!(
            "config contains both {old_name} and {new_name}; remove one before migration so no value is guessed"
        );
    }
    document.insert(new_name.to_string(), old_value);
    Ok(())
}

fn migrate_chat_notifications(chat: &mut Value) -> anyhow::Result<()> {
    let Some(chat) = chat.as_object_mut() else {
        return Ok(());
    };
    let Some(ntfy) = chat.remove("ntfy") else {
        return Ok(());
    };
    if ntfy.is_null() {
        return Ok(());
    }
    if chat
        .get("notifications")
        .is_some_and(|value| !value.is_null())
    {
        chat.insert("ntfy".to_string(), ntfy);
        bail!(
            "config contains both agentChat.ntfy and agentChat.notifications; remove one before migration so no destination is guessed"
        );
    }
    chat.remove("notifications");

    let ntfy = ntfy
        .as_object()
        .context("agentChat.ntfy must be an object or null")?;
    if ntfy.keys().any(|key| key != "url" && key != "token") {
        bail!("agentChat.ntfy contains an unsupported field");
    }
    let raw_url = ntfy
        .get("url")
        .and_then(Value::as_str)
        .context("agentChat.ntfy.url must be an HTTP(S) topic URL")?;
    let parsed =
        reqwest::Url::parse(raw_url).context("agentChat.ntfy.url must be an HTTP(S) topic URL")?;
    if !matches!(parsed.scheme(), "http" | "https")
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || parsed.path().trim_matches('/').is_empty()
    {
        bail!(
            "agentChat.ntfy.url must be an HTTP(S) topic URL without credentials, query or fragment"
        );
    }
    let token = match ntfy.get("token") {
        None | Some(Value::Null) => None,
        Some(Value::String(token))
            if !token.is_empty()
                && reqwest::header::HeaderValue::try_from(format!("Bearer {token}")).is_ok() =>
        {
            Some(token.as_str())
        }
        Some(_) => bail!("agentChat.ntfy.token must be a nonempty valid bearer token or null"),
    };

    let scheme = if parsed.scheme() == "https" {
        "ntfys"
    } else {
        "ntfy"
    };
    let authority_and_path = raw_url
        .split_once("://")
        .map(|(_, remainder)| remainder)
        .context("agentChat.ntfy.url must be an HTTP(S) topic URL")?;
    let service_url = match token {
        Some(token) => format!(
            "{scheme}://{}@{authority_and_path}?auth=token&image=no",
            utf8_percent_encode(token, NON_ALPHANUMERIC)
        ),
        None => format!("{scheme}://{authority_and_path}?image=no"),
    };
    chat.insert(
        "notifications".to_string(),
        serde_json::json!({ "urls": [service_url] }),
    );
    Ok(())
}

enum CurrentContents {
    Original,
    AlreadyMigrated,
}

fn current_contents(
    path: &Path,
    original: &[u8],
    migrated: &[u8],
) -> anyhow::Result<CurrentContents> {
    let current = fs::read(path)
        .with_context(|| format!("re-read config file before migration {}", path.display()))?;
    if current == original {
        return Ok(CurrentContents::Original);
    }
    if current == migrated {
        return Ok(CurrentContents::AlreadyMigrated);
    }
    bail!(
        "config file {} changed while schema migration was being prepared; retry without overwriting the concurrent edit",
        path.display()
    )
}

fn find_existing_backup(
    path: &Path,
    version: u64,
    original: &[u8],
) -> anyhow::Result<Option<PathBuf>> {
    for index in 0_u32.. {
        let candidate = backup_path(path, version, index);
        match fs::read(&candidate) {
            Ok(bytes) if bytes == original => return Ok(Some(candidate)),
            Ok(_) => continue,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("inspect config backup {}", candidate.display()));
            }
        }
    }
    unreachable!("the backup suffix space is not finite")
}

fn backup_path(path: &Path, version: u64, index: u32) -> PathBuf {
    let mut name = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    name.push_str(&format!(".before-schema-v{version}.bak"));
    if index > 0 {
        name.push_str(&format!(".{index}"));
    }
    path.with_file_name(name)
}

fn replace_file(path: &Path, contents: &[u8]) -> anyhow::Result<()> {
    refuse_unsafe_target(path)?;
    let parent = config_parent(path);
    let permissions = fs::metadata(path)
        .with_context(|| format!("inspect config file {}", path.display()))?
        .permissions();
    let mut temp = NamedTempFile::new_in(parent)
        .with_context(|| format!("create temporary migrated config beside {}", path.display()))?;
    temp.write_all(contents)
        .with_context(|| format!("write migrated config {}", path.display()))?;
    temp.as_file()
        .sync_all()
        .with_context(|| format!("sync migrated config {}", path.display()))?;
    fs::set_permissions(temp.path(), permissions)
        .with_context(|| format!("preserve config permissions on {}", path.display()))?;
    persist_replacing(temp, path)
        .with_context(|| format!("replace migrated config {}", path.display()))?;
    sync_parent(parent)?;
    Ok(())
}

fn refuse_unsafe_target(path: &Path) -> anyhow::Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect config file {}", path.display()))?;
    if metadata.file_type().is_symlink() {
        bail!(
            "refusing to migrate symlinked config file {}",
            path.display()
        );
    }
    if !metadata.is_file() {
        bail!("config path is not a regular file: {}", path.display());
    }
    Ok(())
}

fn config_parent(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

#[cfg(not(windows))]
fn persist_replacing(temp: NamedTempFile, path: &Path) -> io::Result<()> {
    temp.persist(path).map(|_| ()).map_err(|error| error.error)
}

#[cfg(windows)]
fn persist_replacing(temp: NamedTempFile, path: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    let source = temp.into_temp_path();
    let wide = |value: &std::ffi::OsStr| {
        value
            .encode_wide()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>()
    };
    let source_wide = wide(source.as_os_str());
    let target_wide = wide(path.as_os_str());
    let moved = unsafe {
        MoveFileExW(
            source_wide.as_ptr(),
            target_wide.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if moved == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(unix)]
fn sync_parent(parent: &Path) -> io::Result<()> {
    fs::File::open(parent)?.sync_all()
}

#[cfg(not(unix))]
fn sync_parent(_parent: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const V1_3_0_CONFIG: &[u8] = include_bytes!("../tests/fixtures/codexify.config.v1.3.0.json");

    #[test]
    fn exact_v1_3_0_config_migrates_with_backup_and_is_idempotent() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("codexify.config.json");
        fs::write(&path, V1_3_0_CONFIG).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        }

        let prepared = prepare(&path, V1_3_0_CONFIG).unwrap();
        assert_eq!(prepared.document["schemaVersion"], 1);
        assert!(prepared.document.get("allowedCommands").is_none());
        assert!(prepared.document["exec"].get("mode").is_none());
        assert!(
            prepared.document["exec"]
                .get("extraAllowedCommands")
                .is_none()
        );
        assert_eq!(prepared.document["exec"]["maxSessions"], 8);
        let outcome = prepared.migration.unwrap().commit().unwrap();
        assert_eq!(outcome.from_version, 0);
        assert_eq!(outcome.to_version, CONFIG_SCHEMA_VERSION);
        assert_eq!(fs::read(&outcome.backup_path).unwrap(), V1_3_0_CONFIG);

        let migrated = fs::read(&path).unwrap();
        let second = prepare(&path, &migrated).unwrap();
        assert!(second.migration.is_none());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o640
            );
        }
    }

    #[test]
    fn all_historical_renames_are_explicit() {
        let path = Path::new("config.json");
        let prepared = prepare(
            path,
            br#"{
              "review":{"maxPatchBytes":1234},
              "allowedCommands":["git"],
              "exec":{"mode":"allowlist","extraAllowedCommands":["git"],"maxSessions":2},
              "artifactEgress":{"maxCachedBytes":42,"maxFileBytes":99},
              "markdownChat":{"enabled":true,"ntfy":{"url":"https://ntfy.example/topic","token":"a/b"}}
            }"#,
        )
        .unwrap();

        assert_eq!(prepared.document["diff"]["maxPatchBytes"], 1234);
        assert_eq!(
            prepared.document["exec"],
            serde_json::json!({"maxSessions": 2})
        );
        assert_eq!(
            prepared.document["artifactEgress"],
            serde_json::json!({"maxFileBytes": 99})
        );
        assert_eq!(prepared.document["agentChat"]["enabled"], true);
        assert_eq!(
            prepared.document["agentChat"]["notifications"]["urls"][0],
            "ntfys://a%2Fb@ntfy.example/topic?auth=token&image=no"
        );
        for removed in ["review", "allowedCommands", "markdownChat"] {
            assert!(prepared.document.get(removed).is_none());
        }
    }

    #[test]
    fn future_schema_and_ambiguous_renames_fail_without_writing() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("config.json");
        let future = br#"{"schemaVersion":999}"#;
        fs::write(&path, future).unwrap();
        assert!(
            prepare(&path, future)
                .err()
                .unwrap()
                .to_string()
                .contains("supports up to")
        );
        assert_eq!(fs::read(&path).unwrap(), future);

        let ambiguous = br#"{"markdownChat":{},"agentChat":{}}"#;
        fs::write(&path, ambiguous).unwrap();
        assert!(
            prepare(&path, ambiguous)
                .err()
                .unwrap()
                .to_string()
                .contains("both markdownChat and agentChat")
        );
        assert_eq!(fs::read(&path).unwrap(), ambiguous);
    }

    #[test]
    fn backup_collisions_and_concurrent_edits_do_not_overwrite_data() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("config.json");
        let original = br#"{"port":3000}"#;
        fs::write(&path, original).unwrap();
        let occupied = path.with_file_name("config.json.before-schema-v1.bak");
        fs::write(&occupied, b"keep this backup").unwrap();

        let prepared = prepare(&path, original).unwrap();
        let outcome = prepared.migration.unwrap().commit().unwrap();
        assert_eq!(
            outcome.backup_path,
            PathBuf::from(format!("{}.1", occupied.display()))
        );
        assert_eq!(fs::read(&occupied).unwrap(), b"keep this backup");
        assert_eq!(fs::read(&outcome.backup_path).unwrap(), original);

        let current = fs::read(&path).unwrap();
        let prepared = prepare(&path, &current).unwrap();
        assert!(prepared.migration.is_none());

        fs::write(&path, original).unwrap();
        let pending = prepare(&path, original).unwrap().migration.unwrap();
        fs::write(&path, br#"{"port":4000}"#).unwrap();
        let error = pending.commit().err().unwrap().to_string();
        assert!(error.contains("changed while schema migration was being prepared"));
        assert_eq!(fs::read(&path).unwrap(), br#"{"port":4000}"#);
    }

    #[cfg(unix)]
    #[test]
    fn migration_refuses_a_symlink_without_changing_its_target() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target.json");
        let link = root.path().join("config.json");
        let original = br#"{"port":3000}"#;
        fs::write(&target, original).unwrap();
        symlink(&target, &link).unwrap();

        let pending = prepare(&link, original).unwrap().migration.unwrap();
        let error = pending.commit().err().unwrap().to_string();

        assert!(error.contains("symlinked config file"));
        assert_eq!(fs::read(target).unwrap(), original);
    }
}
