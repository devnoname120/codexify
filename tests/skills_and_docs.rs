//! Integration tests for skills, project-doc, instructions, environment, and the
//! tools that surface them. Ported from the Bun/TS suites:
//!   src/__tests__/skills.test.ts
//!   src/__tests__/project-doc.test.ts
//!   src/__tests__/instructions.test.ts
//!   src/tools/__tests__/skills-tools.test.ts
//!   src/tools/__tests__/get-project-doc.test.ts
//!   src/tools/__tests__/get-agent-brief.test.ts
//!   src/tools/__tests__/get-environment.test.ts
//!
//! Isolation: skills tests pin config.skills.dirs (never read the developer's
//! real ~/.agents/skills), and anything touching build_instructions pins
//! config.memory.dir to a temp path.

use std::path::{Path, PathBuf};

use serde_json::json;
use tempfile::TempDir;

use codexify::config::default_config;
use codexify::environment::{
    describe_environment, node_arch, node_platform, os_name, render_environment,
};
use codexify::exec_sessions::SessionState;
use codexify::instructions::{AGENT_BRIEF, build_instructions};
use codexify::memory::{remember, save_plan};
use codexify::project_doc::{
    DEFAULT_ROOT_MARKERS, PROJECT_DOC_MAX_BYTES, candidate_filenames, find_project_root,
    load_project_doc, project_doc_paths,
};
use codexify::skills::{
    MAX_SKILL_NAME_BYTES, MAX_SKILL_PACKAGE_FILES, SKILL_FILENAME, SkillScope, discover_skills,
    find_skill, parse_skill_frontmatter, render_skill_catalog, resolve_skill_resource,
    skill_package_files, skill_roots,
};
use codexify::tool::Tool;
use codexify::tools::get_agent_brief::GetAgentBrief;
use codexify::tools::get_environment::GetEnvironment;
use codexify::tools::get_project_doc::GetProjectDoc;
use codexify::tools::skills_list::SkillsList;
use codexify::tools::skills_read::SkillsRead;
use codexify::types::{AppConfig, ExecMode};

// --- helpers -----------------------------------------------------------

fn write_file(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, contents).unwrap();
}

fn markers(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

/// A config whose user skill scope is empty, so the suite never reads whoever's
/// home directory happens to hold skills.
fn skills_config(work_dir: &Path) -> AppConfig {
    let mut c = default_config(work_dir.to_path_buf());
    c.skills.dirs = Some(vec![]);
    c
}

fn repo_root(dir: &Path) -> PathBuf {
    dir.join(".agents").join("skills")
}

/// Writes a skill package and returns the directory it lives in.
fn write_skill(root_dir: &Path, name: &str, body: &str) -> PathBuf {
    let dir = root_dir.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(SKILL_FILENAME), body).unwrap();
    dir
}

fn default_skill_body(name: &str) -> String {
    format!("---\nname: {name}\ndescription: Does {name} things\n---\n\nDo the thing.\n")
}

// --- skillRoots ---------------------------------------------------------

#[test]
fn skill_roots_searches_both_codex_directory_names() {
    let dir = TempDir::new().unwrap();
    let config = skills_config(dir.path());
    let paths: Vec<PathBuf> = skill_roots(&config).into_iter().map(|r| r.path).collect();
    assert!(paths.contains(&dir.path().join(".agents").join("skills")));
    assert!(paths.contains(&dir.path().join(".codex").join("skills")));
}

#[test]
fn skill_roots_repo_before_user() {
    let dir = TempDir::new().unwrap();
    let mut config = skills_config(dir.path());
    config.skills.dirs = Some(vec![
        dir.path().join("personal").to_string_lossy().into_owned(),
    ]);
    let roots = skill_roots(&config);
    let first_user = roots
        .iter()
        .position(|r| r.scope == SkillScope::User)
        .unwrap();
    let last_repo = roots
        .iter()
        .rposition(|r| r.scope == SkillScope::Repo)
        .unwrap();
    assert!(last_repo < first_user);
}

#[test]
fn skill_roots_walks_from_project_root_down() {
    let dir = TempDir::new().unwrap();
    std::fs::create_dir_all(dir.path().join(".git")).unwrap();
    let work_dir = dir.path().join("packages").join("app");
    std::fs::create_dir_all(&work_dir).unwrap();

    let mut config = skills_config(&work_dir);
    config.skills.dirs = Some(vec![]);
    let paths: Vec<PathBuf> = skill_roots(&config).into_iter().map(|r| r.path).collect();

    let outer = repo_root(dir.path());
    let inner = repo_root(&work_dir);
    assert!(paths.contains(&outer));
    assert!(paths.contains(&inner));
    let io = paths.iter().position(|p| p == &outer).unwrap();
    let ii = paths.iter().position(|p| p == &inner).unwrap();
    assert!(io < ii);
}

#[test]
fn skill_roots_defaults_user_scope_to_home() {
    let dir = TempDir::new().unwrap();
    let mut config = default_config(dir.path().to_path_buf());
    config.skills.dirs = None; // fall back to the home directory defaults

    let home = codexify::util::home_dir().expect("home dir");
    let user_roots: Vec<PathBuf> = skill_roots(&config)
        .into_iter()
        .filter(|r| r.scope == SkillScope::User)
        .map(|r| r.path)
        .collect();
    assert!(!user_roots.is_empty());
    for p in &user_roots {
        assert!(p.starts_with(&home), "{p:?} should be under {home:?}");
        assert!(
            !p.starts_with(dir.path()),
            "{p:?} should not be under the repo"
        );
    }
}

#[test]
fn skill_roots_configured_dirs_replace_home_and_resolve_relative_to_work_dir() {
    let dir = TempDir::new().unwrap();
    let mut config = skills_config(dir.path());
    config.skills.dirs = Some(vec!["shared-skills".to_string()]);

    let user_roots: Vec<_> = skill_roots(&config)
        .into_iter()
        .filter(|r| r.scope == SkillScope::User)
        .collect();
    assert_eq!(user_roots.len(), 1);
    assert_eq!(user_roots[0].path, dir.path().join("shared-skills"));
    assert_eq!(user_roots[0].scope, SkillScope::User);
}

