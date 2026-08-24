//! Shared reader for Codex's user-level `config.toml`.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::util::home_dir;

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

pub fn load_codex_config(path: &Path) -> Result<Option<toml::Table>, String> {
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

    parse_codex_config(&contents).map(Some)
}

pub fn parse_codex_config(contents: &str) -> Result<toml::Table, String> {
    let root: toml::Value = toml::from_str(contents)
        .map_err(|_| "Codex config.toml contains invalid TOML".to_string())?;
    root.as_table()
        .cloned()
        .ok_or_else(|| "Codex config.toml must contain a TOML table".to_string())
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
    fn missing_file_is_not_an_error() {
        let temp = tempfile::TempDir::new().unwrap();
        let result = load_codex_config(&temp.path().join("missing.toml")).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn invalid_toml_error_does_not_echo_source() {
        let secret = "secret-that-must-not-appear";
        let contents = format!("[mcp_servers.demo]\ncommand = \\\"{secret}");
        let error = parse_codex_config(&contents).unwrap_err();
        assert!(!error.contains(secret));
    }
}
