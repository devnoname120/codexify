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

pub(crate) fn schema_version(config: &AppConfig) -> String {
    let mut version = version_for_markdown_chat(config.markdown_chat.enabled);
    if config.experimental.agent_tickets {
        version.push_str("+tickets-v1");
    }
    if config.multi_project {
        format!("{version}+workspace-v1")
    } else {
        version
    }
}

pub(crate) fn version_for_markdown_chat(enabled: bool) -> String {
    let version = env!("CARGO_PKG_VERSION");
    if enabled {
        format!("{version}+markdown-chat-v5")
    } else {
        version.to_string()
    }
}

#[derive(Default)]
pub(crate) struct ConnectorSchemaStore {
    versions: Mutex<HashMap<String, String>>,
    directory: Option<PathBuf>,
    tunnel_scoped: bool,
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
    pub(crate) fn new(directory: Option<PathBuf>, tunnel_scoped: bool) -> Self {
        Self {
            directory,
            tunnel_scoped,
            ..Default::default()
        }
    }
    pub(crate) fn for_tunnel(tunnel_id: &str) -> Self {
        Self::new(
            crate::util::home_dir().map(|home| {
                home.join(".codexify/connector-schemas")
                    .join(private_key(&["tunnel", tunnel_id]))
            }),
            true,
        )
    }

    pub(crate) fn discovered(&self, caller: Option<&str>, version: &str) -> std::io::Result<()> {
        match if self.tunnel_scoped {
            Some("tunnel")
        } else {
            caller
        } {
            Some(key) => self.record_reload(key, version),
            None => Ok(()),
        }
    }

    pub(crate) fn connector_version(&self, caller: Option<&str>) -> Option<String> {
        self.version(if self.tunnel_scoped {
            "tunnel"
        } else {
            caller?
        })
    }

    pub(crate) fn conversation_version(
        &self,
        conversation: &crate::project_bindings::ConversationIdentity,
    ) -> Option<String> {
        self.version(&format!("conversation-{}", conversation.stable_key()))
    }

    pub(crate) fn remember_conversation_version(
        &self,
        conversation: &crate::project_bindings::ConversationIdentity,
        version: &str,
    ) -> std::io::Result<()> {
        let key = format!("conversation-{}", conversation.stable_key());
        if self.version(&key).is_none() {
            self.record_reload(&key, version)?;
        }
        Ok(())
    }
    pub(crate) fn for_current_user(config: &AppConfig) -> Self {
        let tunnels = config.configured_openai_tunnels().collect::<Vec<_>>();
        let scope = match tunnels.as_slice() {
            [tunnel] => private_key(&["tunnel", &tunnel.tunnel_id]),
            [] => private_key(&[
                "http",
                &config.work_dir.to_string_lossy(),
                &config.port.to_string(),
            ]),
            _ => {
                let mut ids = tunnels
                    .iter()
                    .map(|tunnel| tunnel.tunnel_id.as_str())
                    .collect::<Vec<_>>();
                ids.sort_unstable();
                let mut parts = vec!["tunnels"];
                parts.extend(ids);
                private_key(&parts)
            }
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

    #[test]
    fn markdown_toggle_changes_schema_without_changing_release_version() {
        assert_ne!(
            version_for_markdown_chat(false),
            version_for_markdown_chat(true)
        );
        assert_eq!(version_for_markdown_chat(false), env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn anonymous_tunnel_discovery_is_persistent_and_independent_of_conversation_echoes() {
        let dir = tempfile::tempdir().unwrap();
        let store = |name: &str| ConnectorSchemaStore {
            directory: Some(dir.path().join(name)),
            tunnel_scoped: true,
            ..Default::default()
        };
        let first = store("first");
        let second = store("second");
        let chat =
            crate::project_bindings::ConversationIdentity::from_openai_session("old-chat").unwrap();
        first.remember_conversation_version(&chat, "old").unwrap();
        assert!(first.connector_version(None).is_none());
        first.discovered(None, "new").unwrap();
        second.discovered(None, "old").unwrap();
        assert_eq!(
            store("first")
                .connector_version(Some("a-caller"))
                .as_deref(),
            Some("new")
        );
        assert_eq!(
            store("second").connector_version(None).as_deref(),
            Some("old")
        );
        first.remember_conversation_version(&chat, "new").unwrap();
        assert_eq!(first.conversation_version(&chat).as_deref(), Some("old"));
        assert_eq!(first.connector_version(None).as_deref(), Some("new"));
        first.discovered(None, "rollback").unwrap();
        assert_eq!(
            store("first").connector_version(None).as_deref(),
            Some("rollback")
        );
        let direct = ConnectorSchemaStore::default();
        direct.discovered(None, "new").unwrap();
        assert!(direct.connector_version(None).is_none());
    }

    #[test]
    fn tunnel_records_do_not_depend_on_other_configured_tunnels() {
        let a = ConnectorSchemaStore::for_tunnel("tunnel_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let b = ConnectorSchemaStore::for_tunnel("tunnel_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        assert_ne!(a.directory, b.directory);
        assert_eq!(
            a.directory,
            ConnectorSchemaStore::for_tunnel("tunnel_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").directory
        );
    }

    #[test]
    fn multi_tunnel_schema_scope_is_independent_of_list_order() {
        let mut config = crate::config::default_config(std::path::PathBuf::from("/tmp/project"));
        let first = crate::types::OpenAiTunnelConfig {
            tunnel_id: "tunnel_0123456789abcdef0123456789abcdef".into(),
            api_key_ref: "env:FIRST_KEY".into(),
            organization_id: None,
            client_path: None,
        };
        let second = crate::types::OpenAiTunnelConfig {
            tunnel_id: "tunnel_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            api_key_ref: "env:SECOND_KEY".into(),
            organization_id: None,
            client_path: None,
        };
        config.openai_tunnel = Some(first.clone());
        config.additional_openai_tunnels.push(second.clone());
        let scope = ConnectorSchemaStore::for_current_user(&config).directory;
        config.openai_tunnel = Some(second);
        config.additional_openai_tunnels = vec![first];
        assert_eq!(
            scope,
            ConnectorSchemaStore::for_current_user(&config).directory
        );
    }

    #[test]
    fn conversation_baseline_survives_restart_without_becoming_a_connector_reload() {
        let root = tempfile::tempdir().unwrap();
        let identity =
            crate::project_bindings::ConversationIdentity::from_openai_session("chat").unwrap();
        let store = ConnectorSchemaStore {
            directory: Some(root.path().into()),
            ..Default::default()
        };
        store
            .remember_conversation_version(&identity, "1.4.0")
            .unwrap();
        store
            .remember_conversation_version(&identity, "1.4.0+markdown-chat")
            .unwrap();
        let restarted = ConnectorSchemaStore {
            directory: Some(root.path().into()),
            ..Default::default()
        };
        assert_eq!(
            restarted.conversation_version(&identity).as_deref(),
            Some("1.4.0")
        );
        assert!(
            restarted
                .version(&caller_key(&meta("user", "org", "chat")).unwrap())
                .is_none()
        );
    }

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