#[test]
fn skill_roots_lists_each_path_once() {
    let dir = TempDir::new().unwrap();
    let mut config = skills_config(dir.path());
    // Configure a dir identical to a repo root; the dedup must drop the copy.
    config.skills.dirs = Some(vec![repo_root(dir.path()).to_string_lossy().into_owned()]);
    let paths: Vec<PathBuf> = skill_roots(&config).into_iter().map(|r| r.path).collect();
    let unique: std::collections::HashSet<&PathBuf> = paths.iter().collect();
    assert_eq!(unique.len(), paths.len());
}

// --- parseSkillFrontmatter ----------------------------------------------

#[test]
fn parse_frontmatter_reads_name_description_short() {
    let parsed = parse_skill_frontmatter(
        "---\nname: review\ndescription: Review a pull request\nmetadata:\n  short-description: Review\n---\n\nbody",
        "fallback",
    )
    .unwrap();
    assert_eq!(parsed.name, "review");
    assert_eq!(parsed.description, "Review a pull request");
    assert_eq!(parsed.short_description, Some("Review".to_string()));
}

#[test]
fn parse_frontmatter_falls_back_to_dir_name_but_not_missing_description() {
    let parsed = parse_skill_frontmatter("---\ndescription: Something\n---\n", "on-disk").unwrap();
    assert_eq!(parsed.name, "on-disk");

    let err = parse_skill_frontmatter("---\nname: x\n---\n", "on-disk").unwrap_err();
    assert!(err.contains("missing field `description`"), "{err}");
}

#[test]
fn parse_frontmatter_collapses_wrapped_scalar() {
    let parsed = parse_skill_frontmatter(
        "---\nname: wrapped\ndescription: >\n  one\n  two\n---\n",
        "fallback",
    )
    .unwrap();
    assert_eq!(parsed.description, "one two");
}

#[test]
fn parse_frontmatter_rejects_no_frontmatter() {
    let err = parse_skill_frontmatter("# Just a heading\n", "x").unwrap_err();
    assert!(err.contains("missing YAML frontmatter"), "{err}");
}

#[test]
fn parse_frontmatter_rejects_invalid_yaml() {
    let err = parse_skill_frontmatter("---\nname: [unclosed\n---\n", "x").unwrap_err();
    assert!(err.contains("invalid YAML"), "{err}");
}

#[test]
fn parse_frontmatter_rejects_overlong_name() {
    let long = "n".repeat(MAX_SKILL_NAME_BYTES + 1);
    let err = parse_skill_frontmatter(&format!("---\nname: {long}\ndescription: d\n---\n"), "x")
        .unwrap_err();
    assert!(err.contains("invalid name"), "{err}");
}

// --- discoverSkills -----------------------------------------------------

#[test]
fn discover_finds_project_skills_sorted() {
    let dir = TempDir::new().unwrap();
    let root = repo_root(dir.path());
    write_skill(&root, "beta", &default_skill_body("beta"));
    write_skill(&root, "alpha", &default_skill_body("alpha"));

    let catalog = discover_skills(&skills_config(dir.path()));
    let names: Vec<&str> = catalog.skills.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, vec!["alpha", "beta"]);
    assert_eq!(catalog.skills[0].scope, SkillScope::Repo);
    assert_eq!(
        catalog.skills[0].path,
        root.join("alpha").join(SKILL_FILENAME)
    );
}

#[test]
fn discover_passes_over_dir_without_skill_md_quietly() {
    let dir = TempDir::new().unwrap();
    let root = repo_root(dir.path());
    std::fs::create_dir_all(root.join("not-a-skill")).unwrap();
    write_file(&root.join("loose.md"), "stray");

    let catalog = discover_skills(&skills_config(dir.path()));
    assert!(catalog.skills.is_empty());
    assert!(catalog.warnings.is_empty());
}

#[test]
fn discover_repo_shadows_user_and_says_so() {
    let dir = TempDir::new().unwrap();
    let user_dir = dir.path().join("personal");
    write_skill(
        &repo_root(dir.path()),
        "deploy",
        "---\nname: deploy\ndescription: The project's own\n---\n",
    );
    write_skill(
        &user_dir,
        "deploy",
        "---\nname: deploy\ndescription: A personal one\n---\n",
    );

    let mut config = skills_config(dir.path());
    config.skills.dirs = Some(vec![user_dir.to_string_lossy().into_owned()]);
    let catalog = discover_skills(&config);

    assert_eq!(catalog.skills.len(), 1);
    assert_eq!(catalog.skills[0].description, "The project's own");
    assert_eq!(catalog.warnings.len(), 1);
    assert_eq!(
        catalog.warnings[0].path,
        user_dir.join("deploy").join(SKILL_FILENAME)
    );
    assert!(
        catalog.warnings[0]
            .message
            .contains("shadowed by the repo skill")
    );
}

#[test]
fn discover_reports_unusable_frontmatter() {
    let dir = TempDir::new().unwrap();
    let root = repo_root(dir.path());
    write_skill(
        &root,
        "broken",
        "---\nname: broken\n---\n\nno description\n",
    );
    write_skill(&root, "fine", &default_skill_body("fine"));

    let catalog = discover_skills(&skills_config(dir.path()));
    let names: Vec<&str> = catalog.skills.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, vec!["fine"]);
    assert!(catalog.warnings[0].message.contains("description"));
}

#[test]
fn discover_searches_nothing_when_disabled() {
    let dir = TempDir::new().unwrap();
    write_skill(
        &repo_root(dir.path()),
        "alpha",
        &default_skill_body("alpha"),
    );
    let mut config = skills_config(dir.path());
    config.skills.enabled = Some(false);
    let catalog = discover_skills(&config);
    assert!(catalog.skills.is_empty());
    assert!(catalog.warnings.is_empty());
    assert!(catalog.roots.is_empty());
}

// --- findSkill ----------------------------------------------------------

#[test]
fn find_skill_exact_then_ci_then_none() {
    let dir = TempDir::new().unwrap();
    write_skill(
        &repo_root(dir.path()),
        "Code-Review",
        &default_skill_body("Code-Review"),
    );
    let catalog = discover_skills(&skills_config(dir.path()));

    assert_eq!(
        find_skill(&catalog, "Code-Review").map(|s| s.name.as_str()),
        Some("Code-Review")
    );
    assert_eq!(
        find_skill(&catalog, "code-review").map(|s| s.name.as_str()),
        Some("Code-Review")
    );
    assert!(find_skill(&catalog, "missing").is_none());
}

// --- resolveSkillResource -----------------------------------------------

