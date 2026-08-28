use std::path::PathBuf;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use crate::apply_patch::{PatchAction, apply_update, parse_patch, render_added_file};
use crate::exec_sessions::SessionState;
use crate::safe_path::resolve_safe_path;
use crate::tool::{Tool, ToolBehavior, parse_tool_args, schema_for, text_output_schema};
use crate::types::{AppConfig, ToolResult};

const GRAMMAR: &str = "*** Begin Patch\n[ one or more hunks ]\n*** End Patch\n\nHunks:\n*** Add File: <path>\n+<line>            (every line of the new file, each prefixed with '+')\n\n*** Delete File: <path>\n\n*** Update File: <path>\n*** Move to: <new path>   (optional — renames the file)\n@@ <optional context line, e.g. a function or class signature>\n <unchanged line>\n-<removed line>\n+<added line>\n*** End of File           (optional — anchors the chunk to the file's end)";

/// Every write the patch will perform, resolved before anything touches disk.
struct PlannedWrite {
    action: PatchAction,
    abs_path: PathBuf,
    dest_path: Option<PathBuf>,
}

fn action_label(action: &PatchAction) -> String {
    match action {
        PatchAction::Add { path, .. } => format!("Add File: {path}"),
        PatchAction::Delete { path } => format!("Delete File: {path}"),
        PatchAction::Update {
            path, move_path, ..
        } => match move_path {
            Some(destination) => format!("Update File: {path} -> {destination}"),
            None => format!("Update File: {path}"),
        },
    }
}

async fn plan_action(
    action: PatchAction,
    work_dir: &std::path::Path,
) -> Result<PlannedWrite, String> {
    let abs_path = resolve_safe_path(action.path(), work_dir, false)?;

    match &action {
        PatchAction::Add { .. } => {
            // Codex's engine does not existence-check an Add: a hunk that adds
            // over an existing path overwrites it. Match that rather than
            // rejecting, so a patch canonical Codex accepts applies here too.
            Ok(PlannedWrite {
                action,
                abs_path,
                dest_path: None,
            })
        }
        PatchAction::Delete { path } => {
            if !abs_path.exists() {
                return Err(format!("Delete File: '{path}' does not exist"));
            }
            Ok(PlannedWrite {
                action,
                abs_path,
                dest_path: None,
            })
        }
        PatchAction::Update {
            path,
            move_path,
            chunks,
        } => {
            if !abs_path.exists() {
                return Err(format!("Update File: '{path}' does not exist"));
            }
            let original = tokio::fs::read_to_string(&abs_path)
                .await
                .map_err(|e| e.to_string())?;
            apply_update(&original, chunks, path).map_err(|e| e.to_string())?;
            let dest_path = match move_path {
                Some(mp) => Some(resolve_safe_path(mp, work_dir, false)?),
                None => None,
            };
            Ok(PlannedWrite {
                action,
                abs_path,
                dest_path,
            })
        }
    }
}

pub struct ApplyPatch;

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ApplyPatchArgs {
    /// Complete patch text, including the begin and end markers.
    #[schemars(length(min = 1))]
    input: String,
}

#[async_trait]
impl Tool for ApplyPatch {
    fn name(&self) -> &'static str {
        "apply_patch"
    }

    fn title(&self) -> String {
        "Apply patch".to_string()
    }

    fn behavior(&self) -> ToolBehavior {
        ToolBehavior::new(
            false,
            true,
            false,
            false,
            "Adds, overwrites, moves, or deletes local project files and is not safe to retry blindly.",
        )
    }

    fn description(&self) -> String {
        format!(
            "Edit files with the same patch grammar and verified-then-sequential semantics as Codex. This is the preferred way to make targeted code changes because it sends only the affected regions.\n\nThe patch is passed as the \"input\" string in this exact format:\n\n{GRAMMAR}\n\nPaths are relative to the project root (work-dir). Every hunk is parsed and checked against the current files before the first write, so a malformed patch or mismatched context changes nothing. Verified file operations are then applied in order. A later filesystem error can therefore leave earlier operations applied, matching Codex; inspect the returned partial-application report before retrying."
        )
    }

    fn input_schema(&self) -> Value {
        schema_for::<ApplyPatchArgs>()
    }

    fn output_schema(&self) -> Option<Value> {
        Some(text_output_schema())
    }

    fn may_modify_project(&self) -> bool {
        true
    }

    async fn call(&self, args: Value, config: &AppConfig, _session: &SessionState) -> ToolResult {
        let ApplyPatchArgs { input } = match parse_tool_args(args) {
            Ok(args) => args,
            Err(error) => return *error,
        };

        // Resolve every hunk first. A patch that fails on its third file must not
        // have written the first two.
        let actions = match parse_patch(&input) {
            Ok(a) => a,
            Err(e) => return ToolResult::error(format!("Invalid patch: {e}")),
        };

        let mut planned: Vec<PlannedWrite> = Vec::new();
        // Codex rejects duplicate source paths while allowing a move destination
        // to become the source of a later operation, which may then fail after
        // earlier operations have already been applied.
        let mut sources: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
        for action in actions {
            match plan_action(action, &config.work_dir).await {
                Ok(w) => {
                    if !sources.insert(w.abs_path.clone()) {
                        return ToolResult::error(format!(
                            "Patch does not apply: multiple operations target {}",
                            w.abs_path.display()
                        ));
                    }
                    planned.push(w);
                }
                Err(e) => return ToolResult::error(format!("Patch does not apply: {e}")),
            }
        }

        let mut summary: Vec<String> = Vec::new();
        for write in planned {
            let PlannedWrite {
                action,
                abs_path,
                dest_path,
            } = write;
            let outcome: std::io::Result<()> = async {
                match &action {
                    PatchAction::Add { path, lines } => {
                        if let Some(parent) = abs_path.parent() {
                            tokio::fs::create_dir_all(parent).await?;
                        }
                        tokio::fs::write(&abs_path, render_added_file(lines)).await?;
                        summary.push(format!("A {path}"));
                    }
                    PatchAction::Delete { path } => {
                        tokio::fs::remove_file(&abs_path).await?;
                        summary.push(format!("D {path}"));
                    }
                    PatchAction::Update {
                        path,
                        move_path,
                        chunks,
                    } => {
                        let original = tokio::fs::read_to_string(&abs_path).await?;
                        let contents =
                            apply_update(&original, chunks, path).map_err(std::io::Error::other)?;
                        match &dest_path {
                            Some(dest) if dest != &abs_path => {
                                if let Some(parent) = dest.parent() {
                                    tokio::fs::create_dir_all(parent).await?;
                                }
                                tokio::fs::write(dest, &contents).await?;
                                tokio::fs::remove_file(&abs_path).await?;
                                summary.push(format!(
                                    "R {path} -> {}",
                                    move_path.as_deref().unwrap_or("")
                                ));
                            }
                            _ => {
                                tokio::fs::write(&abs_path, &contents).await?;
                                summary.push(format!("M {path}"));
                            }
                        }
                    }
                }
                Ok(())
            }
            .await;

            if let Err(e) = outcome {
                let completed = if summary.is_empty() {
                    "<none>".to_string()
                } else {
                    summary.join("\n")
                };
                return ToolResult::error(format!(
                    "Patch failed while applying {}: {e}\nCompleted before failure:\n{completed}\nThe failing operation may also have modified its target; inspect the working tree before retrying.",
                    action_label(&action)
                ));
            }
        }

        ToolResult::text(format!("Patch applied:\n{}", summary.join("\n")))
    }
}
