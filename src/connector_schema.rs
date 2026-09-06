//! Last schema version served to each identifiable ChatGPT connector caller.
//! This is UI bookkeeping, not authentication. Anonymous discovery is never
//! attributed to the most recent conversation or to every connected account.
use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

use rmcp::model::RequestMetaObject;
use sha2::{Digest, Sha256};

use crate::types::AppConfig;

#[derive(Default)]
pub(crate) struct ConnectorSchemaStore {
    versions: Mutex<HashMap<String, String>>,
    directory: Option<PathBuf>,
}

pub(crate) fn caller_key(meta: &RequestMetaObject) -> Option<String> {
    let subject = meta.get("openai/subject")?.as_str()?.trim();
    if subject.is_empty() || subject.len() > 1024 {
        return None;
    }
    let organization = match meta.get("openai/organization") {
        None => "",
        Some(value) => value.as_str()?,
    };
    if organization.len() > 1024 {
        return None;
    }
    // Hash caller identifiers for private filenames, not the connector schema.
    Some(private_key(&[subject, organization]))
}

fn private_key(parts: &[&str]) -> String {
    let mut hash = Sha256::new();
    hash.update(b"connector-schema-caller-v1\0");
    for part in parts {
        hash.update((part.len() as u64).to_le_bytes());
        hash.update(part.as_bytes());
    }
    format!("{:x}", hash.finalize())
}

impl ConnectorSchemaStore {
    pub(crate) fn for_current_user(config: &AppConfig) -> Self {
        let scope = match &config.openai_tunnel {
            Some(tunnel) => private_key(&["tunnel", &tunnel.tunnel_id]),
            None => private_key(&[
                "http",
                &config.work_dir.to_string_lossy(),
                &config.port.to_string(),
            ]),
        };
        Self {
            directory: crate::util::home_dir()
                .map(|home| home.join(".codexify/connector-schemas").join(scope)),
            ..Self::default()
        }
    }

    pub(crate) fn version(&self, key: &str) -> Option<String> {
        let mut versions = self.versions.lock().unwrap();
        if let Some(version) = versions.get(key) {
            return Some(version.clone());
        }
        let version = std::fs::read_to_string(self.directory.as_ref()?.join(key)).ok()?;
        if version.is_empty() || version.len() > 64 {
            return None;
        }
        versions.insert(key.to_string(), version.clone());
        Some(version)
    }

    pub(crate) fn record_reload(&self, key: &str, version: &str) -> std::io::Result<()> {
        let mut versions = self.versions.lock().unwrap();
        // Persistence failure must not erase a reload this process just observed.
        versions.insert(key.to_string(), version.to_string());
        if let Some(directory) = &self.directory {
            std::fs::create_dir_all(directory)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))?;
            }
            let mut file = tempfile::NamedTempFile::new_in(directory)?;
            file.write_all(version.as_bytes())?;
            file.as_file().sync_all()?;
            file.persist(directory.join(key))
                .map_err(|error| error.error)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn meta(subject: &str, organization: &str, conversation: &str) -> RequestMetaObject {
        serde_json::from_value(json!({"openai/subject":subject,"openai/organization":organization,"openai/session":conversation})).unwrap()
    }

    #[test]
    fn caller_identity_spans_conversations_but_separates_accounts_and_workspaces() {
        assert_eq!(
            caller_key(&meta("a", "org", "first")),
            caller_key(&meta("a", "org", "second"))
        );
        assert_ne!(
            caller_key(&meta("a", "org", "first")),
            caller_key(&meta("b", "org", "first"))
        );
        assert_ne!(
            caller_key(&meta("a", "org", "first")),
            caller_key(&meta("a", "other", "first"))
        );
        assert!(
            caller_key(&serde_json::from_value(json!({"openai/session":"first"})).unwrap())
                .is_none()
        );
    }

    #[test]
    fn failed_persistence_does_not_discard_a_reload_observed_by_this_process() {
        let root = tempfile::tempdir().unwrap();
        let blocked = root.path().join("not-a-directory");
        std::fs::write(&blocked, "occupied").unwrap();
        let key = caller_key(&meta("a", "org", "chat")).unwrap();
        let store = ConnectorSchemaStore {
            directory: Some(blocked),
            ..Default::default()
        };
        assert!(store.record_reload(&key, "1.2.4").is_err());
        assert_eq!(store.version(&key).as_deref(), Some("1.2.4"));
    }

    #[test]
    fn reload_versions_survive_restart_and_do_not_update_other_connectors() {
        let root = tempfile::tempdir().unwrap();
        let a = caller_key(&meta("a", "org", "first")).unwrap();
        let b = caller_key(&meta("b", "org", "first")).unwrap();
        let store = ConnectorSchemaStore {
            directory: Some(root.path().into()),
            ..Default::default()
        };
        store.record_reload(&a, "1.2.3").unwrap();
        store.record_reload(&b, "1.2.3").unwrap();
        store.record_reload(&a, "1.2.4").unwrap();
        let restarted = ConnectorSchemaStore {
            directory: Some(root.path().into()),
            ..Default::default()
        };
        assert_eq!(restarted.version(&a).as_deref(), Some("1.2.4"));
        assert_eq!(restarted.version(&b).as_deref(), Some("1.2.3"));
        // A real server rollback is allowed; these are versions, not monotonic generations.
        restarted.record_reload(&a, "1.2.2").unwrap();
        assert_eq!(restarted.version(&a).as_deref(), Some("1.2.2"));
    }
}