#[test]
fn resolve_resource_against_skill_dir() {
    let dir = TempDir::new().unwrap();
    let skill_dir = write_skill(
        &repo_root(dir.path()),
        "alpha",
        &default_skill_body("alpha"),
    );
    let catalog = discover_skills(&skills_config(dir.path()));
    let skill = &catalog.skills[0];

    assert_eq!(
        resolve_skill_resource(skill, "references/api.md").unwrap(),
        skill_dir.join("references").join("api.md")
    );
    assert_eq!(
        resolve_skill_resource(skill, "./scripts/run.py").unwrap(),
        skill_dir.join("scripts").join("run.py")
    );
}

#[test]
fn resolve_resource_refuses_to_escape() {
    let dir = TempDir::new().unwrap();
    write_skill(
        &repo_root(dir.path()),
        "alpha",
        &default_skill_body("alpha"),
    );
    let catalog = discover_skills(&skills_config(dir.path()));
    let skill = &catalog.skills[0];

    for escape in ["../beta/SKILL.md", "a/../../b", "/etc/passwd", ""] {
        let err = resolve_skill_resource(skill, escape).unwrap_err();
        assert!(err.contains("inside the skill"), "{escape}: {err}");
    }
}

// --- skillPackageFiles --------------------------------------------------

#[test]
fn package_files_lists_everything_but_skill_md() {
    let dir = TempDir::new().unwrap();
    let skill_dir = write_skill(
        &repo_root(dir.path()),
        "alpha",
        &default_skill_body("alpha"),
    );
    write_file(&skill_dir.join("references").join("api.md"), "");
    write_file(&skill_dir.join("run.py"), "");

    let catalog = discover_skills(&skills_config(dir.path()));
    let files = skill_package_files(&catalog.skills[0], MAX_SKILL_PACKAGE_FILES);
    assert_eq!(
        files,
        vec!["references/api.md".to_string(), "run.py".to_string()]
    );
}

#[test]
fn package_files_stops_at_cap() {
    let dir = TempDir::new().unwrap();
    let skill_dir = write_skill(
        &repo_root(dir.path()),
        "alpha",
        &default_skill_body("alpha"),
    );
    for i in 0..10 {
        write_file(&skill_dir.join(format!("f{i}.txt")), "");
    }
    let catalog = discover_skills(&skills_config(dir.path()));
    assert_eq!(skill_package_files(&catalog.skills[0], 3).len(), 3);
}

// --- renderSkillCatalog -------------------------------------------------

#[test]
fn render_catalog_none_when_empty() {
    let dir = TempDir::new().unwrap();
    assert!(render_skill_catalog(&discover_skills(&skills_config(dir.path()))).is_none());
}

#[test]
fn render_catalog_name_and_reason_per_skill() {
    let dir = TempDir::new().unwrap();
    write_skill(
        &repo_root(dir.path()),
        "alpha",
        &default_skill_body("alpha"),
    );
    let rendered = render_skill_catalog(&discover_skills(&skills_config(dir.path()))).unwrap();
    // U+2014 em dash kept out of the ASCII source via an escape.
    assert_eq!(rendered, "- alpha (repo) \u{2014} Does alpha things");
}

// --- project-doc: candidateFilenames ------------------------------------

#[test]
fn candidate_filenames_default() {
    use codexify::types::ProjectDocConfig;
    let names = candidate_filenames(&ProjectDocConfig::default());
    assert_eq!(
        names,
        vec!["AGENTS.override.md".to_string(), "AGENTS.md".to_string()]
    );
}

#[test]
fn candidate_filenames_appends_without_dupes_or_blanks() {
    use codexify::types::ProjectDocConfig;
    let cfg = ProjectDocConfig {
        fallback_filenames: Some(vec![
            "CLAUDE.md".into(),
            "AGENTS.md".into(),
            "".into(),
            "CLAUDE.md".into(),
        ]),
        ..Default::default()
    };
    assert_eq!(
        candidate_filenames(&cfg),
        vec![
            "AGENTS.override.md".to_string(),
            "AGENTS.md".to_string(),
            "CLAUDE.md".to_string()
        ]
    );
}

// --- project-doc: findProjectRoot ---------------------------------------

#[test]
fn find_root_nearest_ancestor_with_marker() {
    let dir = TempDir::new().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    std::fs::create_dir_all(repo.join("src").join("deep")).unwrap();
    assert_eq!(
        find_project_root(
            &repo.join("src").join("deep"),
            &markers(DEFAULT_ROOT_MARKERS)
        ),
        Some(repo)
    );
}

#[test]
fn find_root_counts_start_dir() {
    let dir = TempDir::new().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    assert_eq!(
        find_project_root(&repo, &markers(DEFAULT_ROOT_MARKERS)),
        Some(repo.clone())
    );
}

#[test]
fn find_root_null_when_no_ancestor() {
    let dir = TempDir::new().unwrap();
    let loose = dir.path().join("loose");
    std::fs::create_dir_all(&loose).unwrap();
    assert_eq!(
        find_project_root(&loose, &markers(&["marker-that-does-not-exist"])),
        None
    );
}

#[test]
fn find_root_empty_markers_disables_walk() {
    let dir = TempDir::new().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    assert_eq!(find_project_root(&repo, &[]), None);
}

// --- project-doc: projectDocPaths ---------------------------------------

fn doc_config(work_dir: &Path) -> AppConfig {
    default_config(work_dir.to_path_buf())
}

#[test]
fn doc_paths_one_per_dir_root_down() {
    let dir = TempDir::new().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    let outer = repo.join("AGENTS.md");
    let inner = repo.join("src").join("AGENTS.md");
    write_file(&outer, "outer");
    write_file(&inner, "inner");
    assert_eq!(
        project_doc_paths(&doc_config(&repo.join("src"))),
        vec![outer, inner]
    );
}

#[test]
fn doc_paths_prefers_override() {
    let dir = TempDir::new().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    let override_path = repo.join("AGENTS.override.md");
    write_file(&override_path, "override");
    write_file(&repo.join("AGENTS.md"), "plain");
    assert_eq!(project_doc_paths(&doc_config(&repo)), vec![override_path]);
}

#[test]
fn doc_paths_stops_at_project_root() {
    let dir = TempDir::new().unwrap();
    write_file(&dir.path().join("AGENTS.md"), "above the repo");
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    let inside = repo.join("AGENTS.md");
    write_file(&inside, "inside");
    assert_eq!(project_doc_paths(&doc_config(&repo)), vec![inside]);
}

