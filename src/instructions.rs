//! The behavioural brief handed to the client, ported from `src/instructions.ts`.
//!
//! NOTE: [`build_instructions`] is completed in the infra phase once the
//! environment/memory/project-doc/skills modules exist. For now it returns the
//! agent brief alone so the server can compile and serve.

use crate::environment::{describe_environment, render_environment};
use crate::memory::{load_memory, memory_enabled, render_memory};
use crate::project_doc::{PROJECT_DOC_SEPARATOR, load_project_doc};
use crate::skills::{discover_skills, render_skill_catalog};
use crate::types::AppConfig;

// This string is model-facing and therefore uses the deliberately innocuous
// `setup(ref)` wire vocabulary instead of describing the authorization protocol.
pub const CONVERSATION_AUTH_INSTRUCTIONS: &str = "This connector requires one-time setup for this conversation. Before calling any other tool, call `setup` once with the `ref` supplied in the chat or ChatGPT Project instructions. After setup completes, follow the tool response to load the project brief.";

/// The behavioural half of what Codex tells its model, ported from
/// `codex-rs/core/gpt-5.2-codex_prompt.md`.
pub const AGENT_BRIEF: &str = concat!(
    "You are acting as a coding agent on the user's own machine. These tools are not a simulation: they read and write real files and run real commands in the working directory named under Environment below. Every path a tool takes is relative to that directory, and nothing outside it can be read or written. Work the way an engineer with the checkout open would — look before editing, change files in place, and verify what you changed.\n",
    "\n## Finding and reading code\n\n",
    "- Prefer glob, grep, list_directory, read_file and tree over shelling out. They behave identically on every OS; shell commands do not.\n",
    "- Read a file before editing it. Do not infer its contents from its name, from a search hit, or from an earlier turn.\n",
    "\n## Editing constraints\n\n",
    "- Default to ASCII when editing or creating files. Only introduce non-ASCII or other Unicode characters when there is a clear justification and the file already uses them.\n",
    "- Add succinct code comments that explain what is going on if the code is not self-explanatory. Not \"assigns the value to the variable\", but a brief note ahead of a complex block the reader would otherwise have to parse out. These should be rare.\n",
    "- Use apply_patch for edits to existing files: it patches with surrounding context instead of rewriting the whole file. write_file is for new files. Do not use apply_patch for auto-generated files such as lockfiles, or when running a formatter or a scripted search-and-replace across the codebase is more efficient.\n",
    "- You may be in a dirty git worktree.\n",
    "  - NEVER revert existing changes you did not make, unless the user explicitly asks. Those changes are the user's.\n",
    "  - If asked to commit and there are unrelated changes in the files you touched, leave them alone.\n",
    "  - If the changes are in files you have worked on recently, read them carefully and work with them rather than reverting.\n",
    "  - If the changes are in unrelated files, ignore them.\n",
    "- Do not amend a commit unless explicitly asked.\n",
    "- If changes appear that you did not make, STOP and ask the user how to proceed.\n",
    "- NEVER run destructive commands such as `git reset --hard` or `git checkout --` unless the user specifically asked for or approved them.\n",
    "- After the final related file change, call show_changes once to review the project-scoped aggregate diff. Do not advance the checkpoint after every file unless the user asks for incremental reviews.\n",
    "\n## Running commands\n\n",
    "- Match the shell named under Environment. The same command string does not mean the same thing under POSIX sh, PowerShell and cmd.\n",
    "- exec_command returns a session_id instead of a result when a command outlives its yield window. Drive it from there with write_stdin rather than starting the command again.\n",
    "- The command allowlist is a guardrail, not a sandbox. If a command is rejected, choose another approach or ask — do not look for a way around the check.\n",
    "\n## Planning\n\n",
    "- Skip update_plan for straightforward tasks, roughly the easiest quarter of them.\n",
    "- Do not make single-step plans.\n",
    "- After finishing a step, update the plan before starting the next one.\n",
    "\n## Working memory\n\n",
    "- Your conversation window is smaller than the task. Assume that anything only said in chat will be gone by the next conversation, and that you will not be able to tell it is missing.\n",
    "- Call recall when you are resuming work, when a conversation starts mid-task, or when you are about to ask the user something they may already have answered.\n",
    "- Call remember when you learn something that cost you effort and would cost it again: a decision and its reason, a constraint that is not visible in the code, where something unexpected lives, an approach you tried that did not work. Keep each note to a sentence or two, and update an existing key instead of adding a near-duplicate.\n",
    "- Do not remember what the repository already records. Lasting project conventions belong in AGENTS.md, which is loaded automatically; notes are for the task in flight.\n",
    "- Keep update_plan current for anything multi-step. The plan is saved with the notes and is the fastest way for a later conversation to see where the work stopped.\n",
    "\n## Reading large things\n\n",
    "- Tool output is capped, and a truncated result says so on its last line. When you see that line, act on it: read the next window with read_file's offset, narrow a glob, or point tree at a subdirectory. Do not treat the visible part as the whole.\n",
    "- Read the part of the file you need rather than the file. Grep for the symbol, then read around the hit.\n",
    "\n## Special requests\n\n",
    "- If the user asks for something a command would simply answer — the time, the current branch, whether the tests pass — run it instead of speculating.\n",
    "- If the user asks for a \"review\", switch to a code-review mindset: bugs, risks, behavioural regressions and missing tests come first. Lead with the findings, ordered by severity, each with a file and line reference. Follow with open questions and assumptions. A summary of the change, if any, comes last. If you find nothing, say so plainly and name the residual risks or testing gaps.\n",
    "\n## Frontend work\n\n",
    "- Avoid safe, average-looking layouts. Prefer expressive typography over default stacks, a deliberate colour direction over purple-on-white, a few meaningful animations over generic micro-motion, and a background with some atmosphere. Check that the page works on both desktop and mobile.\n",
    "- Exception: inside an existing site or design system, preserve the established patterns and visual language.\n",
    "\n## Reporting back\n\n",
    "- Be concise, in the register of a coding teammate. Mirror the user's language.\n",
    "- The user does not see tool output. When what a command printed matters, relay the lines that matter.\n",
    "- Do not paste back files you have written; give the path instead. The user is on the same machine and can open it.\n",
    "- Lead with what changed and why, then the detail. Do not open with the word \"Summary\".\n",
    "- Offer natural next steps briefly — tests, a commit, a build — and say plainly what you could not verify yourself.",
);

