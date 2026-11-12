//! Optional working-tree counters. No Git work is added to model tool calls.
use std::collections::HashMap;
use std::path::{Path as FilePath, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::{Mutex, Semaphore};

use super::*;

const FRESH_FOR: Duration = Duration::from_secs(20);
const WORK_BUDGET: Duration = Duration::from_secs(2);
const MAX_OUTPUT: u64 = 1024 * 1024;
const MAX_UNTRACKED_BYTES: u64 = 4 * 1024 * 1024;
const MAX_UNTRACKED_FILES: usize = 256;
const MAX_PROJECTS: usize = 64;

struct Entry {
    value: Value,
    checked: Instant,
    refreshing: bool,
}

type Cache = Arc<Mutex<HashMap<PathBuf, Entry>>>;
static CACHE: OnceLock<Cache> = OnceLock::new();
static WORKERS: Semaphore = Semaphore::const_new(2);

pub(super) async fn read(State(state): State<OwnerState>, Path(id): Path<String>) -> ApiResult {
    // Discover only authenticated owner chats and their current bound workspace.
    // Discovery touches the filesystem, so it must not block the async executor.
    let config = state.config.clone();
    let root = tokio::task::spawn_blocking(move || {
        let chat = selected(&state, &id)?;
        Ok::<_, (StatusCode, String)>(chat.work_dir.and_then(|root| root.canonicalize().ok()))
    })
    .await
    .map_err(internal_error)??;
    let Some(root) = root else {
        return Ok(Json(json!({"available":false,"refreshing":false})));
    };
    let cache = CACHE
        .get_or_init(|| Arc::new(Mutex::new(HashMap::new())))
        .clone();
    let mut entries = cache.lock().await;
    if let Some(entry) = entries.get(&root)
        && (entry.refreshing || entry.checked.elapsed() < FRESH_FOR)
    {
        let mut value = entry.value.clone();
        value["refreshing"] = json!(entry.refreshing);
        return Ok(Json(value));
    }
    let Ok(permit) = WORKERS.try_acquire() else {
        let mut value = entries
            .get(&root)
            .map(|entry| entry.value.clone())
            .unwrap_or_else(|| json!({"available":false}));
        value["refreshing"] = json!(true);
        return Ok(Json(value));
    };
    if entries.len() >= MAX_PROJECTS
        && !entries.contains_key(&root)
        && let Some(oldest) = entries
            .iter()
            .filter(|(_, entry)| !entry.refreshing)
            .min_by_key(|(_, entry)| entry.checked)
            .map(|(key, _)| key.clone())
    {
        entries.remove(&oldest);
    }
    let entry = entries.entry(root.clone()).or_insert_with(|| Entry {
        value: json!({"available":false}),
        checked: Instant::now(),
        refreshing: false,
    });
    entry.refreshing = true;
    let mut value = entry.value.clone();
    value["refreshing"] = json!(true);
    drop(entries);
    tokio::spawn(async move {
        let _permit = permit;
        let result = tokio::time::timeout(WORK_BUDGET, collect(&config, &root)).await;
        let mut entries = cache.lock().await;
        if let Some(entry) = entries.get_mut(&root) {
            match result {
                Ok(Ok(summary)) => {
                    entry.value = json!({
                        "available":true,"summary":summary,"baseline":"HEAD",
                        "checkedAtMs":crate::markdown_chat::now_ms(),"stale":false,
                    })
                }
                _ => {
                    entry.value["stale"] = json!(true);
                }
            }
            entry.checked = Instant::now();
            entry.refreshing = false;
        }
    });
    Ok(Json(value))
}

#[derive(Default, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct Summary {
    files: usize,
    additions: u64,
    deletions: u64,
    binary_files: usize,
    partial: bool,
}

fn numstat(bytes: &[u8]) -> Result<Summary, ()> {
    let mut summary = Summary::default();
    let mut records = bytes
        .split(|byte| *byte == 0)
        .filter(|part| !part.is_empty());
    while let Some(record) = records.next() {
        let mut fields = record.splitn(3, |byte| *byte == b'\t');
        let additions = fields.next().ok_or(())?;
        let deletions = fields.next().ok_or(())?;
        let path = fields.next().ok_or(())?;
        if path.is_empty() {
            // With -z, a rename has separate old/new path records.
            records.next().ok_or(())?;
            records.next().ok_or(())?;
        }
        summary.files += 1;
        if additions == b"-" && deletions == b"-" {
            summary.binary_files += 1;
        } else {
            summary.additions += std::str::from_utf8(additions)
                .map_err(|_| ())?
                .parse::<u64>()
                .map_err(|_| ())?;
            summary.deletions += std::str::from_utf8(deletions)
                .map_err(|_| ())?
                .parse::<u64>()
                .map_err(|_| ())?;
        }
    }
    Ok(summary)
}

async fn git(config: &AppConfig, root: &FilePath, args: &[&str]) -> Result<Vec<u8>, ()> {
    let mut command = Command::new("git");
    command
        .args([
            "--no-optional-locks",
            "--literal-pathspecs",
            "-c",
            "core.fsmonitor=false",
        ])
        .args(args)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    crate::process_env::scrub_untrusted_child_env(&mut command, config);
    #[cfg(windows)]
    command.creation_flags(0x08000000); // CREATE_NO_WINDOW
    command
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .env("LANG", "C");
    let mut child = command.spawn().map_err(|_| ())?;
    let stdout = child.stdout.take().ok_or(())?;
    let stderr = child.stderr.take().ok_or(())?;
    let output = async move {
        let mut bytes = Vec::new();
        stdout.take(MAX_OUTPUT + 1).read_to_end(&mut bytes).await?;
        Ok::<_, std::io::Error>(bytes)
    };
    let error = async move {
        let mut bytes = Vec::new();
        stderr.take(4097).read_to_end(&mut bytes).await?;
        Ok::<_, std::io::Error>(bytes)
    };
    let (output, error, status) = tokio::try_join!(output, error, child.wait()).map_err(|_| ())?;
    if !status.success() || output.len() as u64 > MAX_OUTPUT || error.len() > 4096 {
        return Err(());
    }
    Ok(output)
}