#[test]
fn doc_paths_searches_only_work_dir_without_marker() {
    let dir = TempDir::new().unwrap();
    write_file(&dir.path().join("AGENTS.md"), "above");
    let loose = dir.path().join("loose");
    let own = loose.join("AGENTS.md");
    write_file(&own, "own");
    assert_eq!(project_doc_paths(&doc_config(&loose)), vec![own]);
}

#[test]
fn doc_paths_honours_configured_markers_and_fallbacks() {
    let dir = TempDir::new().unwrap();
    let repo = dir.path().join("repo");
    write_file(&repo.join("package.json"), "{}");
    let doc = repo.join("CONTRIBUTING.md");
    write_file(&doc, "fallback");

    let mut config = doc_config(&repo);
    config.project_doc.root_markers = Some(vec!["package.json".to_string()]);
    config.project_doc.fallback_filenames = Some(vec!["CONTRIBUTING.md".to_string()]);
    assert_eq!(project_doc_paths(&config), vec![doc]);
}

#[test]
fn doc_paths_empty_when_none() {
    let dir = TempDir::new().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    assert!(project_doc_paths(&doc_config(&repo)).is_empty());
}

// --- project-doc: loadProjectDoc ----------------------------------------

#[test]
fn load_doc_concatenates_outermost_first() {
    let dir = TempDir::new().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    write_file(&repo.join("AGENTS.md"), "root rules");
    write_file(&repo.join("src").join("AGENTS.md"), "src rules");
    assert_eq!(
        load_project_doc(&doc_config(&repo.join("src")))
            .unwrap()
            .text,
        "root rules\n\nsrc rules"
    );
}

#[test]
fn load_doc_null_when_nothing() {
    let dir = TempDir::new().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    assert!(load_project_doc(&doc_config(&repo)).is_none());
}

#[test]
fn load_doc_skips_whitespace_only_without_spending_budget() {
    let dir = TempDir::new().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    write_file(&repo.join("AGENTS.md"), "   \n\n\t\n");
    write_file(&repo.join("src").join("AGENTS.md"), "real rules");
    let doc = load_project_doc(&doc_config(&repo.join("src"))).unwrap();
    let contents: Vec<&str> = doc.entries.iter().map(|e| e.contents.as_str()).collect();
    assert_eq!(contents, vec!["real rules"]);
}

#[test]
fn load_doc_cuts_short_at_byte_budget() {
    let dir = TempDir::new().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    write_file(&repo.join("AGENTS.md"), "abcdefghij");
    let mut config = doc_config(&repo);
    config.project_doc.max_bytes = Some(4);
    let doc = load_project_doc(&config).unwrap();
    assert_eq!(doc.entries[0].contents, "abcd");
    assert!(doc.entries[0].truncated);
}

#[test]
fn load_doc_shares_one_budget_across_docs() {
    let dir = TempDir::new().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    write_file(&repo.join("AGENTS.md"), "12345");
    write_file(&repo.join("src").join("AGENTS.md"), "67890");
    let mut config = doc_config(&repo.join("src"));
    config.project_doc.max_bytes = Some(8);
    let doc = load_project_doc(&config).unwrap();
    let contents: Vec<&str> = doc.entries.iter().map(|e| e.contents.as_str()).collect();
    assert_eq!(contents, vec!["12345", "678"]);
    let truncated: Vec<bool> = doc.entries.iter().map(|e| e.truncated).collect();
    assert_eq!(truncated, vec![false, true]);
}

#[test]
fn load_doc_counts_bytes_not_chars() {
    let dir = TempDir::new().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    // U+00E9 "e-acute" is two bytes in UTF-8, so a four-byte budget holds two.
    write_file(&repo.join("AGENTS.md"), "\u{e9}\u{e9}\u{e9}");
    let mut config = doc_config(&repo);
    config.project_doc.max_bytes = Some(4);
    assert_eq!(load_project_doc(&config).unwrap().text, "\u{e9}\u{e9}");
}

#[test]
fn load_doc_max_bytes_zero_disables() {
    let dir = TempDir::new().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    write_file(&repo.join("AGENTS.md"), "rules");
    let mut config = doc_config(&repo);
    config.project_doc.max_bytes = Some(0);
    assert!(load_project_doc(&config).is_none());
}

#[test]
fn load_doc_defaults_to_32kib() {
    let dir = TempDir::new().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    write_file(
        &repo.join("AGENTS.md"),
        &"x".repeat(PROJECT_DOC_MAX_BYTES + 10),
    );
    let doc = load_project_doc(&doc_config(&repo)).unwrap();
    assert_eq!(doc.text.len(), PROJECT_DOC_MAX_BYTES);
    assert!(doc.entries[0].truncated);
}

// --- instructions: AGENT_BRIEF ------------------------------------------

#[test]
fn brief_carries_hardest_constraints() {
    assert!(AGENT_BRIEF.contains("git reset --hard"));
    assert!(AGENT_BRIEF.contains("NEVER revert existing changes you did not make"));
    assert!(AGENT_BRIEF.contains("Do not amend a commit unless explicitly asked"));
    assert!(AGENT_BRIEF.contains("Read a file before editing it"));
    assert!(AGENT_BRIEF.contains("Do not make single-step plans"));
}

#[test]
fn brief_covers_lost_context_window() {
    assert!(AGENT_BRIEF.contains("Call recall when you are resuming work"));
    assert!(AGENT_BRIEF.contains("Call remember when you learn something"));
    assert!(AGENT_BRIEF.contains("a truncated result says so on its last line"));
}

#[test]
fn brief_names_the_tools() {
    for tool in [
        "apply_patch",
        "write_file",
        "exec_command",
        "write_stdin",
        "update_plan",
        "grep",
    ] {
        assert!(AGENT_BRIEF.contains(tool), "missing {tool}");
    }
}

#[test]
fn brief_drops_cli_only_rules() {
    assert!(!AGENT_BRIEF.contains("CLI handles styling"));
    assert!(!AGENT_BRIEF.contains("no ANSI codes"));
    assert!(!AGENT_BRIEF.contains("#Lline"));
}

// --- instructions: buildInstructions ------------------------------------

/// Config with memory and skills pinned to temp so build_instructions is
/// deterministic and never reads the developer's real state.
fn brief_config(work_dir: &Path, state_dir: &Path, shell: &str) -> AppConfig {
    let mut c = default_config(work_dir.to_path_buf());
    c.exec.default_shell = Some(shell.to_string());
    c.memory.dir = Some(state_dir.to_string_lossy().into_owned());
    c.skills.dirs = Some(vec![]);
    c
}