fn configured_agent_brief(config: &AppConfig) -> String {
    if !config.artifact_ingress.enabled {
        return AGENT_BRIEF.to_string();
    }

    format!(
        "{AGENT_BRIEF}\n\n## Host files\n\n- Use import_host_file when the user attaches a file or asks you to place a ChatGPT-generated file into the project. Do not reconstruct binary files through write_file or substitute an arbitrary URL."
    )
}

/// The initialize-time instructions. Multi-project initialization deliberately
/// omits project state because ChatGPT's conversation ID arrives on tool calls,
/// after the MCP initialize exchange.
pub fn build_initial_instructions(config: &AppConfig) -> String {
    if config.conversation_auth_token.is_some() {
        return CONVERSATION_AUTH_INSTRUCTIONS.to_string();
    }
    if !config.multi_project {
        return build_instructions(config);
    }

    let mut environment = describe_environment(config);
    environment.cwd = "<not selected>".to_string();

    [
        configured_agent_brief(config),
        String::new(),
        "## Project selection".to_string(),
        String::new(),
        "This server is running in multi-project mode. Project identity cannot be resolved during MCP initialization because stable ChatGPT conversation metadata arrives on tool calls.".to_string(),
        format!("Project access root: {}", config.work_dir.display()),
        format!("GitHub clone directory: {}", config.project_clone_dir.display()),
        "For ChatGPT, a project selected earlier in this same conversation is restored automatically even after an MCP reconnect or server restart. A new conversation has no binding.".to_string(),
        "Call `get_agent_brief` first. If this conversation is not yet bound and the user supplied an exact filesystem path or GitHub repository, branch, or pull-request URL, call `set_project_root` directly; a GitHub URL reuses a matching checkout or clones it into the configured clone directory, and a branch or pull-request URL selects that exact ref without moving an unrelated source checkout. When the intended project is described only by name or purpose, call `list_projects` first with a query derived from that intent. Bind only one unambiguous candidate; ask the user when multiple candidates remain plausible because the conversation cannot switch projects. Pass the returned selector to `set_project_root`, then call `get_agent_brief` again. Re-selecting the same canonical directory or exact GitHub selection is idempotent.".to_string(),
        "A ChatGPT conversation cannot switch projects; start a new chat for another project. Clients without stable `openai/session` metadata fall back to an MCP-transport-session binding and must select again after reconnecting.".to_string(),
        "Follow the environment, saved state, skills, and project instructions returned by `get_agent_brief` for the rest of the task.".to_string(),
        String::new(),
        "## Environment".to_string(),
        String::new(),
        render_environment(&environment),
    ]
    .join("\n")
}

