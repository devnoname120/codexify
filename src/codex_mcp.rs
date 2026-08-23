//! Import user-level MCP server definitions from Codex's `config.toml`.

use std::collections::{BTreeSet, HashMap};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::types::McpServerSpec;
use crate::util::home_dir;

#[derive(Debug, Default)]
pub struct CodexMcpImport {
    pub servers: HashMap<String, McpServerSpec>,
    pub report: Vec<String>,
}

pub fn codex_config_path() -> Result<PathBuf, String> {
    let codex_home = std::env::var_os("CODEX_HOME").filter(|value| !value.as_os_str().is_empty());
    codex_config_path_from(codex_home, home_dir())
}

fn codex_config_path_from(
    codex_home: Option<OsString>,
    default_home: Option<PathBuf>,
) -> Result<PathBuf, String> {
    if let Some(value) = codex_home {
        let path = PathBuf::from(value);
        let metadata = std::fs::metadata(&path).map_err(|_| {
            format!(
                "CODEX_HOME points to {}, but that path does not exist or cannot be read",
                path.display()
            )
        })?;
        if !metadata.is_dir() {
            return Err(format!(
                "CODEX_HOME points to {}, but that path is not a directory",
                path.display()
            ));
        }
        let canonical = path
            .canonicalize()
            .map_err(|_| format!("failed to canonicalize CODEX_HOME at {}", path.display()))?;
        return Ok(canonical.join("config.toml"));
    }

    let home =
        default_home.ok_or_else(|| "could not find the user's home directory".to_string())?;
    Ok(home.join(".codex").join("config.toml"))
}

pub fn discover_codex_mcp_servers(path: &Path) -> Result<Option<CodexMcpImport>, String> {
    let contents = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => {
            return Err(format!(
                "failed to read Codex configuration at {}",
                path.display()
            ));
        }
    };

    parse_codex_mcp_servers(&contents, &|name| std::env::var(name).ok()).map(Some)
}

fn parse_codex_mcp_servers(
    contents: &str,
    env_lookup: &dyn Fn(&str) -> Option<String>,
) -> Result<CodexMcpImport, String> {
    let root: toml::Value = toml::from_str(contents)
        .map_err(|_| "Codex config.toml contains invalid TOML".to_string())?;
    let root = root
        .as_table()
        .ok_or_else(|| "Codex config.toml must contain a TOML table".to_string())?;
    let Some(value) = root.get("mcp_servers") else {
        return Ok(CodexMcpImport::default());
    };
    let Some(servers) = value.as_table() else {
        return Err("Codex `mcp_servers` must be a TOML table".to_string());
    };

    let mut names: Vec<&String> = servers.keys().collect();
    names.sort();

    let mut outcome = CodexMcpImport::default();
    for name in names {
        let value = &servers[name];
        let Some(table) = value.as_table() else {
            outcome
                .report
                .push(format!("{name} -> skipped: server entry must be a table"));
            continue;
        };

        match parse_server(table, env_lookup) {
            Ok(ServerImport::Imported {
                spec,
                ignored_fields,
            }) => {
                let mut line = if spec.disabled {
                    format!("{name} -> imported from Codex (disabled)")
                } else {
                    format!("{name} -> imported from Codex")
                };
                if !ignored_fields.is_empty() {
                    line.push_str("; unsupported fields ignored: ");
                    line.push_str(&ignored_fields.join(", "));
                }
                outcome.servers.insert(name.clone(), *spec);
                outcome.report.push(line);
            }
            Ok(ServerImport::Skipped(reason)) | Err(reason) => {
                outcome.report.push(format!("{name} -> skipped: {reason}"));
            }
        }
    }

    Ok(outcome)
}

enum ServerImport {
    Imported {
        spec: Box<McpServerSpec>,
        ignored_fields: Vec<String>,
    },
    Skipped(String),
}