fn write_project_skill(work_dir: &Path, name: &str, description: &str) {
    let dir = work_dir.join(".agents").join("skills").join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {description}\n---\n"),
    )
    .unwrap();
}

#[test]
fn build_orders_brief_env_doc() {
    let dir = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    std::fs::create_dir_all(dir.path().join(".git")).unwrap();
    write_file(&dir.path().join("AGENTS.md"), "Never force-push.");
    let text = build_instructions(&brief_config(dir.path(), state.path(), "bash"));
    let brief = text.find("You are acting as a coding agent").unwrap();
    let env = text.find("## Environment").unwrap();
    let doc = text.find("--- project-doc ---").unwrap();
    assert!(brief < env);
    assert!(env < doc);
}

#[test]
fn build_includes_whole_brief_verbatim() {
    let dir = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    std::fs::create_dir_all(dir.path().join(".git")).unwrap();
    assert!(
        build_instructions(&brief_config(dir.path(), state.path(), "bash")).contains(AGENT_BRIEF)
    );
}

#[test]
fn build_mentions_native_file_ingress_only_when_the_tool_is_enabled() {
    let dir = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    std::fs::create_dir_all(dir.path().join(".git")).unwrap();
    let enabled = brief_config(dir.path(), state.path(), "bash");
    assert!(build_instructions(&enabled).contains("Use import_host_file"));

    let mut disabled = enabled;
    disabled.artifact_ingress.enabled = false;
    assert!(!build_instructions(&disabled).contains("import_host_file"));
}

#[test]
fn build_mentions_native_file_egress_only_when_the_tool_is_enabled() {
    let dir = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    std::fs::create_dir_all(dir.path().join(".git")).unwrap();
    let enabled = brief_config(dir.path(), state.path(), "bash");
    assert!(build_instructions(&enabled).contains("Use export_host_file"));

    let mut disabled = enabled;
    disabled.artifact_egress.enabled = false;
    assert!(!build_instructions(&disabled).contains("export_host_file"));
}

#[test]
fn build_describes_actual_shell() {
    let dir = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    std::fs::create_dir_all(dir.path().join(".git")).unwrap();
    assert!(
        build_instructions(&brief_config(dir.path(), state.path(), "bash"))
            .contains("Shell for exec_command: bash (posix)")
    );
    assert!(
        build_instructions(&brief_config(dir.path(), state.path(), "powershell"))
            .contains("Write PowerShell, not POSIX sh")
    );
}

#[test]
fn build_ends_with_brief_and_env_when_no_doc() {
    let dir = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    std::fs::create_dir_all(dir.path().join(".git")).unwrap();
    let text = build_instructions(&brief_config(dir.path(), state.path(), "bash"));
    assert!(!text.contains("--- project-doc ---"));
    assert!(text.contains("Working directory:"));
}

#[test]
fn build_says_nothing_about_saved_state_when_none() {
    let dir = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    std::fs::create_dir_all(dir.path().join(".git")).unwrap();
    assert!(
        !build_instructions(&brief_config(dir.path(), state.path(), "bash"))
            .contains("## Saved state")
    );
}

#[test]
fn build_hands_back_saved_plan_and_notes() {
    use codexify::types::{PlanItem, PlanState, PlanStepStatus};
    let dir = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    std::fs::create_dir_all(dir.path().join(".git")).unwrap();
    let config = brief_config(dir.path(), state.path(), "bash");

    save_plan(
        &config,
        Some(PlanState {
            explanation: None,
            plan: vec![PlanItem {
                step: "port the tools".to_string(),
                status: PlanStepStatus::InProgress,
            }],
        }),
    );
    remember(
        &config,
        "why-bun",
        "The runtime ships a test runner.",
        "2026-01-01T00:00:00.000Z",
    );

    let text = build_instructions(&config);
    assert!(text.contains("## Saved state"));
    assert!(text.contains("[~] port the tools"));
    assert!(text.contains("- why-bun: The runtime ships a test runner."));
}

#[test]
fn build_puts_saved_state_before_doc() {
    let dir = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    std::fs::create_dir_all(dir.path().join(".git")).unwrap();
    write_file(&dir.path().join("AGENTS.md"), "Never force-push.");
    let config = brief_config(dir.path(), state.path(), "bash");
    remember(&config, "k", "v", "2026-01-01T00:00:00.000Z");

    let text = build_instructions(&config);
    let env = text.find("## Environment").unwrap();
    let saved = text.find("## Saved state").unwrap();
    let doc = text.find("--- project-doc ---").unwrap();
    assert!(env < saved);
    assert!(saved < doc);
}

#[test]
fn build_omits_saved_state_when_memory_disabled() {
    let dir = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    std::fs::create_dir_all(dir.path().join(".git")).unwrap();
    let config = brief_config(dir.path(), state.path(), "bash");
    remember(&config, "k", "v", "2026-01-01T00:00:00.000Z");

    let mut disabled = config.clone();
    disabled.memory.enabled = Some(false);
    assert!(!build_instructions(&disabled).contains("## Saved state"));
}

#[test]
fn build_announces_no_skill_library_when_none() {
    let dir = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    std::fs::create_dir_all(dir.path().join(".git")).unwrap();
    assert!(
        !build_instructions(&brief_config(dir.path(), state.path(), "bash")).contains("## Skills")
    );
}

#[test]
fn build_lists_installed_skills() {
    let dir = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    std::fs::create_dir_all(dir.path().join(".git")).unwrap();
    write_project_skill(dir.path(), "deploy", "Ship a release");

    let text = build_instructions(&brief_config(dir.path(), state.path(), "bash"));
    assert!(text.contains("## Skills"));
    assert!(text.contains("- deploy (repo) \u{2014} Ship a release"));
    assert!(text.contains("skills_read"));
}

#[test]
fn build_catalogue_after_env_before_doc() {
    let dir = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    std::fs::create_dir_all(dir.path().join(".git")).unwrap();
    write_project_skill(dir.path(), "deploy", "Ship a release");
    write_file(&dir.path().join("AGENTS.md"), "Never force-push.");

    let text = build_instructions(&brief_config(dir.path(), state.path(), "bash"));
    let env = text.find("## Environment").unwrap();
    let skills = text.find("## Skills").unwrap();
    let doc = text.find("--- project-doc ---").unwrap();
    assert!(env < skills);
    assert!(skills < doc);
}

