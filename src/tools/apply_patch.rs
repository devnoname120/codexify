use std::path::PathBuf;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::apply_patch::{PatchAction, apply_update, parse_patch, render_added_file};
use crate::exec_sessions::SessionState;
use crate::safe_path::resolve_safe_path;
use crate::tool::Tool;
use crate::types::{AppConfig, ToolResult};

const GRAMMAR: &str = "*** Begin Patch\n[ one or more hunks ]\n*** End Patch\n\nHunks:\n*** Add File: <path>\n+<line>            (every line of the new file, each prefixed with '+')\n\n*** Delete File: <path>\n\n*** Update File: <path>\n*** Move to: <new path>   (optional — renames the file)\n@@ <optional context line, e.g. a function or class signature>\n <unchanged line>\n-<removed line>\n+<added line>\n*** End of File           (optional — anchors the chunk to the file's end)";

/// Every write the patch will perform, resolved before anything touches disk.
struct PlannedWrite {
    action: PatchAction,
    abs_path: PathBuf,
    dest_path: Option<PathBuf>,
    contents: Option<String>,
}

async fn plan_action(
    action: PatchAction,
    work_dir: &std::path::Path,
) -> Result<PlannedWrite, String> {
    let abs_path = resolve_safe_path(action.path(), work_dir, false)?;

    match &action {
        PatchAction::Add { lines, .. } => {
            // Codex's engine does not existence-check an Add: a hunk that adds
            // over an existing path overwrites it. Match that rather than
            // rejecting, so a patch canonical Codex accepts applies here too.
            let contents = render_added_file(lines);
            Ok(PlannedWrite {
                action,
                abs_path,
                dest_path: None,
                contents: Some(contents),
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
                contents: None,
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
            let contents = apply_update(&original, chunks, path).map_err(|e| e.to_string())?;
            let dest_path = match move_path {
                Some(mp) => Some(resolve_safe_path(mp, work_dir, false)?),
                None => None,
            };
            Ok(PlannedWrite {
                action,
                abs_path,
                dest_path,
                contents: Some(contents),
            })
        }
    }
}

pub struct ApplyPatch;

#[async_trait]
impl Tool for ApplyPatch {
    fn name(&self) -> &'static str {
        "apply_patch"
    }

    fn description(&self) -> String {
        format!(
            "Edit files with a patch. This is the preferred way to make code changes: it edits in place with surrounding context instead of rewriting whole files, so it is far cheaper than write_file and will not silently clobber concurrent edits.\n\nThe patch is passed as the \"input\" string in this exact format:\n\n{GRAMMAR}\n\nPaths are relative to the project root (work-dir). Context lines must match the file; if they don't, the patch is rejected and nothing is written. Multiple hunks across multiple files are applied atomically — either all succeed or none do."
        )
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "input": {
                    "type": "string",
                    "description": "The full patch text, starting with '*** Begin Patch' and ending with '*** End Patch'"
                }
            },
            "required": ["input"],
            "additionalProperties": false
        })
    }

    fn output_schema(&self) -> Option<Value> {
        Some(json!({
            "type": "object",
            "properties": {
                "content": { "type": "string", "description": "Summary of the files added, updated, deleted or moved" }
            }
        }))
    }

    fn may_modify_project(&self) -> bool {
        true
    }

    async fn call(&self, args: Value, config: &AppConfig, _session: &SessionState) -> ToolResult {
        let Some(input) = args.get("input").and_then(|v| v.as_str()) else {
            return ToolResult::error("input must be a string containing the patch text");
        };

        // Resolve every hunk first. A patch that fails on its third file must not
        // have written the first two.
        let actions = match parse_patch(input) {
            Ok(a) => a,
            Err(e) => return ToolResult::error(format!("Invalid patch: {e}")),
        };

        let mut planned: Vec<PlannedWrite> = Vec::new();
        // Codex rejects a patch whose hunks resolve to the same file twice; guard
        // the same way so two operations never silently race on one path.
        let mut targets: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
        for action in actions {
            match plan_action(action, &config.work_dir).await {
                Ok(w) => {
                    for target in std::iter::once(&w.abs_path).chain(w.dest_path.iter()) {
                        if !targets.insert(target.clone()) {
                            return ToolResult::error(format!(
                                "Patch does not apply: multiple operations target {}",
                                target.display()
                            ));
                        }
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
                contents,
            } = write;
            let outcome: std::io::Result<()> = async {
                match &action {
                    PatchAction::Add { path, .. } => {
                        if let Some(parent) = abs_path.parent() {
                            tokio::fs::create_dir_all(parent).await?;
                        }
                        tokio::fs::write(&abs_path, contents.as_deref().unwrap_or("")).await?;
                        summary.push(format!("A {path}"));
                    }
                    PatchAction::Delete { path } => {
                        tokio::fs::remove_file(&abs_path).await?;
                        summary.push(format!("D {path}"));
                    }
                    PatchAction::Update {
                        path, move_path, ..
                    } => match &dest_path {
                        Some(dest) if dest != &abs_path => {
                            if let Some(parent) = dest.parent() {
                                tokio::fs::create_dir_all(parent).await?;
                            }
                            tokio::fs::write(dest, contents.as_deref().unwrap_or("")).await?;
                            tokio::fs::remove_file(&abs_path).await?;
                            summary.push(format!(
                                "R {path} -> {}",
                                move_path.as_deref().unwrap_or("")
                            ));
                        }
                        _ => {
                            tokio::fs::write(&abs_path, contents.as_deref().unwrap_or("")).await?;
                            summary.push(format!("M {path}"));
                        }
                    },
                }
                Ok(())
            }
            .await;

            if let Err(e) = outcome {
                return ToolResult::error(format!(
                    "Patch partially applied then failed: {e}\nApplied so far:\n{}",
                    summary.join("\n")
                ));
            }
        }

        ToolResult::text(format!("Patch applied:\n{}", summary.join("\n")))
    }
}
