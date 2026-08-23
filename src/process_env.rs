//! Environment boundaries for subprocesses.

use std::ffi::{OsStr, OsString};

use tokio::process::Command;

use crate::types::AppConfig;

pub const CHILD_CONTROL_PLANE_API_KEY_ENV: &str = "CODEXIFY_OPENAI_TUNNEL_API_KEY";
pub const CHILD_MCP_AUTHORIZATION_ENV: &str = "CODEXIFY_INTERNAL_MCP_AUTHORIZATION";

const TUNNEL_ENV_PASSTHROUGH: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "USERPROFILE",
    "TMPDIR",
    "TEMP",
    "TMP",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "SYSTEMROOT",
    "WINDIR",
    "COMSPEC",
    "PATHEXT",
];

pub fn scrub_untrusted_child_env(command: &mut Command, config: &AppConfig) {
    if let Some(name) = referenced_tunnel_key_env(config) {
        command.env_remove(name);
    }
    command
        .env_remove(CHILD_CONTROL_PLANE_API_KEY_ENV)
        .env_remove(CHILD_MCP_AUTHORIZATION_ENV);
}

pub fn isolate_tunnel_child_env(command: &mut Command) {
    let preserved = TUNNEL_ENV_PASSTHROUGH
        .iter()
        .filter_map(|name| std::env::var_os(name).map(|value| (OsString::from(name), value)))
        .collect::<Vec<_>>();

    command.env_clear();
    command.envs(preserved);
}

fn referenced_tunnel_key_env(config: &AppConfig) -> Option<&OsStr> {
    config
        .openai_tunnel
        .as_ref()?
        .api_key_ref
        .strip_prefix("env:")
        .map(OsStr::new)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::config::default_config;
    use crate::types::OpenAiTunnelConfig;

    #[test]
    fn untrusted_children_explicitly_remove_the_referenced_tunnel_key() {
        let mut config = default_config(PathBuf::from("/tmp"));
        config.openai_tunnel = Some(OpenAiTunnelConfig {
            tunnel_id: "tunnel_0123456789abcdefghijklmnopqrstuv".into(),
            api_key_ref: "env:PRIVATE_TUNNEL_KEY".into(),
            organization_id: None,
            client_path: None,
        });
        let mut command = Command::new("ignored");

        scrub_untrusted_child_env(&mut command, &config);

        let removed = command
            .as_std()
            .get_envs()
            .filter_map(|(name, value)| value.is_none().then_some(name))
            .collect::<Vec<_>>();
        assert!(removed.contains(&OsStr::new("PRIVATE_TUNNEL_KEY")));
        assert!(removed.contains(&OsStr::new(CHILD_CONTROL_PLANE_API_KEY_ENV)));
        assert!(removed.contains(&OsStr::new(CHILD_MCP_AUTHORIZATION_ENV)));
    }

    #[test]
    fn tunnel_passthrough_excludes_configuration_and_secret_variables() {
        assert!(!TUNNEL_ENV_PASSTHROUGH.contains(&"CONTROL_PLANE_BASE_URL"));
        assert!(!TUNNEL_ENV_PASSTHROUGH.contains(&"CONTROL_PLANE_API_KEY"));
        assert!(!TUNNEL_ENV_PASSTHROUGH.contains(&"OPENAI_API_KEY"));
        assert!(!TUNNEL_ENV_PASSTHROUGH.contains(&"HTTP_PROXY"));
        assert!(!TUNNEL_ENV_PASSTHROUGH.contains(&"SSL_CERT_FILE"));
    }
}