#[test]
fn build_omits_catalogue_when_skills_disabled() {
    let dir = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    std::fs::create_dir_all(dir.path().join(".git")).unwrap();
    write_project_skill(dir.path(), "deploy", "Ship a release");
    let mut config = brief_config(dir.path(), state.path(), "bash");
    config.skills.enabled = Some(false);
    config.skills.dirs = None;
    assert!(!build_instructions(&config).contains("## Skills"));
}

// --- skills_list tool ---------------------------------------------------

#[tokio::test]
async fn skills_list_gives_name_reason_path() {
    let dir = TempDir::new().unwrap();
    let skills_dir = dir.path().join(".agents").join("skills");
    write_skill(
        &skills_dir,
        "deploy",
        "---\nname: deploy\ndescription: Ship a release\n---\n\nbody\n",
    );
    let config = skills_config(dir.path());
    let session = SessionState::new();

    let r = SkillsList.call(json!({}), &config, &session).await;
    let text = r.joined_text();
    assert!(text.contains("1 skill available"));
    assert!(text.contains("- deploy (repo) \u{2014} Ship a release"));
    let skill_path = skills_dir
        .join("deploy")
        .join(SKILL_FILENAME)
        .display()
        .to_string();
    assert!(text.contains(&skill_path));

    let structured = r.structured_content.unwrap();
    assert_eq!(
        structured["skills"],
        json!([{
            "name": "deploy",
            "description": "Ship a release",
            "scope": "repo",
            "path": skill_path,
        }])
    );
}

#[tokio::test]
async fn skills_list_names_dirs_searched_when_none() {
    let dir = TempDir::new().unwrap();
    let config = skills_config(dir.path());
    let session = SessionState::new();
    let r = SkillsList.call(json!({}), &config, &session).await;
    let text = r.joined_text();
    assert!(text.contains("No skills found"));
    let skills_dir = dir
        .path()
        .join(".agents")
        .join("skills")
        .display()
        .to_string();
    assert!(text.contains(&skills_dir));
    assert_eq!(r.structured_content.unwrap()["skills"], json!([]));
}

#[tokio::test]
async fn skills_list_reports_skill_it_could_not_offer() {
    let dir = TempDir::new().unwrap();
    write_skill(
        &dir.path().join(".agents").join("skills"),
        "broken",
        "---\nname: broken\n---\n",
    );
    let config = skills_config(dir.path());
    let session = SessionState::new();
    let r = SkillsList.call(json!({}), &config, &session).await;
    let text = r.joined_text();
    assert!(text.contains("Not offered:"));
    assert!(text.contains("description"));
}

#[tokio::test]
async fn skills_list_says_so_when_disabled() {
    let dir = TempDir::new().unwrap();
    write_skill(
        &dir.path().join(".agents").join("skills"),
        "deploy",
        "---\nname: deploy\ndescription: Ship a release\n---\n",
    );
    let mut config = skills_config(dir.path());
    config.skills.enabled = Some(false);
    let session = SessionState::new();
    let r = SkillsList.call(json!({}), &config, &session).await;
    assert!(r.joined_text().contains("disabled"));
    assert!(!r.is_error);
}

// --- skills_read tool ---------------------------------------------------

#[tokio::test]
async fn skills_read_returns_body_and_points_at_package() {
    let dir = TempDir::new().unwrap();
    let skill_dir = write_skill(
        &dir.path().join(".agents").join("skills"),
        "deploy",
        "---\nname: deploy\ndescription: Ship a release\n---\n\nStep one.\n",
    );
    write_file(&skill_dir.join("scripts").join("release.sh"), "echo hi\n");
    let config = skills_config(dir.path());
    let session = SessionState::new();

    let r = SkillsRead
        .call(json!({"name": "deploy"}), &config, &session)
        .await;
    let body = r.joined_text();
    assert!(body.contains("Step one."));
    assert!(body.contains("scripts/release.sh"));
}

#[tokio::test]
async fn skills_read_reads_file_by_relative_path() {
    let dir = TempDir::new().unwrap();
    let skill_dir = write_skill(
        &dir.path().join(".agents").join("skills"),
        "deploy",
        "---\nname: deploy\ndescription: Ship a release\n---\n",
    );
    write_file(
        &skill_dir.join("references").join("api.md"),
        "the reference\n",
    );
    let config = skills_config(dir.path());
    let session = SessionState::new();

    let r = SkillsRead
        .call(
            json!({"name": "deploy", "resource": "references/api.md"}),
            &config,
            &session,
        )
        .await;
    assert!(r.joined_text().contains("the reference"));
}

#[tokio::test]
async fn skills_read_refuses_resource_outside_skill() {
    let dir = TempDir::new().unwrap();
    write_skill(
        &dir.path().join(".agents").join("skills"),
        "deploy",
        "---\nname: deploy\ndescription: Ship a release\n---\n",
    );
    write_file(&dir.path().join("secret.txt"), "no");
    let config = skills_config(dir.path());
    let session = SessionState::new();

    let r = SkillsRead
        .call(
            json!({"name": "deploy", "resource": "../../../secret.txt"}),
            &config,
            &session,
        )
        .await;
    assert!(r.is_error);
    assert!(r.joined_text().contains("inside the skill"));
}

#[tokio::test]
async fn skills_read_names_available_when_skill_not_found() {
    let dir = TempDir::new().unwrap();
    write_skill(
        &dir.path().join(".agents").join("skills"),
        "deploy",
        "---\nname: deploy\ndescription: Ship a release\n---\n",
    );
    let config = skills_config(dir.path());
    let session = SessionState::new();

    let r = SkillsRead
        .call(json!({"name": "nope"}), &config, &session)
        .await;
    assert!(r.is_error);
    assert!(r.joined_text().contains("Available: deploy."));
}

#[tokio::test]
async fn skills_read_reports_missing_resource() {
    let dir = TempDir::new().unwrap();
    write_skill(
        &dir.path().join(".agents").join("skills"),
        "deploy",
        "---\nname: deploy\ndescription: Ship a release\n---\n",
    );
    let config = skills_config(dir.path());
    let session = SessionState::new();

    let r = SkillsRead
        .call(
            json!({"name": "deploy", "resource": "references/api.md"}),
            &config,
            &session,
        )
        .await;
    assert!(r.is_error);
    assert!(r.joined_text().contains("no file at references/api.md"));
}