fn parse_server(
    table: &toml::Table,
    env_lookup: &dyn Fn(&str) -> Option<String>,
) -> Result<ServerImport, String> {
    let command = optional_string(table, "command")?;
    let url = optional_string(table, "url")?;
    if command.is_some() && url.is_some() {
        return Ok(ServerImport::Skipped(
            "both `command` and `url` are configured".to_string(),
        ));
    }
    if url.is_some() {
        return Ok(ServerImport::Skipped(
            "streamable HTTP transport is not supported yet".to_string(),
        ));
    }
    let Some(command) = command.filter(|value| !value.trim().is_empty()) else {
        return Ok(ServerImport::Skipped(
            "no non-empty stdio `command` is configured".to_string(),
        ));
    };

    let environment_id = optional_string(table, "environment_id")?;
    let experimental_environment = optional_string(table, "experimental_environment")?;
    if environment_id
        .as_deref()
        .is_some_and(|value| value != "local")
        || experimental_environment.as_deref() == Some("remote")
    {
        return Ok(ServerImport::Skipped(
            "non-local execution environments are not supported".to_string(),
        ));
    }

    let mut env = optional_string_map(table, "env")?.unwrap_or_default();
    for env_var in optional_env_vars(table)?.unwrap_or_default() {
        match env_var.source.as_deref() {
            None | Some("local") => {
                if !env.contains_key(&env_var.name)
                    && let Some(value) = env_lookup(&env_var.name)
                {
                    env.insert(env_var.name, value);
                }
            }
            Some("remote") => {
                return Ok(ServerImport::Skipped(
                    "remote-sourced `env_vars` are not supported".to_string(),
                ));
            }
            Some(_) => {
                return Err("`env_vars.source` must be `local` or `remote`".to_string());
            }
        }
    }

    let supported_fields = [
        "args",
        "command",
        "cwd",
        "disabled_tools",
        "enabled",
        "enabled_tools",
        "env",
        "env_vars",
        "environment_id",
        "experimental_environment",
        "url",
    ];
    let supported: BTreeSet<&str> = supported_fields.into_iter().collect();
    let mut ignored_fields = table
        .keys()
        .filter(|key| !supported.contains(key.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    ignored_fields.sort();

    Ok(ServerImport::Imported {
        spec: Box::new(McpServerSpec {
            command: Some(command),
            args: optional_string_list(table, "args")?.unwrap_or_default(),
            env,
            cwd: optional_string(table, "cwd")?,
            disabled: !optional_bool(table, "enabled")?.unwrap_or(true),
            transport: None,
            url: None,
            tools: optional_string_list(table, "enabled_tools")?,
            disabled_tools: optional_string_list(table, "disabled_tools")?,
            mode: None,
        }),
        ignored_fields,
    })
}

#[derive(Debug)]
struct EnvVarRef {
    name: String,
    source: Option<String>,
}

fn optional_env_vars(table: &toml::Table) -> Result<Option<Vec<EnvVarRef>>, String> {
    let Some(value) = table.get("env_vars") else {
        return Ok(None);
    };
    let Some(values) = value.as_array() else {
        return Err("`env_vars` must be an array".to_string());
    };

    let mut result = Vec::with_capacity(values.len());
    for value in values {
        if let Some(name) = value.as_str() {
            result.push(EnvVarRef {
                name: name.to_string(),
                source: None,
            });
            continue;
        }
        let Some(config) = value.as_table() else {
            return Err("each `env_vars` entry must be a string or table".to_string());
        };
        let name = optional_string(config, "name")?
            .ok_or_else(|| "an `env_vars` table is missing `name`".to_string())?;
        let source = optional_string(config, "source")?;
        result.push(EnvVarRef { name, source });
    }
    Ok(Some(result))
}

fn optional_string(table: &toml::Table, key: &str) -> Result<Option<String>, String> {
    match table.get(key) {
        None => Ok(None),
        Some(value) => value
            .as_str()
            .map(|value| Some(value.to_string()))
            .ok_or_else(|| format!("`{key}` must be a string")),
    }
}

fn optional_bool(table: &toml::Table, key: &str) -> Result<Option<bool>, String> {
    match table.get(key) {
        None => Ok(None),
        Some(value) => value
            .as_bool()
            .map(Some)
            .ok_or_else(|| format!("`{key}` must be a boolean")),
    }
}

fn optional_string_list(table: &toml::Table, key: &str) -> Result<Option<Vec<String>>, String> {
    let Some(value) = table.get(key) else {
        return Ok(None);
    };
    let Some(values) = value.as_array() else {
        return Err(format!("`{key}` must be an array of strings"));
    };
    let mut result = Vec::with_capacity(values.len());
    for value in values {
        let Some(value) = value.as_str() else {
            return Err(format!("`{key}` must contain only strings"));
        };
        result.push(value.to_string());
    }
    Ok(Some(result))
}

fn optional_string_map(
    table: &toml::Table,
    key: &str,
) -> Result<Option<HashMap<String, String>>, String> {
    let Some(value) = table.get(key) else {
        return Ok(None);
    };
    let Some(values) = value.as_table() else {
        return Err(format!("`{key}` must be a string map"));
    };
    let mut result = HashMap::with_capacity(values.len());
    for (name, value) in values {
        let Some(value) = value.as_str() else {
            return Err(format!("`{key}` must contain only string values"));
        };
        result.insert(name.clone(), value.to_string());
    }
    Ok(Some(result))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_codex_home_is_canonicalized() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = codex_config_path_from(
            Some(temp.path().as_os_str().to_os_string()),
            Some(PathBuf::from("/unused")),
        )
        .unwrap();
        assert_eq!(
            path,
            temp.path().canonicalize().unwrap().join("config.toml")
        );
    }

    #[test]
    fn default_codex_home_uses_dot_codex() {
        let path = codex_config_path_from(None, Some(PathBuf::from("/home/tester"))).unwrap();
        assert_eq!(path, PathBuf::from("/home/tester/.codex/config.toml"));
    }

    #[test]
    fn imports_stdio_fields_and_filters_without_leaking_values() {
        let contents = r#"
[mcp_servers.demo]
command = "npx"
args = ["-y", "demo-server"]
cwd = "/tmp/demo"
enabled_tools = ["read", "write"]
disabled_tools = ["write"]
required = true
env_vars = ["TOKEN", { name = "SECOND_TOKEN", source = "local" }]

[mcp_servers.demo.env]
STATIC_SECRET = "do-not-log-this"
"#;
        let outcome = parse_codex_mcp_servers(contents, &|name| match name {
            "TOKEN" => Some("resolved-secret".to_string()),
            "SECOND_TOKEN" => Some("second-secret".to_string()),
            _ => None,
        })
        .unwrap();
        let server = outcome.servers.get("demo").unwrap();
        assert_eq!(server.command.as_deref(), Some("npx"));
        assert_eq!(server.args, ["-y", "demo-server"]);
        assert_eq!(server.cwd.as_deref(), Some("/tmp/demo"));
        assert_eq!(
            server.tools.as_ref().unwrap(),
            &vec!["read".to_string(), "write".to_string()]
        );
        assert_eq!(
            server.disabled_tools.as_ref().unwrap(),
            &vec!["write".to_string()]
        );
        assert_eq!(
            server.env.get("TOKEN").map(String::as_str),
            Some("resolved-secret")
        );
        assert_eq!(
            server.env.get("SECOND_TOKEN").map(String::as_str),
            Some("second-secret")
        );
        assert_eq!(
            server.env.get("STATIC_SECRET").map(String::as_str),
            Some("do-not-log-this")
        );
        let report = outcome.report.join("\n");
        assert!(report.contains("required"));
        assert!(!report.contains("resolved-secret"));
        assert!(!report.contains("second-secret"));
        assert!(!report.contains("do-not-log-this"));
    }

    #[test]
    fn imports_disabled_server_but_skips_http_and_remote_servers() {
        let contents = r#"
[mcp_servers.off]
command = "off-server"
enabled = false

[mcp_servers.web]
url = "https://example.invalid/mcp"

[mcp_servers.remote]
command = "remote-server"
environment_id = "executor"
"#;
        let outcome = parse_codex_mcp_servers(contents, &|_| None).unwrap();
        assert!(outcome.servers.get("off").unwrap().disabled);
        assert!(!outcome.servers.contains_key("web"));
        assert!(!outcome.servers.contains_key("remote"));
        let report = outcome.report.join("\n");
        assert!(report.contains("web -> skipped: streamable HTTP"));
        assert!(report.contains("remote -> skipped: non-local"));
    }

    #[test]
    fn bad_server_does_not_hide_valid_sibling() {
        let contents = r#"
[mcp_servers.bad]
command = 42

[mcp_servers.good]
command = "good-server"
"#;
        let outcome = parse_codex_mcp_servers(contents, &|_| None).unwrap();
        assert!(!outcome.servers.contains_key("bad"));
        assert!(outcome.servers.contains_key("good"));
        assert!(
            outcome
                .report
                .join("\n")
                .contains("bad -> skipped: `command`")
        );
    }

    #[test]
    fn remote_sourced_env_var_skips_only_that_server() {
        let contents = r#"
[mcp_servers.remote_env]
command = "remote-env-server"
env_vars = [{ name = "TOKEN", source = "remote" }]

[mcp_servers.good]
command = "good-server"
"#;
        let outcome = parse_codex_mcp_servers(contents, &|_| None).unwrap();
        assert!(!outcome.servers.contains_key("remote_env"));
        assert!(outcome.servers.contains_key("good"));
        assert!(
            outcome
                .report
                .join("\n")
                .contains("remote_env -> skipped: remote-sourced `env_vars`")
        );
    }

    #[test]
    fn invalid_toml_error_does_not_echo_source() {
        let secret = "secret-that-must-not-appear";
        let contents = format!("[mcp_servers.demo]\ncommand = \\\"{secret}");
        let error = parse_codex_mcp_servers(&contents, &|_| None).unwrap_err();
        assert!(!error.contains(secret));
    }

    #[test]
    fn missing_file_is_not_an_error() {
        let temp = tempfile::TempDir::new().unwrap();
        let result = discover_codex_mcp_servers(&temp.path().join("missing.toml")).unwrap();
        assert!(result.is_none());
    }
}
