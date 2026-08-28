//! Working memory that survives a chat. Ports `src/memory.ts`.
//!
//! State lives under the user's home directory, keyed by work directory, so
//! nothing is written into the repository being worked on. Notes are a keyed
//! store rather than an append-only log: overwriting "current approach" is what
//! keeps memory small and current.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::types::{AppConfig, PlanState};
use crate::util::home_dir;

pub const DEFAULT_MEMORY_MAX_BYTES: usize = 16_384;
pub const MEMORY_FILENAME: &str = "memory.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryNote {
    pub value: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Memory {
    #[serde(rename = "workDir")]
    pub work_dir: String,
    #[serde(default)]
    pub plan: Option<PlanState>,
    #[serde(default)]
    pub notes: BTreeMap<String, MemoryNote>,
}

pub fn memory_enabled(config: &AppConfig) -> bool {
    config.memory.enabled != Some(false)
}

pub fn memory_max_bytes(config: &AppConfig) -> usize {
    config.memory.max_bytes.unwrap_or(DEFAULT_MEMORY_MAX_BYTES)
}

fn project_state_dir_name(work_dir: &std::path::Path) -> String {
    let normalized = crate::safe_path::lexical_normalize(work_dir);
    let abs = normalized.to_string_lossy().to_string();
    let mut hasher = Sha256::new();
    hasher.update(abs.as_bytes());
    let digest = hex::encode_short(&hasher.finalize(), 12);

    let basename = normalized
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let slug: String = basename
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let slug = if slug.is_empty() {
        "project".to_string()
    } else {
        slug
    };

    format!("{slug}-{digest}")
}

/// Per-project state directory, outside the work directory by default. The
/// basename is only there to make the directory recognisable to a human; the
/// hash of the absolute path is what keys it.
pub fn memory_dir(config: &AppConfig) -> PathBuf {
    if let Some(dir) = &config.memory.dir {
        let base = PathBuf::from(dir);
        return if config.multi_project {
            base.join(project_state_dir_name(&config.work_dir))
        } else {
            base
        };
    }

    let home = home_dir().unwrap_or_else(|| PathBuf::from("."));
    home.join(".codexify")
        .join("projects")
        .join(project_state_dir_name(&config.work_dir))
}

pub fn memory_path(config: &AppConfig) -> PathBuf {
    memory_dir(config).join(MEMORY_FILENAME)
}

fn empty_memory(config: &AppConfig) -> Memory {
    Memory {
        work_dir: config.work_dir.to_string_lossy().into_owned(),
        plan: None,
        notes: BTreeMap::new(),
    }
}

/// Read what was stored, degrading to empty rather than erroring: a corrupt or
/// unreadable memory file must not take the whole session down with it.
pub fn load_memory(config: &AppConfig) -> Memory {
    if !memory_enabled(config) {
        return empty_memory(config);
    }
    let Ok(raw) = std::fs::read_to_string(memory_path(config)) else {
        return empty_memory(config);
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return empty_memory(config);
    };
    let Some(obj) = value.as_object() else {
        return empty_memory(config);
    };

    // Parse leniently, per-field: a single malformed note must not drop the rest,
    // mirroring the TS which filters notes and takes the plan as-is.
    let work_dir = obj
        .get("workDir")
        .and_then(|v| v.as_str())
        .map(String::from)
        .unwrap_or_else(|| config.work_dir.to_string_lossy().into_owned());

    let plan = obj
        .get("plan")
        .and_then(|p| serde_json::from_value::<PlanState>(p.clone()).ok());

    let mut notes: BTreeMap<String, MemoryNote> = BTreeMap::new();
    if let Some(notes_obj) = obj.get("notes").and_then(|n| n.as_object()) {
        for (key, note) in notes_obj {
            let Some(note_obj) = note.as_object() else {
                continue;
            };
            let Some(val) = note_obj.get("value").and_then(|v| v.as_str()) else {
                continue;
            };
            let updated_at = match note_obj.get("updated_at") {
                Some(serde_json::Value::String(s)) => s.clone(),
                Some(serde_json::Value::Null) | None => String::new(),
                Some(other) => other.to_string(),
            };
            notes.insert(
                key.clone(),
                MemoryNote {
                    value: val.to_string(),
                    updated_at,
                },
            );
        }
    }

    Memory {
        work_dir,
        plan,
        notes,
    }
}

pub fn lock_path(config: &AppConfig) -> PathBuf {
    let mut p = memory_path(config).into_os_string();
    p.push(".lock");
    PathBuf::from(p)
}

// A held lock older than this is assumed to belong to a writer that crashed
// mid-write, and is stolen. A real write here is sub-millisecond.
const LOCK_STALE_MS: u128 = 10_000;
const LOCK_TIMEOUT_MS: u128 = 2_000;
const LOCK_RETRY_MS: u64 = 20;