#[tokio::test]
async fn skills_read_windows_long_body() {
    let dir = TempDir::new().unwrap();
    let lines: Vec<String> = (1..=40).map(|i| format!("line{i}")).collect();
    let body = format!(
        "---\nname: long\ndescription: A long one\n---\n\n{}\n",
        lines.join("\n")
    );
    write_skill(&dir.path().join(".agents").join("skills"), "long", &body);

    let mut config = skills_config(dir.path());
    config.output.max_file_lines = Some(10);
    let session = SessionState::new();

    let first = SkillsRead
        .call(json!({"name": "long"}), &config, &session)
        .await;
    assert!(first.joined_text().contains("call again with offset=10"));

    let second = SkillsRead
        .call(json!({"name": "long", "offset": 10}), &config, &session)
        .await;
    assert!(second.joined_text().contains("line6"));
}

#[tokio::test]
async fn skills_read_errors_when_disabled() {
    let dir = TempDir::new().unwrap();
    write_skill(
        &dir.path().join(".agents").join("skills"),
        "deploy",
        "---\nname: deploy\ndescription: Ship a release\n---\n",
    );
    let mut config = skills_config(dir.path());
    config.skills.enabled = Some(false);
    let session = SessionState::new();

    let r = SkillsRead
        .call(json!({"name": "deploy"}), &config, &session)
        .await;
    assert!(r.is_error);
    assert!(r.joined_text().contains("disabled"));
}

// --- get_project_doc tool -----------------------------------------------

/// Config with a .git root marker in place and memory pinned to temp. The
/// render_project_doc helper is private in Rust, so its assertions are exercised
/// through the tool's text output (which is exactly that helper's return).
fn doc_tool_config(work_dir: &Path, state_dir: &Path) -> AppConfig {
    let mut c = default_config(work_dir.to_path_buf());
    c.exec.default_shell = Some("bash".to_string());
    c.memory.dir = Some(state_dir.to_string_lossy().into_owned());
    c.skills.dirs = Some(vec![]);
    c
}

#[tokio::test]
async fn project_doc_render_names_sources_ahead_of_text() {
    let dir = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    std::fs::create_dir_all(dir.path().join(".git")).unwrap();
    write_file(&dir.path().join("AGENTS.md"), "Use tabs.");
    let config = doc_tool_config(dir.path(), state.path());
    let session = SessionState::new();
    let text = GetProjectDoc
        .call(json!({}), &config, &session)
        .await
        .joined_text();
    assert!(text.contains(&dir.path().join("AGENTS.md").display().to_string()));
    assert!(text.contains("Use tabs."));
}

#[tokio::test]
async fn project_doc_render_flags_truncated() {
    let dir = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    std::fs::create_dir_all(dir.path().join(".git")).unwrap();
    write_file(&dir.path().join("AGENTS.md"), "Use tabs everywhere.");
    let mut config = doc_tool_config(dir.path(), state.path());
    config.project_doc.max_bytes = Some(5);
    let session = SessionState::new();
    let text = GetProjectDoc
        .call(json!({}), &config, &session)
        .await
        .joined_text();
    assert!(text.contains("truncated"));
}

#[tokio::test]
async fn project_doc_render_says_no_conventions_when_none() {
    let dir = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    std::fs::create_dir_all(dir.path().join(".git")).unwrap();
    let config = doc_tool_config(dir.path(), state.path());
    let session = SessionState::new();
    let text = GetProjectDoc
        .call(json!({}), &config, &session)
        .await
        .joined_text();
    assert!(text.contains("No AGENTS.md found"));
}

#[tokio::test]
async fn project_doc_tool_returns_text_and_paths() {
    let dir = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    std::fs::create_dir_all(dir.path().join(".git")).unwrap();
    write_file(
        &dir.path().join("AGENTS.md"),
        "Run bun test before committing.",
    );
    let config = doc_tool_config(dir.path(), state.path());
    let session = SessionState::new();
    let r = GetProjectDoc.call(json!({}), &config, &session).await;
    assert!(!r.is_error);
    assert!(r.joined_text().contains("Run bun test before committing."));
    let path = dir.path().join("AGENTS.md").to_string_lossy().into_owned();
    assert_eq!(
        r.structured_content.unwrap(),
        json!({
            "files": [{ "path": path, "truncated": false }],
            "content": "Run bun test before committing.",
        })
    );
}

#[tokio::test]
async fn project_doc_tool_not_error_when_no_doc() {
    let dir = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    std::fs::create_dir_all(dir.path().join(".git")).unwrap();
    let config = doc_tool_config(dir.path(), state.path());
    let session = SessionState::new();
    let r = GetProjectDoc.call(json!({}), &config, &session).await;
    assert!(!r.is_error);
    assert_eq!(
        r.structured_content.unwrap(),
        json!({ "files": [], "content": "" })
    );
}

#[tokio::test]
async fn project_doc_tool_structured_keys_match_schema() {
    let dir = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    std::fs::create_dir_all(dir.path().join(".git")).unwrap();
    let config = doc_tool_config(dir.path(), state.path());
    let session = SessionState::new();
    let r = GetProjectDoc.call(json!({}), &config, &session).await;
    let mut keys: Vec<String> = r
        .structured_content
        .unwrap()
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect();
    keys.sort();
    assert_eq!(keys, vec!["content".to_string(), "files".to_string()]);
}

#[test]
fn build_inlines_doc_behind_marker() {
    let dir = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    std::fs::create_dir_all(dir.path().join(".git")).unwrap();
    write_file(&dir.path().join("AGENTS.md"), "Never force-push.");
    let text = build_instructions(&doc_tool_config(dir.path(), state.path()));
    assert!(text.contains("--- project-doc ---"));
    assert!(text.find("--- project-doc ---").unwrap() < text.find("Never force-push.").unwrap());
    assert!(text.contains("take precedence over everything above"));
}

#[test]
fn build_omits_marker_when_no_doc() {
    let dir = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    std::fs::create_dir_all(dir.path().join(".git")).unwrap();
    let text = build_instructions(&doc_tool_config(dir.path(), state.path()));
    assert!(!text.contains("--- project-doc ---"));
    assert!(text.contains("Working directory:"));
}

// --- get_agent_brief tool -----------------------------------------------