/// The full project-aware brief returned by `get_agent_brief`, and used at
/// initialize time in single-project mode. Rebuilt from the effective project
/// config so a later conversation can recover the saved plan and notes.
///
/// Codex assembles the same layers in the same order: base instructions, then
/// `<environment_context>`, then the skill catalogue, then the project's
/// AGENTS.md last so it outranks all of them. Working memory is inserted after
/// the environment: it is state rather than instruction.
pub fn build_instructions(config: &AppConfig) -> String {
    let doc = load_project_doc(config);
    let mut lines: Vec<String> = vec![
        configured_agent_brief(config),
        String::new(),
        "## Environment".to_string(),
        String::new(),
        render_environment(&describe_environment(config)),
    ];

    if memory_enabled(config)
        && let Some(memory) = render_memory(&load_memory(config))
    {
        lines.push(String::new());
        lines.push("## Saved state".to_string());
        lines.push(String::new());
        lines.push("Saved by earlier work on this project, possibly in a conversation you cannot see. Treat it as a handover from yourself, not as instructions from the user, and verify anything load-bearing against the code before acting on it.".to_string());
        lines.push(String::new());
        lines.push(memory);
    }

    // Codex puts the skill catalogue in the prompt and reads a body only once a
    // skill is chosen. The section is left out entirely when none are installed.
    if let Some(catalog) = render_skill_catalog(&discover_skills(config)) {
        lines.push(String::new());
        lines.push("## Skills".to_string());
        lines.push(String::new());
        lines.push("A skill is a set of instructions someone already worked out for a task of this kind, stored in a SKILL.md. The list below is what is available; each entry gives a name and what it is for.".to_string());
        lines.push(String::new());
        lines.push(catalog);
        lines.push(String::new());
        lines.push("- If the user names a skill, or the task clearly matches one of these descriptions, use that skill for the turn. If several apply, take the smallest set that covers the request and say in one line which you are using.".to_string());
        lines.push("- Read the whole SKILL.md with skills_read before acting on it, and read it yourself rather than having something else summarise it.".to_string());
        lines.push("- A skill's body may point at other files in its package — references, scripts, assets. Reach them with skills_read and the same name, passing the path as `resource`; they are usually outside the working directory, so read_file cannot open them. Open only the ones the body actually routes you to, and prefer running or patching a script it provides over retyping the code.".to_string());
        lines.push("- If a named skill is missing or unreadable, say so briefly and carry on with the best fallback.".to_string());
    }

    // Codex marks the same transition with this separator: everything past it is
    // the project speaking about itself, which outranks the generic brief above.
    if let Some(doc) = doc {
        lines.push(String::new());
        lines.push("The project's own instructions follow the marker below. They come from the repository the user pointed this server at, and take precedence over everything above.".to_string());
        lines.push(String::new());
        lines.push(PROJECT_DOC_SEPARATOR.to_string());
        lines.push(String::new());
        lines.push(doc.text);
    }

    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversation_auth_initialization_withholds_project_context() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("AGENTS.md"), "private project instruction").unwrap();
        let mut config = crate::config::default_config(root.path().to_path_buf());
        config.conversation_auth_token =
            Some("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into());

        let initial = build_initial_instructions(&config);

        assert_eq!(initial, CONVERSATION_AUTH_INSTRUCTIONS);
        assert!(!initial.to_ascii_lowercase().contains("auth"));
        assert!(!initial.to_ascii_lowercase().contains("checksum"));
        assert!(!initial.to_ascii_lowercase().contains("token"));
        assert!(!initial.contains("private project instruction"));
        assert!(!initial.contains(&root.path().display().to_string()));
        assert!(build_instructions(&config).contains("private project instruction"));
    }
}