static ATOMIC_COUNTER: AtomicU64 = AtomicU64::new(0);

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// Take the per-project write lock. Returns whether it was acquired; when it
/// could not be taken within the timeout the caller writes anyway, degrading to
/// last-writer-wins rather than dropping the note.
fn acquire_lock(config: &AppConfig) -> bool {
    let lock = lock_path(config);
    if std::fs::create_dir_all(memory_dir(config)).is_err() {
        return false;
    }
    let deadline = now_ms() + LOCK_TIMEOUT_MS;
    loop {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock)
        {
            Ok(mut file) => {
                // The holder record is only a debugging aid; the lock still holds.
                let _ = writeln!(file, "{} {}", std::process::id(), now_ms());
                return true;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                match std::fs::metadata(&lock).and_then(|m| m.modified()) {
                    Ok(modified) => {
                        let age = SystemTime::now()
                            .duration_since(modified)
                            .map(|d| d.as_millis())
                            .unwrap_or(0);
                        if age > LOCK_STALE_MS {
                            let _ = std::fs::remove_file(&lock);
                            continue;
                        }
                    }
                    Err(_) => continue, // vanished between open and stat; retry
                }
                if now_ms() >= deadline {
                    return false;
                }
                std::thread::sleep(Duration::from_millis(LOCK_RETRY_MS));
            }
            Err(_) => return false,
        }
    }
}

fn release_lock(config: &AppConfig) {
    let _ = std::fs::remove_file(lock_path(config));
}

/// Run a read-modify-write of the state file under the write lock.
fn with_memory_lock<T>(config: &AppConfig, f: impl FnOnce() -> T) -> T {
    if !memory_enabled(config) {
        return f();
    }
    let acquired = acquire_lock(config);
    let result = f();
    if acquired {
        release_lock(config);
    }
    result
}

/// Write memory back atomically. Returns false when persistence failed, never
/// panics. The write goes to a per-process temp file that is flushed and then
/// renamed over the target.
pub fn save_memory(config: &AppConfig, memory: &Memory) -> bool {
    if !memory_enabled(config) {
        return false;
    }
    let target = memory_path(config);
    let counter = ATOMIC_COUNTER.fetch_add(1, Ordering::SeqCst);
    let mut tmp = target.clone().into_os_string();
    tmp.push(format!(".tmp.{}.{}", std::process::id(), counter));
    let tmp = PathBuf::from(tmp);

    let write = || -> std::io::Result<()> {
        std::fs::create_dir_all(memory_dir(config))?;
        let json = serde_json::to_string_pretty(memory).unwrap_or_else(|_| "{}".to_string());
        {
            let mut file = std::fs::File::create(&tmp)?;
            file.write_all(json.as_bytes())?;
            file.write_all(b"\n")?;
            file.sync_all()?;
        }
        std::fs::rename(&tmp, &target)?;
        Ok(())
    };

    match write() {
        Ok(()) => true,
        Err(_) => {
            let _ = std::fs::remove_file(&tmp);
            false
        }
    }
}

pub fn notes_bytes(notes: &BTreeMap<String, MemoryNote>) -> usize {
    notes
        .iter()
        .map(|(key, note)| key.len() + note.value.len())
        .sum()
}

#[derive(Debug, Clone)]
pub struct RememberResult {
    pub ok: bool,
    pub message: String,
}

#[derive(Clone, Copy)]
enum NoteWriteMode {
    Create,
    Update,
    Upsert,
}

pub fn create_note(config: &AppConfig, key: &str, value: &str, now: &str) -> RememberResult {
    with_memory_lock(config, || {
        write_note_locked(config, key, value, now, NoteWriteMode::Create)
    })
}

pub fn update_note(config: &AppConfig, key: &str, value: &str, now: &str) -> RememberResult {
    with_memory_lock(config, || {
        write_note_locked(config, key, value, now, NoteWriteMode::Update)
    })
}

pub fn delete_note(config: &AppConfig, key: &str) -> RememberResult {
    with_memory_lock(config, || delete_note_locked(config, key))
}

/// Retained for callers of the original storage API. Connector tools expose
/// create, update, and delete as separate operations with distinct annotations.
pub fn remember(config: &AppConfig, key: &str, value: &str, now: &str) -> RememberResult {
    if value.trim().is_empty() {
        delete_note(config, key)
    } else {
        with_memory_lock(config, || {
            write_note_locked(config, key, value, now, NoteWriteMode::Upsert)
        })
    }
}