#[tokio::test]
async fn agent_brief_returns_exactly_build_instructions() {
    let dir = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    std::fs::create_dir_all(dir.path().join(".git")).unwrap();
    write_file(
        &dir.path().join("AGENTS.md"),
        "Run bun test before committing.",
    );
    let config = doc_tool_config(dir.path(), state.path());
    let session = SessionState::new();
    let r = GetAgentBrief.call(json!({}), &config, &session).await;
    assert!(!r.is_error);
    assert_eq!(r.joined_text(), build_instructions(&config));
}

#[tokio::test]
async fn agent_brief_covers_behaviour_env_project() {
    let dir = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    std::fs::create_dir_all(dir.path().join(".git")).unwrap();
    write_file(
        &dir.path().join("AGENTS.md"),
        "Run bun test before committing.",
    );
    let config = doc_tool_config(dir.path(), state.path());
    let session = SessionState::new();
    let text = GetAgentBrief
        .call(json!({}), &config, &session)
        .await
        .joined_text();
    assert!(text.contains("You are acting as a coding agent"));
    assert!(text.contains("Working directory:"));
    assert!(text.contains("Run bun test before committing."));
}

#[test]
fn agent_brief_takes_no_arguments() {
    let schema = GetAgentBrief.input_schema();
    assert_eq!(schema["properties"], json!({}));
    assert_eq!(schema["additionalProperties"], json!(false));
}

#[tokio::test]
async fn agent_brief_works_without_agents_md() {
    let dir = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    std::fs::create_dir_all(dir.path().join(".git")).unwrap();
    let config = doc_tool_config(dir.path(), state.path());
    let session = SessionState::new();
    let r = GetAgentBrief.call(json!({}), &config, &session).await;
    assert!(!r.is_error);
    assert!(r.joined_text().contains("You are acting as a coding agent"));
}

// --- get_environment ----------------------------------------------------

/// The get-environment suite pins workDir to "/tmp/project" and uses an explicit
/// allowlist so rendering of the opt-in policy remains covered.
fn env_config(default_shell: Option<&str>) -> AppConfig {
    let mut c = default_config(PathBuf::from("/tmp/project"));
    c.exec.mode = ExecMode::Allowlist;
    c.exec.extra_allowed_commands = vec!["node".to_string(), "git".to_string(), "ls".to_string()];
    c.exec.default_shell = default_shell.map(|s| s.to_string());
    c
}

#[test]
fn os_name_maps_platform_ids() {
    assert_eq!(os_name("win32"), "Windows");
    assert_eq!(os_name("darwin"), "macOS");
    assert_eq!(os_name("linux"), "Linux");
}

#[test]
fn os_name_passes_unknown_through() {
    assert_eq!(os_name("freebsd"), "freebsd");
}

#[test]
fn describe_reports_shell_it_would_launch() {
    let info = describe_environment(&env_config(Some("pwsh")));
    assert_eq!(info.shell.bin, "pwsh");
    assert_eq!(info.shell.type_, "powershell");
    assert_eq!(
        info.shell.argv_prefix,
        vec!["-NoProfile".to_string(), "-Command".to_string()]
    );
}

#[test]
fn describe_reports_workdir_and_effective_allowlist() {
    let info = describe_environment(&env_config(None));
    assert_eq!(info.cwd, "/tmp/project");
    assert_eq!(
        info.exec.allowed_commands,
        vec!["git".to_string(), "ls".to_string(), "node".to_string()]
    );
}

#[test]
fn describe_describes_host() {
    let info = describe_environment(&env_config(None));
    assert_eq!(info.platform, node_platform());
    assert_eq!(info.arch, node_arch());
    assert_eq!(info.path_separator, std::path::MAIN_SEPARATOR.to_string());
}

#[test]
fn render_gives_powershell_advice() {
    let text = render_environment(&describe_environment(&env_config(Some("powershell.exe"))));
    assert!(text.contains("Get-ChildItem"));
    assert!(!text.contains("POSIX shell syntax"));
}

#[test]
fn render_gives_cmd_advice() {
    let text = render_environment(&describe_environment(&env_config(Some("cmd.exe"))));
    assert!(text.contains("%VAR%"));
}

#[test]
fn render_gives_posix_advice() {
    let text = render_environment(&describe_environment(&env_config(Some("/bin/bash"))));
    assert!(text.contains("POSIX sh syntax"));
    assert!(!text.contains("Get-ChildItem"));
}

#[test]
fn render_spells_out_allowlist() {
    let text = render_environment(&describe_environment(&env_config(None)));
    assert!(text.contains("allowing: git, ls, node"));
}

#[test]
fn render_says_unrestricted_instead_of_listing() {
    let mut config = env_config(None);
    config.exec.mode = ExecMode::Unrestricted;
    let text = render_environment(&describe_environment(&config));
    assert!(text.contains("any command runs"));
    assert!(!text.contains("allowing:"));
}

#[tokio::test]
async fn environment_tool_returns_prose_and_structured() {
    let config = env_config(Some("bash"));
    let session = SessionState::new();
    let r = GetEnvironment.call(json!({}), &config, &session).await;
    assert!(!r.is_error);
    assert!(r.joined_text().contains("Working directory: /tmp/project"));
    let expected = serde_json::to_value(describe_environment(&config)).unwrap();
    assert_eq!(r.structured_content.unwrap(), expected);
}

#[tokio::test]
async fn environment_tool_structured_keys_match_schema() {
    let config = env_config(None);
    let session = SessionState::new();
    let r = GetEnvironment.call(json!({}), &config, &session).await;
    let mut keys: Vec<String> = r
        .structured_content
        .unwrap()
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect();
    keys.sort();
    let mut required = vec![
        "os",
        "platform",
        "arch",
        "cwd",
        "path_separator",
        "shell",
        "exec",
    ];
    required.sort();
    assert_eq!(keys, required);
}

#[test]
fn build_carries_environment_and_notes() {
    let state = TempDir::new().unwrap();
    // work_dir "/tmp/project" as the TS uses; pin memory/skills for determinism.
    let mut config = default_config(PathBuf::from("/tmp/project"));
    config.exec.default_shell = Some("bash".to_string());
    config.memory.dir = Some(state.path().to_string_lossy().into_owned());
    config.skills.dirs = Some(vec![]);

    let text = build_instructions(&config);
    assert!(text.contains("Working directory: /tmp/project"));
    assert!(text.contains("Shell for exec_command: bash (posix)"));
    assert!(text.contains("apply_patch"));
}