async fn collect(config: &AppConfig, root: &FilePath) -> Result<Summary, ()> {
    let baseline = match git(config, root, &["rev-parse", "--verify", "HEAD"]).await {
        Ok(hash) => String::from_utf8(hash).map_err(|_| ())?,
        // Empty/unborn repositories have no HEAD. Compute the empty tree ID
        // without writing an object or assuming the repository's hash format.
        Err(_) => {
            String::from_utf8(git(config, root, &["hash-object", "-t", "tree", "--stdin"]).await?)
                .map_err(|_| ())?
        }
    };
    let bytes = git(
        config,
        root,
        &[
            "diff",
            "--numstat",
            "-z",
            "--relative",
            "--no-ext-diff",
            "--no-textconv",
            "--find-renames",
            baseline.trim(),
            "--",
            ".",
        ],
    )
    .await?;
    let mut summary = numstat(&bytes)?;
    let untracked = git(
        config,
        root,
        &[
            "ls-files",
            "--others",
            "--exclude-standard",
            "-z",
            "--",
            ".",
        ],
    )
    .await?;
    let mut budget = MAX_UNTRACKED_BYTES;
    for (index, path) in untracked
        .split(|byte| *byte == 0)
        .filter(|part| !part.is_empty())
        .enumerate()
    {
        if index >= MAX_UNTRACKED_FILES {
            summary.partial = true;
            break;
        }
        let Ok(path) = std::str::from_utf8(path) else {
            summary.partial = true;
            continue;
        };
        let path = root.join(path);
        let Ok(metadata) = tokio::fs::symlink_metadata(&path).await else {
            summary.partial = true;
            continue;
        };
        summary.files += 1;
        if metadata.file_type().is_symlink() {
            summary.additions += 1;
            continue;
        }
        if !metadata.is_file() || metadata.len() > budget {
            summary.partial = true;
            continue;
        }
        let canonical = tokio::fs::canonicalize(&path).await.map_err(|_| ())?;
        if !canonical.starts_with(root) {
            summary.partial = true;
            continue;
        }
        let mut bytes = Vec::new();
        tokio::fs::File::open(canonical)
            .await
            .map_err(|_| ())?
            .take(budget + 1)
            .read_to_end(&mut bytes)
            .await
            .map_err(|_| ())?;
        if bytes.len() as u64 > budget {
            summary.partial = true;
            continue;
        }
        budget -= bytes.len() as u64;
        if bytes.iter().take(8000).any(|byte| *byte == 0) {
            summary.binary_files += 1;
            continue;
        }
        summary.additions += bytes.iter().filter(|byte| **byte == b'\n').count() as u64
            + u64::from(!bytes.is_empty() && bytes.last() != Some(&b'\n'));
    }
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_handle_renames_binary_and_tabs_without_reading_patch_contents() {
        assert_eq!(
            numstat(b"2\t1\tfile\0-\t-\tbinary\0").unwrap(),
            Summary {
                files: 2,
                additions: 2,
                deletions: 1,
                binary_files: 1,
                partial: false,
            }
        );
        assert_eq!(numstat(b"0\t0\t\0old\0new\0").unwrap().files, 1);
        assert_eq!(numstat(b"3\t0\tname\twith-tab\0").unwrap().additions, 3);
        assert!(numstat(b"invalid\0").is_err());
    }

    #[tokio::test]
    async fn reads_real_scoped_staged_unstaged_and_untracked_changes_without_touching_index() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let config = crate::config::default_config(root.clone());
        git(&config, &root, &["init", "-q"]).await.unwrap();
        std::fs::create_dir(root.join("scope")).unwrap();
        std::fs::write(root.join("scope/tracked.txt"), "old\nkeep\n").unwrap();
        std::fs::write(root.join("outside.txt"), "outside\n").unwrap();
        git(&config, &root, &["add", "."]).await.unwrap();
        git(
            &config,
            &root,
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "-qm",
                "Initial",
            ],
        )
        .await
        .unwrap();
        std::fs::write(root.join("scope/tracked.txt"), "new\nkeep\nextra\n").unwrap();
        git(&config, &root, &["add", "scope/tracked.txt"])
            .await
            .unwrap();
        std::fs::write(root.join("scope/tracked.txt"), "new\nkeep\nextra\nlast\n").unwrap();
        std::fs::write(root.join("scope/new.txt"), "one\ntwo").unwrap();
        std::fs::write(root.join("outside.txt"), "changed outside\n").unwrap();
        let index = std::fs::read(root.join(".git/index")).unwrap();
        let summary = collect(&config, &root.join("scope")).await.unwrap();
        assert_eq!(
            summary,
            Summary {
                files: 2,
                additions: 5,
                deletions: 1,
                ..Summary::default()
            }
        );
        assert_eq!(std::fs::read(root.join(".git/index")).unwrap(), index);
    }
}