fn write_note_locked(
    config: &AppConfig,
    key: &str,
    value: &str,
    now: &str,
    mode: NoteWriteMode,
) -> RememberResult {
    let mut memory = load_memory(config);
    let trimmed_key = key.trim().to_string();
    let existing = memory.notes.get(&trimmed_key);
    match mode {
        NoteWriteMode::Create if existing.is_some() => {
            return RememberResult {
                ok: false,
                message: format!(
                    "Note {trimmed_key:?} already exists. Use update_memory_note to replace it."
                ),
            };
        }
        NoteWriteMode::Update if existing.is_none() => {
            return RememberResult {
                ok: false,
                message: format!(
                    "No note named {trimmed_key:?} exists. Use remember to create it."
                ),
            };
        }
        NoteWriteMode::Update if existing.is_some_and(|note| note.value == value) => {
            return RememberResult {
                ok: true,
                message: format!("Note {trimmed_key:?} already has that value."),
            };
        }
        NoteWriteMode::Create | NoteWriteMode::Update | NoteWriteMode::Upsert => {}
    }

    let mut candidate = memory.notes.clone();
    candidate.insert(
        trimmed_key.clone(),
        MemoryNote {
            value: value.to_string(),
            updated_at: now.to_string(),
        },
    );
    let budget = memory_max_bytes(config);
    let size = notes_bytes(&candidate);
    if size > budget {
        let keys = if memory.notes.is_empty() {
            "none".to_string()
        } else {
            memory.notes.keys().cloned().collect::<Vec<_>>().join(", ")
        };
        return RememberResult {
            ok: false,
            message: format!(
                "Note rejected: notes would total {size} bytes, over the {budget}-byte budget. Remove or shorten one first with forget_memory_note or update_memory_note. Current keys: {keys}."
            ),
        };
    }

    memory.notes = candidate;
    let stored = save_memory(config, &memory);
    RememberResult {
        ok: stored,
        message: if stored {
            format!("Stored note {trimmed_key:?} ({size}/{budget} bytes used).")
        } else {
            format!(
                "Could not write memory to {}; the note is not saved.",
                memory_path(config).display()
            )
        },
    }
}

fn delete_note_locked(config: &AppConfig, key: &str) -> RememberResult {
    let mut memory = load_memory(config);
    let trimmed_key = key.trim().to_string();
    if !memory.notes.contains_key(&trimmed_key) {
        return RememberResult {
            ok: false,
            message: format!("No note named {trimmed_key:?} to remove."),
        };
    }
    memory.notes.remove(&trimmed_key);
    let stored = save_memory(config, &memory);
    RememberResult {
        ok: stored,
        message: if stored {
            format!("Removed note {trimmed_key:?}.")
        } else {
            format!(
                "Could not write memory to {}; the note was not removed.",
                memory_path(config).display()
            )
        },
    }
}

/// Persist the plan alongside the notes, leaving the notes untouched.
pub fn save_plan(config: &AppConfig, plan: Option<PlanState>) -> bool {
    with_memory_lock(config, || {
        let mut memory = load_memory(config);
        memory.plan = plan;
        save_memory(config, &memory)
    })
}

/// Render memory for a model that may have lost the conversation it came from,
/// so it reads as a handover rather than a data dump.
pub fn render_memory(memory: &Memory) -> Option<String> {
    let mut sections: Vec<String> = Vec::new();

    if let Some(plan) = &memory.plan
        && !plan.plan.is_empty()
    {
        let mut lines: Vec<String> = Vec::new();
        lines.push("Plan in progress:".to_string());
        // An empty-string explanation is falsy in the TS and is not rendered.
        if let Some(explanation) = &plan.explanation
            && !explanation.is_empty()
        {
            lines.push(explanation.clone());
        }
        for item in &plan.plan {
            let marker = match item.status.as_str() {
                "in_progress" => "[~]",
                "completed" => "[x]",
                _ => "[ ]",
            };
            lines.push(format!("{marker} {}", item.step));
        }
        let done = plan
            .plan
            .iter()
            .filter(|i| i.status.as_str() == "completed")
            .count();
        lines.push(format!("{done}/{} steps completed", plan.plan.len()));
        sections.push(lines.join("\n"));
    }

    if !memory.notes.is_empty() {
        let mut lines: Vec<String> = vec!["Notes:".to_string()];
        // BTreeMap already iterates keys in sorted order.
        for (key, note) in &memory.notes {
            lines.push(format!("- {key}: {}", note.value));
        }
        sections.push(lines.join("\n"));
    }

    if sections.is_empty() {
        None
    } else {
        Some(sections.join("\n\n"))
    }
}

/// Minimal hex helper, kept local to avoid a dependency.
mod hex {
    pub fn encode_short(bytes: &[u8], chars: usize) -> String {
        let mut s = String::with_capacity(chars);
        for b in bytes {
            if s.len() >= chars {
                break;
            }
            s.push_str(&format!("{b:02x}"));
        }
        s.truncate(chars);
        s
    }
}
