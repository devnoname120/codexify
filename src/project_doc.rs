//! AGENTS.md discovery, ported from `src/project-doc.ts`.
//!
//! Before the first turn Codex collects the AGENTS.md files along the path from
//! the project root to the working directory. An MCP server has no way to send a
//! message, so the text is surfaced through the server's `instructions` and the
//! `get_project_doc` tool instead.

use std::path::{Path, PathBuf};

use crate::types::{AppConfig, ProjectDocConfig};

/// Codex's `project_doc_max_bytes` default. Zero disables loading entirely.
pub const PROJECT_DOC_MAX_BYTES: usize = 32_768;
/// Codex's `project_root_markers` default. An empty list disables the walk up.
pub const DEFAULT_ROOT_MARKERS: &[&str] = &[".git"];
/// Codex's preferred local override, tried ahead of AGENTS.md in each directory.
pub const OVERRIDE_FILENAME: &str = "AGENTS.override.md";
pub const DEFAULT_FILENAME: &str = "AGENTS.md";
/// Tells the model where workspace-scoped instructions begin, as Codex does.
pub const PROJECT_DOC_SEPARATOR: &str = "--- project-doc ---";

pub fn candidate_filenames(settings: &ProjectDocConfig) -> Vec<String> {
    let mut names = vec![OVERRIDE_FILENAME.to_string(), DEFAULT_FILENAME.to_string()];
    if let Some(fallbacks) = &settings.fallback_filenames {
        for name in fallbacks {
            if !name.is_empty() && !names.contains(name) {
                names.push(name.clone());
            }
        }
    }
    names
}

fn root_markers(settings: &ProjectDocConfig) -> Vec<String> {
    settings
        .root_markers
        .clone()
        .unwrap_or_else(|| DEFAULT_ROOT_MARKERS.iter().map(|s| s.to_string()).collect())
}

/// Nearest ancestor of `start_dir` (inclusive) holding one of `markers`, or
/// `None` when none does.
pub fn find_project_root(start_dir: &Path, markers: &[String]) -> Option<PathBuf> {
    if markers.is_empty() {
        return None;
    }
    let mut cursor = start_dir.to_path_buf();
    loop {
        if markers.iter().any(|m| cursor.join(m).exists()) {
            return Some(cursor);
        }
        match cursor.parent() {
            Some(parent) if parent != cursor => cursor = parent.to_path_buf(),
            _ => return None,
        }
    }
}

/// Directories from the project root down to the work directory, outermost
/// first. Shared by AGENTS.md discovery and (in `skills.rs`) repo skill roots.
pub fn project_dirs(config: &AppConfig) -> Vec<PathBuf> {
    if config.multi_project {
        return vec![config.work_dir.clone()];
    }

    let settings = &config.project_doc;
    let root = find_project_root(&config.work_dir, &root_markers(settings));

    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut cursor = config.work_dir.clone();
    loop {
        dirs.push(cursor.clone());
        match &root {
            Some(r) if &cursor == r => break,
            None => break,
            _ => {}
        }
        match cursor.parent() {
            Some(parent) if parent != cursor => cursor = parent.to_path_buf(),
            _ => break,
        }
    }
    dirs.reverse();
    dirs
}

/// Project docs from the project root down to the work directory, at most one
/// per directory, in that order.
pub fn project_doc_paths(config: &AppConfig) -> Vec<PathBuf> {
    let settings = &config.project_doc;
    let dirs = project_dirs(config);
    let names = candidate_filenames(settings);
    let mut found: Vec<PathBuf> = Vec::new();
    for dir in dirs {
        for name in &names {
            let candidate = dir.join(name);
            if candidate.is_file() {
                found.push(candidate);
                break;
            }
        }
    }
    found
}

#[derive(Debug, Clone)]
pub struct ProjectDocEntry {
    pub path: PathBuf,
    pub contents: String,
    /// True when the byte budget cut this file short.
    pub truncated: bool,
}

#[derive(Debug, Clone)]
pub struct ProjectDoc {
    pub entries: Vec<ProjectDocEntry>,
    /// The entries concatenated, which is what the model is meant to read.
    pub text: String,
}

/// Take a byte-boundary-safe prefix of at most `max_bytes` bytes.
fn byte_prefix(data: &[u8], max_bytes: usize) -> &[u8] {
    &data[..data.len().min(max_bytes)]
}

/// Reads every project doc under a shared byte budget, returning `None` when
/// there is nothing to say.
pub fn load_project_doc(config: &AppConfig) -> Option<ProjectDoc> {
    let max_bytes = config
        .project_doc
        .max_bytes
        .unwrap_or(PROJECT_DOC_MAX_BYTES);
    if max_bytes == 0 {
        return None;
    }

    let mut remaining = max_bytes;
    let mut entries: Vec<ProjectDocEntry> = Vec::new();

    for path in project_doc_paths(config) {
        if remaining == 0 {
            break;
        }
        let Ok(data) = std::fs::read(&path) else {
            continue;
        };
        let truncated = data.len() > remaining;
        let slice = byte_prefix(&data, remaining);
        let contents = String::from_utf8_lossy(slice).into_owned();
        if contents.trim().is_empty() {
            continue;
        }
        remaining -= slice.len();
        entries.push(ProjectDocEntry {
            path,
            contents,
            truncated,
        });
    }

    if entries.is_empty() {
        return None;
    }
    let text = entries
        .iter()
        .map(|e| e.contents.clone())
        .collect::<Vec<_>>()
        .join("\n\n");
    Some(ProjectDoc { entries, text })
}
